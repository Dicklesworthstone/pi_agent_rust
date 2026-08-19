//! MCP transports (bd-cv653.6.1).
//!
//! Stdio shares the LSP module's framed JSON-RPC transport with a strict
//! env allowlist; streamable HTTP does POST-per-message with JSON or SSE
//! responses, `Mcp-Session-Id` continuity, and custom headers.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::lsp::jsonrpc::{EnvPolicy, JsonRpcClient, MCP_ENV_ALLOWLIST, await_completion};

/// Default per-request timeout for MCP calls.
pub const DEFAULT_MCP_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on an HTTP response body (10 MiB).
const MAX_HTTP_BODY: usize = 10 * 1024 * 1024;
/// MCP protocol revision this client speaks.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

fn tool_err(code: &str, message: impl Into<String>) -> Error {
    Error::tool("mcp", format!("[{code}] {}", message.into()))
}

/// One transport connection to an MCP server.
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// Send a request and await its response.
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value>;
    /// Send a notification.
    async fn notify(&self, method: &str, params: Value) -> Result<()>;
    /// Whether the transport is still usable.
    fn is_alive(&self) -> bool;
    /// Close the transport (best effort).
    async fn close(&self);
    /// Recent server stderr (stdio) or last HTTP error detail, for `/mcp`.
    fn diagnostics_tail(&self) -> String;
}

// ============================================================================
// stdio transport
// ============================================================================

/// JSON-RPC over a spawned child process with an env allowlist.
pub struct StdioTransport {
    rpc: JsonRpcClient,
    lane: std::sync::Arc<asupersync::sync::Mutex<()>>,
}

impl StdioTransport {
    /// Spawn the server with the MCP env allowlist (no ambient secrets).
    ///
    /// # Errors
    ///
    /// Fails when the command cannot be spawned.
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<Self> {
        let rpc = JsonRpcClient::spawn_with_policy(
            command,
            args,
            env,
            cwd,
            &EnvPolicy::Allowlist(MCP_ENV_ALLOWLIST),
            "mcp",
        )?;
        Ok(Self {
            rpc,
            lane: std::sync::Arc::new(asupersync::sync::Mutex::new(())),
        })
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        // Serialize per connection: one in-flight request keeps ordering
        // deterministic for stdio servers.
        let _lane =
            asupersync::sync::OwnedMutexGuard::lock(std::sync::Arc::clone(&self.lane), cx.cx())
                .await
                .map_err(|_| tool_err("MCP_CANCELLED", "cancelled by ambient context"))?;
        let (id, rx) = self
            .rpc
            .request(method, params)
            .map_err(|err| tool_err(&err.mcp_code(), err.message()))?;
        let outcome = await_completion(rx, timeout, || self.rpc.cancel_request(id)).await;
        match outcome {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(tool_err(&err.mcp_code(), err.message())),
            Err(crate::lsp::jsonrpc::CompletionWaitError::Timeout) => Err(tool_err(
                "MCP_TIMEOUT",
                format!("request timed out after {} ms", timeout.as_millis()),
            )),
            Err(crate::lsp::jsonrpc::CompletionWaitError::Cancelled) => {
                Err(tool_err("MCP_CANCELLED", "cancelled by ambient context"))
            }
            Err(crate::lsp::jsonrpc::CompletionWaitError::Closed) => Err(tool_err(
                "MCP_TRANSPORT_CLOSED",
                "completion channel dropped (server died)",
            )),
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.rpc
            .notify(method, params)
            .map_err(|err| tool_err(&err.mcp_code(), err.message()))
    }

    fn is_alive(&self) -> bool {
        self.rpc.is_alive() && !self.rpc.child_exited()
    }

    async fn close(&self) {
        self.rpc.shutdown();
    }

    fn diagnostics_tail(&self) -> String {
        self.rpc.stderr_tail()
    }
}

// ============================================================================
// Streamable HTTP transport
// ============================================================================

/// Reject CR/LF in a header name or value (header-injection guard). Config
/// files and server-assigned session ids are both untrusted here.
fn require_header_safe(what: &str, value: &str) -> Result<()> {
    if value.contains(['\r', '\n']) {
        return Err(tool_err(
            "MCP_HEADER_UNSAFE",
            format!("{what} contains CR/LF; refusing to send it as a header"),
        ));
    }
    Ok(())
}

/// Streamable HTTP MCP transport.
///
/// POST per message; the response may be a single JSON document or an SSE
/// stream of JSON-RPC messages. The `Mcp-Session-Id` from the initialize
/// response is replayed on later calls.
pub struct HttpTransport {
    client: crate::http::client::Client,
    url: String,
    headers: Vec<(String, String)>,
    session_id: Mutex<Option<String>>,
    alive: std::sync::atomic::AtomicBool,
    lane: std::sync::Arc<asupersync::sync::Mutex<()>>,
}

impl HttpTransport {
    /// # Errors
    ///
    /// Fails when any custom header name/value contains CR/LF.
    pub fn new(url: &str, headers: Vec<(String, String)>) -> Result<Self> {
        for (name, value) in &headers {
            require_header_safe("header name", name)?;
            require_header_safe(&format!("header {name:?} value"), value)?;
        }
        Ok(Self {
            client: crate::http::client::Client::new(),
            url: url.to_string(),
            headers,
            session_id: Mutex::new(None),
            alive: std::sync::atomic::AtomicBool::new(true),
            lane: std::sync::Arc::new(asupersync::sync::Mutex::new(())),
        })
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// One POST round-trip, returning the JSON-RPC response value.
    async fn round_trip(&self, frame: &Value, timeout: Duration) -> Result<Value> {
        let mut request = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        for (name, value) in &self.headers {
            // ubs:ignore CR/LF-validated at HttpTransport::new (require_header_safe)
            request = request.header(name.clone(), value.clone());
        }
        let assigned_session = { Self::lock(&self.session_id).clone() };
        if let Some(session) = assigned_session {
            // ubs:ignore CR/LF-filtered at capture (hostile-server guard above)
            request = request.header("Mcp-Session-Id", session);
        }
        let response = request
            .json(frame)
            .map_err(|err| tool_err("MCP_TRANSPORT_IO", format!("encode: {err}")))?
            .timeout(timeout)
            .send()
            .await
            .map_err(|err| tool_err("MCP_TRANSPORT_IO", format!("send: {err}")))?;

        let status = response.status();
        // Capture the session id the server assigned (initialize response).
        // A CR/LF-laden id is a hostile-server header-injection attempt:
        // fail closed by ignoring it.
        if let Some(assigned) = response
            .headers()
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("mcp-session-id"))
            .map(|(_, value)| value.clone())
            .filter(|value| !value.contains(['\r', '\n']))
        {
            *Self::lock(&self.session_id) = Some(assigned);
        }
        let content_type = response
            .headers()
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        if status == 202 {
            // Accepted: notification/response acknowledgment, no body.
            return Ok(Value::Null);
        }
        if !(200..300).contains(&status) {
            let body = response.text_limited(4096).await.unwrap_or_default();
            return Err(tool_err(
                "MCP_HTTP_STATUS",
                format!("HTTP {status} from {}: {}", self.url, body.trim()),
            ));
        }
        if content_type.contains("text/event-stream") {
            let body = response
                .text_limited(MAX_HTTP_BODY)
                .await
                .map_err(|err| tool_err("MCP_TRANSPORT_IO", format!("read SSE body: {err}")))?;
            return parse_sse_responses(&body);
        }
        let body = response
            .text_limited(MAX_HTTP_BODY)
            .await
            .map_err(|err| tool_err("MCP_TRANSPORT_IO", format!("read body: {err}")))?;
        let value: Value = serde_json::from_str(&body).map_err(|err| {
            tool_err(
                "MCP_PROTOCOL",
                format!("response is not JSON: {err} (body: {:.200})", body.trim()),
            )
        })?;
        // Unwrap the JSON-RPC envelope, same as the SSE path.
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown server error");
            return Err(tool_err(
                "MCP_SERVER_ERROR",
                format!("server error: {message}"),
            ));
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// Parse an SSE body into the first JSON-RPC message carrying a result or
/// error (legacy SSE accept: events may nest `data:` JSON-RPC frames).
fn parse_sse_responses(body: &str) -> Result<Value> {
    let mut parser = crate::sse::SseParser::new();
    let events = parser.feed(body);
    for event in events {
        for line in event.data.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if value.get("result").is_some() || value.get("error").is_some() {
                if let Some(error) = value.get("error") {
                    let message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown server error");
                    return Err(tool_err(
                        "MCP_SERVER_ERROR",
                        format!("server error: {message}"),
                    ));
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }
    Err(tool_err(
        "MCP_PROTOCOL",
        "SSE stream ended without a JSON-RPC response",
    ))
}

/// Request id counter for the HTTP transport (process-global).
static NEXT_HTTP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[async_trait]
impl McpTransport for HttpTransport {
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let _lane =
            asupersync::sync::OwnedMutexGuard::lock(std::sync::Arc::clone(&self.lane), cx.cx())
                .await
                .map_err(|_| tool_err("MCP_CANCELLED", "cancelled by ambient context"))?;
        let id = NEXT_HTTP_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.round_trip(&frame, timeout).await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.round_trip(&frame, DEFAULT_MCP_TIMEOUT)
            .await
            .map(|_| ())
    }

    fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn close(&self) {
        // Local teardown only: no session DELETE in v1 (server support for
        // it is spotty and the session expires server-side regardless).
        self.alive.store(false, std::sync::atomic::Ordering::SeqCst);
        Self::lock(&self.session_id).take();
    }

    fn diagnostics_tail(&self) -> String {
        format!("http transport to {}", self.url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parse_extracts_result() {
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n";
        let value = parse_sse_responses(body).expect("result");
        assert_eq!(value["tools"], serde_json::json!([]));
    }

    #[test]
    fn sse_parse_surfaces_error() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32601,\"message\":\"no method\"}}\n\n";
        let err = parse_sse_responses(body).expect_err("error");
        assert!(err.to_string().contains("no method"), "{err}");
    }

    #[test]
    fn sse_parse_skips_non_rpc_events() {
        let body = "event: ping\ndata: {}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":42}\n\n";
        let value = parse_sse_responses(body).expect("result after skip");
        assert_eq!(value, serde_json::json!(42));
    }

    #[test]
    fn sse_parse_requires_response() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\n";
        assert!(parse_sse_responses(body).is_err());
    }
}
