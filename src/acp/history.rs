//! ACP session recovery and transcript replay. Loading never runs an agent turn.
//!
//! Session IDs are catalog keys, not paths. Disk recovery is restricted to the
//! explicitly configured session directory and the requested workspace. Replay
//! walks the selected branch, not the compacted provider context or sibling
//! branches, and does not grant authority to historic tool calls.

use super::{
    AcpOptions, AcpPermissionClient, AcpSessionsMap, AgentCx,
    INTERNAL_ERROR, INVALID_PARAMS, PROMPT_IN_PROGRESS, SESSION_NOT_FOUND,
    build_acp_session, classify_tool_kind, config_options_for, content,
    json_rpc_notification,
};
use crate::model::{ContentBlock, Message, UserContent};
use crate::session::{Session, SessionEntry, session_message_to_model};
use crate::session_index::{SessionIndex, SessionMeta};
use asupersync::channel::oneshot;
use asupersync::sync::{Mutex, OwnedMutexGuard};
use asupersync::time::{sleep, timeout, wall_now};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::Duration;

#[derive(Debug)]
pub(super) struct HistoryError {
    pub(super) code: i64,
    pub(super) message: String,
}

type HistoryResult<T> = std::result::Result<T, HistoryError>;

impl HistoryError {
    fn invalid(message: impl Into<String>) -> Self {
        Self { code: INVALID_PARAMS, message: message.into() }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self { code: INTERNAL_ERROR, message: message.into() }
    }

    fn missing() -> Self {
        Self {
            code: SESSION_NOT_FOUND,
            message: "Session not found in the requested workspace and configured session directory".into(),
        }
    }

    fn busy() -> Self {
        Self {
            code: PROMPT_IN_PROGRESS,
            message: "Cannot load or resume a session while a prompt is in progress".into(),
        }
    }
}

pub(super) fn requested_session_id(params: &Value) -> HistoryResult<&str> {
    let id = params.get("sessionId").and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty() && id.len() <= 1024)
        .ok_or_else(|| HistoryError::invalid("Missing or invalid sessionId"))?;
    // Do not interpret even a path-looking ID as a filename. The catalog
    // lookup below uses exact equality, not a prefix or substring match.
    Ok(id)
}

fn requested_cwd(params: &Value) -> HistoryResult<PathBuf> {
    let raw = params.get("cwd").and_then(Value::as_str)
        .ok_or_else(|| HistoryError::invalid("Missing required parameter: cwd"))?;
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(HistoryError::invalid("cwd must be an absolute directory path"));
    }
    let canonical = path.canonicalize()
        .map_err(|_| HistoryError::invalid("cwd must name an accessible directory"))?;
    if !canonical.is_dir() {
        return Err(HistoryError::invalid("cwd must name a directory"));
    }
    Ok(canonical)
}

fn validate_mcp_servers(params: &Value) -> HistoryResult<()> {
    match params.get("mcpServers") {
        None => Ok(()),
        Some(Value::Array(servers)) if servers.is_empty() => Ok(()),
        Some(Value::Array(_)) => Err(HistoryError::invalid(
            "ACP client-supplied MCP servers are not supported; no servers were started",
        )),
        Some(_) => Err(HistoryError::invalid("mcpServers must be an array")),
    }
}

fn same_workspace(stored: &Path, requested: &Path) -> bool {
    stored.canonicalize().is_ok_and(|path| path == requested)
}

/// Refresh the derived catalog on a worker, not the ACP runtime thread. Scan
/// the configured root rather than just encode_cwd(canonical_cwd): sessions
/// saved through a workspace symlink can live under a differently named folder.
/// Unlike the terminal picker, keep distinct stores with the same ID visible
/// here so recovery never silently chooses between divergent transcripts.
async fn saved_catalog(root: &Path) -> HistoryResult<Vec<SessionMeta>> {
    let root = root.to_path_buf();
    let (tx, mut rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("acp-session-catalog".into())
        .spawn(move || {
            let index = SessionIndex::for_sessions_root(&root);
            let result = index.reindex_all().and_then(|()| index.list_sessions(None));
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), result);
        })
        .map_err(|error| HistoryError::internal(format!("Cannot start session discovery: {error}")))?;
    let cx = AgentCx::for_current_or_request();
    rx.recv(cx.cx()).await
        .map_err(|_| HistoryError::internal("Session discovery was interrupted"))?
        .map_err(|error| HistoryError::internal(format!("Cannot discover saved sessions: {error}")))
}

/// Reopen the original backing store, never a new session or a copied transcript.
async fn open_saved_session(
    id: &str,
    cwd: &Path,
    root: Option<&Path>,
) -> HistoryResult<Session> {
    let root = root.ok_or_else(HistoryError::missing)?;
    let root = root.canonicalize().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            HistoryError::missing()
        } else {
            HistoryError::internal(format!("Cannot access configured session directory: {error}"))
        }
    })?;
    let candidates = saved_catalog(&root).await?;
    let mut matching_paths = HashSet::new();
    for candidate in candidates.iter().filter(|entry| {
        entry.id == id && same_workspace(Path::new(&entry.cwd), cwd)
    }) {
        let path = Path::new(&candidate.path).canonicalize()
            .map_err(|error| HistoryError::internal(format!("Cannot resolve saved session: {error}")))?;
        if !path.starts_with(&root) || !path.is_file() {
            return Err(HistoryError::invalid("Saved session is outside the configured session directory"));
        }
        matching_paths.insert(path);
    }
    if matching_paths.len() > 1 {
        return Err(HistoryError::invalid("Ambiguous sessionId: multiple saved stores have this ID"));
    }
    let path = matching_paths.into_iter().next().ok_or_else(HistoryError::missing)?;
    let path_text = path.to_str()
        .ok_or_else(|| HistoryError::internal("Saved session path is not UTF-8"))?;
    let session = Session::open(path_text).await
        .map_err(|error| HistoryError::internal(format!("Cannot reopen saved session: {error}")))?;
    // Metadata can be stale. The decoded store, not its cached catalog row,
    // owns the identity and workspace that will be installed in the live map.
    if session.header.id != id || !same_workspace(Path::new(&session.header.cwd), cwd) {
        return Err(HistoryError::invalid("Saved session identity or workspace does not match the request"));
    }
    Ok(session)
}

/// Resolve live or saved state and return the same configuration surface as a
/// new session. The dispatcher serializes management requests and rejects an
/// active prompt before entering this function, including a not-yet-polled one.
/// A load replays before its caller sends the response; resume deliberately does
/// not replay, so clients that retained their transcript do not duplicate it.
pub(super) async fn load(
    params: &Value,
    options: &AcpOptions,
    permission_client: &AcpPermissionClient,
    sessions: &AcpSessionsMap,
    cx: &AgentCx,
    out: &SyncSender<String>,
    replay: bool,
) -> HistoryResult<Value> {
    let id = requested_session_id(params)?;
    let cwd = requested_cwd(params)?;
    validate_mcp_servers(params)?;
    let live = {
        let guard = sessions.lock(cx).await
            .map_err(|_| HistoryError::internal("Session registry is unavailable"))?;
        guard.get(id).cloned()
    };
    let state = if let Some(state) = live {
        state
    } else {
        let saved = open_saved_session(id, &cwd, options.session_dir.as_deref()).await?;
        let (loaded_id, state) = build_acp_session(saved, true, cwd.clone(), options, Some(permission_client))
            .map_err(|error| HistoryError::internal(format!("Cannot restore agent session: {error}")))?;
        if loaded_id != id {
            return Err(HistoryError::internal("Restored session identity changed"));
        }
        let state = Arc::new(Mutex::new(state));
        let mut guard = sessions.lock(cx).await
            .map_err(|_| HistoryError::internal("Session registry is unavailable"))?;
        guard.insert(id.to_string(), Arc::clone(&state));
        state
    };

    let guard = OwnedMutexGuard::lock(state, cx).await
        .map_err(|_| HistoryError::internal("Session state is unavailable"))?;
    if !same_workspace(&guard.cwd, &cwd) {
        return Err(HistoryError::invalid("Session belongs to a different workspace"));
    }
    let agent = guard.agent_session.as_ref().ok_or_else(HistoryError::busy)?;
    let configuration = config_options_for(&guard, &options.available_models)
        .ok_or_else(HistoryError::busy)?;
    if replay {
        let session = OwnedMutexGuard::lock(Arc::clone(&agent.session), cx).await
            .map_err(|_| HistoryError::internal("Session history is unavailable"))?;
        replay_session(&session, id, out).await?;
    }
    Ok(json!({ "sessionId": id, "configOptions": configuration }))
}

/// Bounded backpressure without blocking an async worker on SyncSender::send.
/// The stdout writer owns I/O. A disconnected or stalled client never receives
/// a successful load response claiming that an incomplete replay was complete.
pub(super) async fn send_line(out: &SyncSender<String>, mut line: String) -> HistoryResult<()> {
    let send = async {
        loop {
            match out.try_send(line) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => {
                    return Err(HistoryError::internal("ACP client disconnected during history delivery"));
                }
                Err(TrySendError::Full(unsent)) => line = unsent,
            }
            sleep(wall_now(), Duration::from_millis(5)).await;
        }
    };
    timeout(wall_now(), Duration::from_secs(30), Box::pin(send)).await
        .map_err(|_| HistoryError::internal("ACP client stalled during history delivery"))?
}

async fn send_update(out: &SyncSender<String>, id: &str, update: Value) -> HistoryResult<()> {
    send_line(out, json_rpc_notification(
        "session/update", json!({ "sessionId": id, "update": update }),
    )).await
}

/// Use durable entries instead of provider context: compaction must not erase
/// earlier turns from the editor's history. A rewind remains an explicit marker
/// in this transcript; replay does not re-execute or restore its old effects.
async fn replay_session(session: &Session, id: &str, out: &SyncSender<String>) -> HistoryResult<()> {
    for entry in session.entries_for_current_path() {
        match entry {
            SessionEntry::Message(entry) => {
                if let Some(message) = session_message_to_model(&entry.message) {
                    replay_message(&message, id, out).await?;
                }
            }
            SessionEntry::Compaction(entry) => {
                replay_text("agent_message_chunk", &format!("[Context compacted]\n{}", entry.summary), id, out).await?;
            }
            SessionEntry::BranchSummary(entry) => {
                replay_text("agent_message_chunk", &format!("[Branch summary]\n{}", entry.summary), id, out).await?;
            }
            SessionEntry::Custom(entry) if entry.custom_type == "rewind" => {
                let summary = entry.data.as_ref().and_then(|data| data.get("summary"))
                    .and_then(Value::as_str).unwrap_or("");
                replay_text("agent_message_chunk", &format!("[Conversation rewound]\n{summary}"), id, out).await?;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn replay_text(kind: &str, text: &str, id: &str, out: &SyncSender<String>) -> HistoryResult<()> {
    send_update(out, id, json!({ "sessionUpdate": kind, "content": { "type": "text", "text": text } })).await
}

async fn replay_user_content(content: &UserContent, kind: &str, id: &str, out: &SyncSender<String>) -> HistoryResult<()> {
    match content {
        UserContent::Text(text) => replay_text(kind, text, id, out).await,
        UserContent::Blocks(blocks) => replay_blocks(blocks, kind, id, out).await,
    }
}

async fn replay_blocks(blocks: &[ContentBlock], kind: &str, id: &str, out: &SyncSender<String>) -> HistoryResult<()> {
    for block in blocks {
        // Use exactly the same media conversion as live tool results. Strip
        // only the tool-specific envelope, not the content or its ordering.
        for mut envelope in content::tool_result_content(std::slice::from_ref(block)) {
            let display = envelope.get_mut("content").map(Value::take)
                .ok_or_else(|| HistoryError::internal("Invalid ACP display content envelope"))?;
            send_update(out, id, json!({ "sessionUpdate": kind, "content": display })).await?;
        }
    }
    Ok(())
}

async fn replay_message(message: &Message, id: &str, out: &SyncSender<String>) -> HistoryResult<()> {
    match message {
        Message::User(message) => replay_user_content(&message.content, "user_message_chunk", id, out).await?,
        Message::Assistant(message) => {
            for block in &message.content {
                match block {
                    ContentBlock::ToolCall(call) => {
                        send_update(out, id, json!({
                            "sessionUpdate": "tool_call", "toolCallId": call.id,
                            "title": call.name, "kind": classify_tool_kind(&call.name),
                            "status": "pending", "rawInput": call.arguments,
                        })).await?;
                    }
                    ContentBlock::Thinking(thinking) => {
                        replay_text("agent_thought_chunk", &thinking.thinking, id, out).await?;
                    }
                    _ => replay_blocks(std::slice::from_ref(block), "agent_message_chunk", id, out).await?,
                }
            }
        }
        Message::ToolResult(message) => {
            send_update(out, id, json!({
                "sessionUpdate": "tool_call_update", "toolCallId": message.tool_call_id,
                "title": message.tool_name, "kind": classify_tool_kind(&message.tool_name),
                "status": if message.is_error { "failed" } else { "completed" },
                "content": content::tool_result_content(&message.content),
            })).await?;
        }
        Message::Custom(message) if message.display => {
            replay_user_content(&message.content, "agent_message_chunk", id, out).await?;
        }
        Message::Custom(_) => {}
    }
    Ok(())
}

/// The live catalog is always usable, including non-persisted sessions.
/// Disk discovery and pagination are layered onto this same management path.
pub(super) async fn list(
    params: &Value,
    _options: &AcpOptions,
    sessions: &AcpSessionsMap,
    cx: &AgentCx,
) -> HistoryResult<Value> {
    let filter = if params.get("cwd").is_some() { Some(requested_cwd(params)?) } else { None };
    let entries = {
        let guard = sessions.lock(cx).await
            .map_err(|_| HistoryError::internal("Session registry is unavailable"))?;
        guard.iter().map(|(id, state)| (id.clone(), Arc::clone(state))).collect::<Vec<_>>()
    };
    let mut rows = Vec::new();
    for (id, state) in entries {
        let state = state.lock(cx).await
            .map_err(|_| HistoryError::internal("Session state is unavailable"))?;
        if filter.as_deref().is_none_or(|cwd| same_workspace(&state.cwd, cwd)) {
            rows.push(json!({ "sessionId": id, "cwd": state.cwd }));
        }
    }
    rows.sort_by(|left, right| left["sessionId"].as_str().cmp(&right["sessionId"].as_str()));
    Ok(json!({ "sessions": rows }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthCredential, AuthStorage};
    use crate::model::{AssistantMessage, StopReason, TextContent, Usage, UserMessage};
    use crate::session::SessionStoreKind;
    use asupersync::runtime::RuntimeBuilder;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::Mutex as StdMutex;

    fn user(text: &str) -> Message {
        Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp: 1 })
    }

    fn assistant(text: &str) -> Message {
        Message::assistant(AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            api: "anthropic-messages".into(), provider: "anthropic".into(),
            model: "claude-sonnet-4-5".into(), usage: Usage::default(),
            stop_reason: StopReason::Stop, stop_details: None,
            error_message: None, timestamp: 2,
        })
    }

    fn fixture(root: &Path, cwd: &Path) -> Session {
        let mut session = Session::create_with_dir_and_store(Some(root.into()), SessionStoreKind::Jsonl);
        session.header.cwd = cwd.display().to_string();
        session.set_model_header(Some("anthropic".into()), Some("claude-sonnet-4-5".into()), Some("off".into()));
        session.append_model_message(user("Remember this conversation"));
        session.append_model_message(assistant("Preserved answer"));
        session
    }

    #[test]
    fn saved_session_reopens_original_store_without_mutating_it() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let cwd = project.path().canonicalize().unwrap();
            let mut original = fixture(root.path(), &cwd);
            original.save().await.unwrap();
            let path = original.path.clone().unwrap();
            let before = std::fs::read(&path).unwrap();
            let restored = open_saved_session(&original.header.id, &cwd, Some(root.path())).await.unwrap();
            assert_eq!(restored.header.id, original.header.id);
            assert_eq!(restored.path.as_ref().unwrap().canonicalize().unwrap(), path.canonicalize().unwrap());
            assert_eq!(restored.to_messages_for_current_path().len(), 2);
            assert_eq!(restored.effective_thinking_level_for_current_path().as_deref(), Some("off"));
            assert_eq!(std::fs::read(&path).unwrap(), before);
        });
    }

    #[test]
    fn session_ids_cannot_open_arbitrary_paths_or_other_workspaces() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            let cwd = project.path().canonicalize().unwrap();
            let mut session = fixture(root.path(), &cwd);
            session.save().await.unwrap();
            for id in ["../outside.jsonl", session.path.as_ref().unwrap().to_str().unwrap()] {
                assert_eq!(open_saved_session(id, &cwd, Some(root.path())).await.unwrap_err().code, SESSION_NOT_FOUND);
            }
            assert_eq!(open_saved_session(&session.header.id, &other.path().canonicalize().unwrap(), Some(root.path())).await.unwrap_err().code, SESSION_NOT_FOUND);
            assert_eq!(open_saved_session(&session.header.id, &cwd, None).await.unwrap_err().code, SESSION_NOT_FOUND);
        });
    }

    #[test]
    fn load_validates_workspace_and_does_not_accept_unimplemented_mcp_servers() {
        assert_eq!(requested_cwd(&json!({})).unwrap_err().code, INVALID_PARAMS);
        assert_eq!(requested_cwd(&json!({ "cwd": "relative" })).unwrap_err().code, INVALID_PARAMS);
        assert!(validate_mcp_servers(&json!({ "mcpServers": [] })).is_ok());
        assert!(validate_mcp_servers(&json!({ "mcpServers": [{ "command": "must-not-run" }] })).is_err());
        assert!(validate_mcp_servers(&json!({ "mcpServers": null })).is_err());
        assert!(requested_session_id(&json!({ "sessionId": " " })).is_err());
    }

    #[test]
    fn duplicate_ids_do_not_silently_choose_a_different_transcript() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let cwd = project.path().canonicalize().unwrap();
            let mut session = fixture(root.path(), &cwd);
            session.save().await.unwrap();
            let path = session.path.as_ref().unwrap();
            let original = std::fs::read(path).unwrap();
            let duplicate = path.with_file_name("divergent-copy.jsonl");
            std::fs::write(&duplicate, &original).unwrap();
            let error = open_saved_session(&session.header.id, &cwd, Some(root.path()))
                .await.unwrap_err();
            assert_eq!(error.code, INVALID_PARAMS);
            assert!(error.message.contains("Ambiguous"));
            assert_eq!(std::fs::read(path).unwrap(), original);
            assert_eq!(std::fs::read(duplicate).unwrap(), original);
        });
    }

    #[cfg(unix)]
    #[test]
    fn recovery_finds_sessions_saved_through_a_workspace_alias() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let aliases = tempfile::tempdir().unwrap();
            let cwd = project.path().canonicalize().unwrap();
            let alias = aliases.path().join("workspace");
            std::os::unix::fs::symlink(&cwd, &alias).unwrap();
            let mut saved = fixture(root.path(), &alias);
            saved.save().await.unwrap();
            let restored = open_saved_session(&saved.header.id, &cwd, Some(root.path()))
                .await.unwrap();
            assert_eq!(restored.header.id, saved.header.id);
            assert_eq!(restored.to_messages_for_current_path().len(), 2);
        });
    }

    #[test]
    fn replay_preserves_selected_branch_and_earlier_compacted_turns() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let cwd = project.path().canonicalize().unwrap();
            let mut session = fixture(root.path(), &cwd);
            let branch_point = session.leaf_id.clone().unwrap();
            session.append_model_message(user("abandoned sibling"));
            session.append_model_message(assistant("must not replay sibling"));
            assert!(session.navigate_to(&branch_point));
            let kept = session.append_model_message(user("selected branch"));
            let tip = session.append_model_message(assistant("selected answer"));
            session.save().await.unwrap();
            let path = session.path.clone().unwrap();
            let mut raw = std::fs::read_to_string(&path).unwrap();
            raw.push_str(&json!({
                "type": "compaction", "id": "compaction-fixture", "parentId": tip,
                "timestamp": "2026-01-01T00:00:00Z", "summary": "provider context summary",
                "firstKeptEntryId": kept, "tokensBefore": 1000,
            }).to_string());
            raw.push('\n');
            std::fs::write(&path, &raw).unwrap();
            let restored = Session::open(path.to_str().unwrap()).await.unwrap();
            let (tx, rx) = std::sync::mpsc::sync_channel(32);
            replay_session(&restored, &restored.header.id, &tx).await.unwrap();
            let lines = rx.try_iter().collect::<Vec<_>>();
            assert_eq!(lines.len(), 5);
            let text = lines.join("\n");
            assert!(text.contains("Remember this conversation"));
            assert!(text.contains("Preserved answer"));
            assert!(text.contains("selected branch"));
            assert!(text.contains("provider context summary"));
            assert!(!text.contains("abandoned sibling"));
            assert!(!text.contains("must not replay sibling"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        });
    }

    #[test]
    fn replay_renders_tool_calls_and_results_without_executing_them() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let (tx, rx) = std::sync::mpsc::sync_channel(16);
            let call: Message = serde_json::from_value(json!({
                "role": "assistant", "api": "test", "provider": "test", "model": "test",
                "usage": Usage::default(), "stopReason": "toolUse", "timestamp": 1,
                "content": [
                    { "type": "text", "text": "historic call", "textSignature": "private-signature" },
                    { "type": "toolCall", "id": "call-1", "name": "bash", "arguments": { "command": "do-not-execute" } },
                    { "type": "redacted_thinking", "data": "private-redacted-state" }
                ],
            })).unwrap();
            let result: Message = serde_json::from_value(json!({
                "role": "toolResult", "toolCallId": "call-1", "toolName": "bash",
                "content": [{ "type": "image", "data": "cG5n", "mimeType": "image/png" }],
                "details": { "private": "not-for-display" }, "isError": true, "timestamp": 2,
            })).unwrap();
            replay_message(&call, "session", &tx).await.unwrap();
            replay_message(&result, "session", &tx).await.unwrap();
            let lines = rx.try_iter().collect::<Vec<_>>();
            let updates = lines.iter().map(|line| serde_json::from_str::<Value>(line).unwrap()["params"]["update"].clone()).collect::<Vec<_>>();
            assert_eq!(updates.len(), 3);
            assert_eq!(updates[0]["sessionUpdate"], "agent_message_chunk");
            assert_eq!(updates[1]["sessionUpdate"], "tool_call");
            assert_eq!(updates[2]["toolCallId"], "call-1");
            assert_eq!(updates[2]["status"], "failed");
            assert_eq!(updates[2]["content"][0]["content"]["type"], "image");
            let serialized = lines.join("\n");
            for private in ["private-signature", "private-redacted-state", "not-for-display"] {
                assert!(!serialized.contains(private));
            }
        });
    }

    #[test]
    fn replay_hides_non_display_custom_messages() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let (tx, rx) = std::sync::mpsc::sync_channel(4);
            for display in [false, true] {
                let message = Message::Custom(crate::model::CustomMessage {
                    custom_type: "extension-state".into(),
                    content: UserContent::Text(if display { "visible" } else { "private" }.into()),
                    display, details: None, timestamp: 1,
                });
                replay_message(&message, "session", &tx).await.unwrap();
            }
            let lines = rx.try_iter().collect::<Vec<_>>();
            assert_eq!(lines.len(), 1);
            assert!(lines[0].contains("visible"));
            assert!(!lines[0].contains("private"));
        });
    }

    #[test]
    fn history_delivery_yields_on_backpressure_and_fails_on_disconnect() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            tx.try_send("earlier".to_string()).unwrap();
            let reader = async {
                sleep(wall_now(), Duration::from_millis(10)).await;
                assert_eq!(rx.try_recv().unwrap(), "earlier");
            };
            let (result, ()) = futures::join!(send_line(&tx, "later".to_string()), reader);
            result.unwrap();
            assert_eq!(rx.try_recv().unwrap(), "later");
            drop(rx);
            assert!(send_line(&tx, "not-delivered".into()).await.is_err());
        });
    }

    #[test]
    fn load_rehydrates_and_replays_but_resume_reuses_state_without_replay() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let runtime_handle = runtime.handle();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let cwd = project.path().canonicalize().unwrap();
            let mut saved = fixture(root.path(), &cwd);
            saved.save().await.unwrap();
            let path = saved.path.clone().unwrap();
            let before = std::fs::read(&path).unwrap();
            let mut auth = AuthStorage::load(root.path().join("auth.json")).unwrap();
            auth.set("anthropic", AuthCredential::ApiKey { key: "not-a-live-key".into() });
            let registry = crate::models::ModelRegistry::load(&auth, None);
            let entry = registry.find("anthropic", "claude-sonnet-4-5").unwrap();
            let options = AcpOptions {
                config: crate::config::Config::default(), available_models: vec![entry],
                model_registry: registry, auth, runtime_handle,
                session_dir: Some(root.path().into()), skills_prompt: None,
            };
            let sessions = Arc::new(Mutex::new(HashMap::new()));
            let cx = AgentCx::for_testing();
            let (tx, rx) = std::sync::mpsc::sync_channel(32);
            let client = AcpPermissionClient {
                out_tx: tx.clone(), pending: Arc::new(StdMutex::new(HashMap::new())),
                request_counter: Arc::new(AtomicU64::new(0)),
                timeout: Duration::from_secs(1), cx: cx.clone(),
            };
            let params = json!({ "sessionId": saved.header.id, "cwd": cwd, "mcpServers": [] });
            let result = load(&params, &options, &client, &sessions, &cx, &tx, true).await.unwrap();
            assert_eq!(result["configOptions"][0]["currentValue"], "anthropic/claude-sonnet-4-5");
            assert_eq!(result["configOptions"][1]["currentValue"], "off");
            // The dispatcher sends its response only after load returns.
            send_line(&tx, super::super::json_rpc_ok(json!(7), result)).await.unwrap();
            let lines = rx.try_iter().map(|line| serde_json::from_str::<Value>(&line).unwrap()).collect::<Vec<_>>();
            assert_eq!(lines.len(), 3);
            assert_eq!(lines[0]["params"]["update"]["sessionUpdate"], "user_message_chunk");
            assert_eq!(lines[1]["params"]["update"]["sessionUpdate"], "agent_message_chunk");
            assert_eq!(lines[2]["id"], 7);
            let first = sessions.lock(&cx).await.unwrap().get(&saved.header.id).unwrap().clone();
            load(&params, &options, &client, &sessions, &cx, &tx, false).await.unwrap();
            assert!(rx.try_recv().is_err());
            let second = sessions.lock(&cx).await.unwrap().get(&saved.header.id).unwrap().clone();
            assert!(Arc::ptr_eq(&first, &second));
            let mut state = first.lock(&cx).await.unwrap();
            let retained = state.agent_session.take().unwrap();
            drop(state);
            assert_eq!(load(&params, &options, &client, &sessions, &cx, &tx, true).await.unwrap_err().code, PROMPT_IN_PROGRESS);
            first.lock(&cx).await.unwrap().agent_session = Some(retained);
            assert_eq!(std::fs::read(&path).unwrap(), before);
        });
    }
}
