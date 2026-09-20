//! Native semantic completion: list, resolve/preview, explicitly apply.
//!
//! Handles retain one bounded listing, its immutable synchronized source and
//! its exact server connection. Numeric snippets accept explicit literal
//! substitutions; no command permission is implied. Insertion and auto-import
//! edits share the existing transaction.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::client::DocumentSnapshot;
use super::edits::{FileEvidence, apply_checked, parse_workspace_edit};
use super::registry::ServerEntry;
use super::text::{Position, Range, content_hash_for_drift, position_to_offset_exact};
use super::{
    LspInput, LspTool, MAX_PAYLOAD_BYTES, display_path, resolve_tool_path, text_output, tool_err,
};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;

mod item;
mod snippet;

const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_ITEM_BYTES: usize = 64 * 1024;
const MAX_SERVER_ITEMS: usize = 4096;
const MAX_ITEMS: usize = 128;
const HANDLE_AGE: Duration = Duration::from_secs(300);

fn malformed(message: &str) -> Error {
    tool_err("LSP_COMPLETION_MALFORMED", message)
}

fn stale() -> Error {
    tool_err(
        "LSP_COMPLETION_STALE",
        "completion source, listing or server changed; request a new listing",
    )
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn bounded_size(value: &impl serde::Serialize, limit: usize) -> Result<usize> {
    struct Budget(usize);
    impl Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("completion byte limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut budget = Budget(limit);
    serde_json::to_writer(&mut budget, value).map_err(|_| {
        tool_err(
            "LSP_COMPLETION_LIMIT",
            "completion exceeds its serialized byte limit",
        )
    })?;
    Ok(limit - budget.0)
}

fn read_source(path: &Path) -> Result<String> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(tool_err(
            "LSP_FILE_UNREADABLE",
            "completion requires a regular, nonsymlink source file",
        ));
    }
    if meta.len() > MAX_SOURCE_BYTES as u64 {
        return Err(tool_err(
            "LSP_COMPLETION_LIMIT",
            "completion source exceeds 2 MiB",
        ));
    }
    let mut text = String::new();
    std::fs::File::open(path)?
        .take((MAX_SOURCE_BYTES + 1) as u64)
        .read_to_string(&mut text)
        .map_err(|error| {
            tool_err(
                "LSP_FILE_UNREADABLE",
                format!("cannot read completion source: {error}"),
            )
        })?;
    if text.len() > MAX_SOURCE_BYTES {
        return Err(tool_err(
            "LSP_COMPLETION_LIMIT",
            "completion source exceeds 2 MiB",
        ));
    }
    Ok(text)
}

struct Budget {
    owner: AgentCx,
    started: Instant,
    timeout: Duration,
}

impl Budget {
    fn new(timeout: Duration) -> Self {
        Self {
            owner: AgentCx::for_current_or_request(),
            started: Instant::now(),
            timeout,
        }
    }

    fn remaining(&self) -> Result<Duration> {
        self.owner
            .checkpoint()
            .map_err(|_| tool_err("LSP_CANCELLED", "completion cancelled"))?;
        if !self.owner.capabilities().io {
            return Err(tool_err(
                "LSP_EDIT_PERMISSION",
                "completion owner does not permit filesystem I/O",
            ));
        }
        self.timeout
            .checked_sub(self.started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| tool_err("LSP_TIMEOUT", "completion request budget expired"))
    }
}

struct Source {
    entry: Weak<ServerEntry>,
    path: PathBuf,
    uri: String,
    text: Arc<str>,
    hash: u64,
    document: Option<DocumentSnapshot>,
    position: Position,
    fallback: Option<Range>,
    created: Instant,
}

impl Source {
    fn verify(&self) -> Result<Arc<ServerEntry>> {
        let entry = self
            .entry
            .upgrade()
            .filter(|entry| entry.client.is_alive())
            .ok_or_else(stale)?;
        if self.created.elapsed() >= HANDLE_AGE || !self.path.starts_with(entry.client.root()) {
            return Err(stale());
        }
        let text = entry
            .client
            .synchronized_text(&self.uri)
            .ok_or_else(stale)?;
        if !Arc::ptr_eq(&self.text, &text) {
            return Err(stale());
        }
        if let Some(before) = self.document {
            let now = entry.client.document_snapshots();
            if !now
                .get(&self.path)
                .is_some_and(|now| now.version == before.version && now.hash == before.hash)
            {
                return Err(stale());
            }
        }
        // Compare exact text as well as the transaction hash; do not bless a
        // reread as a new baseline after the request or lazy resolution.
        if read_source(&self.path)?.as_str() != self.text.as_ref() {
            return Err(stale());
        }
        Ok(entry)
    }
}

#[derive(Clone)]
struct Candidate {
    source: Arc<Source>,
    item: Value,
    resolve: bool,
}

#[derive(Default)]
struct State {
    serial: u64,
    items: HashMap<String, Candidate>,
}

#[derive(Default)]
pub(super) struct CompletionCache(Mutex<State>);

impl CompletionCache {
    pub(super) fn clear(&self) {
        lock(&self.0).items.clear();
    }

    fn start(&self) -> Result<u64> {
        let mut state = lock(&self.0);
        state.items.clear();
        state.serial = state.serial.checked_add(1).ok_or_else(|| {
            tool_err(
                "LSP_COMPLETION_LIMIT",
                "completion identity space exhausted",
            )
        })?;
        Ok(state.serial)
    }
}

fn list_parts(raw: &Value) -> Result<(&[Value], Option<&Value>, bool)> {
    bounded_size(raw, MAX_RESPONSE_BYTES)?;
    let (items, defaults, incomplete) = match raw {
        Value::Null => (&[][..], None, false),
        Value::Array(items) => (items.as_slice(), None, false),
        Value::Object(object) => {
            if object.contains_key("applyKind") {
                return Err(malformed("completion applyKind was not negotiated"));
            }
            let items = object
                .get("items")
                .and_then(Value::as_array)
                .ok_or_else(|| malformed("completion list needs items"))?;
            let incomplete = object
                .get("isIncomplete")
                .and_then(Value::as_bool)
                .ok_or_else(|| malformed("completion list needs boolean isIncomplete"))?;
            (items.as_slice(), object.get("itemDefaults"), incomplete)
        }
        _ => {
            return Err(malformed(
                "completion result must be null, an array or a CompletionList",
            ));
        }
    };
    if items.len() > MAX_SERVER_ITEMS {
        return Err(tool_err(
            "LSP_COMPLETION_LIMIT",
            "server returned more than 4096 completions",
        ));
    }
    for item in items {
        item::label(item)?;
        for key in ["sortText", "filterText"] {
            if item
                .get(key)
                .is_some_and(|value| !value.is_null() && !value.is_string())
            {
                return Err(malformed(
                    "completion sortText and filterText must be strings",
                ));
            }
        }
    }
    if let Some(defaults) = defaults {
        let defaults = defaults
            .as_object()
            .ok_or_else(|| malformed("itemDefaults must be an object"))?;
        if defaults.keys().any(|key| {
            !matches!(
                key.as_str(),
                "editRange" | "insertTextFormat" | "insertTextMode" | "data"
            )
        }) {
            return Err(malformed(
                "server returned an unnegotiated completion default",
            ));
        }
    }
    Ok((items, defaults, incomplete))
}

fn sort_key(item: &Value) -> &str {
    item.get("sortText")
        .and_then(Value::as_str)
        .unwrap_or_else(|| item["label"].as_str().unwrap_or(""))
}

fn clipped(value: Option<&Value>, limit: usize) -> Value {
    let Some(text) = value.and_then(Value::as_str) else {
        return Value::Null;
    };
    if text.len() <= limit {
        return json!(text);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    json!(format!("{}…", &text[..end]))
}

fn validate_input(input: &LspInput) -> Result<()> {
    if let Some(values) = &input.snippet_values {
        snippet::validate_values(values)?;
        if input.completion_id.is_none() {
            return Err(tool_err(
                "LSP_USAGE",
                "snippetValues require a selected completionId",
            ));
        }
    }
    if input.symbol.is_some()
        || input.line.is_some()
        || input.action_id.is_some()
        || input.hierarchy_id.is_some()
        || input.method.is_some()
        || input.payload.is_some()
        || input.new_name.is_some()
        || input.new_file.is_some()
        || input.format_options.is_some()
    {
        return Err(tool_err(
            "LSP_USAGE",
            "completion uses file + position, or completionId; unrelated selectors are not accepted",
        ));
    }
    if let Some(id) = &input.completion_id {
        if id.is_empty()
            || id.len() > 128
            || input.file.is_some()
            || input.position.is_some()
            || input.range.is_some()
            || input.query.is_some()
            || input.limit.is_some()
        {
            return Err(tool_err(
                "LSP_USAGE",
                "completionId already identifies its source and selection; do not combine it with listing selectors",
            ));
        }
    } else if input.file.is_none()
        || input.position.is_none()
        || input.apply == Some(true)
        || input.limit == Some(0)
        || input.query.as_ref().is_some_and(|query| query.len() > 128)
    {
        return Err(tool_err(
            "LSP_USAGE",
            "list completions with file and exact position; optional query is a case-sensitive prefix up to 128 bytes; apply requires completionId",
        ));
    }
    Ok(())
}

impl LspTool {
    pub(super) async fn run_completion(&self, input: &LspInput) -> Result<ToolOutput> {
        validate_input(input)?;
        let budget = Budget::new(self.request_timeout(input));
        budget.remaining()?;
        if let Some(id) = input.completion_id.as_deref() {
            return self
                .select_completion(
                    id,
                    input.apply == Some(true),
                    input.snippet_values.as_ref(),
                    &budget,
                )
                .await;
        }
        self.list_completions(input, &budget).await
    }

    #[allow(clippy::too_many_lines)]
    async fn list_completions(&self, input: &LspInput, budget: &Budget) -> Result<ToolOutput> {
        let serial = self.completions.start()?;
        let requested = resolve_tool_path(
            input
                .file
                .as_deref()
                .ok_or_else(|| malformed("missing file"))?,
            &self.cwd,
        );
        // Reject a selected symlink before canonicalization erases its identity.
        let text = read_source(&requested)?;
        let path = requested.canonicalize()?;
        let position = input
            .position
            .ok_or_else(|| malformed("missing position"))?;
        if position_to_offset_exact(&text, position).is_none() {
            return Err(tool_err(
                "LSP_USAGE",
                "completion position must be an exact zero-based UTF-16 boundary",
            ));
        }
        if let Some(range) = input.range {
            item::parsed_range(&json!(range), &text)?;
            item::primary_range(range, position)?;
        }
        let now = budget
            .owner
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        let (uri, entry) = asupersync::time::timeout(now, budget.remaining()?, self.synced(&path))
            .await
            .map_err(|_| {
                tool_err(
                    "LSP_TIMEOUT",
                    "completion server startup exceeded the request budget",
                )
            })??;
        let caps = entry.client.capabilities().raw;
        if caps
            .get("positionEncoding")
            .is_some_and(|encoding| encoding.as_str() != Some("utf-16"))
        {
            return Err(tool_err(
                "LSP_UNSUPPORTED",
                "completion server selected an unadvertised position encoding",
            ));
        }
        let provider = caps
            .get("completionProvider")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                tool_err(
                    "LSP_UNSUPPORTED",
                    "server did not advertise completionProvider",
                )
            })?;
        let resolve = match provider.get("resolveProvider") {
            None => false,
            Some(Value::Bool(resolve)) => *resolve,
            _ => return Err(malformed("completion resolveProvider must be boolean")),
        };
        let synced = entry
            .client
            .synchronized_text(&uri)
            .filter(|synced| synced.as_ref() == text.as_str())
            .ok_or_else(stale)?;
        let document = entry.client.document_snapshots().get(&path).copied();
        let source = Arc::new(Source {
            entry: Arc::downgrade(&entry),
            path,
            uri,
            hash: content_hash_for_drift(&text),
            text: synced,
            document,
            position,
            fallback: input.range,
            created: Instant::now(),
        });
        source.verify()?;
        let raw = entry.client.call("textDocument/completion", json!({
            "textDocument":{"uri":source.uri},"position":position,"context":{"triggerKind":1}
        }), budget.remaining()?).await?;
        budget.remaining()?;
        source.verify()?;
        let (items, defaults, incomplete) = list_parts(&raw)?;
        let prefix = input.query.as_deref().unwrap_or("");
        let mut matching: Vec<_> = items
            .iter()
            .filter(|item| {
                item.get("filterText")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| item["label"].as_str().unwrap_or(""))
                    .starts_with(prefix)
            })
            .collect();
        matching.sort_by(|left, right| sort_key(left).cmp(sort_key(right)));
        let matched = matching.len();
        let limit = input.limit.unwrap_or(50).min(MAX_ITEMS);
        let mut summaries = Vec::new();
        let mut retained = HashMap::new();
        let mut retained_bytes = 0usize;
        for (index, raw) in matching.into_iter().take(limit).enumerate() {
            budget.remaining()?;
            let item = item::materialize(raw, defaults)?;
            retained_bytes += bounded_size(&item, MAX_ITEM_BYTES)?;
            if retained_bytes > MAX_RESPONSE_BYTES {
                break;
            }
            let id = format!("completion-{serial}-{}", index + 1);
            let reason = snippet::prepare(&item, None)
                .and_then(|expanded| {
                    item::edits(&expanded.item, &source.text, position, input.range)?;
                    expanded.require_values()
                })
                .err()
                .map(|error| error.to_string());
            summaries.push(json!({
                "completionId":id,"label":item["label"],"kind":item.get("kind"),
                "detail":clipped(item.get("detail"), 512),"needsResolve":resolve,
                "blockedReason":reason,"snippet":item.get("insertTextFormat").and_then(Value::as_u64)==Some(2)
            }));
            // Leave room for envelope metadata. Never turn structured output
            // into a truncated string or return an ID that wasn't retained.
            if bounded_size(&summaries, MAX_PAYLOAD_BYTES - 8192).is_err() {
                summaries.pop();
                break;
            }
            retained.insert(
                id,
                Candidate {
                    source: Arc::clone(&source),
                    item,
                    resolve,
                },
            );
        }
        budget.remaining()?;
        source.verify()?;
        let count = summaries.len();
        let payload = json!({
            "action":"completion","file":display_path(&source.path, &self.cwd),"position":position,
            "server":entry.spec_name,"count":count,"matched":matched,"isIncomplete":incomplete,
            "truncated":count < matched,"items":summaries,
            "note":"Select completionId to resolve and preview; supply snippetValues for numeric placeholders and repeat them with apply:true. Commands, variables and transforms are not executed."
        });
        bounded_size(&payload, MAX_PAYLOAD_BYTES)?;
        lock(&self.completions.0).items = retained;
        Ok(text_output(payload.to_string(), payload))
    }

    #[allow(clippy::significant_drop_tightening)]
    async fn select_completion(
        &self,
        id: &str,
        apply: bool,
        values: Option<&snippet::Values>,
        budget: &Budget,
    ) -> Result<ToolOutput> {
        budget.remaining()?;
        let mut selected = {
            lock(&self.completions.0)
                .items
                .get(id)
                .cloned()
                .ok_or_else(stale)?
        };
        let entry = selected.source.verify()?;
        if selected.resolve {
            let raw = entry
                .client
                .call(
                    "completionItem/resolve",
                    selected.item.clone(),
                    budget.remaining()?,
                )
                .await?;
            budget.remaining()?;
            selected.source.verify()?;
            selected.item = item::resolved(&selected.item, &raw)?;
            selected.resolve = false;
            {
                let mut cache = lock(&self.completions.0);
                let stored = cache.items.get_mut(id).ok_or_else(stale)?;
                *stored = selected.clone();
            }
        }
        budget.remaining()?;
        selected.source.verify()?;
        let source = &selected.source;
        let planned = snippet::prepare(&selected.item, values).and_then(|expanded| {
            let edits =
                item::edits(&expanded.item, &source.text, source.position, source.fallback)?;
            Ok((expanded, edits))
        });
        let (expanded, edits) = match planned {
            Ok(planned) => planned,
            Err(error) if !apply => {
                let payload = json!({"action":"completion","completionId":id,"applied":false,
                    "canApply":false,"reason":error.to_string(),"label":selected.item["label"]});
                return Ok(text_output(payload.to_string(), payload));
            }
            Err(error) => return Err(error),
        };
        if apply { expanded.require_values()?; }
        let mut changes = serde_json::Map::new();
        changes.insert(source.uri.clone(), json!(edits));
        let plan = parse_workspace_edit(&json!({"changes":changes}))?;
        let documentation = selected.item.get("documentation").and_then(|value| {
            if value.is_string() {
                Some(value)
            } else {
                value.get("value")
            }
        });
        if !apply {
            let payload = json!({"action":"completion","completionId":id,"applied":false,
                "canApply":expanded.missing.is_empty(),"label":selected.item["label"],"file":display_path(&source.path, &self.cwd),
                "detail":clipped(selected.item.get("detail"), 4096),"documentation":clipped(documentation, 8192),
                "edits":edits,"editMode":"replace","additionalEdits":edits.len().saturating_sub(1),
                "snippetPlaceholders":expanded.fields,"missingPlaceholders":expanded.missing,
                "note":"Preview substitutions are not cached; repeat snippetValues when applying. Missing positive tabstops require explicit values, including an empty string to intentionally omit one."});
            bounded_size(&payload, MAX_PAYLOAD_BYTES)?;
            return Ok(text_output(payload.to_string(), payload));
        }
        // Consume all handles before mutation. Even a failed/incomplete rollback
        // cannot leave another completion capable of trusting the old images.
        self.completions.clear();
        let expected = HashMap::from([(source.path.clone(), FileEvidence::Text(source.hash))]);
        let applied = apply_checked(&plan, &expected, || {
            budget.remaining()?;
            source.verify()?;
            budget.remaining()?;
            Ok(())
        });
        entry.client.invalidate_all();
        self.hierarchies.clear();
        let applied = applied?;
        let payload = json!({"action":"completion","applied":true,"label":selected.item["label"],
            "filesChanged":applied.outcome.files_changed.iter().map(|path| display_path(path, &self.cwd)).collect::<Vec<_>>(),
            "additionalEdits":edits.len().saturating_sub(1),"rollbackOnError":true,"commandExecuted":false});
        Ok(text_output(payload.to_string(), payload))
    }
}

#[cfg(test)]
mod tests;
