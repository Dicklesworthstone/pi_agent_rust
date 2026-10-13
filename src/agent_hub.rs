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
//! Native children use an authenticated, process-owned loopback channel to read
//! the parent's roster and their own inbox, and to send to peers or the parent.
//! The parent reads its inbox explicitly; messages never start unsolicited turns.
//!
//! NTM layer distinction: this hub manages pi's OWN spawned children in this
//! process. It does not rebuild ntm's cross-tmux fleet orchestration.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub mod ipc;

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
    /// Process-control authority ends at reap, even when an accepted result
    /// still awaits ordered worktree application. Entry PIDs remain history.
    live_processes: BTreeMap<String, u32>,
    /// Complete user assignments, separate from display previews and from
    /// generated continuation prompts. Kept for the lifetime of this hub.
    original_tasks: BTreeMap<String, String>,
    /// Only the native executor can install a replayable launch policy. A
    /// public roster registration alone must never authorize hub execution.
    native_launches: BTreeMap<String, crate::subagents::RevivalSpec>,
    /// Explicit revivals and corrective retries retain one lineage root so
    /// an active grandchild also fences revival of an older ancestor.
    native_lineages: BTreeMap<String, String>,
    /// A terminal control request can precede process reap and worktree
    /// disposition. Revival waits for the owning lease to finish both.
    active_executions: BTreeSet<String>,
    seq: u64,
    /// Per-recipient delivered/queued bus messages (in-memory view; the
    /// on-disk steer files are the cross-process channel).
    bus: BTreeMap<String, VecDeque<BusMessage>>,
    bus_seq: u64,
    /// Artifacts dir for this session's hub files.
    dir: Option<PathBuf>,
    /// Only an exclusively created namespace belongs to this registry.
    /// Explicit test directories and stale PID directories are never retired.
    owns_dir: bool,
}

pub(crate) struct RevivalSource {
    pub(crate) entry: ChildEntry,
    pub(crate) launch: crate::subagents::RevivalSpec,
    original_task: String,
    lineage_root: String,
}

impl RevivalSource {
    /// Read the bounded transcript after releasing the registry mutex.
    pub(crate) fn continuation_prompt(&self) -> Result<String> {
        revival_prompt(&self.entry, &self.original_task)
    }
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
        self.owns_dir = false;
    }

    /// Allocate one private namespace per registry, not merely per OS PID.
    /// A crashed process can leave queued instructions and transcripts behind;
    /// PID reuse must never turn those files into a new child's input/history.
    fn dir(&mut self) -> Result<PathBuf> {
        self.dir_in(&crate::config::Config::global_dir().join("agent-hub"))
    }

    fn dir_in(&mut self, root: &Path) -> Result<PathBuf> {
        if let Some(dir) = &self.dir {
            if self.owns_dir {
                // Do not silently recreate a vanished namespace or adopt a
                // symlink: older children still reference its original files.
                let metadata = fs::symlink_metadata(dir).map_err(|error| {
                    Error::tool(
                        "hub",
                        format!("inspect hub dir {}: {error}", dir.display()),
                    )
                })?;
                if !metadata.file_type().is_dir() {
                    return Err(Error::tool(
                        "hub",
                        "agent-hub namespace is no longer a directory",
                    ));
                }
            } else {
                // Explicit test overrides remain caller-owned, including
                // fixtures that supply a directory which does not exist yet.
                fs::create_dir_all(dir).map_err(|error| {
                    Error::tool(
                        "hub",
                        format!("create hub test dir {}: {error}", dir.display()),
                    )
                })?;
            }
            return Ok(dir.clone());
        }
        fs::create_dir_all(root).map_err(|error| {
            Error::tool(
                "hub",
                format!("create agent-hub root {}: {error}", root.display()),
            )
        })?;
        for _ in 0..16 {
            let dir = root.join(format!("{}-{}", std::process::id(), uuid::Uuid::new_v4()));
            let mut builder = fs::DirBuilder::new();
            builder.recursive(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            // Exclusive creation, never create_dir_all on the random leaf.
            // Even a collision must not adopt existing queues or recovery files.
            match builder.create(&dir) {
                Ok(()) => {
                    self.dir = Some(dir.clone());
                    self.owns_dir = true;
                    return Ok(dir);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(Error::tool(
                        "hub",
                        format!("create private hub dir {}: {error}", dir.display()),
                    ));
                }
            }
        }
        Err(Error::tool(
            "hub",
            "could not allocate an unused agent-hub namespace",
        ))
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

    pub(crate) fn register_native(
        &mut self,
        name: &str,
        task: &str,
        kind: ChildKind,
        launch: crate::subagents::RevivalSpec,
        revived_from: Option<&str>,
    ) -> Result<ChildEntry> {
        let lineage_root = if let Some(id) = revived_from {
            let prior = self.native_revival_source(id)?;
            if prior.entry.name != name || prior.entry.kind != kind {
                return Err(Error::tool(
                    "hub",
                    "PI_HUB_REVIVAL_POLICY: replacement does not match its native source",
                ));
            }
            Some(prior.lineage_root)
        } else {
            None
        };
        let mut entry = self.register_kind(name, task, kind)?;
        entry.revived_from = revived_from.map(str::to_string);
        self.entries.insert(entry.id.clone(), entry.clone());
        self.native_launches.insert(entry.id.clone(), launch);
        self.native_lineages.insert(
            entry.id.clone(),
            lineage_root.unwrap_or_else(|| entry.id.clone()),
        );
        self.active_executions.insert(entry.id.clone());
        Ok(entry)
    }

    pub(crate) fn finish_native_execution(&mut self, id: &str) {
        self.active_executions.remove(id);
    }

    /// Snapshot only execution-owned launch metadata. This performs no
    /// transcript I/O and registers nothing, so a refused preparation leaves
    /// no Starting entry behind.
    pub(crate) fn native_revival_source(&self, id: &str) -> Result<RevivalSource> {
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| Error::validation(format!("hub: unknown child '{id}'")))?;
        if !entry.status.settled() || self.active_executions.contains(id) {
            return Err(Error::validation(format!(
                "hub: cannot revive '{id}' — execution or cleanup is still active"
            )));
        }
        let lineage_root = self.native_lineages.get(id).ok_or_else(|| {
            Error::tool(
                "hub",
                "PI_HUB_REVIVAL_UNAVAILABLE: original native launch policy is unavailable",
            )
        })?;
        if self
            .active_executions
            .iter()
            .any(|active| self.native_lineages.get(active) == Some(lineage_root))
        {
            return Err(Error::validation(format!(
                "hub: cannot revive '{id}' — a replacement is already active"
            )));
        }
        let launch = self.native_launches.get(id).cloned().ok_or_else(|| {
            Error::tool(
                "hub",
                "PI_HUB_REVIVAL_UNAVAILABLE: original native launch policy is unavailable",
            )
        })?;
        let original_task = self.original_tasks.get(id).cloned().ok_or_else(|| {
            Error::tool(
                "hub",
                "PI_HUB_REVIVAL_UNAVAILABLE: complete original assignment is unavailable",
            )
        })?;
        Ok(RevivalSource {
            entry: entry.clone(),
            launch,
            original_task,
            lineage_root: lineage_root.clone(),
        })
    }

    /// Mark a newly spawned child running. A late spawn callback cannot
    /// resurrect an already killed or cancelled run.
    pub fn mark_running(&mut self, id: &str, pid: u32) {
        if let Some(entry) = self.entries.get_mut(id)
            && entry.status == ChildStatus::Starting
        {
            entry.pid = Some(pid);
            entry.status = ChildStatus::Running;
            self.live_processes.insert(id.to_string(), pid);
        }
    }

    /// A historical PID must never authorize signalling a reused OS process.
    /// A reaped child can still be killed logically while writeback is pending.
    #[must_use]
    pub(crate) fn control_pid(&self, id: &str) -> Option<u32> {
        self.live_processes.get(id).copied()
    }

    pub(crate) fn mark_process_reaped(&mut self, id: &str) {
        self.live_processes.remove(id);
    }

    /// Latch the first terminal outcome. A process-reaping callback must not
    /// overwrite an operator kill or a parent cancellation with `Done`.
    pub fn settle(&mut self, id: &str, status: ChildStatus) {
        if !status.settled() {
            return;
        }
        self.mark_process_reaped(id);
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
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        if let Ok(mut file) = options.open(&entry.transcript_path) {
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

    /// Deliver to a child's steering channel, or retain an explicit parent
    /// inbox message. Parent delivery does not inject a new model turn.
    pub fn bus_send(&mut self, to: &str, from: &str, body: &str) -> Result<BusMessage> {
        if to != "parent" {
            return self.steer(to, from, body);
        }
        let seq = self
            .bus_seq
            .checked_add(1)
            .ok_or_else(|| Error::validation("hub: messaging sequence exhausted"))?;
        let message = BusMessage {
            seq,
            from: from.to_string(),
            to: "parent".to_string(),
            body: body.to_string(),
            sent_ms: now_ms(),
        };
        if body.trim().is_empty() {
            return Err(Error::validation(
                "hub: parent message requires non-empty text",
            ));
        }
        if body.len() > MAX_STEER_FRAME_BYTES
            || serde_json::to_vec(&message)?.len().saturating_add(1) > MAX_STEER_FRAME_BYTES
        {
            return Err(Error::validation("hub: serialized message exceeds 64 KiB"));
        }
        self.bus_seq = seq;
        let queue = self.bus.entry("parent".to_string()).or_default();
        if queue.len() >= MAX_QUEUE_PER_CHILD {
            queue.pop_front();
        }
        queue.push_back(message.clone());
        Ok(message)
    }

    /// Return a bounded retained inbox page. Cursors are exclusive delivery
    /// sequence numbers; reading a page does not consume steering delivery.
    /// oldest_cursor exposes the oldest retained sequence after history rolls.
    pub fn inbox_page(&self, id: &str, cursor: Option<u64>) -> Result<ipc::Reply> {
        if id != "parent" && !self.entries.contains_key(id) {
            return Err(Error::validation(format!("hub: unknown child '{id}'")));
        }
        let cursor = cursor.unwrap_or(0);
        let queue = self.bus.get(id);
        let oldest_cursor = queue
            .and_then(VecDeque::front)
            .map_or(0, |message| message.seq);
        let mut messages = Vec::new();
        let mut bytes = 0_usize;
        let mut next_cursor = cursor;
        let mut has_more = false;
        if let Some(queue) = queue {
            for message in queue.iter().filter(|message| message.seq > cursor) {
                let size = serde_json::to_vec(message)?.len();
                if bytes.saturating_add(size) > ipc::MAX_INBOX_PAGE_BYTES {
                    has_more = true;
                    break;
                }
                bytes += size;
                next_cursor = message.seq;
                messages.push(message.clone());
            }
        }
        Ok(ipc::Reply::Inbox {
            id: id.to_string(),
            messages,
            next_cursor,
            has_more,
            oldest_cursor,
        })
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
        let task = revival_prompt(&prior, &original_task)?;
        // Keep the original assignment as this run's revival source, not
        // the generated prompt containing the previous run's transcript.
        let mut entry = self.register_kind(&prior.name, &original_task, prior.kind)?;
        entry.revived_from = Some(from_id.to_string());
        self.entries.insert(entry.id.clone(), entry.clone());
        Ok((entry, task))
    }

    /// Remove only this registry's retired namespace (session exit).
    /// Busy registries, live children and unfinished native cleanup retain
    /// their files. No PID-derived directory or caller-owned fixture is removed.
    pub fn cleanup_session_files() {
        if let Some(registry) = REGISTRY.get() {
            cleanup_registry_files(registry);
        }
    }

    fn take_retired_dir(&mut self) -> Option<PathBuf> {
        if !self.owns_dir
            || !self.live_processes.is_empty()
            || !self.active_executions.is_empty()
            || self.entries.values().any(|entry| !entry.status.settled())
        {
            return None;
        }
        self.owns_dir = false;
        self.dir.take()
    }
}

fn cleanup_registry_files(registry: &Mutex<AgentHubRegistry>) {
    let Ok(mut hub) = registry.try_lock() else {
        return;
    };
    let Some(dir) = hub.take_retired_dir() else {
        return;
    };
    // A subsequent registration gets a different namespace. Filesystem cleanup
    // must not hold the hub mutex or sweep sibling sessions/recovery artifacts.
    drop(hub);
    match fs::symlink_metadata(&dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            if let Err(error) = fs::remove_dir_all(&dir) {
                tracing::warn!(path = %dir.display(), %error, "could not remove retired hub files");
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        _ => {
            tracing::warn!(path = %dir.display(), "retired hub path changed; leaving it in place");
        }
    }
}

fn revival_prompt(prior: &ChildEntry, original_task: &str) -> Result<String> {
    let tail = read_transcript_tail(&prior.transcript_path, REVIVE_TRANSCRIPT_BUDGET)?;
    let mut vault = crate::secrets::SecretVault::default();
    let (tail, _audit) = crate::secrets::obfuscate(&tail, &mut vault, &[]);
    Ok(format!(
        "{}\n\n[Continuation of a prior run ({}). Its transcript tail follows; \
         pick up where it left off and finish the task.]\n{}",
        original_task,
        prior.status.as_str(),
        tail
    ))
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
                Error::tool(
                    "hub",
                    format!("inspect steer queue {}: {e}", path.display()),
                )
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
                format!(
                    "steering frame at line {} exceeds 64 KiB; batch retained",
                    index + 1
                ),
            ));
        }
        if frame.trim().is_empty() {
            continue;
        }
        let message: BusMessage = serde_json::from_str(frame).map_err(|_| {
            Error::tool(
                "hub",
                format!(
                    "invalid steering frame at line {}; batch retained",
                    index + 1
                ),
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
    fn fresh_namespace_never_adopts_a_stale_pid_queue_or_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join(std::process::id().to_string());
        fs::create_dir(&legacy).unwrap();
        let frame = BusMessage {
            seq: 1,
            from: "old-parent".to_string(),
            to: "worker-1".to_string(),
            body: "instruction from a crashed session".to_string(),
            sent_ms: 0,
        };
        let mut wire = serde_json::to_vec(&frame).unwrap();
        wire.push(b'\n');
        for name in ["worker-1.steer", "worker-1.draining"] {
            fs::write(legacy.join(name), &wire).unwrap();
        }
        let transcript = legacy.join("worker-1.transcript.jsonl");
        fs::write(&transcript, "old session transcript\n").unwrap();

        let mut reg = fresh_registry();
        let dir = reg.dir_in(temp.path()).unwrap();
        let child = reg.register("worker", "new assignment").unwrap();
        assert_eq!(child.id, "worker-1");
        assert_ne!(dir, legacy);
        assert_eq!(child.steer_path.parent(), Some(dir.as_path()));
        assert_eq!(poll_until_ready(&child.steer_path), SteerDrain::Empty);
        assert_eq!(reg.transcript_page(&child.id).unwrap(), "");
        assert!(reg.inbox(&child.id).is_empty());
        for name in ["worker-1.steer", "worker-1.draining"] {
            assert_eq!(fs::read(legacy.join(name)).unwrap(), wire);
        }
        assert_eq!(
            fs::read_to_string(transcript).unwrap(),
            "old session transcript\n"
        );
    }

    #[test]
    fn registries_in_the_same_process_have_independent_disk_delivery_and_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let mut first = fresh_registry();
        let first_dir = first.dir_in(temp.path()).unwrap();
        let a = first.register("worker", "assignment A").unwrap();
        first.steer(&a.id, "parent", "only A").unwrap();
        first.append_transcript(&a.id, "progress A");
        let recovering = a.steer_path.with_extension("draining");
        fs::rename(&a.steer_path, &recovering).unwrap();
        let retained = fs::read(&recovering).unwrap();

        let mut second = fresh_registry();
        let second_dir = second.dir_in(temp.path()).unwrap();
        let b = second.register("worker", "assignment B").unwrap();
        assert_eq!(a.id, b.id);
        assert_ne!(first_dir, second_dir);
        second.steer(&b.id, "parent", "only B").unwrap();
        second.append_transcript(&b.id, "progress B");
        assert_eq!(
            poll_until_ready(&b.steer_path),
            SteerDrain::Messages(vec!["[hub:parent] only B".to_string()])
        );
        assert_eq!(fs::read(&recovering).unwrap(), retained);
        assert_eq!(
            poll_until_ready(&a.steer_path),
            SteerDrain::Messages(vec!["[hub:parent] only A".to_string()])
        );
        assert_eq!(first.transcript_page(&a.id).unwrap(), "progress A\n");
        assert_eq!(second.transcript_page(&b.id).unwrap(), "progress B\n");
    }

    #[test]
    fn namespace_is_stable_for_children_and_failed_creation_is_retryable() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("hub-root");
        fs::write(&root, "do not overwrite").unwrap();
        let mut reg = fresh_registry();
        assert!(reg.dir_in(&root).is_err());
        assert!(reg.dir.is_none());
        assert!(!reg.owns_dir);
        assert_eq!(reg.seq, 0);
        assert!(reg.roster().is_empty());
        assert_eq!(fs::read_to_string(&root).unwrap(), "do not overwrite");
        fs::rename(&root, temp.path().join("retained-file")).unwrap();

        let dir = reg.dir_in(&root).unwrap();
        let first = reg.register("worker", "first").unwrap();
        let second = reg.register("worker", "second").unwrap();
        assert_eq!(first.steer_path.parent(), Some(dir.as_path()));
        assert_eq!(second.steer_path.parent(), Some(dir.as_path()));
        let unrelated = temp.path().join("different-root");
        assert_eq!(reg.dir_in(&unrelated).unwrap(), dir);
        assert!(
            !unrelated.exists(),
            "a cached namespace must not change roots"
        );
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    }

    #[test]
    fn missing_owned_namespace_is_not_recreated_for_later_children() {
        let temp = tempfile::tempdir().unwrap();
        let mut reg = fresh_registry();
        let dir = reg.dir_in(temp.path()).unwrap();
        let child = reg.register("worker", "first").unwrap();
        reg.steer(&child.id, "parent", "retained input").unwrap();
        let wire = fs::read(&child.steer_path).unwrap();
        let moved = temp.path().join("retained-namespace");
        fs::rename(&dir, &moved).unwrap();

        assert!(
            reg.register("worker", "must not adopt a fresh directory")
                .is_err()
        );
        assert_eq!(reg.seq, 1);
        assert_eq!(reg.roster().len(), 1);
        assert!(!dir.exists());
        assert_eq!(
            fs::read(moved.join(child.steer_path.file_name().unwrap())).unwrap(),
            wire
        );
    }

    #[test]
    fn cleanup_waits_for_starting_running_and_native_writeback_owners() {
        let temp = tempfile::tempdir().unwrap();
        let mut reg = fresh_registry();
        let dir = reg.dir_in(temp.path()).unwrap();
        let child = reg.register("worker", "task").unwrap();
        reg.steer(&child.id, "parent", "pending").unwrap();
        let bytes = fs::read(&child.steer_path).unwrap();
        let registry = Mutex::new(reg);
        cleanup_registry_files(&registry);
        assert_eq!(fs::read(&child.steer_path).unwrap(), bytes);

        registry.lock().unwrap().mark_running(&child.id, 4242);
        cleanup_registry_files(&registry);
        assert!(dir.exists());
        {
            let mut hub = registry.lock().unwrap();
            hub.mark_process_reaped(&child.id);
            hub.active_executions.insert(child.id.clone());
        }
        cleanup_registry_files(&registry);
        assert!(dir.exists(), "reap alone is not completed writeback");
        registry.lock().unwrap().mark_killed(&child.id);
        cleanup_registry_files(&registry);
        assert!(
            dir.exists(),
            "terminal status does not release the native lease"
        );
        registry.lock().unwrap().finish_native_execution(&child.id);
        cleanup_registry_files(&registry);
        assert!(!dir.exists());
        assert!(registry.lock().unwrap().dir.is_none());
        cleanup_registry_files(&registry);
        assert!(temp.path().exists());
    }

    #[test]
    fn cleanup_removes_only_its_retired_namespace_and_next_use_gets_a_fresh_one() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join(std::process::id().to_string());
        fs::create_dir(&legacy).unwrap();
        fs::write(legacy.join("recovery"), "retain legacy files").unwrap();
        let mut a = fresh_registry();
        let a_dir = a.dir_in(temp.path()).unwrap();
        let first = a.register("worker", "first").unwrap();
        a.append_transcript(&first.id, "completed work");
        a.settle(&first.id, ChildStatus::Done);
        let mut b = fresh_registry();
        let b_dir = b.dir_in(temp.path()).unwrap();
        let second = b.register("worker", "unrelated live child").unwrap();
        b.steer(&second.id, "parent", "do not remove").unwrap();
        let wire = fs::read(&second.steer_path).unwrap();

        let registry = Mutex::new(a);
        cleanup_registry_files(&registry);
        assert!(!a_dir.exists());
        assert_eq!(fs::read(&second.steer_path).unwrap(), wire);
        assert_eq!(
            fs::read_to_string(legacy.join("recovery")).unwrap(),
            "retain legacy files"
        );
        let next = registry.lock().unwrap().dir_in(temp.path()).unwrap();
        assert_ne!(next, a_dir);
        assert_ne!(next, b_dir);
        assert!(next.exists());
    }

    #[test]
    fn cleanup_does_not_claim_explicit_caller_owned_directories() {
        let temp = tempfile::tempdir().unwrap();
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().join("caller-owned"));
        let child = reg.register("worker", "task").unwrap();
        reg.append_transcript(&child.id, "caller-owned history");
        reg.settle(&child.id, ChildStatus::Done);
        let registry = Mutex::new(reg);
        cleanup_registry_files(&registry);
        assert_eq!(
            fs::read_to_string(child.transcript_path).unwrap(),
            "caller-owned history\n"
        );
        assert!(registry.lock().unwrap().dir.is_some());
        assert!(fresh_registry().take_retired_dir().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn new_namespaces_and_transcripts_are_private_at_creation() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let mut reg = fresh_registry();
        let dir = reg.dir_in(temp.path()).unwrap();
        let child = reg.register("worker", "task").unwrap();
        reg.append_transcript(&child.id, "private transcript");
        reg.steer(&child.id, "parent", "private instruction").unwrap();
        for path in [
            dir,
            child.transcript_path,
            child.steer_path.with_extension("steer.lock"),
            child.steer_path,
        ] {
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn replaced_namespace_symlink_is_neither_adopted_nor_cleaned() {
        let temp = tempfile::tempdir().unwrap();
        let mut reg = fresh_registry();
        let dir = reg.dir_in(temp.path()).unwrap();
        let child = reg.register("worker", "task").unwrap();
        reg.append_transcript(&child.id, "original history");
        let retained = temp.path().join("retained");
        fs::rename(&dir, &retained).unwrap();
        let unrelated = temp.path().join("unrelated");
        fs::create_dir(&unrelated).unwrap();
        fs::write(unrelated.join("keep"), "unrelated data").unwrap();
        std::os::unix::fs::symlink(&unrelated, &dir).unwrap();
        assert!(reg.register("worker", "must not follow the link").is_err());
        assert_eq!(reg.seq, 1);
        reg.settle(&child.id, ChildStatus::Done);
        cleanup_registry_files(&Mutex::new(reg));
        assert_eq!(fs::read_link(dir).unwrap(), unrelated);
        assert_eq!(
            fs::read_to_string(unrelated.join("keep")).unwrap(),
            "unrelated data"
        );
        assert_eq!(
            fs::read_to_string(retained.join(child.transcript_path.file_name().unwrap())).unwrap(),
            "original history\n"
        );
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
        reg.steer(&child.id, "parent", "earlier")
            .expect("first send");
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
        assert_eq!(
            fs::read(&child.steer_path).expect("untouched queue"),
            original
        );
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
        assert_eq!(
            fs::read(&path).expect("pending queue retained"),
            original.to_vec()
        );
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
        let err =
            poll_until_uncontended(&child.steer_path).expect_err("malformed batch is not empty");
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
        reg.steer(&child.id, "parent", "earlier")
            .expect("first send");
        let original = fs::read(&child.steer_path).expect("first frame");
        let torn = &original[..original.len() - 1];
        fs::write(&child.steer_path, torn).expect("simulate missing final newline");
        let err = reg
            .steer(&child.id, "parent", "must not concatenate")
            .expect_err("torn tail must reject new delivery");
        assert!(err.to_string().contains("unterminated frame"));
        assert_eq!(reg.bus_seq, 1);
        assert_eq!(reg.inbox(&child.id).len(), 1);
        assert_eq!(
            fs::read(&child.steer_path).expect("torn bytes retained"),
            torn.to_vec()
        );
        fs::write(&child.steer_path, original).expect("repair terminator");
        assert_eq!(
            reg.steer(&child.id, "parent", "retry").expect("retry").seq,
            2
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
    fn unterminated_valid_json_is_retained_with_its_valid_neighbors() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg.register("worker", "task").expect("register");
        reg.steer(&child.id, "parent", "first").expect("first send");
        reg.steer(&child.id, "parent", "second")
            .expect("second send");
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
        assert_eq!(
            fs::read(path.with_extension("draining")).expect("retained frame"),
            frame
        );
    }

    #[test]
    fn escaped_body_must_fit_the_serialized_frame_budget() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut reg = fresh_registry();
        reg.set_dir_for_tests(temp.path().to_path_buf());
        let child = reg.register("worker", "task").expect("register");
        let body = "\"".repeat(MAX_STEER_FRAME_BYTES / 2);
        assert!(body.len() < MAX_STEER_FRAME_BYTES);
        let err = reg
            .steer(&child.id, "parent", &body)
            .expect_err("escaped wire size");
        assert!(
            err.to_string()
                .contains("serialized steering frame exceeds")
        );
        assert_eq!(reg.bus_seq, 0);
        assert!(reg.inbox(&child.id).is_empty());
        assert!(!child.steer_path.exists());
        assert_eq!(
            reg.steer(&child.id, "parent", "small")
                .expect("later send")
                .seq,
            1
        );
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
    fn reaped_child_keeps_history_but_loses_pid_control_before_writeback() {
        let temp = tempfile::tempdir().unwrap();
        let mut reg = fresh_registry();
        reg.dir = Some(temp.path().to_path_buf());
        let first = reg.register("worker", "first").unwrap();
        let second = reg.register("worker", "second").unwrap();
        assert_eq!(reg.control_pid(&first.id), None);
        reg.mark_running(&first.id, 4242);
        reg.mark_running(&second.id, 4243);
        assert_eq!(reg.control_pid(&first.id), Some(4242));

        reg.mark_process_reaped(&first.id);
        let waiting = reg.get(&first.id).unwrap();
        assert_eq!(waiting.pid, Some(4242));
        assert_eq!(waiting.status, ChildStatus::Running);
        assert_eq!(reg.control_pid(&first.id), None);
        assert_eq!(reg.control_pid(&second.id), Some(4243));
        reg.mark_running(&first.id, 9999);
        assert_eq!(reg.control_pid(&first.id), None);

        reg.mark_killed(&first.id);
        reg.settle(&first.id, ChildStatus::Done);
        let killed = reg.get(&first.id).unwrap();
        assert_eq!(killed.pid, Some(4242));
        assert_eq!(killed.status, ChildStatus::Killed);
        assert_eq!(reg.control_pid(&first.id), None);
        reg.settle(&second.id, ChildStatus::Done);
        assert_eq!(reg.control_pid(&second.id), None);
        assert_eq!(reg.get(&second.id).unwrap().pid, Some(4243));
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
