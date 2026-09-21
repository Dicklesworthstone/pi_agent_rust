//! Agent hub registry (bd-cv653.5.3): a session-scoped roster of spawned
//! subagent children with transcript persistence, steering delivery, kill,
//! revive, and a minimal peer-messaging bus.
//!
//! Children are separate `pi` processes (`--mode json --print --no-session`),
//! so cross-process steering is delivered through an append-only queue file
//! per child (`<id>.steer`); the child's print-mode loop drains that file
//! through a steering [`crate::agent::MessageFetcher`] between turns. The
//! parent's in-memory registry is the roster of record; the on-disk queue
//! files are the delivery mechanism. Writers and readers coordinate through
//! a stable sidecar lock, including recovery of interrupted draining batches.
//!
//! NTM layer distinction: this hub manages pi's OWN spawned children in this
//! process. It does not rebuild ntm's cross-tmux fleet orchestration.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Maximum transcript bytes paged back per `transcript` call.
const TRANSCRIPT_PAGE_BYTES: usize = 32 * 1024;
/// Maximum bytes retained when a transcript is inlined into a revive prompt.
const REVIVE_TRANSCRIPT_BUDGET: usize = 16 * 1024;
/// Maximum queued steering messages retained per child in memory.
const MAX_QUEUE_PER_CHILD: usize = 64;
/// Bound both a single serialized steering frame and the pending disk queue.
const MAX_STEER_FRAME_BYTES: usize = 64 * 1024;
const MAX_STEER_QUEUE_BYTES: u64 = 4 * 1024 * 1024;

/// Lifecycle states for a registered child run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChildStatus {
    Starting,
    Running,
    Done,
    Failed,
    Cancelled,
    /// Operator kill via `hub agent kill` (distinct from parent cancellation).
    Killed,
}

/// Origin of a child run in the parent session.
///
/// Regular tool delegations remain `subagent`; `/tan` uses a distinct kind so
/// roster consumers can identify background tangential work without parsing
/// the child name or task text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChildKind {
    Subagent,
    Tan,
}

impl ChildKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Subagent => "subagent",
            Self::Tan => "tan",
        }
    }
}

impl ChildStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Killed => "killed",
        }
    }

    /// Terminal states: the child process has exited or been reaped.
    #[must_use]
    pub const fn settled(self) -> bool {
        matches!(
            self,
            Self::Done | Self::Failed | Self::Cancelled | Self::Killed
        )
    }
}

/// One registered child run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildEntry {
    /// Unique run id: `<agent>-<seq>` (seq is per-registry monotonic).
    pub id: String,
    /// Agent definition name (e.g. `scout`).
    pub name: String,
    /// Spawn surface (`subagent` tool or `/tan`).
    pub kind: ChildKind,
    /// The task text (truncated for roster display, never used for revival).
    pub task: String,
    pub pid: Option<u32>,
    pub status: ChildStatus,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
    /// Output tokens observed so far (byte-length of accumulated output;
    /// the pipe protocol does not carry token counts — honest proxy).
    pub output_bytes: usize,
    /// Transcript file (JSONL frames emitted by the child).
    pub transcript_path: PathBuf,
    /// Steering queue file consumed by the child between turns.
    pub steer_path: PathBuf,
    /// When revived, the id of the run this one continues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revived_from: Option<String>,
}

/// A peer bus message (child→child, or operator→child) in delivery order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BusMessage {
    pub seq: u64,
    pub from: String,
    pub to: String,
    pub body: String,
    pub sent_ms: u64,
}

/// Session-scoped registry: one per parent process.
#[derive(Default)]
pub struct AgentHubRegistry {
    entries: BTreeMap<String, ChildEntry>,
    /// Complete user assignments, separate from display previews and from
    /// generated continuation prompts. Kept for the lifetime of this hub.
    original_tasks: BTreeMap<String, String>,
    seq: u64,
    /// Per-recipient delivered/queued bus messages (in-memory view; the
    /// on-disk steer files are the cross-process channel).
    bus: BTreeMap<String, VecDeque<BusMessage>>,
    bus_seq: u64,
    /// Artifacts dir for this session's hub files.
    dir: Option<PathBuf>,
}

impl std::fmt::Debug for AgentHubRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Complete assignments and bus bodies must not leak into diagnostics.
        f.debug_struct("AgentHubRegistry")
            .field("children", &self.entries.len())
            .field("original_tasks", &self.original_tasks.len())
            .field("seq", &self.seq)
            .field("bus_seq", &self.bus_seq)
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

static REGISTRY: OnceLock<Mutex<AgentHubRegistry>> = OnceLock::new();

/// The process-wide registry (session-scoped: one hub per parent run).
pub fn registry() -> &'static Mutex<AgentHubRegistry> {
    REGISTRY.get_or_init(|| Mutex::new(AgentHubRegistry::default()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl AgentHubRegistry {
    /// Point the registry's artifacts dir at `dir` (integration-test hook:
    /// keeps hub files out of the real `<global_dir>/agent-hub/` tree).
    #[doc(hidden)]
    pub fn set_dir_for_tests(&mut self, dir: PathBuf) {
        self.dir = Some(dir);
    }

    /// Artifacts directory for hub files: `<global_dir>/agent-hub/<pid>/`.
    /// Created lazily; per-process so concurrent sessions never share files.
    fn dir(&mut self) -> Result<PathBuf> {
        let dir = self.dir.clone().unwrap_or_else(|| {
            crate::config::Config::global_dir()
                .join("agent-hub")
                .join(std::process::id().to_string())
        });
        // Idempotent: also covers pre-set test dirs that were never created.
        fs::create_dir_all(&dir).map_err(|e| {
            Error::tool(
                "hub",
                format!("create agent-hub dir {}: {e}", dir.display()),
            )
        })?;
        self.dir = Some(dir.clone());
        Ok(dir)
    }

    /// Register a child at spawn time. Returns the assigned run id.
    pub fn register(&mut self, name: &str, task: &str) -> Result<ChildEntry> {
        self.register_kind(name, task, ChildKind::Subagent)
    }

    /// Register a child with an explicit spawn kind.
    pub fn register_kind(&mut self, name: &str, task: &str, kind: ChildKind) -> Result<ChildEntry> {
        let seq = self
            .seq
            .checked_add(1)
            .ok_or_else(|| Error::validation("hub: child sequence exhausted"))?;
        let id = format!("{}-{seq}", sanitize_id(name));
        let dir = self.dir()?;
        let entry = ChildEntry {
            id: id.clone(),
            name: name.to_string(),
            kind,
            task: truncate_chars(task, 500),
            pid: None,
            status: ChildStatus::Starting,
            started_ms: now_ms(),
            finished_ms: None,
            output_bytes: 0,
            transcript_path: dir.join(format!("{id}.transcript.jsonl")),
            steer_path: dir.join(format!("{id}.steer")),
            revived_from: None,
        };
        self.seq = seq;
        self.original_tasks.insert(id.clone(), task.to_string());
        self.entries.insert(id, entry.clone());
        Ok(entry)
    }

    /// Mark a newly spawned child running. A late spawn callback cannot
    /// resurrect an already killed or cancelled run.
    pub fn mark_running(&mut self, id: &str, pid: u32) {
        if let Some(entry) = self.entries.get_mut(id)
            && entry.status == ChildStatus::Starting
        {
            entry.pid = Some(pid);
            entry.status = ChildStatus::Running;
        }
    }

    /// Latch the first terminal outcome. A process-reaping callback must not
    /// overwrite an operator kill or a parent cancellation with `Done`.
    pub fn settle(&mut self, id: &str, status: ChildStatus) {
        if !status.settled() {
            return;
        }
        if let Some(entry) = self.entries.get_mut(id)
            && !entry.status.settled()
        {
            entry.status = status;
            entry.finished_ms = Some(now_ms());
        }
    }

    /// Append one raw stdout frame to the child's transcript file and track
    /// output volume. Best-effort: transcript persistence never fails the run.
    pub fn append_transcript(&mut self, id: &str, line: &str) {
        let Some(entry) = self.entries.get_mut(id) else {
            return;
        };
        entry.output_bytes = entry.output_bytes.saturating_add(line.len());
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&entry.transcript_path)
        {
            let _ = writeln!(file, "{line}");
        }
    }

    /// Roster snapshot in registration order.
    #[must_use]
    pub fn roster(&self) -> Vec<ChildEntry> {
        self.entries.values().cloned().collect()
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<ChildEntry> {
        self.entries.get(id).cloned()
    }

    /// Page a transcript tail, redacting known secret shapes through a fresh
    /// detection pass (the session vault lives on the agent, not here, so
    /// hub reads re-detect and emit fresh placeholders).
    pub fn transcript_page(&self, id: &str) -> Result<String> {
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| Error::validation(format!("hub: unknown child '{id}'")))?;
        let tail = read_transcript_tail(&entry.transcript_path, TRANSCRIPT_PAGE_BYTES)?;
        let mut vault = crate::secrets::SecretVault::default();
        let (masked, _audit) = crate::secrets::obfuscate(&tail, &mut vault, &[]);
        Ok(masked)
    }

    /// Queue steering for a live child. Report it in the inbox only after
    /// the cross-process queue has accepted the complete frame.
    /// A busy disk queue returns an error without waiting or recording a
    /// delivery; the caller can retry without duplicating an accepted frame.
    /// An unterminated existing frame must be repaired before appending;
    /// rejecting it preserves both the old bytes and the delivery sequence.
    pub fn steer(&mut self, id: &str, from: &str, body: &str) -> Result<BusMessage> {
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| Error::validation(format!("hub: unknown child '{id}'")))?
            .clone();
        if entry.status.settled() {
            return Err(Error::validation(format!(
                "hub: cannot steer '{}' — status {}",
                id,
                entry.status.as_str()
            )));
        }
        let seq = self
            .bus_seq
            .checked_add(1)
            .ok_or_else(|| Error::validation("hub: steering sequence exhausted"))?;
        let message = BusMessage {
            seq,
            from: from.to_string(),
            to: id.to_string(),
            body: body.to_string(),
            sent_ms: now_ms(),
        };
        append_steer_line(&entry.steer_path, &message)?;
        self.bus_seq = seq;
        let queue = self.bus.entry(id.to_string()).or_default();
        if queue.len() >= MAX_QUEUE_PER_CHILD {
            queue.pop_front();
        }
        queue.push_back(message.clone());
        Ok(message)
    }

    /// Peer bus send: deliver `body` into recipient child's steering channel.
    pub fn bus_send(&mut self, to: &str, from: &str, body: &str) -> Result<BusMessage> {
        self.steer(to, from, body)
    }

    /// Inbox view for one child (parent-side record, in delivery order).
    #[must_use]
    pub fn inbox(&self, id: &str) -> Vec<BusMessage> {
        self.bus
            .get(id)
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Mark a child killed by the operator.
    pub fn mark_killed(&mut self, id: &str) {
        self.settle(id, ChildStatus::Killed);
    }

    /// Register a replacement run with the complete original assignment and
    /// a bounded, redacted transcript tail. Repeated revivals do not nest
    /// previous continuation prompts or silently discard task requirements.
    pub fn revive(&mut self, from_id: &str) -> Result<(ChildEntry, String)> {
        let prior = self
            .entries
            .get(from_id)
            .ok_or_else(|| Error::validation(format!("hub: unknown child '{from_id}'")))?
            .clone();
        if !prior.status.settled() {
            return Err(Error::validation(format!(
                "hub: cannot revive '{from_id}' — still {}",
                prior.status.as_str()
            )));
        }
        let original_task = self.original_tasks.get(from_id).cloned().ok_or_else(|| {
            Error::tool(
                "hub",
                "complete original assignment unavailable; refusing partial revival",
            )
        })?;
        let tail = read_transcript_tail(&prior.transcript_path, REVIVE_TRANSCRIPT_BUDGET)?;
        let mut vault = crate::secrets::SecretVault::default();
        let (tail, _audit) = crate::secrets::obfuscate(&tail, &mut vault, &[]);
        let task = format!(
            "{}\n\n[Continuation of a prior run ({}). Its transcript tail follows; \
             pick up where it left off and finish the task.]\n{}",
            original_task,
            prior.status.as_str(),
            tail
        );
        // Keep the original assignment as this run's revival source, not
        // the generated prompt containing the previous run's transcript.
        let mut entry = self.register_kind(&prior.name, &original_task, prior.kind)?;
        entry.revived_from = Some(from_id.to_string());
        self.entries.insert(entry.id.clone(), entry.clone());
        Ok((entry, task))
    }

    /// Remove this session's hub artifacts directory (session exit).
    pub fn cleanup_session_files() {
        let dir = crate::config::Config::global_dir()
            .join("agent-hub")
            .join(std::process::id().to_string());
        let _ = fs::remove_dir_all(dir);
    }
}

/// The lock inode must never be renamed with the queue: otherwise a writer
/// that already opened the old inode could append after the reader deleted it.
/// Opening read/write also permits native Windows file locking.
fn open_steer_lock(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path.with_extension("steer.lock"))
}

/// Append a complete frame while holding the same stable lock as the reader.
/// A reported write failure does not become an in-memory delivery receipt.
fn append_steer_line(path: &Path, message: &BusMessage) -> Result<()> {
    if message.body.len() > MAX_STEER_FRAME_BYTES {
        return Err(Error::validation("hub: steering message exceeds 64 KiB"));
    }
    let mut line = serde_json::to_vec(message)
        .map_err(|e| Error::validation(format!("serialize bus message: {e}")))?;
    line.push(b'\n');
    if line.len() > MAX_STEER_FRAME_BYTES {
        return Err(Error::validation(
            "hub: serialized steering frame exceeds 64 KiB",
        ));
    }
    let queue_lock = open_steer_lock(path)
        .map_err(|e| Error::tool("hub", format!("open steer lock {}: {e}", path.display())))?;
    // The caller may own the process-wide hub mutex. Waiting for a child
    // here would also stall roster reads, kills, and steering other children.
    match queue_lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(Error::tool(
                "hub",
                "steering queue is busy; message not accepted, retry delivery",
            ));
        }
        Err(std::fs::TryLockError::Error(err)) => {
            return Err(Error::tool(
                "hub",
                format!("lock steer queue {}: {err}", path.display()),
            ));
        }
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| Error::tool("hub", format!("append steer queue {}: {e}", path.display())))?;
    let original_len = file
        .metadata()
        .map_err(|e| Error::tool("hub", format!("stat steer queue {}: {e}", path.display())))?
        .len();
    if original_len > 0 {
        // A process can die between writing a JSON value and its newline.
        // Appending behind that torn record would concatenate the next
        // accepted frame into invalid JSON and poison subsequent delivery.
        let mut last = [0_u8; 1];
        file.seek(SeekFrom::End(-1))
            .and_then(|_| file.read_exact(&mut last))
            .map_err(|e| {
                Error::tool("hub", format!("inspect steer queue {}: {e}", path.display()))
            })?;
        if last[0] != b'\n' {
            return Err(Error::tool(
                "hub",
                "steering queue has an unterminated frame; message not accepted, repair the queue before retrying",
            ));
        }
    }
    if original_len.saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX))
        > MAX_STEER_QUEUE_BYTES
    {
        return Err(Error::tool(
            "hub",
            "steering queue is full; retry after the child consumes pending messages",
        ));
    }
    if let Err(err) = file.write_all(&line).and_then(|()| file.sync_data()) {
        // No reader or cooperating writer can observe the partial append
        // before this rollback. Keep the earlier accepted frames intact.
        let rollback = file.set_len(original_len).and_then(|()| file.sync_data());
        return Err(Error::tool(
            "hub",
            match rollback {
                Ok(()) => format!("write steer queue {}: {err}", path.display()),
                Err(rollback_err) => format!(
                    "write steer queue {}: {err}; rollback failed: {rollback_err}",
                    path.display()
                ),
            },
        ));
    }
    Ok(())
}

fn read_steer_batch(path: &Path) -> Result<Vec<String>> {
    let file = fs::File::open(path).map_err(|e| {
        Error::tool(
            "hub",
            format!("open draining queue {}: {e}", path.display()),
        )
    })?;
    let mut raw = String::new();
    file.take(MAX_STEER_QUEUE_BYTES + 1)
        .read_to_string(&mut raw)
        .map_err(|e| {
            Error::tool(
                "hub",
                format!("read draining queue {}: {e}", path.display()),
            )
        })?;
    if u64::try_from(raw.len()).unwrap_or(u64::MAX) > MAX_STEER_QUEUE_BYTES {
        return Err(Error::tool(
            "hub",
            "draining steering batch exceeds the queue limit",
        ));
    }
    // Newline termination is part of the writer's frame contract, including
    // during recovery. Valid JSON without its terminator is still torn.
    if !raw.is_empty() && !raw.ends_with('\n') {
        return Err(Error::tool(
            "hub",
            "unterminated steering frame; batch retained",
        ));
    }
    // Validate every wire frame, including its newline, before acknowledging
    // any messages. Malformed or oversized frames cannot discard neighbors.
    let mut messages = Vec::new();
    for (index, frame) in raw.split_inclusive('\n').enumerate() {
        if frame.len() > MAX_STEER_FRAME_BYTES {
            return Err(Error::tool(
                "hub",
                format!("steering frame at line {} exceeds 64 KiB; batch retained", index + 1),
            ));
        }
        if frame.trim().is_empty() {
            continue;
        }
        let message: BusMessage = serde_json::from_str(frame).map_err(|_| {
            Error::tool(
                "hub",
                format!("invalid steering frame at line {}; batch retained", index + 1),
            )
        })?;
        messages.push(format!("[hub:{}] {}", message.from, message.body));
    }
    Ok(messages)
}

/// Result of one non-blocking disk-queue poll.
///
/// Only `Empty` establishes that no messages were pending while the lock
/// was held. `Busy` says nothing about queue contents and must be retried.
#[derive(Debug, PartialEq, Eq)]
pub enum SteerDrain {
    Empty,
    Busy,
    /// One nonempty, acknowledged disk batch in delivery order.
    Messages(Vec<String>),
}

/// Poll the child-side queue without hiding contention or I/O failures.
///
/// Recovery always precedes newer messages. Failed batches stay on disk;
/// no messages are returned until the entire batch has been validated and
/// consumed. This acknowledges disk consumption, not model processing: a
/// process crash after return still needs a higher-level acknowledgment.
pub fn poll_steer_file(path: &Path) -> Result<SteerDrain> {
    let queue_lock = open_steer_lock(path)
        .map_err(|e| Error::tool("hub", format!("open steer lock {}: {e}", path.display())))?;
    match queue_lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(SteerDrain::Busy),
        Err(std::fs::TryLockError::Error(err)) => {
            return Err(Error::tool(
                "hub",
                format!("lock steer queue {}: {err}", path.display()),
            ));
        }
    }
    let draining = path.with_extension("draining");
    // There can be at most two batches under this lock: an interrupted
    // drain and the active queue. Skip an empty recovered batch rather than
    // reporting Empty while a newer accepted message is still pending.
    for _ in 0..2 {
        let recovering = draining.try_exists().map_err(|e| {
            Error::tool(
                "hub",
                format!("stat draining queue {}: {e}", draining.display()),
            )
        })?;
        if !recovering {
            match fs::rename(path, &draining) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(SteerDrain::Empty);
                }
                Err(err) => {
                    return Err(Error::tool(
                        "hub",
                        format!("claim steer queue {}: {err}", path.display()),
                    ));
                }
            }
        }
        // Recover an old batch before touching the next queue file. Never
        // overwrite .draining: it may contain accepted, undelivered messages.
        let messages = read_steer_batch(&draining)?;
        fs::remove_file(&draining).map_err(|e| {
            Error::tool(
                "hub",
                format!("consume draining queue {}: {e}", draining.display()),
            )
        })?;
        if !messages.is_empty() {
            return Ok(SteerDrain::Messages(messages));
        }
    }
    Ok(SteerDrain::Empty)
}

/// Adapt disk polling to the agent's [`crate::agent::MessageFetcher`].
///
/// This callback can only return messages, so deferral and failures are
/// logged and left for the next poll. Call [`poll_steer_file`] when the
/// caller needs to distinguish an empty queue from contention or failure.
pub fn drain_steer_file(path: &Path) -> Vec<String> {
    match poll_steer_file(path) {
        Ok(SteerDrain::Messages(messages)) => messages,
        Ok(SteerDrain::Empty) => Vec::new(),
        Ok(SteerDrain::Busy) => {
            tracing::debug!("steering queue busy; retrying later");
            Vec::new()
        }
        Err(err) => {
            tracing::warn!(error = %err, "steering poll failed; pending batches retained for retry");
            Vec::new()
        }
    }
}

/// Read only the requested suffix, even if the child has produced gigabytes
/// of output. Snapshot the end offset so concurrent appends cannot expand the
/// allocation. A missing transcript is normal before the first output frame;
/// other I/O failures must not masquerade as an empty continuation context.
fn read_transcript_tail(path: &Path, max: usize) -> Result<String> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(err) => {
            return Err(Error::tool(
                "hub",
                format!("open transcript {}: {err}", path.display()),
            ));
        }
    };
    let end = file
        .metadata()
        .map_err(|e| Error::tool("hub", format!("stat transcript {}: {e}", path.display())))?
        .len();
    let start = end.saturating_sub(u64::try_from(max).unwrap_or(u64::MAX));
    file.seek(SeekFrom::Start(start))
        .map_err(|e| Error::tool("hub", format!("seek transcript {}: {e}", path.display())))?;
    let mut bytes = Vec::new();
    file.take(end.saturating_sub(start))
        .read_to_end(&mut bytes)
        .map_err(|e| Error::tool("hub", format!("read transcript {}: {e}", path.display())))?;
    // A bounded seek may land inside a UTF-8 codepoint. Discard only its
    // leading continuation bytes; tolerate an in-flight partial final frame.
    let mut first = 0;
    if start > 0 {
        while first < bytes.len() && bytes[first] & 0xc0 == 0x80 {
            first += 1;
        }
    }
    let text = String::from_utf8_lossy(&bytes[first..]);
    Ok(tail_bytes(&text, max))
}

fn sanitize_id(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

fn tail_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    // Transcripts carry raw child output (emoji/CJK are common); a byte
    // offset inside a multibyte char would panic on slicing. Round forward
    // to the next char boundary before searching for the line break.
    let mut start = text.len() - max;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    let boundary = text[start..].find('\n').map_or(start, |i| start + i + 1);
    text[boundary.min(text.len())..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_registry() -> AgentHubRegistry {
        AgentHubRegistry::default()
    }

    /// Retry only real lock contention. Never hide an I/O/protocol error or
    /// an unexpectedly empty queue behind a blanket retry loop.
    fn poll_until_uncontended(path: &Path) -> Result<SteerDrain> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match poll_steer_file(path) {
                Ok(SteerDrain::Busy) => {
                    assert!(std::time::Instant::now() < deadline, "queue remained busy");
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                ready => return ready,
            }
        }
    }

    fn poll_until_ready(path: &Path) -> SteerDrain {
        poll_until_uncontended(path).expect("steering poll")
    }

    #[test]
    fn revival_preserves_full_assignment_without_nesting_previous_prompts() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let original = format!(
            "{}FINAL REQUIREMENT: preserve all data",
            "step; ".repeat(200)
        );
        let child = reg.register("worker", &original).expect("register");
        assert_eq!(child.task.chars().count(), 500, "roster stays bounded");
        reg.append_transcript(&child.id, "first progress");
        reg.settle(&child.id, ChildStatus::Failed);
        let (next, prompt) = reg.revive(&child.id).expect("revive");
        assert!(prompt.starts_with(&original));
        assert!(prompt.contains("first progress"));
        assert_eq!(next.revived_from.as_deref(), Some(child.id.as_str()));
        reg.append_transcript(&next.id, "second progress");
        reg.settle(&next.id, ChildStatus::Failed);
        let (_, prompt) = reg.revive(&next.id).expect("revive again");
        assert!(prompt.starts_with(&original));
        assert!(prompt.contains("second progress"));
        assert!(!prompt.contains("first progress"));
        assert_eq!(prompt.matches("Continuation of a prior run").count(), 1);
        assert!(!format!("{reg:?}").contains("FINAL REQUIREMENT"));
    }

    #[test]
    fn revival_refuses_unreadable_transcript_without_registering_a_child() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg
            .register("worker", "complete assignment")
            .expect("register");
        fs::create_dir(&child.transcript_path).expect("unreadable transcript fixture");
        reg.settle(&child.id, ChildStatus::Failed);
        assert!(reg.revive(&child.id).is_err());
        assert_eq!(reg.roster().len(), 1);
    }

    #[test]
    fn revival_redacts_transcript_credentials() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg.register("worker", "finish the work").expect("register");
        reg.append_transcript(
            &child.id,
            "key is sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        );
        reg.settle(&child.id, ChildStatus::Failed);
        let (_, prompt) = reg.revive(&child.id).expect("revive");
        assert!(!prompt.contains("sk-ant-api03-AAAA"));
        assert!(prompt.contains("<pi-secret:"));
    }

    #[test]
    fn transcript_tail_does_not_read_the_unbounded_prefix() {
        let temp = tempfile::tempdir().expect("hub directory");
        let path = temp.path().join("large.transcript.jsonl");
        let mut file = fs::File::create(&path).expect("transcript");
        file.write_all(&[0xff])
            .expect("invalid UTF-8 in old prefix");
        file.set_len(16 * 1024 * 1024).expect("sparse transcript");
        file.seek(SeekFrom::End(0)).expect("end");
        file.write_all(b"latest progress\n").expect("tail");
        drop(file);
        let tail = read_transcript_tail(&path, 64).expect("bounded suffix");
        assert!(tail.len() <= 64);
        assert!(tail.ends_with("latest progress\n"));
        assert!(!tail.contains('\u{fffd}'));
    }

    #[test]
    fn transcript_tail_rounds_past_split_utf8_prefix() {
        let temp = tempfile::tempdir().expect("hub directory");
        let path = temp.path().join("unicode.transcript.jsonl");
        let text = format!("{}done", "é".repeat(100));
        fs::write(&path, &text).expect("transcript");
        for max in 0..=33 {
            let tail = read_transcript_tail(&path, max).expect("tail");
            assert!(tail.len() <= max);
            assert!(text.ends_with(&tail));
            assert!(!tail.contains('\u{fffd}'));
        }
    }

    #[test]
    fn late_callbacks_cannot_resurrect_or_overwrite_terminal_children() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg.register("worker", "task").expect("register");
        reg.mark_killed(&child.id);
        let killed = reg.get(&child.id).expect("killed entry");
        reg.mark_running(&child.id, 99);
        reg.settle(&child.id, ChildStatus::Done);
        let final_entry = reg.get(&child.id).expect("terminal entry");
        assert_eq!(final_entry.status, ChildStatus::Killed);
        assert_eq!(final_entry.finished_ms, killed.finished_ms);
        assert_eq!(final_entry.pid, killed.pid);
    }

    #[test]
    fn nonterminal_settlement_does_not_finish_a_child() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg.register("worker", "task").expect("register");
        reg.settle(&child.id, ChildStatus::Running);
        let entry = reg.get(&child.id).expect("entry");
        assert_eq!(entry.status, ChildStatus::Starting);
        assert!(entry.finished_ms.is_none());
    }

    #[test]
    fn failed_steer_is_not_reported_in_the_inbox() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg.register("worker", "task").expect("register");
        fs::create_dir(&child.steer_path).expect("block queue file creation");
        assert!(reg.steer(&child.id, "parent", "not delivered").is_err());
        assert!(reg.inbox(&child.id).is_empty());
        assert_eq!(reg.bus_seq, 0);
    }

    #[test]
    fn busy_steer_does_not_block_hub_control_or_record_delivery() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg.register("worker", "task").expect("register");
        let other = reg.register("other", "task").expect("register other");
        reg.steer(&child.id, "parent", "earlier").expect("first send");
        let original = fs::read(&child.steer_path).expect("original queue");
        let lock = open_steer_lock(&child.steer_path).expect("lock file");
        lock.lock().expect("reader lock");
        let child_id = child.id.clone();
        let other_id = other.id.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let blocked = reg
                .bus_send(&child_id, "parent", "not accepted")
                .map(|message| message.seq)
                .map_err(|err| err.to_string());
            let accepted = reg
                .steer(&other_id, "parent", "independent")
                .map(|message| message.seq)
                .map_err(|err| err.to_string());
            reg.mark_killed(&other_id);
            tx.send((blocked, accepted))
                .expect("report completed operations");
            reg
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(10));
        // Release before joining even on timeout, so restoring a blocking
        // writer makes this test fail instead of hanging the entire suite.
        drop(lock);
        let mut reg = worker.join().expect("hub worker");
        let (blocked, accepted) = result.expect("hub must not wait for the queue lock");
        assert!(
            blocked
                .expect_err("busy send must fail")
                .contains("not accepted")
        );
        assert_eq!(accepted.expect("unrelated child remains steerable"), 2);
        assert_eq!(
            reg.get(&other.id).expect("other child").status,
            ChildStatus::Killed
        );
        assert_eq!(reg.bus_seq, 2);
        assert_eq!(reg.inbox(&child.id).len(), 1);
        assert_eq!(fs::read(&child.steer_path).expect("untouched queue"), original);
        assert_eq!(
            reg.steer(&child.id, "parent", "retry").expect("retry").seq,
            3
        );
        assert_eq!(
            poll_until_ready(&child.steer_path),
            SteerDrain::Messages(vec![
                "[hub:parent] earlier".to_string(),
                "[hub:parent] retry".to_string()
            ])
        );
    }

    #[test]
    fn poll_reports_empty_only_after_inspecting_the_queue() {
        let temp = tempfile::tempdir().expect("hub directory");
        let path = temp.path().join("worker.steer");
        assert_eq!(poll_until_ready(&path), SteerDrain::Empty);
        assert!(!path.exists());
        assert!(!path.with_extension("draining").exists());
    }

    #[test]
    fn poll_reports_lock_open_failure_without_claiming_messages() {
        let temp = tempfile::tempdir().expect("hub directory");
        let path = temp.path().join("worker.steer");
        let original = b"pending bytes\n";
        fs::write(&path, original).expect("pending queue");
        fs::create_dir(path.with_extension("steer.lock")).expect("block lock open");
        let err = poll_steer_file(&path).expect_err("I/O failure is not empty or busy");
        assert!(err.to_string().contains("open steer lock"));
        assert_eq!(fs::read(&path).expect("pending queue retained"), original.to_vec());
        assert!(!path.with_extension("draining").exists());
    }

    #[test]
    fn poll_reports_recovery_read_failure_without_touching_newer_messages() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg.register("worker", "task").expect("register");
        reg.steer(&child.id, "parent", "newer").expect("send");
        let original = fs::read(&child.steer_path).expect("queued frame");
        let draining = child.steer_path.with_extension("draining");
        fs::create_dir(&draining).expect("unreadable recovery batch");
        assert!(poll_until_uncontended(&child.steer_path).is_err());
        assert!(draining.is_dir());
        assert_eq!(
            fs::read(&child.steer_path).expect("newer frame retained"),
            original
        );
    }

    #[test]
    fn empty_recovery_does_not_hide_newer_pending_messages() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg.register("worker", "task").expect("register");
        let draining = child.steer_path.with_extension("draining");
        fs::write(&draining, "\n \n").expect("empty interrupted drain");
        reg.steer(&child.id, "parent", "pending").expect("send");
        assert_eq!(
            poll_until_ready(&child.steer_path),
            SteerDrain::Messages(vec!["[hub:parent] pending".to_string()])
        );
        assert_eq!(poll_until_ready(&child.steer_path), SteerDrain::Empty);
    }

    #[test]
    fn drain_defers_while_a_writer_owns_the_queue_lock() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg.register("worker", "task").expect("register");
        reg.steer(&child.id, "parent", "accepted").expect("send");
        let lock = open_steer_lock(&child.steer_path).expect("lock file");
        lock.lock().expect("writer lock");
        assert_eq!(
            poll_steer_file(&child.steer_path).expect("poll locked queue"),
            SteerDrain::Busy
        );
        assert!(child.steer_path.exists());
        drop(lock);
        assert_eq!(
            poll_until_ready(&child.steer_path),
            SteerDrain::Messages(vec!["[hub:parent] accepted".to_string()])
        );
        assert_eq!(poll_until_ready(&child.steer_path), SteerDrain::Empty);
    }

    #[test]
    fn interrupted_drain_is_recovered_before_newer_messages() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg.register("worker", "task").expect("register");
        reg.steer(&child.id, "parent", "first").expect("send first");
        let draining = child.steer_path.with_extension("draining");
        fs::rename(&child.steer_path, &draining).expect("simulate interrupted drain");
        reg.steer(&child.id, "parent", "second")
            .expect("send second");
        assert_eq!(
            poll_until_ready(&child.steer_path),
            SteerDrain::Messages(vec!["[hub:parent] first".to_string()])
        );
        assert!(child.steer_path.exists());
        assert_eq!(
            poll_until_ready(&child.steer_path),
            SteerDrain::Messages(vec!["[hub:parent] second".to_string()])
        );
        assert_eq!(poll_until_ready(&child.steer_path), SteerDrain::Empty);
    }

    #[test]
    fn malformed_batch_is_retained_without_losing_valid_frames() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg.register("worker", "task").expect("register");
        reg.steer(&child.id, "parent", "valid").expect("send");
        let original = fs::read(&child.steer_path).expect("queued bytes");
        let mut file = OpenOptions::new()
            .append(true)
            .open(&child.steer_path)
            .expect("fixture");
        file.write_all(b"{broken\n").expect("partial frame");
        drop(file);
        let err = poll_until_uncontended(&child.steer_path)
            .expect_err("malformed batch is not empty");
        assert!(err.to_string().contains("invalid steering frame at line 2"));
        let draining = child.steer_path.with_extension("draining");
        assert!(draining.exists(), "failed batch must remain recoverable");
        assert!(
            fs::read(&draining)
                .expect("retained batch")
                .starts_with(&original)
        );
        fs::write(&draining, original).expect("repair fixture");
        assert_eq!(
            poll_until_ready(&child.steer_path),
            SteerDrain::Messages(vec!["[hub:parent] valid".to_string()])
        );
        assert_eq!(poll_until_ready(&child.steer_path), SteerDrain::Empty);
    }

    #[test]
    fn oversized_steer_is_rejected_before_recording_delivery() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg.register("worker", "task").expect("register");
        let body = "x".repeat(MAX_STEER_FRAME_BYTES + 1);
        assert!(reg.steer(&child.id, "parent", &body).is_err());
        assert!(reg.inbox(&child.id).is_empty());
        assert!(!child.steer_path.exists());
        reg.steer(&child.id, "parent", "small").expect("later send");
        assert_eq!(reg.inbox(&child.id)[0].seq, 1);
    }

    #[test]
    fn writer_rejects_torn_tail_without_poisoning_the_next_frame() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg.register("worker", "task").expect("register");
        reg.steer(&child.id, "parent", "earlier").expect("first send");
        let original = fs::read(&child.steer_path).expect("first frame");
        let torn = &original[..original.len() - 1];
        fs::write(&child.steer_path, torn).expect("simulate missing final newline");
        let err = reg
            .steer(&child.id, "parent", "must not concatenate")
            .expect_err("torn tail must reject new delivery");
        assert!(err.to_string().contains("unterminated frame"));
        assert_eq!(reg.bus_seq, 1);
        assert_eq!(reg.inbox(&child.id).len(), 1);
        assert_eq!(fs::read(&child.steer_path).expect("torn bytes retained"), torn.to_vec());
        fs::write(&child.steer_path, original).expect("repair terminator");
        assert_eq!(reg.steer(&child.id, "parent", "retry").expect("retry").seq, 2);
        assert_eq!(
            poll_until_ready(&child.steer_path),
            SteerDrain::Messages(vec![
                "[hub:parent] earlier".to_string(),
                "[hub:parent] retry".to_string()
            ])
        );
    }

    #[test]
    fn unterminated_valid_json_is_retained_with_its_valid_neighbors() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg.register("worker", "task").expect("register");
        reg.steer(&child.id, "parent", "first").expect("first send");
        reg.steer(&child.id, "parent", "second").expect("second send");
        let mut torn = fs::read(&child.steer_path).expect("queued frames");
        assert_eq!(torn.pop(), Some(b'\n'));
        fs::write(&child.steer_path, &torn).expect("simulate torn terminator");
        let err = poll_until_uncontended(&child.steer_path)
            .expect_err("valid JSON is not a complete wire frame");
        assert!(err.to_string().contains("unterminated steering frame"));
        let draining = child.steer_path.with_extension("draining");
        assert_eq!(fs::read(&draining).expect("entire batch retained"), torn);
        torn.push(b'\n');
        fs::write(&draining, torn).expect("repair complete batch");
        assert_eq!(
            poll_until_ready(&child.steer_path),
            SteerDrain::Messages(vec![
                "[hub:parent] first".to_string(),
                "[hub:parent] second".to_string()
            ])
        );
        assert_eq!(poll_until_ready(&child.steer_path), SteerDrain::Empty);
    }

    #[test]
    fn recovered_frames_enforce_the_exact_writer_wire_limit() {
        for wire_len in [
            MAX_STEER_FRAME_BYTES - 1,
            MAX_STEER_FRAME_BYTES,
            MAX_STEER_FRAME_BYTES + 1,
        ] {
            let temp = tempfile::tempdir().expect("hub directory");
            let mut reg = fresh_registry();
            reg.set_dir_for_tests(temp.path().to_path_buf());
            let child = reg.register("worker", "task").expect("register");
            let mut message = reg.steer(&child.id, "parent", "neighbor").expect("send");
            let mut batch = fs::read(&child.steer_path).expect("valid prefix");
            message.seq += 1;
            message.body.clear();
            let empty_len = serde_json::to_vec(&message).expect("empty frame").len() + 1;
            message.body = "x".repeat(wire_len - empty_len);
            let mut frame = serde_json::to_vec(&message).expect("sized frame");
            frame.push(b'\n');
            assert_eq!(frame.len(), wire_len);
            batch.extend_from_slice(&frame);
            fs::write(&child.steer_path, &batch).expect("batch fixture");
            let draining = child.steer_path.with_extension("draining");
            fs::rename(&child.steer_path, &draining).expect("interrupted drain");
            if wire_len > MAX_STEER_FRAME_BYTES {
                let err = poll_until_uncontended(&child.steer_path)
                    .expect_err("oversized recovered frame must fail");
                assert!(err.to_string().contains("line 2 exceeds 64 KiB"));
                assert_eq!(fs::read(&draining).expect("all frames retained"), batch);
            } else {
                assert_eq!(
                    poll_until_ready(&child.steer_path),
                    SteerDrain::Messages(vec![
                        "[hub:parent] neighbor".to_string(),
                        format!("[hub:parent] {}", message.body)
                    ])
                );
                assert_eq!(poll_until_ready(&child.steer_path), SteerDrain::Empty);
            }
        }
    }

    #[test]
    fn oversized_blank_frame_is_not_mistaken_for_an_empty_batch() {
        let temp = tempfile::tempdir().expect("hub directory");
        let path = temp.path().join("worker.steer");
        let mut frame = vec![b' '; MAX_STEER_FRAME_BYTES];
        frame.push(b'\n');
        fs::write(&path, &frame).expect("oversized blank frame");
        let err = poll_until_uncontended(&path).expect_err("check size before skipping blanks");
        assert!(err.to_string().contains("line 1 exceeds 64 KiB"));
        assert_eq!(fs::read(path.with_extension("draining")).expect("retained frame"), frame);
    }

    #[test]
    fn escaped_body_must_fit_the_serialized_frame_budget() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg.register("worker", "task").expect("register");
        let body = "\"".repeat(MAX_STEER_FRAME_BYTES / 2);
        assert!(body.len() < MAX_STEER_FRAME_BYTES);
        let err = reg.steer(&child.id, "parent", &body).expect_err("escaped wire size");
        assert!(err.to_string().contains("serialized steering frame exceeds"));
        assert_eq!(reg.bus_seq, 0);
        assert!(reg.inbox(&child.id).is_empty());
        assert!(!child.steer_path.exists());
        assert_eq!(reg.steer(&child.id, "parent", "small").expect("later send").seq, 1);
    }

    #[test]
    fn tail_bytes_never_splits_multibyte_chars() {
        // "é" is 2 bytes; a cut landing inside it must not panic.
        let text = "aé\nbé\ncé";
        for max in 0..=text.len() {
            let tail = tail_bytes(text, max);
            assert!(text.ends_with(&tail), "max={max} tail={tail:?}");
        }
        let emoji = "🙂".repeat(50);
        for max in [1, 2, 3, 5, 7, 33] {
            let _ = tail_bytes(&emoji, max);
        }
    }

    #[test]
    fn registry_fsm_running_to_done() {
        let mut reg = fresh_registry();
        // dir() requires a writable global dir; point it at a temp dir.
        let temp = std::env::temp_dir().join(format!("pi-agent-hub-test-{}", std::process::id()));
        reg.dir = Some(temp.clone());
        let entry = reg.register("scout", "inspect the code").expect("register");
        assert_eq!(entry.status, ChildStatus::Starting);
        assert_eq!(entry.kind, ChildKind::Subagent);
        reg.mark_running(&entry.id, 4242);
        assert_eq!(
            reg.get(&entry.id).expect("get").status,
            ChildStatus::Running
        );
        reg.settle(&entry.id, ChildStatus::Done);
        let settled = reg.get(&entry.id).expect("get");
        assert_eq!(settled.status, ChildStatus::Done);
        assert!(settled.finished_ms.is_some());
        let tan = reg
            .register_kind("tan", "update the changelog", ChildKind::Tan)
            .expect("register tan");
        let roster = reg.roster();
        let tan_roster = roster
            .iter()
            .find(|candidate| candidate.id == tan.id)
            .expect("tan appears in roster");
        assert_eq!(tan_roster.kind, ChildKind::Tan);
        assert_eq!(tan_roster.kind.as_str(), "tan");
        let serialized = serde_json::to_value(tan_roster).expect("serialize roster entry");
        assert_eq!(serialized["kind"], "tan");
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn steer_refuses_settled_child() {
        let mut reg = fresh_registry();
        let temp = std::env::temp_dir().join(format!("pi-agent-hub-test2-{}", std::process::id()));
        reg.dir = Some(temp.clone());
        let entry = reg.register("scout", "task").expect("register");
        reg.settle(&entry.id, ChildStatus::Failed);
        let err = reg.steer(&entry.id, "parent", "hello").unwrap_err();
        assert!(err.to_string().contains("cannot steer"));
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn bus_preserves_delivery_order() {
        let mut reg = fresh_registry();
        let temp = std::env::temp_dir().join(format!("pi-agent-hub-test3-{}", std::process::id()));
        reg.dir = Some(temp.clone());
        let entry = reg.register("worker", "task").expect("register");
        reg.mark_running(&entry.id, 1);
        reg.bus_send(&entry.id, "a", "first").expect("send 1");
        reg.bus_send(&entry.id, "b", "second").expect("send 2");
        let inbox = reg.inbox(&entry.id);
        assert_eq!(inbox.len(), 2);
        assert_eq!(inbox[0].body, "first");
        assert_eq!(inbox[1].body, "second");
        assert!(inbox[0].seq < inbox[1].seq);
        // The on-disk queue mirrors the bus in the same order.
        let drained = poll_until_ready(&entry.steer_path);
        assert_eq!(
            drained,
            SteerDrain::Messages(vec![
                "[hub:a] first".to_string(),
                "[hub:b] second".to_string()
            ])
        );
        // Drain is consume-once.
        assert_eq!(poll_until_ready(&entry.steer_path), SteerDrain::Empty);
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn transcript_page_redacts_secret_shapes() {
        let mut reg = fresh_registry();
        let temp = std::env::temp_dir().join(format!("pi-agent-hub-test4-{}", std::process::id()));
        reg.dir = Some(temp.clone());
        let entry = reg.register("scout", "task").expect("register");
        reg.append_transcript(&entry.id, "{\"note\":\"key is sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"}");
        let page = reg.transcript_page(&entry.id).expect("page");
        assert!(
            !page.contains("sk-ant-api03-AAAA"),
            "raw secret leaked: {page}"
        );
        assert!(page.contains("<pi-secret:"));
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn revive_carries_transcript_context() {
        let mut reg = fresh_registry();
        let temp = std::env::temp_dir().join(format!("pi-agent-hub-test5-{}", std::process::id()));
        reg.dir = Some(temp.clone());
        let entry = reg.register("scout", "original task").expect("register");
        reg.append_transcript(
            &entry.id,
            "{\"type\":\"message_end\",\"text\":\"half-done\"}",
        );
        reg.settle(&entry.id, ChildStatus::Failed);
        let (revived, task) = reg.revive(&entry.id).expect("revive");
        assert_eq!(revived.revived_from.as_deref(), Some(entry.id.as_str()));
        assert!(task.contains("original task"));
        assert!(task.contains("half-done"));
        assert!(task.contains("Continuation"));
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn revive_refuses_running_child() {
        let mut reg = fresh_registry();
        let temp = std::env::temp_dir().join(format!("pi-agent-hub-test6-{}", std::process::id()));
        reg.dir = Some(temp.clone());
        let entry = reg.register("scout", "task").expect("register");
        reg.mark_running(&entry.id, 9);
        let err = reg.revive(&entry.id).unwrap_err();
        assert!(err.to_string().contains("still running"));
        let _ = fs::remove_dir_all(&temp);
    }
}
