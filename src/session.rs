//! Session management and persistence.
//!
//! Sessions are stored as JSONL files with a tree structure that enables
//! branching and history navigation.

use crate::agent_cx::AgentCx;
use crate::cli::Cli;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::extensions::ExtensionSession;
use crate::model::{
    AssistantMessage, ContentBlock, Message, TextContent, ToolResultMessage, UserContent,
    UserMessage,
};
use crate::provider_metadata::{canonical_provider_id, provider_ids_match};
#[cfg(feature = "session-index")]
use crate::session_index::{
    SessionIndex, enqueue_session_index_snapshot_update, is_session_file_path, session_file_stats,
};
use crate::session_store_v2::{self, SessionStoreV2};
#[cfg(feature = "session-index")]
use crate::tui::PiConsole;
use asupersync::channel::oneshot;
use asupersync::sync::Mutex;
use async_trait::async_trait;
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
#[cfg(feature = "session-index")]
use std::io::IsTerminal;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Instant;
#[cfg(all(test, feature = "session-index"))]
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

/// Current session file format version.
pub const SESSION_VERSION: u8 = 3;
const MAX_JSONL_LINE_BYTES: usize = 100 * 1024 * 1024;
const V2_CHAIN_HASH_GENESIS: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";
const ROOT_LEAF_OVERRIDE_SENTINEL: &str = "";

fn finish_worker_result<T, E>(
    handle: thread::JoinHandle<()>,
    recv_result: std::result::Result<Result<T>, E>,
    cancelled_message: &'static str,
) -> Result<T> {
    if let Err(panic_payload) = handle.join() {
        std::panic::resume_unwind(panic_payload);
    }
    recv_result.map_err(|_| crate::Error::session(cancelled_message))?
}

fn read_capped_utf8_line_with_limit<R: std::io::BufRead>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<Option<String>> {
    use std::io::BufRead;

    let limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX.saturating_sub(2))
        .saturating_add(2);
    let mut bytes = Vec::new();
    let bytes_read = reader.take(limit).read_until(b'\n', &mut bytes)?;
    if bytes_read == 0 {
        return Ok(None);
    }

    let content_len = bytes.strip_suffix(b"\n").map_or(bytes.len(), <[u8]>::len);
    if content_len > max_bytes {
        if !bytes.ends_with(b"\n") {
            let mut discard = Vec::new();
            loop {
                discard.clear();
                let discarded = reader.read_until(b'\n', &mut discard)?;
                if discarded == 0 || discard.ends_with(b"\n") {
                    break;
                }
            }
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("JSONL line exceeds {max_bytes} bytes"),
        ));
    }

    String::from_utf8(bytes)
        .map(Some)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}

fn read_capped_utf8_line<R: std::io::BufRead>(reader: &mut R) -> std::io::Result<Option<String>> {
    read_capped_utf8_line_with_limit(reader, MAX_JSONL_LINE_BYTES)
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn save_jsonl_full_rewrite_blocking(
    path: &Path,
    sessions_root: &Path,
    header: &SessionHeader,
    entries: &[SessionEntry],
    persisted_entry_count: usize,
    header_dirty: bool,
) -> Result<(SessionHeader, Vec<SessionEntry>)> {
    let _lock = lock_session_persistence(path)?;
    let (header_to_write, entries_to_write) =
        prepare_jsonl_full_rewrite(path, header, entries, persisted_entry_count, header_dirty)?;
    let original_perms = std::fs::metadata(path).ok().map(|meta| meta.permissions());
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp_file = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut writer = std::io::BufWriter::with_capacity(1 << 20, temp_file.as_file());
        serde_json::to_writer(&mut writer, &header_to_write)?;
        writer.write_all(b"\n")?;
        for entry in &entries_to_write {
            serde_json::to_writer(&mut writer, entry)?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
    }
    temp_file
        .as_file_mut()
        .sync_all()
        .map_err(|e| crate::Error::Io(Box::new(e)))?;
    if let Some(perms) = original_perms {
        temp_file
            .as_file()
            .set_permissions(perms)
            .map_err(|e| crate::Error::Io(Box::new(e)))?;
    }
    temp_file
        .persist(path)
        .map_err(|e| crate::Error::Io(Box::new(e.error)))?;
    sync_parent_dir(path).map_err(|e| crate::Error::Io(Box::new(e)))?;
    #[cfg(feature = "session-index")]
    {
        let mut entries_for_stats = entries_to_write.clone();
        let finalized = finalize_loaded_entries(&mut entries_for_stats);
        let message_count = finalized.message_count;
        let session_name = finalized.name;
        enqueue_session_index_snapshot_update(
            sessions_root,
            path,
            &header_to_write,
            message_count,
            session_name,
        );
    }
    #[cfg(not(feature = "session-index"))]
    let _ = sessions_root;
    Ok((header_to_write, entries_to_write))
}

fn append_jsonl_entries_blocking(
    path: &Path,
    sessions_root: &Path,
    header: &SessionHeader,
    serialized_entries: &[u8],
    message_count: u64,
    session_name: Option<String>,
) -> Result<()> {
    let _lock = lock_session_persistence(path)?;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| crate::Error::Io(Box::new(e)))?;
    file.write_all(serialized_entries)?;
    file.sync_all().map_err(|e| crate::Error::Io(Box::new(e)))?;

    #[cfg(feature = "session-index")]
    enqueue_session_index_snapshot_update(sessions_root, path, header, message_count, session_name);
    #[cfg(not(feature = "session-index"))]
    let _ = (sessions_root, header, message_count, session_name);
    Ok(())
}

fn session_persistence_lock_path(path: &Path) -> PathBuf {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

fn lock_session_persistence(path: &Path) -> Result<SessionPersistenceLockGuard> {
    let lock_path = session_persistence_lock_path(path);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| crate::Error::Io(Box::new(e)))?;
    file.lock_exclusive()?;
    Ok(SessionPersistenceLockGuard { file })
}

#[derive(Debug)]
struct SessionPersistenceLockGuard {
    file: std::fs::File,
}

impl Drop for SessionPersistenceLockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn prepare_jsonl_full_rewrite(
    path: &Path,
    header: &SessionHeader,
    entries: &[SessionEntry],
    persisted_entry_count: usize,
    header_dirty: bool,
) -> Result<(SessionHeader, Vec<SessionEntry>)> {
    let pending_start = persisted_entry_count.min(entries.len());
    let mut merged_entries = entries[..pending_start].to_vec();
    let local_pending = &entries[pending_start..];
    let mut header_to_write = header.clone();

    if path
        .try_exists()
        .map_err(|e| crate::Error::Io(Box::new(e)))?
    {
        let (disk_session, _) = open_jsonl_blocking(path.to_path_buf())?;
        if !header_dirty {
            header_to_write = disk_session.header;
        }

        let known_ids: HashSet<&str> = entries
            .iter()
            .filter_map(|entry| entry.base_id().map(String::as_str))
            .collect();

        for disk_entry in disk_session.entries.into_iter().skip(pending_start) {
            let should_merge = disk_entry
                .base_id()
                .is_none_or(|id| !known_ids.contains(id.as_str()));
            if should_merge {
                merged_entries.push(disk_entry);
            }
        }
    }

    merged_entries.extend_from_slice(local_pending);
    Ok((header_to_write, merged_entries))
}

fn resolve_loaded_leaf_id(
    header: &SessionHeader,
    natural_leaf_id: Option<String>,
    entry_index: &HashMap<String, usize>,
) -> Option<String> {
    match header.current_leaf.as_deref() {
        Some(ROOT_LEAF_OVERRIDE_SENTINEL) => None,
        Some(leaf_id) if entry_index.contains_key(leaf_id) => Some(leaf_id.to_string()),
        _ => natural_leaf_id,
    }
}

fn normalize_loaded_header(mut header: SessionHeader) -> (SessionHeader, bool) {
    let header_dirty = header.materialize_branch_fallbacks();
    (header, header_dirty)
}

fn total_v2_message_count(store: &SessionStoreV2) -> Result<Option<u64>> {
    if let Some(manifest) = store.read_manifest()? {
        return Ok(Some(manifest.counters.messages_total));
    }

    let mut total = 0u64;
    for frame in store.read_all_entries()? {
        if frame.entry_type.eq("message") {
            total = total.saturating_add(1);
        }
    }
    Ok(Some(total))
}

/// Handle to a thread-safe shared session.
#[derive(Clone, Debug)]
pub struct SessionHandle(pub Arc<Mutex<Session>>);

fn current_path_model_pair(session: &Session) -> Option<(String, String)> {
    session.effective_model_for_current_path()
}

fn current_path_model_fields(session: &Session) -> (Option<String>, Option<String>) {
    if let Some((provider, model_id)) = current_path_model_pair(session) {
        (Some(provider), Some(model_id))
    } else {
        session.header.branch_fallback_model_fields()
    }
}

fn current_path_thinking_level(session: &Session) -> Option<String> {
    session.effective_thinking_level_for_current_path()
}

#[async_trait]
impl ExtensionSession for SessionHandle {
    async fn get_state(&self) -> Value {
        let cx = AgentCx::for_current_or_request();
        let Ok(session) = self.0.lock(cx.cx()).await else {
            return serde_json::json!({
                "model": null,
                "thinkingLevel": "off",
                "durabilityMode": "balanced",
                "isStreaming": false,
                "isCompacting": false,
                "steeringMode": "one-at-a-time",
                "followUpMode": "one-at-a-time",
                "sessionFile": null,
                "sessionId": "",
                "sessionName": null,
                "autoCompactionEnabled": false,
                "messageCount": 0,
                "pendingMessageCount": 0,
            });
        };
        let session_file = session.path.as_ref().map(|p| p.display().to_string());
        let session_id = session.header.id.clone();
        let session_name = session.get_name();
        let model =
            current_path_model_pair(&session).map_or(Value::Null, |(provider, model_id)| {
                serde_json::json!({
                    "provider": provider,
                    "id": model_id,
                })
            });
        let thinking_level =
            current_path_thinking_level(&session).unwrap_or_else(|| "off".to_string());
        let message_count = session
            .entries_for_current_path()
            .iter()
            .filter(|entry| matches!(entry, SessionEntry::Message(_)))
            .count();
        let pending_message_count = session.autosave_metrics().pending_mutations;
        let durability_mode = session.autosave_durability_mode().as_str();
        serde_json::json!({
            "model": model,
            "thinkingLevel": thinking_level,
            "durabilityMode": durability_mode,
            "isStreaming": false,
            "isCompacting": false,
            "steeringMode": "one-at-a-time",
            "followUpMode": "one-at-a-time",
            "sessionFile": session_file,
            "sessionId": session_id,
            "sessionName": session_name,
            "autoCompactionEnabled": false,
            "messageCount": message_count,
            "pendingMessageCount": pending_message_count,
        })
    }

    async fn get_messages(&self) -> Vec<SessionMessage> {
        let cx = AgentCx::for_current_or_request();
        let Ok(session) = self.0.lock(cx.cx()).await else {
            return Vec::new();
        };
        // Return messages for the current branch only, filtered to
        // user/assistant/toolResult/bashExecution/custom per spec §3.3.
        session
            .entries_for_current_path()
            .iter()
            .filter_map(|entry| match entry {
                SessionEntry::Message(msg) => match msg.message {
                    SessionMessage::User { .. }
                    | SessionMessage::Assistant { .. }
                    | SessionMessage::ToolResult { .. }
                    | SessionMessage::BashExecution { .. }
                    | SessionMessage::Custom { .. } => Some(msg.message.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    async fn get_entries(&self) -> Vec<Value> {
        let cx = AgentCx::for_current_or_request();
        let Ok(session) = self.0.lock(cx.cx()).await else {
            return Vec::new();
        };
        session
            .entries
            .iter()
            .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
            .collect()
    }

    async fn get_branch(&self) -> Vec<Value> {
        let cx = AgentCx::for_current_or_request();
        let Ok(session) = self.0.lock(cx.cx()).await else {
            return Vec::new();
        };
        session
            .entries_for_current_path()
            .iter()
            .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
            .collect()
    }

    async fn set_name(&self, name: String) -> Result<()> {
        let cx = AgentCx::for_current_or_request();
        let mut session = self
            .0
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(format!("Failed to lock session: {e}")))?;
        #[cfg(test)]
        emit_set_name_deadline_probe(&session.header.id, cx.budget().deadline);
        session.set_name(&name);
        Ok(())
    }

    async fn append_message(&self, message: SessionMessage) -> Result<()> {
        let cx = AgentCx::for_current_or_request();
        let mut session = self
            .0
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(format!("Failed to lock session: {e}")))?;
        session.append_message(message);
        Ok(())
    }

    async fn append_custom_entry(&self, custom_type: String, data: Option<Value>) -> Result<()> {
        let cx = AgentCx::for_current_or_request();
        let mut session = self
            .0
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(format!("Failed to lock session: {e}")))?;
        if custom_type.trim().is_empty() {
            return Err(Error::validation("customType must not be empty"));
        }
        session.append_custom_entry(custom_type, data);
        Ok(())
    }

    async fn set_model(&self, provider: String, model_id: String) -> Result<()> {
        let cx = AgentCx::for_current_or_request();
        let mut session = self
            .0
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(format!("Failed to lock session: {e}")))?;
        let normalized_provider = canonical_provider_id(&provider)
            .unwrap_or(&provider)
            .to_string();
        let (stored_provider, stored_model_id, changed) = match current_path_model_pair(&session) {
            Some((current_provider, current_model_id))
                if provider_ids_match(&current_provider, &provider)
                    && current_model_id.eq_ignore_ascii_case(&model_id) =>
            {
                (current_provider, current_model_id, false)
            }
            _ => (normalized_provider, model_id.clone(), true),
        };
        if changed {
            session.append_model_change(stored_provider.clone(), stored_model_id.clone());
        }
        session.set_model_header(Some(stored_provider), Some(stored_model_id), None);
        Ok(())
    }

    async fn get_model(&self) -> (Option<String>, Option<String>) {
        let cx = AgentCx::for_current_or_request();
        let Ok(session) = self.0.lock(cx.cx()).await else {
            return (None, None);
        };
        current_path_model_fields(&session)
    }

    async fn set_thinking_level(&self, level: String) -> Result<()> {
        let cx = AgentCx::for_current_or_request();
        let mut session = self
            .0
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(format!("Failed to lock session: {e}")))?;
        let changed = !current_path_thinking_level(&session)
            .as_deref()
            .is_some_and(|current| current.eq(level.as_str()));
        if changed {
            session.append_thinking_level_change(level.clone());
        }
        session.set_model_header(None, None, Some(level));
        Ok(())
    }

    async fn get_thinking_level(&self) -> Option<String> {
        let cx = AgentCx::for_current_or_request();
        let Ok(session) = self.0.lock(cx.cx()).await else {
            return None;
        };
        current_path_thinking_level(&session)
    }

    async fn set_label(&self, target_id: String, label: Option<String>) -> Result<()> {
        let cx = AgentCx::for_current_or_request();
        let mut session = self
            .0
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(format!("Failed to lock session: {e}")))?;
        if session.add_label(&target_id, label).is_none() {
            return Err(Error::validation(format!(
                "target entry '{target_id}' not found in session"
            )));
        }
        Ok(())
    }
}

/// Default base URL for the Pi session share viewer.
pub const DEFAULT_SHARE_VIEWER_URL: &str = "https://buildwithpi.ai/session/";

fn build_share_viewer_url(base_url: Option<&str>, gist_id: &str) -> String {
    let base_url = base_url
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_SHARE_VIEWER_URL);
    format!("{base_url}#{gist_id}")
}

/// Get the share viewer URL for a gist ID.
///
/// Matches legacy Pi Agent semantics:
/// - Use `PI_SHARE_VIEWER_URL` env var when set and non-empty
/// - Otherwise fall back to `DEFAULT_SHARE_VIEWER_URL`
/// - Final URL is `{base}#{gist_id}` (no trailing-slash normalization)
#[must_use]
pub fn get_share_viewer_url(gist_id: &str) -> String {
    let base_url = std::env::var("PI_SHARE_VIEWER_URL").ok();
    build_share_viewer_url(base_url.as_deref(), gist_id)
}

/// Session persistence backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStoreKind {
    Jsonl,
    #[cfg(feature = "sqlite-sessions")]
    Sqlite,
}

impl SessionStoreKind {
    /// Stable configuration value for this conversation store.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Jsonl => "jsonl",
            #[cfg(feature = "sqlite-sessions")]
            Self::Sqlite => "sqlite",
        }
    }

    fn from_config(config: &Config) -> Self {
        let Some(value) = config.session_store.as_deref() else {
            return Self::Jsonl;
        };

        if value.eq_ignore_ascii_case("jsonl") {
            return Self::Jsonl;
        }

        if value.eq_ignore_ascii_case("sqlite") {
            #[cfg(feature = "sqlite-sessions")]
            {
                return Self::Sqlite;
            }

            #[cfg(not(feature = "sqlite-sessions"))]
            {
                tracing::warn!(
                    "Config requests session_store=sqlite but binary lacks `sqlite-sessions`; falling back to jsonl"
                );
                return Self::Jsonl;
            }
        }

        tracing::warn!("Unknown session_store `{value}`, falling back to jsonl");
        Self::Jsonl
    }

    const fn extension(self) -> &'static str {
        match self {
            Self::Jsonl => "jsonl",
            #[cfg(feature = "sqlite-sessions")]
            Self::Sqlite => "sqlite",
        }
    }
}

/// Default upper bound for queued autosave mutations before backpressure coalescing kicks in.
const DEFAULT_AUTOSAVE_MAX_PENDING_MUTATIONS: usize = 256;

fn autosave_max_pending_mutations() -> usize {
    std::env::var("PI_SESSION_AUTOSAVE_MAX_PENDING")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_AUTOSAVE_MAX_PENDING_MUTATIONS)
}

/// Default number of incremental appends before forcing a full checkpoint rewrite.
const DEFAULT_COMPACTION_CHECKPOINT_INTERVAL: u64 = 50;

fn compaction_checkpoint_interval() -> u64 {
    std::env::var("PI_SESSION_COMPACTION_INTERVAL")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_COMPACTION_CHECKPOINT_INTERVAL)
}

/// Durability mode for write-behind autosave behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutosaveDurabilityMode {
    Strict,
    Balanced,
    Throughput,
}

impl AutosaveDurabilityMode {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "strict" => Some(Self::Strict),
            "balanced" => Some(Self::Balanced),
            "throughput" => Some(Self::Throughput),
            _ => None,
        }
    }

    fn from_env() -> Self {
        std::env::var("PI_SESSION_DURABILITY_MODE")
            .ok()
            .as_deref()
            .and_then(Self::parse)
            .unwrap_or(Self::Balanced)
    }

    const fn should_flush_on_shutdown(self) -> bool {
        matches!(self, Self::Strict | Self::Balanced)
    }

    const fn best_effort_on_shutdown(self) -> bool {
        matches!(self, Self::Balanced)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Balanced => "balanced",
            Self::Throughput => "throughput",
        }
    }
}

fn resolve_autosave_durability_mode(
    cli_mode: Option<&str>,
    config_mode: Option<&str>,
    env_mode: Option<&str>,
) -> AutosaveDurabilityMode {
    cli_mode
        .and_then(AutosaveDurabilityMode::parse)
        .or_else(|| config_mode.and_then(AutosaveDurabilityMode::parse))
        .or_else(|| env_mode.and_then(AutosaveDurabilityMode::parse))
        .unwrap_or(AutosaveDurabilityMode::Balanced)
}

/// Autosave flush trigger used for observability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutosaveFlushTrigger {
    Manual,
    Periodic,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutosaveMutationKind {
    Message,
    Metadata,
    Label,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AutosaveFlushTicket {
    batch_size: usize,
    started_at: Instant,
    trigger: AutosaveFlushTrigger,
}

/// Snapshot of autosave queue state and lifecycle counters.
#[derive(Debug, Clone, Copy, Default)]
pub struct AutosaveQueueMetrics {
    pub pending_mutations: usize,
    pub max_pending_mutations: usize,
    pub coalesced_mutations: u64,
    pub backpressure_events: u64,
    pub flush_started: u64,
    pub flush_succeeded: u64,
    pub flush_failed: u64,
    pub last_flush_batch_size: usize,
    pub last_flush_duration_ms: Option<u64>,
    pub last_flush_trigger: Option<AutosaveFlushTrigger>,
}

#[derive(Debug, Clone)]
struct AutosaveQueue {
    pending_mutations: usize,
    max_pending_mutations: usize,
    coalesced_mutations: u64,
    backpressure_events: u64,
    flush_started: u64,
    flush_succeeded: u64,
    flush_failed: u64,
    last_flush_batch_size: usize,
    last_flush_duration_ms: Option<u64>,
    last_flush_trigger: Option<AutosaveFlushTrigger>,
}

impl AutosaveQueue {
    fn new() -> Self {
        Self {
            pending_mutations: 0,
            max_pending_mutations: autosave_max_pending_mutations(),
            coalesced_mutations: 0,
            backpressure_events: 0,
            flush_started: 0,
            flush_succeeded: 0,
            flush_failed: 0,
            last_flush_batch_size: 0,
            last_flush_duration_ms: None,
            last_flush_trigger: None,
        }
    }

    #[cfg(test)]
    fn with_limit(max_pending_mutations: usize) -> Self {
        let mut queue = Self::new();
        queue.max_pending_mutations = max_pending_mutations.max(1);
        queue
    }

    const fn metrics(&self) -> AutosaveQueueMetrics {
        AutosaveQueueMetrics {
            pending_mutations: self.pending_mutations,
            max_pending_mutations: self.max_pending_mutations,
            coalesced_mutations: self.coalesced_mutations,
            backpressure_events: self.backpressure_events,
            flush_started: self.flush_started,
            flush_succeeded: self.flush_succeeded,
            flush_failed: self.flush_failed,
            last_flush_batch_size: self.last_flush_batch_size,
            last_flush_duration_ms: self.last_flush_duration_ms,
            last_flush_trigger: self.last_flush_trigger,
        }
    }

    const fn enqueue_mutation(&mut self, _kind: AutosaveMutationKind) {
        if self.pending_mutations == 0 {
            self.pending_mutations = 1;
            return;
        }
        self.coalesced_mutations = self.coalesced_mutations.saturating_add(1);
        if self.pending_mutations < self.max_pending_mutations {
            self.pending_mutations += 1;
        } else {
            self.backpressure_events = self.backpressure_events.saturating_add(1);
        }
    }

    fn begin_flush(&mut self, trigger: AutosaveFlushTrigger) -> Option<AutosaveFlushTicket> {
        if self.pending_mutations == 0 {
            return None;
        }
        let batch_size = self.pending_mutations;
        self.pending_mutations = 0;
        self.flush_started = self.flush_started.saturating_add(1);
        self.last_flush_batch_size = batch_size;
        self.last_flush_trigger = Some(trigger);
        Some(AutosaveFlushTicket {
            batch_size,
            started_at: Instant::now(),
            trigger,
        })
    }

    fn finish_flush(&mut self, ticket: AutosaveFlushTicket, success: bool) {
        let elapsed = ticket.started_at.elapsed().as_millis();
        let elapsed = u64::try_from(elapsed.min(u128::from(u64::MAX)))
            .expect("elapsed milliseconds clamped to u64::MAX");
        self.last_flush_duration_ms = Some(elapsed);
        self.last_flush_trigger = Some(ticket.trigger);
        if success {
            self.flush_succeeded = self.flush_succeeded.saturating_add(1);
            return;
        }

        self.flush_failed = self.flush_failed.saturating_add(1);
        // New mutations may have arrived while the flush was in flight.
        // Restore only into remaining capacity so pending count never exceeds
        // `max_pending_mutations`.
        let available_capacity = self
            .max_pending_mutations
            .saturating_sub(self.pending_mutations);
        let restored = ticket.batch_size.min(available_capacity);
        self.pending_mutations = self.pending_mutations.saturating_add(restored);
        let dropped = ticket.batch_size.saturating_sub(restored);
        if dropped > 0 {
            let dropped = dropped as u64;
            self.backpressure_events = self.backpressure_events.saturating_add(dropped);
            self.coalesced_mutations = self.coalesced_mutations.saturating_add(dropped);
        }
    }
}

// ============================================================================
// Session
// ============================================================================

/// A session manages conversation state and persistence.
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct Session {
    /// Session header
    pub header: SessionHeader,
    /// Session entries (messages, changes, etc.)
    pub entries: Vec<SessionEntry>,
    /// Path to the session file (None for in-memory)
    pub path: Option<PathBuf>,
    /// Current leaf entry ID. Direct modification outside of `session.rs`
    /// is forbidden because it can desynchronize the `is_linear` optimization cache.
    pub(crate) leaf_id: Option<String>,
    /// Base directory for session storage (optional override)
    pub session_dir: Option<PathBuf>,
    store_kind: SessionStoreKind,
    /// Cached entry IDs for O(1) uniqueness checks when appending.
    entry_ids: HashSet<String>,

    // -- Performance caches (Gaps A/B/C) --
    /// True when all entries form a linear chain (no branching).
    /// When true, `entries_for_current_path()` returns all entries without
    /// building a parent map — the 99% fast path.
    is_linear: bool,
    /// Map from entry ID to index in `self.entries` for O(1) lookup.
    entry_index: HashMap<String, usize>,
    /// Incrementally maintained message count (avoids O(n) scan on save).
    cached_message_count: u64,
    /// Most recent session name from `SessionInfo` entries.
    cached_name: Option<String>,
    /// Write-behind autosave queue state and lifecycle counters.
    autosave_queue: AutosaveQueue,
    /// Current durability policy for shutdown final flush behavior.
    autosave_durability: AutosaveDurabilityMode,

    // -- Incremental append state --
    /// Number of entries already persisted to disk (high-water mark).
    /// Uses `Arc<AtomicUsize>` to allow atomic updates from detached background threads,
    /// ensuring state consistency even if the save future is dropped/cancelled.
    persisted_entry_count: Arc<AtomicUsize>,
    /// True when header was modified since last save (forces full rewrite).
    header_dirty: bool,
    /// Incremental appends since last full rewrite (checkpoint counter).
    appends_since_checkpoint: u64,
    /// Sidecar root when session was loaded from V2 storage.
    v2_sidecar_root: Option<PathBuf>,
    /// True when current in-memory entries are a partial hydration view from V2.
    v2_partial_hydration: bool,
    /// Resume mode used when loading from V2 sidecar.
    v2_resume_mode: Option<V2OpenMode>,
    /// True when the JSONL file has advanced beyond the loaded V2 sidecar.
    v2_sidecar_stale: bool,
    /// Offset to add to `cached_message_count` to account for messages not loaded in memory
    /// (e.g. when using V2 tail hydration).
    v2_message_count_offset: u64,
}

impl Clone for Session {
    fn clone(&self) -> Self {
        Self {
            header: self.header.clone(),
            entries: self.entries.clone(),
            path: self.path.clone(),
            leaf_id: self.leaf_id.clone(),
            session_dir: self.session_dir.clone(),
            store_kind: self.store_kind,
            entry_ids: self.entry_ids.clone(),
            is_linear: self.is_linear,
            entry_index: self.entry_index.clone(),
            cached_message_count: self.cached_message_count,
            cached_name: self.cached_name.clone(),
            autosave_queue: self.autosave_queue.clone(),
            autosave_durability: self.autosave_durability,
            // Deep copy the atomic value to preserve value semantics for clones.
            // If we just cloned the Arc, a save on the clone would increment the
            // counter on the original, desynchronizing it from its own entries.
            persisted_entry_count: Arc::new(AtomicUsize::new(
                self.persisted_entry_count.load(Ordering::SeqCst),
            )),
            header_dirty: self.header_dirty,
            appends_since_checkpoint: self.appends_since_checkpoint,
            v2_sidecar_root: self.v2_sidecar_root.clone(),
            v2_partial_hydration: self.v2_partial_hydration,
            v2_resume_mode: self.v2_resume_mode,
            v2_sidecar_stale: self.v2_sidecar_stale,
            v2_message_count_offset: self.v2_message_count_offset,
        }
    }
}

/// Result of planning a `/fork` operation from a specific user message.
///
/// Mirrors legacy semantics:
/// - The new session's leaf is the *parent* of the selected user message (or `None` if root),
///   so the selected message can be re-submitted as a new branch without creating consecutive
///   user messages.
/// - The selected user message text is returned for editor pre-fill.
#[derive(Debug, Clone)]
pub struct ForkPlan {
    /// Entries to copy into the new session file (path to the fork leaf, inclusive).
    pub entries: Vec<SessionEntry>,
    /// Leaf ID to set in the new session (parent of selected user entry).
    pub leaf_id: Option<String>,
    /// Text of the selected user message (for editor pre-fill).
    pub selected_text: String,
}

/// Lightweight snapshot of session data for non-blocking export.
///
/// Captures only the header and entries needed for HTML rendering,
/// avoiding a full `Session` clone (which includes caches, autosave
/// queue, and other internal state).
#[derive(Debug, Clone)]
pub struct ExportSnapshot {
    /// Session header (id, timestamp, cwd).
    pub header: SessionHeader,
    /// Session entries to render.
    pub entries: Vec<SessionEntry>,
    /// Session file path (for default output filename).
    pub path: Option<PathBuf>,
}

impl ExportSnapshot {
    /// Render this snapshot as a standalone HTML document.
    ///
    /// Delegates to the shared rendering logic used by `Session::to_html()`.
    pub fn to_html(&self) -> String {
        render_session_html(&self.header, &self.entries)
    }
}

/// Diagnostics captured while opening a session file.
#[derive(Debug, Clone, Default)]
pub struct SessionOpenDiagnostics {
    pub skipped_entries: Vec<SessionOpenSkippedEntry>,
    pub orphaned_parent_links: Vec<SessionOpenOrphanedParentLink>,
}

#[derive(Debug, Clone)]
pub struct SessionOpenSkippedEntry {
    /// 1-based line number in the session file.
    pub line_number: usize,
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct SessionOpenOrphanedParentLink {
    pub entry_id: String,
    pub missing_parent_id: String,
}

/// Stable schema identifier for session cold-start trace bundles.
pub const SESSION_COLD_START_TRACE_SCHEMA: &str = "pi.session.cold_start_trace.v1";
pub const SESSION_REPLAY_MINIMIZATION_TRACE_SCHEMA: &str =
    "pi.session.replay_minimization_trace.v1";

/// Bounded, redacted trace bundle for diagnosing large-session startup latency.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionColdStartTraceBundle {
    pub schema: String,
    pub session_path_hash: String,
    pub storage: SessionColdStartStorageTrace,
    pub input: SessionColdStartInputTrace,
    pub phases: Vec<SessionColdStartPhaseTrace>,
    pub index_refresh: SessionColdStartIndexRefreshTrace,
    pub open_diagnostics: SessionColdStartOpenDiagnosticsTrace,
    pub replay_minimization: SessionReplayMinimizationTrace,
    pub compaction_scan: SessionColdStartCompactionTrace,
    pub first_render: SessionColdStartFirstRenderTrace,
    pub bounds: SessionColdStartBoundsTrace,
    pub total_elapsed_us: u64,
}

/// Storage backend selection observed during cold-start tracing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionColdStartStorageTrace {
    pub selected_backend: String,
    pub opened_backend: String,
    pub path_extension: String,
    pub sqlite_feature_enabled: bool,
    pub v2_sidecar_present: bool,
    pub v2_sidecar_stale: bool,
    pub fallback_reason: Option<String>,
}

/// Aggregate input shape for the loaded session, without raw content or paths.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionColdStartInputTrace {
    pub total_entries: usize,
    pub total_messages: u64,
}

/// Timed cold-start phase.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionColdStartPhaseTrace {
    pub name: String,
    pub elapsed_us: u64,
    pub status: String,
}

/// Incremental session-index refresh summary.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionColdStartIndexRefreshTrace {
    pub scanned_files: usize,
    pub cache_hit_files: usize,
    pub reused_files: usize,
    pub refreshed_files: usize,
    pub pruned_rows: usize,
    pub failed_files: usize,
}

/// Redacted session-open diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionColdStartOpenDiagnosticsTrace {
    pub skipped_entries: usize,
    pub orphaned_parent_links: usize,
}

/// Bounded replay-minimization evidence for branch-heavy session resumes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionReplayMinimizationTrace {
    pub schema: String,
    pub branch_count: usize,
    pub entry_count: usize,
    pub selected_depth: usize,
    pub scanned_files: usize,
    pub replayed_entries: usize,
    pub skipped_sibling_entries: usize,
    pub deterministic_steps: usize,
    pub fallback_behavior: Option<String>,
    pub verdict: String,
}

/// Bounded compaction scan summary for the current session path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionColdStartCompactionTrace {
    pub scanned_entries: usize,
    pub compaction_entries: usize,
    pub latest_compaction_present: bool,
    pub latest_compaction_index_from_end: Option<usize>,
    pub first_kept_entry_found: Option<bool>,
}

/// First-render readiness projection without message text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionColdStartFirstRenderTrace {
    pub current_path_entries: usize,
    pub projected_messages: usize,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_messages: usize,
    pub system_messages: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
    pub ready: bool,
}

/// Explicit redaction/bounding contract for cold-start trace bundles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionColdStartBoundsTrace {
    pub max_phase_count: usize,
    pub raw_path_included: bool,
    pub raw_cwd_included: bool,
    pub raw_message_content_included: bool,
}

/// Loading strategy for reconstructing a `Session` from a V2 store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V2OpenMode {
    Full,
    ActivePath,
    Tail(u64),
}

const DEFAULT_V2_LAZY_HYDRATION_THRESHOLD: u64 = 10_000;
const DEFAULT_V2_TAIL_HYDRATION_COUNT: u64 = 256;

fn parse_v2_open_mode(raw: &str) -> Option<V2OpenMode> {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }
    match normalized.as_str() {
        "full" => Some(V2OpenMode::Full),
        "active" | "active_path" | "active-path" => Some(V2OpenMode::ActivePath),
        "tail" => Some(V2OpenMode::Tail(DEFAULT_V2_TAIL_HYDRATION_COUNT)),
        _ => normalized
            .strip_prefix("tail:")
            .and_then(|value| value.parse::<u64>().ok().map(V2OpenMode::Tail)),
    }
}

fn resolve_v2_lazy_hydration_threshold(env_raw: Option<&str>) -> u64 {
    env_raw
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_V2_LAZY_HYDRATION_THRESHOLD)
}

fn select_v2_open_mode_for_resume(
    entry_count: u64,
    mode_override_raw: Option<&str>,
    threshold_override_raw: Option<&str>,
) -> (V2OpenMode, &'static str, u64) {
    let lazy_threshold = resolve_v2_lazy_hydration_threshold(threshold_override_raw);
    if let Some(raw) = mode_override_raw {
        if let Some(mode) = parse_v2_open_mode(raw) {
            return (mode, "env_override", lazy_threshold);
        }
    }

    if lazy_threshold > 0 && entry_count > lazy_threshold {
        return (
            V2OpenMode::ActivePath,
            "entry_count_above_lazy_threshold",
            lazy_threshold,
        );
    }

    (V2OpenMode::Full, "default_full", lazy_threshold)
}

impl SessionOpenDiagnostics {
    fn warning_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for skipped in &self.skipped_entries {
            lines.push(format!(
                "Warning: Skipping corrupted entry at line {} in session file: {}",
                skipped.line_number, skipped.error
            ));
        }

        if !self.skipped_entries.is_empty() {
            lines.push(format!(
                "Warning: Skipped {} corrupted entries while loading session",
                self.skipped_entries.len()
            ));
        }

        for orphan in &self.orphaned_parent_links {
            lines.push(format!(
                "Warning: Entry {} references missing parent {}",
                orphan.entry_id, orphan.missing_parent_id
            ));
        }

        if !self.orphaned_parent_links.is_empty() {
            lines.push(format!(
                "Warning: Detected {} orphaned parent links while loading session",
                self.orphaned_parent_links.len()
            ));
        }

        lines
    }
}

impl SessionColdStartTraceBundle {
    /// Emit a stable, bounded logging event for the trace bundle.
    pub fn emit_log(&self) {
        tracing::info!(
            schema = self.schema.as_str(),
            session_path_hash = self.session_path_hash.as_str(),
            selected_backend = self.storage.selected_backend.as_str(),
            opened_backend = self.storage.opened_backend.as_str(),
            total_entries = self.input.total_entries,
            total_messages = self.input.total_messages,
            phase_count = self.phases.len(),
            open_skipped_entries = self.open_diagnostics.skipped_entries,
            open_orphaned_parent_links = self.open_diagnostics.orphaned_parent_links,
            index_cache_hit_files = self.index_refresh.cache_hit_files,
            replay_branch_count = self.replay_minimization.branch_count,
            replay_entry_count = self.replay_minimization.entry_count,
            replay_selected_depth = self.replay_minimization.selected_depth,
            replay_skipped_sibling_entries = self.replay_minimization.skipped_sibling_entries,
            replay_verdict = self.replay_minimization.verdict.as_str(),
            first_render_projected_messages = self.first_render.projected_messages,
            total_elapsed_us = self.total_elapsed_us,
            "session cold-start trace bundle"
        );
    }
}

fn elapsed_us_since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn cold_start_hash_path(path: &Path) -> String {
    let mut digest = format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes()));
    digest.truncate(16);
    digest
}

fn session_cold_start_storage_trace(path: &Path) -> SessionColdStartStorageTrace {
    let path_extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("none")
        .to_string();
    let sqlite_feature_enabled = cfg!(feature = "sqlite-sessions");
    let v2_sidecar_present = session_store_v2::has_v2_sidecar(path);
    let v2_sidecar_stale = if v2_sidecar_present {
        let v2_root = session_store_v2::v2_sidecar_path(path);
        is_v2_sidecar_stale(path, &v2_root)
    } else {
        false
    };

    let (selected_backend, fallback_reason) = if matches!(path_extension.as_str(), "sqlite") {
        if sqlite_feature_enabled {
            ("sqlite", None)
        } else {
            (
                "sqlite_unavailable",
                Some("sqlite_sessions_feature_disabled".to_string()),
            )
        }
    } else if v2_sidecar_present && !v2_sidecar_stale {
        ("v2_sidecar", None)
    } else if v2_sidecar_present {
        ("jsonl", Some("v2_sidecar_stale".to_string()))
    } else {
        ("jsonl", None)
    };

    SessionColdStartStorageTrace {
        selected_backend: selected_backend.to_string(),
        opened_backend: "not_opened".to_string(),
        path_extension,
        sqlite_feature_enabled,
        v2_sidecar_present,
        v2_sidecar_stale,
        fallback_reason,
    }
}

impl Session {
    /// Create a new session from CLI args and config.
    pub async fn new(cli: &Cli, config: &Config) -> Result<Self> {
        let session_dir = cli.session_dir.as_ref().map(PathBuf::from);
        let durability_mode = resolve_autosave_durability_mode(
            cli.session_durability.as_deref(),
            config.session_durability.as_deref(),
            std::env::var("PI_SESSION_DURABILITY_MODE").ok().as_deref(),
        );
        if cli.no_session {
            let mut session = Self::in_memory();
            session.set_autosave_durability_mode(durability_mode);
            return Ok(session);
        }

        if let Some(path) = &cli.session {
            let mut session = Self::open(path).await?;
            session.session_dir = session_dir
                .clone()
                .or_else(|| infer_session_root_from_path(Path::new(path)));
            session.set_autosave_durability_mode(durability_mode);
            return Ok(session);
        }

        if cli.resume {
            #[cfg(feature = "session-index")]
            {
                let picker_input_override = config
                    .session_picker_input
                    .filter(|value| *value > 0)
                    .map(|value| value.to_string());
                let mut session = Box::pin(Self::resume_with_picker(
                    session_dir.as_deref(),
                    config,
                    picker_input_override,
                ))
                .await?;
                session.set_autosave_durability_mode(durability_mode);
                return Ok(session);
            }
            #[cfg(not(feature = "session-index"))]
            return Err(Error::session(
                "Session picker requires the `session-index` feature",
            ));
        }

        if cli.r#continue {
            #[cfg(feature = "session-index")]
            {
                let mut session =
                    Self::continue_recent_in_dir(session_dir.as_deref(), config).await?;
                session.set_autosave_durability_mode(durability_mode);
                return Ok(session);
            }
            #[cfg(not(feature = "session-index"))]
            return Err(Error::session(
                "Recent-session lookup requires the `session-index` feature",
            ));
        }

        let store_kind = SessionStoreKind::from_config(config);
        let mut session = Self::create_with_dir_and_store(session_dir, store_kind);
        session.set_autosave_durability_mode(durability_mode);

        // Create a new session
        Ok(session)
    }

    /// Resume a session by prompting the user to select from recent sessions.
    #[allow(clippy::too_many_lines)]
    #[cfg(feature = "session-index")]
    pub async fn resume_with_picker(
        override_dir: Option<&Path>,
        config: &Config,
        picker_input_override: Option<String>,
    ) -> Result<Self> {
        let is_interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        let mut picker_input_override = picker_input_override;
        if picker_input_override.is_none() && is_interactive {
            if let Some(session) = crate::session_picker::pick_session(override_dir).await {
                return Ok(session);
            }
        }

        let base_dir = override_dir.map_or_else(Config::sessions_dir, PathBuf::from);
        let store_kind = SessionStoreKind::from_config(config);
        let cwd = std::env::current_dir()?;
        let encoded_cwd = encode_cwd(&cwd);
        let project_session_dir = base_dir.join(&encoded_cwd);
        let project_session_dir_missing = indexed_session_path_is_missing(&project_session_dir);

        let base_dir_clone = base_dir.clone();
        let cwd_display = cwd.display().to_string();
        let (tx, mut rx) = oneshot::channel();

        let handle = thread::spawn(move || {
            let indexed_meta = SessionIndex::for_sessions_root(&base_dir_clone)
                .list_sessions(Some(&cwd_display))
                .unwrap_or_default();
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), Ok(indexed_meta));
        });

        let cx = AgentCx::for_request();
        let recv_result = rx.recv(cx.cx()).await;
        let indexed_meta =
            finish_worker_result(handle, recv_result, "Session picker index task cancelled")
                .unwrap_or_default();
        let session_index = SessionIndex::for_sessions_root(&base_dir);
        let (entries, missing_paths) = split_indexed_session_entries(indexed_meta);
        for path in &missing_paths {
            prune_session_index_path(
                &session_index,
                path,
                "Failed to prune missing session from index during picker refresh",
            );
        }

        if project_session_dir_missing {
            return Ok(Self::create_with_dir_and_store(Some(base_dir), store_kind));
        }

        let scanned = scan_sessions_on_disk(&project_session_dir, entries.clone()).await?;
        let mut by_path: HashMap<PathBuf, SessionPickEntry> = HashMap::new();
        for entry in entries {
            by_path.insert(entry.path.clone(), entry);
        }
        for path in &scanned.failed_paths {
            prune_session_index_path(
                &session_index,
                path,
                "Failed to prune unreadable session from index during picker refresh",
            );
            by_path.remove(path);
        }
        refresh_session_index_entries(
            &session_index,
            &scanned.refreshed_entries,
            "Failed to refresh session metadata in index during picker refresh",
        );
        merge_scanned_session_entries(&mut by_path, scanned.entries);
        let mut entries = by_path.into_values().collect::<Vec<_>>();

        if entries.is_empty() {
            return Ok(Self::create_with_dir_and_store(Some(base_dir), store_kind));
        }

        entries.sort_by_key(|entry| std::cmp::Reverse(entry.last_modified_ms));
        let max_entries = 20usize.min(entries.len());
        let mut entries = entries.into_iter().take(max_entries).collect::<Vec<_>>();

        let console = PiConsole::new();
        console.render_info("Select a session to resume:");

        let headers = ["#", "Timestamp", "Messages", "Name", "Path"];

        let mut attempts = 0;
        loop {
            if entries.is_empty() {
                console.render_warning("No resumable sessions available. Starting a new session.");
                return Ok(Self::create_with_dir_and_store(Some(base_dir), store_kind));
            }

            let mut rows: Vec<Vec<String>> = Vec::new();
            for (idx, entry) in entries.iter().enumerate() {
                rows.push(vec![
                    format!("{}", idx + 1),
                    entry.timestamp.clone(),
                    entry.message_count.to_string(),
                    entry.name.clone().unwrap_or_else(|| entry.id.clone()),
                    entry.path.display().to_string(),
                ]);
            }
            let row_refs: Vec<Vec<&str>> = rows
                .iter()
                .map(|row| row.iter().map(String::as_str).collect())
                .collect();
            console.render_table(&headers, &row_refs);

            attempts += 1;
            if attempts > 3 {
                console.render_warning("No selection made. Starting a new session.");
                return Ok(Self::create_with_dir_and_store(Some(base_dir), store_kind));
            }

            print!(
                "Enter selection (1-{}, blank to start new): ",
                entries.len()
            );
            let _ = std::io::stdout().flush();

            let input = if let Some(override_input) = picker_input_override.take() {
                override_input
            } else {
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                input
            };
            let input = input.trim();
            if input.is_empty() {
                console.render_info("Starting a new session.");
                return Ok(Self::create_with_dir_and_store(Some(base_dir), store_kind));
            }

            match input.parse::<usize>() {
                Ok(selection) if selection > 0 && selection <= entries.len() => {
                    let selected = &entries[selection - 1];
                    match Self::open(selected.path.to_string_lossy().as_ref()).await {
                        Ok(mut session) => {
                            session.session_dir = Some(base_dir.clone());
                            return Ok(session);
                        }
                        Err(err) => {
                            tracing::warn!(
                                path = %selected.path.display(),
                                error = %err,
                                "Failed to open selected session while resuming"
                            );
                            prune_session_index_path(
                                &session_index,
                                &selected.path,
                                "Failed to prune unreadable selected session after picker open failure",
                            );
                            entries.remove(selection - 1);

                            if is_interactive {
                                console.render_warning(
                                    "Selected session could not be opened. Pick another session.",
                                );
                                continue;
                            }

                            console.render_warning(
                                "Selected session could not be opened. Starting a new session.",
                            );
                            return Ok(Self::create_with_dir_and_store(
                                Some(base_dir.clone()),
                                store_kind,
                            ));
                        }
                    }
                }
                _ => {
                    console.render_warning("Invalid selection. Try again.");
                }
            }
        }
    }

    /// Create an in-memory (ephemeral) session.
    pub fn in_memory() -> Self {
        Self {
            header: SessionHeader::new(),
            entries: Vec::new(),
            path: None,
            leaf_id: None,
            session_dir: None,
            store_kind: SessionStoreKind::Jsonl,
            entry_ids: HashSet::new(),
            is_linear: true,
            entry_index: HashMap::new(),
            cached_message_count: 0,
            cached_name: None,
            autosave_queue: AutosaveQueue::new(),
            autosave_durability: AutosaveDurabilityMode::from_env(),
            persisted_entry_count: Arc::new(AtomicUsize::new(0)),
            header_dirty: false,
            appends_since_checkpoint: 0,
            v2_sidecar_root: None,
            v2_partial_hydration: false,
            v2_resume_mode: None,
            v2_sidecar_stale: false,
            v2_message_count_offset: 0,
        }
    }

    /// Create a new session.
    pub fn create() -> Self {
        Self::create_with_dir(None)
    }

    /// Create a new session with an optional base directory override.
    pub fn create_with_dir(session_dir: Option<PathBuf>) -> Self {
        Self::create_with_dir_and_store(session_dir, SessionStoreKind::Jsonl)
    }

    pub fn create_with_dir_and_store(
        session_dir: Option<PathBuf>,
        store_kind: SessionStoreKind,
    ) -> Self {
        let header = SessionHeader::new();
        Self {
            header,
            entries: Vec::new(),
            path: None,
            leaf_id: None,
            session_dir,
            store_kind,
            entry_ids: HashSet::new(),
            is_linear: true,
            entry_index: HashMap::new(),
            cached_message_count: 0,
            cached_name: None,
            autosave_queue: AutosaveQueue::new(),
            autosave_durability: AutosaveDurabilityMode::from_env(),
            persisted_entry_count: Arc::new(AtomicUsize::new(0)),
            header_dirty: false,
            appends_since_checkpoint: 0,
            v2_sidecar_root: None,
            v2_partial_hydration: false,
            v2_resume_mode: None,
            v2_sidecar_stale: false,
            v2_message_count_offset: 0,
        }
    }

    /// Open an existing session.
    pub async fn open(path: &str) -> Result<Self> {
        let (session, diagnostics) = Self::open_with_diagnostics(path).await?;
        for warning in diagnostics.warning_lines() {
            warn!("{warning}");
        }
        Ok(session)
    }

    /// Open an existing session and return diagnostics about any recovered corruption.
    pub async fn open_with_diagnostics(path: &str) -> Result<(Self, SessionOpenDiagnostics)> {
        Self::open_path_with_diagnostics(PathBuf::from(path)).await
    }

    /// Build a bounded, redacted trace bundle for session cold-start phases.
    pub async fn cold_start_trace_bundle(
        path: &Path,
        sessions_root: &Path,
    ) -> Result<SessionColdStartTraceBundle> {
        #[cfg(not(feature = "session-index"))]
        let _ = sessions_root;
        let total_start = Instant::now();
        let mut phases = Vec::with_capacity(4);
        let mut storage = session_cold_start_storage_trace(path);

        let open_start = Instant::now();
        let (session, diagnostics) = Self::open_path_with_diagnostics(path.to_path_buf()).await?;
        let open_elapsed_us = elapsed_us_since(open_start);
        storage.opened_backend = session.opened_storage_backend_for_trace().to_string();
        phases.push(SessionColdStartPhaseTrace {
            name: "session_open".to_string(),
            elapsed_us: open_elapsed_us,
            status: "ok".to_string(),
        });

        #[cfg(feature = "session-index")]
        let (index_refresh, indexed_files_scanned) = {
            let index_start = Instant::now();
            let index_summary =
                SessionIndex::for_sessions_root(sessions_root).refresh_incremental()?;
            phases.push(SessionColdStartPhaseTrace {
                name: "session_index_refresh".to_string(),
                elapsed_us: elapsed_us_since(index_start),
                status: "ok".to_string(),
            });
            (
                SessionColdStartIndexRefreshTrace {
                    scanned_files: index_summary.scanned_files,
                    cache_hit_files: index_summary.reused_files,
                    reused_files: index_summary.reused_files,
                    refreshed_files: index_summary.refreshed_files,
                    pruned_rows: index_summary.pruned_rows,
                    failed_files: index_summary.failed_files,
                },
                index_summary.scanned_files,
            )
        };
        #[cfg(not(feature = "session-index"))]
        let (index_refresh, indexed_files_scanned) = (Default::default(), 0);

        let compaction_start = Instant::now();
        let compaction_scan = session.cold_start_compaction_scan_trace();
        phases.push(SessionColdStartPhaseTrace {
            name: "compaction_scan".to_string(),
            elapsed_us: elapsed_us_since(compaction_start),
            status: "ok".to_string(),
        });

        let first_render_start = Instant::now();
        let first_render = session.cold_start_first_render_trace();
        phases.push(SessionColdStartPhaseTrace {
            name: "first_render_ready".to_string(),
            elapsed_us: elapsed_us_since(first_render_start),
            status: "ok".to_string(),
        });

        let replay_minimization = session.cold_start_replay_minimization_trace(
            &storage,
            indexed_files_scanned,
            &diagnostics,
        );

        let bundle = SessionColdStartTraceBundle {
            schema: SESSION_COLD_START_TRACE_SCHEMA.to_string(),
            session_path_hash: cold_start_hash_path(path),
            storage,
            input: SessionColdStartInputTrace {
                total_entries: session.entries.len(),
                total_messages: session.cached_message_count,
            },
            phases,
            index_refresh,
            open_diagnostics: SessionColdStartOpenDiagnosticsTrace {
                skipped_entries: diagnostics.skipped_entries.len(),
                orphaned_parent_links: diagnostics.orphaned_parent_links.len(),
            },
            replay_minimization,
            compaction_scan,
            first_render,
            bounds: SessionColdStartBoundsTrace {
                max_phase_count: 4,
                raw_path_included: false,
                raw_cwd_included: false,
                raw_message_content_included: false,
            },
            total_elapsed_us: elapsed_us_since(total_start),
        };
        bundle.emit_log();
        Ok(bundle)
    }

    async fn open_path_with_diagnostics(path: PathBuf) -> Result<(Self, SessionOpenDiagnostics)> {
        if !path.exists() {
            return Err(crate::Error::SessionNotFound {
                path: path.display().to_string(),
            });
        }

        if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| matches!(ext, "sqlite"))
        {
            #[cfg(feature = "sqlite-sessions")]
            {
                let session = Self::open_sqlite(&path).await?;
                return Ok((session, SessionOpenDiagnostics::default()));
            }

            #[cfg(not(feature = "sqlite-sessions"))]
            {
                return Err(Error::session(
                    "SQLite session files require building with `--features sqlite-sessions`",
                ));
            }
        }

        // Check for V2 sidecar store — enables O(index+tail) resume.
        if session_store_v2::has_v2_sidecar(&path) {
            let v2_root = session_store_v2::v2_sidecar_path(&path);
            let is_stale = is_v2_sidecar_stale(&path, &v2_root);

            if is_stale {
                tracing::warn!(
                    path = %path.display(),
                    "V2 sidecar is stale (source JSONL newer); skipping V2 resume"
                );
            } else {
                match Self::open_v2_with_diagnostics(&path).await {
                    Ok(result) => return Ok(result),
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "V2 sidecar resume failed, falling back to full JSONL parse"
                        );
                    }
                }
            }
        }

        Self::open_jsonl_with_diagnostics(&path).await
    }

    const fn opened_storage_backend_for_trace(&self) -> &'static str {
        match self.store_kind {
            SessionStoreKind::Jsonl => {
                if self.v2_sidecar_root.is_some() {
                    "v2_sidecar"
                } else {
                    "jsonl"
                }
            }
            #[cfg(feature = "sqlite-sessions")]
            SessionStoreKind::Sqlite => "sqlite",
        }
    }

    fn cold_start_current_path_entries(&self) -> Vec<&SessionEntry> {
        if self.leaf_id.is_none() {
            return Vec::new();
        }
        if self.is_linear {
            return self.entries.iter().collect();
        }
        self.entries_for_current_path()
    }

    fn cold_start_total_entries_and_branch_count(&self) -> (usize, usize) {
        let loaded_summary = self.branch_summary();
        let mut entry_count = loaded_summary.total_entries;
        let mut branch_count = loaded_summary.branch_point_count;

        if let Some(v2_root) = self.v2_sidecar_root.as_ref() {
            if let Ok(store) = SessionStoreV2::create(v2_root, 64 * 1024 * 1024) {
                entry_count =
                    entry_count.max(usize::try_from(store.entry_count()).unwrap_or(usize::MAX));
                if let Ok(Some(manifest)) = store.read_manifest() {
                    branch_count = branch_count.max(
                        usize::try_from(manifest.counters.branches_total).unwrap_or(usize::MAX),
                    );
                    entry_count = entry_count.max(
                        usize::try_from(manifest.counters.entries_total).unwrap_or(usize::MAX),
                    );
                }
            }
        }

        (entry_count, branch_count)
    }

    fn cold_start_replay_minimization_trace(
        &self,
        storage: &SessionColdStartStorageTrace,
        indexed_files_scanned: usize,
        diagnostics: &SessionOpenDiagnostics,
    ) -> SessionReplayMinimizationTrace {
        let path_entries = self.cold_start_current_path_entries();
        let selected_depth = path_entries.len();
        let replayed_entries = path_entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry,
                    SessionEntry::Message(_)
                        | SessionEntry::BranchSummary(_)
                        | SessionEntry::Compaction(_)
                )
            })
            .count();
        let (entry_count, branch_count) = self.cold_start_total_entries_and_branch_count();
        let skipped_sibling_entries = entry_count.saturating_sub(selected_depth);
        let deterministic_steps = selected_depth
            .saturating_add(indexed_files_scanned)
            .saturating_add(diagnostics.skipped_entries.len())
            .saturating_add(diagnostics.orphaned_parent_links.len());
        let opened_backend = storage.opened_backend.as_str();
        let selected_backend = storage.selected_backend.as_str();
        let backend_changed = !matches!(
            (opened_backend, selected_backend),
            ("jsonl", "jsonl") | ("v2_sidecar", "v2_sidecar")
        );

        let fallback_behavior = if !diagnostics.orphaned_parent_links.is_empty() {
            Some("orphaned_parent_links_detected".to_string())
        } else if !diagnostics.skipped_entries.is_empty() {
            Some("corrupt_jsonl_entries_skipped".to_string())
        } else if backend_changed {
            Some(format!(
                "{}_fallback_to_{}",
                storage.selected_backend, storage.opened_backend
            ))
        } else if let Some(reason) = storage.fallback_reason.as_ref() {
            Some(reason.clone())
        } else if matches!(opened_backend, "jsonl") && !storage.v2_sidecar_present {
            Some("jsonl_full_scan_without_sidecar".to_string())
        } else {
            None
        };

        let verdict = if diagnostics.orphaned_parent_links.is_empty()
            && diagnostics.skipped_entries.is_empty()
            && skipped_sibling_entries > 0
            && fallback_behavior.is_none()
        {
            "bounded_selected_branch".to_string()
        } else if fallback_behavior.is_some() {
            "fallback_explicit".to_string()
        } else {
            "linear_or_single_branch".to_string()
        };

        SessionReplayMinimizationTrace {
            schema: SESSION_REPLAY_MINIMIZATION_TRACE_SCHEMA.to_string(),
            branch_count,
            entry_count,
            selected_depth,
            scanned_files: indexed_files_scanned,
            replayed_entries,
            skipped_sibling_entries,
            deterministic_steps,
            fallback_behavior,
            verdict,
        }
    }

    fn cold_start_compaction_scan_trace(&self) -> SessionColdStartCompactionTrace {
        let path_entries = self.cold_start_current_path_entries();
        let mut compaction_entries = 0usize;
        let mut latest = None;

        for (idx, entry) in path_entries.iter().enumerate() {
            if let SessionEntry::Compaction(compaction) = entry {
                compaction_entries = compaction_entries.saturating_add(1);
                latest = Some((idx, compaction.first_kept_entry_id.clone()));
            }
        }

        let (latest_compaction_index_from_end, first_kept_entry_found) =
            if let Some((idx, first_kept_entry_id)) = latest {
                let found = path_entries.iter().any(|entry| {
                    entry
                        .base_id()
                        .is_some_and(|entry_id| entry_id.eq(&first_kept_entry_id))
                });
                (
                    Some(path_entries.len().saturating_sub(idx.saturating_add(1))),
                    Some(found),
                )
            } else {
                (None, None)
            };

        SessionColdStartCompactionTrace {
            scanned_entries: path_entries.len(),
            compaction_entries,
            latest_compaction_present: latest_compaction_index_from_end.is_some(),
            latest_compaction_index_from_end,
            first_kept_entry_found,
        }
    }

    fn cold_start_first_render_trace(&self) -> SessionColdStartFirstRenderTrace {
        let path_entries = self.cold_start_current_path_entries();
        let mut trace = SessionColdStartFirstRenderTrace {
            current_path_entries: path_entries.len(),
            projected_messages: 0,
            user_messages: 0,
            assistant_messages: 0,
            tool_messages: 0,
            system_messages: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            total_tokens: 0,
            ready: true,
        };

        for entry in path_entries {
            let SessionEntry::Message(message_entry) = entry else {
                continue;
            };

            match &message_entry.message {
                SessionMessage::User { .. } => {
                    trace.projected_messages = trace.projected_messages.saturating_add(1);
                    trace.user_messages = trace.user_messages.saturating_add(1);
                }
                SessionMessage::Assistant { message } => {
                    trace.projected_messages = trace.projected_messages.saturating_add(1);
                    trace.assistant_messages = trace.assistant_messages.saturating_add(1);
                    trace.input_tokens = trace.input_tokens.saturating_add(message.usage.input);
                    trace.output_tokens = trace.output_tokens.saturating_add(message.usage.output);
                    trace.cache_read_tokens = trace
                        .cache_read_tokens
                        .saturating_add(message.usage.cache_read);
                    trace.cache_write_tokens = trace
                        .cache_write_tokens
                        .saturating_add(message.usage.cache_write);
                    trace.total_tokens = trace
                        .total_tokens
                        .saturating_add(message.usage.total_tokens);
                }
                SessionMessage::ToolResult { .. } | SessionMessage::BashExecution { .. } => {
                    trace.projected_messages = trace.projected_messages.saturating_add(1);
                    trace.tool_messages = trace.tool_messages.saturating_add(1);
                }
                SessionMessage::Custom { display: true, .. } => {
                    trace.projected_messages = trace.projected_messages.saturating_add(1);
                    trace.system_messages = trace.system_messages.saturating_add(1);
                }
                SessionMessage::Custom { display: false, .. }
                | SessionMessage::CompactionSummary { .. }
                | SessionMessage::BranchSummary { .. } => {}
            }
        }

        trace
    }

    /// Open a session from an already-open V2 store with an explicit read mode.
    pub fn open_from_v2(
        store: &SessionStoreV2,
        header: SessionHeader,
        mode: V2OpenMode,
    ) -> Result<(Self, SessionOpenDiagnostics)> {
        header
            .validate()
            .map_err(|reason| crate::Error::session(format!("Invalid session header: {reason}")))?;
        let (header, normalized_header_dirty) = normalize_loaded_header(header);
        let frames = match mode {
            V2OpenMode::Full => store.read_all_entries()?,
            V2OpenMode::ActivePath => match store.head() {
                Some(head) => store.read_active_path(&head.entry_id)?,
                None => Vec::new(),
            },
            V2OpenMode::Tail(count) => store.read_tail_entries(count)?,
        };

        let mut diagnostics = SessionOpenDiagnostics::default();
        let mut entries = Vec::with_capacity(frames.len());
        for frame in &frames {
            match session_store_v2::frame_to_session_entry(frame) {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    diagnostics.skipped_entries.push(SessionOpenSkippedEntry {
                        line_number: usize::try_from(frame.entry_seq).unwrap_or(0),
                        error: e.to_string(),
                    });
                }
            }
        }

        let finalized = finalize_loaded_entries(&mut entries);
        for orphan in &finalized.orphans {
            diagnostics
                .orphaned_parent_links
                .push(SessionOpenOrphanedParentLink {
                    entry_id: orphan.0.clone(),
                    missing_parent_id: orphan.1.clone(),
                });
        }

        let mut v2_message_count_offset = 0;
        if matches!(mode, V2OpenMode::Tail(_) | V2OpenMode::ActivePath) {
            if let Ok(Some(total)) = total_v2_message_count(store) {
                let loaded = finalized.message_count;
                v2_message_count_offset = total.saturating_sub(loaded);
            }
        }

        let entry_count = entries.len();
        let natural_leaf_id = finalized.leaf_id.clone();
        let leaf_id =
            resolve_loaded_leaf_id(&header, natural_leaf_id.clone(), &finalized.entry_index);
        Ok((
            Self {
                header,
                entries,
                path: None,
                leaf_id: leaf_id.clone(),
                session_dir: None,
                store_kind: SessionStoreKind::Jsonl,
                entry_ids: finalized.entry_ids,
                is_linear: finalized.is_linear && leaf_id.eq(&natural_leaf_id),
                entry_index: finalized.entry_index,
                cached_message_count: finalized
                    .message_count
                    .saturating_add(v2_message_count_offset),
                cached_name: finalized.name,
                autosave_queue: AutosaveQueue::new(),
                autosave_durability: AutosaveDurabilityMode::from_env(),
                persisted_entry_count: Arc::new(AtomicUsize::new(entry_count)),
                header_dirty: normalized_header_dirty,
                appends_since_checkpoint: 0,
                v2_sidecar_root: None,
                v2_partial_hydration: !matches!(mode, V2OpenMode::Full),
                v2_resume_mode: Some(mode),
                v2_sidecar_stale: false,
                v2_message_count_offset,
            },
            diagnostics,
        ))
    }

    /// Open using the V2 sidecar store (async wrapper around blocking read).
    async fn open_v2_with_diagnostics(path: &Path) -> Result<(Self, SessionOpenDiagnostics)> {
        let path_buf = path.to_path_buf();
        let (tx, mut rx) = oneshot::channel();
        let cx = AgentCx::for_request();
        let worker_cx = cx.cx().clone();

        let handle = thread::spawn(move || {
            let res = crate::session::open_from_v2_store_blocking(path_buf);
            let _ = tx.send(&worker_cx, res);
        });

        let recv_result = rx.recv(cx.cx()).await;
        finish_worker_result(handle, recv_result, "V2 open task cancelled")
    }

    async fn open_jsonl_with_diagnostics(path: &Path) -> Result<(Self, SessionOpenDiagnostics)> {
        let path_buf = path.to_path_buf();
        let (tx, mut rx) = oneshot::channel();
        let cx = AgentCx::for_request();
        let worker_cx = cx.cx().clone();

        let handle = thread::spawn(move || {
            let res = open_jsonl_blocking(path_buf);
            let _ = tx.send(&worker_cx, res);
        });

        let recv_result = rx.recv(cx.cx()).await;
        finish_worker_result(handle, recv_result, "Open task cancelled")
    }

    #[cfg(feature = "sqlite-sessions")]
    async fn open_sqlite(path: &Path) -> Result<Self> {
        let (header, mut entries) = crate::session_sqlite::load_session(path).await?;
        let (header, normalized_header_dirty) = normalize_loaded_header(header);
        let finalized = finalize_loaded_entries(&mut entries);
        let entry_count = entries.len();
        let natural_leaf_id = finalized.leaf_id.clone();
        let leaf_id =
            resolve_loaded_leaf_id(&header, natural_leaf_id.clone(), &finalized.entry_index);

        Ok(Self {
            header,
            entries,
            path: Some(path.to_path_buf()),
            leaf_id: leaf_id.clone(),
            session_dir: None,
            store_kind: SessionStoreKind::Sqlite,
            entry_ids: finalized.entry_ids,
            is_linear: finalized.is_linear && leaf_id.eq(&natural_leaf_id),
            entry_index: finalized.entry_index,
            cached_message_count: finalized.message_count,
            cached_name: finalized.name,
            autosave_queue: AutosaveQueue::new(),
            autosave_durability: AutosaveDurabilityMode::from_env(),
            persisted_entry_count: Arc::new(AtomicUsize::new(entry_count)),
            header_dirty: normalized_header_dirty,
            appends_since_checkpoint: 0,
            v2_sidecar_root: None,
            v2_partial_hydration: false,
            v2_resume_mode: None,
            v2_sidecar_stale: false,
            v2_message_count_offset: 0,
        })
    }

    /// Continue the most recent session.
    #[cfg(feature = "session-index")]
    pub async fn continue_recent_in_dir(
        override_dir: Option<&Path>,
        config: &Config,
    ) -> Result<Self> {
        let store_kind = SessionStoreKind::from_config(config);
        let base_dir = override_dir.map_or_else(Config::sessions_dir, PathBuf::from);
        let cwd = std::env::current_dir()?;
        let cwd_display = cwd.display().to_string();
        let encoded_cwd = encode_cwd(&cwd);
        let project_session_dir = base_dir.join(&encoded_cwd);
        let project_session_dir_missing = indexed_session_path_is_missing(&project_session_dir);

        // Prefer the session index for fast lookup.
        let base_dir_clone = base_dir.clone();
        let cwd_display_clone = cwd_display.clone();
        let (tx, mut rx) = oneshot::channel();

        let handle = thread::spawn(move || {
            let index = SessionIndex::for_sessions_root(&base_dir_clone);
            let mut indexed_sessions = index
                .list_sessions(Some(&cwd_display_clone))
                .unwrap_or_default();

            if indexed_sessions.is_empty() && index.reindex_all().is_ok() {
                indexed_sessions = index
                    .list_sessions(Some(&cwd_display_clone))
                    .unwrap_or_default();
            }
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), Ok(indexed_sessions));
        });

        let cx = AgentCx::for_request();
        let recv_result = rx.recv(cx.cx()).await;
        let indexed_meta =
            finish_worker_result(handle, recv_result, "Recent session index task cancelled")
                .unwrap_or_default();

        let index = SessionIndex::for_sessions_root(&base_dir);
        let (indexed_sessions, missing_paths) = split_indexed_session_entries(indexed_meta);
        for path in &missing_paths {
            prune_session_index_path(
                &index,
                path,
                "Failed to prune missing session from index during recent-session refresh",
            );
        }

        if project_session_dir_missing {
            return Ok(Self::create_with_dir_and_store(Some(base_dir), store_kind));
        }

        let scanned = scan_sessions_on_disk(&project_session_dir, indexed_sessions.clone()).await?;

        let mut by_path: HashMap<PathBuf, SessionPickEntry> = HashMap::new();
        for entry in indexed_sessions {
            by_path.insert(entry.path.clone(), entry);
        }
        for path in &scanned.failed_paths {
            prune_session_index_path(
                &index,
                path,
                "Failed to prune unreadable session from index during recent-session refresh",
            );
            by_path.remove(path);
        }
        refresh_session_index_entries(
            &index,
            &scanned.refreshed_entries,
            "Failed to refresh session metadata in index during recent-session refresh",
        );
        merge_scanned_session_entries(&mut by_path, scanned.entries);

        let mut candidates = by_path.into_values().collect::<Vec<_>>();
        candidates.sort_by_key(|entry| std::cmp::Reverse(entry.last_modified_ms));

        for entry in &candidates {
            match Self::open(entry.path.to_string_lossy().as_ref()).await {
                Ok(mut session) => {
                    session.session_dir = Some(base_dir.clone());
                    return Ok(session);
                }
                Err(err) => {
                    tracing::warn!(
                        path = %entry.path.display(),
                        error = %err,
                        "Skipping unreadable session candidate while continuing"
                    );
                    prune_session_index_path(
                        &index,
                        &entry.path,
                        "Failed to prune unreadable session after resume candidate open failure",
                    );
                }
            }
        }

        Ok(Self::create_with_dir_and_store(Some(base_dir), store_kind))
    }

    /// Save the session to disk.
    pub async fn save(&mut self) -> Result<()> {
        let ticket = self
            .autosave_queue
            .begin_flush(AutosaveFlushTrigger::Manual);
        let result = self.save_inner().await;
        if let Some(ticket) = ticket {
            self.autosave_queue.finish_flush(ticket, result.is_ok());
        }
        result
    }

    /// Flush queued autosave mutations using the requested trigger.
    ///
    /// This is the write-behind entry point: no-op when there are no pending
    /// mutations, and one persistence operation for all coalesced mutations when
    /// pending work exists.
    pub async fn flush_autosave(&mut self, trigger: AutosaveFlushTrigger) -> Result<()> {
        let Some(ticket) = self.autosave_queue.begin_flush(trigger) else {
            return Ok(());
        };
        let result = self.save_inner().await;
        self.autosave_queue.finish_flush(ticket, result.is_ok());
        result
    }

    /// Final shutdown flush respecting the configured durability mode.
    pub async fn flush_autosave_on_shutdown(&mut self) -> Result<()> {
        if !self.autosave_durability.should_flush_on_shutdown() {
            return Ok(());
        }
        let result = self.flush_autosave(AutosaveFlushTrigger::Shutdown).await;
        if result.is_err() && self.autosave_durability.best_effort_on_shutdown() {
            if let Err(err) = &result {
                tracing::warn!(error = %err, "best-effort autosave flush failed during shutdown");
            }
            return Ok(());
        }
        result
    }

    /// Current autosave queue and lifecycle counters for observability.
    pub const fn autosave_metrics(&self) -> AutosaveQueueMetrics {
        self.autosave_queue.metrics()
    }

    pub const fn autosave_durability_mode(&self) -> AutosaveDurabilityMode {
        self.autosave_durability
    }

    pub const fn set_autosave_durability_mode(&mut self, mode: AutosaveDurabilityMode) {
        self.autosave_durability = mode;
    }

    #[cfg(test)]
    fn set_autosave_queue_limit_for_test(&mut self, max_pending_mutations: usize) {
        self.autosave_queue = AutosaveQueue::with_limit(max_pending_mutations);
    }

    #[cfg(test)]
    const fn set_autosave_durability_for_test(&mut self, mode: AutosaveDurabilityMode) {
        self.autosave_durability = mode;
    }

    /// Ensure a lazily hydrated V2 session is fully hydrated before persisting.
    ///
    /// Partial V2 hydration intentionally loads only a subset of entries for fast
    /// resume. Before any save path that could trigger a full JSONL rewrite, we
    /// must rehydrate all V2 entries to preserve non-active branches.
    fn ensure_full_v2_hydration_before_save(&mut self) -> Result<()> {
        if !self.v2_partial_hydration {
            return Ok(());
        }

        let Some(v2_root) = self.v2_sidecar_root.clone() else {
            tracing::warn!(
                "session marked as partially hydrated from V2 but sidecar root is unavailable; disabling partial flag"
            );
            self.v2_partial_hydration = false;
            return Ok(());
        };

        let pending_start = self
            .persisted_entry_count
            .load(Ordering::SeqCst)
            .min(self.entries.len());
        let previous_mode = self.v2_resume_mode;

        let use_jsonl_rehydration = self
            .path
            .as_ref()
            .is_some_and(|path| self.v2_sidecar_stale || is_v2_sidecar_stale(path, &v2_root));
        let (fully_hydrated, diagnostics, rehydration_source) = if use_jsonl_rehydration {
            let path = self.path.clone().ok_or_else(|| {
                Error::session("missing JSONL path while rehydrating stale V2 session")
            })?;
            let (session, diagnostics) = open_jsonl_blocking(path)?;
            (session, diagnostics, "jsonl")
        } else {
            let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;
            let (session, diagnostics) =
                Self::open_from_v2(&store, self.header.clone(), V2OpenMode::Full)?;
            (session, diagnostics, "v2")
        };
        if !diagnostics.skipped_entries.is_empty() || !diagnostics.orphaned_parent_links.is_empty()
        {
            tracing::error!(
                skipped_entries = diagnostics.skipped_entries.len(),
                orphaned_parent_links = diagnostics.orphaned_parent_links.len(),
                rehydration_source,
                "full V2 rehydration before save failed integrity check; aborting save to prevent data loss"
            );
            return Err(Error::session(format!(
                "V2 rehydration failed with {} skipped entries and {} orphaned links",
                diagnostics.skipped_entries.len(),
                diagnostics.orphaned_parent_links.len()
            )));
        }

        // Extract pending in-memory entries by moving them out of `self.entries`
        // only after full hydration succeeds, preserving fail-safe behavior on
        // early-return errors and avoiding per-entry clone cost.
        let pending_entries = if pending_start >= self.entries.len() {
            Vec::new()
        } else {
            self.entries.split_off(pending_start)
        };

        let persisted_entry_count = fully_hydrated.entries.len();
        let mut merged_entries = fully_hydrated.entries;
        merged_entries.extend(pending_entries);

        let finalized = finalize_loaded_entries(&mut merged_entries);
        self.entries = merged_entries;
        self.leaf_id = finalized.leaf_id;
        self.entry_ids = finalized.entry_ids;
        self.is_linear = finalized.is_linear;
        self.entry_index = finalized.entry_index;
        self.cached_message_count = finalized.message_count;
        self.cached_name = finalized.name;
        self.persisted_entry_count
            .store(persisted_entry_count, Ordering::SeqCst);
        self.v2_partial_hydration = false;
        self.v2_resume_mode = Some(V2OpenMode::Full);
        self.v2_sidecar_stale = false;
        self.v2_message_count_offset = 0;

        tracing::debug!(
            previous_mode = ?previous_mode,
            rehydration_source,
            persisted_entry_count,
            pending_entries = self.entries.len().saturating_sub(persisted_entry_count),
            "fully rehydrated V2 session before save"
        );

        Ok(())
    }

    /// Returns `true` when a full rewrite is required instead of incremental append.
    fn should_full_rewrite(&self) -> bool {
        let persisted_count = self.persisted_entry_count.load(Ordering::SeqCst);

        // First save — no file exists yet.
        if persisted_count == 0 {
            return true;
        }
        // If the backing file disappeared between saves, recover by rewriting
        // the full in-memory session instead of attempting an append.
        if self
            .path
            .as_ref()
            .is_some_and(|path| path.try_exists().is_ok_and(|exists| !exists))
        {
            return true;
        }
        // Header was modified since last save.
        if self.header_dirty {
            return true;
        }
        // Periodic checkpoint to clean up accumulated partial writes.
        if self.appends_since_checkpoint >= compaction_checkpoint_interval() {
            return true;
        }
        // Defensive: if persisted count somehow exceeds entries, force full rewrite.
        if persisted_count > self.entries.len() {
            return true;
        }
        false
    }

    /// Save the session to disk.
    #[allow(clippy::too_many_lines)]
    async fn save_inner(&mut self) -> Result<()> {
        self.ensure_entry_ids();

        let store_kind = match self
            .path
            .as_ref()
            .and_then(|path| path.extension().and_then(|ext| ext.to_str()))
        {
            Some("jsonl") => SessionStoreKind::Jsonl,
            Some("sqlite") => {
                #[cfg(feature = "sqlite-sessions")]
                {
                    SessionStoreKind::Sqlite
                }

                #[cfg(not(feature = "sqlite-sessions"))]
                {
                    return Err(Error::session(
                        "SQLite session files require building with `--features sqlite-sessions`",
                    ));
                }
            }
            _ => self.store_kind,
        };

        if self.path.is_none() {
            // Create a new path
            let base_dir = self
                .session_dir
                .clone()
                .unwrap_or_else(Config::sessions_dir);
            let cwd = if self.header.cwd.trim().is_empty() {
                std::env::current_dir()?
            } else {
                let configured_cwd = PathBuf::from(self.header.cwd.trim());
                if configured_cwd.is_absolute() {
                    configured_cwd
                } else {
                    std::env::current_dir()?.join(configured_cwd)
                }
            };
            let encoded_cwd = encode_cwd(&cwd);
            let project_session_dir = base_dir.join(&encoded_cwd);

            asupersync::fs::create_dir_all(&project_session_dir).await?;

            let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S%.3fZ");
            // Robust against malformed/legacy session ids: keep a short, filename-safe suffix.
            let short_id = {
                let prefix: String = self
                    .header
                    .id
                    .chars()
                    .take(8)
                    .map(|ch| {
                        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                            ch
                        } else {
                            '_'
                        }
                    })
                    .collect();
                if prefix.trim_matches('_').is_empty() {
                    "session".to_string()
                } else {
                    prefix
                }
            };
            let filename = format!("{}_{}.{}", timestamp, short_id, store_kind.extension());
            self.path = Some(project_session_dir.join(filename));
        }

        // Persist a repaired id for legacy or manually corrupted in-memory headers.
        // The filename fallback above still keeps empty ids on-disk-path-safe.
        if self.header.id.trim().is_empty() {
            self.header.id = uuid::Uuid::new_v4().to_string();
            self.header_dirty = true;
        }
        let desired_leaf_override = self.persisted_leaf_override();
        if !self.header.current_leaf.eq(&desired_leaf_override) {
            self.header.current_leaf = desired_leaf_override;
            self.header_dirty = true;
        }
        self.header
            .validate()
            .map_err(|reason| Error::session(format!("Invalid session header: {reason}")))?;

        let session_dir_clone = self.session_dir.clone();
        let path = self.path.clone().ok_or_else(|| {
            Error::session("Session path not set - cannot save session".to_string())
        })?;
        let path_clone = path.clone();

        match store_kind {
            SessionStoreKind::Jsonl => {
                let sessions_root = session_dir_clone.unwrap_or_else(Config::sessions_dir);

                if self.should_full_rewrite() {
                    if self.v2_partial_hydration {
                        self.ensure_full_v2_hydration_before_save()?;
                    }
                    // Gap C: use incrementally maintained stats instead of O(n) scan.
                    // === Full rewrite path (first save, header change, checkpoint) ===
                    let header_snapshot = self.header.clone();
                    let entries_to_save = self.entries.clone();
                    let persisted_entry_count = self.persisted_entry_count.load(Ordering::SeqCst);
                    let header_dirty = self.header_dirty;
                    let path_for_task = path_clone.clone();
                    let sessions_root_for_task = sessions_root.clone();
                    let (saved_header, saved_entries) =
                        asupersync::runtime::spawn_blocking(move || {
                            save_jsonl_full_rewrite_blocking(
                                &path_for_task,
                                &sessions_root_for_task,
                                &header_snapshot,
                                &entries_to_save,
                                persisted_entry_count,
                                header_dirty,
                            )
                        })
                        .await?;

                    let previous_leaf = self.leaf_id.clone();
                    self.header = saved_header;
                    self.entries = saved_entries;
                    let finalized = finalize_loaded_entries(&mut self.entries);
                    self.entry_ids = finalized.entry_ids;
                    self.entry_index = finalized.entry_index;
                    self.cached_message_count = finalized
                        .message_count
                        .saturating_add(self.v2_message_count_offset);
                    self.cached_name = finalized.name;
                    self.leaf_id = previous_leaf
                        .filter(|id| self.entry_index.contains_key(id))
                        .or_else(|| finalized.leaf_id.clone());
                    self.is_linear = finalized.is_linear && self.leaf_id.eq(&finalized.leaf_id);
                    self.persisted_entry_count
                        .store(self.entries.len(), Ordering::SeqCst);
                    self.header_dirty = false;
                    self.appends_since_checkpoint = 0;
                    self.v2_sidecar_stale = self.v2_sidecar_root.is_some();
                } else {
                    let message_count = self.cached_message_count;
                    // === Incremental append path ===
                    let new_start = self.persisted_entry_count.load(Ordering::SeqCst);
                    if new_start < self.entries.len() {
                        let session_name = self.cached_name.clone();
                        // Pre-serialize new entries into a single buffer (typically 1-3 entries).
                        let new_entries = &self.entries[new_start..];
                        // Scale buffer reservation from observed on-disk average entry size to
                        // avoid repeated growth/copy when appending large entries.
                        let estimated_entry_bytes = asupersync::fs::metadata(&path_clone)
                            .await
                            .ok()
                            .and_then(|meta| usize::try_from(meta.len()).ok())
                            .map_or(512, |file_bytes| {
                                let avg = file_bytes / new_start.max(1);
                                avg.clamp(512, 256 * 1024)
                            });
                        let mut serialized_buf = Vec::with_capacity(
                            new_entries
                                .len()
                                .saturating_mul(estimated_entry_bytes.saturating_add(1)),
                        );
                        for entry in new_entries {
                            serde_json::to_writer(&mut serialized_buf, entry)?;
                            serialized_buf.push(b'\n');
                        }
                        let new_count = self.entries.len();

                        let header_snapshot = self.header.clone();
                        let path_for_task = path_clone.clone();
                        let sessions_root_for_task = sessions_root.clone();
                        asupersync::runtime::spawn_blocking(move || {
                            append_jsonl_entries_blocking(
                                &path_for_task,
                                &sessions_root_for_task,
                                &header_snapshot,
                                &serialized_buf,
                                message_count,
                                session_name,
                            )
                        })
                        .await?;

                        self.persisted_entry_count
                            .store(new_count, Ordering::SeqCst);
                        self.appends_since_checkpoint += 1;
                        self.v2_sidecar_stale = self.v2_sidecar_root.is_some();
                    }
                    // No new entries → no-op, nothing to write.
                }
            }
            #[cfg(feature = "sqlite-sessions")]
            SessionStoreKind::Sqlite => {
                let message_count = self.cached_message_count;
                let session_name = self.cached_name.clone();

                if self.should_full_rewrite() {
                    // === Full rewrite path (first save, header change, checkpoint) ===
                    crate::session_sqlite::save_session(&path_clone, &self.header, &self.entries)
                        .await?;
                    self.persisted_entry_count
                        .store(self.entries.len(), Ordering::SeqCst);
                    self.header_dirty = false;
                    self.appends_since_checkpoint = 0;
                } else {
                    // === Incremental append path ===
                    let new_start = self.persisted_entry_count.load(Ordering::SeqCst);
                    if new_start < self.entries.len() {
                        crate::session_sqlite::append_entries(
                            &path_clone,
                            &self.entries[new_start..],
                            new_start,
                            message_count,
                            session_name.as_deref(),
                        )
                        .await?;
                        self.persisted_entry_count
                            .store(self.entries.len(), Ordering::SeqCst);
                        self.appends_since_checkpoint += 1;
                    }
                    // No new entries → no-op, nothing to write.
                }

                #[cfg(feature = "session-index")]
                {
                    let sessions_root = session_dir_clone.unwrap_or_else(Config::sessions_dir);
                    enqueue_session_index_snapshot_update(
                        &sessions_root,
                        &path_clone,
                        &self.header,
                        message_count,
                        session_name,
                    );
                }
            }
        }
        Ok(())
    }

    const fn enqueue_autosave_mutation(&mut self, kind: AutosaveMutationKind) {
        self.autosave_queue.enqueue_mutation(kind);
    }

    fn latest_model_change_for_current_path(&self) -> Option<(String, String)> {
        for entry in self.entries_for_current_path().iter().rev() {
            if let SessionEntry::ModelChange(change) = entry {
                return Some((change.provider.clone(), change.model_id.clone()));
            }
        }
        None
    }

    fn latest_thinking_level_for_current_path(&self) -> Option<String> {
        for entry in self.entries_for_current_path().iter().rev() {
            if let SessionEntry::ThinkingLevelChange(change) = entry {
                return Some(change.thinking_level.clone());
            }
        }
        None
    }

    pub fn effective_model_for_current_path(&self) -> Option<(String, String)> {
        // If there's an explicit model change on the current path, use it
        if let Some(model) = self.latest_model_change_for_current_path() {
            return Some(model);
        }

        // If other branches have model changes, we only inherit EXPLICIT fallbacks.
        // We do NOT inherit `self.header.provider` because that reflects another branch's tip.
        if self.has_any_model_change() {
            return self
                .header
                .fallback_provider
                .clone()
                .zip(self.header.fallback_model_id.clone());
        }

        self.header
            .provider
            .clone()
            .zip(self.header.model_id.clone())
    }

    pub fn effective_thinking_level_for_current_path(&self) -> Option<String> {
        // If there's an explicit thinking level change on the current path, use it
        if let Some(level) = self.latest_thinking_level_for_current_path() {
            return Some(level);
        }

        // If other branches have thinking level changes, we only inherit EXPLICIT fallbacks.
        // We do NOT inherit `self.header.thinking_level` because that reflects another branch's tip.
        if self.has_any_thinking_level_change() {
            return self.header.fallback_thinking_level.clone();
        }

        self.header.thinking_level.clone()
    }

    fn has_any_model_change(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry, SessionEntry::ModelChange(_)))
    }

    fn has_any_thinking_level_change(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry, SessionEntry::ThinkingLevelChange(_)))
    }

    fn persisted_leaf_override(&self) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }

        match (
            self.leaf_id.as_deref(),
            self.entries
                .last()
                .and_then(SessionEntry::base_id)
                .map(String::as_str),
        ) {
            (None, _) => Some(ROOT_LEAF_OVERRIDE_SENTINEL.to_string()),
            (Some(current), Some(natural_tip)) if current.eq(natural_tip) => None,
            (Some(current), _) => Some(current.to_string()),
        }
    }

    fn sync_navigation_state_to_header(&mut self) {
        let mut changed = false;

        let desired_leaf_override = self.persisted_leaf_override();
        if !self.header.current_leaf.eq(&desired_leaf_override) {
            self.header.current_leaf = desired_leaf_override;
            changed = true;
        }

        if let Some((provider, model_id)) = self.effective_model_for_current_path() {
            if !self
                .header
                .provider
                .as_deref()
                .is_some_and(|current| current.eq(provider.as_str()))
                || !self
                    .header
                    .model_id
                    .as_deref()
                    .is_some_and(|current| current.eq(model_id.as_str()))
            {
                self.header.provider = Some(provider);
                self.header.model_id = Some(model_id);
                changed = true;
            }
        } else if self.has_any_model_change()
            && (self.header.provider.is_some() || self.header.model_id.is_some())
        {
            self.header.provider = None;
            self.header.model_id = None;
            changed = true;
        }

        if let Some(thinking_level) = self.effective_thinking_level_for_current_path() {
            if !self
                .header
                .thinking_level
                .as_deref()
                .is_some_and(|current| current.eq(thinking_level.as_str()))
            {
                self.header.thinking_level = Some(thinking_level);
                changed = true;
            }
        } else if self.has_any_thinking_level_change() && self.header.thinking_level.is_some() {
            self.header.thinking_level = None;
            changed = true;
        }

        if changed {
            self.header_dirty = true;
            self.enqueue_autosave_mutation(AutosaveMutationKind::Metadata);
        }
    }

    fn clear_persisted_leaf_override_after_append(&mut self) {
        let desired_leaf_override = self.persisted_leaf_override();
        if !self.header.current_leaf.eq(&desired_leaf_override) {
            self.header.current_leaf = desired_leaf_override;
            self.header_dirty = true;
        }
    }

    /// Append a session message entry.
    pub fn append_message(&mut self, message: SessionMessage) -> String {
        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::Message(MessageEntry { base, message });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        self.entry_index.insert(id.clone(), self.entries.len() - 1);
        self.entry_ids.insert(id.clone());
        self.cached_message_count += 1;
        self.clear_persisted_leaf_override_after_append();
        self.enqueue_autosave_mutation(AutosaveMutationKind::Message);
        id
    }

    /// Append a message from the model message types.
    pub fn append_model_message(&mut self, message: Message) -> String {
        self.append_message(SessionMessage::from(message))
    }

    pub fn append_model_change(&mut self, provider: String, model_id: String) -> String {
        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::ModelChange(ModelChangeEntry {
            base,
            provider,
            model_id,
        });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        self.entry_index.insert(id.clone(), self.entries.len() - 1);
        self.entry_ids.insert(id.clone());
        self.clear_persisted_leaf_override_after_append();
        self.enqueue_autosave_mutation(AutosaveMutationKind::Metadata);
        id
    }

    pub fn append_thinking_level_change(&mut self, thinking_level: String) -> String {
        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::ThinkingLevelChange(ThinkingLevelChangeEntry {
            base,
            thinking_level,
        });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        self.entry_index.insert(id.clone(), self.entries.len() - 1);
        self.entry_ids.insert(id.clone());
        self.clear_persisted_leaf_override_after_append();
        self.enqueue_autosave_mutation(AutosaveMutationKind::Metadata);
        id
    }

    pub fn append_session_info(&mut self, name: Option<String>) -> String {
        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        if name.is_some() {
            self.cached_name.clone_from(&name);
        }
        let entry = SessionEntry::SessionInfo(SessionInfoEntry { base, name });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        self.entry_index.insert(id.clone(), self.entries.len() - 1);
        self.entry_ids.insert(id.clone());
        self.clear_persisted_leaf_override_after_append();
        self.enqueue_autosave_mutation(AutosaveMutationKind::Metadata);
        id
    }

    /// Append a custom entry (extension state, etc).
    pub fn append_custom_entry(
        &mut self,
        custom_type: String,
        data: Option<serde_json::Value>,
    ) -> String {
        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::Custom(CustomEntry {
            base,
            custom_type,
            data,
        });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        self.entry_index.insert(id.clone(), self.entries.len() - 1);
        self.entry_ids.insert(id.clone());
        self.clear_persisted_leaf_override_after_append();
        self.enqueue_autosave_mutation(AutosaveMutationKind::Metadata);
        id
    }

    pub fn append_bash_execution(
        &mut self,
        command: String,
        output: String,
        exit_code: i32,
        cancelled: bool,
        truncated: bool,
        full_output_path: Option<String>,
    ) -> String {
        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::Message(MessageEntry {
            base,
            message: SessionMessage::BashExecution {
                command,
                output,
                exit_code,
                cancelled: Some(cancelled),
                truncated: Some(truncated),
                full_output_path,
                timestamp: Some(chrono::Utc::now().timestamp_millis()),
                extra: HashMap::new(),
            },
        });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        self.entry_index.insert(id.clone(), self.entries.len() - 1);
        self.entry_ids.insert(id.clone());
        self.cached_message_count += 1;
        self.clear_persisted_leaf_override_after_append();
        self.enqueue_autosave_mutation(AutosaveMutationKind::Message);
        id
    }

    /// Get the current session name from the cached value (Gap C).
    pub fn get_name(&self) -> Option<String> {
        self.cached_name.clone()
    }

    /// Set the session name by appending a `SessionInfo` entry.
    pub fn set_name(&mut self, name: &str) -> String {
        self.append_session_info(Some(name.to_string()))
    }

    pub fn append_compaction(
        &mut self,
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
        details: Option<Value>,
        from_hook: Option<bool>,
    ) -> String {
        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::Compaction(CompactionEntry {
            base,
            summary,
            first_kept_entry_id,
            tokens_before,
            details,
            from_hook,
        });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        self.entry_index.insert(id.clone(), self.entries.len() - 1);
        self.entry_ids.insert(id.clone());
        self.clear_persisted_leaf_override_after_append();
        self.enqueue_autosave_mutation(AutosaveMutationKind::Metadata);
        id
    }

    pub fn append_branch_summary(
        &mut self,
        from_id: String,
        summary: String,
        details: Option<Value>,
        from_hook: Option<bool>,
    ) -> String {
        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::BranchSummary(BranchSummaryEntry {
            base,
            from_id,
            summary,
            details,
            from_hook,
        });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        self.entry_index.insert(id.clone(), self.entries.len() - 1);
        self.entry_ids.insert(id.clone());
        self.clear_persisted_leaf_override_after_append();
        self.enqueue_autosave_mutation(AutosaveMutationKind::Metadata);
        id
    }

    pub fn ensure_entry_ids(&mut self) {
        // `rebuild_all_caches()` runs `finalize_loaded_entries()`, which already
        // fills missing entry IDs and rebuilds all derived caches in one pass.
        self.rebuild_all_caches();
    }

    /// Rebuild all derived caches from `self.entries`.
    ///
    /// Called after bulk mutations (save round-trip, ensure_entry_ids) where
    /// incremental maintenance is impractical.
    fn rebuild_all_caches(&mut self) {
        let finalized = finalize_loaded_entries(&mut self.entries);
        self.entry_ids = finalized.entry_ids;
        self.entry_index = finalized.entry_index;
        self.cached_message_count = finalized
            .message_count
            .saturating_add(self.v2_message_count_offset);
        self.cached_name = finalized.name;
        // is_linear requires BOTH: no branching in the entry tree AND the
        // current leaf_id pointing at the last entry.  If the user navigated
        // to a mid-chain entry before saving, the leaf differs from the tip
        // and the fast path would return wrong results.
        self.is_linear = finalized.is_linear && self.leaf_id.eq(&finalized.leaf_id);
    }

    /// Convert session entries to model messages (for provider context).
    pub fn to_messages(&self) -> Vec<Message> {
        let mut messages = Vec::new();
        for entry in &self.entries {
            if let SessionEntry::Message(msg_entry) = entry {
                if let Some(message) = session_message_to_model(&msg_entry.message) {
                    messages.push(message);
                }
            }
        }
        messages
    }

    /// Render the session as a standalone HTML document.
    ///
    /// Delegates to `render_session_html()` for the actual rendering. For
    /// non-blocking export, prefer `export_snapshot().to_html()` which avoids
    /// cloning internal caches.
    pub fn to_html(&self) -> String {
        render_session_html(&self.header, &self.entries)
    }

    /// Update header model info.
    pub fn set_model_header(
        &mut self,
        provider: Option<String>,
        model_id: Option<String>,
        thinking_level: Option<String>,
    ) {
        let changed = provider.is_some() || model_id.is_some() || thinking_level.is_some();
        if provider.is_some() {
            self.header.provider = provider;
        }
        if model_id.is_some() {
            self.header.model_id = model_id;
        }
        if thinking_level.is_some() {
            self.header.thinking_level = thinking_level;
        }
        if changed {
            self.header_dirty = true;
            self.enqueue_autosave_mutation(AutosaveMutationKind::Metadata);
        }
    }

    pub fn set_branched_from(&mut self, path: Option<String>) {
        self.header.parent_session = path;
        self.header_dirty = true;
        self.enqueue_autosave_mutation(AutosaveMutationKind::Metadata);
    }

    /// Create a lightweight snapshot for non-blocking HTML export.
    ///
    /// Captures only the fields needed by `to_html()` (header, entries, path),
    /// avoiding a full `Session::clone()` which includes caches, autosave queues,
    /// persistence state, and other internal bookkeeping.
    pub fn export_snapshot(&self) -> ExportSnapshot {
        ExportSnapshot {
            header: self.header.clone(),
            entries: self.entries.clone(),
            path: self.path.clone(),
        }
    }

    /// Plan a `/fork` from a user message entry ID.
    ///
    /// Returns the entries to copy into a new session (path to the parent of the selected
    /// user message), the new leaf id, and the selected user message text for editor pre-fill.
    pub fn plan_fork_from_user_message(&self, entry_id: &str) -> Result<ForkPlan> {
        let entry = self
            .get_entry(entry_id)
            .ok_or_else(|| Error::session(format!("Fork target not found: {entry_id}")))?;

        let SessionEntry::Message(message_entry) = entry else {
            return Err(Error::session(format!(
                "Fork target is not a message entry: {entry_id}"
            )));
        };

        let SessionMessage::User { content, .. } = &message_entry.message else {
            return Err(Error::session(format!(
                "Fork target is not a user message: {entry_id}"
            )));
        };

        let selected_text = user_content_to_text(content);
        let leaf_id = message_entry.base.parent_id.clone();

        let entries = if let Some(ref leaf_id) = leaf_id {
            if self.is_linear {
                let idx = self.entry_index.get(leaf_id).copied().ok_or_else(|| {
                    Error::session(format!("Failed to build fork: missing entry {leaf_id}"))
                })?;
                self.entries[..=idx].to_vec()
            } else {
                let path_ids = self.get_path_to_entry(leaf_id);
                let mut entries = Vec::new();
                for path_id in path_ids {
                    let entry = self.get_entry(&path_id).ok_or_else(|| {
                        Error::session(format!("Failed to build fork: missing entry {path_id}"))
                    })?;
                    entries.push(entry.clone());
                }
                entries
            }
        } else {
            Vec::new()
        };

        Ok(ForkPlan {
            entries,
            leaf_id,
            selected_text,
        })
    }

    fn next_entry_id(&self) -> String {
        let use_entry_id_cache = session_entry_id_cache_enabled();

        if use_entry_id_cache {
            // Use the cached set for O(1) collision checks.
            // generate_entry_id handles generation + collision retry logic.
            generate_entry_id(&self.entry_ids)
        } else {
            // Fallback: scan entries to build the exclusion set on demand.
            // This is slower (O(N)) but only used if the cache feature flag is disabled.
            let existing = entry_id_set(&self.entries);
            generate_entry_id(&existing)
        }
    }

    // ========================================================================
    // Tree Navigation
    // ========================================================================

    /// Build a map from parent ID to children IDs.
    fn build_children_map(&self) -> HashMap<Option<String>, Vec<String>> {
        let mut children: HashMap<Option<String>, Vec<String>> =
            HashMap::with_capacity(self.entries.len());
        for entry in &self.entries {
            if let Some(id) = entry.base_id() {
                children
                    .entry(entry.base().parent_id.clone())
                    .or_default()
                    .push(id.clone());
            }
        }
        children
    }

    /// Get the path from an entry back to the root (inclusive).
    /// Returns entry IDs in order from root to the specified entry.
    pub fn get_path_to_entry(&self, entry_id: &str) -> Vec<String> {
        // Fast path: in linear sessions, every ancestor chain is a prefix of `entries`.
        if self.is_linear {
            if let Some(&idx) = self.entry_index.get(entry_id) {
                let mut path = Vec::with_capacity(idx + 1);
                for entry in &self.entries[..=idx] {
                    if let Some(id) = entry.base_id() {
                        path.push(id.clone());
                    }
                }
                return path;
            }
        }

        let mut path = Vec::new();
        let mut visited = std::collections::HashSet::with_capacity(self.entries.len().min(128));
        let mut current = Some(entry_id.to_string());

        while let Some(id) = current {
            if !visited.insert(id.clone()) {
                tracing::warn!(
                    "Cycle detected in session tree while building ancestor path at entry: {id}"
                );
                break;
            }
            path.push(id.clone());
            current = self
                .get_entry(&id)
                .and_then(|entry| entry.base().parent_id.clone());
        }

        path.reverse();
        path
    }

    /// Get direct children of an entry.
    pub fn get_children(&self, entry_id: Option<&str>) -> Vec<String> {
        self.entries
            .iter()
            .filter_map(|entry| {
                let id = entry.base_id()?;
                if entry.base().parent_id.as_deref().eq(&entry_id) {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// List all leaf nodes (entries with no children).
    pub fn list_leaves(&self) -> Vec<String> {
        let mut has_children: HashSet<&str> = HashSet::with_capacity(self.entries.len());
        for entry in &self.entries {
            if let Some(parent_id) = entry.base().parent_id.as_deref() {
                has_children.insert(parent_id);
            }
        }

        self.entries
            .iter()
            .filter_map(|e| {
                let id = e.base_id()?;
                if has_children.contains(id.as_str()) {
                    None
                } else {
                    Some(id.clone())
                }
            })
            .collect()
    }

    /// Navigate to a specific entry, making it the current leaf.
    /// Returns true if the entry exists.
    pub fn navigate_to(&mut self, entry_id: &str) -> bool {
        // Gap B: O(1) existence check via entry_index.
        let exists = self.entry_index.contains_key(entry_id);
        if exists {
            // Gap A: navigating away from the tip breaks linearity.
            let is_tip = self
                .entries
                .last()
                .and_then(|e| e.base_id())
                .is_some_and(|id| id.eq(entry_id));
            if !is_tip {
                self.is_linear = false;
            }
            self.leaf_id = Some(entry_id.to_string());
            self.sync_header_to_current_path_metadata();
            true
        } else {
            false
        }
    }

    /// Get the current leaf entry ID.
    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    /// Initialize the session entries and leaf from a `ForkPlan`.
    ///
    /// This safely applies the new entries and leaf, and rebuilds
    /// all internal caches (including the `is_linear` optimization flag).
    pub fn init_from_fork_plan(&mut self, plan: ForkPlan) {
        self.entries = plan.entries;
        self.leaf_id = plan.leaf_id;
        self.rebuild_all_caches();
        self.sync_navigation_state_to_header();
    }

    /// Set the leaf ID directly (for tests only).
    pub fn _test_set_leaf_id(&mut self, id: Option<String>) {
        self.leaf_id = id;
        self.rebuild_all_caches();
        self.sync_navigation_state_to_header();
    }

    fn sync_header_to_current_path_metadata(&mut self) {
        self.sync_navigation_state_to_header();
    }

    /// Revert the last user message on the current path, effectively abandoning it.
    /// This is used during API retries to prevent duplicating the user prompt in the session history.
    pub fn revert_last_user_message(&mut self) -> bool {
        let mut current_id = self.leaf_id.clone();
        let mut reverted_any = false;

        while let Some(id) = current_id {
            if let Some(entry) = self.get_entry(&id) {
                let parent_id = entry.base().parent_id.clone();
                let is_user = if let SessionEntry::Message(msg_entry) = entry {
                    matches!(msg_entry.message, SessionMessage::User { .. })
                } else {
                    false
                };

                self.leaf_id.clone_from(&parent_id);
                self.is_linear = false;
                reverted_any = true;

                if is_user {
                    // We found and reverted the user message, we can stop walking back.
                    break;
                }

                current_id = parent_id;
            } else {
                break;
            }
        }
        if reverted_any {
            self.sync_navigation_state_to_header();
        }
        reverted_any
    }

    /// Reset the leaf pointer to root (before any entries).
    ///
    /// After calling this, the next appended entry will become a new root entry
    /// (`parent_id = None`). This is used by interactive `/tree` navigation when
    /// re-editing the first user message.
    pub fn reset_leaf(&mut self) {
        self.leaf_id = None;
        self.is_linear = false;
        self.sync_navigation_state_to_header();
    }

    /// Create a new branch starting from a specific entry.
    /// Sets the leaf_id to the specified entry so new entries branch from there.
    /// Returns true if the entry exists.
    pub fn create_branch_from(&mut self, entry_id: &str) -> bool {
        self.navigate_to(entry_id)
    }

    /// Get the entry at a specific ID (Gap B: O(1) via `entry_index`).
    pub fn get_entry(&self, entry_id: &str) -> Option<&SessionEntry> {
        self.entry_index
            .get(entry_id)
            .and_then(|&idx| self.entries.get(idx))
    }

    /// Get the entry at a specific ID, mutable (Gap B: O(1) via `entry_index`).
    pub fn get_entry_mut(&mut self, entry_id: &str) -> Option<&mut SessionEntry> {
        self.entry_index
            .get(entry_id)
            .copied()
            .and_then(|idx| self.entries.get_mut(idx))
    }

    /// Entries along the current leaf path, in chronological order.
    ///
    /// Gap A: when `is_linear` is true (the 99% case — no branching has
    /// occurred), this returns all entries directly without building a
    /// parent map or tracing the path.
    pub fn entries_for_current_path(&self) -> Vec<&SessionEntry> {
        let Some(leaf_id) = &self.leaf_id else {
            return Vec::new();
        };

        // Fast path: linear session — all entries are on the current path.
        if self.is_linear {
            return self.entries.iter().collect();
        }

        let mut path_indices = Vec::with_capacity(16);
        let mut visited = HashSet::with_capacity(self.entries.len().min(128));
        let mut current = Some(leaf_id.clone());

        while let Some(id) = current.as_ref() {
            if !visited.insert(id.clone()) {
                tracing::warn!(
                    "Cycle detected in session tree while collecting current path entries at: {id}"
                );
                break;
            }
            let Some(&idx) = self.entry_index.get(id.as_str()) else {
                break;
            };
            let Some(entry) = self.entries.get(idx) else {
                break;
            };
            path_indices.push(idx);
            current.clone_from(&entry.base().parent_id);
        }

        path_indices.reverse();
        path_indices
            .into_iter()
            .filter_map(|idx| self.entries.get(idx))
            .collect()
    }

    /// Convert session entries along the current path to model messages.
    /// This follows parent_id links from leaf_id back to root.
    pub fn to_messages_for_current_path(&self) -> Vec<Message> {
        if self.leaf_id.is_none() {
            return Vec::new();
        }

        if self.is_linear {
            return Self::to_messages_from_path(self.entries.len(), |idx| &self.entries[idx]);
        }

        let path_entries = self.entries_for_current_path();
        Self::to_messages_from_path(path_entries.len(), |idx| path_entries[idx])
    }

    fn append_model_message_for_entry(messages: &mut Vec<Message>, entry: &SessionEntry) {
        match entry {
            SessionEntry::Message(msg_entry) => {
                if let Some(message) = session_message_to_model(&msg_entry.message) {
                    messages.push(message);
                }
            }
            SessionEntry::BranchSummary(summary) => {
                let summary_message = SessionMessage::BranchSummary {
                    summary: summary.summary.clone(),
                    from_id: summary.from_id.clone(),
                };
                if let Some(message) = session_message_to_model(&summary_message) {
                    messages.push(message);
                }
            }
            _ => {}
        }
    }

    fn to_messages_from_path<'a, F>(path_len: usize, entry_at: F) -> Vec<Message>
    where
        F: Fn(usize) -> &'a SessionEntry,
    {
        let mut last_compaction = None;
        for idx in (0..path_len).rev() {
            if let SessionEntry::Compaction(compaction) = entry_at(idx) {
                last_compaction = Some((idx, compaction));
                break;
            }
        }

        if let Some((compaction_idx, compaction)) = last_compaction {
            let mut messages = Vec::with_capacity(path_len);
            let summary_message = SessionMessage::CompactionSummary {
                summary: compaction.summary.clone(),
                tokens_before: compaction.tokens_before,
            };
            if let Some(message) = session_message_to_model(&summary_message) {
                messages.push(message);
            }

            let has_kept_entry = (0..path_len).any(|idx| {
                entry_at(idx)
                    .base_id()
                    .is_some_and(|id| id.eq(&compaction.first_kept_entry_id))
            });

            let mut keep = false;
            let mut past_compaction = false;
            for idx in 0..path_len {
                let entry = entry_at(idx);
                if idx.eq(&compaction_idx) {
                    past_compaction = true;
                }
                if !keep {
                    if has_kept_entry {
                        if entry
                            .base_id()
                            .is_some_and(|id| id.eq(&compaction.first_kept_entry_id))
                        {
                            keep = true;
                        } else {
                            continue;
                        }
                    } else if past_compaction {
                        tracing::warn!(
                            first_kept_entry_id = %compaction.first_kept_entry_id,
                            "Compaction references missing entry; including all post-compaction entries"
                        );
                        keep = true;
                    } else {
                        continue;
                    }
                }
                Self::append_model_message_for_entry(&mut messages, entry);
            }

            return messages;
        }

        let mut messages = Vec::with_capacity(path_len);
        for idx in 0..path_len {
            Self::append_model_message_for_entry(&mut messages, entry_at(idx));
        }
        messages
    }

    /// Find the nearest ancestor that is a fork point (has multiple children)
    /// and return its children (sibling branch roots). Each sibling is represented
    /// by its branch-root entry ID plus the leaf ID reachable from that root.
    ///
    /// Returns `(fork_point_id, sibling_leaves)` where each sibling leaf is
    /// a leaf entry ID reachable through the fork point's children. The current
    /// leaf is included in the list.
    pub fn sibling_branches(&self) -> Option<(Option<String>, Vec<SiblingBranch>)> {
        let children_map = self.build_children_map();
        let leaf_id = self.leaf_id.as_ref()?;
        let path = self.get_path_to_entry(leaf_id);
        if path.is_empty() {
            return None;
        }

        // Walk backwards from current leaf's path to find the nearest fork point.
        // A fork point is any entry whose parent has >1 children, OR None (root)
        // with >1 root entries.
        // We check each entry's parent to see if the parent has multiple children.
        for (idx, entry_id) in path.iter().enumerate().rev() {
            let parent_of_entry = self
                .get_entry(entry_id)
                .and_then(|e| e.base().parent_id.clone());

            let Some(siblings_at_parent) = children_map.get(&parent_of_entry) else {
                continue;
            };

            if siblings_at_parent.len() > 1 {
                // This is a fork point. Collect all leaves reachable from each sibling.
                let mut branches = Vec::new();
                let current_branch_ids: HashSet<&str> =
                    path[idx..].iter().map(String::as_str).collect();
                for sibling_root in siblings_at_parent {
                    let leaf = Self::deepest_leaf_from(&children_map, sibling_root);
                    let (preview, msg_count) = self.path_preview_and_message_count(&leaf);
                    let is_current = current_branch_ids.contains(sibling_root.as_str());
                    branches.push(SiblingBranch {
                        root_id: sibling_root.clone(),
                        leaf_id: leaf,
                        preview,
                        message_count: msg_count,
                        is_current,
                    });
                }
                return Some((parent_of_entry, branches));
            }
        }

        None
    }

    /// Follow the first child chain to reach the deepest leaf from a starting entry.
    fn deepest_leaf_from(
        children_map: &HashMap<Option<String>, Vec<String>>,
        start_id: &str,
    ) -> String {
        let mut current = start_id.to_string();
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current.clone()) {
                tracing::warn!("Cycle detected in session tree at entry: {current}");
                return current;
            }
            let children = children_map.get(&Some(current.clone()));
            match children.and_then(|c| c.first()) {
                Some(child) => current.clone_from(child),
                None => return current,
            }
        }
    }

    /// Compute a short preview (first user message on the path) and the number
    /// of message entries for a leaf in a single parent-chain walk.
    fn path_preview_and_message_count(&self, leaf_id: &str) -> (String, usize) {
        let mut visited = HashSet::with_capacity(self.entries.len().min(128));
        let mut current = Some(leaf_id.to_string());
        let mut preview = None;
        let mut count = 0usize;

        while let Some(id) = current.as_ref() {
            if !visited.insert(id.clone()) {
                tracing::warn!("Cycle detected in session tree while collecting path stats: {id}");
                break;
            }
            let Some(entry) = self.get_entry(id.as_str()) else {
                break;
            };
            if matches!(entry, SessionEntry::Message(_)) {
                count = count.saturating_add(1);
            }
            if let SessionEntry::Message(msg) = entry {
                if let SessionMessage::User { content, .. } = &msg.message {
                    let text = user_content_to_text(content);
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        preview = Some(if trimmed.chars().count() > 60 {
                            let truncated: String = trimmed.chars().take(57).collect();
                            format!("{truncated}...")
                        } else {
                            trimmed.to_string()
                        });
                    }
                }
            }
            current.clone_from(&entry.base().parent_id);
        }

        (preview.unwrap_or_else(|| String::from("(empty)")), count)
    }

    /// Get a summary of branches in this session.
    pub fn branch_summary(&self) -> BranchInfo {
        let leaves = self.list_leaves();
        let children_map = self.build_children_map();

        // Find branch points (entries with multiple children)
        let branch_points: Vec<String> = self
            .entries
            .iter()
            .filter_map(|e| {
                let id = e.base_id()?;
                let children = children_map.get(&Some(id.clone()))?;
                if children.len() > 1 {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();

        BranchInfo {
            total_entries: self.entries.len(),
            leaf_count: leaves.len(),
            branch_point_count: branch_points.len(),
            current_leaf: self.leaf_id.clone(),
            leaves,
            branch_points,
        }
    }

    /// Add a label to an entry.
    pub fn add_label(&mut self, target_id: &str, label: Option<String>) -> Option<String> {
        // Verify target exists
        self.get_entry(target_id)?;

        let id = self.next_entry_id();
        let base = EntryBase::new(self.leaf_id.clone(), id.clone());
        let entry = SessionEntry::Label(LabelEntry {
            base,
            target_id: target_id.to_string(),
            label,
        });
        self.leaf_id = Some(id.clone());
        self.entries.push(entry);
        self.entry_index.insert(id.clone(), self.entries.len() - 1);
        self.entry_ids.insert(id.clone());
        self.clear_persisted_leaf_override_after_append();
        self.enqueue_autosave_mutation(AutosaveMutationKind::Label);
        Some(id)
    }
}

/// Summary of branches in a session.
#[derive(Debug, Clone)]
pub struct BranchInfo {
    pub total_entries: usize,
    pub leaf_count: usize,
    pub branch_point_count: usize,
    pub current_leaf: Option<String>,
    pub leaves: Vec<String>,
    pub branch_points: Vec<String>,
}

/// A sibling branch at a fork point.
#[derive(Debug, Clone)]
pub struct SiblingBranch {
    /// Entry ID of the branch root (child of the fork point).
    pub root_id: String,
    /// Leaf entry ID reachable from this branch root.
    pub leaf_id: String,
    /// Short preview of the first user message on this branch.
    pub preview: String,
    /// Number of message entries along the path.
    pub message_count: usize,
    /// Whether the current session leaf is on this branch.
    pub is_current: bool,
}

#[derive(Debug, Clone)]
#[cfg(feature = "session-index")]
struct SessionPickEntry {
    path: PathBuf,
    id: String,
    cwd: String,
    timestamp: String,
    message_count: u64,
    name: Option<String>,
    last_modified_ms: i64,
    size_bytes: u64,
}

#[cfg(feature = "session-index")]
impl SessionPickEntry {
    fn from_meta(meta: crate::session_index::SessionMeta) -> Self {
        Self {
            path: PathBuf::from(meta.path),
            id: meta.id,
            cwd: meta.cwd,
            timestamp: meta.timestamp,
            message_count: meta.message_count,
            name: meta.name,
            last_modified_ms: meta.last_modified_ms,
            size_bytes: meta.size_bytes,
        }
    }

    fn to_meta(&self) -> crate::session_index::SessionMeta {
        crate::session_index::SessionMeta {
            path: self.path.display().to_string(),
            id: self.id.clone(),
            cwd: self.cwd.clone(),
            timestamp: self.timestamp.clone(),
            message_count: self.message_count,
            last_modified_ms: self.last_modified_ms,
            size_bytes: self.size_bytes,
            name: self.name.clone(),
        }
    }
}

#[cfg(feature = "session-index")]
fn indexed_session_path_is_missing(path: &Path) -> bool {
    match path.try_exists() {
        Ok(exists) => !exists,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "Failed to determine whether indexed session path exists; deferring prune"
            );
            false
        }
    }
}

#[cfg(feature = "session-index")]
fn split_indexed_session_entries(
    metas: Vec<crate::session_index::SessionMeta>,
) -> (Vec<SessionPickEntry>, Vec<PathBuf>) {
    let mut entries = Vec::new();
    let mut missing_paths = Vec::new();

    for meta in metas {
        let path = PathBuf::from(&meta.path);
        if indexed_session_path_is_missing(&path) {
            missing_paths.push(path);
            continue;
        }

        entries.push(SessionPickEntry::from_meta(meta));
    }

    (entries, missing_paths)
}

#[cfg(feature = "session-index")]
fn prune_session_index_path(index: &SessionIndex, path: &Path, reason: &'static str) {
    if let Err(err) = index.delete_session_path(path) {
        tracing::warn!(
            path = %path.display(),
            error = %err,
            reason,
            "Failed to prune session from index"
        );
    }
}

#[cfg(feature = "session-index")]
fn can_reuse_known_entry(known_entry: &SessionPickEntry, disk_ms: i64, disk_size: u64) -> bool {
    (known_entry.last_modified_ms, known_entry.size_bytes)
        .cmp(&(disk_ms, disk_size))
        .is_eq()
}

#[cfg(feature = "session-index")]
struct ScanSessionsResult {
    entries: Vec<SessionPickEntry>,
    refreshed_entries: Vec<SessionPickEntry>,
    failed_paths: Vec<PathBuf>,
}

#[cfg(feature = "session-index")]
fn refresh_session_index_entries(
    index: &SessionIndex,
    entries: &[SessionPickEntry],
    reason: &'static str,
) {
    for entry in entries {
        if let Err(err) = index.upsert_session_meta(entry.to_meta()) {
            tracing::warn!(
                path = %entry.path.display(),
                error = %err,
                reason,
                "Failed to refresh session metadata in index"
            );
        }
    }
}

#[cfg(feature = "session-index")]
fn merge_scanned_session_entries(
    by_path: &mut HashMap<PathBuf, SessionPickEntry>,
    entries: Vec<SessionPickEntry>,
) {
    for entry in entries {
        // Disk is the source of truth for session metadata. The scan either
        // reparsed the file or confirmed the cached snapshot still matches, so
        // it should always win over the earlier index view for that path.
        by_path.insert(entry.path.clone(), entry);
    }
}

#[cfg(feature = "session-index")]
async fn scan_sessions_on_disk(
    project_session_dir: &Path,
    known: Vec<SessionPickEntry>,
) -> Result<ScanSessionsResult> {
    let path_buf = project_session_dir.to_path_buf();
    let (tx, mut rx) = oneshot::channel();

    let handle = thread::Builder::new()
        .name("session-scan".to_string())
        .spawn(move || {
            let res = (|| -> Result<ScanSessionsResult> {
                let mut entries = Vec::new();
                let mut refreshed_entries = Vec::new();
                let mut failed_paths = Vec::new();
                let dir_entries = std::fs::read_dir(&path_buf)
                    .map_err(|e| Error::session(format!("Failed to read sessions: {e}")))?;

                let known_map: HashMap<PathBuf, SessionPickEntry> =
                    known.into_iter().map(|e| (e.path.clone(), e)).collect();

                for entry in dir_entries {
                    let entry =
                        entry.map_err(|e| Error::session(format!("Read dir entry: {e}")))?;
                    let path = entry.path();
                    if is_session_file_path(&path) {
                        // Optimization: if we already have this file indexed and both mtime and
                        // size match, reuse indexed metadata to avoid a full parse.
                        if let Ok((disk_ms, disk_size)) = session_file_stats(&path) {
                            if let Some(known_entry) = known_map.get(&path) {
                                if can_reuse_known_entry(known_entry, disk_ms, disk_size) {
                                    entries.push(known_entry.clone());
                                    continue;
                                }
                            }
                        }

                        match load_session_meta(&path) {
                            Ok(meta) => {
                                refreshed_entries.push(meta.clone());
                                entries.push(meta);
                            }
                            Err(_) => failed_paths.push(path),
                        }
                    }
                }
                Ok(ScanSessionsResult {
                    entries,
                    refreshed_entries,
                    failed_paths,
                })
            })();
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        })
        .map_err(|e| Error::session(format!("Failed to spawn session scan thread: {e}")))?;

    let cx = AgentCx::for_request();
    let recv_result = rx.recv(cx.cx()).await;
    finish_worker_result(handle, recv_result, "Scan task cancelled")
}

#[cfg(feature = "session-index")]
fn load_session_meta(path: &Path) -> Result<SessionPickEntry> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("jsonl") => load_session_meta_jsonl(path),
        #[cfg(feature = "sqlite-sessions")]
        Some("sqlite") => load_session_meta_sqlite(path),
        _ => Err(Error::session(format!(
            "Unsupported session file extension: {}",
            path.display()
        ))),
    }
}

#[derive(Deserialize)]
#[cfg(feature = "session-index")]
struct PartialEntry {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    name: Option<String>,
}

#[cfg(feature = "session-index")]
fn load_session_meta_jsonl(path: &Path) -> Result<SessionPickEntry> {
    let file = std::fs::File::open(path)
        .map_err(|e| Error::session(format!("Failed to read session: {e}")))?;
    let mut reader = BufReader::new(file);

    let Some(header_line) = read_capped_utf8_line(&mut reader)
        .map_err(|e| Error::session(format!("Failed to read header: {e}")))?
    else {
        return Err(Error::session("Empty session file"));
    };

    let header: SessionHeader =
        serde_json::from_str(&header_line).map_err(|e| Error::session(format!("{e}")))?;
    header
        .validate()
        .map_err(|reason| Error::session(format!("Invalid session header: {reason}")))?;

    let mut message_count = 0u64;
    let mut name = None;
    loop {
        let Some(line_content) = read_capped_utf8_line(&mut reader)
            .map_err(|e| Error::session(format!("Failed to read session entry: {e}")))?
        else {
            break;
        };
        if let Ok(entry) = serde_json::from_str::<PartialEntry>(&line_content) {
            match entry.r#type.as_str() {
                "message" => message_count += 1,
                "session_info" if entry.name.is_some() => {
                    name = entry.name;
                }
                _ => {}
            }
        }
    }

    let (last_modified_ms, size_bytes) = session_file_stats(path)?;

    Ok(SessionPickEntry {
        path: path.to_path_buf(),
        id: header.id,
        cwd: header.cwd,
        timestamp: header.timestamp,
        message_count,
        name,
        last_modified_ms,
        size_bytes,
    })
}

#[cfg(feature = "sqlite-sessions")]
#[cfg(feature = "session-index")]
fn load_session_meta_sqlite(path: &Path) -> Result<SessionPickEntry> {
    let meta = futures::executor::block_on(async {
        crate::session_sqlite::load_session_meta(path).await
    })?;
    let header = meta.header;
    header
        .validate()
        .map_err(|reason| Error::session(format!("Invalid session header: {reason}")))?;

    let (last_modified_ms, size_bytes) = session_file_stats(path)?;

    Ok(SessionPickEntry {
        path: path.to_path_buf(),
        id: header.id,
        cwd: header.cwd,
        timestamp: header.timestamp,
        message_count: meta.message_count,
        name: meta.name,
        last_modified_ms,
        size_bytes,
    })
}

// ============================================================================
// Session Header
// ============================================================================

/// Session file header.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u8>,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_thinking_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "leafId")]
    pub current_leaf: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "branchedFrom",
        alias = "parentSession"
    )]
    pub parent_session: Option<String>,
}

impl SessionHeader {
    pub fn new() -> Self {
        let now = chrono::Utc::now();
        Self {
            r#type: "session".to_string(),
            version: Some(SESSION_VERSION),
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            provider: None,
            model_id: None,
            thinking_level: None,
            fallback_provider: None,
            fallback_model_id: None,
            fallback_thinking_level: None,
            current_leaf: None,
            parent_session: None,
        }
    }

    fn branch_fallback_model_fields(&self) -> (Option<String>, Option<String>) {
        (
            self.fallback_provider
                .clone()
                .or_else(|| self.provider.clone()),
            self.fallback_model_id
                .clone()
                .or_else(|| self.model_id.clone()),
        )
    }

    fn materialize_branch_fallbacks(&mut self) -> bool {
        // Track mutations as booleans, then materialize them.
        // This pattern avoids clippy::useless_let_if_seq while
        // remaining readable for multiple independent conditions.
        let set_provider = self.fallback_provider.is_none() && self.provider.is_some();
        let set_model_id = self.fallback_model_id.is_none() && self.model_id.is_some();
        let set_thinking = self.fallback_thinking_level.is_none() && self.thinking_level.is_some();

        if set_provider {
            self.fallback_provider = self.provider.clone();
        }
        if set_model_id {
            self.fallback_model_id = self.model_id.clone();
        }
        if set_thinking {
            self.fallback_thinking_level = self.thinking_level.clone();
        }

        set_provider || set_model_id || set_thinking
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        if !self.r#type.eq("session") {
            return Err(format!("type must be `session`, got `{}`", self.r#type));
        }
        if !self.version.eq(&Some(SESSION_VERSION)) {
            return Err(format!(
                "version must be {SESSION_VERSION}, got {}",
                self.version
                    .map_or_else(|| "none".to_string(), |value| value.to_string())
            ));
        }
        if self.id.trim().is_empty() {
            return Err("id must be non-empty".to_string());
        }
        if self.timestamp.trim().is_empty() {
            return Err("timestamp must be non-empty".to_string());
        }
        if self.cwd.trim().is_empty() {
            return Err("cwd must be non-empty".to_string());
        }
        Ok(())
    }

    pub fn is_valid(&self) -> bool {
        self.validate().is_ok()
    }
}

impl Default for SessionHeader {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Session Entries
// ============================================================================

/// A session entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    Message(MessageEntry),
    ModelChange(ModelChangeEntry),
    ThinkingLevelChange(ThinkingLevelChangeEntry),
    Compaction(CompactionEntry),
    BranchSummary(BranchSummaryEntry),
    Label(LabelEntry),
    SessionInfo(SessionInfoEntry),
    Custom(CustomEntry),
}

impl SessionEntry {
    pub const fn base(&self) -> &EntryBase {
        match self {
            Self::Message(e) => &e.base,
            Self::ModelChange(e) => &e.base,
            Self::ThinkingLevelChange(e) => &e.base,
            Self::Compaction(e) => &e.base,
            Self::BranchSummary(e) => &e.base,
            Self::Label(e) => &e.base,
            Self::SessionInfo(e) => &e.base,
            Self::Custom(e) => &e.base,
        }
    }

    pub const fn base_mut(&mut self) -> &mut EntryBase {
        match self {
            Self::Message(e) => &mut e.base,
            Self::ModelChange(e) => &mut e.base,
            Self::ThinkingLevelChange(e) => &mut e.base,
            Self::Compaction(e) => &mut e.base,
            Self::BranchSummary(e) => &mut e.base,
            Self::Label(e) => &mut e.base,
            Self::SessionInfo(e) => &mut e.base,
            Self::Custom(e) => &mut e.base,
        }
    }

    pub const fn base_id(&self) -> Option<&String> {
        self.base().id.as_ref()
    }
}

/// Base entry fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryBase {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub timestamp: String,
}

impl EntryBase {
    pub fn new(parent_id: Option<String>, id: String) -> Self {
        Self {
            id: Some(id),
            parent_id,
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        }
    }
}

/// Message entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub message: SessionMessage,
}

/// Session message payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "role",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SessionMessage {
    User {
        content: UserContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
    },
    Assistant {
        #[serde(flatten)]
        message: AssistantMessage,
    },
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        content: Vec<ContentBlock>,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(default)]
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
    },
    Custom {
        custom_type: String,
        content: String,
        #[serde(default)]
        display: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
    },
    BashExecution {
        command: String,
        output: String,
        exit_code: i32,
        #[serde(skip_serializing_if = "Option::is_none")]
        cancelled: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        truncated: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        full_output_path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<i64>,
        #[serde(flatten)]
        extra: HashMap<String, Value>,
    },
    BranchSummary {
        summary: String,
        from_id: String,
    },
    CompactionSummary {
        summary: String,
        tokens_before: u64,
    },
}

impl From<Message> for SessionMessage {
    fn from(message: Message) -> Self {
        match message {
            Message::User(user) => Self::User {
                content: user.content,
                timestamp: Some(user.timestamp),
            },
            Message::Assistant(assistant) => Self::Assistant {
                message: Arc::try_unwrap(assistant).unwrap_or_else(|a| (*a).clone()),
            },
            Message::ToolResult(result) => {
                let result = Arc::try_unwrap(result).unwrap_or_else(|a| (*a).clone());
                Self::ToolResult {
                    tool_call_id: result.tool_call_id,
                    tool_name: result.tool_name,
                    content: result.content,
                    details: result.details,
                    is_error: result.is_error,
                    timestamp: Some(result.timestamp),
                }
            }
            Message::Custom(custom) => Self::Custom {
                custom_type: custom.custom_type,
                content: custom.content,
                display: custom.display,
                details: custom.details,
                timestamp: Some(custom.timestamp),
            },
        }
    }
}

/// Model change entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelChangeEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub provider: String,
    pub model_id: String,
}

/// Thinking level change entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingLevelChangeEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub thinking_level: String,
}

/// Compaction entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
}

/// Branch summary entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub from_id: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
}

/// Label entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub target_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Session info entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Custom entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub custom_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// ============================================================================
// Utilities
// ============================================================================

/// Encode a working directory path for use in session directory names.
pub fn encode_cwd(path: &std::path::Path) -> String {
    let s = path.display().to_string();
    let s = s.trim_start_matches(['/', '\\']);
    let s = s.replace(['/', '\\', ':'], "-");
    format!("--{s}--")
}

fn infer_session_root_from_path(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?.to_path_buf();
    if parent
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("--") && name.ends_with("--") && name.len() > 4)
    {
        return parent.parent().map(PathBuf::from).or(Some(parent));
    }
    Some(parent)
}

pub(crate) fn session_message_to_model(message: &SessionMessage) -> Option<Message> {
    match message {
        SessionMessage::User { content, timestamp } => Some(Message::User(UserMessage {
            content: content.clone(),
            timestamp: timestamp.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
        })),
        SessionMessage::Assistant { message } => Some(Message::assistant(message.clone())),
        SessionMessage::ToolResult {
            tool_call_id,
            tool_name,
            content,
            details,
            is_error,
            timestamp,
        } => Some(Message::tool_result(ToolResultMessage {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            content: content.clone(),
            details: details.clone(),
            is_error: *is_error,
            timestamp: timestamp.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
        })),
        SessionMessage::Custom {
            custom_type,
            content,
            display,
            details,
            timestamp,
        } => Some(Message::Custom(crate::model::CustomMessage {
            content: content.clone(),
            custom_type: custom_type.clone(),
            display: *display,
            details: details.clone(),
            timestamp: timestamp.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
        })),
        SessionMessage::BashExecution {
            command,
            output,
            exit_code,
            cancelled,
            truncated,
            full_output_path,
            timestamp,
            extra,
        } => {
            if extra
                .get("excludeFromContext")
                .and_then(Value::as_bool)
                .is_some_and(|v| v)
            {
                return None;
            }
            let text = bash_execution_to_text(
                command,
                output,
                *exit_code,
                cancelled.unwrap_or(false),
                truncated.unwrap_or(false),
                full_output_path.as_deref(),
            );
            Some(Message::User(UserMessage {
                content: UserContent::Blocks(vec![ContentBlock::Text(TextContent::new(text))]),
                timestamp: timestamp.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
            }))
        }
        SessionMessage::BranchSummary { summary, .. } => Some(Message::User(UserMessage {
            content: UserContent::Blocks(vec![ContentBlock::Text(TextContent::new(format!(
                "{BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}"
            )))]),
            timestamp: chrono::Utc::now().timestamp_millis(),
        })),
        SessionMessage::CompactionSummary { summary, .. } => Some(Message::User(UserMessage {
            content: UserContent::Blocks(vec![ContentBlock::Text(TextContent::new(format!(
                "{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}"
            )))]),
            timestamp: chrono::Utc::now().timestamp_millis(),
        })),
    }
}

const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

pub(crate) fn bash_execution_to_text(
    command: &str,
    output: &str,
    exit_code: i32,
    cancelled: bool,
    truncated: bool,
    full_output_path: Option<&str>,
) -> String {
    let mut text = format!("Ran `{command}`\n");
    if output.is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str("```\n");
        text.push_str(output);
        if !output.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("```");
    }

    if cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if exit_code != 0 {
        let _ = write!(text, "\n\nCommand exited with code {exit_code}");
    }

    if truncated {
        if let Some(path) = full_output_path {
            let _ = write!(text, "\n\n[Output truncated. Full output: {path}]");
        } else {
            text.push_str("\n\n[Output truncated]");
        }
    }

    text
}

/// Render session header and entries as a standalone HTML document.
///
/// Shared implementation used by both `Session::to_html()` and
/// `ExportSnapshot::to_html()`.
#[allow(clippy::too_many_lines)]
fn render_session_html(header: &SessionHeader, entries: &[SessionEntry]) -> String {
    let mut html = String::new();
    html.push_str("<!doctype html><html><head><meta charset=\"utf-8\">");
    html.push_str("<title>Pi Session</title>");
    html.push_str("<style>");
    html.push_str(
        "body{font-family:system-ui,-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif;margin:24px;background:#0b0c10;color:#e6e6e6;}
            h1{margin:0 0 8px 0;}
            .meta{color:#9aa0a6;margin-bottom:24px;font-size:14px;}
            .msg{padding:16px 18px;margin:12px 0;border-radius:8px;background:#14161b;}
            .msg.user{border-left:4px solid #4fc3f7;}
            .msg.assistant{border-left:4px solid #81c784;}
            .msg.tool{border-left:4px solid #ffb74d;}
            .msg.system{border-left:4px solid #ef9a9a;}
            .role{font-weight:600;margin-bottom:8px;}
            pre{white-space:pre-wrap;background:#0f1115;padding:12px;border-radius:6px;overflow:auto;}
            .thinking summary{cursor:pointer;}
            img{max-width:100%;height:auto;border-radius:6px;margin-top:8px;}
            .note{color:#9aa0a6;font-size:13px;margin:6px 0;}
            ",
    );
    html.push_str("</style></head><body>");

    let _ = write!(
        html,
        "<h1>Pi Session</h1><div class=\"meta\">Session {} • {} • cwd: {}</div>",
        escape_html(&header.id),
        escape_html(&header.timestamp),
        escape_html(&header.cwd)
    );

    for entry in entries {
        match entry {
            SessionEntry::Message(message) => {
                html.push_str(&render_session_message(&message.message));
            }
            SessionEntry::ModelChange(change) => {
                let _ = write!(
                    html,
                    "<div class=\"msg system\"><div class=\"role\">Model</div><div class=\"note\">{} / {}</div></div>",
                    escape_html(&change.provider),
                    escape_html(&change.model_id)
                );
            }
            SessionEntry::ThinkingLevelChange(change) => {
                let _ = write!(
                    html,
                    "<div class=\"msg system\"><div class=\"role\">Thinking</div><div class=\"note\">{}</div></div>",
                    escape_html(&change.thinking_level)
                );
            }
            SessionEntry::Compaction(compaction) => {
                let _ = write!(
                    html,
                    "<div class=\"msg system\"><div class=\"role\">Compaction</div><pre>{}</pre></div>",
                    escape_html(&compaction.summary)
                );
            }
            SessionEntry::BranchSummary(summary) => {
                let _ = write!(
                    html,
                    "<div class=\"msg system\"><div class=\"role\">Branch Summary</div><pre>{}</pre></div>",
                    escape_html(&summary.summary)
                );
            }
            SessionEntry::SessionInfo(info) => {
                if let Some(name) = &info.name {
                    let _ = write!(
                        html,
                        "<div class=\"msg system\"><div class=\"role\">Session Name</div><div class=\"note\">{}</div></div>",
                        escape_html(name)
                    );
                }
            }
            SessionEntry::Custom(custom) => {
                let _ = write!(
                    html,
                    "<div class=\"msg system\"><div class=\"role\">{}</div></div>",
                    escape_html(&custom.custom_type)
                );
            }
            SessionEntry::Label(_) => {}
        }
    }

    html.push_str("</body></html>");
    html
}

fn render_session_message(message: &SessionMessage) -> String {
    match message {
        SessionMessage::User { content, .. } => {
            let mut html = String::new();
            html.push_str("<div class=\"msg user\"><div class=\"role\">User</div>");
            html.push_str(&render_user_content(content));
            html.push_str("</div>");
            html
        }
        SessionMessage::Assistant { message } => {
            let mut html = String::new();
            html.push_str("<div class=\"msg assistant\"><div class=\"role\">Assistant</div>");
            html.push_str(&render_blocks(&message.content));
            html.push_str("</div>");
            html
        }
        SessionMessage::ToolResult {
            tool_name,
            content,
            is_error,
            details,
            ..
        } => {
            let mut html = String::new();
            let role = if *is_error { "Tool Error" } else { "Tool" };
            let _ = write!(
                html,
                "<div class=\"msg tool\"><div class=\"role\">{}: {}</div>",
                role,
                escape_html(tool_name)
            );
            html.push_str(&render_blocks(content));
            if let Some(details) = details {
                let details_str =
                    serde_json::to_string_pretty(details).unwrap_or_else(|_| details.to_string());
                let _ = write!(html, "<pre>{}</pre>", escape_html(&details_str));
            }
            html.push_str("</div>");
            html
        }
        SessionMessage::Custom {
            custom_type,
            content,
            ..
        } => {
            let mut html = String::new();
            let _ = write!(
                html,
                "<div class=\"msg system\"><div class=\"role\">{}</div><pre>{}</pre></div>",
                escape_html(custom_type),
                escape_html(content)
            );
            html
        }
        SessionMessage::BashExecution {
            command,
            output,
            exit_code,
            ..
        } => {
            let mut html = String::new();
            let _ = write!(
                html,
                "<div class=\"msg tool\"><div class=\"role\">Bash (exit {exit_code})</div><pre>{}</pre><pre>{}</pre></div>",
                escape_html(command),
                escape_html(output)
            );
            html
        }
        SessionMessage::BranchSummary { summary, .. } => {
            format!(
                "<div class=\"msg system\"><div class=\"role\">Branch Summary</div><pre>{}</pre></div>",
                escape_html(summary)
            )
        }
        SessionMessage::CompactionSummary { summary, .. } => {
            format!(
                "<div class=\"msg system\"><div class=\"role\">Compaction</div><pre>{}</pre></div>",
                escape_html(summary)
            )
        }
    }
}

fn render_user_content(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => format!("<pre>{}</pre>", escape_html(text)),
        UserContent::Blocks(blocks) => render_blocks(blocks),
    }
}

fn render_blocks(blocks: &[ContentBlock]) -> String {
    let mut html = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => {
                let _ = write!(html, "<pre>{}</pre>", escape_html(&text.text));
            }
            ContentBlock::Thinking(thinking) => {
                let _ = write!(
                    html,
                    "<details class=\"thinking\"><summary>Thinking</summary><pre>{}</pre></details>",
                    escape_html(&thinking.thinking)
                );
            }
            ContentBlock::Image(image) => {
                let _ = write!(
                    html,
                    "<img src=\"data:{};base64,{}\" alt=\"image\"/>",
                    escape_html(&image.mime_type),
                    escape_html(&image.data)
                );
            }
            ContentBlock::ToolCall(tool_call) => {
                let args = serde_json::to_string_pretty(&tool_call.arguments)
                    .unwrap_or_else(|_| tool_call.arguments.to_string());
                let _ = write!(
                    html,
                    "<div class=\"note\">Tool call: {}</div><pre>{}</pre>",
                    escape_html(&tool_call.name),
                    escape_html(&args)
                );
            }
            ContentBlock::RedactedThinking(_) => {
                // Render a compact placeholder so transcript readers can tell
                // a redacted-thinking block was present without exposing the
                // opaque payload (which is provider-internal safety state).
                html.push_str(
                    "<details class=\"thinking\"><summary>Thinking (redacted)</summary></details>",
                );
            }
        }
    }
    html
}

fn escape_html(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn user_content_to_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => content_blocks_to_text(blocks),
    }
}

fn content_blocks_to_text(blocks: &[ContentBlock]) -> String {
    let mut output = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text_block) => push_line(&mut output, &text_block.text),
            ContentBlock::Image(image) => {
                push_line(&mut output, &format!("[image: {}]", image.mime_type));
            }
            ContentBlock::Thinking(thinking_block) => {
                push_line(&mut output, &thinking_block.thinking);
            }
            ContentBlock::ToolCall(call) => {
                push_line(&mut output, &format!("[tool call: {}]", call.name));
            }
            ContentBlock::RedactedThinking(_) => {
                push_line(&mut output, "[thinking: redacted]");
            }
        }
    }
    output
}

fn push_line(out: &mut String, line: &str) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(line);
}

fn entry_id_set(entries: &[SessionEntry]) -> HashSet<String> {
    entries
        .iter()
        .filter_map(|e| e.base_id().cloned())
        .collect()
}

/// Minimum entry count to activate parallel deserialization (Gap E).
const PARALLEL_THRESHOLD: usize = 512;
/// Number of JSONL lines deserialized per batch in the blocking open path.
const JSONL_PARSE_BATCH_SIZE: usize = 8192;

/// Parse a JSONL session file on the current (blocking) thread.
///
/// Combines Gap E (parallel deserialization) and Gap F (single-pass
/// finalization) for the fastest possible open path.
#[allow(clippy::too_many_lines)]
fn open_jsonl_blocking(path_buf: PathBuf) -> Result<(Session, SessionOpenDiagnostics)> {
    let file = std::fs::File::open(&path_buf).map_err(|e| crate::Error::Io(Box::new(e)))?;
    let mut reader = std::io::BufReader::new(file);

    let Some(header_line) =
        read_capped_utf8_line(&mut reader).map_err(|e| crate::Error::Io(Box::new(e)))?
    else {
        return Err(crate::Error::session("Empty session file"));
    };
    if header_line.trim().is_empty() {
        return Err(crate::Error::session("Empty session file"));
    }

    // Parse header (first line)
    let header: SessionHeader = serde_json::from_str(&header_line)
        .map_err(|e| crate::Error::session(format!("Invalid header: {e}")))?;
    header
        .validate()
        .map_err(|reason| crate::Error::session(format!("Invalid session header: {reason}")))?;
    let (header, normalized_header_dirty) = normalize_loaded_header(header);

    let mut entries = Vec::new();
    let mut diagnostics = SessionOpenDiagnostics::default();

    // Gap E: parallel deserialization for large sessions.
    // Batch processing to bound memory usage while allowing parallelism.
    let num_threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));

    let mut line_batch: Vec<(usize, String)> = Vec::with_capacity(JSONL_PARSE_BATCH_SIZE);
    let mut current_line_num = 2; // Header is line 1

    loop {
        line_batch.clear();
        let mut batch_eof = false;

        for _ in 0..JSONL_PARSE_BATCH_SIZE {
            match read_capped_utf8_line(&mut reader) {
                Ok(None) => {
                    batch_eof = true;
                    break;
                }
                Ok(Some(line)) => {
                    if !line.trim().is_empty() {
                        line_batch.push((current_line_num, line));
                    }
                }
                Err(e) => {
                    diagnostics.skipped_entries.push(SessionOpenSkippedEntry {
                        line_number: current_line_num,
                        error: format!("IO error reading line: {e}"),
                    });
                }
            }
            current_line_num += 1;
        }

        if line_batch.is_empty() {
            if batch_eof {
                break;
            }
            continue;
        }

        if line_batch.len() >= PARALLEL_THRESHOLD && num_threads > 1 {
            let chunk_size = (line_batch.len() / num_threads).max(64);

            let chunk_results: Result<Vec<(Vec<SessionEntry>, Vec<SessionOpenSkippedEntry>)>> =
                std::thread::scope(|s| {
                    line_batch
                        .chunks(chunk_size)
                        .map(|chunk| {
                            s.spawn(move || {
                                let mut ok = Vec::with_capacity(chunk.len());
                                let mut skip = Vec::new();
                                for (line_num, line) in chunk {
                                    match serde_json::from_str::<SessionEntry>(line) {
                                        Ok(entry) => ok.push(entry),
                                        Err(e) => {
                                            skip.push(SessionOpenSkippedEntry {
                                                line_number: *line_num,
                                                error: e.to_string(),
                                            });
                                        }
                                    }
                                }
                                (ok, skip)
                            })
                        })
                        .collect::<Vec<_>>()
                        .into_iter()
                        .map(|h| {
                            h.join().map_err(|panic_payload| {
                                let panic_message =
                                    panic_payload.downcast_ref::<String>().map_or_else(
                                        || {
                                            panic_payload.downcast_ref::<&str>().map_or_else(
                                                || "unknown panic payload".to_string(),
                                                |message| (*message).to_string(),
                                            )
                                        },
                                        std::clone::Clone::clone,
                                    );
                                Error::session(format!(
                                    "parallel session parse worker panicked: {panic_message}"
                                ))
                            })
                        })
                        .collect()
                });
            let chunk_results = chunk_results?;

            for (chunk_entries, chunk_skipped) in chunk_results {
                entries.extend(chunk_entries);
                diagnostics.skipped_entries.extend(chunk_skipped);
            }
        } else {
            // Sequential path
            for (line_num, line) in &line_batch {
                match serde_json::from_str::<SessionEntry>(line) {
                    Ok(entry) => entries.push(entry),
                    Err(e) => {
                        diagnostics.skipped_entries.push(SessionOpenSkippedEntry {
                            line_number: *line_num,
                            error: e.to_string(),
                        });
                    }
                }
            }
        }

        if batch_eof {
            break;
        }
    }

    // --- Single-pass load finalization (Gap F) ---
    let finalized = finalize_loaded_entries(&mut entries);
    for orphan in &finalized.orphans {
        diagnostics
            .orphaned_parent_links
            .push(SessionOpenOrphanedParentLink {
                entry_id: orphan.0.clone(),
                missing_parent_id: orphan.1.clone(),
            });
    }

    let entry_count = entries.len();
    let natural_leaf_id = finalized.leaf_id.clone();
    let leaf_id = resolve_loaded_leaf_id(&header, natural_leaf_id.clone(), &finalized.entry_index);

    Ok((
        Session {
            header,
            entries,
            path: Some(path_buf),
            leaf_id: leaf_id.clone(),
            session_dir: None,
            store_kind: SessionStoreKind::Jsonl,
            entry_ids: finalized.entry_ids,
            is_linear: finalized.is_linear && leaf_id.eq(&natural_leaf_id),
            entry_index: finalized.entry_index,
            cached_message_count: finalized.message_count,
            cached_name: finalized.name,
            autosave_queue: AutosaveQueue::new(),
            autosave_durability: AutosaveDurabilityMode::from_env(),
            persisted_entry_count: Arc::new(AtomicUsize::new(entry_count)),
            header_dirty: normalized_header_dirty,
            appends_since_checkpoint: 0,
            v2_sidecar_root: None,
            v2_partial_hydration: false,
            v2_resume_mode: None,
            v2_sidecar_stale: false,
            v2_message_count_offset: 0,
        },
        diagnostics,
    ))
}

/// Open a session from its V2 sidecar store.
///
/// Reads the JSONL header (first line) for `SessionHeader`, then loads
/// entries from the V2 segment store via its offset index — O(index + tail)
/// instead of the O(n) full-file parse that `open_jsonl_blocking` performs.
#[allow(clippy::too_many_lines)]
fn open_from_v2_store_blocking(jsonl_path: PathBuf) -> Result<(Session, SessionOpenDiagnostics)> {
    // 1. Read JSONL header (first line only).
    let file = std::fs::File::open(&jsonl_path).map_err(|e| crate::Error::Io(Box::new(e)))?;
    let mut reader = BufReader::new(file);
    let Some(header_line) =
        read_capped_utf8_line(&mut reader).map_err(|e| crate::Error::Io(Box::new(e)))?
    else {
        return Err(crate::Error::session("Empty JSONL session file"));
    };
    let header: SessionHeader = serde_json::from_str(header_line.trim())
        .map_err(|e| crate::Error::session(format!("Invalid header in JSONL: {e}")))?;
    header.validate().map_err(|reason| {
        crate::Error::session(format!("Invalid session header in JSONL: {reason}"))
    })?;

    // 2. Open V2 sidecar store.
    let v2_root = session_store_v2::v2_sidecar_path(&jsonl_path);
    let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)?;

    // 3. Choose an explicit hydration strategy for resume:
    // - env override (PI_SESSION_V2_OPEN_MODE)
    // - auto lazy mode for large sessions
    let mode_override_raw = std::env::var("PI_SESSION_V2_OPEN_MODE").ok();
    let threshold_override_raw = std::env::var("PI_SESSION_V2_LAZY_THRESHOLD").ok();
    if let Some(raw) = mode_override_raw.as_deref() {
        if parse_v2_open_mode(raw).is_none() {
            tracing::warn!(
                value = %raw,
                "invalid PI_SESSION_V2_OPEN_MODE; using automatic hydration mode selection"
            );
        }
    }
    if let Some(raw) = threshold_override_raw.as_deref() {
        if raw.trim().parse::<u64>().is_err() {
            tracing::warn!(
                value = %raw,
                "invalid PI_SESSION_V2_LAZY_THRESHOLD; using default lazy hydration threshold"
            );
        }
    }

    let entry_count = store.entry_count();
    let (selected_mode, selection_reason, lazy_threshold) = select_v2_open_mode_for_resume(
        entry_count,
        mode_override_raw.as_deref(),
        threshold_override_raw.as_deref(),
    );
    let mode = if matches!(selected_mode, V2OpenMode::ActivePath)
        && entry_count > 0
        && store.head().is_none()
    {
        tracing::warn!(
            entry_count,
            "active-path hydration selected but store has no head; falling back to full hydration"
        );
        V2OpenMode::Full
    } else {
        selected_mode
    };
    tracing::debug!(
        entry_count,
        lazy_threshold,
        selection_reason,
        ?mode,
        "selected V2 resume hydration mode"
    );

    // 4. Load entries using the selected mode.
    let (mut session, diagnostics) = Session::open_from_v2(&store, header, mode)?;
    session.path = Some(jsonl_path);
    session.v2_sidecar_root = Some(v2_root);
    session.v2_partial_hydration = !matches!(mode, V2OpenMode::Full);
    session.v2_resume_mode = Some(mode);
    Ok((session, diagnostics))
}

/// Create a V2 sidecar store from an existing JSONL session file.
///
/// This is the migration path: parse the full JSONL once and write each entry
/// into the V2 segmented store with offset index. Subsequent opens can then
/// use `open_from_v2_store_blocking` for O(index+tail) resume.
pub fn create_v2_sidecar_from_jsonl(jsonl_path: &Path) -> Result<SessionStoreV2> {
    let v2_root = session_store_v2::v2_sidecar_path(jsonl_path);
    if !v2_root.exists() {
        return build_v2_sidecar_from_jsonl_into(jsonl_path, &v2_root);
    }

    let staging_root = unique_sidecar_aux_path(&v2_root, "staging");
    let _staged_store = match build_v2_sidecar_from_jsonl_into(jsonl_path, &staging_root) {
        Ok(store) => store,
        Err(err) => {
            let _ = cleanup_sidecar_root(&staging_root);
            return Err(err);
        }
    };

    let backup_root = unique_sidecar_aux_path(&v2_root, "backup");
    if let Err(err) = std::fs::rename(&v2_root, &backup_root) {
        let _ = cleanup_sidecar_root(&staging_root);
        return Err(crate::Error::Io(Box::new(err)));
    }

    if let Err(err) = std::fs::rename(&staging_root, &v2_root) {
        let _ = std::fs::rename(&backup_root, &v2_root);
        let _ = cleanup_sidecar_root(&staging_root);
        return Err(crate::Error::Io(Box::new(err)));
    }

    if let Err(err) = cleanup_sidecar_root(&backup_root) {
        tracing::warn!(
            path = %backup_root.display(),
            error = %err,
            "create_v2_sidecar_from_jsonl left backup sidecar after successful swap"
        );
    }

    SessionStoreV2::create(&v2_root, 64 * 1024 * 1024)
}

fn build_v2_sidecar_from_jsonl_into(jsonl_path: &Path, v2_root: &Path) -> Result<SessionStoreV2> {
    let build_result = (|| -> Result<SessionStoreV2> {
        let file = std::fs::File::open(jsonl_path).map_err(|e| crate::Error::Io(Box::new(e)))?;
        let mut reader = std::io::BufReader::new(file);

        let header_line = read_capped_utf8_line(&mut reader)
            .map_err(|e| crate::Error::Io(Box::new(e)))?
            .filter(|l| !l.trim().is_empty())
            .ok_or_else(|| crate::Error::session("Empty JSONL session file"))?;

        let header: SessionHeader = serde_json::from_str(header_line.trim())
            .map_err(|e| crate::Error::session(format!("Invalid header in JSONL: {e}")))?;
        header.validate().map_err(|reason| {
            crate::Error::session(format!("Invalid session header in JSONL: {reason}"))
        })?;

        if v2_root.exists() {
            std::fs::remove_dir_all(v2_root).map_err(|e| crate::Error::Io(Box::new(e)))?;
        }
        let mut store = SessionStoreV2::create(v2_root, 64 * 1024 * 1024)?;

        loop {
            let Some(line) =
                read_capped_utf8_line(&mut reader).map_err(|e| crate::Error::Io(Box::new(e)))?
            else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            let entry: SessionEntry = serde_json::from_str(&line)
                .map_err(|e| crate::Error::session(format!("Bad JSONL entry: {e}")))?;
            let (entry_id, parent_entry_id, entry_type, payload) =
                session_store_v2::session_entry_to_frame_args(&entry)?;
            store.append_entry(entry_id, parent_entry_id, entry_type, payload)?;
        }

        store.write_manifest(header.id, "jsonl")?;

        Ok(store)
    })();

    if build_result.is_err() && v2_root.exists() {
        let _ = std::fs::remove_dir_all(v2_root);
    }

    build_result
}

fn unique_sidecar_aux_path(v2_root: &Path, suffix: &str) -> PathBuf {
    let file_name = v2_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session.v2");
    v2_root.with_file_name(format!(
        "{file_name}.{suffix}.{}",
        uuid::Uuid::new_v4().simple()
    ))
}

fn cleanup_sidecar_root(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_dir_all(path).map_err(|e| crate::Error::Io(Box::new(e)))?;
    }
    Ok(())
}

/// Migrate a JSONL session to V2 with full verification and event logging.
///
/// Returns the `MigrationEvent` that was recorded in the V2 store's migration
/// ledger. The migration is atomic: if verification fails, the sidecar is
/// removed and an error is returned.
pub fn migrate_jsonl_to_v2(
    jsonl_path: &Path,
    correlation_id: &str,
) -> Result<session_store_v2::MigrationEvent> {
    let v2_root = session_store_v2::v2_sidecar_path(jsonl_path);
    let staging_root = unique_sidecar_aux_path(&v2_root, "staging");
    let store = match build_v2_sidecar_from_jsonl_into(jsonl_path, &staging_root) {
        Ok(store) => store,
        Err(err) => {
            let _ = cleanup_sidecar_root(&staging_root);
            return Err(err);
        }
    };

    // Verify fidelity.
    let verification = match verify_v2_against_jsonl(jsonl_path, &store) {
        Ok(verification) => verification,
        Err(err) => {
            let _ = cleanup_sidecar_root(&staging_root);
            return Err(err);
        }
    };

    if !(verification.entry_count_match
        && verification.hash_chain_match
        && verification.index_consistent)
    {
        // Verification failed — remove the sidecar.
        cleanup_sidecar_root(&staging_root)?;
        return Err(crate::Error::session(format!(
            "V2 migration verification failed: count={} hash={} index={}",
            verification.entry_count_match,
            verification.hash_chain_match,
            verification.index_consistent,
        )));
    }

    let event = session_store_v2::MigrationEvent {
        schema: session_store_v2::MIGRATION_EVENT_SCHEMA.to_string(),
        migration_id: uuid::Uuid::new_v4().to_string(),
        phase: "forward".to_string(),
        at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        source_path: jsonl_path.display().to_string(),
        target_path: session_store_v2::v2_sidecar_path(jsonl_path)
            .display()
            .to_string(),
        source_format: "jsonl_v3".to_string(),
        target_format: "native_v2".to_string(),
        verification,
        outcome: "ok".to_string(),
        error_class: None,
        correlation_id: correlation_id.to_string(),
    };
    if let Err(err) = store.append_migration_event(event.clone()) {
        let _ = cleanup_sidecar_root(&staging_root);
        return Err(err);
    }

    let backup_root = if v2_root.exists() {
        let backup_root = unique_sidecar_aux_path(&v2_root, "backup");
        if let Err(err) = std::fs::rename(&v2_root, &backup_root) {
            let _ = cleanup_sidecar_root(&staging_root);
            return Err(crate::Error::Io(Box::new(err)));
        }
        Some(backup_root)
    } else {
        None
    };

    if let Err(err) = std::fs::rename(&staging_root, &v2_root) {
        if let Some(backup_root) = backup_root.as_ref() {
            let _ = std::fs::rename(backup_root, &v2_root);
        }
        let _ = cleanup_sidecar_root(&staging_root);
        return Err(crate::Error::Io(Box::new(err)));
    }

    if let Some(backup_root) = backup_root {
        if let Err(err) = cleanup_sidecar_root(&backup_root) {
            tracing::warn!(
                path = %backup_root.display(),
                error = %err,
                "V2 migration left backup sidecar after successful swap"
            );
        }
    }

    Ok(event)
}

/// Verify a V2 sidecar against its source JSONL for fidelity.
///
/// Compares entry count, entry IDs in order, and validates the V2 store's
/// internal integrity (checksums + hash chain).
pub fn verify_v2_against_jsonl(
    jsonl_path: &Path,
    store: &SessionStoreV2,
) -> Result<session_store_v2::MigrationVerification> {
    // Parse all JSONL entries (skip header).
    let file = std::fs::File::open(jsonl_path).map_err(|e| crate::Error::Io(Box::new(e)))?;
    let mut reader = std::io::BufReader::new(file);

    let Some(header_line) =
        read_capped_utf8_line(&mut reader).map_err(|e| crate::Error::Io(Box::new(e)))?
    else {
        return Err(crate::Error::session("Empty JSONL session file"));
    };
    if header_line.trim().is_empty() {
        return Err(crate::Error::session("Empty JSONL session file"));
    }

    let header: SessionHeader = serde_json::from_str(header_line.trim())
        .map_err(|e| crate::Error::session(format!("Invalid header in JSONL: {e}")))?;
    header.validate().map_err(|reason| {
        crate::Error::session(format!("Invalid session header in JSONL: {reason}"))
    })?;

    let mut jsonl_ids: Vec<String> = Vec::new();
    let mut jsonl_chain_hash = V2_CHAIN_HASH_GENESIS.to_string();

    loop {
        let Some(line) =
            read_capped_utf8_line(&mut reader).map_err(|e| crate::Error::Io(Box::new(e)))?
        else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let entry: SessionEntry = serde_json::from_str(&line)
            .map_err(|e| crate::Error::session(format!("Bad JSONL entry: {e}")))?;
        let id = entry
            .base_id()
            .cloned()
            .ok_or_else(|| crate::Error::session("SessionEntry has no id"))?;
        jsonl_ids.push(id);
        jsonl_chain_hash = session_entry_chain_hash_step(&jsonl_chain_hash, &entry)?;
    }

    // Read V2 store entries.
    let frames = store.read_all_entries()?;
    let v2_ids: Vec<String> = frames.iter().map(|f| f.entry_id.clone()).collect();

    let entry_count_match = jsonl_ids.len().eq(&v2_ids.len()) && jsonl_ids.eq(&v2_ids);

    // Check hash chain via validate_integrity (which also verifies checksums).
    let index_consistent = store.validate_integrity().is_ok();

    let hash_chain_match = jsonl_chain_hash.eq(store.chain_hash());

    Ok(session_store_v2::MigrationVerification {
        entry_count_match,
        hash_chain_match,
        index_consistent,
    })
}

fn is_v2_sidecar_stale(jsonl_path: &Path, v2_root: &Path) -> bool {
    let Some(jsonl_meta) = std::fs::metadata(jsonl_path).ok() else {
        return true;
    };

    let v2_index = v2_root.join("index").join("offsets.jsonl");
    let v2_manifest = v2_root.join("manifest.json");
    let Some(v2_meta) = std::fs::metadata(&v2_index)
        .or_else(|_| std::fs::metadata(&v2_manifest))
        .ok()
    else {
        return true;
    };

    let Some(jsonl_mtime) = jsonl_meta.modified().ok() else {
        return true;
    };
    let Some(v2_mtime) = v2_meta.modified().ok() else {
        return true;
    };

    jsonl_mtime > v2_mtime
}

fn session_entry_chain_hash_step(prev_chain: &str, entry: &SessionEntry) -> Result<String> {
    let (_, _, _, payload) = session_store_v2::session_entry_to_frame_args(entry)?;
    let payload_sha256 = format!("{:x}", Sha256::digest(serde_json::to_vec(&payload)?));
    let mut hasher = Sha256::new();
    hasher.update(prev_chain.as_bytes());
    hasher.update(payload_sha256.as_bytes());
    Ok(format!("{:x}", hasher.finalize()))
}

/// Remove a V2 sidecar, reverting to JSONL-only storage.
///
/// Logs a rollback event in the migration ledger before removing the sidecar.
/// Returns `Ok(())` if the sidecar was removed (or didn't exist).
pub fn rollback_v2_sidecar(jsonl_path: &Path, correlation_id: &str) -> Result<()> {
    let v2_root = session_store_v2::v2_sidecar_path(jsonl_path);
    if !v2_root.exists() {
        return Ok(());
    }

    // Try to log the rollback event before deleting.
    if let Ok(store) = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024) {
        let event = session_store_v2::MigrationEvent {
            schema: session_store_v2::MIGRATION_EVENT_SCHEMA.to_string(),
            migration_id: uuid::Uuid::new_v4().to_string(),
            phase: "rollback_to_jsonl".to_string(),
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            source_path: v2_root.display().to_string(),
            target_path: jsonl_path.display().to_string(),
            source_format: "native_v2".to_string(),
            target_format: "jsonl_v3".to_string(),
            verification: session_store_v2::MigrationVerification {
                entry_count_match: true,
                hash_chain_match: true,
                index_consistent: true,
            },
            outcome: "ok".to_string(),
            error_class: None,
            correlation_id: correlation_id.to_string(),
        };
        let _ = store.append_migration_event(event);
    }

    std::fs::remove_dir_all(&v2_root).map_err(|e| crate::Error::Io(Box::new(e)))?;
    Ok(())
}

/// Current migration state of a JSONL session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationState {
    /// No V2 sidecar exists — pure JSONL.
    Unmigrated,
    /// V2 sidecar exists and passes integrity validation.
    Migrated,
    /// V2 sidecar exists but fails integrity validation.
    Corrupt { error: String },
    /// V2 sidecar directory exists but is missing critical files (partial write).
    Partial,
}

/// Query the migration state of a JSONL session file.
pub fn migration_status(jsonl_path: &Path) -> MigrationState {
    let v2_root = session_store_v2::v2_sidecar_path(jsonl_path);
    if !v2_root.exists() {
        return MigrationState::Unmigrated;
    }

    let segments_dir = v2_root.join("segments");
    if !segments_dir.exists() {
        return MigrationState::Partial;
    }

    let index_path = v2_root.join("index").join("offsets.jsonl");
    if !index_path.exists() {
        match jsonl_has_entry_lines(jsonl_path) {
            Ok(true) => return MigrationState::Partial,
            Ok(false) => {}
            Err(e) => {
                return MigrationState::Corrupt {
                    error: e.to_string(),
                };
            }
        }
    }

    let inspector = match SessionStoreV2::open_for_inspection(&v2_root, 64 * 1024 * 1024) {
        Ok(store) => store,
        Err(e) => {
            return MigrationState::Corrupt {
                error: e.to_string(),
            };
        }
    };

    match inspector.read_index() {
        Ok(_) => match inspector.validate_integrity() {
            Ok(()) => MigrationState::Migrated,
            Err(e) => MigrationState::Corrupt {
                error: e.to_string(),
            },
        },
        Err(e) if migration_status_can_rebuild_index(&e) => {
            match SessionStoreV2::create(&v2_root, 64 * 1024 * 1024) {
                Ok(store) => match verify_v2_against_jsonl(jsonl_path, &store) {
                    Ok(verification)
                        if verification.entry_count_match
                            && verification.hash_chain_match
                            && verification.index_consistent =>
                    {
                        MigrationState::Migrated
                    }
                    Ok(verification) => MigrationState::Corrupt {
                        error: format!(
                            "migration verification failed after index rebuild: count={} hash={} index={}",
                            verification.entry_count_match,
                            verification.hash_chain_match,
                            verification.index_consistent,
                        ),
                    },
                    Err(err) => MigrationState::Corrupt {
                        error: err.to_string(),
                    },
                },
                Err(err) => MigrationState::Corrupt {
                    error: err.to_string(),
                },
            }
        }
        Err(e) => MigrationState::Corrupt {
            error: e.to_string(),
        },
    }
}

fn migration_status_can_rebuild_index(error: &Error) -> bool {
    match error {
        Error::Json(_) => true,
        Error::Io(err) => matches!(
            err.kind(),
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::InvalidData
        ),
        _ => false,
    }
}

/// Dry-run a JSONL → V2 migration without persisting the sidecar.
///
/// Creates the V2 store in a temporary directory, runs verification, then
/// cleans up. Returns the verification result so callers can inspect
/// entry counts and integrity before committing.
pub fn migrate_dry_run(jsonl_path: &Path) -> Result<session_store_v2::MigrationVerification> {
    let tmp_dir =
        tempfile::tempdir().map_err(|e| crate::Error::session(format!("tempdir: {e}")))?;
    let tmp_v2_root = tmp_dir.path().join("dry_run.v2");

    // Parse JSONL and populate a temporary V2 store.
    let file = std::fs::File::open(jsonl_path).map_err(|e| crate::Error::Io(Box::new(e)))?;
    let mut reader = std::io::BufReader::new(file);

    let Some(header_line) =
        read_capped_utf8_line(&mut reader).map_err(|e| crate::Error::Io(Box::new(e)))?
    else {
        return Err(crate::Error::session("Empty JSONL session file"));
    };

    let header: SessionHeader = serde_json::from_str(header_line.trim_end())
        .map_err(|e| crate::Error::session(format!("Invalid header in JSONL: {e}")))?;
    header.validate().map_err(|reason| {
        crate::Error::session(format!("Invalid session header in JSONL: {reason}"))
    })?;

    let mut store = SessionStoreV2::create(&tmp_v2_root, 64 * 1024 * 1024)?;

    loop {
        let Some(line) =
            read_capped_utf8_line(&mut reader).map_err(|e| crate::Error::Io(Box::new(e)))?
        else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let entry: SessionEntry = serde_json::from_str(line.trim_end())
            .map_err(|e| crate::Error::session(format!("Bad JSONL entry: {e}")))?;
        let (entry_id, parent_entry_id, entry_type, payload) =
            session_store_v2::session_entry_to_frame_args(&entry)?;
        store.append_entry(entry_id, parent_entry_id, entry_type, payload)?;
    }

    // Verify against source JSONL (but using the temp store).
    verify_v2_against_jsonl(jsonl_path, &store)
    // tmp_dir drops here → auto-cleanup
}

/// Recover from a partial or corrupted V2 migration.
///
/// If the sidecar is in a partial/corrupt state, removes it and optionally
/// re-runs the migration. Returns the final migration state.
pub fn recover_partial_migration(
    jsonl_path: &Path,
    correlation_id: &str,
    re_migrate: bool,
) -> Result<MigrationState> {
    let status = migration_status(jsonl_path);
    match &status {
        MigrationState::Unmigrated | MigrationState::Migrated => Ok(status),
        MigrationState::Partial | MigrationState::Corrupt { .. } => {
            // Remove the broken sidecar.
            let v2_root = session_store_v2::v2_sidecar_path(jsonl_path);
            if v2_root.exists() {
                std::fs::remove_dir_all(&v2_root).map_err(|e| crate::Error::Io(Box::new(e)))?;
            }

            if re_migrate {
                migrate_jsonl_to_v2(jsonl_path, correlation_id)?;
                Ok(MigrationState::Migrated)
            } else {
                Ok(MigrationState::Unmigrated)
            }
        }
    }
}

fn jsonl_has_entry_lines(jsonl_path: &Path) -> Result<bool> {
    let file = std::fs::File::open(jsonl_path).map_err(|e| crate::Error::Io(Box::new(e)))?;
    let mut reader = std::io::BufReader::new(file);

    let Some(_line) =
        read_capped_utf8_line(&mut reader).map_err(|e| crate::Error::Io(Box::new(e)))?
    else {
        return Err(crate::Error::session("Empty JSONL session file"));
    };

    loop {
        let Some(line) =
            read_capped_utf8_line(&mut reader).map_err(|e| crate::Error::Io(Box::new(e)))?
        else {
            return Ok(false);
        };
        if !line.trim().is_empty() {
            return Ok(true);
        }
    }
}

/// Result of single-pass load finalization (Gap F).
///
/// Replaces the previous multi-pass approach (`ensure_entry_ids` +
/// `entry_id_set` + orphan detection + stats) with a single O(n) scan
/// that produces all required caches at once.
struct LoadFinalization {
    leaf_id: Option<String>,
    entry_ids: HashSet<String>,
    entry_index: HashMap<String, usize>,
    message_count: u64,
    name: Option<String>,
    is_linear: bool,
    orphans: Vec<(String, String)>,
}

/// Single-pass finalization of loaded entries.
///
/// 1. Assigns IDs to entries missing them (`ensure_entry_ids` work).
/// 2. Builds `entry_ids` set and `entry_index` map.
/// 3. Detects orphaned parent links.
/// 4. Computes `session_entry_stats` (message count + name).
/// 5. Determines `is_linear` (no branching, leaf == last entry).
fn finalize_loaded_entries(entries: &mut [SessionEntry]) -> LoadFinalization {
    // First pass: assign missing IDs (same logic as `ensure_entry_ids`).
    let mut entry_ids: HashSet<String> = entries
        .iter()
        .filter_map(|e| e.base_id().cloned())
        .collect();
    for entry in entries.iter_mut() {
        if entry.base().id.is_none() {
            let id = generate_entry_id(&entry_ids);
            entry.base_mut().id = Some(id.clone());
            entry_ids.insert(id);
        }
    }

    // Second (main) pass: build all caches in one scan.
    let mut entry_index = HashMap::with_capacity(entries.len());
    let mut message_count = 0u64;
    let mut name: Option<String> = None;
    let mut leaf_id: Option<String> = None;
    let mut orphans = Vec::new();
    // Track parent_ids seen as children's parent to detect branching.
    let mut parent_id_child_count: HashMap<Option<&str>, u32> = HashMap::new();
    let mut has_branching = false;
    let mut root_count = 0u32;

    for (idx, entry) in entries.iter().enumerate() {
        let Some(id) = entry.base_id() else {
            continue;
        };
        entry_index.insert(id.clone(), idx);
        leaf_id = Some(id.clone());

        // Orphan detection.
        if let Some(parent_id) = entry.base().parent_id.as_ref() {
            if !entry_ids.contains(parent_id) {
                orphans.push((id.clone(), parent_id.clone()));
            }
        } else {
            root_count += 1;
        }

        // Branch detection: if any parent_id has >1 child, it's branched.
        if !has_branching {
            let parent_key = entry.base().parent_id.as_deref();
            let count = parent_id_child_count.entry(parent_key).or_insert(0);
            *count += 1;
            if *count > 1 {
                has_branching = true;
            }
        }

        // Stats.
        match entry {
            SessionEntry::Message(_) => message_count += 1,
            SessionEntry::SessionInfo(info) if info.name.is_some() => {
                name.clone_from(&info.name);
            }
            _ => {}
        }
    }

    // is_linear: no branching detected in the entry set, exactly one root, and no orphans.
    // Note: callers (e.g. rebuild_all_caches) add the additional check that
    // self.leaf_id == finalized.leaf_id to confirm we're at the tip.
    let is_linear = !has_branching && root_count <= 1 && orphans.is_empty();

    LoadFinalization {
        leaf_id,
        entry_ids,
        entry_index,
        message_count,
        name,
        is_linear,
        orphans,
    }
}

fn parse_env_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn session_entry_id_cache_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("PI_SESSION_ENTRY_ID_CACHE").map_or(true, |value| parse_env_bool(&value))
    })
}

#[cfg(test)]
fn ensure_entry_ids(entries: &mut [SessionEntry]) {
    let mut existing = entry_id_set(entries);
    for entry in entries.iter_mut() {
        if entry.base().id.is_none() {
            let id = generate_entry_id(&existing);
            entry.base_mut().id = Some(id.clone());
            existing.insert(id);
        }
    }
}

/// Generate a unique entry ID (8 hex characters), falling back to UUID on collision.
fn generate_entry_id(existing: &HashSet<String>) -> String {
    for _ in 0..100 {
        let uuid = uuid::Uuid::new_v4();
        let id = uuid.simple().to_string()[..8].to_string();
        if !existing.contains(&id) {
            return id;
        }
    }
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
type SetNameDeadlineProbe = Option<(String, std::sync::mpsc::Sender<Option<asupersync::Time>>)>;

#[cfg(test)]
fn set_name_deadline_probe() -> &'static std::sync::Mutex<SetNameDeadlineProbe> {
    static PROBE: std::sync::OnceLock<std::sync::Mutex<SetNameDeadlineProbe>> =
        std::sync::OnceLock::new();
    PROBE.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
fn emit_set_name_deadline_probe(session_id: &str, deadline: Option<asupersync::Time>) {
    let probe = set_name_deadline_probe();
    let guard = probe.lock().expect("lock set_name deadline probe");
    if let Some((target_session_id, tx)) = guard.as_ref() {
        if target_session_id.eq(session_id) {
            let _ = tx.send(deadline);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Cost, StopReason, Usage};
    use asupersync::runtime::RuntimeBuilder;
    use asupersync::sync::Mutex as AsyncMutex;
    use clap::Parser;
    use std::env;
    use std::future::Future;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex as StdMutex, OnceLock};
    use std::time::Duration;

    macro_rules! test_fail {
        ($message:literal $(,)?) => {
            std::panic::panic_any($message)
        };
        ($fmt:literal, $($arg:tt)+) => {
            std::panic::panic_any(format!($fmt, $($arg)+))
        };
    }

    fn make_test_message(text: &str) -> SessionMessage {
        SessionMessage::User {
            content: UserContent::Text(text.to_string()),
            timestamp: Some(0),
        }
    }

    fn make_test_assistant_message(text: &str, total_tokens: u64) -> SessionMessage {
        SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(text.to_string()))],
                api: "test-api".to_string(),
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                usage: Usage {
                    input: total_tokens / 2,
                    output: total_tokens.saturating_sub(total_tokens / 2),
                    total_tokens,
                    ..Usage::default()
                },
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            },
        }
    }

    fn make_test_tool_call_message(tool_call_id: &str) -> SessionMessage {
        SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::ToolCall(crate::model::ToolCall {
                    id: tool_call_id.to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({ "path": "src/session.rs" }),
                    thought_signature: None,
                })],
                api: "test-api".to_string(),
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                usage: Usage {
                    input: 24,
                    output: 16,
                    total_tokens: 40,
                    ..Usage::default()
                },
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp: 0,
            },
        }
    }

    fn make_test_tool_result_message(tool_call_id: &str) -> SessionMessage {
        SessionMessage::ToolResult {
            tool_call_id: tool_call_id.to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "read output for replay harness".to_string(),
            ))],
            details: Some(serde_json::json!({
                "bytes": 31,
                "truncated": false,
            })),
            is_error: false,
            timestamp: Some(0),
        }
    }

    fn make_test_aborted_assistant_message(text: &str) -> SessionMessage {
        SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(text.to_string()))],
                api: "test-api".to_string(),
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                usage: Usage {
                    input: 10,
                    output: 6,
                    total_tokens: 16,
                    ..Usage::default()
                },
                stop_reason: StopReason::Aborted,
                error_message: Some("interrupted by local abort".to_string()),
                timestamp: 0,
            },
        }
    }

    fn run_async<T>(future: impl Future<Output = T>) -> T {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        runtime.block_on(future)
    }

    fn tempdir_under_tmpdir(prefix: &str) -> tempfile::TempDir {
        let tmp_root = env::var_os("TMPDIR").map_or_else(env::temp_dir, PathBuf::from);
        std::fs::create_dir_all(&tmp_root).expect("create TMPDIR root");
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(&tmp_root)
            .expect("create tempdir under TMPDIR")
    }

    fn current_dir_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| StdMutex::new(())).lock().expect("lock")
    }

    struct CurrentDirGuard {
        previous: PathBuf,
    }

    impl CurrentDirGuard {
        fn new(path: &Path) -> Self {
            let previous = env::current_dir().expect("current dir");
            env::set_current_dir(path).expect("set current dir");
            Self { previous }
        }
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.previous);
        }
    }

    #[test]
    fn v2_open_mode_parser_supports_expected_values() {
        assert_eq!(parse_v2_open_mode("full"), Some(V2OpenMode::Full));
        assert_eq!(parse_v2_open_mode("active"), Some(V2OpenMode::ActivePath));
        assert_eq!(
            parse_v2_open_mode("active_path"),
            Some(V2OpenMode::ActivePath)
        );
        assert_eq!(
            parse_v2_open_mode("active-path"),
            Some(V2OpenMode::ActivePath)
        );
        assert_eq!(
            parse_v2_open_mode("tail"),
            Some(V2OpenMode::Tail(DEFAULT_V2_TAIL_HYDRATION_COUNT))
        );
        assert_eq!(parse_v2_open_mode("tail:42"), Some(V2OpenMode::Tail(42)));
        assert_eq!(parse_v2_open_mode("tail:0"), Some(V2OpenMode::Tail(0)));
        assert_eq!(parse_v2_open_mode("bad-mode"), None);
        assert_eq!(parse_v2_open_mode("tail:not-a-number"), None);
    }

    #[test]
    fn v2_open_mode_selection_prefers_env_override_then_threshold() {
        let (mode, reason, threshold) = select_v2_open_mode_for_resume(50_000, Some("full"), None);
        assert_eq!(mode, V2OpenMode::Full);
        assert_eq!(reason, "env_override");
        assert_eq!(threshold, DEFAULT_V2_LAZY_HYDRATION_THRESHOLD);

        let (mode, reason, threshold) =
            select_v2_open_mode_for_resume(50_000, None, Some("not-a-number"));
        assert_eq!(
            mode,
            V2OpenMode::ActivePath,
            "invalid threshold falls back to default threshold"
        );
        assert_eq!(reason, "entry_count_above_lazy_threshold");
        assert_eq!(threshold, DEFAULT_V2_LAZY_HYDRATION_THRESHOLD);

        let (mode, reason, threshold) = select_v2_open_mode_for_resume(50_000, None, Some("500"));
        assert_eq!(mode, V2OpenMode::ActivePath);
        assert_eq!(reason, "entry_count_above_lazy_threshold");
        assert_eq!(threshold, 500);

        let (mode, reason, threshold) = select_v2_open_mode_for_resume(100, None, Some("500"));
        assert_eq!(mode, V2OpenMode::Full);
        assert_eq!(reason, "default_full");
        assert_eq!(threshold, 500);
    }

    #[test]
    fn v2_partial_hydration_rehydrates_before_header_rewrite_save() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("lazy_hydration_branching.jsonl");

        // Build a branching session:
        // root -> a -> b
        //           \-> c (active leaf)
        let mut seed = Session::create();
        seed.path = Some(path.clone());
        let _id_root = seed.append_message(make_test_message("root"));
        let id_a = seed.append_message(make_test_message("a"));
        let id_b = seed.append_message(make_test_message("main-branch"));
        assert!(seed.create_branch_from(&id_a));
        let id_c = seed.append_message(make_test_message("side-branch"));
        run_async(async { seed.save().await }).unwrap();

        // Build sidecar and reopen in ActivePath mode.
        create_v2_sidecar_from_jsonl(&path).unwrap();
        let v2_root = crate::session_store_v2::v2_sidecar_path(&path);
        let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024).unwrap();
        let (mut loaded, _) =
            Session::open_from_v2(&store, seed.header.clone(), V2OpenMode::ActivePath).unwrap();
        loaded.path = Some(path.clone());
        loaded.v2_sidecar_root = Some(v2_root);
        loaded.v2_partial_hydration = true;
        loaded.v2_resume_mode = Some(V2OpenMode::ActivePath);

        let active_ids: Vec<String> = loaded
            .entries
            .iter()
            .filter_map(|entry| entry.base().id.clone())
            .collect();
        assert!(
            !active_ids.contains(&id_b),
            "active path intentionally excludes non-leaf sibling branch"
        );
        assert!(active_ids.contains(&id_c));
        assert_eq!(
            loaded.cached_message_count, seed.cached_message_count,
            "active-path resume should retain total message count metadata"
        );
        assert!(
            loaded.v2_message_count_offset > 0,
            "active-path resume should track hidden messages outside the active path"
        );

        // Force full rewrite path (header dirty). Save must rehydrate first so b survives.
        loaded.set_model_header(Some("provider-updated".to_string()), None, None);
        run_async(async { loaded.save().await }).unwrap();

        let (reopened, _) =
            run_async(async { Session::open_jsonl_with_diagnostics(&path).await }).unwrap();
        let reopened_ids: Vec<String> = reopened
            .entries
            .iter()
            .filter_map(|entry| entry.base().id.clone())
            .collect();
        assert!(
            reopened_ids.contains(&id_b),
            "non-active branch entry must survive full rewrite after lazy hydration"
        );
        assert!(reopened_ids.contains(&id_c));
        assert_eq!(reopened_ids.len(), 4);
    }

    #[test]
    fn v2_partial_hydration_save_keeps_pending_entries_after_rehydrate() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("lazy_hydration_pending_merge.jsonl");

        let mut seed = Session::create();
        seed.path = Some(path.clone());
        let _id_root = seed.append_message(make_test_message("root"));
        let id_a = seed.append_message(make_test_message("a"));
        let id_b = seed.append_message(make_test_message("main-branch"));
        assert!(seed.create_branch_from(&id_a));
        let _id_c = seed.append_message(make_test_message("side-branch"));
        run_async(async { seed.save().await }).unwrap();

        create_v2_sidecar_from_jsonl(&path).unwrap();
        let v2_root = crate::session_store_v2::v2_sidecar_path(&path);
        let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024).unwrap();
        let (mut loaded, _) =
            Session::open_from_v2(&store, seed.header.clone(), V2OpenMode::ActivePath).unwrap();
        loaded.path = Some(path.clone());
        loaded.v2_sidecar_root = Some(v2_root);
        loaded.v2_partial_hydration = true;
        loaded.v2_resume_mode = Some(V2OpenMode::ActivePath);

        let new_id = loaded.append_message(make_test_message("new-on-active-leaf"));
        loaded.set_model_header(Some("provider-updated".to_string()), None, None);
        run_async(async { loaded.save().await }).unwrap();

        let (reopened, _) =
            run_async(async { Session::open_jsonl_with_diagnostics(&path).await }).unwrap();
        let reopened_ids: Vec<String> = reopened
            .entries
            .iter()
            .filter_map(|entry| entry.base().id.clone())
            .collect();
        assert!(
            reopened_ids.contains(&id_b),
            "non-active branch entry must survive rehydration+save"
        );
        assert!(
            reopened_ids.contains(&new_id),
            "pending entry appended on partial session must be preserved"
        );
        assert_eq!(reopened_ids.len(), 5);
    }

    #[test]
    fn v2_partial_hydration_full_rewrite_uses_newer_jsonl_when_sidecar_is_stale() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("lazy_hydration_stale_sidecar.jsonl");

        let mut seed = Session::create();
        seed.path = Some(path.clone());
        let _id_root = seed.append_message(make_test_message("root"));
        let id_a = seed.append_message(make_test_message("a"));
        let id_b = seed.append_message(make_test_message("main-branch"));
        assert!(seed.create_branch_from(&id_a));
        let _id_c = seed.append_message(make_test_message("side-branch"));
        run_async(async { seed.save().await }).unwrap();

        create_v2_sidecar_from_jsonl(&path).unwrap();
        let v2_root = crate::session_store_v2::v2_sidecar_path(&path);
        let store = SessionStoreV2::create(&v2_root, 64 * 1024 * 1024).unwrap();
        let (mut loaded, _) =
            Session::open_from_v2(&store, seed.header.clone(), V2OpenMode::ActivePath).unwrap();
        loaded.path = Some(path.clone());
        loaded.v2_sidecar_root = Some(v2_root.clone());
        loaded.v2_partial_hydration = true;
        loaded.v2_resume_mode = Some(V2OpenMode::ActivePath);

        std::thread::sleep(std::time::Duration::from_secs(1));
        let new_id = loaded.append_message(make_test_message("saved-before-full-rewrite"));
        run_async(async { loaded.save().await }).unwrap();
        assert!(
            is_v2_sidecar_stale(&path, &v2_root),
            "incremental JSONL save should make sidecar stale"
        );

        loaded.set_model_header(Some("provider-updated".to_string()), None, None);
        run_async(async { loaded.save().await }).unwrap();

        let (reopened, _) =
            run_async(async { Session::open_jsonl_with_diagnostics(&path).await }).unwrap();
        let reopened_ids: Vec<String> = reopened
            .entries
            .iter()
            .filter_map(|entry| entry.base().id.clone())
            .collect();
        assert!(
            reopened_ids.contains(&id_b),
            "non-active branch entry must survive full rewrite after stale sidecar"
        );
        assert!(
            reopened_ids.contains(&new_id),
            "entry already saved to JSONL must not be dropped during rehydrate"
        );
        assert_eq!(reopened_ids.len(), 5);
    }

    #[test]
    fn verify_v2_against_jsonl_detects_payload_mismatch_with_matching_ids() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("verify_v2_payload_mismatch.jsonl");

        let mut session = Session::create();
        session.path = Some(path.clone());
        session.append_message(make_test_message("alpha"));
        session.append_message(make_test_message("beta"));
        run_async(async { session.save().await }).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let mut lines = contents.lines();
        let _header_line = lines.next().expect("header");
        let mut tampered_entries: Vec<SessionEntry> = lines
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("parse session entry"))
            .collect();

        let SessionEntry::Message(message_entry) = tampered_entries
            .first_mut()
            .expect("first tampered entry should exist")
        else {
            test_fail!("expected message entry");
        };
        let SessionMessage::User {
            content: UserContent::Text(text),
            ..
        } = &mut message_entry.message
        else {
            test_fail!("expected user text message");
        };
        *text = "alpha-tampered".to_string();

        let tampered_root = temp_dir.path().join("verify_v2_payload_mismatch.v2");
        let mut tampered_store = SessionStoreV2::create(&tampered_root, 64 * 1024 * 1024).unwrap();
        for entry in &tampered_entries {
            let (entry_id, parent_entry_id, entry_type, payload) =
                session_store_v2::session_entry_to_frame_args(entry).unwrap();
            tampered_store
                .append_entry(entry_id, parent_entry_id, entry_type, payload)
                .unwrap();
        }

        let verification = verify_v2_against_jsonl(&path, &tampered_store).unwrap();
        assert!(verification.entry_count_match);
        assert!(verification.index_consistent);
        assert!(
            !verification.hash_chain_match,
            "payload divergence must fail migration verification even when entry ids match"
        );
    }

    #[test]
    fn test_session_handle_mutations_defer_persistence_side_effects() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session::create();
        session.set_autosave_durability_for_test(AutosaveDurabilityMode::Throughput);
        // Point at a directory path so an eager save would fail with an IO error.
        session.path = Some(temp_dir.path().to_path_buf());
        let handle = SessionHandle(Arc::new(AsyncMutex::new(session)));

        run_async(async { handle.set_name("deferred-save".to_string()).await })
            .expect("set_name should not trigger immediate save");
        run_async(async { handle.append_message(make_test_message("hello")).await })
            .expect("append_message should not trigger immediate save");
        run_async(async {
            handle
                .append_custom_entry(
                    "marker".to_string(),
                    Some(serde_json::json!({ "value": 42 })),
                )
                .await
        })
        .expect("append_custom_entry should not trigger immediate save");
        run_async(async {
            handle
                .set_model("prov".to_string(), "model".to_string())
                .await
        })
        .expect("set_model should not trigger immediate save");
        run_async(async { handle.set_thinking_level("high".to_string()).await })
            .expect("set_thinking_level should not trigger immediate save");

        let branch = run_async(async { handle.get_branch().await });
        let message_id = branch
            .iter()
            .find_map(|entry| {
                if entry
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|entry_type| entry_type.eq("message"))
                {
                    entry
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                } else {
                    None
                }
            })
            .expect("message entry id in branch");
        run_async(async {
            handle
                .set_label(message_id, Some("hot-path".to_string()))
                .await
        })
        .expect("set_label should not trigger immediate save");

        let state = run_async(async { handle.get_state().await });
        assert_eq!(
            state.get("sessionName").and_then(Value::as_str),
            Some("deferred-save")
        );
        assert_eq!(
            state.get("thinkingLevel").and_then(Value::as_str),
            Some("high")
        );
        assert_eq!(
            state.get("durabilityMode").and_then(Value::as_str),
            Some("throughput")
        );
        assert_eq!(state.get("messageCount").and_then(Value::as_u64), Some(1));
        assert_eq!(
            state
                .get("model")
                .and_then(|model| model.get("provider"))
                .and_then(Value::as_str),
            Some("prov")
        );
        assert_eq!(
            state
                .get("model")
                .and_then(|model| model.get("id"))
                .and_then(Value::as_str),
            Some("model")
        );

        let (provider, model_id) = run_async(async { handle.get_model().await });
        assert_eq!(provider.as_deref(), Some("prov"));
        assert_eq!(model_id.as_deref(), Some("model"));
    }

    #[test]
    fn session_handle_set_name_inherits_cancelled_context_when_lock_is_held() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            let session = Arc::new(AsyncMutex::new(Session::in_memory()));
            let handle = SessionHandle(Arc::clone(&session));

            let hold_cx = AgentCx::for_request();
            let held_guard = session.lock(hold_cx.cx()).await.expect("lock session");

            let ambient_cx = asupersync::Cx::for_testing();
            ambient_cx.set_cancel_requested(true);
            let _current = asupersync::Cx::set_current(Some(ambient_cx));
            let inner = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_millis(100),
                handle.set_name("cancelled-name".to_string()),
            )
            .await;
            let outcome = inner.expect("cancelled helper should finish before timeout");
            let err = outcome.expect_err("lock acquisition should honor inherited cancellation");
            assert!(
                err.to_string().contains("Failed to lock session"),
                "unexpected error: {err}"
            );

            drop(held_guard);

            let state = SessionHandle(Arc::clone(&session)).get_state().await;
            assert!(
                state.get("sessionName").is_none_or(Value::is_null),
                "cancelled mutation should not update the session name: {state:?}"
            );
        });
    }

    #[test]
    fn session_handle_set_name_inherits_deadline() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            struct ProbeReset;
            impl Drop for ProbeReset {
                fn drop(&mut self) {
                    let mut probe = set_name_deadline_probe()
                        .lock()
                        .expect("lock set_name deadline probe");
                    *probe = None;
                }
            }

            let session_state = Session::in_memory();
            let probe_session_id = session_state.header.id.clone();
            let session = Arc::new(AsyncMutex::new(session_state));
            let handle = SessionHandle(Arc::clone(&session));

            let (probe_tx, probe_rx) = std::sync::mpsc::channel();
            {
                let mut probe = set_name_deadline_probe()
                    .lock()
                    .expect("lock set_name deadline probe");
                assert!(probe.is_none(), "set_name deadline probe already installed");
                *probe = Some((probe_session_id, probe_tx));
            }
            let _probe_reset = ProbeReset;

            let expected_deadline = asupersync::time::wall_now() + Duration::from_secs(30);
            let ambient_cx = AgentCx::for_request_with_budget(asupersync::Budget {
                deadline: Some(expected_deadline),
                ..asupersync::Budget::INFINITE
            });
            let _current = asupersync::Cx::set_current(Some(ambient_cx.cx().clone()));
            handle
                .set_name("deadline-name".to_string())
                .await
                .expect("set_name should succeed with inherited deadline");

            let recorded = probe_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("set_name deadline probe");
            assert_eq!(recorded, Some(expected_deadline));

            let state = SessionHandle(Arc::clone(&session)).get_state().await;
            assert_eq!(
                state.get("sessionName").and_then(Value::as_str),
                Some("deadline-name")
            );
        });
    }

    #[test]
    fn test_session_handle_set_model_and_thinking_level_dedupe_history() {
        let handle = SessionHandle(Arc::new(AsyncMutex::new(Session::in_memory())));

        run_async(async {
            handle
                .set_model("anthropic".to_string(), "claude-sonnet-4-5".to_string())
                .await
        })
        .expect("set model");
        run_async(async {
            handle
                .set_model("anthropic".to_string(), "claude-sonnet-4-5".to_string())
                .await
        })
        .expect("repeat model");
        run_async(async { handle.set_thinking_level("high".to_string()).await })
            .expect("set thinking");
        run_async(async { handle.set_thinking_level("high".to_string()).await })
            .expect("repeat thinking");

        let branch = run_async(async { handle.get_branch().await });
        let model_changes = branch
            .iter()
            .filter(|entry| {
                entry
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|entry_type| entry_type.eq("model_change"))
            })
            .count();
        let thinking_changes = branch
            .iter()
            .filter(|entry| {
                entry
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|entry_type| entry_type.eq("thinking_level_change"))
            })
            .count();
        assert_eq!(model_changes, 1);
        assert_eq!(thinking_changes, 1);
    }

    #[test]
    fn test_session_handle_preserves_alias_equivalent_model_state() {
        let mut session = Session::in_memory();
        session.append_model_change("google".to_string(), "gemini-2.5-pro".to_string());
        session.set_model_header(
            Some("google".to_string()),
            Some("gemini-2.5-pro".to_string()),
            None,
        );
        let handle = SessionHandle(Arc::new(AsyncMutex::new(session)));

        run_async(async {
            handle
                .set_model("gemini".to_string(), "GEMINI-2.5-PRO".to_string())
                .await
        })
        .expect("alias-equivalent model should dedupe");

        let branch = run_async(async { handle.get_branch().await });
        let model_changes: Vec<_> = branch
            .iter()
            .filter_map(|entry| {
                if entry
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|entry_type| entry_type.eq("model_change"))
                {
                    Some((
                        entry.get("provider").and_then(Value::as_str),
                        entry.get("modelId").and_then(Value::as_str),
                    ))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            model_changes,
            vec![(Some("google"), Some("gemini-2.5-pro"))],
            "alias-equivalent set_model should not append duplicate history"
        );

        let (provider, model_id) = run_async(async { handle.get_model().await });
        assert_eq!(provider.as_deref(), Some("google"));
        assert_eq!(model_id.as_deref(), Some("gemini-2.5-pro"));

        let state = run_async(async { handle.get_state().await });
        assert_eq!(state["model"]["provider"], "google");
        assert_eq!(state["model"]["id"], "gemini-2.5-pro");
    }

    #[test]
    fn session_handle_reports_branch_local_model_and_thinking_state() {
        let mut session = Session::in_memory();
        let root_id = session.append_message(make_test_message("root"));

        session.append_model_change("openai".to_string(), "gpt-4o".to_string());
        let branch_a_thinking = session.append_thinking_level_change("low".to_string());
        session.set_model_header(
            Some("openai".to_string()),
            Some("gpt-4o".to_string()),
            Some("low".to_string()),
        );

        assert!(session.create_branch_from(&root_id));
        session.append_model_change("anthropic".to_string(), "claude-sonnet-4-5".to_string());
        session.append_thinking_level_change("high".to_string());
        session.set_model_header(
            Some("anthropic".to_string()),
            Some("claude-sonnet-4-5".to_string()),
            Some("high".to_string()),
        );

        assert!(session.navigate_to(&branch_a_thinking));

        let handle = SessionHandle(Arc::new(AsyncMutex::new(session)));
        let state = run_async(async { handle.get_state().await });
        let (provider, model_id) = run_async(async { handle.get_model().await });
        let thinking_level = run_async(async { handle.get_thinking_level().await });

        assert_eq!(provider.as_deref(), Some("openai"));
        assert_eq!(model_id.as_deref(), Some("gpt-4o"));
        assert_eq!(thinking_level.as_deref(), Some("low"));
        assert_eq!(
            state
                .get("model")
                .and_then(|model| model.get("provider"))
                .and_then(Value::as_str),
            Some("openai")
        );
        assert_eq!(
            state
                .get("model")
                .and_then(|model| model.get("id"))
                .and_then(Value::as_str),
            Some("gpt-4o")
        );
        assert_eq!(
            state.get("thinkingLevel").and_then(Value::as_str),
            Some("low")
        );
    }

    #[test]
    fn session_handle_set_model_and_thinking_level_dedupe_on_switched_branch() {
        let mut session = Session::in_memory();
        let root_id = session.append_message(make_test_message("root"));

        session.append_model_change("openai".to_string(), "gpt-4o".to_string());
        let branch_a_thinking = session.append_thinking_level_change("low".to_string());
        session.set_model_header(
            Some("openai".to_string()),
            Some("gpt-4o".to_string()),
            Some("low".to_string()),
        );

        assert!(session.create_branch_from(&root_id));
        session.append_model_change("anthropic".to_string(), "claude-sonnet-4-5".to_string());
        session.append_thinking_level_change("high".to_string());
        session.set_model_header(
            Some("anthropic".to_string()),
            Some("claude-sonnet-4-5".to_string()),
            Some("high".to_string()),
        );

        assert!(session.navigate_to(&branch_a_thinking));

        let handle = SessionHandle(Arc::new(AsyncMutex::new(session)));

        run_async(async {
            handle
                .set_model("openai".to_string(), "gpt-4o".to_string())
                .await
        })
        .expect("same-branch model should dedupe");
        run_async(async { handle.set_thinking_level("low".to_string()).await })
            .expect("same-branch thinking should dedupe");

        let branch = run_async(async { handle.get_branch().await });
        let model_changes = branch
            .iter()
            .filter(|entry| {
                entry
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|entry_type| entry_type.eq("model_change"))
            })
            .count();
        let thinking_changes = branch
            .iter()
            .filter(|entry| {
                entry
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|entry_type| entry_type.eq("thinking_level_change"))
            })
            .count();

        assert_eq!(model_changes, 1, "expected one branch-local model_change");
        assert_eq!(
            thinking_changes, 1,
            "expected one branch-local thinking_level_change"
        );
    }

    #[test]
    fn test_autosave_queue_coalesces_mutations_per_flush() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session::create();
        session.path = Some(temp_dir.path().join("autosave-coalesce.jsonl"));

        session.append_message(make_test_message("one"));
        session.append_custom_entry("marker".to_string(), None);
        session.append_message(make_test_message("two"));

        let before = session.autosave_metrics();
        assert_eq!(before.pending_mutations, 3);
        assert!(before.coalesced_mutations >= 2);
        assert_eq!(before.flush_succeeded, 0);

        run_async(async { session.flush_autosave(AutosaveFlushTrigger::Periodic).await })
            .expect("periodic flush");

        let after = session.autosave_metrics();
        assert_eq!(after.pending_mutations, 0);
        assert_eq!(after.flush_started, 1);
        assert_eq!(after.flush_succeeded, 1);
        assert_eq!(after.last_flush_batch_size, 3);
        assert_eq!(
            after.last_flush_trigger,
            Some(AutosaveFlushTrigger::Periodic)
        );
    }

    #[test]
    fn test_autosave_queue_backpressure_is_bounded() {
        let mut session = Session::create();
        session.set_autosave_queue_limit_for_test(2);

        for i in 0..5 {
            session.append_message(make_test_message(&format!("message-{i}")));
        }

        let metrics = session.autosave_metrics();
        assert_eq!(metrics.max_pending_mutations, 2);
        assert_eq!(metrics.pending_mutations, 2);
        assert_eq!(metrics.backpressure_events, 3);
        assert!(metrics.coalesced_mutations >= 4);
    }

    #[test]
    fn test_autosave_shutdown_flush_semantics_follow_durability_mode() {
        let temp_dir = tempfile::tempdir().expect("temp dir");

        let mut strict = Session::create();
        // Point at a directory path so strict shutdown flush attempts fail.
        strict.path = Some(temp_dir.path().to_path_buf());
        strict.set_autosave_durability_for_test(AutosaveDurabilityMode::Strict);
        strict.append_message(make_test_message("strict"));

        run_async(async { strict.flush_autosave_on_shutdown().await })
            .expect_err("strict mode should propagate shutdown flush failure");
        let strict_metrics = strict.autosave_metrics();
        assert_eq!(strict_metrics.flush_failed, 1);
        assert!(strict_metrics.pending_mutations > 0);

        let mut throughput = Session::create();
        throughput.path = Some(temp_dir.path().to_path_buf());
        throughput.set_autosave_durability_for_test(AutosaveDurabilityMode::Throughput);
        throughput.append_message(make_test_message("throughput"));

        run_async(async { throughput.flush_autosave_on_shutdown().await })
            .expect("throughput mode skips shutdown flush");
        let throughput_metrics = throughput.autosave_metrics();
        assert_eq!(throughput_metrics.flush_started, 0);
        assert_eq!(throughput_metrics.pending_mutations, 1);
    }

    #[test]
    fn test_session_new_prefers_cli_durability_mode_over_config() {
        let cli =
            crate::cli::Cli::parse_from(["pi", "--no-session", "--session-durability", "strict"]);
        let config: Config =
            serde_json::from_str(r#"{ "sessionDurability": "throughput" }"#).expect("config parse");
        let session =
            run_async(async { Session::new(&cli, &config).await }).expect("create session");
        assert_eq!(
            session.autosave_durability_mode(),
            AutosaveDurabilityMode::Strict
        );
    }

    #[test]
    fn test_session_new_uses_config_durability_mode_when_cli_unset() {
        let cli = crate::cli::Cli::parse_from(["pi", "--no-session"]);
        let config: Config =
            serde_json::from_str(r#"{ "sessionDurability": "throughput" }"#).expect("config parse");
        let session =
            run_async(async { Session::new(&cli, &config).await }).expect("create session");
        assert_eq!(
            session.autosave_durability_mode(),
            AutosaveDurabilityMode::Throughput
        );
    }

    #[test]
    fn test_resolve_autosave_durability_mode_precedence() {
        assert_eq!(
            resolve_autosave_durability_mode(Some("strict"), Some("throughput"), Some("balanced")),
            AutosaveDurabilityMode::Strict
        );
        assert_eq!(
            resolve_autosave_durability_mode(None, Some("throughput"), Some("strict")),
            AutosaveDurabilityMode::Throughput
        );
        assert_eq!(
            resolve_autosave_durability_mode(None, None, Some("strict")),
            AutosaveDurabilityMode::Strict
        );
        assert_eq!(
            resolve_autosave_durability_mode(None, None, None),
            AutosaveDurabilityMode::Balanced
        );
    }

    #[test]
    fn test_resolve_autosave_durability_mode_ignores_invalid_values() {
        assert_eq!(
            resolve_autosave_durability_mode(Some("bad"), Some("throughput"), Some("strict")),
            AutosaveDurabilityMode::Throughput
        );
        assert_eq!(
            resolve_autosave_durability_mode(None, Some("bad"), Some("strict")),
            AutosaveDurabilityMode::Strict
        );
        assert_eq!(
            resolve_autosave_durability_mode(None, None, Some("bad")),
            AutosaveDurabilityMode::Balanced
        );
    }

    #[test]
    fn test_get_share_viewer_url_matches_legacy() {
        assert_eq!(
            build_share_viewer_url(None, "gist-123"),
            "https://buildwithpi.ai/session/#gist-123"
        );
        assert_eq!(
            build_share_viewer_url(Some("https://example.com/session/"), "gist-123"),
            "https://example.com/session/#gist-123"
        );
        assert_eq!(
            build_share_viewer_url(Some("https://example.com/session"), "gist-123"),
            "https://example.com/session#gist-123"
        );
        // Legacy JS uses `process.env.PI_SHARE_VIEWER_URL || DEFAULT`, so empty-string should
        // fall back to default.
        assert_eq!(
            build_share_viewer_url(Some(""), "gist-123"),
            "https://buildwithpi.ai/session/#gist-123"
        );
    }

    #[test]
    fn test_session_linear_history() {
        let mut session = Session::in_memory();

        let id1 = session.append_message(make_test_message("Hello"));
        let id2 = session.append_message(make_test_message("World"));
        let id3 = session.append_message(make_test_message("Test"));

        // Check leaf is the last entry
        assert_eq!(session.leaf_id.as_deref(), Some(id3.as_str()));

        // Check path from last entry
        let path = session.get_path_to_entry(&id3);
        assert_eq!(path, vec![id1.as_str(), id2.as_str(), id3.as_str()]);

        // Check only one leaf
        let leaves = session.list_leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0], id3);
    }

    #[test]
    fn test_session_branching() {
        let mut session = Session::in_memory();

        // Create linear history: A -> B -> C
        let id_a = session.append_message(make_test_message("A"));
        let id_b = session.append_message(make_test_message("B"));
        let id_c = session.append_message(make_test_message("C"));

        // Now branch from B: A -> B -> D
        assert!(session.create_branch_from(&id_b));
        let id_d = session.append_message(make_test_message("D"));

        // Should have 2 leaves: C and D
        let leaves = session.list_leaves();
        assert_eq!(leaves.len(), 2);
        assert!(leaves.contains(&id_c));
        assert!(leaves.contains(&id_d));

        // Path to D should be A -> B -> D
        let path_to_d = session.get_path_to_entry(&id_d);
        assert_eq!(path_to_d, vec![id_a.as_str(), id_b.as_str(), id_d.as_str()]);

        // Path to C should be A -> B -> C
        let path_to_c = session.get_path_to_entry(&id_c);
        assert_eq!(path_to_c, vec![id_a.as_str(), id_b.as_str(), id_c.as_str()]);
    }

    #[test]
    fn test_session_navigation() {
        let mut session = Session::in_memory();

        let id1 = session.append_message(make_test_message("First"));
        let id2 = session.append_message(make_test_message("Second"));

        // Navigate to first entry
        assert!(session.navigate_to(&id1));
        assert_eq!(session.leaf_id.as_deref(), Some(id1.as_str()));

        // Navigate to non-existent entry
        assert!(!session.navigate_to("nonexistent"));
        // leaf_id unchanged
        assert_eq!(session.leaf_id.as_deref(), Some(id1.as_str()));

        // Navigate back to second
        assert!(session.navigate_to(&id2));
        assert_eq!(session.leaf_id.as_deref(), Some(id2.as_str()));
    }

    #[test]
    fn test_navigation_syncs_header_to_current_branch_metadata() {
        let mut session = Session::in_memory();

        let root_id = session.append_message(make_test_message("root"));
        let openai_id = session.append_model_change("openai".to_string(), "gpt-5.4".to_string());
        let high_id = session.append_thinking_level_change("high".to_string());
        let _tip_a = session.append_message(make_test_message("branch-a"));

        assert!(session.create_branch_from(&root_id));
        session.append_model_change("anthropic".to_string(), "claude-sonnet-4".to_string());
        let minimal_id = session.append_thinking_level_change("minimal".to_string());
        let _tip_b = session.append_message(make_test_message("branch-b"));

        assert!(session.navigate_to(&high_id));
        assert_eq!(session.header.provider.as_deref(), Some("openai"));
        assert_eq!(session.header.model_id.as_deref(), Some("gpt-5.4"));
        assert_eq!(session.header.thinking_level.as_deref(), Some("high"));

        assert!(session.navigate_to(&minimal_id));
        assert_eq!(session.header.provider.as_deref(), Some("anthropic"));
        assert_eq!(session.header.model_id.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(session.header.thinking_level.as_deref(), Some("minimal"));

        assert!(session.navigate_to(&openai_id));
        assert_eq!(session.header.provider.as_deref(), Some("openai"));
        assert_eq!(session.header.model_id.as_deref(), Some("gpt-5.4"));
    }

    #[test]
    fn test_navigation_clears_stale_header_metadata_when_target_branch_has_no_override() {
        let mut session = Session::in_memory();

        let root_id = session.append_message(make_test_message("root"));
        let branch_a_tip = session.append_message(make_test_message("branch-a"));

        assert!(session.create_branch_from(&root_id));
        session.append_model_change("anthropic".to_string(), "claude-sonnet-4".to_string());
        session.append_thinking_level_change("high".to_string());
        session.set_model_header(
            Some("anthropic".to_string()),
            Some("claude-sonnet-4".to_string()),
            Some("high".to_string()),
        );

        assert!(session.navigate_to(&branch_a_tip));
        assert!(session.header.provider.is_none());
        assert!(session.header.model_id.is_none());
        assert!(session.header.thinking_level.is_none());
    }

    #[test]
    fn test_open_materializes_header_fallback_for_historyless_branch_navigation() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("legacy-historyless-branch.jsonl");

        let mut legacy = Session::in_memory();
        legacy.header.provider = Some("openai".to_string());
        legacy.header.model_id = Some("gpt-5.4".to_string());
        legacy.header.thinking_level = Some("low".to_string());

        let root_id = legacy.append_message(make_test_message("root"));
        let branch_b_tip = legacy.append_message(make_test_message("branch-b"));

        assert!(legacy.create_branch_from(&root_id));
        legacy.append_model_change("anthropic".to_string(), "claude-sonnet-4".to_string());
        legacy.append_thinking_level_change("high".to_string());
        let branch_a_tip = legacy.append_message(make_test_message("branch-a"));

        legacy.header.current_leaf = Some(branch_b_tip.clone());

        let mut jsonl = serde_json::to_string(&legacy.header).expect("serialize legacy header");
        jsonl.push('\n');
        for entry in &legacy.entries {
            jsonl.push_str(&serde_json::to_string(entry).expect("serialize session entry"));
            jsonl.push('\n');
        }
        std::fs::write(&path, jsonl).expect("write legacy session");

        let mut loaded = run_async(async { Session::open(path.to_string_lossy().as_ref()).await })
            .expect("open legacy session");

        assert_eq!(loaded.leaf_id.as_deref(), Some(branch_b_tip.as_str()));
        assert_eq!(loaded.header.fallback_provider.as_deref(), Some("openai"));
        assert_eq!(loaded.header.fallback_model_id.as_deref(), Some("gpt-5.4"));
        assert_eq!(
            loaded.header.fallback_thinking_level.as_deref(),
            Some("low")
        );

        assert!(loaded.navigate_to(&branch_a_tip));
        assert_eq!(loaded.header.provider.as_deref(), Some("anthropic"));
        assert_eq!(loaded.header.model_id.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(loaded.header.thinking_level.as_deref(), Some("high"));

        assert!(loaded.navigate_to(&branch_b_tip));
        assert_eq!(loaded.header.provider.as_deref(), Some("openai"));
        assert_eq!(loaded.header.model_id.as_deref(), Some("gpt-5.4"));
        assert_eq!(loaded.header.thinking_level.as_deref(), Some("low"));
    }

    #[test]
    fn test_session_get_children() {
        let mut session = Session::in_memory();

        // A -> B -> C
        //   -> D
        let id_a = session.append_message(make_test_message("A"));
        let id_b = session.append_message(make_test_message("B"));
        let _id_c = session.append_message(make_test_message("C"));

        // Branch from A
        session.create_branch_from(&id_a);
        let id_d = session.append_message(make_test_message("D"));

        // A should have 2 children: B and D
        let children_a = session.get_children(Some(&id_a));
        assert_eq!(children_a.len(), 2);
        assert!(children_a.contains(&id_b));
        assert!(children_a.contains(&id_d));

        // Root (None) should have 1 child: A
        let root_children = session.get_children(None);
        assert_eq!(root_children.len(), 1);
        assert_eq!(root_children[0], id_a);
    }

    #[test]
    fn test_branch_summary() {
        let mut session = Session::in_memory();

        // Linear: A -> B
        let id_a = session.append_message(make_test_message("A"));
        let id_b = session.append_message(make_test_message("B"));

        let info = session.branch_summary();
        assert_eq!(info.total_entries, 2);
        assert_eq!(info.leaf_count, 1);
        assert_eq!(info.branch_point_count, 0);

        // Create branch: A -> B, A -> C
        session.create_branch_from(&id_a);
        let _id_c = session.append_message(make_test_message("C"));

        let info = session.branch_summary();
        assert_eq!(info.total_entries, 3);
        assert_eq!(info.leaf_count, 2);
        assert_eq!(info.branch_point_count, 1);
        assert!(info.branch_points.contains(&id_a));
        assert!(info.leaves.contains(&id_b));
    }

    fn build_branch_heavy_session(
        path: &Path,
        fork_count: usize,
        side_branch_len: usize,
    ) -> (Session, String) {
        let mut session = Session::create();
        session.path = Some(path.to_path_buf());
        let mut selected_tip = session.append_message(make_test_message("root"));

        for fork_idx in 0..fork_count {
            assert!(
                session.navigate_to(&selected_tip),
                "navigate to selected tip before side branch {fork_idx}"
            );
            for side_idx in 0..side_branch_len {
                session.append_message(make_test_message(&format!("side-{fork_idx}-{side_idx}")));
            }

            assert!(
                session.navigate_to(&selected_tip),
                "return to selected tip before active branch {fork_idx}"
            );
            selected_tip = session.append_message(make_test_message(&format!("active-{fork_idx}")));
        }

        (session, selected_tip)
    }

    const LARGE_REPLAY_CORRECTNESS_EVIDENCE_SCHEMA: &str = "pi.session.large_replay_correctness.v1";

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    struct LargeReplayCorrectnessEvidence {
        schema: String,
        fixture: String,
        entry_count: usize,
        selected_depth: usize,
        replayed_entries: usize,
        skipped_sibling_entries: usize,
        index_initial_miss_files: usize,
        index_cached_hit_files: usize,
        index_cached_reused_files: usize,
        index_failed_files: usize,
        elapsed_budget_class: String,
        fallback_reason: Option<String>,
        baseline_message_count: usize,
        accelerated_message_count: usize,
        baseline_leaf: String,
        accelerated_leaf: String,
        verdict: String,
    }

    fn cold_start_elapsed_budget_class(elapsed_us: u64) -> &'static str {
        match elapsed_us {
            0..=250_000 => "target",
            250_001..=1_000_000 => "bounded",
            _ => "observed_slow",
        }
    }

    fn current_path_message_json(session: &Session) -> serde_json::Value {
        let mut value = serde_json::to_value(session.to_messages_for_current_path())
            .expect("serialize current-path messages");
        redact_json_timestamps(&mut value);
        value
    }

    fn redact_json_timestamps(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(object) => {
                if object.contains_key("timestamp") {
                    object.insert("timestamp".to_string(), serde_json::json!(0));
                }
                for child in object.values_mut() {
                    redact_json_timestamps(child);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    redact_json_timestamps(item);
                }
            }
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => {}
        }
    }

    fn append_large_replay_mixed_turns(session: &mut Session, selected_tip: &str) -> String {
        assert!(
            session.navigate_to(selected_tip),
            "return to active branch before mixed replay fixture"
        );
        let first_kept_id = selected_tip.to_string();
        session.append_model_change(
            "openai-responses".to_string(),
            "gpt-5.2-replay-harness".to_string(),
        );
        let tool_call_id = "call_large_replay";
        session.append_message(make_test_tool_call_message(tool_call_id));
        session.append_message(make_test_tool_result_message(tool_call_id));
        let aborted_id =
            session.append_message(make_test_aborted_assistant_message("interrupted assistant"));
        session.append_bash_execution(
            "cargo test session replay index".to_string(),
            "cancelled by operator".to_string(),
            130,
            true,
            false,
            None,
        );
        session.append_compaction(
            "large replay harness compaction".to_string(),
            first_kept_id,
            42_000,
            Some(serde_json::json!({
                "reason": "large_replay_correctness_harness",
            })),
            Some(false),
        );
        session.append_branch_summary(
            aborted_id,
            "interrupted turn branch summary".to_string(),
            Some(serde_json::json!({
                "turn_state": "interrupted",
            })),
            Some(false),
        );
        session.append_message(make_test_message("active-after-interrupted-turn"))
    }

    #[allow(clippy::too_many_lines)]
    #[cfg(feature = "session-index")]
    #[test]
    fn cold_start_replay_minimization_bounds_branch_heavy_v2_resume() {
        const FORKS: usize = 700;
        const SIDE_BRANCH_LEN: usize = 15;
        const MIXED_ACTIVE_ENTRIES: usize = 8;
        const MIXED_ACTIVE_REPLAYED_ENTRIES: usize = 7;
        const MIXED_ACTIVE_PROJECTED_MESSAGES: usize = 5;

        let temp = tempdir_under_tmpdir("branch-heavy-v2-resume");
        let path = temp.path().join("branch-heavy.jsonl");
        let (mut session, mut selected_tip) =
            build_branch_heavy_session(&path, FORKS, SIDE_BRANCH_LEN);
        selected_tip = append_large_replay_mixed_turns(&mut session, &selected_tip);
        session.header.current_leaf = Some("stale-missing-leaf".to_string());
        run_async(async { session.save().await }).expect("save branch-heavy session");

        let baseline_loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await })
                .expect("open JSONL baseline session");
        let baseline_messages = current_path_message_json(&baseline_loaded);

        create_v2_sidecar_from_jsonl(&path).expect("create v2 sidecar");

        let first_trace = run_async(async {
            Session::cold_start_trace_bundle(&path, temp.path())
                .await
                .expect("initial cold-start trace")
        });
        let trace = run_async(async {
            Session::cold_start_trace_bundle(&path, temp.path())
                .await
                .expect("cached cold-start trace")
        });

        let accelerated_loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await })
                .expect("open accelerated session");
        let accelerated_messages = current_path_message_json(&accelerated_loaded);

        let expected_entries = 1 + (FORKS * (SIDE_BRANCH_LEN + 1)) + MIXED_ACTIVE_ENTRIES;
        let expected_depth = 1 + FORKS + MIXED_ACTIVE_ENTRIES;
        let expected_replayed_entries = 1 + FORKS + MIXED_ACTIVE_REPLAYED_ENTRIES;
        let expected_projected_messages = 1 + FORKS + MIXED_ACTIVE_PROJECTED_MESSAGES;
        assert_eq!(trace.schema, SESSION_COLD_START_TRACE_SCHEMA);
        assert_eq!(
            trace.replay_minimization.schema,
            SESSION_REPLAY_MINIMIZATION_TRACE_SCHEMA
        );
        assert_eq!(trace.storage.selected_backend, "v2_sidecar");
        assert_eq!(trace.storage.opened_backend, "v2_sidecar");
        assert_eq!(trace.input.total_entries, expected_depth);
        assert_eq!(trace.replay_minimization.entry_count, expected_entries);
        assert_eq!(trace.replay_minimization.branch_count, FORKS);
        assert_eq!(trace.replay_minimization.selected_depth, expected_depth);
        assert_eq!(
            trace.replay_minimization.replayed_entries,
            expected_replayed_entries
        );
        assert_eq!(
            trace.replay_minimization.skipped_sibling_entries,
            expected_entries - expected_depth
        );
        assert!(trace.replay_minimization.scanned_files >= 1);
        assert_eq!(trace.replay_minimization.fallback_behavior, None);
        assert_eq!(trace.replay_minimization.verdict, "bounded_selected_branch");
        assert_eq!(trace.compaction_scan.scanned_entries, expected_depth);
        assert_eq!(trace.compaction_scan.compaction_entries, 1);
        assert!(trace.compaction_scan.latest_compaction_present);
        assert_eq!(trace.compaction_scan.first_kept_entry_found, Some(true));
        assert_eq!(trace.first_render.current_path_entries, expected_depth);
        assert_eq!(
            trace.first_render.projected_messages,
            expected_projected_messages
        );
        assert_eq!(trace.first_render.tool_messages, 2);
        assert_eq!(trace.first_render.assistant_messages, 2);
        assert_eq!(trace.first_render.user_messages, 1 + FORKS + 1);
        assert_eq!(trace.first_render.system_messages, 0);
        assert!(first_trace.index_refresh.refreshed_files >= 1);
        assert_eq!(first_trace.index_refresh.failed_files, 0);
        assert!(trace.index_refresh.cache_hit_files >= 1);
        assert_eq!(
            trace.index_refresh.cache_hit_files,
            trace.index_refresh.reused_files
        );
        assert_eq!(trace.index_refresh.failed_files, 0);

        assert_eq!(baseline_loaded.leaf_id(), Some(selected_tip.as_str()));
        assert_eq!(accelerated_loaded.leaf_id(), Some(selected_tip.as_str()));
        assert!(accelerated_loaded.v2_partial_hydration);
        assert_eq!(
            accelerated_loaded.v2_resume_mode,
            Some(V2OpenMode::ActivePath)
        );
        assert_eq!(
            accelerated_loaded.entries_for_current_path().len(),
            baseline_loaded.entries_for_current_path().len()
        );
        assert_eq!(
            accelerated_messages, baseline_messages,
            "accelerated V2 replay must match full JSONL replay"
        );

        let evidence = LargeReplayCorrectnessEvidence {
            schema: LARGE_REPLAY_CORRECTNESS_EVIDENCE_SCHEMA.to_string(),
            fixture: "branch-heavy-v2-resume".to_string(),
            entry_count: trace.replay_minimization.entry_count,
            selected_depth: trace.replay_minimization.selected_depth,
            replayed_entries: trace.replay_minimization.replayed_entries,
            skipped_sibling_entries: trace.replay_minimization.skipped_sibling_entries,
            index_initial_miss_files: first_trace.index_refresh.refreshed_files,
            index_cached_hit_files: trace.index_refresh.cache_hit_files,
            index_cached_reused_files: trace.index_refresh.reused_files,
            index_failed_files: trace.index_refresh.failed_files,
            elapsed_budget_class: cold_start_elapsed_budget_class(trace.total_elapsed_us)
                .to_string(),
            fallback_reason: trace.replay_minimization.fallback_behavior.clone(),
            baseline_message_count: baseline_loaded.to_messages_for_current_path().len(),
            accelerated_message_count: accelerated_loaded.to_messages_for_current_path().len(),
            baseline_leaf: baseline_loaded.leaf_id().unwrap_or_default().to_string(),
            accelerated_leaf: accelerated_loaded.leaf_id().unwrap_or_default().to_string(),
            verdict: trace.replay_minimization.verdict,
        };
        assert_eq!(evidence.fallback_reason, None);
        assert_eq!(
            evidence.baseline_message_count,
            evidence.accelerated_message_count
        );
        assert!(matches!(
            evidence.elapsed_budget_class.as_str(),
            "target" | "bounded" | "observed_slow"
        ));
        let serialized = serde_json::to_string(&evidence).expect("serialize evidence");
        assert!(!serialized.contains("side-0-0"));
        let parsed: LargeReplayCorrectnessEvidence =
            serde_json::from_str(&serialized).expect("parse evidence");
        assert_eq!(parsed, evidence);
    }

    #[test]
    fn cold_start_replay_minimization_reports_missing_and_stale_sidecar_fallbacks() {
        let temp = tempdir_under_tmpdir("branch-heavy-fallbacks");
        let path = temp.path().join("branch-fallback.jsonl");
        let (mut session, _selected_tip) = build_branch_heavy_session(&path, 12, 3);
        run_async(async { session.save().await }).expect("save branch-heavy session");

        let missing_sidecar_trace = run_async(async {
            Session::cold_start_trace_bundle(&path, temp.path())
                .await
                .expect("missing sidecar trace")
        });
        assert_eq!(missing_sidecar_trace.storage.selected_backend, "jsonl");
        assert_eq!(missing_sidecar_trace.storage.opened_backend, "jsonl");
        assert_eq!(
            missing_sidecar_trace
                .replay_minimization
                .fallback_behavior
                .as_deref(),
            Some("jsonl_full_scan_without_sidecar")
        );
        assert_eq!(
            missing_sidecar_trace.replay_minimization.verdict,
            "fallback_explicit"
        );
        assert!(
            missing_sidecar_trace
                .replay_minimization
                .skipped_sibling_entries
                > 0
        );

        let corrupt_path = temp.path().join("branch-corrupt-tail.jsonl");
        let (mut corrupt_session, _selected_tip) = build_branch_heavy_session(&corrupt_path, 8, 2);
        run_async(async { corrupt_session.save().await }).expect("save corrupt-tail fixture");
        {
            use std::io::Write as _;

            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&corrupt_path)
                .expect("open corrupt-tail fixture");
            writeln!(file, "{{invalid-tail-frame").expect("append corrupt tail frame");
        }

        let corrupt_tail_trace = run_async(async {
            Session::cold_start_trace_bundle(&corrupt_path, temp.path())
                .await
                .expect("corrupt tail trace")
        });
        assert_eq!(
            corrupt_tail_trace.open_diagnostics.skipped_entries, 1,
            "corrupt tail frames must be surfaced in cold-start diagnostics"
        );
        assert_eq!(
            corrupt_tail_trace
                .replay_minimization
                .fallback_behavior
                .as_deref(),
            Some("corrupt_jsonl_entries_skipped")
        );
        assert_eq!(
            corrupt_tail_trace.replay_minimization.verdict,
            "fallback_explicit"
        );

        create_v2_sidecar_from_jsonl(&path).expect("create v2 sidecar");
        std::thread::sleep(Duration::from_millis(25));
        session.append_message(make_test_message("jsonl-tail-after-sidecar"));
        run_async(async { session.save().await }).expect("save stale jsonl tail");

        let stale_sidecar_trace = run_async(async {
            Session::cold_start_trace_bundle(&path, temp.path())
                .await
                .expect("stale sidecar trace")
        });
        assert!(stale_sidecar_trace.storage.v2_sidecar_present);
        assert!(stale_sidecar_trace.storage.v2_sidecar_stale);
        assert_eq!(stale_sidecar_trace.storage.selected_backend, "jsonl");
        assert_eq!(stale_sidecar_trace.storage.opened_backend, "jsonl");
        assert_eq!(
            stale_sidecar_trace
                .replay_minimization
                .fallback_behavior
                .as_deref(),
            Some("v2_sidecar_stale")
        );
        assert_eq!(
            stale_sidecar_trace.replay_minimization.verdict,
            "fallback_explicit"
        );
    }

    #[test]
    fn test_session_jsonl_serialization() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.header.provider = Some("anthropic".to_string());
        session.header.model_id = Some("claude-test".to_string());
        session.header.thinking_level = Some("medium".to_string());

        let user_id = session.append_message(make_test_message("Hello"));
        let assistant = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("Hi!"))],
            api: "anthropic".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-test".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        session.append_message(SessionMessage::Assistant { message: assistant });
        session.append_model_change("anthropic".to_string(), "claude-test".to_string());
        session.append_thinking_level_change("high".to_string());
        session.append_compaction("summary".to_string(), user_id.clone(), 123, None, None);
        session.append_branch_summary(user_id, "branch".to_string(), None, None);
        session.append_session_info(Some("my-session".to_string()));

        run_async(async { session.save().await }).unwrap();

        let path = session.path.clone().unwrap();
        let contents = std::fs::read_to_string(path).unwrap();
        let mut lines = contents.lines();

        let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(header["type"], "session");
        assert_eq!(header["version"], SESSION_VERSION);

        let mut types = Vec::new();
        for line in lines {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            let entry_type = value["type"].as_str().unwrap_or_default().to_string();
            types.push(entry_type);
        }

        assert!(types.contains(&"message".to_string()));
        assert!(types.contains(&"model_change".to_string()));
        assert!(types.contains(&"thinking_level_change".to_string()));
        assert!(types.contains(&"compaction".to_string()));
        assert!(types.contains(&"branch_summary".to_string()));
        assert!(types.contains(&"session_info".to_string()));
    }

    #[cfg(not(feature = "session-index"))]
    #[test]
    fn jsonl_persistence_resumes_without_session_index() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.append_message(make_test_message("before resume"));

        run_async(async { session.save().await }).expect("save JSONL session");
        let path = session.path.clone().expect("saved JSONL path");
        assert_eq!(path.extension().and_then(|ext| ext.to_str()), Some("jsonl"));
        assert!(!temp.path().join("session-index.sqlite").exists());

        let path_string = path.to_string_lossy().into_owned();
        let mut resumed =
            run_async(async { Session::open(&path_string).await }).expect("resume JSONL session");
        assert_eq!(resumed.to_messages().len(), 1);
        resumed.append_message(make_test_message("after resume"));
        run_async(async { resumed.save().await }).expect("save resumed JSONL session");

        let reopened =
            run_async(async { Session::open(&path_string).await }).expect("reopen JSONL session");
        assert_eq!(reopened.to_messages().len(), 2);
        assert!(!temp.path().join("session-index.sqlite").exists());
    }

    #[cfg(feature = "session-index")]
    #[test]
    fn cold_start_trace_bundle_is_bounded_redacted_and_cache_aware() {
        let temp = tempdir_under_tmpdir("pi-session-cold-start-");
        if let Some(tmpdir) = env::var_os("TMPDIR") {
            assert!(temp.path().starts_with(PathBuf::from(tmpdir)));
        }

        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.header.cwd = "/private/project/secret-cwd".to_string();
        session.header.provider = Some("test-provider".to_string());
        session.header.model_id = Some("test-model".to_string());

        let mut first_kept_entry_id = None;
        for idx in 0..640 {
            let text = if idx == 7 {
                "secret-user-message should not appear in trace".to_string()
            } else {
                format!("large-history-message-{idx}")
            };
            let id = session.append_message(make_test_message(&text));
            if idx == 512 {
                first_kept_entry_id = Some(id);
            }
            if idx % 128 == 0 {
                session.append_message(make_test_assistant_message(
                    &format!("secret-assistant-message-{idx}"),
                    32,
                ));
            }
        }

        session.append_compaction(
            "secret compaction summary should not appear in trace".to_string(),
            first_kept_entry_id.expect("first kept entry id"),
            12_345,
            None,
            None,
        );
        for idx in 0..16 {
            session.append_message(make_test_message(&format!("tail-message-{idx}")));
        }

        run_async(async { session.save().await }).expect("save large session");
        let path = session.path.clone().expect("session path");

        let first_trace = run_async(async {
            Session::cold_start_trace_bundle(&path, temp.path())
                .await
                .expect("first cold-start trace")
        });
        let second_trace = run_async(async {
            Session::cold_start_trace_bundle(&path, temp.path())
                .await
                .expect("second cold-start trace")
        });

        assert_eq!(first_trace.schema, SESSION_COLD_START_TRACE_SCHEMA);
        assert_eq!(second_trace.schema, SESSION_COLD_START_TRACE_SCHEMA);
        assert_eq!(second_trace.storage.selected_backend, "jsonl");
        assert_eq!(second_trace.storage.opened_backend, "jsonl");
        assert!(second_trace.index_refresh.scanned_files >= 1);
        assert!(second_trace.index_refresh.cache_hit_files >= 1);
        assert_eq!(
            second_trace.index_refresh.cache_hit_files,
            second_trace.index_refresh.reused_files
        );
        assert_eq!(
            second_trace.open_diagnostics,
            SessionColdStartOpenDiagnosticsTrace {
                skipped_entries: 0,
                orphaned_parent_links: 0,
            }
        );
        assert!(second_trace.compaction_scan.latest_compaction_present);
        assert_eq!(
            second_trace.compaction_scan.first_kept_entry_found,
            Some(true)
        );
        assert!(second_trace.first_render.ready);
        assert!(second_trace.first_render.current_path_entries >= 640);
        assert!(second_trace.first_render.projected_messages >= 640);
        assert!(second_trace.first_render.total_tokens >= 32);

        let phase_names = second_trace
            .phases
            .iter()
            .map(|phase| phase.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            phase_names,
            vec![
                "session_open",
                "session_index_refresh",
                "compaction_scan",
                "first_render_ready",
            ]
        );
        assert!(second_trace.phases.len() <= second_trace.bounds.max_phase_count);
        assert!(!second_trace.bounds.raw_path_included);
        assert!(!second_trace.bounds.raw_cwd_included);
        assert!(!second_trace.bounds.raw_message_content_included);

        let serialized = serde_json::to_string(&second_trace).expect("serialize trace");
        assert!(!serialized.contains("secret-user-message"));
        assert!(!serialized.contains("secret-assistant-message"));
        assert!(!serialized.contains("secret compaction summary"));
        assert!(!serialized.contains("secret-cwd"));
        assert!(!serialized.contains(&path.display().to_string()));
        assert!(!serialized.contains(&temp.path().display().to_string()));
        assert_eq!(second_trace.session_path_hash.len(), 16);
    }

    #[test]
    fn test_save_handles_short_or_empty_session_id() {
        let temp = tempfile::tempdir().unwrap();
        let project_cwd = temp.path().join("project");
        std::fs::create_dir(&project_cwd).expect("create project cwd");
        let project_cwd = project_cwd.display().to_string();

        let mut short_id_session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        short_id_session.header.cwd.clone_from(&project_cwd);
        short_id_session.header.id = "x".to_string();
        run_async(async { short_id_session.save().await }).expect("save with short id");
        let short_name = short_id_session
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .expect("short id filename");
        assert!(short_name.contains("_x."));

        let mut empty_id_session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        empty_id_session.header.cwd.clone_from(&project_cwd);
        empty_id_session.header.id.clear();
        run_async(async { empty_id_session.save().await }).expect("save with empty id");
        let empty_name = empty_id_session
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .expect("empty id filename");
        assert!(empty_name.contains("_session."));

        let mut unsafe_id_session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        unsafe_id_session.header.cwd.clone_from(&project_cwd);
        unsafe_id_session.header.id = "../etc/passwd".to_string();
        run_async(async { unsafe_id_session.save().await }).expect("save with unsafe id");
        let unsafe_path = unsafe_id_session.path.as_ref().expect("unsafe id path");
        let unsafe_name = unsafe_path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("unsafe id filename");
        assert!(unsafe_name.contains("____etc_p."));
        let expected_dir = temp.path().join(encode_cwd(Path::new(&project_cwd)));
        assert_eq!(
            unsafe_path.parent().expect("unsafe id parent"),
            expected_dir.as_path()
        );
    }

    #[test]
    fn test_open_with_diagnostics_skips_corrupted_last_entry_and_recovers_leaf() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        let first_id = session.append_message(make_test_message("Hello"));
        let second_id = session.append_message(make_test_message("World"));
        assert_eq!(session.leaf_id.as_deref(), Some(second_id.as_str()));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().expect("session path set");

        let mut lines = std::fs::read_to_string(&path)
            .expect("read session")
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(lines.len() >= 3, "expected header + 2 entries");

        let corrupted_line_number = lines.len(); // 1-based
        let last_index = lines.len() - 1;
        lines[last_index] = "{ this is not json }".to_string();

        let corrupted_path = temp.path().join("corrupted.jsonl");
        std::fs::write(&corrupted_path, format!("{}\n", lines.join("\n")))
            .expect("write corrupted session");

        let (loaded, diagnostics) = run_async(async {
            Session::open_with_diagnostics(corrupted_path.to_string_lossy().as_ref()).await
        })
        .expect("open corrupted session");

        assert_eq!(diagnostics.skipped_entries.len(), 1);
        assert_eq!(
            diagnostics.skipped_entries[0].line_number,
            corrupted_line_number
        );

        let warnings = diagnostics.warning_lines();
        assert_eq!(warnings.len(), 2, "expected per-line warning + summary");
        assert!(
            warnings[0].starts_with(&format!(
                "Warning: Skipping corrupted entry at line {corrupted_line_number} in session file:"
            )),
            "unexpected warning: {}",
            warnings[0]
        );
        assert_eq!(
            warnings[1],
            "Warning: Skipped 1 corrupted entries while loading session"
        );

        assert_eq!(
            loaded.entries.len(),
            session.entries.len() - 1,
            "expected last entry to be dropped"
        );
        assert_eq!(loaded.leaf_id.as_deref(), Some(first_id.as_str()));
    }

    #[test]
    fn test_save_and_open_round_trip_preserves_compaction_and_branch_summary() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        let root_id = session.append_message(make_test_message("Hello"));
        session.append_compaction("compacted".to_string(), root_id.clone(), 123, None, None);
        session.append_branch_summary(root_id, "branch summary".to_string(), None, None);

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().expect("session path set");

        let loaded = run_async(async { Session::open(path.to_string_lossy().as_ref()).await })
            .expect("reopen session");

        assert!(loaded.entries.iter().any(|entry| {
            matches!(entry, SessionEntry::Compaction(compaction) if compaction.summary.eq("compacted") && compaction.tokens_before.eq(&123))
        }));
        assert!(loaded.entries.iter().any(|entry| {
            matches!(entry, SessionEntry::BranchSummary(summary) if summary.summary.eq("branch summary"))
        }));

        let html = loaded.to_html();
        assert!(html.contains("compacted"));
        assert!(html.contains("branch summary"));
    }

    #[test]
    fn test_concurrent_saves_do_not_corrupt_session_file_unit() {
        let temp = tempfile::tempdir().unwrap();
        let base_dir = temp.path().join("sessions");

        let mut session = Session::create_with_dir(Some(base_dir));
        session.append_message(make_test_message("Hello"));

        run_async(async { session.save().await }).expect("initial save");
        let path = session.path.clone().expect("session path set");

        let path1 = path.clone();
        let path2 = path.clone();

        let t1 = std::thread::spawn(move || {
            let runtime = RuntimeBuilder::current_thread()
                .build()
                .expect("build runtime");
            runtime.block_on(async move {
                let mut s = Session::open(path1.to_string_lossy().as_ref())
                    .await
                    .expect("open session");
                s.append_message(make_test_message("From thread 1"));
                s.save().await
            })
        });

        let t2 = std::thread::spawn(move || {
            let runtime = RuntimeBuilder::current_thread()
                .build()
                .expect("build runtime");
            runtime.block_on(async move {
                let mut s = Session::open(path2.to_string_lossy().as_ref())
                    .await
                    .expect("open session");
                s.append_message(make_test_message("From thread 2"));
                s.save().await
            })
        });

        let r1 = t1.join().expect("thread 1 join");
        let r2 = t2.join().expect("thread 2 join");
        assert!(
            r1.is_ok() || r2.is_ok(),
            "Expected at least one save to succeed: r1={r1:?} r2={r2:?}"
        );

        let loaded = run_async(async { Session::open(path.to_string_lossy().as_ref()).await })
            .expect("open after concurrent saves");
        assert!(!loaded.entries.is_empty());
    }

    #[test]
    fn test_to_messages_for_current_path() {
        let mut session = Session::in_memory();

        // Tree structure:
        // A -> B -> C
        //       \-> D  (D branches from B)
        let _id_a = session.append_message(make_test_message("A"));
        let id_b = session.append_message(make_test_message("B"));
        let _id_c = session.append_message(make_test_message("C"));

        // Navigate to B and add D
        session.create_branch_from(&id_b);
        let id_d = session.append_message(make_test_message("D"));

        // Current path should be A -> B -> D
        session.navigate_to(&id_d);
        let messages = session.to_messages_for_current_path();
        assert_eq!(messages.len(), 3);

        // Verify content
        if let Message::User(user) = &messages[0] {
            if let UserContent::Text(text) = &user.content {
                assert_eq!(text, "A");
            }
        }
        if let Message::User(user) = &messages[2] {
            if let UserContent::Text(text) = &user.content {
                assert_eq!(text, "D");
            }
        }
    }

    #[test]
    fn test_reset_leaf_produces_empty_current_path() {
        let mut session = Session::in_memory();

        let _id_a = session.append_message(make_test_message("A"));
        let _id_b = session.append_message(make_test_message("B"));

        session.reset_leaf();
        assert!(session.entries_for_current_path().is_empty());
        assert!(session.to_messages_for_current_path().is_empty());

        // After reset, the next entry becomes a new root.
        let id_root = session.append_message(make_test_message("Root"));
        let entry = session.get_entry(&id_root).expect("entry");
        assert!(entry.base().parent_id.is_none());
    }

    #[test]
    fn test_encode_cwd() {
        let path = std::path::Path::new("/home/user/project");
        let encoded = encode_cwd(path);
        assert!(encoded.starts_with("--"));
        assert!(encoded.ends_with("--"));
        assert!(encoded.contains("home-user-project"));
    }

    // ======================================================================
    // Session creation and header validation
    // ======================================================================

    #[test]
    fn test_session_header_defaults() {
        let header = SessionHeader::new();
        assert_eq!(header.r#type, "session");
        assert_eq!(header.version, Some(SESSION_VERSION));
        assert!(!header.id.is_empty());
        assert!(!header.timestamp.is_empty());
        assert!(header.provider.is_none());
        assert!(header.model_id.is_none());
        assert!(header.thinking_level.is_none());
        assert!(header.parent_session.is_none());
    }

    #[test]
    fn test_session_create_produces_unique_ids() {
        let s1 = Session::create();
        let s2 = Session::create();
        assert_ne!(s1.header.id, s2.header.id);
    }

    #[test]
    fn test_in_memory_session_has_no_path() {
        let session = Session::in_memory();
        assert!(session.path.is_none());
        assert!(session.leaf_id.is_none());
        assert!(session.entries.is_empty());
    }

    #[test]
    fn test_create_with_dir_stores_session_dir() {
        let temp = tempfile::tempdir().unwrap();
        let session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        assert_eq!(session.session_dir, Some(temp.path().to_path_buf()));
    }

    // ======================================================================
    // Message types: tool result, bash execution, custom
    // ======================================================================

    #[test]
    fn test_append_tool_result_message() {
        let mut session = Session::in_memory();
        let user_id = session.append_message(make_test_message("Hello"));

        let tool_msg = SessionMessage::ToolResult {
            tool_call_id: "call_123".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("file contents"))],
            details: None,
            is_error: false,
            timestamp: Some(1000),
        };
        let tool_id = session.append_message(tool_msg);

        // Verify parent linking
        let entry = session.get_entry(&tool_id).unwrap();
        assert_eq!(entry.base().parent_id.as_deref(), Some(user_id.as_str()));

        // Verify it converts to model message
        let messages = session.to_messages();
        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[1], Message::ToolResult(tr) if tr.tool_call_id.eq("call_123")));
    }

    #[test]
    fn test_tool_result_artifact_metadata_round_trip_without_full_payload() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.append_message(make_test_message("list the huge directory"));

        let omitted_payload = "x".repeat(1024);
        let preview = "entry-0000.txt\nentry-0001.txt\n\n[Full tool output artifact: /tmp/pi-tool-artifacts/call/abc.txt (5000000 bytes, 50000 lines, sha256 abc). Use read on this path to inspect more.]";
        session.append_message(SessionMessage::ToolResult {
            tool_call_id: "call_artifact".to_string(),
            tool_name: "ls".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(preview))],
            details: Some(serde_json::json!({
                "artifact": {
                    "schema": "pi.tool_output_artifact.v1",
                    "id": "tool-artifact-abc",
                    "toolName": "ls",
                    "sourceKind": "directoryEntries",
                    "path": "/tmp/pi-tool-artifacts/call/abc.txt",
                    "metadataPath": "/tmp/pi-tool-artifacts/call/abc.json",
                    "sha256": "abc",
                    "byteCount": 5_000_000_u64,
                    "lineCount": 50_000,
                    "previewBytes": preview.len(),
                    "contentType": "text/plain; charset=utf-8"
                }
            })),
            is_error: false,
            timestamp: Some(12346),
        });

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();
        let jsonl = std::fs::read_to_string(&path).unwrap();
        assert!(jsonl.contains("\"schema\":\"pi.tool_output_artifact.v1\""));
        assert!(!jsonl.contains(&omitted_payload));

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
        let tool_result = loaded
            .to_messages()
            .into_iter()
            .find_map(|message| match message {
                Message::ToolResult(result) if result.tool_call_id.eq("call_artifact") => {
                    Some(result)
                }
                _ => None,
            })
            .expect("artifact tool result");

        assert_eq!(
            tool_result
                .details
                .as_ref()
                .and_then(|details| details.pointer("/artifact/schema"))
                .and_then(Value::as_str),
            Some("pi.tool_output_artifact.v1")
        );
        assert!(tool_result.content.iter().all(|block| match block {
            ContentBlock::Text(text) => !text.text.contains(&omitted_payload),
            _ => true,
        }));
    }

    #[test]
    fn test_append_tool_result_error() {
        let mut session = Session::in_memory();
        session.append_message(make_test_message("Hello"));

        let tool_msg = SessionMessage::ToolResult {
            tool_call_id: "call_err".to_string(),
            tool_name: "bash".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("command not found"))],
            details: None,
            is_error: true,
            timestamp: Some(2000),
        };
        let tool_id = session.append_message(tool_msg);

        let entry = session.get_entry(&tool_id).expect("should find tool entry");
        if let SessionEntry::Message(msg) = entry {
            if let SessionMessage::ToolResult { is_error, .. } = &msg.message {
                assert!(is_error);
            } else {
                test_fail!("Expected SessionMessage::ToolResult, got {:?}", msg.message);
            }
        } else {
            test_fail!("Expected SessionEntry::Message");
        }
    }

    #[test]
    fn test_append_bash_execution() {
        let mut session = Session::in_memory();
        session.append_message(make_test_message("run something"));

        let bash_id = session.append_bash_execution(
            "echo hello".to_string(),
            "hello\n".to_string(),
            0,
            false,
            false,
            None,
        );

        let entry = session.get_entry(&bash_id).expect("should find bash entry");
        if let SessionEntry::Message(msg) = entry {
            if let SessionMessage::BashExecution {
                command, exit_code, ..
            } = &msg.message
            {
                assert_eq!(command, "echo hello");
                assert_eq!(*exit_code, 0);
            } else {
                test_fail!(
                    "Expected SessionMessage::BashExecution, got {:?}",
                    msg.message
                );
            }
        } else {
            test_fail!("Expected SessionEntry::Message");
        }

        // BashExecution converts to User message for model context
        let messages = session.to_messages();
        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[1], Message::User(_)));
    }

    #[test]
    fn test_bash_execution_exclude_from_context() {
        let mut session = Session::in_memory();
        session.append_message(make_test_message("run something"));

        let id = session.next_entry_id();
        let base = EntryBase::new(session.leaf_id.clone(), id.clone());
        let mut extra = HashMap::new();
        extra.insert("excludeFromContext".to_string(), serde_json::json!(true));
        let entry = SessionEntry::Message(MessageEntry {
            base,
            message: SessionMessage::BashExecution {
                command: "secret".to_string(),
                output: "hidden".to_string(),
                exit_code: 0,
                cancelled: None,
                truncated: None,
                full_output_path: None,
                timestamp: Some(0),
                extra,
            },
        });
        session.leaf_id = Some(id);
        session.entries.push(entry);
        session.entry_ids = entry_id_set(&session.entries);

        // The excluded bash execution should not appear in model messages
        let messages = session.to_messages();
        assert_eq!(messages.len(), 1); // only the user message
    }

    #[test]
    fn test_append_custom_message() {
        let mut session = Session::in_memory();
        session.append_message(make_test_message("Hello"));

        let custom_msg = SessionMessage::Custom {
            custom_type: "extension_state".to_string(),
            content: "some state".to_string(),
            display: false,
            details: Some(serde_json::json!({"key": "value"})),
            timestamp: Some(0),
        };
        let custom_id = session.append_message(custom_msg);

        let entry = session
            .get_entry(&custom_id)
            .expect("should find custom entry");
        if let SessionEntry::Message(msg) = entry {
            if let SessionMessage::Custom {
                custom_type,
                display,
                ..
            } = &msg.message
            {
                assert_eq!(custom_type, "extension_state");
                assert!(!display);
            } else {
                test_fail!("Expected SessionMessage::Custom, got {:?}", msg.message);
            }
        } else {
            test_fail!("Expected SessionEntry::Message");
        }
    }

    #[test]
    fn test_append_custom_entry() {
        let mut session = Session::in_memory();
        let root_id = session.append_message(make_test_message("Hello"));

        let custom_id =
            session.append_custom_entry("my_type".to_string(), Some(serde_json::json!(42)));

        let entry = session
            .get_entry(&custom_id)
            .expect("should find custom entry");
        if let SessionEntry::Custom(custom) = entry {
            assert_eq!(custom.custom_type, "my_type");
            assert_eq!(custom.data, Some(serde_json::json!(42)));
            assert_eq!(custom.base.parent_id.as_deref(), Some(root_id.as_str()));
        } else {
            test_fail!("Expected SessionEntry::Custom, got {:?}", entry);
        }
    }

    // ======================================================================
    // Parent linking / tree structure
    // ======================================================================

    #[test]
    fn test_parent_linking_chain() {
        let mut session = Session::in_memory();

        let id1 = session.append_message(make_test_message("A"));
        let id2 = session.append_message(make_test_message("B"));
        let id3 = session.append_message(make_test_message("C"));

        // First entry has no parent
        let e1 = session.get_entry(&id1).unwrap();
        assert!(e1.base().parent_id.is_none());

        // Second entry's parent is first
        let e2 = session.get_entry(&id2).unwrap();
        assert_eq!(e2.base().parent_id.as_deref(), Some(id1.as_str()));

        // Third entry's parent is second
        let e3 = session.get_entry(&id3).unwrap();
        assert_eq!(e3.base().parent_id.as_deref(), Some(id2.as_str()));
    }

    #[test]
    fn test_model_change_updates_leaf() {
        let mut session = Session::in_memory();

        let msg_id = session.append_message(make_test_message("Hello"));
        let change_id = session.append_model_change("openai".to_string(), "gpt-4".to_string());

        assert_eq!(session.leaf_id.as_deref(), Some(change_id.as_str()));

        let entry = session
            .get_entry(&change_id)
            .expect("should find change entry");
        assert_eq!(entry.base().parent_id.as_deref(), Some(msg_id.as_str()));

        if let SessionEntry::ModelChange(mc) = entry {
            assert_eq!(mc.provider, "openai");
            assert_eq!(mc.model_id, "gpt-4");
        } else {
            test_fail!("Expected SessionEntry::ModelChange, got {:?}", entry);
        }
    }

    #[test]
    fn test_thinking_level_change_updates_leaf() {
        let mut session = Session::in_memory();
        session.append_message(make_test_message("Hello"));

        let change_id = session.append_thinking_level_change("high".to_string());
        assert_eq!(session.leaf_id.as_deref(), Some(change_id.as_str()));

        let entry = session
            .get_entry(&change_id)
            .expect("should find change entry");
        if let SessionEntry::ThinkingLevelChange(tlc) = entry {
            assert_eq!(tlc.thinking_level, "high");
        } else {
            test_fail!(
                "Expected SessionEntry::ThinkingLevelChange, got {:?}",
                entry
            );
        }
    }

    // ======================================================================
    // Session name get/set
    // ======================================================================

    #[test]
    fn test_get_name_returns_latest() {
        let mut session = Session::in_memory();

        assert!(session.get_name().is_none());

        session.set_name("first");
        assert_eq!(session.get_name().as_deref(), Some("first"));

        session.set_name("second");
        assert_eq!(session.get_name().as_deref(), Some("second"));
    }

    #[test]
    fn test_set_name_returns_entry_id() {
        let mut session = Session::in_memory();
        let id = session.set_name("test-name");
        assert!(!id.is_empty());
        let entry = session.get_entry(&id).unwrap();
        assert!(matches!(entry, SessionEntry::SessionInfo(_)));
    }

    // ======================================================================
    // Label
    // ======================================================================

    #[test]
    fn test_add_label_to_existing_entry() {
        let mut session = Session::in_memory();
        let msg_id = session.append_message(make_test_message("Hello"));

        let label_id = session.add_label(&msg_id, Some("important".to_string()));
        assert!(label_id.is_some());

        let entry = session
            .get_entry(&label_id.unwrap())
            .expect("should find label entry");
        if let SessionEntry::Label(label) = entry {
            assert_eq!(label.target_id, msg_id);
            assert_eq!(label.label.as_deref(), Some("important"));
        } else {
            test_fail!("Expected SessionEntry::Label, got {:?}", entry);
        }
    }

    #[test]
    fn test_add_label_to_nonexistent_entry_returns_none() {
        let mut session = Session::in_memory();
        let result = session.add_label("nonexistent", Some("label".to_string()));
        assert!(result.is_none());
    }

    // ======================================================================
    // JSONL round-trip (save + reload)
    // ======================================================================

    #[test]
    fn test_round_trip_preserves_all_message_types() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        // Append diverse message types
        session.append_message(make_test_message("user text"));

        let assistant = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("response"))],
            api: "anthropic".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-test".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        session.append_message(SessionMessage::Assistant { message: assistant });

        session.append_message(SessionMessage::ToolResult {
            tool_call_id: "call_1".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("result"))],
            details: None,
            is_error: false,
            timestamp: Some(100),
        });

        session.append_bash_execution("ls".to_string(), "files".to_string(), 0, false, false, None);

        session.append_custom_entry(
            "ext_data".to_string(),
            Some(serde_json::json!({"foo": "bar"})),
        );

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

        assert_eq!(loaded.entries.len(), session.entries.len());
        assert_eq!(loaded.header.id, session.header.id);
        assert_eq!(loaded.header.version, Some(SESSION_VERSION));

        // Verify specific entry types survived the round-trip
        let has_tool_result = loaded.entries.iter().any(|e| {
            matches!(
                e,
                SessionEntry::Message(m) if matches!(
                    &m.message,
                    SessionMessage::ToolResult { tool_name, .. } if tool_name.eq("read")
                )
            )
        });
        assert!(has_tool_result, "tool result should survive round-trip");

        let has_bash = loaded.entries.iter().any(|e| {
            matches!(
                e,
                SessionEntry::Message(m) if matches!(
                    &m.message,
                    SessionMessage::BashExecution { command, .. } if command.eq("ls")
                )
            )
        });
        assert!(has_bash, "bash execution should survive round-trip");

        let has_custom = loaded.entries.iter().any(|e| {
            matches!(
                e,
                SessionEntry::Custom(c) if c.custom_type.eq("ext_data")
            )
        });
        assert!(has_custom, "custom entry should survive round-trip");
    }

    #[test]
    fn test_round_trip_preserves_leaf_id() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        let _id1 = session.append_message(make_test_message("A"));
        let id2 = session.append_message(make_test_message("B"));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

        assert_eq!(loaded.leaf_id.as_deref(), Some(id2.as_str()));
    }

    #[test]
    fn test_round_trip_preserves_selected_branch_leaf_and_header_state() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        let root_id = session.append_message(make_test_message("root"));
        let _openai_model =
            session.append_model_change("openai".to_string(), "gpt-5.4".to_string());
        session.set_model_header(
            Some("openai".to_string()),
            Some("gpt-5.4".to_string()),
            None,
        );
        let high_id = session.append_thinking_level_change("high".to_string());
        session.set_model_header(None, None, Some("high".to_string()));

        assert!(session.create_branch_from(&root_id));
        let _anthropic_model =
            session.append_model_change("anthropic".to_string(), "claude-sonnet-4".to_string());
        session.set_model_header(
            Some("anthropic".to_string()),
            Some("claude-sonnet-4".to_string()),
            None,
        );
        session.append_thinking_level_change("medium".to_string());
        session.set_model_header(None, None, Some("medium".to_string()));

        assert!(session.navigate_to(&high_id));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

        assert_eq!(loaded.leaf_id.as_deref(), Some(high_id.as_str()));
        assert_eq!(
            loaded.header.current_leaf.as_deref(),
            Some(high_id.as_str())
        );
        assert_eq!(loaded.header.provider.as_deref(), Some("openai"));
        assert_eq!(loaded.header.model_id.as_deref(), Some("gpt-5.4"));
        assert_eq!(loaded.header.thinking_level.as_deref(), Some("high"));
    }

    #[test]
    fn test_append_after_branch_navigation_clears_persisted_leaf_override() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        let id_a = session.append_message(make_test_message("A"));
        let id_b = session.append_message(make_test_message("B"));
        session.append_message(make_test_message("C"));

        assert!(session.create_branch_from(&id_a));
        session.append_message(make_test_message("D"));

        assert!(session.navigate_to(&id_b));
        let id_e = session.append_message(make_test_message("E"));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

        assert_eq!(loaded.leaf_id.as_deref(), Some(id_e.as_str()));
        assert!(loaded.header.current_leaf.is_none());
    }

    #[test]
    fn test_round_trip_preserves_header_fields() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.header.provider = Some("anthropic".to_string());
        session.header.model_id = Some("claude-opus".to_string());
        session.header.thinking_level = Some("high".to_string());
        session.header.parent_session = Some("/old/session.jsonl".to_string());

        session.append_message(make_test_message("Hello"));
        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

        assert_eq!(loaded.header.provider.as_deref(), Some("anthropic"));
        assert_eq!(loaded.header.model_id.as_deref(), Some("claude-opus"));
        assert_eq!(loaded.header.thinking_level.as_deref(), Some("high"));
        assert_eq!(
            loaded.header.parent_session.as_deref(),
            Some("/old/session.jsonl")
        );
    }

    #[test]
    fn test_empty_session_save_and_reload() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

        assert!(loaded.entries.is_empty());
        assert!(loaded.leaf_id.is_none());
        assert_eq!(loaded.header.id, session.header.id);
    }

    // ======================================================================
    // Corrupted JSONL recovery
    // ======================================================================

    #[test]
    fn test_corrupted_middle_entry_preserves_surrounding_entries() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        let id1 = session.append_message(make_test_message("First"));
        let id2 = session.append_message(make_test_message("Second"));
        let id3 = session.append_message(make_test_message("Third"));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        // Corrupt the middle entry (line 3, 1-indexed: header=1, first=2, second=3)
        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert!(lines.len() >= 4);
        lines[2] = "GARBAGE JSON".to_string();
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

        let (loaded, diagnostics) = run_async(async {
            Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await
        })
        .unwrap();

        let diag = serde_json::json!({
            "fixture_id": "session-corrupted-middle-entry-replay-integrity",
            "path": path.display().to_string(),
            "seed": "deterministic-static",
            "env": {
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
            },
            "expected": {
                "skipped_entries": 1,
                "orphaned_parent_links": 1,
            },
            "actual": {
                "skipped_entries": diagnostics.skipped_entries.len(),
                "orphaned_parent_links": diagnostics.orphaned_parent_links.len(),
                "leaf_id": loaded.leaf_id,
            },
        })
        .to_string();

        assert_eq!(diagnostics.skipped_entries.len(), 1, "{diag}");
        assert_eq!(diagnostics.skipped_entries[0].line_number, 3, "{diag}");
        assert_eq!(diagnostics.orphaned_parent_links.len(), 1, "{diag}");
        assert_eq!(diagnostics.orphaned_parent_links[0].entry_id, id3, "{diag}");
        assert_eq!(
            diagnostics.orphaned_parent_links[0].missing_parent_id, id2,
            "{diag}"
        );
        assert!(
            diagnostics.warning_lines().iter().any(|line| {
                line.contains("references missing parent")
                    && line.contains(diagnostics.orphaned_parent_links[0].entry_id.as_str())
            }),
            "{diag}"
        );

        // First and third entries should survive
        assert_eq!(loaded.entries.len(), 2, "{diag}");
        assert!(loaded.get_entry(&id1).is_some(), "{diag}");
        assert!(loaded.get_entry(&id3).is_some(), "{diag}");
    }

    #[test]
    fn test_multiple_corrupted_entries_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        session.append_message(make_test_message("A"));
        session.append_message(make_test_message("B"));
        session.append_message(make_test_message("C"));
        session.append_message(make_test_message("D"));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        // Corrupt entries B (line 3) and D (line 5)
        lines[2] = "BAD".to_string();
        lines[4] = "ALSO BAD".to_string();
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

        let (loaded, diagnostics) = run_async(async {
            Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await
        })
        .unwrap();

        assert_eq!(diagnostics.skipped_entries.len(), 2);
        assert_eq!(loaded.entries.len(), 2); // A and C survive
    }

    #[test]
    fn test_corrupted_header_fails_to_open() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bad_header.jsonl");
        std::fs::write(&path, "NOT A VALID HEADER\n{\"type\":\"message\"}\n").unwrap();

        let result = run_async(async {
            Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await
        });
        assert!(
            result.is_err(),
            "corrupted header should cause open failure"
        );
    }

    // ======================================================================
    // Branching and navigation
    // ======================================================================

    #[test]
    fn test_create_branch_from_nonexistent_returns_false() {
        let mut session = Session::in_memory();
        session.append_message(make_test_message("A"));
        assert!(!session.create_branch_from("nonexistent"));
    }

    #[test]
    fn test_deep_branching() {
        let mut session = Session::in_memory();

        // Create A -> B -> C
        let id_a = session.append_message(make_test_message("A"));
        let id_b = session.append_message(make_test_message("B"));
        let _id_c = session.append_message(make_test_message("C"));

        // Branch from A: A -> D
        session.create_branch_from(&id_a);
        let _id_d = session.append_message(make_test_message("D"));

        // Branch from B: A -> B -> E
        session.create_branch_from(&id_b);
        let id_e = session.append_message(make_test_message("E"));

        // Should have 3 leaves: C, D, E
        let leaves = session.list_leaves();
        assert_eq!(leaves.len(), 3);

        // Path to E is A -> B -> E
        let path = session.get_path_to_entry(&id_e);
        assert_eq!(path.len(), 3);
        assert_eq!(path[0], id_a);
        assert_eq!(path[1], id_b);
        assert_eq!(path[2], id_e);
    }

    #[test]
    fn test_sibling_branches_at_fork() {
        let mut session = Session::in_memory();

        // Create A -> B -> C
        let id_a = session.append_message(make_test_message("A"));
        let _id_b = session.append_message(make_test_message("B"));
        let _id_c = session.append_message(make_test_message("C"));

        // Branch from A: A -> D
        session.create_branch_from(&id_a);
        let id_d = session.append_message(make_test_message("D"));

        // Navigate to D to make it current
        session.navigate_to(&id_d);

        let siblings = session.sibling_branches();
        assert!(siblings.is_some());
        let (fork_point, branches) = siblings.unwrap();
        assert!(
            fork_point.is_none()
                || fork_point
                    .as_deref()
                    .is_some_and(|fork_point_id| fork_point_id.eq(id_a.as_str()))
        );
        assert_eq!(branches.len(), 2);

        // One should be current, one not
        let current_count = branches.iter().filter(|b| b.is_current).count();
        assert_eq!(current_count, 1);
    }

    #[test]
    fn test_sibling_branches_no_fork() {
        let mut session = Session::in_memory();
        session.append_message(make_test_message("A"));
        session.append_message(make_test_message("B"));

        // No fork points, so sibling_branches returns None
        assert!(session.sibling_branches().is_none());
    }

    // ======================================================================
    // Plan fork
    // ======================================================================

    #[test]
    fn test_plan_fork_from_user_message() {
        let mut session = Session::in_memory();

        let _id_a = session.append_message(make_test_message("First question"));
        let assistant = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("Answer"))],
            api: "anthropic".to_string(),
            provider: "anthropic".to_string(),
            model: "test".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        let _id_b = session.append_message(SessionMessage::Assistant { message: assistant });
        let id_c = session.append_message(make_test_message("Second question"));

        // Fork from the second user message
        let plan = session.plan_fork_from_user_message(&id_c).unwrap();
        assert_eq!(plan.selected_text, "Second question");
        // Entries should be the path up to (but not including) the forked message
        assert_eq!(plan.entries.len(), 2); // A and B
    }

    #[test]
    fn test_plan_fork_from_root_message() {
        let mut session = Session::in_memory();
        let id_a = session.append_message(make_test_message("Root question"));

        let plan = session.plan_fork_from_user_message(&id_a).unwrap();
        assert_eq!(plan.selected_text, "Root question");
        assert!(plan.entries.is_empty()); // No entries before root
        assert!(plan.leaf_id.is_none());
    }

    #[test]
    fn test_plan_fork_from_nonexistent_fails() {
        let session = Session::in_memory();
        assert!(session.plan_fork_from_user_message("nonexistent").is_err());
    }

    #[test]
    fn test_plan_fork_from_assistant_message_fails() {
        let mut session = Session::in_memory();
        session.append_message(make_test_message("Q"));
        let assistant = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("A"))],
            api: "anthropic".to_string(),
            provider: "anthropic".to_string(),
            model: "test".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        let asst_id = session.append_message(SessionMessage::Assistant { message: assistant });

        assert!(session.plan_fork_from_user_message(&asst_id).is_err());
    }

    // ======================================================================
    // Compaction in message context
    // ======================================================================

    #[test]
    fn test_compaction_truncates_model_context() {
        let mut session = Session::in_memory();

        let _id_a = session.append_message(make_test_message("old message A"));
        let _id_b = session.append_message(make_test_message("old message B"));
        let id_c = session.append_message(make_test_message("kept message C"));

        // Compact: keep from id_c onwards
        session.append_compaction(
            "Summary of old messages".to_string(),
            id_c,
            5000,
            None,
            None,
        );

        let id_d = session.append_message(make_test_message("new message D"));

        // Ensure we're at the right leaf
        session.navigate_to(&id_d);

        let messages = session.to_messages_for_current_path();
        // Should have: compaction summary + kept message C + new message D
        // (old messages A and B should be omitted)
        assert!(messages.len() <= 4); // compaction summary + C + compaction entry + D

        // Verify old messages are not in context
        let all_text: String = messages
            .iter()
            .filter_map(|m| match m {
                Message::User(u) => match &u.content {
                    UserContent::Text(t) => Some(t.clone()),
                    UserContent::Blocks(blocks) => {
                        let texts: Vec<String> = blocks
                            .iter()
                            .filter_map(|b| {
                                if let ContentBlock::Text(t) = b {
                                    Some(t.text.clone())
                                } else {
                                    None
                                }
                            })
                            .collect();
                        Some(texts.join(" "))
                    }
                },
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");

        assert!(
            !all_text.contains("old message A"),
            "compacted message A should not appear in context"
        );
        assert!(
            !all_text.contains("old message B"),
            "compacted message B should not appear in context"
        );
        assert!(
            all_text.contains("kept message C") || all_text.contains("new message D"),
            "kept messages should appear in context"
        );
    }

    // ======================================================================
    // Large session handling
    // ======================================================================

    #[test]
    fn test_large_session_append_and_path() {
        let mut session = Session::in_memory();

        let mut last_id = String::new();
        for i in 0..500 {
            last_id = session.append_message(make_test_message(&format!("msg-{i}")));
        }

        assert_eq!(session.entries.len(), 500);
        assert_eq!(session.leaf_id.as_deref(), Some(last_id.as_str()));

        // Path from root to leaf should include all 500 entries
        let path = session.get_path_to_entry(&last_id);
        assert_eq!(path.len(), 500);

        // Entries for current path should also be 500
        let current = session.entries_for_current_path();
        assert_eq!(current.len(), 500);
    }

    #[test]
    fn test_large_session_save_and_reload() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        for i in 0..200 {
            session.append_message(make_test_message(&format!("message {i}")));
        }

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

        assert_eq!(loaded.entries.len(), 200);
        assert_eq!(loaded.header.id, session.header.id);
    }

    // ======================================================================
    // Entry ID generation
    // ======================================================================

    #[test]
    fn test_ensure_entry_ids_fills_missing() {
        let mut entries = vec![
            SessionEntry::Message(MessageEntry {
                base: EntryBase {
                    id: None,
                    parent_id: None,
                    timestamp: "2025-01-01T00:00:00.000Z".to_string(),
                },
                message: SessionMessage::User {
                    content: UserContent::Text("test".to_string()),
                    timestamp: Some(0),
                },
            }),
            SessionEntry::Message(MessageEntry {
                base: EntryBase {
                    id: Some("existing".to_string()),
                    parent_id: None,
                    timestamp: "2025-01-01T00:00:00.000Z".to_string(),
                },
                message: SessionMessage::User {
                    content: UserContent::Text("test2".to_string()),
                    timestamp: Some(0),
                },
            }),
        ];

        ensure_entry_ids(&mut entries);

        // First entry should now have an ID
        assert!(entries[0].base().id.is_some());
        // Second entry should keep its existing ID
        assert_eq!(entries[1].base().id.as_deref(), Some("existing"));
        // IDs should be unique
        assert_ne!(entries[0].base().id, entries[1].base().id);
    }

    #[test]
    fn test_generate_entry_id_produces_8_char_hex() {
        let existing = HashSet::new();
        let id = generate_entry_id(&existing);
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ======================================================================
    // set_model_header / set_branched_from
    // ======================================================================

    #[test]
    fn test_set_model_header() {
        let mut session = Session::in_memory();
        session.set_model_header(
            Some("anthropic".to_string()),
            Some("claude-opus".to_string()),
            Some("high".to_string()),
        );
        assert_eq!(session.header.provider.as_deref(), Some("anthropic"));
        assert_eq!(session.header.model_id.as_deref(), Some("claude-opus"));
        assert_eq!(session.header.thinking_level.as_deref(), Some("high"));
    }

    #[test]
    fn test_effective_model_and_thinking_use_current_header_without_change_entries() {
        let mut session = Session::in_memory();
        session.set_model_header(
            Some("openai".to_string()),
            Some("gpt-5.4".to_string()),
            Some("medium".to_string()),
        );

        assert_eq!(
            session.effective_model_for_current_path(),
            Some(("openai".to_string(), "gpt-5.4".to_string()))
        );
        assert_eq!(
            session
                .effective_thinking_level_for_current_path()
                .as_deref(),
            Some("medium")
        );
    }

    #[test]
    fn test_set_branched_from() {
        let mut session = Session::in_memory();
        assert!(session.header.parent_session.is_none());

        session.set_branched_from(Some("/path/to/parent.jsonl".to_string()));
        assert_eq!(
            session.header.parent_session.as_deref(),
            Some("/path/to/parent.jsonl")
        );
    }

    // ======================================================================
    // to_html rendering
    // ======================================================================

    #[test]
    fn test_to_html_contains_all_message_types() {
        let mut session = Session::in_memory();

        session.append_message(make_test_message("user question"));

        let assistant = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("assistant answer"))],
            api: "anthropic".to_string(),
            provider: "anthropic".to_string(),
            model: "test".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        session.append_message(SessionMessage::Assistant { message: assistant });
        session.append_model_change("anthropic".to_string(), "claude-test".to_string());
        session.set_name("test-session-html");

        let html = session.to_html();
        assert!(html.contains("<!doctype html>"));
        assert!(html.contains("user question"));
        assert!(html.contains("assistant answer"));
        assert!(html.contains("anthropic"));
        assert!(html.contains("test-session-html"));
    }

    // ======================================================================
    // to_messages conversion
    // ======================================================================

    #[test]
    fn test_to_messages_includes_all_message_entries() {
        let mut session = Session::in_memory();

        session.append_message(make_test_message("Q1"));
        let assistant = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("A1"))],
            api: "anthropic".to_string(),
            provider: "anthropic".to_string(),
            model: "test".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        session.append_message(SessionMessage::Assistant { message: assistant });
        session.append_message(SessionMessage::ToolResult {
            tool_call_id: "c1".to_string(),
            tool_name: "edit".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("edited"))],
            details: None,
            is_error: false,
            timestamp: Some(0),
        });

        // Non-message entries should NOT appear in to_messages()
        session.append_model_change("openai".to_string(), "gpt-4".to_string());
        session.append_session_info(Some("name".to_string()));

        let messages = session.to_messages();
        assert_eq!(messages.len(), 3); // user + assistant + tool_result
    }

    // ======================================================================
    // JSONL format validation
    // ======================================================================

    #[test]
    fn test_jsonl_header_is_first_line() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.append_message(make_test_message("test"));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let contents = std::fs::read_to_string(path).unwrap();
        let first_line = contents.lines().next().unwrap();
        let header: serde_json::Value = serde_json::from_str(first_line).unwrap();

        assert_eq!(header["type"], "session");
        assert_eq!(header["version"], SESSION_VERSION);
        assert!(!header["id"].as_str().unwrap().is_empty());
        assert!(!header["timestamp"].as_str().unwrap().is_empty());
    }

    #[test]
    fn test_jsonl_entries_have_camelcase_fields() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        session.append_message(make_test_message("test"));
        session.append_model_change("provider".to_string(), "model".to_string());

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let contents = std::fs::read_to_string(path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();

        // Check message entry (line 2)
        let msg_value: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert!(msg_value.get("parentId").is_some() || msg_value.get("id").is_some());

        // Check model change entry (line 3)
        let mc_value: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert!(mc_value.get("modelId").is_some());
    }

    // ======================================================================
    // Session open errors
    // ======================================================================

    #[test]
    fn test_open_nonexistent_file_returns_error() {
        let result =
            run_async(async { Session::open("/tmp/nonexistent_session_12345.jsonl").await });
        assert!(result.is_err());
    }

    #[test]
    fn test_open_empty_file_returns_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("empty.jsonl");
        std::fs::write(&path, "").unwrap();

        let result = run_async(async { Session::open(path.to_string_lossy().as_ref()).await });
        assert!(result.is_err());
    }

    #[test]
    fn test_open_rejects_semantically_invalid_header() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("invalid_header.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"note","version":3,"id":"bad","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
        )
        .unwrap();

        let err = run_async(async { Session::open(path.to_string_lossy().as_ref()).await })
            .expect_err("invalid header should fail");
        let message = err.to_string();
        assert!(
            message.contains("Invalid session header"),
            "expected invalid session header error, got {message}"
        );
    }

    #[test]
    fn test_save_rejects_semantically_invalid_header() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.header.r#type = "note".to_string();

        let err =
            run_async(async { session.save().await }).expect_err("invalid header should fail");
        let message = err.to_string();
        assert!(
            message.contains("Invalid session header"),
            "expected invalid session header error, got {message}"
        );
    }

    // ======================================================================
    // get_entry / get_entry_mut
    // ======================================================================

    #[test]
    fn test_get_entry_returns_correct_entry() {
        let mut session = Session::in_memory();
        let id = session.append_message(make_test_message("Hello"));

        let entry = session.get_entry(&id);
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().base().id.as_deref(), Some(id.as_str()));
    }

    #[test]
    fn test_get_entry_mut_allows_modification() {
        let mut session = Session::in_memory();
        let id = session.append_message(make_test_message("Original"));

        let entry = session.get_entry_mut(&id).unwrap();
        if let SessionEntry::Message(msg) = entry {
            msg.message = SessionMessage::User {
                content: UserContent::Text("Modified".to_string()),
                timestamp: Some(0),
            };
        }

        // Verify modification persisted
        let entry = session.get_entry(&id).unwrap();
        if let SessionEntry::Message(msg) = entry {
            if let SessionMessage::User { content, .. } = &msg.message {
                match content {
                    UserContent::Text(t) => assert_eq!(t, "Modified"),
                    UserContent::Blocks(_) => test_fail!("Expected UserContent::Text, got Blocks"),
                }
            } else {
                test_fail!("Expected SessionMessage::User, got {:?}", msg.message);
            }
        }
    }

    #[test]
    fn test_get_entry_nonexistent_returns_none() {
        let session = Session::in_memory();
        assert!(session.get_entry("nonexistent").is_none());
    }

    // ======================================================================
    // Branching round-trip (save with branches, reload, verify)
    // ======================================================================

    #[test]
    fn test_branching_round_trip_preserves_tree_structure() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        // Create: A -> B -> C, then branch from A: A -> D
        let id_a = session.append_message(make_test_message("A"));
        let id_b = session.append_message(make_test_message("B"));
        let id_c = session.append_message(make_test_message("C"));

        session.create_branch_from(&id_a);
        let id_d = session.append_message(make_test_message("D"));

        // Verify pre-save state
        let leaves = session.list_leaves();
        assert_eq!(leaves.len(), 2);

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

        // Verify tree structure survived round-trip
        assert_eq!(loaded.entries.len(), 4);
        let loaded_leaves = loaded.list_leaves();
        assert_eq!(loaded_leaves.len(), 2);
        assert!(loaded_leaves.contains(&id_c));
        assert!(loaded_leaves.contains(&id_d));

        // Verify parent linking
        let path_to_c = loaded.get_path_to_entry(&id_c);
        assert_eq!(path_to_c, vec![id_a.as_str(), id_b.as_str(), id_c.as_str()]);

        let path_to_d = loaded.get_path_to_entry(&id_d);
        assert_eq!(path_to_d, vec![id_a.as_str(), id_d.as_str()]);
    }

    // ======================================================================
    // Session directory resolution from CWD
    // ======================================================================

    #[test]
    fn test_encode_cwd_strips_leading_separators() {
        let path = std::path::Path::new("/home/user/my-project");
        let encoded = encode_cwd(path);
        assert_eq!(encoded, "--home-user-my-project--");
        assert!(!encoded.contains('/'));
    }

    #[test]
    fn test_encode_cwd_handles_deeply_nested_path() {
        let path = std::path::Path::new("/a/b/c/d/e/f");
        let encoded = encode_cwd(path);
        assert_eq!(encoded, "--a-b-c-d-e-f--");
    }

    #[test]
    fn test_save_creates_project_session_dir_from_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.append_message(make_test_message("test"));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        // The saved path should be inside a CWD-encoded subdirectory
        let parent = path.parent().unwrap();
        let dir_name = parent.file_name().unwrap().to_string_lossy();
        assert!(
            dir_name.starts_with("--"),
            "session dir should start with --"
        );
        assert!(dir_name.ends_with("--"), "session dir should end with --");

        // The file should have .jsonl extension
        assert_eq!(path.extension().unwrap(), "jsonl");
    }

    #[test]
    fn test_save_uses_session_header_cwd_for_project_session_dir() {
        let _lock = current_dir_lock();
        let process_cwd = tempfile::tempdir().unwrap();
        let _guard = CurrentDirGuard::new(process_cwd.path());

        let sessions_root = tempfile::tempdir().unwrap();
        let session_cwd = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(sessions_root.path().to_path_buf()));
        session.header.cwd = session_cwd.path().display().to_string();
        session.append_message(make_test_message("test"));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().expect("session path");
        let expected_dir = sessions_root.path().join(encode_cwd(session_cwd.path()));
        let process_dir = sessions_root.path().join(encode_cwd(process_cwd.path()));

        assert_eq!(path.parent(), Some(expected_dir.as_path()));
        assert_ne!(path.parent(), Some(process_dir.as_path()));
    }

    #[cfg(feature = "session-index")]
    #[test]
    fn test_can_reuse_known_entry_requires_matching_mtime_and_size() {
        let known_entry = SessionPickEntry {
            path: PathBuf::from("session.jsonl"),
            id: "session-id".to_string(),
            cwd: "/work".to_string(),
            timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            message_count: 4,
            name: Some("cached".to_string()),
            last_modified_ms: 1234,
            size_bytes: 4096,
        };

        assert!(can_reuse_known_entry(&known_entry, 1234, 4096));
        assert!(!can_reuse_known_entry(&known_entry, 1235, 4096));
        assert!(!can_reuse_known_entry(&known_entry, 1234, 4097));
    }

    #[test]
    fn read_capped_utf8_line_with_limit_rejects_oversized_line_without_newline() {
        let oversized = "x".repeat(5);
        let mut reader = std::io::Cursor::new(oversized.into_bytes());

        let err = read_capped_utf8_line_with_limit(&mut reader, 4).expect_err("oversized line");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("JSONL line exceeds 4 bytes"));
    }

    #[test]
    fn read_capped_utf8_line_with_limit_allows_exact_limit_before_newline() {
        let mut reader = std::io::Cursor::new(b"abcd\n".to_vec());

        let line = read_capped_utf8_line_with_limit(&mut reader, 4)
            .expect("read line")
            .expect("line present");
        assert_eq!(line, "abcd\n");
        assert!(
            read_capped_utf8_line_with_limit(&mut reader, 4)
                .expect("read eof")
                .is_none()
        );
    }

    #[test]
    fn read_capped_utf8_line_with_limit_drains_oversized_line_remainder() {
        let mut reader = std::io::Cursor::new(b"xxxxx\ny\n".to_vec());

        let err = read_capped_utf8_line_with_limit(&mut reader, 4).expect_err("oversized line");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

        let next_line = read_capped_utf8_line_with_limit(&mut reader, 4)
            .expect("read next line")
            .expect("next line present");
        assert_eq!(next_line, "y\n");
    }

    #[cfg(feature = "session-index")]
    #[test]
    fn test_scan_sessions_on_disk_ignores_stale_known_entry_when_size_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.append_message(make_test_message("first"));
        session.append_message(make_test_message("second"));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().expect("session path");
        let metadata = std::fs::metadata(&path).expect("session metadata");
        let disk_size = metadata.len();
        #[allow(clippy::cast_possible_truncation)]
        let disk_ms = metadata
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let stale_known_entry = SessionPickEntry {
            path: path.clone(),
            id: session.header.id.clone(),
            cwd: session.header.cwd.clone(),
            timestamp: session.header.timestamp.clone(),
            message_count: 999,
            name: Some("stale".to_string()),
            last_modified_ms: disk_ms,
            size_bytes: disk_size.saturating_add(1),
        };

        let session_dir = path.parent().expect("session parent").to_path_buf();
        let scanned =
            run_async(async { scan_sessions_on_disk(&session_dir, vec![stale_known_entry]).await })
                .expect("scan sessions");
        assert!(scanned.failed_paths.is_empty());
        assert_eq!(scanned.entries.len(), 1);
        assert_eq!(scanned.refreshed_entries.len(), 1);
        assert_eq!(scanned.entries[0].path, path);
        assert_eq!(scanned.entries[0].message_count, 2);
        assert_eq!(scanned.entries[0].size_bytes, disk_size);
    }

    #[cfg(feature = "session-index")]
    #[test]
    fn test_merge_scanned_session_entries_replaces_cached_entry_when_size_changes() {
        let path = PathBuf::from("session.jsonl");
        let mut by_path = HashMap::from([(
            path.clone(),
            SessionPickEntry {
                path: path.clone(),
                id: "session-id".to_string(),
                cwd: "/work".to_string(),
                timestamp: "2026-01-01T00:00:00.000Z".to_string(),
                message_count: 1,
                name: Some("cached".to_string()),
                last_modified_ms: 1234,
                size_bytes: 4096,
            },
        )]);

        merge_scanned_session_entries(
            &mut by_path,
            vec![SessionPickEntry {
                path: path.clone(),
                id: "session-id".to_string(),
                cwd: "/work".to_string(),
                timestamp: "2026-01-01T00:00:00.000Z".to_string(),
                message_count: 2,
                name: Some("disk".to_string()),
                last_modified_ms: 1234,
                size_bytes: 8192,
            }],
        );

        let merged = by_path.get(&path).expect("merged entry");
        assert_eq!(merged.message_count, 2);
        assert_eq!(merged.name.as_deref(), Some("disk"));
        assert_eq!(merged.size_bytes, 8192);
    }

    #[cfg(feature = "session-index")]
    #[test]
    fn test_merge_scanned_session_entries_replaces_cached_entry_even_if_disk_mtime_regresses() {
        let path = PathBuf::from("session.jsonl");
        let mut by_path = HashMap::from([(
            path.clone(),
            SessionPickEntry {
                path: path.clone(),
                id: "session-id".to_string(),
                cwd: "/work".to_string(),
                timestamp: "2026-01-02T00:00:00.000Z".to_string(),
                message_count: 9,
                name: Some("cached".to_string()),
                last_modified_ms: 2000,
                size_bytes: 4096,
            },
        )]);

        merge_scanned_session_entries(
            &mut by_path,
            vec![SessionPickEntry {
                path: path.clone(),
                id: "session-id".to_string(),
                cwd: "/work".to_string(),
                timestamp: "2026-01-01T00:00:00.000Z".to_string(),
                message_count: 3,
                name: Some("disk".to_string()),
                last_modified_ms: 1000,
                size_bytes: 2048,
            }],
        );

        let merged = by_path.get(&path).expect("merged entry");
        assert_eq!(merged.message_count, 3);
        assert_eq!(merged.name.as_deref(), Some("disk"));
        assert_eq!(merged.last_modified_ms, 1000);
        assert_eq!(merged.size_bytes, 2048);
    }

    #[cfg(feature = "session-index")]
    #[test]
    fn test_scan_sessions_on_disk_reports_failed_paths_for_corrupt_changed_session() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.append_message(make_test_message("first"));
        session.append_message(make_test_message("second"));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().expect("session path");
        let metadata = std::fs::metadata(&path).expect("session metadata");
        let disk_size = metadata.len();
        #[allow(clippy::cast_possible_truncation)]
        let disk_ms = metadata
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let stale_known_entry = SessionPickEntry {
            path: path.clone(),
            id: session.header.id.clone(),
            cwd: session.header.cwd.clone(),
            timestamp: session.header.timestamp.clone(),
            message_count: 999,
            name: Some("stale".to_string()),
            last_modified_ms: disk_ms,
            size_bytes: disk_size,
        };

        std::fs::write(&path, b"not valid jsonl\n").expect("corrupt session");

        let session_dir = path.parent().expect("session parent").to_path_buf();
        let scanned =
            run_async(async { scan_sessions_on_disk(&session_dir, vec![stale_known_entry]).await })
                .expect("scan sessions");

        assert!(scanned.entries.is_empty());
        assert!(scanned.refreshed_entries.is_empty());
        assert_eq!(scanned.failed_paths, vec![path]);
    }

    #[cfg(feature = "session-index")]
    #[test]
    fn test_continue_recent_in_dir_prunes_corrupt_stale_index_entry() {
        let _lock = current_dir_lock();
        let process_cwd = tempfile::tempdir().unwrap();
        let _guard = CurrentDirGuard::new(process_cwd.path());

        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.append_message(make_test_message("first"));
        session.append_message(make_test_message("second"));

        run_async(async { session.save().await }).expect("save session");
        let path = session.path.clone().expect("session path");

        let index = SessionIndex::for_sessions_root(temp.path());
        index.index_session(&session).expect("index session");
        let cwd_display = session.header.cwd.clone();
        let expected_path = path.display().to_string();
        let has_indexed_path = index
            .list_sessions(Some(&cwd_display))
            .expect("list indexed sessions")
            .into_iter()
            .any(|meta| meta.path.eq(&expected_path));
        assert!(
            has_indexed_path,
            "expected indexed session before corruption"
        );

        std::fs::write(&path, b"not valid jsonl\n").expect("corrupt session");

        let resumed = run_async(async {
            Session::continue_recent_in_dir(Some(temp.path()), &Config::default()).await
        })
        .expect("continue recent");

        assert!(resumed.path.is_none(), "expected a fresh unsaved session");
        assert_eq!(resumed.session_dir, Some(temp.path().to_path_buf()));

        let still_indexed = index
            .list_sessions(Some(&cwd_display))
            .expect("list indexed sessions after cleanup")
            .into_iter()
            .any(|meta| meta.path.eq(&expected_path));
        assert!(
            !still_indexed,
            "corrupt session should be pruned from the recent-session index"
        );
    }

    #[cfg(feature = "session-index")]
    #[test]
    fn test_continue_recent_in_dir_prunes_missing_stale_index_entry() {
        let _lock = current_dir_lock();
        let process_cwd = tempfile::tempdir().unwrap();
        let _guard = CurrentDirGuard::new(process_cwd.path());

        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.append_message(make_test_message("first"));

        run_async(async { session.save().await }).expect("save session");
        let path = session.path.clone().expect("session path");

        let index = SessionIndex::for_sessions_root(temp.path());
        index.index_session(&session).expect("index session");
        let cwd_display = session.header.cwd.clone();
        let expected_path = path.display().to_string();
        let has_indexed_path = index
            .list_sessions(Some(&cwd_display))
            .expect("list indexed sessions")
            .into_iter()
            .any(|meta| meta.path.eq(&expected_path));
        assert!(
            has_indexed_path,
            "expected indexed session before moving file"
        );

        let moved_path = path.with_extension("bak");
        std::fs::rename(&path, &moved_path).expect("move session away from indexed path");

        let resumed = run_async(async {
            Session::continue_recent_in_dir(Some(temp.path()), &Config::default()).await
        })
        .expect("continue recent");

        assert!(resumed.path.is_none(), "expected a fresh unsaved session");
        assert_eq!(resumed.session_dir, Some(temp.path().to_path_buf()));

        let still_indexed = index
            .list_sessions(Some(&cwd_display))
            .expect("list indexed sessions after cleanup")
            .into_iter()
            .any(|meta| meta.path.eq(&expected_path));
        assert!(
            !still_indexed,
            "missing session should be pruned from the recent-session index"
        );
    }

    #[cfg(feature = "session-index")]
    #[test]
    fn test_continue_recent_in_dir_prunes_index_when_project_dir_is_missing() {
        let _lock = current_dir_lock();
        let process_cwd = tempfile::tempdir().unwrap();
        let _guard = CurrentDirGuard::new(process_cwd.path());

        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.append_message(make_test_message("first"));

        run_async(async { session.save().await }).expect("save session");
        let path = session.path.clone().expect("session path");

        let index = SessionIndex::for_sessions_root(temp.path());
        index.index_session(&session).expect("index session");
        let cwd_display = session.header.cwd.clone();
        let expected_path = path.display().to_string();
        let cwd = std::path::Path::new(&cwd_display);
        let project_session_dir = temp.path().join(encode_cwd(cwd));
        let moved_project_dir = temp.path().join("moved-project-dir");

        std::fs::rename(&project_session_dir, &moved_project_dir)
            .expect("move project session dir away");

        let resumed = run_async(async {
            Session::continue_recent_in_dir(Some(temp.path()), &Config::default()).await
        })
        .expect("continue recent");

        assert!(resumed.path.is_none(), "expected a fresh unsaved session");
        assert_eq!(resumed.session_dir, Some(temp.path().to_path_buf()));

        let still_indexed = index
            .list_sessions(Some(&cwd_display))
            .expect("list indexed sessions after cleanup")
            .into_iter()
            .any(|meta| meta.path.eq(&expected_path));
        assert!(
            !still_indexed,
            "missing project dir should prune stale rows from the recent-session index"
        );
    }

    #[cfg(feature = "session-index")]
    #[cfg(unix)]
    #[test]
    fn split_indexed_session_entries_keeps_permission_denied_path_out_of_missing_bucket() {
        use crate::session_index::SessionMeta;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let guarded_dir = temp.path().join("guarded");
        std::fs::create_dir(&guarded_dir).expect("create guarded dir");
        let session_path = guarded_dir.join("session.jsonl");
        std::fs::write(&session_path, b"{\"version\":\"3\"}\n").expect("write session file");

        let original_mode = std::fs::metadata(&guarded_dir)
            .expect("guarded dir metadata")
            .permissions()
            .mode();
        std::fs::set_permissions(&guarded_dir, std::fs::Permissions::from_mode(0o000))
            .expect("chmod guarded dir");

        assert!(
            session_path.try_exists().is_err(),
            "expected permission-denied path probe for inaccessible parent directory"
        );

        let meta = SessionMeta {
            path: session_path.display().to_string(),
            id: "session-id".to_string(),
            cwd: temp.path().display().to_string(),
            timestamp: "2026-03-15T00:00:00.000Z".to_string(),
            message_count: 1,
            last_modified_ms: 0,
            size_bytes: 16,
            name: Some("guarded".to_string()),
        };

        let (entries, missing_paths) = split_indexed_session_entries(vec![meta]);

        std::fs::set_permissions(&guarded_dir, std::fs::Permissions::from_mode(original_mode))
            .expect("restore guarded dir permissions");

        assert!(
            missing_paths.is_empty(),
            "permission errors must not be classified as missing indexed sessions"
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, session_path);
    }

    #[cfg(feature = "session-index")]
    #[cfg(unix)]
    #[test]
    fn test_continue_recent_in_dir_prunes_unreadable_cached_entry_on_open_failure() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.append_message(make_test_message("first"));

        run_async(async { session.save().await }).expect("save session");
        let path = session.path.clone().expect("session path");

        let original_mode = std::fs::metadata(&path)
            .expect("session metadata")
            .permissions()
            .mode();

        let index = SessionIndex::for_sessions_root(temp.path());
        index.index_session(&session).expect("index session");
        let expected_path = path.display().to_string();
        let cwd_display = std::env::current_dir()
            .expect("current dir")
            .display()
            .to_string();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("chmod unreadable");

        let resumed = run_async(async {
            Session::continue_recent_in_dir(Some(temp.path()), &Config::default()).await
        })
        .expect("continue recent");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(original_mode))
            .expect("restore permissions");

        assert!(resumed.path.is_none(), "expected a fresh unsaved session");
        assert_eq!(resumed.session_dir, Some(temp.path().to_path_buf()));

        let still_indexed = index
            .list_sessions(Some(&cwd_display))
            .expect("list indexed sessions after cleanup")
            .into_iter()
            .any(|meta| meta.path.eq(&expected_path));
        assert!(
            !still_indexed,
            "unreadable session should be pruned from the recent-session index"
        );
    }

    #[cfg(feature = "session-index")]
    #[test]
    fn test_continue_recent_in_dir_refreshes_index_after_changed_disk_session() {
        let _lock = current_dir_lock();
        let process_cwd = tempfile::tempdir().unwrap();
        let _guard = CurrentDirGuard::new(process_cwd.path());

        let temp = tempfile::tempdir().expect("tempdir");
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.append_message(make_test_message("first"));

        run_async(async { session.save().await }).expect("save session");
        let path = session.path.clone().expect("session path");

        let index = SessionIndex::for_sessions_root(temp.path());
        index.index_session(&session).expect("index session");
        let cwd_display = session.header.cwd.clone();

        std::fs::write(
            &path,
            format!(
                "{}\n{{\"type\":\"message\"}}\n{{\"type\":\"message\"}}\n{{\"type\":\"session_info\",\"name\":\"Refreshed\"}}\n",
                serde_json::to_string(&session.header).expect("serialize header"),
            ),
        )
        .expect("rewrite session");

        let resumed = run_async(async {
            Session::continue_recent_in_dir(Some(temp.path()), &Config::default()).await
        })
        .expect("continue recent");

        assert_eq!(resumed.path.as_ref(), Some(&path));

        let indexed = index
            .list_sessions(Some(&cwd_display))
            .expect("list indexed sessions");
        assert_eq!(indexed.len(), 1);
        assert_eq!(indexed[0].path, path.display().to_string());
        assert_eq!(indexed[0].message_count, 2);
        assert_eq!(indexed[0].name.as_deref(), Some("Refreshed"));
    }

    #[cfg(feature = "session-index")]
    #[test]
    fn test_resume_with_picker_refreshes_index_after_changed_disk_session() {
        let _lock = current_dir_lock();
        let process_cwd = tempfile::tempdir().unwrap();
        let _guard = CurrentDirGuard::new(process_cwd.path());

        let temp = tempfile::tempdir().expect("tempdir");
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.append_message(make_test_message("first"));

        run_async(async { session.save().await }).expect("save session");
        let path = session.path.clone().expect("session path");

        let index = SessionIndex::for_sessions_root(temp.path());
        index.index_session(&session).expect("index session");
        let cwd_display = session.header.cwd.clone();

        std::fs::write(
            &path,
            format!(
                "{}\n{{\"type\":\"message\"}}\n{{\"type\":\"message\"}}\n{{\"type\":\"session_info\",\"name\":\"Refreshed\"}}\n",
                serde_json::to_string(&session.header).expect("serialize header"),
            ),
        )
        .expect("rewrite session");

        let resumed = run_async(async {
            Session::resume_with_picker(
                Some(temp.path()),
                &Config::default(),
                Some("1".to_string()),
            )
            .await
        })
        .expect("resume with picker");

        assert_eq!(resumed.path.as_ref(), Some(&path));

        let indexed = index
            .list_sessions(Some(&cwd_display))
            .expect("list indexed sessions");
        assert_eq!(indexed.len(), 1);
        assert_eq!(indexed[0].path, path.display().to_string());
        assert_eq!(indexed[0].message_count, 2);
        assert_eq!(indexed[0].name.as_deref(), Some("Refreshed"));
    }

    #[cfg(feature = "session-index")]
    #[test]
    fn test_load_session_meta_jsonl_errors_on_invalid_utf8_entry_line() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let session_path = temp.path().join("invalid-utf8.jsonl");

        let mut header = SessionHeader::new();
        header.id = "invalid-utf8".to_string();
        header.cwd = temp.path().display().to_string();
        header.timestamp = "2025-06-01T12:00:00.000Z".to_string();

        std::fs::write(
            &session_path,
            format!(
                "{}\n",
                serde_json::to_string(&header).expect("serialize header")
            ),
        )
        .expect("write header");

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&session_path)
            .expect("open session");
        file.write_all(b"{\"type\":\"message\"}\n")
            .expect("write valid entry");
        file.write_all(b"\xFF\xFE\n").expect("write invalid utf8");
        file.flush().expect("flush session");
        drop(file);

        let err = load_session_meta_jsonl(&session_path).expect_err("invalid utf8 should error");
        assert!(
            err.to_string().contains("Failed to read session entry"),
            "{err}"
        );
    }

    #[cfg(all(feature = "sqlite-sessions", feature = "session-index"))]
    #[test]
    fn test_scan_sessions_on_disk_reloads_sqlite_when_wal_stats_change() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir_and_store(
            Some(temp.path().to_path_buf()),
            SessionStoreKind::Sqlite,
        );
        session.append_message(make_test_message("sqlite"));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().expect("sqlite session path");
        let session_dir = path.parent().expect("session parent").to_path_buf();
        let (base_ms, base_size) = session_file_stats(&path).expect("base stats");

        let mut wal_path = path.as_os_str().to_os_string();
        wal_path.push("-wal");
        let wal_path = PathBuf::from(wal_path);
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        std::fs::write(&wal_path, b"walpayload").expect("write sqlite wal");

        let stale_known_entry = SessionPickEntry {
            path: path.clone(),
            id: session.header.id.clone(),
            cwd: session.header.cwd.clone(),
            timestamp: session.header.timestamp.clone(),
            message_count: 999,
            name: Some("stale".to_string()),
            last_modified_ms: base_ms,
            size_bytes: base_size,
        };

        let scanned =
            run_async(async { scan_sessions_on_disk(&session_dir, vec![stale_known_entry]).await })
                .expect("scan sessions");
        let (updated_ms, updated_size) = session_file_stats(&path).expect("updated stats");

        assert!(scanned.failed_paths.is_empty());
        assert_eq!(scanned.entries.len(), 1);
        assert_eq!(scanned.refreshed_entries.len(), 1);
        assert_eq!(scanned.entries[0].path, path);
        assert_eq!(scanned.entries[0].message_count, 1);
        assert_eq!(scanned.entries[0].size_bytes, updated_size);
        assert_eq!(scanned.entries[0].last_modified_ms, updated_ms);
    }

    #[cfg(all(feature = "sqlite-sessions", feature = "session-index"))]
    #[test]
    fn test_load_session_meta_sqlite_uses_wal_aware_stats() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir_and_store(
            Some(temp.path().to_path_buf()),
            SessionStoreKind::Sqlite,
        );
        session.append_message(make_test_message("sqlite"));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().expect("sqlite session path");

        let mut wal_path = path.as_os_str().to_os_string();
        wal_path.push("-wal");
        let wal_path = PathBuf::from(wal_path);
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        std::fs::write(&wal_path, b"walpayload").expect("write sqlite wal");

        let meta = load_session_meta_sqlite(&path).expect("load sqlite meta");
        let (expected_ms, expected_size) = session_file_stats(&path).expect("sqlite file stats");

        assert_eq!(meta.path, path);
        assert_eq!(meta.size_bytes, expected_size);
        assert_eq!(meta.last_modified_ms, expected_ms);
    }

    // ======================================================================
    // All entries corrupted (only header valid)
    // ======================================================================

    #[test]
    fn test_all_entries_corrupted_produces_empty_session() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.append_message(make_test_message("A"));
        session.append_message(make_test_message("B"));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        // Corrupt all entry lines (keep header at index 0)
        for (i, line) in lines.iter_mut().enumerate().skip(1) {
            *line = format!("GARBAGE_{i}");
        }
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

        let (loaded, diagnostics) = run_async(async {
            Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await
        })
        .unwrap();

        assert_eq!(diagnostics.skipped_entries.len(), 2);
        assert!(loaded.entries.is_empty());
        assert!(loaded.leaf_id.is_none());
        // Header should still be valid
        assert_eq!(loaded.header.id, session.header.id);
    }

    // ======================================================================
    // Unicode and special character content
    // ======================================================================

    #[test]
    fn test_unicode_content_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        let unicode_texts = [
            "Hello \u{1F600} World",    // emoji
            "\u{4F60}\u{597D}",         // Chinese
            "\u{0410}\u{0411}\u{0412}", // Cyrillic
            "caf\u{00E9}",              // accented
            "tab\there\nnewline",       // control chars
            "\"quoted\" and \\escaped", // JSON special chars
        ];

        for text in &unicode_texts {
            session.append_message(make_test_message(text));
        }

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

        assert_eq!(loaded.entries.len(), unicode_texts.len());

        for (i, entry) in loaded.entries.iter().enumerate() {
            if let SessionEntry::Message(msg) = entry {
                if let SessionMessage::User { content, .. } = &msg.message {
                    match content {
                        UserContent::Text(t) => assert_eq!(t, unicode_texts[i]),
                        UserContent::Blocks(_) => {
                            test_fail!("Expected UserContent::Text, got Blocks")
                        }
                    }
                }
            }
        }
    }

    // ======================================================================
    // Multiple compactions
    // ======================================================================

    #[test]
    fn test_multiple_compactions_latest_wins() {
        let mut session = Session::in_memory();

        let _id_a = session.append_message(make_test_message("old A"));
        let _id_b = session.append_message(make_test_message("old B"));
        let id_c = session.append_message(make_test_message("kept C"));

        // First compaction: keep from C
        session.append_compaction("Summary 1".to_string(), id_c, 1000, None, None);

        let _id_d = session.append_message(make_test_message("new D"));
        let id_e = session.append_message(make_test_message("new E"));

        // Second compaction: keep from E
        session.append_compaction("Summary 2".to_string(), id_e, 2000, None, None);

        let id_f = session.append_message(make_test_message("newest F"));

        session.navigate_to(&id_f);
        let messages = session.to_messages_for_current_path();

        // Old messages A, B should definitely not appear
        let all_text: String = messages
            .iter()
            .filter_map(|m| match m {
                Message::User(u) => match &u.content {
                    UserContent::Text(t) => Some(t.clone()),
                    UserContent::Blocks(_) => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");

        assert!(!all_text.contains("old A"), "A should be compacted away");
        assert!(!all_text.contains("old B"), "B should be compacted away");
    }

    // ======================================================================
    // Session with only metadata entries (no messages)
    // ======================================================================

    #[test]
    fn test_session_with_only_metadata_entries() {
        let mut session = Session::in_memory();

        session.append_model_change("anthropic".to_string(), "claude-opus".to_string());
        session.append_thinking_level_change("high".to_string());
        session.set_name("metadata-only");

        // to_messages should return empty (no actual messages)
        let messages = session.to_messages();
        assert!(messages.is_empty());

        // entries_for_current_path should still return the metadata entries
        let entries = session.entries_for_current_path();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn test_metadata_only_session_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        session.append_model_change("openai".to_string(), "gpt-4o".to_string());
        session.append_thinking_level_change("medium".to_string());

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

        assert_eq!(loaded.entries.len(), 2);
        assert!(
            loaded
                .entries
                .iter()
                .any(|e| matches!(e, SessionEntry::ModelChange(_)))
        );
        assert!(
            loaded
                .entries
                .iter()
                .any(|e| matches!(e, SessionEntry::ThinkingLevelChange(_)))
        );
    }

    // ======================================================================
    // Session name round-trip persistence
    // ======================================================================

    #[test]
    fn test_session_name_survives_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        session.append_message(make_test_message("Hello"));
        session.set_name("my-important-session");

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

        assert_eq!(loaded.get_name().as_deref(), Some("my-important-session"));
    }

    // ======================================================================
    // Trailing newline / whitespace in JSONL
    // ======================================================================

    #[test]
    fn test_trailing_whitespace_in_jsonl_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));
        session.append_message(make_test_message("test"));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        // Append extra blank lines at the end
        let mut contents = std::fs::read_to_string(&path).unwrap();
        contents.push_str("\n\n\n");
        std::fs::write(&path, contents).unwrap();

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

        assert_eq!(loaded.entries.len(), 1);
    }

    // ======================================================================
    // Branching after compaction
    // ======================================================================

    #[test]
    fn test_branching_after_compaction() {
        let mut session = Session::in_memory();

        let _id_a = session.append_message(make_test_message("old A"));
        let id_b = session.append_message(make_test_message("kept B"));

        session.append_compaction("Compacted".to_string(), id_b.clone(), 500, None, None);

        let id_c = session.append_message(make_test_message("C after compaction"));

        // Branch from B (the compaction keep-point)
        session.create_branch_from(&id_b);
        let id_d = session.append_message(make_test_message("D branch after compaction"));

        let leaves = session.list_leaves();
        assert_eq!(leaves.len(), 2);
        assert!(leaves.contains(&id_c));
        assert!(leaves.contains(&id_d));
    }

    // ======================================================================
    // Assistant message with tool calls round-trip
    // ======================================================================

    #[test]
    fn test_assistant_with_tool_calls_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let mut session = Session::create_with_dir(Some(temp.path().to_path_buf()));

        session.append_message(make_test_message("read my file"));

        let assistant = AssistantMessage {
            content: vec![
                ContentBlock::Text(TextContent::new("Let me read that for you.")),
                ContentBlock::ToolCall(crate::model::ToolCall {
                    id: "call_abc".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({"path": "src/main.rs"}),
                    thought_signature: None,
                }),
            ],
            api: "anthropic".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-test".to_string(),
            usage: Usage {
                input: 100,
                output: 50,
                cache_read: 0,
                cache_write: 0,
                total_tokens: 150,
                cost: Cost::default(),
            },
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: 12345,
        };
        session.append_message(SessionMessage::Assistant { message: assistant });

        session.append_message(SessionMessage::ToolResult {
            tool_call_id: "call_abc".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("fn main() {}"))],
            details: Some(serde_json::json!({"lines": 1, "truncated": false})),
            is_error: false,
            timestamp: Some(12346),
        });

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

        assert_eq!(loaded.entries.len(), 3);

        // Verify tool call content survived
        let has_tool_call = loaded.entries.iter().any(|e| {
            if let SessionEntry::Message(msg) = e {
                if let SessionMessage::Assistant { message } = &msg.message {
                    return message
                        .content
                        .iter()
                        .any(|c| matches!(c, ContentBlock::ToolCall(tc) if tc.id.eq("call_abc")));
                }
            }
            false
        });
        assert!(has_tool_call, "tool call should survive round-trip");

        // Verify tool result details survived
        let has_details = loaded.entries.iter().any(|e| {
            if let SessionEntry::Message(msg) = e {
                if let SessionMessage::ToolResult { details, .. } = &msg.message {
                    return details.is_some();
                }
            }
            false
        });
        assert!(has_details, "tool result details should survive round-trip");
    }

    // ======================================================================
    // FUZZ-P1.4: Proptest coverage for Session JSONL parsing
    // ======================================================================

    mod proptest_session {
        use super::*;
        use proptest::prelude::*;
        use serde_json::json;

        /// Generate a random valid timestamp string.
        fn timestamp_strategy() -> impl Strategy<Value = String> {
            (
                2020u32..2030,
                1u32..13,
                1u32..29,
                0u32..24,
                0u32..60,
                0u32..60,
            )
                .prop_map(|(y, mo, d, h, mi, s)| {
                    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.000Z")
                })
        }

        /// Generate a random entry ID (8 hex chars).
        fn entry_id_strategy() -> impl Strategy<Value = String> {
            "[0-9a-f]{8}"
        }

        /// Generate an arbitrary JSON value of bounded depth/size.
        fn bounded_json_value(max_depth: u32) -> BoxedStrategy<serde_json::Value> {
            if max_depth == 0 {
                prop_oneof![
                    Just(json!(null)),
                    any::<bool>().prop_map(|b| json!(b)),
                    any::<i64>().prop_map(|n| json!(n)),
                    "[a-zA-Z0-9 ]{0,32}".prop_map(|s| json!(s)),
                ]
                .boxed()
            } else {
                prop_oneof![
                    Just(json!(null)),
                    any::<bool>().prop_map(|b| json!(b)),
                    any::<i64>().prop_map(|n| json!(n)),
                    "[a-zA-Z0-9 ]{0,32}".prop_map(|s| json!(s)),
                    prop::collection::vec(bounded_json_value(max_depth - 1), 0..4)
                        .prop_map(serde_json::Value::Array),
                ]
                .boxed()
            }
        }

        /// Generate a valid `SessionEntry` JSON object for one of the known types.
        #[allow(clippy::too_many_lines)]
        fn valid_session_entry_json() -> impl Strategy<Value = serde_json::Value> {
            let ts = timestamp_strategy();
            let eid = entry_id_strategy();
            let parent = prop::option::of(entry_id_strategy());

            (ts, eid, parent, 0u8..8).prop_flat_map(|(ts, eid, parent, variant)| {
                let base = json!({
                    "id": eid,
                    "parentId": parent,
                    "timestamp": ts,
                });

                match variant {
                    0 => {
                        // Message - User
                        "[a-zA-Z0-9 ]{1,64}"
                            .prop_map(move |text| {
                                let mut v = base.clone();
                                v["type"] = json!("message");
                                v["message"] = json!({
                                    "role": "user",
                                    "content": text,
                                });
                                v
                            })
                            .boxed()
                    }
                    1 => {
                        // Message - Assistant
                        "[a-zA-Z0-9 ]{1,64}"
                            .prop_map(move |text| {
                                let mut v = base.clone();
                                v["type"] = json!("message");
                                v["message"] = json!({
                                    "role": "assistant",
                                    "content": [{"type": "text", "text": text}],
                                    "api": "anthropic",
                                    "provider": "anthropic",
                                    "model": "test-model",
                                    "usage": {
                                        "input": 10,
                                        "output": 5,
                                        "cacheRead": 0,
                                        "cacheWrite": 0,
                                        "totalTokens": 15,
                                        "cost": {"input": 0.0, "output": 0.0, "total": 0.0}
                                    },
                                    "stopReason": "end_turn",
                                    "timestamp": 12345,
                                });
                                v
                            })
                            .boxed()
                    }
                    2 => {
                        // ModelChange
                        ("[a-z]{3,8}", "[a-z0-9-]{5,20}")
                            .prop_map(move |(provider, model)| {
                                let mut v = base.clone();
                                v["type"] = json!("model_change");
                                v["provider"] = json!(provider);
                                v["modelId"] = json!(model);
                                v
                            })
                            .boxed()
                    }
                    3 => {
                        // ThinkingLevelChange
                        prop_oneof![
                            Just("off".to_string()),
                            Just("low".to_string()),
                            Just("medium".to_string()),
                            Just("high".to_string()),
                        ]
                        .prop_map(move |level| {
                            let mut v = base.clone();
                            v["type"] = json!("thinking_level_change");
                            v["thinkingLevel"] = json!(level);
                            v
                        })
                        .boxed()
                    }
                    4 => {
                        // Compaction
                        ("[a-zA-Z0-9 ]{1,32}", entry_id_strategy(), 100u64..100_000)
                            .prop_map(move |(summary, kept_id, tokens)| {
                                let mut v = base.clone();
                                v["type"] = json!("compaction");
                                v["summary"] = json!(summary);
                                v["firstKeptEntryId"] = json!(kept_id);
                                v["tokensBefore"] = json!(tokens);
                                v
                            })
                            .boxed()
                    }
                    5 => {
                        // Label
                        (entry_id_strategy(), prop::option::of("[a-zA-Z0-9 ]{1,16}"))
                            .prop_map(move |(target, label)| {
                                let mut v = base.clone();
                                v["type"] = json!("label");
                                v["targetId"] = json!(target);
                                if let Some(l) = label {
                                    v["label"] = json!(l);
                                }
                                v
                            })
                            .boxed()
                    }
                    6 => {
                        // SessionInfo
                        prop::option::of("[a-zA-Z0-9 ]{1,32}")
                            .prop_map(move |name| {
                                let mut v = base.clone();
                                v["type"] = json!("session_info");
                                if let Some(n) = name {
                                    v["name"] = json!(n);
                                }
                                v
                            })
                            .boxed()
                    }
                    _ => {
                        // Custom
                        ("[a-z_]{3,12}", bounded_json_value(2))
                            .prop_map(move |(custom_type, data)| {
                                let mut v = base.clone();
                                v["type"] = json!("custom");
                                v["customType"] = json!(custom_type);
                                v["data"] = data;
                                v
                            })
                            .boxed()
                    }
                }
            })
        }

        /// Generate a corrupted JSON line (valid JSON but wrong shape for `SessionEntry`).
        fn corrupted_entry_json() -> impl Strategy<Value = String> {
            prop_oneof![
                // Missing "type" field
                Just(r#"{"id":"aaaaaaaa","timestamp":"2024-01-01T00:00:00.000Z"}"#.to_string()),
                // Unknown type
                Just(r#"{"type":"unknown_type","id":"bbbbbbbb","timestamp":"2024-01-01T00:00:00.000Z"}"#.to_string()),
                // Empty object
                Just(r"{}".to_string()),
                // Array instead of object
                Just(r"[1,2,3]".to_string()),
                // Scalar values
                Just(r"42".to_string()),
                Just(r#""just a string""#.to_string()),
                Just(r"null".to_string()),
                Just(r"true".to_string()),
                // Truncated JSON (simulating crash)
                Just(r#"{"type":"message","id":"cccccccc","timestamp":"2024-01-01T"#.to_string()),
                // Valid JSON with wrong field types
                Just(r#"{"type":"message","id":12345,"timestamp":"2024-01-01T00:00:00.000Z"}"#.to_string()),
            ]
        }

        /// Build a complete JSONL file string from header + entries.
        fn build_jsonl(header: &str, entry_lines: &[String]) -> String {
            let mut lines = vec![header.to_string()];
            lines.extend(entry_lines.iter().cloned());
            lines.join("\n")
        }

        // ------------------------------------------------------------------
        // Proptest 1: SessionEntry deserialization never panics
        // ------------------------------------------------------------------
        proptest! {
            #![proptest_config(ProptestConfig {
                cases: 256,
                max_shrink_iters: 200,
                .. ProptestConfig::default()
            })]

            #[test]
            fn session_entry_deser_never_panics(
                entry_json in valid_session_entry_json()
            ) {
                let json_str = entry_json.to_string();
                // Must not panic — Ok or Err is fine
                let _ = serde_json::from_str::<SessionEntry>(&json_str);
            }
        }

        // ------------------------------------------------------------------
        // Proptest 2: Corrupted/malformed input never panics
        // ------------------------------------------------------------------
        proptest! {
            #![proptest_config(ProptestConfig {
                cases: 256,
                max_shrink_iters: 200,
                .. ProptestConfig::default()
            })]

            #[test]
            fn corrupted_entry_deser_never_panics(
                line in corrupted_entry_json()
            ) {
                let _ = serde_json::from_str::<SessionEntry>(&line);
            }

            #[test]
            fn arbitrary_bytes_deser_never_panics(
                raw in prop::collection::vec(any::<u8>(), 0..512)
            ) {
                // Even random bytes must not panic serde
                if let Ok(s) = String::from_utf8(raw) {
                    let _ = serde_json::from_str::<SessionEntry>(&s);
                }
            }
        }

        // ------------------------------------------------------------------
        // Proptest 3: Valid entries round-trip through serialization
        // ------------------------------------------------------------------
        proptest! {
            #![proptest_config(ProptestConfig {
                cases: 256,
                max_shrink_iters: 200,
                .. ProptestConfig::default()
            })]

            #[test]
            fn valid_entry_round_trip(
                entry_json in valid_session_entry_json()
            ) {
                let json_str = entry_json.to_string();
                if let Ok(entry) = serde_json::from_str::<SessionEntry>(&json_str) {
                    // Serialize back
                    let reserialized = serde_json::to_string(&entry).unwrap();
                    // Deserialize again
                    let re_entry = serde_json::from_str::<SessionEntry>(&reserialized).unwrap();
                    // Both should have the same entry ID
                    assert_eq!(entry.base_id(), re_entry.base_id());
                    // Both should have the same type tag
                    assert_eq!(
                        std::mem::discriminant(&entry),
                        std::mem::discriminant(&re_entry)
                    );
                }
            }
        }

        // ------------------------------------------------------------------
        // Proptest 4: Full JSONL load with mixed valid/invalid lines
        //             recovers valid entries and reports diagnostics
        // ------------------------------------------------------------------
        proptest! {
            #![proptest_config(ProptestConfig {
                cases: 128,
                max_shrink_iters: 100,
                .. ProptestConfig::default()
            })]

            #[test]
            fn jsonl_corrupted_recovery(
                valid_entries in prop::collection::vec(valid_session_entry_json(), 1..8),
                corrupted_lines in prop::collection::vec(corrupted_entry_json(), 0..5),
                interleave_seed in any::<u64>(),
            ) {
                let header_json = json!({
                    "type": "session",
                    "version": 3,
                    "id": "testid01",
                    "timestamp": "2024-01-01T00:00:00.000Z",
                    "cwd": "/tmp/test"
                }).to_string();

                // Interleave valid and corrupted lines deterministically
                let valid_strs: Vec<String> = valid_entries.iter().map(ToString::to_string).collect();
                let total = valid_strs.len() + corrupted_lines.len();
                let mut all_lines: Vec<(bool, String)> = Vec::with_capacity(total);
                for s in &valid_strs {
                    all_lines.push((true, s.clone()));
                }
                for s in &corrupted_lines {
                    all_lines.push((false, s.clone()));
                }

                // Deterministic shuffle based on seed
                let mut seed = interleave_seed;
                for i in (1..all_lines.len()).rev() {
                    seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    let j = (seed >> 33) as usize % (i + 1);
                    all_lines.swap(i, j);
                }

                let entry_lines: Vec<String> = all_lines.iter().map(|(_, s)| s.clone()).collect();
                let content = build_jsonl(&header_json, &entry_lines);

                // Write to temp file and load
                let temp_dir = tempfile::tempdir().unwrap();
                let file_path = temp_dir.path().join("test_session.jsonl");
                std::fs::write(&file_path, &content).unwrap();

                let (session, diagnostics) = run_async(async {
                    Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
                }).unwrap();

                // Invariant: parsed + skipped == total lines (all non-empty)
                let total_parsed = session.entries.len();
                assert_eq!(
                    total_parsed + diagnostics.skipped_entries.len(),
                    total,
                    "parsed ({total_parsed}) + skipped ({}) should equal total lines ({total})",
                    diagnostics.skipped_entries.len()
                );
            }
        }

        // ------------------------------------------------------------------
        // Proptest 5: Orphaned parent links are detected
        // ------------------------------------------------------------------
        proptest! {
            #![proptest_config(ProptestConfig {
                cases: 128,
                max_shrink_iters: 100,
                .. ProptestConfig::default()
            })]

            #[test]
            fn orphaned_parent_links_detected(
                n_entries in 2usize..10,
                orphan_idx in 0usize..8,
            ) {
                let orphan_idx = orphan_idx % n_entries;
                let header_json = json!({
                    "type": "session",
                    "version": 3,
                    "id": "testid01",
                    "timestamp": "2024-01-01T00:00:00.000Z",
                    "cwd": "/tmp/test"
                }).to_string();

                let mut entry_lines = Vec::new();
                let mut prev_id: Option<String> = None;

                for i in 0..n_entries {
                    let eid = format!("{i:08x}");
                    let parent = if i.eq(&orphan_idx) {
                        // Point to a nonexistent parent
                        Some("deadbeef".to_string())
                    } else {
                        prev_id.clone()
                    };

                    let entry = json!({
                        "type": "message",
                        "id": eid,
                        "parentId": parent,
                        "timestamp": "2024-01-01T00:00:00.000Z",
                        "message": {
                            "role": "user",
                            "content": format!("msg {i}"),
                        }
                    });
                    entry_lines.push(entry.to_string());
                    prev_id = Some(eid);
                }

                let content = build_jsonl(&header_json, &entry_lines);
                let temp_dir = tempfile::tempdir().unwrap();
                let file_path = temp_dir.path().join("orphan_test.jsonl");
                std::fs::write(&file_path, &content).unwrap();

                let (_session, diagnostics) = run_async(async {
                    Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
                }).unwrap();

                // The orphaned entry should be detected
                let has_orphan = diagnostics.orphaned_parent_links.iter().any(|o| {
                    o.missing_parent_id == "deadbeef"
                });
                assert!(
                    has_orphan,
                    "orphaned parent link to 'deadbeef' should be detected"
                );
            }
        }

        // ------------------------------------------------------------------
        // Proptest 6: ensure_entry_ids assigns IDs to entries without them
        // ------------------------------------------------------------------
        proptest! {
            #![proptest_config(ProptestConfig {
                cases: 128,
                max_shrink_iters: 100,
                .. ProptestConfig::default()
            })]

            #[test]
            fn ensure_entry_ids_fills_gaps(
                n_total in 1usize..20,
                missing_mask in prop::collection::vec(any::<bool>(), 1..20),
            ) {
                let n = n_total.min(missing_mask.len());
                let mut entries: Vec<SessionEntry> = (0..n).map(|i| {
                    let id = if missing_mask[i] {
                        None
                    } else {
                        Some(format!("{i:08x}"))
                    };
                    SessionEntry::Message(MessageEntry {
                        base: EntryBase {
                            id,
                            parent_id: None,
                            timestamp: "2024-01-01T00:00:00.000Z".to_string(),
                        },
                        message: SessionMessage::User {
                            content: UserContent::Text(format!("msg {i}")),
                            timestamp: Some(0),
                        },
                    })
                }).collect();

                ensure_entry_ids(&mut entries);

                // All entries must have IDs after the call
                for entry in &entries {
                    assert!(
                        entry.base_id().is_some(),
                        "all entries must have IDs after ensure_entry_ids"
                    );
                }

                // All IDs must be unique
                let ids: Vec<&String> = entries.iter().filter_map(|e| e.base_id()).collect();
                let unique: std::collections::HashSet<&String> = ids.iter().copied().collect();
                assert_eq!(
                    ids.len(),
                    unique.len(),
                    "all entry IDs must be unique"
                );
            }
        }

        // ------------------------------------------------------------------
        // Proptest 7: SessionHeader deserialization with boundary values
        // ------------------------------------------------------------------
        proptest! {
            #![proptest_config(ProptestConfig {
                cases: 256,
                max_shrink_iters: 200,
                .. ProptestConfig::default()
            })]

            #[test]
            fn session_header_deser_never_panics(
                version in prop::option::of(0u8..255),
                id in "[a-zA-Z0-9-]{0,64}",
                ts in timestamp_strategy(),
                cwd in "(/[a-zA-Z0-9_]{1,8}){0,5}",
                provider in prop::option::of("[a-z]{2,10}"),
                model_id in prop::option::of("[a-z0-9-]{2,20}"),
                thinking_level in prop::option::of("[a-z]{2,8}"),
            ) {
                let mut obj = json!({
                    "type": "session",
                    "id": id,
                    "timestamp": ts,
                    "cwd": cwd,
                });
                if let Some(v) = version {
                    obj["version"] = json!(v);
                }
                if let Some(p) = &provider {
                    obj["provider"] = json!(p);
                }
                if let Some(m) = &model_id {
                    obj["modelId"] = json!(m);
                }
                if let Some(t) = &thinking_level {
                    obj["thinkingLevel"] = json!(t);
                }
                let json_str = obj.to_string();
                let _ = serde_json::from_str::<SessionHeader>(&json_str);
            }
        }

        // ------------------------------------------------------------------
        // Proptest 8: Edge-case JSONL files
        // ------------------------------------------------------------------

        #[test]
        fn empty_file_returns_error() {
            let temp_dir = tempfile::tempdir().unwrap();
            let file_path = temp_dir.path().join("empty.jsonl");
            std::fs::write(&file_path, "").unwrap();

            let result = run_async(async {
                Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
            });
            assert!(result.is_err(), "empty file should return error");
        }

        #[test]
        fn header_only_file_produces_empty_session() {
            let header = json!({
                "type": "session",
                "version": 3,
                "id": "testid01",
                "timestamp": "2024-01-01T00:00:00.000Z",
                "cwd": "/tmp/test"
            })
            .to_string();

            let temp_dir = tempfile::tempdir().unwrap();
            let file_path = temp_dir.path().join("header_only.jsonl");
            std::fs::write(&file_path, &header).unwrap();

            let (session, diagnostics) = run_async(async {
                Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
            })
            .unwrap();

            assert!(
                session.entries.is_empty(),
                "header-only file should have no entries"
            );
            assert!(diagnostics.skipped_entries.is_empty(), "no lines to skip");
        }

        #[test]
        fn file_with_only_invalid_lines_has_diagnostics() {
            let header = json!({
                "type": "session",
                "version": 3,
                "id": "testid01",
                "timestamp": "2024-01-01T00:00:00.000Z",
                "cwd": "/tmp/test"
            })
            .to_string();

            let content = format!(
                "{}\n{}\n{}\n{}",
                header,
                r#"{"bad":"json","no":"type"}"#,
                r"not json at all",
                r#"{"type":"nonexistent_type","id":"aaa","timestamp":"2024-01-01T00:00:00.000Z"}"#,
            );

            let temp_dir = tempfile::tempdir().unwrap();
            let file_path = temp_dir.path().join("all_invalid.jsonl");
            std::fs::write(&file_path, &content).unwrap();

            let (session, diagnostics) = run_async(async {
                Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
            })
            .unwrap();

            assert!(
                session.entries.is_empty(),
                "all-invalid file should have no entries"
            );
            assert_eq!(
                diagnostics.skipped_entries.len(),
                3,
                "should have 3 skipped entries"
            );
        }

        #[test]
        fn duplicate_entry_ids_are_loaded_without_panic() {
            let header = json!({
                "type": "session",
                "version": 3,
                "id": "testid01",
                "timestamp": "2024-01-01T00:00:00.000Z",
                "cwd": "/tmp/test"
            })
            .to_string();

            let entry1 = json!({
                "type": "message",
                "id": "deadbeef",
                "timestamp": "2024-01-01T00:00:00.000Z",
                "message": {"role": "user", "content": "first"}
            })
            .to_string();

            let entry2 = json!({
                "type": "message",
                "id": "deadbeef",
                "timestamp": "2024-01-01T00:00:01.000Z",
                "message": {"role": "user", "content": "second (duplicate id)"}
            })
            .to_string();

            let content = format!("{header}\n{entry1}\n{entry2}");

            let temp_dir = tempfile::tempdir().unwrap();
            let file_path = temp_dir.path().join("dup_ids.jsonl");
            std::fs::write(&file_path, &content).unwrap();

            // Must not panic
            let (session, _diagnostics) = run_async(async {
                Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
            })
            .unwrap();

            assert_eq!(session.entries.len(), 2, "both entries should be loaded");
        }
    }

    // ------------------------------------------------------------------
    // Incremental append tests
    // ------------------------------------------------------------------

    #[test]
    fn test_incremental_append_writes_only_new_entries() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        // First save: full rewrite (persisted_entry_count == 0).
        session.append_message(make_test_message("msg A"));
        session.append_message(make_test_message("msg B"));
        run_async(async { session.save().await }).unwrap();

        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 2);
        assert_eq!(session.appends_since_checkpoint, 0);

        let path = session.path.clone().unwrap();
        let lines_after_first = std::fs::read_to_string(&path).unwrap().lines().count();
        // 1 header + 2 entries = 3 lines
        assert_eq!(lines_after_first, 3);

        // Add more entries and save again (incremental append).
        session.append_message(make_test_message("msg C"));
        run_async(async { session.save().await }).unwrap();

        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 3);
        assert_eq!(session.appends_since_checkpoint, 1);

        let lines_after_second = std::fs::read_to_string(&path).unwrap().lines().count();
        // 1 header + 3 entries = 4 lines
        assert_eq!(lines_after_second, 4);
    }

    #[test]
    fn test_header_change_forces_full_rewrite() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("msg A"));
        run_async(async { session.save().await }).unwrap();
        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 1);
        assert!(!session.header_dirty);

        // Modify header.
        session.set_model_header(Some("new-provider".to_string()), None, None);
        assert!(session.header_dirty);

        session.append_message(make_test_message("msg B"));
        run_async(async { session.save().await }).unwrap();

        // Full rewrite resets all counters.
        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 2);
        assert!(!session.header_dirty);
        assert_eq!(session.appends_since_checkpoint, 0);

        // Verify header on disk has the new provider.
        let path = session.path.clone().unwrap();
        let first_line = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        let header: serde_json::Value = serde_json::from_str(&first_line).unwrap();
        assert_eq!(header["provider"], "new-provider");
    }

    #[test]
    fn test_compaction_entry_uses_incremental_append() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        let id_a = session.append_message(make_test_message("msg A"));
        run_async(async { session.save().await }).unwrap();
        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 1);

        // Append a compaction entry. This should still be eligible for
        // incremental append; checkpoint rewrite cadence handles periodic
        // full rewrites for cleanup/corruption recovery.
        session.append_compaction("summary".to_string(), id_a, 100, None, None);
        session.append_message(make_test_message("msg B"));

        run_async(async { session.save().await }).unwrap();

        // Incremental append: persisted count advances and checkpoint counter increments.
        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 3);
        assert_eq!(session.appends_since_checkpoint, 1);

        let path = session.path.clone().unwrap();
        let lines_after_second = std::fs::read_to_string(&path).unwrap().lines().count();
        // 1 header + 3 entries = 4 lines
        assert_eq!(lines_after_second, 4);
    }

    #[test]
    fn test_checkpoint_interval_forces_full_rewrite() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        // First save (full rewrite).
        session.append_message(make_test_message("initial"));
        run_async(async { session.save().await }).unwrap();

        // Simulate many incremental appends by setting the counter near threshold.
        let interval = compaction_checkpoint_interval();
        session.appends_since_checkpoint = interval;

        // Next save should trigger full rewrite due to checkpoint.
        session.append_message(make_test_message("triggers checkpoint"));
        run_async(async { session.save().await }).unwrap();

        // Full rewrite resets counters.
        assert_eq!(session.appends_since_checkpoint, 0);
        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_incremental_append_load_round_trip() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        // First save.
        session.append_message(make_test_message("msg A"));
        session.append_message(make_test_message("msg B"));
        run_async(async { session.save().await }).unwrap();

        // Incremental append.
        session.append_message(make_test_message("msg C"));
        run_async(async { session.save().await }).unwrap();

        let path = session.path.clone().unwrap();

        // Reload and verify all entries present.
        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

        assert_eq!(loaded.entries.len(), 3);
        // Verify the entry content by checking that we have messages A, B, C.
        let texts: Vec<&str> = loaded
            .entries
            .iter()
            .filter_map(|e| match e {
                SessionEntry::Message(m) => match &m.message {
                    SessionMessage::User {
                        content: UserContent::Text(t),
                        ..
                    } => Some(t.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["msg A", "msg B", "msg C"]);
    }

    #[test]
    fn test_persisted_entry_count_set_on_open() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("msg A"));
        session.append_message(make_test_message("msg B"));
        session.append_message(make_test_message("msg C"));
        run_async(async { session.save().await }).unwrap();

        let path = session.path.clone().unwrap();
        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

        assert_eq!(loaded.persisted_entry_count.load(Ordering::SeqCst), 3);
        assert!(!loaded.header_dirty);
        assert_eq!(loaded.appends_since_checkpoint, 0);
    }

    #[test]
    fn test_no_new_entries_is_noop() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("msg A"));
        run_async(async { session.save().await }).unwrap();

        let path = session.path.clone().unwrap();
        let mtime_before = std::fs::metadata(&path).unwrap().modified().unwrap();

        // Sleep briefly to ensure mtime would change if file was written.
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Save again with no changes.
        run_async(async { session.save().await }).unwrap();

        let mtime_after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "file should not be modified on no-op save"
        );
        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_incremental_append_caches_stay_valid() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("msg A"));
        run_async(async { session.save().await }).unwrap();

        // After full rewrite, caches rebuilt.
        assert_eq!(session.entry_index.len(), 1);

        // Incremental append: add more entries.
        let id_b = session.append_message(make_test_message("msg B"));
        let id_c = session.append_message(make_test_message("msg C"));
        run_async(async { session.save().await }).unwrap();

        // Caches should still be valid (not rebuilt, but maintained incrementally).
        assert_eq!(session.entry_index.len(), 3);
        assert!(session.entry_index.contains_key(&id_b));
        assert!(session.entry_index.contains_key(&id_c));
        assert_eq!(session.cached_message_count, 3);
    }

    #[test]
    fn test_set_branched_from_marks_header_dirty() {
        let mut session = Session::create();
        assert!(!session.header_dirty);

        session.set_branched_from(Some("/some/path".to_string()));
        assert!(session.header_dirty);
    }

    // ====================================================================
    // Crash-consistency and recovery tests (bd-3ar8v.2.7)
    // ====================================================================

    /// Helper: build a valid JSONL session file string with header + N entries.
    fn build_crash_test_session_file(num_entries: usize) -> String {
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "crash-test",
            "timestamp": "2024-06-01T00:00:00.000Z",
            "cwd": "/tmp/test"
        });
        let mut lines = vec![serde_json::to_string(&header).unwrap()];
        for i in 0..num_entries {
            let entry = serde_json::json!({
                "type": "message",
                "id": format!("entry-{i}"),
                "timestamp": "2024-06-01T00:00:00.000Z",
                "message": {"role": "user", "content": format!("message {i}")}
            });
            lines.push(serde_json::to_string(&entry).unwrap());
        }
        lines.join("\n")
    }

    #[test]
    fn crash_empty_file_returns_error() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("empty.jsonl");
        std::fs::write(&file_path, "").unwrap();

        let result = run_async(async {
            Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
        });
        assert!(result.is_err(), "empty file should fail to open");
    }

    #[test]
    fn crash_corrupted_header_returns_error() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("bad_header.jsonl");
        std::fs::write(&file_path, "NOT VALID JSON\n").unwrap();

        let result = run_async(async {
            Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
        });
        assert!(result.is_err(), "corrupted header should fail");
    }

    #[test]
    fn crash_header_only_loads_empty() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("header_only.jsonl");
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "hdr-only",
            "timestamp": "2024-06-01T00:00:00.000Z",
            "cwd": "/tmp/test"
        });
        std::fs::write(
            &file_path,
            format!("{}\n", serde_json::to_string(&header).unwrap()),
        )
        .unwrap();

        let (session, diagnostics) = run_async(async {
            Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
        })
        .unwrap();

        assert!(session.entries.is_empty());
        assert!(diagnostics.skipped_entries.is_empty());
    }

    #[test]
    fn crash_truncated_last_entry_recovers_preceding() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("truncated.jsonl");

        let mut content = build_crash_test_session_file(3);
        let truncation_point = content.rfind('\n').unwrap();
        content.truncate(truncation_point);
        content.push_str("\n{\"type\":\"message\",\"id\":\"partial");

        std::fs::write(&file_path, &content).unwrap();

        let (session, diagnostics) = run_async(async {
            Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
        })
        .unwrap();

        assert_eq!(session.entries.len(), 2);
        assert_eq!(diagnostics.skipped_entries.len(), 1);
    }

    #[test]
    fn crash_multiple_corrupted_entries_recovers_valid() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("multi_corrupt.jsonl");

        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "multi-corrupt",
            "timestamp": "2024-06-01T00:00:00.000Z",
            "cwd": "/tmp/test"
        });

        let valid_entry = |id: &str, text: &str| {
            serde_json::json!({
                "type": "message",
                "id": id,
                "timestamp": "2024-06-01T00:00:00.000Z",
                "message": {"role": "user", "content": text}
            })
            .to_string()
        };

        let lines = [
            serde_json::to_string(&header).unwrap(),
            valid_entry("v1", "first"),
            "GARBAGE LINE 1".to_string(),
            valid_entry("v2", "second"),
            "{incomplete json".to_string(),
            valid_entry("v3", "third"),
        ];

        std::fs::write(&file_path, lines.join("\n")).unwrap();

        let (session, diagnostics) = run_async(async {
            Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
        })
        .unwrap();

        assert_eq!(session.entries.len(), 3, "3 valid entries survive");
        assert_eq!(diagnostics.skipped_entries.len(), 2);
    }

    #[test]
    fn crash_incremental_append_survives_partial_write() {
        use std::io::Write;

        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("msg A"));
        session.append_message(make_test_message("msg B"));
        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        // Simulate crash during append: write truncated entry.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(
            file,
            "\n{{\"type\":\"message\",\"id\":\"crash-entry\",\"timestamp\":\"2024-06-01"
        )
        .unwrap();
        drop(file);

        let (loaded, diagnostics) = run_async(async {
            Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await
        })
        .unwrap();

        assert_eq!(loaded.entries.len(), 2, "original entries recovered");
        assert_eq!(diagnostics.skipped_entries.len(), 1);
    }

    #[test]
    fn crash_full_rewrite_atomic_persist() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("original"));
        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let original_content = std::fs::read_to_string(&path).unwrap();

        session.set_model_header(Some("new-provider".to_string()), None, None);
        session.append_message(make_test_message("second"));
        run_async(async { session.save().await }).unwrap();

        let new_content = std::fs::read_to_string(&path).unwrap();
        assert_ne!(original_content, new_content);

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
        assert_eq!(loaded.entries.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn crash_full_rewrite_preserves_existing_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("original"));
        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();

        session.set_model_header(Some("new-provider".to_string()), None, None);
        session.append_message(make_test_message("second"));
        run_async(async { session.save().await }).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o444, "full rewrite must preserve existing mode bits");
    }

    #[test]
    fn full_rewrite_preserves_entries_appended_by_other_writer() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("original"));
        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let mut stale_rewriter =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
        let mut appender =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();

        appender.append_message(make_test_message("from appender"));
        run_async(async { appender.save().await }).unwrap();

        stale_rewriter.set_model_header(Some("new-provider".to_string()), None, None);
        run_async(async { stale_rewriter.save().await }).unwrap();

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
        let entry_texts = loaded
            .entries
            .iter()
            .filter_map(|entry| match entry {
                SessionEntry::Message(message) => match &message.message {
                    SessionMessage::User { content, .. } => match content {
                        UserContent::Text(text) => Some(text.clone()),
                        UserContent::Blocks(_) => None,
                    },
                    SessionMessage::Assistant { message } => {
                        message.content.iter().find_map(|block| match block {
                            ContentBlock::Text(TextContent { text, .. }) => Some(text.clone()),
                            _ => None,
                        })
                    }
                    SessionMessage::ToolResult { .. } => None,
                    SessionMessage::Custom { .. } => None,
                    SessionMessage::BashExecution { .. } => None,
                    SessionMessage::BranchSummary { .. } => None,
                    SessionMessage::CompactionSummary { .. } => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(
            entry_texts.iter().any(|text| text.eq("from appender")),
            "full rewrite should preserve entries appended after this session was opened"
        );
        assert_eq!(loaded.header.provider.as_deref(), Some("new-provider"));
    }

    #[test]
    fn crash_flush_failure_restores_pending_mutations() {
        let mut queue = AutosaveQueue::with_limit(10);

        queue.enqueue_mutation(AutosaveMutationKind::Message);
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        queue.enqueue_mutation(AutosaveMutationKind::Metadata);
        assert_eq!(queue.pending_mutations, 3);

        let ticket = queue
            .begin_flush(AutosaveFlushTrigger::Periodic)
            .expect("should have ticket");
        assert_eq!(queue.pending_mutations, 0);

        queue.finish_flush(ticket, false);
        assert_eq!(queue.pending_mutations, 3, "mutations restored");
        assert_eq!(queue.flush_failed, 1);
    }

    #[test]
    fn crash_flush_failure_respects_queue_capacity() {
        let mut queue = AutosaveQueue::with_limit(3);

        for _ in 0..3 {
            queue.enqueue_mutation(AutosaveMutationKind::Message);
        }
        let ticket = queue.begin_flush(AutosaveFlushTrigger::Periodic).unwrap();

        queue.enqueue_mutation(AutosaveMutationKind::Message);
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        assert_eq!(queue.pending_mutations, 2);

        queue.finish_flush(ticket, false);
        assert_eq!(queue.pending_mutations, 3, "capped at max");
        assert!(queue.backpressure_events >= 2);
    }

    #[test]
    fn crash_shutdown_strict_propagates_error() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.path = Some(
            temp_dir
                .path()
                .join("nonexistent_dir")
                .join("session.jsonl"),
        );
        session.set_autosave_durability_for_test(AutosaveDurabilityMode::Strict);
        session.append_message(make_test_message("must save"));
        session
            .autosave_queue
            .enqueue_mutation(AutosaveMutationKind::Message);

        let result = run_async(async { session.flush_autosave_on_shutdown().await });
        assert!(result.is_err(), "strict mode propagates errors");
    }

    #[test]
    fn crash_shutdown_balanced_swallows_error() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.path = Some(
            temp_dir
                .path()
                .join("nonexistent_dir")
                .join("session.jsonl"),
        );
        session.set_autosave_durability_for_test(AutosaveDurabilityMode::Balanced);
        session.append_message(make_test_message("best effort"));
        session
            .autosave_queue
            .enqueue_mutation(AutosaveMutationKind::Message);

        let result = run_async(async { session.flush_autosave_on_shutdown().await });
        assert!(result.is_ok(), "balanced mode swallows errors");
    }

    #[test]
    fn crash_shutdown_throughput_skips_flush() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.path = Some(
            temp_dir
                .path()
                .join("nonexistent_dir")
                .join("session.jsonl"),
        );
        session.set_autosave_durability_for_test(AutosaveDurabilityMode::Throughput);
        session.append_message(make_test_message("no flush"));
        session
            .autosave_queue
            .enqueue_mutation(AutosaveMutationKind::Message);

        let result = run_async(async { session.flush_autosave_on_shutdown().await });
        assert!(result.is_ok());
        assert!(session.autosave_queue.pending_mutations > 0);
    }

    #[test]
    fn crash_save_reload_preserves_all_entry_types() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        let id_a = session.append_message(make_test_message("msg A"));
        session.append_model_change("provider-x".to_string(), "model-y".to_string());
        session.append_thinking_level_change("high".to_string());
        session.append_compaction("summary".to_string(), id_a, 500, None, None);
        session.append_message(make_test_message("msg B"));

        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
        assert_eq!(loaded.entries.len(), session.entries.len());
    }

    #[test]
    fn crash_checkpoint_rewrite_cleans_corruption() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("initial"));
        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        for i in 0..5 {
            session.append_message(make_test_message(&format!("msg {i}")));
            run_async(async { session.save().await }).unwrap();
        }

        // Corrupt an appended entry on disk.
        let content = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<String> = content.lines().map(String::from).collect();
        lines[3] = "CORRUPTED_ENTRY".to_string();
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

        // Force checkpoint: full rewrite replaces corrupted file with clean data.
        session.appends_since_checkpoint = compaction_checkpoint_interval();
        session.append_message(make_test_message("post checkpoint"));
        run_async(async { session.save().await }).unwrap();
        assert_eq!(session.appends_since_checkpoint, 0);

        let (reloaded, diagnostics) = run_async(async {
            Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await
        })
        .unwrap();
        assert!(diagnostics.skipped_entries.is_empty());
        assert_eq!(reloaded.entries.len(), 7);
    }

    #[test]
    fn crash_trailing_newlines_loads_cleanly() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("trailing_nl.jsonl");

        let mut content = build_crash_test_session_file(2);
        content.push_str("\n\n\n");
        std::fs::write(&file_path, &content).unwrap();

        let (session, diagnostics) = run_async(async {
            Session::open_with_diagnostics(file_path.to_string_lossy().as_ref()).await
        })
        .unwrap();

        assert_eq!(session.entries.len(), 2);
        assert!(diagnostics.skipped_entries.is_empty());
    }

    #[test]
    fn crash_noop_save_after_reload_is_idempotent() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("hello"));
        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();
        let content_before = std::fs::read_to_string(&path).unwrap();

        let mut loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
        run_async(async { loaded.save().await }).unwrap();

        let content_after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content_before, content_after);
    }

    #[test]
    fn crash_corrupt_then_continue_operation() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("msg A"));
        session.append_message(make_test_message("msg B"));
        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        // Corrupt last entry.
        let content = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<String> = content.lines().map(String::from).collect();
        *lines.last_mut().unwrap() = "BROKEN_JSON".to_string();
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();

        let (mut recovered, diagnostics) = run_async(async {
            Session::open_with_diagnostics(path.to_string_lossy().as_ref()).await
        })
        .unwrap();
        assert_eq!(diagnostics.skipped_entries.len(), 1);
        assert_eq!(recovered.entries.len(), 1);

        // Continue: add and save.
        recovered.path = Some(path.clone());
        recovered.session_dir = Some(temp_dir.path().to_path_buf());
        recovered.append_message(make_test_message("msg C"));
        run_async(async { recovered.save().await }).unwrap();

        let reloaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
        assert_eq!(reloaded.entries.len(), 2, "A and C present after recovery");
    }

    #[test]
    fn crash_defensive_rewrite_when_persisted_exceeds_entries() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("msg A"));
        run_async(async { session.save().await }).unwrap();

        session.persisted_entry_count.store(999, Ordering::SeqCst);
        assert!(session.should_full_rewrite());

        session.append_message(make_test_message("msg B"));
        run_async(async { session.save().await }).unwrap();
        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 2);
        assert_eq!(session.appends_since_checkpoint, 0);
    }

    #[test]
    fn crash_persisted_count_unchanged_on_append_failure() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("msg A"));
        run_async(async { session.save().await }).unwrap();
        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 1);

        let path = session.path.clone().unwrap();
        session.append_message(make_test_message("msg B"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
            if std::fs::OpenOptions::new().append(true).open(&path).is_ok() {
                // Some environments (for example root-run test runners) bypass chmod restrictions.
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
                return;
            }
        }
        #[cfg(not(unix))]
        {
            return;
        }

        let result = run_async(async { session.save().await });

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        assert!(result.is_err());
        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 1);

        run_async(async { session.save().await }).unwrap();
        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn crash_missing_session_file_forces_full_rewrite_recovery() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("msg A"));
        run_async(async { session.save().await }).unwrap();

        let path = session.path.clone().unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(session.should_full_rewrite());

        session.append_message(make_test_message("msg B"));
        run_async(async { session.save().await }).unwrap();

        let reloaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
        assert_eq!(reloaded.entries.len(), 2);
        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 2);
        assert_eq!(session.appends_since_checkpoint, 0);
    }

    #[test]
    fn crash_queue_backpressure_at_limit() {
        let mut queue = AutosaveQueue::with_limit(3);

        queue.enqueue_mutation(AutosaveMutationKind::Message);
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        queue.enqueue_mutation(AutosaveMutationKind::Metadata);
        assert_eq!(queue.pending_mutations, 3);

        queue.enqueue_mutation(AutosaveMutationKind::Label);
        assert_eq!(queue.pending_mutations, 3, "capped");
        assert_eq!(queue.backpressure_events, 1);
    }

    #[test]
    fn crash_flush_failure_with_intervening_mutations() {
        let mut queue = AutosaveQueue::with_limit(8);

        for _ in 0..4 {
            queue.enqueue_mutation(AutosaveMutationKind::Message);
        }
        let ticket = queue.begin_flush(AutosaveFlushTrigger::Periodic).unwrap();

        queue.enqueue_mutation(AutosaveMutationKind::Message);
        queue.enqueue_mutation(AutosaveMutationKind::Metadata);
        assert_eq!(queue.pending_mutations, 2);

        // restore_budget = 8 - 2 = 6, restored = min(4, 6) = 4
        queue.finish_flush(ticket, false);
        assert_eq!(queue.pending_mutations, 6);
        assert_eq!(queue.flush_failed, 1);
    }

    #[test]
    fn crash_queue_metrics_snapshot() {
        let mut queue = AutosaveQueue::with_limit(5);
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        queue.enqueue_mutation(AutosaveMutationKind::Metadata);
        queue.enqueue_mutation(AutosaveMutationKind::Label);

        let metrics = queue.metrics();
        assert_eq!(metrics.pending_mutations, 3);
        assert_eq!(metrics.max_pending_mutations, 5);
        assert_eq!(metrics.coalesced_mutations, 2);
        assert_eq!(metrics.flush_started, 0);
        assert!(metrics.last_flush_duration_ms.is_none());
    }

    #[test]
    fn crash_double_flush_is_noop() {
        let mut queue = AutosaveQueue::with_limit(10);
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        let ticket = queue.begin_flush(AutosaveFlushTrigger::Periodic).unwrap();
        queue.finish_flush(ticket, true);

        assert!(queue.begin_flush(AutosaveFlushTrigger::Manual).is_none());
    }

    #[test]
    fn crash_finish_worker_result_propagates_panic_before_cancellation() {
        let handle = thread::spawn(|| -> () {
            test_fail!("jsonl worker panic");
        });

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<()> =
                finish_worker_result::<(), _>(handle, Err(()), "Save task cancelled");
        }));

        assert!(
            panic.is_err(),
            "worker panic should not be masked as cancellation"
        );
    }

    #[test]
    fn crash_finish_worker_result_maps_nonpanic_cancellation_to_session_error() {
        let handle = thread::spawn(|| {});

        let err = finish_worker_result::<(), _>(handle, Err(()), "Save task cancelled")
            .expect_err("error");

        assert!(
            err.to_string().contains("Save task cancelled"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn crash_finish_worker_result_returns_success_payload() {
        let handle = thread::spawn(|| {});

        let value =
            finish_worker_result::<usize, ()>(handle, Ok(Ok(7usize)), "task cancelled").unwrap();

        assert_eq!(value, 7);
    }

    #[test]
    fn crash_entries_survive_failed_full_rewrite() {
        // Entries are cloned during full rewrite to avoid losing them if the async future drops.
        // On error, the session must still contain the entries in memory.
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("msg A"));
        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        session.set_model_header(Some("new-provider".to_string()), None, None);
        session.append_message(make_test_message("msg B"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let parent = path.parent().unwrap();
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o555)).unwrap();
            if tempfile::NamedTempFile::new_in(parent).is_ok() {
                // Some environments (for example root-run test runners) bypass chmod restrictions.
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
                return;
            }
        }
        #[cfg(not(unix))]
        {
            return;
        }

        let result = run_async(async { session.save().await });
        assert!(result.is_err());

        assert_eq!(session.entries.len(), 2, "entries restored");
        assert_eq!(session.entry_index.len(), 2);
        assert!(session.header_dirty);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let parent = path.parent().unwrap();
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        run_async(async { session.save().await }).unwrap();
        assert!(!session.header_dirty);
    }

    #[test]
    fn crash_metrics_accumulate_across_failure_recovery() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("msg A"));
        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        let m = session.autosave_metrics();
        assert_eq!(m.flush_succeeded, 1);
        assert_eq!(m.flush_failed, 0);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
            if std::fs::OpenOptions::new().append(true).open(&path).is_ok() {
                // Some environments (for example root-run test runners) bypass chmod restrictions.
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
                return;
            }
        }
        #[cfg(not(unix))]
        {
            return;
        }

        session.append_message(make_test_message("msg B"));
        let _ = run_async(async { session.save().await });

        let m = session.autosave_metrics();
        assert_eq!(m.flush_failed, 1);
        assert!(m.pending_mutations > 0);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        run_async(async { session.save().await }).unwrap();

        let m = session.autosave_metrics();
        assert_eq!(m.flush_succeeded, 2);
        assert_eq!(m.flush_failed, 1);
        assert_eq!(m.pending_mutations, 0);
        assert_eq!(m.flush_started, 3);
    }

    #[test]
    fn crash_many_sequential_appends_accumulate() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("initial"));
        run_async(async { session.save().await }).unwrap();

        for i in 0..10 {
            session.append_message(make_test_message(&format!("append-{i}")));
            run_async(async { session.save().await }).unwrap();
        }

        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 11);
        assert_eq!(session.appends_since_checkpoint, 10);

        let path = session.path.clone().unwrap();
        let line_count = std::fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(line_count, 12, "1 header + 11 entries");

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
        assert_eq!(loaded.entries.len(), 11);
    }

    #[test]
    fn crash_load_unsaved_entry_absent() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("saved A"));
        session.append_message(make_test_message("saved B"));
        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        session.append_message(make_test_message("unsaved C"));
        assert_eq!(session.entries.len(), 3);

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
        assert_eq!(loaded.entries.len(), 2, "unsaved entry absent");
    }

    #[test]
    fn test_clone_has_independent_persisted_entry_count() {
        let session = Session::create();
        // Set initial count
        session.persisted_entry_count.store(10, Ordering::SeqCst);

        // Clone the session
        let clone = session.clone();

        // Verify clone sees initial value
        assert_eq!(clone.persisted_entry_count.load(Ordering::SeqCst), 10);

        // Update original
        session.persisted_entry_count.store(20, Ordering::SeqCst);

        // Verify clone is UNCHANGED (independent atomic)
        assert_eq!(clone.persisted_entry_count.load(Ordering::SeqCst), 10);

        // Update clone
        clone.persisted_entry_count.store(30, Ordering::SeqCst);

        // Verify original is UNCHANGED
        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 20);
    }

    #[test]
    fn crash_append_retry_after_transient_failure() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("msg A"));
        run_async(async { session.save().await }).unwrap();
        let path = session.path.clone().unwrap();

        session.append_message(make_test_message("msg B"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
            if std::fs::OpenOptions::new().append(true).open(&path).is_ok() {
                // Some environments (for example root-run test runners) bypass chmod restrictions.
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
                return;
            }
        }
        #[cfg(not(unix))]
        {
            return;
        }

        let result = run_async(async { session.save().await });
        assert!(result.is_err());
        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 1);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        run_async(async { session.save().await }).unwrap();
        assert_eq!(session.persisted_entry_count.load(Ordering::SeqCst), 2);

        let loaded =
            run_async(async { Session::open(path.to_string_lossy().as_ref()).await }).unwrap();
        assert_eq!(loaded.entries.len(), 2);
    }

    #[test]
    fn crash_durability_mode_parsing() {
        assert_eq!(
            AutosaveDurabilityMode::parse("strict"),
            Some(AutosaveDurabilityMode::Strict)
        );
        assert_eq!(
            AutosaveDurabilityMode::parse("BALANCED"),
            Some(AutosaveDurabilityMode::Balanced)
        );
        assert_eq!(
            AutosaveDurabilityMode::parse("  Throughput  "),
            Some(AutosaveDurabilityMode::Throughput)
        );
        assert_eq!(AutosaveDurabilityMode::parse("invalid"), None);
        assert_eq!(AutosaveDurabilityMode::parse(""), None);
    }

    #[test]
    fn crash_durability_resolve_precedence() {
        assert_eq!(
            resolve_autosave_durability_mode(Some("strict"), Some("balanced"), Some("throughput")),
            AutosaveDurabilityMode::Strict
        );
        assert_eq!(
            resolve_autosave_durability_mode(None, Some("throughput"), Some("strict")),
            AutosaveDurabilityMode::Throughput
        );
        assert_eq!(
            resolve_autosave_durability_mode(None, None, Some("strict")),
            AutosaveDurabilityMode::Strict
        );
        assert_eq!(
            resolve_autosave_durability_mode(None, None, None),
            AutosaveDurabilityMode::Balanced
        );
    }

    // =========================================================================
    // bd-3ar8v.2.9: Comprehensive autosave queue and durability state machine
    // =========================================================================

    // --- Queue boundary: minimum capacity (limit=1) ---

    #[test]
    fn autosave_queue_limit_one_accepts_single_mutation() {
        let mut queue = AutosaveQueue::with_limit(1);
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        assert_eq!(queue.pending_mutations, 1);
        assert_eq!(queue.coalesced_mutations, 0);
        assert_eq!(queue.backpressure_events, 0);
    }

    #[test]
    fn autosave_queue_limit_one_backpressures_second_mutation() {
        let mut queue = AutosaveQueue::with_limit(1);
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        queue.enqueue_mutation(AutosaveMutationKind::Metadata);
        assert_eq!(queue.pending_mutations, 1, "capped at 1");
        assert_eq!(queue.backpressure_events, 1);
        assert_eq!(queue.coalesced_mutations, 1);
    }

    #[test]
    fn autosave_queue_limit_one_flush_and_refill() {
        let mut queue = AutosaveQueue::with_limit(1);
        queue.enqueue_mutation(AutosaveMutationKind::Message);

        let ticket = queue.begin_flush(AutosaveFlushTrigger::Manual).unwrap();
        assert_eq!(queue.pending_mutations, 0);
        assert_eq!(ticket.batch_size, 1);
        queue.finish_flush(ticket, true);

        // Refill works after flush.
        queue.enqueue_mutation(AutosaveMutationKind::Metadata);
        assert_eq!(queue.pending_mutations, 1);
        assert_eq!(queue.flush_succeeded, 1);
    }

    // --- Queue boundary: with_limit enforces minimum of 1 ---

    #[test]
    fn autosave_queue_with_limit_zero_clamps_to_one() {
        let queue = AutosaveQueue::with_limit(0);
        assert_eq!(queue.max_pending_mutations, 1);
    }

    // --- Empty queue operations ---

    #[test]
    fn autosave_queue_begin_flush_on_empty_returns_none() {
        let mut queue = AutosaveQueue::with_limit(10);
        assert!(queue.begin_flush(AutosaveFlushTrigger::Manual).is_none());
        assert_eq!(queue.flush_started, 0, "no flush attempt recorded");
    }

    #[test]
    fn autosave_queue_metrics_on_fresh_queue() {
        let queue = AutosaveQueue::with_limit(256);
        let m = queue.metrics();
        assert_eq!(m.pending_mutations, 0);
        assert_eq!(m.max_pending_mutations, 256);
        assert_eq!(m.coalesced_mutations, 0);
        assert_eq!(m.backpressure_events, 0);
        assert_eq!(m.flush_started, 0);
        assert_eq!(m.flush_succeeded, 0);
        assert_eq!(m.flush_failed, 0);
        assert_eq!(m.last_flush_batch_size, 0);
        assert!(m.last_flush_duration_ms.is_none());
        assert!(m.last_flush_trigger.is_none());
    }

    // --- All three mutation kinds ---

    #[test]
    fn autosave_queue_all_mutation_kinds() {
        let mut queue = AutosaveQueue::with_limit(10);
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        queue.enqueue_mutation(AutosaveMutationKind::Metadata);
        queue.enqueue_mutation(AutosaveMutationKind::Label);
        assert_eq!(queue.pending_mutations, 3);
        // First mutation has no coalescing; subsequent two do.
        assert_eq!(queue.coalesced_mutations, 2);
    }

    // --- Multiple consecutive flushes with mixed outcomes ---

    #[test]
    fn autosave_queue_consecutive_success_flushes() {
        let mut queue = AutosaveQueue::with_limit(5);

        for round in 1..=3_u64 {
            queue.enqueue_mutation(AutosaveMutationKind::Message);
            queue.enqueue_mutation(AutosaveMutationKind::Metadata);
            let ticket = queue.begin_flush(AutosaveFlushTrigger::Periodic).unwrap();
            queue.finish_flush(ticket, true);
            assert_eq!(queue.pending_mutations, 0);
            assert_eq!(queue.flush_succeeded, round);
            assert_eq!(queue.flush_started, round);
            assert_eq!(queue.last_flush_batch_size, 2);
        }
        assert_eq!(queue.flush_failed, 0);
    }

    #[test]
    fn autosave_queue_alternating_success_failure() {
        let mut queue = AutosaveQueue::with_limit(10);

        // Round 1: success
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        let t1 = queue.begin_flush(AutosaveFlushTrigger::Periodic).unwrap();
        queue.finish_flush(t1, true);
        assert_eq!(queue.flush_succeeded, 1);
        assert_eq!(queue.flush_failed, 0);
        assert_eq!(queue.pending_mutations, 0);

        // Round 2: failure (mutations restored)
        queue.enqueue_mutation(AutosaveMutationKind::Metadata);
        queue.enqueue_mutation(AutosaveMutationKind::Label);
        let t2 = queue.begin_flush(AutosaveFlushTrigger::Manual).unwrap();
        queue.finish_flush(t2, false);
        assert_eq!(queue.flush_succeeded, 1);
        assert_eq!(queue.flush_failed, 1);
        assert_eq!(queue.pending_mutations, 2, "restored from failure");

        // Round 3: success (clears the restored mutations)
        let t3 = queue.begin_flush(AutosaveFlushTrigger::Shutdown).unwrap();
        assert_eq!(t3.batch_size, 2);
        queue.finish_flush(t3, true);
        assert_eq!(queue.flush_succeeded, 2);
        assert_eq!(queue.flush_failed, 1);
        assert_eq!(queue.pending_mutations, 0);
        assert_eq!(queue.flush_started, 3);
    }

    // --- Failure when queue is completely full (zero capacity to restore) ---

    #[test]
    fn autosave_queue_failure_drops_all_when_full() {
        let mut queue = AutosaveQueue::with_limit(3);

        // Fill to capacity and flush.
        for _ in 0..3 {
            queue.enqueue_mutation(AutosaveMutationKind::Message);
        }
        let ticket = queue.begin_flush(AutosaveFlushTrigger::Periodic).unwrap();
        assert_eq!(ticket.batch_size, 3);
        assert_eq!(queue.pending_mutations, 0);

        // Fill queue completely while flush is in flight.
        for _ in 0..3 {
            queue.enqueue_mutation(AutosaveMutationKind::Metadata);
        }
        assert_eq!(queue.pending_mutations, 3);

        // Flush fails — no capacity to restore, all 3 batch mutations are dropped.
        let bp_before = queue.backpressure_events;
        queue.finish_flush(ticket, false);
        assert_eq!(queue.pending_mutations, 3, "capped at max");
        assert_eq!(queue.flush_failed, 1);
        assert_eq!(
            queue.backpressure_events,
            bp_before + 3,
            "dropped mutations counted as backpressure"
        );
    }

    // --- Flush trigger tracking ---

    #[test]
    fn autosave_queue_tracks_trigger_across_flushes() {
        let mut queue = AutosaveQueue::with_limit(10);

        // Manual trigger.
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        let t1 = queue.begin_flush(AutosaveFlushTrigger::Manual).unwrap();
        assert_eq!(t1.trigger, AutosaveFlushTrigger::Manual);
        queue.finish_flush(t1, true);
        assert_eq!(
            queue.metrics().last_flush_trigger,
            Some(AutosaveFlushTrigger::Manual)
        );

        // Periodic trigger.
        queue.enqueue_mutation(AutosaveMutationKind::Metadata);
        let t2 = queue.begin_flush(AutosaveFlushTrigger::Periodic).unwrap();
        queue.finish_flush(t2, true);
        assert_eq!(
            queue.metrics().last_flush_trigger,
            Some(AutosaveFlushTrigger::Periodic)
        );

        // Shutdown trigger.
        queue.enqueue_mutation(AutosaveMutationKind::Label);
        let t3 = queue.begin_flush(AutosaveFlushTrigger::Shutdown).unwrap();
        queue.finish_flush(t3, true);
        assert_eq!(
            queue.metrics().last_flush_trigger,
            Some(AutosaveFlushTrigger::Shutdown)
        );
    }

    // --- Flush records duration ---

    #[test]
    fn autosave_queue_flush_records_duration() {
        let mut queue = AutosaveQueue::with_limit(10);
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        let ticket = queue.begin_flush(AutosaveFlushTrigger::Manual).unwrap();
        queue.finish_flush(ticket, true);
        // Duration should be recorded (>= 0ms).
        assert!(queue.metrics().last_flush_duration_ms.is_some());
    }

    // --- Rapid enqueue-flush cycles ---

    #[test]
    fn autosave_queue_rapid_single_mutation_flushes() {
        let mut queue = AutosaveQueue::with_limit(10);
        let rounds = 20;

        for _ in 0..rounds {
            queue.enqueue_mutation(AutosaveMutationKind::Message);
            let ticket = queue.begin_flush(AutosaveFlushTrigger::Periodic).unwrap();
            queue.finish_flush(ticket, true);
        }

        let m = queue.metrics();
        assert_eq!(m.flush_started, rounds);
        assert_eq!(m.flush_succeeded, rounds);
        assert_eq!(m.flush_failed, 0);
        assert_eq!(m.pending_mutations, 0);
        assert_eq!(m.last_flush_batch_size, 1);
    }

    // --- Saturating counter behavior under heavy load ---

    #[test]
    fn autosave_queue_many_backpressure_events_accumulate() {
        let mut queue = AutosaveQueue::with_limit(1);
        let excess: u64 = 100;

        // First enqueue goes into the queue; rest are backpressure.
        for _ in 0..=excess {
            queue.enqueue_mutation(AutosaveMutationKind::Message);
        }
        assert_eq!(queue.pending_mutations, 1);
        assert_eq!(queue.backpressure_events, excess);
    }

    // --- Durability mode: as_str roundtrip ---

    #[test]
    fn autosave_durability_mode_as_str_roundtrip() {
        for mode in [
            AutosaveDurabilityMode::Strict,
            AutosaveDurabilityMode::Balanced,
            AutosaveDurabilityMode::Throughput,
        ] {
            let s = mode.as_str();
            let parsed = AutosaveDurabilityMode::parse(s);
            assert_eq!(parsed, Some(mode), "roundtrip failed for {s}");
        }
    }

    // --- Durability mode: should_flush/best_effort truth table ---

    #[test]
    fn autosave_durability_mode_shutdown_behavior_truth_table() {
        assert!(AutosaveDurabilityMode::Strict.should_flush_on_shutdown());
        assert!(!AutosaveDurabilityMode::Strict.best_effort_on_shutdown());

        assert!(AutosaveDurabilityMode::Balanced.should_flush_on_shutdown());
        assert!(AutosaveDurabilityMode::Balanced.best_effort_on_shutdown());

        assert!(!AutosaveDurabilityMode::Throughput.should_flush_on_shutdown());
        assert!(!AutosaveDurabilityMode::Throughput.best_effort_on_shutdown());
    }

    // --- Durability mode: case-insensitive parsing ---

    #[test]
    fn autosave_durability_mode_parse_case_insensitive() {
        assert_eq!(
            AutosaveDurabilityMode::parse("STRICT"),
            Some(AutosaveDurabilityMode::Strict)
        );
        assert_eq!(
            AutosaveDurabilityMode::parse("Balanced"),
            Some(AutosaveDurabilityMode::Balanced)
        );
        assert_eq!(
            AutosaveDurabilityMode::parse("tHrOuGhPuT"),
            Some(AutosaveDurabilityMode::Throughput)
        );
    }

    // --- Durability mode: whitespace trimming ---

    #[test]
    fn autosave_durability_mode_parse_trims_whitespace() {
        assert_eq!(
            AutosaveDurabilityMode::parse("  strict  "),
            Some(AutosaveDurabilityMode::Strict)
        );
        assert_eq!(
            AutosaveDurabilityMode::parse("\tbalanced\n"),
            Some(AutosaveDurabilityMode::Balanced)
        );
    }

    // --- Session-level: save on empty queue is no-op ---

    #[test]
    fn autosave_session_save_on_empty_queue_is_noop() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        // Save without any mutations — should succeed and not change metrics.
        let m_before = session.autosave_metrics();
        run_async(async { session.flush_autosave(AutosaveFlushTrigger::Manual).await }).unwrap();
        let m_after = session.autosave_metrics();

        assert_eq!(m_before.flush_started, m_after.flush_started);
        assert_eq!(m_after.pending_mutations, 0);
    }

    // --- Session-level: mode change mid-session ---

    #[test]
    fn autosave_session_mode_change_mid_session() {
        let mut session = Session::create();
        assert_eq!(
            session.autosave_durability_mode(),
            AutosaveDurabilityMode::Balanced,
            "default is balanced"
        );

        session.set_autosave_durability_mode(AutosaveDurabilityMode::Strict);
        assert_eq!(
            session.autosave_durability_mode(),
            AutosaveDurabilityMode::Strict
        );

        session.set_autosave_durability_mode(AutosaveDurabilityMode::Throughput);
        assert_eq!(
            session.autosave_durability_mode(),
            AutosaveDurabilityMode::Throughput
        );
    }

    // --- Session-level: all mutation types enqueue correctly ---

    #[test]
    fn autosave_session_all_mutation_types_enqueue() {
        let mut session = Session::create();

        let first_entry_id = session.append_message(make_test_message("msg"));
        assert_eq!(session.autosave_metrics().pending_mutations, 1);

        session.append_model_change("prov".to_string(), "model".to_string());
        assert_eq!(session.autosave_metrics().pending_mutations, 2);

        session.append_thinking_level_change("high".to_string());
        assert_eq!(session.autosave_metrics().pending_mutations, 3);

        session.append_session_info(Some("test-session".to_string()));
        assert_eq!(session.autosave_metrics().pending_mutations, 4);

        session.append_custom_entry("custom".to_string(), None);
        assert_eq!(session.autosave_metrics().pending_mutations, 5);

        // Label mutation (needs existing entry to target).
        session.add_label(&first_entry_id, Some("test-label".to_string()));
        assert_eq!(session.autosave_metrics().pending_mutations, 6);
    }

    // --- Session-level: flush then verify metrics ---

    #[test]
    fn autosave_session_manual_save_resets_pending() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("a"));
        session.append_message(make_test_message("b"));
        session.append_message(make_test_message("c"));
        assert_eq!(session.autosave_metrics().pending_mutations, 3);

        run_async(async { session.save().await }).unwrap();

        let m = session.autosave_metrics();
        assert_eq!(m.pending_mutations, 0);
        assert_eq!(m.flush_succeeded, 1);
        assert_eq!(m.last_flush_batch_size, 3);
        assert_eq!(m.last_flush_trigger, Some(AutosaveFlushTrigger::Manual));
    }

    // --- Session-level: periodic flush trigger tracking ---

    #[test]
    fn autosave_session_periodic_flush_tracks_trigger() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        session.append_message(make_test_message("periodic msg"));
        run_async(async { session.flush_autosave(AutosaveFlushTrigger::Periodic).await }).unwrap();

        let m = session.autosave_metrics();
        assert_eq!(m.last_flush_trigger, Some(AutosaveFlushTrigger::Periodic));
        assert_eq!(m.flush_succeeded, 1);
    }

    // --- Session-level: shutdown flush with balanced mode success ---

    #[test]
    fn autosave_session_balanced_shutdown_succeeds_on_valid_path() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());
        session.set_autosave_durability_for_test(AutosaveDurabilityMode::Balanced);

        session.append_message(make_test_message("balanced ok"));
        run_async(async { session.flush_autosave_on_shutdown().await }).unwrap();

        let m = session.autosave_metrics();
        assert_eq!(m.flush_succeeded, 1);
        assert_eq!(m.pending_mutations, 0);
        assert_eq!(m.last_flush_trigger, Some(AutosaveFlushTrigger::Shutdown));
    }

    // --- Queue: partial restoration on failure with various fill levels ---

    #[test]
    fn autosave_queue_failure_partial_restoration() {
        let mut queue = AutosaveQueue::with_limit(5);

        // Fill to 4 and flush (batch=4).
        for _ in 0..4 {
            queue.enqueue_mutation(AutosaveMutationKind::Message);
        }
        let ticket = queue.begin_flush(AutosaveFlushTrigger::Periodic).unwrap();
        assert_eq!(ticket.batch_size, 4);

        // Add 2 while flush is in flight.
        queue.enqueue_mutation(AutosaveMutationKind::Metadata);
        queue.enqueue_mutation(AutosaveMutationKind::Label);
        assert_eq!(queue.pending_mutations, 2);

        // Fail: available_capacity = 5 - 2 = 3, restored = min(4,3) = 3, dropped = 1.
        let bp_before = queue.backpressure_events;
        let coal_before = queue.coalesced_mutations;
        queue.finish_flush(ticket, false);
        assert_eq!(queue.pending_mutations, 5, "2 new + 3 restored = 5");
        assert_eq!(queue.backpressure_events, bp_before + 1, "1 dropped");
        assert_eq!(
            queue.coalesced_mutations,
            coal_before + 1,
            "1 dropped coalesced"
        );
    }

    // --- Queue: success flush does not restore ---

    #[test]
    fn autosave_queue_success_does_not_restore_pending() {
        let mut queue = AutosaveQueue::with_limit(10);

        queue.enqueue_mutation(AutosaveMutationKind::Message);
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        let ticket = queue.begin_flush(AutosaveFlushTrigger::Manual).unwrap();

        // Add 1 mutation while flush is in flight.
        queue.enqueue_mutation(AutosaveMutationKind::Label);
        assert_eq!(queue.pending_mutations, 1);

        // Success: only the in-flight mutation remains.
        queue.finish_flush(ticket, true);
        assert_eq!(queue.pending_mutations, 1, "only new mutation remains");
        assert_eq!(queue.flush_succeeded, 1);
    }

    // --- Queue: large batch size tracking ---

    #[test]
    fn autosave_queue_large_batch_tracking() {
        let mut queue = AutosaveQueue::with_limit(500);

        for _ in 0..200 {
            queue.enqueue_mutation(AutosaveMutationKind::Message);
        }

        let ticket = queue.begin_flush(AutosaveFlushTrigger::Periodic).unwrap();
        assert_eq!(ticket.batch_size, 200);
        queue.finish_flush(ticket, true);

        let m = queue.metrics();
        assert_eq!(m.last_flush_batch_size, 200);
        assert_eq!(m.flush_succeeded, 1);
        assert_eq!(m.pending_mutations, 0);
    }

    // --- Durability resolve: all invalid falls through to default ---

    #[test]
    fn autosave_resolve_all_invalid_returns_balanced() {
        assert_eq!(
            resolve_autosave_durability_mode(Some("bad"), Some("worse"), Some("nope")),
            AutosaveDurabilityMode::Balanced
        );
    }

    // --- Session-level: metrics accumulate across many save/flush cycles ---

    #[test]
    fn autosave_session_metrics_accumulate_over_many_cycles() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());

        let cycles: u64 = 10;
        for i in 0..cycles {
            session.append_message(make_test_message(&format!("cycle-{i}")));
            run_async(async { session.save().await }).unwrap();
        }

        let m = session.autosave_metrics();
        assert_eq!(m.flush_started, cycles);
        assert_eq!(m.flush_succeeded, cycles);
        assert_eq!(m.flush_failed, 0);
        assert_eq!(m.pending_mutations, 0);
        assert_eq!(m.last_flush_batch_size, 1);
    }

    // --- Queue: coalesced count is cumulative (not per-flush) ---

    #[test]
    fn autosave_queue_coalesced_is_cumulative() {
        let mut queue = AutosaveQueue::with_limit(10);

        // Batch 1: 3 mutations => 2 coalesced.
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        assert_eq!(queue.coalesced_mutations, 2);

        let t1 = queue.begin_flush(AutosaveFlushTrigger::Manual).unwrap();
        queue.finish_flush(t1, true);

        // Batch 2: 2 mutations => 1 more coalesced (total 3).
        queue.enqueue_mutation(AutosaveMutationKind::Label);
        queue.enqueue_mutation(AutosaveMutationKind::Label);
        assert_eq!(queue.coalesced_mutations, 3);
    }

    // --- Session-level: autosave_queue_limit changes batch size behavior ---

    #[test]
    fn autosave_session_respects_queue_limit() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());
        session.set_autosave_queue_limit_for_test(3);

        for i in 0..10 {
            session.append_message(make_test_message(&format!("lim-{i}")));
        }

        let m = session.autosave_metrics();
        assert_eq!(m.pending_mutations, 3);
        assert_eq!(m.max_pending_mutations, 3);
        assert_eq!(m.backpressure_events, 7);

        // Flush should only capture 3 (the capped count).
        run_async(async { session.save().await }).unwrap();
        let m = session.autosave_metrics();
        assert_eq!(m.last_flush_batch_size, 3);
        assert_eq!(m.pending_mutations, 0);
    }

    // --- Session-level: throughput mode shutdown with successful prior manual save ---

    #[test]
    fn autosave_session_throughput_shutdown_skips_after_manual_save() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mut session = Session::create();
        session.session_dir = Some(temp_dir.path().to_path_buf());
        session.set_autosave_durability_for_test(AutosaveDurabilityMode::Throughput);

        session.append_message(make_test_message("saved"));
        run_async(async { session.save().await }).unwrap();
        assert_eq!(session.autosave_metrics().flush_succeeded, 1);

        // Add more mutations but don't save.
        session.append_message(make_test_message("unsaved"));
        assert_eq!(session.autosave_metrics().pending_mutations, 1);

        // Shutdown skips flush in throughput mode.
        run_async(async { session.flush_autosave_on_shutdown().await }).unwrap();
        assert_eq!(
            session.autosave_metrics().pending_mutations,
            1,
            "unsaved mutation remains"
        );
        assert_eq!(
            session.autosave_metrics().flush_succeeded,
            1,
            "no new flush"
        );
    }

    // --- Queue: begin_flush atomically clears pending ---

    #[test]
    fn autosave_queue_begin_flush_is_atomic_clear() {
        let mut queue = AutosaveQueue::with_limit(10);

        queue.enqueue_mutation(AutosaveMutationKind::Message);
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        queue.enqueue_mutation(AutosaveMutationKind::Message);
        assert_eq!(queue.pending_mutations, 3);

        let ticket = queue.begin_flush(AutosaveFlushTrigger::Periodic).unwrap();

        // Pending is immediately 0, even before finish_flush.
        assert_eq!(queue.pending_mutations, 0);
        assert_eq!(ticket.batch_size, 3);

        // New mutations start fresh.
        queue.enqueue_mutation(AutosaveMutationKind::Label);
        assert_eq!(queue.pending_mutations, 1);

        queue.finish_flush(ticket, true);
        assert_eq!(queue.pending_mutations, 1, "new mutation preserved");
    }

    // --- Queue: multiple failures accumulate flush_failed ---

    #[test]
    fn autosave_queue_multiple_failures_accumulate() {
        let mut queue = AutosaveQueue::with_limit(10);

        // Each round: enqueue 1 new + restored from prior failure.
        // Round 1: enqueue → pending=1, flush fails → restore 1 → pending=1
        // Round 2: enqueue → pending=2, flush fails → restore 2 → pending=2
        // Round N: pending grows by 1 each round because failures restore.
        for round in 1..=5_u64 {
            queue.enqueue_mutation(AutosaveMutationKind::Message);
            #[allow(clippy::cast_possible_truncation)]
            let expected_batch = round as usize;
            let ticket = queue.begin_flush(AutosaveFlushTrigger::Periodic).unwrap();
            assert_eq!(ticket.batch_size, expected_batch);
            queue.finish_flush(ticket, false);
            assert_eq!(queue.flush_failed, round);
            assert_eq!(queue.pending_mutations, expected_batch, "restored batch");
        }
        assert_eq!(queue.flush_succeeded, 0);
        assert_eq!(queue.flush_started, 5);
    }

    // --- ExportSnapshot and non-blocking export ---

    #[test]
    fn export_snapshot_captures_header_and_entries() {
        let mut session = Session::create();
        session.append_message(make_test_message("hello world"));
        session.append_message(make_test_message("second message"));

        let snapshot = session.export_snapshot();
        assert_eq!(snapshot.header.id, session.header.id);
        assert_eq!(snapshot.header.timestamp, session.header.timestamp);
        assert_eq!(snapshot.header.cwd, session.header.cwd);
        assert_eq!(snapshot.entries.len(), session.entries.len());
        assert_eq!(snapshot.path, session.path);
    }

    #[test]
    fn export_snapshot_does_not_include_internal_caches() {
        let mut session = Session::create();
        for i in 0..10 {
            session.append_message(make_test_message(&format!("msg {i}")));
        }
        // The snapshot should be lighter than a full Session clone because
        // it skips autosave_queue, entry_index, entry_ids, and other caches.
        let snapshot = session.export_snapshot();
        assert_eq!(snapshot.entries.len(), 10);
        // Verify the snapshot is a distinct copy (not sharing references).
        assert_eq!(snapshot.header.id, session.header.id);
    }

    #[test]
    fn export_snapshot_html_matches_session_html() {
        let mut session = Session::create();
        session.append_message(make_test_message("hello"));
        session.append_message(make_test_message("world"));

        let session_html = session.to_html();
        let snapshot_html = session.export_snapshot().to_html();
        assert_eq!(session_html, snapshot_html);
    }

    #[test]
    fn export_snapshot_empty_session() {
        let session = Session::create();
        let snapshot = session.export_snapshot();
        assert!(snapshot.entries.is_empty());
        let html = snapshot.to_html();
        assert!(html.contains("Pi Session"));
        assert!(html.contains("</html>"));
    }

    #[test]
    fn render_session_html_contains_header_info() {
        let mut session = Session::create();
        session.header.id = "test-session-id-xyz".to_string();
        session.header.cwd = "/test/cwd/path".to_string();

        let html = render_session_html(&session.header, &session.entries);
        assert!(html.contains("test-session-id-xyz"));
        assert!(html.contains("/test/cwd/path"));
    }

    #[test]
    fn render_session_html_renders_all_entry_types() {
        let mut session = Session::create();

        // Message entry.
        session.append_message(make_test_message("user text here"));

        // Model change entry.
        session.append_model_change("anthropic".to_string(), "claude-sonnet-4-5".to_string());

        // Thinking level change entry.
        session.entries.push(SessionEntry::ThinkingLevelChange(
            ThinkingLevelChangeEntry {
                base: EntryBase::new(None, "tlc1".to_string()),
                thinking_level: "high".to_string(),
            },
        ));

        let html = render_session_html(&session.header, &session.entries);
        assert!(html.contains("user text here"));
        assert!(html.contains("anthropic"));
        assert!(html.contains("claude-sonnet-4-5"));
        assert!(html.contains("high"));
    }

    #[test]
    fn export_snapshot_with_path() {
        let mut session = Session::create();
        session.path = Some(PathBuf::from("/tmp/my-session.jsonl"));
        session.append_message(make_test_message("msg"));

        let snapshot = session.export_snapshot();
        assert_eq!(
            snapshot.path.as_deref(),
            Some(Path::new("/tmp/my-session.jsonl"))
        );
    }

    #[test]
    fn fork_plan_snapshot_consistency() {
        let mut session = Session::create();
        let msg1 = make_test_message("first message");
        session.append_message(msg1);
        let msg1_id = session.entries[0].base_id().unwrap().clone();

        let msg2 = make_test_message("second message");
        session.append_message(msg2);
        let msg2_id = session.entries[1].base_id().unwrap().clone();

        // Plan fork from the second message.
        let plan = session.plan_fork_from_user_message(&msg2_id).unwrap();

        // Fork plan entries should include the path up to the parent.
        assert_eq!(plan.leaf_id, Some(msg1_id));
        // The plan captures a snapshot of entries — modifying session shouldn't affect plan.
        let plan_entry_count = plan.entries.len();
        session.append_message(make_test_message("third message"));
        assert_eq!(plan.entries.len(), plan_entry_count);
    }
}
