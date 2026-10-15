//! Read-only views of native child results, published by the acceptance lease.
//!
//! The hub owns identities and lifecycle; this cache owns only bounded result
//! bytes. A transcript frame is never a published result. Cache keys include
//! the hub's private namespace, so reused display IDs cannot alias old runs.

use super::{ResolvedDoc, URL_ROUTER_SCHEMA};
use crate::agent_hub::{ChildEntry, ChildStatus};
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

const MAX_RESULTS: usize = 64;
const MAX_RESULT_BYTES: usize = 1024 * 1024;
const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;
const MAX_REFERENCE_BYTES: usize = 4096;
const MAX_PATH_DEPTH: usize = 64;
const MAX_ROSTER_ENTRIES: usize = 128;

#[derive(Serialize, Deserialize)]
struct PublishedOutput {
    output: String,
    data: Option<Value>,
    schema_valid: Option<bool>,
    error: Option<String>,
}

struct CachedOutput {
    namespace: PathBuf,
    id: String,
    encoded: Arc<str>,
}

#[derive(Default)]
struct OutputCache {
    entries: VecDeque<CachedOutput>,
    bytes: usize,
}

impl OutputCache {
    fn insert(&mut self, entry: &ChildEntry, encoded: String) {
        // First publication wins. Publication is not a way to revise a run,
        // and an oversized value must not evict otherwise usable results.
        if encoded.len() > MAX_RESULT_BYTES || self.get(entry).is_some() {
            return;
        }
        while self.entries.len() >= MAX_RESULTS
            || self.bytes.saturating_add(encoded.len()) > MAX_CACHE_BYTES
        {
            let Some(oldest) = self.entries.pop_front() else {
                return;
            };
            self.bytes = self.bytes.saturating_sub(oldest.encoded.len());
        }
        self.bytes += encoded.len();
        self.entries.push_back(CachedOutput {
            namespace: entry.transcript_path.clone(),
            id: entry.id.clone(),
            encoded: Arc::from(encoded),
        });
    }

    fn get(&self, entry: &ChildEntry) -> Option<Arc<str>> {
        self.entries
            .iter()
            .find(|item| item.id == entry.id && item.namespace == entry.transcript_path)
            .map(|item| Arc::clone(&item.encoded))
    }
}

fn cache() -> &'static Mutex<OutputCache> {
    static CACHE: OnceLock<Mutex<OutputCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(OutputCache::default()))
}

/// Called only after native result acceptance and worktree disposition. The
/// caller releases the hub mutex first; readers use that same lock order.
/// Losing optional retention must never undo an already accepted child run.
pub(crate) fn publish(
    entry: &ChildEntry,
    output: &str,
    data: Option<&Value>,
    schema_valid: Option<bool>,
    failure: Option<&str>,
) {
    if !entry.status.settled() || output.len() > MAX_RESULT_BYTES {
        return;
    }
    let snapshot = PublishedOutput {
        output: output.to_string(),
        data: data
            .filter(|_| entry.status == ChildStatus::Done && schema_valid == Some(true))
            .cloned(),
        schema_valid,
        error: failure.map(str::to_string),
    };
    let Ok(encoded) = serde_json::to_string(&snapshot) else {
        return;
    };
    if let Ok(mut cache) = cache().lock() {
        cache.insert(entry, encoded);
    }
}

fn error(code: &str, message: impl std::fmt::Display) -> Error {
    Error::tool("read", format!("{code}: {message}"))
}

struct Reference<'a> {
    id: &'a str,
    segments: Vec<String>,
    dotted: bool,
}

impl<'a> Reference<'a> {
    fn parse(reference: &'a str) -> Result<Self> {
        if reference.len() > MAX_REFERENCE_BYTES {
            return Err(error(
                "PI_URL_AGENT_REFERENCE",
                "agent reference exceeds 4096 bytes",
            ));
        }
        let (id, path) = reference.split_once('/').unwrap_or((reference, ""));
        if id.is_empty()
            || matches!(id, "." | ".." | "all")
            || !id
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(error(
                "PI_URL_AGENT_REFERENCE",
                "expected agent://<id> or agent://<id>/<JSON path>; read agent:// to list children",
            ));
        }
        let mut segments = Vec::new();
        if !path.is_empty() {
            for segment in path.split('/') {
                if segment.is_empty() || segments.len() == MAX_PATH_DEPTH {
                    return Err(error(
                        "PI_URL_AGENT_REFERENCE",
                        "empty or over-deep JSON path",
                    ));
                }
                segments.push(decode_segment(segment)?);
            }
        }
        Ok(Self {
            id,
            segments,
            // Slash-separated keys are always literal. The older dotted
            // spelling is a fallback only for a single, unescaped component.
            dotted: !path.contains('/') && !path.contains('%') && path.contains('.'),
        })
    }
}

const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_segment(segment: &str) -> Result<String> {
    let mut decoded = Vec::with_capacity(segment.len());
    let mut bytes = segment.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(hex_digit);
            let low = bytes.next().and_then(hex_digit);
            let (Some(high), Some(low)) = (high, low) else {
                return Err(error(
                    "PI_URL_AGENT_REFERENCE",
                    "invalid percent escape in JSON path",
                ));
            };
            decoded.push(high * 16 + low);
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| error("PI_URL_AGENT_REFERENCE", "JSON path is not valid UTF-8"))
}

fn walk<'a, 's>(
    mut value: &'a Value,
    segments: impl IntoIterator<Item = &'s str>,
) -> Option<&'a Value> {
    for segment in segments {
        value = match value {
            Value::Object(object) => object.get(segment)?,
            Value::Array(array)
                if !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit()) =>
            {
                array.get(segment.parse::<usize>().ok()?)?
            }
            _ => return None,
        };
    }
    Some(value)
}

fn project<'a>(value: &'a Value, reference: &Reference<'_>) -> Result<&'a Value> {
    walk(value, reference.segments.iter().map(String::as_str))
        .or_else(|| {
            if !reference.dotted {
                return None;
            }
            let path = reference.segments.first()?;
            if path.split('.').count() > MAX_PATH_DEPTH || path.split('.').any(str::is_empty) {
                return None;
            }
            walk(value, path.split('.'))
        })
        .ok_or_else(|| {
            error(
                "PI_URL_AGENT_FIELD_NOT_FOUND",
                "no value at the requested JSON path",
            )
        })
}

fn masked(text: &str) -> String {
    let mut vault = crate::secrets::SecretVault::default();
    crate::secrets::obfuscate(text, &mut vault, &[]).0
}

fn document(
    reference: &str,
    content: String,
    content_type: &str,
    metadata: Value,
) -> ResolvedDoc {
    ResolvedDoc {
        schema: URL_ROUTER_SCHEMA.to_string(),
        scheme: "agent".to_string(),
        reference: reference.to_string(),
        content: masked(&content),
        content_type: content_type.to_string(),
        metadata,
        line_addressable: true,
    }
}

fn render(reference: &str, entry: &ChildEntry, encoded: Option<&str>) -> Result<ResolvedDoc> {
    let parsed = Reference::parse(reference)?;
    let snapshot = encoded
        .map(serde_json::from_str::<PublishedOutput>)
        .transpose()
        .map_err(|_| {
            error(
                "PI_URL_AGENT_OUTPUT_UNAVAILABLE",
                "retained output is unreadable",
            )
        })?;
    let mut metadata = json!({
        "id": entry.id,
        "status": entry.status.as_str(),
        "outputAvailable": snapshot.is_some(),
        "outputBytes": entry.output_bytes,
        "schemaValid": snapshot.as_ref().and_then(|output| output.schema_valid),
    });
    if let Some(failure) = snapshot.as_ref().and_then(|output| output.error.as_deref()) {
        metadata["error"] = Value::String(masked(failure));
    }
    if parsed.segments.is_empty() {
        let Some(snapshot) = snapshot else {
            // Running and cancelled children still have identities. Their
            // status is useful, but it is not a successfully published output.
            return Ok(document(
                reference,
                serde_json::to_string_pretty(&metadata)?,
                "application/json",
                metadata,
            ));
        };
        let content = if entry.status != ChildStatus::Done {
            format!(
                "[agent {}: {}]\n{}",
                entry.id,
                entry.status.as_str(),
                snapshot.output
            )
        } else if snapshot.schema_valid == Some(false) {
            format!(
                "[agent {}: output failed schema validation]\n{}",
                entry.id, snapshot.output
            )
        } else {
            snapshot.output
        };
        return Ok(document(reference, content, "text/markdown", metadata));
    }
    if !entry.status.settled() {
        return Err(error(
            "PI_URL_AGENT_OUTPUT_PENDING",
            "child has not completed result acceptance",
        ));
    }
    if entry.status != ChildStatus::Done {
        return Err(error(
            "PI_URL_AGENT_OUTPUT_REJECTED",
            "failed, killed or cancelled child has no accepted JSON result",
        ));
    }
    let snapshot = snapshot.ok_or_else(|| {
        error(
            "PI_URL_AGENT_OUTPUT_UNAVAILABLE",
            "no retained result for this run (not published or evicted); inspect its hub transcript",
        )
    })?;
    let value = match (snapshot.schema_valid, snapshot.data) {
        (Some(false), _) => {
            return Err(error(
                "PI_URL_AGENT_OUTPUT_REJECTED",
                "child output failed schema validation",
            ));
        }
        (Some(true), Some(data)) => data,
        (Some(true), None) => {
            return Err(error(
                "PI_URL_AGENT_OUTPUT_UNAVAILABLE",
                "validated result data is unavailable",
            ));
        }
        (None, _) => serde_json::from_str(&snapshot.output).map_err(|_| {
            error(
                "PI_URL_AGENT_NOT_JSON",
                "successful child output is not a JSON value",
            )
        })?,
    };
    let selected = project(&value, &parsed)?;
    let (content, content_type) = match selected {
        Value::String(text) => (text.clone(), "text/plain"),
        _ => (serde_json::to_string_pretty(selected)?, "application/json"),
    };
    metadata["extracted"] = json!(parsed.segments);
    Ok(document(reference, content, content_type, metadata))
}

pub(super) fn resolve(reference: &str) -> Result<ResolvedDoc> {
    if reference.is_empty() {
        let mut roster = crate::agent_hub::registry()
            .lock()
            .map_err(|_| {
                error(
                    "PI_URL_AGENT_HUB_UNAVAILABLE",
                    "agent registry is poisoned",
                )
            })?
            .roster();
        roster.sort_by(|a, b| b.started_ms.cmp(&a.started_ms).then_with(|| a.id.cmp(&b.id)));
        let total = roster.len();
        let entries: Vec<_> = roster
            .into_iter()
            .take(MAX_ROSTER_ENTRIES)
            .map(|entry| {
                json!({
                    "id": entry.id,
                    "status": entry.status.as_str(),
                    "outputBytes": entry.output_bytes,
                    "url": format!("agent://{}", entry.id),
                })
            })
            .collect();
        let truncated = total > MAX_ROSTER_ENTRIES;
        let metadata = json!({"total": total, "truncated": truncated, "scope": "parentProcess"});
        return Ok(document(
            reference,
            serde_json::to_string_pretty(&json!({
                "agents": entries, "total": total, "truncated": truncated,
            }))?,
            "application/json",
            metadata,
        ));
    }
    let parsed = Reference::parse(reference)?;
    let entry = crate::agent_hub::registry()
        .lock()
        .map_err(|_| {
            error(
                "PI_URL_AGENT_HUB_UNAVAILABLE",
                "agent registry is poisoned",
            )
        })?
        .get(parsed.id)
        .ok_or_else(|| {
            error(
                "PI_URL_AGENT_NOT_FOUND",
                "unknown child in this parent process; read agent:// for its roster",
            )
        })?;
    // Never hold the hub mutex during cache access, decoding or redaction.
    let encoded = cache()
        .lock()
        .map_err(|_| error("PI_URL_AGENT_OUTPUT_UNAVAILABLE", "result cache is poisoned"))?
        .get(&entry);
    render(reference, &entry, encoded.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_hub::ChildKind;

    fn entry(id: &str, status: ChildStatus) -> ChildEntry {
        ChildEntry {
            id: id.to_string(),
            name: "worker".to_string(),
            kind: ChildKind::Subagent,
            task: "not exposed by agent URLs".to_string(),
            pid: None,
            status,
            started_ms: 1,
            finished_ms: status.settled().then_some(2),
            output_bytes: 100,
            transcript_path: PathBuf::from(format!("session-a/{id}.jsonl")),
            steer_path: PathBuf::from(format!("session-a/{id}.steer")),
            revived_from: None,
        }
    }

    fn snapshot(data: Value) -> String {
        serde_json::to_string(&PublishedOutput {
            output: "accepted prose".to_string(),
            data: Some(data),
            schema_valid: Some(true),
            error: None,
        })
        .unwrap()
    }

    #[test]
    fn slash_and_dotted_paths_extract_the_accepted_value() {
        let entry = entry("worker-1", ChildStatus::Done);
        let encoded = snapshot(json!({
            "findings":[{"path":"src/main.rs"}], "empty":null, "count":3,
        }));
        for path in ["findings/0/path", "findings.0.path"] {
            let doc = render(&format!("worker-1/{path}"), &entry, Some(&encoded)).unwrap();
            assert_eq!(doc.content, "src/main.rs");
            assert_eq!(doc.content_type, "text/plain");
        }
        assert_eq!(
            render("worker-1/empty", &entry, Some(&encoded)).unwrap().content,
            "null"
        );
        assert_eq!(
            render("worker-1/count", &entry, Some(&encoded)).unwrap().content,
            "3"
        );
        assert!(
            render("worker-1/missing", &entry, Some(&encoded))
                .unwrap_err()
                .to_string()
                .contains("FIELD_NOT_FOUND")
        );
    }

    #[test]
    fn slash_segments_are_literal_and_decoded_exactly_once() {
        let entry = entry("Parent.Child-1", ChildStatus::Done);
        let encoded = snapshot(json!({
            "a.b":{"c/d":"café"}, "a":{"b":"nested"}, "%2F":"literal percent",
        }));
        assert_eq!(
            render("Parent.Child-1/a.b/c%2Fd", &entry, Some(&encoded))
                .unwrap()
                .content,
            "café"
        );
        assert_eq!(
            render("Parent.Child-1/%252F", &entry, Some(&encoded))
                .unwrap()
                .content,
            "literal percent"
        );
        let doc = render("Parent.Child-1/a.b", &entry, Some(&encoded)).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&doc.content).unwrap(),
            json!({"c/d":"café"})
        );
    }

    #[test]
    fn malformed_paths_do_not_become_filesystem_reads() {
        for path in [
            "../x", "worker-1/a//b", "worker-1/%", "worker-1/%GG",
            "worker-1/%FF", "worker-1?secret=x", "all",
        ] {
            assert!(Reference::parse(path).is_err(), "{path}");
        }
        assert!(Reference::parse(&format!("worker-1/{}", "x/".repeat(65))).is_err());
        assert!(Reference::parse(&"x".repeat(MAX_REFERENCE_BYTES + 1)).is_err());
        assert_eq!(decode_segment("caf%C3%A9").unwrap(), "café");
        assert_eq!(decode_segment("a+b").unwrap(), "a+b");
    }

    #[test]
    fn array_indices_and_null_are_not_confused_with_missing_fields() {
        let entry = entry("worker-1", ChildStatus::Done);
        let encoded = snapshot(json!([null, {"0":"object key"}]));
        assert_eq!(
            render("worker-1/0", &entry, Some(&encoded)).unwrap().content,
            "null"
        );
        assert_eq!(
            render("worker-1/1/0", &entry, Some(&encoded)).unwrap().content,
            "object key"
        );
        for path in ["-1", "+1", "-", "2", "0/child", "184467440737095516160"] {
            assert!(
                render(&format!("worker-1/{path}"), &entry, Some(&encoded)).is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn pending_failed_and_unvalidated_results_cannot_supply_typed_values() {
        let encoded = snapshot(json!({"ok":true}));
        for status in [
            ChildStatus::Starting,
            ChildStatus::Running,
            ChildStatus::Failed,
            ChildStatus::Killed,
            ChildStatus::Cancelled,
        ] {
            let entry = entry("worker-1", status);
            assert!(render("worker-1/ok", &entry, Some(&encoded)).is_err());
            let doc = render("worker-1", &entry, None).unwrap();
            assert_eq!(doc.metadata["status"], status.as_str());
            assert_eq!(doc.metadata["outputAvailable"], false);
        }
        let entry = entry("worker-1", ChildStatus::Done);
        let invalid = serde_json::to_string(&PublishedOutput {
            output: "{\"ok\":true}".into(),
            data: None,
            schema_valid: Some(false),
            error: None,
        })
        .unwrap();
        assert!(
            render("worker-1/ok", &entry, Some(&invalid))
                .unwrap_err()
                .to_string()
                .contains("OUTPUT_REJECTED")
        );
        assert!(
            render("worker-1/ok", &entry, None)
                .unwrap_err()
                .to_string()
                .contains("OUTPUT_UNAVAILABLE")
        );
    }

    #[test]
    fn successful_untyped_json_is_projectable_but_prose_is_not_guessed() {
        let entry = entry("worker-1", ChildStatus::Done);
        for (output, valid) in [("{\"ok\":true}", true), ("Here is {\"ok\":true}", false)] {
            let encoded = serde_json::to_string(&PublishedOutput {
                output: output.into(),
                data: None,
                schema_valid: None,
                error: None,
            })
            .unwrap();
            assert_eq!(render("worker-1/ok", &entry, Some(&encoded)).is_ok(), valid);
        }
    }

    #[test]
    fn cache_is_immutable_bounded_and_namespace_qualified() {
        let mut cache = OutputCache::default();
        let first = entry("worker-1", ChildStatus::Done);
        cache.insert(&first, "first".into());
        cache.insert(&first, "replacement".into());
        assert_eq!(cache.get(&first).as_deref(), Some("first"));
        let mut other = first.clone();
        other.transcript_path = PathBuf::from("session-b/worker-1.jsonl");
        assert!(cache.get(&other).is_none());
        cache.insert(&other, "other namespace".into());
        assert_eq!(cache.get(&other).as_deref(), Some("other namespace"));
        for index in 2..=MAX_RESULTS + 1 {
            cache.insert(
                &entry(&format!("worker-{index}"), ChildStatus::Done),
                "small".into(),
            );
        }
        assert_eq!(cache.entries.len(), MAX_RESULTS);
        assert!(cache.get(&first).is_none());
        assert_eq!(
            cache.bytes,
            cache.entries.iter().map(|entry| entry.encoded.len()).sum::<usize>()
        );
        let before = cache.bytes;
        cache.insert(
            &entry("oversize", ChildStatus::Done),
            "x".repeat(MAX_RESULT_BYTES + 1),
        );
        assert_eq!(cache.bytes, before);
    }

    #[test]
    fn cache_enforces_the_total_byte_budget_not_just_entry_count() {
        let mut cache = OutputCache::default();
        for index in 0..12 {
            cache.insert(
                &entry(&format!("worker-{index}"), ChildStatus::Done),
                "x".repeat(MAX_RESULT_BYTES),
            );
        }
        assert_eq!(cache.bytes, MAX_CACHE_BYTES);
        assert_eq!(cache.entries.len(), MAX_CACHE_BYTES / MAX_RESULT_BYTES);
        assert!(cache.get(&entry("worker-0", ChildStatus::Done)).is_none());
        assert!(cache.get(&entry("worker-11", ChildStatus::Done)).is_some());
    }
}
