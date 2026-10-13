//! Connection-local workspace settings and server-to-client workspace requests.
//!
//! Only explicitly supplied language-server settings are exposed. This module
//! never reads agent configuration, environment variables, or scopeUri targets.
//! Responses use one immutable snapshot, even when a concurrent update arrives.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::{LspClient, uri_to_path};
use super::request::RequestBudget;
use crate::agent_cx::AgentCx;
use crate::error::Result;
use crate::lsp::jsonrpc::{RpcErrorObject, TransportError};
use crate::lsp::tool_err;

const MAX_SETTINGS_BYTES: usize = 64 * 1024;
const MAX_REPLY_BYTES: usize = 256 * 1024;
const MAX_ITEMS: usize = 128;
const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 8192;
const MAX_URI_BYTES: usize = 8192;

type Reply = std::result::Result<Value, RpcErrorObject>;

fn invalid(message: &str) -> RpcErrorObject {
    RpcErrorObject { code: -32602, message: message.into(), data: None }
}

fn bounded(value: &impl serde::Serialize, limit: usize) -> bool {
    struct Limit(usize);
    impl Write for Limit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("workspace configuration byte limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    serde_json::to_writer(&mut Limit(limit), value).is_ok()
}

/// Check recursion before serialization, including Values assembled by Rust
/// callers rather than the JSON parser. No unbounded auxiliary node vector.
fn shape(value: &Value, depth: usize, nodes: &mut usize) -> bool {
    if depth > MAX_DEPTH || *nodes == MAX_NODES { return false; }
    *nodes += 1;
    match value {
        Value::Array(values) => values.iter().all(|value| shape(value, depth + 1, nodes)),
        Value::Object(values) => values.values().all(|value| shape(value, depth + 1, nodes)),
        _ => true,
    }
}

/// Validation precedes server startup or replacement of a previous snapshot.
#[derive(Clone)]
pub(in crate::lsp) struct Settings(Arc<Value>);

impl Settings {
    pub(in crate::lsp) fn parse(value: &Value) -> Result<Self> {
        if !value.is_object() || !shape(value, 0, &mut 0)
            || !bounded(value, MAX_SETTINGS_BYTES)
        {
            return Err(tool_err("LSP_CONFIGURATION_INVALID",
                "settings must be an object within 64 KiB, 32 levels and 8192 values"));
        }
        Ok(Self(Arc::new(value.clone())))
    }

    pub(in crate::lsp) fn value(&self) -> &Value { &self.0 }
}

struct State {
    settings: Arc<Value>,
    revision: u64,
}

pub(super) struct Workspace {
    root: PathBuf,
    folders: Value,
    state: Mutex<State>,
}

impl Workspace {
    pub(super) fn new(root_uri: &str) -> Result<Self> {
        let root = uri_to_path(root_uri).filter(|_| root_uri.len() <= MAX_URI_BYTES)
            .ok_or_else(|| tool_err("LSP_CONFIGURATION_INVALID", "invalid workspace root URI"))?;
        Ok(Self {
            root,
            folders: json!([{"uri":root_uri,"name":"workspace"}]),
            state: Mutex::new(State { settings: Arc::new(json!({})), revision: 0 }),
        })
    }

    pub(super) fn respond(&self, method: &str, params: &Value) -> Option<Reply> {
        match method {
            "workspace/configuration" => Some(self.configuration(params)),
            "workspace/workspaceFolders" => Some(
                if params.is_null() || params.as_object().is_some_and(serde_json::Map::is_empty) {
                    Ok(self.folders.clone())
                } else { Err(invalid("workspaceFolders takes no parameters")) }
            ),
            _ => None,
        }
    }

    fn configuration(&self, params: &Value) -> Reply {
        if !shape(params, 0, &mut 0) || !bounded(params, MAX_SETTINGS_BYTES) {
            return Err(invalid("configuration request exceeds its structural or byte limit"));
        }
        let items = params.get("items").and_then(Value::as_array)
            .filter(|items| items.len() <= MAX_ITEMS)
            .ok_or_else(|| invalid("configuration requires at most 128 items"))?;
        let settings = Arc::clone(&LspClient::lock(&self.state).settings);
        let mut values = Vec::with_capacity(items.len());
        for item in items {
            let item = item.as_object().ok_or_else(|| invalid("configuration item must be an object"))?;
            let section = match item.get("section") {
                None | Some(Value::Null) => "",
                Some(Value::String(section)) if section.len() <= 1024 => section,
                _ => return Err(invalid("configuration section must be a bounded string")),
            };
            let scoped = match item.get("scopeUri") {
                None | Some(Value::Null) => true,
                Some(Value::String(uri)) if !uri.is_empty() && uri.len() <= MAX_URI_BYTES
                    && !uri.chars().any(|ch| ch.is_control() || ch.is_whitespace()) => {
                    url::Url::parse(uri).map_err(|_| invalid("scopeUri must be absolute"))?;
                    // Component-aware lexical scope only. Do not follow links,
                    // stat the target, fetch a URI, or consult another root.
                    uri_to_path(uri).is_some_and(|path| path.starts_with(&self.root))
                }
                _ => return Err(invalid("scopeUri must be a bounded absolute URI")),
            };
            let value = if !scoped {
                &Value::Null
            } else if section.is_empty() {
                settings.as_ref()
            } else {
                section.split('.').try_fold(settings.as_ref(), |value, component| {
                    value.as_object()?.get(component)
                }).unwrap_or(&Value::Null)
            };
            values.push(value);
        }
        // Charge repeated sections before cloning: a small settings snapshot
        // must not expand into an unbounded server reply through repetition.
        if !bounded(&values, MAX_REPLY_BYTES) {
            return Err(invalid("configuration response exceeds 256 KiB; request fewer sections"));
        }
        Ok(Value::Array(values.into_iter().cloned().collect()))
    }
}

impl LspClient {
    pub(in crate::lsp) fn workspace_settings(&self) -> (Arc<Value>, u64) {
        let state = Self::lock(&self.workspace.state);
        (Arc::clone(&state.settings), state.revision)
    }

    /// Wait for atomic admission within one owner/deadline budget. Only a
    /// wholly unaccepted transaction is retried; successful queue admission
    /// is reported even when cancellation arrives immediately afterwards.
    pub(in crate::lsp) async fn set_workspace_settings_with_timeout(
        &self,
        settings: &Settings,
        timeout: Duration,
    ) -> Result<(bool, u64)> {
        let owner = AgentCx::for_current_or_request();
        let budget = RequestBudget::new(timeout);
        crate::lsp::refactor_preview::check_owner(&owner)?;
        let _lane = budget.acquire(&self.request_lane).await?;
        loop {
            if let Some(outcome) = self.try_set_workspace_settings(settings, &owner, &budget)? {
                return Ok(outcome);
            }
            // No workspace/document mutex or queue reservation survives this
            // wait. Rebuild against the current revision and documents after
            // waking; immediate synchronization callers may have changed them.
            budget.pause(super::WAIT_TICK).await?;
        }
    }

    /// Single-attempt counterpart used to verify refusal without waiting.
    #[cfg(test)]
    pub(in crate::lsp) fn set_workspace_settings(&self, settings: &Settings) -> Result<(bool, u64)> {
        let owner = AgentCx::for_current_or_request();
        let budget = RequestBudget::new(super::DEFAULT_REQUEST_TIMEOUT);
        self.try_set_workspace_settings(settings, &owner, &budget)?
            .ok_or_else(|| tool_err("LSP_BACKPRESSURE", "workspace configuration queue is full"))
    }

    /// Replace, never merge. A queued change is not a server acknowledgement.
    /// Closures and the new settings enter the transport as one transaction.
    /// Refusal preserves the synchronized documents, diagnostics and revision;
    /// successful admission retires old incarnations before exposing new settings.
    // Retain both guards through admission and local publication. A document
    // sync cannot overtake the configuration, and a reverse configuration
    // request cannot observe the old snapshot after the update is queued.
    #[allow(clippy::significant_drop_tightening)]
    fn try_set_workspace_settings(
        &self,
        settings: &Settings,
        owner: &AgentCx,
        budget: &RequestBudget,
    ) -> Result<Option<(bool, u64)>> {
        crate::lsp::refactor_preview::check_owner(owner)?;
        budget.remaining()?;
        if !self.is_alive() {
            return Err(tool_err("LSP_TRANSPORT_CLOSED", "configuration connection closed"));
        }
        let mut state = Self::lock(&self.workspace.state);
        budget.remaining()?;
        if state.revision != 0 && *state.settings == *settings.0 {
            return Ok(Some((false, state.revision)));
        }
        let revision = state.revision.checked_add(1)
            .ok_or_else(|| tool_err("LSP_CONFIGURATION_INVALID", "configuration revision exhausted"))?;
        let mut docs = Self::lock(&self.open_docs);
        let mut uris: Vec<_> = docs
            .iter()
            .filter(|(_, doc)| doc.opened)
            .map(|(uri, _)| uri.as_str())
            .collect();
        uris.sort_unstable();
        let mut notifications: Vec<_> = uris
            .into_iter()
            .map(|uri| ("textDocument/didClose", json!({"textDocument":{"uri":uri}})))
            .collect();
        notifications.push((
            "workspace/didChangeConfiguration",
            json!({"settings":settings.value()}),
        ));
        let mut batch = super::JsonRpcClient::prepare_notifications(notifications);
        crate::lsp::refactor_preview::check_owner(owner)?;
        budget.remaining()?;
        // The ordinary notify path retires the server when the queue is full.
        // Batch admission instead refuses the entire unposted transaction,
        // with no didClose prefix and no mutation of the old local baseline.
        match self.rpc.try_admit_notifications(&mut batch) {
            Ok(()) => {}
            Err(TransportError::Backpressure(_)) => return Ok(None),
            Err(error) => return Err(tool_err(error.code(), error.message())),
        }
        // Once accepted, publish even if cancellation arrives now: the peer
        // may already be consuming the batch. There is no safe local rollback.
        docs.clear();
        Self::lock(&self.diagnostics).clear();
        // Pull reports retain their monotonic request counters. Retiring the
        // documents prevents their old result IDs from matching a new wire
        // version, while versionless documents never reuse such IDs at all.
        self.quiescent.store(false, std::sync::atomic::Ordering::SeqCst);
        state.settings = Arc::clone(&settings.0);
        state.revision = revision;
        Ok(Some((true, revision)))
    }
}

#[cfg(test)]
mod tests;
