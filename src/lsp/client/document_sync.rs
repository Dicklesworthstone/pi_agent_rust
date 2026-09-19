//! Stateful document synchronization using the server's negotiated policy.
//!
//! Serialize local snapshots and notifications under the document lock. Never
//! publish a version until its notifications have been written successfully;
//! an indeterminate write retires the connection rather than reusing a stale
//! incremental baseline. The retained source texts have a bounded working set.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::{Value, json};

use super::{LspClient, OpenDoc, content_hash, try_path_to_uri};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};

const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_OPEN_DOCUMENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_OPEN_DOCUMENTS: usize = 128;
const MAX_DOCUMENT_VERSION: u64 = i32::MAX as u64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct SyncPolicy {
    pub(super) change: u64,
    open_close: bool,
    // None = no save notification; Some(true) includes the saved text.
    save: Option<bool>,
}

fn sync_error(message: &str) -> Error {
    Error::tool("lsp", format!("[LSP_SYNC_UNSUPPORTED] {message}"))
}

fn bool_option(value: &Value, key: &str) -> Result<bool> {
    match value.get(key) {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(sync_error("invalid textDocumentSync boolean option")),
    }
}

impl SyncPolicy {
    pub(super) fn parse(capabilities: &Value) -> Result<Self> {
        // Every position-producing/consuming LSP path in Pi uses UTF-16.
        // An incompatible choice must fail before any document is sent.
        if let Some(encoding) = capabilities.get("positionEncoding")
            && encoding.as_str() != Some("utf-16")
        {
            return Err(sync_error("server selected an unsupported position encoding"));
        }
        let Some(sync) = capabilities.get("textDocumentSync") else {
            return Ok(Self::default());
        };
        if let Some(change) = sync.as_u64() {
            if change > 2 {
                return Err(sync_error("invalid textDocumentSync kind"));
            }
            return Ok(Self { change, open_close: change != 0, save: None });
        }
        if !sync.is_object() {
            return Err(sync_error("textDocumentSync must be a kind or an options object"));
        }
        let change = match sync.get("change") {
            None => 0,
            Some(value) => value.as_u64().filter(|value| *value <= 2)
                .ok_or_else(|| sync_error("invalid textDocumentSync change kind"))?,
        };
        let save = match sync.get("save") {
            None | Some(Value::Bool(false)) => None,
            Some(Value::Bool(true)) => Some(false),
            Some(options) if options.is_object() => Some(bool_option(options, "includeText")?),
            Some(_) => return Err(sync_error("invalid textDocumentSync save option")),
        };
        Ok(Self { change, open_close: bool_option(sync, "openClose")?, save })
    }
}

fn splits_crlf(text: &str, offset: usize) -> bool {
    offset > 0 && text.as_bytes().get(offset - 1) == Some(&b'\r')
        && text.as_bytes().get(offset) == Some(&b'\n')
}

/// UTF-16 position at an already validated byte boundary. CR, LF and CRLF
/// each represent one line ending. Callers never supply the middle of CRLF.
fn position(text: &str, offset: usize) -> Value {
    let mut line = 0_u32;
    let mut character = 0_u32;
    let mut chars = text[..offset].chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') { chars.next(); }
                line += 1;
                character = 0;
            }
            '\n' => { line += 1; character = 0; }
            _ => character += u32::try_from(ch.len_utf16()).unwrap_or(2),
        }
    }
    json!({"line": line, "character": character})
}

/// One exact replacement between the longest shared prefix and suffix. This
/// avoids whole-document retransmission for incremental servers, without an
/// unbounded diff algorithm or splitting a Unicode scalar / CRLF delimiter.
fn incremental_change(before: &str, after: &str) -> Value {
    let mut start = before.bytes().zip(after.bytes()).take_while(|(a, b)| a == b).count();
    while start > 0 && (!before.is_char_boundary(start) || !after.is_char_boundary(start)
        || splits_crlf(before, start) || splits_crlf(after, start))
    {
        start -= 1;
    }
    let mut suffix = before[start..].bytes().rev().zip(after[start..].bytes().rev())
        .take_while(|(a, b)| a == b).count();
    while suffix > 0 && (!before.is_char_boundary(before.len() - suffix)
        || !after.is_char_boundary(after.len() - suffix)
        || splits_crlf(before, before.len() - suffix) || splits_crlf(after, after.len() - suffix))
    {
        suffix -= 1;
    }
    json!({
        "range": {"start": position(before, start), "end": position(before, before.len() - suffix)},
        "text": &after[start..after.len() - suffix]
    })
}

fn read_document(path: &Path) -> Result<String> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(Error::tool("lsp", "[LSP_FILE_UNREADABLE] document is not a regular file"));
    }
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::tool("lsp", "[LSP_FILE_UNREADABLE] document is not a regular file"));
    }
    if metadata.len() > MAX_DOCUMENT_BYTES as u64 {
        return Err(Error::tool("lsp", "[LSP_DOCUMENT_LIMIT] source exceeds 16 MiB"));
    }
    let mut text = String::new();
    file.take(MAX_DOCUMENT_BYTES as u64 + 1).read_to_string(&mut text)?;
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err(Error::tool("lsp", "[LSP_DOCUMENT_LIMIT] source exceeds 16 MiB"));
    }
    Ok(text)
}

impl LspClient {
    fn next_version(&self) -> Result<u64> {
        self.next_document_version.try_update(Ordering::SeqCst, Ordering::SeqCst,
            |value| if value <= MAX_DOCUMENT_VERSION { Some(value + 1) } else { None })
            .map_err(|_| Error::tool("lsp", "[LSP_VERSION_EXHAUSTED] reload the language server"))
    }

    fn document_notify(&self, docs: &mut HashMap<String, OpenDoc>, method: &str, params: Value) -> Result<()> {
        let result = if self.rpc.is_alive() {
            self.rpc.notify(method, params)
        } else {
            Err(super::TransportError::Closed("document transport is not alive".to_string()))
        };
        if let Err(error) = result {
            self.rpc.kill();
            docs.clear();
            Self::lock(&self.diagnostics).clear();
            return Err(Error::tool("lsp", format!("[{}] {}", error.code(), error.message())));
        }
        Ok(())
    }

    fn reserve_document(&self, docs: &mut HashMap<String, OpenDoc>, uri: &str, size: usize) -> Result<()> {
        loop {
            let (count, bytes) = docs.iter().filter(|(key, _)| key.as_str() != uri)
                .fold((0, 0_usize), |(count, bytes), (_, doc)| (count + 1, bytes + doc.text.len()));
            if count < MAX_OPEN_DOCUMENTS && bytes + size <= MAX_OPEN_DOCUMENT_BYTES {
                return Ok(());
            }
            let oldest = docs.iter().filter(|(key, _)| key.as_str() != uri)
                .min_by_key(|(key, doc)| (doc.version, *key)).map(|(key, _)| key.clone())
                .ok_or_else(|| Error::tool("lsp", "[LSP_DOCUMENT_LIMIT] no document cache capacity"))?;
            self.close_document(docs, &oldest)?;
        }
    }

    fn close_document(&self, docs: &mut HashMap<String, OpenDoc>, uri: &str) -> Result<()> {
        if docs.get(uri).is_some_and(|doc| doc.opened) {
            self.document_notify(docs, "textDocument/didClose", json!({"textDocument":{"uri":uri}}))?;
        }
        docs.remove(uri);
        Self::lock(&self.diagnostics).remove(uri);
        Ok(())
    }

    /// Synchronize a bounded source snapshot using the negotiated change mode.
    /// Unchanged text is a no-op; language changes reopen the document. Only
    /// servers without change support need the close/open fallback.
    pub fn ensure_synced(&self, path: &Path, language_id: &str) -> Result<String> {
        let owner = AgentCx::for_current_or_request();
        owner.checkpoint().map_err(|_| Error::from(super::LspCallError::Cancelled))?;
        let canonical = path.canonicalize()?;
        let uri = try_path_to_uri(&canonical)?;
        let policy = SyncPolicy::parse(&Self::lock(&self.capabilities).raw)?;
        // Keep the read, baseline comparison and wire order serialized. No
        // lock survives an await. Other files may still change externally.
        let mut docs = Self::lock(&self.open_docs);
        if !self.rpc.is_alive() {
            docs.clear();
            Self::lock(&self.diagnostics).clear();
            return Err(Error::tool("lsp", "[LSP_TRANSPORT_CLOSED] document transport is not alive"));
        }
        let text = read_document(&canonical)?;
        if docs.get(&uri).is_some_and(|doc| doc.text.as_ref() == text && doc.language_id == language_id) {
            return Ok(uri);
        }
        owner.checkpoint().map_err(|_| Error::from(super::LspCallError::Cancelled))?;
        let prior = docs.get(&uri).cloned();
        let reopen = prior.as_ref().is_some_and(|doc| doc.language_id != language_id
            || (policy.change == 0 && policy.open_close));
        let opening = prior.is_none() || reopen;
        let version = if (opening && policy.open_close) || (!opening && policy.change != 0) {
            self.next_version()?
        } else {
            0 // A baseline never sent to the server is not a versioned snapshot.
        };
        self.reserve_document(&mut docs, &uri, text.len())?;
        Self::lock(&self.diagnostics).remove(&uri);
        self.quiescent.store(false, Ordering::SeqCst);
        if reopen { self.close_document(&mut docs, &uri)?; }
        if opening && policy.open_close {
            self.document_notify(&mut docs, "textDocument/didOpen", json!({"textDocument":{
                "uri":uri,"languageId":language_id,"version":version,"text":text
            }}))?;
        } else if !opening && policy.change != 0 {
            let change = if policy.change == 2 {
                incremental_change(&prior.as_ref().expect("existing baseline").text, &text)
            } else {
                json!({"text":text})
            };
            self.document_notify(&mut docs, "textDocument/didChange", json!({
                "textDocument":{"uri":uri,"version":version}, "contentChanges":[change]
            }))?;
        }
        // A changed disk snapshot is already saved. Do not invent save events
        // on first open; include the text only when the server requested it.
        if prior.as_ref().is_some_and(|doc| doc.text.as_ref() != text)
            && let Some(include_text) = policy.save
        {
            let mut params = json!({"textDocument":{"uri":uri}});
            if include_text { params["text"] = Value::String(text.clone()); }
            self.document_notify(&mut docs, "textDocument/didSave", params)?;
        }
        docs.insert(uri.clone(), OpenDoc {
            version, disk_hash: content_hash(&text), language_id: language_id.to_string(),
            text: Arc::from(text), opened: policy.open_close,
        });
        Ok(uri)
    }

    pub fn invalidate(&self, uri: &str) {
        let Some(uri) = super::file_uri::normalize_uri(uri) else { return };
        let mut docs = Self::lock(&self.open_docs);
        let _ = self.close_document(&mut docs, &uri);
    }

    pub fn invalidate_all(&self) {
        let mut docs = Self::lock(&self.open_docs);
        let uris: Vec<_> = docs.keys().cloned().collect();
        for uri in uris {
            if self.close_document(&mut docs, &uri).is_err() { break; }
        }
        Self::lock(&self.diagnostics).clear();
    }

    pub(super) fn accept_diagnostics(&self, params: &Value) {
        let (Some(uri), Some(diagnostics)) = (params.get("uri").and_then(Value::as_str),
            params.get("diagnostics").and_then(Value::as_array)) else { return };
        let Some(uri) = super::file_uri::normalize_uri(uri) else { return };
        // Same lock order as synchronization: a notification cannot pass the
        // version check, wait for a change, then publish stale diagnostics.
        let docs = Self::lock(&self.open_docs);
        if let Some(version) = params.get("version") {
            let Some(version) = version.as_u64().filter(|value| *value > 0 && *value <= MAX_DOCUMENT_VERSION) else { return };
            if docs.get(&uri).is_none_or(|doc| doc.version != version) { return; }
        }
        Self::lock(&self.diagnostics).insert(uri, diagnostics.clone());
    }
}

#[cfg(test)]
mod tests;
