//! Legacy MCP HTTP+SSE transport (protocol revision 2024-11-05).
//!
//! The GET stream advertises a same-origin POST endpoint, and carries every
//! JSON-RPC response. It is a connection resource, not a single call's body.
//! An interrupted POST is never replayed: tools may already have run.

use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{FutureExt as _, StreamExt as _};
use serde_json::{Value, json};

use super::super::{auth, config, manager};
use super::{
    ClientRoots, DEFAULT_MCP_TIMEOUT, HTTP_CANCEL_TIMEOUT, HttpTransport, MAX_HTTP_BODY,
    McpTransport, RequestDispatch, is_server_error, redacted_origin,
    reject_authentication_status, tool_err, validate_initialize_result,
    validate_jsonrpc_response, validate_outgoing_params,
};
use crate::error::{Error, Result};
use crate::lsp::jsonrpc::{CompletionWaitError, await_completion};
use crate::mcp::progress::ProgressRegistry;

const MAX_SERVER_REPLIES: usize = 8;
const MAX_ENDPOINT_BYTES: usize = 16 * 1024;

/// A legacy SSE connection with an independent, bounded server-message reader.
pub struct SseTransport {
    state: Arc<SseState>,
    lane: Arc<asupersync::sync::Mutex<()>>,
}

struct SseState {
    client: crate::http::client::Client,
    url: url::Url,
    headers: Vec<(String, String)>,
    authentication: Option<auth::AuthBinding>,
    authentication_failed: AtomicBool,
    endpoint: OnceLock<String>,
    started: AtomicBool,
    endpoint_ready: asupersync::sync::Notify,
    alive: AtomicBool,
    aborted: asupersync::sync::Notify,
    failure: Mutex<Option<String>>,
    pending: Mutex<Option<SsePending>>,
    next_id: AtomicU64,
    progress: Arc<ProgressRegistry>,
    roots: ClientRoots,
    tools_list_changed: AtomicBool,
}

struct SsePending {
    id: u64,
    dispatch: Arc<RequestDispatch>,
    response: SyncSender<Result<Value>>,
}

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl SseTransport {
    /// # Errors
    ///
    /// Rejects malformed URLs, URL credentials, fragments, and unsafe or
    /// protocol-owned custom headers before opening a connection.
    pub fn new(url: &str, headers: Vec<(String, String)>) -> Result<Self> {
        if url.chars().any(|character| character.is_control() || character.is_whitespace())
            || has_url_userinfo(url)
        {
            return Err(tool_err(
                "MCP_CONFIG_INVALID",
                "legacy SSE URL must omit whitespace and credentials",
            ));
        }
        let url = url::Url::parse(url)
            .map_err(|_| tool_err("MCP_CONFIG_INVALID", "invalid legacy SSE URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(tool_err(
                "MCP_CONFIG_INVALID",
                "legacy SSE URL requires HTTP(S), a host, and no credentials or fragment",
            ));
        }
        let headers = config::normalize_http_headers(headers).map_err(|reason| {
            tool_err(
                "MCP_CONFIG_INVALID",
                format!("invalid custom legacy SSE headers: {reason}"),
            )
        })?;
        Ok(Self {
            state: Arc::new(SseState {
                client: crate::http::client::Client::new(),
                url,
                headers,
                authentication: None,
                authentication_failed: AtomicBool::new(false),
                endpoint: OnceLock::new(),
                started: AtomicBool::new(false),
                endpoint_ready: asupersync::sync::Notify::new(),
                alive: AtomicBool::new(true),
                aborted: asupersync::sync::Notify::new(),
                failure: Mutex::new(None),
                pending: Mutex::new(None),
                next_id: AtomicU64::new(1),
                progress: Arc::new(ProgressRegistry::default()),
                roots: ClientRoots::default(),
                tools_list_changed: AtomicBool::new(false),
            }),
            lane: Arc::new(asupersync::sync::Mutex::new(())),
        })
    }

    /// Preserve explicit Authorization headers and bind managed credentials
    /// to the same trusted definition used by Streamable HTTP.
    pub(crate) fn with_authentication(mut self, binding: auth::AuthBinding) -> Result<Self> {
        if self
            .state
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        {
            return Err(tool_err(
                "MCP_AUTH_STATIC_HEADER",
                "managed OAuth cannot replace an explicit Authorization header",
            ));
        }
        let state = Arc::get_mut(&mut self.state).ok_or_else(|| {
            tool_err("MCP_CONFIG_INVALID", "cannot change active SSE authentication")
        })?;
        state.authentication = Some(binding);
        Ok(self)
    }
}

impl Drop for SseTransport {
    fn drop(&mut self) {
        self.state.abort();
    }
}

fn has_url_userinfo(value: &str) -> bool {
    let authority = value
        .strip_prefix("//")
        .or_else(|| value.split_once("://").map(|(_, authority)| authority));
    authority.is_some_and(|authority| {
        authority
            .trim_start_matches('/')
            .split(['/', '?', '#'])
            .next()
            .is_some_and(|authority| authority.contains('@'))
    })
}

/// Check the raw advertised URI before URL normalization can erase controls.
/// Relative paths and query-only references are supported; redirects and
/// endpoints on other origins never receive configured credentials.
fn endpoint_url(base: &url::Url, advertised: &str) -> Result<String> {
    if advertised.is_empty()
        || advertised.len() > MAX_ENDPOINT_BYTES
        || advertised.trim() != advertised
        || advertised.chars().any(|character| character.is_control() || character.is_whitespace())
        || has_url_userinfo(advertised)
        || advertised.contains('\\')
    {
        return Err(tool_err("MCP_PROTOCOL", "invalid legacy SSE endpoint URI"));
    }
    let endpoint = base
        .join(advertised)
        .map_err(|_| tool_err("MCP_PROTOCOL", "invalid legacy SSE endpoint URI"))?;
    if endpoint.origin() != base.origin()
        || !matches!(endpoint.scheme(), "http" | "https")
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(tool_err(
            "MCP_PROTOCOL",
            "legacy SSE endpoint must retain the configured origin and omit credentials and fragments",
        ));
    }
    Ok(endpoint.to_string())
}

/// Bound each wire event, including unfinished data and metadata. Reset only
/// on an empty SSE line, so many small events do not exhaust a lifetime quota.
/// Actual SSE decoding, split UTF-8, and CR/LF semantics stay in SseStream.
#[derive(Default)]
struct EventBudget {
    bytes: usize,
    line_has_bytes: bool,
    previous_cr: bool,
}

impl EventBudget {
    fn check(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        for &byte in chunk {
            self.bytes = self.bytes.saturating_add(1);
            if self.bytes > MAX_HTTP_BODY {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "legacy SSE event exceeds the 10 MiB limit",
                ));
            }
            if byte == b'\n' && self.previous_cr {
                self.previous_cr = false;
                continue;
            }
            self.previous_cr = byte == b'\r';
            if matches!(byte, b'\r' | b'\n') {
                if !self.line_has_bytes {
                    self.bytes = 0;
                }
                self.line_has_bytes = false;
            } else {
                self.line_has_bytes = true;
            }
        }
        Ok(())
    }
}

impl SseState {
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn abort(&self) {
        self.alive.store(false, Ordering::SeqCst);
        lock(&self.pending).take();
        self.aborted.notify_waiters();
        self.endpoint_ready.notify_waiters();
    }

    fn fail(&self, error: &Error) {
        if manager::is_authentication_error(error) {
            self.authentication_failed.store(true, Ordering::Release);
        }
        let mut failure = lock(&self.failure);
        if failure.is_none() {
            *failure = Some(error.to_string().chars().take(512).collect());
        }
        drop(failure);
        self.abort();
    }

    fn closed_error(&self) -> Error {
        if self.authentication_failed.load(Ordering::Acquire) {
            return tool_err("MCP_AUTH_REQUIRED", "legacy SSE authentication is required");
        }
        let detail = lock(&self.failure).clone();
        tool_err(
            "MCP_TRANSPORT_CLOSED",
            detail.map_or_else(
                || "legacy SSE connection closed".to_string(),
                |detail| config::display_safe(&detail),
            ),
        )
    }

    async fn run_until_abort<F, T>(&self, operation: F) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        if !self.is_alive() {
            return Err(self.closed_error());
        }
        let operation = operation.fuse();
        let aborted = self.aborted.wait_until(|| !self.is_alive()).fuse();
        futures::pin_mut!(operation, aborted);
        match futures::future::select(operation, aborted).await {
            futures::future::Either::Left((result, _)) => result,
            futures::future::Either::Right(((), _)) => Err(self.closed_error()),
        }
    }

    async fn authorization(&self, refresh: bool) -> Result<Option<String>> {
        let result = match &self.authentication {
            Some(binding) => binding.authorization_header(refresh).await,
            None => Ok(None),
        };
        if result.as_ref().is_err_and(manager::is_authentication_error) {
            self.authentication_failed.store(true, Ordering::Release);
        }
        result
    }

    fn check_status(&self, status: u16) -> Result<()> {
        if let Err(error) = reject_authentication_status(status) {
            self.authentication_failed.store(true, Ordering::Release);
            return Err(error);
        }
        if !(200..300).contains(&status) {
            // The native client does not follow redirects. Drop their bodies
            // and never resend credentials to a Location target.
            return Err(tool_err(
                "MCP_HTTP_STATUS",
                format!("legacy SSE server returned HTTP {status}"),
            ));
        }
        Ok(())
    }

    async fn connect(self: &Arc<Self>) -> Result<()> {
        if !self.is_alive() {
            return Err(self.closed_error());
        }
        if self.endpoint.get().is_some() {
            return Ok(());
        }
        if !self.started.swap(true, Ordering::AcqRel) {
            let runtime = asupersync::runtime::Runtime::current_handle().ok_or_else(|| {
                tool_err("MCP_RUNTIME_UNAVAILABLE", "legacy SSE requires an active runtime")
            })?;
            let mut request = self
                .client
                .get(self.url.as_str())
                .header("Accept", "text/event-stream");
            for (name, value) in &self.headers {
                request = request.try_header(name.clone(), value.clone())?;
            }
            if let Some(value) = self.authorization(true).await? {
                request = request.try_header("Authorization", value)?;
            }
            if !self.is_alive() {
                return Err(self.closed_error());
            }
            // The caller's absolute deadline covers establishment. The body
            // must outlive that deadline after the endpoint is published.
            let response = request.no_timeout().send().await.map_err(|_| {
                tool_err("MCP_TRANSPORT_IO", "could not open legacy SSE GET stream")
            })?;
            self.check_status(response.status())?;
            let content_type = response
                .headers()
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                .map_or("", |(_, value)| value.as_str());
            if response.status() != 200
                || !content_type
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .eq_ignore_ascii_case("text/event-stream")
            {
                return Err(tool_err(
                    "MCP_PROTOCOL",
                    "legacy SSE GET requires HTTP 200 with text/event-stream",
                ));
            }
            let state = Arc::clone(self);
            runtime
                .try_spawn(async move {
                    let result = Box::pin(state.run_until_abort(state.read_stream(response))).await;
                    if let Err(error) = result {
                        state.fail(&error);
                    }
                })
                .map_err(|_| {
                    tool_err("MCP_RUNTIME_UNAVAILABLE", "could not start legacy SSE reader")
                })?;
        }
        self.endpoint_ready
            .wait_until(|| self.endpoint.get().is_some() || !self.is_alive())
            .await;
        if self.is_alive() && self.endpoint.get().is_some() {
            Ok(())
        } else {
            Err(self.closed_error())
        }
    }

    /// A single POST, with no redirect, reconnect, or authentication replay.
    /// Cleanup may use the captured endpoint after abort, but cannot refresh.
    async fn post(
        &self,
        frame: &Value,
        timeout: Duration,
        dispatch: Option<&RequestDispatch>,
        refresh: bool,
    ) -> Result<()> {
        let endpoint = self.endpoint.get().ok_or_else(|| {
            tool_err("MCP_TRANSPORT_UNAVAILABLE", "legacy SSE endpoint is not established")
        })?;
        let mut request = self
            .client
            .post(endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        for (name, value) in &self.headers {
            request = request.try_header(name.clone(), value.clone())?;
        }
        if let Some(value) = self.authorization(refresh).await? {
            request = request.try_header("Authorization", value)?;
        }
        let request = request.json(frame).map_err(|_| {
            tool_err("MCP_PROTOCOL", "could not encode legacy SSE JSON-RPC message")
        })?;
        if refresh && !self.is_alive() {
            return Err(self.closed_error());
        }
        if let Some(dispatch) = dispatch
            && !dispatch.claim()
        {
            return Err(tool_err("MCP_CANCELLED", "request revoked before SSE POST"));
        }
        let response = request.timeout(timeout).send().await.map_err(|_| {
            tool_err("MCP_TRANSPORT_IO", "legacy SSE POST did not complete")
        })?;
        self.check_status(response.status())?;
        // Legacy servers commonly send a textual 202 Accepted body. It has
        // no JSON-RPC meaning: dropping it avoids an unbounded body wait.
        Ok(())
    }

    async fn read_stream(
        self: &Arc<Self>,
        response: crate::http::client::Response,
    ) -> Result<()> {
        let mut budget = EventBudget::default();
        let bytes = response.bytes_stream().map(move |chunk| {
            chunk.and_then(|chunk| {
                budget.check(&chunk)?;
                Ok(chunk)
            })
        });
        let mut stream = crate::sse::SseStream::new(bytes);
        let mut replies: FuturesUnordered<BoxFuture<'static, Result<()>>> =
            FuturesUnordered::new();
        let mut turns = 0_u8;
        loop {
            if turns == 64 {
                asupersync::runtime::yield_now().await;
                turns = 0;
            }
            turns += 1;
            let next = if replies.is_empty() {
                stream.next().await
            } else {
                // Poll reply POSTs and the GET body concurrently. A server
                // waiting for roots/list must not block other RPC responses.
                match futures::future::select(replies.next(), stream.next()).await {
                    futures::future::Either::Left((reply, _)) => {
                        if let Some(result) = reply {
                            result?;
                        }
                        continue;
                    }
                    futures::future::Either::Right((event, _)) => event,
                }
            };
            let event = next.ok_or_else(|| {
                tool_err("MCP_TRANSPORT_CLOSED", "legacy SSE GET stream ended")
            })?;
            let event = event.map_err(|error| {
                let code = if error.kind() == std::io::ErrorKind::InvalidData {
                    "MCP_PROTOCOL"
                } else {
                    "MCP_TRANSPORT_IO"
                };
                tool_err(code, "legacy SSE stream framing or read failed")
            })?;
            self.handle_event(event, &mut replies)?;
        }
    }

    fn handle_event(
        self: &Arc<Self>,
        event: crate::sse::SseEvent,
        replies: &mut FuturesUnordered<BoxFuture<'static, Result<()>>>,
    ) -> Result<()> {
        if event.event == "endpoint" {
            let endpoint = endpoint_url(&self.url, &event.data)?;
            if let Some(current) = self.endpoint.get() {
                if current != &endpoint {
                    return Err(tool_err("MCP_PROTOCOL", "legacy SSE endpoint changed"));
                }
            } else {
                self.endpoint.set(endpoint).map_err(|_| {
                    tool_err("MCP_PROTOCOL", "legacy SSE endpoint was already established")
                })?;
                self.endpoint_ready.notify_waiters();
            }
            return Ok(());
        }
        if event.event != "message" || event.data.trim().is_empty() {
            return Ok(());
        }
        if self.endpoint.get().is_none() {
            return Err(tool_err(
                "MCP_PROTOCOL",
                "legacy SSE message arrived before the endpoint",
            ));
        }
        let value: Value = serde_json::from_str(&event.data).map_err(|_| {
            tool_err("MCP_PROTOCOL", "legacy SSE message contains invalid JSON")
        })?;
        match value {
            Value::Array(messages) if !messages.is_empty() => {
                for message in messages {
                    self.handle_message(message, replies)?;
                }
                Ok(())
            }
            message => self.handle_message(message, replies),
        }
    }

    fn handle_message(
        self: &Arc<Self>,
        message: Value,
        replies: &mut FuturesUnordered<BoxFuture<'static, Result<()>>>,
    ) -> Result<()> {
        let object = message.as_object().ok_or_else(|| {
            tool_err("MCP_PROTOCOL", "legacy SSE JSON-RPC message must be an object")
        })?;
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(tool_err("MCP_PROTOCOL", "legacy SSE requires JSON-RPC 2.0"));
        }
        if let Some(method) = object.get("method") {
            let method = method
                .as_str()
                .filter(|method| !method.is_empty())
                .ok_or_else(|| tool_err("MCP_PROTOCOL", "invalid legacy SSE RPC method"))?;
            if object.contains_key("result")
                || object.contains_key("error")
                || object.get("params").is_some_and(|params| !params.is_object())
            {
                return Err(tool_err("MCP_PROTOCOL", "invalid legacy SSE request envelope"));
            }
            let Some(id) = object.get("id") else {
                if method == "notifications/progress" {
                    self.progress.publish(&message, 0);
                } else if method == "notifications/tools/list_changed" {
                    self.tools_list_changed.store(true, Ordering::Release);
                }
                return Ok(());
            };
            if !matches!(id, Value::String(_) | Value::Number(_)) {
                return Err(tool_err("MCP_PROTOCOL", "invalid legacy SSE server request id"));
            }
            if replies.len() >= MAX_SERVER_REPLIES {
                return Err(tool_err(
                    "MCP_PROTOCOL",
                    "legacy SSE server exceeded the pending client-response limit",
                ));
            }
            let frame = self.roots.response(method, id);
            let state = Arc::clone(self);
            replies.push(Box::pin(async move {
                HttpTransport::run_with_deadline(
                    state.post(&frame, DEFAULT_MCP_TIMEOUT, None, true),
                    DEFAULT_MCP_TIMEOUT,
                    "legacy SSE server response",
                )
                .await
            }));
            return Ok(());
        }
        let mut pending = lock(&self.pending);
        let waiting = pending.as_ref().ok_or_else(|| {
            tool_err("MCP_PROTOCOL", "unsolicited legacy SSE JSON-RPC response")
        })?;
        if !waiting.dispatch.is_claimed() {
            return Err(tool_err("MCP_PROTOCOL", "legacy SSE response preceded dispatch"));
        }
        let result = validate_jsonrpc_response(&message, waiting.id);
        if result.as_ref().is_err_and(|error| !is_server_error(error)) {
            return result.map(|_| ());
        }
        if let Some(waiting) = pending.take() {
            let _ = waiting.response.try_send(result);
        }
        Ok(())
    }
}

/// Own the connection from lane admission until the request is accounted for.
/// Dropping a caller future revokes unsent work and synchronously retires the
/// GET stream; only an already-dispatched request can produce cancellation.
struct PendingSseRequest {
    state: Arc<SseState>,
    dispatch: Arc<RequestDispatch>,
    id: Option<u64>,
    cancel_allowed: bool,
    runtime: Option<asupersync::runtime::RuntimeHandle>,
    armed: bool,
}

impl PendingSseRequest {
    fn new(
        state: Arc<SseState>,
        dispatch: Arc<RequestDispatch>,
        cancel_allowed: bool,
    ) -> Self {
        Self {
            state,
            dispatch,
            id: None,
            cancel_allowed,
            runtime: asupersync::runtime::Runtime::current_handle(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
        let mut pending = lock(&self.state.pending);
        if pending.as_ref().is_some_and(|pending| Some(pending.id) == self.id) {
            pending.take();
        }
    }
}

impl Drop for PendingSseRequest {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let dispatched = self.dispatch.cancel();
        self.state.abort();
        if dispatched && self.cancel_allowed
            && let (Some(runtime), Some(id)) = (self.runtime.take(), self.id)
        {
            let state = Arc::clone(&self.state);
            let _ = runtime.try_spawn(async move {
                let frame = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": {
                        "requestId": id,
                        "reason": "request timed out or was cancelled",
                    },
                });
                let _ = HttpTransport::run_with_deadline(
                    state.post(&frame, HTTP_CANCEL_TIMEOUT, None, false),
                    HTTP_CANCEL_TIMEOUT,
                    "legacy SSE cancellation",
                )
                .await;
            });
        }
    }
}

async fn request_deadline<F, T>(operation: F, timeout: Duration) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let owner = crate::agent_cx::AgentCx::for_current_or_request();
    let now = owner
        .cx()
        .timer_driver()
        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
    let cancellation = async {
        let (sender, mut receiver) = asupersync::channel::oneshot::channel::<()>();
        // Retain the sender so only this operation's owner cancellation can
        // finish the receive. The persistent SSE reader never captures it.
        let _ = receiver.recv(owner.cx()).await;
        drop(sender);
    }
    .fuse();
    let operation = operation.fuse();
    let deadline = asupersync::time::sleep(now, timeout).fuse();
    futures::pin_mut!(cancellation, deadline, operation);
    futures::select_biased! {
        () = cancellation => Err(tool_err("MCP_CANCELLED", "legacy SSE owner was cancelled")),
        () = deadline => Err(tool_err("MCP_TIMEOUT", "legacy SSE operation exceeded its deadline")),
        result = operation => result,
    }
}

impl SseState {
    fn response_channel(
        &self,
        id: u64,
        dispatch: Arc<RequestDispatch>,
    ) -> Result<Receiver<Result<Value>>> {
        let (sender, receiver) = sync_channel(1);
        let mut pending = lock(&self.pending);
        if pending.is_some() {
            return Err(tool_err("MCP_PROTOCOL", "legacy SSE request lane is occupied"));
        }
        *pending = Some(SsePending {
            id,
            dispatch,
            response: sender,
        });
        Ok(receiver)
    }

    async fn exchange(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        timeout: Duration,
        pending: &mut PendingSseRequest,
        rejected: &AtomicBool,
    ) -> Result<Value> {
        self.connect().await?;
        let id = self
            .next_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| tool_err("MCP_PROTOCOL", "legacy SSE request id space exhausted"))?;
        pending.id = Some(id);
        let response = self.response_channel(id, Arc::clone(&pending.dispatch))?;
        let frame = json!({
            "jsonrpc": "2.0", "id": id, "method": method,
            "params": if params.is_null() { json!({}) } else { params },
        });
        if let Err(error) = self.post(&frame, timeout, Some(&pending.dispatch), true).await {
            // Only a rejection from this POST establishes non-delivery.
            // Authentication failure in an independent server-reply task
            // cannot prove that this already-accepted request did not run.
            if definitive_http_rejection(&error) {
                rejected.store(true, Ordering::Release);
            }
            return Err(error);
        }
        match await_completion(response, timeout, || {}).await {
            Ok(result) => result,
            Err(CompletionWaitError::Timeout) => {
                Err(tool_err("MCP_TIMEOUT", "legacy SSE response timed out"))
            }
            Err(CompletionWaitError::Cancelled) => {
                Err(tool_err("MCP_CANCELLED", "legacy SSE request was cancelled"))
            }
            Err(CompletionWaitError::Closed) => Err(self.closed_error()),
        }
    }
}

#[async_trait]
impl McpTransport for SseTransport {
    fn preferred_protocol_version(&self) -> &'static str {
        "2024-11-05"
    }

    fn configure_workspace_root(&self, workspace: &Path) -> Result<()> {
        self.state.roots.configure(workspace)
    }

    fn client_capabilities(&self) -> Value {
        self.state.roots.capabilities()
    }

    fn progress_registry(&self) -> Option<&Arc<ProgressRegistry>> {
        Some(&self.state.progress)
    }

    fn take_tools_list_changed(&self) -> bool {
        self.state.tools_list_changed.swap(false, Ordering::AcqRel)
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.request_with_dispatch(method, params, timeout, Arc::new(RequestDispatch::new()))
            .await
    }

    async fn request_with_dispatch(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        dispatch: Arc<RequestDispatch>,
    ) -> Result<Value> {
        validate_outgoing_params(method, &params)?;
        let owner = crate::agent_cx::AgentCx::for_current_or_request();
        owner
            .checkpoint()
            .map_err(|_| tool_err("MCP_CANCELLED", "cancelled before legacy SSE dispatch"))?;
        if timeout.is_zero() {
            return Err(tool_err("MCP_TIMEOUT", "legacy SSE deadline elapsed before dispatch"));
        }
        let rejected = AtomicBool::new(false);
        let result = request_deadline(
            async {
                let _lane = asupersync::sync::OwnedMutexGuard::lock(
                    Arc::clone(&self.lane),
                    owner.cx(),
                )
                .await
                .map_err(|_| tool_err("MCP_CANCELLED", "cancelled before legacy SSE admission"))?;
                let mut pending = PendingSseRequest::new(
                    Arc::clone(&self.state),
                    Arc::clone(&dispatch),
                    method != "initialize",
                );
                let result = Box::pin(self.state.run_until_abort(
                    self.state.exchange(method, params, timeout, &mut pending, &rejected),
                ))
                .await;
                match result {
                    Ok(value) => {
                        if method == "initialize" {
                            validate_initialize_result(&value)?;
                        }
                        pending.disarm();
                        Ok(value)
                    }
                    Err(error) if is_server_error(&error) => {
                        pending.disarm();
                        Err(error)
                    }
                    Err(error) if rejected.load(Ordering::Acquire) => {
                        pending.disarm();
                        self.state.fail(&error);
                        Err(error)
                    }
                    Err(error) => {
                        self.state.fail(&error);
                        Err(error)
                    }
                }
            },
            timeout,
        )
        .await;
        match result {
            Err(error)
                if dispatch.may_have_been_sent()
                    && !is_server_error(&error)
                    && !rejected.load(Ordering::Acquire) =>
            {
                Err(tool_err(
                    "MCP_DELIVERY_INDETERMINATE",
                    format!(
                        "legacy SSE request may have executed and was not replayed: {}",
                        config::display_safe(&error.to_string()),
                    ),
                ))
            }
            result => result,
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        validate_outgoing_params(method, &params)?;
        let owner = crate::agent_cx::AgentCx::for_current_or_request();
        request_deadline(
            async {
                let _lane = asupersync::sync::OwnedMutexGuard::lock(
                    Arc::clone(&self.lane),
                    owner.cx(),
                )
                .await
                .map_err(|_| tool_err("MCP_CANCELLED", "cancelled before legacy SSE notification"))?;
                let mut pending = PendingSseRequest::new(
                    Arc::clone(&self.state),
                    Arc::new(RequestDispatch::new()),
                    false,
                );
                let result = Box::pin(self.state.run_until_abort(async {
                    self.state.connect().await?;
                    let frame = json!({
                        "jsonrpc": "2.0", "method": method,
                        "params": if params.is_null() { json!({}) } else { params },
                    });
                    self.state.post(&frame, DEFAULT_MCP_TIMEOUT, None, true).await
                }))
                .await;
                if let Err(error) = &result {
                    self.state.fail(error);
                } else {
                    pending.disarm();
                }
                result
            },
            DEFAULT_MCP_TIMEOUT,
        )
        .await
    }

    fn is_alive(&self) -> bool {
        self.state.is_alive()
    }

    fn authentication_required(&self) -> bool {
        self.state.authentication_failed.load(Ordering::Acquire)
    }

    fn abort(&self) {
        self.state.abort();
    }

    async fn close(&self) {
        self.state.abort();
    }

    fn diagnostics_tail(&self) -> String {
        let mut result = format!("legacy SSE transport to {}", redacted_origin(self.state.url.as_str()));
        if let Some(detail) = lock(&self.state.failure).as_ref() {
            result.push_str(": ");
            result.push_str(&config::display_safe(detail));
        }
        result
    }
}

fn definitive_http_rejection(error: &Error) -> bool {
    manager::is_authentication_error(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, Read as _, Write as _};
    use std::net::{SocketAddr, TcpListener, TcpStream};

    #[derive(Clone, Copy)]
    enum Mode {
        Working,
        ForeignEndpoint,
        MissingEndpoint,
        RedirectGet,
        RedirectPost,
        PostServerError,
        DropAfterPost,
        HoldPost,
        RejectReply,
    }

    #[derive(Clone)]
    struct Captured {
        line: String,
        headers: Vec<(String, String)>,
        body: Value,
    }

    struct Fixture {
        addr: SocketAddr,
        requests: Arc<Mutex<Vec<Captured>>>,
        post_seen: Arc<Mutex<Receiver<()>>>,
        stopping: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    fn capture(stream: &TcpStream) -> Option<Captured> {
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        let mut reader = std::io::BufReader::new(stream.try_clone().ok()?);
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let mut headers = Vec::new();
        let mut length = 0;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).ok()? == 0 {
                return None;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            let (name, value) = header.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>().ok()?;
            }
            headers.push((name.to_string(), value.trim().to_string()));
        }
        if length > MAX_HTTP_BODY {
            return None;
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).ok()?;
        Some(Captured {
            line: line.trim_end().to_string(),
            headers,
            body: if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body).ok()?
            },
        })
    }

    fn chunk(stream: &mut TcpStream, bytes: &[u8]) {
        write!(stream, "{:x}\r\n", bytes.len()).expect("chunk length");
        stream.write_all(bytes).expect("chunk data");
        stream.write_all(b"\r\n").expect("chunk terminator");
        stream.flush().expect("flush SSE");
    }

    fn event(stream: &mut TcpStream, kind: &str, data: &str) {
        chunk(stream, format!("event: {kind}\ndata: {data}\n\n").as_bytes());
    }

    fn message(stream: &mut TcpStream, value: Value) {
        event(stream, "message", &value.to_string());
    }

    fn accepted(stream: &mut TcpStream) {
        stream
            .write_all(
                b"HTTP/1.1 202 Accepted\r\nContent-Length: 8\r\nConnection: close\r\n\r\nAccepted",
            )
            .expect("POST acceptance");
    }

    fn redirect(stream: &mut TcpStream) {
        stream
            .write_all(
                b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .expect("redirect response");
    }

    fn initialized(id: Value) -> Value {
        json!({
            "jsonrpc": "2.0", "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {"listChanged": true}},
                "serverInfo": {"name": "legacy-fixture", "version": "1"},
            },
        })
    }

    impl Fixture {
        #[allow(clippy::too_many_lines)]
        fn start(mode: Mode) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind SSE fixture");
            let addr = listener.local_addr().expect("fixture address");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stopping = Arc::new(AtomicBool::new(false));
            let (post_tx, post_rx) = std::sync::mpsc::channel();
            let recorded = Arc::clone(&requests);
            let stopped = Arc::clone(&stopping);
            let thread = std::thread::spawn(move || {
                let mut events: Option<TcpStream> = None;
                let mut initialize_id = None;
                let mut held_posts = Vec::new();
                loop {
                    let Ok((mut stream, _)) = listener.accept() else {
                        break;
                    };
                    if stopped.load(Ordering::SeqCst) {
                        break;
                    }
                    stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
                    let Some(request) = capture(&stream) else {
                        continue;
                    };
                    lock(&recorded).push(request.clone());
                    if request.line.starts_with("GET ") {
                        if matches!(mode, Mode::RedirectGet) {
                            redirect(&mut stream);
                            continue;
                        }
                        stream
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                            )
                            .expect("SSE response headers");
                        match mode {
                            Mode::ForeignEndpoint => {
                                event(&mut stream, "endpoint", "http://127.0.0.1:1/stolen");
                            }
                            Mode::MissingEndpoint => chunk(&mut stream, b": waiting\n\n"),
                            _ => event(&mut stream, "endpoint", "messages?session=safe"),
                        }
                        events = Some(stream);
                        continue;
                    }
                    let _ = post_tx.send(());
                    if matches!(mode, Mode::HoldPost) {
                        held_posts.push(stream);
                        continue;
                    }
                    if matches!(mode, Mode::RedirectPost) {
                        redirect(&mut stream);
                        continue;
                    }
                    if matches!(mode, Mode::PostServerError) {
                        stream
                            .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .expect("server failure after POST");
                        continue;
                    }
                    if matches!(mode, Mode::RejectReply) && request.body.get("method").is_none() {
                        stream
                            .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .expect("reject client response");
                        continue;
                    }
                    accepted(&mut stream);
                    if matches!(mode, Mode::DropAfterPost) {
                        events.take();
                        continue;
                    }
                    let Some(events) = events.as_mut() else {
                        continue;
                    };
                    let frame = request.body;
                    match frame.get("method").and_then(Value::as_str) {
                        Some("initialize") => {
                            initialize_id = frame.get("id").cloned();
                            // Force a client response while initialization is
                            // still waiting on the independent GET channel.
                            message(events, json!({
                                "jsonrpc": "2.0", "id": "workspace",
                                "method": "roots/list", "params": {},
                            }));
                        }
                        Some("tools/list") => {
                            message(events, json!({
                                "jsonrpc": "2.0", "id": frame["id"],
                                "result": {"tools": [{
                                    "name": "echo",
                                    "description": "Echo",
                                    "inputSchema": {"type": "object"},
                                }]},
                            }));
                        }
                        Some("tools/call") => {
                            if matches!(mode, Mode::RejectReply) {
                                message(events, json!({
                                    "jsonrpc": "2.0", "id": "workspace",
                                    "method": "roots/list", "params": {},
                                }));
                                continue;
                            }
                            message(events, json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/tools/list_changed",
                                "params": {},
                            }));
                            if let Some(token) = frame.pointer("/params/_meta/progressToken") {
                                message(events, json!({
                                    "jsonrpc": "2.0", "method": "notifications/progress",
                                    "params": {"progressToken": token, "progress": 1, "total": 2},
                                }));
                            }
                            let response = json!({
                                "jsonrpc": "2.0", "id": frame["id"],
                                "result": {"content": [{"type": "text", "text": "héllo"}]},
                            });
                            let wire = format!("event: message\ndata: {response}\n\n");
                            let split = wire.find('é').expect("UTF-8 result") + 1;
                            chunk(events, &wire.as_bytes()[..split]);
                            chunk(events, &wire.as_bytes()[split..]);
                        }
                        None if frame.get("id") == Some(&Value::String("workspace".into())) => {
                            message(events, initialized(initialize_id.take().expect("initialize id")));
                        }
                        Some(_) if frame.get("id").is_some() => {
                            message(events, json!({
                                "jsonrpc": "2.0", "id": frame["id"], "result": {},
                            }));
                        }
                        _ => {}
                    }
                }
            });
            Self {
                addr,
                requests,
                post_seen: Arc::new(Mutex::new(post_rx)),
                stopping,
                thread: Some(thread),
            }
        }

        fn url(&self) -> String {
            format!("http://{}/nested/sse?secret=not-for-diagnostics", self.addr)
        }

        fn captured(&self) -> Vec<Captured> {
            lock(&self.requests).clone()
        }

        fn tool_posts(&self) -> usize {
            lock(&self.requests)
                .iter()
                .filter(|request| request.body.get("method").and_then(Value::as_str) == Some("tools/call"))
                .count()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stopping.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(self.addr);
            if let Some(thread) = self.thread.take() {
                thread.join().expect("fixture thread");
            }
        }
    }

    fn runtime() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
    }

    #[test]
    fn legacy_handshake_routes_roots_progress_and_responses_over_get() {
        let fixture = Fixture::start(Mode::Working);
        let transport = SseTransport::new(
            &fixture.url(),
            vec![("Authorization".into(), "Bearer fixture-token".into())],
        )
        .expect("transport");
        transport
            .configure_workspace_root(&std::env::current_dir().expect("workspace"))
            .expect("roots");
        let dispatch = Arc::new(RequestDispatch::new());
        let mut progress = transport
            .progress_registry()
            .expect("progress router")
            .subscribe(Arc::clone(&dispatch), 0)
            .expect("progress subscription");
        let runtime = runtime();
        runtime.block_on(async {
            let result = transport
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": transport.preferred_protocol_version(),
                        "capabilities": transport.client_capabilities(),
                        "clientInfo": {"name": "pi-test", "version": "1"},
                    }),
                    Duration::from_secs(3),
                )
                .await
                .expect("legacy initialize");
            assert_eq!(result["protocolVersion"], "2024-11-05");
            transport
                .notify("notifications/initialized", json!({}))
                .await
                .expect("initialized notification");
            let result = transport
                .request_with_dispatch(
                    "tools/call",
                    json!({"name": "echo", "_meta": {"progressToken": progress.token()}}),
                    Duration::from_secs(3),
                    dispatch,
                )
                .await
                .expect("legacy tool call");
            assert_eq!(result["content"][0]["text"], "héllo");
            assert!(transport.take_tools_list_changed());
            assert!(!transport.take_tools_list_changed());
            assert!(transport.is_alive());
            transport.close().await;
        });
        let waker = futures::task::noop_waker();
        let mut context = std::task::Context::from_waker(&waker);
        let update = match progress.poll_update(&mut context) {
            std::task::Poll::Ready(Some(update)) => update,
            _ => panic!("expected routed tool progress"),
        };
        assert_eq!(update.details.expect("progress details")["mcpProgress"]["progress"], 1);
        let requests = fixture.captured();
        assert!(requests[0].line.starts_with("GET /nested/sse?"));
        let posts: Vec<_> = requests.iter().skip(1).collect();
        assert_eq!(posts.len(), 4, "initialize, roots response, initialized, tool");
        assert!(posts.iter().all(|request| request.line.starts_with("POST /nested/messages?session=safe ")));
        assert!(requests.iter().all(|request| request.headers.iter().any(
            |(name, value)| name.eq_ignore_ascii_case("authorization") && value == "Bearer fixture-token"
        )));
        let roots = posts.iter().find(|request| request.body["id"] == "workspace").expect("roots response");
        assert!(roots.body["result"]["roots"][0]["uri"].as_str().expect("root URI").starts_with("file://"));
        assert!(!transport.diagnostics_tail().contains("not-for-diagnostics"));
    }

    #[test]
    fn imported_omp_sse_configuration_mounts_and_calls_tools_through_manager() {
        for transport_hint in ["sse", " sse "] {
            let fixture = Fixture::start(Mode::Working);
            let workspace = tempfile::tempdir().expect("workspace");
            let config_dir = workspace.path().join(".omp");
            let global_dir = workspace.path().join("global");
            std::fs::create_dir_all(&config_dir).expect("OMP config directory");
            std::fs::create_dir_all(&global_dir).expect("global directory");
            std::fs::write(
                config_dir.join("mcp.json"),
                json!({
                    "mcpServers": {
                        "legacy": {
                            "type": transport_hint,
                            "url": fixture.url(),
                            "timeout": 3000,
                            "headers": {"Authorization": "Bearer fixture-token"},
                        },
                    },
                })
                .to_string(),
            )
            .expect("write imported OMP configuration");
            let manager = manager::McpManager::bootstrap(
                workspace.path(),
                &global_dir,
                &[],
                true,
            )
            .expect("discover imported configuration");
            assert!(manager.warnings().is_empty());
            runtime().block_on(async {
                let tools = manager.trust("legacy").await.expect("initialize and list tools");
                assert_eq!(tools.len(), 1);
                assert_eq!(tools[0].name, "echo");
                let mounted = manager.mounted_tool_metas();
                assert_eq!(mounted.len(), 1);
                assert_eq!(mounted[0].0, "legacy");
                assert_eq!(mounted[0].1[0].name, "echo");
                let result = manager
                    .call_tool("legacy", "echo", json!({}))
                    .await
                    .expect("call imported SSE tool");
                assert_eq!(result["content"][0]["text"], "héllo");
                assert_eq!(manager.shutdown_all().await, vec!["legacy".to_string()]);
            });
            let requests = fixture.captured();
            assert!(requests[0].line.starts_with("GET /nested/sse?"));
            let initialize = requests
                .iter()
                .find(|request| request.body["method"] == "initialize")
                .expect("manager initialization request");
            assert_eq!(
                initialize.body["params"]["protocolVersion"],
                "2024-11-05"
            );
            assert!(requests.iter().skip(1).all(|request| {
                request.line.starts_with("POST /nested/messages?session=safe ")
            }));
            assert!(requests.iter().all(|request| {
                request.headers.iter().any(|(name, value)| {
                    name.eq_ignore_ascii_case("authorization")
                        && value == "Bearer fixture-token"
                })
            }));
            assert_eq!(fixture.tool_posts(), 1);
        }
    }

    #[test]
    fn legacy_endpoint_resolution_preserves_origin_without_url_credentials() {
        let base = url::Url::parse("https://mcp.example.test/stream/sse?key=secret").expect("base");
        assert_eq!(
            endpoint_url(&base, "../messages?session=one").expect("relative endpoint"),
            "https://mcp.example.test/messages?session=one"
        );
        assert_eq!(
            endpoint_url(&base, "?session=two").expect("query endpoint"),
            "https://mcp.example.test/stream/sse?session=two"
        );
        for advertised in [
            "",
            " /messages",
            "/mess ages",
            "/messages\n",
            "https://@mcp.example.test/messages",
            "//@mcp.example.test/messages",
            "/\\foreign.test/messages",
            "//foreign.test/messages",
            "http://mcp.example.test/messages",
            "https://mcp.example.test:8443/messages",
            "https://user:password@mcp.example.test/messages",
            "/messages#secret",
            "javascript:alert(1)",
        ] {
            assert!(endpoint_url(&base, advertised).is_err(), "{advertised:?}");
        }
    }

    #[test]
    fn legacy_foreign_endpoint_and_get_redirect_never_receive_a_post() {
        for mode in [Mode::ForeignEndpoint, Mode::RedirectGet] {
            let fixture = Fixture::start(mode);
            let transport = SseTransport::new(
                &fixture.url(),
                vec![("Authorization".into(), "Bearer private".into())],
            )
            .expect("transport");
            let dispatch = Arc::new(RequestDispatch::new());
            let error = runtime()
                .block_on(transport.request_with_dispatch(
                    "tools/call",
                    json!({"name": "side_effect"}),
                    Duration::from_secs(2),
                    Arc::clone(&dispatch),
                ))
                .expect_err("endpoint must fail closed");
            assert!(!dispatch.may_have_been_sent());
            assert!(!transport.is_alive());
            assert_eq!(fixture.captured().len(), 1, "{error}");
            assert_eq!(fixture.tool_posts(), 0);
            assert!(!error.to_string().contains("Bearer private"));
            assert!(!error.to_string().contains("not-for-diagnostics"));
        }
    }

    #[test]
    fn legacy_endpoint_deadline_aborts_get_without_claiming_dispatch() {
        let fixture = Fixture::start(Mode::MissingEndpoint);
        let transport = SseTransport::new(&fixture.url(), Vec::new()).expect("transport");
        let dispatch = Arc::new(RequestDispatch::new());
        let started = std::time::Instant::now();
        let error = runtime()
            .block_on(transport.request_with_dispatch(
                "tools/call",
                json!({"name": "side_effect"}),
                Duration::from_millis(100),
                Arc::clone(&dispatch),
            ))
            .expect_err("missing endpoint must time out");
        assert!(error.to_string().contains("MCP_TIMEOUT"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(!dispatch.may_have_been_sent());
        assert!(!transport.is_alive());
        assert_eq!(fixture.tool_posts(), 0);
    }

    #[test]
    fn legacy_endpoint_wait_honors_owner_cancellation_before_dispatch() {
        runtime().block_on(async {
            let transport = SseTransport::new("http://127.0.0.1:1/sse", Vec::new())
                .expect("transport");
            // Isolate the state after GET setup but before its endpoint event.
            // This guarantees cancellation exercises the Notify wait itself;
            // the network handshake is covered by the fixture test above.
            transport.state.started.store(true, Ordering::Release);
            let owner = crate::agent_cx::AgentCx::for_request();
            let _current = owner.cx().clone().set_current_restricted();
            let dispatch = Arc::new(RequestDispatch::new());
            let mut request = Box::pin(transport.request_with_dispatch(
                "tools/call",
                json!({"name": "side_effect"}),
                Duration::from_secs(30),
                Arc::clone(&dispatch),
            ));
            let mut context = std::task::Context::from_waker(futures::task::noop_waker_ref());
            assert!(request.as_mut().poll(&mut context).is_pending());
            owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel endpoint wait"));
            let error = match request.as_mut().poll(&mut context) {
                std::task::Poll::Ready(Err(error)) => error,
                _ => panic!("owner cancellation must end an idle endpoint wait immediately"),
            };
            assert!(error.to_string().contains("MCP_CANCELLED"), "{error}");
            assert!(!dispatch.may_have_been_sent());
            assert!(!transport.is_alive());
        });
    }

    #[test]
    fn legacy_stream_loss_after_acceptance_is_never_replayed() {
        let fixture = Fixture::start(Mode::DropAfterPost);
        let transport = SseTransport::new(&fixture.url(), Vec::new()).expect("transport");
        let error = runtime()
            .block_on(transport.request(
                "tools/call",
                json!({"name": "side_effect"}),
                Duration::from_secs(2),
            ))
            .expect_err("accepted request lost its response stream");
        assert!(error.to_string().contains("MCP_DELIVERY_INDETERMINATE"), "{error}");
        assert_eq!(fixture.tool_posts(), 1);
        assert_eq!(fixture.captured().iter().filter(|request| request.line.starts_with("GET ")).count(), 1);
        assert!(!transport.is_alive());
    }

    #[test]
    fn legacy_post_http_failures_are_not_followed_or_replayed() {
        for mode in [Mode::RedirectPost, Mode::PostServerError] {
            let fixture = Fixture::start(mode);
            let transport = SseTransport::new(
                &fixture.url(),
                vec![("Authorization".into(), "Bearer private".into())],
            )
            .expect("transport");
            let error = runtime()
                .block_on(transport.request(
                    "tools/call",
                    json!({"name": "side_effect"}),
                    Duration::from_secs(2),
                ))
                .expect_err("POST status cannot prove that a tool did not execute");
            assert!(error.to_string().contains("MCP_DELIVERY_INDETERMINATE"), "{error}");
            assert!(error.to_string().contains("MCP_HTTP_STATUS"), "{error}");
            assert_eq!(fixture.tool_posts(), 1);
            assert_eq!(fixture.captured().iter().filter(|request| request.line.starts_with("GET ")).count(), 1);
            assert!(!transport.is_alive());
        }
    }

    #[test]
    fn legacy_abort_interrupts_post_without_waiting_for_its_headers() {
        let fixture = Fixture::start(Mode::HoldPost);
        let transport = Arc::new(SseTransport::new(&fixture.url(), Vec::new()).expect("transport"));
        let post_seen = Arc::clone(&fixture.post_seen);
        let aborting = Arc::clone(&transport);
        let aborter = std::thread::spawn(move || {
            let observed = lock(&post_seen).recv_timeout(Duration::from_secs(2)).is_ok();
            aborting.abort();
            observed
        });
        let started = std::time::Instant::now();
        let error = runtime()
            .block_on(transport.request(
                "tools/call",
                json!({"name": "side_effect"}),
                Duration::from_secs(30),
            ))
            .expect_err("abort must stop the blocked POST");
        assert!(aborter.join().expect("abort thread"));
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(error.to_string().contains("MCP_DELIVERY_INDETERMINATE"), "{error}");
        assert_eq!(fixture.tool_posts(), 1);
        assert!(!transport.is_alive());
    }

    #[test]
    fn legacy_revoked_dispatch_never_posts_tool_work() {
        let fixture = Fixture::start(Mode::Working);
        let transport = SseTransport::new(&fixture.url(), Vec::new()).expect("transport");
        let dispatch = Arc::new(RequestDispatch::new());
        assert!(dispatch.revoke_unsent());
        let error = runtime()
            .block_on(transport.request_with_dispatch(
                "tools/call",
                json!({"name": "side_effect"}),
                Duration::from_secs(2),
                dispatch,
            ))
            .expect_err("revoked request must not be posted");
        assert!(error.to_string().contains("MCP_CANCELLED"), "{error}");
        assert_eq!(fixture.tool_posts(), 0);
    }

    #[test]
    fn legacy_reply_auth_failure_keeps_accepted_tool_delivery_indeterminate() {
        let fixture = Fixture::start(Mode::RejectReply);
        let transport = SseTransport::new(&fixture.url(), Vec::new()).expect("transport");
        let error = runtime()
            .block_on(transport.request(
                "tools/call",
                json!({"name": "side_effect"}),
                Duration::from_secs(2),
            ))
            .expect_err("server reply authentication failure must retire the stream");
        assert!(error.to_string().contains("MCP_DELIVERY_INDETERMINATE"), "{error}");
        assert!(transport.authentication_required());
        assert_eq!(fixture.tool_posts(), 1);
        assert!(!transport.is_alive());
    }

    #[test]
    fn legacy_stream_budget_is_per_event_and_handles_crlf_boundaries() {
        let data = "x".repeat(64 * 1024);
        let wire = format!("data: {data}\r\n\r\n").into_bytes();
        let events = MAX_HTTP_BODY / wire.len() + 2;
        let chunks = futures::stream::iter(
            (0..events).flat_map(move |_| {
                // Split the CRLF delimiter itself across byte-stream chunks.
                let split = wire.len() - 1;
                [Ok(wire[..split].to_vec()), Ok(wire[split..].to_vec())]
            }),
        );
        let mut budget = EventBudget::default();
        let bytes = chunks.map(move |chunk: std::io::Result<Vec<u8>>| {
            chunk.and_then(|chunk| {
                budget.check(&chunk)?;
                Ok(chunk)
            })
        });
        let count = futures::executor::block_on(async {
            let mut stream = crate::sse::SseStream::new(bytes);
            let mut count = 0;
            while let Some(event) = stream.next().await {
                assert_eq!(event.expect("bounded event").data.len(), 64 * 1024);
                count += 1;
            }
            count
        });
        assert_eq!(count, events, "a long-lived stream must not have a lifetime byte cap");
        let mut oversized = EventBudget::default();
        assert!(oversized.check(&vec![b'x'; MAX_HTTP_BODY + 1]).is_err());
    }
}
