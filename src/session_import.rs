//! Foreign session import (bd-cv653.6.4).
//!
//! Claude Code and Codex logs become native, persisted Pi conversations. A
//! source envelope can produce multiple messages: in particular, every tool
//! result in a Claude batch and both Codex tool-output variants are imported.
//! Unsupported records and partial conversions retain their complete source
//! bytes as audit attachments, not a truncated excerpt or an active prompt.
//!
//! Imports are content-addressed and conversion-versioned. Re-importing with
//! this reader is idempotent, but an older lossy import cannot mask a repaired
//! conversion. The native session uses the current workspace and normal model
//! selection; foreign cwd/model metadata is historical, not a live setting.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::Digest;

use crate::error::{Error, Result};
use crate::session::Session;

mod conversion;

/// Result-envelope schema (the public fields remain unchanged).
pub const IMPORT_SCHEMA: &str = "pi.session_import.v1";
/// Conversion semantics, included in provenance and content-addressed ids.
const IMPORT_FORMAT_REVISION: u32 = 2;
const MAX_IMPORT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_HEADER_BYTES: u64 = 64 * 1024;

/// The outcome of one import.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportOutcome {
    pub schema: String,
    pub source: String,
    pub original_path: String,
    pub session_id: String,
    pub session_path: String,
    /// Native messages imported (one source record may produce several).
    pub imported: usize,
    /// Source lines requiring a preserved attachment. A partially converted
    /// line contributes here even when its supported messages were imported.
    pub skipped: usize,
    pub already_imported: bool,
    pub report: Vec<String>,
}

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

fn session_id_for(source: ImportSource, content: &[u8]) -> String {
    let mut digest = sha2::Sha256::new();
    digest.update(IMPORT_SCHEMA.as_bytes());
    digest.update(IMPORT_FORMAT_REVISION.to_be_bytes());
    digest.update(source.as_str().as_bytes());
    digest.update([0]);
    digest.update(content);
    let hex = crate::package_manager::hex_encode(&digest.finalize());
    // Session filenames use the FIRST eight id characters. A fixed
    // "import-c..." prefix made unrelated imports share a filename suffix.
    format!("{}-import-{}", &hex[..24], source.as_str())
}

fn read_source(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)
        .map_err(|error| Error::tool("import", format!("failed to open {}: {error}", path.display())))?;
    let metadata = file.metadata()
        .map_err(|error| Error::tool("import", format!("failed to stat {}: {error}", path.display())))?;
    if !metadata.is_file() {
        return Err(Error::tool("import", "source must be a regular file"));
    }
    if metadata.len() > MAX_IMPORT_BYTES {
        return Err(Error::tool("import", "source exceeds the 128 MiB import limit"));
    }
    let mut raw = Vec::new();
    file.take(MAX_IMPORT_BYTES + 1).read_to_end(&mut raw)
        .map_err(|error| Error::tool("import", format!("failed to read {}: {error}", path.display())))?;
    if raw.len() as u64 > MAX_IMPORT_BYTES {
        return Err(Error::tool("import", "source grew beyond the 128 MiB import limit"));
    }
    Ok(raw)
}

/// Import a Claude Code log, preserving batched tool exchanges and metadata.
///
/// # Errors
/// Fails on source I/O, admission limits, inaccessible destination scans, or
/// native persistence errors. Per-record corruption is retained and reported.
pub fn import_claude(path: &Path, target_dir: Option<&Path>) -> Result<ImportOutcome> {
    import_bytes(ImportSource::Claude, &read_source(path)?, path, target_dir)
}

/// Import a Codex rollout, including function/custom-tool calls and outputs.
///
/// # Errors
/// See [`import_claude`].
pub fn import_codex(path: &Path, target_dir: Option<&Path>) -> Result<ImportOutcome> {
    import_bytes(ImportSource::Codex, &read_source(path)?, path, target_dir)
}

/// Read only a bounded first line, and match the actual header id rather than
/// finding the id as a substring somewhere in a potentially huge transcript.
fn header_has_id(path: &Path, id: &str) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut header = Vec::new();
    if BufReader::new(file).take(MAX_HEADER_BYTES + 1).read_until(b'\n', &mut header).is_err()
        || header.len() as u64 > MAX_HEADER_BYTES
    {
        return false;
    }
    serde_json::from_slice::<Value>(&header).ok()
        .and_then(|header| header.get("id").and_then(Value::as_str).map(str::to_string))
        .is_some_and(|candidate| candidate == id)
}

fn find_imported_session(root: &Path, id: &str) -> Result<Option<PathBuf>> {
    let suffix = format!("{}.jsonl", &id[..8]);
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(Error::tool("import", format!(
                "cannot check existing imports in {}: {error}", dir.display()
            ))),
        };
        for entry in entries {
            let entry = entry.map_err(|error| Error::tool("import", format!("cannot inspect destination entry: {error}")))?;
            let kind = entry.file_type().map_err(|error| Error::tool("import", format!("cannot inspect destination type: {error}")))?;
            let path = entry.path();
            // Do not follow child symlinks out of the session root or into
            // cycles while looking for an import made in another cwd bucket.
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file()
                && path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.ends_with(&suffix))
                && header_has_id(&path, id)
            {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

fn import_bytes(
    source: ImportSource,
    raw: &[u8],
    original_path: &Path,
    target_dir: Option<&Path>,
) -> Result<ImportOutcome> {
    if raw.len() as u64 > MAX_IMPORT_BYTES {
        return Err(Error::tool("import", "source exceeds the 128 MiB import limit"));
    }
    let id = session_id_for(source, raw);
    let target_root = target_dir.map_or_else(crate::config::Config::sessions_dir, Path::to_path_buf);
    if let Some(existing_path) = find_imported_session(&target_root, &id)? {
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
    // "codex/foreign-codex" and "claude/foreign-claude" are not routable
    // native models. Leave model selection to the user's normal configuration.
    session.header.provider = None;
    session.header.model_id = None;
    session.header.cwd = std::env::current_dir().map(|cwd| cwd.display().to_string()).unwrap_or_default();
    session.append_custom_entry("foreign_import".to_string(), Some(json!({
        "schema": IMPORT_SCHEMA,
        "formatRevision": IMPORT_FORMAT_REVISION,
        "source": source.as_str(),
        "originalPath": original_path.display().to_string(),
        "sourceSha256": crate::package_manager::hex_encode(&sha2::Sha256::digest(raw)),
        "importedAtMs": now_ms(),
    })));

    let mut reader = conversion::ForeignReader::new(source);
    let mut imported = 0;
    let mut skipped = 0;
    let mut report = Vec::new();
    // Parse bytes per line. Lossy UTF-8 decoding would change corrupt input
    // before it could be archived, making the claimed preservation false.
    for (line_no, line) in raw.split(|byte| *byte == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let entry = match serde_json::from_slice::<Value>(line) {
            Ok(entry) => entry,
            Err(_) => {
                record_unmappable_line(&mut session, line_no, "corrupt JSON or UTF-8", line, &mut skipped, &mut report);
                continue;
            }
        };
        let converted = reader.convert(&entry);
        if !converted.notes.is_empty() {
            record_unmappable_line(&mut session, line_no, &converted.notes.join("; "), line, &mut skipped, &mut report);
        } else if converted.metadata {
            session.append_custom_entry("foreign_metadata".to_string(), Some(json!({
                "schema": IMPORT_SCHEMA, "line": line_no + 1, "entry": entry,
            })));
        }
        // Provider/model information is provenance, not a configuration change.
        if let Some(model) = entry.pointer("/message/model") {
            session.append_custom_entry("foreign_model".to_string(), Some(json!({
                "source": source.as_str(), "line": line_no + 1, "model": model,
                "usage": entry.pointer("/message/usage"),
            })));
        }
        for message in converted.messages {
            session.append_model_message(message);
            imported += 1;
        }
    }
    report.push(format!("imported {imported} message(s); preserved {skipped} line(s) with unresolved content"));
    let actual_path = futures::executor::block_on(async {
        session.save().await?;
        Ok::<_, Error>(session.path.clone())
    }).map_err(|error| Error::tool("import", format!("failed to write session: {error}")))?
        .ok_or_else(|| Error::tool("import", "session save produced no path"))?;

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

fn record_unmappable_line(
    session: &mut Session,
    line_no: usize,
    note: &str,
    line: &[u8],
    skipped: &mut usize,
    report: &mut Vec<String>,
) {
    *skipped += 1;
    report.push(format!("line {}: {note}; original retained as attachment", line_no + 1));
    let mut attachment = json!({
        "schema": IMPORT_SCHEMA, "line": line_no + 1, "reason": note,
    });
    match std::str::from_utf8(line) {
        Ok(raw) => attachment["raw"] = Value::String(raw.to_string()),
        Err(_) => attachment["rawBase64"] = Value::String(base64::engine::general_purpose::STANDARD.encode(line)),
    }
    session.append_custom_entry("foreign_attachment".to_string(), Some(attachment));
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ContentBlock, Message};

    fn claude_fixture() -> String {
        [
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/proj","message":{"role":"user","content":[{"type":"text","text":"fix the parser"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"On it."},{"type":"thinking","thinking":"checking tests first"}],"model":"claude-3"}}"#,
            "this is not json",
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:02Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"tc1","name":"read","input":{"path":"src/parser.rs"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tc1","content":"parser source"}]}}"#,
        ].join("\n")
    }

    fn codex_fixture() -> String {
        [
            r#"{"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":"cx1","cwd":"/tmp/proj"}}"#,
            r#"{"type":"response_item","timestamp":"2026-01-01T00:00:01Z","payload":{"type":"message","role":"user","content":[{"text":"fix the parser"}]}}"#,
            r#"{"type":"response_item","timestamp":"2026-01-01T00:00:02Z","payload":{"type":"reasoning","summary":[{"text":"tests first"}]}}"#,
            r#"{"type":"response_item","timestamp":"2026-01-01T00:00:03Z","payload":{"type":"function_call","name":"read","arguments":"{\"path\":\"src/parser.rs\"}","call_id":"c1"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"parser source"}}"#,
        ].join("\n")
    }

    #[test]
    fn claude_fixture_imports_with_corruption_tolerance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("claude.jsonl");
        std::fs::write(&source, claude_fixture()).expect("write");
        let outcome = import_claude(&source, Some(dir.path())).expect("import");
        assert_eq!(outcome.imported, 4, "{:?}", outcome.report);
        assert_eq!(outcome.skipped, 1, "{:?}", outcome.report);
        assert!(!outcome.already_imported);
        let again = import_claude(&source, Some(dir.path())).expect("re-import");
        assert!(again.already_imported);
        assert_eq!(again.session_id, outcome.session_id);
        let session = futures::executor::block_on(Session::open(&outcome.session_path)).expect("load");
        let messages = session.to_messages_for_current_path();
        assert_eq!(messages.len(), 4);
        assert!(matches!(&messages[3], Message::ToolResult(result) if result.tool_name == "read"));
        let saved = std::fs::read_to_string(&outcome.session_path).expect("saved");
        assert!(saved.contains("this is not json"));
    }

    #[test]
    fn codex_fixture_imports_reasoning_as_thinking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("codex.jsonl");
        std::fs::write(&source, codex_fixture()).expect("write");
        let outcome = import_codex(&source, Some(dir.path())).expect("import");
        assert_eq!(outcome.imported, 4, "{:?}", outcome.report);
        let session = futures::executor::block_on(Session::open(&outcome.session_path)).expect("load");
        let messages = session.to_messages_for_current_path();
        assert!(messages.iter().any(|message| match message {
            Message::Assistant(assistant) => assistant.content.iter().any(|block| matches!(block, ContentBlock::Thinking(_))),
            _ => false,
        }));
        assert!(matches!(&messages[3], Message::ToolResult(result) if result.tool_name == "read"));
        assert!(session.header.provider.is_none());
        assert!(session.header.model_id.is_none());
        let saved = std::fs::read_to_string(&outcome.session_path).expect("saved");
        assert!(saved.contains("/tmp/proj"), "foreign cwd remains provenance");
    }

    #[test]
    fn ids_are_stable_source_specific_and_have_content_specific_filename_prefixes() {
        let first = session_id_for(ImportSource::Claude, b"a");
        assert_eq!(first, session_id_for(ImportSource::Claude, b"a"));
        assert_ne!(first, session_id_for(ImportSource::Codex, b"a"));
        assert_ne!(&first[..8], &session_id_for(ImportSource::Claude, b"b")[..8]);
        assert!(!first.starts_with("import-"));
    }

    #[test]
    fn header_probe_requires_an_exact_header_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.jsonl");
        let id = session_id_for(ImportSource::Codex, b"fixture");
        std::fs::write(&path, json!({"id": "other", "description": id}).to_string()).expect("write");
        assert!(!header_has_id(&path, &id));
        std::fs::write(&path, json!({"id": id}).to_string()).expect("write");
        assert!(header_has_id(&path, &id));
    }

    #[test]
    fn unmappable_records_keep_complete_bytes_including_invalid_utf8() {
        let dir = tempfile::tempdir().expect("tempdir");
        let unknown = json!({"type": "response_item", "payload": {"type": "future_item", "opaque": "x".repeat(2000)}}).to_string();
        let invalid = [0xff, 0xfe, b'x'];
        let mut raw = unknown.as_bytes().to_vec();
        raw.push(b'\n');
        raw.extend_from_slice(&invalid);
        let outcome = import_bytes(ImportSource::Codex, &raw, Path::new("foreign.jsonl"), Some(dir.path())).expect("import");
        assert_eq!(outcome.skipped, 2);
        let saved = std::fs::read_to_string(&outcome.session_path).expect("saved");
        let values: Vec<Value> = saved.lines().map(|line| serde_json::from_str(line).expect("native JSONL")).collect();
        assert!(values.iter().any(|entry| entry.pointer("/data/raw").and_then(Value::as_str) == Some(unknown.as_str())));
        let encoded = base64::engine::general_purpose::STANDARD.encode(invalid);
        assert!(values.iter().any(|entry| entry.pointer("/data/rawBase64").and_then(Value::as_str) == Some(encoded.as_str())));
    }

    #[test]
    fn source_admission_rejects_nonfiles_and_oversized_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_source(dir.path()).is_err());
        let path = dir.path().join("large.jsonl");
        let file = std::fs::File::create(&path).expect("create sparse file");
        file.set_len(MAX_IMPORT_BYTES + 1).expect("set len");
        assert!(read_source(&path).is_err());
    }
}
