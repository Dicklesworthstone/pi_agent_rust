//! Session-owned native process supervisor.
//!
//! This is the single subprocess stack behind the pinned Devin process tools
//! (`exec`, `shell_command`, `get_output`, `write_to_process`, `kill_shell`).
//! It deliberately reuses the primitives the native `bash` tool already relies
//! on (`command_with_default_sigpipe_in_dir`, process-group isolation, the
//! `sysinfo`-based group/tree termination helpers, `AgentCx` cancellation, and
//! `truncate_tail`) instead of introducing a second, divergent way to run
//! child processes.
//!
//! Every process the supervisor starts is owned by the session that created
//! it. Session cancellation and session stop terminate the whole process group
//! of each owned process. A process outlives the session only when the caller
//! explicitly asked for detachment, and that request is recorded on the
//! registry entry so it stays auditable.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, ToolUpdate, command_with_default_sigpipe_in_dir,
    exit_status_code, isolate_command_process_group, kill_process_group_tree,
    terminate_process_group_tree, truncate_tail,
};

/// Temp-file prefix for spilled process output. Shared with
/// [`crate::tools::cleanup_temp_files`] so stale artifacts are reaped.
pub const PROCESS_ARTIFACT_FILE_PREFIX: &str = "pi-devin-proc-";

/// Bytes retained in memory per stream, per process. Output beyond this is
/// dropped from the ring and remains available through the spill artifact.
pub const PROCESS_STREAM_BUFFER_BYTES: usize = 256 * 1024;

/// Bytes of combined output after which the supervisor spills to an artifact
/// file instead of growing the in-memory record.
pub const PROCESS_ARTIFACT_THRESHOLD_BYTES: usize = 64 * 1024;

/// Delay between SIGTERM and SIGKILL when terminating a process group.
pub const PROCESS_TERMINATE_GRACE: Duration = Duration::from_millis(1_500);

/// Default foreground timeout, matching the native `bash` tool.
pub const PROCESS_DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

const POLL_TICK: Duration = Duration::from_millis(10);

/// Minimum spacing between streaming updates. Each update re-reads and
/// re-truncates the retained buffer, so a chatty process would otherwise pay
/// that cost every `POLL_TICK` regardless of how few bytes it produced.
const UPDATE_INTERVAL: Duration = Duration::from_millis(100);

/// Upper bound on retained registry entries. Finished, non-detached entries are
/// evicted oldest-first once the registry grows past this.
const MAX_REGISTRY_ENTRIES: usize = 256;

/// Lifecycle status of a supervised process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    Running,
    Exited,
    Killed,
    TimedOut,
    Cancelled,
}

impl ProcessStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Killed => "killed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Which stream a chunk of output came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStream {
    Stdout,
    Stderr,
}

/// How the caller wants the process to be run.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// Command line handed to the shell.
    pub command: String,
    /// Working directory. Policy has already confirmed it is contained.
    pub cwd: PathBuf,
    /// Shell used to interpret `command`; defaults to bash then `sh`.
    pub shell: Option<String>,
    /// Foreground timeout. `None` disables the timeout.
    pub timeout: Option<Duration>,
    /// Return immediately with a process id instead of streaming to completion.
    pub background: bool,
    /// Keep the process alive past session cleanup. Explicit and audited.
    pub detached: bool,
    /// Keep stdin open for `write_to_process`. When false stdin is closed right
    /// after spawn so non-interactive commands observe EOF.
    pub interactive: bool,
    /// Tool that requested the spawn, recorded on the registry entry.
    pub tool_name: String,
}

impl SpawnRequest {
    #[must_use]
    pub fn new(command: impl Into<String>, cwd: impl Into<PathBuf>, tool_name: &str) -> Self {
        Self {
            command: command.into(),
            cwd: cwd.into(),
            shell: None,
            timeout: Some(PROCESS_DEFAULT_TIMEOUT),
            background: false,
            detached: false,
            interactive: false,
            tool_name: tool_name.to_string(),
        }
    }
}

/// Serializable view of one registry entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessRecord {
    pub id: String,
    pub command: String,
    pub cwd: PathBuf,
    pub tool_name: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub status: ProcessStatus,
    pub exit_code: Option<i32>,
    pub pid: Option<u32>,
    /// Process-group id. The supervisor puts every child in its own group, so
    /// this equals the child pid and is what termination targets.
    pub process_group: Option<u32>,
    pub background: bool,
    pub detached: bool,
    pub stdin_open: bool,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub dropped_bytes: usize,
    pub artifact_path: Option<PathBuf>,
}

/// Result of a completed foreground run, or of the initial background handoff.
#[derive(Debug, Clone)]
pub struct ProcessOutcome {
    pub record: ProcessRecord,
    pub output: String,
    pub truncated: bool,
}

impl ProcessOutcome {
    #[must_use]
    pub fn is_error(&self) -> bool {
        match self.record.status {
            ProcessStatus::Running => false,
            ProcessStatus::Exited => self.record.exit_code != Some(0),
            ProcessStatus::Killed | ProcessStatus::TimedOut | ProcessStatus::Cancelled => true,
        }
    }

    /// Artifact references suitable for an audit record. Never the output.
    #[must_use]
    pub fn artifact_refs(&self) -> Vec<String> {
        artifact_refs_for(self.record.artifact_path.as_deref())
    }
}

/// Audit artifact references for a spill artifact path.
///
/// The `file://` shape is part of the audit contract, so every producer of
/// artifact references goes through this one helper rather than formatting the
/// URI again at each call site.
#[must_use]
pub fn artifact_refs_for(path: Option<&Path>) -> Vec<String> {
    path.map_or_else(Vec::new, |path| vec![format!("file://{}", path.display())])
}

/// Bounded ring of one stream's output plus absolute offsets so `get_output`
/// can report incrementally and still detect what it missed.
#[derive(Debug)]
struct StreamBuffer {
    data: Vec<u8>,
    /// Absolute offset of `data[0]` in the stream.
    start: usize,
    /// Absolute offset just past the last byte produced.
    total: usize,
    limit: usize,
    /// Absolute offset already returned to `get_output`.
    cursor: usize,
}

impl StreamBuffer {
    const fn new(limit: usize) -> Self {
        Self {
            data: Vec::new(),
            start: 0,
            total: 0,
            limit,
            cursor: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.data.extend_from_slice(chunk);
        self.total = self.total.saturating_add(chunk.len());
        if self.data.len() > self.limit {
            let excess = self.data.len() - self.limit;
            self.data.drain(..excess);
            self.start = self.start.saturating_add(excess);
        }
    }

    /// Everything retained, regardless of the incremental cursor.
    fn retained(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }

    /// Bytes evicted from the ring before the reader could see them.
    const fn dropped(&self) -> usize {
        self.start
    }

    /// Consume everything produced since the last call. Returns the text and
    /// how many bytes were lost to eviction since the previous cursor.
    fn take_incremental(&mut self) -> (String, usize) {
        let from = self.cursor.max(self.start);
        let missed = from.saturating_sub(self.cursor);
        let offset = from - self.start;
        let text = String::from_utf8_lossy(&self.data[offset..]).into_owned();
        self.cursor = self.total;
        (text, missed)
    }
}

#[derive(Debug)]
struct ProcessOutputState {
    stdout: StreamBuffer,
    stderr: StreamBuffer,
    artifact: Option<std::fs::File>,
    artifact_path: Option<PathBuf>,
    artifact_failed: bool,
}

#[derive(Debug)]
struct ProcessLifecycle {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    status: ProcessStatus,
    exit_code: Option<i32>,
    ended_at: Option<DateTime<Utc>>,
}

/// One supervised process and everything the registry records about it.
#[derive(Debug)]
struct ProcessEntry {
    id: String,
    /// Session that owns this process. Part of the artifact file name because
    /// process ids restart at `proc-1` in every supervisor.
    session_id: String,
    command: String,
    cwd: PathBuf,
    tool_name: String,
    started_at: DateTime<Utc>,
    pid: Option<u32>,
    background: bool,
    detached: bool,
    lifecycle: Mutex<ProcessLifecycle>,
    output: Mutex<ProcessOutputState>,
}

impl ProcessEntry {
    fn lock_lifecycle(&self) -> std::sync::MutexGuard<'_, ProcessLifecycle> {
        self.lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_output(&self) -> std::sync::MutexGuard<'_, ProcessOutputState> {
        self.output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn ingest(&self, stream: ProcessStream, chunk: &[u8]) {
        let mut output = self.lock_output();
        let produced = output.stdout.total + output.stderr.total;
        match stream {
            ProcessStream::Stdout => output.stdout.push(chunk),
            ProcessStream::Stderr => output.stderr.push(chunk),
        }

        if output.artifact.is_none()
            && !output.artifact_failed
            && produced.saturating_add(chunk.len()) > PROCESS_ARTIFACT_THRESHOLD_BYTES
        {
            // `open_artifact` seeds the file from the retained buffers, which
            // already hold `chunk` because it was pushed above. Writing the
            // incremental chunk again here would duplicate it at the spill
            // boundary, so the tick that creates the artifact writes nothing
            // more.
            self.open_artifact(&mut output);
            return;
        }
        if let Some(file) = output.artifact.as_mut()
            && file.write_all(chunk).is_err()
        {
            output.artifact = None;
            output.artifact_failed = true;
        }
    }

    /// Create the spill artifact with owner-only permissions and seed it with
    /// whatever is still retained in memory.
    fn open_artifact(&self, output: &mut ProcessOutputState) {
        // Process ids are per-supervisor counters that restart at `proc-1`, so
        // the session id is what keeps two concurrent sessions from truncating
        // each other's artifact.
        let path = std::env::temp_dir().join(format!(
            "{PROCESS_ARTIFACT_FILE_PREFIX}{}-{}.log",
            sanitize_file_component(&self.session_id),
            sanitize_file_component(&self.id)
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                let seeded = file
                    .write_all(output.stdout.data.as_slice())
                    .and_then(|()| file.write_all(output.stderr.data.as_slice()));
                if seeded.is_err() {
                    output.artifact_failed = true;
                    return;
                }
                output.artifact_path = Some(path);
                output.artifact = Some(file);
            }
            Err(err) => {
                tracing::debug!("failed to open process artifact {}: {err}", path.display());
                output.artifact_failed = true;
            }
        }
    }

    /// Reap the child without blocking and fold the exit status into the
    /// lifecycle. Returns the terminal status once the child is gone.
    fn poll_exit(&self) -> Option<ProcessStatus> {
        let mut lifecycle = self.lock_lifecycle();
        let outcome = if lifecycle.status.is_terminal() {
            Some(lifecycle.status)
        } else {
            match lifecycle.child.as_mut().map(std::process::Child::try_wait) {
                None | Some(Ok(None)) => None,
                Some(Ok(Some(status))) => {
                    lifecycle.exit_code = Some(exit_status_code(status));
                    lifecycle.status = ProcessStatus::Exited;
                    lifecycle.ended_at = Some(Utc::now());
                    lifecycle.child = None;
                    lifecycle.stdin = None;
                    Some(ProcessStatus::Exited)
                }
                Some(Err(_)) => {
                    lifecycle.status = ProcessStatus::Exited;
                    lifecycle.exit_code = Some(-1);
                    lifecycle.ended_at = Some(Utc::now());
                    lifecycle.child = None;
                    lifecycle.stdin = None;
                    Some(ProcessStatus::Exited)
                }
            }
        };
        drop(lifecycle);
        outcome
    }

    /// Mark a supervisor-driven termination and drop the child handle. The
    /// caller is responsible for having signalled the process group.
    fn mark_terminated(&self, status: ProcessStatus, exit_code: Option<i32>) {
        let mut lifecycle = self.lock_lifecycle();
        if lifecycle.status.is_terminal() {
            return;
        }
        lifecycle.status = status;
        lifecycle.exit_code = exit_code;
        lifecycle.ended_at = Some(Utc::now());
        lifecycle.stdin = None;
        if let Some(mut child) = lifecycle.child.take() {
            let _ = child.kill();
            // Reap on a helper thread so a stubborn child never blocks the
            // agent loop; the group signal has already been delivered.
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }

    fn record(&self) -> ProcessRecord {
        let lifecycle = self.lock_lifecycle();
        let status = lifecycle.status;
        let exit_code = lifecycle.exit_code;
        let ended_at = lifecycle.ended_at;
        let stdin_open = lifecycle.stdin.is_some();
        drop(lifecycle);

        let output = self.lock_output();
        ProcessRecord {
            id: self.id.clone(),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            tool_name: self.tool_name.clone(),
            started_at: self.started_at,
            ended_at,
            status,
            exit_code,
            pid: self.pid,
            process_group: self.pid,
            background: self.background,
            detached: self.detached,
            stdin_open,
            stdout_bytes: output.stdout.total,
            stderr_bytes: output.stderr.total,
            dropped_bytes: output.stdout.dropped() + output.stderr.dropped(),
            artifact_path: output.artifact_path.clone(),
        }
    }

    fn produced_bytes(&self) -> usize {
        let output = self.lock_output();
        let produced = output.stdout.total + output.stderr.total;
        drop(output);
        produced
    }

    /// Combined retained output, stdout first, for foreground reporting.
    fn retained_output(&self) -> String {
        let output = self.lock_output();
        let mut text = output.stdout.retained();
        let stderr = output.stderr.retained();
        drop(output);
        if !stderr.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&stderr);
        }
        text
    }
}

/// Session-owned registry of supervised processes.
#[derive(Debug)]
pub struct ProcessSupervisor {
    session_id: String,
    next_id: AtomicU64,
    entries: Mutex<HashMap<String, Arc<ProcessEntry>>>,
}

/// Shared handle installed on the tool registry and on session teardown paths.
pub type SharedProcessSupervisor = Arc<ProcessSupervisor>;

impl ProcessSupervisor {
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            next_id: AtomicU64::new(1),
            entries: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn shared(session_id: impl Into<String>) -> SharedProcessSupervisor {
        Arc::new(Self::new(session_id))
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<ProcessEntry>>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn entry(&self, process_id: &str) -> Result<Arc<ProcessEntry>> {
        self.lock_entries()
            .get(process_id)
            .map(Arc::clone)
            .ok_or_else(|| {
                Error::tool(
                    "process_supervisor",
                    format!("unknown process id `{process_id}`"),
                )
            })
    }

    /// Every registry entry, oldest first.
    #[must_use]
    pub fn records(&self) -> Vec<ProcessRecord> {
        let mut records: Vec<ProcessRecord> = self
            .lock_entries()
            .values()
            .map(|entry| entry.record())
            .collect();
        records.sort_by_key(|record| record.started_at);
        records
    }

    /// One registry entry.
    pub fn record(&self, process_id: &str) -> Result<ProcessRecord> {
        Ok(self.entry(process_id)?.record())
    }

    fn spawn(&self, request: &SpawnRequest) -> Result<Arc<ProcessEntry>> {
        if !request.cwd.is_dir() {
            return Err(Error::tool(
                "process_supervisor",
                format!(
                    "working directory does not exist: {}",
                    request.cwd.display()
                ),
            ));
        }

        let shell = request.shell.clone().unwrap_or_else(default_shell);
        let mut command =
            command_with_default_sigpipe_in_dir(&shell, &request.cwd).map_err(|err| {
                Error::tool(
                    "process_supervisor",
                    format!("failed to prepare shell: {err}"),
                )
            })?;
        command
            .arg("-c")
            .arg(&request.command)
            .current_dir(&request.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Own process group so the whole tree can be signalled later, even if
        // the shell itself exits first.
        isolate_command_process_group(&mut command);

        let mut child = command.spawn().map_err(|err| {
            Error::tool(
                "process_supervisor",
                format!("failed to spawn process: {err}"),
            )
        })?;

        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();
        let stdin = if request.interactive { stdin } else { None };

        let id = format!("proc-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let entry = Arc::new(ProcessEntry {
            id: id.clone(),
            session_id: self.session_id.clone(),
            command: request.command.clone(),
            cwd: request.cwd.clone(),
            tool_name: request.tool_name.clone(),
            started_at: Utc::now(),
            pid: Some(pid),
            background: request.background,
            detached: request.detached,
            lifecycle: Mutex::new(ProcessLifecycle {
                child: Some(child),
                stdin,
                status: ProcessStatus::Running,
                exit_code: None,
                ended_at: None,
            }),
            output: Mutex::new(ProcessOutputState {
                stdout: StreamBuffer::new(PROCESS_STREAM_BUFFER_BYTES),
                stderr: StreamBuffer::new(PROCESS_STREAM_BUFFER_BYTES),
                artifact: None,
                artifact_path: None,
                artifact_failed: false,
            }),
        });

        // Dedicated pump threads, matching the reasoning in the bash tool: the
        // read loop blocks until EOF, so it must not occupy a runtime blocking
        // slot for the life of a long-running or background process.
        if let Some(stdout) = stdout {
            let sink = Arc::clone(&entry);
            thread::spawn(move || pump(stdout, &sink, ProcessStream::Stdout));
        }
        if let Some(stderr) = stderr {
            let sink = Arc::clone(&entry);
            thread::spawn(move || pump(stderr, &sink, ProcessStream::Stderr));
        }

        let mut entries = self.lock_entries();
        entries.insert(id, Arc::clone(&entry));
        if entries.len() > MAX_REGISTRY_ENTRIES {
            prune_finished(&mut entries);
        }
        drop(entries);
        Ok(entry)
    }

    /// Start a process and stream its output through `on_update` until it
    /// exits, times out, or the ambient context is cancelled.
    pub async fn run_foreground(
        &self,
        request: SpawnRequest,
        on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
    ) -> Result<ProcessOutcome> {
        let entry = self.spawn(&request)?;
        let cx = AgentCx::for_current_or_request();
        let started = std::time::Instant::now();
        let mut emitted = 0_usize;
        let mut last_update = started;
        // Why the cause is tracked locally rather than written straight onto
        // the entry: closing the entry early would make `poll_exit` report a
        // terminal status on the next tick and short-circuit the grace period,
        // so a process that ignores SIGTERM would never be SIGKILLed.
        let mut cause: Option<ProcessStatus> = None;
        let mut terminate_deadline: Option<std::time::Instant> = None;

        loop {
            if entry.poll_exit().is_some() {
                break;
            }

            let produced = entry.produced_bytes();
            if produced != emitted && last_update.elapsed() >= UPDATE_INTERVAL {
                emitted = produced;
                last_update = std::time::Instant::now();
                emit_update(&entry, started, request.timeout, on_update);
            }

            if let Some(deadline) = terminate_deadline {
                if std::time::Instant::now() >= deadline {
                    // Grace expired: SIGKILL the group and the whole tree.
                    kill_process_group_tree(entry.pid);
                    entry.mark_terminated(cause.unwrap_or(ProcessStatus::Killed), Some(-1));
                    break;
                }
            } else if let Some(timeout) = request.timeout
                && started.elapsed() >= timeout
            {
                cause = Some(ProcessStatus::TimedOut);
                terminate_process_group_tree(entry.pid);
                terminate_deadline = Some(std::time::Instant::now() + PROCESS_TERMINATE_GRACE);
            } else if cx.checkpoint().is_err() {
                cause = Some(ProcessStatus::Cancelled);
                terminate_process_group_tree(entry.pid);
                terminate_deadline = Some(std::time::Instant::now() + PROCESS_TERMINATE_GRACE);
            }

            cx.time().sleep(POLL_TICK).await;
        }

        // Give the pump threads a moment to flush the tail of the pipes.
        let drain_deadline = std::time::Instant::now() + Duration::from_millis(250);
        let mut settled = entry.produced_bytes();
        while std::time::Instant::now() < drain_deadline {
            cx.time().sleep(POLL_TICK).await;
            let produced = entry.produced_bytes();
            if produced == settled {
                break;
            }
            settled = produced;
        }

        entry.finish_pending_termination();
        // A process that exits during the grace window still exited *because*
        // it timed out or was cancelled, so report that cause rather than a
        // bare `exited`.
        if let Some(cause) = cause {
            entry.attribute_termination(cause);
        }
        emit_update(&entry, started, request.timeout, on_update);
        Ok(finish(&entry, request.detached))
    }

    /// Start a process and return as soon as it is registered.
    pub fn start_background(&self, request: SpawnRequest) -> Result<ProcessOutcome> {
        let mut request = request;
        request.background = true;
        let entry = self.spawn(&request)?;
        let record = entry.record();
        let output = format!(
            "Started background process `{}` (pid {}). Use `get_output` to read incremental output and `kill_shell` to stop it.",
            record.id,
            record
                .pid
                .map_or_else(|| "unknown".to_string(), |pid| pid.to_string())
        );
        Ok(ProcessOutcome {
            record,
            output,
            truncated: false,
        })
    }

    /// Read everything produced since the previous `get_output` call.
    pub fn get_output(&self, process_id: &str) -> Result<(ProcessRecord, String, usize)> {
        let entry = self.entry(process_id)?;
        entry.poll_exit();
        let (stdout, stdout_missed) = {
            let mut output = entry.lock_output();
            output.stdout.take_incremental()
        };
        let (stderr, stderr_missed) = {
            let mut output = entry.lock_output();
            output.stderr.take_incremental()
        };

        let mut text = stdout;
        if !stderr.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&stderr);
        }
        Ok((entry.record(), text, stdout_missed + stderr_missed))
    }

    /// Write to a live process's stdin.
    pub fn write_to_process(
        &self,
        process_id: &str,
        data: &str,
        close_stdin: bool,
    ) -> Result<ProcessRecord> {
        let entry = self.entry(process_id)?;
        entry.poll_exit();
        let mut lifecycle = entry.lock_lifecycle();
        if lifecycle.status.is_terminal() {
            let status = lifecycle.status;
            drop(lifecycle);
            return Err(Error::tool(
                "write_to_process",
                format!(
                    "process `{process_id}` is not running (status: {})",
                    status.as_str()
                ),
            ));
        }
        let Some(mut stdin) = lifecycle.stdin.take() else {
            drop(lifecycle);
            return Err(Error::tool(
                "write_to_process",
                format!(
                    "stdin for process `{process_id}` is closed; start it with `interactive: true` to keep stdin open"
                ),
            ));
        };
        // The handle is taken out of the guard before the write: `write_all`
        // blocks whenever the child stops draining its stdin pipe, and holding
        // the lifecycle lock across that would wedge `poll_exit`,
        // `mark_terminated`, `kill`, and session shutdown for as long as the
        // child misbehaves.
        drop(lifecycle);

        let mut written = stdin.write_all(data.as_bytes());
        if written.is_ok() {
            written = stdin.flush();
        }

        let mut lifecycle = entry.lock_lifecycle();
        // Put the handle back only when it is still usable: a failed write
        // means the pipe is broken, `close_stdin` asked for EOF, and a process
        // that ended while the lock was released must not look interactive.
        if written.is_ok() && !close_stdin && !lifecycle.status.is_terminal() {
            lifecycle.stdin = Some(stdin);
        }
        drop(lifecycle);

        written.map_err(|err| {
            Error::tool(
                "write_to_process",
                format!("failed to write to process `{process_id}` stdin: {err}"),
            )
        })?;
        Ok(entry.record())
    }

    /// Terminate a process group gracefully, then forcefully after the grace
    /// period.
    pub async fn kill(&self, process_id: &str, grace: Option<Duration>) -> Result<ProcessRecord> {
        let entry = self.entry(process_id)?;
        if entry.poll_exit().is_some() {
            return Ok(entry.record());
        }
        Ok(terminate_entry(&entry, grace.unwrap_or(PROCESS_TERMINATE_GRACE)).await)
    }

    /// Terminate every process this session still owns.
    ///
    /// Detached processes are deliberately left running; they were requested
    /// explicitly and their registry entry records that choice.
    pub async fn shutdown(&self) -> Vec<ProcessRecord> {
        let entries: Vec<Arc<ProcessEntry>> = self
            .lock_entries()
            .values()
            .filter(|entry| !entry.detached)
            .map(Arc::clone)
            .collect();

        // One shared grace period for the whole session: signal every group
        // first, then wait once. Terminating entries one at a time would make
        // session stop cost up to `entries.len() * PROCESS_TERMINATE_GRACE`.
        let cx = AgentCx::for_current_or_request();
        let mut pending: Vec<&Arc<ProcessEntry>> = Vec::new();
        for entry in &entries {
            if entry.poll_exit().is_none() {
                terminate_process_group_tree(entry.pid);
                pending.push(entry);
            }
        }
        if !pending.is_empty() {
            let deadline = std::time::Instant::now() + PROCESS_TERMINATE_GRACE;
            while std::time::Instant::now() < deadline {
                pending.retain(|entry| entry.poll_exit().is_none());
                if pending.is_empty() {
                    break;
                }
                cx.time().sleep(POLL_TICK).await;
            }
            for entry in pending {
                kill_process_group_tree(entry.pid);
                entry.mark_terminated(ProcessStatus::Killed, Some(-1));
            }
        }
        let terminated: Vec<ProcessRecord> = entries.iter().map(|entry| entry.record()).collect();
        self.lock_entries()
            .retain(|_, entry| entry.detached || !entry.record().status.is_terminal());
        terminated
    }

    /// Best-effort synchronous teardown used by `Drop` and non-async callers.
    pub fn shutdown_blocking(&self) {
        let entries: Vec<Arc<ProcessEntry>> = self
            .lock_entries()
            .values()
            .filter(|entry| !entry.detached)
            .map(Arc::clone)
            .collect();
        for entry in entries {
            if entry.poll_exit().is_some() {
                continue;
            }
            terminate_process_group_tree(entry.pid);
            kill_process_group_tree(entry.pid);
            entry.mark_terminated(ProcessStatus::Killed, Some(-1));
        }
    }
}

impl Drop for ProcessSupervisor {
    fn drop(&mut self) {
        self.shutdown_blocking();
    }
}

impl ProcessEntry {
    /// Record why a stopped process stopped, overriding a plain `Exited` that
    /// `poll_exit` may have observed during the termination grace period.
    fn attribute_termination(&self, status: ProcessStatus) {
        let mut lifecycle = self.lock_lifecycle();
        lifecycle.status = status;
        lifecycle.ended_at.get_or_insert_with(Utc::now);
        lifecycle.stdin = None;
        drop(lifecycle);
    }

    /// Reap the child after a signalled termination so no zombie is left.
    fn finish_pending_termination(&self) {
        let mut lifecycle = self.lock_lifecycle();
        if let Some(mut child) = lifecycle.child.take() {
            if let Ok(Some(status)) = child.try_wait() {
                lifecycle
                    .exit_code
                    .get_or_insert_with(|| exit_status_code(status));
            } else {
                let _ = child.kill();
                thread::spawn(move || {
                    let _ = child.wait();
                });
                lifecycle.exit_code.get_or_insert(-1);
            }
        }
        if lifecycle.status == ProcessStatus::Running {
            lifecycle.status = ProcessStatus::Exited;
        }
        lifecycle.ended_at.get_or_insert_with(Utc::now);
        lifecycle.stdin = None;
    }
}

/// Build the caller-facing outcome for a finished foreground run.
fn finish(entry: &Arc<ProcessEntry>, detached: bool) -> ProcessOutcome {
    use std::fmt::Write as _;

    let record = entry.record();
    let raw = entry.retained_output();
    let truncation = truncate_tail(raw, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let truncated = truncation.truncated || record.dropped_bytes > 0;
    let mut output = if truncation.content.is_empty() {
        "(no output)".to_string()
    } else {
        truncation.content
    };

    if truncated {
        let artifact = record.artifact_path.as_ref().map_or_else(
            || " No artifact was written.".to_string(),
            |path| format!(" Full output artifact: {}", path.display()),
        );
        let _ = write!(
            output,
            "\n\n[Output truncated: {} bytes produced, {} dropped from the in-memory buffer.{artifact}]",
            record.stdout_bytes + record.stderr_bytes,
            record.dropped_bytes,
        );
    }

    match record.status {
        ProcessStatus::TimedOut => {
            output.push_str("\n\nProcess timed out and its process group was terminated.");
        }
        ProcessStatus::Cancelled => {
            output.push_str("\n\nProcess was cancelled and its process group was terminated.");
        }
        ProcessStatus::Killed => output.push_str("\n\nProcess group was killed."),
        ProcessStatus::Exited => {
            if record.exit_code != Some(0) {
                let _ = write!(
                    output,
                    "\n\nProcess exited with code {}",
                    record.exit_code.unwrap_or(-1)
                );
            }
        }
        ProcessStatus::Running => {}
    }

    if detached {
        output.push_str("\n\n[Detached: this process is exempt from session cleanup.]");
    }

    ProcessOutcome {
        record,
        output,
        truncated,
    }
}

fn prune_finished(entries: &mut HashMap<String, Arc<ProcessEntry>>) {
    let mut finished: Vec<(DateTime<Utc>, String)> = entries
        .iter()
        .filter(|(_, entry)| {
            let lifecycle = entry.lock_lifecycle();
            let terminal = lifecycle.status.is_terminal();
            drop(lifecycle);
            !entry.detached && terminal
        })
        .map(|(id, entry)| (entry.started_at, id.clone()))
        .collect();
    finished.sort_by_key(|(started_at, _)| *started_at);
    for (_, id) in finished
        .into_iter()
        .take(entries.len().saturating_sub(MAX_REGISTRY_ENTRIES))
    {
        entries.remove(&id);
    }
}

async fn terminate_entry(entry: &Arc<ProcessEntry>, grace: Duration) -> ProcessRecord {
    let cx = AgentCx::for_current_or_request();
    // Graceful first: SIGTERM the whole group so children get a chance to exit.
    terminate_process_group_tree(entry.pid);
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        if entry.poll_exit().is_some() {
            return entry.record();
        }
        cx.time().sleep(POLL_TICK).await;
    }
    // Then forceful: SIGKILL the group and the whole descendant tree.
    kill_process_group_tree(entry.pid);
    entry.mark_terminated(ProcessStatus::Killed, Some(-1));
    entry.record()
}

fn emit_update(
    entry: &Arc<ProcessEntry>,
    started: std::time::Instant,
    timeout: Option<Duration>,
    on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
) {
    let Some(callback) = on_update else {
        return;
    };
    let record = entry.record();
    let text = entry.retained_output();
    let truncation = truncate_tail(text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    callback(ToolUpdate {
        content: vec![ContentBlock::Text(TextContent::new(truncation.content))],
        details: Some(serde_json::json!({
            "schema": PROCESS_UPDATE_SCHEMA_V1,
            "processId": record.id,
            "status": record.status.as_str(),
            "progress": {
                "elapsedMs": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                "timeoutMs": timeout.map(|value| u64::try_from(value.as_millis()).unwrap_or(u64::MAX)),
                "byteCount": record.stdout_bytes + record.stderr_bytes,
                "droppedBytes": record.dropped_bytes,
            },
        })),
    });
}

/// Stable schema id for streaming process updates.
pub const PROCESS_UPDATE_SCHEMA_V1: &str = "pi.devin.process.update.v1";

fn pump<R: Read>(mut reader: R, entry: &Arc<ProcessEntry>, stream: ProcessStream) {
    let mut buf = vec![0_u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => entry.ingest(stream, &buf[..read]),
            Err(ref err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => {
                entry.ingest(
                    ProcessStream::Stderr,
                    format!("\n[process supervisor: failed to read {stream:?}: {err}]\n")
                        .as_bytes(),
                );
                break;
            }
        }
    }
}

/// Reduce an identifier to characters that are safe in a temp file name.
fn sanitize_file_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn default_shell() -> String {
    for candidate in ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"] {
        if Path::new(candidate).exists() {
            return candidate.to_string();
        }
    }
    "sh".to_string()
}
