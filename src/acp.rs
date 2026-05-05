//! ACP (Agent Client Protocol) support for Zed editor integration.
//!
//! ACP is a JSON-RPC 2.0 protocol over stdio that Zed editor uses to
//! communicate with external agents. This module implements the server
//! side of the protocol, translating ACP method calls into pi's core
//! agent/session/tool infrastructure.
//!
//! ## Protocol methods
//!
//! - `initialize` — exchange capabilities and protocol version
//! - `session/new` — create a new agent session
//! - `prompt` — send a prompt and stream back results
//! - `cancel` — cancel an in-progress prompt
//! - `session/list` — list existing sessions
//!
//! ## Streaming
//!
//! Prompt results are streamed as `prompt/progress` JSON-RPC notifications.
//! Each notification carries incremental content (text deltas, tool calls,
//! tool results) so the client can render in real time.

#![allow(clippy::too_many_lines)]
#![allow(clippy::significant_drop_tightening)]

use crate::agent::{AbortHandle, AbortSignal, AgentEvent, AgentSession};
use crate::agent_cx::AgentCx;
use crate::auth::AuthStorage;
use crate::compaction::ResolvedCompactionSettings;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::{AssistantMessage, AssistantMessageEvent, ContentBlock};
use crate::models::ModelEntry;
use crate::provider::StreamOptions;
use crate::provider_metadata::provider_ids_match;
use crate::providers;
use crate::session::Session;
use crate::tools::ToolRegistry;
use asupersync::runtime::RuntimeHandle;
use asupersync::sync::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
const PROMPT_NOT_FOUND: i64 = -32003;

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

/// Content item in ACP prompt progress.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum AcpContentItem {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

// Permission callback support is not yet implemented; tool_approval
// capability is advertised as false until the ACP spec stabilises the
// request_permission flow.

// ============================================================================
// ACP Session state
// ============================================================================

struct AcpSessionState {
    /// The agent session. Wrapped in Option so it can be temporarily taken
    /// out during prompt execution without holding the session lock.
    agent_session: Option<AgentSession>,
    cwd: PathBuf,
    session_id: String,
}

// ============================================================================
// ACP Server
// ============================================================================

/// Options for starting the ACP server.
#[derive(Clone)]
pub struct AcpOptions {
    pub config: Config,
    pub available_models: Vec<ModelEntry>,
    pub auth: AuthStorage,
    pub runtime_handle: RuntimeHandle,
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
    let active_prompts: Arc<Mutex<HashMap<String, AbortHandle>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let initialized = Arc::new(AtomicBool::new(false));

    while let Ok(line) = in_rx.recv(&cx).await {
        // Parse the JSON-RPC request.
        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(err) => {
                let _ = out_tx.send(json_rpc_error(
                    Value::Null,
                    PARSE_ERROR,
                    format!("Parse error: {err}"),
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

                match handle_session_new(&request.params, &options, &cx).await {
                    Ok((session_id, state)) => {
                        let models: Vec<AcpModel> = options
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

                        let state_arc = Arc::new(Mutex::new(state));
                        if let Ok(mut guard) = sessions.lock(&cx).await {
                            guard.insert(session_id.clone(), state_arc);
                        }

                        let _ = out_tx.send(json_rpc_ok(
                            id,
                            json!({
                                "sessionId": session_id,
                                "models": models,
                                "modes": modes,
                            }),
                        ));
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

            "prompt" => {
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
                let message_text = request
                    .params
                    .get("message")
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

                let Some(message_text) = message_text else {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        INVALID_PARAMS,
                        "Missing required parameter: message",
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

                // Check if this session already has an active prompt.
                {
                    let has_active = active_prompts.lock(&cx).await.is_ok_and(|guard| {
                        guard
                            .keys()
                            .any(|k| k.starts_with(&format!("{session_id}:")))
                    });
                    if has_active {
                        let _ = out_tx.send(json_rpc_error(
                            id,
                            PROMPT_IN_PROGRESS,
                            format!("Session {session_id} already has an active prompt"),
                        ));
                        continue;
                    }
                }

                // Generate a prompt ID for tracking.
                let prompt_seq = prompt_counter.fetch_add(1, Ordering::SeqCst);
                let prompt_id = format!("{session_id}:prompt-{prompt_seq}");

                // Create an abort handle for this prompt.
                let (abort_handle, abort_signal) = AbortHandle::new();
                if let Ok(mut guard) = active_prompts.lock(&cx).await {
                    guard.insert(prompt_id.clone(), abort_handle);
                }

                // Acknowledge the prompt immediately.
                let _ = out_tx.send(json_rpc_ok(
                    id,
                    json!({
                        "promptId": prompt_id,
                    }),
                ));

                // Spawn the prompt execution.
                let out_tx_prompt = out_tx.clone();
                let active_prompts_cleanup = Arc::clone(&active_prompts);
                let prompt_id_cleanup = prompt_id.clone();
                let prompt_cx = cx.clone();
                let prompt_session_id = session_id.clone();

                options.runtime_handle.spawn(async move {
                    run_prompt(
                        session_state,
                        message_text,
                        abort_signal,
                        out_tx_prompt,
                        prompt_id.clone(),
                        prompt_session_id,
                        prompt_cx.clone(),
                    )
                    .await;

                    // Clean up the active prompt.
                    if let Ok(mut guard) = active_prompts_cleanup.lock(&prompt_cx).await {
                        guard.remove(&prompt_id_cleanup);
                    }
                });
            }

            "cancel" => {
                let prompt_id = request
                    .params
                    .get("promptId")
                    .and_then(Value::as_str)
                    .map(String::from);

                let Some(prompt_id) = prompt_id else {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        INVALID_PARAMS,
                        "Missing required parameter: promptId",
                    ));
                    continue;
                };

                let aborted = active_prompts.lock(&cx).await.is_ok_and(|guard| {
                    guard.get(&prompt_id).is_some_and(|handle| {
                        handle.abort();
                        true
                    })
                });

                if aborted {
                    let _ = out_tx.send(json_rpc_ok(id, json!({ "cancelled": true })));
                } else {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        PROMPT_NOT_FOUND,
                        format!("No active prompt with id: {prompt_id}"),
                    ));
                }
            }

            "session/list" => {
                let session_list: Vec<Value> = sessions.lock(&cx).await.map_or_else(
                    |_| Vec::new(),
                    |guard| {
                        guard
                            .keys()
                            .map(|sid| json!({ "sessionId": sid }))
                            .collect()
                    },
                );

                let _ = out_tx.send(json_rpc_ok(id, json!({ "sessions": session_list })));
            }

            "session/load" => {
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

                let exists = sessions
                    .lock(&cx)
                    .await
                    .is_ok_and(|guard| guard.contains_key(&session_id));

                if exists {
                    let models: Vec<AcpModel> = options
                        .available_models
                        .iter()
                        .map(|entry| AcpModel {
                            id: entry.model.id.clone(),
                            name: entry.model.name.clone(),
                            provider: Some(entry.model.provider.clone()),
                        })
                        .collect();

                    let _ = out_tx.send(json_rpc_ok(
                        id,
                        json!({
                            "sessionId": session_id,
                            "models": models,
                        }),
                    ));
                } else {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        SESSION_NOT_FOUND,
                        format!("Session not found: {session_id}"),
                    ));
                }
            }

            "session/resume" => {
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

                let exists = sessions
                    .lock(&cx)
                    .await
                    .is_ok_and(|guard| guard.contains_key(&session_id));

                if exists {
                    let _ = out_tx.send(json_rpc_ok(
                        id,
                        json!({
                            "sessionId": session_id,
                            "resumed": true,
                        }),
                    ));
                } else {
                    let _ = out_tx.send(json_rpc_error(
                        id,
                        SESSION_NOT_FOUND,
                        format!("Session not found: {session_id}"),
                    ));
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

    let guard = sessions
        .lock(cx)
        .await
        .map_err(|e| format!("Lock failed: {e}"))?;

    if guard.is_empty() {
        return Err("No active sessions — cannot validate file path".to_string());
    }

    let allowed_cwds: Vec<PathBuf> = if let Some(sid) = session_id {
        match guard.get(sid) {
            Some(state) => {
                if let Ok(s) = state.lock(cx).await {
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
            if let Ok(s) = state.lock(cx).await {
                cwds.push(s.cwd.clone());
            }
        }
        cwds
    };

    // Canonicalize each cwd and check if the resolved path starts with it.
    for cwd in &allowed_cwds {
        if let Ok(canonical_cwd) = cwd.canonicalize() {
            if resolved.starts_with(&canonical_cwd) {
                return Ok(());
            }
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
        "protocolVersion": "2025-01-01",
        "serverInfo": {
            "name": "pi-agent",
            "version": version,
        },
        "capabilities": {
            "streaming": true,
            "toolApproval": false,
        },
    })
}

fn select_acp_model_entry(config: &Config, available_models: &[ModelEntry]) -> Option<ModelEntry> {
    if let (Some(default_provider), Some(default_model)) = (
        config.default_provider.as_deref(),
        config.default_model.as_deref(),
    ) {
        if let Some(entry) = available_models.iter().find(|entry| {
            provider_ids_match(&entry.model.provider, default_provider)
                && entry.model.id.eq_ignore_ascii_case(default_model)
        }) {
            return Some(entry.clone());
        }
    }

    if let Some(default_provider) = config.default_provider.as_deref() {
        if let Some(entry) = available_models
            .iter()
            .find(|entry| provider_ids_match(&entry.model.provider, default_provider))
        {
            return Some(entry.clone());
        }
    }

    if let Some(default_model) = config.default_model.as_deref() {
        if let Some(entry) = available_models
            .iter()
            .find(|entry| entry.model.id.eq_ignore_ascii_case(default_model))
        {
            return Some(entry.clone());
        }
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

/// Build a system prompt for ACP mode without requiring a `Cli` struct.
fn build_acp_system_prompt(cwd: &std::path::Path, enabled_tools: &[&str]) -> String {
    use std::fmt::Write as _;

    let tool_descriptions = [
        ("read", "Read file contents"),
        ("bash", "Execute bash commands"),
        ("edit", "Make surgical edits to files"),
        ("write", "Write file contents"),
        ("grep", "Search file contents with regex"),
        ("find", "Find files by name pattern"),
        ("ls", "List directory contents"),
    ];

    let mut prompt = String::from(
        "You are a helpful AI coding assistant integrated into the user's editor via ACP (Agent Client Protocol). \
         You have access to the following tools:\n\n",
    );

    for (name, description) in &tool_descriptions {
        if enabled_tools.contains(name) {
            let _ = writeln!(prompt, "- **{name}**: {description}");
        }
    }

    prompt.push_str(
        "\nUse these tools to help the user with coding tasks. \
         Be concise and precise. When making file changes, explain what you're doing.\n",
    );

    // Load project context files (pi.md, AGENTS.md) if they exist.
    for filename in &["pi.md", "AGENTS.md", ".pi"] {
        let path = cwd.join(filename);
        if path.is_file() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let _ = write!(prompt, "\n## {filename}\n\n{content}\n\n");
            }
        }
    }

    let date_time = chrono::Utc::now()
        .format("%Y-%m-%d %H:%M:%S UTC")
        .to_string();
    let _ = write!(prompt, "\nCurrent date and time: {date_time}");
    let _ = write!(prompt, "\nCurrent working directory: {}", cwd.display());

    prompt
}

async fn handle_session_new(
    params: &Value,
    options: &AcpOptions,
    _cx: &AgentCx,
) -> Result<(String, AcpSessionState)> {
    let cwd = params.get("cwd").and_then(Value::as_str).map_or_else(
        || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        PathBuf::from,
    );

    // Create a new in-memory session.
    let mut session = Session::in_memory();
    session.header.cwd = cwd.display().to_string();
    let session_id = session.header.id.clone();

    // Set up the enabled tools (all standard tools).
    let enabled_tools: Vec<&str> = vec!["read", "bash", "edit", "write", "grep", "find", "ls"];
    let tools = ToolRegistry::new(&enabled_tools, &cwd, Some(&options.config));

    // ACP should respect the same configured default provider/model preference
    // as the normal startup path instead of picking an arbitrary ready model.
    let model_entry = select_acp_model_entry(&options.config, &options.available_models)
        .ok_or_else(|| Error::provider("acp", "No models available"))?;

    let provider = providers::create_provider(&model_entry, None)
        .map_err(|e| Error::provider("acp", e.to_string()))?;

    // Build system prompt directly (avoids constructing a Cli struct).
    let system_prompt = build_acp_system_prompt(&cwd, &enabled_tools);

    // Resolve API key from auth storage and model entry.
    let api_key = options
        .auth
        .resolve_api_key(&model_entry.model.provider, None)
        .or_else(|| model_entry.api_key.clone())
        .and_then(|k| {
            let trimmed = k.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        });

    let stream_options = StreamOptions {
        api_key,
        thinking_level: Some(resolve_acp_thinking_level(&options.config, &model_entry)),
        headers: model_entry.headers.clone(),
        ..StreamOptions::default()
    };

    let agent_config = crate::agent::AgentConfig {
        system_prompt: Some(system_prompt),
        max_tool_iterations: crate::agent::max_tool_iterations_default(),
        stream_options,
        block_images: options.config.image_block_images(),
        fail_closed_hooks: options.config.fail_closed_hooks(),
    };

    let mut agent = crate::agent::Agent::new(provider, tools, agent_config);
    if let Some(client) =
        crate::c137_blink::start_session(&cwd, options.runtime_handle.clone())
    {
        agent.set_blink_client(client);
    }
    let session_arc = Arc::new(Mutex::new(session));
    let compaction_settings = ResolvedCompactionSettings {
        enabled: options.config.compaction_enabled(),
        reserve_tokens: options.config.compaction_reserve_tokens(),
        keep_recent_tokens: options.config.compaction_keep_recent_tokens(),
        context_window_tokens: if model_entry.model.context_window == 0 {
            ResolvedCompactionSettings::default().context_window_tokens
        } else {
            model_entry.model.context_window
        },
    };

    let agent_session = AgentSession::new(agent, session_arc, false, compaction_settings)
        .with_runtime_handle(options.runtime_handle.clone());

    Ok((
        session_id.clone(),
        AcpSessionState {
            agent_session: Some(agent_session),
            cwd,
            session_id,
        },
    ))
}

/// Execute a prompt and stream progress notifications.
async fn run_prompt(
    session_state: Arc<Mutex<AcpSessionState>>,
    message: String,
    abort_signal: AbortSignal,
    out_tx: std::sync::mpsc::SyncSender<String>,
    prompt_id: String,
    session_id: String,
    cx: AgentCx,
) {
    let out_tx_events = out_tx.clone();
    let prompt_id_events = prompt_id.clone();
    let session_id_events = session_id.clone();

    // Build the event handler that translates AgentEvents into ACP notifications.
    let event_handler = build_acp_event_handler(out_tx_events, prompt_id_events, session_id_events);

    // Take the agent_session out of the lock, run the prompt, then put it back.
    // This avoids holding the session mutex across the entire prompt execution,
    // which could block other operations (session/list, cancel, etc.) for minutes.
    // Safety: the concurrent-prompt guard in the dispatcher prevents a second
    // prompt on the same session, so no one will see the None state.
    let mut agent_session = {
        let mut guard = match session_state.lock(&cx).await {
            Ok(guard) => guard,
            Err(err) => {
                let _ = out_tx.send(json_rpc_notification(
                    "prompt/end",
                    json!({
                        "promptId": prompt_id,
                        "sessionId": session_id,
                        "error": format!("Session lock failed: {err}"),
                    }),
                ));
                return;
            }
        };
        let Some(agent) = guard.agent_session.take() else {
            let _ = out_tx.send(json_rpc_notification(
                "prompt/end",
                json!({
                    "promptId": prompt_id,
                    "sessionId": session_id,
                    "error": "Session is busy (agent_session unavailable)",
                }),
            ));
            return;
        };
        agent
    };

    let result = agent_session
        .run_text_with_abort(message, Some(abort_signal), event_handler)
        .await;

    // Put the agent_session back.
    if let Ok(mut guard) = session_state.lock(&cx).await {
        guard.agent_session = Some(agent_session);
    }

    // Send prompt/end notification.
    match result {
        Ok(ref msg) => {
            let content = assistant_message_to_acp_content(msg);
            let _ = out_tx.send(json_rpc_notification(
                "prompt/end",
                json!({
                    "promptId": prompt_id,
                    "sessionId": session_id,
                    "content": content,
                    "stopReason": serde_json::to_value(msg.stop_reason)
                        .unwrap_or_else(|_| json!("unknown")),
                }),
            ));
        }
        Err(ref err) => {
            let _ = out_tx.send(json_rpc_notification(
                "prompt/end",
                json!({
                    "promptId": prompt_id,
                    "sessionId": session_id,
                    "error": err.to_string(),
                }),
            ));
        }
    }
}

/// Build an event handler that translates `AgentEvent`s into ACP `prompt/progress` notifications.
fn build_acp_event_handler(
    out_tx: std::sync::mpsc::SyncSender<String>,
    prompt_id: String,
    session_id: String,
) -> impl Fn(AgentEvent) + Send + Sync + 'static {
    move |event: AgentEvent| {
        let notification = match &event {
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => match assistant_message_event {
                AssistantMessageEvent::TextDelta { delta, .. } => Some(json_rpc_notification(
                    "prompt/progress",
                    json!({
                        "promptId": prompt_id,
                        "sessionId": session_id,
                        "kind": "textDelta",
                        "content": [{
                            "type": "text",
                            "text": delta,
                        }],
                    }),
                )),
                AssistantMessageEvent::TextEnd { content, .. } => Some(json_rpc_notification(
                    "prompt/progress",
                    json!({
                        "promptId": prompt_id,
                        "sessionId": session_id,
                        "kind": "textEnd",
                        "content": [{
                            "type": "text",
                            "text": content,
                        }],
                    }),
                )),
                AssistantMessageEvent::ThinkingDelta { delta, .. } => Some(json_rpc_notification(
                    "prompt/progress",
                    json!({
                        "promptId": prompt_id,
                        "sessionId": session_id,
                        "kind": "thinkingDelta",
                        "content": [{
                            "type": "thinking",
                            "text": delta,
                        }],
                    }),
                )),
                AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                    Some(json_rpc_notification(
                        "prompt/progress",
                        json!({
                            "promptId": prompt_id,
                            "sessionId": session_id,
                            "kind": "toolUse",
                            "content": [{
                                "type": "tool_use",
                                "id": tool_call.id,
                                "name": tool_call.name,
                                "input": tool_call.arguments,
                            }],
                        }),
                    ))
                }
                _ => None,
            },

            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => Some(json_rpc_notification(
                "prompt/progress",
                json!({
                    "promptId": prompt_id,
                    "sessionId": session_id,
                    "kind": "toolExecutionStart",
                    "toolCallId": tool_call_id,
                    "toolName": tool_name,
                    "args": args,
                }),
            )),

            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => {
                let content_text = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                Some(json_rpc_notification(
                    "prompt/progress",
                    json!({
                        "promptId": prompt_id,
                        "sessionId": session_id,
                        "kind": "toolResult",
                        "toolName": tool_name,
                        "content": [{
                            "type": "tool_result",
                            "toolUseId": tool_call_id,
                            "content": content_text,
                            "isError": is_error,
                        }],
                    }),
                ))
            }

            AgentEvent::TurnStart { turn_index, .. } => Some(json_rpc_notification(
                "prompt/progress",
                json!({
                    "promptId": prompt_id,
                    "sessionId": session_id,
                    "kind": "turnStart",
                    "turnIndex": turn_index,
                }),
            )),

            AgentEvent::TurnEnd { turn_index, .. } => Some(json_rpc_notification(
                "prompt/progress",
                json!({
                    "promptId": prompt_id,
                    "sessionId": session_id,
                    "kind": "turnEnd",
                    "turnIndex": turn_index,
                }),
            )),

            AgentEvent::AgentStart { .. } => Some(json_rpc_notification(
                "prompt/progress",
                json!({
                    "promptId": prompt_id,
                    "sessionId": session_id,
                    "kind": "agentStart",
                }),
            )),

            AgentEvent::AgentEnd { error, .. } => Some(json_rpc_notification(
                "prompt/progress",
                json!({
                    "promptId": prompt_id,
                    "sessionId": session_id,
                    "kind": "agentEnd",
                    "error": error,
                }),
            )),

            // Other events are not surfaced as ACP notifications.
            _ => None,
        };

        if let Some(notif) = notification {
            let _ = out_tx.send(notif);
        }
    }
}

/// Convert an `AssistantMessage` to a list of ACP content items.
fn assistant_message_to_acp_content(msg: &AssistantMessage) -> Vec<AcpContentItem> {
    let mut items = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text(t) => {
                items.push(AcpContentItem::Text {
                    text: t.text.clone(),
                });
            }
            ContentBlock::Thinking(t) => {
                items.push(AcpContentItem::Thinking {
                    text: t.thinking.clone(),
                });
            }
            ContentBlock::ToolCall(tc) => {
                items.push(AcpContentItem::ToolUse {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    input: tc.arguments.clone(),
                });
            }
            ContentBlock::Image(_) => {
                // Images are not surfaced through ACP text protocol.
            }
        }
    }
    items
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{InputType, Model, ModelCost};
    use std::collections::HashMap;

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
            "prompt/progress",
            json!({"promptId": "p1", "kind": "textDelta"}),
        );
        let parsed: Value = serde_json::from_str(&notif).expect("valid json");
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "prompt/progress");
        assert_eq!(parsed["params"]["promptId"], "p1");
        assert!(parsed.get("id").is_none());
    }

    #[test]
    fn handle_initialize_returns_correct_shape() {
        let result = handle_initialize();

        assert_eq!(result["protocolVersion"], "2025-01-01");
        assert_eq!(result["serverInfo"]["name"], "pi-agent");
        assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        assert!(result["capabilities"]["streaming"].as_bool().unwrap());
        assert!(!result["capabilities"]["toolApproval"].as_bool().unwrap());
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
    fn assistant_message_to_acp_content_converts_blocks() {
        use crate::model::{TextContent, ToolCall};

        let msg = AssistantMessage {
            content: vec![
                ContentBlock::Text(TextContent::new("Hello")),
                ContentBlock::ToolCall(ToolCall {
                    id: "tc1".into(),
                    name: "read".into(),
                    arguments: json!({"path": "/tmp/test.txt"}),
                    thought_signature: None,
                }),
            ],
            ..Default::default()
        };

        let items = assistant_message_to_acp_content(&msg);
        assert_eq!(items.len(), 2);

        match &items[0] {
            AcpContentItem::Text { text } => assert_eq!(text, "Hello"),
            _ => panic!("Expected Text item"),
        }

        match &items[1] {
            AcpContentItem::ToolUse { id, name, input } => {
                assert_eq!(id, "tc1");
                assert_eq!(name, "read");
                assert_eq!(input["path"], "/tmp/test.txt");
            }
            _ => panic!("Expected ToolUse item"),
        }
    }
}
