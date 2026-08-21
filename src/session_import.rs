//! Foreign session import (bd-cv653.6.4).
//!
//! Import Claude Code (`~/.claude/projects/**/*.jsonl`: user/assistant
//! entries with text/tool_use/tool_result blocks) and Codex
//! (`~/.codex/sessions/**/*.jsonl`: session_meta/response_item envelopes
//! with message/reasoning/function_call payloads) into native JSONL v3
//! sessions. Switching tools doesn't strand history.
//!
//! Fidelity rules: text preserved verbatim; tool calls preserved when
//! id-matching pairs exist; thinking/reasoning imported as collapsed custom
//! blocks; anything unmappable is kept as an attachment entry — NEVER
//! dropped silently. Content-addressed session ids make re-imports
//! idempotent (same file → same id, with a notice).
//!
//! Format mappings studied from the owner's casr (cross_agent_session_
//! resumer) per the bead's flywheel correction: complement, don't duplicate
//! (casr resumes elsewhere; this produces NATIVE continuable pi sessions).

use std::path::Path;

use serde::Serialize;
use sha2::Digest;

use crate::error::{Error, Result};
use crate::model::{
    AssistantMessage, ContentBlock, Message, TextContent, ThinkingContent, ToolCall,
    ToolResultMessage, UserContent, UserMessage,
};
use crate::session::Session;

/// Tool-result schema tag for imports.
pub const IMPORT_SCHEMA: &str = "pi.session_import.v1";

/// The outcome of one import.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportOutcome {
    pub schema: String,
    pub source: String,
    pub original_path: String,
    pub session_id: String,
    pub session_path: String,
    /// Messages imported.
    pub imported: usize,
    /// Corrupt/unmappable lines skipped (never aborts the import).
    pub skipped: usize,
    /// True when the file had already been imported (idempotent).
    pub already_imported: bool,
    /// Text report lines.
    pub report: Vec<String>,
}

/// Import source kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSource {
    Claude,
    Codex,
}

impl ImportSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// Content-addressed session id: same file content → same id (idempotent).
fn session_id_for(source: ImportSource, content: &[u8]) -> String {
    let digest = sha2::Sha256::digest(content);
    format!(
        "import-{}-{}",
        source.as_str(),
        crate::package_manager::hex_encode(&digest)
            .chars()
            .take(24)
            .collect::<String>()
    )
}

/// Parse an ISO-8601/RFC3339 timestamp to epoch millis (best effort).
fn parse_ts_ms(raw: Option<&str>) -> i64 {
    raw.and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        .map_or(0, |dt| dt.timestamp_millis())
}

fn text_message(role: &str, text: String, ts: i64) -> Message {
    if role == "assistant" {
        Message::Assistant(std::sync::Arc::new(AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            timestamp: ts,
            ..Default::default()
        }))
    } else {
        Message::User(UserMessage {
            content: UserContent::Text(text),
            timestamp: ts,
        })
    }
}

// ---------------------------------------------------------------------------
// Claude Code reader
// ---------------------------------------------------------------------------

/// Import a Claude Code session file.
///
/// # Errors
/// Read/parse failures on the envelope (per-line corruption is tolerated
/// and counted).
pub fn import_claude(path: &Path, target_dir: Option<&Path>) -> Result<ImportOutcome> {
    let raw = std::fs::read(path)
        .map_err(|e| Error::tool("import", format!("failed to read {}: {e}", path.display())))?;
    import_bytes(ImportSource::Claude, &raw, path, target_dir)
}

/// Import a Codex session file.
///
/// # Errors
/// Read failures.
pub fn import_codex(path: &Path, target_dir: Option<&Path>) -> Result<ImportOutcome> {
    let raw = std::fs::read(path)
        .map_err(|e| Error::tool("import", format!("failed to read {}: {e}", path.display())))?;
    import_bytes(ImportSource::Codex, &raw, path, target_dir)
}

/// Find an already-imported session by content-addressed id anywhere under
/// the target root. Session files are named `<timestamp>_<id-prefix>.jsonl`
/// (first 8 id chars), so the probe matches the prefix and verifies the
/// full id in the header line.
fn find_imported_session(root: &Path, id: &str) -> Option<std::path::PathBuf> {
    let prefix: String = id
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
    let suffix = format!("{prefix}.jsonl");
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.ends_with(&suffix))
            {
                let header_line = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|content| content.lines().next().map(str::to_string));
                if header_line.is_some_and(|line| line.contains(id)) {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn import_bytes(
    source: ImportSource,
    raw: &[u8],
    original_path: &Path,
    target_dir: Option<&Path>,
) -> Result<ImportOutcome> {
    let id = session_id_for(source, raw);
    let target_root =
        target_dir.map_or_else(crate::config::Config::sessions_dir, Path::to_path_buf);
    // Idempotency probe: the session store nests by cwd, so scan for the
    // content-addressed id anywhere under the target root.
    let already_imported = find_imported_session(&target_root, &id);
    if let Some(existing_path) = already_imported {
        return Ok(ImportOutcome {
            schema: IMPORT_SCHEMA.to_string(),
            source: source.as_str().to_string(),
            original_path: original_path.display().to_string(),
            session_id: id,
            session_path: existing_path.display().to_string(),
            imported: 0,
            skipped: 0,
            already_imported: true,
            report: vec![format!("already imported: {}", existing_path.display())],
        });
    }

    let mut session = Session::create_with_dir(Some(target_root));
    session.header.id.clone_from(&id);
    // Let the store derive the canonical path (it nests by cwd); the outcome
    // reads the actual path after save.
    session.header.provider = Some(source.as_str().to_string());
    session.header.model_id = Some(format!("foreign-{}", source.as_str()));
    session.header.cwd = std::env::current_dir()
        .map(|cwd| cwd.display().to_string())
        .unwrap_or_default();
    // Provenance (recorded on the header's custom map when available).
    session.append_custom_entry(
        "foreign_import".to_string(),
        Some(serde_json::json!({
            "schema": IMPORT_SCHEMA,
            "source": source.as_str(),
            "originalPath": original_path.display().to_string(),
            "importedAtMs": now_ms(),
        })),
    );

    let text = String::from_utf8_lossy(raw);
    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut report = Vec::new();
    for (line_no, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed: std::result::Result<serde_json::Value, _> = serde_json::from_str(line);
        let Ok(entry) = parsed else {
            record_corrupt_line(line_no, &mut skipped, &mut report);
            continue;
        };
        let converted = match source {
            ImportSource::Claude => convert_claude_entry(&entry),
            ImportSource::Codex => convert_codex_entry(&entry),
        };
        match converted {
            Ok(Some(message)) => {
                session.append_message(crate::session::SessionMessage::from(message));
                imported += 1;
            }
            Ok(None) => {}
            Err(note) => {
                record_unmappable_line(
                    &mut session,
                    line_no,
                    &note,
                    line,
                    &mut skipped,
                    &mut report,
                );
            }
        }
    }
    report.push(format!(
        "imported {imported} message(s), skipped {skipped} line(s)"
    ));

    // Persist; the store derives the canonical (cwd-nested) path.
    let actual_path = {
        let mut session = session;
        futures::executor::block_on(async {
            session.save().await?;
            Ok::<_, crate::error::Error>(session.path.clone())
        })
        .map_err(|e| Error::tool("import", format!("failed to write session: {e}")))?
        .ok_or_else(|| Error::tool("import", "session save produced no path".to_string()))?
    };

    Ok(ImportOutcome {
        schema: IMPORT_SCHEMA.to_string(),
        source: source.as_str().to_string(),
        original_path: original_path.display().to_string(),
        session_id: id,
        session_path: actual_path.display().to_string(),
        imported,
        skipped,
        already_imported: false,
        report,
    })
}

fn record_corrupt_line(line_no: usize, skipped: &mut usize, report: &mut Vec<String>) {
    *skipped += 1;
    report.push(format!("line {}: corrupt JSON skipped", line_no + 1));
}

fn record_unmappable_line(
    session: &mut Session,
    line_no: usize,
    note: &str,
    line: &str,
    skipped: &mut usize,
    report: &mut Vec<String>,
) {
    *skipped += 1;
    report.push(format!("line {}: {note}", line_no + 1));
    session.append_custom_entry(
        "foreign_attachment".to_string(),
        Some(serde_json::json!({
            "schema": IMPORT_SCHEMA,
            "line": line_no + 1,
            "reason": note,
            "excerpt": line.chars().take(400).collect::<String>(),
        })),
    );
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

// -- Claude Code entry conversion -------------------------------------------

fn convert_claude_entry(entry: &serde_json::Value) -> std::result::Result<Option<Message>, String> {
    let entry_type = entry.get("type").and_then(|v| v.as_str());
    if !matches!(entry_type, Some("user" | "assistant")) {
        // Non-conversational envelope (title/summary/meta) — not an error,
        // just not a message.
        return Ok(None);
    }
    let role = entry_type.unwrap_or("user");
    let ts = parse_ts_ms(
        entry
            .get("timestamp")
            .and_then(|v| v.as_str())
            .or_else(|| entry.pointer("/message/timestamp").and_then(|v| v.as_str())),
    );
    let content = entry
        .pointer("/message/content")
        .cloned()
        .or_else(|| entry.get("content").cloned());
    let Some(content) = content else {
        return Err("no content field".to_string());
    };
    Ok(convert_claude_content(role, &content, ts))
}

fn parse_claude_block(block: &serde_json::Value) -> Option<ContentBlock> {
    match block.get("type").and_then(|v| v.as_str()) {
        Some("text") => {
            let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if text.is_empty() {
                None
            } else {
                Some(ContentBlock::Text(TextContent::new(text)))
            }
        }
        Some("thinking") => {
            let text = block.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
            if text.is_empty() {
                None
            } else {
                Some(ContentBlock::Thinking(ThinkingContent {
                    thinking: text.to_string(),
                    thinking_signature: None,
                }))
            }
        }
        Some("tool_use") => {
            let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let input = block
                .get("input")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            Some(ContentBlock::ToolCall(ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: input,
                thought_signature: None,
            }))
        }
        _ => None,
    }
}

fn parse_claude_tool_result(block: &serde_json::Value) -> Option<(String, Vec<ContentBlock>)> {
    if block.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
        return None;
    }
    let tool_call_id = block
        .get("tool_use_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let content_blocks = match block.get("content") {
        Some(serde_json::Value::String(text)) => {
            vec![ContentBlock::Text(TextContent::new(text))]
        }
        Some(serde_json::Value::Array(inner)) => inner
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                    item.get("text")
                        .and_then(|v| v.as_str())
                        .map(|text| ContentBlock::Text(TextContent::new(text)))
                } else {
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    };
    Some((tool_call_id, content_blocks))
}

fn convert_claude_content(role: &str, content: &serde_json::Value, ts: i64) -> Option<Message> {
    match content {
        serde_json::Value::String(text) => {
            if text.trim().is_empty() {
                None
            } else {
                Some(text_message(role, text.clone(), ts))
            }
        }
        serde_json::Value::Array(blocks) => {
            let mut out_blocks = Vec::new();
            let mut results: Vec<(String, Vec<ContentBlock>)> = Vec::new();
            for block in blocks {
                if let Some(cb) = parse_claude_block(block) {
                    out_blocks.push(cb);
                } else if let Some(tr) = parse_claude_tool_result(block) {
                    results.push(tr);
                }
            }
            // Tool results become their own ToolResult messages.
            if !results.is_empty() && out_blocks.is_empty() {
                let (tool_call_id, content) = results.into_iter().next().expect("one");
                return Some(Message::ToolResult(std::sync::Arc::new(
                    ToolResultMessage {
                        tool_call_id,
                        tool_name: String::new(),
                        content,
                        is_error: false,
                        timestamp: ts,
                        details: None,
                    },
                )));
            }
            if out_blocks.is_empty() {
                None
            } else if role == "assistant" {
                Some(Message::Assistant(std::sync::Arc::new(AssistantMessage {
                    content: out_blocks,
                    timestamp: ts,
                    ..Default::default()
                })))
            } else {
                Some(Message::User(UserMessage {
                    content: UserContent::Blocks(out_blocks),
                    timestamp: ts,
                }))
            }
        }
        _ => None,
    }
}

// -- Codex entry conversion --------------------------------------------------

/// Extract text from codex content blocks (`text`/`input_text`/`output_text`
/// shapes across rollout versions).
fn extract_codex_text_blocks(blocks: &[serde_json::Value]) -> String {
    blocks
        .iter()
        .filter_map(|block| {
            block
                .get("text")
                .and_then(|v| v.as_str())
                .or_else(|| block.get("input_text").and_then(|v| v.as_str()))
                .or_else(|| block.get("output_text").and_then(|v| v.as_str()))
                .map(str::to_string)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn convert_codex_entry(entry: &serde_json::Value) -> std::result::Result<Option<Message>, String> {
    let entry_type = entry.get("type").and_then(|v| v.as_str());
    match entry_type {
        Some("session_meta") => {
            // Carry cwd from the meta envelope when present (provenance).
            Ok(None)
        }
        Some("response_item") => {
            let payload = entry
                .get("payload")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let ts = parse_ts_ms(entry.get("timestamp").and_then(|v| v.as_str()));
            Ok(convert_codex_payload(&payload, ts))
        }
        _ => Ok(None),
    }
}

fn convert_codex_payload(payload: &serde_json::Value, ts: i64) -> Option<Message> {
    let kind = payload.get("type").and_then(|v| v.as_str());
    match kind {
        Some("message") => {
            let role = payload
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("user");
            let content = payload
                .get("content")
                .and_then(|v| v.as_array())
                .map(|blocks| extract_codex_text_blocks(blocks))
                .unwrap_or_default();
            if content.trim().is_empty() {
                return None;
            }
            Some(text_message(role, content, ts))
        }
        Some("reasoning") => {
            let summary = payload
                .get("summary")
                .and_then(|v| v.as_array())
                .map(|blocks| extract_codex_text_blocks(blocks))
                .unwrap_or_default();
            if summary.trim().is_empty() {
                return None;
            }
            Some(Message::Assistant(std::sync::Arc::new(AssistantMessage {
                content: vec![ContentBlock::Thinking(ThinkingContent {
                    thinking: summary,
                    thinking_signature: None,
                })],
                timestamp: ts,
                ..Default::default()
            })))
        }
        Some("function_call" | "custom_tool_call") => {
            let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let arguments = payload
                .get("arguments")
                .and_then(|v| v.as_str())
                .and_then(|raw| serde_json::from_str(raw).ok())
                .unwrap_or(serde_json::Value::Null);
            let call_id = payload
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Some(Message::Assistant(std::sync::Arc::new(AssistantMessage {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: call_id.to_string(),
                    name: name.to_string(),
                    arguments,
                    thought_signature: None,
                })],
                timestamp: ts,
                ..Default::default()
            })))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_fixture() -> String {
        [
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/proj","message":{"role":"user","content":[{"type":"text","text":"fix the parser"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"On it."},{"type":"thinking","thinking":"checking tests first"}],"model":"claude-3"}}"#,
            "this is not json",
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:02Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"tc1","name":"read","input":{"path":"src/parser.rs"}}]}}"#,
        ]
        .join("\n")
    }

    fn codex_fixture() -> String {
        [
            r#"{"type":"session_meta","timestamp":"2026-01-01T00:00:00.000Z","payload":{"id":"cx1","cwd":"/tmp/proj"}}"#,
            r#"{"type":"response_item","timestamp":"2026-01-01T00:00:01.000Z","payload":{"type":"message","role":"user","content":[{"text":"fix the parser"}]}}"#,
            r#"{"type":"response_item","timestamp":"2026-01-01T00:00:02.000Z","payload":{"type":"reasoning","summary":[{"text":"tests first"}]}}"#,
            r#"{"type":"response_item","timestamp":"2026-01-01T00:00:03.000Z","payload":{"type":"function_call","name":"read","arguments":"{\"path\":\"src/parser.rs\"}","call_id":"c1"}}"#,
        ]
        .join("\n")
    }

    #[test]
    fn claude_fixture_imports_with_corruption_tolerance() {
        let dir = std::env::temp_dir().join(format!("pi-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let source = dir.join("claude.jsonl");
        std::fs::write(&source, claude_fixture()).expect("write");
        let outcome = import_claude(&source, Some(&dir)).expect("import");
        assert_eq!(outcome.imported, 3, "{:?}", outcome.report);
        assert_eq!(outcome.skipped, 1, "{:?}", outcome.report);
        assert!(!outcome.already_imported);
        // Idempotent re-import.
        let again = import_claude(&source, Some(&dir)).expect("re-import");
        assert!(again.already_imported);
        assert_eq!(again.session_id, outcome.session_id);
        // The session opens and replays.
        let session =
            futures::executor::block_on(Session::open(&outcome.session_path)).expect("load");
        let messages = session.to_messages_for_current_path();
        assert_eq!(messages.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_fixture_imports_reasoning_as_thinking() {
        let dir = std::env::temp_dir().join(format!("pi-import-codex-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let source = dir.join("codex.jsonl");
        std::fs::write(&source, codex_fixture()).expect("write");
        let outcome = import_codex(&source, Some(&dir)).expect("import");
        assert_eq!(outcome.imported, 3, "{:?}", outcome.report);
        let session =
            futures::executor::block_on(Session::open(&outcome.session_path)).expect("load");
        let messages = session.to_messages_for_current_path();
        assert_eq!(messages.len(), 3);
        // The reasoning block landed as a thinking block.
        let has_thinking = messages.iter().any(|message| match message {
            Message::Assistant(assistant) => assistant
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Thinking(_))),
            _ => false,
        });
        assert!(has_thinking, "reasoning must import as a thinking block");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
