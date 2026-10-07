//! ACP (Agent Client Protocol) support for Zed editor integration.
//!
//! ACP is a JSON-RPC 2.0 protocol over stdio that Zed editor uses to
//! communicate with external agents. This module implements the server
//! side of the protocol, translating ACP method calls into pi's core
//! agent/session/tool infrastructure.
//!
//! ## Protocol methods
//!
//! - `initialize` — exchange capabilities and protocol version (integer)
//! - `session/new` — create a new agent session
//! - `session/prompt` — send a prompt; the response (with `stopReason`) is
//!   delivered only after the turn completes
//! - `session/cancel` — abort the current prompt turn for a session
//! - `session/list`, `session/load`, `session/resume` — session management
//! - `session/set_model` — switch the live session's provider/model at runtime
//! - `session/set_config_option` — set a runtime option (e.g. thinking/effort)
//!
//! ## Streaming
//!
//! Incremental output is streamed via `session/update` notifications, whose
//! `params.update.sessionUpdate` discriminator identifies the kind of update
//! (`agent_message_chunk`, `agent_thought_chunk`, `tool_call`,
//! `tool_call_update`). Clients render in real time and the in-flight
//! `session/prompt` request stays open until the turn produces a `stopReason`.

#![allow(clippy::too_many_lines)]
#![allow(clippy::significant_drop_tightening)]

mod content;
mod history;
mod mcp;
#[cfg(test)]
mod recovery_tests;

use crate::agent::{
    AbortHandle, AbortSignal, AgentEvent, AgentSession, ToolApprovalDecision, ToolApprovalHandler,
    ToolApprovalRequest,
};
use crate::agent_cx::AgentCx;
use crate::auth::AuthStorage;
use crate::compaction::ResolvedCompactionSettings;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::{AssistantMessageEvent, ContentBlock};
use crate::models::{ModelEntry, ModelRegistry};
#[cfg(test)]
use crate::provider::StreamOptions;
use crate::provider_metadata::provider_ids_match;
use crate::providers;
use crate::sdk::{AgentSessionHandle, EventListeners, FailoverOptions};
use crate::session::{Session, SessionStoreKind};
use crate::tools::ToolRegistry;
use asupersync::channel::oneshot;
use asupersync::runtime::RuntimeHandle;
use asupersync::sync::{Mutex, OwnedMutexGuard};
use asupersync::time::{timeout, wall_now};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

// ============================================================================
// JSON-RPC 2.0 types
// ============================================================================

/// A JSON-RPC 2.0 request.
#[derive(Debug, Clone, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 notification (no `id` field).
#[derive(Debug, Clone, Serialize)]
struct JsonRpcNotification {
    jsonrpc: String,
    method: String,
    params: Value,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

// Standard JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

// ACP-specific error codes.
const SESSION_NOT_FOUND: i64 = -32001;
const PROMPT_IN_PROGRESS: i64 = -32002;

fn json_rpc_ok(id: Value, result: Value) -> String {
    serde_json::to_string(&JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: Some(result),
        error: None,
    })
    .expect("serialize json-rpc response")
}

fn json_rpc_error(id: Value, code: i64, message: impl Into<String>) -> String {
    serde_json::to_string(&JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.into(),
            data: None,
        }),
    })
    .expect("serialize json-rpc error")
}

fn json_rpc_notification(method: &str, params: Value) -> String {
    serde_json::to_string(&JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: method.to_string(),
        params,
    })
    .expect("serialize json-rpc notification")
}

// ============================================================================
// ACP Protocol types
// ============================================================================

type AcpSessionsMap = Arc<Mutex<HashMap<String, Arc<Mutex<AcpSessionState>>>>>;
type PendingPermissionMap = Arc<StdMutex<HashMap<String, oneshot::Sender<Value>>>>;

const ACP_PERMISSION_ALLOW_ONCE: &str = "allow-once";
const ACP_PERMISSION_REJECT_ONCE: &str = "reject-once";
const ACP_PERMISSION_TIMEOUT_MS: u64 = 120_000;

// Note: AcpServerCapabilities and AcpServerInfo are constructed inline
// via json!() in handle_initialize for simplicity.

/// ACP model descriptor.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AcpModel {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
}

/// ACP mode descriptor.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AcpMode {
    slug: String,
    name: String,
    description: String,
}

// ============================================================================
// ACP Session state
// ============================================================================

struct AcpSessionState {
    /// The agent session. Wrapped in Option so it can be temporarily taken
    /// out during prompt execution without holding the session lock.
    agent_session: Option<AgentSessionHandle>,
    /// Ready launch catalog plus the admitted startup entry, retained across
    /// model changes so a selected ad-hoc model remains available to return to.
    available_models: Vec<ModelEntry>,
    cwd: PathBuf,
    /// Remains reachable during a turn so EOF/exit can close external tools.
    mcp: Option<Arc<mcp::SessionMcp>>,
}

// ============================================================================
// ACP Server
// ============================================================================

/// Host settings supplied by the process launching the editor agent. Reopening
/// a branch restores its own model and effort, while credentials and execution
/// controls remain runtime-only.
#[derive(Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct AcpLaunchOptions {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub models: Option<String>,
    pub api_key: Option<String>,
    pub tools: Option<String>,
    pub no_tools: bool,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub no_context_files: bool,
    pub hide_cwd_in_prompt: bool,
    pub max_tool_iterations: Option<usize>,
    pub max_time: Option<u64>,
}

impl AcpLaunchOptions {
    #[must_use]
    pub fn from_cli(cli: &crate::cli::Cli) -> Self {
        Self {
            provider: cli.provider.clone(),
            model: cli.model.clone(),
            thinking: cli.thinking.clone(),
            models: cli.models.clone(),
            api_key: crate::models::normalize_api_key_opt(cli.api_key.clone()),
            tools: Some(cli.tools.clone()),
            no_tools: cli.no_tools,
            system_prompt: cli.system_prompt.clone(),
            append_system_prompt: cli.append_system_prompt.clone(),
            no_context_files: cli.no_context_files,
            hide_cwd_in_prompt: cli.hide_cwd_in_prompt,
            max_tool_iterations: cli.max_tool_iterations,
            max_time: cli.max_time,
        }
    }

    fn selection_cli(&self) -> Result<crate::cli::Cli> {
        use clap::{CommandFactory as _, FromArgMatches as _};
        // The host already parsed its flags and environment once. Construct
        // neutral defaults here: re-reading unrelated typed environment flags
        // can reject a session even when the host overrode them successfully.
        let matches = crate::cli::Cli::command()
            .mut_args(|arg| arg.env(None::<&str>))
            .try_get_matches_from(["pi"])
            .map_err(|error| Error::config(error.to_string()))?;
        let mut cli = crate::cli::Cli::from_arg_matches(&matches)
            .map_err(|error| Error::config(error.to_string()))?;
        cli.provider.clone_from(&self.provider);
        cli.model.clone_from(&self.model);
        cli.thinking.clone_from(&self.thinking);
        cli.models.clone_from(&self.models);
        cli.api_key = crate::models::normalize_api_key_opt(self.api_key.clone());
        if let Some(tools) = &self.tools {
            cli.tools.clone_from(tools);
        }
        cli.no_tools = self.no_tools;
        cli.system_prompt.clone_from(&self.system_prompt);
        cli.append_system_prompt.clone_from(&self.append_system_prompt);
        cli.no_context_files = self.no_context_files;
        cli.hide_cwd_in_prompt = self.hide_cwd_in_prompt;
        cli.max_tool_iterations = self.max_tool_iterations;
        cli.max_time = self.max_time;
        Ok(cli)
    }
}

/// Options for starting the ACP server.
#[derive(Clone)]
pub struct AcpOptions {
    pub config: Config,
    pub launch: AcpLaunchOptions,
    pub available_models: Vec<ModelEntry>,
    /// Full model registry (every known/loaded model, not just the ready ones in
    /// `available_models`). Used so a live session can switch to any registered
    /// model via `session/set_model` and resolve its credentials/headers — the
    /// `AgentSession::set_provider_model` path requires the registry to locate
    /// the target model entry.
    pub model_registry: ModelRegistry,
    pub auth: AuthStorage,
    /// Providers whose startup OAuth refresh failed. Keep only identities here;
    /// remote refresh error bodies must not reach editor protocol responses.
    pub oauth_refresh_failures: Vec<String>,
    pub runtime_handle: RuntimeHandle,
    /// When set (from the `--session-dir` CLI flag), ACP sessions persist to
    /// this directory and autosave is enabled. After an editor restart they
    /// can be reopened by ID via `session/load` or `session/resume`, as well
    /// as via `pi --session`/`--resume`. When `None`, sessions remain in-memory.
    pub session_dir: Option<PathBuf>,
    /// The "available skills" system-prompt block, rendered by the host from
    /// its resource loader (`--no-skills` and trust applied), so the model
    /// in an editor session knows which skills it can load.
    pub skills_prompt: Option<String>,
}

#[derive(Clone)]
struct AcpPermissionClient {
    out_tx: std::sync::mpsc::SyncSender<String>,
    pending: PendingPermissionMap,
    request_counter: Arc<AtomicU64>,
    timeout: Duration,
    cx: AgentCx,
}

struct PendingPermissionGuard {
    pending: PendingPermissionMap,
    key: String,
}

impl Drop for PendingPermissionGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.pending.lock() {
            guard.remove(&self.key);
        }
    }
}

impl AcpPermissionClient {
    fn handler_for_session(&self, session_id: String) -> ToolApprovalHandler {
        let client = self.clone();
        Arc::new(move |request: ToolApprovalRequest| {
            let client = client.clone();
            let session_id = session_id.clone();
            Box::pin(async move { client.request_permission(&session_id, request).await })
        })
    }

    async fn request_permission(
        &self,
        session_id: &str,
        request: ToolApprovalRequest,
    ) -> ToolApprovalDecision {
        let request_id = Value::String(format!(
            "pi-tool-permission-{}",
            self.request_counter.fetch_add(1, Ordering::SeqCst)
        ));
        let request_key = json_rpc_id_key(&request_id);
        let (reply_tx, mut reply_rx) = oneshot::channel();

        if let Ok(mut guard) = self.pending.lock() {
            guard.insert(request_key.clone(), reply_tx);
        } else {
            return ToolApprovalDecision::deny("permission request registry unavailable");
        }
        let _pending_guard = PendingPermissionGuard {
            pending: Arc::clone(&self.pending),
            key: request_key,
        };

        let request_line = json_rpc_permission_request(&request_id, session_id, &request);
        if self.out_tx.send(request_line).is_err() {
            return ToolApprovalDecision::deny("permission request client disconnected");
        }

        let response = timeout(
            wall_now(),
            self.timeout,
            Box::pin(reply_rx.recv(self.cx.cx())),
        )
        .await;

        match response {
            Ok(Ok(value)) => permission_response_to_decision(&value),
            Ok(Err(_)) => ToolApprovalDecision::deny("permission response channel closed"),
            Err(_) => ToolApprovalDecision::deny("permission request timed out"),
        }
    }
}

/// Run the ACP server over stdio.
///
/// Reads JSON-RPC requests line-by-line from stdin, dispatches them,
/// and writes JSON-RPC responses/notifications to stdout.
pub async fn run_stdio(options: AcpOptions) -> Result<()> {
    let (in_tx, in_rx) = asupersync::channel::mpsc::channel::<String>(256);
    let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(1024);

    // Stdin reader thread.
    std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut reader = io::BufReader::new(stdin.lock());
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let trimmed = line.trim().to_string();
                    if trimmed.is_empty() {
                        continue;
                    }
                    // Retry loop with backpressure.
                    let mut to_send = trimmed;
                    loop {
                        match in_tx.try_send(to_send) {
                            Ok(()) => break,
                            Err(asupersync::channel::mpsc::SendError::Full(unsent)) => {
                                to_send = unsent;
                                std::thread::sleep(std::time::Duration::from_millis(10));
                            }
                            Err(_) => return,
                        }
                    }
                }
            }
        }
    });

    // Stdout writer thread.
    std::thread::spawn(move || {
        let stdout = io::stdout();
        let mut writer = io::BufWriter::new(stdout.lock());
        for line in out_rx {
            if writer.write_all(line.as_bytes()).is_err() {
                break;
            }
            if writer.write_all(b"\n").is_err() {
                break;
            }
            if writer.flush().is_err() {
                break;
            }
        }
    });

    run(options, in_rx, out_tx).await
}

/// Core ACP event loop.
async fn run(
    options: AcpOptions,
    mut in_rx: asupersync::channel::mpsc::Receiver<String>,
    out_tx: std::sync::mpsc::SyncSender<String>,
) -> Result<()> {
    let cx = AgentCx::for_current_or_request();
    let sessions: AcpSessionsMap = Arc::new(Mutex::new(HashMap::new()));
    let prompt_counter = Arc::new(AtomicU64::new(0));
    let permission_counter = Arc::new(AtomicU64::new(0));
    let pending_permissions: PendingPermissionMap = Arc::new(StdMutex::new(HashMap::new()));
    let active_prompts: Arc<Mutex<HashMap<String, AbortHandle>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let initialized = Arc::new(AtomicBool::new(false));

    // Capture dispatch errors so every exit path still tears down MCP state.
    let result: Result<()> = async {
    while let Ok(line) = in_rx.recv(&cx).await {
        let raw_message: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(err) => {
                let _ = out_tx.send(json_rpc_error(
                    Value::Null,
                    PARSE_ERROR,
                    format!("Parse error: {err}"),
                ));
                continue;
            }
        };

        if raw_message.get("method").is_none() {
            let _ = route_permission_response(&raw_message, &pending_permissions, &cx);
            continue;
        }

        // Parse the JSON-RPC request.
        let request: JsonRpcRequest = match serde_json::from_value(raw_message) {
            Ok(req) => req,
            Err(err) => {
                let _ = out_tx.send(json_rpc_error(
                    Value::Null,
                    INVALID_REQUEST,
                    format!("Invalid request: {err}"),
                ));
                continue;
            }
        };

        // Validate JSON-RPC version.
        if request.jsonrpc != "2.0" {
            if let Some(ref id) = request.id {
                let _ = out_tx.send(json_rpc_error(
                    id.clone(),
                    INVALID_REQUEST,
                    "Expected jsonrpc version 2.0",
                ));
            }
            continue;
        }

        let id = request.id.clone().unwrap_or(Value::Null);

        match request.method.as_str() {
            "initialize" => {
                let result = handle_initialize();
                initialized.store(true, Ordering::SeqCst);
                let _ = out_tx.send(json_rpc_ok(id, result));
            }

            // `initialized` is a notification the client sends after
            // processing the `initialize` response. We accept it silently.
            "initialized" => {}

            // `shutdown` is the graceful shutdown request.
            "shutdown" => {
                let _ = out_tx.send(json_rpc_ok(id, json!(null)));
            }

            // `exit` notification tells us to terminate.
            "exit" => {
                break;
            }

            "session/new" => {
                if !initialized.load(Ordering::SeqCst) {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        INVALID_REQUEST,
                        "Server not initialized. Call 'initialize' first.",
                    ));
                    continue;
                }

                // Validate the entire definition list before constructing state.
                // The constructor repeats this for non-dispatch callers/tests.
                let validated = history::requested_cwd(&request.params)
                    .map_err(|error| error.message)
                    .and_then(|cwd| crate::mcp::config::parse_acp_servers(&request.params, &cwd).map(|_| ()));
                if let Err(error) = validated {
                    let _ = out_tx.send(json_rpc_error(id, INVALID_PARAMS, error));
                    continue;
                }
                let permission_client = AcpPermissionClient {
                    out_tx: out_tx.clone(),
                    pending: Arc::clone(&pending_permissions),
                    request_counter: Arc::clone(&permission_counter),
                    timeout: acp_permission_timeout(),
                    cx: cx.clone(),
                };

                match handle_session_new(&request.params, &options, Some(&permission_client)) {
                    Ok((session_id, state)) => {
                        let models: Vec<AcpModel> = state
                            .available_models
                            .iter()
                            .map(|entry| AcpModel {
                                id: entry.model.id.clone(),
                                name: entry.model.name.clone(),
                                provider: Some(entry.model.provider.clone()),
                            })
                            .collect();

                        let modes = vec![
                            AcpMode {
                                slug: "agent".to_string(),
                                name: "Agent".to_string(),
                                description: "Full autonomous coding agent with tool access"
                                    .to_string(),
                            },
                            AcpMode {
                                slug: "chat".to_string(),
                                name: "Chat".to_string(),
                                description: "Conversational mode without tool execution"
                                    .to_string(),
                            },
                        ];

                        let config_options = config_options_for(&state);
                        let mcp_state = state.mcp.clone();
                        let state_arc = Arc::new(Mutex::new(state));
                        {
                            let mut guard = sessions.lock(&cx).await
                                .map_err(|_| Error::session("Session registry is unavailable"))?;
                            guard.insert(session_id.clone(), state_arc);
                        }

                        // `configOptions` is the ACP-standard surface (model +
                        // thought_level selects); `models`/`modes` stay for
                        // clients built against the earlier shape.
                        let _ = out_tx.send(json_rpc_ok(
                            id,
                            json!({
                                "sessionId": session_id,
                                "configOptions": config_options,
                                "models": models,
                                "modes": modes,
                            }),
                        ));
                        if let Some(mcp_state) = mcp_state {
                            mcp::announce(&mcp_state, &session_id, &out_tx).await
                                .map_err(Error::session)?;
                        }
                    }
                    Err(err) => {
                        let _ = out_tx.send(json_rpc_error(
                            id,
                            INTERNAL_ERROR,
                            format!("Failed to create session: {err}"),
                        ));
                    }
                }
            }

            "session/prompt" => {
                if !initialized.load(Ordering::SeqCst) {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        INVALID_REQUEST,
                        "Server not initialized",
                    ));
                    continue;
                }

                let session_id = request
                    .params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(String::from);

                let Some(session_id) = session_id else {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        INVALID_PARAMS,
                        "Missing required parameter: sessionId",
                    ));
                    continue;
                };

                // Decode the complete ContentBlock[] before starting a turn.
                // Images stay native image blocks, and embedded editor context
                // retains its provenance without implicitly opening its URI.
                let prompt_blocks = request.params.get("prompt").and_then(Value::as_array);
                let Some(prompt_blocks) = prompt_blocks else {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        INVALID_PARAMS,
                        "Missing required parameter: prompt (expected array of ContentBlock)",
                    ));
                    continue;
                };

                let mcp_command = match mcp::command_from_prompt(prompt_blocks) {
                    Ok(command) => command,
                    Err(error) => {
                        let _ = out_tx.send(json_rpc_error(id, INVALID_PARAMS, error));
                        continue;
                    }
                };
                let message_content = match content::extract_prompt_content(prompt_blocks) {
                    Ok(content) => content,
                    Err(err) => {
                        let _ = out_tx.send(json_rpc_error(id, INVALID_PARAMS, err));
                        continue;
                    }
                };

                let session_state = {
                    sessions
                        .lock(&cx)
                        .await
                        .map_or_else(|_| None, |guard| guard.get(&session_id).cloned())
                };

                let Some(session_state) = session_state else {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        SESSION_NOT_FOUND,
                        format!("Session not found: {session_id}"),
                    ));
                    continue;
                };

                // Per spec, only one prompt turn may be active per session.
                {
                    let has_active = active_prompts
                        .lock(&cx)
                        .await
                        .is_ok_and(|guard| guard.contains_key(&session_id));
                    if has_active {
                        let _ = out_tx.send(json_rpc_error(
                            id,
                            PROMPT_IN_PROGRESS,
                            format!("Session {session_id} already has an active prompt"),
                        ));
                        continue;
                    }
                }

                // Bump the counter so prompt-turn diagnostics stay unique even
                // across the same session_id.
                let _ = prompt_counter.fetch_add(1, Ordering::SeqCst);

                let (abort_handle, abort_signal) = AbortHandle::new();
                {
                    let mut guard = active_prompts.lock(&cx).await
                        .map_err(|_| Error::session("Active prompt registry is unavailable"))?;
                    guard.insert(session_id.clone(), abort_handle);
                }

                // Per ACP, the server replies to session/prompt only after the
                // turn completes — with a stopReason. Spawn the work and have
                // the spawned task own the response (carrying the original `id`).
                let out_tx_prompt = out_tx.clone();
                let active_prompts_cleanup = Arc::clone(&active_prompts);
                let prompt_cx = cx.clone();
                let prompt_session_id = session_id.clone();
                let response_id = id.clone();

                options.runtime_handle.spawn(async move {
                    let stop_reason = run_prompt(
                        session_state,
                        message_content,
                        mcp_command,
                        abort_signal,
                        out_tx_prompt.clone(),
                        prompt_session_id.clone(),
                        prompt_cx.clone(),
                    )
                    .await;

                    if let Ok(mut guard) = active_prompts_cleanup.lock(&prompt_cx).await {
                        guard.remove(&prompt_session_id);
                    }

                    let _ = out_tx_prompt.send(json_rpc_ok(
                        response_id,
                        json!({ "stopReason": stop_reason }),
                    ));
                });
            }

            // ACP defines session/cancel as a notification that aborts the
            // current prompt turn for `sessionId`. We accept the request form
            // too (some clients still send it as a request) and respond with
            // an empty result so they don't error on a missing reply.
            "session/cancel" => {
                let session_id_opt = request
                    .params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(String::from);

                let Some(session_id) = session_id_opt else {
                    if request.id.is_some() {
                        let _ = out_tx.send(json_rpc_error(
                            id,
                            INVALID_PARAMS,
                            "Missing required parameter: sessionId",
                        ));
                    }
                    continue;
                };

                if let Ok(guard) = active_prompts.lock(&cx).await
                    && let Some(handle) = guard.get(&session_id)
                {
                    handle.abort();
                }

                if request.id.is_some() {
                    let _ = out_tx.send(json_rpc_ok(id, json!({})));
                }
            }

            "session/list" => {
                if !initialized.load(Ordering::SeqCst) {
                    let _ = out_tx.send(json_rpc_error(id, INVALID_REQUEST, "Server not initialized"));
                    continue;
                }
                let response = match history::list(&request.params, &options, &sessions, &cx).await {
                    Ok(result) => json_rpc_ok(id, result),
                    Err(error) => json_rpc_error(id, error.code, error.message),
                };
                history::send_line(&out_tx, response)
                    .await
                    .map_err(|error| Error::session(error.message))?;
            }

            "session/load" | "session/resume" => {
                if !initialized.load(Ordering::SeqCst) {
                    let _ = out_tx.send(json_rpc_error(id, INVALID_REQUEST, "Server not initialized"));
                    continue;
                }
                let session_id = match history::requested_session_id(&request.params) {
                    Ok(session_id) => session_id,
                    Err(error) => {
                        let _ = out_tx.send(json_rpc_error(id, error.code, error.message));
                        continue;
                    }
                };
                // Check the reservation as well as agent_session: a spawned
                // prompt may not yet have taken the agent out of its state.
                let busy = active_prompts
                    .lock(&cx)
                    .await
                    .map_err(|error| Error::session(format!("Active prompt registry unavailable: {error}")))?
                    .contains_key(session_id);
                if busy {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        PROMPT_IN_PROGRESS,
                        "Cannot load or resume a session while a prompt is in progress",
                    ));
                    continue;
                }
                let permission_client = AcpPermissionClient {
                    out_tx: out_tx.clone(),
                    pending: Arc::clone(&pending_permissions),
                    request_counter: Arc::clone(&permission_counter),
                    timeout: acp_permission_timeout(),
                    cx: cx.clone(),
                };
                let replay = request.method == "session/load";
                let result = history::load(
                    &request.params,
                    &options,
                    &permission_client,
                    &sessions,
                    &cx,
                    &out_tx,
                    replay,
                )
                .await;
                // All replay notifications precede this response. Resume is
                // intentionally history-free for clients that kept their view.
                let response = match result {
                    Ok(mut result) => {
                        if !replay {
                            result["resumed"] = json!(true);
                        }
                        json_rpc_ok(id, result)
                    }
                    Err(error) => json_rpc_error(id, error.code, error.message),
                };
                history::send_line(&out_tx, response)
                    .await
                    .map_err(|error| Error::session(error.message))?;
            }

            // Dynamic, per-session model switch (#105). Switches the live
            // session's provider/model so clients can change models at runtime
            // (e.g. to gpt-5.5) without restarting or editing static config.
            "session/set_model" => {
                let session_id = request
                    .params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(String::from);
                let Some(session_id) = session_id else {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        INVALID_PARAMS,
                        "Missing required parameter: sessionId",
                    ));
                    continue;
                };

                let session_state = {
                    sessions
                        .lock(&cx)
                        .await
                        .map_or_else(|_| None, |guard| guard.get(&session_id).cloned())
                };
                let Some(session_state) = session_state else {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        SESSION_NOT_FOUND,
                        format!("Session not found: {session_id}"),
                    ));
                    continue;
                };

                match apply_set_model_request(&session_state, &request.params, &cx).await {
                    Ok((provider, model)) => {
                        let _ = out_tx.send(json_rpc_ok(
                            id,
                            json!({
                                "sessionId": session_id,
                                "model": { "provider": provider, "id": model },
                            }),
                        ));
                    }
                    Err(msg) => {
                        let _ = out_tx.send(json_rpc_error(id, INVALID_PARAMS, msg));
                    }
                }
            }

            // Dynamic, per-session config option (#105). Currently applies the
            // reasoning/thinking effort to the live session; unknown or
            // restart-only options return a structured error (never a silent
            // success). See the runtime-vs-restart contract above.
            "session/set_config_option" => {
                let session_id = request
                    .params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(String::from);
                let Some(session_id) = session_id else {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        INVALID_PARAMS,
                        "Missing required parameter: sessionId",
                    ));
                    continue;
                };

                // ACP's `configId` names the option (`name`/`key` accepted
                // too); the value lives under `value`.
                let Some(name) = config_option_id(&request.params) else {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        INVALID_PARAMS,
                        "Missing required parameter: configId",
                    ));
                    continue;
                };
                let value = request.params.get("value").cloned().unwrap_or(Value::Null);

                // `model` is the ACP model selector: same switch as
                // session/set_model, with the value as `provider/id` or an id.
                let model_request = if name.eq_ignore_ascii_case("model") {
                    match value.as_str() {
                        Some(model) => Some(json!({ "model": model })),
                        None => {
                            let _ = out_tx.send(json_rpc_error(
                                id,
                                INVALID_PARAMS,
                                "Invalid value for config option 'model': expected a model id or provider/id string",
                            ));
                            continue;
                        }
                    }
                } else {
                    None
                };
                let option = if model_request.is_some() {
                    None
                } else {
                    match parse_config_option(name, &value) {
                        Ok(option) => Some(option),
                        Err(msg) => {
                            let _ = out_tx.send(json_rpc_error(id, INVALID_PARAMS, msg));
                            continue;
                        }
                    }
                };

                let session_state = {
                    sessions
                        .lock(&cx)
                        .await
                        .map_or_else(|_| None, |guard| guard.get(&session_id).cloned())
                };
                let Some(session_state) = session_state else {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        SESSION_NOT_FOUND,
                        format!("Session not found: {session_id}"),
                    ));
                    continue;
                };

                let applied = match (model_request, option) {
                    (Some(params), _) => {
                        apply_set_model_request(&session_state, &params, &cx)
                            .await
                            .map(drop)
                    }
                    (None, Some(option)) => {
                        apply_set_config_option(&session_state, option, &cx).await
                    }
                    (None, None) => Ok(()),
                };
                match applied {
                    Ok(()) => {
                        // ACP answers with the complete config state.
                        let config_options = session_state.lock(&cx).await.ok().and_then(|guard| {
                            config_options_for(&guard)
                        });
                        let _ = out_tx.send(json_rpc_ok(
                            id,
                            json!({
                                "configOptions": config_options,
                                "sessionId": session_id,
                                "name": name,
                                "applied": true,
                            }),
                        ));
                    }
                    Err(msg) => {
                        let _ = out_tx.send(json_rpc_error(id, INVALID_PARAMS, msg));
                    }
                }
            }

            // File I/O methods. Paths must be under a known session's cwd
            // to prevent arbitrary filesystem access.
            "read_text_file" => {
                let path_str = match request.params.get("path").and_then(Value::as_str) {
                    Some(p) if !p.is_empty() => p,
                    _ => {
                        let _ = out_tx.send(json_rpc_error(
                            id,
                            INVALID_PARAMS,
                            "Missing or empty required parameter: path",
                        ));
                        continue;
                    }
                };
                let session_id = request.params.get("sessionId").and_then(Value::as_str);

                if let Err(msg) = validate_file_path(path_str, session_id, &sessions, &cx).await {
                    let _ = out_tx.send(json_rpc_error(id, INVALID_PARAMS, msg));
                    continue;
                }

                let max_bytes = 10 * 1024 * 1024; // 10MB limit for ACP
                match asupersync::fs::metadata(path_str).await {
                    Ok(meta) if meta.len() > max_bytes => {
                        let _ = out_tx.send(json_rpc_error(
                            id,
                            INTERNAL_ERROR,
                            format!(
                                "File too large ({} bytes). Maximum allowed via ACP is {} bytes.",
                                meta.len(),
                                max_bytes
                            ),
                        ));
                        continue;
                    }
                    _ => {}
                }

                match asupersync::fs::read(path_str).await {
                    Ok(bytes) => {
                        let contents = String::from_utf8_lossy(&bytes).into_owned();
                        let _ = out_tx.send(json_rpc_ok(id, json!({ "contents": contents })));
                    }
                    Err(err) => {
                        let _ = out_tx.send(json_rpc_error(
                            id,
                            INTERNAL_ERROR,
                            format!("Failed to read file: {err}"),
                        ));
                    }
                }
            }

            "write_text_file" => {
                let path_str = match request.params.get("path").and_then(Value::as_str) {
                    Some(p) if !p.is_empty() => p,
                    _ => {
                        let _ = out_tx.send(json_rpc_error(
                            id,
                            INVALID_PARAMS,
                            "Missing or empty required parameter: path",
                        ));
                        continue;
                    }
                };
                let Some(contents) = request.params.get("contents").and_then(Value::as_str) else {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        INVALID_PARAMS,
                        "Missing required parameter: contents",
                    ));
                    continue;
                };
                let session_id = request.params.get("sessionId").and_then(Value::as_str);

                if let Err(msg) = validate_file_path(path_str, session_id, &sessions, &cx).await {
                    let _ = out_tx.send(json_rpc_error(id, INVALID_PARAMS, msg));
                    continue;
                }

                match asupersync::fs::write(path_str, contents.as_bytes()).await {
                    Ok(()) => {
                        let _ = out_tx.send(json_rpc_ok(id, json!({ "success": true })));
                    }
                    Err(err) => {
                        let _ = out_tx.send(json_rpc_error(
                            id,
                            INTERNAL_ERROR,
                            format!("Failed to write file: {err}"),
                        ));
                    }
                }
            }

            // Unknown method.
            _ => {
                let _ = out_tx.send(json_rpc_error(
                    id,
                    METHOD_NOT_FOUND,
                    format!("Method not found: {}", request.method),
                ));
            }
        }
    }
    Ok(())
    }.await;

    let cleanup = AgentCx::for_request();
    if let Ok(guard) = active_prompts.lock(&cleanup).await {
        for abort in guard.values() { abort.abort(); }
    }
    if let Ok(mut pending) = pending_permissions.lock() { pending.clear(); }
    mcp::shutdown(&sessions).await;
    result
}

// ============================================================================
// Permission request routing
// ============================================================================

const fn acp_permission_timeout() -> Duration {
    Duration::from_millis(ACP_PERMISSION_TIMEOUT_MS)
}

fn json_rpc_id_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| id.to_string())
}

fn route_permission_response(
    message: &Value,
    pending: &PendingPermissionMap,
    cx: &AgentCx,
) -> bool {
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return false;
    }

    let Some(id) = message.get("id") else {
        return false;
    };
    let key = json_rpc_id_key(id);
    let response = message
        .get("result")
        .cloned()
        .or_else(|| message.get("error").map(|error| json!({ "error": error })))
        .unwrap_or(Value::Null);

    let sender = pending.lock().ok().and_then(|mut guard| guard.remove(&key));

    sender.is_some_and(|sender| {
        let _ = sender.send(cx.cx(), response);
        true
    })
}

fn json_rpc_permission_request(
    request_id: &Value,
    session_id: &str,
    request: &ToolApprovalRequest,
) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "session/request_permission",
        "params": {
            "sessionId": session_id,
            "toolCall": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": request.tool_call_id,
                "title": request.tool_name,
                "kind": classify_tool_kind(&request.tool_name),
                "status": "pending",
                "rawInput": request.arguments,
            },
            "options": [
                {
                    "optionId": ACP_PERMISSION_ALLOW_ONCE,
                    "name": "Allow once",
                    "kind": "allow_once",
                },
                {
                    "optionId": ACP_PERMISSION_REJECT_ONCE,
                    "name": "Reject",
                    "kind": "reject_once",
                },
            ],
        },
    }))
    .expect("serialize json-rpc permission request")
}

fn permission_response_to_decision(response: &Value) -> ToolApprovalDecision {
    if response.get("error").is_some() {
        return ToolApprovalDecision::deny("permission request failed");
    }

    let Some(outcome) = response.get("outcome") else {
        return ToolApprovalDecision::deny("permission response missing outcome");
    };
    match outcome.get("outcome").and_then(Value::as_str) {
        Some("selected") => match outcome.get("optionId").and_then(Value::as_str) {
            Some(ACP_PERMISSION_ALLOW_ONCE) => ToolApprovalDecision::Allow,
            Some(ACP_PERMISSION_REJECT_ONCE) => {
                ToolApprovalDecision::deny("permission rejected by client")
            }
            Some(_) => ToolApprovalDecision::deny("permission response selected unknown option"),
            None => ToolApprovalDecision::deny("permission response selected without optionId"),
        },
        Some("cancelled") => ToolApprovalDecision::deny("permission request cancelled"),
        Some(_) => ToolApprovalDecision::deny("permission response has unknown outcome"),
        None => ToolApprovalDecision::deny("permission response outcome malformed"),
    }
}

// ============================================================================
// Path validation
// ============================================================================

/// Validate that a file path is under at least one session's cwd.
/// If a sessionId is provided, validates against that specific session.
/// Otherwise, validates against any active session's cwd.
/// Returns `Ok(())` if valid, `Err(message)` if rejected.
async fn validate_file_path(
    path_str: &str,
    session_id: Option<&str>,
    sessions: &AcpSessionsMap,
    cx: &AgentCx,
) -> std::result::Result<(), String> {
    let resolved = if let Ok(p) = std::path::Path::new(path_str).canonicalize() {
        p
    } else {
        // If the file doesn't exist yet (write case), canonicalize the parent.
        let parent = std::path::Path::new(path_str).parent();
        match parent.and_then(|p| p.canonicalize().ok()) {
            Some(p) => p.join(
                std::path::Path::new(path_str)
                    .file_name()
                    .unwrap_or_default(),
            ),
            None => {
                return Err(format!(
                    "Path does not exist and parent is invalid: {path_str}"
                ));
            }
        }
    };

    // OwnedMutexGuard: the sessions guard is held across the per-session
    // lock awaits below, and the borrowed guard is !Send (future_not_send).
    let guard = OwnedMutexGuard::lock(Arc::clone(sessions), cx)
        .await
        .map_err(|e| format!("Lock failed: {e}"))?;

    if guard.is_empty() {
        return Err("No active sessions — cannot validate file path".to_string());
    }

    let allowed_cwds: Vec<PathBuf> = if let Some(sid) = session_id {
        match guard.get(sid) {
            Some(state) => {
                if let Ok(s) = OwnedMutexGuard::lock(Arc::clone(state), cx).await {
                    vec![s.cwd.clone()]
                } else {
                    return Err("Session lock failed".to_string());
                }
            }
            None => return Err(format!("Session not found: {sid}")),
        }
    } else {
        let mut cwds = Vec::new();
        for state in guard.values() {
            if let Ok(s) = OwnedMutexGuard::lock(Arc::clone(state), cx).await {
                cwds.push(s.cwd.clone());
            }
        }
        cwds
    };

    // Canonicalize each cwd and check if the resolved path starts with it.
    for cwd in &allowed_cwds {
        if let Ok(canonical_cwd) = cwd.canonicalize()
            && resolved.starts_with(&canonical_cwd)
        {
            return Ok(());
        }
        // Also check without canonicalization for cwd (it may not exist on disk).
        if resolved.starts_with(cwd) {
            return Ok(());
        }
    }

    Err(format!(
        "Path '{path_str}' is outside all session working directories",
    ))
}

// ============================================================================
// Method handlers
// ============================================================================

fn handle_initialize() -> Value {
    let version = env!("CARGO_PKG_VERSION");
    json!({
        "protocolVersion": 1,
        "agentInfo": {
            "name": "pi-agent",
            "version": version,
        },
        "agentCapabilities": {
            // Loads replay the selected conversation before responding and
            // reopen persisted stores when --session-dir is configured.
            "loadSession": true,
            "mcpCapabilities": {
                "http": true,
                "sse": false,
            },
            "promptCapabilities": content::prompt_capabilities(),
            "sessionCapabilities": { "list": {} },
            "_meta": {
                "pi.dev": {
                    "toolApproval": true,
                    "requestPermission": true,
                },
            },
        },
        "authMethods": [],
    })
}

fn select_acp_model_entry(config: &Config, available_models: &[ModelEntry]) -> Option<ModelEntry> {
    if let (Some(default_provider), Some(default_model)) = (
        config.default_provider.as_deref(),
        config.default_model.as_deref(),
    ) && let Some(entry) = available_models.iter().find(|entry| {
        provider_ids_match(&entry.model.provider, default_provider)
            && entry.model.id.eq_ignore_ascii_case(default_model)
    }) {
        return Some(entry.clone());
    }

    if let Some(default_provider) = config.default_provider.as_deref()
        && let Some(entry) = available_models
            .iter()
            .find(|entry| provider_ids_match(&entry.model.provider, default_provider))
    {
        return Some(entry.clone());
    }

    if let Some(default_model) = config.default_model.as_deref()
        && let Some(entry) = available_models
            .iter()
            .find(|entry| entry.model.id.eq_ignore_ascii_case(default_model))
    {
        return Some(entry.clone());
    }

    available_models.first().cloned()
}

fn resolve_acp_thinking_level(
    config: &Config,
    model_entry: &ModelEntry,
) -> crate::model::ThinkingLevel {
    let requested = config
        .default_thinking_level
        .as_deref()
        .and_then(|value| value.parse().ok())
        .unwrap_or(crate::model::ThinkingLevel::XHigh);
    model_entry.clamp_thinking_level(requested)
}

/// Resolve against the actual new/reopened session, without constructing a
/// throwaway agent. ACP retains its configured-default selection when no model
/// choice was supplied, and shares the CLI's explicit model/effort resolver.
fn resolve_acp_selection(
    session: &Session,
    cwd: &std::path::Path,
    options: &AcpOptions,
    cli: &mut crate::cli::Cli,
) -> Result<crate::app::ModelSelection> {
    let mut registry = options.model_registry.clone();
    let mut scoped_models = Vec::new();
    if let Some((provider, model)) = session.effective_model_for_current_path() {
        let entry = registry
            .find(&provider, &model)
            .or_else(|| crate::models::ad_hoc_model_entry(&provider, &model))
            .ok_or_else(|| {
                Error::provider(
                    "acp",
                    format!("Saved session model is not registered: {provider}/{model}"),
                )
            })?;
        cli.provider = Some(provider);
        cli.model = Some(model);
        // A launch option configures new conversations, never the loaded branch.
        cli.thinking = Some(
            match session.effective_thinking_level_for_current_path() {
                Some(level) => entry
                    .clamp_thinking_level(level.parse().map_err(|_| {
                        Error::session("Saved session has an invalid thinking level")
                    })?)
                    .to_string(),
                None => resolve_acp_thinking_level(&options.config, &entry).to_string(),
            },
        );
        registry.merge_entries(vec![entry]);
    } else if cli.provider.is_none() && cli.model.is_none() {
        let scope_override = options
            .config
            .model_scope_overrides
            .as_deref()
            .and_then(|overrides| crate::failover::best_scope_override(overrides, cwd));
        let patterns = cli
            .models
            .as_deref()
            .map(crate::app::parse_models_arg)
            .or_else(|| scope_override.and_then(|scope| scope.enabled_models.clone()))
            .or_else(|| options.config.enabled_models.clone())
            .unwrap_or_default();
        scoped_models = crate::app::resolve_startup_model_scope(
            cli,
            session,
            &patterns,
            &registry,
            &options.config,
            cwd,
        )
        .map_err(|error| Error::config(error.to_string()))?;
        let scoped = scoped_models
            .iter()
            .find(|scoped| {
                options
                    .config
                    .default_provider
                    .as_deref()
                    .is_some_and(|provider| {
                        provider_ids_match(provider, &scoped.model.model.provider)
                    })
                    && options.config.default_model.as_deref().is_some_and(|model| {
                        model.eq_ignore_ascii_case(&scoped.model.model.id)
                    })
            })
            .or_else(|| scoped_models.first());
        let entry = if let Some(scoped) = scoped {
            if cli.thinking.is_none() {
                cli.thinking = scoped.thinking_level.map(|level| level.to_string());
            }
            scoped.model.clone()
        } else {
            if cli.api_key.is_some() {
                return Err(Error::config(
                    "--api-key requires a model to be specified via --provider/--model or --models",
                ));
            }
            select_acp_model_entry(&options.config, &options.available_models)
                .ok_or_else(|| Error::provider("acp", "No models available"))?
        };
        cli.provider = Some(entry.model.provider.clone());
        cli.model = Some(entry.model.id.clone());
        registry.merge_entries(vec![entry]);
    }
    crate::app::select_model_and_thinking(
        cli,
        &options.config,
        session,
        &registry,
        &scoped_models,
        &Config::global_dir(),
    )
    .map_err(|error| Error::config(error.to_string()))
}

/// Build pi's shared system prompt with the host's explicit prompt, context,
/// cwd, and tool controls, plus the editor note. An unreadable explicit prompt
/// rejects session construction instead of silently replacing host instructions.
fn build_acp_system_prompt(
    cli: &crate::cli::Cli,
    cwd: &std::path::Path,
    enabled_tools: &[&str],
    config: &Config,
    skills_prompt: Option<&str>,
) -> Result<String> {
    let test_mode = std::env::var_os("PI_TEST_MODE").is_some();
    let foreign_rules = if config.foreign_rules_enabled() && !test_mode && !cli.no_context_files {
        crate::context_files::discover_foreign_rules(cwd)
    } else {
        crate::context_files::ForeignRules::default()
    };
    let package_dir = crate::app::stable_package_dir(&Config::package_dir(), Some(cwd));
    let mut prompt = crate::app::build_system_prompt(
        cli,
        cwd,
        enabled_tools,
        skills_prompt.filter(|block| enabled_tools.contains(&"read") && !block.is_empty()),
        &Config::global_dir(),
        &package_dir,
        test_mode,
        !cli.hide_cwd_in_prompt,
        Some(&foreign_rules),
        config,
    )
    .map_err(|error| Error::config(format!("Cannot build ACP system prompt: {error}")))?;
    prompt.push_str(
        "\n\nYou are running inside the user's editor via ACP (Agent Client \
         Protocol). When making file changes, explain what you're doing.",
    );
    Ok(prompt)
}

/// Apply the host's requested tools or CLI defaults, excluding terminal-only
/// tools with no ACP surface. Every call still reaches the editor's permission
/// hook because ACP sessions carry no terminal approval state.
fn acp_enabled_tools(cli: &crate::cli::Cli) -> Vec<String> {
    const HOST_COUPLED: [&str; 3] = ["ask", "todo", "submit_plan"];
    cli.enabled_tools()
        .into_iter()
        .filter(|name| !HOST_COUPLED.contains(name))
        .map(String::from)
        .collect()
}

/// Build the backing session for a new ACP session.
///
/// When `--session-dir` is configured, the session persists to that directory
/// using the configured store kind, and autosave is enabled (`save_enabled =
/// true`) so the ACP session can be resumed later via `pi --session`/`--resume`
/// (#102). Without it, ACP keeps its existing in-memory, non-persisted behavior.
/// Takes the two inputs it needs (rather than the whole `AcpOptions`) so it can
/// be unit-tested without constructing auth/runtime handles.
fn new_acp_session(
    session_dir: Option<&PathBuf>,
    config: &Config,
    cwd: &std::path::Path,
) -> (Session, bool) {
    let mut session = session_dir.map_or_else(Session::in_memory, |dir| {
        Session::create_with_dir_and_store(Some(dir.clone()), SessionStoreKind::from_config(config))
    });
    session.header.cwd = cwd.display().to_string();
    (session, session_dir.is_some())
}

fn handle_session_new(
    params: &Value,
    options: &AcpOptions,
    permission_client: Option<&AcpPermissionClient>,
) -> Result<(String, AcpSessionState)> {
    let cwd = history::requested_cwd(params).map_err(|error| Error::session(error.message))?;
    let servers = crate::mcp::config::parse_acp_servers(params, &cwd)
        .map_err(Error::session)?.unwrap_or_default();

    // Create the backing session. Persists to disk when --session-dir is set
    // (save_enabled), otherwise in-memory (existing default behavior).
    let (session, save_enabled) =
        new_acp_session(options.session_dir.as_ref(), &options.config, &cwd);
    let (id, mut state) = build_acp_session(session, save_enabled, cwd.clone(), options, permission_client)?;
    let mcp_state = mcp::prepare(&cwd, &Config::global_dir(), servers);
    if let (Some(agent), Some(mcp_state)) = (state.agent_session.as_mut(), mcp_state.as_ref()) {
        mcp::mount(agent.session_mut(), mcp_state);
    }
    state.mcp = mcp_state;
    Ok((id, state))
}

/// New and reopened sessions use the same tool registry, approval handler,
/// auth resolution, system prompt, and persistence owner. Restore the selected
/// branch's settings instead of another branch's header tip or startup defaults.
fn build_acp_session(
    mut session: Session,
    save_enabled: bool,
    cwd: PathBuf,
    options: &AcpOptions,
    permission_client: Option<&AcpPermissionClient>,
) -> Result<(String, AcpSessionState)> {
    let session_id = session.header.id.clone();

    let mut cli = options.launch.selection_cli()?;
    let enabled_tools = acp_enabled_tools(&cli);
    let enabled_tools: Vec<&str> = enabled_tools.iter().map(String::as_str).collect();
    let tools = if cli.no_tools {
        ToolRegistry::without_builtins(Some(&options.config))
    } else {
        ToolRegistry::new(&enabled_tools, &cwd, Some(&options.config))
    };

    let selection = resolve_acp_selection(&session, &cwd, options, &mut cli)?;
    let model_entry = &selection.model_entry;
    if cli.api_key.is_none()
        && options
            .oauth_refresh_failures
            .iter()
            .any(|provider| provider_ids_match(provider, &model_entry.model.provider))
    {
        return Err(Error::auth(format!(
            "OAuth token refresh failed for {}; run `pi auth login {}` to renew it",
            model_entry.model.provider, model_entry.model.provider,
        )));
    }
    let api_key = crate::app::resolve_api_key(&options.auth, &cli, model_entry)
        .map_err(|error| Error::provider("acp", error.to_string()))?;
    let stream_options =
        crate::app::build_stream_options(&options.config, api_key, &selection, &session);
    crate::app::update_session_for_selection(&mut session, &selection);
    let mut registry = options.model_registry.clone();
    registry.merge_entries(vec![model_entry.clone()]);
    let mut available_models = options.available_models.clone();
    if !available_models.iter().any(|entry| {
        provider_ids_match(&entry.model.provider, &model_entry.model.provider)
            && entry.model.id.eq_ignore_ascii_case(&model_entry.model.id)
    }) {
        available_models.push(model_entry.clone());
    }

    let provider = providers::create_provider(model_entry, None)
        .map_err(|e| Error::provider("acp", e.to_string()))?;

    let system_prompt = build_acp_system_prompt(
        &cli,
        &cwd,
        &enabled_tools,
        &options.config,
        options.skills_prompt.as_deref(),
    )?;

    let agent_config = crate::agent::AgentConfig {
        system_prompt: Some(system_prompt),
        max_tool_iterations: cli.max_tool_iterations.map_or_else(
            crate::agent::resolved_max_tool_iterations_default,
            |limit| crate::agent::clamp_max_tool_iterations(Some(limit)),
        ),
        stream_options,
        block_images: options.config.image_block_images(),
        model_accepts_images: model_entry
            .model
            .input
            .contains(&crate::provider::InputType::Image),
        fail_closed_hooks: options.config.fail_closed_hooks(),
        tool_approval: permission_client
            .map(|client| client.handler_for_session(session_id.clone())),
        keyword_settings: options.config.keywords.clone(),
        max_time: cli.max_time.map(Duration::from_secs),
        turn_recovery: options.config.turn_recovery_mode(),
        approval_state: None,
        bash_settings: options.config.bash.clone(),
        // Configured vault mode and patterns, as the CLI and SDK apply them.
        secrets: options.config.secrets.clone(),
    };

    let agent = crate::agent::Agent::new(provider, tools, agent_config);
    let session_arc = Arc::new(Mutex::new(session));
    let compaction_settings = ResolvedCompactionSettings {
        enabled: options.config.compaction_enabled(),
        reserve_tokens: options.config.compaction_reserve_tokens(),
        keep_recent_tokens: options.config.compaction_keep_recent_tokens(),
        mode: options.config.compaction_mode(),
        render_mode: options.config.compaction_render_mode(),
        context_window_tokens: if model_entry.model.context_window == 0 {
            ResolvedCompactionSettings::default().context_window_tokens
        } else {
            model_entry.model.context_window
        },
    };

    // Wire the model registry and auth storage so the session can switch
    // provider/model at runtime via `session/set_model` (set_provider_model
    // needs the registry to find the target model and resolve its credentials).
    let agent_session = AgentSession::new(agent, session_arc, save_enabled, compaction_settings)
        .with_runtime_handle(options.runtime_handle.clone())
        .with_model_registry(registry.clone())
        .with_auth_storage(options.auth.clone())
        .with_api_key_override(cli.api_key.clone());
    // Keep the exact configured AgentSession for the lifetime of the editor
    // session. The SDK owns one durable recovery driver and its cross-turn
    // fallback state; rebuilding a handle per prompt would lose that state.
    let agent_session =
        AgentSessionHandle::from_session_with_listeners(agent_session, EventListeners::default())
            .with_retry(crate::failover::RetryPolicy::from_config(&options.config))
            .with_failover(FailoverOptions::from_config(
                &options.config,
                registry.models().to_vec(),
                options.auth.clone(),
                cli.api_key,
            ));

    Ok((
        session_id,
        AcpSessionState {
            agent_session: Some(agent_session),
            available_models,
            cwd,
            mcp: None,
        },
    ))
}

// ============================================================================
// Runtime reconfiguration (session/set_model, session/set_config_option)
// ============================================================================
//
// Runtime-vs-restart configuration contract for ACP sessions
// ----------------------------------------------------------
// A live ACP session is backed by an `AgentSession` whose provider/model and
// per-request stream options (thinking level, etc.) can be mutated in place.
// The following options are settable at runtime on an existing session:
//
//   * model              — switch the active provider/model pair. The target
//                          must be a registered model with usable credentials.
//                          Param shapes: `{ "provider": "...", "model": "..." }`,
//                          or just a model id via `model`/`modelId`/`value`
//                          (provider resolved from the registry).
//   * thinking level     — controls reasoning effort. Accepted option names:
//                          `thought_level`, `thinking_level`, `thinking`,
//                          `reasoning`, `effort`, `reasoning_effort`. Values:
//                          off|none|minimal|low|medium|high|xhigh|max (the level is
//                          clamped to what the active model supports — e.g. a
//                          non-reasoning model is forced to `off`).
//
// Everything else (tool set, cwd, system prompt, compaction limits, image
// handling) is fixed at `session/new` time and requires a new session to
// change — `session/set_config_option` returns a structured `INVALID_PARAMS`
// error naming the option and the settable set rather than silently succeeding.

/// Thinking levels offered by the `thought_level` config option, in order.
const THOUGHT_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The session's ACP `configOptions` (GH #245): a `model` select (values are
/// `provider/id`) and a `thought_level` select, each with its current value.
/// `current_model` is `(provider, id)`; `thinking` the current level name.
fn session_config_options(
    current_model: (&str, &str),
    thinking: &str,
    available_models: &[ModelEntry],
) -> Value {
    let (provider, model_id) = current_model;
    let current = format!("{provider}/{model_id}");
    let mut model_options: Vec<Value> = available_models
        .iter()
        .map(|entry| {
            json!({
                "value": format!("{}/{}", entry.model.provider, entry.model.id),
                "name": entry.model.name,
            })
        })
        .collect();
    if !model_options
        .iter()
        .any(|option| option["value"].as_str() == Some(current.as_str()))
    {
        model_options.insert(0, json!({ "value": current, "name": model_id }));
    }
    let thought_options: Vec<Value> = THOUGHT_LEVELS
        .iter()
        .map(|level| json!({ "value": level, "name": level }))
        .collect();
    json!([
        {
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": current,
            "options": model_options,
        },
        {
            "id": "thought_level",
            "name": "Thinking",
            "category": "thought_level",
            "type": "select",
            "currentValue": thinking,
            "options": thought_options,
        },
    ])
}

/// [`session_config_options`] for a live session, or `None` while a prompt
/// holds the agent session.
fn config_options_for(state: &AcpSessionState) -> Option<Value> {
    let agent_session = state.agent_session.as_ref()?;
    Some(config_options_for_handle(agent_session, &state.available_models))
}

fn config_options_for_handle(handle: &AgentSessionHandle, available_models: &[ModelEntry]) -> Value {
    let (provider, model) = handle.model();
    session_config_options(
        (&provider, &model),
        &handle.thinking_level().unwrap_or_default().to_string(),
        available_models,
    )
}

/// ACP does not expose Pi's retry/failover event types. Keep the editor informed
/// with bounded status text and the standard complete config-option update.
/// Provider error bodies stay out of these notices, just as on final errors.
struct AcpRecoveryUpdates {
    out: std::sync::mpsc::SyncSender<String>,
    session_id: String,
    session: Arc<Mutex<Session>>,
    available_models: Vec<ModelEntry>,
    last_configuration: StdMutex<Value>,
}

impl AcpRecoveryUpdates {
    fn new(
        handle: &AgentSessionHandle,
        available_models: Vec<ModelEntry>,
        out: &std::sync::mpsc::SyncSender<String>,
        session_id: &str,
    ) -> Arc<Self> {
        let configuration = config_options_for_handle(handle, &available_models);
        Arc::new(Self {
            out: out.clone(),
            session_id: session_id.to_string(),
            session: handle.session_store(),
            available_models,
            last_configuration: StdMutex::new(configuration),
        })
    }

    fn observe(&self, event: &AgentEvent) {
        match event {
            AgentEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                ..
            } => self.notice(format!(
                "Retrying provider request (attempt {attempt}/{max_attempts}, in {delay_ms} ms)."
            )),
            AgentEvent::FailoverStart {
                from_provider,
                from_model,
                to_provider,
                to_model,
                ..
            } => {
                self.notice(format!(
                    "Provider fallback: {from_provider}/{from_model} → {to_provider}/{to_model}."
                ));
                self.committed_model(to_provider, to_model);
            }
            AgentEvent::FailoverEnd {
                success: true,
                provider,
                model,
                restored_primary: true,
            } => {
                self.notice(format!("Restored primary provider: {provider}/{model}."));
                self.committed_model(provider, model);
            }
            _ => {}
        }
    }

    fn notice(&self, text: String) {
        let _ = self.out.send(json_rpc_notification(
            "session/update",
            json!({
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": format!("\n\n{text}\n\n") },
                },
            }),
        ));
    }

    fn committed_model(&self, provider: &str, model: &str) {
        let configuration = {
            // Recovery publishes the event after its durable transition has
            // released Session. If another reader currently owns the store,
            // finish() reconciles the actual runtime selection before reply.
            let Ok(session) = self.session.try_lock() else {
                return;
            };
            let Some((active_provider, active_model)) = session.effective_model_for_current_path()
            else {
                return;
            };
            if !provider_ids_match(provider, &active_provider)
                || !model.eq_ignore_ascii_case(&active_model)
            {
                return;
            }
            let thinking = session
                .effective_thinking_level_for_current_path()
                .unwrap_or_else(|| "off".to_string());
            session_config_options((provider, model), &thinking, &self.available_models)
        };
        self.configuration(configuration);
    }

    fn configuration(&self, configuration: Value) {
        {
            let mut previous = self
                .last_configuration
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *previous == configuration {
                return;
            }
            previous.clone_from(&configuration);
        }
        let _ = self.out.send(json_rpc_notification(
            "session/update",
            json!({
                "sessionId": self.session_id,
                "update": {
                    "sessionUpdate": "config_option_update",
                    "configOptions": configuration,
                },
            }),
        ));
    }

    fn finish(&self, handle: &AgentSessionHandle) {
        self.configuration(config_options_for_handle(handle, &self.available_models));
    }
}

/// The option id of a `session/set_config_option` request: ACP's `configId`,
/// or the older `name`/`key` this server accepted first.
fn config_option_id(params: &Value) -> Option<&str> {
    ["configId", "name", "key"]
        .iter()
        .find_map(|key| params.get(*key).and_then(Value::as_str))
}

/// A configuration option recognized by `session/set_config_option`.
#[derive(Debug)]
enum RuntimeConfigOption {
    /// Reasoning/thinking effort, applied to the live session's stream options.
    ThinkingLevel(crate::model::ThinkingLevel),
}

/// The set of `session/set_config_option` names this server understands, for
/// inclusion in actionable error messages.
const SETTABLE_CONFIG_OPTIONS: &str =
    "model, thought_level (aliases: thinking_level, thinking, reasoning, effort, reasoning_effort)";

/// Resolve the target `(provider, model)` for a `session/set_model` request.
///
/// Accepts either an explicit `{provider, model}` pair or a bare model
/// identifier supplied via `model`, `modelId`, or `value` (the ACP client in
/// issue #105 sends `config=model value=gpt-5.5`). When only a model id is
/// given, the provider is resolved from the registry. Returns a human-readable
/// error string on missing/unknown input.
fn resolve_set_model_target(
    params: &Value,
    registry: &ModelRegistry,
) -> std::result::Result<(String, String), String> {
    let provider = params
        .get("provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let model = params
        .get("model")
        .and_then(Value::as_str)
        .or_else(|| params.get("modelId").and_then(Value::as_str))
        .or_else(|| params.get("value").and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let Some(model) = model else {
        return Err(
            "Missing required parameter: model (or modelId/value) for session/set_model"
                .to_string(),
        );
    };

    if let Some(provider) = provider {
        // Validate the explicit pair against the registry up front so the error
        // names what was requested rather than a generic switch failure.
        if registry.find(provider, model).is_none() {
            return Err(format!(
                "Unknown model: provider={provider} model={model} (not in the model registry)"
            ));
        }
        return Ok((provider.to_string(), model.to_string()));
    }

    if let Some((provider, model_id)) = model.split_once('/')
        && let Some(entry) = registry.find(provider, model_id)
    {
        return Ok((entry.model.provider, entry.model.id));
    }

    // No provider given — resolve it from the registry by model id.
    match registry.find_by_id(model) {
        Some(entry) => Ok((entry.model.provider, entry.model.id)),
        None => Err(format!(
            "Unknown model: {model} (no provider supplied and no match in the model registry)"
        )),
    }
}

/// Parse a `session/set_config_option` request into a recognized option.
///
/// Returns `Ok(Some(_))` for a settable option, or an error string for an
/// unknown option name or an invalid value. (`Ok(None)` is unused today but
/// keeps room for options that are accepted-but-ignored.)
fn parse_config_option(
    name: &str,
    value: &Value,
) -> std::result::Result<RuntimeConfigOption, String> {
    let key = name.trim().to_ascii_lowercase();
    match key.as_str() {
        "thought_level" | "thinking_level" | "thinking" | "reasoning" | "effort"
        | "reasoning_effort" => {
            // Accept a JSON string ("off") or a bare number (0..=4).
            let raw = value.as_str().map(str::to_string).or_else(|| {
                value
                    .as_i64()
                    .map(|n| n.to_string())
                    .or_else(|| value.as_u64().map(|n| n.to_string()))
            });
            let Some(raw) = raw else {
                return Err(format!(
                    "Invalid value for config option '{name}': expected a string or integer thinking level (off|minimal|low|medium|high|xhigh|max)"
                ));
            };
            raw.parse::<crate::model::ThinkingLevel>().map_or_else(
                |_| {
                    Err(format!(
                        "Invalid value for config option '{name}': '{raw}' (expected off|minimal|low|medium|high|xhigh|max)"
                    ))
                },
                |level| Ok(RuntimeConfigOption::ThinkingLevel(level)),
            )
        }
        _ => Err(format!(
            "Unknown or non-runtime config option: '{name}'. Settable at runtime: {SETTABLE_CONFIG_OPTIONS}. Other options are fixed at session/new and require a new session."
        )),
    }
}

/// Resolve and apply either editor model selector through the live session's
/// registry. It includes ad-hoc startup entries that the process catalog lacks.
async fn apply_set_model_request(
    session_state: &Arc<Mutex<AcpSessionState>>,
    params: &Value,
    cx: &AgentCx,
) -> std::result::Result<(String, String), String> {
    let Ok(mut guard) = OwnedMutexGuard::lock(Arc::clone(session_state), cx).await else {
        return Err("session state lock unavailable".to_string());
    };
    let Some(agent_session) = guard.agent_session.as_mut() else {
        return Err("Cannot change model while a prompt is in progress".to_string());
    };
    let registry = agent_session
        .session()
        .model_registry()
        .ok_or_else(|| "session model registry unavailable".to_string())?;
    let (provider, model) = resolve_set_model_target(params, registry)?;
    agent_session
        .set_model(&provider, &model)
        .await
        .map_err(|error| error.to_string())?;
    Ok(agent_session.model())
}

/// Apply a resolved `session/set_model` to a live session.
///
/// Returns the active `(provider, model)` on success. The agent session may be
/// `None` if a prompt is currently in flight (it is taken out of the state
/// during a turn); callers should surface that as a retryable error.
#[cfg(test)]
async fn apply_set_model(
    session_state: &Arc<Mutex<AcpSessionState>>,
    provider: &str,
    model: &str,
    cx: &AgentCx,
) -> std::result::Result<(String, String), String> {
    apply_set_model_request(
        session_state,
        &json!({ "provider": provider, "model": model }),
        cx,
    )
    .await
}

/// Apply a parsed `session/set_config_option` to a live session.
async fn apply_set_config_option(
    session_state: &Arc<Mutex<AcpSessionState>>,
    option: RuntimeConfigOption,
    cx: &AgentCx,
) -> std::result::Result<(), String> {
    // OwnedMutexGuard: held across the set_* awaits below (future_not_send).
    let Ok(mut guard) = OwnedMutexGuard::lock(Arc::clone(session_state), cx).await else {
        return Err("session state lock unavailable".to_string());
    };
    let Some(agent_session) = guard.agent_session.as_mut() else {
        return Err("Cannot change configuration while a prompt is in progress".to_string());
    };
    match option {
        RuntimeConfigOption::ThinkingLevel(level) => agent_session
            .set_thinking_level(level)
            .await
            .map_err(|e| e.to_string()),
    }
}

/// Execute a prompt for a session and stream `session/update` notifications.
///
/// Returns the ACP `stopReason` string so the dispatcher can attach it to the
/// `session/prompt` response (which only completes when the turn does).
async fn run_prompt(
    session_state: Arc<Mutex<AcpSessionState>>,
    message: Vec<ContentBlock>,
    mcp_command: Option<mcp::Command>,
    abort_signal: AbortSignal,
    out_tx: std::sync::mpsc::SyncSender<String>,
    session_id: String,
    cx: AgentCx,
) -> &'static str {
    if abort_signal.is_aborted() || cx.is_cancel_requested() {
        return ACP_STOP_REASON_CANCELLED;
    }
    // Take the agent_session out of the lock, run the prompt, then put it back.
    // Holding the session mutex across the whole turn would block session/cancel
    // and session/list. The concurrent-prompt guard upstream guarantees only one
    // task is in here per session at a time, so the Option swap is safe.
    let taken = match session_state.lock(&cx).await {
        Ok(mut guard) => guard
            .agent_session
            .take()
            .map(|agent| (agent, guard.mcp.clone(), guard.available_models.clone()))
            .ok_or_else(|| Error::session("Agent session is unavailable while a prompt is active")),
        Err(error) => Err(Error::from(error)),
    };
    let (mut agent_session, mcp_state, available_models) = match taken {
        Ok(taken) => taken,
        Err(error) => return report_prompt_error(&out_tx, &session_id, &error).await,
    };
    let recovery_updates =
        AcpRecoveryUpdates::new(&agent_session, available_models, &out_tx, &session_id);
    let recovery_callback = Arc::clone(&recovery_updates);
    let event_handler = build_acp_event_handler(out_tx.clone(), session_id.clone());

    let prepared = mcp::before_prompt(
        mcp_state.as_ref(), agent_session.session_mut(), mcp_command,
        &abort_signal, &cx, &out_tx, &session_id,
    ).await;
    let stop_reason = if let Some(reason) = prepared {
        reason
    } else {
        match agent_session
            .prompt_with_content_with_abort(message, abort_signal, move |event| {
                recovery_callback.observe(&event);
                event_handler(event);
            })
            .await
        {
            Ok(message) if message.stop_reason == crate::model::StopReason::Error => {
                let error = Error::provider(
                    message.provider,
                    message
                        .error_message
                        .unwrap_or_else(|| "Request failed".to_string()),
                );
                report_prompt_error(&out_tx, &session_id, &error).await
            }
            Ok(message) => {
                // Synthetic boundary messages have no provider stream. Publish
                // only after the SDK completes this turn's durable persistence.
                if let Some(marker) = crate::agent::time_cap_marker(&message) {
                    let _ = history::send_line(
                        &out_tx,
                        json_rpc_notification(
                            "session/update",
                            json!({
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": { "type": "text", "text": marker },
                                },
                            }),
                        ),
                    )
                    .await;
                }
                map_stop_reason(message.stop_reason)
            }
            Err(error) => report_prompt_error(&out_tx, &session_id, &error).await,
        }
    };

    // Even a failed/aborted fallback can have committed a different model.
    // Publish the final selections before returning the prompt response.
    recovery_updates.finish(&agent_session);
    // Cancellation ends provider work, not the obligation to return its owner
    // to the editor session. Cleanup must not use a cancelled request context.
    let cleanup_cx = AgentCx::for_request();
    if let Ok(mut guard) = session_state.lock(&cleanup_cx).await {
        guard.agent_session = Some(agent_session);
    }

    stop_reason
}

/// ACP has no error stop reason. Emit one notice after the complete session
/// outcome, including its save: stream terminals are deliberately not rendered
/// by `build_acp_event_handler`, so `Error`/`MessageEnd`/`AgentEnd` events cannot
/// duplicate this notice. MCP operator responses use their own completed path.
async fn report_prompt_error(
    out_tx: &std::sync::mpsc::SyncSender<String>,
    session_id: &str,
    error: &Error,
) -> &'static str {
    if matches!(error, Error::Aborted) {
        return ACP_STOP_REASON_CANCELLED;
    }
    let summary = if error.is_session_persistence() {
        "Session persistence failed. Further prompts are blocked for this session. \
         Check storage space and permissions, then start a new session or resume the saved session."
            .to_string()
    } else if let Error::Provider { provider, message } = error {
        let summary = crate::error::ProviderErrorSummary::from_error_text(Some(provider), message);
        format!("{}\n{}", summary.headline(), summary.retry_note(None))
    } else {
        // Hint context and raw provider payloads can contain credentials,
        // private prompts or paths. Only the public category summary is sent.
        error.hints().summary
    };
    let _ = history::send_line(
        out_tx,
        json_rpc_notification(
            "session/update",
            json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": format!("\n\nError: {summary}\n") },
                },
            }),
        ),
    )
    .await;
    ACP_STOP_REASON_ERROR
}

// ACP stopReason values per the protocol spec.
const ACP_STOP_REASON_END_TURN: &str = "end_turn";
const ACP_STOP_REASON_MAX_TOKENS: &str = "max_tokens";
const ACP_STOP_REASON_CANCELLED: &str = "cancelled";
// Spec lists: end_turn | max_tokens | max_turn_requests | refusal | cancelled.
// Provider/local errors use end_turn to keep the response well-formed;
// report_prompt_error publishes their safe summary before the response.
const ACP_STOP_REASON_ERROR: &str = "end_turn";

const fn map_stop_reason(reason: crate::model::StopReason) -> &'static str {
    use crate::model::StopReason;
    match reason {
        StopReason::Stop | StopReason::ToolUse | StopReason::PauseTurn | StopReason::Refusal => {
            ACP_STOP_REASON_END_TURN
        }
        StopReason::Length => ACP_STOP_REASON_MAX_TOKENS,
        StopReason::Aborted => ACP_STOP_REASON_CANCELLED,
        StopReason::Error => ACP_STOP_REASON_ERROR,
    }
}

/// Extract baseline text/link content. Rich prompt blocks are handled by the
/// content module and must never be flattened through this text-only helper.
fn extract_prompt_text(blocks: &[Value]) -> std::result::Result<String, String> {
    let mut out = String::new();
    for block in blocks {
        let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
        match block_type {
            "text" => {
                let Some(text) = block.get("text").and_then(Value::as_str) else {
                    return Err(
                        "Prompt block of type \"text\" missing required field \"text\"".to_string(),
                    );
                };
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
            "resource_link" => {
                // Surface the URI inline without implicitly opening it.
                let Some(uri) = block.get("uri").and_then(Value::as_str) else {
                    return Err(
                        "Prompt block of type \"resource_link\" missing required field \"uri\""
                            .to_string(),
                    );
                };
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(uri);
            }
            "" => {
                return Err(
                    "Prompt block missing required discriminator field \"type\"".to_string()
                );
            }
            other => {
                return Err(format!(
                    "Prompt block type \"{other}\" is not supported by the text-only content decoder"
                ));
            }
        }
    }
    Ok(out)
}

/// Build an event handler that translates `AgentEvent`s into ACP `session/update`
/// notifications. The wire shape is:
///
/// ```text
/// { "jsonrpc": "2.0", "method": "session/update",
///   "params": { "sessionId": ..., "update": { "sessionUpdate": <kind>, ... } } }
/// ```
fn build_acp_event_handler(
    out_tx: std::sync::mpsc::SyncSender<String>,
    session_id: String,
) -> impl Fn(AgentEvent) + Send + Sync + 'static {
    move |event: AgentEvent| {
        let update = match &event {
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => match assistant_message_event {
                AssistantMessageEvent::TextDelta { delta, .. } => Some(json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": delta },
                })),
                AssistantMessageEvent::ThinkingDelta { delta, .. } => Some(json!({
                    "sessionUpdate": "agent_thought_chunk",
                    "content": { "type": "text", "text": delta },
                })),
                // Everything else (TextEnd, ToolCallEnd, etc.) is intentionally
                // skipped: TextEnd carries text already delivered via TextDelta and
                // would double the message; the tool_call announcement is sent
                // from ToolExecutionStart so status transitions line up
                // (pending -> in_progress -> completed); ToolCallEnd is
                // model-stream metadata that would duplicate the announcement.
                _ => None,
            },

            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => Some(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": tool_call_id,
                "title": tool_name,
                "kind": classify_tool_kind(tool_name),
                "status": "pending",
                "rawInput": args,
            })),

            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name: _,
                args: _,
                partial_result,
            } => {
                let tool_content = content::tool_result_content(&partial_result.content);
                let mut update = json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tool_call_id,
                    "status": "in_progress",
                });
                if !tool_content.is_empty() {
                    update["content"] = json!(tool_content);
                }
                Some(update)
            }

            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name: _,
                result,
                is_error,
            } => {
                let tool_content = content::tool_result_content(&result.content);

                Some(json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tool_call_id,
                    "status": if *is_error { "failed" } else { "completed" },
                    "content": tool_content,
                }))
            }

            // Turn-/agent-level events have no direct ACP equivalent. They were
            // useful for pi's own debug stream but ACP clients render the chunks
            // and tool_call updates above without needing them.
            _ => None,
        };

        if let Some(update) = update {
            let _ = out_tx.send(json_rpc_notification(
                "session/update",
                json!({
                    "sessionId": session_id,
                    "update": update,
                }),
            ));
        }
    }
}

/// Map a pi tool name to one of ACP's `kind` values. The enum is small and
/// drives client-side icons; "other" is the documented fallback.
fn classify_tool_kind(tool_name: &str) -> &'static str {
    let lower = tool_name.to_ascii_lowercase();
    if matches!(lower.as_str(), "read" | "read_text_file" | "view" | "cat") {
        "read"
    } else if matches!(
        lower.as_str(),
        "edit" | "write" | "write_text_file" | "patch" | "apply_patch" | "create"
    ) {
        "edit"
    } else if matches!(lower.as_str(), "delete" | "rm" | "remove") {
        "delete"
    } else if matches!(lower.as_str(), "move" | "mv" | "rename") {
        "move"
    } else if matches!(
        lower.as_str(),
        "search" | "grep" | "ripgrep" | "rg" | "find" | "glob"
    ) {
        "search"
    } else if matches!(
        lower.as_str(),
        "execute" | "bash" | "shell" | "run" | "exec"
    ) {
        "execute"
    } else if matches!(lower.as_str(), "fetch" | "http" | "curl" | "web_fetch") {
        "fetch"
    } else if matches!(lower.as_str(), "think" | "thinking") {
        "think"
    } else {
        "other"
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssistantMessage, StopReason, StreamEvent, TextContent};
    use crate::provider::{InputType, Model, ModelCost};
    use asupersync::runtime::RuntimeBuilder;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;

    #[derive(Clone, Copy)]
    enum PromptTestOutcome {
        Complete,
        OpenError,
        StreamError,
        Aborted,
        StreamAborted,
    }

    struct PromptTestProvider {
        outcome: PromptTestOutcome,
        calls: AtomicUsize,
        save_fault: Option<(Arc<Mutex<Session>>, PathBuf)>,
    }

    const PRIVATE_PROMPT_ERROR: &str =
        "HTTP 503: {\"authorization\":\"PRIVATE-CREDENTIAL\",\"prompt\":\"PRIVATE-PROMPT\"}";

    #[async_trait::async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl crate::provider::Provider for PromptTestProvider {
        fn name(&self) -> &str {
            "acp-test-provider"
        }

        fn api(&self) -> &str {
            "acp-test-api"
        }

        fn model_id(&self) -> &str {
            "acp-test-model"
        }

        async fn stream(
            &self,
            _context: &crate::provider::Context<'_>,
            _options: &StreamOptions,
        ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some((stored, path)) = &self.save_fault {
                stored.try_lock().expect("prompt already persisted").path = Some(path.clone());
            }
            match self.outcome {
                PromptTestOutcome::OpenError => {
                    return Err(Error::provider(self.name(), PRIVATE_PROMPT_ERROR));
                }
                PromptTestOutcome::Aborted => return Err(Error::Aborted),
                _ => {}
            }
            let partial = AssistantMessage {
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                api: self.api().to_string(),
                ..AssistantMessage::default()
            };
            let mut message = partial.clone();
            let mut events = vec![Ok(StreamEvent::Start { partial })];
            if matches!(self.outcome, PromptTestOutcome::Complete) {
                message.content = vec![ContentBlock::Text(TextContent::new("Provider reply"))];
                events.push(Ok(StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "Provider reply".to_string(),
                }));
                events.push(Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message,
                }));
            } else {
                message.stop_reason = if matches!(self.outcome, PromptTestOutcome::StreamAborted) {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                message.error_message = Some(PRIVATE_PROMPT_ERROR.to_string());
                events.push(Ok(StreamEvent::Error {
                    reason: message.stop_reason,
                    error: message,
                }));
            }
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    async fn prompt_test_state(
        root: &std::path::Path,
        outcome: PromptTestOutcome,
        fail_after_provider: bool,
    ) -> (
        Arc<Mutex<AcpSessionState>>,
        Arc<PromptTestProvider>,
        Arc<Mutex<Session>>,
    ) {
        let mut stored = Session::create_with_dir_and_store(
            Some(root.to_path_buf()),
            SessionStoreKind::Jsonl,
        );
        stored.set_model_header(
            Some("acp-test-provider".to_string()),
            Some("acp-test-model".to_string()),
            Some("off".to_string()),
        );
        stored.save().await.expect("persist initial metadata");
        let stored = Arc::new(Mutex::new(stored));
        let provider = Arc::new(PromptTestProvider {
            outcome,
            calls: AtomicUsize::new(0),
            save_fault: fail_after_provider.then(|| (Arc::clone(&stored), root.to_path_buf())),
        });
        let agent = crate::agent::Agent::new(
            Arc::clone(&provider) as Arc<dyn crate::provider::Provider>,
            ToolRegistry::new(&[], root, None),
            crate::agent::AgentConfig::default(),
        );
        let session = AgentSession::new(
            agent,
            Arc::clone(&stored),
            true,
            ResolvedCompactionSettings {
                enabled: false,
                ..ResolvedCompactionSettings::default()
            },
        );
        let state = Arc::new(Mutex::new(AcpSessionState {
            agent_session: Some(AgentSessionHandle::from_session_with_listeners(
                session,
                EventListeners::default(),
            )),
            available_models: Vec::new(),
            cwd: root.to_path_buf(),
            mcp: None,
        }));
        (state, provider, stored)
    }

    async fn drive_test_prompt(
        state: Arc<Mutex<AcpSessionState>>,
        signal: AbortSignal,
    ) -> (&'static str, Vec<Value>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        let reason = run_prompt(
            state,
            vec![ContentBlock::Text(TextContent::new("editor prompt"))],
            None,
            signal,
            tx,
            "editor-session".to_string(),
            AgentCx::for_current_or_request(),
        )
        .await;
        let updates = rx
            .try_iter()
            .map(|line| serde_json::from_str(&line).expect("ACP notification"))
            .collect();
        (reason, updates)
    }

    fn prompt_notice_text(update: &Value) -> &str {
        assert_eq!(update["method"], "session/update");
        assert_eq!(update["params"]["sessionId"], "editor-session");
        assert_eq!(
            update["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        update["params"]["update"]["content"]["text"]
            .as_str()
            .expect("text notice")
    }

    #[test]
    fn prompt_errors_are_reported_once_without_private_provider_payloads() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            for outcome in [PromptTestOutcome::OpenError, PromptTestOutcome::StreamError] {
                let root = tempfile::tempdir().unwrap();
                let (state, provider, _) = prompt_test_state(root.path(), outcome, false).await;
                let (_, signal) = AbortHandle::new();
                let (reason, updates) = drive_test_prompt(state, signal).await;
                assert_eq!(reason, ACP_STOP_REASON_ERROR);
                assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
                assert_eq!(
                    updates.len(),
                    1,
                    "stream terminal and result must not duplicate errors"
                );
                let text = prompt_notice_text(&updates[0]);
                assert!(text.contains("HTTP 503"), "{text}");
                assert!(!text.contains("PRIVATE-CREDENTIAL"));
                assert!(!text.contains("PRIVATE-PROMPT"));
            }
        });
    }

    #[test]
    fn prompt_save_failure_and_quarantine_are_visible_before_returning_to_the_editor() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            for after_provider in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let (state, provider, stored) = prompt_test_state(
                    root.path(),
                    PromptTestOutcome::Complete,
                    after_provider,
                )
                .await;
                let original = stored.try_lock().unwrap().path.clone();
                if !after_provider {
                    stored.try_lock().unwrap().path = Some(root.path().to_path_buf());
                }
                let (_, signal) = AbortHandle::new();
                let (reason, updates) = drive_test_prompt(Arc::clone(&state), signal).await;
                assert_eq!(reason, ACP_STOP_REASON_ERROR);
                assert_eq!(updates.len(), if after_provider { 2 } else { 1 });
                let text = prompt_notice_text(updates.last().unwrap());
                assert!(text.contains("Session persistence failed"), "{text}");
                assert!(text.contains("resume the saved session"));
                assert!(!text.contains(&root.path().display().to_string()));

                stored.try_lock().unwrap().path = original;
                let (_, signal) = AbortHandle::new();
                let (reason, updates) = drive_test_prompt(state, signal).await;
                assert_eq!(reason, ACP_STOP_REASON_ERROR);
                assert_eq!(
                    updates.len(),
                    1,
                    "rejected admission still needs a visible error"
                );
                assert!(prompt_notice_text(&updates[0]).contains("Session persistence failed"));
                assert_eq!(
                    provider.calls.load(Ordering::SeqCst),
                    usize::from(after_provider)
                );
            }
        });
    }

    #[test]
    fn prompt_preflight_errors_do_not_expose_raw_error_context() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let (state, provider, stored) =
                prompt_test_state(root.path(), PromptTestOutcome::Complete, false).await;
            stored.try_lock().unwrap().header.provider =
                Some("PRIVATE-UNRESOLVED-PROVIDER".to_string());
            let (_, signal) = AbortHandle::new();
            let (reason, updates) = drive_test_prompt(state, signal).await;
            assert_eq!(reason, ACP_STOP_REASON_ERROR);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
            assert_eq!(updates.len(), 1);
            let text = prompt_notice_text(&updates[0]);
            assert!(text.contains("Validation failed"));
            assert!(!text.contains("PRIVATE-UNRESOLVED-PROVIDER"));
        });
    }

    #[test]
    fn prompt_cancellation_retains_cancelled_without_an_error_notice() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            for outcome in [PromptTestOutcome::Aborted, PromptTestOutcome::StreamAborted] {
                let root = tempfile::tempdir().unwrap();
                let (state, provider, _) = prompt_test_state(root.path(), outcome, false).await;
                let (_, signal) = AbortHandle::new();
                let (reason, updates) = drive_test_prompt(Arc::clone(&state), signal).await;
                assert_eq!(reason, ACP_STOP_REASON_CANCELLED);
                assert!(updates.is_empty());
                assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
                assert!(state.try_lock().unwrap().agent_session.is_some());

                let (abort, signal) = AbortHandle::new();
                abort.abort();
                let (reason, updates) = drive_test_prompt(state, signal).await;
                assert_eq!(reason, ACP_STOP_REASON_CANCELLED);
                assert!(updates.is_empty());
                assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
            }
        });
    }

    #[test]
    fn acp_offers_the_cli_default_tools_without_host_coupled_ones() {
        let cli = AcpLaunchOptions::default()
            .selection_cli()
            .expect("neutral ACP CLI");
        let tools = acp_enabled_tools(&cli);
        for expected in [
            "read",
            "bash",
            "edit",
            "hashline_edit",
            "ast_grep",
            "web_search",
        ] {
            assert!(
                tools.iter().any(|t| t == expected),
                "{expected} in {tools:?}"
            );
        }
        for host_only in ["ask", "todo", "submit_plan"] {
            assert!(
                !tools.iter().any(|t| t == host_only),
                "{host_only} in {tools:?}"
            );
        }
    }

    /// ACP sessions get pi's real system prompt (full tool guidance, every
    /// context file) plus the editor note, not the old hand-written one.
    #[test]
    fn acp_system_prompt_is_pi_prompt_with_context_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("CLAUDE.md"), "acp-claude-md-marker").expect("write");
        let tools = ["read", "bash", "edit", "write", "grep", "find", "ls"];
        let cli = AcpLaunchOptions::default()
            .selection_cli()
            .expect("neutral ACP CLI");
        let prompt = build_acp_system_prompt(
            &cli,
            dir.path(),
            &tools,
            &Config::default(),
            Some("\n\n<available_skills>acp-skill-marker</available_skills>"),
        )
        .expect("ACP system prompt");
        assert!(prompt.contains("acp-skill-marker"), "skills are listed");
        assert!(
            prompt.contains("Make surgical edits to files (find exact text and replace)"),
            "pi's own tool guidance: {prompt}"
        );
        assert!(prompt.contains("via ACP (Agent Client Protocol)"));
        // Context files are skipped in PI_TEST_MODE by design.
        if std::env::var_os("PI_TEST_MODE").is_none() {
            assert!(prompt.contains("acp-claude-md-marker"), "CLAUDE.md is read");
        }
    }

    #[test]
    fn new_acp_session_in_memory_without_session_dir() {
        // No --session-dir → existing behavior: in-memory, persistence disabled.
        let (session, save_enabled) =
            new_acp_session(None, &Config::default(), std::path::Path::new("/tmp/proj"));
        assert!(
            !save_enabled,
            "ACP without --session-dir must keep persistence disabled"
        );
        assert!(
            session.session_dir.is_none(),
            "no session dir should be set: {:?}",
            session.session_dir
        );
        assert_eq!(session.header.cwd, "/tmp/proj");
    }

    #[test]
    fn new_acp_session_persists_with_session_dir() {
        // --session-dir set → session persists there and autosave is enabled (#102).
        let dir = PathBuf::from("/tmp/acp-sessions");
        let (session, save_enabled) = new_acp_session(
            Some(&dir),
            &Config::default(),
            std::path::Path::new("/tmp/proj"),
        );
        assert!(
            save_enabled,
            "ACP with --session-dir must enable persistence (#102)"
        );
        assert_eq!(
            session.session_dir.as_deref(),
            Some(dir.as_path()),
            "session must persist to the provided --session-dir"
        );
        assert_eq!(session.header.cwd, "/tmp/proj");
    }

    fn test_model_entry(provider: &str, id: &str) -> ModelEntry {
        ModelEntry {
            model: Model {
                id: id.to_string(),
                name: id.to_string(),
                api: "openai-responses".to_string(),
                provider: provider.to_string(),
                base_url: "https://example.invalid".to_string(),
                reasoning: true,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 8_192,
                headers: HashMap::new(),
            },
            api_key: None,
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        }
    }

    fn pending_permissions_empty(pending: &PendingPermissionMap) -> bool {
        pending.lock().is_ok_and(|guard| guard.is_empty())
    }

    #[test]
    fn json_rpc_ok_response_format() {
        let response = json_rpc_ok(Value::Number(1.into()), json!({"key": "value"}));
        let parsed: Value = serde_json::from_str(&response).expect("valid json");
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["result"]["key"], "value");
        assert!(parsed.get("error").is_none());
    }

    #[test]
    fn json_rpc_error_response_format() {
        let response = json_rpc_error(Value::String("test-id".into()), PARSE_ERROR, "bad json");
        let parsed: Value = serde_json::from_str(&response).expect("valid json");
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], "test-id");
        assert!(parsed.get("result").is_none());
        assert_eq!(parsed["error"]["code"], PARSE_ERROR);
        assert_eq!(parsed["error"]["message"], "bad json");
    }

    #[test]
    fn json_rpc_notification_format() {
        let notif = json_rpc_notification(
            "session/update",
            json!({
                "sessionId": "sess-1",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "hi" },
                },
            }),
        );
        let parsed: Value = serde_json::from_str(&notif).expect("valid json");
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "session/update");
        assert_eq!(parsed["params"]["sessionId"], "sess-1");
        assert_eq!(
            parsed["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert!(parsed.get("id").is_none());
    }

    /// GH #245: configOptions carry ACP's reserved `model` and
    /// `thought_level` selects with current values; a current model missing
    /// from the available list is still offered.
    #[test]
    fn session_config_options_follow_the_acp_shape() {
        let mut entry = crate::models::ad_hoc_model_entry("openai", "gpt-5").expect("entry");
        entry.model.name = "GPT-5".to_string();
        let options = session_config_options(("openai", "gpt-5"), "high", &[entry]);
        let model = &options[0];
        assert_eq!(model["id"], "model");
        assert_eq!(model["category"], "model");
        assert_eq!(model["type"], "select");
        assert_eq!(model["currentValue"], "openai/gpt-5");
        assert_eq!(model["options"][0]["value"], "openai/gpt-5");
        assert_eq!(model["options"][0]["name"], "GPT-5");
        let thought = &options[1];
        assert_eq!(thought["id"], "thought_level");
        assert_eq!(thought["category"], "thought_level");
        assert_eq!(thought["currentValue"], "high");
        assert_eq!(thought["options"].as_array().map(Vec::len), Some(7));

        let unlisted = session_config_options(("local", "llama"), "off", &[]);
        assert_eq!(unlisted[0]["options"][0]["value"], "local/llama");
    }

    #[test]
    fn set_config_option_reads_the_acp_config_id() {
        assert_eq!(
            config_option_id(&json!({ "configId": "model", "value": "x" })),
            Some("model")
        );
        assert_eq!(
            config_option_id(&json!({ "name": "thinking" })),
            Some("thinking")
        );
        assert_eq!(config_option_id(&json!({ "value": "x" })), None);
    }

    #[test]
    fn handle_initialize_returns_correct_shape() {
        let result = handle_initialize();

        // ACP requires protocolVersion as an integer, not a string.
        assert_eq!(result["protocolVersion"], 1);
        assert_eq!(result["agentInfo"]["name"], "pi-agent");
        assert_eq!(result["agentInfo"]["version"], env!("CARGO_PKG_VERSION"));
        // Loading now replays history before returning its response.
        assert_eq!(result["agentCapabilities"]["loadSession"], true);
        // GH #245: session/list is implemented, so it is advertised.
        assert!(result["agentCapabilities"]["sessionCapabilities"]["list"].is_object());
        // Native image and embedded-context prompt decoding is wired to the agent.
        assert_eq!(
            result["agentCapabilities"]["promptCapabilities"]["audio"],
            false
        );
        assert_eq!(
            result["agentCapabilities"]["promptCapabilities"]["image"],
            true
        );
        assert_eq!(
            result["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
            true
        );
        // mcpCapabilities advertised explicitly so the client knows transports.
        assert_eq!(
            result["agentCapabilities"]["mcpCapabilities"]["http"],
            true
        );
        assert_eq!(result["agentCapabilities"]["mcpCapabilities"]["sse"], false);
        // Tool approval is exposed as implementation metadata; the standard
        // permission request itself is an Agent -> Client JSON-RPC call.
        assert_eq!(
            result["agentCapabilities"]["_meta"]["pi.dev"]["toolApproval"],
            true
        );
        assert_eq!(
            result["agentCapabilities"]["_meta"]["pi.dev"]["requestPermission"],
            true
        );
        // authMethods is required even when empty.
        assert!(result["authMethods"].is_array());
        assert_eq!(result["authMethods"].as_array().unwrap().len(), 0);
        // Old fields must be gone — old clients that read them will fail loudly.
        assert!(result.get("serverInfo").is_none());
        assert!(result.get("capabilities").is_none());
    }

    #[test]
    fn select_acp_model_entry_prefers_exact_configured_model() {
        let config = Config {
            default_provider: Some("anthropic".to_string()),
            default_model: Some("claude-opus-4-5".to_string()),
            ..Config::default()
        };
        let available = vec![
            test_model_entry("openai", "gpt-5.2"),
            test_model_entry("anthropic", "claude-opus-4-5"),
        ];

        let selected = select_acp_model_entry(&config, &available).expect("selected model");

        assert_eq!(selected.model.provider, "anthropic");
        assert_eq!(selected.model.id, "claude-opus-4-5");
    }

    #[test]
    fn select_acp_model_entry_prefers_default_provider_when_model_is_unset() {
        let config = Config {
            default_provider: Some("anthropic".to_string()),
            ..Config::default()
        };
        let available = vec![
            test_model_entry("openai", "gpt-5.2"),
            test_model_entry("anthropic", "claude-sonnet-4"),
        ];

        let selected = select_acp_model_entry(&config, &available).expect("selected model");

        assert_eq!(selected.model.provider, "anthropic");
        assert_eq!(selected.model.id, "claude-sonnet-4");
    }

    #[test]
    fn select_acp_model_entry_prefers_default_model_when_provider_is_unset() {
        let config = Config {
            default_model: Some("gpt-5.2".to_string()),
            ..Config::default()
        };
        let available = vec![
            test_model_entry("anthropic", "claude-sonnet-4"),
            test_model_entry("openai", "gpt-5.2"),
        ];

        let selected = select_acp_model_entry(&config, &available).expect("selected model");

        assert_eq!(selected.model.provider, "openai");
        assert_eq!(selected.model.id, "gpt-5.2");
    }

    #[test]
    fn select_acp_model_entry_matches_provider_aliases() {
        let config = Config {
            default_provider: Some("gemini-cli".to_string()),
            default_model: Some("gemini-2.5-pro".to_string()),
            ..Config::default()
        };
        let available = vec![
            test_model_entry("openai", "gpt-5.2"),
            test_model_entry("google-gemini-cli", "gemini-2.5-pro"),
        ];

        let selected = select_acp_model_entry(&config, &available).expect("selected model");

        assert_eq!(selected.model.provider, "google-gemini-cli");
        assert_eq!(selected.model.id, "gemini-2.5-pro");
    }

    #[test]
    fn select_acp_model_entry_falls_back_to_first_available_model() {
        let available = vec![
            test_model_entry("openai", "gpt-5.2"),
            test_model_entry("anthropic", "claude-sonnet-4"),
        ];

        let selected =
            select_acp_model_entry(&Config::default(), &available).expect("selected model");

        assert_eq!(selected.model.provider, "openai");
        assert_eq!(selected.model.id, "gpt-5.2");
    }

    #[test]
    fn resolve_acp_thinking_level_defaults_to_highest_supported_level() {
        let config = Config::default();
        let model_entry = test_model_entry("openai", "gpt-5.2");

        let thinking = resolve_acp_thinking_level(&config, &model_entry);

        assert_eq!(thinking, crate::model::ThinkingLevel::XHigh);
    }

    #[test]
    fn resolve_acp_thinking_level_clamps_non_reasoning_models_to_off() {
        let config = Config::default();
        let mut model_entry = test_model_entry("ollama", "llama3.2");
        model_entry.model.reasoning = false;

        let thinking = resolve_acp_thinking_level(&config, &model_entry);

        assert_eq!(thinking, crate::model::ThinkingLevel::Off);
    }

    #[test]
    fn extract_prompt_text_concatenates_text_blocks() {
        let blocks = vec![
            json!({ "type": "text", "text": "first line" }),
            json!({ "type": "text", "text": "second line" }),
        ];
        let result = extract_prompt_text(&blocks).expect("extracts text");
        assert_eq!(result, "first line\nsecond line");
    }

    #[test]
    fn extract_prompt_text_appends_resource_link_uri() {
        let blocks = vec![
            json!({ "type": "text", "text": "see also" }),
            json!({
                "type": "resource_link",
                "uri": "file:///tmp/notes.md",
                "name": "notes.md",
            }),
        ];
        let result = extract_prompt_text(&blocks).expect("extracts text");
        assert_eq!(result, "see also\nfile:///tmp/notes.md");
    }

    #[test]
    fn extract_prompt_text_rejects_unsupported_block_type() {
        // The text-only helper must not silently flatten images. Rich prompt
        // decoding and native image preservation are covered by content tests.
        let blocks = vec![json!({ "type": "image", "data": "base64..." })];
        let err = extract_prompt_text(&blocks).expect_err("should reject");
        assert!(err.contains("not supported"), "got: {err}");
    }

    #[test]
    fn extract_prompt_text_rejects_text_block_without_text_field() {
        let blocks = vec![json!({ "type": "text" })];
        let err = extract_prompt_text(&blocks).expect_err("should reject");
        assert!(
            err.contains("missing required field \"text\""),
            "got: {err}"
        );
    }

    #[test]
    fn extract_prompt_text_rejects_block_without_type_discriminator() {
        let blocks = vec![json!({ "text": "no type" })];
        let err = extract_prompt_text(&blocks).expect_err("should reject");
        assert!(err.contains("missing required discriminator"), "got: {err}");
    }

    #[test]
    fn map_stop_reason_covers_acp_values() {
        use crate::model::StopReason;
        assert_eq!(map_stop_reason(StopReason::Stop), "end_turn");
        assert_eq!(map_stop_reason(StopReason::ToolUse), "end_turn");
        assert_eq!(map_stop_reason(StopReason::Length), "max_tokens");
        assert_eq!(map_stop_reason(StopReason::Aborted), "cancelled");
        // Errors collapse to end_turn — the failure has already been streamed
        // via session/update; the response only carries a stopReason string.
        assert_eq!(map_stop_reason(StopReason::Error), "end_turn");
    }

    #[test]
    fn classify_tool_kind_maps_common_names() {
        assert_eq!(classify_tool_kind("read"), "read");
        assert_eq!(classify_tool_kind("read_text_file"), "read");
        assert_eq!(classify_tool_kind("EDIT"), "edit");
        assert_eq!(classify_tool_kind("apply_patch"), "edit");
        assert_eq!(classify_tool_kind("rg"), "search");
        assert_eq!(classify_tool_kind("bash"), "execute");
        assert_eq!(classify_tool_kind("curl"), "fetch");
        assert_eq!(classify_tool_kind("rm"), "delete");
        assert_eq!(classify_tool_kind("mv"), "move");
        assert_eq!(classify_tool_kind("think"), "think");
        // Unrecognised tool names fall through to the documented default.
        assert_eq!(classify_tool_kind("playwright_screenshot"), "other");
    }

    #[test]
    fn permission_response_approves_allow_once() {
        let decision = permission_response_to_decision(&json!({
            "outcome": {
                "outcome": "selected",
                "optionId": ACP_PERMISSION_ALLOW_ONCE,
            },
        }));

        assert_eq!(decision, ToolApprovalDecision::Allow);
    }

    #[test]
    fn permission_response_denies_reject_once_and_cancelled() {
        let rejected = permission_response_to_decision(&json!({
            "outcome": {
                "outcome": "selected",
                "optionId": ACP_PERMISSION_REJECT_ONCE,
            },
        }));
        let cancelled = permission_response_to_decision(&json!({
            "outcome": {
                "outcome": "cancelled",
            },
        }));

        assert!(matches!(
            rejected,
            ToolApprovalDecision::Deny { ref reason }
                if reason.contains("rejected")
        ));
        assert!(matches!(
            cancelled,
            ToolApprovalDecision::Deny { ref reason }
                if reason.contains("cancelled")
        ));
    }

    #[test]
    fn permission_response_malformed_is_denied() {
        let cases = [
            json!({}),
            json!({ "outcome": { "outcome": "selected" } }),
            json!({ "outcome": { "outcome": "selected", "optionId": "unknown" } }),
            json!({ "outcome": { "outcome": "weird" } }),
            json!({ "error": { "code": -32601, "message": "Method not found" } }),
        ];

        for case in cases {
            assert!(
                matches!(
                    permission_response_to_decision(&case),
                    ToolApprovalDecision::Deny { .. }
                ),
                "case should deny: {case}"
            );
        }
    }

    #[test]
    fn permission_request_emits_json_rpc_and_accepts_routed_response() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(8);
            let pending = Arc::new(StdMutex::new(HashMap::new()));
            let cx = AgentCx::for_testing();
            let client = AcpPermissionClient {
                out_tx,
                pending: Arc::clone(&pending),
                request_counter: Arc::new(AtomicU64::new(0)),
                timeout: Duration::from_secs(1),
                cx: cx.clone(),
            };
            let request = ToolApprovalRequest {
                tool_call_id: "call-1".to_string(),
                tool_name: "bash".to_string(),
                arguments: json!({ "command": "echo ok" }),
            };

            let responder = async {
                let outbound = out_rx.recv().expect("permission request");
                let parsed: Value = serde_json::from_str(&outbound).expect("valid request json");
                assert_eq!(parsed["jsonrpc"], "2.0");
                assert_eq!(parsed["method"], "session/request_permission");
                assert_eq!(parsed["params"]["sessionId"], "sess-1");
                assert_eq!(
                    parsed["params"]["toolCall"]["sessionUpdate"],
                    "tool_call_update"
                );
                assert_eq!(parsed["params"]["toolCall"]["toolCallId"], "call-1");
                assert_eq!(parsed["params"]["toolCall"]["kind"], "execute");
                assert_eq!(parsed["params"]["toolCall"]["status"], "pending");
                assert_eq!(
                    parsed["params"]["options"][0]["optionId"],
                    ACP_PERMISSION_ALLOW_ONCE
                );

                assert!(route_permission_response(
                    &json!({
                        "jsonrpc": "2.0",
                        "id": parsed["id"].clone(),
                        "result": {
                            "outcome": {
                                "outcome": "selected",
                                "optionId": ACP_PERMISSION_ALLOW_ONCE,
                            },
                        },
                    }),
                    &pending,
                    &cx,
                ));
            };

            let (decision, ()) =
                futures::join!(client.request_permission("sess-1", request), responder);
            assert_eq!(decision, ToolApprovalDecision::Allow);
            assert!(pending_permissions_empty(&pending));
        });
    }

    #[test]
    fn permission_request_times_out_fail_closed() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let (out_tx, _out_rx) = std::sync::mpsc::sync_channel::<String>(8);
            let pending = Arc::new(StdMutex::new(HashMap::new()));
            let client = AcpPermissionClient {
                out_tx,
                pending: Arc::clone(&pending),
                request_counter: Arc::new(AtomicU64::new(0)),
                timeout: Duration::from_millis(1),
                cx: AgentCx::for_testing(),
            };

            let decision = client
                .request_permission(
                    "sess-1",
                    ToolApprovalRequest {
                        tool_call_id: "call-1".to_string(),
                        tool_name: "edit".to_string(),
                        arguments: json!({}),
                    },
                )
                .await;

            assert!(matches!(
                decision,
                ToolApprovalDecision::Deny { ref reason }
                    if reason.contains("timed out")
            ));
            assert!(pending_permissions_empty(&pending));
        });
    }

    #[test]
    fn permission_request_client_disconnect_fail_closed() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<String>(8);
            drop(out_rx);
            let pending = Arc::new(StdMutex::new(HashMap::new()));
            let client = AcpPermissionClient {
                out_tx,
                pending: Arc::clone(&pending),
                request_counter: Arc::new(AtomicU64::new(0)),
                timeout: Duration::from_secs(1),
                cx: AgentCx::for_testing(),
            };

            let decision = client
                .request_permission(
                    "sess-1",
                    ToolApprovalRequest {
                        tool_call_id: "call-1".to_string(),
                        tool_name: "write".to_string(),
                        arguments: json!({}),
                    },
                )
                .await;

            assert!(matches!(
                decision,
                ToolApprovalDecision::Deny { ref reason }
                    if reason.contains("disconnected")
            ));
            assert!(pending_permissions_empty(&pending));
        });
    }

    // ── session/set_model + session/set_config_option (#105) ──────────────

    /// Build an `AcpSessionState` backed by a real registry + auth so the
    /// runtime-reconfig handlers can be exercised end to end. Mirrors the SDK's
    /// `set_model` test wiring: credentials present for `anthropic`/`openai`,
    /// active model `anthropic/claude-sonnet-4-5`.
    fn make_set_model_session_state() -> (Arc<Mutex<AcpSessionState>>, AuthStorage, ModelRegistry) {
        use crate::agent::{Agent, AgentConfig};
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load auth");
        auth.set(
            "anthropic",
            crate::auth::AuthCredential::ApiKey {
                key: "anthropic-key".to_string(),
            },
        );
        auth.set(
            "openai",
            crate::auth::AuthCredential::ApiKey {
                key: "openai-key".to_string(),
            },
        );

        let registry = ModelRegistry::load(&auth, None);
        let entry = registry
            .find("anthropic", "claude-sonnet-4-5")
            .expect("anthropic model in registry");
        let provider = providers::create_provider(&entry, None).expect("create anthropic provider");
        let tools = ToolRegistry::new(&[], std::path::Path::new("."), None);
        let agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                system_prompt: None,
                max_tool_iterations: 50,
                stream_options: StreamOptions::default(),
                block_images: false,
                model_accepts_images: true,
                fail_closed_hooks: false,
                tool_approval: None,
                keyword_settings: None,
                max_time: None,
                turn_recovery: crate::turn_recovery::TurnRecoveryMode::default(),
                approval_state: None,
                bash_settings: None,
                secrets: None,
            },
        );

        let mut session = Session::in_memory();
        session.header.provider = Some("anthropic".to_string());
        session.header.model_id = Some("claude-sonnet-4-5".to_string());

        let agent_session = AgentSession::new(
            agent,
            Arc::new(Mutex::new(session)),
            false,
            ResolvedCompactionSettings::default(),
        )
        .with_model_registry(registry.clone())
        .with_auth_storage(auth.clone());

        let state = Arc::new(Mutex::new(AcpSessionState {
            agent_session: Some(AgentSessionHandle::from_session_with_listeners(
                agent_session,
                EventListeners::default(),
            )),
            available_models: registry.get_available(),
            cwd: PathBuf::from("."),
            mcp: None,
        }));
        (state, auth, registry)
    }

    #[test]
    fn resolve_set_model_target_accepts_explicit_provider_model() {
        let auth = AuthStorage::load(std::env::temp_dir().join("pi-acp-resolve-auth.json"))
            .expect("load auth");
        let registry = ModelRegistry::load(&auth, None);
        let params = json!({ "provider": "openai", "model": "gpt-5.5" });
        let (provider, model) =
            resolve_set_model_target(&params, &registry).expect("resolves explicit pair");
        assert_eq!(provider, "openai");
        assert_eq!(model, "gpt-5.5");
    }

    #[test]
    fn resolve_set_model_target_resolves_provider_from_bare_value() {
        // The issue #105 client sends `config=model value=gpt-5.5` (no provider).
        let auth = AuthStorage::load(std::env::temp_dir().join("pi-acp-resolve-auth2.json"))
            .expect("load auth");
        let registry = ModelRegistry::load(&auth, None);
        let params = json!({ "value": "gpt-5.5" });
        let (provider, model) =
            resolve_set_model_target(&params, &registry).expect("resolves bare value");
        assert_eq!(model, "gpt-5.5");
        assert_eq!(provider, "openai", "provider resolved from registry");
    }

    #[test]
    fn resolve_set_model_target_rejects_missing_model() {
        let auth = AuthStorage::load(std::env::temp_dir().join("pi-acp-resolve-auth3.json"))
            .expect("load auth");
        let registry = ModelRegistry::load(&auth, None);
        let err = resolve_set_model_target(&json!({}), &registry).expect_err("missing model");
        assert!(err.contains("Missing required parameter"), "got: {err}");
    }

    #[test]
    fn resolve_set_model_target_rejects_unknown_model() {
        let auth = AuthStorage::load(std::env::temp_dir().join("pi-acp-resolve-auth4.json"))
            .expect("load auth");
        let registry = ModelRegistry::load(&auth, None);
        let err = resolve_set_model_target(&json!({ "model": "totally-made-up-model" }), &registry)
            .expect_err("unknown model");
        assert!(err.contains("Unknown model"), "got: {err}");
    }

    #[test]
    fn parse_config_option_accepts_thinking_aliases() {
        for name in [
            "thought_level",
            "thinking_level",
            "thinking",
            "reasoning",
            "effort",
            "reasoning_effort",
        ] {
            let option =
                parse_config_option(name, &json!("off")).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(matches!(
                option,
                RuntimeConfigOption::ThinkingLevel(crate::model::ThinkingLevel::Off)
            ));
        }
    }

    #[test]
    fn parse_config_option_accepts_numeric_value() {
        let option = parse_config_option("effort", &json!(3)).expect("numeric level");
        assert!(matches!(
            option,
            RuntimeConfigOption::ThinkingLevel(crate::model::ThinkingLevel::High)
        ));
    }

    #[test]
    fn parse_config_option_rejects_unknown_option() {
        let err = parse_config_option("temperature", &json!(0.7)).expect_err("unknown option");
        assert!(
            err.contains("Unknown or non-runtime config option"),
            "got: {err}"
        );
        assert!(err.contains("require a new session"), "got: {err}");
    }

    #[test]
    fn parse_config_option_rejects_bad_thinking_value() {
        let err = parse_config_option("thought_level", &json!("ludicrous")).expect_err("bad value");
        assert!(err.contains("Invalid value"), "got: {err}");
    }

    #[test]
    fn apply_set_model_switches_provider_and_model() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let cx = AgentCx::for_testing();
            let (state, _auth, _registry) = make_set_model_session_state();

            let (provider, model) = apply_set_model(&state, "openai", "gpt-4o", &cx)
                .await
                .expect("switch succeeds");
            assert_eq!(provider, "openai");
            assert_eq!(model, "gpt-4o");

            // The live agent now reports the new provider/model.
            let guard = state.lock(&cx).await.expect("lock state");
            let agent_session = guard.agent_session.as_ref().expect("session present");
            let active = agent_session.session().agent.provider();
            assert_eq!(active.name(), "openai");
            assert_eq!(active.model_id(), "gpt-4o");
        });
    }

    #[test]
    fn apply_set_config_option_applies_thinking_level() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let cx = AgentCx::for_testing();
            let (state, _auth, _registry) = make_set_model_session_state();

            apply_set_config_option(
                &state,
                RuntimeConfigOption::ThinkingLevel(crate::model::ThinkingLevel::Off),
                &cx,
            )
            .await
            .expect("apply thinking level");

            let guard = state.lock(&cx).await.expect("lock state");
            let agent_session = guard.agent_session.as_ref().expect("session present");
            assert_eq!(
                agent_session.session().agent.stream_options().thinking_level,
                Some(crate::model::ThinkingLevel::Off)
            );
        });
    }

    #[test]
    fn apply_set_model_rejects_unknown_model() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let cx = AgentCx::for_testing();
            let (state, _auth, _registry) = make_set_model_session_state();

            // Provider exists but the model id is not in the registry → the
            // underlying set_provider_model rejects the switch.
            let err = apply_set_model(&state, "openai", "no-such-model", &cx)
                .await
                .expect_err("unknown model rejected");
            assert!(
                err.contains("switch") || err.contains("Unable") || err.contains("Unknown"),
                "got: {err}"
            );

            // Active model is unchanged after a failed switch.
            let guard = state.lock(&cx).await.expect("lock state");
            let agent_session = guard.agent_session.as_ref().expect("session present");
            assert_eq!(agent_session.session().agent.provider().name(), "anthropic");
        });
    }
}
