//! Stable SDK-facing API surface for embedding Pi as a library.
//!
//! This module is the supported entry point for external library consumers.
//! Prefer importing from `pi::sdk` instead of deep internal modules.
//!
//! # Examples
//!
//! ```rust
//! use pi::sdk::{AgentEvent, Message, ToolDefinition};
//!
//! let _events: Vec<AgentEvent> = Vec::new();
//! let _messages: Vec<Message> = Vec::new();
//! let _tools: Vec<ToolDefinition> = Vec::new();
//! ```
//!
//! Internal implementation types are intentionally not part of this surface.
//!
//! ```compile_fail
//! use pi::sdk::RpcSharedState;
//! ```

use crate::app;
use crate::auth::AuthStorage;
use crate::cli::Cli;
use crate::compaction::ResolvedCompactionSettings;
use crate::models::default_models_path;
use crate::provider::ThinkingBudgets;
use crate::providers;
use clap::Parser;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub use crate::agent::{
    AbortHandle, AbortSignal, Agent, AgentConfig, AgentEvent, AgentSession, QueueMode,
};
pub use crate::config::Config;
pub use crate::error::{Error, Result};
pub use crate::extensions::{ExtensionManager, ExtensionPolicy, ExtensionRegion};
pub use crate::model::ThinkingLevel;
pub use crate::model::{
    AssistantMessage, ContentBlock, Cost, CustomMessage, ImageContent, Message, StopDetails,
    StopReason, StreamEvent, TextContent, ThinkingContent, ToolCall, ToolResultMessage, Usage,
    UserContent, UserMessage,
};
pub use crate::models::{ModelEntry, ModelRegistry};
pub use crate::provider::{
    Context as ProviderContext, InputType, Model, ModelCost, Provider, StreamOptions,
    ThinkingBudgets as ProviderThinkingBudgets, ToolDef,
};
pub use crate::session::Session;
pub use crate::tools::{Tool, ToolOutput, ToolRegistry, ToolUpdate};

/// Stable alias for model-exposed tool schema definitions.
pub type ToolDefinition = ToolDef;

// ============================================================================
// Tool Factory Functions
// ============================================================================

use crate::tools::{
    BashTool, EditTool, FindTool, GrepTool, HashlineEditTool, LsTool, ReadTool, WriteTool,
};

/// Built-in tool names included in the default non-delegating SDK registry.
///
/// The opt-in `subagent` tool is configured through the CLI/session delegation
/// surface and is intentionally not constructed by [`create_all_tools`].
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read",
    "bash",
    "edit",
    "write",
    "grep",
    "find",
    "ls",
    "hashline_edit",
];

/// Create a read tool configured for `cwd`.
pub fn create_read_tool(cwd: &Path) -> Box<dyn Tool> {
    Box::new(ReadTool::new(cwd))
}

/// Create a bash tool configured for `cwd`.
pub fn create_bash_tool(cwd: &Path) -> Box<dyn Tool> {
    Box::new(BashTool::new(cwd))
}

/// Create an edit tool configured for `cwd`.
pub fn create_edit_tool(cwd: &Path) -> Box<dyn Tool> {
    Box::new(EditTool::new(cwd))
}

/// Create a write tool configured for `cwd`.
pub fn create_write_tool(cwd: &Path) -> Box<dyn Tool> {
    Box::new(WriteTool::new(cwd))
}

/// Create a grep tool configured for `cwd`.
pub fn create_grep_tool(cwd: &Path) -> Box<dyn Tool> {
    Box::new(GrepTool::new(cwd))
}

/// Create a find tool configured for `cwd`.
pub fn create_find_tool(cwd: &Path) -> Box<dyn Tool> {
    Box::new(FindTool::new(cwd))
}

/// Create an ls tool configured for `cwd`.
pub fn create_ls_tool(cwd: &Path) -> Box<dyn Tool> {
    Box::new(LsTool::new(cwd))
}

/// Create a hashline edit tool configured for `cwd`.
pub fn create_hashline_edit_tool(cwd: &Path) -> Box<dyn Tool> {
    Box::new(HashlineEditTool::new(cwd))
}

/// Create the default non-delegating built-in tools configured for `cwd`.
pub fn create_all_tools(cwd: &Path) -> Vec<Box<dyn Tool>> {
    vec![
        create_read_tool(cwd),
        create_bash_tool(cwd),
        create_edit_tool(cwd),
        create_write_tool(cwd),
        create_grep_tool(cwd),
        create_find_tool(cwd),
        create_ls_tool(cwd),
        create_hashline_edit_tool(cwd),
    ]
}

/// Convert a [`Tool`] into its [`ToolDefinition`] schema.
pub fn tool_to_definition(tool: &dyn Tool) -> ToolDefinition {
    ToolDefinition {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        parameters: tool.parameters(),
    }
}

/// Return schemas for the default non-delegating built-in tools.
pub fn all_tool_definitions(cwd: &Path) -> Vec<ToolDefinition> {
    create_all_tools(cwd)
        .iter()
        .map(|t| tool_to_definition(t.as_ref()))
        .collect()
}

// ============================================================================
// Streaming Callbacks and Tool Hooks
// ============================================================================

/// Opaque identifier for an event subscription.
///
/// Returned by [`AgentSessionHandle::subscribe`] and used to remove the
/// listener via [`AgentSessionHandle::unsubscribe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

/// Callback invoked when a tool execution starts.
///
/// Arguments: `(tool_name, input_args)`.
pub type OnToolStart = Arc<dyn Fn(&str, &Value) + Send + Sync>;

/// Callback invoked when a tool execution ends.
///
/// Arguments: `(tool_name, output, is_error)`.
pub type OnToolEnd = Arc<dyn Fn(&str, &ToolOutput, bool) + Send + Sync>;

/// Callback invoked for every raw provider [`StreamEvent`].
///
/// This gives SDK consumers direct access to the low-level streaming protocol
/// before events are wrapped into [`AgentEvent::MessageUpdate`].
pub type OnStreamEvent = Arc<dyn Fn(&StreamEvent) + Send + Sync>;

pub type EventSubscriber = Arc<dyn Fn(AgentEvent) + Send + Sync>;
type EventSubscribers = HashMap<SubscriptionId, EventSubscriber>;

/// Collection of session-level event listeners.
///
/// These are registered once and invoked for every prompt throughout the
/// session lifetime, in contrast to per-prompt callbacks on
/// [`AgentSessionHandle::prompt`].
#[derive(Clone, Default)]
pub struct EventListeners {
    next_id: Arc<AtomicU64>,
    subscribers: Arc<std::sync::Mutex<EventSubscribers>>,
    pub on_tool_start: Option<OnToolStart>,
    pub on_tool_end: Option<OnToolEnd>,
    pub on_stream_event: Option<OnStreamEvent>,
}

impl EventListeners {
    fn new() -> Self {
        Self {
            next_id: Arc::new(AtomicU64::new(1)),
            subscribers: Arc::new(std::sync::Mutex::new(HashMap::new())),
            on_tool_start: None,
            on_tool_end: None,
            on_stream_event: None,
        }
    }

    /// Register a session-level event listener.
    pub fn subscribe(&self, listener: EventSubscriber) -> SubscriptionId {
        let id = SubscriptionId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let mut subs = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        subs.insert(id, listener);
        id
    }

    /// Remove a previously registered listener.
    pub fn unsubscribe(&self, id: SubscriptionId) -> bool {
        let mut subs = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        subs.remove(&id).is_some()
    }

    /// Dispatch an [`AgentEvent`] to all registered subscribers.
    pub fn notify(&self, event: &AgentEvent) {
        let listeners: Vec<_> = {
            let subs = self
                .subscribers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            subs.values().cloned().collect()
        };
        for listener in listeners {
            listener(event.clone());
        }
    }

    /// Dispatch tool-start to the typed hook (if set).
    pub fn notify_tool_start(&self, tool_name: &str, args: &Value) {
        if let Some(cb) = &self.on_tool_start {
            cb(tool_name, args);
        }
    }

    /// Dispatch tool-end to the typed hook (if set).
    pub fn notify_tool_end(&self, tool_name: &str, output: &ToolOutput, is_error: bool) {
        if let Some(cb) = &self.on_tool_end {
            cb(tool_name, output, is_error);
        }
    }

    /// Dispatch a raw stream event (if hook is set).
    pub fn notify_stream_event(&self, event: &StreamEvent) {
        if let Some(cb) = &self.on_stream_event {
            cb(event);
        }
    }
}

impl std::fmt::Debug for EventListeners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.subscribers.lock().map_or(0, |s| s.len());
        let next_id = self.next_id.load(Ordering::Relaxed);
        f.debug_struct("EventListeners")
            .field("subscriber_count", &count)
            .field("next_id", &next_id)
            .field("has_on_tool_start", &self.on_tool_start.is_some())
            .field("has_on_tool_end", &self.on_tool_end.is_some())
            .field("has_on_stream_event", &self.on_stream_event.is_some())
            .finish()
    }
}

/// SDK session construction options.
///
/// These options provide the programmatic equivalent of the core CLI startup
/// path used in `src/main.rs`.
#[derive(Clone)]
pub struct SessionOptions {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub thinking: Option<crate::model::ThinkingLevel>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub enabled_tools: Option<Vec<String>>,
    pub working_directory: Option<PathBuf>,
    pub no_session: bool,
    pub session_path: Option<PathBuf>,
    pub session_dir: Option<PathBuf>,
    pub extension_paths: Vec<PathBuf>,
    pub extension_policy: Option<String>,
    pub repair_policy: Option<String>,
    pub include_cwd_in_prompt: bool,
    pub max_tool_iterations: usize,

    /// Optional factory for the session's [`ToolRegistry`].
    ///
    /// When `None` (the default), [`create_agent_session`] builds the
    /// built-in tool set from [`SessionOptions::enabled_tools`] exactly
    /// as it always has — existing callers are unaffected.
    ///
    /// Setting this lets a downstream embedder layer custom tools onto
    /// the registry: approval-gated wrappers around the built-ins, a
    /// read-only "plan mode" subset, a `Task` tool that spawns nested
    /// sandboxed sessions, etc. Implement [`ToolFactory`] on your own
    /// `Send + Sync` type and call [`default_tool_registry`] from inside
    /// it to inherit the current built-in resolution rules.
    ///
    /// Wrapped in `Arc` because [`SessionOptions`] is cloned through
    /// the session-handle pipeline and the factory may need to outlive
    /// the options struct.
    pub tool_factory: Option<Arc<dyn ToolFactory>>,

    /// Session-level event listener invoked for every [`AgentEvent`].
    ///
    /// Unlike the per-prompt callback passed to [`AgentSessionHandle::prompt`],
    /// this fires for all prompts throughout the session lifetime.
    pub on_event: Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,

    /// Typed callback invoked when tool execution starts.
    pub on_tool_start: Option<OnToolStart>,

    /// Typed callback invoked when tool execution ends.
    pub on_tool_end: Option<OnToolEnd>,

    /// Callback for raw provider [`StreamEvent`]s.
    pub on_stream_event: Option<OnStreamEvent>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            provider: None,
            model: None,
            api_key: None,
            thinking: None,
            system_prompt: None,
            append_system_prompt: None,
            enabled_tools: None,
            working_directory: None,
            no_session: true,
            session_path: None,
            session_dir: None,
            extension_paths: Vec::new(),
            extension_policy: None,
            repair_policy: None,
            include_cwd_in_prompt: true,
            max_tool_iterations: crate::agent::resolved_max_tool_iterations_default(),
            tool_factory: None,
            on_event: None,
            on_tool_start: None,
            on_tool_end: None,
            on_stream_event: None,
        }
    }
}

/// Hook for assembling the session's [`ToolRegistry`].
///
/// Attach an `Arc<dyn ToolFactory>` to [`SessionOptions::tool_factory`]
/// to inject custom tools, wrap the built-ins, or restrict the active
/// set. The factory runs once during [`create_agent_session`], after
/// the tool-name allowlist has been resolved from
/// [`SessionOptions::enabled_tools`].
///
/// To layer on top of the existing built-ins instead of starting from
/// scratch, call [`default_tool_registry`] from inside your impl and
/// register additional tools (or wrap existing ones) on the returned
/// registry.
pub trait ToolFactory: Send + Sync {
    /// Build the registry for a new session.
    ///
    /// - `enabled` is the resolved tool-name allowlist after
    ///   [`SessionOptions::enabled_tools`] and CLI defaults are merged.
    /// - `cwd` is the working directory the session opened in.
    /// - `config` is pi's loaded [`Config`], passed through so custom
    ///   tools can read settings such as `block_images` without
    ///   duplicating the config-loading logic.
    fn create_tool_registry(&self, enabled: &[&str], cwd: &Path, config: &Config) -> ToolRegistry;
}

/// Build the same [`ToolRegistry`] [`create_agent_session`] uses by
/// default.
///
/// Useful inside a [`ToolFactory`] impl when you want to start from
/// the standard built-in tool set and then layer custom tools on top
/// (e.g. wrap each tool with an approval gate, or add a `Task` tool
/// that spawns a nested session).
pub fn default_tool_registry(enabled: &[&str], cwd: &Path, config: &Config) -> ToolRegistry {
    ToolRegistry::new(enabled, cwd, Some(config))
}

/// Lightweight handle for programmatic embedding.
///
/// This wraps `AgentSession` and exposes high-level request methods while still
/// allowing access to the underlying session when needed.
///
/// Session-level event listeners can be registered via [`Self::subscribe`] or
/// by providing callbacks on [`SessionOptions`].  These fire for **every**
/// prompt, in addition to the per-prompt `on_event` callback.
pub struct AgentSessionHandle {
    session: AgentSession,
    listeners: EventListeners,
}

/// Snapshot of the current agent session state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionState {
    pub session_id: Option<String>,
    pub provider: String,
    pub model_id: String,
    pub thinking_level: Option<crate::model::ThinkingLevel>,
    pub save_enabled: bool,
    pub message_count: usize,
}

/// Prompt completion payload returned by `SessionTransport`.
#[derive(Debug, Clone)]
pub enum SessionPromptResult {
    InProcess(Box<AssistantMessage>),
    RpcEvents(Vec<Value>),
}

/// Event wrapper used by the unified `SessionTransport` callback.
#[derive(Debug, Clone)]
pub enum SessionTransportEvent {
    InProcess(AgentEvent),
    Rpc(Value),
}

/// Unified session state snapshot across in-process and RPC transports.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionTransportState {
    InProcess(AgentSessionState),
    Rpc(Box<RpcSessionState>),
}

/// Model metadata exposed by RPC APIs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RpcModelInfo {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub input: Vec<InputType>,
    #[serde(default)]
    pub context_window: u32,
    #[serde(default)]
    pub max_tokens: u32,
    #[serde(default)]
    pub cost: Option<ModelCost>,
}

/// Session state payload returned by RPC `get_state`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct RpcSessionState {
    #[serde(default)]
    pub model: Option<RpcModelInfo>,
    #[serde(default)]
    pub thinking_level: String,
    #[serde(default)]
    pub is_streaming: bool,
    #[serde(default)]
    pub is_compacting: bool,
    #[serde(default)]
    pub steering_mode: String,
    #[serde(default)]
    pub follow_up_mode: String,
    #[serde(default)]
    pub session_file: Option<String>,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub session_name: Option<String>,
    #[serde(default)]
    pub auto_compaction_enabled: bool,
    #[serde(default)]
    pub auto_retry_enabled: bool,
    #[serde(default)]
    pub message_count: usize,
    #[serde(default)]
    pub pending_message_count: usize,
    #[serde(default)]
    pub durability_mode: String,
}

/// Session-level token aggregates returned by RPC `get_session_stats`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RpcTokenStats {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
}

/// Session stats payload returned by RPC `get_session_stats`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionStats {
    #[serde(default)]
    pub session_file: Option<String>,
    pub session_id: String,
    pub user_messages: u64,
    pub assistant_messages: u64,
    pub tool_calls: u64,
    pub tool_results: u64,
    pub total_messages: u64,
    pub tokens: RpcTokenStats,
    pub cost: f64,
}

/// Result payload for `new_session` and `switch_session`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcCancelledResult {
    pub cancelled: bool,
}

/// Result payload returned by RPC `cycle_model`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RpcCycleModelResult {
    pub model: RpcModelInfo,
    pub thinking_level: crate::model::ThinkingLevel,
    pub is_scoped: bool,
}

/// Result payload returned by RPC `cycle_thinking_level`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcThinkingLevelResult {
    pub level: crate::model::ThinkingLevel,
}

/// Bash execution result returned by RPC `bash`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RpcBashResult {
    pub output: String,
    pub exit_code: i32,
    pub cancelled: bool,
    pub truncated: bool,
    pub full_output_path: Option<String>,
}

/// Compaction result returned by RPC `compact`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RpcCompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    #[serde(default)]
    pub details: Value,
}

/// Result payload returned by RPC `fork`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcForkResult {
    pub text: String,
    pub cancelled: bool,
}

/// Forkable message entry returned by RPC `get_fork_messages`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RpcForkMessage {
    pub entry_id: String,
    pub text: String,
}

/// Slash command metadata returned by RPC `get_commands`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcCommandInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub source: String,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

/// Export HTML response payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcExportHtmlResult {
    pub path: String,
}

/// Last-assistant-text response payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcLastAssistantText {
    pub text: Option<String>,
}

/// Extension UI response payload used by RPC `extension_ui_response`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RpcExtensionUiResponse {
    Value { value: Value },
    Confirmed { confirmed: bool },
    Cancelled,
}

/// Process-boundary transport options for SDK callers that prefer RPC mode.
#[derive(Debug, Clone)]
pub struct RpcTransportOptions {
    pub binary_path: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

impl Default for RpcTransportOptions {
    fn default() -> Self {
        Self {
            binary_path: PathBuf::from("pi"),
            args: vec!["--mode".to_string(), "rpc".to_string()],
            cwd: None,
        }
    }
}

/// Subprocess-backed SDK transport for `pi --mode rpc`.
pub struct RpcTransportClient {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_request_id: u64,
}

/// Unified adapter over in-process and subprocess-backed session control.
pub enum SessionTransport {
    InProcess(Box<AgentSessionHandle>),
    RpcSubprocess(RpcTransportClient),
}

impl SessionTransport {
    pub async fn in_process(options: SessionOptions) -> Result<Self> {
        create_agent_session(options)
            .await
            .map(Box::new)
            .map(Self::InProcess)
    }

    pub fn rpc_subprocess(options: RpcTransportOptions) -> Result<Self> {
        RpcTransportClient::connect(options).map(Self::RpcSubprocess)
    }

    #[allow(clippy::missing_const_for_fn)]
    pub fn as_in_process_mut(&mut self) -> Option<&mut AgentSessionHandle> {
        match self {
            Self::InProcess(handle) => Some(handle.as_mut()),
            Self::RpcSubprocess(_) => None,
        }
    }

    #[allow(clippy::missing_const_for_fn)]
    pub fn as_rpc_mut(&mut self) -> Option<&mut RpcTransportClient> {
        match self {
            Self::InProcess(_) => None,
            Self::RpcSubprocess(client) => Some(client),
        }
    }

    /// Send one prompt over whichever transport is active.
    ///
    /// - In-process mode returns the final assistant message.
    /// - RPC mode waits for `agent_end` and returns collected raw events.
    pub async fn prompt(
        &mut self,
        input: impl Into<String>,
        on_event: impl Fn(SessionTransportEvent) + Send + Sync + 'static,
    ) -> Result<SessionPromptResult> {
        let input = input.into();
        let on_event = Arc::new(on_event);
        match self {
            Self::InProcess(handle) => {
                let on_event = Arc::clone(&on_event);
                let assistant = handle
                    .prompt(input, move |event| {
                        (on_event)(SessionTransportEvent::InProcess(event));
                    })
                    .await?;
                Ok(SessionPromptResult::InProcess(Box::new(assistant)))
            }
            Self::RpcSubprocess(client) => {
                let events = client.prompt(input).await?;
                for event in events.iter().cloned() {
                    (on_event)(SessionTransportEvent::Rpc(event));
                }
                Ok(SessionPromptResult::RpcEvents(events))
            }
        }
    }

    /// Return a state snapshot from the active transport.
    pub async fn state(&mut self) -> Result<SessionTransportState> {
        match self {
            Self::InProcess(handle) => handle.state().await.map(SessionTransportState::InProcess),
            Self::RpcSubprocess(client) => client
                .get_state()
                .await
                .map(Box::new)
                .map(SessionTransportState::Rpc),
        }
    }

    /// Update provider/model for the active transport.
    pub async fn set_model(&mut self, provider: &str, model_id: &str) -> Result<()> {
        match self {
            Self::InProcess(handle) => handle.set_model(provider, model_id).await,
            Self::RpcSubprocess(client) => {
                let _ = client.set_model(provider, model_id).await?;
                Ok(())
            }
        }
    }

    /// Shut down transport resources (best effort for in-process, explicit for RPC).
    pub fn shutdown(&mut self) -> Result<()> {
        match self {
            Self::InProcess(_) => Ok(()),
            Self::RpcSubprocess(client) => client.shutdown(),
        }
    }
}

impl RpcTransportClient {
    pub fn connect(options: RpcTransportOptions) -> Result<Self> {
        let mut command = Command::new(&options.binary_path);
        command
            .args(&options.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(cwd) = options.cwd {
            command.current_dir(cwd);
        }

        let mut child = command.spawn().map_err(|err| {
            Error::config(format!(
                "Failed to spawn RPC subprocess {}: {err}",
                options.binary_path.display()
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::config("RPC subprocess stdin is not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::config("RPC subprocess stdout is not piped"))?;

        Ok(Self {
            child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
            next_request_id: 1,
        })
    }

    #[allow(
        clippy::unused_async,
        reason = "SDK RPC transport keeps an async public API"
    )]
    pub async fn request(&mut self, command: &str, payload: Map<String, Value>) -> Result<Value> {
        let request_id = self.next_request_id();
        let mut command_payload = Map::new();
        command_payload.insert("type".to_string(), Value::String(command.to_string()));
        command_payload.insert("id".to_string(), Value::String(request_id.clone()));
        command_payload.extend(payload);

        self.write_json_line(&Value::Object(command_payload))?;
        self.wait_for_response(&request_id, command)
    }

    fn parse_response_data<T: DeserializeOwned>(data: Value, command: &str) -> Result<T> {
        serde_json::from_value(data).map_err(|err| {
            Error::api(format!(
                "Failed to decode RPC `{command}` response payload: {err}"
            ))
        })
    }

    async fn request_typed<T: DeserializeOwned>(
        &mut self,
        command: &str,
        payload: Map<String, Value>,
    ) -> Result<T> {
        let data = self.request(command, payload).await?;
        Self::parse_response_data(data, command)
    }

    async fn request_no_data(&mut self, command: &str, payload: Map<String, Value>) -> Result<()> {
        let _ = self.request(command, payload).await?;
        Ok(())
    }

    pub async fn steer(&mut self, message: impl Into<String>) -> Result<()> {
        let mut payload = Map::new();
        payload.insert("message".to_string(), Value::String(message.into()));
        self.request_no_data("steer", payload).await
    }

    pub async fn follow_up(&mut self, message: impl Into<String>) -> Result<()> {
        let mut payload = Map::new();
        payload.insert("message".to_string(), Value::String(message.into()));
        self.request_no_data("follow_up", payload).await
    }

    pub async fn abort(&mut self) -> Result<()> {
        self.request_no_data("abort", Map::new()).await
    }

    pub async fn new_session(
        &mut self,
        parent_session: Option<&Path>,
    ) -> Result<RpcCancelledResult> {
        let mut payload = Map::new();
        if let Some(parent_session) = parent_session {
            payload.insert(
                "parentSession".to_string(),
                Value::String(parent_session.display().to_string()),
            );
        }
        self.request_typed("new_session", payload).await
    }

    pub async fn get_state(&mut self) -> Result<RpcSessionState> {
        self.request_typed("get_state", Map::new()).await
    }

    pub async fn get_session_stats(&mut self) -> Result<RpcSessionStats> {
        self.request_typed("get_session_stats", Map::new()).await
    }

    pub async fn get_messages(&mut self) -> Result<Vec<Value>> {
        #[derive(Deserialize)]
        struct MessagesPayload {
            messages: Vec<Value>,
        }
        let payload: MessagesPayload = self.request_typed("get_messages", Map::new()).await?;
        Ok(payload.messages)
    }

    pub async fn get_available_models(&mut self) -> Result<Vec<RpcModelInfo>> {
        #[derive(Deserialize)]
        struct ModelsPayload {
            models: Vec<RpcModelInfo>,
        }
        let payload: ModelsPayload = self
            .request_typed("get_available_models", Map::new())
            .await?;
        Ok(payload.models)
    }

    pub async fn set_model(&mut self, provider: &str, model_id: &str) -> Result<RpcModelInfo> {
        let mut payload = Map::new();
        payload.insert("provider".to_string(), Value::String(provider.to_string()));
        payload.insert("modelId".to_string(), Value::String(model_id.to_string()));
        self.request_typed("set_model", payload).await
    }

    pub async fn cycle_model(&mut self) -> Result<Option<RpcCycleModelResult>> {
        self.request_typed("cycle_model", Map::new()).await
    }

    pub async fn set_thinking_level(&mut self, level: crate::model::ThinkingLevel) -> Result<()> {
        let mut payload = Map::new();
        payload.insert("level".to_string(), Value::String(level.to_string()));
        self.request_no_data("set_thinking_level", payload).await
    }

    pub async fn cycle_thinking_level(&mut self) -> Result<Option<RpcThinkingLevelResult>> {
        self.request_typed("cycle_thinking_level", Map::new()).await
    }

    pub async fn set_steering_mode(&mut self, mode: &str) -> Result<()> {
        let mut payload = Map::new();
        payload.insert("mode".to_string(), Value::String(mode.to_string()));
        self.request_no_data("set_steering_mode", payload).await
    }

    pub async fn set_follow_up_mode(&mut self, mode: &str) -> Result<()> {
        let mut payload = Map::new();
        payload.insert("mode".to_string(), Value::String(mode.to_string()));
        self.request_no_data("set_follow_up_mode", payload).await
    }

    pub async fn set_auto_compaction(&mut self, enabled: bool) -> Result<()> {
        let mut payload = Map::new();
        payload.insert("enabled".to_string(), Value::Bool(enabled));
        self.request_no_data("set_auto_compaction", payload).await
    }

    pub async fn set_auto_retry(&mut self, enabled: bool) -> Result<()> {
        let mut payload = Map::new();
        payload.insert("enabled".to_string(), Value::Bool(enabled));
        self.request_no_data("set_auto_retry", payload).await
    }

    pub async fn abort_retry(&mut self) -> Result<()> {
        self.request_no_data("abort_retry", Map::new()).await
    }

    pub async fn set_session_name(&mut self, name: impl Into<String>) -> Result<()> {
        let mut payload = Map::new();
        payload.insert("name".to_string(), Value::String(name.into()));
        self.request_no_data("set_session_name", payload).await
    }

    pub async fn get_last_assistant_text(&mut self) -> Result<Option<String>> {
        let payload: RpcLastAssistantText = self
            .request_typed("get_last_assistant_text", Map::new())
            .await?;
        Ok(payload.text)
    }

    pub async fn export_html(&mut self, output_path: Option<&Path>) -> Result<RpcExportHtmlResult> {
        let mut payload = Map::new();
        if let Some(path) = output_path {
            payload.insert(
                "outputPath".to_string(),
                Value::String(path.display().to_string()),
            );
        }
        self.request_typed("export_html", payload).await
    }

    pub async fn bash(&mut self, command: impl Into<String>) -> Result<RpcBashResult> {
        let mut payload = Map::new();
        payload.insert("command".to_string(), Value::String(command.into()));
        self.request_typed("bash", payload).await
    }

    pub async fn abort_bash(&mut self) -> Result<()> {
        self.request_no_data("abort_bash", Map::new()).await
    }

    pub async fn compact(&mut self) -> Result<RpcCompactionResult> {
        self.compact_with_instructions(None).await
    }

    pub async fn compact_with_instructions(
        &mut self,
        custom_instructions: Option<&str>,
    ) -> Result<RpcCompactionResult> {
        let mut payload = Map::new();
        if let Some(custom_instructions) = custom_instructions {
            payload.insert(
                "customInstructions".to_string(),
                Value::String(custom_instructions.to_string()),
            );
        }
        self.request_typed("compact", payload).await
    }

    pub async fn switch_session(&mut self, session_path: &Path) -> Result<RpcCancelledResult> {
        let mut payload = Map::new();
        payload.insert(
            "sessionPath".to_string(),
            Value::String(session_path.display().to_string()),
        );
        self.request_typed("switch_session", payload).await
    }

    pub async fn fork(&mut self, entry_id: impl Into<String>) -> Result<RpcForkResult> {
        let mut payload = Map::new();
        payload.insert("entryId".to_string(), Value::String(entry_id.into()));
        self.request_typed("fork", payload).await
    }

    pub async fn get_fork_messages(&mut self) -> Result<Vec<RpcForkMessage>> {
        #[derive(Deserialize)]
        struct ForkMessagesPayload {
            messages: Vec<RpcForkMessage>,
        }
        let payload: ForkMessagesPayload =
            self.request_typed("get_fork_messages", Map::new()).await?;
        Ok(payload.messages)
    }

    pub async fn get_commands(&mut self) -> Result<Vec<RpcCommandInfo>> {
        #[derive(Deserialize)]
        struct CommandsPayload {
            commands: Vec<RpcCommandInfo>,
        }
        let payload: CommandsPayload = self.request_typed("get_commands", Map::new()).await?;
        Ok(payload.commands)
    }

    pub async fn extension_ui_response(
        &mut self,
        request_id: &str,
        response: RpcExtensionUiResponse,
    ) -> Result<bool> {
        #[derive(Deserialize)]
        struct ExtensionUiResolvedPayload {
            resolved: bool,
        }

        let mut payload = Map::new();
        payload.insert(
            "requestId".to_string(),
            Value::String(request_id.to_string()),
        );

        match response {
            RpcExtensionUiResponse::Value { value } => {
                payload.insert("value".to_string(), value);
            }
            RpcExtensionUiResponse::Confirmed { confirmed } => {
                payload.insert("confirmed".to_string(), Value::Bool(confirmed));
            }
            RpcExtensionUiResponse::Cancelled => {
                payload.insert("cancelled".to_string(), Value::Bool(true));
            }
        }

        let response: Option<ExtensionUiResolvedPayload> =
            self.request_typed("extension_ui_response", payload).await?;
        Ok(response.is_none_or(|payload| payload.resolved))
    }

    pub async fn prompt(&mut self, message: impl Into<String>) -> Result<Vec<Value>> {
        self.prompt_with_options(message, None, None).await
    }

    #[allow(
        clippy::unused_async,
        reason = "SDK RPC transport keeps an async public API"
    )]
    pub async fn prompt_with_options(
        &mut self,
        message: impl Into<String>,
        images: Option<Vec<ImageContent>>,
        streaming_behavior: Option<&str>,
    ) -> Result<Vec<Value>> {
        let request_id = self.next_request_id();
        let mut payload = Map::new();
        payload.insert("type".to_string(), Value::String("prompt".to_string()));
        payload.insert("id".to_string(), Value::String(request_id.clone()));
        payload.insert("message".to_string(), Value::String(message.into()));
        if let Some(images) = images {
            payload.insert(
                "images".to_string(),
                serde_json::to_value(images).map_err(|err| Error::Json(Box::new(err)))?,
            );
        }
        if let Some(streaming_behavior) = streaming_behavior {
            payload.insert(
                "streamingBehavior".to_string(),
                Value::String(streaming_behavior.to_string()),
            );
        }
        let payload = Value::Object(payload);
        self.write_json_line(&payload)?;

        let mut saw_ack = false;
        let mut events = Vec::new();
        loop {
            let item = self.read_json_line()?;
            let item_type = item.get("type").and_then(Value::as_str);
            if item_type == Some("response") {
                if item.get("id").and_then(Value::as_str) != Some(request_id.as_str()) {
                    continue;
                }
                let success = item
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !success {
                    return Err(rpc_error_from_response(&item, "prompt"));
                }
                saw_ack = true;
                continue;
            }

            if saw_ack {
                let reached_end = item_type == Some("agent_end");
                events.push(item);
                if reached_end {
                    return Ok(events);
                }
            }
        }
    }

    pub fn shutdown(&mut self) -> Result<()> {
        if self
            .child
            .try_wait()
            .map_err(|err| Error::Io(Box::new(err)))?
            .is_none()
        {
            self.child.kill().map_err(|err| Error::Io(Box::new(err)))?;
        }
        let _ = self.child.wait();
        Ok(())
    }

    fn next_request_id(&mut self) -> String {
        let id = format!("rpc-{}", self.next_request_id);
        self.next_request_id = self.next_request_id.saturating_add(1);
        id
    }

    fn write_json_line(&mut self, payload: &Value) -> Result<()> {
        let encoded = serde_json::to_string(payload).map_err(|err| Error::Json(Box::new(err)))?;
        self.stdin
            .write_all(encoded.as_bytes())
            .map_err(|err| Error::Io(Box::new(err)))?;
        self.stdin
            .write_all(b"\n")
            .map_err(|err| Error::Io(Box::new(err)))?;
        self.stdin.flush().map_err(|err| Error::Io(Box::new(err)))?;
        Ok(())
    }

    fn read_json_line(&mut self) -> Result<Value> {
        let mut line = String::new();
        let read = self
            .stdout
            .read_line(&mut line)
            .map_err(|err| Error::Io(Box::new(err)))?;
        if read == 0 {
            return Err(Error::api(
                "RPC subprocess exited before sending a response",
            ));
        }
        serde_json::from_str(line.trim_end()).map_err(|err| Error::Json(Box::new(err)))
    }

    fn wait_for_response(&mut self, request_id: &str, command: &str) -> Result<Value> {
        loop {
            let item = self.read_json_line()?;
            let Some(item_type) = item.get("type").and_then(Value::as_str) else {
                continue;
            };
            if item_type != "response" {
                continue;
            }
            if item.get("id").and_then(Value::as_str) != Some(request_id) {
                continue;
            }
            if item.get("command").and_then(Value::as_str) != Some(command) {
                continue;
            }

            let success = item
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if success {
                return Ok(item.get("data").cloned().unwrap_or(Value::Null));
            }
            return Err(rpc_error_from_response(&item, command));
        }
    }
}

impl Drop for RpcTransportClient {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn rpc_error_from_response(response: &Value, command: &str) -> Error {
    let error = response
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("RPC command failed");
    Error::api(format!("RPC {command} failed: {error}"))
}

impl AgentSessionHandle {
    /// Create a handle from a pre-built `AgentSession` with custom listeners.
    ///
    /// This is useful for tests and advanced embedding scenarios where
    /// the full `create_agent_session()` flow is not needed.
    pub const fn from_session_with_listeners(
        session: AgentSession,
        listeners: EventListeners,
    ) -> Self {
        Self { session, listeners }
    }

    /// Send one user prompt through the agent loop.
    ///
    /// The `on_event` callback receives events for this prompt only.
    /// Session-level listeners registered via [`Self::subscribe`] or
    /// [`SessionOptions`] callbacks also fire for every event.
    pub async fn prompt(
        &mut self,
        input: impl Into<String>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let combined = self.make_combined_callback(on_event);
        self.session.run_text(input.into(), combined).await
    }

    /// Send one user prompt through the agent loop with an explicit abort signal.
    pub async fn prompt_with_abort(
        &mut self,
        input: impl Into<String>,
        abort_signal: AbortSignal,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let combined = self.make_combined_callback(on_event);
        self.session
            .run_text_with_abort(input.into(), Some(abort_signal), combined)
            .await
    }

    /// Continue the current agent loop without adding a new user prompt.
    ///
    /// This is useful for retry/continuation flows where session history or
    /// injected messages should drive the next turn without synthesizing a new
    /// user message through [`Self::prompt`].
    pub async fn continue_turn(
        &mut self,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let combined = self.make_combined_callback(on_event);
        self.session
            .sync_runtime_selection_from_session_header()
            .await?;
        self.session
            .agent
            .run_continue_with_abort(None, combined)
            .await
    }

    /// Continue the current agent loop with an explicit abort signal.
    pub async fn continue_turn_with_abort(
        &mut self,
        abort_signal: AbortSignal,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let combined = self.make_combined_callback(on_event);
        self.session
            .sync_runtime_selection_from_session_header()
            .await?;
        self.session
            .agent
            .run_continue_with_abort(Some(abort_signal), combined)
            .await
    }

    /// Create a new abort handle/signal pair for prompt cancellation.
    pub fn new_abort_handle() -> (AbortHandle, AbortSignal) {
        AbortHandle::new()
    }

    /// Register a session-level event listener.
    ///
    /// The listener fires for every [`AgentEvent`] across all future prompts
    /// until removed via [`Self::unsubscribe`].
    ///
    /// Returns a [`SubscriptionId`] that can be used to remove the listener.
    pub fn subscribe(
        &self,
        listener: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> SubscriptionId {
        self.listeners.subscribe(Arc::new(listener))
    }

    /// Remove a previously registered event listener.
    ///
    /// Returns `true` if the listener was found and removed.
    pub fn unsubscribe(&self, id: SubscriptionId) -> bool {
        self.listeners.unsubscribe(id)
    }

    /// Access the session-level event listeners.
    pub const fn listeners(&self) -> &EventListeners {
        &self.listeners
    }

    /// Mutable access to session-level event listeners.
    ///
    /// Allows updating typed hooks (`on_tool_start`, `on_tool_end`,
    /// `on_stream_event`) after session creation.
    pub const fn listeners_mut(&mut self) -> &mut EventListeners {
        &mut self.listeners
    }

    // -----------------------------------------------------------------
    // Extensions & Capability Policy
    // -----------------------------------------------------------------

    /// Whether this session has extensions loaded.
    pub const fn has_extensions(&self) -> bool {
        self.session.extensions.is_some()
    }

    /// Return a reference to the extension manager (if extensions are loaded).
    pub fn extension_manager(&self) -> Option<&ExtensionManager> {
        self.session
            .extensions
            .as_ref()
            .map(ExtensionRegion::manager)
    }

    /// Return a reference to the extension region (if extensions are loaded).
    ///
    /// The region wraps the extension manager with lifecycle management.
    pub const fn extension_region(&self) -> Option<&ExtensionRegion> {
        self.session.extensions.as_ref()
    }

    // -----------------------------------------------------------------
    // Provider & Model
    // -----------------------------------------------------------------

    /// Return the active provider/model pair.
    pub fn model(&self) -> (String, String) {
        let provider = self.session.agent.provider();
        (provider.name().to_string(), provider.model_id().to_string())
    }

    /// Update the active provider/model pair and persist it to session metadata.
    pub async fn set_model(&mut self, provider: &str, model_id: &str) -> Result<()> {
        self.session.set_provider_model(provider, model_id).await
    }

    /// Return the currently configured thinking level.
    pub const fn thinking_level(&self) -> Option<crate::model::ThinkingLevel> {
        self.session.agent.stream_options().thinking_level
    }

    /// Alias for thinking level access, matching the SDK naming style.
    pub const fn thinking(&self) -> Option<crate::model::ThinkingLevel> {
        self.thinking_level()
    }

    /// Update thinking level and persist it to session metadata.
    ///
    /// Delegates to [`AgentSession::set_thinking_level`] so the runtime
    /// reconfiguration logic (clamping, history dedupe, persistence) lives in
    /// one place and stays consistent across the SDK handle and the ACP
    /// transport, which both need it.
    pub async fn set_thinking_level(&mut self, level: crate::model::ThinkingLevel) -> Result<()> {
        self.session.set_thinking_level(level).await
    }

    /// Update the persisted session display name.
    ///
    /// Records a `SessionInfo` entry with the new name on the leaf path and
    /// flushes session metadata to disk, mirroring how the in-process rename
    /// path works for runtime CLIs that need to retitle a live session.
    pub async fn set_session_name(&mut self, name: impl Into<String>) -> Result<()> {
        let name = name.into();
        let cx = crate::agent_cx::AgentCx::for_request();
        {
            let mut guard = self
                .session
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            guard.append_session_info(Some(name));
        }
        self.session.persist_session().await
    }

    /// Read the per-prompt `max_tokens` cap currently configured on the
    /// session's stream options.
    ///
    /// `None` means the provider's default applies (e.g. 4096 for the
    /// openai-compat provider), which can truncate generations that emit
    /// large tool-call arguments. Embedders carrying tool-heavy traffic
    /// should raise this via [`Self::set_max_tokens`].
    pub const fn max_tokens(&self) -> Option<u32> {
        self.session.agent.stream_options().max_tokens
    }

    /// Override the per-prompt `max_tokens` cap.
    ///
    /// Persists for the lifetime of the in-process handle only; not written
    /// to session metadata. Pass `None` to fall back to the provider's
    /// default cap.
    pub const fn set_max_tokens(&mut self, max_tokens: Option<u32>) {
        self.session.agent.stream_options_mut().max_tokens = max_tokens;
    }

    /// Return all model messages for the current session path.
    pub async fn messages(&self) -> Result<Vec<Message>> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let guard = self
            .session
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        Ok(guard.to_messages_for_current_path())
    }

    /// Return a lightweight state snapshot.
    pub async fn state(&self) -> Result<AgentSessionState> {
        let (provider, model_id) = self.model();
        let thinking_level = self.thinking_level();
        let save_enabled = self.session.save_enabled();
        let cx = crate::agent_cx::AgentCx::for_request();
        let guard = self
            .session
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        let session_id = Some(guard.header.id.clone());
        let message_count = guard.to_messages_for_current_path().len();

        Ok(AgentSessionState {
            session_id,
            provider,
            model_id,
            thinking_level,
            save_enabled,
            message_count,
        })
    }

    /// Trigger an immediate compaction pass (if compaction is enabled).
    pub async fn compact(
        &mut self,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<()> {
        self.session.compact_now(on_event).await
    }

    /// Access the underlying `AgentSession`.
    pub const fn session(&self) -> &AgentSession {
        &self.session
    }

    /// Mutable access to the underlying `AgentSession`.
    pub const fn session_mut(&mut self) -> &mut AgentSession {
        &mut self.session
    }

    /// Consume the handle and return the inner `AgentSession`.
    pub fn into_inner(self) -> AgentSession {
        self.session
    }

    /// Build a combined callback that fans out to the per-prompt callback,
    /// session-level subscribers, and typed hooks.
    fn make_combined_callback(
        &self,
        per_prompt: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> impl Fn(AgentEvent) + Send + Sync + 'static {
        let listeners = self.listeners.clone();
        move |event: AgentEvent| {
            // Typed tool hooks — fire before generic listeners.
            match &event {
                AgentEvent::ToolExecutionStart {
                    tool_name, args, ..
                } => {
                    listeners.notify_tool_start(tool_name, args);
                }
                AgentEvent::ToolExecutionEnd {
                    tool_name,
                    result,
                    is_error,
                    ..
                } => {
                    listeners.notify_tool_end(tool_name, result, *is_error);
                }
                AgentEvent::MessageUpdate {
                    assistant_message_event,
                    ..
                } => {
                    // Forward raw stream events from the nested
                    // `AssistantMessageEvent` when possible.
                    if let Some(stream_ev) =
                        stream_event_from_assistant_message_event(assistant_message_event)
                    {
                        listeners.notify_stream_event(&stream_ev);
                    }
                }
                _ => {}
            }

            // Session-level generic subscribers.
            listeners.notify(&event);

            // Per-prompt callback.
            per_prompt(event);
        }
    }
}

/// Extract a raw [`StreamEvent`] equivalent from an [`AssistantMessageEvent`].
///
/// This lets the typed `on_stream_event` hook fire with the low-level provider
/// protocol event rather than the wrapped agent-level event.
fn stream_event_from_assistant_message_event(
    event: &crate::model::AssistantMessageEvent,
) -> Option<StreamEvent> {
    use crate::model::AssistantMessageEvent as AME;
    match event {
        AME::TextStart { content_index, .. } => Some(StreamEvent::TextStart {
            content_index: *content_index,
        }),
        AME::TextDelta {
            content_index,
            delta,
            ..
        } => Some(StreamEvent::TextDelta {
            content_index: *content_index,
            delta: delta.clone(),
        }),
        AME::TextEnd {
            content_index,
            content,
            ..
        } => Some(StreamEvent::TextEnd {
            content_index: *content_index,
            content: content.clone(),
        }),
        AME::ThinkingStart { content_index, .. } => Some(StreamEvent::ThinkingStart {
            content_index: *content_index,
        }),
        AME::ThinkingDelta {
            content_index,
            delta,
            ..
        } => Some(StreamEvent::ThinkingDelta {
            content_index: *content_index,
            delta: delta.clone(),
        }),
        AME::ThinkingEnd {
            content_index,
            content,
            ..
        } => Some(StreamEvent::ThinkingEnd {
            content_index: *content_index,
            content: content.clone(),
        }),
        AME::ToolCallStart {
            content_index,
            partial,
        } => {
            // #129: recover the tool-call id/name from the partial so the
            // reconstructed stream stays correlatable from the start.
            let (id, name) = match partial.content.get(*content_index) {
                Some(ContentBlock::ToolCall(tc)) => (tc.id.clone(), tc.name.clone()),
                _ => (String::new(), String::new()),
            };
            Some(StreamEvent::ToolCallStart {
                content_index: *content_index,
                id,
                name,
            })
        }
        AME::ToolCallDelta {
            content_index,
            delta,
            ..
        } => Some(StreamEvent::ToolCallDelta {
            content_index: *content_index,
            delta: delta.clone(),
        }),
        AME::ToolCallEnd {
            content_index,
            tool_call,
            ..
        } => Some(StreamEvent::ToolCallEnd {
            content_index: *content_index,
            tool_call: tool_call.clone(),
        }),
        AME::Done { reason, message } => Some(StreamEvent::Done {
            reason: *reason,
            message: (**message).clone(),
        }),
        AME::Error { reason, error } => Some(StreamEvent::Error {
            reason: *reason,
            error: (**error).clone(),
        }),
        AME::Start { .. } => None,
    }
}

fn resolve_path_for_cwd(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn build_stream_options_with_optional_key(
    config: &Config,
    api_key: Option<String>,
    selection: &app::ModelSelection,
    session: &Session,
) -> StreamOptions {
    let mut options = StreamOptions {
        api_key,
        headers: selection.model_entry.headers.clone(),
        session_id: Some(session.header.id.clone()),
        thinking_level: Some(selection.thinking_level),
        // Seed the per-request output cap from the model registry's `maxTokens`
        // so embedders inherit the configured limit by default; they can still
        // override it via `set_max_tokens`.
        max_tokens: Some(selection.model_entry.model.max_tokens),
        ..Default::default()
    };

    if let Some(budgets) = &config.thinking_budgets {
        let defaults = ThinkingBudgets::default();
        options.thinking_budgets = Some(ThinkingBudgets {
            minimal: budgets.minimal.unwrap_or(defaults.minimal),
            low: budgets.low.unwrap_or(defaults.low),
            medium: budgets.medium.unwrap_or(defaults.medium),
            high: budgets.high.unwrap_or(defaults.high),
            xhigh: budgets.xhigh.unwrap_or(defaults.xhigh),
            max: budgets.max.unwrap_or(defaults.max),
        });
    }

    options
}

/// Create a fully configured embeddable agent session.
///
/// This is the programmatic entrypoint for non-CLI consumers that want to run
/// Pi sessions in-process.
#[allow(clippy::too_many_lines)]
pub async fn create_agent_session(options: SessionOptions) -> Result<AgentSessionHandle> {
    let process_cwd =
        std::env::current_dir().map_err(|e| Error::config(format!("cwd lookup failed: {e}")))?;
    let cwd = options.working_directory.as_deref().map_or_else(
        || process_cwd.clone(),
        |path| resolve_path_for_cwd(path, &process_cwd),
    );
    let resolved_session_path = options
        .session_path
        .as_deref()
        .map(|path| resolve_path_for_cwd(path, &cwd));
    let resolved_session_dir = options
        .session_dir
        .as_deref()
        .map(|path| resolve_path_for_cwd(path, &cwd));

    let mut cli = Cli::try_parse_from(["pi"])
        .map_err(|e| Error::validation(format!("CLI init failed: {e}")))?;
    cli.no_session = options.no_session;
    cli.provider = options.provider.clone();
    cli.model = options.model.clone();
    cli.api_key = options.api_key.clone();
    cli.system_prompt = options.system_prompt.clone();
    cli.append_system_prompt = options.append_system_prompt.clone();
    cli.hide_cwd_in_prompt = !options.include_cwd_in_prompt;
    cli.thinking = options.thinking.map(|t| t.to_string());
    cli.session = resolved_session_path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    cli.session_dir = resolved_session_dir
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    if let Some(enabled_tools) = &options.enabled_tools {
        if enabled_tools.is_empty() {
            cli.no_tools = true;
        } else {
            cli.no_tools = false;
            cli.tools = enabled_tools.join(",");
        }
    }

    let config = Config::load()?;

    let mut auth = AuthStorage::load_async(Config::auth_path()).await?;
    auth.refresh_expired_oauth_tokens().await?;

    let global_dir = Config::global_dir();
    let package_dir = Config::package_dir();
    let models_path = default_models_path(&global_dir);
    let model_registry = ModelRegistry::load(&auth, Some(models_path));

    let mut session = Session::new(&cli, &config).await?;
    if resolved_session_path.is_none() {
        session.header.cwd = cwd.display().to_string();
    }
    let scoped_patterns = if let Some(models_arg) = &cli.models {
        app::parse_models_arg(models_arg)
    } else {
        config.enabled_models.clone().unwrap_or_default()
    };
    let scoped_models = if scoped_patterns.is_empty() {
        Vec::new()
    } else {
        app::resolve_model_scope(&scoped_patterns, &model_registry, cli.api_key.is_some())
    };

    let selection = app::select_model_and_thinking(
        &cli,
        &config,
        &session,
        &model_registry,
        &scoped_models,
        &global_dir,
    )
    .map_err(|err| Error::validation(err.to_string()))?;
    app::update_session_for_selection(&mut session, &selection);

    let enabled_tools_owned = cli
        .enabled_tools()
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let enabled_tools = enabled_tools_owned
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();

    let system_prompt = app::build_system_prompt(
        &cli,
        &cwd,
        &enabled_tools,
        None,
        &global_dir,
        &package_dir,
        std::env::var_os("PI_TEST_MODE").is_some(),
        options.include_cwd_in_prompt,
    )
    .map_err(|err| Error::validation(err.to_string()))?;

    let provider = providers::create_provider(&selection.model_entry, None)
        .map_err(|e| Error::provider("sdk", e.to_string()))?;

    let api_key = app::resolve_api_key(&auth, &cli, &selection.model_entry)
        .map_err(|err| Error::validation(err.to_string()))?;

    let stream_options =
        build_stream_options_with_optional_key(&config, api_key, &selection, &session);

    let agent_config = AgentConfig {
        system_prompt: Some(system_prompt),
        max_tool_iterations: options.max_tool_iterations,
        stream_options,
        block_images: config.image_block_images(),
        fail_closed_hooks: config.fail_closed_hooks(),
        tool_approval: None,
    };

    let tools = options.tool_factory.as_ref().map_or_else(
        || ToolRegistry::new(&enabled_tools, &cwd, Some(&config)),
        |factory| factory.create_tool_registry(&enabled_tools, &cwd, &config),
    );
    let session_arc = Arc::new(asupersync::sync::Mutex::new(session));

    let context_window_tokens = if selection.model_entry.model.context_window == 0 {
        ResolvedCompactionSettings::default().context_window_tokens
    } else {
        selection.model_entry.model.context_window
    };
    let compaction_settings = ResolvedCompactionSettings {
        enabled: config.compaction_enabled(),
        reserve_tokens: config.compaction_reserve_tokens(),
        keep_recent_tokens: config.compaction_keep_recent_tokens(),
        context_window_tokens,
    };

    let mut agent_session = AgentSession::new(
        Agent::new(provider, tools, agent_config),
        Arc::clone(&session_arc),
        !cli.no_session,
        compaction_settings,
    );
    agent_session.set_api_key_override(options.api_key.clone());

    if !options.extension_paths.is_empty() {
        let extension_paths = options
            .extension_paths
            .iter()
            .map(|path| resolve_path_for_cwd(path, &cwd))
            .collect::<Vec<_>>();
        let resolved_ext_policy =
            config.resolve_extension_policy_with_metadata(options.extension_policy.as_deref());
        let resolved_repair_policy =
            config.resolve_repair_policy_with_metadata(options.repair_policy.as_deref());

        agent_session
            .enable_extensions_with_policy(
                &enabled_tools,
                &cwd,
                Some(&config),
                &extension_paths,
                Some(resolved_ext_policy.policy),
                Some(resolved_repair_policy.effective_mode),
                None,
            )
            .await?;
    }

    agent_session.set_model_registry(model_registry.clone());
    agent_session.set_auth_storage(auth);

    let history = {
        let cx = crate::agent_cx::AgentCx::for_request();
        let guard = session_arc
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        guard.to_messages_for_current_path()
    };
    if !history.is_empty() {
        agent_session.agent.replace_messages(history);
    }

    let mut listeners = EventListeners::new();
    if let Some(on_event) = options.on_event {
        listeners.subscribe(on_event);
    }
    listeners.on_tool_start = options.on_tool_start;
    listeners.on_tool_end = options.on_tool_end;
    listeners.on_stream_event = options.on_stream_event;

    Ok(AgentSessionHandle {
        session: agent_session,
        listeners,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use asupersync::runtime::reactor::create_reactor;
    use asupersync::sync::Mutex as AsyncMutex;
    use std::env;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    fn run_async<F>(future: F) -> F::Output
    where
        F: std::future::Future,
    {
        let reactor = create_reactor().expect("create reactor");
        let runtime = RuntimeBuilder::current_thread()
            .with_reactor(reactor)
            .build()
            .expect("build runtime");
        runtime.block_on(future)
    }

    fn current_dir_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_current_dir_lock()
    }

    struct CurrentDirGuard {
        previous: PathBuf,
    }

    impl CurrentDirGuard {
        fn new(path: &Path) -> Self {
            let previous = env::current_dir().expect("current dir");
            env::set_current_dir(path).expect("set current dir");
            Self { previous }
        }
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.previous);
        }
    }

    fn hermetic_session_options(working_directory: &Path) -> SessionOptions {
        SessionOptions {
            provider: Some("openai".to_string()),
            model: Some("gpt-4o".to_string()),
            api_key: Some("dummy-key".to_string()),
            working_directory: Some(working_directory.to_path_buf()),
            no_session: true,
            ..SessionOptions::default()
        }
    }

    #[test]
    fn create_agent_session_with_explicit_test_provider_succeeds() {
        let tmp = tempdir().expect("tempdir");
        let options = hermetic_session_options(tmp.path());

        let handle = run_async(create_agent_session(options)).expect("create session");
        let provider = handle.session().agent.provider();
        assert!(!provider.name().is_empty());
        assert!(!provider.model_id().is_empty());
        assert_eq!(handle.model().0, provider.name());
        assert_eq!(handle.model().1, provider.model_id());
    }

    #[test]
    fn create_agent_session_respects_provider_model_and_clamps_thinking() {
        let tmp = tempdir().expect("tempdir");
        let options = SessionOptions {
            provider: Some("openai".to_string()),
            model: Some("gpt-4o".to_string()),
            api_key: Some("dummy-key".to_string()),
            thinking: Some(crate::model::ThinkingLevel::Low),
            working_directory: Some(tmp.path().to_path_buf()),
            no_session: true,
            ..SessionOptions::default()
        };

        let handle = run_async(create_agent_session(options)).expect("create session");
        let provider = handle.session().agent.provider();
        assert_eq!(provider.name(), "openai");
        assert_eq!(provider.model_id(), "gpt-4o");
        assert_eq!(
            handle.session().agent.stream_options().thinking_level,
            Some(crate::model::ThinkingLevel::Off)
        );
    }

    #[test]
    fn create_agent_session_no_session_keeps_ephemeral_state() {
        let tmp = tempdir().expect("tempdir");
        let options = hermetic_session_options(tmp.path());

        let handle = run_async(create_agent_session(options)).expect("create session");
        assert!(!handle.session().save_enabled());

        let path_is_none = run_async(async {
            let cx = crate::agent_cx::AgentCx::for_request();
            let guard = handle
                .session()
                .session
                .lock(cx.cx())
                .await
                .expect("lock session");
            guard.path.is_none()
        });
        assert!(path_is_none);
    }

    #[test]
    fn create_agent_session_uses_working_directory_for_new_session_header_and_path() {
        let _lock = current_dir_lock();
        let process_cwd = tempdir().expect("process cwd");
        let sdk_cwd = tempdir().expect("sdk cwd");
        let session_root = tempdir().expect("session root");
        let _guard = CurrentDirGuard::new(process_cwd.path());

        let handle = run_async(create_agent_session(SessionOptions {
            provider: Some("openai".to_string()),
            model: Some("gpt-4o".to_string()),
            api_key: Some("dummy-key".to_string()),
            working_directory: Some(sdk_cwd.path().to_path_buf()),
            no_session: false,
            session_dir: Some(session_root.path().to_path_buf()),
            ..SessionOptions::default()
        }))
        .expect("create session");

        let (header_cwd, path) = run_async(async {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut guard = handle
                .session()
                .session
                .lock(cx.cx())
                .await
                .expect("lock session");
            guard.save().await.expect("save sdk session");
            (
                guard.header.cwd.clone(),
                guard.path.clone().expect("saved session path"),
            )
        });

        let expected_dir = session_root
            .path()
            .join(crate::session::encode_cwd(sdk_cwd.path()));
        let process_dir = session_root
            .path()
            .join(crate::session::encode_cwd(process_cwd.path()));

        assert_eq!(header_cwd, sdk_cwd.path().display().to_string());
        assert_eq!(path.parent(), Some(expected_dir.as_path()));
        assert_ne!(path.parent(), Some(process_dir.as_path()));
    }

    #[test]
    fn create_agent_session_resolves_relative_session_dir_against_working_directory() {
        let _lock = current_dir_lock();
        let process_cwd = tempdir().expect("process cwd");
        let sdk_cwd = tempdir().expect("sdk cwd");
        let _guard = CurrentDirGuard::new(process_cwd.path());

        let handle = run_async(create_agent_session(SessionOptions {
            provider: Some("openai".to_string()),
            model: Some("gpt-4o".to_string()),
            api_key: Some("dummy-key".to_string()),
            working_directory: Some(sdk_cwd.path().to_path_buf()),
            no_session: false,
            session_dir: Some(PathBuf::from("sessions")),
            ..SessionOptions::default()
        }))
        .expect("create session");

        let path = run_async(async {
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut guard = handle
                .session()
                .session
                .lock(cx.cx())
                .await
                .expect("lock session");
            guard.save().await.expect("save sdk session");
            guard.path.clone().expect("saved session path")
        });

        let expected_dir = sdk_cwd
            .path()
            .join("sessions")
            .join(crate::session::encode_cwd(sdk_cwd.path()));
        let process_dir = process_cwd
            .path()
            .join("sessions")
            .join(crate::session::encode_cwd(sdk_cwd.path()));

        assert_eq!(path.parent(), Some(expected_dir.as_path()));
        assert_ne!(path.parent(), Some(process_dir.as_path()));
    }

    #[test]
    fn create_agent_session_resolves_relative_session_path_against_working_directory() {
        let _lock = current_dir_lock();
        let process_cwd = tempdir().expect("process cwd");
        let sdk_cwd = tempdir().expect("sdk cwd");
        let _guard = CurrentDirGuard::new(process_cwd.path());

        let session_path = sdk_cwd.path().join("relative").join("existing.jsonl");
        std::fs::create_dir_all(session_path.parent().expect("session parent"))
            .expect("create session parent");
        let mut header = crate::session::SessionHeader::new();
        header.cwd = sdk_cwd.path().display().to_string();
        let header_json = serde_json::to_string(&header).expect("serialize session header");
        std::fs::write(&session_path, format!("{header_json}\n")).expect("write session");

        let handle = run_async(create_agent_session(SessionOptions {
            provider: Some("openai".to_string()),
            model: Some("gpt-4o".to_string()),
            api_key: Some("dummy-key".to_string()),
            working_directory: Some(sdk_cwd.path().to_path_buf()),
            no_session: false,
            session_path: Some(PathBuf::from("relative/existing.jsonl")),
            ..SessionOptions::default()
        }))
        .expect("create session");

        let opened_path = run_async(async {
            let cx = crate::agent_cx::AgentCx::for_request();
            let guard = handle
                .session()
                .session
                .lock(cx.cx())
                .await
                .expect("lock session");
            guard.path.clone().expect("opened session path")
        });

        assert_eq!(opened_path, session_path);
    }

    #[test]
    fn from_session_with_listeners_set_model_switches_provider_model() {
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
        let tools = crate::tools::ToolRegistry::new(&[], std::path::Path::new("."), None);
        let agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                system_prompt: None,
                max_tool_iterations: 50,
                stream_options: StreamOptions::default(),
                block_images: false,
                fail_closed_hooks: false,
                tool_approval: None,
            },
        );

        let mut session = Session::in_memory();
        session.header.provider = Some("anthropic".to_string());
        session.header.model_id = Some("claude-sonnet-4-5".to_string());

        let mut agent_session = AgentSession::new(
            agent,
            Arc::new(AsyncMutex::new(session)),
            false,
            ResolvedCompactionSettings::default(),
        );
        agent_session.set_model_registry(registry);
        agent_session.set_auth_storage(auth);

        let mut handle =
            AgentSessionHandle::from_session_with_listeners(agent_session, EventListeners::new());
        run_async(handle.set_model("openai", "gpt-4o")).expect("set model");
        let provider = handle.session().agent.provider();
        assert_eq!(provider.name(), "openai");
        assert_eq!(provider.model_id(), "gpt-4o");
    }

    #[test]
    fn create_agent_session_set_thinking_level_clamps_and_dedupes_history() {
        let tmp = tempdir().expect("tempdir");
        let options = SessionOptions {
            provider: Some("openai".to_string()),
            model: Some("gpt-4o".to_string()),
            api_key: Some("dummy-key".to_string()),
            working_directory: Some(tmp.path().to_path_buf()),
            no_session: true,
            ..SessionOptions::default()
        };

        let mut handle = run_async(create_agent_session(options)).expect("create session");
        run_async(handle.set_thinking_level(crate::model::ThinkingLevel::High))
            .expect("set thinking");
        run_async(handle.set_thinking_level(crate::model::ThinkingLevel::High))
            .expect("reapply thinking");

        assert_eq!(
            handle.session().agent.stream_options().thinking_level,
            Some(crate::model::ThinkingLevel::Off)
        );

        let thinking_changes = run_async(async {
            let cx = crate::agent_cx::AgentCx::for_request();
            let guard = handle
                .session()
                .session
                .lock(cx.cx())
                .await
                .expect("lock session");
            assert_eq!(guard.header.thinking_level.as_deref(), Some("off"));
            guard
                .entries_for_current_path()
                .iter()
                .filter(|entry| {
                    matches!(entry, crate::session::SessionEntry::ThinkingLevelChange(_))
                })
                .count()
        });
        assert_eq!(thinking_changes, 1);
    }

    #[test]
    fn from_session_with_listeners_set_thinking_level_uses_session_header_target() {
        let dir = tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let auth = crate::auth::AuthStorage::load(auth_path).expect("load auth");
        let mut registry = ModelRegistry::load(&auth, None);
        registry.merge_entries(vec![ModelEntry {
            model: Model {
                id: "plain-model".to_string(),
                name: "Plain Model".to_string(),
                api: "openai-completions".to_string(),
                provider: "acme".to_string(),
                base_url: "https://example.invalid/v1".to_string(),
                reasoning: false,
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
            auth_header: false,
            compat: None,
            oauth_config: None,
        }]);
        let entry = registry
            .find("anthropic", "claude-sonnet-4-5")
            .expect("anthropic model in registry");
        let provider = providers::create_provider(&entry, None).expect("create anthropic provider");
        let tools = crate::tools::ToolRegistry::new(&[], std::path::Path::new("."), None);
        let agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                system_prompt: None,
                max_tool_iterations: 50,
                stream_options: StreamOptions::default(),
                block_images: false,
                fail_closed_hooks: false,
                tool_approval: None,
            },
        );

        let mut session = Session::in_memory();
        session.header.provider = Some("acme".to_string());
        session.header.model_id = Some("plain-model".to_string());

        let mut agent_session = AgentSession::new(
            agent,
            Arc::new(AsyncMutex::new(session)),
            false,
            ResolvedCompactionSettings::default(),
        );
        agent_session.set_model_registry(registry);

        let mut handle =
            AgentSessionHandle::from_session_with_listeners(agent_session, EventListeners::new());
        run_async(handle.set_thinking_level(crate::model::ThinkingLevel::High))
            .expect("set thinking");

        assert_eq!(
            handle.session().agent.stream_options().thinking_level,
            Some(crate::model::ThinkingLevel::Off)
        );
        assert_eq!(handle.model().0, "anthropic");
        assert_eq!(handle.model().1, "claude-sonnet-4-5");
    }

    #[test]
    fn compact_without_history_is_noop() {
        let tmp = tempdir().expect("tempdir");
        let options = hermetic_session_options(tmp.path());

        let mut handle = run_async(create_agent_session(options)).expect("create session");
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_for_callback = Arc::clone(&events);
        run_async(handle.compact(move |event| {
            events_for_callback
                .lock()
                .expect("compact callback lock")
                .push(event);
        }))
        .expect("compact");

        assert!(
            events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "expected no compaction lifecycle events for empty session"
        );
    }

    #[test]
    fn resolve_path_for_cwd_uses_cwd_for_relative_paths() {
        let cwd = Path::new("/tmp/pi-sdk-cwd");
        assert_eq!(
            resolve_path_for_cwd(Path::new("relative/file.txt"), cwd),
            PathBuf::from("/tmp/pi-sdk-cwd/relative/file.txt")
        );
        assert_eq!(
            resolve_path_for_cwd(Path::new("/etc/hosts"), cwd),
            PathBuf::from("/etc/hosts")
        );
    }

    // =====================================================================
    // EventListeners tests
    // =====================================================================

    #[test]
    fn event_listeners_subscribe_and_notify() {
        let listeners = EventListeners::new();
        let received = Arc::new(Mutex::new(Vec::new()));

        let recv_clone = Arc::clone(&received);
        let id = listeners.subscribe(Arc::new(move |event| {
            recv_clone
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }));

        let event = AgentEvent::AgentStart {
            session_id: "test-123".into(),
        };
        listeners.notify(&event);

        let events = received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(events.len(), 1);

        // Verify unsubscribe
        drop(events);
        assert!(listeners.unsubscribe(id));
        listeners.notify(&AgentEvent::AgentStart {
            session_id: "test-456".into(),
        });
        assert_eq!(
            received
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
    }

    #[test]
    fn event_listeners_unsubscribe_nonexistent_returns_false() {
        let listeners = EventListeners::new();
        assert!(!listeners.unsubscribe(SubscriptionId(999)));
    }

    #[test]
    fn event_listeners_multiple_subscribers() {
        let listeners = EventListeners::new();
        let count_a = Arc::new(Mutex::new(0u32));
        let count_b = Arc::new(Mutex::new(0u32));

        let ca = Arc::clone(&count_a);
        listeners.subscribe(Arc::new(move |_| {
            *ca.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        }));

        let cb = Arc::clone(&count_b);
        listeners.subscribe(Arc::new(move |_| {
            *cb.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        }));

        listeners.notify(&AgentEvent::AgentStart {
            session_id: "s".into(),
        });

        assert_eq!(
            *count_a
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            1
        );
        assert_eq!(
            *count_b
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            1
        );
    }

    #[test]
    fn event_listeners_tool_hooks_fire() {
        let listeners = EventListeners::new();
        let starts = Arc::new(Mutex::new(Vec::new()));
        let ends = Arc::new(Mutex::new(Vec::new()));

        let s = Arc::clone(&starts);
        let mut listeners = listeners;
        listeners.on_tool_start = Some(Arc::new(move |name, args| {
            s.lock()
                .expect("lock")
                .push((name.to_string(), args.clone()));
        }));

        let e = Arc::clone(&ends);
        listeners.on_tool_end = Some(Arc::new(move |name, _output, is_error| {
            e.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((name.to_string(), is_error));
        }));

        let args = serde_json::json!({"path": "/foo"});
        listeners.notify_tool_start("bash", &args);
        let output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("ok"))],
            details: None,
            is_error: false,
        };
        listeners.notify_tool_end("bash", &output, false);

        {
            let s = starts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(s.len(), 1);
            assert_eq!(s[0].0, "bash");
            drop(s);
        }

        {
            let e = ends
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(e.len(), 1);
            assert_eq!(e[0].0, "bash");
            assert!(!e[0].1);
            drop(e);
        }
    }

    #[test]
    fn event_listeners_stream_event_hook_fires() {
        let mut listeners = EventListeners::new();
        let received = Arc::new(Mutex::new(Vec::new()));

        let r = Arc::clone(&received);
        listeners.on_stream_event = Some(Arc::new(move |ev| {
            r.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("{ev:?}"));
        }));

        let event = StreamEvent::TextDelta {
            content_index: 0,
            delta: "hello".to_string(),
        };
        listeners.notify_stream_event(&event);

        assert_eq!(
            received
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
    }

    #[test]
    fn session_options_on_event_wired_into_listeners() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let r = Arc::clone(&received);
        let tmp = tempdir().expect("tempdir");

        let options = SessionOptions {
            on_event: Some(Arc::new(move |event| {
                r.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(format!("{event:?}"));
            })),
            ..hermetic_session_options(tmp.path())
        };

        let handle = run_async(create_agent_session(options)).expect("create session");
        // Verify the listener was registered
        let count = handle
            .listeners()
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert_eq!(
            count, 1,
            "on_event from SessionOptions should register one subscriber"
        );
    }

    #[test]
    fn subscribe_unsubscribe_on_handle() {
        let tmp = tempdir().expect("tempdir");
        let options = hermetic_session_options(tmp.path());

        let handle = run_async(create_agent_session(options)).expect("create session");
        let id = handle.subscribe(|_event| {});
        assert_eq!(
            handle
                .listeners()
                .subscribers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );

        assert!(handle.unsubscribe(id));
        assert_eq!(
            handle
                .listeners()
                .subscribers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            0
        );

        // Double unsubscribe returns false
        assert!(!handle.unsubscribe(id));
    }

    #[test]
    fn stream_event_from_assistant_message_event_converts_text_delta() {
        use crate::model::AssistantMessageEvent as AME;

        let partial = Arc::new(AssistantMessage {
            content: Vec::new(),
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        });
        let ame = AME::TextDelta {
            content_index: 2,
            delta: "chunk".to_string(),
            partial,
        };
        let result = stream_event_from_assistant_message_event(&ame);
        assert!(result.is_some());
        match result.unwrap() {
            StreamEvent::TextDelta {
                content_index,
                delta,
            } => {
                assert_eq!(content_index, 2);
                assert_eq!(delta, "chunk");
            }
            other => unreachable!("expected TextDelta, got {other:?}"),
        }
    }

    #[test]
    fn stream_event_from_assistant_message_event_start_returns_none() {
        use crate::model::AssistantMessageEvent as AME;

        let partial = Arc::new(AssistantMessage {
            content: Vec::new(),
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        });
        let ame = AME::Start { partial };
        assert!(stream_event_from_assistant_message_event(&ame).is_none());
    }

    #[test]
    fn event_listeners_debug_impl() {
        let listeners = EventListeners::new();
        let debug = format!("{listeners:?}");
        assert!(debug.contains("subscriber_count"));
        assert!(debug.contains("has_on_tool_start"));
    }

    // =====================================================================
    // Extension convenience method tests
    // =====================================================================

    #[test]
    fn has_extensions_false_by_default() {
        let tmp = tempdir().expect("tempdir");
        let options = hermetic_session_options(tmp.path());

        let handle = run_async(create_agent_session(options)).expect("create session");
        assert!(
            !handle.has_extensions(),
            "session without extension_paths should have no extensions"
        );
        assert!(handle.extension_manager().is_none());
        assert!(handle.extension_region().is_none());
    }

    // =====================================================================
    // Tool factory function tests
    // =====================================================================

    #[test]
    fn create_read_tool_has_correct_name() {
        let tmp = tempdir().expect("tempdir");
        let tool = super::create_read_tool(tmp.path());
        assert_eq!(tool.name(), "read");
        assert!(!tool.description().is_empty());
        let params = tool.parameters();
        assert!(params.is_object(), "parameters should be a JSON object");
    }

    #[test]
    fn create_bash_tool_has_correct_name() {
        let tmp = tempdir().expect("tempdir");
        let tool = super::create_bash_tool(tmp.path());
        assert_eq!(tool.name(), "bash");
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn create_edit_tool_has_correct_name() {
        let tmp = tempdir().expect("tempdir");
        let tool = super::create_edit_tool(tmp.path());
        assert_eq!(tool.name(), "edit");
    }

    #[test]
    fn create_write_tool_has_correct_name() {
        let tmp = tempdir().expect("tempdir");
        let tool = super::create_write_tool(tmp.path());
        assert_eq!(tool.name(), "write");
    }

    #[test]
    fn create_grep_tool_has_correct_name() {
        let tmp = tempdir().expect("tempdir");
        let tool = super::create_grep_tool(tmp.path());
        assert_eq!(tool.name(), "grep");
    }

    #[test]
    fn create_find_tool_has_correct_name() {
        let tmp = tempdir().expect("tempdir");
        let tool = super::create_find_tool(tmp.path());
        assert_eq!(tool.name(), "find");
    }

    #[test]
    fn create_ls_tool_has_correct_name() {
        let tmp = tempdir().expect("tempdir");
        let tool = super::create_ls_tool(tmp.path());
        assert_eq!(tool.name(), "ls");
    }

    #[test]
    fn create_all_tools_returns_eight() {
        let tmp = tempdir().expect("tempdir");
        let tools = super::create_all_tools(tmp.path());
        assert_eq!(tools.len(), 8, "should create all 8 built-in tools");

        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        for expected in BUILTIN_TOOL_NAMES {
            assert!(names.contains(expected), "missing tool: {expected}");
        }
    }

    #[test]
    fn tool_to_definition_preserves_schema() {
        let tmp = tempdir().expect("tempdir");
        let tool = super::create_read_tool(tmp.path());
        let def = super::tool_to_definition(tool.as_ref());
        assert_eq!(def.name, "read");
        assert!(!def.description.is_empty());
        assert!(def.parameters.is_object());
        assert!(
            def.parameters.get("properties").is_some(),
            "schema should have properties"
        );
    }

    #[test]
    fn all_tool_definitions_returns_eight_schemas() {
        let tmp = tempdir().expect("tempdir");
        let defs = super::all_tool_definitions(tmp.path());
        assert_eq!(defs.len(), 8);

        for def in &defs {
            assert!(!def.name.is_empty());
            assert!(!def.description.is_empty());
            assert!(def.parameters.is_object());
        }
    }

    #[test]
    fn builtin_tool_names_matches_create_all() {
        let tmp = tempdir().expect("tempdir");
        let tools = super::create_all_tools(tmp.path());
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            names.as_slice(),
            BUILTIN_TOOL_NAMES,
            "create_all_tools order should match BUILTIN_TOOL_NAMES"
        );
    }

    #[test]
    fn tool_registry_from_factory_tools() {
        let tmp = tempdir().expect("tempdir");
        let tools = super::create_all_tools(tmp.path());
        let registry = ToolRegistry::from_tools(tools);
        assert!(registry.get("read").is_some());
        assert!(registry.get("bash").is_some());
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn set_session_name_records_session_info_entry() {
        let tmp = tempdir().expect("tempdir");
        let options = hermetic_session_options(tmp.path());

        let mut handle = run_async(create_agent_session(options)).expect("create session");
        run_async(handle.set_session_name("renamed-by-sdk")).expect("set session name");

        let info_entries = run_async(async {
            let cx = crate::agent_cx::AgentCx::for_request();
            let guard = handle
                .session()
                .session
                .lock(cx.cx())
                .await
                .expect("lock session");
            guard
                .entries_for_current_path()
                .iter()
                .filter_map(|entry| match entry {
                    crate::session::SessionEntry::SessionInfo(info) => info.name.clone(),
                    _ => None,
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(info_entries, vec!["renamed-by-sdk".to_string()]);
    }

    #[test]
    fn max_tokens_defaults_to_registry_value_and_set_overrides() {
        let tmp = tempdir().expect("tempdir");
        let options = hermetic_session_options(tmp.path());

        let mut handle = run_async(create_agent_session(options)).expect("create session");
        // The session now seeds `max_tokens` from the model registry's
        // configured `maxTokens` so the value users set in `models.json`
        // actually takes effect (instead of falling back to the provider's
        // hardcoded per-request default).
        let seeded = handle.max_tokens();
        assert!(
            seeded.is_some_and(|n| n > 0),
            "expected max_tokens to be seeded from the registry, got {seeded:?}"
        );

        handle.set_max_tokens(Some(32_000));
        assert_eq!(handle.max_tokens(), Some(32_000));
        assert_eq!(
            handle.session().agent.stream_options().max_tokens,
            Some(32_000)
        );

        // Embedders can still fall back to the provider default explicitly.
        handle.set_max_tokens(None);
        assert_eq!(handle.max_tokens(), None);
    }
}
