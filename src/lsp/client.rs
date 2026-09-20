//! LSP initialization, synchronized document versions, requests and diagnostics.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;

use super::jsonrpc::{JsonRpcClient, TransportError};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};

mod document_sync;
mod file_uri;
mod pull_diagnostics;
mod request;
#[cfg(test)]
mod test_server;
pub use file_uri::{path_to_uri, try_path_to_uri, uri_to_path};

const WAIT_TICK: Duration = Duration::from_millis(10);
const WARMUP_RETRY_CADENCE: Duration = Duration::from_millis(250);
const WARMUP_EMPTY_RESULT_WINDOW: Duration = Duration::from_secs(180);

fn is_warmup_empty_retryable(method: &str) -> bool {
    matches!(
        method,
        "textDocument/definition"
            | "textDocument/typeDefinition"
            | "textDocument/implementation"
            | "textDocument/references"
            | "textDocument/hover"
            | "textDocument/rename"
            | "workspace/willRenameFiles"
    )
}

fn is_empty_result(value: &Value) -> bool {
    if value.is_null() {
        return true;
    }
    match value {
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => {
            map.is_empty()
                || ((map.contains_key("changes") || map.contains_key("documentChanges"))
                    && map
                        .get("changes")
                        .and_then(Value::as_object)
                        .is_none_or(serde_json::Map::is_empty)
                    && map
                        .get("documentChanges")
                        .and_then(Value::as_array)
                        .is_none_or(Vec::is_empty))
        }
        _ => false,
    }
}

pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_DIAGNOSTICS_WAIT: Duration = Duration::from_millis(2000);

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LspCallError {
    Timeout { timeout_ms: u64 },
    Cancelled,
    Transport(TransportError),
}

impl LspCallError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Timeout { .. } => "LSP_TIMEOUT",
            Self::Cancelled => "LSP_CANCELLED",
            Self::Transport(err) => err.code(),
        }
    }

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

fn content_hash(content: &str) -> u64 {
    super::text::content_hash_for_drift(content)
}

/// A request-time view used to reject edits against a different document version.
#[derive(Debug, Clone, Copy)]
pub struct DocumentSnapshot {
    pub version: u64,
    pub hash: u64,
}

#[derive(Debug, Clone)]
struct OpenDoc {
    version: u64,
    disk_hash: u64,
    language_id: String,
    text: std::sync::Arc<str>,
    opened: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ServerCapabilities {
    pub raw: Value,
    pub will_rename_files: bool,
    pub sync_kind: u64,
    pub server_name: Option<String>,
}

pub struct LspClient {
    rpc: JsonRpcClient,
    root: PathBuf,
    root_uri: String,
    open_docs: Mutex<HashMap<String, OpenDoc>>,
    diagnostics: Mutex<HashMap<String, Vec<Value>>>,
    pull_reports: Mutex<pull_diagnostics::ReportCache>,
    request_lane: std::sync::Arc<asupersync::sync::Mutex<()>>,
    capabilities: Mutex<ServerCapabilities>,
    connected_at: std::time::Instant,
    quiescent: std::sync::atomic::AtomicBool,
    // Never reuse version 1 after closing/reopening a file. Delayed versioned
    // edits must not accidentally match a new incarnation of that document.
    next_document_version: AtomicU64,
}

impl LspClient {
    #[allow(clippy::too_many_lines)]
    pub async fn connect(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        root: &Path,
        initialization_options: Option<&Value>,
        timeout: Duration,
    ) -> Result<Self> {
        // Resolve the workspace before spawning; never send an ambiguous or
        // relative native path as the server's document root.
        let root = root.canonicalize()?;
        let root_uri = try_path_to_uri(&root)?;
        let rpc = JsonRpcClient::spawn(command, args, env, &root)?;
        let client = Self {
            rpc,
            root,
            root_uri: root_uri.clone(),
            open_docs: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(HashMap::new()),
            pull_reports: Mutex::new(pull_diagnostics::ReportCache::default()),
            request_lane: std::sync::Arc::new(asupersync::sync::Mutex::new(())),
            capabilities: Mutex::new(ServerCapabilities::default()),
            connected_at: std::time::Instant::now(),
            quiescent: std::sync::atomic::AtomicBool::new(false),
            next_document_version: AtomicU64::new(1),
        };
        let mut params = serde_json::json!({
            "processId":std::process::id(),"rootUri":root_uri,
            "workspaceFolders":[{"uri":root_uri,"name":"workspace"}],
            "clientInfo":{"name":"pi_agent_rust","version":crate::platform::VERSION},
            "capabilities":{
                "general":{"positionEncodings":["utf-16"]},
                "textDocument":{
                    "synchronization":{"didSave":true,"dynamicRegistration":false},
                    "publishDiagnostics":{"relatedInformation":true,"versionSupport":true},
                    "diagnostic":{"dynamicRegistration":false,"relatedDocumentSupport":false},
                    "hover":{"contentFormat":["markdown","plaintext"]},
                    "inlayHint":{
                        "dynamicRegistration":false,
                        "resolveSupport":{"properties":["tooltip","label.tooltip","label.location"]}
                    },
                    "signatureHelp":{
                        "dynamicRegistration":false,"contextSupport":true,
                        "signatureInformation":{
                            "documentationFormat":["markdown","plaintext"],
                            "parameterInformation":{"labelOffsetSupport":true},
                            "activeParameterSupport":true
                        }
                    },
                    "completion":{
                        "dynamicRegistration":false,"contextSupport":true,"insertTextMode":1,
                        "completionItem":{
                            "snippetSupport":true,"insertReplaceSupport":true,
                            "documentationFormat":["markdown","plaintext"],
                            "insertTextModeSupport":{"valueSet":[1]},
                            "resolveSupport":{"properties":["detail","documentation","additionalTextEdits"]}
                        },
                        "completionList":{"itemDefaults":["editRange","insertTextFormat","insertTextMode","data"]}
                    },
                    "definition":{"linkSupport":false},"typeDefinition":{"linkSupport":false},
                    "implementation":{"linkSupport":false},"references":{},
                    "callHierarchy":{"dynamicRegistration":false},
                    "typeHierarchy":{"dynamicRegistration":false},
                    "documentSymbol":{"hierarchicalDocumentSymbolSupport":true},
                    "rename":{"prepareSupport":false,"honorsChangeAnnotations":false},
                    "codeAction":{
                        "dynamicRegistration":false,"dataSupport":true,
                        "disabledSupport":true,"isPreferredSupport":true,
                        "codeActionLiteralSupport":{"codeActionKind":{"valueSet":[
                            "quickfix","refactor","refactor.extract","refactor.inline","refactor.rewrite","source"
                        ]}},
                        "resolveSupport":{"properties":["edit"]}
                    }
                },
                "workspace":{
                    "applyEdit":true,"workspaceEdit":{
                        "documentChanges":true,"resourceOperations":["create","rename","delete"]
                    },
                    "symbol":{},"workspaceFolders":true,
                    "fileOperations":{"didRename":true,"willRename":true}
                },
                "window":{"workDoneProgress":true}
            }
        });
        if let Some(options) = initialization_options {
            params["initializationOptions"] = options.clone();
        }
        let result = client
            .call("initialize", params, timeout)
            .await
            .map_err(|err| {
                client.rpc.kill();
                Error::from(err)
            })?;
        let caps = result.get("capabilities").cloned().unwrap_or(Value::Null);
        let will_rename_files = caps
            .pointer("/workspace/fileOperations/willRenameFiles")
            .is_some();
        let sync_kind = document_sync::SyncPolicy::parse(&caps)
            .inspect_err(|_| {
                client.rpc.kill();
            })?
            .change;
        let server_name = result
            .get("serverInfo")
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string);
        *Self::lock(&client.capabilities) = ServerCapabilities {
            raw: caps,
            will_rename_files,
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

    #[must_use]
    pub fn capabilities(&self) -> ServerCapabilities {
        Self::lock(&self.capabilities).clone()
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.rpc.is_alive() && !self.rpc.child_exited()
    }

    #[must_use]
    pub fn stderr_tail(&self) -> String {
        self.rpc.stderr_tail()
    }

    #[must_use]
    pub fn dropped_notifications(&self) -> u64 {
        self.rpc.dropped_notifications()
    }

    #[must_use]
    pub fn diagnostics_snapshot(&self) -> HashMap<String, Vec<Value>> {
        self.poll_notifications();
        Self::lock(&self.diagnostics).clone()
    }

    #[must_use]
    pub fn open_document_count(&self) -> usize {
        Self::lock(&self.open_docs).len()
    }

    #[must_use]
    pub fn document_snapshots(&self) -> HashMap<PathBuf, DocumentSnapshot> {
        Self::lock(&self.open_docs)
            .iter()
            .filter(|(_, doc)| doc.version != 0)
            .filter_map(|(uri, doc)| {
                uri_to_path(uri).map(|path| {
                    (
                        path,
                        DocumentSnapshot {
                            version: doc.version,
                            hash: doc.disk_hash,
                        },
                    )
                })
            })
            .collect()
    }

    /// Immutable identity of the current local document incarnation. Keeping
    /// this Arc lets a read workflow detect close/reopen even without a wire
    /// version. Callers retaining it must bound their own source working set.
    pub(in crate::lsp) fn synchronized_text(&self, uri: &str) -> Option<std::sync::Arc<str>> {
        let uri = file_uri::normalize_uri(uri)?;
        Self::lock(&self.open_docs)
            .get(&uri)
            .map(|document| std::sync::Arc::clone(&document.text))
    }

    pub fn poll_notifications(&self) {
        for notification in self.rpc.drain_notifications() {
            if notification.method == "textDocument/publishDiagnostics" {
                self.accept_diagnostics(&notification.params);
            } else if notification.method == "experimental/serverStatus"
                && let Some(quiescent) = notification
                    .params
                    .get("quiescent")
                    .and_then(Value::as_bool)
            {
                self.quiescent.store(quiescent, Ordering::SeqCst);
            }
        }
    }

    pub async fn wait_for_diagnostics(&self, uri: &str, wait: Duration) -> bool {
        let Some(uri) = file_uri::normalize_uri(uri) else {
            return false;
        };
        if self.has_pull_diagnostics() && !wait.is_zero() {
            return self.refresh_document_diagnostics(&uri, wait).await.is_ok();
        }
        let cx = AgentCx::for_current_or_request();
        let start = cx
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        loop {
            if cx.checkpoint().is_err() || !self.is_alive() {
                return false;
            }
            self.poll_notifications();
            {
                let cache = Self::lock(&self.diagnostics);
                if let Some(diags) = cache.get(&uri) {
                    let settled = !diags.is_empty()
                        || self.quiescent.load(Ordering::SeqCst)
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
            if Duration::from_nanos(now.duration_since(start)) >= wait {
                return Self::lock(&self.diagnostics).contains_key(&uri);
            }
            let remaining = wait.saturating_sub(Duration::from_nanos(now.duration_since(start)));
            cx.time().sleep(WAIT_TICK.min(remaining)).await;
        }
    }

    pub async fn stop(&self) {
        let _ = self.call("shutdown", Value::Null, SHUTDOWN_TIMEOUT).await;
        self.rpc.shutdown();
    }

    pub fn kill(&self) {
        self.rpc.kill();
    }

    pub fn set_server_request_handler(&self, handler: super::jsonrpc::ServerRequestHandler) {
        self.rpc.set_server_request_handler(handler);
    }

    pub fn call_no_wait_notify(
        &self,
        method: &str,
        params: Value,
    ) -> std::result::Result<(), TransportError> {
        self.rpc.notify(method, params)
    }
}

#[must_use]
pub fn parse_locations(result: &Value) -> Vec<(String, super::text::Range)> {
    let mut out = Vec::new();
    let items: Vec<&Value> = match result {
        Value::Array(items) => items.iter().collect(),
        single @ Value::Object(_) => vec![single],
        _ => return out,
    };
    for item in items {
        let (uri, range) = if let Some(uri) = item.get("targetUri").and_then(Value::as_str) {
            (
                uri,
                item.get("targetSelectionRange")
                    .or_else(|| item.get("targetRange")),
            )
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

fn marked_string_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => map.get("value").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

#[must_use]
pub fn hover_to_text(result: &Value) -> Option<String> {
    let contents = result.get("contents")?;
    match contents {
        Value::Array(items) => {
            let parts: Vec<_> = items.iter().filter_map(marked_string_text).collect();
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
    #[cfg(unix)]
    fn uri_roundtrip_plain() {
        let path = PathBuf::from("/tmp/workspace/src/main.rs");
        let uri = path_to_uri(&path);
        assert_eq!(uri, "file:///tmp/workspace/src/main.rs");
        assert_eq!(uri_to_path(&uri), Some(path));
    }

    #[test]
    #[cfg(unix)]
    fn uri_encodes_specials() {
        let path = PathBuf::from("/tmp/my project/fi#1?.rs");
        let uri = path_to_uri(&path);
        assert_eq!(uri, "file:///tmp/my%20project/fi%231%3F.rs");
        assert_eq!(uri_to_path(&uri), Some(path));
    }

    #[test]
    fn uri_rejects_non_file() {
        assert_eq!(uri_to_path("https://example.com/x"), None);
    }

    #[test]
    fn parse_locations_handles_all_shapes() {
        let single = serde_json::json!({"uri":"file:///a.rs","range":{"start":{"line":1,"character":2},"end":{"line":1,"character":5}}});
        let got = parse_locations(&single);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "file:///a.rs");
        assert_eq!(got[0].1.start.line, 1);
        assert_eq!(parse_locations(&serde_json::json!([single])).len(), 1);
        let link = serde_json::json!({"targetUri":"file:///b.rs","targetRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"targetSelectionRange":{"start":{"line":0,"character":1},"end":{"line":0,"character":2}}});
        let got = parse_locations(&link);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "file:///b.rs");
        assert_eq!(got[0].1.start.character, 1);
        assert!(parse_locations(&Value::Null).is_empty());
        assert!(parse_locations(&serde_json::json!(42)).is_empty());
    }

    #[test]
    fn hover_text_handles_markup_and_marked() {
        let markup =
            serde_json::json!({"contents":{"kind":"markdown","value":"```rust\nfn x()\n```"}});
        assert_eq!(
            hover_to_text(&markup),
            Some("```rust\nfn x()\n```".to_string())
        );
        let marked =
            serde_json::json!({"contents":[{"language":"rust","value":"fn x()"},"docs here"]});
        assert_eq!(
            hover_to_text(&marked),
            Some("fn x()\n\ndocs here".to_string())
        );
        assert_eq!(hover_to_text(&serde_json::json!({})), None);
    }

    #[test]
    fn command_errors_are_not_warmup_retries() {
        assert!(!is_warmup_empty_retryable("workspace/executeCommand"));
        assert!(!is_warmup_empty_retryable("codeAction/resolve"));
        assert!(is_warmup_empty_retryable("textDocument/definition"));
        assert!(is_warmup_empty_retryable("workspace/willRenameFiles"));
    }
}
