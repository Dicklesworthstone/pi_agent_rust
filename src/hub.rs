//! Hub-style process supervision (bd-cv653.5.4).
//!
//! Long-running services, watchers, REPLs, and debuggers live here instead
//! of timeout-hacked `bash` calls. Every service spawns on a PTY (stdin
//! stays writable for `send`), output streams to a bounded raw artifact plus
//! a byte- and line-bounded text ring, and readiness is *observed* — a
//! `ready.log` regex and/or a `ready.port` TCP accept must both pass within
//! the timeout before `start` returns.
//!
//! Lifecycle: scoped to the durable session and its live tool registry.
//! Session replacement or shutdown retires that owner's services; final host
//! shutdown also stops remaining services. `detached: true` skips cleanup
//! once startup is accepted and remains addressable by the same session.
//! Detached PTYs still belong to this host process; the persisted roster does
//! not provide cross-process reattachment or guarantee host-exit survival.
//!
//! The `hub` tool's `jobs` action group wraps the background-jobs registry
//! (bd-cv653.3.10); the `messaging` action group lands with the agent-hub
//! registry (bd-cv653.5.3).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::{Error, Result};

/// Tool-result schema tag for service descriptors (stable audit contract).
pub const SERVICE_SCHEMA: &str = "pi.hub.service.v1";

/// Maximum original output retained in one service's raw artifact. Output
/// beyond this prefix still feeds readiness and the live text ring.
pub const SERVICE_LOG_BYTE_CAP: u64 = 16 * 1024 * 1024;

/// Default readiness budget when the caller passes none.
const DEFAULT_READY_TIMEOUT_SECS: u64 = 30;

/// Bounded line ring kept per service for `logs` cursors.
const RING_LINE_CAP: usize = 10_000;

/// Bound retained text independently of newline frequency. An oversized line
/// retains its newest UTF-8-safe suffix; raw artifacts have a separate cap.
const RING_BYTE_CAP: usize = 1024 * 1024;
const RING_LINE_BYTE_CAP: usize = 16 * 1024;
const TRUNCATED_LINE_PREFIX: &str = "[...truncated...] ";

/// Grace window between TERM and KILL on stop, mirroring the bash tool.
const TERMINATE_GRACE: Duration = Duration::from_secs(3);
const SETTLEMENT_BUDGET: Duration = Duration::from_secs(8);
const MONITOR_POLL_INTERVAL: Duration = Duration::from_millis(25);
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(1);
const IO_POLL_INTERVAL: Duration = Duration::from_millis(10);
const INPUT_BUDGET: Duration = Duration::from_secs(8);

/// Keep service identifiers portable and bounded before deriving artifact
/// names from them.
const MAX_SERVICE_NAME_BYTES: usize = 128;

/// A durable session and the particular live registry hosting it. A prepared
/// replacement may load the same session without acquiring its predecessor's
/// processes or authority to shut them down.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceOwner {
    session_id: String,
    instance_id: String,
}

impl ServiceOwner {
    pub(crate) const fn new(session_id: String, instance_id: String) -> Self {
        Self {
            session_id,
            instance_id,
        }
    }

    /// Durable identity used for explicitly detached service resumption.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Identity of the live tool registry that admitted the service.
    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[derive(Default)]
struct ServiceOwnerState {
    generation: u64,
    active_shutdowns: usize,
    incomplete_shutdown: bool,
}

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
    Stopping,
    Exited,
    Killed,
    Failed,
}

impl ServiceStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Exited => "exited",
            Self::Killed => "killed",
            Self::Failed => "failed",
        }
    }

    const fn live(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }

    const fn accepts_input(self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

/// Requests never signal a copied PID. The monitor retains the child handle
/// through escalation and reap; completion belongs to this exact incarnation,
/// even when another caller has already restarted the retained service name.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // Independent startup, input and monitor atomics.
struct ServiceControl {
    stop_requested: AtomicBool,
    force_requested: AtomicBool,
    input_closed: AtomicBool,
    startup_accepted: AtomicBool,
    completion: Mutex<Option<ServiceSnapshot>>,
    completed: Condvar,
}

impl ServiceControl {
    fn request_stop(&self, force: bool) {
        self.input_closed.store(true, Ordering::Release);
        if force {
            self.force_requested.store(true, Ordering::Release);
        }
        self.stop_requested.store(true, Ordering::Release);
    }

    // Waiters are notified while the completion lock is still held, as written.
    #[allow(clippy::significant_drop_tightening)]
    fn finish(&self, snapshot: ServiceSnapshot) {
        let mut completion = self
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if completion.is_none() {
            *completion = Some(snapshot);
        }
        self.completed.notify_all();
    }

    // The guard is consumed by the condvar wait and handed back; the lint has
    // no narrower scope to offer.
    #[allow(clippy::significant_drop_tightening)]
    fn wait_until(&self, name: &str, deadline: Instant) -> Result<ServiceSnapshot> {
        let completion = self
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (completion, _) = self
            .completed
            .wait_timeout_while(
                completion,
                deadline.saturating_duration_since(Instant::now()),
                |completion| completion.is_none(),
            )
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        completion.clone().ok_or_else(|| {
            Error::tool(
                "hub",
                format!(
                    "PI_HUB_STOP_TIMEOUT: service '{name}' has not finished cleanup; \
                     its name remains reserved until process and output cleanup finish"
                ),
            )
        })
    }
}

/// One tool invocation owns a startup until its result reaches the caller.
/// The control is retained directly: cancellation must never find a newer
/// incarnation by looking up the service's reusable name.
#[derive(Clone)]
pub(crate) struct ServiceRequest {
    owner: crate::agent_cx::AgentCx,
    service_owner: ServiceOwner,
    generation: u64,
    cancelled: Arc<AtomicBool>,
    startup: Arc<Mutex<Option<OwnedStartup>>>,
}

#[derive(Clone)]
struct OwnedStartup {
    name: String,
    control: Arc<ServiceControl>,
    cleanup_deadline: Option<Instant>,
}

impl ServiceRequest {
    fn new(service_owner: &ServiceOwner) -> Result<Self> {
        Self::after_resolution(service_owner, None)
    }

    fn after_resolution(
        service_owner: &ServiceOwner,
        instance_generation: Option<u64>,
    ) -> Result<Self> {
        if service_owner.session_id.trim().is_empty() {
            return Err(Error::tool(
                "hub",
                "PI_HUB_SESSION_UNAVAILABLE: service owner is empty",
            ));
        }
        let generation = {
            let mut reg = registry().lock().map_err(|_| registry_err())?;
            if instance_generation.is_some_and(|generation| {
                generation != reg.instance_generation(service_owner.instance_id())
            }) {
                return Err(Error::tool(
                    "hub",
                    "PI_HUB_STALE_SESSION: session changed during service owner lookup",
                ));
            }
            reg.ensure_owner_open(service_owner)?;
            reg.owners
                .entry(service_owner.clone())
                .or_default()
                .generation
        };
        Ok(Self {
            owner: crate::agent_cx::AgentCx::for_current_or_request(),
            service_owner: service_owner.clone(),
            generation,
            cancelled: Arc::new(AtomicBool::new(false)),
            startup: Arc::new(Mutex::new(None)),
        })
    }

    pub(crate) fn service_owner(&self) -> &ServiceOwner {
        &self.service_owner
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) || self.owner.checkpoint().is_err() {
            return Err(Error::tool(
                "hub",
                "PI_HUB_CANCELLED: service operation cancelled",
            ));
        }
        registry()
            .lock()
            .map_err(|_| registry_err())?
            .check_request(self)
    }

    fn accept(&self) -> Result<()> {
        self.check()?;
        let startup = self
            .startup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let reg = registry().lock().map_err(|_| registry_err())?;
        reg.check_request(self)?;
        if let Some(startup) = startup {
            reg.ensure_accepting_starts()?;
            startup
                .control
                .startup_accepted
                .store(true, Ordering::Release);
        }
        Ok(())
    }

    fn check_spawn(&self) -> Result<()> {
        self.check()?;
        let capabilities = self.owner.capabilities();
        if !capabilities.io || !capabilities.spawn || !capabilities.time {
            return Err(Error::tool(
                "hub",
                "PI_HUB_PERMISSION: service startup requires I/O, spawn and timer capabilities",
            ));
        }
        Ok(())
    }

    // The startup slot stays locked across the cancellation check and the
    // write, so `cancel()` cannot slip between them.
    #[allow(clippy::significant_drop_tightening)]
    fn own_startup(&self, name: &str, control: &Arc<ServiceControl>) -> Result<()> {
        let mut startup = self
            .startup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.check()?;
        *startup = Some(OwnedStartup {
            name: name.to_string(),
            control: Arc::clone(control),
            cleanup_deadline: None,
        });
        Ok(())
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(startup) = self
            .startup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            startup
                .cleanup_deadline
                .get_or_insert_with(|| Instant::now() + SETTLEMENT_BUDGET);
            startup.control.request_stop(true);
        }
    }

    fn cancel_and_wait(&self, error: Error) -> Error {
        self.cancel();
        let startup = self
            .startup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(startup) = startup
            && let Err(cleanup) = startup.control.wait_until(
                &startup.name,
                startup
                    .cleanup_deadline
                    .expect("cancellation sets the startup cleanup deadline"),
            )
        {
            return Error::tool(
                "hub",
                format!("{error}; startup cleanup remains pending: {cleanup}"),
            );
        }
        error
    }
}

struct ServiceRequestGuard {
    request: ServiceRequest,
    accepted: bool,
}

impl Drop for ServiceRequestGuard {
    fn drop(&mut self) {
        if !self.accepted {
            self.request.cancel();
        }
    }
}

/// Keep foreign blocking I/O off the async executor even when a standalone
/// request context has no runtime blocking pool. Failure to create a worker
/// is an error, never an inline fallback that stalls unrelated tasks.
async fn service_worker<T, F>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (sender, receiver) = futures::channel::oneshot::channel();
    std::thread::Builder::new()
        .name("hub-operation".to_string())
        .spawn(move || {
            let result = operation();
            let _ = sender.send(result);
        })
        .map_err(|error| Error::tool("hub", format!("Failed to start service worker: {error}")))?;
    receiver
        .await
        .map_err(|_| Error::tool("hub", "Service worker exited without a result"))
}

/// Offload one service operation while retaining cancellation and ownership
/// through result delivery. An abandoned future only requests cleanup; the
/// service monitor remains the process owner and publishes its actual reap.
pub(crate) async fn run_service_request<T, F>(
    scope: &crate::jobs::JobSessionScope,
    operation: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&ServiceRequest) -> Result<T> + Send + 'static,
{
    let owner = crate::agent_cx::AgentCx::for_current_or_request();
    let instance_generation = registry()
        .lock()
        .map_err(|_| registry_err())?
        .instance_generation(scope.instance_id());
    let deadline = Instant::now() + INPUT_BUDGET;
    let mut resolving = Box::pin(owner.with_current(scope.session_id()));
    let session_id = loop {
        if owner.checkpoint().is_err() {
            return Err(Error::tool(
                "hub",
                "PI_HUB_CANCELLED: session lookup cancelled",
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::tool(
                "hub",
                "PI_HUB_SESSION_TIMEOUT: session lookup timed out",
            ));
        }
        let time = owner.time();
        let pause = Box::pin(time.sleep(remaining.min(IO_POLL_INTERVAL)));
        match futures::future::select(resolving, pause).await {
            futures::future::Either::Left((result, _)) => break result?,
            futures::future::Either::Right(((), pending)) => resolving = pending,
        }
    };
    let request = ServiceRequest::after_resolution(
        &scope.service_owner(session_id),
        Some(instance_generation),
    )?;
    request.check()?;
    let mut guard = ServiceRequestGuard {
        request: request.clone(),
        accepted: false,
    };
    let worker_request = request.clone();
    let result = service_worker(move || {
        let result = worker_request
            .check()
            .and_then(|()| operation(&worker_request));
        let result = result.and_then(|value| worker_request.check().map(|()| value));
        result.map_err(|error| worker_request.cancel_and_wait(error))
    })
    .await;
    let result = match result {
        Ok(Ok(value)) => Ok(value),
        // The operation's worker already waited for any startup cleanup.
        // Do not silently double the settlement budget after a timeout.
        Ok(Err(error)) => return Err(error),
        Err(error) => Err(error),
    };
    match result.and_then(|value| request.accept().map(|()| value)) {
        Ok(value) => {
            guard.accepted = true;
            Ok(value)
        }
        Err(error) => {
            // Cancellation can win after the worker sent a successful start,
            // but before the executor delivered it. Keep cleanup off-thread
            // and await the exact startup control in that race too.
            request.cancel();
            Err(service_worker(move || request.cancel_and_wait(error)).await?)
        }
    }
}

/// Output-drain progress, independent of whether the child has already exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LogDrainState {
    Pending,
    Streaming,
    Complete,
    Failed,
}

/// Raw artifact accounting. A complete drain means the reader reached EOF;
/// `truncated` and `write_error` independently report an incomplete artifact.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogCapture {
    pub byte_limit: u64,
    pub observed_bytes: u64,
    /// Bytes accepted by successful writes, including partial writes before
    /// failure. This is not an fsync or durable-storage acknowledgement.
    pub retained_bytes: u64,
    pub dropped_bytes: u64,
    pub truncated: bool,
    pub drain_state: LogDrainState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_error: Option<String>,
}

impl Default for LogCapture {
    fn default() -> Self {
        Self {
            byte_limit: SERVICE_LOG_BYTE_CAP,
            observed_bytes: 0,
            retained_bytes: 0,
            dropped_bytes: 0,
            truncated: false,
            drain_state: LogDrainState::Pending,
            write_error: None,
            read_error: None,
        }
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
    capture: LogCapture,
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
            capture: LogCapture::default(),
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

type ServiceWriter = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

struct ServiceEntry {
    owner: ServiceOwner,
    spec: LaunchSpec,
    status: ServiceStatus,
    pid: Option<u32>,
    started_ms: i64,
    exit_code: Option<i32>,
    /// Preserve the process outcome while its output owner finishes cleanup.
    pending_status: Option<ServiceStatus>,
    log_path: PathBuf,
    ring: Arc<Mutex<Ring>>,
    /// Cached PTY writer, taken once before spawn —
    /// portable-pty's `UnixMasterWriter::drop` sends `\n`+VEOF, so caching
    /// the writer is what keeps the child's stdin open across sends.
    writer: ServiceWriter,
    control: Arc<ServiceControl>,
}

impl ServiceEntry {
    /// Return the old owner so its backend-specific destructor runs without
    /// retaining the registry lock. Existing sends retain this incarnation's
    /// control and abort before retrying any blocked write.
    fn close_input(&mut self) -> ServiceWriter {
        self.control.input_closed.store(true, Ordering::Release);
        std::mem::replace(&mut self.writer, Arc::new(Mutex::new(None)))
    }

    fn current_status(&self) -> ServiceStatus {
        if self.status.live() && self.control.stop_requested.load(Ordering::Acquire) {
            ServiceStatus::Stopping
        } else {
            self.status
        }
    }
}

fn release_service_input(writer: Option<ServiceWriter>) {
    if let Some(writer) = writer {
        let handle = writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(handle);
    }
}

/// Serializable service descriptor.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSnapshot {
    pub schema: String,
    pub owner_session_id: String,
    pub owner_instance_id: String,
    pub name: String,
    pub command: String,
    pub cwd: String,
    pub pid: Option<u32>,
    pub status: String,
    pub started_ms: i64,
    pub exit_code: Option<i32>,
    pub log_path: String,
    pub log_capture: LogCapture,
    pub detached: bool,
    pub ready: bool,
}

impl ServiceSnapshot {
    fn from_entry(entry: &ServiceEntry) -> Self {
        let status = entry.current_status();
        Self {
            schema: SERVICE_SCHEMA.to_string(),
            owner_session_id: entry.owner.session_id.clone(),
            owner_instance_id: entry.owner.instance_id.clone(),
            name: entry.spec.name.clone(),
            command: format!("{} {}", entry.spec.program, entry.spec.args.join(" ")),
            cwd: entry.spec.cwd.display().to_string(),
            pid: entry.pid,
            status: status.as_str().to_string(),
            started_ms: entry.started_ms,
            exit_code: entry.exit_code,
            log_path: entry.log_path.display().to_string(),
            log_capture: entry
                .ring
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .capture
                .clone(),
            detached: entry.spec.detached,
            ready: status == ServiceStatus::Running,
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
    /// Earliest source-line cursor still retained in the live text ring.
    pub oldest_cursor: u64,
    /// Requested source lines evicted before an incremental cursor was read.
    pub lost_lines: u64,
    pub status: String,
    pub log_path: String,
    pub log_capture: LogCapture,
}

#[derive(Default)]
struct ServiceRegistry {
    services: HashMap<(ServiceOwner, String), ServiceEntry>,
    owners: HashMap<ServiceOwner, ServiceOwnerState>,
    instance_generations: HashMap<String, u64>,
    retired_instances: HashSet<String>,
    shutting_down: bool,
}

impl ServiceRegistry {
    fn instance_generation(&self, instance_id: &str) -> u64 {
        self.instance_generations
            .get(instance_id)
            .copied()
            .unwrap_or(0)
    }

    fn ensure_owner_open(&self, owner: &ServiceOwner) -> Result<()> {
        if self.retired_instances.contains(&owner.instance_id) {
            return Err(Error::tool(
                "hub",
                "PI_HUB_SESSION_RETIRED: this live service registry has shut down",
            ));
        }
        if self
            .owners
            .get(owner)
            .is_some_and(|state| state.active_shutdowns != 0 || state.incomplete_shutdown)
        {
            return Err(Error::tool(
                "hub",
                "PI_HUB_SESSION_CLOSING: this live session's services are closing",
            ));
        }
        Ok(())
    }

    fn check_request(&self, request: &ServiceRequest) -> Result<()> {
        self.ensure_owner_open(&request.service_owner)?;
        let generation = self
            .owners
            .get(&request.service_owner)
            .map_or(0, |state| state.generation);
        if generation != request.generation {
            return Err(Error::tool(
                "hub",
                "PI_HUB_STALE_SESSION: service request belongs to a retired session generation",
            ));
        }
        Ok(())
    }

    fn lookup(&self, owner: &ServiceOwner, name: &str) -> Result<&ServiceEntry> {
        let exact = self.services.get(&(owner.clone(), name.to_string()));
        if let Some(entry) = exact
            && entry.status.live()
        {
            return Ok(entry);
        }
        // Explicit detachment survives replacement, but never grants access
        // across persistent sessions. Ambiguous detached names fail closed.
        let detached = self.services.values().filter(|entry| {
            entry.owner.session_id == owner.session_id
                && entry.spec.name == name
                && entry.spec.detached
                && entry.control.startup_accepted.load(Ordering::Acquire)
        });
        // A restart retains the previous settled descriptor. That history
        // must not conceal the uniquely live detached incarnation on resume.
        let has_live = detached.clone().any(|entry| entry.status.live());
        if !has_live && let Some(entry) = exact {
            return Ok(entry);
        }
        let mut detached = detached.filter(|entry| !has_live || entry.status.live());
        let Some(entry) = detached.next() else {
            return Err(unknown_service(name));
        };
        if detached.next().is_some() {
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_AMBIGUOUS_SERVICE: more than one detached service is named '{name}'"
                ),
            ));
        }
        Ok(entry)
    }

    fn lookup_mut(&mut self, owner: &ServiceOwner, name: &str) -> Result<&mut ServiceEntry> {
        let entry = self.lookup(owner, name)?;
        let key = (entry.owner.clone(), entry.spec.name.clone());
        self.services
            .get_mut(&key)
            .ok_or_else(|| unknown_service(name))
    }

    fn ensure_accepting_starts(&self) -> Result<()> {
        if self.shutting_down {
            return Err(Error::tool(
                "hub",
                "PI_HUB_SHUTTING_DOWN: service startup is closed for process shutdown",
            ));
        }
        Ok(())
    }

    /// Reserve before any filesystem or process side effects. The ring's Arc
    /// identity is also the incarnation token, so stopped names can be reused
    /// without letting an old monitor or readiness waiter mutate the new run.
    fn reserve(
        &mut self,
        owner: &ServiceOwner,
        spec: &LaunchSpec,
        ring: &Arc<Mutex<Ring>>,
        log_path: &std::path::Path,
    ) -> Result<()> {
        self.ensure_accepting_starts()?;
        self.ensure_owner_open(owner)?;
        let key = (owner.clone(), spec.name.clone());
        if let Some(existing) = self.services.get(&key)
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
        if spec.detached
            && self.services.values().any(|entry| {
                entry.owner.session_id == owner.session_id
                    && entry.spec.name == spec.name
                    && entry.spec.detached
                    && entry.status.live()
            })
        {
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NAME_TAKEN: a live detached service named '{}' already belongs to this session",
                    spec.name
                ),
            ));
        }
        self.services.insert(
            key,
            ServiceEntry {
                owner: owner.clone(),
                spec: spec.clone(),
                status: ServiceStatus::Starting,
                pid: None,
                started_ms: now_ms(),
                exit_code: None,
                pending_status: None,
                log_path: log_path.to_path_buf(),
                ring: Arc::clone(ring),
                writer: Arc::new(Mutex::new(None)),
                control: Arc::new(ServiceControl::default()),
            },
        );
        Ok(())
    }

    fn current(&self, name: &str, ring: &Arc<Mutex<Ring>>) -> Option<&ServiceEntry> {
        self.services
            .values()
            .find(|entry| entry.spec.name == name && Arc::ptr_eq(&entry.ring, ring))
    }

    fn current_mut(&mut self, name: &str, ring: &Arc<Mutex<Ring>>) -> Option<&mut ServiceEntry> {
        self.services
            .values_mut()
            .find(|entry| entry.spec.name == name && Arc::ptr_eq(&entry.ring, ring))
    }

    fn mark_ready(&mut self, name: &str, ring: &Arc<Mutex<Ring>>) -> Result<ServiceSnapshot> {
        let entry = self
            .current_mut(name, ring)
            .ok_or_else(|| stale_service(name))?;
        if !entry.current_status().accepts_input() {
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NOT_READY: service '{name}' is {} before readiness was observed",
                    entry.current_status().as_str()
                ),
            ));
        }
        entry.status = ServiceStatus::Running;
        Ok(ServiceSnapshot::from_entry(entry))
    }

    fn begin_draining(
        &mut self,
        name: &str,
        ring: &Arc<Mutex<Ring>>,
        code: i32,
    ) -> Option<ServiceWriter> {
        let entry = self.current_mut(name, ring)?;
        entry.pending_status = Some(
            if entry.status == ServiceStatus::Stopping
                || entry.control.stop_requested.load(Ordering::Acquire)
            {
                ServiceStatus::Killed
            } else if code == 0 {
                ServiceStatus::Exited
            } else {
                ServiceStatus::Failed
            },
        );
        // Keep the name reserved until the log pump releases its resources.
        entry.status = ServiceStatus::Stopping;
        entry.pid = None;
        entry.exit_code = Some(code);
        Some(entry.close_input())
    }

    fn settle(&mut self, name: &str, ring: &Arc<Mutex<Ring>>, code: i32) -> bool {
        let Some(entry) = self.current_mut(name, ring) else {
            return false;
        };
        if let Some(status) = entry.pending_status.take() {
            entry.status = status;
        } else if entry.status == ServiceStatus::Stopping
            || entry.control.stop_requested.load(Ordering::Acquire)
        {
            entry.status = ServiceStatus::Killed;
        } else if entry.status.live() {
            entry.status = if code == 0 {
                ServiceStatus::Exited
            } else {
                ServiceStatus::Failed
            };
        }
        entry.exit_code = Some(code);
        entry.pid = None;
        entry.control.input_closed.store(true, Ordering::Release);
        entry.control.finish(ServiceSnapshot::from_entry(entry));
        true
    }

    /// A stale timeout has no authority over the current service's PID.
    fn time_out(&mut self, name: &str, ring: &Arc<Mutex<Ring>>) -> Option<Arc<ServiceControl>> {
        let entry = self.current_mut(name, ring)?;
        if !entry.status.live() {
            return None;
        }
        entry.status = ServiceStatus::Stopping;
        entry.control.request_stop(true);
        Some(Arc::clone(&entry.control))
    }
}

fn stale_service(name: &str) -> Error {
    Error::tool(
        "hub",
        format!("PI_HUB_STALE_SERVICE: service '{name}' was replaced during startup"),
    )
}

fn unknown_service(name: &str) -> Error {
    Error::tool(
        "hub",
        format!("PI_HUB_UNKNOWN_SERVICE: no service named '{name}' in this session"),
    )
}

/// A failed setup settles only its own reservation. This guard is created
/// before the child guard, so child cleanup runs before the reservation settles.
struct PendingService {
    name: String,
    ring: Arc<Mutex<Ring>>,
    control: Arc<ServiceControl>,
    armed: bool,
    output_started: bool,
}

impl PendingService {
    fn reserve(
        spec: &LaunchSpec,
        ring: &Arc<Mutex<Ring>>,
        log_path: &std::path::Path,
        request: &ServiceRequest,
    ) -> Result<Self> {
        let mut reg = registry().lock().map_err(|_| registry_err())?;
        reg.check_request(request)?;
        reg.reserve(&request.service_owner, spec, ring, log_path)?;
        let control = Arc::clone(
            &reg.current(&spec.name, ring)
                .ok_or_else(|| stale_service(&spec.name))?
                .control,
        );
        drop(reg);
        Ok(Self {
            name: spec.name.clone(),
            ring: Arc::clone(ring),
            control,
            armed: true,
            output_started: false,
        })
    }
}

impl Drop for PendingService {
    fn drop(&mut self) {
        if self.armed {
            let writer = registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .current_mut(&self.name, &self.ring)
                .map(ServiceEntry::close_input);
            release_service_input(writer);
            if !self.output_started {
                let mut ring = self
                    .ring
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                ring.capture.drain_state = LogDrainState::Failed;
                ring.capture.read_error = Some(
                    "PI_HUB_LOG_NOT_STARTED: service output capture did not start".to_string(),
                );
            }
            let mut reg = registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if reg.settle(&self.name, &self.ring, -1) {
                persist_detached_state(&reg);
            }
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
    fn monitor(mut self, control: &ServiceControl) -> i32 {
        let mut terminate_at: Option<Instant> = None;
        let pid = self.child.process_id();
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    if let Some(pid) = pid {
                        crate::tools::terminate_reaped_child_discipline(pid);
                    }
                    return i32::try_from(status.exit_code()).unwrap_or(-1);
                }
                Ok(None) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    // Wait errors do not prove that a numeric PID is still
                    // ours. The retained child handle is safe to terminate.
                    let _ = self.child.kill();
                    return self.wait();
                }
            }
            let now = Instant::now();
            if control.force_requested.load(Ordering::Acquire)
                || terminate_at.is_some_and(|deadline| now >= deadline)
            {
                crate::tools::kill_process_group_tree(self.child.process_id());
                let _ = self.child.kill();
                return self.wait();
            }
            if terminate_at.is_none() && control.stop_requested.load(Ordering::Acquire) {
                crate::tools::terminate_process_group_tree(self.child.process_id());
                terminate_at = Some(Instant::now() + TERMINATE_GRACE);
            }
            std::thread::sleep(MONITOR_POLL_INTERVAL);
        }
    }

    fn wait(mut self) -> i32 {
        self.wait_for_reap()
    }

    fn wait_for_reap(&mut self) -> i32 {
        let pid = self.child.process_id();
        loop {
            match self.child.wait() {
                Ok(status) => {
                    self.reaped = true;
                    if let Some(pid) = pid {
                        crate::tools::terminate_reaped_child_discipline(pid);
                    }
                    return i32::try_from(status.exit_code()).unwrap_or(-1);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {
                    // A failed wait is not evidence of reap. Retain ownership
                    // and keep retrying; bounded callers can report stopping,
                    // but cannot free the service name or claim completion.
                    let _ = self.child.kill();
                    std::thread::sleep(MONITOR_POLL_INTERVAL);
                }
            }
        }
    }
}

impl Drop for ServiceChild {
    fn drop(&mut self) {
        if !self.reaped {
            crate::tools::kill_process_group_tree(self.child.process_id());
            let _ = self.child.kill();
            let _ = self.wait_for_reap();
        }
    }
}

struct SpawnedService {
    child: ServiceChild,
    master: Box<dyn portable_pty::MasterPty + Send>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
}

/// The monitor owns the output thread until its final bytes and capture state
/// are published. A surviving descendant must not retain a detached reader
/// after the service is reported as finished.
struct ServiceOutputPump {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    ring: Arc<Mutex<Ring>>,
}

impl ServiceOutputPump {
    fn spawn<R, W>(name: &str, reader: R, artifact: W, ring: &Arc<Mutex<Ring>>) -> Result<Self>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let pump_stop = Arc::clone(&stop);
        let pump_ring = Arc::clone(ring);
        let handle = std::thread::Builder::new()
            .name(format!("hub-output-{name}"))
            .spawn(move || pump_service_stream(reader, artifact, &pump_ring, &pump_stop))
            .map_err(|error| {
                Error::tool(
                    "hub",
                    format!("Failed to start service output pump: {error}"),
                )
            })?;
        Ok(Self {
            stop,
            handle: Some(handle),
            ring: Arc::clone(ring),
        })
    }

    fn drain_and_join(&mut self) {
        let deadline = Instant::now() + OUTPUT_DRAIN_GRACE;
        while self
            .handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
            && Instant::now() < deadline
        {
            std::thread::park_timeout(
                IO_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        self.stop.store(true, Ordering::Release);
        handle.thread().unpark();
        // Unix readers are nonblocking and observe stop between reads. Other
        // backends may still be releasing a blocking read; retain ownership
        // and keep the service stopping instead of claiming a completed drain.
        if handle.join().is_err() {
            let mut ring = self
                .ring
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ring.finish_partial();
            ring.capture.drain_state = LogDrainState::Failed;
            ring.capture.read_error =
                Some("PI_HUB_LOG_PUMP_FAILED: service output thread panicked".to_string());
        }
    }
}

impl Drop for ServiceOutputPump {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// Keep cleanup ordering explicit on successful handoff, thread-start failure,
/// and unwinding: reap the process, close the PTY owner, join output, then make
/// the reservation available for reuse.
struct ServiceMonitorResources {
    child: Option<ServiceChild>,
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    output: Option<ServiceOutputPump>,
    pending: PendingService,
}

impl ServiceMonitorResources {
    fn run(mut self) {
        let code = self
            .child
            .take()
            .expect("service monitor owns its child")
            .monitor(&self.pending.control);
        let writer = {
            let mut reg = registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let writer = reg.begin_draining(&self.pending.name, &self.pending.ring, code);
            persist_detached_state(&reg);
            writer
        };
        release_service_input(writer);
        drop(self.master.take());
        if let Some(mut output) = self.output.take() {
            output.drain_and_join();
        }
        {
            let mut reg = registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if reg.settle(&self.pending.name, &self.pending.ring, code) {
                persist_detached_state(&reg);
            }
        }
        self.pending.armed = false;
    }
}

impl Drop for ServiceMonitorResources {
    fn drop(&mut self) {
        self.pending
            .control
            .input_closed
            .store(true, Ordering::Release);
        drop(self.child.take());
        let writer = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current_mut(&self.pending.name, &self.pending.ring)
            .map(ServiceEntry::close_input);
        release_service_input(writer);
        drop(self.master.take());
        drop(self.output.take());
        // PendingService drops after these owners, so even failed handoff
        // cannot publish a terminal snapshot while output is still changing.
    }
}

fn readiness_deadline(now: Instant, budget: Duration) -> Result<Instant> {
    now.checked_add(budget).ok_or_else(|| {
        Error::validation(
            "PI_HUB_INVALID_READY_TIMEOUT: readiness timeout is too large".to_string(),
        )
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

/// Order child creation through monitor handoff before shutdown's snapshot.
/// Independent startups retain parallel read access until shutdown closes it.
/// Never wait for this lock while retaining the registry lock: a startup owns
/// this lock first and needs the registry to publish or clean up its child.
fn startup_lock() -> &'static RwLock<()> {
    static LOCK: RwLock<()> = RwLock::new(());
    &LOCK
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

/// Retain a diagnostic roster of detached services. A numeric PID is not an
/// ownership token and is never imported for control by a later process.
fn persist_detached_state(reg: &ServiceRegistry) {
    #[derive(Serialize)]
    struct DetachedRecord {
        owner_session_id: String,
        owner_instance_id: String,
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
            owner_session_id: entry.owner.session_id.clone(),
            owner_instance_id: entry.owner.instance_id.clone(),
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

/// Spawn a service and block until readiness is observed (or the budget expires).
///
/// Both supplied gates must be observed. With no gates, a successful spawn is
/// acknowledged without resurrecting a later exit.
///
/// # Errors
/// `PI_HUB_NAME_TAKEN` for a duplicate live name; `PI_HUB_NOT_READY` when
/// the gates do not pass in time (the process is killed — no half-started
/// surprise daemons); tool errors for spawn failures.
pub fn start(owner: &ServiceOwner, spec: &LaunchSpec) -> Result<ServiceSnapshot> {
    let request = ServiceRequest::new(owner)?;
    let mut guard = ServiceRequestGuard {
        request: request.clone(),
        accepted: false,
    };
    let result = start_inner(spec, &request)
        .and_then(|snapshot| request.accept().map(|()| snapshot))
        .map_err(|error| request.cancel_and_wait(error));
    guard.accepted = result.is_ok();
    result
}

pub(crate) fn start_for_request(
    spec: &LaunchSpec,
    request: &ServiceRequest,
) -> Result<ServiceSnapshot> {
    start_inner(spec, request)
}

#[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
fn start_inner(spec: &LaunchSpec, request: &ServiceRequest) -> Result<ServiceSnapshot> {
    {
        let reg = registry().lock().map_err(|_| registry_err())?;
        reg.ensure_accepting_starts()?;
        reg.check_request(request)?;
    }
    request.check_spawn()?;
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
            format!(
                "Working directory is not a directory: {}",
                spec.cwd.display()
            ),
        ));
    }

    #[cfg(test)]
    run_startup_test_hook(&name, StartupTestStage::BeforeReservation);
    let startup = startup_lock().read().map_err(|_| registry_err())?;
    let mut output = Ring::new(RING_LINE_CAP);
    output.watch_readiness(log_regex);
    let ring = Arc::new(Mutex::new(output));
    let log_dir = hub_artifact_dir();
    let log_path = log_dir.join(format!("{name}-{}.log", uuid::Uuid::new_v4()));
    let pending = PendingService::reserve(spec, &ring, &log_path, request)?;
    request.own_startup(&name, &pending.control)?;
    std::fs::create_dir_all(&log_dir)
        .map_err(|e| Error::tool("hub", format!("Failed to create hub artifact dir: {e}")))?;
    // All fallible artifact/PTY handle setup precedes child creation.
    let artifact = create_service_log(&log_path)?;
    let SpawnedService {
        child,
        master,
        reader,
        writer,
    } = spawn_pty(spec, request)?;
    #[cfg(test)]
    run_startup_test_hook(&name, StartupTestStage::BeforePublication);
    let initial_snapshot = {
        let mut reg = registry().lock().map_err(|_| registry_err())?;
        reg.ensure_accepting_starts()?;
        reg.check_request(request)?;
        let entry = reg
            .current_mut(&name, &ring)
            .ok_or_else(|| stale_service(&name))?;
        if !entry.current_status().accepts_input() {
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

    let mut monitor = ServiceMonitorResources {
        child: Some(child),
        master: Some(master),
        output: None,
        pending,
    };
    monitor.output = Some(ServiceOutputPump::spawn(&name, reader, artifact, &ring)?);
    monitor.pending.output_started = true;
    std::thread::Builder::new()
        .name(format!("hub-monitor-{name}"))
        .spawn(move || monitor.run())
        .map_err(|error| {
            Error::tool(
                "hub",
                format!("Failed to start service exit monitor: {error}"),
            )
        })?;
    // Readiness can wait for user code or a network listener. The monitor now
    // owns the child, so shutdown can cancel it without waiting for readiness.
    drop(startup);

    // No-gate starts acknowledge the publication above, even if a very short
    // command has since exited. Never resurrect its settled registry entry.
    if !has_gates {
        return Ok(initial_snapshot);
    }

    loop {
        request.check()?;
        let status = {
            let reg = registry().lock().map_err(|_| registry_err())?;
            reg.current(&name, &ring)
                .ok_or_else(|| stale_service(&name))?
                .current_status()
        };
        if !status.accepts_input() {
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
            reg.check_request(request)?;
            let snapshot = reg.mark_ready(&name, &ring)?;
            persist_detached_state(&reg);
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            // The caller's request shares this first cleanup deadline;
            // reporting a timeout must not trigger a second full wait.
            request.cancel();
            let control = {
                let mut reg = registry().lock().map_err(|_| registry_err())?;
                let control = reg.time_out(&name, &ring);
                persist_detached_state(&reg);
                control
            };
            let cleanup = control
                .as_ref()
                .map(|control| control.wait_until(&name, Instant::now() + SETTLEMENT_BUDGET))
                .transpose();
            let cleanup_message = cleanup.map_or_else(
                |error| format!("Startup is stopping. {error}"),
                |_| "Startup was stopped.".to_string(),
            );
            let tail = ring_tail(&ring, 20);
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NOT_READY: service '{name}' failed readiness within {}s \
                     (log gate passed: {log_passed}, port gate passed: {port_passed}). \
                     {cleanup_message}\nLog tail:\n{tail}",
                    budget.as_secs()
                ),
            ));
        }
        std::thread::sleep(
            Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn spawn_pty(spec: &LaunchSpec, request: &ServiceRequest) -> Result<SpawnedService> {
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

    // The portable PTY reader preserves its EOF/EIO handling. On Unix, share
    // nonblocking mode through the existing safe duplicated-master helper so
    // neither a silent descendant nor a full input queue can strand a worker.
    #[cfg(unix)]
    crate::tools::prepare_bash_pty_master(pair.master.as_ref())
        .map_err(|error| Error::tool("hub", format!("Failed to prepare PTY I/O: {error}")))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| Error::tool("hub", format!("Failed to clone PTY reader: {e}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| Error::tool("hub", format!("Failed to open PTY writer: {e}")))?;
    request.check_spawn()?;
    #[cfg(test)]
    run_startup_test_hook(&spec.name, StartupTestStage::BeforeSpawn);
    {
        let reg = registry().lock().map_err(|_| registry_err())?;
        reg.ensure_accepting_starts()?;
        reg.check_request(request)?;
    }
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

/// Stop writing after the byte cap or the first permanent write error, but
/// continue accounting for every byte drained from the service. Never insert
/// diagnostics into the raw byte stream or the readiness matcher.
fn write_service_artifact<W: Write>(artifact: &mut W, capture: &mut LogCapture, data: &[u8]) {
    capture.observed_bytes = capture.observed_bytes.saturating_add(data.len() as u64);
    let remaining = capture.byte_limit.saturating_sub(capture.retained_bytes);
    let count = data
        .len()
        .min(usize::try_from(remaining).unwrap_or(usize::MAX));
    let mut offset = 0;
    while offset < count && capture.write_error.is_none() {
        match artifact.write(&data[offset..count]) {
            Ok(0) => {
                capture.write_error =
                    Some("PI_HUB_LOG_WRITE_FAILED: artifact writer made no progress".to_string());
            }
            Ok(written) => {
                offset += written;
                capture.retained_bytes = capture.retained_bytes.saturating_add(written as u64);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                capture.write_error = Some(format!("PI_HUB_LOG_WRITE_FAILED: {error}"));
            }
        }
    }
    capture.dropped_bytes = capture
        .observed_bytes
        .saturating_sub(capture.retained_bytes);
    capture.truncated = capture.dropped_bytes != 0;
}

// The ring is locked while a chunk is pushed and its capture counters are
// copied, so readers never see bytes without the matching counts.
#[allow(clippy::significant_drop_tightening)]
fn pump_service_stream<R: Read, W: Write>(
    mut reader: R,
    mut artifact: W,
    ring: &Mutex<Ring>,
    stop: &AtomicBool,
) {
    let mut capture = LogCapture {
        drain_state: LogDrainState::Streaming,
        ..LogCapture::default()
    };
    ring.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .capture
        .clone_from(&capture);
    let mut chunk = [0u8; 8192];
    let mut pending = Vec::with_capacity(chunk.len() + 3);
    loop {
        if stop.load(Ordering::Acquire) {
            capture.drain_state = LogDrainState::Failed;
            capture.read_error =
                Some("PI_HUB_LOG_DRAIN_INCOMPLETE: output capture stopped before EOF".to_string());
            break;
        }
        match reader.read(&mut chunk) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::park_timeout(IO_POLL_INTERVAL);
            }
            Ok(0) => {
                capture.drain_state = LogDrainState::Complete;
                break;
            }
            Err(error) => {
                capture.drain_state = LogDrainState::Failed;
                capture.read_error = Some(format!("PI_HUB_LOG_READ_FAILED: {error}"));
                break;
            }
            Ok(n) => {
                let data = &chunk[..n]; // ubs:ignore n bounded by read into chunk
                write_service_artifact(&mut artifact, &mut capture, data);
                pending.extend_from_slice(data);
                let mut ring = ring
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let consumed = push_service_bytes(&mut ring, &pending);
                ring.capture.clone_from(&capture);
                drop(pending.drain(..consumed));
            }
        }
    }
    loop {
        match artifact.flush() {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                capture
                    .write_error
                    .get_or_insert_with(|| format!("PI_HUB_LOG_FLUSH_FAILED: {error}"));
                break;
            }
            Ok(()) => break,
        }
    }
    let mut ring = ring
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ring.push_chunk(&String::from_utf8_lossy(&pending));
    ring.finish_partial();
    ring.capture = capture;
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

/// List this live owner's services and explicitly detached services retained
/// for the same durable session. Persisted PIDs are never adopted.
///
/// # Errors
/// Registry lock failure.
pub fn ps(owner: &ServiceOwner) -> Result<Vec<ServiceSnapshot>> {
    let reg = registry().lock().map_err(|_| registry_err())?;
    Ok(reg
        .services
        .values()
        .filter(|entry| {
            entry.owner == *owner
                || (entry.spec.detached
                    && entry.owner.session_id == owner.session_id
                    && entry.control.startup_accepted.load(Ordering::Acquire))
        })
        .map(ServiceSnapshot::from_entry)
        .collect())
}

/// Read service logs.
///
/// `since` returns lines newer than the cursor; `tail` returns the last N
/// lines; `grep` filters (substring, case-sensitive); `wait_ms` bounds how
/// long `logs` blocks waiting for new lines when `since` is supplied.
/// Retention is capped at 10,000 lines and 1 MiB of text. Oversized lines
/// keep a UTF-8-safe tail with an explicit truncation marker. The raw artifact
/// retains the first 16 MiB of original bytes and reports any omissions or
/// I/O failures. Eviction never rewinds the source-line cursor.
///
/// # Errors
/// `PI_HUB_UNKNOWN_SERVICE` for unknown names.
pub fn logs(
    owner: &ServiceOwner,
    name: &str,
    since: Option<u64>,
    tail: Option<usize>,
    grep: Option<&str>,
    wait_ms: u64,
) -> Result<LogPage> {
    logs_inner(
        name,
        since,
        tail,
        grep,
        wait_ms,
        &ServiceRequest::new(owner)?,
    )
}

pub(crate) fn logs_for_request(
    name: &str,
    since: Option<u64>,
    tail: Option<usize>,
    grep: Option<&str>,
    wait_ms: u64,
    request: &ServiceRequest,
) -> Result<LogPage> {
    logs_inner(name, since, tail, grep, wait_ms, request)
}

#[allow(clippy::significant_drop_tightening)]
fn logs_inner(
    name: &str,
    since: Option<u64>,
    tail: Option<usize>,
    grep: Option<&str>,
    wait_ms: u64,
    request: &ServiceRequest,
) -> Result<LogPage> {
    let deadline = Instant::now() + Duration::from_millis(wait_ms.min(60_000));
    loop {
        request.check()?;
        {
            let reg = registry().lock().map_err(|_| registry_err())?;
            reg.check_request(request)?;
            let entry = reg.lookup(&request.service_owner, name)?;
            let ring = entry.ring.lock().map_err(|_| registry_err())?;
            let (mut lines, cursor) = ring.since(since.unwrap_or(0));
            let oldest_cursor = ring.next_index.saturating_sub(ring.lines.len() as u64);
            let lost_lines = since.map_or(0, |since| oldest_cursor.saturating_sub(since));
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
            if !lines.is_empty()
                || !seeking
                || matches!(
                    ring.capture.drain_state,
                    LogDrainState::Complete | LogDrainState::Failed
                )
                || Instant::now() >= deadline
            {
                return Ok(LogPage {
                    schema: "pi.hub.logs.v1".to_string(), // ubs:ignore loop returns immediately after
                    name: name.to_string(), // ubs:ignore loop returns immediately after
                    lines,
                    cursor,
                    oldest_cursor,
                    lost_lines,
                    status: entry.current_status().as_str().to_string(), // ubs:ignore loop returns after
                    log_path: entry.log_path.display().to_string(),
                    log_capture: ring.capture.clone(),
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
/// Cancellation or `PI_HUB_INPUT_TIMEOUT` can follow partial input delivery.
pub fn send_text(owner: &ServiceOwner, name: &str, text: &str, enter: bool) -> Result<()> {
    send_text_for_request(name, text, enter, &ServiceRequest::new(owner)?)
}

pub(crate) fn send_text_for_request(
    name: &str,
    text: &str,
    enter: bool,
    request: &ServiceRequest,
) -> Result<()> {
    write_to_master(
        name,
        &[text.as_bytes(), if enter { b"\r" } else { b"" }],
        request,
    )
}

/// Send named keys: ENTER, TAB, ESCAPE, CTRL_C, CTRL_D, UP, DOWN, LEFT,
/// RIGHT.
///
/// # Errors
/// Named validation error for unknown key names.
pub fn send_keys(owner: &ServiceOwner, name: &str, keys: &[String]) -> Result<()> {
    send_keys_for_request(name, keys, &ServiceRequest::new(owner)?)
}

pub(crate) fn send_keys_for_request(
    name: &str,
    keys: &[String],
    request: &ServiceRequest,
) -> Result<()> {
    // Map every key up front so the write loop carries no allocation.
    let mapped: Vec<&[u8]> = keys
        .iter()
        .map(|key| key_bytes(key))
        .collect::<Result<_>>()?;
    if mapped.is_empty() {
        return request.check();
    }
    // Retain one incarnation and one deadline across the entire key sequence.
    write_to_master(name, &mapped, request)
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
fn write_to_master(name: &str, chunks: &[&[u8]], request: &ServiceRequest) -> Result<()> {
    let deadline = Instant::now() + INPUT_BUDGET;
    request.check()?;
    let (writer, control) = {
        let reg = registry().lock().map_err(|_| registry_err())?;
        reg.check_request(request)?;
        let entry = reg.lookup(&request.service_owner, name)?;
        if !entry.current_status().accepts_input() {
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NOT_RUNNING: service '{name}' is {} — stdin is closed",
                    entry.current_status().as_str()
                ),
            ));
        }
        (Arc::clone(&entry.writer), Arc::clone(&entry.control))
    };
    let mut guard = loop {
        input_checkpoint(name, &control, request, deadline)?;
        match writer.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(_)) => return Err(registry_err()),
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::park_timeout(IO_POLL_INTERVAL);
            }
        }
    };
    let Some(writer) = guard.as_mut() else {
        return Err(Error::tool(
            "hub",
            format!("PI_HUB_NO_INPUT: service '{name}' has no writable PTY master"),
        ));
    };
    for chunk in chunks {
        let mut remaining = *chunk;
        while !remaining.is_empty() {
            input_checkpoint(name, &control, request, deadline)?;
            match writer.write(remaining) {
                Ok(0) => {
                    return Err(Error::tool(
                        "hub",
                        "PI_HUB_INPUT_FAILED: service stdin writer made no progress",
                    ));
                }
                Ok(written) => remaining = &remaining[written..],
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::park_timeout(IO_POLL_INTERVAL);
                }
                Err(error) => {
                    return Err(Error::tool(
                        "hub",
                        format!("Failed to write to service stdin: {error}"),
                    ));
                }
            }
        }
    }
    loop {
        input_checkpoint(name, &control, request, deadline)?;
        match writer.flush() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::park_timeout(IO_POLL_INTERVAL);
            }
            Err(error) => {
                return Err(Error::tool(
                    "hub",
                    format!("Failed to flush service stdin: {error}"),
                ));
            }
        }
    }
}

fn input_checkpoint(
    name: &str,
    control: &ServiceControl,
    request: &ServiceRequest,
    deadline: Instant,
) -> Result<()> {
    request.check()?;
    if control.input_closed.load(Ordering::Acquire) {
        return Err(Error::tool(
            "hub",
            format!("PI_HUB_NOT_RUNNING: service '{name}' stdin is closed"),
        ));
    }
    if Instant::now() >= deadline {
        return Err(Error::tool(
            "hub",
            format!(
                "PI_HUB_INPUT_TIMEOUT: service '{name}' did not accept input within the \
                 send deadline; some input may already have been written"
            ),
        ));
    }
    Ok(())
}

/// Send a signal to the service's process tree.
///
/// # Errors
/// `PI_HUB_UNKNOWN_SERVICE` / `PI_HUB_NOT_RUNNING`.
#[allow(clippy::significant_drop_tightening)]
pub fn send_signal(owner: &ServiceOwner, name: &str, signal: sysinfo::Signal) -> Result<()> {
    send_signal_for_request(name, signal, &ServiceRequest::new(owner)?)
}

pub(crate) fn send_signal_for_request(
    name: &str,
    signal: sysinfo::Signal,
    request: &ServiceRequest,
) -> Result<()> {
    request.check()?;
    let pid = {
        let mut reg = registry().lock().map_err(|_| registry_err())?;
        reg.check_request(request)?;
        let entry = reg.lookup_mut(&request.service_owner, name)?;
        if !entry.status.live() {
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NOT_RUNNING: service '{name}' is {}",
                    entry.status.as_str()
                ),
            ));
        }
        if matches!(signal, sysinfo::Signal::Term | sysinfo::Signal::Kill) {
            entry.status = ServiceStatus::Stopping;
            entry.control.request_stop(signal == sysinfo::Signal::Kill);
            persist_detached_state(&reg);
            return Ok(());
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
/// `PI_HUB_UNKNOWN_SERVICE` / `PI_HUB_NOT_RUNNING`; `PI_HUB_STOP_TIMEOUT`
/// if cleanup is still pending after the bounded wait. Such services remain
/// `stopping`, retain their name, and may be awaited by another `stop` call.
#[allow(clippy::significant_drop_tightening)]
pub fn stop(owner: &ServiceOwner, name: &str) -> Result<ServiceSnapshot> {
    stop_for_request(name, &ServiceRequest::new(owner)?)
}

pub(crate) fn stop_for_request(name: &str, request: &ServiceRequest) -> Result<ServiceSnapshot> {
    request.check()?;
    let control = {
        let mut reg = registry().lock().map_err(|_| registry_err())?;
        reg.check_request(request)?;
        let entry = reg.lookup_mut(&request.service_owner, name)?;
        if !entry.status.live() {
            return Err(Error::tool(
                "hub",
                format!(
                    "PI_HUB_NOT_RUNNING: service '{name}' already settled ({})",
                    entry.status.as_str()
                ),
            ));
        }
        entry.status = ServiceStatus::Stopping;
        entry.control.request_stop(false);
        Arc::clone(&entry.control)
    };
    control.wait_until(name, Instant::now() + SETTLEMENT_BUDGET)
}

/// Restart reuses the retained launch spec. Running services are stopped
/// first; completed names re-spawn directly.
/// A detached service resumed from an older live registry is stopped under
/// its original control, then the replacement belongs to the requesting owner.
///
/// # Errors
/// `PI_HUB_UNKNOWN_SERVICE` for unknown names; start errors otherwise.
pub fn restart(owner: &ServiceOwner, name: &str) -> Result<ServiceSnapshot> {
    let request = ServiceRequest::new(owner)?;
    let mut guard = ServiceRequestGuard {
        request: request.clone(),
        accepted: false,
    };
    let result = restart_inner(name, &request)
        .and_then(|snapshot| request.accept().map(|()| snapshot))
        .map_err(|error| request.cancel_and_wait(error));
    guard.accepted = result.is_ok();
    result
}

pub(crate) fn restart_for_request(name: &str, request: &ServiceRequest) -> Result<ServiceSnapshot> {
    restart_inner(name, request)
}

#[allow(clippy::significant_drop_tightening)]
fn restart_inner(name: &str, request: &ServiceRequest) -> Result<ServiceSnapshot> {
    request.check_spawn()?;
    let (spec, control) = {
        let mut reg = registry().lock().map_err(|_| registry_err())?;
        reg.ensure_accepting_starts()?;
        reg.check_request(request)?;
        let entry = reg.lookup_mut(&request.service_owner, name)?;
        let control = if entry.status.live() {
            entry.status = ServiceStatus::Stopping;
            entry.control.request_stop(false);
            Some(Arc::clone(&entry.control))
        } else {
            None
        };
        (entry.spec.clone(), control)
    };
    if let Some(control) = control {
        control.wait_until(name, Instant::now() + SETTLEMENT_BUDGET)?;
    }
    start_inner(&spec, request)
}

/// Full descriptor for one service.
///
/// # Errors
/// `PI_HUB_UNKNOWN_SERVICE` for unknown names.
#[allow(clippy::significant_drop_tightening)]
pub fn describe(owner: &ServiceOwner, name: &str) -> Result<ServiceSnapshot> {
    let reg = registry().lock().map_err(|_| registry_err())?;
    let entry = reg.lookup(owner, name)?;
    Ok(ServiceSnapshot::from_entry(entry))
}

/// An already-published shutdown fence and its exact service incarnations.
/// Dropping this token keeps admission fenced and stop requests effective;
/// cleanup must be retried before this owner may admit new services.
pub struct ServiceShutdown {
    owner: ServiceOwner,
    generation: u64,
    controls: Vec<(String, Arc<ServiceControl>)>,
    deadline: Instant,
    finished: bool,
}

impl ServiceShutdown {
    /// Await process reap and output cleanup without blocking the executor.
    /// All services share the deadline established when the fence was begun.
    /// A clean finish permits fresh requests for this owner; older requests
    /// remain invalid because their captured generation cannot advance.
    /// Permanently retired registries stay closed after cleanup succeeds.
    ///
    /// # Errors
    /// Returns an error if the cleanup worker cannot start or any service
    /// does not settle within the original shared shutdown budget.
    pub async fn finish(self) -> Result<usize> {
        service_worker(move || {
            let mut shutdown = self;
            let mut failures = Vec::new();
            for (name, control) in &shutdown.controls {
                if let Err(error) = control.wait_until(name, shutdown.deadline) {
                    failures.push(error.to_string());
                }
            }
            let count = shutdown.controls.len();
            shutdown.complete(failures.is_empty());
            if failures.is_empty() {
                Ok(count)
            } else {
                Err(Error::tool(
                    "hub",
                    format!("PI_HUB_SESSION_STOP_INCOMPLETE: {}", failures.join("; ")),
                ))
            }
        })
        .await?
    }

    fn complete(&mut self, clean: bool) {
        if self.finished {
            return;
        }
        let mut reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(state) = reg.owners.get_mut(&self.owner) {
            state.active_shutdowns = state.active_shutdowns.saturating_sub(1);
            if clean && state.generation == self.generation {
                state.incomplete_shutdown = false;
            }
        }
        self.finished = true;
    }
}

impl Drop for ServiceShutdown {
    fn drop(&mut self) {
        self.complete(false);
    }
}

/// Fence this live owner synchronously and request every owned stop before
/// returning. Call after a session transition commits, then finish outside
/// session locks. Other registries, even those loading the same persistent
/// session ID, retain their services and startup authority.
///
/// # Errors
/// Returns a registry-lock or exhausted-generation error before publishing
/// a partial shutdown fence.
pub fn begin_session_shutdown(owner: &ServiceOwner) -> Result<ServiceShutdown> {
    begin_shutdown(owner, false)
}

/// Permanently retire a consumed SDK handle's live registry, including any
/// owner lookup that has not resolved yet. Fresh handles loading the same
/// persisted session have a different registry identity and remain usable.
/// The durable ID may be empty when its store cannot be read: this operation
/// uses the live registry identity and retires every session it hosted.
///
/// # Errors
/// Returns a registry-lock or exhausted-generation error before publishing
/// a partial shutdown fence.
pub fn begin_registry_shutdown(owner: &ServiceOwner) -> Result<ServiceShutdown> {
    begin_shutdown(owner, true)
}

fn begin_shutdown(owner: &ServiceOwner, retire_registry: bool) -> Result<ServiceShutdown> {
    let mut reg = registry().lock().map_err(|_| registry_err())?;
    let instance_generation = reg
        .instance_generation(owner.instance_id())
        .checked_add(1)
        .ok_or_else(|| Error::tool("hub", "PI_HUB_SESSION_LIMIT: registry generation exhausted"))?;
    let state = reg.owners.entry(owner.clone()).or_default();
    let generation = state
        .generation
        .checked_add(1)
        .ok_or_else(|| Error::tool("hub", "PI_HUB_SESSION_LIMIT: service generation exhausted"))?;
    let active_shutdowns = state
        .active_shutdowns
        .checked_add(1)
        .ok_or_else(|| Error::tool("hub", "PI_HUB_SESSION_LIMIT: too many shutdown attempts"))?;
    state.generation = generation;
    state.active_shutdowns = active_shutdowns;
    state.incomplete_shutdown = true;
    reg.instance_generations
        .insert(owner.instance_id.clone(), instance_generation);
    if retire_registry {
        reg.retired_instances.insert(owner.instance_id.clone());
    }
    let controls = reg
        .services
        .values_mut()
        .filter(|entry| {
            (entry.owner == *owner
                || (retire_registry && entry.owner.instance_id == owner.instance_id))
                && entry.status.live()
                && (!entry.spec.detached || !entry.control.startup_accepted.load(Ordering::Acquire))
        })
        .map(|entry| {
            entry.status = ServiceStatus::Stopping;
            entry.control.request_stop(true);
            (entry.spec.name.clone(), Arc::clone(&entry.control))
        })
        .collect();
    Ok(ServiceShutdown {
        owner: owner.clone(),
        generation,
        controls,
        deadline: Instant::now() + SETTLEMENT_BUDGET,
        finished: false,
    })
}

/// Kill the current non-detached services, leaving admission open for later
/// starts. Use [`shutdown_session_services`] for final process shutdown.
// The registry lock covers both marking the victims and persisting that
// state, so the detached-state file never records a half-applied stop.
#[allow(clippy::significant_drop_tightening)]
pub fn kill_session_services() {
    let startup = startup_lock()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let controls = {
        let mut reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let victims: Vec<_> = reg
            .services
            .values_mut()
            .filter(|entry| {
                entry.status.live()
                    && (!entry.spec.detached
                        || !entry.control.startup_accepted.load(Ordering::Acquire))
            })
            .map(|entry| {
                entry.status = ServiceStatus::Stopping;
                entry.control.request_stop(true);
                (entry.spec.name.clone(), Arc::clone(&entry.control))
            })
            .collect();
        persist_detached_state(&reg);
        victims
    };
    drop(startup);
    let deadline = Instant::now() + SETTLEMENT_BUDGET;
    for (name, control) in controls {
        // All victims receive their request before any wait. A foreign OS
        // wait that exceeds the shared bound remains truthfully `stopping`.
        let _ = control.wait_until(&name, deadline);
    }
}

/// Permanently close service admission, then stop all non-detached services.
///
/// The fence is published before waiting for an in-flight startup. That
/// startup either refuses before child creation, reaps its unpublished child,
/// or hands ownership to the monitor before the shutdown snapshot is taken.
/// Existing detached services retain their explicit cleanup exemption, but no
/// new starts or restarts (including detached ones) are admitted after this.
pub fn shutdown_session_services() {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .shutting_down = true;
    kill_session_services();
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum StartupTestStage {
    BeforeReservation,
    BeforeSpawn,
    BeforePublication,
}

#[cfg(test)]
type StartupTestHook = Arc<dyn Fn(StartupTestStage) + Send + Sync>;

#[cfg(test)]
fn startup_test_hook() -> &'static Mutex<Option<(String, StartupTestHook)>> {
    static HOOK: Mutex<Option<(String, StartupTestHook)>> = Mutex::new(None);
    &HOOK
}

#[cfg(test)]
fn run_startup_test_hook(name: &str, stage: StartupTestStage) {
    let hook = startup_test_hook()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .filter(|(owner, _)| owner == name)
        .map(|(_, hook)| Arc::clone(hook));
    if let Some(hook) = hook {
        hook(stage);
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

    fn test_owner() -> ServiceOwner {
        ServiceOwner::new(
            "hub-unit-session".to_string(),
            "hub-unit-instance".to_string(),
        )
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .blocking_threads(1, 4)
            .build()
            .expect("hub test runtime")
            .block_on(future)
    }

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
            assert_eq!(
                ring.bytes,
                ring.lines.iter().map(String::len).sum::<usize>()
            );
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
                pump_service_stream(reader, &mut artifact, &ring, &AtomicBool::new(false));
                let ring = ring.into_inner().expect("ring");
                assert_eq!(artifact, input);
                assert_eq!(ring.since(0).0, vec!["first", "prêt 界🙂 tail"]);
                assert_eq!(ring.next_index, 2);
                assert!(ring.partial.is_empty());
                assert_eq!(ring.capture.observed_bytes, input.len() as u64);
                assert_eq!(ring.capture.retained_bytes, input.len() as u64);
                assert_eq!(ring.capture.dropped_bytes, 0);
                assert!(!ring.capture.truncated);
                assert!(ring.capture.write_error.is_none());
                assert_eq!(
                    ring.capture.drain_state,
                    if terminal_error {
                        LogDrainState::Failed
                    } else {
                        LogDrainState::Complete
                    }
                );
                assert_eq!(ring.capture.read_error.is_some(), terminal_error);
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
            pump_service_stream(reader, &mut artifact, &ring, &AtomicBool::new(false));
            assert_eq!(artifact, input);
            assert_eq!(ring.into_inner().expect("ring").since(0).0, expected);
        }
    }

    #[test]
    fn stream_pump_retains_raw_artifact_when_text_is_truncated() {
        let input = format!("{}final", "x".repeat(RING_BYTE_CAP * 2));
        let ring = Mutex::new(Ring::new(8));
        let mut artifact = Vec::new();
        pump_service_stream(
            input.as_bytes(),
            &mut artifact,
            &ring,
            &AtomicBool::new(false),
        );
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
    fn capped_artifact_keeps_draining_and_observes_readiness_after_the_cap() {
        let prefix_len = SERVICE_LOG_BYTE_CAP + 8192;
        let tail = b"\nready-after-artifact-cap\nlast line";
        let reader = std::io::repeat(b'x')
            .take(prefix_len)
            .chain(tail.as_slice());
        let mut output = Ring::new(8);
        output.watch_readiness(Some(
            regex::Regex::new("(?m)^ready-after-artifact-cap$").expect("regex"),
        ));
        let ring = Mutex::new(output);
        let mut artifact = Vec::new();
        pump_service_stream(reader, &mut artifact, &ring, &AtomicBool::new(false));
        assert_eq!(artifact.len() as u64, SERVICE_LOG_BYTE_CAP);
        assert!(artifact.iter().all(|byte| *byte == b'x'));
        let ring = ring.into_inner().expect("ring");
        assert!(ring.ready_log_passed);
        let (lines, cursor) = ring.since(0);
        assert_eq!(cursor, 3);
        assert_eq!(&lines[1..], &["ready-after-artifact-cap", "last line"]);
        assert_eq!(ring.capture.observed_bytes, prefix_len + tail.len() as u64);
        assert_eq!(ring.capture.retained_bytes, SERVICE_LOG_BYTE_CAP);
        assert_eq!(ring.capture.dropped_bytes, 8192 + tail.len() as u64);
        assert!(ring.capture.truncated);
        assert_eq!(ring.capture.drain_state, LogDrainState::Complete);
        assert!(ring.capture.read_error.is_none());
        assert!(ring.capture.write_error.is_none());
    }

    /// Exercise the real pump's short-write and failure paths without an
    /// unreliable host-level disk exhaustion dependency.
    #[allow(clippy::struct_excessive_bools)]
    struct FailingArtifact {
        bytes: Vec<u8>,
        remaining: usize,
        interrupt_next: bool,
        fail_with_zero: bool,
        failed: bool,
        writes_after_failure: usize,
        flush_error: bool,
    }

    impl Write for FailingArtifact {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.failed {
                self.writes_after_failure += 1;
            }
            if self.interrupt_next {
                self.interrupt_next = false;
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            if self.remaining == 0 {
                self.failed = true;
                return if self.fail_with_zero {
                    Ok(0)
                } else {
                    Err(std::io::Error::other("artifact storage unavailable"))
                };
            }
            let written = bytes.len().min(self.remaining).min(3);
            self.bytes.extend_from_slice(&bytes[..written]);
            self.remaining -= written;
            self.interrupt_next = true;
            Ok(written)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.flush_error {
                Err(std::io::Error::other("artifact flush failed"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn artifact_write_failures_report_actual_prefix_and_keep_draining() {
        let input = b"first line\nready after write failure\nlast line";
        let ready_log = regex::Regex::new("(?m)^ready after write failure$").expect("regex");
        for fail_with_zero in [false, true] {
            let mut artifact = FailingArtifact {
                bytes: Vec::new(),
                remaining: 7,
                interrupt_next: true,
                fail_with_zero,
                failed: false,
                writes_after_failure: 0,
                flush_error: false,
            };
            let reader = FragmentedReader {
                bytes: input,
                width: 5,
                interrupt_next: true,
                terminal_error: false,
            };
            let mut output = Ring::new(8);
            output.watch_readiness(Some(ready_log.clone()));
            let ring = Mutex::new(output);
            pump_service_stream(reader, &mut artifact, &ring, &AtomicBool::new(false));
            assert_eq!(artifact.bytes, &input[..7]);
            assert_eq!(artifact.writes_after_failure, 0);
            let ring = ring.into_inner().expect("ring");
            assert!(ring.ready_log_passed);
            assert_eq!(
                ring.since(0).0,
                ["first line", "ready after write failure", "last line"]
            );
            assert_eq!(ring.capture.observed_bytes, input.len() as u64);
            assert_eq!(ring.capture.retained_bytes, 7);
            assert_eq!(ring.capture.dropped_bytes, input.len() as u64 - 7);
            assert!(ring.capture.truncated);
            assert_eq!(ring.capture.drain_state, LogDrainState::Complete);
            assert!(ring.capture.read_error.is_none());
            assert!(
                ring.capture
                    .write_error
                    .as_deref()
                    .is_some_and(|error| error.contains("PI_HUB_LOG_WRITE_FAILED"))
            );
        }
    }

    #[test]
    fn artifact_flush_failure_does_not_claim_an_error_free_capture() {
        let input = b"all bytes accepted\n";
        let mut artifact = FailingArtifact {
            bytes: Vec::new(),
            remaining: input.len(),
            interrupt_next: true,
            fail_with_zero: false,
            failed: false,
            writes_after_failure: 0,
            flush_error: true,
        };
        let ring = Mutex::new(Ring::new(8));
        pump_service_stream(
            input.as_slice(),
            &mut artifact,
            &ring,
            &AtomicBool::new(false),
        );
        assert_eq!(artifact.bytes, input);
        let capture = ring.into_inner().expect("ring").capture;
        assert_eq!(capture.observed_bytes, input.len() as u64);
        assert_eq!(capture.retained_bytes, input.len() as u64);
        assert_eq!(capture.dropped_bytes, 0);
        assert!(!capture.truncated);
        assert_eq!(capture.drain_state, LogDrainState::Complete);
        assert!(capture.read_error.is_none());
        assert!(
            capture
                .write_error
                .as_deref()
                .is_some_and(|error| error.contains("PI_HUB_LOG_FLUSH_FAILED"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn process_settlement_waits_for_output_and_preserves_the_reaped_outcome() {
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().expect("stream pair");
        reader.set_nonblocking(true).expect("nonblocking reader");
        let ring = Arc::new(Mutex::new(Ring::new(8)));
        let mut reg = ServiceRegistry::default();
        let launch = spec("hub-output-settlement", "unused", &[], None);
        reg.reserve(
            &test_owner(),
            &launch,
            &ring,
            &PathBuf::from("settlement.log"),
        )
        .expect("reserve");
        let dir = tempfile::tempdir().expect("temp dir");
        let log_path = dir.path().join("output.log");
        let artifact = create_service_log(&log_path).expect("artifact");
        let mut pump =
            ServiceOutputPump::spawn(&launch.name, reader, artifact, &ring).expect("output pump");
        writer.write_all(b"before exit\n").expect("first bytes");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if ring.lock().expect("ring").capture.observed_bytes != 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "reader must consume the first bytes"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        release_service_input(reg.begin_draining(&launch.name, &ring, 0));
        let snapshot =
            ServiceSnapshot::from_entry(reg.current(&launch.name, &ring).expect("entry"));
        assert_eq!(snapshot.status, "stopping");
        assert_eq!(snapshot.log_capture.drain_state, LogDrainState::Streaming);
        assert!(
            reg.reserve(&test_owner(), &launch, &ring, &log_path)
                .is_err()
        );
        let control = Arc::clone(&reg.current(&launch.name, &ring).expect("entry").control);
        assert!(control.completion.lock().expect("completion").is_none());
        // A stop arriving during output cleanup must not rewrite an already
        // observed normal exit as a killed process.
        control.request_stop(true);
        writer.write_all(b"trailing output").expect("tail bytes");
        drop(writer);
        pump.drain_and_join();
        assert!(pump.handle.is_none(), "the output owner must be joined");
        assert!(reg.settle(&launch.name, &ring, 0));
        assert_eq!(
            std::fs::read(log_path).expect("artifact"),
            b"before exit\ntrailing output"
        );
        let snapshot =
            ServiceSnapshot::from_entry(reg.current(&launch.name, &ring).expect("entry"));
        assert_eq!(snapshot.status, "exited");
        assert_eq!(snapshot.log_capture.drain_state, LogDrainState::Complete);
        let completed = control
            .wait_until(&launch.name, Instant::now())
            .expect("settled completion");
        assert_eq!(completed.log_capture.drain_state, LogDrainState::Complete);
        assert_eq!(
            completed.log_capture.observed_bytes,
            snapshot.log_capture.observed_bytes
        );
        assert_eq!(
            ring.lock().expect("ring").since(1),
            (vec!["trailing output".to_string()], 2)
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_owner_closes_a_retained_descriptor_on_drain_expiry_or_failed_handoff() {
        for drain in [true, false] {
            let (reader, mut retained_slave) =
                std::os::unix::net::UnixStream::pair().expect("stream pair");
            reader.set_nonblocking(true).expect("nonblocking reader");
            retained_slave
                .set_read_timeout(Some(Duration::from_millis(250)))
                .expect("bounded peer read");
            let ring = Arc::new(Mutex::new(Ring::new(8)));
            let mut pump =
                ServiceOutputPump::spawn("hub-retained-output", reader, std::io::sink(), &ring)
                    .expect("pump");
            let partial = b"tail \xf0\x9f";
            retained_slave.write_all(partial).expect("partial output");
            let deadline = Instant::now() + Duration::from_secs(5);
            while ring.lock().expect("ring").capture.observed_bytes != partial.len() as u64 {
                assert!(
                    Instant::now() < deadline,
                    "pump must observe partial output"
                );
                std::thread::sleep(IO_POLL_INTERVAL);
            }
            let started = Instant::now();
            if drain {
                pump.drain_and_join();
            }
            drop(pump);
            assert!(
                started.elapsed() < OUTPUT_DRAIN_GRACE + Duration::from_secs(2),
                "a retained descriptor must not strand the output owner"
            );
            let output = ring.lock().expect("ring");
            assert_eq!(output.capture.drain_state, LogDrainState::Failed);
            assert!(
                output
                    .capture
                    .read_error
                    .as_deref()
                    .is_some_and(|error| error.contains("PI_HUB_LOG_DRAIN_INCOMPLETE"))
            );
            assert_eq!(output.since(0).0, vec!["tail \u{fffd}"]);
            drop(output);
            assert_eq!(
                retained_slave.read(&mut [0u8; 1]).expect("reader closed"),
                0,
                "joining the pump must release the actual reader descriptor"
            );
        }
    }

    struct PartialInput {
        bytes: Arc<Mutex<Vec<u8>>>,
        attempts: usize,
        flush_blocked: bool,
    }

    impl Write for PartialInput {
        fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
            self.attempts += 1;
            match self.attempts {
                1 => Err(std::io::ErrorKind::Interrupted.into()),
                3 => Err(std::io::ErrorKind::WouldBlock.into()),
                _ => {
                    let count = input.len().min(2);
                    self.bytes
                        .lock()
                        .expect("input bytes")
                        .extend_from_slice(&input[..count]);
                    Ok(count)
                }
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if std::mem::replace(&mut self.flush_blocked, false) {
                Err(std::io::ErrorKind::WouldBlock.into())
            } else {
                Ok(())
            }
        }
    }

    fn reserve_input_fixture(name: &str, writer: Box<dyn Write + Send>) -> PendingService {
        let launch = spec(name, "unused", &[], None);
        let ring = Arc::new(Mutex::new(Ring::new(8)));
        let request = ServiceRequest::new(&test_owner()).expect("input owner");
        let pending =
            PendingService::reserve(&launch, &ring, &PathBuf::from("input.log"), &request)
                .expect("input reservation");
        registry()
            .lock()
            .expect("registry")
            .current_mut(name, &ring)
            .expect("input fixture")
            .writer = Arc::new(Mutex::new(Some(writer)));
        pending
    }

    #[test]
    fn service_input_retries_partial_writes_without_duplicate_or_missing_bytes() {
        let _lock = test_lock();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let pending = reserve_input_fixture(
            "hub-partial-input",
            Box::new(PartialInput {
                bytes: Arc::clone(&bytes),
                attempts: 0,
                flush_blocked: true,
            }),
        );
        send_text(&test_owner(), &pending.name, "prêt 界🙂", true).expect("partial text writes");
        send_keys(
            &test_owner(),
            &pending.name,
            &["UP".to_string(), "ENTER".to_string()],
        )
        .expect("partial key writes");
        assert_eq!(
            &*bytes.lock().expect("input bytes"),
            "prêt 界🙂\r\x1b[A\r".as_bytes()
        );
    }

    struct StalledInput {
        bytes: Arc<Mutex<Vec<u8>>>,
        blocked: Option<std::sync::mpsc::SyncSender<()>>,
    }

    impl Write for StalledInput {
        fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
            let mut bytes = self.bytes.lock().expect("input bytes");
            if bytes.is_empty() {
                let count = input.len().min(2);
                bytes.extend_from_slice(&input[..count]);
                return Ok(count);
            }
            drop(bytes);
            if let Some(blocked) = self.blocked.take() {
                blocked.send(()).expect("blocked input observation");
            }
            Err(std::io::ErrorKind::WouldBlock.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn cancellation_interrupts_backpressured_input_without_stopping_the_service() {
        let _lock = test_lock();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (blocked_tx, blocked_rx) = std::sync::mpsc::sync_channel(1);
        let pending = reserve_input_fixture(
            "hub-cancel-input",
            Box::new(StalledInput {
                bytes: Arc::clone(&bytes),
                blocked: Some(blocked_tx),
            }),
        );
        let request = ServiceRequest::new(&test_owner()).expect("request owner");
        let worker_request = request.clone();
        let name = pending.name.clone();
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let result = send_text_for_request(&name, "blocked input", true, &worker_request);
            done_tx.send(result).expect("send result");
        });
        blocked_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("backpressure reached");
        request.cancel();
        let error = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("cancelled input returns promptly")
            .expect_err("cancelled input");
        worker.join().expect("input worker");
        assert!(error.to_string().contains("PI_HUB_CANCELLED"));
        assert_eq!(&*bytes.lock().expect("input bytes"), b"bl");
        assert!(!pending.control.stop_requested.load(Ordering::Acquire));
        assert_eq!(
            describe(&test_owner(), &pending.name)
                .expect("live service")
                .status,
            "starting"
        );
    }

    #[test]
    fn reservation_rejects_duplicate_before_a_pid_exists() {
        let mut reg = ServiceRegistry::default();
        let launch = spec("hub-reserved", "unused", &[], None);
        let first = Arc::new(Mutex::new(Ring::new(8)));
        let second = Arc::new(Mutex::new(Ring::new(8)));
        reg.reserve(&test_owner(), &launch, &first, &PathBuf::from("first.log"))
            .expect("reserve");
        let error = reg
            .reserve(
                &test_owner(),
                &launch,
                &second,
                &PathBuf::from("second.log"),
            )
            .expect_err("pending launch owns the name");
        assert!(error.to_string().contains("PI_HUB_NAME_TAKEN"));
        assert!(
            reg.current(&launch.name, &first)
                .expect("first")
                .pid
                .is_none()
        );
        assert!(reg.current(&launch.name, &second).is_none());
    }

    #[test]
    fn cleanup_timeout_keeps_name_reserved_until_incarnation_completes() {
        let mut reg = ServiceRegistry::default();
        let launch = spec("hub-cleanup-pending", "unused", &[], None);
        let old = Arc::new(Mutex::new(Ring::new(8)));
        let new = Arc::new(Mutex::new(Ring::new(8)));
        reg.reserve(&test_owner(), &launch, &old, &PathBuf::from("old.log"))
            .expect("old reservation");
        reg.current_mut(&launch.name, &old).expect("entry").pid = Some(42);
        let control = reg.time_out(&launch.name, &old).expect("stop request");
        let error = control
            .wait_until(&launch.name, Instant::now())
            .expect_err("cleanup has not completed");
        assert!(error.to_string().contains("PI_HUB_STOP_TIMEOUT"));
        let pending = reg.current(&launch.name, &old).expect("old entry");
        assert_eq!(pending.status, ServiceStatus::Stopping);
        assert_eq!(pending.pid, Some(42));
        assert_eq!(pending.exit_code, None);
        assert!(reg.mark_ready(&launch.name, &old).is_err());
        assert!(
            reg.reserve(&test_owner(), &launch, &new, &PathBuf::from("new.log"))
                .unwrap_err()
                .to_string()
                .contains("PI_HUB_NAME_TAKEN")
        );

        assert!(reg.settle(&launch.name, &old, 137));
        reg.reserve(&test_owner(), &launch, &new, &PathBuf::from("new.log"))
            .expect("reaped service releases its name");
        let completed = control
            .wait_until(&launch.name, Instant::now())
            .expect("completion survives replacement");
        assert_eq!(completed.status, "killed");
        assert_eq!(completed.exit_code, Some(137));
        assert_eq!(completed.pid, None);
        assert_eq!(completed.log_path, "old.log");
        assert_eq!(
            reg.current(&launch.name, &new).expect("new entry").status,
            ServiceStatus::Starting
        );
    }

    #[test]
    fn stale_exit_readiness_and_timeout_cannot_mutate_replacement() {
        let mut reg = ServiceRegistry::default();
        let launch = spec("hub-generation", "unused", &[], None);
        let old = Arc::new(Mutex::new(Ring::new(8)));
        let new = Arc::new(Mutex::new(Ring::new(8)));
        reg.reserve(&test_owner(), &launch, &old, &PathBuf::from("old.log"))
            .expect("old");
        reg.time_out(&launch.name, &old);
        assert!(reg.settle(&launch.name, &old, 137));
        reg.reserve(&test_owner(), &launch, &new, &PathBuf::from("new.log"))
            .expect("new");
        reg.current_mut(&launch.name, &new).expect("new entry").pid = Some(42);
        reg.mark_ready(&launch.name, &new).expect("ready");
        assert!(!reg.settle(&launch.name, &old, 1));
        assert!(
            reg.mark_ready(&launch.name, &old)
                .unwrap_err()
                .to_string()
                .contains("PI_HUB_STALE_SERVICE")
        );
        assert!(reg.time_out(&launch.name, &old).is_none());
        let current = reg.current(&launch.name, &new).expect("replacement");
        assert_eq!(current.status, ServiceStatus::Running);
        assert_eq!(current.pid, Some(42));
        assert_eq!(current.exit_code, None);
        assert_eq!(current.log_path, PathBuf::from("new.log"));
    }

    #[test]
    fn readiness_cannot_resurrect_any_terminal_status() {
        for terminal in [
            ServiceStatus::Stopping,
            ServiceStatus::Exited,
            ServiceStatus::Failed,
            ServiceStatus::Killed,
        ] {
            let mut reg = ServiceRegistry::default();
            let launch = spec("hub-terminal", "unused", &[], None);
            let ring = Arc::new(Mutex::new(Ring::new(8)));
            reg.reserve(
                &test_owner(),
                &launch,
                &ring,
                &PathBuf::from("terminal.log"),
            )
            .expect("reserve");
            reg.current_mut(&launch.name, &ring).expect("entry").status = terminal;
            assert!(reg.mark_ready(&launch.name, &ring).is_err());
            assert_eq!(
                reg.current(&launch.name, &ring).expect("entry").status,
                terminal
            );
        }
    }

    #[test]
    fn exit_monitor_preserves_killed_status_and_clears_only_its_pid() {
        let mut reg = ServiceRegistry::default();
        let launch = spec("hub-killed", "unused", &[], None);
        let ring = Arc::new(Mutex::new(Ring::new(8)));
        reg.reserve(&test_owner(), &launch, &ring, &PathBuf::from("killed.log"))
            .expect("reserve");
        reg.current_mut(&launch.name, &ring).expect("entry").pid = Some(42);
        let control = reg.time_out(&launch.name, &ring).expect("stop requested");
        assert!(control.stop_requested.load(Ordering::Acquire));
        assert!(control.force_requested.load(Ordering::Acquire));
        assert_eq!(
            reg.current(&launch.name, &ring).expect("stopping").status,
            ServiceStatus::Stopping
        );
        assert!(reg.settle(&launch.name, &ring, 137));
        let settled = reg.current(&launch.name, &ring).expect("entry");
        assert_eq!(settled.status, ServiceStatus::Killed);
        assert_eq!(settled.pid, None);
        assert_eq!(settled.exit_code, Some(137));
    }

    #[test]
    fn final_shutdown_closes_new_and_reused_service_names() {
        let mut reg = ServiceRegistry::default();
        let mut launch = spec("hub-shutdown-reservation", "unused", &[], None);
        let ring = Arc::new(Mutex::new(Ring::new(8)));
        reg.reserve(
            &test_owner(),
            &launch,
            &ring,
            &PathBuf::from("previous.log"),
        )
        .expect("initial reservation");
        assert!(reg.settle(&launch.name, &ring, 0));
        reg.shutting_down = true;

        for (name, detached) in [
            ("hub-shutdown-reservation", false),
            ("hub-shutdown-new", false),
            ("hub-shutdown-detached", true),
        ] {
            launch.name = name.to_string();
            launch.detached = detached;
            let error = reg
                .reserve(
                    &test_owner(),
                    &launch,
                    &ring,
                    &PathBuf::from("never-created.log"),
                )
                .expect_err("process shutdown permanently closes admission");
            assert!(error.to_string().contains("PI_HUB_SHUTTING_DOWN"));
        }
        assert_eq!(reg.services.len(), 1);
        assert_eq!(
            reg.lookup(&test_owner(), "hub-shutdown-reservation")
                .expect("service")
                .status,
            ServiceStatus::Exited
        );
    }

    #[test]
    fn abandoned_owner_shutdown_fences_unaccepted_detached_startup_until_retry() {
        let _guard = test_lock();
        let scope = crate::jobs::JobSessionScope::fixed("hub-abandoned-owner");
        let owner = scope.service_owner("hub-abandoned-owner");
        let request = ServiceRequest::new(&owner).expect("admitted owner");
        let mut launch = spec("unaccepted-detached", "unused", &[], None);
        launch.detached = true;
        let ring = Arc::new(Mutex::new(Ring::new(8)));
        let pending =
            PendingService::reserve(&launch, &ring, &PathBuf::from("unused.log"), &request)
                .expect("reserve before shutdown");
        let replacement = crate::jobs::JobSessionScope::fixed("hub-abandoned-owner")
            .service_owner("hub-abandoned-owner");
        assert!(
            describe(&replacement, &launch.name).is_err(),
            "unaccepted detachment is private to its startup"
        );
        assert!(ps(&replacement).expect("replacement services").is_empty());
        let shutdown = begin_session_shutdown(&owner).expect("publish owner fence");
        assert!(pending.control.stop_requested.load(Ordering::Acquire));
        drop(shutdown);
        drop(pending);
        assert!(
            ServiceRequest::new(&owner).is_err(),
            "abandoned cleanup remains fenced"
        );
        let retry = begin_session_shutdown(&owner).expect("retry incomplete cleanup");
        block_on(retry.finish()).expect("prove all old services settled");
        assert!(
            request.check().is_err(),
            "the old request never gains a new generation"
        );
        ServiceRequest::new(&owner).expect("clean return to the same owner admits fresh work");
    }

    #[test]
    fn owner_lookup_started_before_navigation_cannot_reenter_after_clean_shutdown() {
        let _guard = test_lock();
        let scope = crate::jobs::JobSessionScope::fixed("hub-lookup-owner");
        let owner = scope.service_owner("hub-lookup-owner");
        let (release, pending) = futures::channel::oneshot::channel::<()>();
        let pending = Arc::new(Mutex::new(Some(pending)));
        scope.bind(Arc::new(move || {
            let pending = pending
                .lock()
                .expect("resolver gate")
                .take()
                .expect("one lookup");
            Box::pin(async move {
                pending.await.ok()?;
                Some("hub-lookup-owner".to_string())
            })
        }));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        block_on(async {
            let mut operation = Box::pin(run_service_request(&scope, move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }));
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(operation.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            begin_session_shutdown(&owner)
                .expect("navigation fence")
                .finish()
                .await
                .expect("clean old generation");
            release.send(()).expect("release old identity lookup");
            let error = operation.await.expect_err("late lookup must be stale");
            assert!(error.to_string().contains("PI_HUB_STALE_SESSION"));
        });
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "no stale worker side effects"
        );
        scope.bind(Arc::new(|| {
            Box::pin(async { Some("hub-lookup-owner".to_string()) })
        }));
        block_on(run_service_request(&scope, |_| Ok(())))
            .expect("fresh operation may return to the same durable session");
    }

    #[cfg(unix)]
    #[test]
    fn owner_shutdown_fences_reservation_spawn_and_publication_without_retiring_the_host() {
        let _guard = test_lock();
        for (label, target) in [
            ("reservation", StartupTestStage::BeforeReservation),
            ("spawn", StartupTestStage::BeforeSpawn),
            ("publication", StartupTestStage::BeforePublication),
        ] {
            let root = tempfile::tempdir().expect("startup race directory");
            let owner = crate::jobs::JobSessionScope::fixed(label).service_owner(label);
            let launch = LaunchSpec {
                name: format!("hub-owner-{label}"),
                program: "/bin/sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    "printf '%s' \"$$\" > child.pid; exec sleep 300".to_string(),
                ],
                cwd: root.path().to_path_buf(),
                env: Vec::new(),
                ready: None,
                detached: false,
            };
            let (reached, entered) = std::sync::mpsc::sync_channel(1);
            let (release, resume) = std::sync::mpsc::sync_channel(1);
            let resume = Mutex::new(resume);
            *startup_test_hook().lock().expect("startup hook") = Some((
                launch.name.clone(),
                Arc::new(move |stage| {
                    if stage == target {
                        reached.send(()).expect("startup reached gate");
                        resume
                            .lock()
                            .expect("resume gate")
                            .recv_timeout(Duration::from_secs(10))
                            .expect("release fenced startup");
                    }
                }),
            ));
            let starting_owner = owner.clone();
            let startup = std::thread::spawn(move || start(&starting_owner, &launch));
            entered
                .recv_timeout(Duration::from_secs(10))
                .expect("startup paused");
            let pid = if target == StartupTestStage::BeforePublication {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if let Ok(text) = std::fs::read_to_string(root.path().join("child.pid"))
                        && let Ok(pid) = text.parse::<i32>()
                    {
                        break Some(pid);
                    }
                    assert!(
                        Instant::now() < deadline,
                        "spawned child must publish its PID"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            } else {
                None
            };
            let shutdown = begin_session_shutdown(&owner)
                .expect("owner shutdown does not wait for startup lock");
            if target == StartupTestStage::BeforeReservation {
                block_on(shutdown.finish()).expect("nothing reserved yet");
                release.send(()).expect("release stale unreserved launch");
            } else {
                release.send(()).expect("release owned launch for reap");
                block_on(shutdown.finish()).expect("reserved startup cleaned up");
            }
            let result = startup.join().expect("startup thread");
            *startup_test_hook().lock().expect("startup hook") = None;
            assert!(
                result.is_err(),
                "old startup must not survive its owner's transition"
            );
            assert!(
                ps(&owner)
                    .expect("owner services")
                    .iter()
                    .all(|entry| entry.pid.is_none())
            );
            if let Some(pid) = pid {
                assert!(
                    rustix::process::test_kill_process(
                        rustix::process::Pid::from_raw(pid).expect("PID")
                    )
                    .is_err(),
                    "a spawned but unpublished child must be reaped"
                );
            } else {
                assert!(
                    !root.path().join("child.pid").exists(),
                    "fence wins before process creation"
                );
            }
            ServiceRequest::new(&owner)
                .expect("same owner can admit fresh work after clean shutdown");
        }
    }

    #[cfg(unix)]
    #[test]
    fn final_shutdown_fences_in_flight_startups() {
        const CHILD_STAGE: &str = "PI_HUB_SHUTDOWN_TEST_STAGE";
        let Ok(mode) = std::env::var(CHILD_STAGE) else {
            // Closing admission is intentionally irreversible. Each schedule
            // runs in a fresh process, with no test-only reopening of the fence.
            for mode in ["reservation", "spawn", "publication", "detached_restart"] {
                let proof = tempfile::tempdir().expect("shutdown proof directory");
                let completed = proof.path().join("completed");
                let output =
                    std::process::Command::new(std::env::current_exe().expect("test binary"))
                        .args([
                            "--exact",
                            "hub::tests::final_shutdown_fences_in_flight_startups",
                            "--nocapture",
                        ])
                        .env(CHILD_STAGE, mode)
                        .env("PI_HUB_SHUTDOWN_TEST_PROOF", &completed)
                        .env("PI_CODING_AGENT_DIR", proof.path())
                        .output()
                        .expect("isolated shutdown fixture");
                assert!(
                    output.status.success(),
                    "shutdown schedule {mode} failed:\n{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(
                    std::fs::read(&completed).expect("selected child test completed"),
                    b"closed-and-reaped"
                );
            }
            return;
        };

        let root = tempfile::tempdir().expect("fixture directory");
        if mode == "detached_restart" {
            let launch = LaunchSpec {
                name: "hub-shutdown-detached".to_string(),
                program: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), "exec sleep 300".to_string()],
                cwd: root.path().to_path_buf(),
                env: Vec::new(),
                ready: None,
                detached: true,
            };
            let initial = start(&test_owner(), &launch).expect("start detached service");
            shutdown_session_services();
            let restarted = restart(&test_owner(), &launch.name);
            let retained = describe(&test_owner(), &launch.name);
            // Clean up before assertions so a regression cannot orphan the
            // explicit detached fixture when its child test fails.
            let stopped = stop(&test_owner(), &launch.name);
            let error = restarted.expect_err("shutdown must refuse detached restarts");
            assert!(error.to_string().contains("PI_HUB_SHUTTING_DOWN"));
            let retained = retained.expect("retained detached service");
            assert_eq!(retained.pid, initial.pid);
            assert_eq!(retained.status, "running");
            let stopped = stopped.expect("explicit stop remains available");
            assert_eq!(stopped.status, "killed");
            assert!(stopped.pid.is_none());
            let pid = initial.pid.expect("detached child PID");
            let pid = rustix::process::Pid::from_raw(i32::try_from(pid).expect("PID fits i32"))
                .expect("positive PID");
            assert!(
                rustix::process::test_kill_process(pid).is_err(),
                "explicit stop must reap the detached fixture"
            );
            std::fs::write(
                std::env::var_os("PI_HUB_SHUTDOWN_TEST_PROOF").expect("shutdown proof path"),
                b"closed-and-reaped",
            )
            .expect("record completed detached shutdown assertions");
            return;
        }

        let target_stage = match mode.as_str() {
            "reservation" => StartupTestStage::BeforeReservation,
            "spawn" => StartupTestStage::BeforeSpawn,
            "publication" => StartupTestStage::BeforePublication,
            _ => panic!("unknown shutdown schedule"),
        };
        let marker = root.path().join("child.pid");
        let launch = LaunchSpec {
            name: "hub-shutdown-race".to_string(),
            program: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                "printf '%s' \"$$\" > child.pid; exec sleep 300".to_string(),
            ],
            cwd: root.path().to_path_buf(),
            env: Vec::new(),
            ready: None,
            detached: false,
        };
        let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
        let resume_rx = Mutex::new(resume_rx);
        let child_spawned = Arc::new(AtomicBool::new(false));
        let hook_spawned = Arc::clone(&child_spawned);
        let hook: StartupTestHook = Arc::new(move |stage| {
            if stage == StartupTestStage::BeforePublication {
                hook_spawned.store(true, Ordering::Release);
            }
            if stage == target_stage {
                reached_tx.send(()).expect("announce startup pause");
                resume_rx
                    .lock()
                    .expect("resume receiver")
                    .recv_timeout(Duration::from_secs(10))
                    .expect("resume startup after shutdown fence");
            }
        });
        *startup_test_hook().lock().expect("startup hook") = Some((launch.name.clone(), hook));
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let startup = std::thread::spawn(move || {
            started_tx
                .send(start(&test_owner(), &launch))
                .expect("startup result");
        });
        reached_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("startup reached deterministic pause");

        let child_pid = if target_stage == StartupTestStage::BeforePublication {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(pid) = std::fs::read_to_string(&marker)
                    && let Ok(pid) = pid.parse::<i32>()
                {
                    break Some(pid);
                }
                assert!(Instant::now() < deadline, "child did not record its PID");
                std::thread::sleep(Duration::from_millis(5));
            }
        } else {
            None
        };
        let (closed_tx, closed_rx) = std::sync::mpsc::sync_channel(1);
        let shutdown = std::thread::spawn(move || {
            shutdown_session_services();
            closed_tx.send(()).expect("shutdown result");
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if registry().lock().expect("registry").shutting_down {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "shutdown must publish admission fence before waiting for startup"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        if target_stage != StartupTestStage::BeforeReservation {
            assert!(
                matches!(
                    closed_rx.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                ),
                "shutdown must await ownership handoff or failed-start cleanup"
            );
        }
        resume_tx.send(()).expect("release paused startup");
        let error = started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("startup completed")
            .expect_err("shutdown refuses the in-flight launch");
        assert!(error.to_string().contains("PI_HUB_SHUTTING_DOWN"));
        closed_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("shutdown completed");
        startup.join().expect("startup thread");
        shutdown.join().expect("shutdown thread");
        assert!(
            ps(&test_owner())
                .expect("services")
                .iter()
                .all(|entry| entry.pid.is_none())
        );
        if let Some(pid) = child_pid {
            let pid = rustix::process::Pid::from_raw(pid).expect("positive PID");
            assert!(
                rustix::process::test_kill_process(pid).is_err(),
                "unpublished child must be reaped before shutdown returns"
            );
        } else {
            assert!(
                !child_spawned.load(Ordering::Acquire),
                "shutdown must refuse the launch before creating an OS child"
            );
            assert!(
                !marker.exists(),
                "refused startup must not execute user code"
            );
        }
        std::fs::write(
            std::env::var_os("PI_HUB_SHUTDOWN_TEST_PROOF").expect("shutdown proof path"),
            b"closed-and-reaped",
        )
        .expect("record completed shutdown assertions");
    }

    #[test]
    fn failed_start_guard_does_not_settle_a_new_reservation() {
        let _lock = test_lock();
        let launch = spec("hub-pending-guard", "unused", &[], None);
        let old = Arc::new(Mutex::new(Ring::new(8)));
        let new = Arc::new(Mutex::new(Ring::new(8)));
        let request = ServiceRequest::new(&test_owner()).expect("request owner");
        let pending_old =
            PendingService::reserve(&launch, &old, &PathBuf::from("old.log"), &request)
                .expect("old reservation");
        registry()
            .lock()
            .expect("registry")
            .time_out(&launch.name, &old);
        registry() // ubs:ignore[rust.async.lock-unwrap] -- Synchronous fixture injects completed cleanup for the old incarnation.
            .lock()
            .expect("registry")
            .settle(&launch.name, &old, -1);
        let pending_new =
            PendingService::reserve(&launch, &new, &PathBuf::from("new.log"), &request)
                .expect("new reservation");
        drop(pending_old);
        assert_eq!(
            describe(&test_owner(), &launch.name)
                .expect("current")
                .status,
            "starting"
        );
        drop(pending_new);
        let failed = describe(&test_owner(), &launch.name).expect("failed setup");
        assert_eq!(failed.status, "failed");
        assert_eq!(failed.pid, None);
        assert_eq!(failed.exit_code, Some(-1));
    }

    #[test]
    #[allow(clippy::trivial_regex)]
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
        pump_service_stream(reader, std::io::sink(), &ring, &AtomicBool::new(false));
        let output = ring.into_inner().expect("ring");
        assert_eq!(output.since(0).0, vec!["b", "c", "d"]);
        assert!(
            output.ready_log_passed,
            "observed readiness must survive eviction"
        );
    }

    #[test]
    #[allow(clippy::trivial_regex)]
    fn readiness_does_not_match_synthetic_truncation_annotations() {
        let mut ring = Ring::new(8);
        ring.watch_readiness(Some(regex::Regex::new("truncated").expect("regex")));
        ring.push_chunk(&"x".repeat(RING_LINE_BYTE_CAP * 2));
        ring.push_chunk("\n");
        assert!(ring.since(0).0[0].starts_with(TRUNCATED_LINE_PREFIX));
        assert!(!ring.ready_log_passed);
        ring.push_chunk("service says truncated\n");
        assert!(
            ring.ready_log_passed,
            "literal service output still matches"
        );
    }

    #[test]
    fn overflowing_readiness_timeout_is_rejected_before_spawn() {
        let _lock = test_lock();
        let error = start(
            &test_owner(),
            &spec(
                "hub-timeout-overflow",
                "pi-hub-program-that-does-not-exist",
                &[],
                Some(ReadySpec {
                    timeout_secs: Some(u64::MAX),
                    ..ReadySpec::default()
                }),
            ),
        )
        .expect_err("timeout must be validated before program resolution");
        assert!(error.to_string().contains("PI_HUB_INVALID_READY_TIMEOUT"));
        assert!(describe(&test_owner(), "hub-timeout-overflow").is_err());
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
            self.events.lock().expect("events").push("try_wait"); // ubs:ignore[rust.async.lock-unwrap] -- Synchronous fake-child observation; poisoning fails the test.
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
        assert_eq!(
            *events.lock().expect("events"),
            vec!["kill", "wait", "wait"]
        );
    }

    #[test]
    fn child_guard_retries_wait_failure_before_releasing_ownership() {
        let (child, events) =
            recording_child(&[WaitStep::Failed, WaitStep::Interrupted, WaitStep::Exit(137)]);
        drop(child);
        assert_eq!(
            *events.lock().expect("events"),
            vec!["kill", "wait", "kill", "wait", "wait"]
        );
    }

    #[test]
    fn forced_monitor_uses_owned_handle_and_reaps_before_returning() {
        let (child, events) = recording_child(&[WaitStep::Interrupted, WaitStep::Exit(137)]);
        let control = ServiceControl::default();
        control.request_stop(true);
        assert_eq!(child.monitor(&control), 137);
        assert_eq!(
            *events.lock().expect("events"),
            vec!["try_wait", "kill", "wait", "wait"]
        );
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
        assert_eq!(child.wait(), 137);
        assert_eq!(
            *events.lock().expect("events"),
            vec!["wait", "kill", "wait"]
        );
    }

    #[test]
    fn invalid_name_is_rejected_before_program_resolution() {
        let _guard = crate::hub::test_lock();
        let err = start(
            &test_owner(),
            &spec(
                "../../outside-hub",
                "pi-hub-program-that-does-not-exist",
                &[],
                None,
            ),
        )
        .expect_err("path-like service name must fail before spawn");
        assert!(
            err.to_string().contains("PI_HUB_INVALID_NAME"),
            "name validation must win before program resolution: {err}"
        );
    }

    #[test]
    fn invalid_ready_regex_is_rejected_before_program_resolution() {
        let _guard = crate::hub::test_lock();
        let err = start(
            &test_owner(),
            &spec(
                "hub-test-invalid-ready-regex",
                "pi-hub-program-that-does-not-exist",
                &[],
                Some(ReadySpec {
                    log: Some("[".to_string()),
                    port: None,
                    timeout_secs: Some(1),
                }),
            ),
        )
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
        let result = start(
            &test_owner(),
            &spec(
                "hub-test-dead",
                "sleep",
                &["30"],
                Some(ReadySpec {
                    log: None,
                    port: Some(39_991),
                    timeout_secs: Some(1),
                }),
            ),
        );
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("PI_HUB_NOT_READY"),
            "expected not-ready error, got: {err}"
        );
        let settled = describe(&test_owner(), "hub-test-dead").expect("readiness cleanup settled");
        assert_eq!(settled.status, "killed");
        assert_eq!(settled.pid, None);
        assert!(settled.exit_code.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_live_name_rejected() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-dupe";
        let first = start(&test_owner(), &spec(name, "sleep", &["30"], None)).expect("first start");
        assert_eq!(first.status, "running");
        let second = start(&test_owner(), &spec(name, "sleep", &["30"], None));
        let err = second.unwrap_err();
        assert!(
            err.to_string().contains("PI_HUB_NAME_TAKEN"),
            "expected name-taken error, got: {err}"
        );
        let _ = stop(&test_owner(), name);
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
        let snapshot = start(&test_owner(), &launch).expect("repl start");
        assert_eq!(snapshot.status, "running");
        send_text(&test_owner(), name, "print(40 + 2)", true).expect("send");
        let page = logs(&test_owner(), name, None, Some(50), Some("42"), 5_000).expect("logs");
        assert!(
            page.lines.iter().any(|line| line.contains("42")),
            "repl output must contain 42: {:?}",
            page.lines
        );
        let _ = stop(&test_owner(), name);
    }

    #[cfg(unix)]
    #[test]
    fn restart_after_completion_works() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-restart";
        // A service that exits on its own, then restarts from the retained spec.
        let first =
            start(&test_owner(), &spec(name, "echo", &["first-run"], None)).expect("first run");
        assert_eq!(first.status, "running");
        std::thread::sleep(Duration::from_millis(400));
        let settled = describe(&test_owner(), name).expect("describe");
        assert!(
            settled.status == "exited",
            "echo should have exited: {settled:?}"
        );
        let restarted = restart(&test_owner(), name).expect("restart");
        assert_eq!(restarted.status, "running");
        assert_ne!(
            first.log_path, restarted.log_path,
            "each run must own its artifact"
        );
        std::thread::sleep(Duration::from_millis(400));
        let page = logs(
            &test_owner(),
            name,
            None,
            Some(50),
            Some("first-run"),
            5_000,
        )
        .expect("logs");
        assert!(
            page.lines.iter().any(|line| line.contains("first-run")),
            "restarted service must run the retained spec: {:?}",
            page.lines
        );
        let _ = stop(&test_owner(), name).ok();
    }

    #[cfg(unix)]
    #[test]
    fn status_stays_running_for_live_repl() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-stable";
        let snapshot = start(
            &test_owner(),
            &spec(
                name,
                "python3",
                &["-i", "-q"],
                Some(ReadySpec {
                    log: Some(">>>".to_string()),
                    port: None,
                    timeout_secs: Some(10),
                }),
            ),
        )
        .expect("repl start");
        assert_eq!(snapshot.status, "running");
        for wait_ms in [100u64, 300, 600] {
            std::thread::sleep(Duration::from_millis(wait_ms));
            let current = describe(&test_owner(), name).expect("describe");
            assert_eq!(
                current.status, "running",
                "status flipped to {} after {}ms with python alive (exit_code {:?})",
                current.status, wait_ms, current.exit_code
            );
        }
        send_keys(&test_owner(), name, &["CTRL_C".to_string()]).expect("keys");
        std::thread::sleep(Duration::from_millis(200));
        let after = describe(&test_owner(), name).expect("describe after keys");
        assert_eq!(after.status, "running", "status after CTRL_C: {after:?}");
        let _ = stop(&test_owner(), name);
    }

    #[cfg(unix)]
    #[test]
    fn stop_leaves_no_survivors() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-stop";
        let snapshot = start(&test_owner(), &spec(name, "sleep", &["300"], None)).expect("start");
        let pid = snapshot.pid.expect("pid");
        let stopped = stop(&test_owner(), name).expect("stop");
        assert_eq!(stopped.status, "killed");
        assert_eq!(stopped.pid, None);
        assert!(stopped.exit_code.is_some());
        let pid = rustix::process::Pid::from_raw(i32::try_from(pid).expect("pid fits i32"))
            .expect("positive child pid");
        assert!(
            rustix::process::test_kill_process(pid).is_err(),
            "stop must return after reap, without a surviving process or zombie"
        );
    }

    #[cfg(unix)]
    #[test]
    fn logs_cursor_advances_incrementally() {
        let _guard = crate::hub::test_lock();
        let name = "hub-test-cursor";
        let _ = start(
            &test_owner(),
            &spec(
                name,
                "sh",
                &["-c", "echo first; sleep 300"],
                Some(ReadySpec {
                    log: Some("first".to_string()),
                    port: None,
                    timeout_secs: Some(10),
                }),
            ),
        )
        .expect("start");
        let first_page = logs(&test_owner(), name, None, None, None, 0).expect("first page");
        assert!(first_page.lines.iter().any(|line| line.contains("first")));
        let cursor = first_page.cursor;
        send_text(&test_owner(), name, "echo second", true).expect("send");
        let second_page = logs(
            &test_owner(),
            name,
            Some(cursor),
            None,
            Some("second"),
            5_000,
        )
        .expect("page 2");
        assert!(
            second_page.lines.iter().any(|line| line.contains("second")),
            "incremental page must contain the new line: {:?}",
            second_page.lines
        );
        assert!(second_page.cursor > cursor);
        let _ = stop(&test_owner(), name);
    }
}
