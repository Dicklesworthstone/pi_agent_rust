//! LSP protocol client: initialize handshake, document synchronization,
//! typed requests, diagnostics cache, and graceful shutdown.
//!
//! Built on [`super::jsonrpc::JsonRpcClient`]. Requests are serialized per
//! server (spec: "serialize requests per server"); the async wait loop polls
//! the completion channel on a tick so per-request timeouts and ambient
//! cancellation both fire promptly and always send `$/cancelRequest`
//! (bd-cv653.1.1).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use super::jsonrpc::{JsonRpcClient, TransportError};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};

/// Poll tick for request completion waits (matches the bash tool's cadence).
const WAIT_TICK: Duration = Duration::from_millis(10);
/// Cadence for retrying the server-warmup error signature.
const WARMUP_RETRY_CADENCE: Duration = Duration::from_millis(250);
/// Window after connect during which empty position-lookup results may be
/// the server still indexing rather than a truthful "not found".
const WARMUP_EMPTY_RESULT_WINDOW: Duration = Duration::from_secs(60);

/// Position-lookup methods whose empty result during warmup may be indexing
/// lag rather than truth. Narrow on purpose: symbols/diagnostics are never
/// retried (an empty symbol list is a legitimate answer).
fn is_position_lookup(method: &str) -> bool {
    matches!(
        method,
        "textDocument/definition"
            | "textDocument/typeDefinition"
            | "textDocument/implementation"
            | "textDocument/references"
            | "textDocument/hover"
    )
}

/// Whether a result is "empty" in the not-found sense (null or `[]`).
fn is_empty_result(value: &Value) -> bool {
    value.is_null() || matches!(value, Value::Array(items) if items.is_empty())
}
/// Default per-request timeout.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Timeout for the graceful `shutdown` request during stop.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `diagnostics` waits for a fresh publish after opening a file.
pub const DEFAULT_DIAGNOSTICS_WAIT: Duration = Duration::from_millis(2000);

/// Why an LSP call failed.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LspCallError {
    /// Request exceeded its deadline; `$/cancelRequest` was sent.
    Timeout { timeout_ms: u64 },
    /// Ambient cancellation interrupted the wait; `$/cancelRequest` was sent.
    Cancelled,
    /// Transport-level failure (server error object, closed pipe, I/O).
    Transport(TransportError),
}

impl LspCallError {
    /// Machine-readable taxonomy code for logs and tool details.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Timeout { .. } => "LSP_TIMEOUT",
            Self::Cancelled => "LSP_CANCELLED",
            Self::Transport(err) => err.code(),
        }
    }

    /// Human-readable summary.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Timeout { timeout_ms } => format!("request timed out after {timeout_ms} ms"),
            Self::Cancelled => "cancelled by ambient context".to_string(),
            Self::Transport(err) => err.message(),
        }
    }
}

impl From<LspCallError> for Error {
    fn from(err: LspCallError) -> Self {
        Self::tool("lsp", format!("[{}] {}", err.code(), err.message()))
    }
}

/// Percent-encode a path segment for a `file://` URI.
///
/// Encodes everything outside the URI-unreserved set plus `/` (kept as the
/// path separator). Good enough for POSIX paths; Windows drive letters are
/// out of scope for the v1 surface.
#[must_use]
pub fn path_to_uri(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len() + 8);
    out.push_str("file://");
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => {
                use std::fmt::Write as _;
                out.push('%');
                let _ = write!(out, "{byte:02X}");
            }
        }
    }
    out
}

/// Decode a `file://` URI back to a path. Returns `None` for non-file URIs.
#[must_use]
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let mut out = Vec::with_capacity(rest.len());
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            let value = u8::from_str_radix(hex, 16).ok()?;
            out.push(value);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out).into_owned()))
}

/// FNV-1a hash of file content (drift detection; not cryptographic).
#[must_use]
fn content_hash(content: &str) -> u64 {
    super::text::content_hash_for_drift(content)
}

/// One open text document's sync state.
#[derive(Debug, Clone)]
struct OpenDoc {
    version: u64,
    disk_hash: u64,
    language_id: String,
}

/// Server capability snapshot captured from `initialize`.
#[derive(Debug, Clone, Default)]
pub struct ServerCapabilities {
    /// Raw `capabilities` object from the initialize result.
    pub raw: Value,
    /// `workspace.fileOperations.willRenameFiles` advertised.
    pub will_rename_files: bool,
    /// The server's text-document sync kind (1 = full, 2 = incremental).
    pub sync_kind: u64,
    /// Server display name (`serverInfo.name`).
    pub server_name: Option<String>,
}

/// A connected, initialized language server.
pub struct LspClient {
    rpc: JsonRpcClient,
    root: PathBuf,
    root_uri: String,
    open_docs: Mutex<HashMap<String, OpenDoc>>,
    diagnostics: Mutex<HashMap<String, Vec<Value>>>,
    request_lane: std::sync::Arc<asupersync::sync::Mutex<()>>,
    capabilities: Mutex<ServerCapabilities>,
    connected_at: std::time::Instant,
    /// rust-analyzer's `experimental/serverStatus` quiescent flag (true when
    /// the server reports no pending work). Stays false for servers that
    /// never send the notification.
    quiescent: std::sync::atomic::AtomicBool,
}

impl LspClient {
    /// Spawn and initialize a server.
    ///
    /// `server_request_handler` receives server→client requests the generic
    /// transport would otherwise answer with null; returning `Some(result)`
    /// overrides the response (used for `workspace/applyEdit`).
    ///
    /// # Errors
    ///
    /// Fails when the process cannot spawn, the transport dies during the
    /// handshake, or `initialize` returns an error.
    pub async fn connect(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        root: &Path,
        initialization_options: Option<&Value>,
        timeout: Duration,
    ) -> Result<Self> {
        let rpc = JsonRpcClient::spawn(command, args, env, root)?;
        let root = root.to_path_buf();
        let root_uri = path_to_uri(&root);
        let client = Self {
            rpc,
            root,
            root_uri: root_uri.clone(),
            open_docs: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(HashMap::new()),
            request_lane: std::sync::Arc::new(asupersync::sync::Mutex::new(())),
            capabilities: Mutex::new(ServerCapabilities::default()),
            connected_at: std::time::Instant::now(),
            quiescent: std::sync::atomic::AtomicBool::new(false),
        };

        let mut initialize_params = serde_json::json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": [{ "uri": root_uri, "name": "workspace" }],
            "clientInfo": {
                "name": "pi_agent_rust",
                "version": crate::platform::VERSION,
            },
            "capabilities": {
                "textDocument": {
                    "synchronization": { "didSave": true, "dynamicRegistration": false },
                    "publishDiagnostics": { "relatedInformation": true, "versionSupport": false },
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "definition": { "linkSupport": false },
                    "typeDefinition": { "linkSupport": false },
                    "implementation": { "linkSupport": false },
                    "references": {},
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "rename": { "prepareSupport": false, "honorsChangeAnnotations": false },
                    "codeAction": {
                        "dynamicRegistration": false,
                        "codeActionLiteralSupport": {
                            "codeActionKind": {
                                "valueSet": [
                                    "quickfix", "refactor", "refactor.extract",
                                    "refactor.inline", "refactor.rewrite", "source"
                                ]
                            }
                        },
                        "resolveSupport": { "properties": ["edit"] }
                    }
                },
                "workspace": {
                    "applyEdit": true,
                    "workspaceEdit": {
                        "documentChanges": true,
                        "resourceOperations": ["create", "rename", "delete"]
                    },
                    "symbol": {},
                    "workspaceFolders": true,
                    "fileOperations": { "didRename": true, "willRename": true }
                },
                "window": { "workDoneProgress": true }
            }
        });
        if let Some(options) = initialization_options {
            initialize_params["initializationOptions"] = options.clone();
        }

        let result = client
            .call("initialize", initialize_params, timeout)
            .await
            .map_err(|err| {
                client.rpc.kill();
                Error::from(err)
            })?;

        let caps = result.get("capabilities").cloned().unwrap_or(Value::Null);
        let will_rename = caps
            .pointer("/workspace/fileOperations/willRenameFiles")
            .is_some();
        let sync_kind = caps.get("textDocumentSync").map_or(1, |sync| {
            sync.get("change")
                .and_then(Value::as_u64)
                .or_else(|| sync.as_u64())
                .unwrap_or(1)
        });
        let server_name = result
            .get("serverInfo")
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string);
        *Self::lock(&client.capabilities) = ServerCapabilities {
            raw: caps,
            will_rename_files: will_rename,
            sync_kind,
            server_name,
        };

        client
            .rpc
            .notify("initialized", serde_json::json!({}))
            .map_err(|err| Error::tool("lsp", format!("[LSP_TRANSPORT_IO] {}", err.message())))?;
        Ok(client)
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Server capabilities from the initialize handshake.
    #[must_use]
    pub fn capabilities(&self) -> ServerCapabilities {
        Self::lock(&self.capabilities).clone()
    }

    /// Workspace root this server is bound to.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether the underlying transport is still alive.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.rpc.is_alive() && !self.rpc.child_exited()
    }

    /// Bounded tail of server stderr (diagnostics surface).
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        self.rpc.stderr_tail()
    }

    /// Notifications dropped due to queue overflow.
    #[must_use]
    pub fn dropped_notifications(&self) -> u64 {
        self.rpc.dropped_notifications()
    }

    /// URIs with cached diagnostics (from `textDocument/publishDiagnostics`).
    #[must_use]
    pub fn diagnostics_snapshot(&self) -> HashMap<String, Vec<Value>> {
        self.poll_notifications();
        Self::lock(&self.diagnostics).clone()
    }

    /// Number of currently open documents (status surface).
    #[must_use]
    pub fn open_document_count(&self) -> usize {
        Self::lock(&self.open_docs).len()
    }

    /// Merge queued notifications into the diagnostics cache and track
    /// server quiescence (`experimental/serverStatus`).
    pub fn poll_notifications(&self) {
        for notification in self.rpc.drain_notifications() {
            if notification.method == "textDocument/publishDiagnostics"
                && let (Some(uri), Some(diags)) = (
                    notification.params.get("uri").and_then(Value::as_str),
                    notification
                        .params
                        .get("diagnostics")
                        .and_then(Value::as_array),
                )
            {
                Self::lock(&self.diagnostics).insert(uri.to_string(), diags.clone());
            } else if notification.method == "experimental/serverStatus"
                && let Some(quiescent) = notification
                    .params
                    .get("quiescent")
                    .and_then(Value::as_bool)
            {
                self.quiescent
                    .store(quiescent, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    /// Block (async, tick-polled) until diagnostics for `uri` are fresh
    /// enough to trust, or `wait` elapses. Returns true when a publish
    /// arrived.
    ///
    /// "Fresh enough" means a publish for `uri` arrived AND at least one of:
    /// the publish was non-empty, the server reported quiescence
    /// (rust-analyzer's `experimental/serverStatus`), or the warmup window
    /// has passed. An EMPTY publish inside the warmup window keeps waiting —
    /// rust-analyzer publishes empty diagnostics for freshly opened files
    /// before its first analysis completes.
    pub async fn wait_for_diagnostics(&self, uri: &str, wait: Duration) -> bool {
        let cx = AgentCx::for_current_or_request();
        let start = cx
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        loop {
            self.poll_notifications();
            {
                let cache = Self::lock(&self.diagnostics);
                if let Some(diags) = cache.get(uri) {
                    let settled = !diags.is_empty()
                        || self.quiescent.load(std::sync::atomic::Ordering::SeqCst)
                        || self.connected_at.elapsed() >= WARMUP_EMPTY_RESULT_WINDOW;
                    if settled {
                        return true;
                    }
                }
            }
            let now = cx
                .cx()
                .timer_driver()
                .map_or_else(asupersync::time::wall_now, |timer| timer.now());
            if std::time::Duration::from_nanos(now.duration_since(start)) >= wait {
                return Self::lock(&self.diagnostics).contains_key(uri);
            }
            asupersync::time::sleep(now, WAIT_TICK).await;
        }
    }

    /// Serialized, timeout- and cancellation-aware request.
    ///
    /// Retries the narrow "server still warming up" signature (rust-analyzer
    /// answers `-32602 No references found` for valid positions while it is
    /// still indexing) on a 250 ms cadence within the caller's timeout; all
    /// other errors return immediately. The match is message-specific so
    /// real usage errors are never retried away.
    ///
    /// # Errors
    ///
    /// Returns [`LspCallError`] on timeout, ambient cancellation, or
    /// transport failure. Timeout and cancellation both send
    /// `$/cancelRequest` before returning.
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> std::result::Result<Value, LspCallError> {
        let cx = AgentCx::for_current_or_request();
        let start = cx
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        let mut attempt = self.call_once(method, params.clone(), timeout).await;
        loop {
            // Retryable warmup/transient signatures:
            // - `-32602 No references found`: rust-analyzer answers valid
            //   positions with this while still indexing.
            // - `-32801 ContentModified`: the LSP spec's designated
            //   retryable error; here it is the same warmup race (the doc
            //   was synced from disk milliseconds earlier, so genuine drift
            //   is impossible inside one tool call).
            let retryable = matches!(
                &attempt,
                Err(LspCallError::Transport(TransportError::Server(err)))
                    if (err.code == -32602 && err.message.contains("No references found"))
                        || err.code == -32801
            );
            // Empty position-lookup results inside the warmup window may be
            // indexing lag; retrying only ever DELAYS an empty answer, it
            // can never fabricate a result.
            let empty_during_warmup = matches!(&attempt, Ok(value) if is_empty_result(value))
                && is_position_lookup(method)
                && self.connected_at.elapsed() < WARMUP_EMPTY_RESULT_WINDOW;
            if !retryable && !empty_during_warmup {
                return attempt;
            }
            let now = cx
                .cx()
                .timer_driver()
                .map_or_else(asupersync::time::wall_now, |timer| timer.now());
            let elapsed = std::time::Duration::from_nanos(now.duration_since(start));
            let remaining = timeout.saturating_sub(elapsed);
            if remaining < WARMUP_RETRY_CADENCE * 2 {
                return attempt;
            }
            asupersync::time::sleep(now, WARMUP_RETRY_CADENCE).await;
            if cx.checkpoint().is_err() {
                return Err(LspCallError::Cancelled);
            }
            attempt = self.call_once(method, params.clone(), remaining).await;
        }
    }

    /// One serialized request round-trip.
    async fn call_once(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> std::result::Result<Value, LspCallError> {
        let cx = AgentCx::for_current_or_request();
        // Serialize requests per server (spec). The owned guard is Send, so
        // the wait loop below can await while holding it; the guard releases
        // on drop.
        let _lane = asupersync::sync::OwnedMutexGuard::lock(
            std::sync::Arc::clone(&self.request_lane),
            cx.cx(),
        )
        .await
        .map_err(|_| LspCallError::Cancelled)?;
        let (id, rx) = self
            .rpc
            .request(method, params)
            .map_err(LspCallError::Transport)?;
        let start = cx
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        loop {
            match rx.try_recv() {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(err)) => return Err(LspCallError::Transport(err)),
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(LspCallError::Transport(TransportError::Closed(
                        "completion channel dropped".to_string(),
                    )));
                }
            }
            let now = cx
                .cx()
                .timer_driver()
                .map_or_else(asupersync::time::wall_now, |timer| timer.now());
            if std::time::Duration::from_nanos(now.duration_since(start)) >= timeout {
                self.rpc.cancel_request(id);
                return Err(LspCallError::Timeout {
                    timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                });
            }
            if cx.checkpoint().is_err() {
                self.rpc.cancel_request(id);
                return Err(LspCallError::Cancelled);
            }
            asupersync::time::sleep(now, WAIT_TICK).await;
        }
    }

    /// Ensure the server's view of `path` matches disk: open the document,
    /// or close + reopen when the on-disk content changed since we opened it.
    ///
    /// Close/reopen is used instead of incremental `didChange` so the resync
    /// is correct under every server sync kind (full or incremental).
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or notifications cannot be sent.
    pub fn ensure_synced(&self, path: &Path, language_id: &str) -> Result<String> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let uri = path_to_uri(&canonical);
        let content = std::fs::read_to_string(&canonical).map_err(|err| {
            Error::tool(
                "lsp",
                format!(
                    "[LSP_FILE_UNREADABLE] cannot read {}: {err}",
                    canonical.display()
                ),
            )
        })?;
        let disk_hash = content_hash(&content);
        let prior = Self::lock(&self.open_docs).get(&uri).cloned();
        match prior {
            Some(doc) if doc.disk_hash == disk_hash => Ok(uri),
            Some(_) => {
                // Drifted: close + reopen with the current disk content.
                let _ = self.rpc.notify(
                    "textDocument/didClose",
                    serde_json::json!({ "textDocument": { "uri": uri } }),
                );
                Self::lock(&self.open_docs).remove(&uri);
                self.open_document(&uri, &content, disk_hash, language_id)
            }
            None => self.open_document(&uri, &content, disk_hash, language_id),
        }
    }

    fn open_document(
        &self,
        uri: &str,
        content: &str,
        disk_hash: u64,
        language_id: &str,
    ) -> Result<String> {
        self.rpc
            .notify(
                "textDocument/didOpen",
                serde_json::json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id,
                        "version": 1,
                        "text": content,
                    }
                }),
            )
            .map_err(|err| Error::tool("lsp", format!("[{}] {}", err.code(), err.message())))?;
        Self::lock(&self.open_docs).insert(
            uri.to_string(),
            OpenDoc {
                version: 1,
                disk_hash,
                language_id: language_id.to_string(),
            },
        );
        Ok(uri.to_string())
    }

    /// Forget a document locally; the next `ensure_synced` reopens it from
    /// disk. Used after external (WorkspaceEdit) mutation.
    pub fn invalidate(&self, uri: &str) {
        if Self::lock(&self.open_docs).remove(uri).is_some() {
            let _ = self.rpc.notify(
                "textDocument/didClose",
                serde_json::json!({ "textDocument": { "uri": uri } }),
            );
        }
    }

    /// Forget every open document (used after rename_file moves).
    pub fn invalidate_all(&self) {
        let uris: Vec<String> = Self::lock(&self.open_docs).keys().cloned().collect();
        for uri in uris {
            self.invalidate(&uri);
        }
    }

    /// Graceful stop: `shutdown` request, `exit` notification, then kill if
    /// the process does not exit within a short grace window.
    pub async fn stop(&self) {
        let _ = self.call("shutdown", Value::Null, SHUTDOWN_TIMEOUT).await;
        self.rpc.shutdown();
    }

    /// Hard kill.
    pub fn kill(&self) {
        self.rpc.kill();
    }

    /// Install the server→client request hook (see
    /// [`JsonRpcClient::set_server_request_handler`]).
    pub fn set_server_request_handler(&self, handler: super::jsonrpc::ServerRequestHandler) {
        self.rpc.set_server_request_handler(handler);
    }

    /// Fire-and-forget notification wrapper (errors intentionally dropped by
    /// callers on best-effort paths like `didRenameFiles`).
    pub fn call_no_wait_notify(
        &self,
        method: &str,
        params: Value,
    ) -> std::result::Result<(), TransportError> {
        self.rpc.notify(method, params)
    }
}

/// Parse a `Location | Location[] | LocationLink[] | null` result into a
/// flat list of `(uri, range)` pairs.
#[must_use]
pub fn parse_locations(result: &Value) -> Vec<(String, super::text::Range)> {
    let mut out = Vec::new();
    let items: Vec<&Value> = match result {
        Value::Array(items) => items.iter().collect(),
        single @ Value::Object(_) => vec![single],
        _ => return out,
    };
    for item in items {
        // LocationLink carries targetUri/targetSelectionRange.
        let (uri, range) = if let Some(uri) = item.get("targetUri").and_then(Value::as_str) {
            let range = item
                .get("targetSelectionRange")
                .or_else(|| item.get("targetRange"));
            (uri, range)
        } else {
            let Some(uri) = item.get("uri").and_then(Value::as_str) else {
                continue;
            };
            (uri, item.get("range"))
        };
        if let Some(range) = range.and_then(|r| serde_json::from_value(r.clone()).ok()) {
            out.push((uri.to_string(), range));
        }
    }
    out
}

/// Extract text from a `MarkedString` (`"..."` or `{language, value}`) or
/// `MarkupContent` (`{kind, value}`).
fn marked_string_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => map.get("value").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// Extract displayable text from a hover result.
#[must_use]
pub fn hover_to_text(result: &Value) -> Option<String> {
    let contents = result.get("contents")?;
    match contents {
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(marked_string_text).collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n\n"))
            }
        }
        single => marked_string_text(single),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrip_plain() {
        let path = PathBuf::from("/tmp/workspace/src/main.rs");
        let uri = path_to_uri(&path);
        assert_eq!(uri, "file:///tmp/workspace/src/main.rs");
        assert_eq!(uri_to_path(&uri), Some(path));
    }

    #[test]
    fn uri_encodes_specials() {
        let path = PathBuf::from("/tmp/my project/fi#1?.rs");
        let uri = path_to_uri(&path);
        assert_eq!(uri, "file:///tmp/my%20project/fi%231%3F.rs");
        assert_eq!(
            uri_to_path(&uri),
            Some(PathBuf::from("/tmp/my project/fi#1?.rs"))
        );
    }

    #[test]
    fn uri_rejects_non_file() {
        assert_eq!(uri_to_path("https://example.com/x"), None);
    }

    #[test]
    fn parse_locations_handles_all_shapes() {
        // Single Location object.
        let single = serde_json::json!({
            "uri": "file:///a.rs",
            "range": { "start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5} }
        });
        let got = parse_locations(&single);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "file:///a.rs");
        assert_eq!(got[0].1.start.line, 1);

        // Array.
        let array = serde_json::json!([single]);
        assert_eq!(parse_locations(&array).len(), 1);

        // LocationLink.
        let link = serde_json::json!({
            "targetUri": "file:///b.rs",
            "targetRange": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3} },
            "targetSelectionRange": { "start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 2} }
        });
        let got = parse_locations(&link);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "file:///b.rs");
        assert_eq!(got[0].1.start.character, 1);

        // Null and garbage.
        assert!(parse_locations(&Value::Null).is_empty());
        assert!(parse_locations(&serde_json::json!(42)).is_empty());
    }

    #[test]
    fn hover_text_handles_markup_and_marked() {
        let markup = serde_json::json!({
            "contents": { "kind": "markdown", "value": "```rust\nfn x()\n```" }
        });
        assert_eq!(
            hover_to_text(&markup),
            Some("```rust\nfn x()\n```".to_string())
        );
        let marked_array = serde_json::json!({
            "contents": [{ "language": "rust", "value": "fn x()" }, "docs here"]
        });
        assert_eq!(
            hover_to_text(&marked_array),
            Some("fn x()\n\ndocs here".to_string())
        );
        assert_eq!(hover_to_text(&serde_json::json!({})), None);
    }
}
