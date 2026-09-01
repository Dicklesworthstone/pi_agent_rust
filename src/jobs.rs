//! Background bash jobs (bd-cv653.3.10).
//!
//! `bash {background: true}` returns immediately with a job id; the process
//! runs detached under the same timeout/tree-kill discipline as the
//! foreground path (longer default ceiling). Output streams to a rolling
//! artifact file plus a bounded in-memory tail; when the job settles the
//! monitor thread pushes a completion notice into the follow-up queue so
//! the agent sees it at the next turn boundary.
//!
//! Management surface: the `jobs` tool (list/wait/cancel). The future hub
//! tool's jobs action group (bd-cv653.5.4) wraps this same registry, so
//! the consolidation costs zero rework.
//!
//! Session scoping: the registry lives for the process, but every descriptor,
//! management operation, and completion notice carries its originating
//! session id. Cross-session ids fail exactly like unknown ids. `kill_all`
//! remains the final process-shutdown chokepoint so no child survives exit.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(all(test, unix))]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use asupersync::sync::Notify;
use asupersync::types::Time;
use futures::FutureExt;
use serde::Serialize;

use crate::error::{Error, Result};
use crate::model::{Message, UserContent, UserMessage};

/// Future returned by the live session-identity resolver shared by the jobs
/// tools and their completion-notice fetcher.
pub type JobSessionIdFuture = futures::future::BoxFuture<'static, Option<String>>;

/// Resolves the session that owns a job operation at the instant it runs.
///
/// Reading the live session rather than caching its startup id keeps RPC and
/// interactive new/switch/fork transitions scoped without special-case
/// rebinding at every transition site.
pub type JobSessionIdResolver = Arc<dyn Fn() -> JobSessionIdFuture + Send + Sync>;

/// Shared, dynamically resolved job ownership scope for one tool registry.
#[derive(Clone)]
pub struct JobSessionScope {
    resolver: Arc<Mutex<JobSessionIdResolver>>,
}

impl JobSessionScope {
    /// Create a fixed scope, primarily for standalone tool embeddings and
    /// focused tests that do not have a live [`crate::session::Session`].
    #[must_use]
    pub fn fixed(session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        let resolver: JobSessionIdResolver = Arc::new(move || {
            let session_id = session_id.clone();
            Box::pin(async move { Some(session_id) })
        });
        Self {
            resolver: Arc::new(Mutex::new(resolver)),
        }
    }

    /// Rebind this shared scope to a live session resolver.
    pub fn bind(&self, resolver: JobSessionIdResolver) {
        *self
            .resolver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = resolver;
    }

    /// Resolve a non-empty owner id, failing closed when the live session is
    /// unavailable instead of falling back to a process-global namespace.
    pub async fn session_id(&self) -> Result<String> {
        let resolver = self
            .resolver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        resolver()
            .await
            .filter(|session_id| !session_id.trim().is_empty())
            .ok_or_else(|| {
                Error::tool(
                    "jobs",
                    "PI_JOBS_SESSION_UNAVAILABLE: current agent session identity is unavailable"
                        .to_string(),
                )
            })
    }
}

impl Default for JobSessionScope {
    fn default() -> Self {
        Self::fixed(format!("standalone-{}", uuid::Uuid::new_v4().simple()))
    }
}

/// Tool-result schema tag for job descriptors (stable audit contract).
pub const JOB_SCHEMA: &str = "pi.bash_job.v1";

/// Maximum concurrently running jobs; the next spawn is rejected with a
/// named capacity error.
const MAX_CONCURRENT_JOBS: usize = 8;

/// Default per-job ceiling when the caller passes no timeout (30 minutes).
const DEFAULT_JOB_TIMEOUT_SECS: u64 = 1800;

/// Grace window between TERM and KILL on timeout/cancel, mirroring the
/// foreground bash escalation.
const TERMINATE_GRACE: Duration = Duration::from_secs(3);

/// Bounded in-memory output tail kept per job for notices and `wait`.
const OUTPUT_TAIL_BYTES: usize = 64 * 1024;

/// Hard cap for one job's on-disk artifact. The in-memory tail continues to
/// update after this point, while the snapshot reports truncation explicitly.
const MAX_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;

/// Refuse new jobs before the dedicated directory can exceed this aggregate
/// budget. Automatic rotation is opt-in through
/// `PI_JOBS_ARTIFACT_RETENTION=rotate`; the default preserves every artifact.
const MAX_TOTAL_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

/// Bound inode consumption independently from bytes (for example, jobs that
/// produce no output still create an artifact).
const MAX_ARTIFACT_FILES: usize = 4096;

/// Opt-in rotation always preserves this many newest settled artifacts in
/// addition to every active artifact whose exclusive file lock is held.
const MIN_RETAINED_ARTIFACT_FILES: usize = 8;

/// Pipes normally reach EOF immediately after the process tree is reaped. A
/// bounded drain prevents an escaped descendant that retained a descriptor
/// from blocking terminal publication forever.
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Unix job pipes are nonblocking so a sealed pump observes cancellation
/// within this interval even when an escaped process retains the write end.
const OUTPUT_PUMP_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[cfg(all(test, unix))]
static JOB_STREAM_PREPARE_CALLS: AtomicUsize = AtomicUsize::new(0);

#[cfg(all(test, unix))]
static JOB_PUMP_THREADS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Very large async waits are represented as repeated bounded timer sleeps so
/// `Instant` overflow never turns a valid request into a panic.
const MAX_ASYNC_WAIT_SLICE: Duration = Duration::from_hours(1);

/// Maximum completion notices retained per session in each storage tier: the
/// registry and the Agent's one staged batch. A transition can therefore
/// temporarily hold two batches; restoring the older staged batch reapplies
/// this newest-wins cap and emits telemetry for any eviction.
pub(crate) const MAX_COMPLETION_NOTICES_PER_SESSION: usize = 64;

/// Process-wide backstop across many session identities in a long-lived RPC
/// host. The per-session cap above preserves fairness; this bound prevents a
/// client that continually creates sessions from retaining notices forever.
const MAX_TOTAL_COMPLETION_NOTICES: usize = 512;

/// Bound retained model-visible command metadata independently from the
/// command passed to the shell. The suffix makes truncation explicit.
const MAX_RETAINED_COMMAND_BYTES: usize = 64 * 1024;

/// Bound arbitrary host-produced notice text, including `/tan` task text.
const MAX_COMPLETION_NOTICE_BYTES: usize = 32 * 1024;

/// Settled descriptors remain queryable for recent history, but the process
/// must not retain every command/tail forever during a long-lived RPC session.
const MAX_RETAINED_SETTLED_JOBS_PER_SESSION: usize = 128;

/// Process-wide backstop for settled descriptors across many short-lived
/// session identities. Per-session pruning runs first so one busy session
/// cannot evict another session's recent descriptor under ordinary load.
const MAX_TOTAL_RETAINED_SETTLED_JOBS: usize = 512;

/// Bounds both owner-shutdown fence acquisition and job settlement.
const SESSION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// How a background job settled (or is settling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum JobStatus {
    Running,
    Exited,
    Killed,
    TimedOut,
    Failed,
}

impl JobStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Killed => "killed",
            Self::TimedOut => "timedOut",
            Self::Failed => "failed",
        }
    }

    const fn settled(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// Live registry entry. The output tail is shared with the pump threads.
struct JobEntry {
    owner_session_id: String,
    id: String,
    command: String,
    started_at_ms: i64,
    sequence: u64,
    settled_sequence: Option<u64>,
    status: JobStatus,
    exit_code: Option<i32>,
    pid: Option<u32>,
    artifact_path: PathBuf,
    artifact_cleanup: ArtifactCleanupOutcome,
    tail: Arc<Mutex<TailBuffer>>,
    artifact: Arc<Mutex<ArtifactSink>>,
    output_complete: bool,
    cancel_requested: bool,
    process_live: bool,
    settled_snapshot: Arc<Mutex<Option<JobSnapshot>>>,
    settled_notify: Arc<Notify>,
    cancel_deadline: Arc<CancelDeadline>,
}

/// Serializable snapshot handed to tool results and notices.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSnapshot {
    pub schema: String,
    pub id: String,
    pub command: String,
    pub started_at_ms: i64,
    pub status: String,
    pub exit_code: Option<i32>,
    pub pid: Option<u32>,
    pub artifact_path: String,
    pub artifact_cleanup: ArtifactCleanupOutcome,
    pub output_tail: String,
    pub artifact_truncated: bool,
    pub artifact_error: Option<String>,
    pub output_complete: bool,
}

/// Cleanup performed before this job reserved its artifact file.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactCleanupOutcome {
    pub policy: String,
    pub removed_files: usize,
    pub reclaimed_bytes: u64,
}

impl ArtifactCleanupOutcome {
    fn setup_failure_context(&self) -> String {
        format!(
            "; artifact cleanup before setup failure: policy={}, removedFiles={}, reclaimedBytes={}",
            self.policy, self.removed_files, self.reclaimed_bytes
        )
    }
}

impl JobSnapshot {
    fn from_source_best_effort(source: &JobSnapshotSource) -> Self {
        let output_tail = source
            .tail
            .try_lock()
            .map(|tail| tail.text())
            .unwrap_or_default();
        let (artifact_truncated, artifact_error) = source.artifact.try_lock().map_or_else(
            |_| {
                (
                    true,
                    Some("artifact state unavailable while snapshotting".to_string()),
                )
            },
            |artifact| (artifact.truncated, artifact.write_error.clone()),
        );
        Self::from_source_fields(source, output_tail, artifact_truncated, artifact_error)
    }

    fn from_source_fields(
        source: &JobSnapshotSource,
        output_tail: String,
        artifact_truncated: bool,
        artifact_error: Option<String>,
    ) -> Self {
        Self {
            schema: JOB_SCHEMA.to_string(),
            id: source.id.clone(),
            command: source.command.clone(),
            started_at_ms: source.started_at_ms,
            status: source.status.as_str().to_string(),
            exit_code: source.exit_code,
            pid: source.pid,
            artifact_path: source.artifact_path.display().to_string(),
            artifact_cleanup: source.artifact_cleanup.clone(),
            output_tail,
            artifact_truncated,
            artifact_error,
            output_complete: source.output_complete,
        }
    }
}

#[derive(Clone)]
struct JobSnapshotSource {
    id: String,
    command: String,
    started_at_ms: i64,
    status: JobStatus,
    exit_code: Option<i32>,
    pid: Option<u32>,
    artifact_path: PathBuf,
    artifact_cleanup: ArtifactCleanupOutcome,
    tail: Arc<Mutex<TailBuffer>>,
    artifact: Arc<Mutex<ArtifactSink>>,
    output_complete: bool,
}

impl JobSnapshotSource {
    fn from_entry(entry: &JobEntry) -> Self {
        Self {
            id: entry.id.clone(),
            command: entry.command.clone(),
            started_at_ms: entry.started_at_ms,
            status: entry.status,
            exit_code: entry.exit_code,
            pid: entry.pid,
            artifact_path: entry.artifact_path.clone(),
            artifact_cleanup: entry.artifact_cleanup.clone(),
            tail: Arc::clone(&entry.tail),
            artifact: Arc::clone(&entry.artifact),
            output_complete: entry.output_complete,
        }
    }
}

#[derive(Clone)]
struct JobWaitHandle {
    owner_session_id: String,
    id: String,
    settled_snapshot: Arc<Mutex<Option<JobSnapshot>>>,
    settled_notify: Arc<Notify>,
    cancel_deadline: Arc<CancelDeadline>,
}

struct CancelDeadline {
    started: Mutex<bool>,
    expired: AtomicBool,
    settlement: Arc<(Mutex<bool>, Condvar)>,
    notify: Notify,
}

impl CancelDeadline {
    fn new() -> Self {
        Self {
            started: Mutex::new(false),
            expired: AtomicBool::new(false),
            settlement: Arc::new((Mutex::new(false), Condvar::new())),
            notify: Notify::new(),
        }
    }

    fn start(self: &Arc<Self>, timeout: Duration) -> Result<bool> {
        self.start_with(timeout, |deadline, timeout| {
            std::thread::Builder::new()
                .name("pi-job-cancel-deadline".to_string())
                .spawn(move || deadline.run(timeout))
                .map(|_| ())
        })
    }

    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn start_with<F>(self: &Arc<Self>, timeout: Duration, spawn: F) -> Result<bool>
    where
        F: FnOnce(Arc<Self>, Duration) -> std::io::Result<()>,
    {
        // Serialize the state transition through successful thread creation.
        // A duplicate caller must never observe `started=true` while the first
        // caller can still fail to create the only deadline monitor.
        let mut started = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *started {
            return Ok(false);
        }
        let deadline = Arc::clone(self);
        spawn(deadline, timeout).map_err(|err| {
            Error::tool(
                "jobs",
                format!("Failed to start cancellation deadline monitor: {err}"),
            )
        })?;
        *started = true;
        Ok(true)
    }

    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn run(&self, timeout: Duration) {
        let (lock, wake) = &*self.settlement;
        let settled = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (settled, _) = wake
            .wait_timeout_while(settled, timeout, |settled| !*settled)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let expired = !*settled;
        drop(settled);
        if expired {
            self.expired.store(true, Ordering::Release);
            self.notify.notify_waiters();
        }
    }

    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn finish(&self) {
        let (lock, wake) = &*self.settlement;
        let mut settled = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *settled = true;
        wake.notify_one();
    }
}

struct ArtifactSink {
    file: Option<std::fs::File>,
    bytes_written: usize,
    cap: usize,
    truncated: bool,
    write_error: Option<String>,
}

impl ArtifactSink {
    const fn new(file: std::fs::File, cap: usize) -> Self {
        Self {
            file: Some(file),
            bytes_written: 0,
            cap,
            truncated: false,
            write_error: None,
        }
    }

    fn write(&mut self, data: &[u8]) {
        if self.write_error.is_some() || self.file.is_none() {
            return;
        }
        let remaining = self.cap.saturating_sub(self.bytes_written);
        let to_write = data.len().min(remaining);
        if to_write < data.len() {
            self.truncated = true;
        }
        if to_write == 0 {
            return;
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if let Err(err) = file.write_all(&data[..to_write]) {
            self.write_error = Some(err.to_string());
            return;
        }
        self.bytes_written = self.bytes_written.saturating_add(to_write);
    }

    fn seal(&mut self) {
        // Dropping the file closes it and releases its live-budget lock. Do
        // not put an unbounded filesystem flush on the process-reap and
        // terminal-publication critical path.
        drop(self.file.take());
    }
}

/// Bounded tail: retains the LAST `cap` bytes of job output.
struct TailBuffer {
    buf: std::collections::VecDeque<u8>,
    cap: usize,
    sealed: bool,
}

impl TailBuffer {
    fn new(cap: usize) -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(cap.min(8192)),
            cap,
            sealed: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if self.sealed {
            return;
        }
        if chunk.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&chunk[chunk.len() - self.cap..]);
            return;
        }
        let overflow = (self.buf.len() + chunk.len()).saturating_sub(self.cap);
        if overflow > 0 {
            self.buf.drain(..overflow.min(self.buf.len()));
        }
        self.buf.extend(chunk);
    }

    fn text(&self) -> String {
        let (first, second) = self.buf.as_slices();
        let mut bytes = Vec::with_capacity(first.len() + second.len());
        bytes.extend_from_slice(first);
        bytes.extend_from_slice(second);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    const fn seal(&mut self) {
        self.sealed = true;
    }
}

#[derive(Default)]
struct JobRegistry {
    jobs: HashMap<String, JobEntry>,
    closing_owners: HashMap<String, OwnerShutdownState>,
    owner_shutdown_generations: HashMap<String, u64>,
    owner_spawns_in_flight: HashMap<String, usize>,
    starting_jobs: usize,
    next_job_sequence: u64,
    next_settled_sequence: u64,
    notices: VecDeque<CompletionNotice>,
}

#[derive(Default)]
struct OwnerShutdownState {
    active_attempts: usize,
}

struct CompletionNotice {
    owner_session_id: String,
    text: String,
}

fn registry() -> &'static Mutex<JobRegistry> {
    static REGISTRY: std::sync::LazyLock<Mutex<JobRegistry>> =
        std::sync::LazyLock::new(|| Mutex::new(JobRegistry::default()));
    &REGISTRY
}

fn lifecycle_lock() -> &'static Mutex<()> {
    static LIFECYCLE: Mutex<()> = Mutex::new(());
    &LIFECYCLE
}

#[cfg(test)]
#[derive(Clone, Default)]
#[allow(clippy::type_complexity)]
struct SpawnBackgroundTestHooks {
    owner_session_id: Option<String>,
    after_initial_owner_check: Option<Arc<dyn Fn() + Send + Sync>>,
    before_os_spawn: Option<Arc<dyn Fn() + Send + Sync>>,
    before_registry_publication: Option<Arc<dyn Fn(u32) + Send + Sync>>,
    after_registry_publication: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    fail_monitor_spawn: bool,
    panic_monitor_after_start: bool,
}

#[cfg(test)]
fn spawn_background_test_hooks() -> &'static Mutex<SpawnBackgroundTestHooks> {
    static HOOKS: std::sync::LazyLock<Mutex<SpawnBackgroundTestHooks>> =
        std::sync::LazyLock::new(|| Mutex::new(SpawnBackgroundTestHooks::default()));
    &HOOKS
}

#[cfg(test)]
fn invoke_after_initial_owner_check_hook(owner_session_id: &str) {
    let hooks = spawn_background_test_hooks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let hook = if hooks.owner_session_id.as_deref() == Some(owner_session_id) {
        hooks.after_initial_owner_check.clone()
    } else {
        None
    };
    drop(hooks);
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
fn invoke_before_os_spawn_hook(owner_session_id: &str) {
    let hooks = spawn_background_test_hooks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let hook = if hooks.owner_session_id.as_deref() == Some(owner_session_id) {
        hooks.before_os_spawn.clone()
    } else {
        None
    };
    drop(hooks);
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
fn invoke_before_registry_publication_hook(owner_session_id: &str, pid: u32) {
    let hooks = spawn_background_test_hooks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let hook = if hooks.owner_session_id.as_deref() == Some(owner_session_id) {
        hooks.before_registry_publication.clone()
    } else {
        None
    };
    drop(hooks);
    if let Some(hook) = hook {
        hook(pid);
    }
}

#[cfg(test)]
fn invoke_after_registry_publication_hook(owner_session_id: &str, id: &str) {
    let hooks = spawn_background_test_hooks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let hook = if hooks.owner_session_id.as_deref() == Some(owner_session_id) {
        hooks.after_registry_publication.clone()
    } else {
        None
    };
    drop(hooks);
    if let Some(hook) = hook {
        hook(id);
    }
}

#[cfg(test)]
fn monitor_spawn_failure_requested(owner_session_id: &str) -> bool {
    let hooks = spawn_background_test_hooks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    hooks.owner_session_id.as_deref() == Some(owner_session_id) && hooks.fail_monitor_spawn
}

#[cfg(test)]
fn monitor_panic_requested(owner_session_id: &str) -> bool {
    let hooks = spawn_background_test_hooks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    hooks.owner_session_id.as_deref() == Some(owner_session_id) && hooks.panic_monitor_after_start
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn running_count(reg: &JobRegistry) -> usize {
    reg.jobs
        .values()
        .filter(|job| job.status == JobStatus::Running)
        .count()
}

fn session_closing_error(owner_session_id: &str) -> Error {
    Error::tool(
        "jobs",
        format!(
            "PI_JOBS_SESSION_CLOSING: agent session {owner_session_id:?} is shutting down and cannot start background jobs"
        ),
    )
}

fn remove_quiescent_owner_generation(reg: &mut JobRegistry, owner_session_id: &str) {
    if !reg.closing_owners.contains_key(owner_session_id)
        && reg
            .owner_spawns_in_flight
            .get(owner_session_id)
            .is_none_or(|count| *count == 0)
    {
        reg.owner_shutdown_generations.remove(owner_session_id);
    }
}

// Guard scope is deliberate; tightening drops would change lock-hold semantics.
#[allow(clippy::significant_drop_tightening)]
fn ensure_session_accepting_jobs(owner_session_id: &str) -> Result<()> {
    {
        let reg = registry()
            .lock()
            .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
        let Some(state) = reg.closing_owners.get(owner_session_id) else {
            return Ok(());
        };
        if state.active_attempts > 0 {
            return Err(session_closing_error(owner_session_id));
        }
    }

    // A failed shutdown keeps a fail-closed owner fence. Reconcile it on the
    // next spawn only when no earlier spawn owns the lifecycle seam and every
    // owner job has settled. Older attempts that are still waiting to enter
    // the seam remain generation-tracked and therefore cannot publish. If
    // this call already owns the lifecycle lock, `try_lock` deliberately fails
    // and preserves the fence.
    let _lifecycle = match lifecycle_lock().try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::WouldBlock) => {
            return Err(session_closing_error(owner_session_id));
        }
        Err(std::sync::TryLockError::Poisoned(_)) => {
            return Err(Error::tool(
                "jobs",
                "jobs lifecycle lock poisoned".to_string(),
            ));
        }
    };
    let mut reg = registry()
        .lock()
        .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
    let Some(state) = reg.closing_owners.get(owner_session_id) else {
        // Another spawn may have reconciled the same stale fence while this
        // caller waited for the lifecycle lane. The owner is already open.
        return Ok(());
    };
    let can_reopen = state.active_attempts == 0
        && !reg
            .jobs
            .values()
            .any(|job| job.owner_session_id == owner_session_id && !job.status.settled());
    if can_reopen {
        reg.closing_owners.remove(owner_session_id);
        remove_quiescent_owner_generation(&mut reg, owner_session_id);
        Ok(())
    } else {
        Err(session_closing_error(owner_session_id))
    }
}

struct SessionSpawnAttempt {
    owner_session_id: String,
    generation: u64,
}

impl SessionSpawnAttempt {
    const fn generation(&self) -> u64 {
        self.generation
    }
}

impl Drop for SessionSpawnAttempt {
    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn drop(&mut self) {
        let mut reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remove_counter = reg
            .owner_spawns_in_flight
            .get_mut(&self.owner_session_id)
            .is_some_and(|count| {
                *count = count.saturating_sub(1);
                *count == 0
            });
        if remove_counter {
            reg.owner_spawns_in_flight.remove(&self.owner_session_id);
        }
        remove_quiescent_owner_generation(&mut reg, &self.owner_session_id);
    }
}

// Guard scope is deliberate; tightening drops would change lock-hold semantics.
#[allow(clippy::significant_drop_tightening)]
fn capture_session_spawn_generation(owner_session_id: &str) -> Result<SessionSpawnAttempt> {
    ensure_session_accepting_jobs(owner_session_id)?;
    let mut reg = registry()
        .lock()
        .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
    if reg.closing_owners.contains_key(owner_session_id) {
        return Err(session_closing_error(owner_session_id));
    }
    let generation = reg
        .owner_shutdown_generations
        .get(owner_session_id)
        .copied()
        .unwrap_or_default();
    let count = reg
        .owner_spawns_in_flight
        .entry(owner_session_id.to_string())
        .or_default();
    *count = count.checked_add(1).ok_or_else(|| {
        Error::tool(
            "jobs",
            format!(
                "PI_JOBS_SESSION_SPAWN_OVERFLOW: too many concurrent background spawns for session {owner_session_id:?}"
            ),
        )
    })?;
    Ok(SessionSpawnAttempt {
        owner_session_id: owner_session_id.to_string(),
        generation,
    })
}

// Guard scope is deliberate; tightening drops would change lock-hold semantics.
#[allow(clippy::significant_drop_tightening)]
fn ensure_session_spawn_generation(owner_session_id: &str, expected: u64) -> Result<()> {
    ensure_session_accepting_jobs(owner_session_id)?;
    let reg = registry()
        .lock()
        .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
    let current = reg
        .owner_shutdown_generations
        .get(owner_session_id)
        .copied()
        .unwrap_or_default();
    if reg.closing_owners.contains_key(owner_session_id) || current != expected {
        return Err(session_closing_error(owner_session_id));
    }
    Ok(())
}

struct StartingJobSlot {
    active: bool,
}

impl StartingJobSlot {
    fn commit(mut self, reg: &mut JobRegistry) {
        reg.starting_jobs = reg.starting_jobs.saturating_sub(1);
        self.active = false;
    }
}

impl Drop for StartingJobSlot {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reg.starting_jobs = reg.starting_jobs.saturating_sub(1);
    }
}

// Guard scope is deliberate; tightening drops would change lock-hold semantics.
#[allow(clippy::significant_drop_tightening)]
fn reserve_job_slot() -> Result<(String, u64, StartingJobSlot)> {
    let mut reg = registry()
        .lock()
        .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
    if running_count(&reg).saturating_add(reg.starting_jobs) >= MAX_CONCURRENT_JOBS {
        return Err(Error::tool(
            "bash",
            format!(
                "PI_JOBS_AT_CAPACITY: {MAX_CONCURRENT_JOBS} background jobs already running; \
                 cancel one with the jobs tool or wait for a completion before starting more."
            ),
        ));
    }
    reg.starting_jobs = reg.starting_jobs.saturating_add(1);
    let sequence = reg.next_job_sequence;
    reg.next_job_sequence = reg.next_job_sequence.saturating_add(1);
    let id = format!("job-{}", uuid::Uuid::new_v4().simple());
    Ok((id, sequence, StartingJobSlot { active: true }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactRetentionPolicy {
    Preserve,
    Rotate,
}

impl ArtifactRetentionPolicy {
    fn from_value(value: Option<&OsStr>) -> Result<Self> {
        let Some(value) = value else {
            return Ok(Self::Preserve);
        };
        let value = value.to_str().ok_or_else(|| {
            Error::tool(
                "bash",
                "PI_JOBS_ARTIFACT_RETENTION must be valid UTF-8: preserve or rotate".to_string(),
            )
        })?;
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "preserve" => Ok(Self::Preserve),
            "rotate" => Ok(Self::Rotate),
            _ => Err(Error::tool(
                "bash",
                format!(
                    "PI_JOBS_ARTIFACT_RETENTION has unsupported value {value:?}; expected preserve or rotate"
                ),
            )),
        }
    }

    fn from_environment() -> Result<Self> {
        Self::from_value(std::env::var_os("PI_JOBS_ARTIFACT_RETENTION").as_deref())
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Preserve => "preserve",
            Self::Rotate => "rotate",
        }
    }
}

#[derive(Debug)]
struct ArtifactCleanupCandidate {
    path: PathBuf,
    modified: std::time::SystemTime,
    identity: std::fs::Metadata,
}

#[cfg(unix)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    left.volume_serial_number().is_some()
        && left.file_index().is_some()
        && left.volume_serial_number() == right.volume_serial_number()
        && left.file_index() == right.file_index()
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(_left: &std::fs::Metadata, _right: &std::fs::Metadata) -> bool {
    false
}

fn is_managed_job_artifact_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(id) = name
        .strip_prefix("job-")
        .and_then(|name| name.strip_suffix(".log"))
    else {
        return false;
    };
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn artifact_cleanup_candidates(jobs_dir: &Path) -> std::io::Result<Vec<ArtifactCleanupCandidate>> {
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(jobs_dir)? {
        let entry = entry?;
        if !is_managed_job_artifact_name(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() {
            continue;
        }
        let artifact = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(artifact) => artifact,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        match fs4::FileExt::try_lock(&artifact) {
            Ok(()) => {}
            Err(fs4::TryLockError::WouldBlock) => continue,
            Err(fs4::TryLockError::Error(err)) => return Err(err),
        }
        let opened = artifact.metadata()?;
        let current = match std::fs::symlink_metadata(&path) {
            Ok(current) => current,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                fs4::FileExt::unlock(&artifact)?;
                continue;
            }
            Err(err) => return Err(err),
        };
        if !current.file_type().is_file()
            || !same_file_identity(&opened, &current)
            || !same_file_identity(&metadata, &current)
        {
            fs4::FileExt::unlock(&artifact)?;
            continue;
        }
        fs4::FileExt::unlock(&artifact)?;
        let modified = opened.modified()?;
        candidates.push(ArtifactCleanupCandidate {
            path,
            modified,
            identity: opened,
        });
    }
    candidates.sort_by(|left, right| {
        left.modified
            .cmp(&right.modified)
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(candidates)
}

fn remove_unlocked_artifact(candidate: &ArtifactCleanupCandidate) -> std::io::Result<Option<u64>> {
    let before = match std::fs::symlink_metadata(&candidate.path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if !same_file_identity(&candidate.identity, &before) {
        return Ok(None);
    }
    let artifact = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&candidate.path)
    {
        Ok(artifact) => artifact,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    match fs4::FileExt::try_lock(&artifact) {
        Ok(()) => {}
        Err(fs4::TryLockError::WouldBlock) => return Ok(None),
        Err(fs4::TryLockError::Error(err)) => return Err(err),
    }
    let opened = artifact.metadata()?;
    let current = match std::fs::symlink_metadata(&candidate.path) {
        Ok(current) => current,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs4::FileExt::unlock(&artifact)?;
            return Ok(None);
        }
        Err(err) => return Err(err),
    };
    if !current.file_type().is_file()
        || !same_file_identity(&before, &current)
        || !same_file_identity(&opened, &current)
    {
        fs4::FileExt::unlock(&artifact)?;
        return Ok(None);
    }
    let removed_bytes = opened.len();
    let remove_result = std::fs::remove_file(&candidate.path);
    fs4::FileExt::unlock(&artifact)?;
    remove_result?;
    Ok(Some(removed_bytes))
}

fn artifact_directory_usage(jobs_dir: &Path) -> std::io::Result<(u64, usize)> {
    let mut bytes = 0u64;
    let mut entries = 0usize;
    for entry in std::fs::read_dir(jobs_dir)? {
        let entry = entry?;
        if entry.file_name() == ".artifact-budget.lock" {
            continue;
        }
        entries = entries.saturating_add(1);
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_file() {
            bytes = bytes.saturating_add(metadata.len());
            if path.extension().is_some_and(|extension| extension == "log") {
                let artifact = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)?;
                let opened = artifact.metadata()?;
                let current = std::fs::symlink_metadata(&path)?;
                if !current.file_type().is_file()
                    || !same_file_identity(&metadata, &current)
                    || !same_file_identity(&opened, &current)
                {
                    return Err(std::io::Error::other(format!(
                        "artifact identity changed while inspecting {}",
                        path.display()
                    )));
                }
                match fs4::FileExt::try_lock(&artifact) {
                    Ok(()) => fs4::FileExt::unlock(&artifact)?,
                    Err(fs4::TryLockError::WouldBlock) => {
                        bytes = bytes.saturating_add(
                            u64::try_from(MAX_ARTIFACT_BYTES)
                                .unwrap_or(u64::MAX)
                                .saturating_sub(metadata.len()),
                        );
                    }
                    Err(fs4::TryLockError::Error(err)) => return Err(err),
                }
            }
        }
    }
    Ok((bytes, entries))
}

fn artifact_capacity_available(
    stored_bytes: u64,
    stored_entries: usize,
    max_bytes: u64,
    max_entries: usize,
) -> bool {
    let reserved_bytes = u64::try_from(MAX_ARTIFACT_BYTES).unwrap_or(u64::MAX);
    stored_entries < max_entries && stored_bytes.saturating_add(reserved_bytes) <= max_bytes
}

fn enforce_artifact_retention(
    jobs_dir: &Path,
    policy: ArtifactRetentionPolicy,
    max_bytes: u64,
    max_entries: usize,
    min_retained: usize,
) -> Result<ArtifactCleanupOutcome> {
    let (mut stored_bytes, mut stored_entries) =
        artifact_directory_usage(jobs_dir).map_err(|err| {
            Error::tool(
                "bash",
                format!("Failed to inspect jobs artifact dir: {err}"),
            )
        })?;
    let mut outcome = ArtifactCleanupOutcome {
        policy: policy.as_str().to_string(),
        removed_files: 0,
        reclaimed_bytes: 0,
    };
    if artifact_capacity_available(stored_bytes, stored_entries, max_bytes, max_entries) {
        return Ok(outcome);
    }

    if policy == ArtifactRetentionPolicy::Rotate {
        let candidates = artifact_cleanup_candidates(jobs_dir).map_err(|err| {
            Error::tool(
                "bash",
                format!("Failed to inspect jobs artifact retention candidates: {err}"),
            )
        })?;
        let removable = candidates.len().saturating_sub(min_retained);
        for candidate in candidates.into_iter().take(removable) {
            if artifact_capacity_available(stored_bytes, stored_entries, max_bytes, max_entries) {
                break;
            }
            let removed_bytes = remove_unlocked_artifact(&candidate).map_err(|err| {
                Error::tool(
                    "bash",
                    format!(
                        "Failed to rotate jobs artifact {}: {err}",
                        candidate.path.display()
                    ),
                )
            })?;
            if let Some(removed_bytes) = removed_bytes {
                stored_bytes = stored_bytes.saturating_sub(removed_bytes);
                stored_entries = stored_entries.saturating_sub(1);
                outcome.removed_files = outcome.removed_files.saturating_add(1);
                outcome.reclaimed_bytes = outcome.reclaimed_bytes.saturating_add(removed_bytes);
            }
        }
    }

    (stored_bytes, stored_entries) = artifact_directory_usage(jobs_dir)
        .map_err(|err| Error::tool("bash", format!("Failed to verify jobs artifact dir: {err}")))?;
    if !artifact_capacity_available(stored_bytes, stored_entries, max_bytes, max_entries) {
        return Err(Error::tool(
            "bash",
            format!(
                "PI_JOBS_ARTIFACT_CAPACITY: refusing a new background job because {} accounts for {stored_entries} entries and {stored_bytes} bytes (limits: {max_entries} entries, {max_bytes} bytes including live-job reservations; retention policy: {}; cleanup removed {} files and reclaimed {} bytes)",
                jobs_dir.display(),
                policy.as_str(),
                outcome.removed_files,
                outcome.reclaimed_bytes
            ),
        ));
    }
    Ok(outcome)
}

#[cfg(test)]
fn ensure_artifact_budget(jobs_dir: &Path, max_bytes: u64, max_entries: usize) -> Result<()> {
    enforce_artifact_retention(
        jobs_dir,
        ArtifactRetentionPolicy::Preserve,
        max_bytes,
        max_entries,
        MIN_RETAINED_ARTIFACT_FILES,
    )
    .map(|_| ())
}

#[cfg(unix)]
fn acquire_artifact_budget_lock(jobs_dir: &Path) -> Result<std::fs::File> {
    let lock_path = jobs_dir.join(".artifact-budget.lock");
    let lock = rustix::fs::open(
        &lock_path,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .map(std::fs::File::from)
    .map_err(|err| {
        Error::tool(
            "bash",
            format!("Failed to open jobs artifact budget lock: {err}"),
        )
    })?;
    let metadata = lock.metadata().map_err(|err| {
        Error::tool(
            "bash",
            format!("Failed to inspect jobs artifact budget lock: {err}"),
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(Error::tool(
            "bash",
            "Failed to open jobs artifact budget lock: path is not a regular file".to_string(),
        ));
    }
    fs4::FileExt::lock(&lock).map_err(|err| {
        Error::tool(
            "bash",
            format!("Failed to acquire jobs artifact budget lock: {err}"),
        )
    })?;
    let locked = lock.metadata().map_err(|err| {
        Error::tool(
            "bash",
            format!("Failed to re-inspect jobs artifact budget lock: {err}"),
        )
    })?;
    let current = std::fs::symlink_metadata(&lock_path).map_err(|err| {
        Error::tool(
            "bash",
            format!("Failed to re-verify jobs artifact budget lock: {err}"),
        )
    })?;
    if !current.file_type().is_file() || !same_file_identity(&locked, &current) {
        return Err(Error::tool(
            "bash",
            "Failed to acquire jobs artifact budget lock: path identity changed while locking"
                .to_string(),
        ));
    }
    Ok(lock)
}

#[cfg(not(unix))]
fn acquire_artifact_budget_lock(jobs_dir: &Path) -> Result<std::fs::File> {
    let lock_path = jobs_dir.join(".artifact-budget.lock");
    let before = std::fs::symlink_metadata(&lock_path).ok();
    if before
        .as_ref()
        .is_some_and(|metadata| !metadata.file_type().is_file())
    {
        return Err(Error::tool(
            "bash",
            "Failed to open jobs artifact budget lock: path is not a regular file".to_string(),
        ));
    }
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|err| {
            Error::tool(
                "bash",
                format!("Failed to open jobs artifact budget lock: {err}"),
            )
        })?;
    let opened = lock.metadata().map_err(|err| {
        Error::tool(
            "bash",
            format!("Failed to inspect jobs artifact budget lock: {err}"),
        )
    })?;
    let current = std::fs::symlink_metadata(&lock_path).map_err(|err| {
        Error::tool(
            "bash",
            format!("Failed to verify jobs artifact budget lock: {err}"),
        )
    })?;
    if !current.file_type().is_file()
        || !same_file_identity(&opened, &current)
        || before
            .as_ref()
            .is_some_and(|before| !same_file_identity(before, &current))
    {
        return Err(Error::tool(
            "bash",
            "Failed to open jobs artifact budget lock: path identity changed".to_string(),
        ));
    }
    fs4::FileExt::lock(&lock).map_err(|err| {
        Error::tool(
            "bash",
            format!("Failed to acquire jobs artifact budget lock: {err}"),
        )
    })?;
    let locked = lock.metadata().map_err(|err| {
        Error::tool(
            "bash",
            format!("Failed to re-inspect jobs artifact budget lock: {err}"),
        )
    })?;
    let current = std::fs::symlink_metadata(&lock_path).map_err(|err| {
        Error::tool(
            "bash",
            format!("Failed to re-verify jobs artifact budget lock: {err}"),
        )
    })?;
    if !current.file_type().is_file() || !same_file_identity(&locked, &current) {
        return Err(Error::tool(
            "bash",
            "Failed to acquire jobs artifact budget lock: path identity changed while locking"
                .to_string(),
        ));
    }
    Ok(lock)
}

fn create_job_artifact(jobs_dir: &Path, id: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    let artifact_path = jobs_dir.join(format!("{id}.log"));
    let artifact = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&artifact_path)?;
    fs4::FileExt::lock(&artifact)?;
    Ok((artifact_path, artifact))
}

struct BackgroundChild {
    child: Option<std::process::Child>,
}

impl BackgroundChild {
    const fn new(child: std::process::Child) -> Self {
        Self { child: Some(child) }
    }

    fn id(&self) -> Option<u32> {
        self.child.as_ref().map(std::process::Child::id)
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child
            .as_mut()
            .map_or(Ok(None), std::process::Child::try_wait)
    }

    fn kill_and_wait(&mut self) -> Option<i32> {
        let mut child = self.child.take()?;
        crate::tools::kill_process_group_tree(Some(child.id()));
        let _ = child.kill();
        child.wait().ok().and_then(|status| status.code())
    }

    fn disarm(&mut self) {
        let _ = self.child.take();
    }
}

impl Drop for BackgroundChild {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let pid = child.id();
        if matches!(child.try_wait(), Ok(Some(_))) {
            crate::tools::terminate_reaped_child_discipline(pid);
            return;
        }
        crate::tools::kill_process_group_tree(Some(pid));
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Spawn a command as a background job. The mediation gate in the bash tool
/// has already classified the command by the time we get here.
///
/// # Errors
/// Named `PI_JOBS_AT_CAPACITY` when 8 jobs are already running,
/// `PI_JOBS_SESSION_CLOSING` when owner teardown has started, or tool errors
/// for spawn/artifact failures.
// Guard scope is deliberate; tightening drops would change lock-hold semantics.
#[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
pub fn spawn_background(
    owner_session_id: &str,
    cwd: &Path,
    shell_path: Option<&str>,
    command_prefix: Option<&str>,
    command: &str,
    timeout_secs: Option<u64>,
    artifact_root: Option<&Path>,
) -> Result<JobSnapshot> {
    if owner_session_id.trim().is_empty() {
        return Err(Error::tool(
            "jobs",
            "PI_JOBS_SESSION_UNAVAILABLE: current agent session identity is unavailable"
                .to_string(),
        ));
    }
    // Fail promptly when teardown is already visible. The check under the
    // lifecycle lock below closes the race with a newly published fence.
    let spawn_attempt = capture_session_spawn_generation(owner_session_id)?;
    let spawn_generation = spawn_attempt.generation();
    #[cfg(test)]
    invoke_after_initial_owner_check_hook(owner_session_id);
    // Serialize the fallible spawn-to-monitor ownership transfer with the
    // process-wide and owner-scoped shutdown snapshots, so neither can miss
    // a child between OS spawn and registry publication.
    let _lifecycle = lifecycle_lock()
        .lock()
        .map_err(|_| Error::tool("jobs", "jobs lifecycle lock poisoned".to_string()))?;
    ensure_session_spawn_generation(owner_session_id, spawn_generation)?;
    if !cwd.exists() {
        return Err(Error::tool(
            "bash",
            format!(
                "Working directory does not exist: {}\nCannot execute bash commands.",
                cwd.display()
            ),
        ));
    }

    let timeout_secs = match timeout_secs {
        None => Some(DEFAULT_JOB_TIMEOUT_SECS),
        Some(0) => None,
        Some(value) => Some(value),
    };

    let retained_command = truncate_utf8_bytes(command, MAX_RETAINED_COMMAND_BYTES);
    let shell_command = command_prefix.filter(|p| !p.trim().is_empty()).map_or_else(
        || command.to_string(),
        |prefix| format!("{prefix}\n{command}"),
    );
    let shell_command = format!("trap 'code=$?; wait; exit $code' EXIT\n{shell_command}");

    let shell = shell_path.unwrap_or_else(|| {
        for path in ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"] {
            if Path::new(path).exists() {
                return path;
            }
        }
        "sh"
    });

    let mut cmd = crate::tools::command_with_default_sigpipe_in_dir(shell, cwd)
        .map_err(|e| Error::tool("bash", format!("Failed to prepare shell: {e}")))?;
    cmd.arg("-c")
        .arg(&shell_command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::tools::isolate_command_process_group(&mut cmd);

    let artifact_dir = artifact_root.map_or_else(
        || crate::config::Config::global_dir().join("tool-output-artifacts"),
        Path::to_path_buf,
    );
    let jobs_dir = artifact_dir.join("jobs");
    std::fs::create_dir_all(&jobs_dir)
        .map_err(|e| Error::tool("bash", format!("Failed to create jobs artifact dir: {e}")))?;

    let (id, sequence, slot) = reserve_job_slot()?;
    let artifact_budget_lock = acquire_artifact_budget_lock(&jobs_dir)?;
    let retention_policy = ArtifactRetentionPolicy::from_environment()?;
    let artifact_cleanup = enforce_artifact_retention(
        &jobs_dir,
        retention_policy,
        MAX_TOTAL_ARTIFACT_BYTES,
        MAX_ARTIFACT_FILES,
        MIN_RETAINED_ARTIFACT_FILES,
    )?;
    let cleanup_failure_context = artifact_cleanup.setup_failure_context();
    let (artifact_path, artifact) = create_job_artifact(&jobs_dir, &id).map_err(|e| {
        Error::tool(
            "bash",
            format!("Failed to create job artifact: {e}{cleanup_failure_context}"),
        )
    })?;
    drop(artifact_budget_lock);
    let artifact = Arc::new(Mutex::new(ArtifactSink::new(artifact, MAX_ARTIFACT_BYTES)));
    let tail = Arc::new(Mutex::new(TailBuffer::new(OUTPUT_TAIL_BYTES)));
    let output_sealed = Arc::new(AtomicBool::new(false));
    let settled_snapshot = Arc::new(Mutex::new(None));
    let settled_notify = Arc::new(Notify::new());
    let cancel_deadline = Arc::new(CancelDeadline::new());

    let started_at = Instant::now();
    let started_at_ms = now_ms();
    // Owner shutdown marks the registry before waiting for this lifecycle
    // lock. Recheck after the fallible artifact setup so a stalled pre-spawn
    // path cannot create a child after its session begins closing.
    #[cfg(test)]
    invoke_before_os_spawn_hook(owner_session_id);
    ensure_session_spawn_generation(owner_session_id, spawn_generation)
        .map_err(|err| Error::tool("jobs", format!("{err}{cleanup_failure_context}")))?;
    let mut child = cmd.spawn().map_err(|e| {
        Error::tool(
            "bash",
            format!("Failed to spawn shell: {e}{cleanup_failure_context}"),
        )
    })?;
    if !crate::tools::attach_child_job_discipline(&child) {
        crate::tools::kill_process_group_tree(Some(child.id()));
        let _ = child.kill();
        let _ = child.wait();
        return Err(Error::tool(
            "bash",
            format!(
                "Failed to attach background shell to platform process-tree discipline{cleanup_failure_context}"
            ),
        ));
    }
    let pid = child.id();
    let mut child = BackgroundChild::new(child);
    let stdout = child
        .child
        .as_mut()
        .and_then(|child| child.stdout.take())
        .ok_or_else(|| Error::tool("bash", format!("Missing stdout{cleanup_failure_context}")))?;
    let stderr = child
        .child
        .as_mut()
        .and_then(|child| child.stderr.take())
        .ok_or_else(|| Error::tool("bash", format!("Missing stderr{cleanup_failure_context}")))?;
    prepare_job_stream(&stdout).map_err(|err| {
        Error::tool(
            "bash",
            format!("Failed to prepare job stdout: {err}{cleanup_failure_context}"),
        )
    })?;
    prepare_job_stream(&stderr).map_err(|err| {
        Error::tool(
            "bash",
            format!("Failed to prepare job stderr: {err}{cleanup_failure_context}"),
        )
    })?;

    // Pump threads: dedicated OS threads for the same reason as the
    // foreground path (unbounded blocking reads must not starve the
    // runtime's blocking pool).
    let stdout_tail = Arc::clone(&tail);
    let stdout_artifact = Arc::clone(&artifact);
    let stdout_sealed = Arc::clone(&output_sealed);
    let stdout_pump = std::thread::Builder::new()
        .name(format!("pi-job-{id}-stdout"))
        .spawn(move || pump_job_stream(stdout, &stdout_artifact, &stdout_tail, &stdout_sealed))
        .map_err(|err| {
            Error::tool(
                "bash",
                format!("Failed to start job stdout pump: {err}{cleanup_failure_context}"),
            )
        })?;
    let stderr_tail = Arc::clone(&tail);
    let stderr_artifact = Arc::clone(&artifact);
    let stderr_sealed = Arc::clone(&output_sealed);
    let stderr_pump = match std::thread::Builder::new()
        .name(format!("pi-job-{id}-stderr"))
        .spawn(move || pump_job_stream(stderr, &stderr_artifact, &stderr_tail, &stderr_sealed))
    {
        Ok(handle) => handle,
        Err(err) => {
            child.kill_and_wait();
            output_sealed.store(true, Ordering::Release);
            let mut stdout_pump = Some(stdout_pump);
            let _ = finish_pump(&mut stdout_pump, Instant::now() + OUTPUT_DRAIN_GRACE);
            return Err(Error::tool(
                "bash",
                format!("Failed to start job stderr pump: {err}{cleanup_failure_context}"),
            ));
        }
    };

    #[cfg(test)]
    invoke_before_registry_publication_hook(owner_session_id, pid);
    let snapshot_source = {
        let mut reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current_generation = reg
            .owner_shutdown_generations
            .get(owner_session_id)
            .copied()
            .unwrap_or_default();
        if reg.closing_owners.contains_key(owner_session_id)
            || current_generation != spawn_generation
        {
            drop(reg);
            child.kill_and_wait();
            output_sealed.store(true, Ordering::Release);
            let drain_deadline = Instant::now() + OUTPUT_DRAIN_GRACE;
            let mut stdout_pump = Some(stdout_pump);
            let mut stderr_pump = Some(stderr_pump);
            let _ = finish_pump(&mut stdout_pump, drain_deadline);
            let _ = finish_pump(&mut stderr_pump, drain_deadline);
            return Err(Error::tool(
                "jobs",
                format!(
                    "{}{cleanup_failure_context}",
                    session_closing_error(owner_session_id)
                ),
            ));
        }
        let entry = JobEntry {
            owner_session_id: owner_session_id.to_string(),
            id: id.clone(),
            command: retained_command,
            started_at_ms,
            sequence,
            settled_sequence: None,
            status: JobStatus::Running,
            exit_code: None,
            pid: Some(pid),
            artifact_path,
            artifact_cleanup,
            tail: Arc::clone(&tail),
            artifact: Arc::clone(&artifact),
            output_complete: false,
            cancel_requested: false,
            process_live: true,
            settled_snapshot,
            settled_notify,
            cancel_deadline,
        };
        let snapshot_source = JobSnapshotSource::from_entry(&entry);
        reg.jobs.insert(id.clone(), entry);
        slot.commit(&mut reg);
        snapshot_source
    };
    #[cfg(test)]
    invoke_after_registry_publication_hook(owner_session_id, &id);

    // The monitor is the sole process owner from this point through reap and
    // bounded output drain. Keep the resources behind a shared take-on-start
    // slot so a failed thread creation retains them for synchronous RAII
    // cleanup instead of dropping detached pump handles inside the closure.
    let monitor_id = id.clone();
    let monitor_resources = Arc::new(Mutex::new(Some(MonitorResources {
        id: id.clone(),
        child,
        stdout_pump: Some(stdout_pump),
        stderr_pump: Some(stderr_pump),
        output_sealed,
        artifact: Arc::clone(&artifact),
        tail: Arc::clone(&tail),
        monitor_started: false,
        settlement_published: false,
    })));
    let thread_resources = Arc::clone(&monitor_resources);
    #[cfg(test)]
    let fail_monitor_spawn = monitor_spawn_failure_requested(owner_session_id);
    #[cfg(not(test))]
    let fail_monitor_spawn = false;
    #[cfg(test)]
    let panic_monitor = monitor_panic_requested(owner_session_id);
    let monitor_spawn = if fail_monitor_spawn {
        Err(std::io::Error::other("injected job monitor spawn failure"))
    } else {
        std::thread::Builder::new()
            .name(format!("pi-job-{id}-monitor"))
            .spawn(move || {
                let mut resources = thread_resources
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                    .expect("monitor resources remain available at thread start");
                resources.monitor_started = true;
                #[cfg(test)]
                assert!(!panic_monitor, "injected job monitor panic");
                monitor_job(&monitor_id, started_at, timeout_secs, resources);
            })
    };
    if let Err(err) = monitor_spawn {
        let cleanup = monitor_resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(cleanup);
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .jobs
            .remove(&id);
        return Err(Error::tool(
            "bash",
            format!("Failed to start job monitor: {err}{cleanup_failure_context}"),
        ));
    }

    // Ownership has reached the monitor before inspecting I/O-owned state.
    // A pump stalled in filesystem I/O must not strand the child, lifecycle
    // lock, and cancellation path inside this spawn call.
    Ok(JobSnapshot::from_source_best_effort(&snapshot_source))
}

#[cfg(unix)]
fn prepare_job_stream(reader: &impl std::os::fd::AsFd) -> std::io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(reader)?;
    rustix::fs::fcntl_setfl(reader, flags | rustix::fs::OFlags::NONBLOCK)?;
    #[cfg(test)]
    JOB_STREAM_PREPARE_CALLS.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

#[cfg(not(unix))]
fn prepare_job_stream<R>(_reader: &R) -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
struct JobPumpThreadTestGuard;

#[cfg(all(test, unix))]
impl JobPumpThreadTestGuard {
    fn enter() -> Self {
        JOB_PUMP_THREADS_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

#[cfg(all(test, unix))]
impl Drop for JobPumpThreadTestGuard {
    fn drop(&mut self) {
        JOB_PUMP_THREADS_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

fn pump_job_stream<R: Read>(
    mut reader: R,
    artifact: &Mutex<ArtifactSink>,
    tail: &Mutex<TailBuffer>,
    output_sealed: &AtomicBool,
) -> std::io::Result<()> {
    #[cfg(all(test, unix))]
    let _thread_guard = JobPumpThreadTestGuard::enter();
    let mut chunk = [0u8; 8192];
    loop {
        if output_sealed.load(Ordering::Acquire) {
            return Ok(());
        }
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(OUTPUT_PUMP_POLL_INTERVAL);
            }
            Err(err) => return Err(err),
            Ok(n) => {
                if output_sealed.load(Ordering::Acquire) {
                    return Ok(());
                }
                let data = &chunk[..n];
                artifact
                    .lock()
                    .map_err(|_| std::io::Error::other("job artifact state poisoned"))?
                    .write(data);
                if let Ok(mut tail) = tail.lock() {
                    tail.push(data);
                }
            }
        }
    }
}

fn finish_pump(
    handle: &mut Option<std::thread::JoinHandle<std::io::Result<()>>>,
    deadline: Instant,
) -> bool {
    let Some(pump) = handle.as_ref() else {
        return true;
    };
    while !pump.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !pump.is_finished() {
        return false;
    }
    let pump = handle.take().expect("finished pump handle remains present");
    matches!(pump.join(), Ok(Ok(())))
}

struct MonitorResources {
    id: String,
    child: BackgroundChild,
    stdout_pump: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    stderr_pump: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    output_sealed: Arc<AtomicBool>,
    artifact: Arc<Mutex<ArtifactSink>>,
    tail: Arc<Mutex<TailBuffer>>,
    monitor_started: bool,
    settlement_published: bool,
}

impl MonitorResources {
    fn seal_output_best_effort(&self) {
        self.output_sealed.store(true, Ordering::Release);
        if let Ok(mut artifact) = self.artifact.try_lock() {
            artifact.seal();
        }
        if let Ok(mut tail) = self.tail.try_lock() {
            tail.seal();
        }
    }

    fn stop_pumps(&mut self, deadline: Instant) -> (bool, bool) {
        (
            finish_pump(&mut self.stdout_pump, deadline),
            finish_pump(&mut self.stderr_pump, deadline),
        )
    }
}

impl Drop for MonitorResources {
    fn drop(&mut self) {
        let _ = self.child.kill_and_wait();
        self.output_sealed.store(true, Ordering::Release);
        let deadline = Instant::now() + OUTPUT_DRAIN_GRACE;
        let _ = self.stop_pumps(deadline);
        self.seal_output_best_effort();
        if !self.settlement_published {
            if self.monitor_started {
                settle_job_and_enqueue_notice(&self.id, JobStatus::Failed, None, false);
            } else {
                settle_job_without_notice(&self.id, JobStatus::Failed, None, false);
            }
            self.settlement_published = true;
        }
    }
}

fn last_chars(text: &str, cap: usize) -> String {
    let char_count = text.chars().count();
    text.chars().skip(char_count.saturating_sub(cap)).collect()
}

fn truncate_utf8_bytes(text: &str, max_bytes: usize) -> String {
    const SUFFIX: &str = "\n...[truncated]";
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let suffix = &SUFFIX[..SUFFIX.len().min(max_bytes)];
    let content_cap = max_bytes - suffix.len();
    let mut end = content_cap.min(text.len());
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut truncated = String::with_capacity(max_bytes);
    truncated.push_str(&text[..end]);
    truncated.push_str(suffix);
    truncated
}

// Guard scope is deliberate; tightening drops would change lock-hold semantics.
#[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
fn monitor_job(
    id: &str,
    started_at: Instant,
    timeout_secs: Option<u64>,
    mut resources: MonitorResources,
) {
    let timeout = timeout_secs.map(Duration::from_secs);
    let mut terminate_at: Option<Instant> = None;
    let mut termination_status: Option<JobStatus> = None;
    let root_pid = resources.child.id();

    let exit_code = loop {
        // Keep the nonblocking reap observation and `process_live` transition
        // under the same registry lock used by cancellation. Once wait reports
        // an exit, no concurrent caller can relabel that natural exit as a
        // cancellation of a recycled numeric PID.
        let wait_result = {
            let mut reg = registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let result = resources.child.try_wait();
            if matches!(result, Ok(Some(_)))
                && let Some(job) = reg.jobs.get_mut(id)
            {
                job.pid = None;
                job.process_live = false;
            }
            result
        };
        match wait_result {
            Ok(Some(status)) => {
                // The root has been reaped, but descendants may still own its
                // process-group/job handles and inherited output pipes. Close
                // that discipline before settlement so no child or pump
                // thread survives a natural root exit.
                if let Some(root_pid) = root_pid {
                    crate::tools::terminate_reaped_child_discipline(root_pid);
                }
                resources.child.disarm();
                break status.code();
            }
            Ok(None) => {}
            Err(_) => break resources.child.kill_and_wait(),
        }

        let now = Instant::now();
        if let Some(deadline) = terminate_at {
            if now >= deadline {
                break resources.child.kill_and_wait();
            }
        } else {
            let cancel_requested = registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .jobs
                .get(id)
                .is_some_and(|job| job.cancel_requested);
            if cancel_requested {
                termination_status = Some(JobStatus::Killed);
                crate::tools::terminate_process_group_tree(resources.child.id());
                terminate_at = Some(now + TERMINATE_GRACE);
            } else if let Some(timeout) = timeout
                && now.duration_since(started_at) >= timeout
            {
                termination_status = Some(JobStatus::TimedOut);
                crate::tools::terminate_process_group_tree(resources.child.id());
                terminate_at = Some(now + TERMINATE_GRACE);
            }
        }

        std::thread::sleep(Duration::from_millis(25));
    };

    // KILL/wait and error-recovery paths reap outside the nonblocking seam
    // above. Clear their process identity before draining output too.
    {
        let mut reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(job) = reg.jobs.get_mut(id) {
            job.pid = None;
            job.process_live = false;
        }
    }

    let drain_deadline = Instant::now() + OUTPUT_DRAIN_GRACE;
    let (stdout_complete, stderr_complete) = resources.stop_pumps(drain_deadline);
    let output_complete = stdout_complete && stderr_complete;
    resources.output_sealed.store(true, Ordering::Release);
    let stop_deadline = Instant::now() + OUTPUT_PUMP_POLL_INTERVAL.saturating_mul(10);
    let (stdout_stopped, stderr_stopped) = resources.stop_pumps(stop_deadline);
    if (!stdout_stopped || !stderr_stopped)
        && let Ok(mut artifact) = resources.artifact.try_lock()
        && artifact.write_error.is_none()
    {
        artifact.write_error =
            Some("output pump did not stop after the bounded cancellation deadline".to_string());
    }
    if output_complete {
        resources
            .artifact
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .seal();
        resources
            .tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .seal();
    } else {
        if let Ok(mut artifact) = resources.artifact.try_lock() {
            artifact.seal();
        }
        if let Ok(mut tail) = resources.tail.try_lock() {
            tail.seal();
        }
    }

    // Settle only after reap and bounded pipe drain. Classify by the action
    // that actually initiated termination, not by a cancellation request that
    // may have arrived after the OS process exited but before reap observation.
    let (status, code) = match termination_status {
        Some(JobStatus::Killed) => (JobStatus::Killed, None),
        Some(JobStatus::TimedOut) => (JobStatus::TimedOut, exit_code),
        _ => (
            if exit_code == Some(0) {
                JobStatus::Exited
            } else {
                JobStatus::Failed
            },
            exit_code,
        ),
    };
    settle_job_and_enqueue_notice(id, status, code, output_complete);
    resources.settlement_published = true;
}

fn settle_job_and_enqueue_notice(
    id: &str,
    status: JobStatus,
    exit_code: Option<i32>,
    output_complete: bool,
) {
    settle_job(id, status, exit_code, output_complete, true);
}

fn settle_job_without_notice(
    id: &str,
    status: JobStatus,
    exit_code: Option<i32>,
    output_complete: bool,
) {
    settle_job(id, status, exit_code, output_complete, false);
}

// Guard scope is deliberate; tightening drops would change lock-hold semantics.
#[allow(clippy::significant_drop_tightening)]
fn settle_job(
    id: &str,
    status: JobStatus,
    exit_code: Option<i32>,
    output_complete: bool,
    emit_notice: bool,
) {
    // Build the potentially I/O-contended snapshot without holding the global
    // registry. The best-effort mode prevents a blocked artifact write from
    // stalling settlement or unrelated job operations.
    let source = {
        let reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reg.jobs
            .get(id)
            .filter(|job| !job.status.settled())
            .map(|job| {
                let mut source = JobSnapshotSource::from_entry(job);
                source.status = status;
                source.exit_code = exit_code;
                source.pid = None;
                source.output_complete = output_complete;
                source
            })
    };
    let Some(source) = source else {
        return;
    };
    let snapshot = JobSnapshot::from_source_best_effort(&source);
    let tail_excerpt = last_chars(&snapshot.output_tail, 4096);
    let notice = emit_notice.then(|| format!(
        "[background job {} settled: {} (exit {}; outputComplete={}; artifactTruncated={})]\ncommand: {}\nartifact: {}\noutput tail:\n{}",
        snapshot.id,
        snapshot.status,
        snapshot
            .exit_code
            .map_or_else(|| "n/a".to_string(), |code| code.to_string()),
        snapshot.output_complete,
        snapshot.artifact_truncated,
        snapshot.command.lines().next().unwrap_or(&snapshot.command),
        snapshot.artifact_path,
        if tail_excerpt.is_empty() {
            "(no output)"
        } else {
            &tail_excerpt
        }
    ));

    let notify = {
        let mut reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if reg.jobs.get(id).is_none_or(|job| job.status.settled()) {
            return;
        }
        let settled_sequence = reg.next_settled_sequence;
        reg.next_settled_sequence = reg.next_settled_sequence.saturating_add(1);
        let Some(job) = reg.jobs.get_mut(id) else {
            return;
        };
        job.status = status;
        job.exit_code = exit_code;
        job.pid = None;
        job.process_live = false;
        job.output_complete = output_complete;
        job.settled_sequence = Some(settled_sequence);
        *job.settled_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(snapshot);
        let notify = Arc::clone(&job.settled_notify);
        let owner_session_id = job.owner_session_id.clone();
        job.cancel_deadline.finish();
        if let Some(notice) = notice {
            enqueue_completion_notice(&mut reg, &owner_session_id, &notice);
        }
        prune_settled_jobs(&mut reg);
        notify
    };
    notify.notify_waiters();
}

fn prune_settled_jobs(reg: &mut JobRegistry) {
    let mut settled_by_owner: HashMap<String, Vec<(u64, String)>> = HashMap::new();
    for job in reg.jobs.values().filter(|job| job.status.settled()) {
        if let Some(sequence) = job.settled_sequence {
            settled_by_owner
                .entry(job.owner_session_id.clone())
                .or_default()
                .push((sequence, job.id.clone()));
        }
    }

    let mut remove_ids = Vec::new();
    for settled in settled_by_owner.values_mut() {
        settled.sort();
        let remove_count = settled
            .len()
            .saturating_sub(MAX_RETAINED_SETTLED_JOBS_PER_SESSION);
        remove_ids.extend(settled.iter().take(remove_count).map(|(_, id)| id.clone()));
    }
    for id in remove_ids {
        reg.jobs.remove(&id);
    }

    let mut settled: Vec<_> = reg
        .jobs
        .values()
        .filter(|job| job.status.settled())
        .filter_map(|job| {
            job.settled_sequence
                .map(|sequence| (sequence, job.id.clone()))
        })
        .collect();
    settled.sort();
    let remove_count = settled
        .len()
        .saturating_sub(MAX_TOTAL_RETAINED_SETTLED_JOBS);
    for (_, id) in settled.into_iter().take(remove_count) {
        reg.jobs.remove(&id);
    }
}

/// List snapshots owned by one session, newest last.
///
/// # Errors
/// Tool error when the registry is poisoned.
pub fn list(owner_session_id: &str) -> Result<Vec<JobSnapshot>> {
    let reg = registry()
        .lock()
        .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
    let mut sources: Vec<_> = reg
        .jobs
        .values()
        .filter(|job| job.owner_session_id == owner_session_id)
        .map(|job| {
            let settled = job
                .settled_snapshot
                .try_lock()
                .ok()
                .and_then(|snapshot| snapshot.clone());
            (job.sequence, settled, JobSnapshotSource::from_entry(job))
        })
        .collect();
    sources.sort_by_key(|(sequence, _, _)| *sequence);
    drop(reg);
    Ok(sources
        .iter()
        .map(|(_, settled, source)| {
            settled
                .clone()
                .unwrap_or_else(|| JobSnapshot::from_source_best_effort(source))
        })
        .collect())
}

fn unknown_job_error(id: &str) -> Error {
    Error::tool(
        "jobs",
        format!("PI_JOBS_UNKNOWN_ID: no background job named '{id}'"),
    )
}

// Guard scope is deliberate; tightening drops would change lock-hold semantics.
#[allow(clippy::significant_drop_tightening)]
fn wait_handle(owner_session_id: &str, id: &str) -> Result<JobWaitHandle> {
    let reg = registry()
        .lock()
        .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
    let job = reg
        .jobs
        .get(id)
        .filter(|job| job.owner_session_id == owner_session_id)
        .ok_or_else(|| unknown_job_error(id))?;
    Ok(JobWaitHandle {
        owner_session_id: owner_session_id.to_string(),
        id: id.to_string(),
        settled_snapshot: Arc::clone(&job.settled_snapshot),
        settled_notify: Arc::clone(&job.settled_notify),
        cancel_deadline: Arc::clone(&job.cancel_deadline),
    })
}

fn settled_snapshot(handle: &JobWaitHandle) -> Option<JobSnapshot> {
    handle
        .settled_snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn snapshot_now_best_effort(handle: &JobWaitHandle) -> Result<JobSnapshot> {
    if let Some(snapshot) = settled_snapshot(handle) {
        return Ok(snapshot);
    }
    let reg = registry()
        .lock()
        .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
    if let Some(job) = reg
        .jobs
        .get(&handle.id)
        .filter(|job| job.owner_session_id == handle.owner_session_id)
    {
        let source = JobSnapshotSource::from_entry(job);
        drop(reg);
        return Ok(JobSnapshot::from_source_best_effort(&source));
    }
    drop(reg);
    if let Some(snapshot) = settled_snapshot(handle) {
        return Ok(snapshot);
    }
    Err(unknown_job_error(&handle.id))
}

fn wait_with_handle(handle: &JobWaitHandle, timeout: Duration) -> Result<JobSnapshot> {
    let started = Instant::now();
    loop {
        if let Some(snapshot) = settled_snapshot(handle) {
            return Ok(snapshot);
        }
        if started.elapsed() >= timeout {
            return snapshot_now_best_effort(handle);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn remaining_wait_slice(now: Instant, deadline: Option<Instant>) -> Option<Duration> {
    match deadline {
        Some(deadline) if now >= deadline => None,
        Some(deadline) => Some(
            deadline
                .saturating_duration_since(now)
                .min(MAX_ASYNC_WAIT_SLICE),
        ),
        None => Some(MAX_ASYNC_WAIT_SLICE),
    }
}

/// Wait for a job to settle (bounded), returning its snapshot either way.
///
/// # Errors
/// Named `PI_JOBS_UNKNOWN_ID` for unknown or foreign-session job ids.
#[allow(clippy::significant_drop_tightening)]
pub fn wait(owner_session_id: &str, id: &str, timeout: Duration) -> Result<JobSnapshot> {
    let handle = wait_handle(owner_session_id, id)?;
    wait_with_handle(&handle, timeout)
}

/// Async variant used by tool execution so a long wait continues yielding to
/// abort/steering and unrelated sessions.
///
/// # Errors
/// Named `PI_JOBS_UNKNOWN_ID` for unknown or foreign-session job ids.
pub async fn wait_async(
    owner_session_id: &str,
    id: &str,
    timeout: Duration,
) -> Result<JobSnapshot> {
    wait_async_with_slice(owner_session_id, id, timeout, MAX_ASYNC_WAIT_SLICE).await
}

// u64 nanoseconds cover centuries of timeout; the truncating cast cannot
// overflow for any accepted job timeout.
#[allow(clippy::cast_possible_truncation)]
async fn wait_async_with_slice(
    owner_session_id: &str,
    id: &str,
    timeout: Duration,
    max_wait_slice: Duration,
) -> Result<JobSnapshot> {
    /// Time-based variant of [`remaining_wait_slice`] for timer-driver
    /// deadlines (bd-9zmyf wave fix-forward): asupersync `Time` has no
    /// `checked_add`, and `duration_since` yields nanoseconds directly.
    fn remaining_wait_slice_at(now: Time, deadline: Option<Time>) -> Option<Duration> {
        match deadline {
            Some(deadline) if now >= deadline => None,
            Some(deadline) => {
                Some(Duration::from_nanos(deadline.duration_since(now)).min(MAX_ASYNC_WAIT_SLICE))
            }
            None => Some(MAX_ASYNC_WAIT_SLICE),
        }
    }

    let handle = wait_handle(owner_session_id, id)?;
    let cx = crate::agent_cx::AgentCx::for_current_or_request();
    let now = cx
        .cx()
        .timer_driver()
        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
    let deadline = now.saturating_add_nanos(timeout.as_nanos() as u64);
    loop {
        if let Some(snapshot) = settled_snapshot(&handle) {
            return Ok(snapshot);
        }
        let now = cx
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        let Some(sleep_for) = remaining_wait_slice_at(now, Some(deadline))
            .map(|sleep_for| sleep_for.min(max_wait_slice))
        else {
            return snapshot_now_best_effort(&handle);
        };
        let notified = handle
            .settled_notify
            .wait_until(|| settled_snapshot(&handle).is_some())
            .fuse();
        let deadline_sleep = asupersync::time::sleep(now, sleep_for).fuse();
        futures::pin_mut!(notified, deadline_sleep);
        match futures::future::select(notified, deadline_sleep).await {
            futures::future::Either::Left(((), _)) => {}
            futures::future::Either::Right(((), _)) => {
                if let Some(snapshot) = settled_snapshot(&handle) {
                    return Ok(snapshot);
                }
            }
        }
    }
}

async fn wait_for_settlement_wall(
    handle: &JobWaitHandle,
    timeout: Duration,
) -> Result<JobSnapshot> {
    if let Some(snapshot) = settled_snapshot(handle) {
        return Ok(snapshot);
    }
    let _started_deadline = handle.cancel_deadline.start(timeout)?;
    let settlement = handle
        .settled_notify
        .wait_until(|| settled_snapshot(handle).is_some())
        .fuse();
    let deadline = handle
        .cancel_deadline
        .notify
        .wait_until(|| handle.cancel_deadline.expired.load(Ordering::Acquire))
        .fuse();
    futures::pin_mut!(settlement, deadline);
    match futures::future::select(settlement, deadline).await {
        futures::future::Either::Left(((), _)) => settled_snapshot(handle)
            .ok_or_else(|| Error::tool("jobs", "job settlement notification lost".to_string())),
        futures::future::Either::Right(((), _)) => snapshot_now_best_effort(handle),
    }
}

// Guard scope is deliberate; tightening drops would change lock-hold semantics.
#[allow(clippy::significant_drop_tightening)]
fn request_cancel(owner_session_id: &str, id: &str) -> Result<JobWaitHandle> {
    let mut reg = registry()
        .lock()
        .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
    let Some(job) = reg
        .jobs
        .get_mut(id)
        .filter(|job| job.owner_session_id == owner_session_id)
    else {
        return Err(unknown_job_error(id));
    };
    if job.status.settled() || !job.process_live {
        return Err(Error::tool(
            "jobs",
            format!(
                "PI_JOBS_NOT_RUNNING: job '{id}' no longer owns a live process ({})",
                job.status.as_str()
            ),
        ));
    }
    job.cancel_requested = true;
    Ok(JobWaitHandle {
        owner_session_id: owner_session_id.to_string(),
        id: id.to_string(),
        settled_snapshot: Arc::clone(&job.settled_snapshot),
        settled_notify: Arc::clone(&job.settled_notify),
        cancel_deadline: Arc::clone(&job.cancel_deadline),
    })
}

/// Cancel a running job with the bash timeout escalation (TERM → grace →
/// KILL + tree walk).
///
/// # Errors
/// Named `PI_JOBS_UNKNOWN_ID` for unknown job ids; `PI_JOBS_NOT_RUNNING`
/// when the job already settled.
#[allow(clippy::significant_drop_tightening)]
pub fn cancel(owner_session_id: &str, id: &str) -> Result<JobSnapshot> {
    let handle = request_cancel(owner_session_id, id)?;
    // The monitor thread applies the KILL escalation and records the final
    // status; wait briefly so the snapshot reflects the settle.
    let snapshot = wait_with_handle(&handle, Duration::from_secs(10))?;
    if snapshot.status == JobStatus::Running.as_str() {
        return Err(Error::tool(
            "jobs",
            format!("PI_JOBS_CANCEL_TIMEOUT: job '{id}' did not settle after cancellation"),
        ));
    }
    Ok(snapshot)
}

/// Async cancellation variant for tool entry points. The process monitor owns
/// TERM → KILL escalation; this wait yields to the runtime instead of pinning an
/// executor worker for the grace period.
///
/// # Errors
/// Same named errors as [`cancel`], plus `PI_JOBS_CANCEL_TIMEOUT` if the monitor
/// cannot publish a terminal state within the bounded cleanup window.
pub async fn cancel_async(owner_session_id: &str, id: &str) -> Result<JobSnapshot> {
    let handle = request_cancel(owner_session_id, id)?;
    let snapshot = wait_for_settlement_wall(&handle, Duration::from_secs(10)).await?;
    if snapshot.status == JobStatus::Running.as_str() {
        return Err(Error::tool(
            "jobs",
            format!("PI_JOBS_CANCEL_TIMEOUT: job '{id}' did not settle after cancellation"),
        ));
    }
    Ok(snapshot)
}

/// Drain pending completion notices as follow-up messages for the agent.
/// The Agent's owner-aware job handoff calls this on every poll.
#[must_use]
pub fn take_completion_notices(owner_session_id: &str) -> Vec<Message> {
    let mut reg = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut matched = Vec::new();
    let mut retained = VecDeque::with_capacity(reg.notices.len());
    while let Some(notice) = reg.notices.pop_front() {
        if notice.owner_session_id == owner_session_id {
            matched.push(completion_notice_message(notice.text));
        } else {
            retained.push_back(notice);
        }
    }
    reg.notices = retained;
    matched
}

/// Return staged notices to the bounded registry when the live Agent session
/// changes before delivery. Entries are prepended because they predate notices
/// that could have settled while they were staged; normal per-owner and global
/// retention then discards the oldest entries if either bound is exceeded.
// Guard scope is deliberate; tightening drops would change lock-hold semantics.
#[allow(clippy::significant_drop_tightening)]
pub(crate) fn restore_completion_notices(notices: Vec<(String, Message)>) {
    let restored = notices
        .into_iter()
        .filter_map(|(owner_session_id, message)| {
            if owner_session_id.trim().is_empty() {
                return None;
            }
            let Message::User(UserMessage {
                content: UserContent::Text(text),
                ..
            }) = message
            else {
                return None;
            };
            Some(CompletionNotice {
                owner_session_id,
                text: truncate_utf8_bytes(&text, MAX_COMPLETION_NOTICE_BYTES),
            })
        })
        .collect::<Vec<_>>();
    if restored.is_empty() {
        return;
    }

    let mut reg = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dropped = restore_completion_notices_into(&mut reg, restored);
    if dropped > 0 {
        tracing::warn!(
            dropped,
            "restored completion notices exceeded bounded retention; discarded oldest notices"
        );
    }
}

fn restore_completion_notices_into(
    reg: &mut JobRegistry,
    restored: Vec<CompletionNotice>,
) -> usize {
    for notice in restored.into_iter().rev() {
        reg.notices.push_front(notice);
    }
    prune_completion_notices(reg)
}

fn prune_completion_notices(reg: &mut JobRegistry) -> usize {
    let before = reg.notices.len();
    let mut owner_counts = HashMap::<String, usize>::new();
    for notice in &reg.notices {
        *owner_counts
            .entry(notice.owner_session_id.clone())
            .or_default() += 1;
    }

    let mut retained = VecDeque::with_capacity(reg.notices.len());
    while let Some(notice) = reg.notices.pop_front() {
        if let Some(owner_count) = owner_counts.get_mut(&notice.owner_session_id)
            && *owner_count > MAX_COMPLETION_NOTICES_PER_SESSION
        {
            *owner_count -= 1;
            continue;
        }
        retained.push_back(notice);
    }
    reg.notices = retained;
    while reg.notices.len() > MAX_TOTAL_COMPLETION_NOTICES {
        let _ = reg.notices.pop_front();
    }
    before.saturating_sub(reg.notices.len())
}

fn completion_notice_message(text: String) -> Message {
    Message::User(UserMessage {
        content: UserContent::Text(text),
        timestamp: now_ms(),
    })
}

/// Enqueue a host-produced background completion for the existing follow-up delivery path.
///
/// `/tan` shares this seam with background bash jobs so queue
/// modes, persistence, RPC behavior, and turn-boundary semantics stay
/// identical.
///
/// # Errors
/// Returns `PI_JOBS_SESSION_UNAVAILABLE` when the owner identity is empty or
/// whitespace-only.
pub fn push_completion_notice(owner_session_id: &str, text: impl Into<String>) -> Result<()> {
    if owner_session_id.trim().is_empty() {
        return Err(Error::tool(
            "jobs",
            "PI_JOBS_SESSION_UNAVAILABLE: completion notice owner is empty".to_string(),
        ));
    }
    let text = text.into();
    let text = truncate_utf8_bytes(&text, MAX_COMPLETION_NOTICE_BYTES);
    let mut reg = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    enqueue_completion_notice(&mut reg, owner_session_id, &text);
    Ok(())
}

fn enqueue_completion_notice(reg: &mut JobRegistry, owner_session_id: &str, text: &str) {
    let owner_notice_count = reg
        .notices
        .iter()
        .filter(|notice| notice.owner_session_id == owner_session_id)
        .count();
    if owner_notice_count >= MAX_COMPLETION_NOTICES_PER_SESSION
        && let Some(oldest) = reg
            .notices
            .iter()
            .position(|notice| notice.owner_session_id == owner_session_id)
    {
        let _ = reg.notices.remove(oldest);
    }
    reg.notices.push_back(CompletionNotice {
        owner_session_id: owner_session_id.to_string(),
        text: truncate_utf8_bytes(text, MAX_COMPLETION_NOTICE_BYTES),
    });
    while reg.notices.len() > MAX_TOTAL_COMPLETION_NOTICES {
        let _ = reg.notices.pop_front();
    }
}

struct SessionShutdownAttempt {
    owner_session_id: String,
    finished: bool,
}

impl SessionShutdownAttempt {
    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn begin(owner_session_id: &str) -> Result<Self> {
        let mut reg = registry()
            .lock()
            .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
        let current_generation = reg
            .owner_shutdown_generations
            .get(owner_session_id)
            .copied()
            .unwrap_or_default();
        let Some(next_generation) = current_generation.checked_add(1) else {
            reg.closing_owners
                .entry(owner_session_id.to_string())
                .or_default()
                .active_attempts = usize::MAX;
            return Err(Error::tool(
                "jobs",
                format!(
                    "PI_JOBS_SESSION_SHUTDOWN_OVERFLOW: shutdown generation exhausted for session {owner_session_id:?}"
                ),
            ));
        };
        reg.owner_shutdown_generations
            .insert(owner_session_id.to_string(), next_generation);
        let state = reg
            .closing_owners
            .entry(owner_session_id.to_string())
            .or_default();
        state.active_attempts = state.active_attempts.checked_add(1).ok_or_else(|| {
            Error::tool(
                "jobs",
                format!(
                    "PI_JOBS_SESSION_SHUTDOWN_OVERFLOW: too many concurrent shutdown attempts for session {owner_session_id:?}"
                ),
            )
        })?;
        Ok(Self {
            owner_session_id: owner_session_id.to_string(),
            finished: false,
        })
    }

    fn finish_success(mut self) -> Result<()> {
        self.finished = true;
        finish_session_shutdown_attempt(&self.owner_session_id, true)
    }
}

impl Drop for SessionShutdownAttempt {
    fn drop(&mut self) {
        if !self.finished {
            let _ = finish_session_shutdown_attempt(&self.owner_session_id, false);
        }
    }
}

// Guard scope is deliberate; tightening drops would change lock-hold semantics.
#[allow(clippy::significant_drop_tightening)]
fn finish_session_shutdown_attempt(owner_session_id: &str, clear_when_safe: bool) -> Result<()> {
    let mut reg = registry()
        .lock()
        .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
    let Some(state) = reg.closing_owners.get_mut(owner_session_id) else {
        return Err(Error::tool(
            "jobs",
            format!(
                "PI_JOBS_SESSION_SHUTDOWN_STATE_LOST: session {owner_session_id:?} has no shutdown state"
            ),
        ));
    };
    state.active_attempts = state.active_attempts.checked_sub(1).ok_or_else(|| {
        Error::tool(
            "jobs",
            format!(
                "PI_JOBS_SESSION_SHUTDOWN_STATE_LOST: session {owner_session_id:?} has no active shutdown attempt"
            ),
        )
    })?;
    let last_attempt = state.active_attempts == 0;
    let all_jobs_settled = !reg
        .jobs
        .values()
        .any(|job| job.owner_session_id == owner_session_id && !job.status.settled());
    if clear_when_safe && last_attempt && all_jobs_settled {
        reg.closing_owners.remove(owner_session_id);
        remove_quiescent_owner_generation(&mut reg, owner_session_id);
    }
    Ok(())
}

fn request_session_shutdown(
    owner_session_id: &str,
) -> Result<(SessionShutdownAttempt, Vec<JobWaitHandle>)> {
    request_session_shutdown_with_timeout(owner_session_id, SESSION_SHUTDOWN_TIMEOUT)
}

// Guard scope is deliberate; tightening drops would change lock-hold semantics.
#[allow(clippy::significant_drop_tightening)]
fn request_session_shutdown_with_timeout(
    owner_session_id: &str,
    lifecycle_timeout: Duration,
) -> Result<(SessionShutdownAttempt, Vec<JobWaitHandle>)> {
    if owner_session_id.trim().is_empty() {
        return Err(Error::tool(
            "jobs",
            "PI_JOBS_SESSION_UNAVAILABLE: current agent session identity is unavailable"
                .to_string(),
        ));
    }

    let attempt = SessionShutdownAttempt::begin(owner_session_id)?;
    // Publish the closing fence before waiting for a possibly slow spawn.
    // Spawn checks this both before OS creation and again before registry
    // publication, so an in-flight owner cannot escape the later snapshot.
    let deadline = Instant::now()
        .checked_add(lifecycle_timeout)
        .unwrap_or_else(Instant::now);
    let _lifecycle = loop {
        match lifecycle_lock().try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(Error::tool(
                    "jobs",
                    "jobs lifecycle lock poisoned".to_string(),
                ));
            }
            Err(std::sync::TryLockError::WouldBlock) if Instant::now() >= deadline => {
                return Err(Error::tool(
                    "jobs",
                    format!(
                        "PI_JOBS_SESSION_SHUTDOWN_LOCK_TIMEOUT: session {owner_session_id:?} could not acquire the jobs lifecycle fence within {} seconds",
                        lifecycle_timeout.as_secs_f64()
                    ),
                ));
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    let mut reg = registry()
        .lock()
        .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
    let mut handles = Vec::new();
    for job in reg
        .jobs
        .values_mut()
        .filter(|job| job.owner_session_id == owner_session_id && !job.status.settled())
    {
        if job.process_live {
            job.cancel_requested = true;
        }
        handles.push(JobWaitHandle {
            owner_session_id: owner_session_id.to_string(),
            id: job.id.clone(),
            settled_snapshot: Arc::clone(&job.settled_snapshot),
            settled_notify: Arc::clone(&job.settled_notify),
            cancel_deadline: Arc::clone(&job.cancel_deadline),
        });
    }
    Ok((attempt, handles))
}

/// Kill every running background job owned by one agent session.
///
/// Session-handle replacement uses this owner-scoped seam so an old session
/// cannot leave inaccessible children behind and cannot terminate jobs owned
/// by another live handle in the same process.
///
/// # Errors
/// Named `PI_JOBS_SESSION_UNAVAILABLE` for a blank owner, registry/lifecycle
/// errors, `PI_JOBS_SESSION_SHUTDOWN_LOCK_TIMEOUT` when the closing fence
/// cannot be acquired, or `PI_JOBS_SESSION_SHUTDOWN_INCOMPLETE` when any
/// requested job remains unsettled after the bounded cancellation window.
pub async fn kill_session(owner_session_id: &str) -> Result<()> {
    let request_owner = owner_session_id.to_string();
    let (attempt, handles) =
        asupersync::runtime::spawn_blocking(move || request_session_shutdown(&request_owner))
            .await?;
    let results = futures::future::join_all(
        handles
            .iter()
            .map(|handle| wait_for_settlement_wall(handle, SESSION_SHUTDOWN_TIMEOUT)),
    )
    .await;
    let mut failures = Vec::new();
    for (handle, result) in handles.iter().zip(results) {
        match result {
            Ok(snapshot) if snapshot.status == JobStatus::Running.as_str() => {
                failures.push(format!("{}: cancellation timed out", handle.id));
            }
            Ok(_) => {}
            Err(err) => failures.push(format!("{}: {err}", handle.id)),
        }
    }
    if failures.is_empty() {
        attempt.finish_success()?;
        Ok(())
    } else {
        Err(Error::tool(
            "jobs",
            format!(
                "PI_JOBS_SESSION_SHUTDOWN_INCOMPLETE: {}",
                failures.join("; ")
            ),
        ))
    }
}

/// Kill every running job (process exit). Called once from the main
/// shutdown chokepoint; documented behavior — no orphan daemons.
pub fn kill_all() {
    let _lifecycle = lifecycle_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let jobs: Vec<(String, String)> = {
        let mut reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for job in reg.jobs.values_mut() {
            if !job.status.settled() && job.process_live {
                job.cancel_requested = true;
            }
        }
        reg.jobs
            .values()
            .filter(|job| !job.status.settled())
            .map(|job| (job.owner_session_id.clone(), job.id.clone()))
            .collect()
    };
    for (owner_session_id, id) in jobs {
        let _ = wait(
            &owner_session_id,
            &id,
            TERMINATE_GRACE + OUTPUT_DRAIN_GRACE + Duration::from_secs(1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SESSION_ID: &str = "jobs-test-session";

    fn process_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static TEST_LOCK: Mutex<()> = Mutex::new(());
        TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct SpawnBackgroundTestHookGuard;

    impl Drop for SpawnBackgroundTestHookGuard {
        fn drop(&mut self) {
            *spawn_background_test_hooks()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                SpawnBackgroundTestHooks::default();
        }
    }

    fn install_spawn_background_test_hooks(
        hooks: SpawnBackgroundTestHooks,
    ) -> SpawnBackgroundTestHookGuard {
        *spawn_background_test_hooks()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = hooks;
        SpawnBackgroundTestHookGuard
    }

    #[test]
    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn spawn_background_test_hooks_are_scoped_to_the_configured_owner() {
        let _guard = process_test_guard();
        let after_initial = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let before_spawn = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let before_publication = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let after_publication = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _hooks = install_spawn_background_test_hooks(SpawnBackgroundTestHooks {
            owner_session_id: Some("hook-owner".to_string()),
            after_initial_owner_check: Some({
                let calls = Arc::clone(&after_initial);
                Arc::new(move || {
                    calls.fetch_add(1, Ordering::AcqRel);
                })
            }),
            before_os_spawn: Some({
                let calls = Arc::clone(&before_spawn);
                Arc::new(move || {
                    calls.fetch_add(1, Ordering::AcqRel);
                })
            }),
            before_registry_publication: Some({
                let calls = Arc::clone(&before_publication);
                Arc::new(move |_| {
                    calls.fetch_add(1, Ordering::AcqRel);
                })
            }),
            after_registry_publication: Some({
                let calls = Arc::clone(&after_publication);
                Arc::new(move |_| {
                    calls.fetch_add(1, Ordering::AcqRel);
                })
            }),
            fail_monitor_spawn: true,
            panic_monitor_after_start: true,
        });

        invoke_after_initial_owner_check_hook("foreign-owner");
        invoke_before_os_spawn_hook("foreign-owner");
        invoke_before_registry_publication_hook("foreign-owner", 1);
        invoke_after_registry_publication_hook("foreign-owner", "job-foreign");
        assert!(!monitor_spawn_failure_requested("foreign-owner"));
        assert!(!monitor_panic_requested("foreign-owner"));
        assert_eq!(after_initial.load(Ordering::Acquire), 0);
        assert_eq!(before_spawn.load(Ordering::Acquire), 0);
        assert_eq!(before_publication.load(Ordering::Acquire), 0);
        assert_eq!(after_publication.load(Ordering::Acquire), 0);

        invoke_after_initial_owner_check_hook("hook-owner");
        invoke_before_os_spawn_hook("hook-owner");
        invoke_before_registry_publication_hook("hook-owner", 1);
        invoke_after_registry_publication_hook("hook-owner", "job-owned");
        assert!(monitor_spawn_failure_requested("hook-owner"));
        assert!(monitor_panic_requested("hook-owner"));
        assert_eq!(after_initial.load(Ordering::Acquire), 1);
        assert_eq!(before_spawn.load(Ordering::Acquire), 1);
        assert_eq!(before_publication.load(Ordering::Acquire), 1);
        assert_eq!(after_publication.load(Ordering::Acquire), 1);
    }

    #[derive(Default)]
    struct SpawnHookGate {
        state: Mutex<SpawnHookGateState>,
        changed: Condvar,
    }

    #[derive(Default)]
    struct SpawnHookGateState {
        entered: bool,
        released: bool,
        timed_out: bool,
    }

    // Guard scope is deliberate; these condvar helpers must hold their guards
    // across the wait, so tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    impl SpawnHookGate {
        fn enter_and_wait(&self) {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.entered = true;
            self.changed.notify_all();
            let (mut state, timeout) = self
                .changed
                .wait_timeout_while(state, Duration::from_secs(2), |state| !state.released)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if timeout.timed_out() && !state.released {
                state.timed_out = true;
            }
        }

        fn wait_until_entered(&self) {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (state, timeout) = self
                .changed
                .wait_timeout_while(state, Duration::from_secs(2), |state| !state.entered)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                state.entered && !timeout.timed_out(),
                "background spawn did not reach its race hook"
            );
        }

        fn release(&self) {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.released = true;
            self.changed.notify_all();
        }

        fn assert_not_timed_out(&self) {
            assert!(
                !self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .timed_out,
                "spawn race hook was not released within its test budget"
            );
        }
    }

    #[derive(Default)]
    struct CountedSpawnHookGate {
        state: Mutex<CountedSpawnHookGateState>,
        changed: Condvar,
    }

    #[derive(Default)]
    struct CountedSpawnHookGateState {
        entered: usize,
        release_permits: usize,
        timed_out: bool,
    }

    // Guard scope is deliberate; these condvar helpers must hold their guards
    // across the wait, so tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    impl CountedSpawnHookGate {
        fn enter_and_wait(&self) {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.entered += 1;
            self.changed.notify_all();
            let (mut state, timeout) = self
                .changed
                .wait_timeout_while(state, Duration::from_secs(2), |state| {
                    state.release_permits == 0
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if timeout.timed_out() && state.release_permits == 0 {
                state.timed_out = true;
            } else {
                state.release_permits -= 1;
            }
        }

        fn wait_until_entered(&self, expected: usize) {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (state, timeout) = self
                .changed
                .wait_timeout_while(state, Duration::from_secs(2), |state| {
                    state.entered < expected
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                state.entered >= expected && !timeout.timed_out(),
                "only {} of {expected} background spawns reached their race hook",
                state.entered
            );
        }

        fn release_one(&self) {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.release_permits += 1;
            self.changed.notify_one();
        }

        fn assert_not_timed_out(&self) {
            assert!(
                !self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .timed_out,
                "a counted background spawn hook timed out before release"
            );
        }
    }

    fn temp_root() -> PathBuf {
        static NEXT_ROOT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT_ROOT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("pi-jobs-test-{}-{sequence}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }

    fn synthetic_entry(
        id: &str,
        artifact_path: PathBuf,
        sequence: u64,
        process_live: bool,
    ) -> JobEntry {
        let file = std::fs::File::create(&artifact_path).expect("synthetic artifact");
        JobEntry {
            owner_session_id: TEST_SESSION_ID.to_string(),
            id: id.to_string(),
            command: "true".to_string(),
            started_at_ms: 1,
            sequence,
            settled_sequence: None,
            status: JobStatus::Running,
            exit_code: None,
            pid: process_live.then_some(123_456),
            artifact_path,
            artifact_cleanup: ArtifactCleanupOutcome {
                policy: ArtifactRetentionPolicy::Preserve.as_str().to_string(),
                removed_files: 0,
                reclaimed_bytes: 0,
            },
            tail: Arc::new(Mutex::new(TailBuffer::new(8))),
            artifact: Arc::new(Mutex::new(ArtifactSink::new(file, 16))),
            output_complete: false,
            cancel_requested: false,
            process_live,
            settled_snapshot: Arc::new(Mutex::new(None)),
            settled_notify: Arc::new(Notify::new()),
            cancel_deadline: Arc::new(CancelDeadline::new()),
        }
    }

    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        std::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }

    fn wait_for_output(id: &str, marker: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let snapshot = wait(TEST_SESSION_ID, id, Duration::ZERO).expect("job snapshot");
            if snapshot.output_tail.contains(marker) {
                return;
            }
            assert!(Instant::now() < deadline, "job never emitted {marker:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_path(path: &Path, timeout: Duration) {
        let started = Instant::now();
        while !path.exists() {
            assert!(
                started.elapsed() < timeout,
                "timed out waiting for {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn tail_buffer_keeps_last_bytes() {
        let mut tail = TailBuffer::new(8);
        tail.push(b"hello ");
        tail.push(b"world!");
        // "hello world!" is 12 bytes; the tail retains the last 8.
        assert_eq!(tail.text(), "o world!");
    }

    #[test]
    fn completion_excerpt_keeps_most_recent_characters() {
        let text = format!("{}LATEST", "x".repeat(5000));
        let excerpt = last_chars(&text, 4096);
        assert_eq!(excerpt.chars().count(), 4096);
        assert!(excerpt.ends_with("LATEST"));
    }

    #[test]
    fn artifact_sink_caps_bytes_and_reports_write_errors() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("artifact.log");
        let file = std::fs::File::create(&path).expect("create artifact");
        let mut sink = ArtifactSink::new(file, 4);
        sink.write(b"abcdef");
        assert_eq!(sink.bytes_written, 4);
        assert!(sink.truncated);
        assert!(sink.write_error.is_none());
        assert_eq!(std::fs::metadata(&path).expect("metadata").len(), 4);

        let read_only = std::fs::File::open(&path).expect("open read-only");
        let mut failing = ArtifactSink::new(read_only, 8);
        failing.write(b"x");
        assert!(failing.write_error.is_some());
        failing.write(b"ignored after first failure");
        assert_eq!(failing.bytes_written, 0);

        sink.seal();
        assert!(sink.file.is_none(), "settlement must close the artifact fd");
    }

    #[cfg(unix)]
    #[test]
    fn sealed_nonblocking_pipe_pump_joins_while_writer_remains_open() {
        let _guard = process_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("pipe-holder child");
        let stdout = child.stdout.take().expect("child stdout");
        prepare_job_stream(&stdout).expect("nonblocking child pipe");
        let artifact_file =
            std::fs::File::create(temp.path().join("pump.log")).expect("pump artifact");
        let artifact = Arc::new(Mutex::new(ArtifactSink::new(artifact_file, 64)));
        let tail = Arc::new(Mutex::new(TailBuffer::new(64)));
        let sealed = Arc::new(AtomicBool::new(false));
        let pump_artifact = Arc::clone(&artifact);
        let pump_tail = Arc::clone(&tail);
        let pump_sealed = Arc::clone(&sealed);
        let pump = std::thread::spawn(move || {
            pump_job_stream(stdout, &pump_artifact, &pump_tail, &pump_sealed)
        });
        std::thread::sleep(OUTPUT_PUMP_POLL_INTERVAL.saturating_mul(2));
        assert!(
            !pump.is_finished(),
            "an open writer with no data must keep an unsealed pump pending"
        );

        sealed.store(true, Ordering::Release);
        let mut pump = Some(pump);
        let joined = finish_pump(
            &mut pump,
            Instant::now() + OUTPUT_PUMP_POLL_INTERVAL.saturating_mul(10),
        );
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            joined,
            "sealing must stop and join the reader even while the writer remains open"
        );
        assert!(pump.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn artifact_creation_is_exclusive_and_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let jobs_dir = temp.path().join("jobs");
        std::fs::create_dir_all(&jobs_dir).expect("jobs dir");
        let victim = temp.path().join("victim.txt");
        std::fs::write(&victim, "preserve-me").expect("victim");
        symlink(&victim, jobs_dir.join("job-planted.log")).expect("planted symlink");

        let error = create_job_artifact(&jobs_dir, "job-planted")
            .expect_err("create_new must refuse an existing symlink");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(&victim).expect("victim remains readable"),
            "preserve-me"
        );
    }

    #[cfg(unix)]
    #[test]
    fn artifact_budget_lock_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let jobs_dir = temp.path().join("jobs");
        std::fs::create_dir_all(&jobs_dir).expect("jobs dir");
        let victim = temp.path().join("victim.txt");
        std::fs::write(&victim, "preserve-me").expect("victim");
        symlink(&victim, jobs_dir.join(".artifact-budget.lock")).expect("planted symlink");

        let error = acquire_artifact_budget_lock(&jobs_dir)
            .expect_err("NOFOLLOW must reject a planted budget-lock symlink");
        assert!(error.to_string().contains("artifact budget lock"));
        assert_eq!(
            std::fs::read_to_string(&victim).expect("victim remains readable"),
            "preserve-me"
        );
    }

    #[test]
    fn aggregate_artifact_budget_refuses_bytes_and_entries() {
        let _guard = process_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("one.log"), b"12345").expect("first artifact");
        let bytes_error = ensure_artifact_budget(temp.path(), 4, 10)
            .expect_err("stored bytes above the budget must refuse new jobs");
        assert!(
            bytes_error
                .to_string()
                .contains("PI_JOBS_ARTIFACT_CAPACITY")
        );

        std::fs::write(temp.path().join("two.log"), b"").expect("second artifact");
        let entries_error = ensure_artifact_budget(temp.path(), u64::MAX, 2)
            .expect_err("entry count at the budget must refuse new jobs");
        assert!(
            entries_error
                .to_string()
                .contains("PI_JOBS_ARTIFACT_CAPACITY")
        );
    }

    #[test]
    fn aggregate_artifact_budget_reserves_locked_live_files_at_full_cap() {
        let _guard = process_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        let live_path = temp.path().join("job-live.log");
        let live = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&live_path)
            .expect("live artifact");
        fs4::FileExt::lock(&live).expect("reserve live artifact");

        let two_job_budget = u64::try_from(MAX_ARTIFACT_BYTES)
            .expect("artifact cap fits")
            .saturating_mul(2);
        let error = ensure_artifact_budget(temp.path(), two_job_budget - 1, 10)
            .expect_err("live artifact plus prospective job must reserve two full caps");
        assert!(error.to_string().contains("PI_JOBS_ARTIFACT_CAPACITY"));
        fs4::FileExt::unlock(&live).expect("release live artifact reservation");
    }

    #[test]
    fn artifact_retention_policy_requires_an_explicit_rotate_opt_in() {
        assert_eq!(
            ArtifactRetentionPolicy::from_value(None).expect("default policy"),
            ArtifactRetentionPolicy::Preserve
        );
        assert_eq!(
            ArtifactRetentionPolicy::from_value(Some(OsStr::new(" preserve ")))
                .expect("preserve policy"),
            ArtifactRetentionPolicy::Preserve
        );
        assert_eq!(
            ArtifactRetentionPolicy::from_value(Some(OsStr::new("ROTATE"))).expect("rotate policy"),
            ArtifactRetentionPolicy::Rotate
        );
        let error = ArtifactRetentionPolicy::from_value(Some(OsStr::new("delete-all")))
            .expect_err("unknown cleanup policies must fail closed");
        assert!(error.to_string().contains("expected preserve or rotate"));
    }

    #[test]
    fn managed_artifact_name_validation_is_exact() {
        assert!(is_managed_job_artifact_name(OsStr::new(
            "job-0123456789abcdef0123456789abcdef.log"
        )));
        for invalid in [
            "job-0123456789abcdef0123456789abcde.log",
            "job-0123456789abcdef0123456789abcdef0.log",
            "job-0123456789ABCDEF0123456789ABCDEF.log",
            "job-0123456789abcdef0123456789abcdef.txt",
            "../job-0123456789abcdef0123456789abcdef.log",
            "job-0123456789abcdef/0123456789abcdef.log",
        ] {
            assert!(
                !is_managed_job_artifact_name(OsStr::new(invalid)),
                "invalid artifact name was accepted: {invalid:?}"
            );
        }
    }

    #[test]
    fn job_snapshot_serializes_artifact_cleanup_outcome() {
        let temp = tempfile::tempdir().expect("tempdir");
        let id = "job-cleanup-snapshot";
        let mut entry = synthetic_entry(id, temp.path().join("snapshot.log"), 0, false);
        entry.artifact_cleanup = ArtifactCleanupOutcome {
            policy: "rotate".to_string(),
            removed_files: 3,
            reclaimed_bytes: 17,
        };
        let snapshot = JobSnapshot::from_source_best_effort(&JobSnapshotSource::from_entry(&entry));
        let value = serde_json::to_value(snapshot).expect("serialize job snapshot");
        assert_eq!(value["artifactCleanup"]["policy"], "rotate");
        assert_eq!(value["artifactCleanup"]["removedFiles"], 3);
        assert_eq!(value["artifactCleanup"]["reclaimedBytes"], 17);
    }

    #[test]
    fn artifact_preservation_policy_never_removes_managed_files() {
        let _guard = process_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("job-00000000000000000000000000000000.log");
        std::fs::write(&path, b"preserve-me").expect("artifact fixture");
        let error = enforce_artifact_retention(
            temp.path(),
            ArtifactRetentionPolicy::Preserve,
            u64::try_from(MAX_ARTIFACT_BYTES).expect("artifact cap fits"),
            10,
            0,
        )
        .expect_err("preservation refuses capacity instead of deleting");
        assert!(error.to_string().contains("retention policy: preserve"));
        assert_eq!(
            std::fs::read_to_string(path).expect("preserved artifact"),
            "preserve-me"
        );
    }

    #[test]
    fn artifact_cleanup_candidate_identity_rejects_same_name_replacement() {
        let _guard = process_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("job-00000000000000000000000000000000.log");
        std::fs::write(&path, b"old!").expect("old artifact");
        let candidate = artifact_cleanup_candidates(temp.path())
            .expect("candidate inventory")
            .pop()
            .expect("managed candidate");
        std::fs::rename(&path, temp.path().join("displaced.log"))
            .expect("replace candidate identity");
        std::fs::write(&path, b"new!").expect("same-name replacement");

        assert_eq!(
            remove_unlocked_artifact(&candidate).expect("identity check"),
            None
        );
        assert_eq!(
            std::fs::read_to_string(path).expect("replacement preserved"),
            "new!"
        );
    }

    #[test]
    fn artifact_rotation_reclaims_oldest_settled_files_and_reports_outcome() {
        let _guard = process_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        let mut paths = Vec::new();
        for index in 0..4u64 {
            let path = temp.path().join(format!("job-{index:032x}.log"));
            std::fs::write(&path, b"data").expect("artifact fixture");
            filetime::set_file_mtime(
                &path,
                filetime::FileTime::from_unix_time(
                    1_700_000_000 + i64::try_from(index).expect("index fits"),
                    0,
                ),
            )
            .expect("artifact timestamp");
            paths.push(path);
        }
        let reserved = u64::try_from(MAX_ARTIFACT_BYTES).expect("artifact cap fits");
        let outcome = enforce_artifact_retention(
            temp.path(),
            ArtifactRetentionPolicy::Rotate,
            reserved + 8,
            10,
            2,
        )
        .expect("rotation admits one new artifact");
        assert_eq!(
            outcome,
            ArtifactCleanupOutcome {
                policy: "rotate".to_string(),
                removed_files: 2,
                reclaimed_bytes: 8,
            }
        );
        assert!(!paths[0].exists());
        assert!(!paths[1].exists());
        assert!(paths[2].exists());
        assert!(paths[3].exists());

        let restart_outcome = enforce_artifact_retention(
            temp.path(),
            ArtifactRetentionPolicy::Rotate,
            reserved + 8,
            10,
            2,
        )
        .expect("the retained restart state remains within budget");
        assert_eq!(restart_outcome.removed_files, 0);
        assert_eq!(restart_outcome.reclaimed_bytes, 0);
    }

    #[test]
    fn failed_rotation_reports_partial_cleanup_outcome() {
        let _guard = process_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        for index in 0..3u64 {
            let path = temp.path().join(format!("job-{index:032x}.log"));
            std::fs::write(&path, b"data").expect("artifact fixture");
            filetime::set_file_mtime(
                &path,
                filetime::FileTime::from_unix_time(
                    1_700_000_000 + i64::try_from(index).expect("index fits"),
                    0,
                ),
            )
            .expect("artifact timestamp");
        }
        let reserved = u64::try_from(MAX_ARTIFACT_BYTES).expect("artifact cap fits");
        let error = enforce_artifact_retention(
            temp.path(),
            ArtifactRetentionPolicy::Rotate,
            reserved.saturating_add(4),
            10,
            2,
        )
        .expect_err("the recent-artifact floor prevents sufficient reclamation");
        let message = error.to_string();
        assert!(message.contains("cleanup removed 1 files"));
        assert!(message.contains("reclaimed 4 bytes"));
        assert_eq!(
            std::fs::read_dir(temp.path())
                .expect("artifact directory")
                .filter_map(std::result::Result::ok)
                .filter(|entry| is_managed_job_artifact_name(&entry.file_name()))
                .count(),
            2,
            "the two newest artifacts remain after partial rotation"
        );
    }

    #[test]
    fn artifact_rotation_preserves_active_and_newest_settled_files() {
        let _guard = process_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        let mut paths = Vec::new();
        for index in 0..3u64 {
            let path = temp.path().join(format!("job-{index:032x}.log"));
            std::fs::write(&path, b"data").expect("artifact fixture");
            filetime::set_file_mtime(
                &path,
                filetime::FileTime::from_unix_time(
                    1_700_000_000 + i64::try_from(index).expect("index fits"),
                    0,
                ),
            )
            .expect("artifact timestamp");
            paths.push(path);
        }
        let active = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&paths[0])
            .expect("active artifact");
        fs4::FileExt::lock(&active).expect("active artifact lock");

        let reserved = u64::try_from(MAX_ARTIFACT_BYTES).expect("artifact cap fits");
        let outcome = enforce_artifact_retention(
            temp.path(),
            ArtifactRetentionPolicy::Rotate,
            reserved.saturating_mul(2).saturating_add(4),
            10,
            1,
        )
        .expect("one unlocked settled artifact is reclaimable");
        assert_eq!(outcome.removed_files, 1);
        assert_eq!(outcome.reclaimed_bytes, 4);
        assert!(paths[0].exists(), "active artifact must be preserved");
        assert!(!paths[1].exists(), "oldest settled artifact rotates first");
        assert!(
            paths[2].exists(),
            "newest settled artifact must be preserved"
        );
        fs4::FileExt::unlock(&active).expect("release active artifact");
    }

    #[cfg(unix)]
    #[test]
    fn artifact_rotation_never_follows_or_removes_symlinks() {
        use std::os::unix::fs::symlink;

        let _guard = process_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        let victim = temp.path().join("victim.txt");
        std::fs::write(&victim, b"preserve-me").expect("victim fixture");
        let planted = temp.path().join("job-00000000000000000000000000000000.log");
        symlink(&victim, &planted).expect("planted artifact symlink");

        let error = enforce_artifact_retention(
            temp.path(),
            ArtifactRetentionPolicy::Rotate,
            u64::MAX,
            1,
            0,
        )
        .expect_err("a symlink cannot become a cleanup candidate");
        assert!(error.to_string().contains("PI_JOBS_ARTIFACT_CAPACITY"));
        assert!(planted.symlink_metadata().is_ok());
        assert_eq!(
            std::fs::read_to_string(&victim).expect("victim remains readable"),
            "preserve-me"
        );
    }

    #[test]
    fn artifact_budget_lock_child() {
        let Ok(mode) = std::env::var("PI_JOBS_BUDGET_LOCK_CHILD") else {
            return;
        };
        let jobs_dir = PathBuf::from(
            std::env::var_os("PI_JOBS_BUDGET_LOCK_DIR").expect("child lock directory"),
        );
        let marker_dir = PathBuf::from(
            std::env::var_os("PI_JOBS_BUDGET_MARKER_DIR").expect("child marker directory"),
        );
        if mode == "probe" {
            std::fs::write(marker_dir.join("probe-attempted"), b"").expect("probe marker");
        }
        let _lock = acquire_artifact_budget_lock(&jobs_dir).expect("child budget lock");
        match mode.as_str() {
            "holder" => {
                std::fs::write(marker_dir.join("holder-acquired"), b"").expect("holder marker");
                wait_for_path(&marker_dir.join("release-holder"), Duration::from_secs(5));
            }
            "probe" => {
                std::fs::write(marker_dir.join("probe-acquired"), b"")
                    .expect("probe acquired marker");
            }
            other => panic!("unknown child mode {other:?}"),
        }
    }

    #[test]
    fn artifact_budget_lock_serializes_independent_processes() {
        let _guard = process_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        let jobs_dir = temp.path().join("jobs");
        let marker_dir = temp.path().join("markers");
        std::fs::create_dir_all(&jobs_dir).expect("jobs directory");
        std::fs::create_dir_all(&marker_dir).expect("marker directory");
        let test_binary = std::env::current_exe().expect("current test binary");
        let spawn_child = |mode: &str| {
            std::process::Command::new(&test_binary)
                .args(["--exact", "jobs::tests::artifact_budget_lock_child"])
                .env("PI_JOBS_BUDGET_LOCK_CHILD", mode)
                .env("PI_JOBS_BUDGET_LOCK_DIR", &jobs_dir)
                .env("PI_JOBS_BUDGET_MARKER_DIR", &marker_dir)
                .spawn()
                .expect("spawn budget-lock child")
        };

        let mut holder = spawn_child("holder");
        wait_for_path(&marker_dir.join("holder-acquired"), Duration::from_secs(2));
        let mut probe = spawn_child("probe");
        wait_for_path(&marker_dir.join("probe-attempted"), Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(100));
        let probe_was_blocked = !marker_dir.join("probe-acquired").exists();
        std::fs::write(marker_dir.join("release-holder"), b"").expect("release holder");
        assert!(holder.wait().expect("wait for holder").success());
        assert!(probe.wait().expect("wait for probe").success());
        assert!(
            probe_was_blocked,
            "the second process acquired the artifact budget lock concurrently"
        );
        assert!(marker_dir.join("probe-acquired").exists());
    }

    #[test]
    fn cancellation_deadline_monitor_is_coalesced_per_job() {
        let deadline = Arc::new(CancelDeadline::new());
        assert!(
            deadline
                .start(Duration::from_secs(2))
                .expect("start first deadline")
        );
        assert!(
            !deadline
                .start(Duration::from_secs(2))
                .expect("reuse first deadline"),
            "a duplicate cancellation must not create another OS deadline thread"
        );
        deadline.finish();
    }

    #[test]
    fn cancellation_deadline_spawn_failure_cannot_fool_a_duplicate_starter() {
        let deadline = Arc::new(CancelDeadline::new());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_deadline = Arc::clone(&deadline);
        let first = std::thread::spawn(move || {
            first_deadline.start_with(Duration::from_secs(2), move |_, _| {
                entered_tx.send(()).expect("announce injected spawn");
                release_rx.recv().expect("release injected spawn");
                Err(std::io::Error::other("injected thread spawn failure"))
            })
        });
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first starter reached injected spawn");

        let (second_tx, second_rx) = std::sync::mpsc::channel();
        let second_deadline = Arc::clone(&deadline);
        let second = std::thread::spawn(move || {
            let result = second_deadline.start(Duration::from_secs(2));
            second_tx.send(result).expect("return second start result");
        });
        assert!(
            second_rx.recv_timeout(Duration::from_millis(25)).is_err(),
            "duplicate starter must wait until the in-flight spawn succeeds or fails"
        );

        release_tx.send(()).expect("release first starter");
        assert!(
            first.join().expect("first starter thread").is_err(),
            "the injected spawn must fail"
        );
        assert!(
            second_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("second starter result")
                .expect("second starter must retry successfully"),
            "the second starter must create the deadline thread after the failure"
        );
        deadline.finish();
        second.join().expect("second starter thread");
    }

    #[test]
    fn session_shutdown_fences_spawns_and_waits_for_reaped_monitor_settlement() {
        let _guard = process_test_guard();
        let root = temp_root();
        let id = format!("job-shutdown-fence-{}", uuid::Uuid::new_v4().simple());
        let entry = synthetic_entry(&id, root.join(format!("{id}.log")), 0, false);
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .jobs
            .insert(id.clone(), entry);

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("shutdown runtime");
            done_tx
                .send(runtime.block_on(kill_session(TEST_SESSION_ID)))
                .expect("return shutdown result");
        });

        let closing_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .closing_owners
                .contains_key(TEST_SESSION_ID)
            {
                break;
            }
            assert!(
                Instant::now() < closing_deadline,
                "session shutdown never published its owner fence"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "a reaped but unsettled monitor must keep owner shutdown pending"
        );
        let spawn_error = spawn_background(
            TEST_SESSION_ID,
            &root,
            None,
            None,
            "sleep 300",
            Some(300),
            Some(&root),
        )
        .expect_err("a closing owner must reject new background jobs");
        assert!(
            spawn_error.to_string().contains("PI_JOBS_SESSION_CLOSING"),
            "unexpected closing-owner error: {spawn_error}"
        );

        settle_job_and_enqueue_notice(&id, JobStatus::Exited, Some(0), true);
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("owner shutdown completion")
            .expect("owner shutdown result");
        shutdown.join().expect("shutdown thread");
        let mut reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!reg.closing_owners.contains_key(TEST_SESSION_ID));
        reg.jobs.remove(&id);
        drop(reg);
        let _ = take_completion_notices(TEST_SESSION_ID);
    }

    #[test]
    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn shutdown_fence_after_initial_check_is_seen_under_lifecycle_lock() {
        let _guard = process_test_guard();
        let root = temp_root();
        let owner = format!("shutdown-under-lock-{}", uuid::Uuid::new_v4().simple());
        let gate = Arc::new(SpawnHookGate::default());
        let hook_gate = Arc::clone(&gate);
        let reached_pre_spawn = Arc::new(AtomicBool::new(false));
        let hook_reached_pre_spawn = Arc::clone(&reached_pre_spawn);
        let _hooks = install_spawn_background_test_hooks(SpawnBackgroundTestHooks {
            owner_session_id: Some(owner.clone()),
            after_initial_owner_check: Some(Arc::new(move || {
                hook_gate.enter_and_wait();
            })),
            before_os_spawn: Some(Arc::new(move || {
                hook_reached_pre_spawn.store(true, Ordering::Release);
            })),
            ..SpawnBackgroundTestHooks::default()
        });
        let spawn_owner = owner.clone();
        let spawn_root = root;
        let spawn = std::thread::spawn(move || {
            spawn_background(
                &spawn_owner,
                &spawn_root,
                None,
                None,
                "true",
                Some(300),
                Some(&spawn_root),
            )
        });

        gate.wait_until_entered();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("shutdown runtime");
        runtime
            .block_on(kill_session(&owner))
            .expect("empty owner shutdown");
        assert!(
            !registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .closing_owners
                .contains_key(&owner),
            "successful empty shutdown should clear its active latch"
        );
        assert!(
            registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .owner_shutdown_generations
                .contains_key(&owner),
            "the generation must remain while an old captured spawn is paused"
        );
        gate.release();
        let error = match spawn.join().expect("background spawn thread") {
            Err(error) => error,
            Ok(snapshot) => {
                let _ = cancel(&owner, &snapshot.id);
                panic!("owner spawn escaped a completed shutdown after its early check")
            }
        };
        gate.assert_not_timed_out();
        assert!(error.to_string().contains("PI_JOBS_SESSION_CLOSING"));
        assert!(
            !reached_pre_spawn.load(Ordering::Acquire),
            "under-lifecycle owner check must reject before artifact and OS-spawn setup"
        );
        let reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!reg.owner_spawns_in_flight.contains_key(&owner));
        assert!(
            !reg.owner_shutdown_generations.contains_key(&owner),
            "the final stale spawn departure should reclaim its generation"
        );
    }

    #[test]
    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn shutdown_generation_waits_for_every_stale_spawn_capture() {
        let _guard = process_test_guard();
        let root = temp_root();
        let owner = format!("shutdown-two-captures-{}", uuid::Uuid::new_v4().simple());
        let gate = Arc::new(CountedSpawnHookGate::default());
        let hook_gate = Arc::clone(&gate);
        let _hooks = install_spawn_background_test_hooks(SpawnBackgroundTestHooks {
            owner_session_id: Some(owner.clone()),
            after_initial_owner_check: Some(Arc::new(move || {
                hook_gate.enter_and_wait();
            })),
            ..SpawnBackgroundTestHooks::default()
        });
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let mut spawns = Vec::new();
        for _ in 0..2 {
            let result_tx = result_tx.clone();
            let spawn_owner = owner.clone();
            let spawn_root = root.clone();
            spawns.push(std::thread::spawn(move || {
                let result = spawn_background(
                    &spawn_owner,
                    &spawn_root,
                    None,
                    None,
                    "true",
                    Some(300),
                    Some(&spawn_root),
                );
                result_tx.send(result).expect("return spawn result");
            }));
        }
        drop(result_tx);

        gate.wait_until_entered(2);
        assert_eq!(
            registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .owner_spawns_in_flight
                .get(&owner)
                .copied(),
            Some(2)
        );
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("shutdown runtime");
        runtime
            .block_on(kill_session(&owner))
            .expect("empty owner shutdown");

        gate.release_one();
        match result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first stale spawn result")
        {
            Err(error) => assert!(error.to_string().contains("PI_JOBS_SESSION_CLOSING")),
            Ok(snapshot) => {
                let _ = cancel(&owner, &snapshot.id);
                panic!("first stale spawn escaped the completed shutdown")
            }
        }
        {
            let reg = registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(reg.owner_spawns_in_flight.get(&owner).copied(), Some(1));
            assert!(
                reg.owner_shutdown_generations.contains_key(&owner),
                "one remaining stale capture must keep the generation alive"
            );
        }

        gate.release_one();
        match result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second stale spawn result")
        {
            Err(error) => assert!(error.to_string().contains("PI_JOBS_SESSION_CLOSING")),
            Ok(snapshot) => {
                let _ = cancel(&owner, &snapshot.id);
                panic!("second stale spawn escaped the completed shutdown")
            }
        }
        gate.assert_not_timed_out();
        for spawn in spawns {
            spawn.join().expect("background spawn thread");
        }
        let reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!reg.owner_spawns_in_flight.contains_key(&owner));
        assert!(!reg.owner_shutdown_generations.contains_key(&owner));
    }

    #[test]
    fn shutdown_fence_during_artifact_setup_prevents_os_spawn() {
        let _guard = process_test_guard();
        let root = temp_root();
        let owner = format!("shutdown-pre-spawn-{}", uuid::Uuid::new_v4().simple());
        let gate = Arc::new(SpawnHookGate::default());
        let hook_gate = Arc::clone(&gate);
        let reached_os_spawn = Arc::new(AtomicBool::new(false));
        let hook_reached_os_spawn = Arc::clone(&reached_os_spawn);
        let _hooks = install_spawn_background_test_hooks(SpawnBackgroundTestHooks {
            owner_session_id: Some(owner.clone()),
            before_os_spawn: Some(Arc::new(move || {
                hook_gate.enter_and_wait();
            })),
            before_registry_publication: Some(Arc::new(move |_| {
                hook_reached_os_spawn.store(true, Ordering::Release);
            })),
            ..SpawnBackgroundTestHooks::default()
        });
        let starting_jobs_before = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .starting_jobs;
        let spawn_owner = owner.clone();
        let spawn_root = root;
        let spawn = std::thread::spawn(move || {
            spawn_background(
                &spawn_owner,
                &spawn_root,
                None,
                None,
                "sleep 300",
                Some(300),
                Some(&spawn_root),
            )
        });

        gate.wait_until_entered();
        let attempt = SessionShutdownAttempt::begin(&owner).expect("publish shutdown fence");
        gate.release();
        let error = match spawn.join().expect("background spawn thread") {
            Err(error) => error,
            Ok(snapshot) => {
                let _ = cancel(&owner, &snapshot.id);
                panic!("owner spawn escaped a fence published before OS spawn")
            }
        };
        gate.assert_not_timed_out();
        assert!(error.to_string().contains("PI_JOBS_SESSION_CLOSING"));
        assert!(
            !reached_os_spawn.load(Ordering::Acquire),
            "pre-spawn owner check must reject before creating a child"
        );
        assert_eq!(
            registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .starting_jobs,
            starting_jobs_before,
            "rejected spawn must restore capacity accounting"
        );
        attempt.finish_success().expect("finish shutdown attempt");
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_fence_after_os_spawn_reaps_child_before_publication() {
        let _guard = process_test_guard();
        let root = temp_root();
        let owner = format!("shutdown-post-spawn-{}", uuid::Uuid::new_v4().simple());
        let gate = Arc::new(SpawnHookGate::default());
        let hook_gate = Arc::clone(&gate);
        let spawned_pid = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let hook_spawned_pid = Arc::clone(&spawned_pid);
        let _hooks = install_spawn_background_test_hooks(SpawnBackgroundTestHooks {
            owner_session_id: Some(owner.clone()),
            before_registry_publication: Some(Arc::new(move |pid| {
                hook_spawned_pid.store(pid, Ordering::Release);
                hook_gate.enter_and_wait();
            })),
            ..SpawnBackgroundTestHooks::default()
        });
        let starting_jobs_before = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .starting_jobs;
        let spawn_owner = owner.clone();
        let spawn_root = root;
        let spawn = std::thread::spawn(move || {
            spawn_background(
                &spawn_owner,
                &spawn_root,
                None,
                None,
                "sleep 300",
                Some(300),
                Some(&spawn_root),
            )
        });

        gate.wait_until_entered();
        let pid = spawned_pid.load(Ordering::Acquire);
        assert_ne!(pid, 0, "post-spawn hook must observe the child pid");
        assert!(
            process_exists(pid),
            "test child must be live before fencing"
        );
        let attempt = SessionShutdownAttempt::begin(&owner).expect("publish shutdown fence");
        gate.release();
        let error = match spawn.join().expect("background spawn thread") {
            Err(error) => error,
            Ok(snapshot) => {
                let _ = cancel(&owner, &snapshot.id);
                panic!("owner child was published after its shutdown fence")
            }
        };
        gate.assert_not_timed_out();
        assert!(error.to_string().contains("PI_JOBS_SESSION_CLOSING"));
        assert!(!process_exists(pid), "rejected child must be reaped");
        let reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            reg.jobs.values().all(|job| job.owner_session_id != owner),
            "rejected child must never enter the registry"
        );
        assert_eq!(reg.starting_jobs, starting_jobs_before);
        drop(reg);
        attempt.finish_success().expect("finish shutdown attempt");
    }

    #[test]
    fn concurrent_session_shutdown_attempts_keep_fence_until_last_success() {
        let _guard = process_test_guard();
        let owner = format!("shutdown-overlap-{}", uuid::Uuid::new_v4().simple());
        let (first, first_handles) =
            request_session_shutdown_with_timeout(&owner, Duration::from_secs(1))
                .expect("first shutdown snapshot");
        let (second, second_handles) =
            request_session_shutdown_with_timeout(&owner, Duration::from_secs(1))
                .expect("second shutdown snapshot");
        assert!(first_handles.is_empty());
        assert!(second_handles.is_empty());
        assert_eq!(
            registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .closing_owners
                .get(&owner)
                .map(|state| state.active_attempts),
            Some(2)
        );

        first.finish_success().expect("first shutdown completion");
        assert!(
            ensure_session_accepting_jobs(&owner).is_err(),
            "one successful caller must not clear another caller's owner fence"
        );
        assert_eq!(
            registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .closing_owners
                .get(&owner)
                .map(|state| state.active_attempts),
            Some(1)
        );

        second.finish_success().expect("second shutdown completion");
        ensure_session_accepting_jobs(&owner).expect("last success reopens owner");
        assert!(
            !registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .closing_owners
                .contains_key(&owner)
        );
    }

    #[test]
    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn quiescent_owner_shutdown_generations_do_not_accumulate() {
        let _guard = process_test_guard();
        let generations_before = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .owner_shutdown_generations
            .len();

        for index in 0..64 {
            let owner = format!(
                "shutdown-generation-churn-{index}-{}",
                uuid::Uuid::new_v4().simple()
            );
            SessionShutdownAttempt::begin(&owner)
                .expect("begin quiescent shutdown")
                .finish_success()
                .expect("finish quiescent shutdown");
            let reg = registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(!reg.closing_owners.contains_key(&owner));
            assert!(!reg.owner_spawns_in_flight.contains_key(&owner));
            assert!(!reg.owner_shutdown_generations.contains_key(&owner));
        }

        assert_eq!(
            registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .owner_shutdown_generations
                .len(),
            generations_before,
            "short-lived quiescent owners must not grow the process registry"
        );
    }

    #[test]
    fn session_shutdown_lifecycle_fence_acquisition_is_bounded() {
        let _guard = process_test_guard();
        let owner = format!("shutdown-lock-{}", uuid::Uuid::new_v4().simple());
        let lifecycle_guard = lifecycle_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let request_owner = owner.clone();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let request = std::thread::spawn(move || {
            result_tx
                .send(request_session_shutdown_with_timeout(
                    &request_owner,
                    Duration::from_millis(25),
                ))
                .expect("return bounded shutdown request");
        });

        let request_result = match result_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => result,
            Err(err) => {
                drop(lifecycle_guard);
                request.join().expect("shutdown-fence request thread");
                panic!("shutdown fence acquisition did not remain bounded: {err}");
            }
        };
        let error = match request_result {
            Err(error) => error,
            Ok((_attempt, _handles)) => {
                panic!("held lifecycle lock unexpectedly allowed owner shutdown")
            }
        };
        assert!(
            error
                .to_string()
                .contains("PI_JOBS_SESSION_SHUTDOWN_LOCK_TIMEOUT"),
            "unexpected lifecycle-timeout error: {error}"
        );
        assert!(
            registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .closing_owners
                .contains_key(&owner),
            "a timed-out fence must keep rejecting late owner spawns"
        );
        assert!(
            ensure_session_accepting_jobs(&owner).is_err(),
            "an in-flight lifecycle owner must prevent premature fence reconciliation"
        );
        drop(lifecycle_guard);
        request.join().expect("shutdown-fence request thread");
        ensure_session_accepting_jobs(&owner).expect("safe stale-fence reconciliation");
        assert!(
            !registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .closing_owners
                .contains_key(&owner)
        );
    }

    #[test]
    fn settlement_is_bounded_when_artifact_state_is_busy() {
        let _guard = process_test_guard();
        let root = temp_root();
        let id = format!("job-busy-artifact-{}", uuid::Uuid::new_v4().simple());
        let entry = synthetic_entry(&id, root.join(format!("{id}.log")), 0, false);
        let artifact = Arc::clone(&entry.artifact);
        let handle = JobWaitHandle {
            owner_session_id: TEST_SESSION_ID.to_string(),
            id: id.clone(),
            settled_snapshot: Arc::clone(&entry.settled_snapshot),
            settled_notify: Arc::clone(&entry.settled_notify),
            cancel_deadline: Arc::clone(&entry.cancel_deadline),
        };
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .jobs
            .insert(id.clone(), entry);

        let artifact_guard = artifact.lock().expect("artifact lock");
        settle_job_and_enqueue_notice(&id, JobStatus::Exited, Some(0), false);
        let snapshot = settled_snapshot(&handle).expect("terminal snapshot");
        assert_eq!(snapshot.status, "exited");
        assert!(
            snapshot.artifact_truncated,
            "unavailable artifact state must be reported conservatively"
        );
        assert_eq!(
            snapshot.artifact_error.as_deref(),
            Some("artifact state unavailable while snapshotting")
        );
        drop(artifact_guard);

        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .jobs
            .remove(&id);
        let _ = take_completion_notices(TEST_SESSION_ID);
    }

    #[test]
    fn list_is_bounded_when_artifact_state_is_busy() {
        let _guard = process_test_guard();
        let root = temp_root();
        let id = format!("job-busy-list-{}", uuid::Uuid::new_v4().simple());
        let entry = synthetic_entry(&id, root.join(format!("{id}.log")), 0, false);
        let artifact = Arc::clone(&entry.artifact);
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .jobs
            .insert(id.clone(), entry);

        let artifact_guard = artifact.lock().expect("artifact lock");
        let (listed_tx, listed_rx) = std::sync::mpsc::channel();
        let listing = std::thread::spawn(move || {
            listed_tx
                .send(list(TEST_SESSION_ID))
                .expect("return list result");
        });
        let prompt_result = listed_rx.recv_timeout(Duration::from_secs(2));
        drop(artifact_guard);
        listing.join().expect("listing thread");
        let snapshots = prompt_result
            .expect("list must not wait for a busy artifact")
            .expect("list result");
        let snapshot = snapshots
            .iter()
            .find(|snapshot| snapshot.id == id)
            .expect("busy job remains listed");
        assert!(snapshot.artifact_truncated);
        assert!(snapshot.artifact_error.is_some());

        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .jobs
            .remove(&id);
    }

    #[test]
    fn cancellation_refuses_a_reaped_process_identity() {
        let _guard = process_test_guard();
        let root = temp_root();
        let id = format!("job-reaped-{}", uuid::Uuid::new_v4().simple());
        let entry = synthetic_entry(&id, root.join(format!("{id}.log")), 0, false);
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .jobs
            .insert(id.clone(), entry);

        let error = request_cancel(TEST_SESSION_ID, &id)
            .err()
            .expect("reaped process must not be signalled");
        assert!(error.to_string().contains("PI_JOBS_NOT_RUNNING"));
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .jobs
            .remove(&id);
    }

    #[test]
    fn settled_job_retention_is_bounded() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut reg = JobRegistry::default();
        for index in 0..(MAX_RETAINED_SETTLED_JOBS_PER_SESSION + 3) {
            let id = format!("job-{index:03}");
            let file =
                std::fs::File::create(temp.path().join(format!("{id}.log"))).expect("artifact");
            let mut artifact = ArtifactSink::new(file, 16);
            artifact.seal();
            reg.jobs.insert(
                id.clone(),
                JobEntry {
                    owner_session_id: TEST_SESSION_ID.to_string(),
                    id,
                    command: "true".to_string(),
                    started_at_ms: i64::try_from(index).expect("index fits"),
                    sequence: u64::try_from(index).expect("index fits"),
                    settled_sequence: Some(u64::try_from(index).expect("index fits")),
                    status: JobStatus::Exited,
                    exit_code: Some(0),
                    pid: None,
                    artifact_path: temp.path().join(format!("job-{index:03}.log")),
                    artifact_cleanup: ArtifactCleanupOutcome {
                        policy: ArtifactRetentionPolicy::Preserve.as_str().to_string(),
                        removed_files: 0,
                        reclaimed_bytes: 0,
                    },
                    tail: Arc::new(Mutex::new(TailBuffer::new(8))),
                    artifact: Arc::new(Mutex::new(artifact)),
                    output_complete: true,
                    cancel_requested: false,
                    process_live: false,
                    settled_snapshot: Arc::new(Mutex::new(None)),
                    settled_notify: Arc::new(Notify::new()),
                    cancel_deadline: Arc::new(CancelDeadline::new()),
                },
            );
        }

        prune_settled_jobs(&mut reg);
        assert_eq!(reg.jobs.len(), MAX_RETAINED_SETTLED_JOBS_PER_SESSION);
        assert!(!reg.jobs.contains_key("job-000"));
        assert!(reg.jobs.contains_key(&format!(
            "job-{:03}",
            MAX_RETAINED_SETTLED_JOBS_PER_SESSION + 2
        )));
    }

    #[test]
    fn settled_job_retention_is_fair_across_sessions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut reg = JobRegistry::default();
        let owner_a_id = "owner-a-job".to_string();
        let mut owner_a = synthetic_entry(&owner_a_id, temp.path().join("owner-a.log"), 0, false);
        owner_a.owner_session_id = "owner-a".to_string();
        owner_a.status = JobStatus::Exited;
        owner_a.settled_sequence = Some(0);
        reg.jobs.insert(owner_a_id.clone(), owner_a);

        for index in 0..MAX_RETAINED_SETTLED_JOBS_PER_SESSION {
            let id = format!("owner-b-{index:03}");
            let mut entry = synthetic_entry(
                &id,
                temp.path().join(format!("{id}.log")),
                u64::try_from(index + 1).expect("index fits"),
                false,
            );
            entry.owner_session_id = "owner-b".to_string();
            entry.status = JobStatus::Exited;
            entry.settled_sequence = Some(u64::try_from(index + 1).expect("index fits"));
            reg.jobs.insert(id, entry);
        }

        prune_settled_jobs(&mut reg);
        assert!(
            reg.jobs.contains_key(&owner_a_id),
            "one session's retained history must not be evicted by another session's ordinary cap"
        );
    }

    #[test]
    fn settled_job_retention_has_a_process_wide_backstop() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut reg = JobRegistry::default();
        for index in 0..=MAX_TOTAL_RETAINED_SETTLED_JOBS {
            let id = format!("global-owner-job-{index:03}");
            let mut entry = synthetic_entry(
                &id,
                temp.path().join(format!("{id}.log")),
                u64::try_from(index).expect("index fits"),
                false,
            );
            entry.owner_session_id = format!("global-owner-{index:03}");
            entry.status = JobStatus::Exited;
            entry.settled_sequence = Some(u64::try_from(index).expect("index fits"));
            reg.jobs.insert(id, entry);
        }

        prune_settled_jobs(&mut reg);
        assert_eq!(reg.jobs.len(), MAX_TOTAL_RETAINED_SETTLED_JOBS);
        assert!(!reg.jobs.contains_key("global-owner-job-000"));
        assert!(reg.jobs.contains_key(&format!(
            "global-owner-job-{MAX_TOTAL_RETAINED_SETTLED_JOBS:03}"
        )));
    }

    #[test]
    fn host_completion_notice_uses_follow_up_message_shape() {
        let marker = "[background tan tan-1 settled: completed]".to_string();
        let message = completion_notice_message(marker.clone());
        assert!(matches!(
            message,
            Message::User(UserMessage {
                content: UserContent::Text(text),
                ..
            }) if text == marker
        ));
    }

    #[test]
    fn completion_notices_are_drained_only_by_their_owner_session() {
        let owner_a = format!("owner-a-{}", uuid::Uuid::new_v4().simple());
        let owner_b = format!("owner-b-{}", uuid::Uuid::new_v4().simple());
        push_completion_notice(&owner_a, "notice-a").expect("owner-a notice");
        push_completion_notice(&owner_b, "notice-b").expect("owner-b notice");

        let first = take_completion_notices(&owner_a);
        assert_eq!(first.len(), 1);
        assert!(matches!(
            &first[0],
            Message::User(UserMessage {
                content: UserContent::Text(text),
                ..
            }) if text == "notice-a"
        ));
        assert!(
            take_completion_notices(&owner_a).is_empty(),
            "draining one owner must not duplicate its notice"
        );

        let second = take_completion_notices(&owner_b);
        assert_eq!(second.len(), 1);
        assert!(matches!(
            &second[0],
            Message::User(UserMessage {
                content: UserContent::Text(text),
                ..
            }) if text == "notice-b"
        ));
    }

    #[test]
    fn completion_notice_retention_is_fair_across_sessions() {
        let mut reg = JobRegistry::default();
        enqueue_completion_notice(&mut reg, "owner-a", "owner-a-notice");
        for index in 0..=MAX_COMPLETION_NOTICES_PER_SESSION {
            enqueue_completion_notice(&mut reg, "owner-b", &format!("owner-b-notice-{index}"));
        }

        assert_eq!(
            reg.notices
                .iter()
                .filter(|notice| notice.owner_session_id == "owner-a")
                .count(),
            1,
            "one owner's ordinary cap pressure must not evict another owner"
        );
        assert_eq!(
            reg.notices
                .iter()
                .filter(|notice| notice.owner_session_id == "owner-b")
                .count(),
            MAX_COMPLETION_NOTICES_PER_SESSION
        );
        assert!(
            reg.notices
                .iter()
                .all(|notice| notice.text != "owner-b-notice-0")
        );
    }

    #[test]
    fn completion_notice_retention_has_a_process_wide_backstop() {
        let mut reg = JobRegistry::default();
        for index in 0..=MAX_TOTAL_COMPLETION_NOTICES {
            enqueue_completion_notice(
                &mut reg,
                &format!("owner-{index}"),
                &format!("notice-{index}"),
            );
        }

        assert_eq!(reg.notices.len(), MAX_TOTAL_COMPLETION_NOTICES);
        assert!(reg.notices.iter().all(|notice| notice.text != "notice-0"));
        assert!(
            reg.notices
                .iter()
                .any(|notice| notice.text == format!("notice-{MAX_TOTAL_COMPLETION_NOTICES}"))
        );
    }

    #[test]
    fn restored_completion_notices_preserve_fifo_before_newer_registry_entries() {
        let mut reg = JobRegistry::default();
        enqueue_completion_notice(&mut reg, "owner-a", "newer");
        let restored = ["older-1", "older-2"]
            .into_iter()
            .map(|text| CompletionNotice {
                owner_session_id: "owner-a".to_string(),
                text: text.to_string(),
            })
            .collect();

        assert_eq!(restore_completion_notices_into(&mut reg, restored), 0);
        assert_eq!(
            reg.notices
                .iter()
                .map(|notice| notice.text.as_str())
                .collect::<Vec<_>>(),
            ["older-1", "older-2", "newer"]
        );
    }

    #[test]
    fn saturated_restore_keeps_the_newest_per_owner_batch() {
        let mut reg = JobRegistry::default();
        for index in
            MAX_COMPLETION_NOTICES_PER_SESSION..MAX_COMPLETION_NOTICES_PER_SESSION.saturating_mul(2)
        {
            enqueue_completion_notice(&mut reg, "owner-a", &format!("notice-{index}"));
        }
        let restored = (0..MAX_COMPLETION_NOTICES_PER_SESSION)
            .map(|index| CompletionNotice {
                owner_session_id: "owner-a".to_string(),
                text: format!("notice-{index}"),
            })
            .collect();

        assert_eq!(
            restore_completion_notices_into(&mut reg, restored),
            MAX_COMPLETION_NOTICES_PER_SESSION
        );
        assert_eq!(reg.notices.len(), MAX_COMPLETION_NOTICES_PER_SESSION);
        assert_eq!(
            reg.notices.front().map(|notice| notice.text.as_str()),
            Some("notice-64")
        );
        assert_eq!(
            reg.notices.back().map(|notice| notice.text.as_str()),
            Some("notice-127")
        );
    }

    #[test]
    fn host_completion_notice_rejects_empty_owner_without_consuming_capacity() {
        let valid_owner = format!("valid-owner-{}", uuid::Uuid::new_v4().simple());
        for (owner, text) in [("", "empty-owner"), ("   ", "blank-owner")] {
            let error = push_completion_notice(owner, text).expect_err("invalid owner");
            assert!(error.to_string().contains("PI_JOBS_SESSION_UNAVAILABLE"));
            assert!(
                take_completion_notices(owner).is_empty(),
                "an invalid owner must fail before consuming registry capacity"
            );
        }
        push_completion_notice(&valid_owner, "valid-notice").expect("valid notice");

        let notices = take_completion_notices(&valid_owner);
        assert_eq!(notices.len(), 1);
        assert!(matches!(
            &notices[0],
            Message::User(UserMessage {
                content: UserContent::Text(text),
                ..
            }) if text == "valid-notice"
        ));
    }

    #[test]
    fn retained_text_limits_are_utf8_safe_and_explicit() {
        let oversized = "界".repeat(MAX_COMPLETION_NOTICE_BYTES);
        let truncated = truncate_utf8_bytes(&oversized, MAX_COMPLETION_NOTICE_BYTES);
        assert!(truncated.len() <= MAX_COMPLETION_NOTICE_BYTES);
        assert!(truncated.ends_with("\n...[truncated]"));

        let mut reg = JobRegistry::default();
        enqueue_completion_notice(&mut reg, TEST_SESSION_ID, &oversized);
        let retained = reg.notices.pop_front().expect("bounded notice");
        assert!(retained.text.len() <= MAX_COMPLETION_NOTICE_BYTES);
        assert!(retained.text.ends_with("\n...[truncated]"));
    }

    #[test]
    fn cross_session_job_ids_fail_closed_without_metadata() {
        let _guard = process_test_guard();
        let root = temp_root();
        let owner = format!("owner-{}", uuid::Uuid::new_v4().simple());
        let foreign_owner = format!("foreign-{}", uuid::Uuid::new_v4().simple());
        let id = format!("job-private-{}", uuid::Uuid::new_v4().simple());
        let artifact_path = root.join("private-artifact.log");
        let mut entry = synthetic_entry(&id, artifact_path.clone(), 0, false);
        entry.owner_session_id.clone_from(&owner);
        entry.command = "printf private-command".to_string();
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .jobs
            .insert(id.clone(), entry);

        assert_eq!(list(&owner).expect("owner list").len(), 1);
        assert!(list(&foreign_owner).expect("foreign list").is_empty());
        for error in [
            wait(&foreign_owner, &id, Duration::ZERO).expect_err("foreign wait"),
            request_cancel(&foreign_owner, &id)
                .err()
                .expect("foreign cancel"),
        ] {
            let rendered = error.to_string();
            assert!(rendered.contains("PI_JOBS_UNKNOWN_ID"));
            assert!(!rendered.contains("private-command"));
            assert!(!rendered.contains(&artifact_path.display().to_string()));
        }

        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .jobs
            .remove(&id);
    }

    #[test]
    fn spawn_list_wait_cycle() {
        let _guard = process_test_guard();
        let root = temp_root();
        #[cfg(unix)]
        JOB_STREAM_PREPARE_CALLS.store(0, Ordering::Relaxed);
        let snapshot = spawn_background(
            TEST_SESSION_ID,
            &root,
            None,
            None,
            "echo job-output-marker",
            Some(30),
            Some(&root),
        )
        .expect("spawn");
        #[cfg(unix)]
        assert_eq!(
            JOB_STREAM_PREPARE_CALLS.load(Ordering::Relaxed),
            2,
            "the production spawn path must make both pipe readers cancellation-aware"
        );
        assert_eq!(snapshot.status, "running");
        let settled = wait(TEST_SESSION_ID, &snapshot.id, Duration::from_secs(10)).expect("wait");
        assert_eq!(settled.status, "exited");
        assert_eq!(settled.exit_code, Some(0));
        assert!(settled.output_tail.contains("job-output-marker"));
        assert!(settled.output_complete);
        assert!(!settled.artifact_truncated);
        assert!(settled.artifact_error.is_none());
        assert!(std::path::Path::new(&settled.artifact_path).exists());
        let listed = list(TEST_SESSION_ID).expect("list");
        assert!(listed.iter().any(|job| job.id == settled.id));
        let notices = take_completion_notices(TEST_SESSION_ID);
        assert!(
            notices.iter().any(|message| matches!(
                message,
                Message::User(UserMessage {
                    content: UserContent::Text(text),
                    ..
                }) if text.contains(&settled.id) && text.contains("job-output-marker")
            )),
            "settled publication and its completion notice must be atomic"
        );
    }

    #[test]
    fn background_metadata_excludes_configured_shell_prefix() {
        let _guard = process_test_guard();
        let root = temp_root();
        let prefix_secret = "PI_PRIVATE_PREFIX_MARKER=must-not-leak";
        let user_command = "printf prefix-metadata-ok";
        let snapshot = spawn_background(
            TEST_SESSION_ID,
            &root,
            None,
            Some(prefix_secret),
            user_command,
            Some(30),
            Some(&root),
        )
        .expect("spawn with configured prefix");
        assert_eq!(snapshot.command, user_command);
        assert!(!snapshot.command.contains(prefix_secret));
        let settled = wait(TEST_SESSION_ID, &snapshot.id, Duration::from_secs(10))
            .expect("prefixed job settles");
        assert_eq!(settled.command, user_command);
        assert!(!settled.command.contains(prefix_secret));

        let notices = take_completion_notices(TEST_SESSION_ID);
        let rendered = notices
            .iter()
            .filter_map(|message| match message {
                Message::User(UserMessage {
                    content: UserContent::Text(text),
                    ..
                }) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains(user_command));
        assert!(!rendered.contains(prefix_secret));
    }

    #[test]
    fn background_metadata_bounds_retained_user_command() {
        let _guard = process_test_guard();
        let root = temp_root();
        let user_command = format!(
            "printf retained-command-ok\n# {}",
            "界".repeat(MAX_RETAINED_COMMAND_BYTES / 3 + 100)
        );
        assert!(user_command.len() > MAX_RETAINED_COMMAND_BYTES);

        let snapshot = spawn_background(
            TEST_SESSION_ID,
            &root,
            None,
            None,
            &user_command,
            Some(30),
            Some(&root),
        )
        .expect("spawn with oversized user command");
        assert!(snapshot.command.len() <= MAX_RETAINED_COMMAND_BYTES);
        assert!(snapshot.command.ends_with("\n...[truncated]"));

        let settled = wait(TEST_SESSION_ID, &snapshot.id, Duration::from_secs(10))
            .expect("oversized-command job settles");
        assert_eq!(settled.command, snapshot.command);
        assert!(settled.output_tail.contains("retained-command-ok"));
        let _ = take_completion_notices(TEST_SESSION_ID);
    }

    #[test]
    fn settled_waits_accept_extreme_durations_without_overflow() {
        let _guard = process_test_guard();
        let root = temp_root();
        let id = format!("job-huge-wait-{}", uuid::Uuid::new_v4().simple());
        let mut entry = synthetic_entry(&id, root.join(format!("{id}.log")), 0, false);
        entry.status = JobStatus::Exited;
        entry.exit_code = Some(0);
        entry.output_complete = true;
        let snapshot = JobSnapshot::from_source_best_effort(&JobSnapshotSource::from_entry(&entry));
        *entry
            .settled_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(snapshot);
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .jobs
            .insert(id.clone(), entry);

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let async_snapshot = runtime
            .block_on(wait_async(TEST_SESSION_ID, &id, Duration::MAX))
            .expect("async settled wait");
        assert_eq!(async_snapshot.status, "exited");
        let sync_snapshot = wait(TEST_SESSION_ID, &id, Duration::MAX).expect("sync settled wait");
        assert_eq!(sync_snapshot.status, "exited");

        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .jobs
            .remove(&id);
    }

    #[test]
    fn async_wait_slices_do_not_become_premature_deadlines() {
        let now = Instant::now();
        let deadline = now.checked_add(Duration::from_hours(3));
        assert_eq!(
            remaining_wait_slice(now, deadline),
            Some(MAX_ASYNC_WAIT_SLICE)
        );
        let after_one_slice = now
            .checked_add(MAX_ASYNC_WAIT_SLICE)
            .expect("one-hour instant");
        assert_eq!(
            remaining_wait_slice(after_one_slice, deadline),
            Some(MAX_ASYNC_WAIT_SLICE),
            "an intermediate timer wake must continue waiting"
        );
        assert_eq!(
            remaining_wait_slice(deadline.expect("representable deadline"), deadline),
            None
        );
        assert_eq!(
            remaining_wait_slice(now, now.checked_add(Duration::MAX)),
            Some(MAX_ASYNC_WAIT_SLICE),
            "an unrepresentable deadline must remain a bounded infinite wait"
        );
    }

    #[test]
    fn async_wait_continues_after_intermediate_timer_slice() {
        let _guard = process_test_guard();
        let root = temp_root();
        let id = format!("job-sliced-wait-{}", uuid::Uuid::new_v4().simple());
        let entry = synthetic_entry(&id, root.join(format!("{id}.log")), 0, false);
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .jobs
            .insert(id.clone(), entry);

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let remained_pending = runtime.block_on(async {
            let cx = crate::agent_cx::AgentCx::for_current_or_request();
            let now = cx
                .cx()
                .timer_driver()
                .map_or_else(asupersync::time::wall_now, |timer| timer.now());
            let waiting = wait_async_with_slice(
                TEST_SESSION_ID,
                &id,
                Duration::from_secs(1),
                Duration::from_millis(5),
            )
            .fuse();
            let observation = asupersync::time::sleep(now, Duration::from_millis(30)).fuse();
            futures::pin_mut!(waiting, observation);
            matches!(
                futures::future::select(waiting, observation).await,
                futures::future::Either::Right(((), _))
            )
        });
        assert!(
            remained_pending,
            "an intermediate timer slice must not complete a still-running job wait"
        );

        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .jobs
            .remove(&id);
    }

    #[test]
    fn cancel_kills_running_job() {
        let _guard = process_test_guard();
        let root = temp_root();
        let snapshot = spawn_background(
            TEST_SESSION_ID,
            &root,
            None,
            None,
            "trap '' TERM; echo cancel-ready; while :; do sleep 1; done",
            Some(120),
            Some(&root),
        )
        .expect("spawn");
        wait_for_output(&snapshot.id, "cancel-ready", Duration::from_secs(2));
        let started = Instant::now();
        let cancelled = cancel(TEST_SESSION_ID, &snapshot.id).expect("cancel");
        assert_eq!(cancelled.status, "killed");
        assert!(
            started.elapsed() >= TERMINATE_GRACE,
            "TERM-ignoring job must reach KILL escalation"
        );
        assert!(cancelled.output_complete);
        #[cfg(unix)]
        assert!(!process_exists(snapshot.pid.expect("pid")));
        // Drain the completion notice (pushed asynchronously by the monitor
        // thread) so it cannot leak into concurrently running agent-loop
        // tests through the process-global follow-up queue.
        for _ in 0..200 {
            let drained = take_completion_notices(TEST_SESSION_ID);
            if drained.iter().any(|message| {
                matches!(
                    message,
                    Message::User(UserMessage {
                        content: UserContent::Text(text),
                        ..
                    }) if text.contains(&snapshot.id)
                )
            }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn wait_rejects_unknown_id() {
        let err = wait(
            TEST_SESSION_ID,
            "job-does-not-exist",
            Duration::from_millis(10),
        )
        .unwrap_err();
        assert!(err.to_string().contains("PI_JOBS_UNKNOWN_ID"));
    }

    #[test]
    fn artifact_creation_failure_happens_before_process_spawn() {
        let _guard = process_test_guard();
        let root = temp_root();
        std::fs::write(root.join("jobs"), "not a directory").expect("conflicting jobs path");
        let marker = root.join("spawned-marker");
        let command = format!("printf ran > '{}'", marker.display());

        let err = spawn_background(
            TEST_SESSION_ID,
            &root,
            None,
            None,
            &command,
            Some(30),
            Some(&root),
        )
        .expect_err("artifact creation must fail");
        assert!(
            err.to_string()
                .contains("Failed to create jobs artifact dir")
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !marker.exists(),
            "process must not spawn before artifact setup"
        );
        assert_eq!(registry().lock().expect("registry").starting_jobs, 0);
    }

    #[test]
    fn timeout_remains_running_until_term_ignoring_process_is_reaped() {
        let _guard = process_test_guard();
        let root = temp_root();
        let snapshot = spawn_background(
            TEST_SESSION_ID,
            &root,
            None,
            None,
            "trap '' TERM; echo timeout-ready; while :; do sleep 1; done",
            Some(1),
            Some(&root),
        )
        .expect("spawn");
        wait_for_output(&snapshot.id, "timeout-ready", Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(1200));
        let during_grace =
            wait(TEST_SESSION_ID, &snapshot.id, Duration::ZERO).expect("snapshot during grace");
        assert_eq!(during_grace.status, "running");

        let settled = wait(TEST_SESSION_ID, &snapshot.id, Duration::from_secs(8))
            .expect("timed out job settles");
        assert_eq!(settled.status, "timedOut");
        assert!(settled.output_complete);
        #[cfg(unix)]
        assert!(!process_exists(snapshot.pid.expect("pid")));
    }

    #[cfg(unix)]
    #[test]
    fn natural_root_exit_reaps_a_descendant_holding_output_pipes() {
        let _guard = process_test_guard();
        let root = temp_root();
        let descendant_pid_path = root.join("descendant.pid");
        let command = format!(
            "(sleep 300 & printf '%s' \"$!\" > '{}') &",
            descendant_pid_path.display()
        );
        let snapshot = spawn_background(
            TEST_SESSION_ID,
            &root,
            None,
            None,
            &command,
            Some(30),
            Some(&root),
        )
        .expect("spawn descendant fixture");
        let settled = wait(TEST_SESSION_ID, &snapshot.id, Duration::from_secs(10))
            .expect("root and descendant settle");
        assert_eq!(settled.status, "exited");
        assert!(
            settled.output_complete,
            "descendant pipe holders must be reaped before settlement"
        );

        let descendant_pid = std::fs::read_to_string(&descendant_pid_path)
            .expect("descendant pid fixture")
            .parse::<u32>()
            .expect("numeric descendant pid");
        for _ in 0..100 {
            if !process_exists(descendant_pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !process_exists(descendant_pid),
            "descendant {descendant_pid} survived natural root exit"
        );
        let _ = take_completion_notices(TEST_SESSION_ID);
    }

    #[test]
    fn monitor_panic_settles_the_published_job_and_wakes_waiters() {
        let _guard = process_test_guard();
        let root = temp_root();
        let owner = format!("monitor-panic-{}", uuid::Uuid::new_v4().simple());
        let _hooks = install_spawn_background_test_hooks(SpawnBackgroundTestHooks {
            owner_session_id: Some(owner.clone()),
            panic_monitor_after_start: true,
            ..SpawnBackgroundTestHooks::default()
        });

        let snapshot = spawn_background(
            &owner,
            &root,
            None,
            None,
            "sleep 300",
            Some(300),
            Some(&root),
        )
        .expect("monitor thread creation succeeds before injected panic");
        let settled = wait(&owner, &snapshot.id, Duration::from_secs(5))
            .expect("monitor panic publishes terminal snapshot");
        assert_eq!(settled.status, JobStatus::Failed.as_str());
        assert_eq!(settled.pid, None);
        assert!(!settled.output_complete);
        #[cfg(unix)]
        assert_eq!(JOB_PUMP_THREADS_IN_FLIGHT.load(Ordering::Acquire), 0);
        assert_eq!(take_completion_notices(&owner).len(), 1);
        assert!(take_completion_notices(&owner).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn monitor_spawn_failure_joins_pumps_even_with_an_escaped_writer() {
        struct KillPidOnDrop(u32);

        impl Drop for KillPidOnDrop {
            fn drop(&mut self) {
                let _ = std::process::Command::new("/bin/kill")
                    .args(["-KILL", &self.0.to_string()])
                    .status();
            }
        }

        let _guard = process_test_guard();
        let root = temp_root();
        let owner = format!("monitor-spawn-fail-{}", uuid::Uuid::new_v4().simple());
        let descendant_pid_path = root.join("monitor-spawn-fail-descendant.pid");
        let script = root.join("monitor-spawn-fail-escape.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\n/bin/sh -c 'printf \"%s\" \"$$\" > \"$1\"; exec /bin/sleep 300' child \"$1\" &\n",
        )
        .expect("escaped descriptor fixture");
        let hook_pid_path = descendant_pid_path.clone();
        let pumps_observed = Arc::new(AtomicBool::new(false));
        let hook_pumps_observed = Arc::clone(&pumps_observed);
        let captured_wait = Arc::new(Mutex::new(None));
        let hook_captured_wait = Arc::clone(&captured_wait);
        let hook_owner = owner.clone();
        let publication_gate = Arc::new(SpawnHookGate::default());
        let hook_publication_gate = Arc::clone(&publication_gate);
        let _hooks = install_spawn_background_test_hooks(SpawnBackgroundTestHooks {
            owner_session_id: Some(owner.clone()),
            before_registry_publication: Some(Arc::new(move |_| {
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline {
                    if hook_pid_path.exists()
                        && JOB_PUMP_THREADS_IN_FLIGHT.load(Ordering::Acquire) == 2
                    {
                        hook_pumps_observed.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            })),
            after_registry_publication: Some(Arc::new(move |id| {
                *hook_captured_wait
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(wait_handle(&hook_owner, id).expect("published wait handle"));
                hook_publication_gate.enter_and_wait();
            })),
            fail_monitor_spawn: true,
            ..SpawnBackgroundTestHooks::default()
        });
        let command = format!(
            "setsid /bin/sh '{}' '{}' &",
            script.display(),
            descendant_pid_path.display()
        );

        let spawn_owner = owner.clone();
        let spawn_root = root.clone();
        let spawn = std::thread::spawn(move || {
            spawn_background(
                &spawn_owner,
                &spawn_root,
                None,
                None,
                &command,
                Some(300),
                Some(&spawn_root),
            )
        });
        publication_gate.wait_until_entered();
        let descendant_pid = std::fs::read_to_string(&descendant_pid_path)
            .expect("escaped descendant pid")
            .parse::<u32>()
            .expect("numeric escaped descendant pid");
        let descendant_cleanup = KillPidOnDrop(descendant_pid);
        let captured_wait = captured_wait
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("publication hook captures a wait handle");
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("waiter runtime");
        let failed_snapshot = runtime.block_on(async {
            let notification = captured_wait
                .settled_notify
                .wait_until(|| settled_snapshot(&captured_wait).is_some())
                .fuse();
            futures::pin_mut!(notification);
            assert!(
                futures::poll!(notification.as_mut()).is_pending(),
                "waiter must be registered before monitor creation fails"
            );
            publication_gate.release();
            let now = asupersync::time::wall_now();
            let timeout = asupersync::time::sleep(now, Duration::from_secs(5)).fuse();
            futures::pin_mut!(timeout);
            match futures::future::select(notification, timeout).await {
                futures::future::Either::Left(((), _)) => settled_snapshot(&captured_wait)
                    .expect("notification publishes the failed snapshot"),
                futures::future::Either::Right(((), _)) => {
                    panic!("monitor-spawn failure did not wake the registered waiter")
                }
            }
        });
        let error = spawn
            .join()
            .expect("monitor-failure spawn thread")
            .expect_err("monitor creation failure must reject the background job");
        publication_gate.assert_not_timed_out();

        assert!(error.to_string().contains("Failed to start job monitor"));
        assert!(
            pumps_observed.load(Ordering::Acquire),
            "both pump workers must be live before monitor creation fails"
        );
        assert_eq!(JOB_PUMP_THREADS_IN_FLIGHT.load(Ordering::Acquire), 0);
        assert_eq!(failed_snapshot.status, JobStatus::Failed.as_str());
        assert!(!failed_snapshot.output_complete);
        let reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            reg.jobs.values().all(|job| job.owner_session_id != owner),
            "a rejected monitor handoff must remove its published registry entry"
        );
        drop(reg);
        assert!(take_completion_notices(&owner).is_empty());

        let artifacts = std::fs::read_dir(root.join("jobs"))
            .expect("jobs artifact directory")
            .filter_map(std::result::Result::ok)
            .filter(|entry| is_managed_job_artifact_name(&entry.file_name()))
            .collect::<Vec<_>>();
        assert_eq!(artifacts.len(), 1);
        let artifact = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(artifacts[0].path())
            .expect("rejected job artifact remains inspectable");
        fs4::FileExt::try_lock(&artifact).expect("rejected job artifact lock must be released");
        fs4::FileExt::unlock(&artifact).expect("release test artifact lock");

        drop(descendant_cleanup);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn escaped_descriptor_holder_cannot_outlive_its_pump_worker() {
        struct KillPidOnDrop(u32);

        impl Drop for KillPidOnDrop {
            fn drop(&mut self) {
                let _ = std::process::Command::new("/bin/kill")
                    .args(["-KILL", &self.0.to_string()])
                    .status();
            }
        }

        let _guard = process_test_guard();
        let root = temp_root();
        let descendant_pid_path = root.join("escaped-descendant.pid");
        let late_write_trigger = root.join("escaped-descendant-write-now");
        let script = root.join("escape-pipes.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\n/bin/sh -c 'printf \"%s\" \"$$\" > \"$1\"; while [ ! -e \"$2\" ]; do /bin/sleep 0.05; done; printf \"%s\" \"late-output-marker\"; exec /bin/sleep 300' child \"$1\" \"$2\" &\n",
        )
        .expect("escaped descriptor fixture");
        let command = format!(
            "setsid /bin/sh '{}' '{}' '{}' &",
            script.display(),
            descendant_pid_path.display(),
            late_write_trigger.display()
        );
        let snapshot = spawn_background(
            TEST_SESSION_ID,
            &root,
            None,
            None,
            &command,
            Some(30),
            Some(&root),
        )
        .expect("spawn escaped descriptor fixture");
        wait_for_path(&descendant_pid_path, Duration::from_secs(2));
        let descendant_pid = std::fs::read_to_string(&descendant_pid_path)
            .expect("escaped descendant pid")
            .parse::<u32>()
            .expect("numeric escaped descendant pid");
        let descendant_cleanup = KillPidOnDrop(descendant_pid);

        let started = Instant::now();
        let settled = wait(TEST_SESSION_ID, &snapshot.id, Duration::from_secs(10))
            .expect("escaped descriptor job settles");
        assert_eq!(settled.status, "exited");
        assert!(
            !settled.output_complete,
            "the escaped writer prevents natural EOF and must be reported"
        );
        assert!(
            started.elapsed() < OUTPUT_DRAIN_GRACE + Duration::from_secs(2),
            "pump cancellation must remain bounded"
        );
        assert_eq!(
            JOB_PUMP_THREADS_IN_FLIGHT.load(Ordering::Acquire),
            0,
            "terminal publication must not leave detached pipe workers"
        );
        let artifact_before =
            std::fs::read(&settled.artifact_path).expect("read artifact at terminal publication");
        std::fs::write(&late_write_trigger, b"").expect("release escaped late writer");
        std::thread::sleep(Duration::from_millis(200));
        let after_late_write = wait(TEST_SESSION_ID, &snapshot.id, Duration::ZERO)
            .expect("terminal snapshot remains queryable");
        assert_eq!(after_late_write.output_tail, settled.output_tail);
        assert_eq!(
            std::fs::read(&settled.artifact_path).expect("re-read terminal artifact"),
            artifact_before,
            "an escaped writer cannot mutate the artifact after terminal publication"
        );

        drop(descendant_cleanup);
        for _ in 0..100 {
            if !process_exists(descendant_pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !process_exists(descendant_pid),
            "escaped descriptor fixture {descendant_pid} survived explicit cleanup"
        );
        let _ = take_completion_notices(TEST_SESSION_ID);
    }

    #[test]
    fn concurrent_spawns_respect_capacity() {
        let _guard = process_test_guard();
        let root = temp_root();
        let barrier = Arc::new(std::sync::Barrier::new(MAX_CONCURRENT_JOBS + 2));
        let mut callers = Vec::new();
        for _ in 0..=MAX_CONCURRENT_JOBS {
            let caller_root = root.clone();
            let caller_barrier = Arc::clone(&barrier);
            callers.push(std::thread::spawn(move || {
                caller_barrier.wait();
                spawn_background(
                    TEST_SESSION_ID,
                    &caller_root,
                    None,
                    None,
                    "sleep 60",
                    Some(120),
                    Some(&caller_root),
                )
            }));
        }
        barrier.wait();
        let results: Vec<_> = callers
            .into_iter()
            .map(|caller| caller.join().expect("spawn caller"))
            .collect();
        let succeeded: Vec<_> = results
            .iter()
            .filter_map(|result| result.as_ref().ok())
            .collect();
        let rejected: Vec<_> = results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .collect();
        assert_eq!(succeeded.len(), MAX_CONCURRENT_JOBS);
        assert_eq!(rejected.len(), 1);
        assert!(rejected[0].to_string().contains("PI_JOBS_AT_CAPACITY"));

        for snapshot in succeeded {
            cancel(TEST_SESSION_ID, &snapshot.id).expect("cleanup capacity test job");
        }
    }
}
