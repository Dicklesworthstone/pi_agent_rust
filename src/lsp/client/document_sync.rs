//! Stateful document synchronization using the server's negotiated policy.
//!
//! Publish a snapshot only after its complete notification batch is admitted.
//! Capacity waits release the document lock and revalidate both the source and
//! retained baselines before retrying. An indeterminate write still retires the
//! connection rather than reusing a stale incremental baseline.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};

use super::request::RequestBudget;
use super::{
    DEFAULT_REQUEST_TIMEOUT, JsonRpcClient, LspCallError, LspClient, OpenDoc, TransportError,
    WAIT_TICK, content_hash, try_path_to_uri,
};
use crate::error::{Error, Result};
use crate::lsp::jsonrpc::PreparedNotifications;

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

struct SyncPlan {
    basis: HashMap<String, OpenDoc>,
    uri: String,
    policy: SyncPolicy,
    document: OpenDoc,
    evicted: Vec<String>,
    notifications: PreparedNotifications,
}

impl SyncPlan {
    fn matches(
        &self,
        docs: &HashMap<String, OpenDoc>,
        uri: &str,
        language_id: &str,
        policy: SyncPolicy,
        text: &str,
    ) -> bool {
        self.uri == uri
            && self.policy == policy
            && self.document.language_id == language_id
            && self.document.text.as_ref() == text
            && self.basis.len() == docs.len()
            && self.basis.iter().all(|(uri, before)| {
                docs.get(uri).is_some_and(|now| {
                    before.version == now.version
                        && before.opened == now.opened
                        && before.language_id == now.language_id
                        && before.text == now.text
                })
            })
    }
}

enum SyncAttempt {
    Synced(String),
    Busy(TransportError),
}

fn sync_error(message: &str) -> Error {
    Error::tool("lsp", format!("[LSP_SYNC_UNSUPPORTED] {message}"))
}

fn refactor_sync_conflict() -> Error {
    Error::tool(
        "lsp",
        "[LSP_EDIT_CONFLICT] open document identities changed during synchronization; retry the refactor",
    )
}

fn same_open_documents(
    before: &HashMap<String, OpenDoc>,
    after: &HashMap<String, OpenDoc>,
) -> bool {
    before.len() == after.len()
        && before.iter().all(|(uri, expected)| {
            after.get(uri).is_some_and(|current| {
                expected.version == current.version
                    && expected.opened == current.opened
                    && expected.language_id == current.language_id
                    && Arc::ptr_eq(&expected.text, &current.text)
            })
        })
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
            return Err(sync_error(
                "server selected an unsupported position encoding",
            ));
        }
        let Some(sync) = capabilities.get("textDocumentSync") else {
            return Ok(Self::default());
        };
        if let Some(change) = sync.as_u64() {
            if change > 2 {
                return Err(sync_error("invalid textDocumentSync kind"));
            }
            return Ok(Self {
                change,
                open_close: change != 0,
                save: None,
            });
        }
        if !sync.is_object() {
            return Err(sync_error(
                "textDocumentSync must be a kind or an options object",
            ));
        }
        let change = match sync.get("change") {
            None => 0,
            Some(value) => value
                .as_u64()
                .filter(|value| *value <= 2)
                .ok_or_else(|| sync_error("invalid textDocumentSync change kind"))?,
        };
        let save = match sync.get("save") {
            None | Some(Value::Bool(false)) => None,
            Some(Value::Bool(true)) => Some(false),
            Some(options) if options.is_object() => Some(bool_option(options, "includeText")?),
            Some(_) => return Err(sync_error("invalid textDocumentSync save option")),
        };
        Ok(Self {
            change,
            open_close: bool_option(sync, "openClose")?,
            save,
        })
    }
}

fn splits_crlf(text: &str, offset: usize) -> bool {
    offset > 0
        && text.as_bytes().get(offset - 1) == Some(&b'\r')
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
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                line += 1;
                character = 0;
            }
            '\n' => {
                line += 1;
                character = 0;
            }
            _ => character += u32::try_from(ch.len_utf16()).unwrap_or(2),
        }
    }
    json!({"line": line, "character": character})
}

/// One exact replacement between the longest shared prefix and suffix. This
/// avoids whole-document retransmission for incremental servers, without an
/// unbounded diff algorithm or splitting a Unicode scalar / CRLF delimiter.
fn incremental_change(before: &str, after: &str) -> Value {
    let mut start = before
        .bytes()
        .zip(after.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while start > 0
        && (!before.is_char_boundary(start)
            || !after.is_char_boundary(start)
            || splits_crlf(before, start)
            || splits_crlf(after, start))
    {
        start -= 1;
    }
    let mut suffix = before[start..]
        .bytes()
        .rev()
        .zip(after[start..].bytes().rev())
        .take_while(|(a, b)| a == b)
        .count();
    while suffix > 0
        && (!before.is_char_boundary(before.len() - suffix)
            || !after.is_char_boundary(after.len() - suffix)
            || splits_crlf(before, before.len() - suffix)
            || splits_crlf(after, after.len() - suffix))
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
        return Err(Error::tool(
            "lsp",
            "[LSP_FILE_UNREADABLE] document is not a regular file",
        ));
    }
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::tool(
            "lsp",
            "[LSP_FILE_UNREADABLE] document is not a regular file",
        ));
    }
    if metadata.len() > MAX_DOCUMENT_BYTES as u64 {
        return Err(Error::tool(
            "lsp",
            "[LSP_DOCUMENT_LIMIT] source exceeds 16 MiB",
        ));
    }
    let mut text = String::new();
    file.take(MAX_DOCUMENT_BYTES as u64 + 1)
        .read_to_string(&mut text)?;
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err(Error::tool(
            "lsp",
            "[LSP_DOCUMENT_LIMIT] source exceeds 16 MiB",
        ));
    }
    Ok(text)
}

impl LspClient {
    fn next_version(&self) -> Result<u64> {
        self.next_document_version
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                if value <= MAX_DOCUMENT_VERSION {
                    Some(value + 1)
                } else {
                    None
                }
            })
            .map_err(|_| Error::tool("lsp", "[LSP_VERSION_EXHAUSTED] reload the language server"))
    }

    fn document_notify(
        &self,
        docs: &mut HashMap<String, OpenDoc>,
        method: &str,
        params: Value,
    ) -> Result<()> {
        let result = if self.rpc.is_alive() {
            self.rpc.notify(method, params)
        } else {
            Err(super::TransportError::Closed(
                "document transport is not alive".to_string(),
            ))
        };
        if let Err(error) = result {
            self.rpc.kill();
            docs.clear();
            Self::lock(&self.diagnostics).clear();
            return Err(Error::tool(
                "lsp",
                format!("[{}] {}", error.code(), error.message()),
            ));
        }
        Ok(())
    }

    fn document_evictions(
        docs: &HashMap<String, OpenDoc>,
        uri: &str,
        size: usize,
    ) -> Result<Vec<String>> {
        let mut candidates: Vec<_> = docs.iter().filter(|(key, _)| key.as_str() != uri).collect();
        candidates.sort_by(|(left_uri, left), (right_uri, right)| {
            (left.version, left_uri).cmp(&(right.version, right_uri))
        });
        let mut count = candidates.len();
        let mut bytes: usize = candidates.iter().map(|(_, doc)| doc.text.len()).sum();
        let mut evicted = Vec::new();
        for (key, doc) in candidates {
            if count < MAX_OPEN_DOCUMENTS && bytes + size <= MAX_OPEN_DOCUMENT_BYTES {
                return Ok(evicted);
            }
            evicted.push(key.clone());
            count -= 1;
            bytes -= doc.text.len();
        }
        if count < MAX_OPEN_DOCUMENTS && bytes + size <= MAX_OPEN_DOCUMENT_BYTES {
            Ok(evicted)
        } else {
            Err(Error::tool(
                "lsp",
                "[LSP_DOCUMENT_LIMIT] no document cache capacity",
            ))
        }
    }

    fn close_document(&self, docs: &mut HashMap<String, OpenDoc>, uri: &str) -> Result<()> {
        if docs.get(uri).is_some_and(|doc| doc.opened) {
            self.document_notify(
                docs,
                "textDocument/didClose",
                json!({"textDocument":{"uri":uri}}),
            )?;
        }
        docs.remove(uri);
        Self::lock(&self.diagnostics).remove(uri);
        Ok(())
    }

    fn plan_document_sync(
        &self,
        docs: &HashMap<String, OpenDoc>,
        uri: &str,
        language_id: &str,
        policy: SyncPolicy,
        text: String,
    ) -> Result<SyncPlan> {
        let prior = docs.get(uri);
        let reopen = prior.is_some_and(|doc| {
            doc.language_id != language_id || (policy.change == 0 && policy.open_close)
        });
        let opening = prior.is_none() || reopen;
        let version = if (opening && policy.open_close) || (!opening && policy.change != 0) {
            self.next_version()?
        } else {
            0 // A baseline never sent to the server is not a versioned snapshot.
        };
        let evicted = Self::document_evictions(docs, uri, text.len())?;
        let mut notifications = Vec::new();
        for closing in evicted
            .iter()
            .map(String::as_str)
            .chain(reopen.then_some(uri))
        {
            if docs.get(closing).is_some_and(|doc| doc.opened) {
                notifications.push((
                    "textDocument/didClose",
                    json!({"textDocument":{"uri":closing}}),
                ));
            }
        }
        if opening && policy.open_close {
            notifications.push((
                "textDocument/didOpen",
                json!({"textDocument":{
                    "uri":uri,"languageId":language_id,"version":version,"text":text
                }}),
            ));
        } else if !opening && policy.change != 0 {
            let change = if policy.change == 2 {
                incremental_change(&prior.expect("existing baseline").text, &text)
            } else {
                json!({"text":text})
            };
            notifications.push((
                "textDocument/didChange",
                json!({
                    "textDocument":{"uri":uri,"version":version}, "contentChanges":[change]
                }),
            ));
        }
        // A changed disk snapshot is already saved. Do not invent save events
        // on first open; include the text only when the server requested it.
        if prior.is_some_and(|doc| doc.text.as_ref() != text)
            && let Some(include_text) = policy.save
        {
            let mut params = json!({"textDocument":{"uri":uri}});
            if include_text {
                params["text"] = Value::String(text.clone());
            }
            notifications.push(("textDocument/didSave", params));
        }
        Ok(SyncPlan {
            basis: docs.clone(),
            uri: uri.to_string(),
            policy,
            document: OpenDoc {
                version,
                disk_hash: content_hash(&text),
                language_id: language_id.to_string(),
                text: Arc::from(text),
                opened: policy.open_close,
                push_generation: 0,
            },
            evicted,
            notifications: JsonRpcClient::prepare_notifications(notifications),
        })
    }

    // The diagnostics guard is taken under `open_docs` so that dropping
    // evicted or re-synced URIs from the cache is atomic with the insert.
    #[allow(clippy::significant_drop_tightening)]
    fn try_ensure_synced(
        &self,
        path: &Path,
        language_id: &str,
        budget: &RequestBudget,
        prepared: &mut Option<SyncPlan>,
        retained: Option<&mut HashMap<String, OpenDoc>>,
    ) -> Result<SyncAttempt> {
        budget.remaining()?;
        let canonical = path.canonicalize()?;
        let uri = try_path_to_uri(&canonical)?;
        let policy = SyncPolicy::parse(&Self::lock(&self.capabilities).raw)?;
        // This guard covers one bounded attempt, never a capacity wait. Both
        // the incremental baseline and planned evictions can change while an
        // async caller is waiting, so neither may be trusted without rechecking.
        let mut docs = Self::lock(&self.open_docs);
        if retained.as_ref().is_some_and(|expected| {
            !expected.contains_key(&uri) || !same_open_documents(expected, &docs)
        }) {
            return Err(refactor_sync_conflict());
        }
        if !self.rpc.is_alive() {
            docs.clear();
            Self::lock(&self.diagnostics).clear();
            return Err(Error::tool(
                "lsp",
                "[LSP_TRANSPORT_CLOSED] document transport is not alive",
            ));
        }
        let text = read_document(&canonical)?;
        budget.remaining()?;
        if docs
            .get(&uri)
            .is_some_and(|doc| doc.text.as_ref() == text && doc.language_id == language_id)
        {
            return Ok(SyncAttempt::Synced(uri));
        }
        if prepared
            .as_ref()
            .is_none_or(|plan| !plan.matches(&docs, &uri, language_id, policy, &text))
        {
            *prepared = Some(self.plan_document_sync(&docs, &uri, language_id, policy, text)?);
        }
        budget.remaining()?;
        let plan = prepared.as_mut().expect("prepared document transaction");
        if retained.is_some() && !plan.evicted.is_empty() {
            return Err(refactor_sync_conflict());
        }
        if let Err(error) = self.rpc.try_admit_notifications(&mut plan.notifications) {
            if matches!(error, TransportError::Backpressure(_)) {
                return Ok(SyncAttempt::Busy(error));
            }
            // A permanently rejected batch changed neither endpoint. Only a
            // failed transport invalidates the previous synchronized baseline.
            if !self.rpc.is_alive() {
                docs.clear();
                Self::lock(&self.diagnostics).clear();
            }
            return Err(LspCallError::Transport(error).into());
        }
        // Once admission succeeds, publish before checking cancellation again:
        // the server may receive this transaction even if its owner is dropped.
        let plan = prepared.take().expect("admitted document transaction");
        if let Some(retained) = retained {
            retained.insert(uri.clone(), plan.document.clone());
        }
        {
            let mut diagnostics = Self::lock(&self.diagnostics);
            for evicted in plan.evicted {
                docs.remove(&evicted);
                diagnostics.remove(&evicted);
            }
            diagnostics.remove(&uri);
            self.quiescent.store(false, Ordering::SeqCst);
            docs.insert(uri.clone(), plan.document);
        }
        drop(docs);
        budget.remaining()?;
        Ok(SyncAttempt::Synced(uri))
    }

    /// Attempt a complete document synchronization without waiting for queue
    /// capacity. Busy and oversized updates preserve the prior baseline and
    /// server; callers that can wait should use `ensure_synced_with_timeout`.
    pub fn ensure_synced(&self, path: &Path, language_id: &str) -> Result<String> {
        let budget = RequestBudget::new(DEFAULT_REQUEST_TIMEOUT);
        match self.try_ensure_synced(path, language_id, &budget, &mut None, None)? {
            SyncAttempt::Synced(uri) => Ok(uri),
            SyncAttempt::Busy(error) => Err(LspCallError::Transport(error).into()),
        }
    }

    /// Synchronize within one owner/deadline budget, including capacity waits.
    /// An unadmitted batch has no remote effects and can be dropped safely.
    pub async fn ensure_synced_with_timeout(
        &self,
        path: &Path,
        language_id: &str,
        timeout: Duration,
    ) -> Result<String> {
        let budget = RequestBudget::new(timeout);
        // Reuse the request admission lane so only one async caller retains
        // a source/batch while waiting, and queries cannot overtake its sync.
        // Immediate callers still revalidate through the document mutex.
        let _lane = budget.acquire(&self.request_lane).await?;
        let mut prepared = None;
        loop {
            match self.try_ensure_synced(path, language_id, &budget, &mut prepared, None)? {
                SyncAttempt::Synced(uri) => return Ok(uri),
                SyncAttempt::Busy(_) => budget.pause(WAIT_TICK).await?,
            }
        }
    }

    /// Refresh the retained working set before computing a fresh workspace
    /// rename. One lane and deadline cover all documents and capacity waits;
    /// an already reviewed transaction must never call this method.
    pub(in crate::lsp) async fn ensure_open_documents_synced(
        &self,
        timeout: Duration,
    ) -> Result<()> {
        let budget = RequestBudget::new(timeout);
        let _lane = budget.acquire(&self.request_lane).await?;
        let mut retained = Self::lock(&self.open_docs).clone();
        let mut documents: Vec<_> = retained
            .iter()
            .map(|(uri, document)| (uri.clone(), document.language_id.clone()))
            .collect();
        documents.sort_by(|left, right| left.0.cmp(&right.0));
        for (uri, language_id) in documents {
            let path = super::uri_to_path(&uri).ok_or_else(refactor_sync_conflict)?;
            let mut prepared = None;
            loop {
                budget.remaining()?;
                // A document deleted, replaced, or turned into a link since it
                // was opened is never read again. Nothing retires it before
                // the idle sweep, so refusing would fail every later rename;
                // close it and the server falls back to the disk.
                // Canonicalization is rechecked during synchronization; this
                // does not provide an OS sandbox against racing path swaps.
                let current = match std::fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                        path.canonicalize()
                            .ok()
                            .and_then(|canonical| try_path_to_uri(&canonical).ok())
                    }
                    Ok(_) => None,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                    Err(err) => return Err(err.into()),
                };
                if current.as_deref() != Some(uri.as_str()) {
                    self.close_document(&mut Self::lock(&self.open_docs), &uri)?;
                    retained.remove(&uri);
                    break;
                }
                match self.try_ensure_synced(
                    &path,
                    &language_id,
                    &budget,
                    &mut prepared,
                    Some(&mut retained),
                )? {
                    SyncAttempt::Synced(current) if current == uri => break,
                    SyncAttempt::Synced(_) => return Err(refactor_sync_conflict()),
                    SyncAttempt::Busy(_) => budget.pause(WAIT_TICK).await?,
                }
            }
        }
        budget.remaining()?;
        if !same_open_documents(&retained, &Self::lock(&self.open_docs)) {
            return Err(refactor_sync_conflict());
        }
        Ok(())
    }

    pub fn invalidate(&self, uri: &str) {
        let Some(uri) = super::file_uri::normalize_uri(uri) else {
            return;
        };
        let mut docs = Self::lock(&self.open_docs);
        let _ = self.close_document(&mut docs, &uri);
    }

    pub fn invalidate_all(&self) {
        let mut docs = Self::lock(&self.open_docs);
        let uris: Vec<_> = docs.keys().cloned().collect();
        for uri in uris {
            if self.close_document(&mut docs, &uri).is_err() {
                break;
            }
        }
        drop(docs);
        Self::lock(&self.diagnostics).clear();
    }

    // `open_docs` stays locked from the version check to the insert, so a
    // notification cannot pass the check and then publish stale diagnostics.
    #[allow(clippy::significant_drop_tightening)]
    pub(super) fn accept_diagnostics(&self, params: &Value) {
        let (Some(uri), Some(diagnostics)) = (
            params.get("uri").and_then(Value::as_str),
            params.get("diagnostics").and_then(Value::as_array),
        ) else {
            return;
        };
        let Some(uri) = super::file_uri::normalize_uri(uri) else {
            return;
        };
        // Same lock order as synchronization: a notification cannot pass the
        // version check, wait for a change, then publish stale diagnostics.
        let mut docs = Self::lock(&self.open_docs);
        if let Some(version) = params.get("version") {
            let Some(version) = version
                .as_u64()
                .filter(|value| *value > 0 && *value <= MAX_DOCUMENT_VERSION)
            else {
                return;
            };
            if docs.get(&uri).is_none_or(|doc| doc.version != version) {
                return;
            }
        }
        if let Some(doc) = docs.get_mut(&uri) {
            doc.push_generation = doc.push_generation.saturating_add(1);
        }
        Self::lock(&self.diagnostics).insert(uri, diagnostics.clone());
    }
}

#[cfg(test)]
mod tests;
