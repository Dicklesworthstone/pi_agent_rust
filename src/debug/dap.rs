//! Content-Length-framed DAP over owned adapter stdio or loopback TCP.
//! Dedicated, bounded writer/reader lanes keep blocking I/O off the reactor.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver as StdReceiver, SyncSender as StdSyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::Value;

use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::lsp::jsonrpc::{await_completion, encode_frame, read_frame_with_scratch};

mod delve;

const MAX_PENDING: usize = 32;
const MAX_OUTBOUND: usize = 2 * 1024 * 1024;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const QUEUED: u8 = 0;
const WRITING: u8 = 1;
const SENT: u8 = 2;
const CANCELLED: u8 = 3;

type OutputTail = Arc<Mutex<crate::lsp::jsonrpc::PublicTailBuffer>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn tool_err(code: &str, message: impl Into<String>) -> Error {
    Error::tool("debug", format!("[{code}] {}", message.into()))
}

#[derive(Debug, Clone)]
pub struct DapEvent {
    pub event: String,
    pub body: Value,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DapError {
    Adapter { command: String, message: String },
    Transport(String),
    Timeout { timeout_ms: u64 },
    Cancelled,
}

impl DapError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Adapter { .. } => "DAP_ADAPTER_ERROR",
            Self::Transport(_) => "DAP_TRANSPORT",
            Self::Timeout { .. } => "DAP_TIMEOUT",
            Self::Cancelled => "DAP_CANCELLED",
        }
    }

    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Adapter { command, message } => format!("{command} failed: {message}"),
            Self::Transport(reason) => format!("transport: {reason}"),
            Self::Timeout { timeout_ms } => format!("timed out after {timeout_ms} ms"),
            Self::Cancelled => "cancelled by ambient context".to_string(),
        }
    }
}

impl From<DapError> for Error {
    fn from(err: DapError) -> Self {
        Self::tool("debug", format!("[{}] {}", err.code(), err.message()))
    }
}

impl std::fmt::Display for DapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

type PendingMap = Mutex<HashMap<u64, StdSyncSender<std::result::Result<Value, DapError>>>>;

struct Lifetime {
    child: Mutex<crate::tools::ProcessGuard>,
    // Shutting down a clone wakes both TCP I/O lanes, even if the peer never
    // closes its end. Process teardown alone is insufficient for TCP streams.
    socket: Option<TcpStream>,
    alive: AtomicBool,
    pending: Arc<PendingMap>,
}

impl Lifetime {
    fn close(&self, reason: &str) {
        if self.alive.swap(false, Ordering::SeqCst) {
            if let Some(socket) = &self.socket {
                let _ = socket.shutdown(Shutdown::Both);
            }
            let waiting = std::mem::take(&mut *lock(&self.pending));
            for (_, sender) in waiting {
                let _ = sender.try_send(Err(DapError::Transport(reason.to_string())));
            }
            let _ = lock(&self.child).kill();
        }
    }
}

struct Frame {
    bytes: Vec<u8>,
    phase: Arc<AtomicU8>,
}

struct PendingLease {
    seq: u64,
    life: Arc<Lifetime>,
    phase: Arc<AtomicU8>,
    answered: bool,
}

impl Drop for PendingLease {
    fn drop(&mut self) {
        lock(&self.life.pending).remove(&self.seq);
        if !self.answered
            && self
                .phase
                .compare_exchange(QUEUED, CANCELLED, Ordering::SeqCst, Ordering::SeqCst)
                == Err(WRITING)
        {
            self.life
                .close("request cancelled during a write; adapter connection retired");
        }
    }
}

// Reverse-request refusals share the bounded writer lane. The reader never
// blocks attempting a synchronous write to an unresponsive adapter.
struct ReplyWriter(StdSyncSender<Frame>);
impl Write for ReplyWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_OUTBOUND {
            return Err(std::io::Error::other("DAP reply exceeds outbound limit"));
        }
        self.0
            .try_send(Frame {
                bytes: bytes.to_vec(),
                phase: Arc::new(AtomicU8::new(QUEUED)),
            })
            .map_err(|_| std::io::Error::other("DAP writer queue unavailable"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// Startup keeps these handles local until all required pipes/socket clones
// exist. Dropping a failed or cancelled startup also drops its process guard.
struct AdapterIo {
    child: crate::tools::ProcessGuard,
    input: Box<dyn Write + Send>,
    output: Box<dyn Read + Send>,
    socket: Option<TcpStream>,
    tail: OutputTail,
}

/// The session owns the adapter; each request owns its pending wait and frame.
/// Blocking OS I/O and process cleanup are not hard real-time operations.
pub struct DapTransport {
    life: Arc<Lifetime>,
    writer: StdSyncSender<Frame>,
    pending: Arc<PendingMap>,
    next_seq: Arc<AtomicU64>,
    stderr_tail: OutputTail,
    event_rx: Mutex<StdReceiver<DapEvent>>,
    frames_read: Arc<AtomicU64>,
}

impl DapTransport {
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<Self> {
        Self::spawn_inner(command, args, env, cwd, false)
    }

    pub fn spawn_with_env(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<Self> {
        Self::spawn_inner(command, args, env, cwd, true)
    }

    fn spawn_inner(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
        explicit_env: bool,
    ) -> Result<Self> {
        let owner = AgentCx::for_current_or_request();
        let mut cmd = std::process::Command::new(command);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if explicit_env {
            cmd.env_clear();
        }
        cmd.envs(
            env.iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
        cmd.env_remove("CARGO_TARGET_DIR");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }
        let child = owner.process().spawn_checked(&mut cmd).map_err(|err| {
            tool_err(
                "DAP_ADAPTER_MISSING",
                format!("failed to spawn debug adapter {command:?}: {err}"),
            )
        })?;
        // spawn_checked already owns an atomic Windows Job registration.
        // Reattaching would replace and close the Job containing the adapter.
        let mut child = crate::tools::ProcessGuard::new(
            child,
            crate::tools::ProcessCleanupMode::ProcessGroupTree,
        );
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.take_stdin(), child.take_stdout(), child.take_stderr())
        else {
            let _ = child.kill();
            return Err(tool_err("DAP_TRANSPORT", "adapter pipes unavailable"));
        };
        owner
            .checkpoint()
            .map_err(|_| tool_err("DAP_CANCELLED", "cancelled during adapter spawn"))?;
        let tail = Arc::new(Mutex::new(crate::lsp::jsonrpc::PublicTailBuffer::new()));
        spawn_output_pump(stderr, Arc::clone(&tail));
        Ok(Self::from_io(AdapterIo {
            child,
            input: Box::new(stdin),
            output: Box::new(stdout),
            socket: None,
            tail,
        }))
    }

    fn from_io(io: AdapterIo) -> Self {
        let pending: Arc<PendingMap> = Arc::new(Mutex::new(HashMap::new()));
        let life = Arc::new(Lifetime {
            child: Mutex::new(io.child),
            socket: io.socket,
            alive: AtomicBool::new(true),
            pending: Arc::clone(&pending),
        });
        let next_seq = Arc::new(AtomicU64::new(1));
        let (event_tx, event_rx) = std::sync::mpsc::sync_channel(512);
        let (writer, writes) = std::sync::mpsc::sync_channel::<Frame>(MAX_PENDING);
        let frames_read = Arc::new(AtomicU64::new(0));
        spawn_writer(io.input, writes, Arc::clone(&life));
        spawn_reader(
            io.output,
            Arc::clone(&life),
            Arc::clone(&io.tail),
            writer.clone(),
            Arc::clone(&next_seq),
            Arc::clone(&frames_read),
            event_tx,
        );
        Self {
            life,
            writer,
            pending,
            next_seq,
            stderr_tail: io.tail,
            event_rx: Mutex::new(event_rx),
            frames_read,
        }
    }

    #[must_use]
    pub fn frames_read(&self) -> u64 {
        self.frames_read.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn is_alive(&self) -> bool {
        if !self.life.alive.load(Ordering::SeqCst) {
            return false;
        }
        let status = lock(&self.life.child).try_wait_child();
        if !matches!(status, Ok(None)) {
            self.life
                .close("adapter process exited or could not be queried");
        }
        self.life.alive.load(Ordering::SeqCst)
    }

    pub async fn request(
        &self,
        command: &str,
        arguments: Value,
        timeout: Duration,
    ) -> std::result::Result<Value, DapError> {
        let owner = AgentCx::for_current_or_request();
        owner.checkpoint().map_err(|_| DapError::Cancelled)?;
        if !owner.capabilities().io || !owner.capabilities().time {
            return Err(DapError::Transport(
                "DAP request requires I/O and timer capabilities".into(),
            ));
        }
        if !self.is_alive() {
            return Err(DapError::Transport("adapter is not running".into()));
        }
        if command.is_empty() || command.len() > 256 || timeout.is_zero() {
            return Err(DapError::Transport("invalid DAP command or timeout".into()));
        }
        let seq = next_sequence(&self.next_seq)?;
        let bytes = encode_frame(
            &serde_json::json!({"seq":seq,"type":"request","command":command,"arguments":arguments}),
        );
        if bytes.len() > MAX_OUTBOUND {
            return Err(DapError::Transport(
                "outbound DAP frame exceeds 2 MiB".into(),
            ));
        }
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        {
            let mut pending = lock(&self.pending);
            if !self.life.alive.load(Ordering::SeqCst) {
                return Err(DapError::Transport("adapter closed before dispatch".into()));
            }
            if pending.len() >= MAX_PENDING {
                return Err(DapError::Transport("too many pending DAP requests".into()));
            }
            pending.insert(seq, sender);
        }
        let phase = Arc::new(AtomicU8::new(QUEUED));
        let mut lease = PendingLease {
            seq,
            life: Arc::clone(&self.life),
            phase: Arc::clone(&phase),
            answered: false,
        };
        self.writer
            .try_send(Frame { bytes, phase })
            .map_err(|_| DapError::Transport("DAP writer queue is full or closed".into()))?;
        let outcome = await_completion(receiver, timeout, || {}).await;
        lease.answered = outcome.is_ok();
        match outcome {
            Ok(result) => result,
            Err(crate::lsp::jsonrpc::CompletionWaitError::Timeout) => {
                if lease.phase.load(Ordering::SeqCst) != SENT {
                    self.life
                        .close("DAP writer did not dispatch before the request deadline");
                }
                Err(DapError::Timeout {
                    timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                })
            }
            Err(crate::lsp::jsonrpc::CompletionWaitError::Cancelled) => Err(DapError::Cancelled),
            Err(crate::lsp::jsonrpc::CompletionWaitError::Closed) => {
                Err(DapError::Transport("completion channel closed".into()))
            }
        }
    }

    pub fn drain_events(&self) -> Vec<DapEvent> {
        let receiver = lock(&self.event_rx);
        receiver.try_iter().collect()
    }

    #[must_use]
    pub fn stderr_tail(&self) -> String {
        lock(&self.stderr_tail).tail()
    }

    pub fn kill(&self) {
        self.life.close("adapter terminated by the client");
    }
}

impl Drop for DapTransport {
    fn drop(&mut self) {
        self.kill();
    }
}

fn next_sequence(sequence: &AtomicU64) -> std::result::Result<u64, DapError> {
    sequence
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
            value.checked_add(1)
        })
        .map_err(|_| DapError::Transport("DAP sequence numbers exhausted".into()))
}

fn spawn_output_pump(mut reader: impl Read + Send + 'static, tail: OutputTail) {
    std::thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        while let Ok(count) = reader.read(&mut buf) {
            if count == 0 {
                break;
            }
            lock(&tail).push(&String::from_utf8_lossy(&buf[..count]));
        }
    });
}

fn spawn_writer(
    mut writer: impl Write + Send + 'static,
    writes: StdReceiver<Frame>,
    life: Arc<Lifetime>,
) {
    std::thread::spawn(move || {
        while life.alive.load(Ordering::SeqCst) {
            let frame = match writes.recv_timeout(Duration::from_millis(100)) {
                Ok(frame) => frame,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if frame
                .phase
                .compare_exchange(QUEUED, WRITING, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                continue;
            }
            if !life.alive.load(Ordering::SeqCst) {
                break;
            }
            if writer
                .write_all(&frame.bytes)
                .and_then(|()| writer.flush())
                .is_err()
            {
                life.close("adapter write failed");
                break;
            }
            frame.phase.store(SENT, Ordering::SeqCst);
        }
    });
}

fn spawn_reader(
    output: impl Read + Send + 'static,
    life: Arc<Lifetime>,
    tail: OutputTail,
    writer: StdSyncSender<Frame>,
    sequence: Arc<AtomicU64>,
    frames: Arc<AtomicU64>,
    events: StdSyncSender<DapEvent>,
) {
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(output);
        let mut scratch = Vec::new();
        let writer = Mutex::new(ReplyWriter(writer));
        let reason = loop {
            match read_frame_with_scratch(&mut reader, &mut scratch) {
                Ok(Some(message)) => {
                    frames.fetch_add(1, Ordering::SeqCst);
                    if !dispatch(&message, &life.pending, &events, &tail, &writer, &sequence) {
                        break "DAP event/reply buffer overflow or invalid control message";
                    }
                }
                Ok(None) => break "adapter closed DAP stream (EOF)",
                Err(_) => break "invalid or truncated DAP frame",
            }
        };
        life.close(reason);
    });
}

fn dispatch<W: Write>(
    message: &Value,
    pending: &PendingMap,
    events: &StdSyncSender<DapEvent>,
    tail: &Mutex<crate::lsp::jsonrpc::PublicTailBuffer>,
    writer: &Mutex<W>,
    sequence: &AtomicU64,
) -> bool {
    match message.get("type").and_then(Value::as_str) {
        Some("response") => {
            let Some(seq) = message["request_seq"].as_u64() else {
                return false;
            };
            let sender = lock(pending).remove(&seq);
            if let Some(sender) = sender {
                let result = if message["success"] == true {
                    Ok(message.get("body").cloned().unwrap_or(Value::Null))
                } else {
                    Err(DapError::Adapter {
                        command: message["command"].as_str().unwrap_or("").to_string(),
                        message: message["message"]
                            .as_str()
                            .unwrap_or("adapter reported failure")
                            .to_string(),
                    })
                };
                let _ = sender.try_send(result);
            }
        }
        Some("event") => {
            let Some(event) = message["event"].as_str().filter(|event| !event.is_empty()) else {
                return false;
            };
            let body = message.get("body").cloned().unwrap_or(Value::Null);
            if event == "output" {
                lock(tail).push(body["output"].as_str().unwrap_or(""));
                return true;
            }
            if event.len() > 256
                || serde_json::to_vec(&body).map_or(true, |bytes| bytes.len() > MAX_EVENT_BYTES)
            {
                return false;
            }
            if events
                .try_send(DapEvent {
                    event: event.to_string(),
                    body,
                })
                .is_err()
            {
                return false;
            }
        }
        Some("request") => {
            let Some(request_seq) = message["seq"].as_u64() else {
                return false;
            };
            let Some(command) = message["command"]
                .as_str()
                .filter(|command| command.len() <= 256)
            else {
                return false;
            };
            let Ok(seq) = next_sequence(sequence) else {
                return false;
            };
            let bytes = encode_frame(&serde_json::json!({
                "seq":seq,"type":"response","request_seq":request_seq,"command":command,
                "success":false,"message":"pi_agent_rust declines reverse requests (no terminal host)"
            }));
            let mut writer = lock(writer);
            if writer
                .write_all(&bytes)
                .and_then(|()| writer.flush())
                .is_err()
            {
                return false;
            }
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod reliability;
#[cfg(test)]
mod tests;
