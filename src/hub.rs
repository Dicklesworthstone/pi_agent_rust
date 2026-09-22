//! Hub-style process supervision (bd-cv653.5.4).
//!
//! Long-running services, watchers, REPLs, and debuggers live here instead
//! of timeout-hacked `bash` calls. Every service spawns on a PTY (stdin
//! stays writable for `send`), output streams to a raw artifact log plus
//! a byte- and line-bounded text ring, and readiness is *observed* — a
//! `ready.log` regex and/or a `ready.port` TCP accept must both pass within
//! the timeout before `start` returns.
//!
//! Lifecycle: session-scoped by default (killed at the main shutdown
//! chokepoint, same as background jobs); `detached: true` services survive
//! session exit and are re-discovered from the state file under the hub
//! artifact dir.
//!
//! The `hub` tool's `jobs` action group wraps the background-jobs registry
//! (bd-cv653.3.10); the `messaging` action group lands with the agent-hub
//! registry (bd-cv653.5.3).

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::{Error, Result};

/// Tool-result schema tag for service descriptors (stable audit contract).
pub const SERVICE_SCHEMA: &str = "pi.hub.service.v1";

/// Default readiness budget when the caller passes none.
const DEFAULT_READY_TIMEOUT_SECS: u64 = 30;

/// Bounded line ring kept per service for `logs` cursors.
const RING_LINE_CAP: usize = 10_000;

/// Bound retained text independently of newline frequency. Raw artifact bytes
/// are unaffected; an oversized line retains its newest UTF-8-safe suffix.
const RING_BYTE_CAP: usize = 1024 * 1024;
const RING_LINE_BYTE_CAP: usize = 16 * 1024;
const TRUNCATED_LINE_PREFIX: &str = "[...truncated...] ";

/// Grace window between TERM and KILL on stop, mirroring the bash tool.
const TERMINATE_GRACE: Duration = Duration::from_secs(3);

/// Keep service identifiers portable and bounded before deriving artifact
/// names from them.
const MAX_SERVICE_NAME_BYTES: usize = 128;

/// Readiness gates for `start`. Both supplied gates MUST pass.
#[derive(Debug, Clone, Default)]
pub struct ReadySpec {
    /// Regex that must match retained service output (up to 1 MiB of completed
    /// lines and a 16 KiB tail per line, including the current partial line).
    /// A match is latched at read time; subsequent eviction cannot undo it.
    pub log: Option<String>,
    /// TCP port on 127.0.0.1 that must accept a connection.
    pub port: Option<u16>,
    /// Overall readiness budget in seconds (default 30).
    pub timeout_secs: Option<u64>,
}

/// Retained launch spec (restart reuses it verbatim).
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub ready: Option<ReadySpec>,
    pub detached: bool,
}

/// Live service state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ServiceStatus {
    Starting,
    Running,
    Exited,
    Killed,
    Failed,
}

impl ServiceStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Killed => "killed",
            Self::Failed => "failed",
        }
    }

    const fn live(self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

/// One ring entry: a completed output line with its cursor index.
struct Ring {
    lines: VecDeque<String>,
    /// Distinguish synthetic truncation markers from bytes the service wrote.
    truncated_lines: VecDeque<bool>,
    next_index: u64,
    /// Partial line currently being assembled (not yet cursor-addressable).
    partial: String,
    partial_truncated: bool,
    /// Text bytes in completed lines; the partial line has a separate bound.
    bytes: usize,
    cap: usize,
    ready_log: Option<regex::Regex>,
    ready_log_passed: bool,
}

impl Ring {
    fn new(cap: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(cap.min(256)),
            truncated_lines: VecDeque::with_capacity(cap.min(256)),
            next_index: 0,
            partial: String::new(),
            partial_truncated: false,
            bytes: 0,
            cap,
            ready_log: None,
            ready_log_passed: true,
        }
    }

    fn watch_readiness(&mut self, pattern: Option<regex::Regex>) {
        self.ready_log_passed = pattern.as_ref().is_none_or(|re| re.is_match(&self.text()));
        self.ready_log = pattern;
    }

    fn push_chunk(&mut self, chunk: &str) {
        for fragment in chunk.split_inclusive('\n') {
            if let Some(text) = fragment.strip_suffix('\n') {
                self.push_partial(text);
                self.finish_line();
            } else {
                self.push_partial(fragment);
            }
        }
        // Observe at the stream seam, before a later burst can evict the
        // readiness marker. Once observed, the log gate remains satisfied.
        if !self.ready_log_passed {
            self.ready_log_passed = self
                .ready_log
                .as_ref()
                .is_some_and(|re| re.is_match(&self.text()));
        }
    }

    fn push_partial(&mut self, text: &str) {
        let limit = RING_LINE_BYTE_CAP - TRUNCATED_LINE_PREFIX.len();
        let total = self.partial.len().saturating_add(text.len());
        if total > limit {
            self.partial_truncated = true;
            let discard = total - limit;
            if discard >= self.partial.len() {
                let mut start = discard - self.partial.len();
                while !text.is_char_boundary(start) {
                    start += 1;
                }
                self.partial.clear();
                self.partial.push_str(&text[start..]);
                return;
            }
            let mut start = discard;
            while !self.partial.is_char_boundary(start) {
                start += 1;
            }
            drop(self.partial.drain(..start));
        }
        self.partial.push_str(text);
    }

    fn finish_line(&mut self) {
        let mut line = std::mem::take(&mut self.partial);
        line.truncate(line.trim_end_matches('\r').len());
        let truncated = std::mem::take(&mut self.partial_truncated);
        if truncated {
            line.insert_str(0, TRUNCATED_LINE_PREFIX);
        }
        // Cursors count source lines, not retained bytes or transport chunks.
        self.next_index = self.next_index.saturating_add(1);
        if self.cap == 0 {
            return;
        }
        while self.lines.len() >= self.cap || self.bytes + line.len() > RING_BYTE_CAP {
            let Some(evicted) = self.lines.pop_front() else {
                break;
            };
            self.truncated_lines.pop_front();
            self.bytes -= evicted.len();
        }
        self.bytes += line.len();
        self.lines.push_back(line);
        self.truncated_lines.push_back(truncated);
    }

    /// Only the output reader knows when all trailing bytes have arrived.
    fn finish_partial(&mut self) {
        if !self.partial.is_empty() || self.partial_truncated {
            self.finish_line();
        }
    }

    /// Readiness sees only service text, never our truncation annotations.
    fn text(&self) -> String {
        let mut text = String::with_capacity(self.bytes + self.lines.len() + self.partial.len());
        for (line, truncated) in self.lines.iter().zip(&self.truncated_lines) {
            text.push_str(if *truncated {
                line.strip_prefix(TRUNCATED_LINE_PREFIX).unwrap_or(line)
            } else {
                line
            });
            text.push('\n');
        }
        text.push_str(&self.partial);
        text
    }

    /// Lines with index >= `since`, plus the current head cursor.
    fn since(&self, since: u64) -> (Vec<String>, u64) {
        let oldest = self.next_index.saturating_sub(self.lines.len() as u64);
        let skip = usize::try_from(since.saturating_sub(oldest)).unwrap_or(usize::MAX);
        let lines = self.lines.iter().skip(skip).cloned().collect();
        (lines, self.next_index)
    }
}

struct ServiceEntry {
    spec: LaunchSpec,
    status: ServiceStatus,
    pid: Option<u32>,
    started_ms: i64,
    exit_code: Option<i32>,
    log_path: PathBuf,
    ring: Arc<Mutex<Ring>>,
    /// Cached PTY writer, taken once before spawn —
    /// portable-pty's `UnixMasterWriter::drop` sends `\n`+VEOF, so caching
    /// the writer is what keeps the child's stdin open across sends.
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
}

/// Serializable service descriptor.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSnapshot {
    pub schema: String,
    pub name: String,
    pub command: String,
    pub cwd: String,
    pub pid: Option<u32>,
    pub status: String,
    pub started_ms: i64,
    pub exit_code: Option<i32>,
    pub log_path: String,
    pub detached: bool,
    pub ready: bool,
}

impl ServiceSnapshot {
    fn from_entry(entry: &ServiceEntry) -> Self {
        Self {
            schema: SERVICE_SCHEMA.to_string(),
            name: entry.spec.name.clone(),
            command: format!("{} {}", entry.spec.program, entry.spec.args.join(" ")),
            cwd: entry.spec.cwd.display().to_string(),
            pid: entry.pid,
            status: entry.status.as_str().to_string(),
            started_ms: entry.started_ms,
            exit_code: entry.exit_code,
            log_path: entry.log_path.display().to_string(),
            detached: entry.spec.detached,
            ready: entry.status == ServiceStatus::Running,
        }
    }
}

/// One page of service log lines with the cursor for the next read.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogPage {
    pub schema: String,
    pub name: String,
    pub lines: Vec<String>,
    /// Opaque cursor for the next `logs` call (returns newer lines only).
    pub cursor: u64,
    pub status: String,
}

#[derive(Default)]
struct ServiceRegistry {
    services: HashMap<String, ServiceEntry>,
}

impl ServiceRegistry {
    /// Reserve before any filesystem or process side effects. The ring's Arc
    /// identity is also the incarnation token, so stopped names can be reused
    /// without letting an old monitor or readiness waiter mutate the new run.
    fn reserve(
        &mut self,
        spec: &LaunchSpec,
        ring: &Arc<Mutex<Ring>>,
        log_path: &std::path::Path,
    ) -> Result<()> {
        if let Some(existing) = self.services.get(&spec.name)
            && existing.status.live()
        {
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NAME_TAKEN: a live service named '{}' already exists (pid {:?})",
                    spec.name, existing.pid
                ),
            ));
        }
        self.services.insert(
            spec.name.clone(),
            ServiceEntry {
                spec: spec.clone(),
                status: ServiceStatus::Starting,
                pid: None,
                started_ms: now_ms(),
                exit_code: None,
                log_path: log_path.to_path_buf(),
                ring: Arc::clone(ring),
                writer: Arc::new(Mutex::new(None)),
            },
        );
        Ok(())
    }

    fn current(&self, name: &str, ring: &Arc<Mutex<Ring>>) -> Option<&ServiceEntry> {
        self.services
            .get(name)
            .filter(|entry| Arc::ptr_eq(&entry.ring, ring))
    }

    fn current_mut(&mut self, name: &str, ring: &Arc<Mutex<Ring>>) -> Option<&mut ServiceEntry> {
        self.services
            .get_mut(name)
            .filter(|entry| Arc::ptr_eq(&entry.ring, ring))
    }

    fn mark_ready(&mut self, name: &str, ring: &Arc<Mutex<Ring>>) -> Result<ServiceSnapshot> {
        let entry = self
            .current_mut(name, ring)
            .ok_or_else(|| stale_service(name))?;
        if !entry.status.live() {
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NOT_READY: service '{name}' is {} before readiness was observed",
                    entry.status.as_str()
                ),
            ));
        }
        entry.status = ServiceStatus::Running;
        Ok(ServiceSnapshot::from_entry(entry))
    }

    fn settle(&mut self, name: &str, ring: &Arc<Mutex<Ring>>, code: i32) -> bool {
        let Some(entry) = self.current_mut(name, ring) else {
            return false;
        };
        if entry.status.live() {
            entry.status = if code == 0 {
                ServiceStatus::Exited
            } else {
                ServiceStatus::Failed
            };
        }
        entry.exit_code = Some(code);
        entry.pid = None;
        true
    }

    /// A stale timeout has no authority over the current service's PID.
    fn time_out(&mut self, name: &str, ring: &Arc<Mutex<Ring>>) -> Option<u32> {
        let entry = self.current_mut(name, ring)?;
        if !entry.status.live() {
            return None;
        }
        entry.status = ServiceStatus::Killed;
        entry.pid
    }
}

fn stale_service(name: &str) -> Error {
    Error::tool(
        "hub",
        format!("PI_HUB_STALE_SERVICE: service '{name}' was replaced during startup"),
    )
}

/// A failed setup settles only its own reservation. This guard is created
/// before the child guard, so child cleanup runs before the reservation settles.
struct PendingService {
    name: String,
    ring: Arc<Mutex<Ring>>,
    armed: bool,
}

impl PendingService {
    fn reserve(
        spec: &LaunchSpec,
        ring: &Arc<Mutex<Ring>>,
        log_path: &std::path::Path,
    ) -> Result<Self> {
        let mut reg = registry().lock().map_err(|_| registry_err())?;
        reg.reserve(spec, ring, log_path)?;
        drop(reg);
        Ok(Self {
            name: spec.name.clone(),
            ring: Arc::clone(ring),
            armed: true,
        })
    }
}

impl Drop for PendingService {
    fn drop(&mut self) {
        if self.armed
            && let Ok(mut reg) = registry().lock()
            && reg.settle(&self.name, &self.ring, -1)
        {
            persist_detached_state(&reg);
        }
    }
}

/// Own the child even across thread-start errors or unwinding. On successful
/// handoff the monitor consumes this guard by waiting; every other path kills
/// and reaps before dropping the handle.
struct ServiceChild {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reaped: bool,
}

impl ServiceChild {
    fn wait(mut self) -> i32 {
        loop {
            match self.child.wait() {
                Ok(status) => {
                    self.reaped = true;
                    return i32::try_from(status.exit_code()).unwrap_or(-1);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return -1,
            }
        }
    }
}

impl Drop for ServiceChild {
    fn drop(&mut self) {
        if !self.reaped {
            crate::tools::kill_process_group_tree(self.child.process_id());
            let _ = self.child.kill();
            loop {
                match self.child.wait() {
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    _ => break,
                }
            }
        }
    }
}

struct SpawnedService {
    child: ServiceChild,
    master: Box<dyn portable_pty::MasterPty + Send>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
}

fn readiness_deadline(now: Instant, budget: Duration) -> Result<Instant> {
    now.checked_add(budget).ok_or_else(|| {
        Error::validation("PI_HUB_INVALID_READY_TIMEOUT: readiness timeout is too large".to_string())
    })
}

fn create_service_log(path: &std::path::Path) -> Result<std::fs::File> {
    // Never truncate a previous run or follow an existing symlink, even if a
    // generated name collides. The path is returned in the service descriptor.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| Error::tool("hub", format!("Failed to create service log: {error}")))
}

fn registry() -> &'static Mutex<ServiceRegistry> {
    static REGISTRY: std::sync::LazyLock<Mutex<ServiceRegistry>> =
        std::sync::LazyLock::new(|| Mutex::new(ServiceRegistry::default()));
    &REGISTRY
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn hub_artifact_dir() -> PathBuf {
    crate::config::Config::global_dir()
        .join("tool-output-artifacts")
        .join("hub")
}

fn detached_state_path() -> PathBuf {
    hub_artifact_dir().join("detached-services.json")
}

fn registry_err() -> Error {
    Error::tool("hub", "hub registry poisoned".to_string())
}

fn validated_service_name(name: &str) -> Result<&str> {
    let is_portable = !name.is_empty()
        && name.len() <= MAX_SERVICE_NAME_BYTES
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    if !is_portable {
        return Err(Error::validation(format!(
            "PI_HUB_INVALID_NAME: service names must be 1-{MAX_SERVICE_NAME_BYTES} ASCII bytes containing only letters, digits, '.', '-', or '_'"
        )));
    }
    Ok(name)
}

/// Persist the detached-service roster (name, pid, log, spec) so a later
/// session can rediscover survivors.
fn persist_detached_state(reg: &ServiceRegistry) {
    #[derive(Serialize)]
    struct DetachedRecord {
        name: String,
        pid: Option<u32>,
        log_path: String,
        program: String,
        args: Vec<String>,
        cwd: String,
    }
    let records: Vec<DetachedRecord> = reg
        .services
        .values()
        .filter(|entry| entry.spec.detached && entry.status.live())
        .map(|entry| DetachedRecord {
            name: entry.spec.name.clone(),
            pid: entry.pid,
            log_path: entry.log_path.display().to_string(),
            program: entry.spec.program.clone(),
            args: entry.spec.args.clone(),
            cwd: entry.spec.cwd.display().to_string(),
        })
        .collect();
    let path = detached_state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&records) {
        let _ = std::fs::write(path, json);
    }
}

/// Spawn a service and block until readiness is observed (or the budget
/// expires). Both supplied gates must be observed. With no gates, a
/// successful spawn is acknowledged without resurrecting a later exit.
///
/// # Errors
/// `PI_HUB_NAME_TAKEN` for a duplicate live name; `PI_HUB_NOT_READY` when
/// the gates do not pass in time (the process is killed — no half-started
/// surprise daemons); tool errors for spawn failures.
#[allow(clippy::too_many_lines)]
#[allow(clippy::significant_drop_tightening)]
pub fn start(spec: &LaunchSpec) -> Result<ServiceSnapshot> {
    let name = validated_service_name(&spec.name)?.to_string();
    let ready = spec.ready.clone().unwrap_or_default();
    let has_gates = ready.log.is_some() || ready.port.is_some();
    let budget = Duration::from_secs(ready.timeout_secs.unwrap_or(DEFAULT_READY_TIMEOUT_SECS));
    let deadline = readiness_deadline(Instant::now(), budget)?;
    let log_regex = ready
        .log
        .as_deref()
        .map(|pattern| {
            regex::Regex::new(pattern)
                .map_err(|e| Error::validation(format!("Invalid ready.log regex '{pattern}': {e}")))
        })
        .transpose()?;
    if !spec.cwd.is_dir() {
        return Err(Error::tool(
            "hub",
            format!("Working directory is not a directory: {}", spec.cwd.display()),
        ));
    }

    let mut output = Ring::new(RING_LINE_CAP);
    output.watch_readiness(log_regex);
    let ring = Arc::new(Mutex::new(output));
    let log_dir = hub_artifact_dir();
    let log_path = log_dir.join(format!("{name}-{}.log", uuid::Uuid::new_v4()));
    let mut pending = PendingService::reserve(spec, &ring, &log_path)?;
    std::fs::create_dir_all(&log_dir)
        .map_err(|e| Error::tool("hub", format!("Failed to create hub artifact dir: {e}")))?;
    // All fallible artifact/PTY handle setup precedes child creation.
    let artifact = create_service_log(&log_path)?;
    let SpawnedService {
        child,
        master,
        reader,
        writer,
    } = spawn_pty(spec)?;
    let initial_snapshot = {
        let mut reg = registry().lock().map_err(|_| registry_err())?;
        let entry = reg
            .current_mut(&name, &ring)
            .ok_or_else(|| stale_service(&name))?;
        if !entry.status.live() {
            return Err(Error::tool(
                "hub",
                format!("PI_HUB_NOT_READY: service '{name}' was stopped during startup"),
            ));
        }
        entry.pid = child.child.process_id();
        entry.started_ms = now_ms();
        entry.writer = Arc::new(Mutex::new(Some(writer)));
        if !has_gates {
            entry.status = ServiceStatus::Running;
        }
        let snapshot = ServiceSnapshot::from_entry(entry);
        persist_detached_state(&reg);
        snapshot
    };

    let pump_ring = Arc::clone(&ring);
    std::thread::Builder::new()
        .name(format!("hub-output-{name}"))
        .spawn(move || pump_service_stream(reader, artifact, &pump_ring))
        .map_err(|error| {
            Error::tool("hub", format!("Failed to start service output pump: {error}"))
        })?;

    let monitor_name = name.clone();
    let monitor_ring = Arc::clone(&ring);
    std::thread::Builder::new()
        .name(format!("hub-monitor-{name}"))
        .spawn(move || {
            // Keep the PTY owner alive for the whole child lifetime, not just
            // the readiness call. Reader/writer handles need not own it.
            let _master = master;
            let code = child.wait();
            if let Ok(mut reg) = registry().lock()
                && reg.settle(&monitor_name, &monitor_ring, code)
            {
                persist_detached_state(&reg);
            }
        })
        .map_err(|error| {
            Error::tool("hub", format!("Failed to start service exit monitor: {error}"))
        })?;
    pending.armed = false;

    // No-gate starts acknowledge the publication above, even if a very short
    // command has since exited. Never resurrect its settled registry entry.
    if !has_gates {
        return Ok(initial_snapshot);
    }

    loop {
        let status = {
            let reg = registry().lock().map_err(|_| registry_err())?;
            reg.current(&name, &ring)
                .ok_or_else(|| stale_service(&name))?
                .status
        };
        if !status.live() {
            let tail = ring_tail(&ring, 20);
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NOT_READY: service '{name}' became {} before readiness was observed.\n\
                     Log tail:\n{tail}",
                    status.as_str()
                ),
            ));
        }
        let log_passed = ring.lock().map_err(|_| registry_err())?.ready_log_passed;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let port_passed = ready.port.is_none_or(|port| {
            if remaining.is_zero() {
                return false;
            }
            let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            std::net::TcpStream::connect_timeout(
                &address,
                remaining.min(Duration::from_millis(100)),
            )
            .is_ok()
        });
        if log_passed && port_passed && Instant::now() <= deadline {
            let mut reg = registry().lock().map_err(|_| registry_err())?;
            let snapshot = reg.mark_ready(&name, &ring)?;
            persist_detached_state(&reg);
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            let pid = {
                let mut reg = registry().lock().map_err(|_| registry_err())?;
                let pid = reg.time_out(&name, &ring);
                persist_detached_state(&reg);
                pid
            };
            crate::tools::kill_process_group_tree(pid);
            let tail = ring_tail(&ring, 20);
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NOT_READY: service '{name}' failed readiness within {}s \
                     (log gate passed: {log_passed}, port gate passed: {port_passed}). \
                     Startup was stopped.\nLog tail:\n{tail}",
                    budget.as_secs()
                ),
            ));
        }
        std::thread::sleep(
            Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn spawn_pty(spec: &LaunchSpec) -> Result<SpawnedService> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| Error::tool("hub", format!("Failed to allocate PTY: {e}")))?;

    let mut cmd = CommandBuilder::new(&spec.program);
    cmd.args(&spec.args);
    cmd.cwd(&spec.cwd);
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| Error::tool("hub", format!("Failed to clone PTY reader: {e}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| Error::tool("hub", format!("Failed to open PTY writer: {e}")))?;
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| Error::tool("hub", format!("Failed to spawn service: {e}")))?;
    drop(pair.slave);
    Ok(SpawnedService {
        child: ServiceChild {
            child,
            reaped: false,
        },
        master: pair.master,
        reader,
        writer,
    })
}

/// Decode only complete UTF-8 prefixes. At most three bytes remain pending
/// between reads; malformed sequences use the same replacement as lossy UTF-8.
fn push_service_bytes(ring: &mut Ring, bytes: &[u8]) -> usize {
    let mut offset = 0;
    while offset < bytes.len() {
        match std::str::from_utf8(&bytes[offset..]) {
            Ok(text) => {
                ring.push_chunk(text);
                return bytes.len();
            }
            Err(error) => {
                let valid_end = offset + error.valid_up_to();
                ring.push_chunk(&String::from_utf8_lossy(&bytes[offset..valid_end]));
                offset = valid_end;
                let Some(invalid_bytes) = error.error_len() else {
                    return offset;
                };
                ring.push_chunk("\u{fffd}");
                offset += invalid_bytes;
            }
        }
    }
    offset
}

fn pump_service_stream<R: Read, W: Write>(mut reader: R, mut artifact: W, ring: &Mutex<Ring>) {
    let mut chunk = [0u8; 8192];
    let mut pending = Vec::with_capacity(chunk.len() + 3);
    loop {
        match reader.read(&mut chunk) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let data = &chunk[..n]; // ubs:ignore n bounded by read into chunk
                let _ = artifact.write_all(data);
                pending.extend_from_slice(data);
                let Ok(mut ring) = ring.lock() else {
                    return;
                };
                let consumed = push_service_bytes(&mut ring, &pending);
                drop(pending.drain(..consumed));
            }
        }
    }
    if let Ok(mut ring) = ring.lock() {
        ring.push_chunk(&String::from_utf8_lossy(&pending));
        ring.finish_partial();
    }
}

fn ring_tail(ring: &Mutex<Ring>, count: usize) -> String {
    ring.lock().map_or_else(
        |_| "(log unavailable)".to_string(),
        |ring| {
            let (lines, _) = ring.since(0);
            let tail: Vec<String> = lines.iter().rev().take(count).rev().cloned().collect();
            if tail.is_empty() {
                "(no output)".to_string()
            } else {
                tail.join("\n")
            }
        },
    )
}

/// List every service this session, plus detached survivors from the state
/// file that this process has not adopted.
///
/// # Errors
/// Registry lock failure.
pub fn ps() -> Result<Vec<ServiceSnapshot>> {
    let reg = registry().lock().map_err(|_| registry_err())?;
    Ok(reg
        .services
        .values()
        .map(ServiceSnapshot::from_entry)
        .collect())
}

/// Read service logs.
///
/// `since` returns lines newer than the cursor; `tail` returns the last N
/// lines; `grep` filters (substring, case-sensitive); `wait_ms` bounds how
/// long `logs` blocks waiting for new lines when `since` is supplied.
/// Retention is capped at 10,000 lines and 1 MiB of text. Oversized lines
/// keep a UTF-8-safe tail with an explicit truncation marker; the raw artifact
/// retains the original bytes. Eviction never rewinds the source-line cursor.
///
/// # Errors
/// `PI_HUB_UNKNOWN_SERVICE` for unknown names.
#[allow(clippy::significant_drop_tightening)]
pub fn logs(
    name: &str,
    since: Option<u64>,
    tail: Option<usize>,
    grep: Option<&str>,
    wait_ms: u64,
) -> Result<LogPage> {
    let deadline = Instant::now() + Duration::from_millis(wait_ms.min(60_000));
    loop {
        {
            let reg = registry().lock().map_err(|_| registry_err())?;
            let Some(entry) = reg.services.get(name) else {
                return Err(Error::tool(
                    "hub",
                    format!("PI_HUB_UNKNOWN_SERVICE: no service named '{name}'"), // ubs:ignore cold error path
                ));
            };
            let ring = entry.ring.lock().map_err(|_| registry_err())?;
            let (mut lines, cursor) = ring.since(since.unwrap_or(0));
            if since.is_none()
                && let Some(count) = tail
            {
                lines = lines.iter().rev().take(count).rev().cloned().collect();
            }
            if let Some(needle) = grep {
                lines.retain(|line| line.contains(needle));
            }
            // Wait only makes sense when the caller is looking for something:
            // an incremental cursor or a grep filter. A bare snapshot read
            // returns immediately.
            let seeking = since.is_some() || grep.is_some();
            if !lines.is_empty() || !seeking || Instant::now() >= deadline {
                return Ok(LogPage {
                    schema: "pi.hub.logs.v1".to_string(), // ubs:ignore loop returns immediately after
                    name: name.to_string(), // ubs:ignore loop returns immediately after
                    lines,
                    cursor,
                    status: entry.status.as_str().to_string(), // ubs:ignore loop returns after
                });
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Send text to a running service's PTY stdin (`enter` appends CR).
///
/// # Errors
/// `PI_HUB_UNKNOWN_SERVICE` / `PI_HUB_NOT_RUNNING` for invalid targets.
#[allow(clippy::significant_drop_tightening)]
pub fn send_text(name: &str, text: &str, enter: bool) -> Result<()> {
    write_to_master(name, |writer| {
        writer.write_all(text.as_bytes())?;
        if enter {
            writer.write_all(b"\r")?;
        }
        writer.flush()
    })
}

/// Send named keys: ENTER, TAB, ESCAPE, CTRL_C, CTRL_D, UP, DOWN, LEFT,
/// RIGHT.
///
/// # Errors
/// Named validation error for unknown key names.
pub fn send_keys(name: &str, keys: &[String]) -> Result<()> {
    // Map every key up front so the write loop carries no allocation.
    let mapped: Vec<&[u8]> = keys
        .iter()
        .map(|key| key_bytes(key))
        .collect::<Result<_>>()?;
    for bytes in mapped {
        write_to_master(name, |writer| {
            writer.write_all(bytes)?;
            writer.flush()
        })?;
    }
    Ok(())
}

fn key_bytes(key: &str) -> Result<&'static [u8]> {
    match key.to_ascii_uppercase().as_str() {
        "ENTER" => Ok(b"\r"),
        "TAB" => Ok(b"\t"),
        "ESCAPE" => Ok(b"\x1b"),
        "CTRL_C" => Ok(b"\x03"),
        "CTRL_D" => Ok(b"\x04"),
        "UP" => Ok(b"\x1b[A"),
        "DOWN" => Ok(b"\x1b[B"),
        "RIGHT" => Ok(b"\x1b[C"),
        "LEFT" => Ok(b"\x1b[D"),
        other => Err(Error::validation(format!(
            "Unknown key '{other}'; expected ENTER, TAB, ESCAPE, CTRL_C, CTRL_D, \
             UP, DOWN, LEFT, RIGHT"
        ))),
    }
}

#[allow(clippy::significant_drop_tightening)]
fn write_to_master(
    name: &str,
    write: impl FnOnce(&mut dyn Write) -> std::io::Result<()>,
) -> Result<()> {
    let writer = {
        let reg = registry().lock().map_err(|_| registry_err())?;
        let Some(entry) = reg.services.get(name) else {
            return Err(Error::tool(
                "hub",
                format!("PI_HUB_UNKNOWN_SERVICE: no service named '{name}'"), // ubs:ignore cold error path
            ));
        };
        if !entry.status.live() {
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NOT_RUNNING: service '{name}' is {} — stdin is closed",
                    entry.status.as_str()
                ),
            ));
        }
        Arc::clone(&entry.writer)
    };
    let mut guard = writer.lock().map_err(|_| registry_err())?;
    let Some(writer) = guard.as_mut() else {
        return Err(Error::tool(
            "hub",
            format!("PI_HUB_NO_INPUT: service '{name}' has no writable PTY master"),
        ));
    };
    write(writer.as_mut())
        .map_err(|e| Error::tool("hub", format!("Failed to write to service stdin: {e}")))
}

/// Send a signal to the service's process tree.
///
/// # Errors
/// `PI_HUB_UNKNOWN_SERVICE` / `PI_HUB_NOT_RUNNING`.
#[allow(clippy::significant_drop_tightening)]
pub fn send_signal(name: &str, signal: sysinfo::Signal) -> Result<()> {
    let pid = {
        let reg = registry().lock().map_err(|_| registry_err())?;
        let Some(entry) = reg.services.get(name) else {
            return Err(Error::tool(
                "hub",
                format!("PI_HUB_UNKNOWN_SERVICE: no service named '{name}'"), // ubs:ignore cold error path
            ));
        };
        if !entry.status.live() {
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NOT_RUNNING: service '{name}' is {}",
                    entry.status.as_str()
                ),
            ));
        }
        entry.pid
    };
    let Some(pid) = pid else {
        return Err(Error::tool(
            "hub",
            format!("PI_HUB_NOT_RUNNING: service '{name}' has no live pid"),
        ));
    };
    signal_pid_tree(pid, signal);
    Ok(())
}

fn signal_pid_tree(pid: u32, signal: sysinfo::Signal) {
    let root = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let mut children: HashMap<sysinfo::Pid, Vec<sysinfo::Pid>> = HashMap::new();
    for (pid_key, proc_) in sys.processes() {
        if let Some(parent) = proc_.parent() {
            children.entry(parent).or_default().push(*pid_key);
        }
    }
    let mut stack = vec![root];
    let mut visited = std::collections::HashSet::new();
    while let Some(current) = stack.pop() {
        if !visited.insert(current) {
            continue;
        }
        if let Some(proc_) = sys.process(current) {
            let _ = proc_.kill_with(signal).unwrap_or_else(|| proc_.kill());
        }
        if let Some(kids) = children.get(&current) {
            stack.extend(kids.iter().copied());
        }
    }
}

/// Stop a service: TERM → grace → KILL with the full process-tree walk
/// (same discipline as the bash tool).
///
/// # Errors
/// `PI_HUB_UNKNOWN_SERVICE` / `PI_HUB_NOT_RUNNING`.
#[allow(clippy::significant_drop_tightening)]
pub fn stop(name: &str) -> Result<ServiceSnapshot> {
    let pid = {
        let mut reg = registry().lock().map_err(|_| registry_err())?;
        let Some(entry) = reg.services.get_mut(name) else {
            return Err(Error::tool(
                "hub",
                format!("PI_HUB_UNKNOWN_SERVICE: no service named '{name}'"), // ubs:ignore cold error path
            ));
        };
        if !entry.status.live() {
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NOT_RUNNING: service '{name}' already settled ({})",
                    entry.status.as_str()
                ),
            ));
        }
        entry.status = ServiceStatus::Killed;
        entry.pid
    };
    crate::tools::terminate_process_group_tree(pid);
    // The exit monitor records the settle; give it the grace window.
    std::thread::sleep(TERMINATE_GRACE.min(Duration::from_millis(500)));
    if let Some(pid) = pid {
        let still_alive = std::path::Path::new(&format!("/proc/{pid}")).exists();
        if still_alive {
            crate::tools::kill_process_group_tree(Some(pid));
        }
    }
    describe(name)
}

/// Restart reuses the retained launch spec. Running services are stopped
/// first; completed names re-spawn directly.
///
/// # Errors
/// `PI_HUB_UNKNOWN_SERVICE` for unknown names; start errors otherwise.
#[allow(clippy::significant_drop_tightening)]
pub fn restart(name: &str) -> Result<ServiceSnapshot> {
    let (spec, was_live) = {
        let reg = registry().lock().map_err(|_| registry_err())?;
        let Some(entry) = reg.services.get(name) else {
            return Err(Error::tool(
                "hub",
                format!("PI_HUB_UNKNOWN_SERVICE: no service named '{name}'"), // ubs:ignore cold error path
            ));
        };
        (entry.spec.clone(), entry.status.live())
    };
    if was_live {
        let _ = stop(name);
    }
    start(&spec)
}

/// Full descriptor for one service.
///
/// # Errors
/// `PI_HUB_UNKNOWN_SERVICE` for unknown names.
#[allow(clippy::significant_drop_tightening)]
pub fn describe(name: &str) -> Result<ServiceSnapshot> {
    let reg = registry().lock().map_err(|_| registry_err())?;
    let Some(entry) = reg.services.get(name) else {
        return Err(Error::tool(
            "hub",
            format!("PI_HUB_UNKNOWN_SERVICE: no service named '{name}'"), // ubs:ignore cold error path
        ));
    };
    Ok(ServiceSnapshot::from_entry(entry))
}

/// Kill every non-detached service (session exit). Called once from the
/// main shutdown chokepoint next to `jobs::kill_all`.
pub fn kill_session_services() {
    let pids: Vec<(String, Option<u32>)> = {
        let Ok(mut reg) = registry().lock() else {
            return;
        };
        let victims: Vec<(String, Option<u32>)> = reg
            .services
            .values_mut()
            .filter(|entry| entry.status.live() && !entry.spec.detached)
            .map(|entry| {
                entry.status = ServiceStatus::Killed;
                (entry.spec.name.clone(), entry.pid)
            })
            .collect();
        persist_detached_state(&reg);
        victims
    };
    for (_, pid) in pids {
        crate::tools::kill_process_group_tree(pid);
    }
}

/// Tests share the process-global registry; serialize them. Poison from a
/// failed peer is tolerated (the lock only serializes).
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::LazyLock<Mutex<()>> = std::sync::LazyLock::new(|| Mutex::new(()));
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-hub-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }

    fn spec(name: &str, program: &str, args: &[&str], ready: Option<ReadySpec>) -> LaunchSpec {
        LaunchSpec {
            name: name.to_string(),
            program: program.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            cwd: temp_root(),
            env: Vec::new(),
            ready,
            detached: false,
        }
    }

    #[test]
    fn ring_cursor_semantics() {
        let mut ring = Ring::new(8);
        ring.push_chunk("one\ntwo\nthr");
        ring.push_chunk("ee\nfour\nfive\n");
        let (lines, cursor) = ring.since(0);
        assert_eq!(lines, vec!["one", "two", "three", "four", "five"]);
        assert_eq!(cursor, 5);
        let (newer, _) = ring.since(3);
        assert_eq!(newer, vec!["four", "five"]);

        // Cap eviction drops the oldest lines while cursors stay monotonic.
        let mut capped = Ring::new(3);
        capped.push_chunk("a\nb\nc\nd\n");
        let (retained, cursor) = capped.since(0);
        assert_eq!(retained, vec!["b", "c", "d"]);
        assert_eq!(cursor, 4);
    }

    #[test]
    fn ring_bounds_unterminated_unicode_lines_and_marks_only_truncated_lines() {
        let mut ring = Ring::new(8);
        let chunk = "界🙂".repeat(1024);
        for _ in 0..64 {
            ring.push_chunk(&chunk);
            assert!(ring.partial.len() <= RING_LINE_BYTE_CAP);
            assert_eq!(ring.next_index, 0);
        }
        // Exercise the single-fragment path as well as incremental overflow.
        ring.push_chunk(&"界".repeat(RING_LINE_BYTE_CAP));
        ring.push_chunk("ready\nordinary\n");
        let (lines, cursor) = ring.since(0);
        assert_eq!(cursor, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with(TRUNCATED_LINE_PREFIX));
        assert!(lines[0].ends_with("ready"));
        assert!(lines[0].len() <= RING_LINE_BYTE_CAP);
        assert!(!lines[0].contains('\u{fffd}'));
        assert_eq!(lines[1], "ordinary");
        assert!(ring.partial.is_empty());
        assert!(!ring.partial_truncated);
    }

    #[test]
    fn ring_byte_eviction_preserves_source_line_cursors() {
        let mut ring = Ring::new(RING_LINE_CAP);
        let padding = "x".repeat(8192);
        for index in 0..256 {
            ring.push_chunk(&format!("{index:04}:{padding}\n"));
            assert!(ring.bytes <= RING_BYTE_CAP);
            assert_eq!(ring.bytes, ring.lines.iter().map(String::len).sum::<usize>());
        }
        let (lines, cursor) = ring.since(0);
        assert_eq!(cursor, 256);
        assert!(lines.len() < 256, "byte budget must evict before line cap");
        assert!(!lines.is_empty());
        let oldest = cursor - u64::try_from(lines.len()).expect("retained count");
        assert_eq!(ring.since(oldest).0, lines);
        assert!(ring.since(255).0[0].starts_with("0255:"));
        assert!(ring.since(cursor).0.is_empty());
    }

    #[test]
    fn ring_zero_capacity_and_final_flush_preserve_cursor_semantics() {
        let mut ring = Ring::new(0);
        ring.push_chunk("one\ntail");
        ring.finish_partial();
        ring.finish_partial();
        assert_eq!(ring.since(0), (Vec::<String>::new(), 2));
        assert_eq!(ring.bytes, 0);
        assert!(ring.partial.is_empty());
    }

    #[test]
    fn readiness_text_preserves_partial_lines_without_a_synthetic_leading_newline() {
        let mut ring = Ring::new(8);
        ring.push_chunk("ready");
        assert_eq!(ring.text(), "ready");
        ring.push_chunk("\r\nnext");
        assert_eq!(ring.text(), "ready\nnext");
        ring.finish_partial();
        assert_eq!(ring.since(0).0, vec!["ready", "next"]);
    }

    /// Exercise the actual stream pump with arbitrary read boundaries, EINTR,
    /// and a terminal read error, without subprocess scheduling or disk I/O.
    struct FragmentedReader<'a> {
        bytes: &'a [u8],
        width: usize,
        interrupt_next: bool,
        terminal_error: bool,
    }

    impl Read for FragmentedReader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.interrupt_next {
                self.interrupt_next = false;
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            if self.bytes.is_empty() && self.terminal_error {
                return Err(std::io::Error::other("terminal PTY read error"));
            }
            let count = buffer.len().min(self.width).min(self.bytes.len());
            buffer[..count].copy_from_slice(&self.bytes[..count]);
            self.bytes = &self.bytes[count..];
            self.interrupt_next = true;
            Ok(count)
        }
    }

    #[test]
    fn stream_pump_preserves_utf8_across_reads_and_flushes_after_eof_or_error() {
        let input = "first\nprêt 界🙂 tail".as_bytes();
        for width in 1..=input.len() {
            for terminal_error in [false, true] {
                let reader = FragmentedReader {
                    bytes: input,
                    width,
                    interrupt_next: true,
                    terminal_error,
                };
                let ring = Mutex::new(Ring::new(8));
                let mut artifact = Vec::new();
                pump_service_stream(reader, &mut artifact, &ring);
                let ring = ring.into_inner().expect("ring");
                assert_eq!(artifact, input);
                assert_eq!(ring.since(0).0, vec!["first", "prêt 界🙂 tail"]);
                assert_eq!(ring.next_index, 2);
                assert!(ring.partial.is_empty());
            }
        }
    }

    #[test]
    fn stream_pump_invalid_and_incomplete_utf8_matches_whole_stream_lossy_decoding() {
        let input = b"head\n\xf0\x9f\x99\x82\xff\xe2\x82!\xf0\x9f";
        let expected: Vec<String> = String::from_utf8_lossy(input)
            .lines()
            .map(str::to_string)
            .collect();
        for width in 1..=input.len() {
            let reader = FragmentedReader {
                bytes: input,
                width,
                interrupt_next: true,
                terminal_error: false,
            };
            let ring = Mutex::new(Ring::new(8));
            let mut artifact = Vec::new();
            pump_service_stream(reader, &mut artifact, &ring);
            assert_eq!(artifact, input);
            assert_eq!(ring.into_inner().expect("ring").since(0).0, expected);
        }
    }

    #[test]
    fn stream_pump_retains_raw_artifact_when_text_is_truncated() {
        let input = format!("{}final", "x".repeat(RING_BYTE_CAP * 2));
        let ring = Mutex::new(Ring::new(8));
        let mut artifact = Vec::new();
        pump_service_stream(input.as_bytes(), &mut artifact, &ring);
        let ring = ring.into_inner().expect("ring");
        assert_eq!(artifact, input.as_bytes());
        let (lines, cursor) = ring.since(0);
        assert_eq!(cursor, 1);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with(TRUNCATED_LINE_PREFIX));
        assert!(lines[0].ends_with("final"));
        assert!(lines[0].len() <= RING_LINE_BYTE_CAP);
    }

    #[test]
    fn reservation_rejects_duplicate_before_a_pid_exists() {
        let mut reg = ServiceRegistry::default();
        let launch = spec("hub-reserved", "unused", &[], None);
        let first = Arc::new(Mutex::new(Ring::new(8)));
        let second = Arc::new(Mutex::new(Ring::new(8)));
        reg.reserve(&launch, &first, &PathBuf::from("first.log")).expect("reserve");
        let error = reg
            .reserve(&launch, &second, &PathBuf::from("second.log"))
            .expect_err("pending launch owns the name");
        assert!(error.to_string().contains("PI_HUB_NAME_TAKEN"));
        assert!(reg.current(&launch.name, &first).expect("first").pid.is_none());
        assert!(reg.current(&launch.name, &second).is_none());
    }

    #[test]
    fn stale_exit_readiness_and_timeout_cannot_mutate_replacement() {
        let mut reg = ServiceRegistry::default();
        let launch = spec("hub-generation", "unused", &[], None);
        let old = Arc::new(Mutex::new(Ring::new(8)));
        let new = Arc::new(Mutex::new(Ring::new(8)));
        reg.reserve(&launch, &old, &PathBuf::from("old.log")).expect("old");
        reg.time_out(&launch.name, &old);
        reg.reserve(&launch, &new, &PathBuf::from("new.log")).expect("new");
        reg.current_mut(&launch.name, &new).expect("new entry").pid = Some(42);
        reg.mark_ready(&launch.name, &new).expect("ready");
        assert!(!reg.settle(&launch.name, &old, 1));
        assert!(
            reg.mark_ready(&launch.name, &old)
                .unwrap_err()
                .to_string()
                .contains("PI_HUB_STALE_SERVICE")
        );
        assert_eq!(reg.time_out(&launch.name, &old), None);
        let current = reg.current(&launch.name, &new).expect("replacement");
        assert_eq!(current.status, ServiceStatus::Running);
        assert_eq!(current.pid, Some(42));
        assert_eq!(current.exit_code, None);
        assert_eq!(current.log_path, PathBuf::from("new.log"));
    }

    #[test]
    fn readiness_cannot_resurrect_any_terminal_status() {
        for terminal in [
            ServiceStatus::Exited,
            ServiceStatus::Failed,
            ServiceStatus::Killed,
        ] {
            let mut reg = ServiceRegistry::default();
            let launch = spec("hub-terminal", "unused", &[], None);
            let ring = Arc::new(Mutex::new(Ring::new(8)));
            reg.reserve(&launch, &ring, &PathBuf::from("terminal.log")).expect("reserve");
            reg.current_mut(&launch.name, &ring).expect("entry").status = terminal;
            assert!(reg.mark_ready(&launch.name, &ring).is_err());
            assert_eq!(reg.current(&launch.name, &ring).expect("entry").status, terminal);
        }
    }

    #[test]
    fn exit_monitor_preserves_killed_status_and_clears_only_its_pid() {
        let mut reg = ServiceRegistry::default();
        let launch = spec("hub-killed", "unused", &[], None);
        let ring = Arc::new(Mutex::new(Ring::new(8)));
        reg.reserve(&launch, &ring, &PathBuf::from("killed.log")).expect("reserve");
        reg.current_mut(&launch.name, &ring).expect("entry").pid = Some(42);
        assert_eq!(reg.time_out(&launch.name, &ring), Some(42));
        assert!(reg.settle(&launch.name, &ring, 137));
        let settled = reg.current(&launch.name, &ring).expect("entry");
        assert_eq!(settled.status, ServiceStatus::Killed);
        assert_eq!(settled.pid, None);
        assert_eq!(settled.exit_code, Some(137));
    }

    #[test]
    fn failed_start_guard_does_not_settle_a_new_reservation() {
        let _lock = test_lock();
        let launch = spec("hub-pending-guard", "unused", &[], None);
        let old = Arc::new(Mutex::new(Ring::new(8)));
        let new = Arc::new(Mutex::new(Ring::new(8)));
        let pending_old = PendingService::reserve(&launch, &old, &PathBuf::from("old.log"))
            .expect("old reservation");
        registry().lock().expect("registry").time_out(&launch.name, &old);
        let pending_new = PendingService::reserve(&launch, &new, &PathBuf::from("new.log"))
            .expect("new reservation");
        drop(pending_old);
        assert_eq!(describe(&launch.name).expect("current").status, "starting");
        drop(pending_new);
        let failed = describe(&launch.name).expect("failed setup");
        assert_eq!(failed.status, "failed");
        assert_eq!(failed.pid, None);
        assert_eq!(failed.exit_code, Some(-1));
    }

    #[test]
    fn readiness_marker_is_latched_before_output_eviction() {
        let mut output = Ring::new(3);
        output.watch_readiness(Some(regex::Regex::new("^ready 界🙂").expect("regex")));
        let ring = Mutex::new(output);
        let input = "ready 界🙂\na\nb\nc\nd\n".as_bytes();
        let reader = FragmentedReader {
            bytes: input,
            width: 1,
            interrupt_next: true,
            terminal_error: false,
        };
        pump_service_stream(reader, std::io::sink(), &ring);
        let output = ring.into_inner().expect("ring");
        assert_eq!(output.since(0).0, vec!["b", "c", "d"]);
        assert!(output.ready_log_passed, "observed readiness must survive eviction");
    }

    #[test]
    fn readiness_does_not_match_synthetic_truncation_annotations() {
        let mut ring = Ring::new(8);
        ring.watch_readiness(Some(regex::Regex::new("truncated").expect("regex")));
        ring.push_chunk(&"x".repeat(RING_LINE_BYTE_CAP * 2));
        ring.push_chunk("\n");
        assert!(ring.since(0).0[0].starts_with(TRUNCATED_LINE_PREFIX));
        assert!(!ring.ready_log_passed);
        ring.push_chunk("service says truncated\n");
        assert!(ring.ready_log_passed, "literal service output still matches");
    }

    #[test]
    fn overflowing_readiness_timeout_is_rejected_before_spawn() {
        let _lock = test_lock();
        let error = start(&spec(
            "hub-timeout-overflow",
            "pi-hub-program-that-does-not-exist",
            &[],
            Some(ReadySpec {
                timeout_secs: Some(u64::MAX),
                ..ReadySpec::default()
            }),
        ))
        .expect_err("timeout must be validated before program resolution");
        assert!(error.to_string().contains("PI_HUB_INVALID_READY_TIMEOUT"));
        assert!(describe("hub-timeout-overflow").is_err());
        let now = Instant::now();
        assert_eq!(
            readiness_deadline(now, Duration::ZERO).expect("zero budget"),
            now
        );
    }

    #[test]
    fn service_artifact_creation_never_truncates_existing_logs() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("service.log");
        std::fs::write(&path, b"previous run").expect("old log");
        assert!(create_service_log(&path).is_err());
        assert_eq!(std::fs::read(&path).expect("old bytes"), b"previous run");
        let fresh = dir.path().join("fresh.log");
        create_service_log(&fresh)
            .expect("fresh log")
            .write_all(b"new run")
            .expect("write");
        assert_eq!(std::fs::read(fresh).expect("fresh bytes"), b"new run");
    }

    #[cfg(unix)]
    #[test]
    fn service_artifact_creation_rejects_symlinks() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("target");
        let link = dir.path().join("service.log");
        std::fs::write(&target, b"untouched").expect("target");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert!(create_service_log(&link).is_err());
        assert_eq!(std::fs::read(target).expect("target bytes"), b"untouched");
    }

    #[derive(Debug, Clone)]
    enum WaitStep {
        Interrupted,
        Failed,
        Exit(u32),
    }

    #[derive(Debug, Clone)]
    struct RecordingChild {
        events: Arc<Mutex<Vec<&'static str>>>,
        steps: VecDeque<WaitStep>,
    }

    impl portable_pty::ChildKiller for RecordingChild {
        fn kill(&mut self) -> std::io::Result<()> {
            self.events.lock().expect("events").push("kill");
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(self.clone())
        }
    }

    impl portable_pty::Child for RecordingChild {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            self.events.lock().expect("events").push("wait");
            match self.steps.pop_front().unwrap_or(WaitStep::Exit(0)) {
                WaitStep::Interrupted => Err(std::io::ErrorKind::Interrupted.into()),
                WaitStep::Failed => Err(std::io::Error::other("injected wait failure")),
                WaitStep::Exit(code) => Ok(portable_pty::ExitStatus::with_exit_code(code)),
            }
        }

        fn process_id(&self) -> Option<u32> {
            None // No real process: this tests ownership, not OS tree signalling.
        }

        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }

    type ChildEvents = Arc<Mutex<Vec<&'static str>>>;

    fn recording_child(steps: &[WaitStep]) -> (ServiceChild, ChildEvents) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let child = ServiceChild {
            child: Box::new(RecordingChild {
                events: Arc::clone(&events),
                steps: steps.iter().cloned().collect(),
            }),
            reaped: false,
        };
        (child, events)
    }

    #[test]
    fn child_guard_kills_and_reaps_on_setup_failure() {
        let (child, events) = recording_child(&[WaitStep::Interrupted, WaitStep::Exit(137)]);
        drop(child);
        assert_eq!(*events.lock().expect("events"), vec!["kill", "wait", "wait"]);
    }

    #[test]
    fn child_monitor_retries_interruption_without_killing_a_reaped_child() {
        let (child, events) = recording_child(&[WaitStep::Interrupted, WaitStep::Exit(7)]);
        assert_eq!(child.wait(), 7);
        assert_eq!(*events.lock().expect("events"), vec!["wait", "wait"]);
    }

    #[test]
    fn child_monitor_wait_failure_still_kills_and_reaps() {
        let (child, events) = recording_child(&[WaitStep::Failed, WaitStep::Exit(137)]);
        assert_eq!(child.wait(), -1);
        assert_eq!(*events.lock().expect("events"), vec!["wait", "kill", "wait"]);
    }

    #[test]
    fn invalid_name_is_rejected_before_program_resolution() {
        let _guard = crate::hub::test_lock();
        let err = start(&spec(
            "../../outside-hub",
            "pi-hub-program-that-does-not-exist",
            &[],
            None,
        ))
        .expect_err("path-like service name must fail before spawn");
        assert!(
            err.to_string().contains("PI_HUB_INVALID_NAME"),
            "name validation must win before program resolution: {err}"
        );
    }

    #[test]
    fn invalid_ready_regex_is_rejected_before_program_resolution() {
        let _guard = crate::hub::test_lock();
        let err = start(&spec(
            "hub-test-invalid-ready-regex",
            "pi-hub-program-that-does-not-exist",
            &[],
            Some(ReadySpec {
                log: Some("[".to_string()),
                port: None,
                timeout_secs: Some(1),
            }),
        ))
        .expect_err("invalid regex must fail before spawn");
        assert!(
            err.to_string().contains("Invalid ready.log regex"),
            "regex validation must win before program resolution: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn readiness_requires_observed_port() {
        let _guard = crate::hub::test_lock();
        // No listener on the port → readiness must time out and kill.
        let result = start(&spec(
            "hub-test-dead",
            "sleep",
            &["30"],
            Some(ReadySpec {
                log: None,
                port: Some(39_991),
                timeout_secs: Some(1),
            }),
        ));
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("PI_HUB_NOT_READY"),
            "expected not-ready error, got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_live_name_rejected() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-dupe";
        let first = start(&spec(name, "sleep", &["30"], None)).expect("first start");
        assert_eq!(first.status, "running");
        let second = start(&spec(name, "sleep", &["30"], None));
        let err = second.unwrap_err();
        assert!(
            err.to_string().contains("PI_HUB_NAME_TAKEN"),
            "expected name-taken error, got: {err}"
        );
        let _ = stop(name);
    }

    #[cfg(unix)]
    #[test]
    fn send_text_drives_repl() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-repl";
        let mut launch = spec(
            name,
            "python3",
            &["-u", "-i", "-q"],
            Some(ReadySpec {
                log: Some(">>>".to_string()),
                port: None,
                timeout_secs: Some(20),
            }),
        );
        launch
            .env
            .push(("PYTHONUNBUFFERED".to_string(), "1".to_string()));
        let snapshot = start(&launch).expect("repl start");
        assert_eq!(snapshot.status, "running");
        send_text(name, "print(40 + 2)", true).expect("send");
        let page = logs(name, None, Some(50), Some("42"), 5_000).expect("logs");
        assert!(
            page.lines.iter().any(|line| line.contains("42")),
            "repl output must contain 42: {:?}",
            page.lines
        );
        let _ = stop(name);
    }

    #[cfg(unix)]
    #[test]
    fn restart_after_completion_works() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-restart";
        // A service that exits on its own, then restarts from the retained spec.
        let first = start(&spec(name, "echo", &["first-run"], None)).expect("first run");
        assert_eq!(first.status, "running");
        std::thread::sleep(Duration::from_millis(400));
        let settled = describe(name).expect("describe");
        assert!(
            settled.status == "exited",
            "echo should have exited: {settled:?}"
        );
        let restarted = restart(name).expect("restart");
        assert_eq!(restarted.status, "running");
        assert_ne!(
            first.log_path, restarted.log_path,
            "each run must own its artifact"
        );
        std::thread::sleep(Duration::from_millis(400));
        let page = logs(name, None, Some(50), Some("first-run"), 5_000).expect("logs");
        assert!(
            page.lines.iter().any(|line| line.contains("first-run")),
            "restarted service must run the retained spec: {:?}",
            page.lines
        );
        let _ = stop(name).ok();
    }

    #[cfg(unix)]
    #[test]
    fn status_stays_running_for_live_repl() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-stable";
        let snapshot = start(&spec(
            name,
            "python3",
            &["-i", "-q"],
            Some(ReadySpec {
                log: Some(">>>".to_string()),
                port: None,
                timeout_secs: Some(10),
            }),
        ))
        .expect("repl start");
        assert_eq!(snapshot.status, "running");
        for wait_ms in [100u64, 300, 600] {
            std::thread::sleep(Duration::from_millis(wait_ms));
            let current = describe(name).expect("describe");
            assert_eq!(
                current.status, "running",
                "status flipped to {} after {}ms with python alive (exit_code {:?})",
                current.status, wait_ms, current.exit_code
            );
        }
        send_keys(name, &["CTRL_C".to_string()]).expect("keys");
        std::thread::sleep(Duration::from_millis(200));
        let after = describe(name).expect("describe after keys");
        assert_eq!(after.status, "running", "status after CTRL_C: {after:?}");
        let _ = stop(name);
    }

    #[cfg(unix)]
    #[test]
    fn stop_leaves_no_survivors() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-stop";
        let snapshot = start(&spec(name, "sleep", &["300"], None)).expect("start");
        let pid = snapshot.pid.expect("pid");
        let stopped = stop(name).expect("stop");
        assert_eq!(stopped.status, "killed");
        std::thread::sleep(Duration::from_millis(500));
        // A reaped-away process is dead; a not-yet-reaped zombie (state Z)
        // is dead too — only a live state fails the assertion.
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| stat.rsplit(')').next()?.trim().chars().next());
        assert!(
            state.is_none() || state == Some('Z'),
            "process {pid} survived stop (state {state:?})"
        );
    }

    #[cfg(unix)]
    #[test]
    fn logs_cursor_advances_incrementally() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-cursor";
        let _ = start(&spec(
            name,
            "sh",
            &["-c", "echo first; sleep 300"],
            Some(ReadySpec {
                log: Some("first".to_string()),
                port: None,
                timeout_secs: Some(10),
            }),
        ))
        .expect("start");
        let first_page = logs(name, None, None, None, 0).expect("first page");
        assert!(first_page.lines.iter().any(|line| line.contains("first")));
        let cursor = first_page.cursor;
        send_text(name, "echo second", true).expect("send");
        let second_page = logs(name, Some(cursor), None, Some("second"), 5_000).expect("page 2");
        assert!(
            second_page.lines.iter().any(|line| line.contains("second")),
            "incremental page must contain the new line: {:?}",
            second_page.lines
        );
        assert!(second_page.cursor > cursor);
        let _ = stop(name);
    }
}
