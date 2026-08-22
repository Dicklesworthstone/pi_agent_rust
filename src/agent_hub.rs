//! Agent hub registry (bd-cv653.5.3): a session-scoped roster of spawned
//! subagent children with transcript persistence, steering delivery, kill,
//! revive, and a minimal peer-messaging bus.
//!
//! Children are separate `pi` processes (`--mode json --print --no-session`),
//! so cross-process steering is delivered through an append-only queue file
//! per child (`<id>.steer`); the child's print-mode loop drains that file
//! through a steering [`crate::agent::MessageFetcher`] between turns. The
//! parent's in-memory registry is the roster of record; the on-disk queue
//! files are the delivery mechanism.
//!
//! NTM layer distinction: this hub manages pi's OWN spawned children in this
//! process. It does not rebuild ntm's cross-tmux fleet orchestration.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
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
    /// The task text (truncated for roster display).
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
#[derive(Debug, Default)]
pub struct AgentHubRegistry {
    entries: BTreeMap<String, ChildEntry>,
    seq: u64,
    /// Per-recipient delivered/queued bus messages (in-memory view; the
    /// on-disk steer files are the cross-process channel).
    bus: BTreeMap<String, VecDeque<BusMessage>>,
    bus_seq: u64,
    /// Artifacts dir for this session's hub files.
    dir: Option<PathBuf>,
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
        self.seq = self.seq.saturating_add(1);
        let id = format!("{}-{}", sanitize_id(name), self.seq);
        let dir = self.dir()?;
        let entry = ChildEntry {
            id: id.clone(),
            name: name.to_string(),
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
        self.entries.insert(id, entry.clone());
        Ok(entry)
    }

    /// Mark the child running with its pid.
    pub fn mark_running(&mut self, id: &str, pid: u32) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.pid = Some(pid);
            entry.status = ChildStatus::Running;
        }
    }

    /// Settle a run (terminal status + finish timestamp).
    pub fn settle(&mut self, id: &str, status: ChildStatus) {
        if let Some(entry) = self.entries.get_mut(id) {
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
        let raw = match fs::read_to_string(&entry.transcript_path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => {
                return Err(Error::tool(
                    "hub",
                    format!("read transcript {}: {err}", entry.transcript_path.display()),
                ));
            }
        };
        let tail = tail_bytes(&raw, TRANSCRIPT_PAGE_BYTES);
        let mut vault = crate::secrets::SecretVault::default();
        let (masked, _audit) = crate::secrets::obfuscate(&tail, &mut vault, &[]);
        Ok(masked)
    }

    /// Queue a steering message for a running child: in-memory record +
    /// append to the child's steer file (the cross-process channel).
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
        let message = self.enqueue_bus(id, from, body);
        append_steer_line(&entry.steer_path, &message)?;
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

    fn enqueue_bus(&mut self, to: &str, from: &str, body: &str) -> BusMessage {
        self.bus_seq = self.bus_seq.saturating_add(1);
        let message = BusMessage {
            seq: self.bus_seq,
            from: from.to_string(),
            to: to.to_string(),
            body: body.to_string(),
            sent_ms: now_ms(),
        };
        let queue = self.bus.entry(to.to_string()).or_default();
        if queue.len() >= MAX_QUEUE_PER_CHILD {
            queue.pop_front();
        }
        queue.push_back(message.clone());
        message
    }

    /// Mark a child killed by the operator.
    pub fn mark_killed(&mut self, id: &str) {
        self.settle(id, ChildStatus::Killed);
    }

    /// Register a revived run continuing `from_id`: fresh entry carrying the
    /// prior transcript as context. Returns the new entry plus the task text
    /// to launch (original task + transcript tail + continue directive).
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
        let transcript = fs::read_to_string(&prior.transcript_path).unwrap_or_default();
        let tail = tail_bytes(&transcript, REVIVE_TRANSCRIPT_BUDGET);
        let task = format!(
            "{}\n\n[Continuation of a prior run ({}). Its transcript tail follows; \
             pick up where it left off and finish the task.]\n{}",
            prior.task,
            prior.status.as_str(),
            tail
        );
        let mut entry = self.register(&prior.name, &task)?;
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

/// Append one steering frame to the child's queue file. The child drains the
/// file between turns; each line is one JSON message.
fn append_steer_line(path: &Path, message: &BusMessage) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| Error::tool("hub", format!("append steer queue {}: {e}", path.display())))?;
    let line = serde_json::to_string(message)
        .map_err(|e| Error::validation(format!("serialize bus message: {e}")))?;
    writeln!(file, "{line}")
        .map_err(|e| Error::tool("hub", format!("write steer queue {}: {e}", path.display())))
}

/// Child-side drain: consume the steer file, returning queued bodies in
/// delivery order. Called by the print-mode steering fetcher.
pub fn drain_steer_file(path: &Path) -> Vec<String> {
    // Rename-consume: read-then-truncate loses any line the parent appends
    // between the read and the truncating write (the parent appends from a
    // different process). rename is atomic, and a parent append racing the
    // rename lands wholly in the old file (drained now) or a fresh steer
    // file (drained next poll) — never destroyed.
    let draining = path.with_extension("draining");
    if fs::rename(path, &draining).is_err() {
        // Nothing queued (file absent) or transient; retry next poll.
        return Vec::new();
    }
    let raw = fs::read_to_string(&draining).unwrap_or_default();
    let _ = fs::remove_file(&draining);
    if raw.is_empty() {
        return Vec::new();
    }
    raw.lines()
        .filter_map(|line| {
            serde_json::from_str::<BusMessage>(line)
                .ok()
                .map(|m| format!("[hub:{}] {}", m.from, m.body))
        })
        .collect()
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
        reg.mark_running(&entry.id, 4242);
        assert_eq!(
            reg.get(&entry.id).expect("get").status,
            ChildStatus::Running
        );
        reg.settle(&entry.id, ChildStatus::Done);
        let settled = reg.get(&entry.id).expect("get");
        assert_eq!(settled.status, ChildStatus::Done);
        assert!(settled.finished_ms.is_some());
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
        let drained = drain_steer_file(&entry.steer_path);
        assert_eq!(
            drained,
            vec!["[hub:a] first".to_string(), "[hub:b] second".to_string()]
        );
        // Drain is consume-once.
        assert!(drain_steer_file(&entry.steer_path).is_empty());
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
