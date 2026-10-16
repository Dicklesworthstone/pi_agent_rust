//! Native, bounded CDP transport. A connection belongs to one tool operation;
//! cancellation drops it instead of reusing a possibly partially written frame.

use super::{
    BrowserLaunchOptions, BrowserTabInfo, dialog, download, exports, frames, interaction, launch,
    output, policy, required, storage,
};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use asupersync::net::TcpStream;
use asupersync::net::websocket::{Message, WebSocket, WebSocketConfig};
use futures::future::{Either, select};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

mod contexts;
mod diagnostics;

const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_EVENTS: usize = 8192;
const MAX_DOWNLOAD_RECORDS: usize = 128;
const MAX_REFERENCE_DOCUMENTS: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct DownloadRecord {
    pub(super) guid: String,
    pub(super) frame_id: String,
    pub(super) url: String,
    pub(super) suggested_filename: String,
    pub(super) state: String,
    pub(super) received_bytes: f64,
    pub(super) total_bytes: f64,
}

/// The download records whose guid was not already present in `before`.
fn downloads_since_in(
    downloads: &BTreeMap<String, DownloadRecord>,
    before: &BTreeSet<String>,
) -> Vec<DownloadRecord> {
    downloads
        .iter()
        .filter(|(guid, _)| !before.contains(*guid))
        .map(|(_, record)| record.clone())
        .collect()
}

/// Validate each event completely before changing retained state. A repeated
/// start may acknowledge the same transfer, never replace its source or reset
/// terminal progress. The connection also latches errors so best-effort cleanup
/// cannot hide a malformed event from the eventual publication gate.
fn record_download_event_into(
    downloads: &mut BTreeMap<String, DownloadRecord>,
    value: &Value,
) -> Result<()> {
    let method = value["method"].as_str().unwrap_or_default();
    if method == "Browser.downloadWillBegin" {
        let params = &value["params"];
        let guid = required(params, "guid")?;
        let frame_id = required(params, "frameId")?;
        let url = required(params, "url")?;
        let suggested = required(params, "suggestedFilename")?;
        if guid.is_empty()
            || guid.len() > 128
            || guid.chars().any(char::is_control)
            || frame_id.is_empty()
            || frame_id.len() > 256
            || frame_id.chars().any(char::is_control)
            || url.is_empty()
            || url.len() > 64 * 1024
            || url.chars().any(char::is_control)
            || suggested.len() > 4096
            || suggested.chars().any(char::is_control)
        {
            return Err(Error::tool("browser", "invalid bounded download metadata"));
        }
        if let Some(existing) = downloads.get(guid) {
            if existing.frame_id == frame_id
                && existing.url == url
                && existing.suggested_filename == suggested
            {
                return Ok(());
            }
            return Err(Error::tool(
                "browser",
                "download identity changed after its initial event",
            ));
        }
        if downloads.len() >= MAX_DOWNLOAD_RECORDS {
            return Err(Error::tool(
                "browser",
                "too many browser downloads are being tracked",
            ));
        }
        downloads.insert(
            guid.to_owned(),
            DownloadRecord {
                guid: guid.to_owned(),
                frame_id: frame_id.to_owned(),
                url: url.to_owned(),
                suggested_filename: suggested.to_owned(),
                state: "inProgress".into(),
                received_bytes: 0.0,
                total_bytes: 0.0,
            },
        );
    } else if method == "Browser.downloadProgress" {
        let params = &value["params"];
        let guid = required(params, "guid")?;
        if let Some(record) = downloads.get_mut(guid) {
            let state = required(params, "state")?;
            if !matches!(state, "inProgress" | "completed" | "canceled") {
                return Err(Error::tool("browser", "invalid browser download state"));
            }
            let number = |name: &str| {
                params[name]
                    .as_f64()
                    .filter(|value| value.is_finite() && *value >= 0.0)
                    .ok_or_else(|| Error::tool("browser", format!("invalid download {name}")))
            };
            let received = number("receivedBytes")?;
            let total = number("totalBytes")?;
            // totalBytes is an estimate and may change (including from zero).
            // Received bytes must not retreat, and terminal events are final.
            if received < record.received_bytes
                || (record.state != "inProgress"
                    && (record.state != state
                        || (record.received_bytes - received).abs() > 0.0
                        || (record.total_bytes - total).abs() > 0.0))
            {
                return Err(Error::tool(
                    "browser",
                    "download progress regressed or changed after termination",
                ));
            }
            state.clone_into(&mut record.state);
            record.received_bytes = received;
            record.total_bytes = total;
        }
        // A progress event without a start on this connection is not a new
        // downloadable artifact. It may belong to a pre-existing browser job.
    }
    Ok(())
}

#[derive(Default)]
pub(super) struct Session {
    tabs: BTreeMap<String, String>,
    active: Option<String>,
    endpoint: Option<String>,
    references: BTreeMap<(String, String), interaction::References>,
    next_ref: u64,
    // Field order matters: stop the owned browser before dropping upload copies.
    browser: Option<launch::ManagedBrowser>,
    uploads: interaction::upload::Store,
    downloads_denied: bool,
    pending_download_context: Option<contexts::Context>,
}

fn validate(args: &Value, allowlist: Option<&[String]>) -> Result<u64> {
    let action = required(args, "action")?;
    frames::validate(args)?;
    dialog::validate_action(args)?;
    dialog::Expected::from_args(args, allowlist)?;
    diagnostics::validate(args)?;
    match action {
        "start" | "status" | "stop" => {
            if args.as_object().is_some_and(|object| {
                object
                    .keys()
                    .any(|key| !matches!(key.as_str(), "action" | "timeout_ms"))
            }) {
                return Err(Error::tool(
                    "browser",
                    "lifecycle actions accept only action and timeout_ms; launch configuration belongs to the host",
                ));
            }
        }
        action if storage::is_action(action) => storage::validate(args)?,
        action if interaction::forms::is_action(action) => interaction::forms::validate(args)?,
        "upload" => {
            interaction::upload::validate(args)?;
        }
        "download" => download::validate(args)?,
        "screenshot" | "print_pdf" => exports::validate(args)?,
        "open" | "goto" => policy::check_navigation(required(args, "url")?, allowlist)?,
        "evaluate" => {
            required(args, "script")?;
        }
        "handle_dialog" | "close" | "list_tabs" | "list_frames" | "snapshot" | "ax_tree"
        | "diagnostics" => {}
        "click" | "wait_for" => {
            required(args, "selector")?;
        }
        "hover" | "drag" => {
            interaction::pointer::validate_input(args)?;
        }
        "type" | "fill" => {
            required(args, "selector")?;
            required(args, "text")?;
        }
        "press" => {
            interaction::key_event(required(args, "key")?)?;
        }
        "scroll" => {
            interaction::delta(args, "delta_x", 0.0)?;
            interaction::delta(args, "delta_y", 600.0)?;
        }
        _ => return Err(Error::tool("browser", format!("unknown action: {action}"))),
    }
    let timeout_ms = match args.get("timeout_ms") {
        None => {
            if action == "wait_for" {
                5000
            } else {
                30_000
            }
        }
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=120_000).contains(n))
            .ok_or_else(|| Error::tool("browser", "timeout_ms must be an integer in 1..=120000"))?,
    };
    if let Some(tab) = args.get("tab")
        && tab.as_str().is_none_or(|s| s.is_empty() || s.len() > 256)
    {
        return Err(Error::tool(
            "browser",
            "tab must be a nonempty string of at most 256 bytes",
        ));
    }
    for field in ["script", "text", "selector", "output_path"] {
        if let Some(value) = args.get(field)
            && value.as_str().is_none_or(|s| {
                s.len() > 64 * 1024 || (matches!(field, "selector" | "output_path") && s.is_empty())
            })
        {
            return Err(Error::tool(
                "browser",
                format!(
                    "{field} must be a string of at most 64 KiB (selectors and paths cannot be empty)"
                ),
            ));
        }
    }
    Ok(timeout_ms)
}

pub(super) async fn execute(
    state: &std::sync::Arc<asupersync::sync::Mutex<Session>>,
    endpoint_override: Option<&str>,
    launch_options: Option<&BrowserLaunchOptions>,
    cwd: &Path,
    allowlist: Option<&[String]>,
    args: &Value,
) -> Result<ToolOutput> {
    let timeout_ms = validate(args, allowlist)?;
    let connection = launch::Connection::resolve(endpoint_override, launch_options)?;
    storage::validate_connection(args, &connection)?;
    let owner = AgentCx::for_current_or_request();
    let caps = owner.capabilities();
    if !caps.io || !caps.time || !caps.entropy {
        return Err(Error::tool(
            "browser",
            "CDP requires I/O, timer and entropy capabilities",
        ));
    }
    owner
        .checkpoint()
        .map_err(|_| Error::tool("browser", "browser operation cancelled"))?;
    let operation = async {
        let mut state =
            asupersync::sync::OwnedMutexGuard::lock(std::sync::Arc::clone(state), owner.cx())
                .await
                .map_err(|e| Error::tool("browser", format!("browser session lock: {e}")))?;
        let action = required(args, "action")?;
        let connected = state.connect(&owner, cwd, &connection, action).await?;
        if matches!(action, "start" | "status" | "stop") {
            let running = connected.is_some();
            let mode = match connection {
                launch::Connection::Attach(_) => "attached",
                launch::Connection::Managed(_) => "managed",
            };
            return Ok(output(
                format!(
                    "Browser {mode}: {}",
                    if running { "running" } else { "stopped" }
                ),
                json!({
                    "backend": "cdp", "mode": mode, "running": running,
                    "owned": state.browser.is_some(),
                    "process_id": state.browser.as_ref().map(launch::ManagedBrowser::id),
                    "endpoint": state.endpoint,
                }),
            ));
        }
        let mut cdp = connected.ok_or_else(|| Error::tool("browser", "browser is not running"))?;
        cdp.timeout_ms = timeout_ms;
        let result = state.execute(&owner, &mut cdp, cwd, allowlist, args).await;
        diagnostics::finish(&owner, &mut cdp, result).await
    };
    let cancelled = async {
        let (sender, mut receiver) = asupersync::channel::oneshot::channel::<()>();
        let _ = receiver.recv(owner.cx()).await;
        drop(sender);
    };
    let watchdog = async {
        let delay = async {
            owner.time().sleep(Duration::from_millis(timeout_ms)).await;
        };
        match select(Box::pin(delay), Box::pin(cancelled)).await {
            Either::Left(_) => {
                "browser operation timed out; remote side effects may already have occurred"
            }
            Either::Right(_) => {
                "browser operation cancelled; remote side effects may already have occurred"
            }
        }
    };
    match select(Box::pin(operation), Box::pin(watchdog)).await {
        Either::Left((result, _)) => result,
        Either::Right((message, pending)) => {
            drop(pending);
            Err(Error::tool("browser", message))
        }
    }
}

pub(super) struct Cdp {
    socket: WebSocket<TcpStream>,
    next_id: u64,
    session_id: Option<String>,
    frame_scope: Option<frames::Scope>,
    context_binding: Option<contexts::Binding>,
    pub(super) frame_targets: frames::Targets,
    loaded: BTreeSet<(String, String)>,
    downloads: BTreeMap<String, DownloadRecord>,
    download_failure: bool,
    timeout_ms: u64,
    dialog: dialog::State,
    diagnostics: Option<diagnostics::Capture>,
}

impl Cdp {
    async fn connect(
        owner: &AgentCx,
        endpoint: &url::Url,
        expected_debugger_path: Option<&str>,
    ) -> Result<Self> {
        let client = owner.http().client();
        let response = client.get(&format!("{}json/version", endpoint.as_str()))
            .timeout(Duration::from_secs(5)).send().await
            .map_err(|e| Error::tool("browser", format!("cannot attach to Chromium at {endpoint}: {e}. Check the explicitly configured endpoint or the managed browser process")))?;
        if response.status() != 200 {
            return Err(Error::tool(
                "browser",
                format!("CDP discovery returned HTTP {}", response.status()),
            ));
        }
        let version: Value = serde_json::from_slice(&response.bytes_limited(64 * 1024).await?)
            .map_err(|e| Error::tool("browser", format!("invalid CDP discovery JSON: {e}")))?;
        let advertised = required(&version, "webSocketDebuggerUrl")?;
        let mut websocket = policy::endpoint(advertised, true)?;
        if websocket.port_or_known_default() != endpoint.port_or_known_default() {
            return Err(Error::tool(
                "browser",
                "CDP discovery changed the endpoint port",
            ));
        }
        if expected_debugger_path.is_some_and(|path| path != websocket.path()) {
            return Err(Error::tool(
                "browser",
                "CDP discovery does not match the owned browser's DevToolsActivePort identity",
            ));
        }
        websocket
            .set_host(endpoint.host_str())
            .map_err(|_| Error::tool("browser", "invalid CDP host"))?;
        let config = WebSocketConfig::default()
            .max_frame_size(MAX_MESSAGE_BYTES)
            .max_message_size(MAX_MESSAGE_BYTES)
            .connect_timeout(Some(Duration::from_secs(5)));
        let socket = WebSocket::connect_with_config(owner.cx(), websocket.as_str(), config)
            .await
            .map_err(|e| Error::tool("browser", format!("CDP WebSocket connection failed: {e}")))?;
        Ok(Self {
            socket,
            next_id: 0,
            session_id: None,
            frame_scope: None,
            context_binding: None,
            frame_targets: frames::Targets::default(),
            loaded: BTreeSet::new(),
            downloads: BTreeMap::new(),
            download_failure: false,
            timeout_ms: 30_000,
            dialog: dialog::State::default(),
            diagnostics: None,
        })
    }

    async fn receive(&mut self, owner: &AgentCx) -> Result<Value> {
        loop {
            match self
                .socket
                .recv(owner.cx())
                .await
                .map_err(|e| Error::tool("browser", format!("CDP receive failed: {e}")))?
            {
                Some(Message::Text(text)) => {
                    let value: Value = serde_json::from_str(&text)
                        .map_err(|e| Error::tool("browser", format!("invalid CDP JSON: {e}")))?;
                    if !value.is_object() {
                        return Err(Error::tool("browser", "CDP message must be an object"));
                    }
                    if let Some(capture) = self.diagnostics.as_mut() {
                        capture.observe(&value);
                    }
                    if value["method"] == "Page.lifecycleEvent"
                        && matches!(
                            value["params"]["name"].as_str(),
                            Some("DOMContentLoaded" | "load")
                        )
                        && let (Some(frame), Some(loader)) = (
                            value["params"]["frameId"].as_str(),
                            value["params"]["loaderId"].as_str(),
                        )
                    {
                        if self.loaded.len() >= 256 {
                            self.loaded.clear();
                        }
                        self.loaded.insert((frame.into(), loader.into()));
                    }
                    self.record_download_event(&value)?;
                    self.frame_targets
                        .observe(&value, self.session_id.as_deref())?;
                    if value["method"] == "Inspector.targetCrashed" {
                        return Err(Error::tool("browser", "browser target crashed"));
                    }
                    return Ok(value);
                }
                Some(Message::Ping(_) | Message::Pong(_)) => {}
                Some(Message::Binary(_)) => {
                    return Err(Error::tool("browser", "unexpected binary CDP message"));
                }
                Some(Message::Close(_)) | None => {
                    return Err(Error::tool(
                        "browser",
                        "CDP connection closed before the command completed",
                    ));
                }
            }
        }
    }

    async fn send_request(
        &mut self,
        owner: &AgentCx,
        method: &str,
        params: Value,
        page: bool,
    ) -> Result<u64> {
        let session = if page {
            Some(self.page_session_id()?.to_owned())
        } else {
            None
        };
        self.send_session_request(owner, method, params, session.as_deref())
            .await
    }

    async fn send_session_request(
        &mut self,
        owner: &AgentCx,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<u64> {
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| Error::tool("browser", "CDP request ID exhausted"))?;
        let id = self.next_id;
        let mut request = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            request["sessionId"] = json!(session);
        }
        self.socket
            .send(owner.cx(), Message::text(request.to_string()))
            .await
            .map_err(|e| Error::tool("browser", format!("CDP send failed: {e}")))?;
        Ok(id)
    }

    /// Handle only this connection's expected dialog. Never recurse through
    /// call(): the original command can reply before the dialog acknowledgement.
    async fn process_dialog(
        &mut self,
        owner: &AgentCx,
        value: &Value,
        explicit_response: bool,
    ) -> Result<bool> {
        if self.dialog.acknowledge(value, self.session_id.as_deref())? {
            return Ok(true);
        }
        if let Some(params) =
            self.dialog
                .response_for(value, self.session_id.as_deref(), explicit_response)?
        {
            owner.checkpoint().map_err(|_| dialog::response_failed())?;
            let accepted = params["accept"] == true;
            let id = self
                .send_request(owner, "Page.handleJavaScriptDialog", params, true)
                .await
                .map_err(|_| dialog::response_failed())?;
            self.dialog.record_sent(id, accepted);
            return Ok(true);
        }
        Ok(false)
    }

    async fn call(
        &mut self,
        owner: &AgentCx,
        method: &str,
        params: Value,
        page: bool,
    ) -> Result<Value> {
        let id = self.send_request(owner, method, params, page).await?;
        let mut primary = None;
        for _ in 0..MAX_EVENTS {
            let response = self.receive(owner).await?;
            if !self
                .process_dialog(owner, &response, method == "Page.handleJavaScriptDialog")
                .await?
                && response["id"].as_u64() == Some(id)
            {
                if primary.is_some() {
                    return Err(Error::tool("browser", "duplicate CDP command response"));
                }
                if page
                    && (self.dialog.requested() || method == "Page.handleJavaScriptDialog")
                    && response["sessionId"].as_str() != self.session_id.as_deref()
                {
                    return Err(dialog::response_failed());
                }
                primary = Some(response);
            }
            if !self.dialog.pending()
                && let Some(response) = primary.take()
            {
                if let Some(error) = response.get("error") {
                    return Err(Error::tool("browser", format!("{method} failed: {error}")));
                }
                return response
                    .get("result")
                    .filter(|v| v.is_object())
                    .cloned()
                    .ok_or_else(|| {
                        Error::tool(
                            "browser",
                            format!("{method} response is missing its result"),
                        )
                    });
            }
        }
        Err(Error::tool(
            "browser",
            "too many CDP events without a command response",
        ))
    }

    async fn finish_expected_dialog(&mut self, owner: &AgentCx) -> Result<()> {
        // Input acknowledgement can precede the page's asynchronous dialog.
        // Keep the same socket and total operation deadline until it is handled.
        for _ in 0..MAX_EVENTS {
            if self.dialog.completed() {
                return Ok(());
            }
            let value = self.receive(owner).await?;
            self.process_dialog(owner, &value, false).await?;
        }
        if self.dialog.completed() {
            return Ok(());
        }
        Err(Error::tool(
            "browser",
            "too many CDP events waiting for the expected dialog",
        ))
    }

    pub(super) async fn command(
        &mut self,
        owner: &AgentCx,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        if let Some(scope) = self.frame_scope.clone() {
            // Bypass this routing entrypoint while checking the ancestry:
            // check_current uses explicit session commands, so validation
            // cannot recurse through command() or fall back to the page.
            scope.check_current(owner, self).await?;
            let session = scope.command_session(method)?;
            let response = self.frame_command(owner, session, method, params).await?;
            scope.check_targets(self)?;
            return Ok(response);
        }
        self.call(owner, method, params, true).await
    }

    pub(super) async fn browser_command(
        &mut self,
        owner: &AgentCx,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        self.call(owner, method, params, false).await
    }

    fn current_download_context(&self) -> contexts::Context {
        self.context_binding.as_ref().map_or(contexts::Context::Default, |binding| {
            binding.context.clone()
        })
    }

    pub(super) fn download_context_metadata(&self) -> Value {
        self.current_download_context().metadata()
    }

    pub(super) async fn check_download_context(&mut self, owner: &AgentCx) -> Result<()> {
        if let Some(binding) = self.context_binding.clone() {
            binding.check(owner, self).await?;
        }
        Ok(())
    }

    /// Policy and cancellation use the original page's browser context, not
    /// its DOM renderer session. Cleanup never falls back to another profile.
    pub(super) async fn download_command(
        &mut self,
        owner: &AgentCx,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        self.current_download_context().command(owner, self, method, params).await
    }

    pub(super) fn page_session_id(&self) -> Result<&str> {
        self.session_id
            .as_deref()
            .filter(|session| {
                !session.is_empty()
                    && session.len() <= 256
                    && !session.chars().any(char::is_control)
            })
            .ok_or_else(|| Error::tool("browser", "no attached page"))
    }

    /// Route to a live related frame without changing the page attachment.
    /// Frame operations never accept dialogs implicitly. The outer operation
    /// owns the deadline/cancellation and drops every attachment with the socket.
    pub(super) async fn frame_command(
        &mut self,
        owner: &AgentCx,
        session: &str,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        if !self
            .frame_targets
            .admits(self.session_id.as_deref(), session)
        {
            return Err(Error::tool(
                "browser",
                "frame session is no longer attached",
            ));
        }
        owner
            .checkpoint()
            .map_err(|_| Error::tool("browser", "browser operation cancelled"))?;
        let id = self
            .send_session_request(owner, method, params, Some(session))
            .await?;
        for _ in 0..MAX_EVENTS {
            let response = self.receive(owner).await?;
            if !self
                .frame_targets
                .admits(self.session_id.as_deref(), session)
            {
                return Err(Error::tool(
                    "browser",
                    "frame detached during browser operation",
                ));
            }
            if response["method"] == "Page.javascriptDialogOpening"
                && response["sessionId"].as_str().is_some_and(|session| {
                    self.frame_targets
                        .admits(self.session_id.as_deref(), session)
                })
            {
                return Err(Error::tool(
                    "browser",
                    "a dialog interrupted the frame operation; handle it explicitly",
                ));
            }
            if let Some(result) = frame_command_result(&response, id, session)? {
                return Ok(result);
            }
        }
        Err(Error::tool(
            "browser",
            "too many CDP events without a frame command response",
        ))
    }

    pub(super) async fn pump_event(&mut self, owner: &AgentCx) -> Result<()> {
        self.receive(owner).await.map(|_| ())
    }

    pub(super) fn download_ids(&self) -> BTreeSet<String> {
        self.downloads.keys().cloned().collect()
    }

    pub(super) fn downloads_since(&self, before: &BTreeSet<String>) -> Vec<DownloadRecord> {
        downloads_since_in(&self.downloads, before)
    }

    pub(super) fn download_record(&self, guid: &str) -> Option<DownloadRecord> {
        self.downloads.get(guid).cloned()
    }

    pub(super) fn check_download_events(&self) -> Result<()> {
        if self.download_failure {
            return Err(Error::tool(
                "browser",
                "download event validation failed; no artifact may be published",
            ));
        }
        Ok(())
    }

    fn record_download_event(&mut self, value: &Value) -> Result<()> {
        let result = record_download_event_into(&mut self.downloads, value);
        self.download_failure |= result.is_err();
        result
    }

    pub(super) const fn frame_selected(&self) -> bool {
        self.frame_scope.is_some()
    }

    /// Resolve only the pinned frame, or the main document when no frame was
    /// requested. Never substitute the root after a detach or navigation.
    pub(super) async fn frame_document(&mut self, owner: &AgentCx) -> Result<Value> {
        if let Some(scope) = self.frame_scope.clone() {
            return scope.check_current(owner, self).await;
        }
        let tree = self.command(owner, "Page.getFrameTree", json!({})).await?;
        tree["frameTree"]
            .get("frame")
            .cloned()
            .ok_or_else(|| Error::tool("browser", "page has no main-frame document"))
    }

    pub(super) async fn evaluate(&mut self, owner: &AgentCx, expression: &str) -> Result<Value> {
        let context = self.frame_scope.as_ref().map(frames::Scope::context_id);
        if context.is_some() {
            self.frame_document(owner).await?;
        }
        let mut params = json!({
            "expression": expression, "returnByValue": true, "awaitPromise": true,
            "timeout": self.timeout_ms, "allowUnsafeEvalBlockedByCSP": false
        });
        if let Some(context) = context {
            params["contextId"] = json!(context);
        }
        let response = self.command(owner, "Runtime.evaluate", params).await?;
        let value = evaluation_value(&response)?;
        if context.is_some() {
            self.frame_document(owner).await?;
        }
        Ok(value)
    }

    async fn navigate(&mut self, owner: &AgentCx, url: &str) -> Result<()> {
        self.command(owner, "Page.enable", json!({})).await?;
        self.command(
            owner,
            "Page.setLifecycleEventsEnabled",
            json!({"enabled": true}),
        )
        .await?;
        let navigation = self
            .command(owner, "Page.navigate", json!({"url": url}))
            .await?;
        if let Some(error) = navigation
            .get("errorText")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Err(Error::tool(
                "browser",
                format!("navigation failed: {error}"),
            ));
        }
        if navigation["isDownload"] == true {
            return Err(Error::tool(
                "browser",
                "navigation started a download, not a loaded page",
            ));
        }
        if let Some(loader) = navigation.get("loaderId").and_then(Value::as_str) {
            let frame = required(&navigation, "frameId")?;
            let key = (frame.to_owned(), loader.to_owned());
            for _ in 0..MAX_EVENTS {
                if self.loaded.contains(&key) {
                    return Ok(());
                }
                self.receive(owner).await?;
            }
            return Err(Error::tool(
                "browser",
                "navigation did not reach DOMContentLoaded",
            ));
        }
        Ok(())
    }
}

fn frame_command_result(response: &Value, id: u64, session: &str) -> Result<Option<Value>> {
    if response["id"].as_u64() != Some(id) {
        return Ok(None);
    }
    if response["sessionId"].as_str() != Some(session) {
        return Err(Error::tool(
            "browser",
            "CDP frame response has the wrong session identity",
        ));
    }
    if response.get("error").is_some() {
        // A remote diagnostic can contain page contents or credentials. It is
        // not safe to echo it as a browser-tool error or treat it as a fallback.
        return Err(Error::tool(
            "browser",
            "CDP frame command failed; refresh frames before retrying",
        ));
    }
    response
        .get("result")
        .filter(|value| value.is_object())
        .cloned()
        .map(Some)
        .ok_or_else(|| Error::tool("browser", "CDP frame response has no object result"))
}

pub(super) fn evaluation_value(response: &Value) -> Result<Value> {
    if let Some(exception) = response.get("exceptionDetails") {
        return Err(Error::tool(
            "browser",
            format!("JavaScript exception: {exception}"),
        ));
    }
    let result = response
        .get("result")
        .filter(|v| v.is_object())
        .ok_or_else(|| Error::tool("browser", "Runtime.evaluate returned no remote object"))?;
    if let Some(value) = result.get("value") {
        return Ok(value.clone());
    }
    if let Some(value) = result.get("unserializableValue") {
        return Ok(json!({"type": result["type"], "unserializableValue": value}));
    }
    if result["type"] == "undefined" {
        return Ok(json!({"type": "undefined"}));
    }
    Err(Error::tool(
        "browser",
        "JavaScript result cannot be represented by value",
    ))
}

impl Session {
    fn clear_pages(&mut self) {
        self.tabs.clear();
        self.references.clear();
        self.active = None;
        self.endpoint = None;
        self.downloads_denied = false;
        self.pending_download_context = None;
        // Never reuse element IDs across process lifetimes. Upload copies are
        // retained separately: pending File objects can outlive a tab/navigation.
    }

    async fn connect(
        &mut self,
        owner: &AgentCx,
        cwd: &Path,
        connection: &launch::Connection,
        action: &str,
    ) -> Result<Option<Cdp>> {
        if action == "stop" {
            if matches!(connection, launch::Connection::Attach(_)) {
                return Err(Error::tool(
                    "browser",
                    "cannot stop an attached browser; Pi does not own its process",
                ));
            }
            if let Some(browser) = self.browser.as_mut() {
                browser.stop()?;
            }
            self.browser = None;
            self.uploads.clear();
            self.clear_pages();
            return Ok(None);
        }
        let mut startup = None;
        let (endpoint, expected_path) = match connection {
            launch::Connection::Attach(endpoint) => {
                if self.browser.is_some() {
                    return Err(Error::tool(
                        "browser",
                        "stop the owned browser before changing to an attached endpoint",
                    ));
                }
                (endpoint.clone(), None)
            }
            launch::Connection::Managed(options) => {
                if let Some(browser) = self.browser.as_mut()
                    && !browser.running()?
                {
                    self.browser = None;
                    self.uploads.clear();
                    self.clear_pages();
                    if action == "status" {
                        return Ok(None);
                    }
                    return Err(Error::tool(
                        "browser",
                        "the owned browser exited; its tabs are gone. Retry start or open to create a fresh isolated browser",
                    ));
                }
                if self.browser.is_none() {
                    if action == "status" {
                        return Ok(None);
                    }
                    if action == "handle_dialog" {
                        return Err(Error::tool(
                            "browser",
                            "no owned browser is running; handling a dialog never launches a browser",
                        ));
                    }
                    if storage::is_action(action) {
                        return Err(Error::tool(
                            "browser",
                            "BROWSER_STATE_NOT_RUNNING: open the intended origin in a Pi-owned browser before saving or restoring state",
                        ));
                    }
                    startup = Some(launch::ManagedBrowser::launch(owner, cwd, options).await?);
                }
                let browser = self
                    .browser
                    .as_ref()
                    .or(startup.as_ref())
                    .ok_or_else(|| Error::tool("browser", "managed browser was not created"))?;
                (
                    browser.address.http.clone(),
                    Some(browser.address.debugger_path.clone()),
                )
            }
        };
        if self.pending_download_context.is_some()
            && self.endpoint.as_deref() != Some(endpoint.as_str())
        {
            return Err(Error::tool(
                "browser",
                "BROWSER_CONTEXT_RECOVERY_ENDPOINT: recover the pending download policy on its original endpoint before switching browsers",
            ));
        }
        // Adopt startup only after discovery AND WebSocket identity/handshake
        // succeed. Failure or cancellation before then drops the local process.
        let mut cdp = Cdp::connect(owner, &endpoint, expected_path.as_deref()).await?;
        if self.endpoint.as_deref() != Some(endpoint.as_str()) {
            self.clear_pages();
        }
        if let Some(context) = self.pending_download_context.clone() {
            context.restore(owner, &mut cdp).await?;
            self.pending_download_context = None;
            if context == contexts::Context::Default {
                self.downloads_denied = true;
            }
        }
        if !self.downloads_denied {
            cdp.browser_command(
                owner,
                "Browser.setDownloadBehavior",
                json!({"behavior":"deny","eventsEnabled":true}),
            )
            .await?;
            self.downloads_denied = true;
        }
        self.endpoint = Some(endpoint.to_string());
        if let Some(browser) = startup {
            self.browser = Some(browser);
        }
        Ok(Some(cdp))
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &mut self,
        owner: &AgentCx,
        cdp: &mut Cdp,
        cwd: &Path,
        allowlist: Option<&[String]>,
        args: &Value,
    ) -> Result<ToolOutput> {
        let action = required(args, "action")?;
        let response = cdp
            .call(owner, "Target.getTargets", json!({}), false)
            .await?;
        let targets = response
            .get("targetInfos")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::tool("browser", "Target.getTargets returned no targets"))?;
        let pages: BTreeMap<String, Value> = targets
            .iter()
            .filter(|v| v["type"] == "page")
            .filter_map(|v| v["targetId"].as_str().map(|id| (id.to_owned(), v.clone())))
            .collect();
        self.tabs.retain(|_, id| pages.contains_key(id));
        self.references.retain(|(id, _), _| pages.contains_key(id));
        if self
            .active
            .as_ref()
            .is_some_and(|name| !self.tabs.contains_key(name))
        {
            self.active = None;
        }
        let tab = args
            .get("tab")
            .and_then(Value::as_str)
            .or(self.active.as_deref())
            .unwrap_or("default")
            .to_owned();
        if action == "list_tabs" {
            let tabs: Vec<_> = pages
                .iter()
                .map(|(id, info)| {
                    let name = self
                        .tabs
                        .iter()
                        .find(|(_, target)| *target == id)
                        .map_or(id, |(name, _)| name);
                    BrowserTabInfo {
                        name: name.clone(),
                        url: info["url"].as_str().unwrap_or_default().into(),
                        title: info["title"].as_str().unwrap_or_default().into(),
                        is_active: self.active.as_ref() == Some(name),
                    }
                })
                .collect();
            let lines = tabs
                .iter()
                .map(|t| format!("- [{}] \"{}\" -> {}", t.name, t.title, t.url))
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(output(
                format!("Active browser tabs ({}):\n{lines}", tabs.len()),
                json!({"tabs": tabs, "backend": "cdp"}),
            ));
        }
        let existing = self
            .tabs
            .get(&tab)
            .cloned()
            .or_else(|| pages.contains_key(&tab).then(|| tab.clone()));
        let target = if action == "open" && existing.is_none() {
            let created = cdp
                .call(
                    owner,
                    "Target.createTarget",
                    json!({"url": "about:blank"}),
                    false,
                )
                .await?;
            let id = required(&created, "targetId")?.to_owned();
            self.tabs.insert(tab.clone(), id.clone());
            id
        } else {
            existing.ok_or_else(|| Error::tool("browser", format!("cannot {action} nonexistent tab {tab}; use open or select a target from list_tabs")))?
        };
        if action == "close" {
            let response = cdp
                .call(
                    owner,
                    "Target.closeTarget",
                    json!({"targetId": target}),
                    false,
                )
                .await?;
            if response["success"] != true {
                return Err(Error::tool(
                    "browser",
                    "Chromium refused to close the target",
                ));
            }
            self.tabs.retain(|_, id| id != &target);
            self.references.retain(|(id, _), _| id != &target);
            if self
                .active
                .as_ref()
                .is_some_and(|name| !self.tabs.contains_key(name))
            {
                self.active = self.tabs.keys().next().cloned();
            }
            return Ok(output(
                format!("Closed tab {tab}"),
                json!({"closed_tab": tab, "remaining_count": self.tabs.len(), "backend": "cdp"}),
            ));
        }
        if !matches!(action, "open" | "goto") {
            let current = pages
                .get(&target)
                .and_then(|v| v["url"].as_str())
                .ok_or_else(|| Error::tool("browser", "target has no current URL"))?;
            policy::check_navigation(current, allowlist)?;
        }
        if let Some(info) = pages.get(&target) {
            cdp.context_binding = contexts::select(owner, cdp, info).await?;
        }
        let attached = cdp
            .call(
                owner,
                "Target.attachToTarget",
                json!({"targetId": target, "flatten": true}),
                false,
            )
            .await?;
        cdp.session_id = Some(required(&attached, "sessionId")?.to_owned());
        self.tabs.insert(tab.clone(), target.clone());
        self.active = Some(tab.clone());
        if action == "list_frames" {
            return frames::list(owner, cdp, &tab, allowlist).await;
        }
        let frame = args.get("frame").and_then(Value::as_str);
        if let Some(frame) = frame {
            let scope = frames::select(owner, cdp, frame, allowlist).await?;
            scope.admit_action(action)?;
            cdp.frame_scope = Some(scope);
        }
        let reference_key = (target.clone(), frame.unwrap_or_default().to_owned());
        diagnostics::start(owner, cdp, args, allowlist).await?;
        if let Some(expected) = dialog::Expected::from_args(args, allowlist)? {
            // An already-open dialog surfaced by Page.enable is not consent
            // for a future action. Arm only after enabling has completed.
            cdp.command(owner, "Page.enable", json!({})).await?;
            cdp.dialog.arm(expected);
        }
        if let Some(capture) = cdp.diagnostics.as_mut() {
            capture.begin_action();
        }
        let result = match action {
            action if storage::is_action(action) => {
                storage::execute(owner, cdp, cwd, &tab, allowlist, args).await
            }
            "diagnostics" => Ok(output(
                format!("Observing diagnostics for tab {tab}"),
                json!({"tab": tab, "backend": "cdp"}),
            )),
            // No Runtime.evaluate or DOM query: a modal dialog can suspend
            // page JavaScript. Target metadata already passed the URL guard.
            "handle_dialog" => dialog::execute(owner, cdp, &tab, args).await,
            "open" | "goto" => {
                let url = required(args, "url")?;
                cdp.navigate(owner, url).await?;
                let info = cdp
                    .evaluate(owner, "({url: location.href, title: document.title})")
                    .await?;
                let final_url = required(&info, "url")?;
                policy::check_navigation(final_url, allowlist)?;
                let title = required(&info, "title")?;
                Ok(output(
                    format!("Navigated tab {tab} to {final_url} (Title: \"{title}\")"),
                    json!({"tab": tab, "url": final_url, "title": title, "loaded": true, "backend": "cdp"}),
                ))
            }
            "snapshot" | "ax_tree" => {
                let (result, references) = interaction::snapshot(
                    owner,
                    cdp,
                    &tab,
                    self.references.get(&reference_key),
                    &mut self.next_ref,
                    action == "ax_tree",
                )
                .await?;
                if !self.references.contains_key(&reference_key)
                    && self.references.len() >= MAX_REFERENCE_DOCUMENTS
                {
                    self.references.pop_first();
                }
                self.references.insert(reference_key.clone(), references);
                Ok(result)
            }
            "click" | "hover" | "drag" | "type" | "fill" | "press" | "scroll" | "wait_for" => {
                interaction::execute(owner, cdp, &tab, self.references.get(&reference_key), args)
                    .await
            }
            action if interaction::forms::is_action(action) => {
                interaction::forms::execute(
                    owner,
                    cdp,
                    &tab,
                    self.references.get(&reference_key),
                    args,
                )
                .await
            }
            "upload" => {
                self.uploads
                    .execute(
                        owner,
                        cdp,
                        cwd,
                        self.references.get(&reference_key),
                        args,
                        allowlist,
                    )
                    .await
            }
            "download" => {
                // Persist the original context before allowing a transfer. If
                // cancellation drops cleanup, connect restores that context
                // before any subsequent action, even on a different tab.
                self.downloads_denied = false;
                self.pending_download_context = Some(cdp.current_download_context());
                let result = download::execute(
                    owner,
                    cdp,
                    cwd,
                    &tab,
                    self.references.get(&reference_key),
                    args,
                    allowlist,
                )
                .await;
                if result.is_ok() {
                    self.downloads_denied = true;
                    self.pending_download_context = None;
                }
                result
            }
            "screenshot" | "print_pdf" => exports::execute(owner, cdp, cwd, &tab, args).await,
            "evaluate" => {
                let value = cdp.evaluate(owner, required(args, "script")?).await?;
                Ok(output(
                    format!("Evaluation result: {value}"),
                    json!({"result": value, "backend": "cdp"}),
                ))
            }
            _ => Err(Error::tool(
                "browser",
                format!("unsupported CDP action: {action}"),
            )),
        };
        let mut result = result?;
        if let Some(frame) = frame
            && let Some(details) = result.details.as_mut().and_then(Value::as_object_mut)
        {
            details.insert("frame_id".to_string(), json!(frame));
        }
        if cdp.dialog.requested() {
            cdp.finish_expected_dialog(owner).await?;
            cdp.dialog.annotate(&mut result);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_responses_are_bound_to_both_request_and_session_without_payload_leaks() {
        let response = json!({"id": 7, "sessionId": "child", "result": {"value": 42}});
        assert_eq!(
            frame_command_result(&response, 7, "child").unwrap(),
            Some(json!({"value": 42}))
        );
        assert!(
            frame_command_result(&response, 8, "child")
                .unwrap()
                .is_none()
        );
        assert!(frame_command_result(&response, 7, "page").is_err());
        for invalid in [
            json!({"id": 7, "result": {}}),
            json!({"id": 7, "sessionId": "child", "result": null}),
            json!({"id": 7, "sessionId": "child", "error": {"message": "PRIVATE-CANARY"}}),
        ] {
            let error = frame_command_result(&invalid, 7, "child").unwrap_err();
            assert!(!error.to_string().contains("PRIVATE-CANARY"));
        }
    }

    #[test]
    fn javascript_exceptions_and_unserializable_values_are_not_fake_successes() {
        assert!(
            evaluation_value(
                &json!({"exceptionDetails": {"text": "Uncaught"}, "result": {"type": "object"}})
            )
            .is_err()
        );
        assert!(evaluation_value(&json!({})).is_err());
        assert_eq!(
            evaluation_value(&json!({"result": {"type": "number", "value": 42}})).unwrap(),
            json!(42)
        );
        assert_eq!(
            evaluation_value(&json!({"result": {"type": "number", "unserializableValue": "NaN"}}))
                .unwrap()["unserializableValue"],
            "NaN"
        );
        assert_eq!(
            evaluation_value(&json!({"result": {"type": "undefined"}})).unwrap()["type"],
            "undefined"
        );
    }

    #[test]
    fn lifecycle_never_accepts_model_selected_process_configuration() {
        assert!(validate(&json!({"action":"start"}), None).is_ok());
        for field in [
            "executable_path",
            "user_data_dir",
            "args",
            "headless",
            "endpoint",
            "tab",
        ] {
            let mut input = json!({"action":"start"});
            input[field] = json!("untrusted");
            assert!(validate(&input, None).is_err(), "{field}");
        }
    }

    #[test]
    fn frame_preflight_runs_before_any_connection_or_launch() {
        assert!(validate(&json!({"action":"list_frames","tab":"work"}), None).is_ok());
        assert!(
            validate(
                &json!({"action":"snapshot","tab":"work","frame":"child"}),
                None
            )
            .is_ok()
        );
        assert!(validate(&json!({"action":"snapshot","frame":"child"}), None).is_err());
        assert!(
            validate(
                &json!({"action":"goto","tab":"work","frame":"child","url":"https://example.com"}),
                None
            )
            .is_err()
        );
        assert!(
            validate(
                &json!({"action":"evaluate","tab":"work","frame":"child"}),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn idle_status_and_stop_do_not_launch_and_stop_does_not_own_attached_browsers() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        let managed = launch::Connection::Managed(BrowserLaunchOptions {
            executable_path: Some(std::path::PathBuf::from("/nonexistent/do-not-launch")),
            ..Default::default()
        });
        let mut session = Session::default();
        assert!(
            runtime
                .block_on(session.connect(&owner, Path::new("."), &managed, "status"))
                .unwrap()
                .is_none()
        );
        assert!(
            runtime
                .block_on(session.connect(&owner, Path::new("."), &managed, "stop"))
                .unwrap()
                .is_none()
        );
        let attached =
            launch::Connection::Attach(policy::endpoint("http://127.0.0.1:9", false).unwrap());
        assert!(
            runtime
                .block_on(session.connect(&owner, Path::new("."), &attached, "stop"))
                .is_err()
        );
        session.next_ref = 42;
        session.clear_pages();
        assert_eq!(session.next_ref, 42);
    }

    #[test]
    fn download_events_are_correlated_and_bounded() {
        let mut downloads = BTreeMap::new();
        downloads.insert(
            "old".to_string(),
            DownloadRecord {
                guid: "old".into(),
                frame_id: "old-frame".into(),
                url: "https://example.com/old".into(),
                suggested_filename: "old.bin".into(),
                state: "completed".into(),
                received_bytes: 1.0,
                total_bytes: 1.0,
            },
        );
        let before: BTreeSet<String> = downloads.keys().cloned().collect();
        // Driven through the free functions the `Cdp` methods delegate to: this
        // exercise needs a download table, not a live CDP websocket.
        record_download_event_into(
            &mut downloads,
            &json!({"method":"Browser.downloadWillBegin","params":{
                "guid":"new","frameId":"frame-1","url":"https://example.com/file","suggestedFilename":"file.bin"
            }}),
        )
        .unwrap();
        record_download_event_into(
            &mut downloads,
            &json!({"method":"Browser.downloadProgress","params":{
                "guid":"new","state":"completed","receivedBytes":7,"totalBytes":7
            }}),
        )
        .unwrap();
        let fresh = downloads_since_in(&downloads, &before);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].state, "completed");
        assert_eq!(fresh[0].received_bytes, 7.0);
        assert_eq!(fresh[0].frame_id, "frame-1");
    }

    fn download_start(guid: &str) -> Value {
        json!({"method":"Browser.downloadWillBegin","params":{
            "guid":guid,"frameId":"frame-1","url":"https://example.com/file","suggestedFilename":"file.bin"
        }})
    }

    fn download_progress(state: &str, received: f64, total: f64) -> Value {
        json!({"method":"Browser.downloadProgress","params":{
            "guid":"job","state":state,"receivedBytes":received,"totalBytes":total
        }})
    }

    #[test]
    fn repeated_download_starts_preserve_source_and_terminal_progress() {
        let mut downloads = BTreeMap::new();
        let start = download_start("job");
        record_download_event_into(&mut downloads, &start).unwrap();
        record_download_event_into(&mut downloads, &download_progress("completed", 7.0, 7.0)).unwrap();
        let saved = downloads.clone();
        record_download_event_into(&mut downloads, &start).unwrap();
        assert_eq!(downloads, saved);
        for field in ["frameId", "url", "suggestedFilename"] {
            let mut changed = start.clone();
            changed["params"][field] = json!("PRIVATE-CANARY");
            let error = record_download_event_into(&mut downloads, &changed).unwrap_err();
            assert!(!error.to_string().contains("PRIVATE-CANARY"));
            assert_eq!(downloads, saved);
        }
    }

    #[test]
    fn invalid_download_progress_cannot_partially_complete_a_record() {
        let mut downloads = BTreeMap::new();
        record_download_event_into(&mut downloads, &download_start("job")).unwrap();
        record_download_event_into(&mut downloads, &download_progress("inProgress", 4.0, 0.0)).unwrap();
        let saved = downloads.clone();
        for invalid in [
            download_progress("completed", 7.0, f64::NAN),
            download_progress("completed", -1.0, 7.0),
            download_progress("completed", 3.0, 7.0),
            download_progress("unknown", 7.0, 7.0),
        ] {
            assert!(record_download_event_into(&mut downloads, &invalid).is_err());
            assert_eq!(downloads, saved);
        }
        // An unknown total becoming known is ordinary progress, not a reset.
        record_download_event_into(&mut downloads, &download_progress("inProgress", 5.0, 10.0)).unwrap();
        record_download_event_into(&mut downloads, &download_progress("completed", 7.0, 7.0)).unwrap();
        assert_eq!(downloads["job"].state, "completed");
    }

    #[test]
    fn terminal_downloads_cannot_resurrect_or_change_byte_counts() {
        for terminal in ["completed", "canceled"] {
            let mut downloads = BTreeMap::new();
            record_download_event_into(&mut downloads, &download_start("job")).unwrap();
            let event = download_progress(terminal, 7.0, 9.0);
            record_download_event_into(&mut downloads, &event).unwrap();
            let saved = downloads.clone();
            record_download_event_into(&mut downloads, &event).unwrap();
            for invalid in [
                download_progress("inProgress", 7.0, 9.0),
                download_progress(terminal, 8.0, 9.0),
                download_progress(terminal, 7.0, 10.0),
                download_progress(if terminal == "completed" { "canceled" } else { "completed" }, 7.0, 9.0),
            ] {
                assert!(record_download_event_into(&mut downloads, &invalid).is_err());
                assert_eq!(downloads, saved);
            }
        }
    }

    #[test]
    fn download_metadata_requires_bounded_source_identity_without_leaking_it() {
        for (field, value) in [
            ("frameId", Value::Null),
            ("frameId", json!("")),
            ("frameId", json!("f".repeat(257))),
            ("frameId", json!("PRIVATE-CANARY\n")),
            ("guid", json!("")),
            ("url", json!("")),
            ("url", json!("https://example.com/PRIVATE-CANARY\n")),
        ] {
            let mut event = download_start("job");
            event["params"][field] = value;
            let mut downloads = BTreeMap::new();
            let error = record_download_event_into(&mut downloads, &event).unwrap_err();
            assert!(!error.to_string().contains("PRIVATE-CANARY"));
            assert!(downloads.is_empty());
        }
    }

    #[test]
    fn download_record_budget_allows_duplicates_not_extra_transfer_identities() {
        let mut downloads = BTreeMap::new();
        for i in 0..MAX_DOWNLOAD_RECORDS {
            record_download_event_into(&mut downloads, &download_start(&format!("job-{i}"))).unwrap();
        }
        record_download_event_into(&mut downloads, &download_start("job-0")).unwrap();
        assert!(record_download_event_into(&mut downloads, &download_start("overflow")).is_err());
        assert_eq!(downloads.len(), MAX_DOWNLOAD_RECORDS);
        record_download_event_into(&mut downloads, &download_progress("completed", 7.0, 7.0)).unwrap();
        assert!(!downloads.contains_key("job"));
    }
}
