//! JSON-RPC 2.0 transport for language-server child processes.
//!
//! One `JsonRpcClient` owns one child process. Dedicated stdout/stdin pump
//! threads isolate blocking pipes from runtime workers. Outbound frames have
//! count and byte admission limits; a pending map correlates responses. The
//! shared async completion wait retains its request owner and retires pending
//! work on timeout, cancellation, disconnection, or future drop.

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver as StdReceiver, SyncSender as StdSyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::tools::{ProcessCleanupMode, ProcessGuard};

mod completion;
mod outbound;
pub use completion::{CompletionWaitError, await_completion};

/// Hard cap on a single JSON-RPC frame body (64 MiB); larger frames are
/// treated as transport corruption and kill the connection.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
/// Bound on the notification queue; overflow drops the incoming notification
/// and is recorded in `dropped_notifications`.
const NOTIFICATION_QUEUE_CAP: usize = 1024;
/// Bound on retained server stderr (diagnostics surface), bytes.
const STDERR_TAIL_CAP: usize = 32 * 1024;
/// A server that never answers cannot accumulate unbounded completion slots.
const MAX_PENDING_REQUESTS: usize = 1024;
/// Only dedicated pipe workers wait here, never a runtime worker. A retired
/// connection must stop even if an escaped descendant still owns a pipe.
const PIPE_POLL_INTERVAL: Duration = Duration::from_millis(5);

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
    /// No work was admitted because bounded transport capacity is in use.
    /// Callers may wait within their original deadline before trying again.
    Backpressure(String),
}

impl TransportError {
    /// Machine-readable taxonomy code for logs and tool details.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Server(_) => "LSP_SERVER_ERROR",
            Self::Closed(_) => "LSP_TRANSPORT_CLOSED",
            Self::Io(_) => "LSP_TRANSPORT_IO",
            Self::Backpressure(_) => "LSP_TRANSPORT_BUSY",
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
            Self::Backpressure(reason) => format!("transport capacity busy: {reason}"),
        }
    }
}

/// A server notification (method + params), queued for the client layer.
#[derive(Debug, Clone)]
pub struct ServerNotification {
    pub method: String,
    pub params: Value,
    /// Losses observed before this notification was queued. Consumers can
    /// reject cached state and older queued reports across an overflow.
    pub(super) dropped_before: u64,
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

/// Shared frame-admission writer. The mutex never guards a blocking pipe.
type SharedWriter = Mutex<outbound::QueuedWriter>;

/// Pending request completions: reader thread sends exactly one result.
type PendingMap = Mutex<HashMap<u64, StdSyncSender<std::result::Result<Value, TransportError>>>>;

/// Retain one identity and encoded frame while waiting for admission. Taking
/// the frame after acceptance prevents another attempt from posting it twice.
pub(super) struct PreparedRequest {
    id: u64,
    frame: Option<Vec<u8>>,
}

/// An all-or-none group of document notifications. Retain the encoded bytes
/// across capacity waits and make a successful admission impossible to repeat.
pub(super) struct PreparedNotifications {
    frames: Option<Vec<Vec<u8>>>,
}

/// Hook for server→client requests (e.g. `workspace/applyEdit`). Returning
/// `Some(result)` overrides the default null response.
pub type ServerRequestHandler = std::sync::Arc<dyn Fn(&str, &Value) -> Option<Value> + Send + Sync>;

/// Native request services can reject invalid parameters without turning an
/// error object into a successful `result`. The public result-only callback
/// remains useful for application-specific requests such as applyEdit.
pub(super) type ServerRequestResponder = Arc<
    dyn Fn(&str, &Value) -> Option<std::result::Result<Value, RpcErrorObject>> + Send + Sync,
>;

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

/// Read one framed message from `reader` without consuming any bytes past the
/// frame, so back-to-back frames survive sequential calls (this wrapper has no
/// scratch to carry over-read bytes between calls). Returns `Ok(None)` on
/// clean EOF before any header byte. Crate-public: the DAP transport uses the
/// same framing (bd-cv653.1.2).
pub(crate) fn read_frame(reader: &mut BufReader<impl Read>) -> std::io::Result<Option<Value>> {
    // Headers byte-at-a-time (cheap through the BufReader) so nothing beyond
    // this frame is pulled out of the reader.
    let mut header: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if reader.read(&mut byte)? == 0 {
            if header.is_empty() {
                return Ok(None); // clean EOF
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF mid-headers",
            ));
        }
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
        if header.len() > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "headers exceed 64 KiB",
            ));
        }
    }
    let length = parse_content_length(&header)?;
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

/// Parse the `Content-Length` value out of a raw header block, enforcing the
/// frame-size cap.
fn parse_content_length(header_bytes: &[u8]) -> std::io::Result<usize> {
    let headers = String::from_utf8_lossy(header_bytes);
    let mut content_length: Option<usize> = None;
    for line in headers.split("\r\n") {
        if let Some(value) = line
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
    Ok(length)
}

/// Read one framed message, carrying leftover bytes in `scratch` between
/// calls. `scratch` holds any over-read bytes from the previous frame.
pub(crate) fn read_frame_with_scratch(
    reader: &mut BufReader<impl Read>,
    scratch: &mut Vec<u8>,
) -> std::io::Result<Option<Value>> {
    let trace = std::env::var_os("PI_DAP_TRACE").is_some();
    let mut chunk = [0u8; 8192];
    // Phase 1: headers (scratch may already hold some).
    let body_start = loop {
        if let Some(pos) = find_subslice(scratch, b"\r\n\r\n") {
            break pos + 4;
        }
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(None); // EOF
        }
        scratch.extend_from_slice(&chunk[..read]);
        if scratch.len() > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "headers exceed 64 KiB",
            ));
        }
    };
    if trace {
        let headers = String::from_utf8_lossy(&scratch[..body_start]);
        eprintln!("[dap-frame] headers: {headers:?}");
    }
    let length = parse_content_length(&scratch[..body_start])?;
    // Phase 2: body (scratch already holds the first bytes after headers).
    let mut body: Vec<u8> = scratch.split_off(body_start);
    while body.len() < length {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF mid-body",
            ));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    // Preserve over-read bytes for the next frame.
    let leftover = body.split_off(length);
    *scratch = leftover;
    if trace {
        eprintln!(
            "[dap-frame] body {} bytes: {:?}",
            length,
            String::from_utf8_lossy(&body[..length.min(120)])
        );
    }
    let value = serde_json::from_slice(&body).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid JSON body: {err}"),
        )
    })?;
    Ok(Some(value))
}

/// Find the first occurrence of `needle` in `hay`.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
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

/// Crate-shared bounded tail buffer (the DAP transport reuses it for
/// adapter stderr and process output — bd-cv653.1.2).
#[derive(Debug)]
pub(crate) struct PublicTailBuffer {
    inner: TailBuffer,
}

impl PublicTailBuffer {
    /// A 32 KiB tail buffer.
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self {
            inner: TailBuffer {
                data: String::new(),
                cap: 32 * 1024,
            },
        }
    }

    /// Append, discarding the oldest content past the cap.
    pub(crate) fn push(&mut self, chunk: &str) {
        self.inner.push(chunk);
    }

    /// The retained tail.
    #[must_use]
    pub(crate) fn tail(&self) -> String {
        self.inner.data.clone()
    }
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

/// Blocking adapters belong only to the dedicated pipe workers. Retrying
/// WouldBlock inside Read/Write preserves the parser's partially read body and
/// the writer's partially sent frame while still observing retirement.
struct InterruptiblePipe<P> {
    pipe: P,
    alive: Arc<AtomicBool>,
}

impl<P> InterruptiblePipe<P> {
    fn new(pipe: P, alive: &Arc<AtomicBool>) -> Self {
        Self {
            pipe,
            alive: Arc::clone(alive),
        }
    }

    fn check_open(&self) -> std::io::Result<()> {
        if self.alive.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "server transport retired",
            ))
        }
    }
}

impl<P: Read> Read for InterruptiblePipe<P> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        loop {
            self.check_open()?;
            match self.pipe.read(buffer) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(PIPE_POLL_INTERVAL);
                }
                result => return result,
            }
        }
    }
}

impl<P: Write> Write for InterruptiblePipe<P> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        loop {
            self.check_open()?;
            match self.pipe.write(bytes) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(PIPE_POLL_INTERVAL);
                }
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        loop {
            self.check_open()?;
            match self.pipe.flush() {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(PIPE_POLL_INTERVAL);
                }
                result => return result,
            }
        }
    }
}

#[cfg(unix)]
fn prepare_pipe(pipe: &impl std::os::fd::AsFd) -> std::io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(pipe)?;
    rustix::fs::fcntl_setfl(pipe, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}

/// Keep the actual child handle until a reap succeeds. Cancellation starts a
/// worker without waiting; final drop still owns and joins that same worker.
struct ServerProcess {
    cleanup: Mutex<ProcessCleanup>,
}

struct ProcessCleanup {
    child: Option<ProcessGuard>,
    reaper: Option<ProcessReaper>,
}

struct ProcessReaper {
    worker: JoinHandle<std::io::Result<std::process::ExitStatus>>,
    child: Arc<Mutex<Option<ProcessGuard>>>,
}

impl ServerProcess {
    const fn new(child: ProcessGuard) -> Self {
        Self {
            cleanup: Mutex::new(ProcessCleanup {
                child: Some(child),
                reaper: None,
            }),
        }
    }

    // The cleanup guard keeps `close` from retiring the child while its
    // pipes are being taken.
    #[allow(clippy::significant_drop_tightening)]
    fn take_pipes(
        &self,
    ) -> std::io::Result<(
        std::process::ChildStdin,
        std::process::ChildStdout,
        std::process::ChildStderr,
    )> {
        let mut cleanup = lock(&self.cleanup);
        let child = cleanup
            .child
            .as_mut()
            .ok_or_else(|| std::io::Error::other("server process already retired"))?;
        let stdin = child
            .take_stdin()
            .ok_or_else(|| std::io::Error::other("missing server stdin"))?;
        let stdout = child
            .take_stdout()
            .ok_or_else(|| std::io::Error::other("missing server stdout"))?;
        let stderr = child
            .take_stderr()
            .ok_or_else(|| std::io::Error::other("missing server stderr"))?;
        #[cfg(unix)]
        {
            prepare_pipe(&stdin)?;
            prepare_pipe(&stdout)?;
            prepare_pipe(&stderr)?;
        }
        Ok((stdin, stdout, stderr))
    }

    #[allow(clippy::option_if_let_else)]
    fn child_exited(&self) -> bool {
        let mut cleanup = lock(&self.cleanup);
        match cleanup.child.as_mut() {
            Some(child) => matches!(child.try_wait_child(), Ok(Some(_))),
            None => cleanup
                .reaper
                .as_ref()
                .is_none_or(|reaper| reaper.worker.is_finished()),
        }
    }

    fn close(&self, wait: bool) -> std::io::Result<()> {
        let mut cleanup = lock(&self.cleanup);
        if let Some(child) = cleanup.child.take() {
            let child = Arc::new(Mutex::new(Some(child)));
            let worker_child = Arc::clone(&child);
            if let Ok(worker) = std::thread::Builder::new()
                .name("pi-lsp-reaper".to_string())
                .spawn(move || {
                    lock(&worker_child)
                        .as_mut()
                        .ok_or_else(|| std::io::Error::other("server reaper lost its child"))?
                        .terminate_and_wait()
                })
            {
                cleanup.reaper = Some(ProcessReaper { worker, child });
            } else {
                // The OS can refuse another thread. Cleanup still owns
                // the child and must not silently drop its reap obligation.
                let mut child = lock(&child)
                    .take()
                    .ok_or_else(|| std::io::Error::other("server cleanup lost its child"))?;
                if let Err(error) = child.terminate_and_wait() {
                    cleanup.child = Some(child);
                    return Err(error);
                }
            }
        }
        let reaper = if wait { cleanup.reaper.take() } else { None };
        drop(cleanup);
        if let Some(reaper) = reaper {
            let result = reaper
                .worker
                .join()
                .map_err(|_| std::io::Error::other("server reaper panicked"))
                .and_then(std::convert::identity);
            if result.is_err() {
                // A failed wait or panic leaves the guarded child available
                // for a later close or final owner drop to retry.
                lock(&self.cleanup).child = lock(&reaper.child).take();
            }
            result?;
        }
        Ok(())
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if let Err(error) = self.close(true) {
            tracing::warn!("Failed to reap language server: {error}");
        }
    }
}

/// A failed pipe or panicking callback must retire the connection and its
/// process even when the caller retains the client without issuing more work.
struct ServerWorkerGuard {
    process: Arc<ServerProcess>,
    pending: Arc<PendingMap>,
    alive: Arc<AtomicBool>,
    writer: outbound::ShutdownHandle,
}

impl Drop for ServerWorkerGuard {
    fn drop(&mut self) {
        outbound::close_pending(
            &self.pending,
            &self.alive,
            &TransportError::Closed("server pipe worker stopped".to_string()),
        );
        self.writer.close();
        if let Err(error) = self.process.close(false) {
            tracing::warn!("Failed to start language server cleanup: {error}");
        }
    }
}

#[derive(Default)]
struct ServerWorkers {
    stdin: Option<JoinHandle<()>>,
    stdout: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
}

/// Build the reader-thread body: parse frames, complete pending requests,
/// queue notifications, answer server→client requests. On exit the
/// transport flips dead and every pending request fails fast.
#[allow(clippy::too_many_arguments)]
fn reader_loop(
    stdout: impl Read + Send + 'static,
    pending: std::sync::Arc<PendingMap>,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    writer: std::sync::Arc<SharedWriter>,
    stderr_tail: std::sync::Arc<Mutex<TailBuffer>>,
    notification_tx: StdSyncSender<ServerNotification>,
    dropped: std::sync::Arc<AtomicU64>,
    handler: std::sync::Arc<Mutex<Option<ServerRequestResponder>>>,
) -> impl FnOnce() + Send + 'static {
    move || {
        let mut reader = BufReader::new(stdout);
        let mut scratch = Vec::new();
        let close_reason = loop {
            if !alive.load(Ordering::Acquire) {
                break "server transport retired".to_string();
            }
            match read_frame_with_scratch(&mut reader, &mut scratch) {
                Ok(Some(message)) => {
                    // BufReader/scratch can hold complete frames even after
                    // retirement, without another call to the pipe adapter.
                    if !alive.load(Ordering::Acquire) {
                        break "server transport retired".to_string();
                    }
                    if let Err(err) = handle_message(
                        &message,
                        &pending,
                        &writer,
                        &notification_tx,
                        &dropped,
                        &stderr_tail,
                        &handler,
                    ) {
                        // A missing reply can strand the peer in a nested
                        // request (configuration, applyEdit, MCP callbacks).
                        // Retire pending work now, without waiting for EOF or
                        // pretending the reply was delivered.
                        break format!("server reply queue failed: {err}");
                    }
                }
                Ok(None) => break "server closed stdout (EOF)".to_string(),
                Err(err) => break format!("frame read error: {err}"),
            }
        };
        outbound::close_pending(&pending, &alive, &TransportError::Closed(close_reason));
    }
}

/// One JSON-RPC connection to a language-server child process.
pub struct JsonRpcClient {
    process: Arc<ServerProcess>,
    workers: Mutex<ServerWorkers>,
    writer: std::sync::Arc<SharedWriter>,
    pending: std::sync::Arc<PendingMap>,
    next_id: AtomicU64,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stderr_tail: std::sync::Arc<Mutex<TailBuffer>>,
    notification_rx: Mutex<StdReceiver<ServerNotification>>,
    dropped_notifications: std::sync::Arc<AtomicU64>,
    server_request_handler: std::sync::Arc<Mutex<Option<ServerRequestResponder>>>,
}

impl JsonRpcClient {
    /// Spawn `command` with `args`/`env` rooted at `cwd` and start the
    /// reader, writer, and stderr pump threads.
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
    #[allow(clippy::too_many_lines)]
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
        crate::tools::isolate_command_process_group(&mut cmd);
        let child = crate::tools::spawn_command_with_job_discipline(&mut cmd).map_err(|err| {
            Error::tool(
                flavor,
                format!(
                    "[{}_SERVER_MISSING] failed to spawn {command:?}: {err}",
                    flavor.to_ascii_uppercase()
                ),
            )
        })?;
        // Install the process owner before any fallible pipe/worker setup.
        // Its Drop explicitly terminates and reaps even an already-exited
        // root's surviving process group or retained Windows Job.
        let process = Arc::new(ServerProcess::new(ProcessGuard::new(
            child,
            ProcessCleanupMode::ProcessGroupTree,
        )));
        let (stdin, stdout, stderr) = process
            .take_pipes()
            .map_err(|err| Error::tool(flavor, format!("failed to prepare server pipes: {err}")))?;
        let pending: Arc<PendingMap> = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let (queued_writer, stdin_worker) = outbound::QueuedWriter::start(
            InterruptiblePipe::new(stdin, &alive),
            Arc::clone(&pending),
            Arc::clone(&alive),
            Some(Arc::clone(&process)),
        )
        .map_err(|err| Error::tool(flavor, format!("failed to start server writer: {err}")))?;
        let (notification_tx, notification_rx) =
            std::sync::mpsc::sync_channel::<ServerNotification>(NOTIFICATION_QUEUE_CAP);
        let client = Self {
            process,
            workers: Mutex::new(ServerWorkers {
                stdin: Some(stdin_worker),
                ..ServerWorkers::default()
            }),
            writer: Arc::new(Mutex::new(queued_writer)),
            pending,
            next_id: AtomicU64::new(1),
            alive,
            stderr_tail: Arc::new(Mutex::new(TailBuffer {
                data: String::new(),
                cap: STDERR_TAIL_CAP,
            })),
            notification_rx: Mutex::new(notification_rx),
            dropped_notifications: Arc::new(AtomicU64::new(0)),
            server_request_handler: Arc::new(Mutex::new(None)),
        };

        // Reader thread: parse frames, complete pending requests, queue
        // notifications, answer server->client requests with null.
        {
            let cleanup = ServerWorkerGuard {
                process: Arc::clone(&client.process),
                pending: Arc::clone(&client.pending),
                alive: Arc::clone(&client.alive),
                writer: lock(&client.writer).shutdown_handle(),
            };
            let reader_body = reader_loop(
                InterruptiblePipe::new(stdout, &client.alive),
                Arc::clone(&client.pending),
                Arc::clone(&client.alive),
                Arc::clone(&client.writer),
                Arc::clone(&client.stderr_tail),
                notification_tx,
                Arc::clone(&client.dropped_notifications),
                Arc::clone(&client.server_request_handler),
            );
            let worker = std::thread::Builder::new()
                .name("pi-lsp-stdout".to_string())
                .spawn(move || {
                    let _cleanup = cleanup;
                    reader_body();
                })
                .map_err(|err| {
                    Error::tool(flavor, format!("failed to start server reader: {err}"))
                })?;
            lock(&client.workers).stdout = Some(worker);
        }

        // Stderr pump: retain a bounded tail for diagnostics surfacing.
        {
            let stderr_tail = Arc::clone(&client.stderr_tail);
            let alive = Arc::clone(&client.alive);
            let stderr = InterruptiblePipe::new(stderr, &alive);
            let pump_body = move || {
                let mut reader = BufReader::new(stderr);
                let mut buf = [0u8; 4096];
                while alive.load(Ordering::Acquire) {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = String::from_utf8_lossy(&buf[..n]);
                            lock(&stderr_tail).push(&chunk);
                        }
                    }
                }
            };
            let worker = std::thread::Builder::new()
                .name("pi-lsp-stderr".to_string())
                .spawn(pump_body)
                .map_err(|err| {
                    Error::tool(flavor, format!("failed to start server stderr: {err}"))
                })?;
            lock(&client.workers).stderr = Some(worker);
        }

        Ok(client)
    }

    /// Install a handler for server→client requests. The handler runs on the
    /// reader thread; it must stay fast and non-blocking (file I/O for
    /// `workspace/applyEdit` is acceptable and bounded).
    pub fn set_server_request_handler(&self, handler: ServerRequestHandler) {
        self.set_server_request_responder(Arc::new(move |method, params| {
            handler(method, params).map(Ok)
        }));
    }

    pub(super) fn set_server_request_responder(&self, handler: ServerRequestResponder) {
        *lock(&self.server_request_handler) = Some(handler);
    }

    /// Whether the transport is still up.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(super) fn pause_outbound_for_test(
        &self,
        capacity: usize,
        bytes: usize,
    ) -> outbound::PausedOutbound<'_> {
        outbound::PausedOutbound::new(self, capacity, bytes)
    }

    /// Queue a request and return its completion receiver and request id.
    /// Only the dedicated pump writes the blocking pipe. An abandoned request
    /// is skipped if it is still queued; an in-flight write may already have
    /// reached the server and is never retried here.
    ///
    /// # Errors
    /// Returns an error for a dead transport, exhausted request slots/ids, or
    /// outbound admission failure. `Backpressure` refuses only this unposted
    /// request; it has made no remote change and is not retried here. Broken
    /// transport errors still retire the connection and all pending work.
    pub fn request(
        &self,
        method: &str,
        params: Value,
    ) -> std::result::Result<
        (u64, StdReceiver<std::result::Result<Value, TransportError>>),
        TransportError,
    > {
        let mut request = self.prepare_request(method, params)?;
        self.try_admit_request(&mut request)
    }

    pub(super) fn prepare_request(
        &self,
        method: &str,
        params: Value,
    ) -> std::result::Result<PreparedRequest, TransportError> {
        if !self.is_alive() {
            return Err(TransportError::Closed(
                "server transport is not alive".to_string(),
            ));
        }
        let id = self
            .next_id
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |id| id.checked_add(1))
            .map_err(|_| TransportError::Io("request id space exhausted".to_string()))?;
        let mut frame_value = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
        });
        frame_value["params"] = params;
        Ok(PreparedRequest {
            id,
            frame: Some(encode_frame(&frame_value)),
        })
    }

    /// Retry only a frame that has never been accepted by the writer. No
    /// completion slot or queue reservation survives a backpressure result.
    pub(super) fn try_admit_request(
        &self,
        request: &mut PreparedRequest,
    ) -> std::result::Result<
        (u64, StdReceiver<std::result::Result<Value, TransportError>>),
        TransportError,
    > {
        let frame = request
            .frame
            .as_ref()
            .ok_or_else(|| TransportError::Io("request was already admitted".to_string()))?;
        let id = request.id;
        // Check permanent rejection before any transient pending-slot limit.
        // Keep the same writer -> pending lock order used by cancellation;
        // this mutex only guards bounded admission, never the blocking pipe.
        let mut writer = lock(&self.writer);
        writer
            .validate_frame_size(frame.len())
            .map_err(|error| TransportError::Io(format!("request queue failed: {error}")))?;
        let rx = {
            // Closure marks the transport dead before taking this same lock.
            // No request can appear after the terminal pending-map drain.
            let mut pending = lock(&self.pending);
            if !self.is_alive() {
                return Err(TransportError::Closed(
                    "server transport is not alive".to_string(),
                ));
            }
            if pending.len() >= MAX_PENDING_REQUESTS {
                return Err(TransportError::Backpressure(
                    "pending request limit exceeded".to_string(),
                ));
            }
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            pending.insert(id, tx);
            rx
        };
        let write_result = writer
            .write_request(frame, id)
            .and_then(|()| writer.flush());
        drop(writer);
        if let Err(err) = write_result {
            lock(&self.pending).remove(&id);
            // QueuedWriter admits complete frames atomically. WouldBlock
            // means no bytes were accepted, unlike a partially written pipe:
            // preserve earlier requests and document synchronization state.
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Err(TransportError::Backpressure(format!(
                    "request queue failed: {err}"
                )));
            }
            // A frame larger than the entire budget can never fit, but it
            // was not posted either. Reject it without disrupting older work.
            if err.kind() != std::io::ErrorKind::InvalidInput {
                self.kill();
            }
            return Err(TransportError::Io(format!("request queue failed: {err}")));
        }
        request.frame = None;
        Ok((id, rx))
    }

    pub(super) fn prepare_notifications(
        notifications: Vec<(&str, Value)>,
    ) -> PreparedNotifications {
        PreparedNotifications {
            frames: Some(
                notifications
                    .into_iter()
                    .map(|(method, params)| {
                        encode_frame(&serde_json::json!({
                            "jsonrpc": "2.0", "method": method, "params": params
                        }))
                    })
                    .collect(),
            ),
        }
    }

    /// Admit a complete document update without publishing a partial prefix.
    /// Backpressure and permanent size rejection leave the connection and
    /// retained batch intact. Other failures retire an indeterminate transport.
    pub(super) fn try_admit_notifications(
        &self,
        batch: &mut PreparedNotifications,
    ) -> std::result::Result<(), TransportError> {
        let frames = batch.frames.as_mut().ok_or_else(|| {
            TransportError::Io("notification batch was already admitted".to_string())
        })?;
        let write_result = {
            let mut writer = lock(&self.writer);
            writer
                .write_notification_batch(frames)
                .and_then(|()| writer.flush())
        };
        if let Err(error) = write_result {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err(TransportError::Backpressure(format!(
                    "notification queue failed: {error}"
                )));
            }
            if error.kind() != std::io::ErrorKind::InvalidInput {
                self.kill();
            }
            return Err(TransportError::Io(format!(
                "notification queue failed: {error}"
            )));
        }
        batch.frames = None;
        Ok(())
    }

    /// Queue a notification (no response expected), without blocking on stdin.
    /// Success means accepted by the bounded queue, not remote acknowledgement.
    ///
    /// # Errors
    /// Returns an error and retires the transport when admission fails.
    pub fn notify(&self, method: &str, params: Value) -> std::result::Result<(), TransportError> {
        let mut frame_value = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        frame_value["params"] = params;
        let frame = encode_frame(&frame_value);
        let write_result = {
            let mut guard = lock(&self.writer);
            guard.write_all(&frame).and_then(|()| guard.flush())
        };
        if let Err(err) = write_result {
            self.kill();
            return Err(TransportError::Io(format!(
                "notification queue failed: {err}"
            )));
        }
        Ok(())
    }

    /// Retire a request exactly once. Unclaimed frames are withdrawn without
    /// reaching the server; a claimed frame is followed by `$/cancelRequest`
    /// through reserved control capacity. Safe from completion-future drop:
    /// neither a blocked pipe nor a full ordinary queue can block cancellation.
    pub fn cancel_request(&self, id: u64) {
        let result = {
            // Hold admission until the retired request is withdrawn or its
            // cancellation is queued. A newly admitted successor cannot
            // overtake cancellation or consume its reserved control capacity.
            let writer = lock(&self.writer);
            if lock(&self.pending).remove(&id).is_some() {
                writer.cancel_request(id)
            } else {
                Ok(())
            }
        };
        if result.is_err() {
            self.kill();
        }
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
        let exited = self.process.child_exited();
        if exited {
            // Helpers can keep both output pipes open after the root exits.
            // Retire them even when no more request is sent on this client.
            self.kill();
        }
        exited
    }

    /// Best-effort exit notification, then a hard stop. The client layer owns
    /// the timed shutdown handshake; this method never waits for pipe drainage.
    pub fn shutdown(&self) {
        let _ = self.notify("exit", Value::Null);
        self.kill();
    }

    /// Retire every pending request and start owned process-tree cleanup.
    /// No pipe or worker join is performed here. Cleanup uses a reaper thread;
    /// only OS thread-creation failure falls back to synchronous termination.
    pub fn kill(&self) {
        outbound::close_pending(
            &self.pending,
            &self.alive,
            &TransportError::Closed("server transport stopped".to_string()),
        );
        lock(&self.writer).shutdown_handle().close();
        if let Err(error) = self.process.close(false) {
            lock(&self.stderr_tail).push(&format!("[cleanup] {error}\n"));
        }
    }

    fn finish_cleanup(&self) -> std::io::Result<()> {
        // Do not join potentially blocked Windows pipes if the OS failed to
        // terminate the owned Job. Keep the handles for a later retry.
        self.process.close(true)?;
        let workers = std::mem::take(&mut *lock(&self.workers));
        let current = std::thread::current().id();
        let mut panicked = false;
        for worker in [workers.stdin, workers.stdout, workers.stderr]
            .into_iter()
            .flatten()
        {
            // A server callback may release the final client owner. Its own
            // reader will exit after the callback; a thread cannot join itself.
            if worker.thread().id() != current && worker.join().is_err() {
                panicked = true;
            }
        }
        if panicked {
            Err(std::io::Error::other("server pipe worker panicked"))
        } else {
            Ok(())
        }
    }
}

impl Drop for JsonRpcClient {
    fn drop(&mut self) {
        self.kill();
        if let Err(error) = self.finish_cleanup() {
            tracing::warn!("Failed to finish language server cleanup: {error}");
        }
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
    handler: &Mutex<Option<ServerRequestResponder>>,
) -> std::io::Result<()> {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str);
    let is_response = method.is_none() && id.is_some();

    if is_response {
        // Response to one of our requests (ids we mint are u64).
        let Some(numeric_id) = id.and_then(|v| v.as_u64()) else {
            return Ok(());
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
        return Ok(());
    }

    if let (Some(id), Some(method)) = (id, method) {
        // Server -> client request: consult the installed handler first
        // (workspace services, applyEdit and friends). Unhandled requests keep
        // the transport's default null response; native services may instead
        // produce a proper JSON-RPC error with the same opaque request id.
        // The id is echoed verbatim (servers may use string ids).
        // Never invoke host code under the registration lock. A handler may
        // replace its registration or enter other client APIs while answering.
        let handler = lock(handler).clone();
        let custom = handler.as_ref().and_then(|handler| {
            handler(
                method,
                &message.get("params").cloned().unwrap_or(Value::Null),
            )
        });
        let mut response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
        });
        match custom.unwrap_or(Ok(Value::Null)) {
            Ok(result) => response["result"] = result,
            Err(error) => response["error"] = serde_json::json!(error),
        }
        let frame = encode_frame(&response);
        {
            let mut guard = lock(writer);
            guard.write_all(&frame).and_then(|()| guard.flush())?;
        }
        return Ok(());
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
            dropped_before: dropped.load(Ordering::SeqCst),
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_request_errors_echo_the_id_without_a_success_result() {
        for id in [serde_json::json!(7), serde_json::json!("server/7")] {
            let handler: ServerRequestResponder = Arc::new(|_, _| {
                Some(Err(RpcErrorObject {
                    code: -32602,
                    message: "invalid workspace request".into(),
                    data: None,
                }))
            });
            let writer = Mutex::new(Vec::new());
            let (notifications, receiver) = std::sync::mpsc::sync_channel(1);
            handle_message(
                &serde_json::json!({"jsonrpc":"2.0","id":id,
                    "method":"workspace/configuration","params":{}}),
                &Mutex::new(HashMap::new()),
                &writer,
                &notifications,
                &AtomicU64::new(0),
                &Mutex::new(TailBuffer::default()),
                &Mutex::new(Some(handler)),
            )
            .expect("error reply admitted");
            let bytes = writer.into_inner().unwrap();
            let reply = read_frame(&mut BufReader::new(bytes.as_slice())).unwrap().unwrap();
            assert_eq!(reply["id"], id);
            assert_eq!(reply["error"]["code"], -32602);
            assert!(reply.get("result").is_none());
            assert!(reply["error"].get("data").is_none());
            assert!(receiver.try_recv().is_err());
        }
    }

    #[test]
    fn server_reply_write_failure_is_not_reported_as_success() {
        let pending = Mutex::new(HashMap::new());
        let (notification_tx, _rx) = std::sync::mpsc::sync_channel(1);
        let (reader, pipe) = std::io::pipe().expect("pipe");
        drop(reader);
        let result = handle_message(
            &serde_json::json!({"jsonrpc":"2.0", "id":"server/7",
                "method":"workspace/configuration", "params":{}}),
            &pending,
            &Mutex::new(pipe),
            &notification_tx,
            &AtomicU64::new(0),
            &Mutex::new(TailBuffer::default()),
            &Mutex::new(None),
        );
        assert_eq!(
            result.expect_err("reply pipe is broken").kind(),
            std::io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn server_request_handler_can_replace_its_registration_without_deadlocking() {
        let handlers = std::sync::Arc::new(Mutex::new(None::<ServerRequestResponder>));
        let registration = std::sync::Arc::downgrade(&handlers);
        *lock(&handlers) = Some(std::sync::Arc::new(move |method, params| {
            assert_eq!(method, "workspace/configuration");
            assert_eq!(params["section"], "rust");
            let registration = registration.upgrade().expect("registration alive");
            *registration
                .try_lock()
                .expect("callback must not hold registration lock") = None;
            Some(Ok(serde_json::json!({"configured":true})))
        }));
        let (reader, pipe) = std::io::pipe().expect("pipe");
        let (notification_tx, _rx) = std::sync::mpsc::sync_channel(1);
        handle_message(
            &serde_json::json!({"jsonrpc":"2.0", "id":"server/17",
                "method":"workspace/configuration", "params":{"section":"rust"}}),
            &Mutex::new(HashMap::new()),
            &Mutex::new(pipe),
            &notification_tx,
            &AtomicU64::new(0),
            &Mutex::new(TailBuffer::default()),
            &handlers,
        )
        .expect("reply admitted");
        let response = read_frame(&mut BufReader::new(reader)).expect("reply frame");
        assert_eq!(
            response,
            Some(serde_json::json!({"jsonrpc":"2.0", "id":"server/17",
                "result":{"configured":true}}))
        );
        assert!(lock(&handlers).is_none());
    }

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
        )
        .expect("route response");
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
        )
        .expect("queue server reply");
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
            )
            .expect("route notification");
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
        let first = notification_rx
            .try_recv()
            .expect("first queued notification");
        assert_eq!(first.params["i"], 0);
        assert_eq!(first.dropped_before, 0);
        handle_message(
            &serde_json::json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"i":4}}),
            &pending,
            &writer,
            &notification_tx,
            &dropped,
            &stderr_tail,
            &Mutex::new(None),
        )
        .expect("notification after capacity is released");
        let older = notification_rx
            .try_recv()
            .expect("older queued notification");
        assert_eq!(older.params["i"], 1);
        assert_eq!(older.dropped_before, 0);
        let fresh = notification_rx.try_recv().expect("notification after loss");
        assert_eq!(fresh.params["i"], 4);
        assert_eq!(fresh.dropped_before, 2);
        assert!(notification_rx.try_recv().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn kill_completes_pending_without_waiting_for_stdout_eof() {
        let temp = tempfile::tempdir().expect("tempdir");
        let client = JsonRpcClient::spawn("cat", &[], &[], temp.path()).expect("cat");
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        lock(&client.pending).insert(99, sender);
        client.kill();
        assert!(matches!(
            receiver.try_recv(),
            Ok(Err(TransportError::Closed(_)))
        ));
        assert!(lock(&client.pending).is_empty());
        assert!(matches!(
            client.request("later", Value::Null),
            Err(TransportError::Closed(_))
        ));
    }

    #[cfg(unix)]
    fn wait_for_fixture(mut ready: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    #[cfg(unix)]
    fn fixture_process_status(pid: u32) -> Option<sysinfo::ProcessStatus> {
        let pid = sysinfo::Pid::from_u32(pid);
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        system.process(pid).map(sysinfo::Process::status)
    }

    #[cfg(unix)]
    fn fixture_process_running(pid: u32) -> bool {
        fixture_process_status(pid).is_some_and(|status| {
            !matches!(
                status,
                sysinfo::ProcessStatus::Zombie | sysinfo::ProcessStatus::Dead
            )
        })
    }

    #[cfg(unix)]
    fn fixture_pids(root: &Path) -> (u32, u32) {
        let mut pids = None;
        assert!(
            wait_for_fixture(|| {
                pids = std::fs::read_to_string(root.join("pids"))
                    .ok()
                    .and_then(|text| {
                        let mut values = text.split_whitespace();
                        Some((values.next()?.parse().ok()?, values.next()?.parse().ok()?))
                    });
                pids.is_some()
            }),
            "server fixture did not publish its process identities"
        );
        pids.expect("fixture identities")
    }

    #[cfg(unix)]
    fn tree_fixture(root: &Path, exit_root: bool) -> (JsonRpcClient, u32, u32) {
        // The helper explicitly inherits stdin as well as stdout/stderr, so
        // killing only the shell cannot unblock any of the three pipe workers.
        let ending = if exit_root { "exit 0" } else { "wait" };
        let script = format!("sleep 30 <&0 &\nprintf '%s %s\\n' \"$$\" \"$!\" > pids\n{ending}\n");
        let client = JsonRpcClient::spawn("/bin/sh", &["-c".to_string(), script], &[], root)
            .expect("server tree");
        let (parent, helper) = fixture_pids(root);
        assert!(fixture_process_running(helper), "helper must start alive");
        if exit_root {
            assert!(wait_for_fixture(|| client.process.child_exited()));
            assert!(
                fixture_process_running(helper),
                "root exit must leave inherited pipes open"
            );
        }
        (client, parent, helper)
    }

    #[cfg(unix)]
    fn workers_finished(client: &JsonRpcClient) -> bool {
        let workers = lock(&client.workers);
        [&workers.stdin, &workers.stdout, &workers.stderr]
            .into_iter()
            .all(|worker| worker.as_ref().is_none_or(JoinHandle::is_finished))
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_reaps_live_and_exited_root_trees_with_a_retained_client() {
        for exit_root in [false, true] {
            let root = tempfile::tempdir().expect("tempdir");
            let (client, parent, helper) = tree_fixture(root.path(), exit_root);
            let client = Arc::new(client);
            let retained = Arc::clone(&client);
            let (_, response) = client
                .request("test/pending", Value::Null)
                .expect("request");
            client.shutdown();
            assert!(matches!(
                response.try_recv(),
                Ok(Err(TransportError::Closed(_)))
            ));
            assert!(!retained.is_alive());
            assert!(
                wait_for_fixture(|| workers_finished(&retained)),
                "pipe workers survived retirement"
            );
            retained.finish_cleanup().expect("owned cleanup");
            assert_eq!(fixture_process_status(parent), None, "root must be reaped");
            assert!(
                wait_for_fixture(|| !fixture_process_running(helper)),
                "helper survived shutdown"
            );
            assert!(matches!(
                retained.request("later", Value::Null),
                Err(TransportError::Closed(_))
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn dropping_client_reaps_live_and_exited_root_descendants() {
        for exit_root in [false, true] {
            let root = tempfile::tempdir().expect("tempdir");
            let (client, parent, helper) = tree_fixture(root.path(), exit_root);
            drop(client);
            assert_eq!(
                fixture_process_status(parent),
                None,
                "drop must reap its root"
            );
            assert!(
                wait_for_fixture(|| !fixture_process_running(helper)),
                "helper survived client drop"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn failed_pipe_setup_reaps_an_exited_root_and_its_remaining_group() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "sleep 30 &\nprintf '%s %s\\n' \"$$\" \"$!\" > pids\nexit 0\n",
            ])
            .current_dir(root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::tools::isolate_command_process_group(&mut command);
        let child = crate::tools::spawn_command_with_job_discipline(&mut command)
            .expect("incomplete pipe fixture");
        let process = ServerProcess::new(ProcessGuard::new(
            child,
            ProcessCleanupMode::ProcessGroupTree,
        ));
        let (parent, helper) = fixture_pids(root.path());
        assert!(wait_for_fixture(|| process.child_exited()));
        assert!(fixture_process_running(helper));
        assert!(
            process.take_pipes().is_err(),
            "stdin was deliberately not piped"
        );
        drop(process);
        assert_eq!(fixture_process_status(parent), None);
        assert!(wait_for_fixture(|| !fixture_process_running(helper)));
    }

    #[cfg(unix)]
    #[test]
    fn observing_root_exit_retires_the_connection_before_pipe_eof() {
        let root = tempfile::tempdir().expect("tempdir");
        let (client, parent, helper) = tree_fixture(root.path(), true);
        assert!(client.is_alive(), "inherited output prevents reader EOF");
        assert!(client.child_exited());
        assert!(!client.is_alive());
        assert!(wait_for_fixture(|| workers_finished(&client)));
        client.finish_cleanup().expect("owned cleanup");
        assert_eq!(fixture_process_status(parent), None);
        assert!(wait_for_fixture(|| !fixture_process_running(helper)));
    }

    #[cfg(unix)]
    #[test]
    fn killing_a_stalled_stdin_write_stops_and_joins_all_workers() {
        let root = tempfile::tempdir().expect("tempdir");
        let (client, parent, helper) = tree_fixture(root.path(), false);
        let (_, response) = client
            .request(
                "test/large",
                serde_json::json!({"data":"x".repeat(8 * 1024 * 1024)}),
            )
            .expect("bounded queue admits the frame");
        // Neither process reads stdin; this frame is larger than pipe capacity.
        // Observe the real writer claiming it, rather than timing a scheduler.
        assert!(wait_for_fixture(
            || lock(&client.writer).has_in_flight_frame()
        ));
        client.kill();
        assert!(matches!(
            response.try_recv(),
            Ok(Err(TransportError::Closed(_)))
        ));
        assert!(
            wait_for_fixture(|| workers_finished(&client)),
            "stdin worker stayed blocked"
        );
        client.finish_cleanup().expect("all workers joined");
        assert_eq!(fixture_process_status(parent), None);
        assert!(wait_for_fixture(|| !fixture_process_running(helper)));
    }

    #[cfg(unix)]
    #[test]
    fn malformed_output_retires_a_retained_client_and_its_process_tree() {
        let root = tempfile::tempdir().expect("tempdir");
        let script = "sleep 30 <&0 &\nprintf '%s %s\\n' \"$$\" \"$!\" > pids\nwhile [ ! -f release ]; do sleep 0.01; done\nprintf 'Content-Length: 1\\r\\n\\r\\n?'\nwait\n";
        let client = JsonRpcClient::spawn(
            "/bin/sh",
            &["-c".to_string(), script.to_string()],
            &[],
            root.path(),
        )
        .expect("malformed server");
        let (parent, helper) = fixture_pids(root.path());
        let (_, response) = client
            .request("test/pending", Value::Null)
            .expect("request");
        std::fs::write(root.path().join("release"), b"").expect("release bad frame");
        assert!(matches!(
            response.recv_timeout(Duration::from_secs(5)),
            Ok(Err(TransportError::Closed(_)))
        ));
        assert!(!client.is_alive());
        assert!(
            wait_for_fixture(|| workers_finished(&client)),
            "failure left pipe workers alive"
        );
        client.finish_cleanup().expect("failure cleanup");
        assert_eq!(fixture_process_status(parent), None);
        assert!(wait_for_fixture(|| !fixture_process_running(helper)));
    }

    #[cfg(unix)]
    #[test]
    fn fragmented_headers_and_bodies_survive_nonblocking_waits_without_replay() {
        let root = tempfile::tempdir().expect("tempdir");
        let first =
            encode_frame(&serde_json::json!({"jsonrpc":"2.0", "id":1, "result":"snowman ☃"}));
        let second = encode_frame(&serde_json::json!({"jsonrpc":"2.0", "id":2, "result":"second"}));
        let body_start = find_subslice(&first, b"\r\n\r\n").expect("headers") + 4;
        std::fs::write(root.path().join("first"), &first[..10]).expect("first fragment");
        std::fs::write(root.path().join("middle"), &first[10..body_start + 9])
            .expect("middle fragment");
        let mut final_bytes = first[body_start + 9..].to_vec();
        final_bytes.extend_from_slice(&second);
        std::fs::write(root.path().join("last"), final_bytes).expect("final frames");
        let script = "cat first\nprintf ready > stage1\nwhile [ ! -f release1 ]; do sleep 0.01; done\ncat middle\nprintf ready > stage2\nwhile [ ! -f release2 ]; do sleep 0.01; done\ncat last\nexec sleep 30\n";
        let client = JsonRpcClient::spawn(
            "/bin/sh",
            &["-c".to_string(), script.to_string()],
            &[],
            root.path(),
        )
        .expect("fragmented server");
        let (_, first_response) = client
            .request("test/first", Value::Null)
            .expect("first request");
        let (_, second_response) = client
            .request("test/second", Value::Null)
            .expect("second request");
        assert!(wait_for_fixture(|| root.path().join("stage1").exists()));
        assert!(matches!(
            first_response.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        std::fs::write(root.path().join("release1"), b"").expect("finish headers");
        assert!(wait_for_fixture(|| root.path().join("stage2").exists()));
        assert!(matches!(
            first_response.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        std::fs::write(root.path().join("release2"), b"").expect("finish bodies");
        assert_eq!(
            first_response
                .recv_timeout(Duration::from_secs(5))
                .expect("first delivered")
                .expect("first success"),
            "snowman ☃"
        );
        assert_eq!(
            second_response
                .recv_timeout(Duration::from_secs(5))
                .expect("second delivered")
                .expect("second success"),
            "second"
        );
        assert!(lock(&client.pending).is_empty());
        assert!(client.is_alive());
    }

    #[cfg(unix)]
    #[test]
    fn final_client_drop_inside_a_server_callback_does_not_join_its_own_reader() {
        let root = tempfile::tempdir().expect("tempdir");
        let request = encode_frame(
            &serde_json::json!({"jsonrpc":"2.0", "id":"peer", "method":"test/dropOwner"}),
        );
        std::fs::write(root.path().join("callback"), request).expect("callback frame");
        let script = "sleep 30 <&0 &\nprintf '%s %s\\n' \"$$\" \"$!\" > pids\nwhile [ ! -f release ]; do sleep 0.01; done\ncat callback\nwait\n";
        let client = Arc::new(
            JsonRpcClient::spawn(
                "/bin/sh",
                &["-c".to_string(), script.to_string()],
                &[],
                root.path(),
            )
            .expect("callback server"),
        );
        let (parent, helper) = fixture_pids(root.path());
        let owner = Arc::new(Mutex::new(Some(Arc::clone(&client))));
        let weak = Arc::downgrade(&client);
        let (completed, completion) = std::sync::mpsc::sync_channel(1);
        client.set_server_request_handler(Arc::new(move |_, _| {
            let last_owner = lock(&owner).take().expect("final client owner");
            drop(last_owner);
            completed.send(()).expect("callback completed");
            None
        }));
        drop(client);
        std::fs::write(root.path().join("release"), b"").expect("release callback");
        completion
            .recv_timeout(Duration::from_secs(5))
            .expect("callback must not self-join");
        assert!(weak.upgrade().is_none());
        assert_eq!(fixture_process_status(parent), None);
        assert!(wait_for_fixture(|| !fixture_process_running(helper)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn escaped_pipe_holder_cannot_keep_retired_workers_alive() {
        struct EscapeCleanup(u32);
        impl Drop for EscapeCleanup {
            fn drop(&mut self) {
                if let Ok(raw) = i32::try_from(self.0)
                    && let Some(group) = rustix::process::Pid::from_raw(raw)
                {
                    let _ =
                        rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
                }
            }
        }

        let root = tempfile::tempdir().expect("tempdir");
        let script = "setsid /bin/sh -c 'printf \"%s\" \"$$\" > escaped; exec sleep 30' <&0 &\nwhile [ ! -s escaped ]; do sleep 0.01; done\nexit 0\n";
        let client = JsonRpcClient::spawn(
            "/bin/sh",
            &["-c".to_string(), script.to_string()],
            &[],
            root.path(),
        )
        .expect("escaped pipe server");
        let mut escaped = None;
        assert!(wait_for_fixture(|| {
            escaped = std::fs::read_to_string(root.path().join("escaped"))
                .ok()
                .and_then(|text| text.parse::<u32>().ok());
            escaped.is_some()
        }));
        let escaped = escaped.expect("escaped helper identity");
        let cleanup = EscapeCleanup(escaped);
        assert!(wait_for_fixture(|| client.process.child_exited()));
        assert!(fixture_process_running(escaped));
        client.kill();
        assert!(
            wait_for_fixture(|| workers_finished(&client)),
            "escaped descriptors pinned a worker"
        );
        client
            .finish_cleanup()
            .expect("workers join without pipe EOF");
        assert!(
            fixture_process_running(escaped),
            "fixture must keep its descriptors open throughout cleanup"
        );
        drop(cleanup);
        assert!(wait_for_fixture(|| !fixture_process_running(escaped)));
    }
}
