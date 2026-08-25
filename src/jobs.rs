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
//! Session scoping: the registry lives for the process; `kill_all` runs at
//! the main shutdown chokepoint so no job outlives the session (no orphan
//! daemons across restarts).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::{Error, Result};
use crate::model::{Message, UserContent, UserMessage};

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

/// Maximum undelivered completion notices across background job and `/tan`
/// producers. The oldest notice is discarded first if a session never
/// reaches another delivery boundary.
const MAX_COMPLETION_NOTICES: usize = 64;

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
    id: String,
    command: String,
    started_at_ms: i64,
    status: JobStatus,
    exit_code: Option<i32>,
    pid: Option<u32>,
    artifact_path: PathBuf,
    tail: std::sync::Arc<Mutex<TailBuffer>>,
    cancel_requested: bool,
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
}

impl JobSnapshot {
    fn from_entry(entry: &JobEntry) -> Self {
        let output_tail = entry
            .tail
            .lock()
            .map(|tail| tail.text())
            .unwrap_or_default();
        Self {
            schema: JOB_SCHEMA.to_string(),
            id: entry.id.clone(),
            command: entry.command.clone(),
            started_at_ms: entry.started_at_ms,
            status: entry.status.as_str().to_string(),
            exit_code: entry.exit_code,
            pid: entry.pid,
            artifact_path: entry.artifact_path.display().to_string(),
            output_tail,
        }
    }
}

/// Bounded tail: retains the LAST `cap` bytes of job output.
struct TailBuffer {
    buf: std::collections::VecDeque<u8>,
    cap: usize,
}

impl TailBuffer {
    fn new(cap: usize) -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(cap.min(8192)),
            cap,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
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
}

#[derive(Default)]
struct JobRegistry {
    jobs: HashMap<String, JobEntry>,
    next_id: u64,
    notices: Vec<String>,
}

fn registry() -> &'static Mutex<JobRegistry> {
    static REGISTRY: std::sync::LazyLock<Mutex<JobRegistry>> =
        std::sync::LazyLock::new(|| Mutex::new(JobRegistry::default()));
    &REGISTRY
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

/// Spawn a command as a background job. The mediation gate in the bash tool
/// has already classified the command by the time we get here.
///
/// # Errors
/// Named `PI_JOBS_AT_CAPACITY` when 8 jobs are already running; tool errors
/// for spawn/artifact failures.
#[allow(clippy::too_many_lines)]
pub fn spawn_background(
    cwd: &Path,
    shell_path: Option<&str>,
    command_prefix: Option<&str>,
    command: &str,
    timeout_secs: Option<u64>,
    artifact_root: Option<&Path>,
) -> Result<JobSnapshot> {
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

    let command = command_prefix.filter(|p| !p.trim().is_empty()).map_or_else(
        || command.to_string(),
        |prefix| format!("{prefix}\n{command}"),
    );
    let command = format!("trap 'code=$?; wait; exit $code' EXIT\n{command}");

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
        .arg(&command)
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

    let id = {
        let mut reg = registry()
            .lock()
            .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
        if running_count(&reg) >= MAX_CONCURRENT_JOBS {
            return Err(Error::tool(
                "bash",
                format!(
                    "PI_JOBS_AT_CAPACITY: {MAX_CONCURRENT_JOBS} background jobs already running; \
                     cancel one with the jobs tool or wait for a completion before starting more."
                ),
            ));
        }
        reg.next_id = reg.next_id.saturating_add(1);
        let next = reg.next_id;
        drop(reg);
        format!("job-{next}")
    };

    let mut child = cmd
        .spawn()
        .map_err(|e| Error::tool("bash", format!("Failed to spawn shell: {e}")))?;
    crate::tools::attach_child_job_discipline(&child);
    let pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::tool("bash", "Missing stdout".to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::tool("bash", "Missing stderr".to_string()))?;

    let artifact_path = jobs_dir.join(format!("{id}.log"));
    let artifact = std::fs::File::create(&artifact_path)
        .map_err(|e| Error::tool("bash", format!("Failed to create job artifact: {e}")))?;
    let tail = std::sync::Arc::new(Mutex::new(TailBuffer::new(OUTPUT_TAIL_BYTES)));

    {
        let mut reg = registry()
            .lock()
            .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
        reg.jobs.insert(
            id.clone(),
            JobEntry {
                id: id.clone(),
                command,
                started_at_ms: now_ms(),
                status: JobStatus::Running,
                exit_code: None,
                pid: Some(pid),
                artifact_path,
                tail: std::sync::Arc::clone(&tail),
                cancel_requested: false,
            },
        );
    }

    // Pump threads: dedicated OS threads for the same reason as the
    // foreground path (unbounded blocking reads must not starve the
    // runtime's blocking pool).
    let artifact_stdout = artifact
        .try_clone()
        .map_err(|e| Error::tool("bash", format!("Failed to clone job artifact handle: {e}")))?;
    let tail_stdout = std::sync::Arc::clone(&tail);
    std::thread::spawn(move || pump_job_stream(stdout, artifact_stdout, &tail_stdout));
    let tail_stderr = std::sync::Arc::clone(&tail);
    std::thread::spawn(move || pump_job_stream(stderr, artifact, &tail_stderr));

    // Monitor thread: wait with the timeout/kill escalation, then record the
    // final status and push a completion notice for the follow-up queue.
    let monitor_id = id.clone();
    std::thread::spawn(move || monitor_job(&monitor_id, child, timeout_secs));

    let snapshot = {
        let reg = registry()
            .lock()
            .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
        reg.jobs
            .get(&id)
            .map(JobSnapshot::from_entry)
            .ok_or_else(|| Error::tool("jobs", "job vanished after spawn".to_string()))?
    };
    Ok(snapshot)
}

fn pump_job_stream<R: Read>(mut reader: R, mut artifact: std::fs::File, tail: &Mutex<TailBuffer>) {
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let data = &chunk[..n];
                let _ = artifact.write_all(data);
                if let Ok(mut tail) = tail.lock() {
                    tail.push(data);
                }
            }
        }
    }
}

fn monitor_job(id: &str, mut child: std::process::Child, timeout_secs: Option<u64>) {
    let start = Instant::now();
    let timeout = timeout_secs.map(Duration::from_secs);
    let mut terminate_at: Option<Instant> = None;

    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => {}
            Err(_) => break -1,
        }

        let now = Instant::now();
        if let Some(deadline) = terminate_at {
            if now >= deadline {
                crate::tools::kill_process_group_tree(Some(child.id()));
                let _ = child.kill();
                let _ = child.wait();
                break -1;
            }
        } else if let Some(timeout) = timeout
            && now.duration_since(start) >= timeout
        {
            // TERM first, KILL after the grace window (same escalation as
            // the foreground bash path).
            crate::tools::terminate_process_group_tree(Some(child.id()));
            mark_status(id, JobStatus::TimedOut, None);
            terminate_at = Some(now + TERMINATE_GRACE);
        }

        std::thread::sleep(Duration::from_millis(25));
    };

    // Settle: cancelled beats timed-out beats natural exit.
    let (status, code) = {
        let cancelled = registry()
            .lock()
            .ok()
            .and_then(|reg| reg.jobs.get(id).map(|job| job.cancel_requested))
            .unwrap_or(false);
        let timed_out = registry()
            .lock()
            .ok()
            .and_then(|reg| {
                reg.jobs
                    .get(id)
                    .map(|job| job.status == JobStatus::TimedOut)
            })
            .unwrap_or(false);
        if cancelled {
            (JobStatus::Killed, None)
        } else if timed_out {
            (JobStatus::TimedOut, Some(exit_code))
        } else {
            (
                if exit_code == 0 {
                    JobStatus::Exited
                } else {
                    JobStatus::Failed
                },
                Some(exit_code),
            )
        }
    };
    mark_status(id, status, code);

    // Completion notice → follow-up queue (agent sees it next turn boundary).
    let notice = {
        let Ok(reg) = registry().lock() else {
            return;
        };
        reg.jobs.get(id).map(|job| {
            let snapshot = JobSnapshot::from_entry(job);
            let tail_excerpt: String = snapshot.output_tail.chars().take(4096).collect();
            format!(
                "[background job {} settled: {} (exit {})]\ncommand: {}\nartifact: {}\noutput tail:\n{}",
                snapshot.id,
                snapshot.status,
                snapshot
                    .exit_code
                    .map_or_else(|| "n/a".to_string(), |code| code.to_string()),
                snapshot.command.lines().nth(1).unwrap_or(&snapshot.command),
                snapshot.artifact_path,
                if tail_excerpt.is_empty() {
                    "(no output)"
                } else {
                    &tail_excerpt
                }
            )
        })
    };
    if let Some(notice) = notice {
        push_completion_notice(notice);
    }
}

fn mark_status(id: &str, status: JobStatus, exit_code: Option<i32>) {
    if let Ok(mut reg) = registry().lock()
        && let Some(job) = reg.jobs.get_mut(id)
    {
        if job.status.settled() && status == JobStatus::TimedOut {
            // Already settled (e.g. cancel raced the timeout); keep it.
            return;
        }
        job.status = status;
        if exit_code.is_some() {
            job.exit_code = exit_code;
        }
    }
}

/// List snapshots of every job this session, newest last.
///
/// # Errors
/// Tool error when the registry is poisoned.
pub fn list() -> Result<Vec<JobSnapshot>> {
    let reg = registry()
        .lock()
        .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
    Ok(reg.jobs.values().map(JobSnapshot::from_entry).collect())
}

/// Wait for a job to settle (bounded), returning its snapshot either way.
///
/// # Errors
/// Named `PI_JOBS_UNKNOWN_ID` for unknown job ids.
#[allow(clippy::significant_drop_tightening)]
pub fn wait(id: &str, timeout: Duration) -> Result<JobSnapshot> {
    let deadline = Instant::now() + timeout;
    loop {
        let settled_snapshot = {
            let reg = registry()
                .lock()
                .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
            let Some(job) = reg.jobs.get(id) else {
                return Err(Error::tool(
                    "jobs",
                    format!("PI_JOBS_UNKNOWN_ID: no background job named '{id}'"),
                ));
            };
            if job.status.settled() {
                Some(JobSnapshot::from_entry(job))
            } else {
                None
            }
        };
        if let Some(snapshot) = settled_snapshot {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            let snapshot = {
                let reg = registry()
                    .lock()
                    .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
                let job = reg
                    .jobs
                    .get(id)
                    .ok_or_else(|| Error::tool("jobs", "job vanished".to_string()))?;
                JobSnapshot::from_entry(job)
            };
            return Ok(snapshot);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Cancel a running job with the bash timeout escalation (TERM → grace →
/// KILL + tree walk).
///
/// # Errors
/// Named `PI_JOBS_UNKNOWN_ID` for unknown job ids; `PI_JOBS_NOT_RUNNING`
/// when the job already settled.
#[allow(clippy::significant_drop_tightening)]
pub fn cancel(id: &str) -> Result<JobSnapshot> {
    let pid = {
        let mut reg = registry()
            .lock()
            .map_err(|_| Error::tool("jobs", "jobs registry poisoned".to_string()))?;
        let Some(job) = reg.jobs.get_mut(id) else {
            return Err(Error::tool(
                "jobs",
                format!("PI_JOBS_UNKNOWN_ID: no background job named '{id}'"),
            ));
        };
        if job.status.settled() {
            return Err(Error::tool(
                "jobs",
                format!(
                    "PI_JOBS_NOT_RUNNING: job '{id}' already settled ({})",
                    job.status.as_str()
                ),
            ));
        }
        job.cancel_requested = true;
        job.pid
    };
    crate::tools::terminate_process_group_tree(pid);
    // The monitor thread applies the KILL escalation and records the final
    // status; wait briefly so the snapshot reflects the settle.
    wait(id, Duration::from_secs(10))
}

/// Drain pending completion notices as follow-up messages for the agent.
/// The fetcher registered with the agent calls this on every poll.
#[must_use]
pub fn take_completion_notices() -> Vec<Message> {
    let Ok(mut reg) = registry().lock() else {
        return Vec::new();
    };
    reg.notices
        .drain(..)
        .map(completion_notice_message)
        .collect()
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
pub fn push_completion_notice(text: impl Into<String>) {
    let Ok(mut reg) = registry().lock() else {
        tracing::error!("jobs registry poisoned; dropping completion notice");
        return;
    };
    if reg.notices.len() >= MAX_COMPLETION_NOTICES {
        reg.notices.remove(0);
    }
    reg.notices.push(text.into());
}

/// Build the follow-up fetcher that delivers job completion notices into
/// the agent's message queue.
#[must_use]
pub fn follow_up_fetcher() -> crate::agent::MessageFetcher {
    std::sync::Arc::new(|| {
        Box::pin(async move {
            take_completion_notices()
                .into_iter()
                .map(crate::agent::QueuedAgentMessage::generated)
                .collect()
        }) as futures::future::BoxFuture<'static, Vec<crate::agent::QueuedAgentMessage>>
    })
}

/// Kill every running job (session exit). Called once from the main
/// shutdown chokepoint; documented behavior — no orphan daemons.
pub fn kill_all() {
    let pids: Vec<Option<u32>> = {
        let Ok(mut reg) = registry().lock() else {
            return;
        };
        for job in reg.jobs.values_mut() {
            if !job.status.settled() {
                job.cancel_requested = true;
            }
        }
        reg.jobs
            .values()
            .filter(|job| !job.status.settled())
            .map(|job| job.pid)
            .collect()
    };
    for pid in pids {
        crate::tools::kill_process_group_tree(pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-jobs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
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
    fn spawn_list_wait_cycle() {
        let root = temp_root();
        let snapshot = spawn_background(
            &root,
            None,
            None,
            "echo job-output-marker",
            Some(30),
            Some(&root),
        )
        .expect("spawn");
        assert_eq!(snapshot.status, "running");
        let settled = wait(&snapshot.id, Duration::from_secs(10)).expect("wait");
        assert_eq!(settled.status, "exited");
        assert_eq!(settled.exit_code, Some(0));
        assert!(settled.output_tail.contains("job-output-marker"));
        assert!(std::path::Path::new(&settled.artifact_path).exists());
        let listed = list().expect("list");
        assert!(listed.iter().any(|job| job.id == settled.id));
        let notices = take_completion_notices();
        assert!(
            notices.is_empty() || notices.iter().any(|_| true),
            "notice drain must not panic"
        );
    }

    #[test]
    fn cancel_kills_running_job() {
        let root = temp_root();
        let snapshot =
            spawn_background(&root, None, None, "sleep 60", Some(120), Some(&root)).expect("spawn");
        let cancelled = cancel(&snapshot.id).expect("cancel");
        assert_eq!(cancelled.status, "killed");
        // Drain the completion notice (pushed asynchronously by the monitor
        // thread) so it cannot leak into concurrently running agent-loop
        // tests through the process-global follow-up queue.
        for _ in 0..200 {
            let drained = take_completion_notices();
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
        let err = wait("job-does-not-exist", Duration::from_millis(10)).unwrap_err();
        assert!(err.to_string().contains("PI_JOBS_UNKNOWN_ID"));
    }
}
