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
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use asupersync::sync::Notify;
use futures::FutureExt;
use fs4::FileExt as _;
use serde::Serialize;

use crate::error::{Error, Result};
use crate::model::{Message, UserContent, UserMessage};

/// Future returned by the live session-identity resolver shared by the jobs
/// tools and their completion-notice fetcher.
pub type JobSessionIdFuture = futures::future::BoxFuture<'static, Option<String>>;

/// Resolves the session that owns a job operation at the instant it runs.
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

/// Background-job artifacts are never deleted automatically. Refuse
/// new jobs before the dedicated directory can exceed this aggregate budget.
const MAX_TOTAL_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

/// Bound inode consumption independently from bytes (for example, jobs that
/// produce no output still create an artifact).
const MAX_ARTIFACT_FILES: usize = 4096;

/// Pipes normally reach EOF immediately after the process tree is reaped. A
/// bounded drain prevents an escaped descendant that retained a descriptor
/// from blocking terminal publication forever.
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Very large async waits are represented as repeated bounded timer sleeps so
/// `Instant` overflow never turns a valid request into a panic.
const MAX_ASYNC_WAIT_SLICE: Duration = Duration::from_secs(60 * 60);

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
    pub output_tail: String,
    pub artifact_truncated: bool,
    pub artifact_error: Option<String>,
    pub output_complete: bool,
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

    fn seal(&mut self) {
        self.sealed = true;
    }
}

#[derive(Default)]
struct JobRegistry {
    jobs: HashMap<String, JobEntry>,
    starting_jobs: usize,
    next_job_sequence: u64,
    next_settled_sequence: u64,
    notices: VecDeque<CompletionNotice>,
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
                match artifact.try_lock_exclusive() {
                    Ok(()) => fs4::FileExt::unlock(&artifact)?,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        bytes = bytes.saturating_add(
                            u64::try_from(MAX_ARTIFACT_BYTES)
                                .unwrap_or(u64::MAX)
                                .saturating_sub(metadata.len()),
                        );
                    }
                    Err(err) => return Err(err),
                }
            }
        }
    }
    Ok((bytes, entries))
}

fn ensure_artifact_budget(
    jobs_dir: &Path,
    max_bytes: u64,
    max_entries: usize,
) -> Result<()> {
    let (stored_bytes, stored_entries) = artifact_directory_usage(jobs_dir)
        .map_err(|err| Error::tool("bash", format!("Failed to inspect jobs artifact dir: {err}")))?;
    let reserved_bytes = u64::try_from(MAX_ARTIFACT_BYTES).unwrap_or(u64::MAX);
    if stored_entries >= max_entries
        || stored_bytes.saturating_add(reserved_bytes) > max_bytes
    {
        return Err(Error::tool(
            "bash",
            format!(
                "PI_JOBS_ARTIFACT_CAPACITY: refusing a new background job because {} accounts for {stored_entries} entries and {stored_bytes} bytes (limits: {max_entries} entries, {max_bytes} bytes including live-job reservations)",
                jobs_dir.display()
            ),
        ));
    }
    Ok(())
}

fn acquire_artifact_budget_lock(jobs_dir: &Path) -> Result<std::fs::File> {
    let lock_path = jobs_dir.join(".artifact-budget.lock");
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
    lock.lock_exclusive().map_err(|err| {
        Error::tool(
            "bash",
            format!("Failed to acquire jobs artifact budget lock: {err}"),
        )
    })?;
    Ok(lock)
}

fn create_job_artifact(jobs_dir: &Path, id: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    let artifact_path = jobs_dir.join(format!("{id}.log"));
    let artifact = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&artifact_path)?;
    artifact.lock_exclusive()?;
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
/// Named `PI_JOBS_AT_CAPACITY` when 8 jobs are already running; tool errors
/// for spawn/artifact failures.
#[allow(clippy::too_many_lines)]
pub fn spawn_background(
    owner_session_id: &str,
    cwd: &Path,
    shell_path: Option<&str>,
    command_prefix: Option<&str>,
    command: &str,
    timeout_secs: Option<u64>,
    artifact_root: Option<&Path>,
) -> Result<JobSnapshot> {
    // Serialize the fallible spawn-to-monitor ownership transfer with
    // `kill_all`, so shutdown cannot miss a child between OS spawn and
    // registry publication.
    let _lifecycle = lifecycle_lock()
        .lock()
        .map_err(|_| Error::tool("jobs", "jobs lifecycle lock poisoned".to_string()))?;
    if owner_session_id.trim().is_empty() {
        return Err(Error::tool(
            "jobs",
            "PI_JOBS_SESSION_UNAVAILABLE: current agent session identity is unavailable"
                .to_string(),
        ));
    }
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
    ensure_artifact_budget(
        &jobs_dir,
        MAX_TOTAL_ARTIFACT_BYTES,
        MAX_ARTIFACT_FILES,
    )?;
    let (artifact_path, artifact) = create_job_artifact(&jobs_dir, &id)
        .map_err(|e| Error::tool("bash", format!("Failed to create job artifact: {e}")))?;
    drop(artifact_budget_lock);
    let artifact = Arc::new(Mutex::new(ArtifactSink::new(
        artifact,
        MAX_ARTIFACT_BYTES,
    )));
    let tail = Arc::new(Mutex::new(TailBuffer::new(OUTPUT_TAIL_BYTES)));
    let output_sealed = Arc::new(AtomicBool::new(false));
    let settled_snapshot = Arc::new(Mutex::new(None));
    let settled_notify = Arc::new(Notify::new());
    let cancel_deadline = Arc::new(CancelDeadline::new());

    let started_at = Instant::now();
    let started_at_ms = now_ms();
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::tool("bash", format!("Failed to spawn shell: {e}")))?;
    if !crate::tools::attach_child_job_discipline(&child) {
        crate::tools::kill_process_group_tree(Some(child.id()));
        let _ = child.kill();
        let _ = child.wait();
        return Err(Error::tool(
            "bash",
            "Failed to attach background shell to platform process-tree discipline".to_string(),
        ));
    }
    let pid = child.id();
    let mut child = BackgroundChild::new(child);
    let stdout = child
        .child
        .as_mut()
        .and_then(|child| child.stdout.take())
        .ok_or_else(|| Error::tool("bash", "Missing stdout".to_string()))?;
    let stderr = child
        .child
        .as_mut()
        .and_then(|child| child.stderr.take())
        .ok_or_else(|| Error::tool("bash", "Missing stderr".to_string()))?;

    // Pump threads: dedicated OS threads for the same reason as the
    // foreground path (unbounded blocking reads must not starve the
    // runtime's blocking pool).
    let stdout_tail = Arc::clone(&tail);
    let stdout_artifact = Arc::clone(&artifact);
    let stdout_sealed = Arc::clone(&output_sealed);
    let stdout_pump = std::thread::Builder::new()
        .name(format!("pi-job-{id}-stdout"))
        .spawn(move || {
            pump_job_stream(
                stdout,
                &stdout_artifact,
                &stdout_tail,
                &stdout_sealed,
            )
        })
        .map_err(|err| {
            Error::tool(
                "bash",
                format!("Failed to start job stdout pump: {err}"),
            )
        })?;
    let stderr_tail = Arc::clone(&tail);
    let stderr_artifact = Arc::clone(&artifact);
    let stderr_sealed = Arc::clone(&output_sealed);
    let stderr_pump = match std::thread::Builder::new()
        .name(format!("pi-job-{id}-stderr"))
        .spawn(move || {
            pump_job_stream(
                stderr,
                &stderr_artifact,
                &stderr_tail,
                &stderr_sealed,
            )
        })
    {
        Ok(handle) => handle,
        Err(err) => {
            child.kill_and_wait();
            let _ = stdout_pump.join();
            return Err(Error::tool(
                "bash",
                format!("Failed to start job stderr pump: {err}"),
            ));
        }
    };

    let snapshot_source = {
        let mut reg = registry()
            .lock()
            .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
        reg.jobs.insert(
            id.clone(),
            JobEntry {
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
                tail: Arc::clone(&tail),
                artifact: Arc::clone(&artifact),
                output_complete: false,
                cancel_requested: false,
                process_live: true,
                settled_snapshot,
                settled_notify,
                cancel_deadline,
            },
        );
        slot.commit(&mut reg);
        reg.jobs
            .get(&id)
            .map(JobSnapshotSource::from_entry)
            .ok_or_else(|| Error::tool("jobs", "job vanished after spawn".to_string()))?
    };

    // The monitor is the sole process owner from this point through reap and
    // bounded output drain. If thread creation fails, dropping the captured
    // child guard kills and reaps the process before this function returns.
    let monitor_id = id.clone();
    let monitor_artifact = Arc::clone(&artifact);
    let monitor_tail = Arc::clone(&tail);
    if let Err(err) = std::thread::Builder::new()
        .name(format!("pi-job-{id}-monitor"))
        .spawn(move || {
            monitor_job(
                &monitor_id,
                child,
                started_at,
                timeout_secs,
                stdout_pump,
                stderr_pump,
                output_sealed,
                monitor_artifact,
                monitor_tail,
            );
        })
    {
        if let Ok(mut reg) = registry().lock() {
            reg.jobs.remove(&id);
        }
        return Err(Error::tool(
            "bash",
            format!("Failed to start job monitor: {err}"),
        ));
    }

    // Ownership has reached the monitor before inspecting I/O-owned state.
    // A pump stalled in filesystem I/O must not strand the child, lifecycle
    // lock, and cancellation path inside this spawn call.
    Ok(JobSnapshot::from_source_best_effort(&snapshot_source))
}

fn pump_job_stream<R: Read>(
    mut reader: R,
    artifact: &Mutex<ArtifactSink>,
    tail: &Mutex<TailBuffer>,
    output_sealed: &AtomicBool,
) -> std::io::Result<()> {
    let mut chunk = [0u8; 8192];
    loop {
        if output_sealed.load(Ordering::Acquire) {
            return Ok(());
        }
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
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
    handle: std::thread::JoinHandle<std::io::Result<()>>,
    deadline: Instant,
) -> bool {
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !handle.is_finished() {
        return false;
    }
    matches!(handle.join(), Ok(Ok(())))
}

fn last_chars(text: &str, cap: usize) -> String {
    let char_count = text.chars().count();
    text.chars()
        .skip(char_count.saturating_sub(cap))
        .collect()
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

fn monitor_job(
    id: &str,
    mut child: BackgroundChild,
    started_at: Instant,
    timeout_secs: Option<u64>,
    stdout_pump: std::thread::JoinHandle<std::io::Result<()>>,
    stderr_pump: std::thread::JoinHandle<std::io::Result<()>>,
    output_sealed: Arc<AtomicBool>,
    artifact: Arc<Mutex<ArtifactSink>>,
    tail: Arc<Mutex<TailBuffer>>,
) {
    let timeout = timeout_secs.map(Duration::from_secs);
    let mut terminate_at: Option<Instant> = None;
    let mut termination_status: Option<JobStatus> = None;
    let root_pid = child.id();

    let exit_code = loop {
        // Keep the nonblocking reap observation and `process_live` transition
        // under the same registry lock used by cancellation. Once wait reports
        // an exit, no concurrent caller can relabel that natural exit as a
        // cancellation of a recycled numeric PID.
        let wait_result = {
            let mut reg = registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let result = child.try_wait();
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
                child.disarm();
                break status.code();
            }
            Ok(None) => {}
            Err(_) => break child.kill_and_wait(),
        }

        let now = Instant::now();
        if let Some(deadline) = terminate_at {
            if now >= deadline {
                break child.kill_and_wait();
            }
        } else {
            let cancel_requested = registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .jobs
                .get(id)
                .map(|job| job.cancel_requested)
                .unwrap_or(false);
            if cancel_requested {
                termination_status = Some(JobStatus::Killed);
                crate::tools::terminate_process_group_tree(child.id());
                terminate_at = Some(now + TERMINATE_GRACE);
            } else if let Some(timeout) = timeout
                && now.duration_since(started_at) >= timeout
            {
                termination_status = Some(JobStatus::TimedOut);
                crate::tools::terminate_process_group_tree(child.id());
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
    let stdout_complete = finish_pump(stdout_pump, drain_deadline);
    let stderr_complete = finish_pump(stderr_pump, drain_deadline);
    let output_complete = stdout_complete && stderr_complete;
    output_sealed.store(true, Ordering::Release);
    if output_complete {
        artifact
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .seal();
        tail.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .seal();
    } else {
        if let Ok(mut artifact) = artifact.try_lock() {
            artifact.seal();
        }
        if let Ok(mut tail) = tail.try_lock() {
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
}

fn settle_job_and_enqueue_notice(
    id: &str,
    status: JobStatus,
    exit_code: Option<i32>,
    output_complete: bool,
) {
    // Build the potentially I/O-contended snapshot without holding the global
    // registry. The best-effort mode prevents a blocked artifact write from
    // stalling settlement or unrelated job operations.
    let source = {
        let reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reg.jobs.get(id).map(|job| {
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
    let notice = format!(
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
    );

    let notify = {
        let mut reg = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        *job
            .settled_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(snapshot);
        let notify = Arc::clone(&job.settled_notify);
        let owner_session_id = job.owner_session_id.clone();
        job.cancel_deadline.finish();
        enqueue_completion_notice(&mut reg, &owner_session_id, notice);
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
        remove_ids.extend(
            settled
                .iter()
                .take(remove_count)
                .map(|(_, id)| id.clone()),
        );
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
            (
                job.sequence,
                settled,
                JobSnapshotSource::from_entry(job),
            )
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

async fn wait_async_with_slice(
    owner_session_id: &str,
    id: &str,
    timeout: Duration,
    max_wait_slice: Duration,
) -> Result<JobSnapshot> {
    let handle = wait_handle(owner_session_id, id)?;
    let cx = crate::agent_cx::AgentCx::for_current_or_request();
    let now = cx
        .cx()
        .timer_driver()
        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
    let deadline = now.checked_add(timeout);
    loop {
        if let Some(snapshot) = settled_snapshot(&handle) {
            return Ok(snapshot);
        }
        let now = cx
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        let Some(sleep_for) = remaining_wait_slice(now, deadline)
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
    enqueue_completion_notice(&mut reg, owner_session_id, text);
    Ok(())
}

fn enqueue_completion_notice(reg: &mut JobRegistry, owner_session_id: &str, text: String) {
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
        text: truncate_utf8_bytes(&text, MAX_COMPLETION_NOTICE_BYTES),
    });
    while reg.notices.len() > MAX_TOTAL_COMPLETION_NOTICES {
        let _ = reg.notices.pop_front();
    }
}

/// Kill every running job (session exit). Called once from the main
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

    fn temp_root() -> PathBuf {
        static NEXT_ROOT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT_ROOT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "pi-jobs-test-{}-{sequence}",
            std::process::id()
        ));
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

    #[test]
    fn aggregate_artifact_budget_refuses_bytes_and_entries() {
        let _guard = process_test_guard();
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("one.log"), b"12345").expect("first artifact");
        let bytes_error = ensure_artifact_budget(temp.path(), 4, 10)
            .expect_err("stored bytes above the budget must refuse new jobs");
        assert!(bytes_error.to_string().contains("PI_JOBS_ARTIFACT_CAPACITY"));

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
        live.lock_exclusive().expect("reserve live artifact");

        let two_job_budget = u64::try_from(MAX_ARTIFACT_BYTES)
            .expect("artifact cap fits")
            .saturating_mul(2);
        let error = ensure_artifact_budget(temp.path(), two_job_budget - 1, 10)
            .expect_err("live artifact plus prospective job must reserve two full caps");
        assert!(error.to_string().contains("PI_JOBS_ARTIFACT_CAPACITY"));
        fs4::FileExt::unlock(&live).expect("release live artifact reservation");
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
                std::fs::write(marker_dir.join("holder-acquired"), b"")
                    .expect("holder marker");
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
        wait_for_path(
            &marker_dir.join("holder-acquired"),
            Duration::from_secs(2),
        );
        let mut probe = spawn_child("probe");
        wait_for_path(
            &marker_dir.join("probe-attempted"),
            Duration::from_secs(2),
        );
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
            .expect_err("reaped process must not be signalled");
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
            let file = std::fs::File::create(temp.path().join(format!("{id}.log")))
                .expect("artifact");
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
        let mut owner_a = synthetic_entry(
            &owner_a_id,
            temp.path().join("owner-a.log"),
            0,
            false,
        );
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
        enqueue_completion_notice(&mut reg, "owner-a", "owner-a-notice".to_string());
        for index in 0..=MAX_COMPLETION_NOTICES_PER_SESSION {
            enqueue_completion_notice(
                &mut reg,
                "owner-b",
                format!("owner-b-notice-{index}"),
            );
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
        assert!(reg
            .notices
            .iter()
            .all(|notice| notice.text != "owner-b-notice-0"));
    }

    #[test]
    fn completion_notice_retention_has_a_process_wide_backstop() {
        let mut reg = JobRegistry::default();
        for index in 0..=MAX_TOTAL_COMPLETION_NOTICES {
            enqueue_completion_notice(
                &mut reg,
                &format!("owner-{index}"),
                format!("notice-{index}"),
            );
        }

        assert_eq!(reg.notices.len(), MAX_TOTAL_COMPLETION_NOTICES);
        assert!(reg.notices.iter().all(|notice| notice.text != "notice-0"));
        assert!(reg
            .notices
            .iter()
            .any(|notice| notice.text == format!("notice-{MAX_TOTAL_COMPLETION_NOTICES}")));
    }

    #[test]
    fn restored_completion_notices_preserve_fifo_before_newer_registry_entries() {
        let mut reg = JobRegistry::default();
        enqueue_completion_notice(&mut reg, "owner-a", "newer".to_string());
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
        for index in MAX_COMPLETION_NOTICES_PER_SESSION
            ..MAX_COMPLETION_NOTICES_PER_SESSION.saturating_mul(2)
        {
            enqueue_completion_notice(&mut reg, "owner-a", format!("notice-{index}"));
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
            reg.notices
                .front()
                .map(|notice| notice.text.as_str()),
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
        enqueue_completion_notice(&mut reg, TEST_SESSION_ID, oversized);
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
            request_cancel(&foreign_owner, &id).expect_err("foreign cancel"),
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
        assert_eq!(snapshot.status, "running");
        let settled = wait(TEST_SESSION_ID, &snapshot.id, Duration::from_secs(10))
            .expect("wait");
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
        let sync_snapshot =
            wait(TEST_SESSION_ID, &id, Duration::MAX).expect("sync settled wait");
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
        let deadline = now.checked_add(Duration::from_secs(3 * 60 * 60));
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
            let observation =
                asupersync::time::sleep(now, Duration::from_millis(30)).fuse();
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
        std::fs::write(root.join("jobs"), "not a directory")
            .expect("conflicting jobs path");
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
        assert!(err.to_string().contains("Failed to create jobs artifact dir"));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!marker.exists(), "process must not spawn before artifact setup");
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
        let during_grace = wait(TEST_SESSION_ID, &snapshot.id, Duration::ZERO)
            .expect("snapshot during grace");
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
        let succeeded: Vec<_> = results.iter().filter_map(|result| result.as_ref().ok()).collect();
        let rejected: Vec<_> = results.iter().filter_map(|result| result.as_ref().err()).collect();
        assert_eq!(succeeded.len(), MAX_CONCURRENT_JOBS);
        assert_eq!(rejected.len(), 1);
        assert!(rejected[0].to_string().contains("PI_JOBS_AT_CAPACITY"));

        for snapshot in succeeded {
            cancel(TEST_SESSION_ID, &snapshot.id).expect("cleanup capacity test job");
        }
    }
}
