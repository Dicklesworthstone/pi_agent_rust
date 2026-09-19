//! One-hop call-graph exploration with server-owned item identities.
//!
//! Preparation resolves a position once; subsequent hops send the exact item
//! (including opaque data) back to the same live server. Handles retain a
//! bounded source incarnation and expire instead of reopening a different one.
//! Returned resource URIs are labels, never instructions to read or fetch them.

use std::collections::{HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::registry::ServerEntry;
use super::text::Range;
use super::{LspInput, LspTool, display_path, resolve_tool_path, text_output, tool_err, uri_to_path};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ITEM_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_ITEMS: usize = 1024;
const MAX_RETURNED_ITEMS: usize = 128;
const MAX_CALL_SITES: usize = 16_384;
const MAX_SHOWN_CALL_SITES: usize = 64;
const MAX_CACHE_ITEMS: usize = 256;
const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;
const HANDLE_TTL: Duration = Duration::from_secs(120);
const ID_PLACEHOLDER: &str = "00000000-0000-0000-0000-000000000000";

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn malformed(message: &str) -> Error {
    tool_err("LSP_HIERARCHY_PROTOCOL", message)
}

fn expired() -> Error {
    tool_err("LSP_HIERARCHY_EXPIRED", "hierarchy source or server changed, or handle expired; start again with file and symbol")
}

struct ByteLimit(usize);

impl Write for ByteLimit {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("hierarchy byte limit exceeded"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn encoded_size(value: &Value, limit: usize) -> Result<usize> {
    let mut remaining = ByteLimit(limit);
    serde_json::to_writer(&mut remaining, value)
        .map_err(|_| tool_err("LSP_HIERARCHY_LIMIT", "hierarchy response or item exceeds its byte limit"))?;
    Ok(limit - remaining.0)
}

fn range(value: &Value) -> Result<Range> {
    let range: Range = serde_json::from_value(value.clone())
        .map_err(|_| malformed("invalid hierarchy range"))?;
    if range.end < range.start || [range.start, range.end].iter().any(|position| {
        position.line > i32::MAX as u32 || position.character > i32::MAX as u32
    }) {
        return Err(malformed("hierarchy ranges must be ordered LSP positions"));
    }
    Ok(range)
}

#[derive(Clone)]
struct Item {
    raw: Arc<Value>,
    bytes: usize,
    range: Range,
    selection: Range,
}

impl Item {
    fn parse(raw: &Value) -> Result<Self> {
        let bytes = encoded_size(raw, MAX_ITEM_BYTES)?;
        if raw.get("name").and_then(Value::as_str).is_none_or(|name| name.is_empty() || name.len() > 4096)
            || raw.get("uri").and_then(Value::as_str).is_none_or(|uri| {
                uri.is_empty() || uri.len() > 8192 || uri.chars().any(char::is_control)
                    || url::Url::parse(uri).is_err()
            })
            || !raw.get("kind").and_then(Value::as_u64).is_some_and(|kind| (1..=26).contains(&kind))
        {
            return Err(malformed("hierarchy item needs a bounded name, URI and symbol kind"));
        }
        if let Some(detail) = raw.get("detail")
            && detail.as_str().is_none_or(|detail| detail.len() > 8192)
        {
            return Err(malformed("hierarchy detail must be a bounded string"));
        }
        if let Some(tags) = raw.get("tags")
            && tags.as_array().is_none_or(|tags| tags.len() > 16 || tags.iter().any(|tag| tag.as_u64() != Some(1)))
        {
            return Err(malformed("invalid hierarchy symbol tags"));
        }
        let outer = range(&raw["range"])?;
        let selection = range(&raw["selectionRange"])?;
        if selection.start < outer.start || selection.end > outer.end {
            return Err(malformed("hierarchy selectionRange is outside its enclosing range"));
        }
        Ok(Self { raw: Arc::new(raw.clone()), bytes, range: outer, selection })
    }

    fn summary(&self, cwd: &std::path::Path) -> Value {
        let raw = self.raw.as_ref();
        let name = raw["name"].as_str().expect("validated name");
        let shown_name: String = name.chars().take(256).collect();
        let uri = raw["uri"].as_str().expect("validated URI");
        let file = uri_to_path(uri).map_or_else(|| uri.to_string(), |path| display_path(&path, cwd));
        let mut summary = json!({
            "hierarchyId":ID_PLACEHOLDER,"name":shown_name,"nameTruncated":shown_name.len()!=name.len(),
            "kind":raw["kind"],"uri":uri,"file":file,"range":self.range,
            "selectionRange":self.selection,"tags":raw.get("tags")
        });
        if let Some(detail) = raw.get("detail").and_then(Value::as_str) {
            let shown: String = detail.chars().take(512).collect();
            summary["detailTruncated"] = json!(shown.len() != detail.len());
            summary["detail"] = json!(shown);
        }
        // Opaque data and unknown server fields stay in the cache, not in the
        // model-visible summary. They are preserved exactly on the next hop.
        summary
    }
}

fn response_items(raw: &Value) -> Result<&[Value]> {
    encoded_size(raw, MAX_RESPONSE_BYTES)?;
    match raw {
        Value::Null => Ok(&[]),
        Value::Array(items) if items.len() <= MAX_RESPONSE_ITEMS => Ok(items),
        Value::Array(_) => Err(tool_err("LSP_HIERARCHY_LIMIT", "too many hierarchy items")),
        _ => Err(malformed("hierarchy result must be an array or null")),
    }
}

struct Origin {
    entry: Weak<ServerEntry>,
    path: std::path::PathBuf,
    uri: String,
    text: Arc<str>,
    created: Instant,
}

impl Origin {
    fn check(&self, entry: &Arc<ServerEntry>, owner: &AgentCx) -> Result<()> {
        owner.checkpoint().map_err(|_| tool_err("LSP_CANCELLED", "hierarchy request cancelled"))?;
        if !owner.capabilities().io {
            return Err(tool_err("LSP_HIERARCHY_AUTHORITY", "hierarchy source verification requires I/O capability"));
        }
        if self.created.elapsed() >= HANDLE_TTL || !entry.client.is_alive()
            || !Weak::ptr_eq(&self.entry, &Arc::downgrade(entry))
            || !entry.client.synchronized_text(&self.uri).is_some_and(|text| Arc::ptr_eq(&text, &self.text))
        {
            return Err(expired());
        }
        // Compare exact source bytes without retaining another whole source.
        // This detects observed external edits, not an atomic filesystem snapshot.
        let metadata = std::fs::symlink_metadata(&self.path).map_err(|_| expired())?;
        if !metadata.is_file() || metadata.len() != self.text.len() as u64 {
            return Err(expired());
        }
        let mut file = std::fs::File::open(&self.path).map_err(|_| expired())?;
        if !file.metadata().map_err(|_| expired())?.is_file() {
            return Err(expired());
        }
        let mut offset = 0usize;
        let mut buffer = [0u8; 8192];
        loop {
            owner.checkpoint().map_err(|_| tool_err("LSP_CANCELLED", "hierarchy source verification cancelled"))?;
            let count = file.read(&mut buffer).map_err(|_| expired())?;
            if count == 0 { break; }
            if self.text.as_bytes().get(offset..offset + count) != Some(&buffer[..count]) {
                return Err(expired());
            }
            offset += count;
        }
        if offset != self.text.len() { return Err(expired()); }
        Ok(())
    }
}

#[derive(Clone)]
struct CachedItem {
    id: String,
    origin: Arc<Origin>,
    item: Item,
}

#[derive(Default)]
pub(super) struct HierarchyCache {
    entries: Mutex<VecDeque<CachedItem>>,
}

impl HierarchyCache {
    pub(super) fn clear(&self) { lock(&self.entries).clear(); }

    fn get(&self, id: &str) -> Result<CachedItem> {
        let mut entries = lock(&self.entries);
        entries.retain(|cached| cached.origin.created.elapsed() < HANDLE_TTL && cached.origin.entry.strong_count() > 0);
        entries.iter().find(|cached| cached.id == id).cloned().ok_or_else(expired)
    }

    fn retain(&self, origin: &Arc<Origin>, items: Vec<Item>) -> Result<Vec<String>> {
        let new_bytes: usize = items.iter().map(|item| item.bytes).sum();
        if items.len() > MAX_CACHE_ITEMS || new_bytes.saturating_add(origin.text.len()) > MAX_CACHE_BYTES {
            return Err(tool_err("LSP_HIERARCHY_LIMIT", "hierarchy traversal exceeds the retained working set"));
        }
        let mut entries = lock(&self.entries);
        entries.retain(|cached| cached.origin.created.elapsed() < HANDLE_TTL && cached.origin.entry.strong_count() > 0);
        // Charge each shared source once, including sources whose server has
        // closed them. An Arc retained by a handle still owns those source bytes.
        while entries.len() + items.len() > MAX_CACHE_ITEMS
            || retained_bytes(&entries, origin).saturating_add(new_bytes) > MAX_CACHE_BYTES
        {
            entries.pop_front();
        }
        let mut ids = Vec::with_capacity(items.len());
        for item in items {
            let id = uuid::Uuid::new_v4().to_string();
            ids.push(id.clone());
            entries.push_back(CachedItem { id, origin: Arc::clone(origin), item });
        }
        Ok(ids)
    }
}

fn retained_bytes(entries: &VecDeque<CachedItem>, incoming: &Arc<Origin>) -> usize {
    let mut origins = HashSet::new();
    origins.insert(Arc::as_ptr(incoming));
    let mut bytes = incoming.text.len();
    for entry in entries {
        bytes = bytes.saturating_add(entry.item.bytes);
        if origins.insert(Arc::as_ptr(&entry.origin)) {
            bytes = bytes.saturating_add(entry.origin.text.len());
        }
    }
    bytes
}

#[derive(Clone, Copy)]
enum Direction { Incoming, Outgoing }

impl Direction {
    fn from_action(action: &str) -> Result<Self> {
        match action {
            "incoming_calls" => Ok(Self::Incoming),
            "outgoing_calls" => Ok(Self::Outgoing),
            _ => Err(tool_err("LSP_USAGE", "unknown hierarchy action")),
        }
    }

    const fn method(self) -> &'static str {
        match self {
            Self::Incoming => "callHierarchy/incomingCalls",
            Self::Outgoing => "callHierarchy/outgoingCalls",
        }
    }

    fn capability(self, capabilities: &Value) -> Result<()> {
        match capabilities.get("callHierarchyProvider") {
            Some(Value::Bool(true) | Value::Object(_)) => Ok(()),
            None | Some(Value::Bool(false)) => Err(tool_err("LSP_HIERARCHY_UNSUPPORTED", "server did not advertise call hierarchy support")),
            _ => Err(malformed("invalid call hierarchy capability")),
        }
    }
}

struct Related {
    item: Item,
    site_uri: String,
    sites: Vec<Range>,
}

fn related(raw: &Value, selected: &Item, direction: Direction) -> Result<Vec<Related>> {
    let mut out = Vec::new();
    let mut site_count = 0usize;
    for call in response_items(raw)? {
        let key = match direction { Direction::Incoming => "from", Direction::Outgoing => "to" };
        let item = Item::parse(&call[key])?;
        let sites = call.get("fromRanges").and_then(Value::as_array)
            .ok_or_else(|| malformed("call hierarchy entry needs fromRanges"))?;
        site_count = site_count.saturating_add(sites.len());
        if site_count > MAX_CALL_SITES {
            return Err(tool_err("LSP_HIERARCHY_LIMIT", "too many call-site ranges"));
        }
        let sites = sites.iter().map(range).collect::<Result<Vec<_>>>()?;
        let caller = match direction { Direction::Incoming => &item, Direction::Outgoing => selected };
        let site_uri = caller.raw["uri"].as_str().expect("validated URI").to_string();
        out.push(Related { item, site_uri, sites });
    }
    Ok(out)
}

struct Budget {
    owner: AgentCx,
    start: Instant,
    timeout: Duration,
}

impl Budget {
    fn remaining(&self) -> Result<Duration> {
        self.owner.checkpoint().map_err(|_| tool_err("LSP_CANCELLED", "hierarchy request cancelled"))?;
        let remaining = self.timeout.saturating_sub(self.start.elapsed());
        if remaining.is_zero() {
            Err(tool_err("LSP_TIMEOUT", "hierarchy workflow exhausted its request budget"))
        } else { Ok(remaining) }
    }
}

impl LspTool {
    pub(super) async fn run_hierarchy(&self, input: &LspInput) -> Result<ToolOutput> {
        let direction = Direction::from_action(&input.action)?;
        let limit = input.limit.unwrap_or(super::DEFAULT_LOCATION_LIMIT).min(MAX_RETURNED_ITEMS);
        if limit == 0 || input.apply == Some(true) || input.payload.is_some() || input.action_id.is_some() {
            return Err(tool_err("LSP_USAGE", "hierarchy queries are read-only, require a positive limit and accept no raw payload or actionId"));
        }
        let budget = Budget {
            owner: AgentCx::for_current_or_request(), start: Instant::now(), timeout: self.request_timeout(input),
        };
        budget.remaining()?;
        if !budget.owner.capabilities().io {
            return Err(tool_err("LSP_HIERARCHY_AUTHORITY", "hierarchy queries require I/O capability"));
        }
        let (entry, origin, selected) = if let Some(id) = input.hierarchy_id.as_deref() {
            if input.file.is_some() || input.line.is_some() || input.symbol.is_some() {
                return Err(tool_err("LSP_USAGE", "use hierarchyId instead of file, line and symbol"));
            }
            let cached = self.hierarchies.get(id)?;
            let entry = cached.origin.entry.upgrade().ok_or_else(expired)?;
            (entry, cached.origin, cached.item)
        } else {
            let file = input.file.as_deref().filter(|file| !file.is_empty())
                .ok_or_else(|| tool_err("LSP_USAGE", "hierarchy query requires file and symbol, or hierarchyId"))?;
            let symbol = input.symbol.as_deref().ok_or_else(|| tool_err("LSP_USAGE", "hierarchy query requires symbol"))?;
            let path = resolve_tool_path(file, &self.cwd).canonicalize()?;
            let (uri, entry) = self.synced(&path).await?;
            direction.capability(&entry.client.capabilities().raw)?;
            let text = entry.client.synchronized_text(&uri).ok_or_else(expired)?;
            let position = Self::resolve_position_in(&path, &text, input.line, symbol)?;
            let origin = Arc::new(Origin { entry: Arc::downgrade(&entry), path, uri, text, created: Instant::now() });
            origin.check(&entry, &budget.owner)?;
            let raw = entry.client.call(
                "textDocument/prepareCallHierarchy", json!({"textDocument":{"uri":origin.uri},"position":position}), budget.remaining()?,
            ).await?;
            origin.check(&entry, &budget.owner)?;
            let mut items = response_items(&raw)?.iter().map(Item::parse).collect::<Result<Vec<_>>>()?;
            if items.len() != 1 {
                let total = items.len();
                let rows = items.into_iter().map(|item| {
                    let row = item.summary(&self.cwd);
                    (item, row)
                }).collect();
                budget.remaining()?;
                let output = self.hierarchy_output(input, &entry, &origin, None, rows, total, total > 1, limit)?;
                budget.remaining()?;
                return Ok(output);
            }
            (entry, origin, items.pop().expect("one prepared item"))
        };
        direction.capability(&entry.client.capabilities().raw)?;
        origin.check(&entry, &budget.owner)?;
        let raw = entry.client.call(direction.method(), json!({"item":selected.raw.as_ref()}), budget.remaining()?).await?;
        origin.check(&entry, &budget.owner)?;
        // Validate the entire result, including any tail omitted by the display
        // limit. A malformed tail is not a successfully completed empty graph.
        let related = related(&raw, &selected, direction)?;
        let total = related.len();
        let rows = related.into_iter().map(|related| {
            let mut row = related.item.summary(&self.cwd);
            row["callSiteUri"] = json!(related.site_uri);
            row["fromRanges"] = json!(related.sites.iter().take(MAX_SHOWN_CALL_SITES).collect::<Vec<_>>());
            row["callSiteCount"] = json!(related.sites.len());
            row["callSitesTruncated"] = json!(related.sites.len() > MAX_SHOWN_CALL_SITES);
            (related.item, row)
        }).collect();
        budget.remaining()?;
        let output = self.hierarchy_output(input, &entry, &origin, Some(selected), rows, total, false, limit)?;
        budget.remaining()?;
        Ok(output)
    }

    #[allow(clippy::too_many_arguments)]
    fn hierarchy_output(
        &self, input: &LspInput, entry: &Arc<ServerEntry>, origin: &Arc<Origin>,
        source: Option<Item>, rows: Vec<(Item, Value)>, total: usize, selection_required: bool, limit: usize,
    ) -> Result<ToolOutput> {
        let mut payload = json!({
            "action":input.action,"server":entry.spec_name,"source":source.as_ref().map(|item| item.summary(&self.cwd)),
            "selectionRequired":selection_required,"count":0,"total":total,"truncated":false,"items":[],
            "rangeEncoding":"zero-based UTF-16",
            "note":"One server-reported hop, not a whole-program graph. Reuse hierarchyId to traverse; source changes and server restarts expire handles."
        });
        let mut bytes = encoded_size(&payload, super::MAX_PAYLOAD_BYTES)?.saturating_add(64);
        let has_source = source.is_some();
        let mut retained: Vec<_> = source.into_iter().collect();
        let mut shown = Vec::new();
        for (item, row) in rows.into_iter().take(limit) {
            let size = encoded_size(&row, super::MAX_PAYLOAD_BYTES)?.saturating_add(1);
            if size > super::MAX_PAYLOAD_BYTES.saturating_sub(bytes) { break; }
            bytes += size;
            retained.push(item);
            shown.push(row);
        }
        let mut ids = self.hierarchies.retain(origin, retained)?.into_iter();
        if has_source { payload["source"]["hierarchyId"] = json!(ids.next().expect("source ID")); }
        for row in &mut shown { row["hierarchyId"] = json!(ids.next().expect("item ID")); }
        payload["count"] = json!(shown.len());
        payload["truncated"] = json!(shown.len() != total);
        payload["items"] = json!(shown);
        Ok(text_output(payload.to_string(), payload))
    }
}

#[cfg(test)]
mod tests;
