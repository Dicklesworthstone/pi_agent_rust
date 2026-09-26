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

mod extension_bootstrap;
mod recovery;

use crate::app;
use crate::auth::AuthStorage;
use crate::cli::Cli;
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub use crate::agent::{
    AbortHandle, AbortSignal, Agent, AgentConfig, AgentEvent, AgentSession, QueueMode,
};
pub use crate::compaction::ResolvedCompactionSettings;
pub use crate::config::Config;
pub use crate::error::{Error, Result};
pub use crate::extension_dispatcher::ExtensionUiHandler;
pub use crate::extensions::{
    ExtensionManager, ExtensionPolicy, ExtensionRegion, ExtensionUiRequest, ExtensionUiResponse,
};
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

/// MCP discovery inputs for one SDK-owned session.
#[derive(Debug, Clone, Default)]
pub struct McpSessionOptions {
    /// Explicit MCP configuration files, ordered after discovered project and
    /// global configuration in the same way as CLI `--mcp-config` values.
    pub config_paths: Vec<PathBuf>,
    /// Optional global directory override. Embedders and tests can keep trust
    /// state isolated; normal CLI callers use [`Config::global_dir`].
    pub global_dir: Option<PathBuf>,
}

/// What a session needs to walk a configured fallback chain.
///
/// Print mode and the RPC server each resolve these from their own plumbing —
/// `FailoverResolution` and `RpcOptions` respectively. An SDK session has
/// neither, so an embedder supplies them here (bd-u2qv4).
#[derive(Clone)]
pub struct FailoverOptions {
    /// `retry.fallbackChains`: a role name or exact `provider/model` spec
    /// mapped to the ordered fallbacks that follow it.
    pub chains: std::collections::HashMap<String, Vec<String>>,
    /// Models a chain spec can resolve against. A well-formed spec that names
    /// nothing here still resolves to an ad-hoc entry.
    pub available_models: Vec<crate::models::ModelEntry>,
    /// Credential store consulted per candidate. An entry with no usable
    /// credential is skipped rather than installed: failing over into an auth
    /// error is strictly worse than the quota error that started it.
    pub auth: crate::auth::AuthStorage,
    /// An explicit `--api-key` equivalent, which pins and never rotates.
    pub cli_api_key: Option<String>,
    /// Seconds before the primary may be used again after a swap.
    pub cooldown_secs: u64,
}

impl FailoverOptions {
    /// Read the chain configuration out of a [`Config`], with the models and
    /// credentials the caller already has.
    ///
    /// `None` when no fallback chain is configured, which is the same condition
    /// under which both existing surfaces decline to fail over.
    #[must_use]
    pub fn from_config(
        config: &Config,
        available_models: Vec<crate::models::ModelEntry>,
        auth: crate::auth::AuthStorage,
        cli_api_key: Option<String>,
    ) -> Option<Self> {
        let chains = config
            .retry
            .as_ref()
            .and_then(|retry| retry.fallback_chains.as_ref())?
            .clone();
        Some(Self {
            chains,
            available_models,
            auth,
            cli_api_key,
            cooldown_secs: config.failover_cooldown_secs(),
        })
    }
}

/// SDK session construction options.
///
/// These options provide the programmatic equivalent of the core CLI startup
/// path used in `src/main.rs`.
// Independent on/off toggles mirroring CLI flags; an enum or builder state
// machine would obscure the flag-to-field mapping.
#[allow(clippy::struct_excessive_bools)]
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
    /// Whether project-local configuration under the working directory may
    /// be loaded. Programmatic callers default to fail-closed; CLI hosts pass
    /// the decision produced by `workspace_trust::establish`.
    pub workspace_trusted: bool,
    pub no_session: bool,
    pub session_path: Option<PathBuf>,
    pub session_dir: Option<PathBuf>,
    /// Optional override for the package directory (`PI_PACKAGE_DIR`
    /// equivalent) used for prompt documentation discovery. Relative values
    /// are resolved against the session's working directory so advertised
    /// paths stay readable through the same roots the tools use (bd-jtehj).
    pub package_dir: Option<PathBuf>,
    pub extension_paths: Vec<PathBuf>,
    pub extension_policy: Option<String>,
    pub repair_policy: Option<String>,
    /// Pre-parsed extension CLI flags to apply to this session's live runtime
    /// after extension registration and before dependent startup bridges.
    pub extension_flags: Vec<crate::cli::ExtensionCliFlag>,
    pub include_cwd_in_prompt: bool,
    /// The "available skills" block for the system prompt, as the CLI host
    /// renders it from its resource loader (`--no-skills`, trust and the
    /// read-tool rule applied). `None` lists no skills, which is what an
    /// embedder that loads no skills gets.
    pub skills_prompt: Option<String>,
    pub max_tool_iterations: usize,

    /// Provider retry for turns driven through this session.
    ///
    /// `None` — the default — returns a transient provider failure to the
    /// caller unchanged. `Some(policy)` applies to ordinary and abort-aware
    /// prompts and continuations alike, including `SessionTransport::InProcess`.
    ///
    /// The shared policy in [`crate::failover`] is also used by print mode and
    /// RPC. [`crate::failover::RetryPolicy::from_config`] reads it from
    /// configuration and yields `None` when the user has turned retry off.
    ///
    /// Retry alone. Walking a configured fallback CHAIN additionally needs
    /// [`SessionOptions::failover`].
    pub retry: Option<crate::failover::RetryPolicy>,

    /// Cross-model failover for turns driven through this session.
    ///
    /// `None` — the default — leaves a configured `retry.fallbackChains` inert,
    /// which is what both interactive stacks did: the chain the user configured
    /// never ran on the surface they configured it for, while the identical
    /// request in print mode or over RPC walked it (bd-u2qv4).
    ///
    /// Requires [`SessionOptions::retry`] as well. Failover happens when the
    /// same-provider retry budget is spent, so with no retry policy there is no
    /// point at which the chain would be consulted.
    pub failover: Option<FailoverOptions>,

    /// Opt in to MCP discovery for this SDK session.
    ///
    /// The manager is constructed inside [`create_agent_session`] so the
    /// session's own extension runtime can register contributed servers before
    /// the single connect-and-mount pass. This avoids cross-wiring tools from
    /// a different, already-dropped session (bd-vjfol).
    pub mcp: Option<McpSessionOptions>,
    /// The advisor (bd-cv653.3.3): a second model that reviews each turn.
    /// `None` — the default — runs no advisor. Each session built from these
    /// options gets its own runtime (fresh guard and failure count).
    pub advisor: Option<crate::advisor::AdvisorOptions>,
    /// Runtime this session dispatches background work on.
    ///
    /// Required for extension observation events to reach extensions at all:
    /// `EventCoalescer` spawns its batched dispatch onto a runtime, so without
    /// a handle the SDK path can build a coalescer that never fires. That was
    /// bd-82331 — the default interactive stack delivered lifecycle events and
    /// nothing else, silently, because no surface on this path supplied one.
    ///
    /// Omitting it while extensions are loaded makes
    /// [`create_agent_session`] build a runtime for the session and say so at
    /// debug on `pi::sdk`, so observation events still arrive (bd-8rvry). That
    /// runtime lives on the returned handle and is shut down with it. Supply
    /// one whenever the host already has a runtime: sharing it is cheaper than
    /// the four worker threads the fallback starts, and it keeps extension
    /// dispatch on the same executor as the rest of the host's work.
    ///
    /// Omitting it with no extensions loaded costs nothing and is the normal
    /// case for an embedder that does not use them.
    pub runtime_handle: Option<asupersync::runtime::RuntimeHandle>,

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

    /// Optional multi-root workspace handle (bd-cv653.3.12). When set, the
    /// session's tool registry confines paths to primary + additional roots
    /// and `/add-dir` // `/remove-dir` mutate the shared set live.
    pub workspace: Option<crate::workspace::WorkspaceHandle>,

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

    /// Optional UI handler bridging extension UI requests (including
    /// capability prompts) to the host application.
    ///
    /// In interactive mode the TUI answers these prompts; without a handler,
    /// SDK sessions keep the historical fail-closed behavior: extension UI
    /// requests error out and capability prompts resolve to deny.
    ///
    /// Only consulted when [`SessionOptions::extension_paths`] loads at least
    /// one extension. A capability prompt arrives as a `"confirm"` request;
    /// respond with `value: Value::Bool(allow)` for the default persistence
    /// behavior, or `value: json!({"allow": bool, "persist": bool})` to
    /// control whether the decision is persisted across sessions
    /// (`persist: false` keeps it scoped to this session).
    pub extension_ui_handler: Option<Arc<dyn ExtensionUiHandler>>,

    /// Whether extension capability prompt decisions are persisted to the
    /// on-disk permission store (`~/.pi/extension-permissions.json`).
    ///
    /// `true` (the default) matches the CLI/TUI behavior. `false` scopes all
    /// prompt decisions for this session to the in-memory cache, unless an
    /// individual handler response overrides with `persist: true`.
    pub persist_extension_permissions: bool,

    /// Optional per-session compaction settings override.
    ///
    /// When `Some`, the settings are used verbatim for this session. When
    /// `None` (the default), settings derive from the global config and the
    /// selected model's context window exactly as before. Either way the
    /// settings are fixed at session creation: a later
    /// [`AgentSessionHandle::set_model`] does not re-derive them (in
    /// particular, `context_window_tokens` is not updated to the new model's
    /// window).
    pub compaction_settings: Option<ResolvedCompactionSettings>,

    /// Graduated approval gating for this session (issue #196).
    ///
    /// When `Some`, tool calls are evaluated against the mode
    /// (`ask`/`write`/`yolo`) exactly as in the classic CLI stack. Calls that
    /// require approval are routed to [`SessionOptions::tool_approval`] when
    /// set; otherwise, when the session's ask tool is enabled, an approval
    /// card is bridged through the interactive ask surface
    /// ([`crate::ask::approval_handler_via_ask`]). With neither available the
    /// call is denied with an explicit reason (fail closed).
    ///
    /// `None` (the default) preserves the historical SDK behavior: no
    /// approval gating.
    pub approval_state: Option<crate::approval::ApprovalState>,

    /// Explicit tool-approval prompt handler.
    ///
    /// Takes precedence over the ask-surface bridge that
    /// [`SessionOptions::approval_state`] would otherwise install. Only
    /// consulted for calls that `approval_state` gates (or, matching
    /// `AgentConfig` semantics, for every tool call when `approval_state` is
    /// `None`).
    pub tool_approval: Option<crate::agent::ToolApprovalHandler>,
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
            workspace_trusted: false,
            package_dir: None,
            no_session: true,
            session_path: None,
            session_dir: None,
            extension_paths: Vec::new(),
            extension_policy: None,
            extension_flags: Vec::new(),
            tool_factory: None,
            workspace: None,
            repair_policy: None,
            include_cwd_in_prompt: true,
            skills_prompt: None,
            max_tool_iterations: crate::agent::resolved_max_tool_iterations_default(),
            retry: None,
            failover: None,
            mcp: None,
            advisor: None,
            runtime_handle: None,
            on_event: None,
            on_tool_start: None,
            on_tool_end: None,
            on_stream_event: None,
            extension_ui_handler: None,
            persist_extension_permissions: true,
            compaction_settings: None,
            approval_state: None,
            tool_approval: None,
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
    /// Ask tool handle when the session enabled it (bd-cv653.3.8). `AskTool`
    /// is `Clone` over shared state, so a host can install a channel UI and
    /// resolve pending cards via `respond_ui` — without it the tool falls
    /// back to its non-interactive policy.
    ask_tool: Option<crate::ask::AskTool>,
    /// Multi-root workspace handle when the session was created with one
    /// (bd-cv653.3.12). Clones share the live root set.
    workspace: Option<crate::workspace::WorkspaceHandle>,
    /// MCP manager owned by this exact SDK session, when enabled.
    mcp_manager: Option<Arc<crate::mcp::McpManager>>,
    /// Provider retry policy for turns driven through this handle, from
    /// [`SessionOptions::retry`]. `None` is the historical behaviour: a
    /// transient failure goes straight back to the caller.
    retry: Option<crate::failover::RetryPolicy>,
    /// Chain configuration for turns driven through this handle, from
    /// [`SessionOptions::failover`]. Behind an `Arc` so a turn can hold it
    /// while the session itself is mutated by a swap.
    failover: Option<Arc<FailoverOptions>>,
    /// Where the chain walk left off, what to return to, and when the cooldown
    /// on returning started. Per-process, matching both existing surfaces.
    failover_state: crate::failover::FailoverState,
    /// Runtime this session built for itself because extensions were loaded and
    /// the embedder supplied no [`SessionOptions::runtime_handle`] (bd-8rvry).
    ///
    /// Held only to keep it alive: the session uses it through the handle
    /// installed on [`AgentSession`]. `None` is the common case — either the
    /// embedder supplied a runtime, or no extension is loaded to observe with.
    /// Dropping this handle shuts it down with everything else it owns.
    event_runtime: Option<asupersync::runtime::Runtime>,
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
    InProcess(Box<AgentEvent>),
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
    /// Estimated tokens the next provider request will see after this
    /// compaction is applied. Additive/backward-compatible: defaults to `0`
    /// when deserializing payloads emitted before this field existed.
    #[serde(default)]
    pub tokens_after: u64,
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
const RPC_MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
const RPC_MAX_PRE_ACK_EVENTS: usize = 256;
const RPC_MAX_PRE_ACK_BYTES: usize = 4 * 1024 * 1024;

pub struct RpcTransportClient {
    child: Child,
    stdin: Arc<Mutex<BufWriter<ChildStdin>>>,
    stdout: BufReader<ChildStdout>,
    next_request_id: Arc<AtomicU64>,
}

/// Write-only control lane for a running RPC prompt.
///
/// The prompt task remains the sole stdout reader. A cloned control handle may
/// be used from another thread or from a live event callback to dispatch steer,
/// follow-up, or abort requests while that prompt is still active. Dispatch
/// success means the command was written and flushed, not that the subprocess
/// acknowledged or completed it.
#[derive(Clone)]
pub struct RpcControlHandle {
    stdin: Arc<Mutex<BufWriter<ChildStdin>>>,
    next_request_id: Arc<AtomicU64>,
}

impl std::fmt::Debug for RpcControlHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcControlHandle").finish_non_exhaustive()
    }
}

impl RpcControlHandle {
    fn send(&self, command: &str, payload: Map<String, Value>) -> Result<String> {
        let request_id = next_rpc_request_id(&self.next_request_id)?;
        let mut frame = Map::new();
        frame.insert("type".to_string(), Value::String(command.to_string()));
        frame.insert("id".to_string(), Value::String(request_id.clone()));
        frame.extend(payload);
        write_rpc_json_line(&self.stdin, &Value::Object(frame))?;
        Ok(request_id)
    }

    pub fn steer(&self, message: impl Into<String>) -> Result<String> {
        let mut payload = Map::new();
        payload.insert("message".to_string(), Value::String(message.into()));
        self.send("steer", payload)
    }

    pub fn follow_up(&self, message: impl Into<String>) -> Result<String> {
        let mut payload = Map::new();
        payload.insert("message".to_string(), Value::String(message.into()));
        self.send("follow_up", payload)
    }

    pub fn abort(&self) -> Result<String> {
        self.send("abort", Map::new())
    }
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
                        (on_event)(SessionTransportEvent::InProcess(Box::new(event)));
                    })
                    .await?;
                Ok(SessionPromptResult::InProcess(Box::new(assistant)))
            }
            Self::RpcSubprocess(client) => {
                let callback = Arc::clone(&on_event);
                let events = client
                    .prompt_with_options_streaming(input, None, None, move |event| {
                        (callback)(SessionTransportEvent::Rpc(event));
                    })
                    .await?;
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
            stdin: Arc::new(Mutex::new(BufWriter::new(stdin))),
            stdout: BufReader::new(stdout),
            next_request_id: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Clone a write-only lane that remains usable while `prompt*` borrows
    /// this client for its single stdout reader.
    #[must_use]
    pub fn control_handle(&self) -> RpcControlHandle {
        RpcControlHandle {
            stdin: Arc::clone(&self.stdin),
            next_request_id: Arc::clone(&self.next_request_id),
        }
    }

    #[allow(
        clippy::unused_async,
        reason = "SDK RPC transport keeps an async public API"
    )]
    pub async fn request(&mut self, command: &str, payload: Map<String, Value>) -> Result<Value> {
        if payload.contains_key("type") || payload.contains_key("id") {
            return Err(Error::validation(
                "RPC request payload cannot override reserved type/id fields",
            ));
        }
        let request_id = self.next_request_id()?;
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
        request_generation: u64,
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
        payload.insert(
            "requestGeneration".to_string(),
            Value::from(request_generation),
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

    pub async fn prompt_with_options(
        &mut self,
        message: impl Into<String>,
        images: Option<Vec<ImageContent>>,
        streaming_behavior: Option<&str>,
    ) -> Result<Vec<Value>> {
        self.prompt_with_options_streaming(message, images, streaming_behavior, |_| {})
            .await
    }

    /// Run one RPC prompt while delivering raw events as soon as they are read.
    ///
    /// Some servers can emit lifecycle events before the prompt response is
    /// flushed. Those events are retained under explicit count/byte bounds and
    /// released in order once the matching success acknowledgement arrives.
    /// A failed acknowledgement never leaks speculative events to the caller.
    #[allow(
        clippy::unused_async,
        reason = "SDK RPC transport keeps an async public API"
    )]
    pub async fn prompt_with_options_streaming(
        &mut self,
        message: impl Into<String>,
        images: Option<Vec<ImageContent>>,
        streaming_behavior: Option<&str>,
        mut on_event: impl FnMut(Value),
    ) -> Result<Vec<Value>> {
        let request_id = self.next_request_id()?;
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
        self.write_json_line(&Value::Object(payload))?;

        let mut saw_ack = false;
        let mut events = Vec::new();
        // Annotated, not inferred: the first USE of the element type is
        // `event.get("type")` in the drain loop below, and the only thing that
        // would constrain it is the `pre_ack.push(item)` further down. Method
        // resolution does not wait, so without this the crate does not compile
        // (E0282).
        let mut pre_ack: Vec<Value> = Vec::new();
        let mut pre_ack_bytes = 0usize;
        loop {
            let item = self.read_json_line()?;
            let item_type = item.get("type").and_then(Value::as_str);
            if item_type == Some("response") {
                if item.get("id").and_then(Value::as_str) != Some(request_id.as_str()) {
                    continue;
                }
                if item.get("command").and_then(Value::as_str) != Some("prompt") {
                    return Err(Error::api(
                        "RPC prompt acknowledgement used the matching id with the wrong command",
                    ));
                }
                let success = item
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !success {
                    return Err(rpc_error_from_response(&item, "prompt"));
                }
                if saw_ack {
                    return Err(Error::api("RPC prompt sent a duplicate acknowledgement"));
                }
                saw_ack = true;
                // `mem::take` rather than `drain(..)`: same result (the buffer
                // is left empty and refilled by later iterations) and it is
                // what `clippy::iter_with_drain` asks for.
                for event in std::mem::take(&mut pre_ack) {
                    let reached_end =
                        event.get("type").and_then(Value::as_str) == Some("agent_end");
                    on_event(event.clone());
                    events.push(event);
                    if reached_end {
                        return Ok(events);
                    }
                }
                continue;
            }

            if !saw_ack {
                let encoded_len = serde_json::to_vec(&item)
                    .map_err(|err| Error::Json(Box::new(err)))?
                    .len();
                if pre_ack.len() >= RPC_MAX_PRE_ACK_EVENTS
                    || encoded_len > RPC_MAX_PRE_ACK_BYTES.saturating_sub(pre_ack_bytes)
                {
                    return Err(Error::api(
                        "RPC prompt emitted too many events before its acknowledgement",
                    ));
                }
                pre_ack_bytes += encoded_len;
                pre_ack.push(item);
                continue;
            }

            let reached_end = item_type == Some("agent_end");
            on_event(item.clone());
            events.push(item);
            if reached_end {
                return Ok(events);
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

    fn next_request_id(&self) -> Result<String> {
        next_rpc_request_id(&self.next_request_id)
    }

    fn write_json_line(&self, payload: &Value) -> Result<()> {
        write_rpc_json_line(&self.stdin, payload)
    }

    fn read_json_line(&mut self) -> Result<Value> {
        let mut line = Vec::new();
        loop {
            let available = self
                .stdout
                .fill_buf()
                .map_err(|err| Error::Io(Box::new(err)))?;
            if available.is_empty() {
                if line.is_empty() {
                    return Err(Error::api(
                        "RPC subprocess exited before sending a response",
                    ));
                }
                return Err(Error::api(
                    "RPC subprocess ended in the middle of a JSON line",
                ));
            }
            if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
                if newline > RPC_MAX_LINE_BYTES.saturating_sub(line.len()) {
                    let _ = self.shutdown();
                    return Err(Error::api("RPC subprocess JSON line exceeded 8 MiB"));
                }
                line.extend_from_slice(&available[..newline]);
                self.stdout.consume(newline + 1);
                break;
            }
            if available.len() > RPC_MAX_LINE_BYTES.saturating_sub(line.len()) {
                let _ = self.shutdown();
                return Err(Error::api("RPC subprocess JSON line exceeded 8 MiB"));
            }
            let available_len = available.len();
            line.extend_from_slice(available);
            self.stdout.consume(available_len);
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        serde_json::from_slice(&line).map_err(|err| Error::Json(Box::new(err)))
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

fn next_rpc_request_id(counter: &AtomicU64) -> Result<String> {
    let id = counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .map_err(|_| Error::api("RPC request id space exhausted"))?;
    Ok(format!("rpc-{id}"))
}

fn write_rpc_json_line(stdin: &Mutex<BufWriter<ChildStdin>>, payload: &Value) -> Result<()> {
    let encoded = serde_json::to_string(payload).map_err(|err| Error::Json(Box::new(err)))?;
    let mut stdin = stdin
        .lock()
        .map_err(|_| Error::api("RPC subprocess stdin writer lock poisoned"))?;
    stdin
        .write_all(encoded.as_bytes())
        .map_err(|err| Error::Io(Box::new(err)))?;
    stdin
        .write_all(b"\n")
        .map_err(|err| Error::Io(Box::new(err)))?;
    stdin.flush().map_err(|err| Error::Io(Box::new(err)))
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

/// Proof that replacement preflight completed without mutating runtime
/// resources. The caller must install the prepared candidate before consuming
/// the old handle with `commit_resource_shutdown`.
pub(crate) struct PreparedSessionShutdown {
    owner_session_id: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum McpShutdownOutcome {
    #[default]
    NotOwned,
    Released,
    Indeterminate,
}

/// Outcome of exhaustively stopping resources owned by one SDK session handle.
#[derive(Debug, Default)]
pub struct SessionResourceShutdown {
    failures: Vec<String>,
    mcp: McpShutdownOutcome,
}

const SESSION_MCP_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl SessionResourceShutdown {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.failures.is_empty()
    }

    #[must_use]
    pub const fn completed_cleanly(&self) -> bool {
        self.failures.is_empty()
    }

    /// A replacement may start its MCP manager only after the previous
    /// manager's absence or complete release has been established.
    #[must_use]
    pub(crate) const fn permits_replacement_mcp_activation(&self) -> bool {
        matches!(
            self.mcp,
            McpShutdownOutcome::NotOwned | McpShutdownOutcome::Released
        )
    }

    pub fn failures(&self) -> impl Iterator<Item = &str> {
        self.failures.iter().map(String::as_str)
    }

    pub fn messages(&self) -> impl Iterator<Item = &str> {
        self.failures.iter().map(String::as_str)
    }

    pub(crate) fn fail(&mut self, message: String) {
        self.failures.push(message);
    }
}

impl AgentSessionHandle {
    /// Create a handle from a pre-built `AgentSession` with custom listeners.
    ///
    /// This is useful for tests and advanced embedding scenarios where
    /// the full `create_agent_session()` flow is not needed.
    ///
    /// No fallback event runtime is provisioned here, unlike
    /// [`create_agent_session`]: the caller built the `AgentSession` itself and
    /// owns the decision of what it dispatches on. If it carries extensions
    /// that should observe, install a runtime on it with
    /// `AgentSession::with_runtime_handle` first (bd-8rvry).
    pub const fn from_session_with_listeners(
        session: AgentSession,
        listeners: EventListeners,
    ) -> Self {
        Self {
            session,
            listeners,
            ask_tool: None,
            workspace: None,
            mcp_manager: None,
            // A handle built straight from a session has no options to read a
            // policy from; the caller opts in through `create_agent_session`.
            retry: None,
            failover: None,
            failover_state: crate::failover::FailoverState::new_empty(),
            event_runtime: None,
        }
    }

    /// Host picker handle for sessions built through [`create_agent_session`].
    /// Cloning is cheap shared state. It exists even when the model-facing
    /// `ask` tool is disabled, so permission prompts remain reachable.
    #[must_use]
    pub fn ask_tool(&self) -> Option<crate::ask::AskTool> {
        self.ask_tool.clone()
    }

    /// Multi-root workspace handle when the session was created with one
    /// (bd-cv653.3.12). Clones share the live root set.
    #[must_use]
    pub fn workspace(&self) -> Option<crate::workspace::WorkspaceHandle> {
        self.workspace.clone()
    }

    /// MCP manager owned by this session, when MCP discovery was enabled.
    #[must_use]
    pub fn mcp_manager(&self) -> Option<Arc<crate::mcp::McpManager>> {
        self.mcp_manager.clone()
    }

    /// Permanently remove this deferred session's MCP manager when the
    /// predecessor's singleton transports were not proven released.
    pub(crate) fn disable_mcp(&mut self) {
        self.mcp_manager = None;
    }

    /// Flush durable state and capture session ownership without stopping any
    /// runtime resource. On success the caller can synchronously install its
    /// prepared replacement before cleanup reaches an irreversible step.
    // `&mut self` is deliberate: the preflight must hold exclusive access to
    // the handle while ownership is captured, even though no field is mutated.
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(crate) async fn preflight_replacement(
        &mut self,
    ) -> std::result::Result<PreparedSessionShutdown, SessionResourceShutdown> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut session = match asupersync::sync::OwnedMutexGuard::lock(
            Arc::clone(&self.session.session),
            cx.cx(),
        )
        .await
        {
            Ok(session) => session,
            Err(err) => {
                let warning = format!("failed to lock session for resource shutdown: {err}");
                tracing::warn!(
                    event = "sdk.session.shutdown.jobs_owner_failed",
                    "{warning}"
                );
                let mut report = SessionResourceShutdown::default();
                report.fail(warning);
                return Err(report);
            }
        };
        let owner_session_id = session.header.id.clone();
        if self.session.save_enabled()
            && let Err(err) = session.flush_autosave_on_shutdown().await
        {
            let warning = format!("failed to flush session autosave: {err}");
            tracing::warn!(event = "sdk.session.shutdown.autosave_failed", "{warning}");
            let mut report = SessionResourceShutdown::default();
            report.fail(warning);
            return Err(report);
        }
        Ok(PreparedSessionShutdown { owner_session_id })
    }

    async fn shutdown_runtime_resources(
        self,
        owner_session_id: Option<&str>,
        mut report: SessionResourceShutdown,
    ) -> SessionResourceShutdown {
        if let Some(owner_session_id) = owner_session_id
            && let Err(err) = crate::jobs::kill_session(owner_session_id).await
        {
            let warning = format!("failed to stop session-owned background jobs: {err}");
            tracing::warn!(event = "sdk.session.shutdown.jobs_failed", "{warning}");
            report.fail(warning);
        }

        if let Some(region) = self.session.extensions.as_ref()
            && !region.shutdown().await
        {
            let warning = "extension runtime did not stop within its shutdown budget".to_string();
            tracing::warn!(
                event = "sdk.session.shutdown.extension_timeout",
                "{warning}"
            );
            report.fail(warning);
        }

        if let Some(manager) = self.mcp_manager.clone() {
            report.mcp = McpShutdownOutcome::Indeterminate;
            if asupersync::time::timeout(
                asupersync::time::wall_now(),
                SESSION_MCP_SHUTDOWN_TIMEOUT,
                manager.shutdown_all(),
            )
            .await
            .is_ok()
            {
                report.mcp = McpShutdownOutcome::Released;
            } else {
                let warning = format!(
                    "MCP manager did not stop within {} seconds",
                    SESSION_MCP_SHUTDOWN_TIMEOUT.as_secs()
                );
                tracing::warn!(event = "sdk.session.shutdown.mcp_timeout", "{warning}");
                report.fail(warning);
            }
        }

        report
    }

    /// Consume an old handle after its prepared replacement has become current,
    /// attempting every independent runtime resource family even after errors.
    pub(crate) async fn commit_resource_shutdown(
        self,
        prepared: PreparedSessionShutdown,
    ) -> SessionResourceShutdown {
        self.shutdown_runtime_resources(
            Some(prepared.owner_session_id.as_str()),
            SessionResourceShutdown::default(),
        )
        .await
    }

    /// Exhaustively discard a newly prepared handle that was never committed.
    /// Its session state is intentionally not autosaved, but every owner-scoped
    /// runtime family is still attempted without process-wide job cleanup.
    pub(crate) async fn discard_uncommitted_resources(self) -> SessionResourceShutdown {
        let cx = crate::agent_cx::AgentCx::for_request();
        let mut report = SessionResourceShutdown::default();
        let owner_session_id = match asupersync::sync::OwnedMutexGuard::lock(
            Arc::clone(&self.session.session),
            cx.cx(),
        )
        .await
        {
            Ok(session) => Some(session.header.id.clone()),
            Err(err) => {
                let warning = format!("failed to resolve discarded session ownership: {err}");
                tracing::warn!(event = "sdk.session.discard.owner_failed", "{warning}");
                report.fail(warning);
                None
            }
        };
        self.shutdown_runtime_resources(owner_session_id.as_deref(), report)
            .await
    }

    /// Await shutdown of every runtime resource owned by this session.
    ///
    /// Final driver exit uses this exhaustive seam: persistence or ownership
    /// failures are reported, but they never skip later independent cleanup.
    pub async fn shutdown_owned_resources(self) -> SessionResourceShutdown {
        let mut report = SessionResourceShutdown::default();
        let cx = crate::agent_cx::AgentCx::for_request();
        let owner_session_id = match asupersync::sync::OwnedMutexGuard::lock(
            Arc::clone(&self.session.session),
            cx.cx(),
        )
        .await
        {
            Ok(mut session) => {
                let owner_session_id = session.header.id.clone();
                if self.session.save_enabled()
                    && let Err(err) = session.flush_autosave_on_shutdown().await
                {
                    let warning = format!("failed to flush session autosave: {err}");
                    tracing::warn!(event = "sdk.session.shutdown.autosave_failed", "{warning}");
                    report.fail(warning);
                }
                Some(owner_session_id)
            }
            Err(err) => {
                let warning = format!("failed to lock session for resource shutdown: {err}");
                tracing::warn!(
                    event = "sdk.session.shutdown.jobs_owner_failed",
                    "{warning}"
                );
                report.fail(warning);
                None
            }
        };
        self.shutdown_runtime_resources(owner_session_id.as_deref(), report)
            .await
    }

    /// Mount cached tools for one MCP server, skipping exact names already
    /// present in this Agent. Returns the number of wrappers added.
    ///
    /// Runtime trust/test controls use this shared seam so repeated commands
    /// cannot duplicate provider-visible tool definitions (bd-vjfol).
    #[must_use]
    pub fn mount_mcp_server_tools_if_absent(&mut self, server_name: &str) -> usize {
        let Some(manager) = self.mcp_manager.clone() else {
            return 0;
        };
        let mut wrappers = crate::mcp::mount_server_tools(&manager, server_name);
        wrappers.retain(|tool| !self.session.agent.has_tool(tool.name()));
        let mounted = wrappers.len();
        self.session.agent.extend_tools(wrappers);
        mounted
    }

    /// Start acknowledged MCP servers and mount their cached tools into this
    /// exact Agent. Session replacement calls this only after the previous
    /// handle has been dropped, preventing overlapping singleton transports.
    pub(crate) async fn activate_mcp(&mut self) {
        let Some(manager) = self.mcp_manager.clone() else {
            return;
        };
        let mut wrappers = crate::mcp::connect_trusted_and_mount_tools(&manager).await;
        wrappers.retain(|tool| !self.session.agent.has_tool(tool.name()));
        self.session.agent.extend_tools(wrappers);
    }

    /// Bring MCP servers that extensions registered after startup into the
    /// live session (bd-8m21l).
    ///
    /// Startup copies the extension-registered server definitions into the
    /// MCP manager once; a `registerMcpServer` call from a later extension
    /// callback only updated the extension manager's snapshot and stayed
    /// unreachable until restart. This drains the snapshot: every definition
    /// whose name the MCP manager does not know yet is registered under the
    /// same trust gate as at startup, and when anything was new the trusted
    /// servers are connected and only tool names not already mounted are
    /// added. Returns the number of newly registered definitions. Called at
    /// the start of every prompt; cheap when nothing changed.
    pub async fn sync_extension_mcp_registrations(&mut self) -> usize {
        let Some(manager) = self.mcp_manager.clone() else {
            return 0;
        };
        let Some(extensions) = self.extension_manager().cloned() else {
            return 0;
        };
        crate::mcp::sync_extension_registrations(&manager, &extensions, &mut self.session.agent)
            .await
    }

    /// The session store this handle drives, for the rare caller that needs
    /// NON-BLOCKING access to it.
    ///
    /// `/share` is the reason this exists: exporting a session must report
    /// "the session is busy, retry" rather than park a user-cancellable
    /// subprocess flow behind a mutex another task is holding. Anything that
    /// can afford to wait should use [`Self::with_session`] instead, which
    /// cannot leave the lock held across a caller's await.
    #[must_use]
    pub fn session_store(&self) -> Arc<asupersync::sync::Mutex<crate::session::Session>> {
        Arc::clone(&self.session.session)
    }

    /// Whether a tool of this name is installed on the live agent.
    ///
    /// `/tan` is the caller this exists for: the `subagent` tool is opt-in, so
    /// a session without it has to say so plainly rather than fail somewhere
    /// inside the child launch (bd-ydz1t.2).
    #[must_use]
    pub fn has_tool(&self, name: &str) -> bool {
        self.session.agent.has_tool(name)
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

    /// The compaction settings this session resolved at creation time.
    ///
    /// Reflects [`SessionOptions::compaction_settings`] when an override was
    /// supplied, or the config/model-derived defaults otherwise.
    pub const fn compaction_settings(&self) -> &ResolvedCompactionSettings {
        self.session.compaction_settings()
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

    /// Step the thinking level one place through the levels the running model
    /// actually offers, wrapping past the last one back to the first.
    ///
    /// Returns the level now in force, or `None` when there is nothing to
    /// cycle through: a non-reasoning model offers only `Off`, and a model the
    /// registry cannot resolve offers nothing at all. `None` is not an error —
    /// the caller reports it as "this model does not support thinking", the
    /// same as the charmed stack and the `cycle_thinking_level` RPC.
    ///
    /// The level list comes from [`ModelEntry::available_thinking_levels`],
    /// which is the single definition of that policy, and the write goes
    /// through [`Self::set_thinking_level`], so clamping, history dedupe and
    /// persistence behave exactly as they do for an explicit `/thinking`.
    pub async fn cycle_thinking_level(&mut self) -> Result<Option<crate::model::ThinkingLevel>> {
        let Some(entry) = self.session.current_model_entry() else {
            return Ok(None);
        };
        let levels = entry.available_thinking_levels();
        if levels.len() <= 1 {
            return Ok(None);
        }
        let current = self.thinking_level().unwrap_or_default();
        // An unknown current level starts the cycle at the beginning rather
        // than failing: a level clamped away by a model switch must not strand
        // the key.
        let index = levels
            .iter()
            .position(|level| *level == current)
            .unwrap_or(0);
        // `cycle` supplies the wrap, so this reads the same as indexing at
        // `(index + 1) % len` without an index that has to be proven in range.
        // The list is non-empty here, so `nth` always yields.
        let Some(next) = levels.iter().copied().cycle().nth(index + 1) else {
            return Ok(None);
        };
        self.set_thinking_level(next).await?;
        Ok(Some(next))
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

    /// OMP `/fresh`: reset provider stream state without touching the
    /// transcript. Stream options are rebound to a new session id (which
    /// re-derives the prompt cache key), and a `fresh` custom entry records
    /// the reset. Returns the new id.
    pub async fn fresh_stream_state(&mut self) -> Result<String> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let new_id = {
            let mut guard = self
                .session
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            crate::checkpoint::fresh_stream_state(&mut self.session.agent, &mut guard)
        };
        self.session.persist_session().await?;
        Ok(new_id)
    }

    /// Record an approval-mode change (`/approval`) in the session, as the
    /// classic stack does, so the transition is auditable after the fact.
    pub async fn record_approval_mode(
        &mut self,
        mode: crate::approval::ApprovalMode,
    ) -> Result<()> {
        let cx = crate::agent_cx::AgentCx::for_request();
        {
            let mut guard = self
                .session
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            guard.append_custom_entry(
                "approval_mode".to_string(),
                Some(serde_json::json!({"mode": mode.as_str()})),
            );
        }
        self.session.persist_session().await
    }

    /// `/checkpoint [name] [note]` (bd-cv653.3.7): mark the active context so
    /// a later [`Self::rewind_to_checkpoint`] can collapse everything after it.
    pub async fn mark_checkpoint(
        &mut self,
        name: &str,
        note: Option<&str>,
    ) -> Result<crate::checkpoint::Checkpoint> {
        let messages = self.session.agent.messages().to_vec();
        let cx = crate::agent_cx::AgentCx::for_request();
        let checkpoint = {
            let mut guard = self
                .session
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            crate::checkpoint::mark_checkpoint(&mut guard, name, note, &messages)
        };
        self.session.persist_session().await?;
        Ok(checkpoint)
    }

    /// `/rewind [name]` (bd-cv653.3.7): collapse the active context since the
    /// named (default: latest) checkpoint into one summarized report. The
    /// session tree keeps every original entry; the rewind is recorded in it.
    pub async fn rewind_to_checkpoint(
        &mut self,
        name: Option<&str>,
    ) -> Result<crate::checkpoint::RewindOutcome> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let checkpoint = {
            let guard = self
                .session
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            crate::checkpoint::find_checkpoint(&guard, name)
        }
        .ok_or_else(|| {
            Error::session(name.map_or_else(
                || String::from("No checkpoints yet — mark one with /checkpoint"),
                |name| format!("No checkpoint named '{name}'"),
            ))
        })?;
        let agent = &self.session.agent;
        let span =
            agent.messages()[checkpoint.message_count.min(agent.messages().len())..].to_vec();
        if span.is_empty() {
            return Err(Error::session(format!(
                "Nothing to rewind — the active context is already at '{}'.",
                checkpoint.name
            )));
        }
        // Keyless providers (replay/test/local) summarize fine without a key.
        let api_key = agent.stream_options().api_key.clone().unwrap_or_default();
        let settings = crate::compaction::ResolvedCompactionSettings {
            enabled: true,
            ..Default::default()
        };
        let summary =
            crate::checkpoint::summarize_span(&span, agent.provider(), &api_key, &settings)
                .await
                .unwrap_or_else(|err| {
                    format!(
                        "(summarization failed: {err}; the span was collapsed without a report)"
                    )
                });
        let outcome = crate::checkpoint::apply_rewind_to_active(
            &mut self.session.agent,
            &checkpoint,
            summary,
        );
        {
            let mut guard = self
                .session
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            guard.append_custom_entry(
                "rewind".to_string(),
                Some(serde_json::to_value(&outcome).unwrap_or_default()),
            );
        }
        self.session.persist_session().await?;
        Ok(outcome)
    }

    /// OMP `/retry`: move the session leaf to the parent of the last
    /// retryable user turn, so re-sending its text (returned) lands as a
    /// SIBLING branch. The abandoned turn stays in the tree for `/tree`. The
    /// agent's context is rebuilt from the new path before this returns.
    pub async fn prepare_retry(&mut self) -> Result<String> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let (text, messages) = {
            let mut guard = self
                .session
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            let prepared = crate::checkpoint::prepare_retry_branch(&mut guard)
                .ok_or_else(|| Error::session("No user turn to retry".to_string()))?;
            (prepared.text, guard.to_messages_for_current_path())
        };
        self.session.agent.replace_messages(messages);
        self.session.persist_session().await?;
        Ok(text)
    }

    /// OMP `/branch` and double-Esc rewind: move the session leaf to just
    /// before the user message `entry_id`, so the next prompt lands as its
    /// sibling while the old path stays in the tree. Returns the message
    /// for the editor. The agent's context is rebuilt from the new path and
    /// the move persisted before this returns.
    pub async fn rewind_to_user_message(
        &mut self,
        entry_id: &str,
    ) -> Result<crate::checkpoint::RewindPreparation> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let (prepared, messages) = {
            let mut guard = self
                .session
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            let prepared = crate::checkpoint::rewind_to_user_entry(&mut guard, entry_id)
                .ok_or_else(|| {
                    Error::session(format!("No user message {entry_id} on this branch"))
                })?;
            (prepared, guard.to_messages_for_current_path())
        };
        self.session.agent.replace_messages(messages);
        self.session.persist_session().await?;
        Ok(prepared)
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

    /// `/add-dir` driver (bd-cv653.3.12): validate + add an additional
    /// workspace root on the shared handle and persist the canonical set
    /// into the session header. Returns a user-facing status line.
    pub async fn add_workspace_root(&mut self, dir: impl AsRef<Path>) -> Result<String> {
        let canonical = crate::workspace::validate_new_root(dir.as_ref())?;
        let mut workspace = self
            .workspace
            .clone()
            .ok_or_else(|| Error::validation("no workspace handle in this session"))?;
        let already = workspace
            .snapshot_or(Path::new("."))
            .contains_canonical(&canonical);
        if !already {
            workspace.add_root(&canonical);
        }
        let roots = workspace.additional_roots();
        let cx = crate::agent_cx::AgentCx::for_request();
        {
            let mut guard = self
                .session
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            guard.set_additional_roots(&roots);
        }
        let display = canonical.display().to_string();
        Ok(if already {
            format!("Already a workspace root: {display}")
        } else {
            format!("Workspace root added: {display}")
        })
    }

    /// `/remove-dir` driver (bd-cv653.3.12): revoke an additional root on
    /// the shared handle (immediate for every tool holding a clone) and
    /// persist. Returns a user-facing status line.
    pub async fn remove_workspace_root(&mut self, dir: impl AsRef<Path>) -> Result<String> {
        let mut workspace = self
            .workspace
            .clone()
            .ok_or_else(|| Error::validation("no workspace handle in this session"))?;
        let removed = workspace.remove_root(dir.as_ref());
        if !removed {
            return Ok(format!("Not a workspace root: {}", dir.as_ref().display()));
        }
        let roots = workspace.additional_roots();
        let cx = crate::agent_cx::AgentCx::for_request();
        {
            let mut guard = self
                .session
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| Error::session(e.to_string()))?;
            guard.set_additional_roots(&roots);
        }
        Ok(format!(
            "Workspace root removed: {}",
            dir.as_ref().display()
        ))
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

    /// Run a read-only closure against the locked session.
    ///
    /// Lets embedding hosts take arbitrary snapshots (message history, header
    /// fields, usage) without the SDK growing one accessor per shape — e.g.
    /// the ftui launch path rebuilds its transcript after a session resume
    /// via `interactive::conversation_from_session`.
    pub async fn with_session<R>(
        &self,
        f: impl FnOnce(&crate::session::Session) -> R,
    ) -> Result<R> {
        let cx = crate::agent_cx::AgentCx::for_request();
        let guard = self
            .session
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| Error::session(e.to_string()))?;
        Ok(f(&guard))
    }

    /// Trigger an immediate compaction pass (if compaction is enabled).
    pub async fn compact(
        &mut self,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<()> {
        self.session.compact_now(on_event).await
    }

    /// OMP `/shake`: compaction that drops bulky tool output from the older
    /// span instead of summarizing it with the model (no provider request).
    pub async fn shake(
        &mut self,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<()> {
        self.session.shake_now(on_event).await
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
        // Extension observation events (bd-82331). Lifecycle events reach
        // extensions from inside the agent loop, but message_* and
        // tool_execution_* only arrive if the surface routes them through a
        // coalescer. Print, RPC and the classic stack each install one; this
        // path did not, so every extension running under the default
        // interactive stack — which is the SDK path since v0.4.0 — silently
        // observed nothing.
        //
        // Installing it here rather than in `interactive_ftui` is deliberate:
        // this is the single fan-out point every SDK event travels through, so
        // a future SDK-based surface inherits the routing instead of having to
        // remember it.
        //
        // Construction is deliberately unconditional when extensions exist.
        // Gating it on `has_any_event_hooks()` here would save four `Arc`
        // allocations per prompt and would be wrong: that answer is computed
        // once, at the start of the turn, while an extension may register a
        // handler part-way through it. The correct fast path is the per-event
        // `has_hook_for` check inside `dispatch_agent_event_lazy`, which
        // re-reads the manager's lock-free snapshot on every event and returns
        // before serializing when nothing is listening. That is where the
        // "subscribes to nothing costs nothing" guarantee actually lives.
        let coalescer = self
            .extension_manager()
            .map(|manager| crate::extensions::EventCoalescer::new(manager.clone()));
        let event_runtime = self.session.runtime_handle().cloned();
        if coalescer.is_some() && event_runtime.is_none() {
            // Nothing to spawn onto: the routing cannot work and would fail
            // silently, which is the exact shape of the bug this fixes. Emitted
            // per prompt rather than once, deliberately — a single line at
            // startup is easy to scroll past, and the condition is static, so
            // repeating it costs nothing and makes it findable from any point
            // in a session transcript.
            tracing::debug!(
                target: "pi::sdk",
                "extensions are loaded but this session has no runtime handle; \
                 message and tool-execution events will not reach them"
            );
        }
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

            // Extension observation events, batched with lazy serialization.
            // The coalescer skips lifecycle events (already dispatched
            // in-loop) and returns before serializing when no extension has a
            // hook for this event kind, so a session whose extensions
            // subscribe to nothing pays nothing here (bd-82331).
            if let (Some(coal), Some(runtime)) = (coalescer.as_ref(), event_runtime.as_ref()) {
                coal.dispatch_agent_event_lazy(&event, runtime);
            }

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

fn load_session_config(
    cwd: &Path,
    global_dir: &Path,
    config_override: Option<&Path>,
    workspace_trusted: bool,
) -> Result<Config> {
    Config::load_with_roots_and_project_trust(config_override, global_dir, cwd, workspace_trusted)
}

fn build_stream_options_with_optional_key(
    config: &Config,
    api_key: Option<String>,
    selection: &app::ModelSelection,
    session: &Session,
) -> StreamOptions {
    // Match the CLI path (`app::build_stream_options`): prompt caching
    // defaults to short retention, so SDK embedders get the same
    // Anthropic cache breakpoints instead of silently paying full input
    // price. `PI_CACHE_RETENTION` overrides ("long"/"none").
    let cache_retention =
        app::cache_retention_from_env(std::env::var("PI_CACHE_RETENTION").ok().as_deref());
    let mut options = StreamOptions {
        api_key,
        headers: selection.model_entry.headers.clone(),
        session_id: Some(session.header.id.clone()),
        thinking_level: Some(selection.thinking_level),
        cache_retention,
        // Session-scoped cache affinity for OpenAI-shaped requests (gh #188),
        // matching the CLI path.
        prompt_cache_key: app::resolve_prompt_cache_key(
            std::env::var("PI_PROMPT_CACHE_KEY").ok().as_deref(),
            cache_retention,
            Some(session.header.id.as_str()),
        ),
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
pub async fn create_agent_session(options: SessionOptions) -> Result<AgentSessionHandle> {
    let mut handle = create_agent_session_deferred_mcp(options).await?;
    handle.activate_mcp().await;
    Ok(handle)
}

/// Build a session without starting acknowledged MCP transports.
///
/// The default FTUI uses this only for atomic `/new` and `/resume` handoff:
/// construct every fallible session component first, await shutdown of the
/// old handle, then call [`AgentSessionHandle::activate_mcp`] so singleton MCP
/// servers are never live in both sessions at once (bd-vjfol).
#[allow(clippy::too_many_lines)]
pub(crate) async fn create_agent_session_deferred_mcp(
    options: SessionOptions,
) -> Result<AgentSessionHandle> {
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

    let global_dir = Config::global_dir();
    let config_override = Config::config_path_override_from_env(&cwd);
    let config = load_session_config(
        &cwd,
        &global_dir,
        config_override.as_deref(),
        options.workspace_trusted,
    )?;
    let auth_path = Config::auth_path();
    let auth_result = AuthStorage::load_async(auth_path.clone()).await;
    let mut auth = match auth_result {
        Ok(auth) => auth,
        Err(err)
            if options
                .api_key
                .as_deref()
                .is_some_and(|k| !k.trim().is_empty()) =>
        {
            tracing::warn!(
                "stored credentials are unavailable ({err}); continuing with explicit api_key only"
            );
            AuthStorage::empty_at(auth_path)
        }
        Err(err) => return Err(err),
    };
    // gh #218: refresh per provider; only a failure for the provider this
    // session actually selects is an error (checked after selection below).
    let oauth_refresh = auth.refresh_expired_oauth_tokens_report().await;
    if !oauth_refresh.failed.is_empty() {
        tracing::warn!(
            providers = ?oauth_refresh.failed_provider_ids(),
            "stored OAuth credentials could not be refreshed; ignored unless the session selects one of them"
        );
    }

    let raw_package_dir = options
        .package_dir
        .clone()
        .unwrap_or_else(crate::config::Config::package_dir);
    // bd-jtehj: interpret relative package roots against the session cwd —
    // never the ambient process cwd — before prompt discovery sees them.
    let package_dir = crate::app::stable_package_dir(&raw_package_dir, Some(&cwd));
    let models_path = default_models_path(&global_dir);
    let mut model_registry = ModelRegistry::load(&auth, Some(models_path));

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

    // The session owns the extension runtime, so registration must precede
    // final provider selection. Do not persist a provisional model: resumed
    // extension identities and explicit selectors must survive registration.
    let has_extensions = !options.extension_paths.is_empty();
    let selection = if has_extensions {
        extension_bootstrap::provisional_selection(&model_registry)?
    } else {
        app::select_model_and_thinking(
            &cli,
            &config,
            &session,
            &model_registry,
            &scoped_models,
            &global_dir,
        )
        .map_err(|err| Error::validation(err.to_string()))?
    };
    if !has_extensions {
        app::update_session_for_selection(&mut session, &selection);
    }

    let enabled_tools_owned = cli
        .enabled_tools()
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let enabled_tools = enabled_tools_owned
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();

    let sdk_test_mode = std::env::var_os("PI_TEST_MODE").is_some();
    // Foreign-format workspace rules (bd-cv653.6.2), shared with the agent's
    // scoped-rule activation below.
    let foreign_rules = if config.foreign_rules_enabled() && !sdk_test_mode && !cli.no_context_files
    {
        crate::context_files::discover_foreign_rules(&cwd)
    } else {
        crate::context_files::ForeignRules::default()
    };
    let system_prompt = app::build_system_prompt(
        &cli,
        &cwd,
        &enabled_tools,
        options
            .skills_prompt
            .as_deref()
            .filter(|block| !block.is_empty()),
        &global_dir,
        &package_dir,
        sdk_test_mode,
        options.include_cwd_in_prompt,
        Some(&foreign_rules),
        &config,
    )
    .map_err(|err| Error::validation(err.to_string()))?;

    let provider = providers::create_provider(&selection.model_entry, None)
        .map_err(|e| Error::provider("sdk", e.to_string()))?;

    let api_key = if has_extensions {
        None
    } else {
        app::resolve_api_key(&auth, &cli, &selection.model_entry)
            .map_err(|err| Error::validation(err.to_string()))?
    };
    if !has_extensions
        && cli.api_key.is_none()
        && let Some(failure) = oauth_refresh.failure_for(&selection.model_entry.model.provider)
    {
        return Err(Error::auth(format!(
            "OAuth token refresh failed for: {} ({})",
            failure.provider, failure.error
        )));
    }

    let stream_options =
        build_stream_options_with_optional_key(&config, api_key, &selection, &session);

    let agent_config = AgentConfig {
        system_prompt: Some(system_prompt),
        max_tool_iterations: options.max_tool_iterations,
        stream_options,
        block_images: config.image_block_images(),
        model_accepts_images: selection
            .model_entry
            .model
            .input
            .contains(&crate::provider::InputType::Image),
        fail_closed_hooks: config.fail_closed_hooks(),
        tool_approval: options.tool_approval.clone(),
        keyword_settings: config.keywords.clone(),
        max_time: None,
        turn_recovery: config.turn_recovery_mode(),
        approval_state: options.approval_state.clone(),
        bash_settings: config.bash.clone(),
        // The configured vault mode and patterns. `None` meant the built-in
        // obfuscate default whatever was set, so on the default (SDK-built)
        // stack `block` quietly became `obfuscate`, `off` was ignored and
        // user `extra_patterns` never applied.
        secrets: config.secrets.clone(),
    };

    let tools = options.tool_factory.as_ref().map_or_else(
        // The default registry carries an undo recorder (bd-cv653.3.13) so
        // SDK-driven surfaces (ftui, embedders) can offer /undo //redo too.
        || {
            ToolRegistry::with_mutation_recorder(
                &enabled_tools,
                &cwd,
                Some(&config),
                Some(std::sync::Arc::new(
                    crate::undo::FileMutationRecorder::default(),
                )),
                options.workspace.as_ref(),
            )
        },
        |factory| factory.create_tool_registry(&enabled_tools, &cwd, &config),
    );
    // Permission prompts belong to the host, not to the model-facing ask
    // capability. Every registry owns one picker even when "ask" is disabled.
    let host_ask = tools.host_ask_tool();
    let session_arc = Arc::new(asupersync::sync::Mutex::new(session));

    let compaction_settings = options.compaction_settings.clone().unwrap_or_else(|| {
        let context_window_tokens = if selection.model_entry.model.context_window == 0 {
            ResolvedCompactionSettings::default().context_window_tokens
        } else {
            selection.model_entry.model.context_window
        };
        ResolvedCompactionSettings {
            enabled: config.compaction_enabled(),
            reserve_tokens: config.compaction_reserve_tokens(),
            keep_recent_tokens: config.compaction_keep_recent_tokens(),
            context_window_tokens,
            mode: config.compaction_mode(),
            render_mode: config.compaction_render_mode(),
        }
    });

    let mut agent_session = AgentSession::new(
        Agent::new(provider, tools, agent_config),
        Arc::clone(&session_arc),
        !cli.no_session,
        compaction_settings,
    );
    if let Some(handle) = options.runtime_handle.clone() {
        agent_session = agent_session.with_runtime_handle(handle);
    }
    // settings.json `steeringMode` / `followUpMode`. The classic UI and RPC
    // apply them to their own agents; without this every SDK-built session,
    // the default FTUI included, delivered queued messages one at a time
    // whatever was configured. Extensions enabled below copy these modes.
    agent_session.set_queue_modes(config.steering_queue_mode(), config.follow_up_queue_mode());
    agent_session.set_api_key_override(options.api_key.clone());
    agent_session.advisor = options
        .advisor
        .as_ref()
        .map(crate::advisor::AdvisorOptions::runtime);
    if foreign_rules.scoped_rules().next().is_some() {
        agent_session
            .agent
            .set_foreign_scoped_rules(foreign_rules.rules.clone(), cwd.clone());
    }
    // Session-coupled todo tool joins after construction (opt-in), matching
    // the CLI wiring in main.rs.
    if enabled_tools.contains(&"todo") {
        let todo_session = Arc::clone(&agent_session.session);
        agent_session.agent.extend_tools(vec![
            Box::new(crate::todo::TodoTool::new(todo_session)) as Box<dyn crate::tools::Tool>
        ]);
    }
    // The host picker always exists. Enabling "ask" grants the model the
    // structured-question tool by exposing this same shared handle in the
    // schema; disabling it does not remove the host's authorization surface.
    let ask_tool_handle = Some(host_ask.clone());
    if enabled_tools.contains(&"ask") {
        agent_session.agent.extend_tools(vec![
            Box::new(host_ask.clone()) as Box<dyn crate::tools::Tool>
        ]);
    }
    // Approval prompts bridge through the host picker whether or not the
    // model-facing ask tool is enabled.
    if let Some(approval_state) = &options.approval_state
        && options.tool_approval.is_none()
    {
        agent_session
            .agent
            .set_tool_approval(Some(crate::ask::approval_handler_via_ask(
                host_ask,
                approval_state.clone(),
            )));
    }

    if has_extensions {
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
                crate::agent::ExtensionHostConfiguration {
                    ui_handler: options.extension_ui_handler.clone(),
                    persist_permission_decisions: options.persist_extension_permissions,
                    cli_flags: options.extension_flags.clone(),
                },
            )
            .await?;
        extension_bootstrap::finish_selection(
            &mut agent_session,
            &mut model_registry,
            &mut auth,
            extension_bootstrap::SelectionInputs {
                cli: &cli,
                config: &config,
                scoped_patterns: &scoped_patterns,
                global_dir: &global_dir,
                oauth_refresh: &oauth_refresh,
                preserve_compaction_window: options.compaction_settings.is_some(),
            },
        )
        .await?;
    }

    // Extensions observe through a coalescer that spawns onto a runtime
    // (bd-82331). Every surface in this repo now supplies one, but an embedder
    // that loads extensions and omits `runtime_handle` would still get the four
    // lifecycle events and nothing else, with no error — the exact failure
    // bd-82331 existed to remove. Build one rather than leave the trap set for
    // everyone outside this tree, and say so at debug: four worker threads
    // appearing because you loaded an extension should not be a surprise
    // (bd-8rvry).
    //
    // Gated on extensions actually being loaded, so a session that will never
    // dispatch an observation event allocates nothing.
    let event_runtime = if options.runtime_handle.is_none() && agent_session.extensions.is_some() {
        match asupersync::runtime::RuntimeBuilder::new().build() {
            Ok(runtime) => {
                agent_session = agent_session.with_runtime_handle(runtime.handle());
                tracing::debug!(
                    target: "pi::sdk",
                    "extensions are loaded and no runtime handle was supplied; built one so \
                     message and tool-execution events reach them"
                );
                Some(runtime)
            }
            Err(err) => {
                // Deliberately not fatal. Everything except extension
                // observation still works, and refusing to create the session
                // would be a worse trade than the degradation it replaces —
                // but it is a warning, not a debug line, because the events
                // really are being dropped.
                tracing::warn!(
                    target: "pi::sdk",
                    error = %err,
                    "extensions are loaded but no runtime could be built for them; message and \
                     tool-execution events will not reach extensions on this session"
                );
                None
            }
        }
    } else {
        None
    };

    let mcp_manager = if let Some(mcp) = &options.mcp {
        let global_dir = mcp.global_dir.clone().unwrap_or_else(Config::global_dir);
        let manager = Arc::new(crate::mcp::McpManager::bootstrap(
            &cwd,
            &global_dir,
            &mcp.config_paths,
            options.workspace_trusted,
        )?);
        if let Some(extension_manager) = agent_session
            .extensions
            .as_ref()
            .map(ExtensionRegion::manager)
        {
            for spec in extension_manager.extension_mcp_servers() {
                let name = spec.get("name").and_then(Value::as_str).unwrap_or_default();
                if !name.is_empty() {
                    manager.register_extension_server(name, &spec);
                }
            }
        }
        Some(manager)
    } else {
        None
    };

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
    let failover_state = match options.failover.as_ref() {
        None => crate::failover::FailoverState::new_empty(),
        Some(failover) => {
            let cx = crate::agent_cx::AgentCx::for_request();
            let inner = agent_session.session.lock(cx.cx()).await.ok();
            inner.as_deref().map_or_else(
                || crate::failover::FailoverState::with_cooldown_secs(failover.cooldown_secs),
                |s| {
                    crate::failover::FailoverState::reconstruct_from_session(
                        s,
                        failover.cooldown_secs,
                        chrono::Utc::now(),
                    )
                },
            )
        }
    };
    Ok(AgentSessionHandle {
        session: agent_session,
        listeners,
        ask_tool: ask_tool_handle,
        workspace: options.workspace.clone(),
        mcp_manager,
        retry: options.retry,
        failover_state,
        failover: options.failover.map(Arc::new),
        event_runtime,
    })
}

#[cfg(test)]
mod tests {
    mod recovery;

    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use asupersync::runtime::reactor::create_reactor;
    use asupersync::sync::Mutex as AsyncMutex;
    use std::env;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    /// Drives `future` to completion on a `current_thread` runtime.
    ///
    /// Note that this polls on the CALLING thread, which is the libtest thread,
    /// so the frames land on whatever stack libtest gave it. These session
    /// builds do not fit the 2 MiB default in a debug build, and a stack
    /// overflow ABORTS the process rather than failing one test, so the whole
    /// binary reports nothing — not a tally with one failure in it.
    ///
    /// Boxing the future is enough on its own: every one of the 66 `sdk::`
    /// tests passes on an unmodified libtest thread with it, and overflows
    /// without it. The builder-level `thread_stack_size` that rescued
    /// `sdk_unit` and `sdk_integration` (bd-79qxb) cannot help here, and
    /// neither can moving the work to a reserved thread: several of these
    /// futures hold an `asupersync::sync::MutexGuard` across an await and so
    /// are not `Send`. `RUST_MIN_STACK` in `.cargo/config.toml` also covers
    /// this, but only for a process cargo launches; boxing additionally covers
    /// running the built test binary directly, which cargo's `[env]` never
    /// reaches.
    fn run_async<F>(future: F) -> F::Output
    where
        F: std::future::Future,
    {
        let reactor = create_reactor().expect("create reactor");
        let runtime = RuntimeBuilder::current_thread()
            .with_reactor(reactor)
            .build()
            .expect("build runtime");
        runtime.block_on(Box::pin(future))
    }

    fn current_dir_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_current_dir_lock()
    }

    /// Fails its first `failures` calls with a retryable provider error, then
    /// answers normally. Counts calls so a test can prove how many were made.
    struct FlakyThenOkProvider {
        failures: usize,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        /// Identity this double reports. The retry tests use a synthetic one;
        /// the restoration tests need a REAL provider id, because restoring
        /// reconstructs the primary through `providers::create_provider` and a
        /// synthetic id has no route to reconstruct.
        name: String,
        model: String,
        /// Provider-reported usage for silent context-overflow regressions.
        input_tokens: u64,
    }

    #[async_trait::async_trait]
    impl crate::provider::Provider for FlakyThenOkProvider {
        fn name(&self) -> &str {
            &self.name
        }

        #[allow(clippy::unnecessary_literal_bound)]
        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            &self.model
        }

        async fn stream(
            &self,
            _context: &crate::provider::Context<'_>,
            _options: &crate::provider::StreamOptions,
        ) -> crate::error::Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<Item = crate::error::Result<crate::model::StreamEvent>>
                        + Send,
                >,
            >,
        > {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut partial = crate::model::AssistantMessage {
                content: Vec::new(),
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: crate::model::Usage {
                    input: self.input_tokens,
                    ..crate::model::Usage::default()
                },
                stop_reason: crate::model::StopReason::Error,
                stop_details: None,
                error_message: Some("503 service unavailable".to_string()),
                timestamp: 0,
            };
            if call < self.failures {
                let events = vec![
                    Ok(crate::model::StreamEvent::Start {
                        partial: partial.clone(),
                    }),
                    Ok(crate::model::StreamEvent::Error {
                        reason: crate::model::StopReason::Error,
                        error: partial,
                    }),
                ];
                return Ok(Box::pin(futures::stream::iter(events)));
            }
            partial.stop_reason = crate::model::StopReason::Stop;
            partial.error_message = None;
            let events = vec![
                Ok(crate::model::StreamEvent::Start {
                    partial: partial.clone(),
                }),
                Ok(crate::model::StreamEvent::Done {
                    reason: crate::model::StopReason::Stop,
                    message: partial,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    fn flaky_handle(failures: usize) -> (AgentSessionHandle, Arc<std::sync::atomic::AtomicUsize>) {
        flaky_handle_as(failures, "test-provider", "test-model")
    }

    /// A handle over a SAVING, on-disk session, for tests that care what
    /// reaches the file rather than what reaches memory. The other helpers here
    /// build an in-memory session with saving off, which cannot distinguish a
    /// turn that persisted from one that did not.
    fn saving_handle(dir: &Path) -> AgentSessionHandle {
        let provider = Arc::new(FlakyThenOkProvider {
            failures: 0,
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            name: "test-provider".to_string(),
            model: "test-model".to_string(),
            input_tokens: 0,
        });
        let agent = crate::agent::Agent::new(
            provider,
            crate::tools::ToolRegistry::new(&[], Path::new("."), None),
            crate::agent::AgentConfig::default(),
        );
        let session = AgentSession::new(
            agent,
            Arc::new(AsyncMutex::new(crate::session::Session::create_with_dir(
                Some(dir.to_path_buf()),
            ))),
            true,
            crate::compaction::ResolvedCompactionSettings::default(),
        );
        AgentSessionHandle::from_session_with_listeners(session, EventListeners::default())
    }

    fn flaky_handle_as(
        failures: usize,
        name: &str,
        model: &str,
    ) -> (AgentSessionHandle, Arc<std::sync::atomic::AtomicUsize>) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = Arc::new(FlakyThenOkProvider {
            failures,
            calls: Arc::clone(&calls),
            name: name.to_string(),
            model: model.to_string(),
            input_tokens: 0,
        });
        let agent = crate::agent::Agent::new(
            provider,
            crate::tools::ToolRegistry::new(&[], Path::new("."), None),
            crate::agent::AgentConfig::default(),
        );
        let session = AgentSession::new(
            agent,
            Arc::new(AsyncMutex::new(crate::session::Session::in_memory())),
            false,
            crate::compaction::ResolvedCompactionSettings::default(),
        );
        (
            AgentSessionHandle::from_session_with_listeners(session, EventListeners::default()),
            calls,
        )
    }

    /// Install a failover policy whose chain names `spec`, with no credential
    /// requirement, so the walk reaches provider construction.
    fn with_chain(handle: AgentSessionHandle, spec: &str) -> AgentSessionHandle {
        with_chain_cooldown(handle, spec, 300)
    }

    /// A loopback HTTP endpoint that answers every request with a 401, the
    /// non-retryable response a real provider gives the tests' fake key.
    fn spawn_unauthorized_stub() -> String {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let address = listener.local_addr().expect("stub address");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = [0_u8; 8192];
                let _ = stream.read(&mut request);
                let body = r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#;
                let _ = write!(
                    stream,
                    "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        format!("http://{address}/v1")
    }

    /// [`with_chain`] with an explicit cooldown. Zero means the primary is
    /// restorable the moment it is captured, which is how the restoration path
    /// is exercised without a sleep.
    fn with_chain_cooldown(
        handle: AgentSessionHandle,
        spec: &str,
        cooldown_secs: u64,
    ) -> AgentSessionHandle {
        let auth_path = tempdir().expect("tempdir").path().join("auth.json");
        let auth = crate::auth::AuthStorage::load(auth_path).expect("auth load");
        // The fallback must not reach the real provider: with `test-key` the
        // live endpoint answers 401 on a networked host but a firewalled or
        // saturated one gets a retryable connection error instead, which adds
        // a retry cycle and makes the event order host-dependent. Serve the
        // same deterministic 401 locally.
        let (provider, model_id) =
            crate::provider_metadata::split_provider_model_spec(spec).expect("chain spec");
        let mut fallback =
            crate::models::ad_hoc_model_entry(provider, model_id).expect("fallback entry");
        fallback.model.base_url = spawn_unauthorized_stub();
        handle.with_failover(Some(FailoverOptions {
            chains: std::collections::HashMap::from([(
                "default".to_string(),
                vec![spec.to_string()],
            )]),
            available_models: vec![fallback],
            auth,
            cli_api_key: Some("test-key".to_string()),
            cooldown_secs,
        }))
    }

    /// Run one turn that fails over, and report the handle sitting on the
    /// fallback with the primary captured.
    fn handle_after_one_failover(cooldown_secs: u64) -> AgentSessionHandle {
        // A real primary identity: restoring RECONSTRUCTS it through
        // `providers::create_provider`, so a synthetic id would decline there
        // and the test would pass for the wrong reason.
        let (handle, _calls) = flaky_handle_as(usize::MAX, "anthropic", "claude-3-5-haiku-latest");
        let mut handle = with_chain_cooldown(
            handle.with_retry(Some(crate::failover::RetryPolicy {
                max_retries: 1,
                max_failovers_per_turn: 1,
                base_delay_ms: 1,
                max_delay_ms: 1,
            })),
            "openai/gpt-4o-mini",
            cooldown_secs,
        );
        let (_abort_handle, abort_signal) = AgentSessionHandle::new_abort_handle();
        let _ = run_async(handle.prompt_with_abort("hello", abort_signal, |_| {}));
        assert_eq!(
            handle.session.agent.provider().model_id(),
            "gpt-4o-mini",
            "the setup turn must have installed the fallback"
        );
        handle
    }

    /// Record every `FailoverEnd` as `(restored_primary, model)`.
    type RestoreLog = Arc<Mutex<Vec<(bool, String)>>>;
    fn restore_recorder() -> (RestoreLog, Arc<dyn Fn(AgentEvent) + Send + Sync>) {
        let seen: RestoreLog = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let shared: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(move |event| {
            if let AgentEvent::FailoverEnd {
                restored_primary,
                model,
                ..
            } = event
            {
                recorder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((restored_primary, model));
            }
        });
        (seen, shared)
    }

    fn fast_retry_policy(max_retries: u32) -> crate::failover::RetryPolicy {
        crate::failover::RetryPolicy {
            max_retries,
            max_failovers_per_turn: 0,
            base_delay_ms: 1,
            max_delay_ms: 1,
        }
    }

    /// The gap bd-u2qv4 names: a transient provider failure is a hard error on
    /// the surfaces most people use, while print mode and RPC retry and finish.
    /// With a policy installed, the SDK path — which the default FTUI drives —
    /// now resumes the turn and completes it.
    #[test]
    fn a_configured_retry_policy_resumes_a_transient_failure() {
        let (handle, calls) = flaky_handle(1);
        let mut handle = handle.with_retry(Some(fast_retry_policy(3)));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorder = Arc::clone(&seen);

        let (_abort_handle, abort_signal) = AgentSessionHandle::new_abort_handle();
        let message = run_async(
            handle.prompt_with_abort("hello", abort_signal, move |event| {
                let name = match &event {
                    AgentEvent::AutoRetryStart { .. } => Some("auto_retry_start"),
                    AgentEvent::AutoRetryEnd { .. } => Some("auto_retry_end"),
                    _ => None,
                };
                if let Some(name) = name {
                    recorder
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(name.to_string());
                }
            }),
        )
        .expect("the retried turn must complete");

        assert_eq!(
            message.stop_reason,
            crate::model::StopReason::Stop,
            "the turn must finish on the retry, not surface the 503"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one failed attempt plus one retry"
        );
        let events = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let start = events.iter().position(|name| name == "auto_retry_start");
        let end = events.iter().position(|name| name == "auto_retry_end");
        assert!(
            start.is_some() && end.is_some() && start < end,
            "the host must see the retry, in order: {events:?}"
        );
    }

    /// The planted negative, and the compatibility guarantee: with no policy —
    /// the default for every embedder that existed before this — the identical
    /// failure comes straight back, unretried.
    #[test]
    fn without_a_policy_a_transient_failure_is_returned_unchanged() {
        let (mut handle, calls) = flaky_handle(1);
        let (_abort_handle, abort_signal) = AgentSessionHandle::new_abort_handle();
        let message = run_async(handle.prompt_with_abort("hello", abort_signal, |_| {}))
            .expect("the turn returns its errored message rather than failing the call");

        assert_eq!(
            message.stop_reason,
            crate::model::StopReason::Error,
            "no policy means no retry"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one provider call"
        );
    }

    /// The other half of bd-u2qv4: a configured fallback CHAIN was inert on
    /// this surface. With one installed, a provider that never recovers is left
    /// behind for the next chain entry once the retry budget is spent, and the
    /// host sees the failover lifecycle it already knows how to render.
    #[test]
    fn a_configured_chain_is_walked_once_the_retry_budget_is_spent() {
        let (handle, calls) = flaky_handle(usize::MAX);
        let mut handle = with_chain(
            handle.with_retry(Some(crate::failover::RetryPolicy {
                max_retries: 1,
                max_failovers_per_turn: 1,
                base_delay_ms: 1,
                max_delay_ms: 1,
            })),
            "openai/gpt-4o-mini",
        );
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorder = Arc::clone(&seen);

        let (_abort_handle, abort_signal) = AgentSessionHandle::new_abort_handle();
        let _ = run_async(
            handle.prompt_with_abort("hello", abort_signal, move |event| {
                // Retry events are recorded too, because the ORDER of the two
                // lifecycles is the thing under test (bd-2vmu6): a host that
                // sees `FailoverStart` before the `AutoRetryEnd` it supersedes
                // cannot tell which lifecycle the later events belong to.
                let name = match &event {
                    AgentEvent::AutoRetryStart { attempt, .. } => {
                        Some(format!("auto_retry_start:{attempt}"))
                    }
                    AgentEvent::AutoRetryEnd { attempt, .. } => {
                        Some(format!("auto_retry_end:{attempt}"))
                    }
                    AgentEvent::FailoverStart { to_provider, .. } => {
                        Some(format!("failover_start:{to_provider}"))
                    }
                    AgentEvent::FailoverEnd { .. } => Some("failover_end".to_string()),
                    _ => None,
                };
                if let Some(name) = name {
                    recorder
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(name);
                }
            }),
        );

        let events = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(
            events,
            vec![
                "auto_retry_start:1".to_string(),
                "auto_retry_end:1".to_string(),
                "failover_start:openai".to_string(),
                "failover_end".to_string()
            ],
            "every Start must be closed before the next opens, in this exact \
             order: {events:?}"
        );
        assert!(
            calls.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "the failing provider was called at least once before the swap"
        );
    }

    /// bd-9o9i2: `continue_turn_with_abort` drove the inner `Agent` directly,
    /// and the raw loop persists nothing — everything that writes a turn to the
    /// session lives on `AgentSession`. So a continuation ran a full turn,
    /// streamed its events, returned its assistant message, and wrote nothing.
    ///
    /// The asymmetry was inside one type and, after the retry work, inside one
    /// call: a turn that failed and was RETRIED persisted correctly, because
    /// the retry went through `AgentSession`, while the same turn succeeding
    /// first time did not.
    ///
    /// Asserted against the session REOPENED FROM DISK rather than the live
    /// handle, because the live `Session` would show the entry even if it were
    /// never flushed.
    #[test]
    fn a_continuation_persists_its_turn_to_the_reopened_session() {
        let dir = tempdir().expect("tempdir");
        let mut handle = saving_handle(dir.path());

        let message = run_async(handle.continue_turn(|_| {})).expect("continuation completes");
        assert_eq!(
            message.stop_reason,
            crate::model::StopReason::Stop,
            "the provider double completes on the first attempt"
        );

        let path = run_async(async {
            let store = handle.session_store();
            let cx = crate::agent_cx::AgentCx::for_request();
            let inner = store.lock(cx.cx()).await.expect("session lock");
            inner.path.clone()
        })
        .expect("a persisted continuation gives the session a path on disk");

        let reopened = run_async(crate::session::Session::open(&path.display().to_string()))
            .expect("reopen the session file");
        assert!(
            !reopened.to_messages_for_current_path().is_empty(),
            "a continuation must write its turn to the session file; the raw \
             Agent loop persists nothing (bd-9o9i2)"
        );
    }

    /// OMP `/fresh`: the stream is rebound to a new id each time (two calls
    /// never share one) and the reset is recorded in the session FILE, while
    /// the conversation itself is left alone.
    #[test]
    fn fresh_stream_state_rebinds_the_stream_and_records_the_reset_on_disk() {
        let dir = tempdir().expect("tempdir");
        let mut handle = saving_handle(dir.path());
        let before = handle.session.agent.stream_options().session_id.clone();

        let first = run_async(handle.fresh_stream_state()).expect("fresh");
        let second = run_async(handle.fresh_stream_state()).expect("fresh again");
        assert_ne!(first, second, "each /fresh gets its own id");
        assert_ne!(before.as_deref(), Some(second.as_str()));
        assert_eq!(
            handle.session.agent.stream_options().session_id.as_deref(),
            Some(second.as_str()),
            "the live stream options carry the newest id"
        );
        assert!(
            handle.session.agent.messages().is_empty(),
            "no transcript change"
        );

        let path = run_async(async {
            let store = handle.session_store();
            let cx = crate::agent_cx::AgentCx::for_request();
            let inner = store.lock(cx.cx()).await.expect("session lock");
            inner.path.clone()
        })
        .expect("the reset is persisted");
        let reopened = run_async(crate::session::Session::open(&path.display().to_string()))
            .expect("reopen the session file");
        let recorded: Vec<String> = reopened
            .entries
            .iter()
            .filter_map(|entry| match entry {
                crate::session::SessionEntry::Custom(custom) if custom.custom_type == "fresh" => {
                    custom
                        .data
                        .as_ref()
                        .and_then(|data| data["newSessionId"].as_str())
                        .map(str::to_string)
                }
                _ => None,
            })
            .collect();
        assert_eq!(recorded, vec![first, second]);
    }

    /// OMP `/shake`: the older span is compacted by dropping bulky content,
    /// recorded as a `shake` compaction, and the provider is never called.
    #[test]
    fn shake_compacts_without_a_provider_request() {
        let dir = tempdir().expect("tempdir");
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = Arc::new(FlakyThenOkProvider {
            failures: 0,
            calls: Arc::clone(&calls),
            name: "test-provider".to_string(),
            model: "test-model".to_string(),
            input_tokens: 0,
        });
        let agent = crate::agent::Agent::new(
            provider,
            crate::tools::ToolRegistry::new(&[], Path::new("."), None),
            crate::agent::AgentConfig::default(),
        );
        // A small window so ~3k tokens of history crosses the threshold.
        let settings = crate::compaction::ResolvedCompactionSettings {
            enabled: true,
            context_window_tokens: 2_000,
            reserve_tokens: 1_000,
            keep_recent_tokens: 1,
            ..Default::default()
        };
        let session = AgentSession::new(
            agent,
            Arc::new(AsyncMutex::new(crate::session::Session::create_with_dir(
                Some(dir.path().to_path_buf()),
            ))),
            true,
            settings,
        );
        let mut handle =
            AgentSessionHandle::from_session_with_listeners(session, EventListeners::default());
        let bulky = "history ".repeat(200);
        run_async(async {
            let store = handle.session_store();
            let cx = crate::agent_cx::AgentCx::for_request();
            let mut session = store.lock(cx.cx()).await.expect("session lock");
            for i in 0..4 {
                session.append_message(crate::session::SessionMessage::User {
                    content: crate::model::UserContent::Text(format!("user-{i} {bulky}")),
                    timestamp: Some(i),
                });
                session.append_message(crate::session::SessionMessage::Assistant {
                    message: crate::model::AssistantMessage {
                        content: vec![crate::model::ContentBlock::Text(
                            crate::model::TextContent::new(format!("assistant-{i} {bulky}")),
                        )],
                        ..Default::default()
                    },
                });
            }
        });

        run_async(handle.shake(|_| {})).expect("shake");

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "shake must not reach the provider"
        );
        let mode = run_async(handle.with_session(|session| {
            session.entries.iter().find_map(|entry| match entry {
                crate::session::SessionEntry::Compaction(compaction) => compaction
                    .details
                    .as_ref()
                    .and_then(|details| details["mode"].as_str().map(str::to_string)),
                _ => None,
            })
        }))
        .expect("session");
        assert_eq!(mode.as_deref(), Some("shake"));
    }

    /// `/checkpoint` then `/rewind`: the turn after the checkpoint collapses
    /// out of the agent's context, and the rewind is recorded in the file.
    /// Planted negatives: no checkpoint, a wrong name, and nothing to rewind.
    #[test]
    fn rewind_collapses_the_span_after_a_checkpoint() {
        let dir = tempdir().expect("tempdir");
        let mut handle = saving_handle(dir.path());
        let no_checkpoint = run_async(handle.rewind_to_checkpoint(None)).unwrap_err();
        assert!(no_checkpoint.to_string().contains("No checkpoints yet"));

        let checkpoint = run_async(handle.mark_checkpoint("start", None)).expect("mark");
        assert_eq!(
            (checkpoint.name.as_str(), checkpoint.message_count),
            ("start", 0)
        );
        let nothing = run_async(handle.rewind_to_checkpoint(None)).unwrap_err();
        assert!(
            nothing.to_string().contains("Nothing to rewind"),
            "{nothing}"
        );

        let (_abort, signal) = AgentSessionHandle::new_abort_handle();
        run_async(handle.prompt_with_abort("explore something", signal, |_| {})).expect("turn");
        let turn_len = handle.session.agent.messages().len();
        assert!(turn_len >= 2, "user + assistant");
        let wrong = run_async(handle.rewind_to_checkpoint(Some("nope"))).unwrap_err();
        assert!(wrong.to_string().contains("No checkpoint named 'nope'"));

        let outcome = run_async(handle.rewind_to_checkpoint(Some("start"))).expect("rewind");
        assert_eq!(outcome.collapsed_messages, turn_len);
        assert!(
            handle.session.agent.messages().len() <= 1,
            "at most the rewind report remains in context"
        );

        let path = run_async(async {
            let store = handle.session_store();
            let cx = crate::agent_cx::AgentCx::for_request();
            let inner = store.lock(cx.cx()).await.expect("session lock");
            inner.path.clone()
        })
        .expect("persisted");
        let reopened = run_async(crate::session::Session::open(&path.display().to_string()))
            .expect("reopen the session file");
        assert!(reopened.entries.iter().any(|entry| matches!(
            entry,
            crate::session::SessionEntry::Custom(custom) if custom.custom_type == "rewind"
        )));
    }

    /// OMP `/retry`: the re-sent turn is a sibling of the abandoned one. On
    /// disk both user entries exist; the active path holds only the retry.
    /// Planted negative: an empty session has nothing to retry.
    #[test]
    fn prepare_retry_branches_the_last_turn_and_the_file_keeps_both() {
        let dir = tempdir().expect("tempdir");
        let mut handle = saving_handle(dir.path());
        assert!(
            run_async(handle.prepare_retry()).is_err(),
            "nothing to retry yet"
        );

        let (_abort, signal) = AgentSessionHandle::new_abort_handle();
        run_async(handle.prompt_with_abort("hello", signal, |_| {})).expect("first turn");
        let text = run_async(handle.prepare_retry()).expect("retry plan");
        assert_eq!(text, "hello");
        assert!(
            handle.session.agent.messages().is_empty(),
            "the agent's context no longer holds the abandoned turn"
        );
        let (_abort, signal) = AgentSessionHandle::new_abort_handle();
        run_async(handle.prompt_with_abort(text, signal, |_| {})).expect("retried turn");

        let path = run_async(async {
            let store = handle.session_store();
            let cx = crate::agent_cx::AgentCx::for_request();
            let inner = store.lock(cx.cx()).await.expect("session lock");
            inner.path.clone()
        })
        .expect("persisted");
        let reopened = run_async(crate::session::Session::open(&path.display().to_string()))
            .expect("reopen the session file");
        let users_in_file = reopened
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry,
                    crate::session::SessionEntry::Message(message)
                        if matches!(message.message, crate::session::SessionMessage::User { .. })
                )
            })
            .count();
        assert_eq!(users_in_file, 2, "the abandoned turn stays in the tree");
        let users_on_path = reopened
            .to_messages_for_current_path()
            .iter()
            .filter(|message| matches!(message, crate::model::Message::User(_)))
            .count();
        assert_eq!(users_on_path, 1, "the active path holds only the retry");
    }

    /// bd-9o9i2 criterion 3, and the half that matters for safety rather than
    /// bookkeeping: the raw `Agent` loop does not consult the provider
    /// admission gate, so a continuation could issue against a provider that
    /// had been quarantined — re-billing work against a session record nobody
    /// can describe, which is the exact failure the gate exists to prevent.
    ///
    /// Asserted on the CALL COUNT, not just the error: a continuation that
    /// returned an error after reaching the provider would still have done the
    /// damage.
    #[test]
    fn a_continuation_cannot_issue_against_a_quarantined_provider() {
        let (handle, calls) = flaky_handle(0);
        let mut handle = handle;
        handle
            .session
            .provider_admission_gate()
            .block("quarantined for this test".to_string());

        let result = run_async(handle.continue_turn(|_| {}));

        assert!(
            result.is_err(),
            "a quarantined provider must refuse the continuation"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the provider must never be reached at all"
        );
    }

    /// The half of the policy nothing on this stack had: going BACK. Print mode
    /// and RPC both reinstall the captured primary once its cooldown expires;
    /// without it a session that took a single 429 stays pinned to the fallback
    /// for as long as it runs, which on an interactive surface is hours
    /// (bd-gm481). With a zero cooldown the next prompt restores.
    #[test]
    fn the_captured_primary_is_restored_once_its_cooldown_expires() {
        let mut handle = handle_after_one_failover(0);
        let captured = handle
            .failover_state
            .primary()
            .cloned()
            .expect("a swap records the primary it left");
        assert_eq!(captured.model_id, "claude-3-5-haiku-latest");

        let (seen, shared) = restore_recorder();
        run_async(handle.maybe_restore_primary(&shared)).expect("restore primary");

        assert_eq!(
            handle.session.agent.provider().model_id(),
            "claude-3-5-haiku-latest",
            "the cooldown expired, so the primary must be live again"
        );
        assert!(
            handle.failover_state.primary().is_none(),
            "a restored primary clears the chain state, so a later failure walks the chain from \
             the top instead of resuming past entries it never used"
        );
        let events = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(
            events,
            vec![(true, "claude-3-5-haiku-latest".to_string())],
            "the host must be told the primary is back, or the model indicator lies: {events:?}"
        );
    }

    /// The planted negative for the restoration half: while the cooldown is
    /// still live the fallback stays installed. Restoring early would send the
    /// next prompt straight back into the error that caused the failover, which
    /// is worse than running on a model that works.
    #[test]
    fn a_live_cooldown_keeps_the_fallback_installed() {
        let mut handle = handle_after_one_failover(300);

        let (seen, shared) = restore_recorder();
        run_async(handle.maybe_restore_primary(&shared)).expect("cooldown check");

        assert_eq!(
            handle.session.agent.provider().model_id(),
            "gpt-4o-mini",
            "the cooldown is live, so the fallback must stay installed"
        );
        assert!(
            handle.failover_state.primary().is_some(),
            "the primary must stay captured for a later restoration"
        );
        assert!(
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "nothing was restored, so no restoration must be reported"
        );
    }

    /// The planted negative for the chain half: with no FailoverOptions the
    /// same failure never consults a chain, which is what every embedder had
    /// before and what both interactive stacks did.
    #[test]
    fn without_failover_options_no_chain_is_consulted() {
        let (handle, _calls) = flaky_handle(usize::MAX);
        let mut handle = handle.with_retry(Some(fast_retry_policy(1)));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorder = Arc::clone(&seen);

        let (_abort_handle, abort_signal) = AgentSessionHandle::new_abort_handle();
        let _ = run_async(
            handle.prompt_with_abort("hello", abort_signal, move |event| {
                if matches!(event, AgentEvent::FailoverStart { .. }) {
                    recorder
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push("failover_start".to_string());
                }
            }),
        );

        assert!(
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "no chain configured means no failover"
        );
    }

    /// A retry budget bounds the attempts rather than looping on a provider
    /// that never recovers.
    #[test]
    fn the_retry_budget_bounds_a_provider_that_never_recovers() {
        let (handle, calls) = flaky_handle(usize::MAX);
        let mut handle = handle.with_retry(Some(fast_retry_policy(2)));
        let (_abort_handle, abort_signal) = AgentSessionHandle::new_abort_handle();
        let message = run_async(handle.prompt_with_abort("hello", abort_signal, |_| {}))
            .expect("the exhausted turn still returns its message");

        assert_eq!(message.stop_reason, crate::model::StopReason::Error);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "the first attempt plus max_retries, and no more"
        );
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
    fn sdk_config_loader_uses_session_root_and_workspace_trust() {
        let tmp = tempdir().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        let global_dir = tmp.path().join("global");
        std::fs::create_dir_all(cwd.join(".pi")).expect("create project config dir");
        std::fs::create_dir_all(&global_dir).expect("create global config dir");
        std::fs::write(
            global_dir.join("settings.json"),
            r#"{"defaultThinkingLevel":"low"}"#,
        )
        .expect("write global settings");
        std::fs::write(
            cwd.join(".pi/settings.json"),
            r#"{"defaultThinkingLevel":"high"}"#,
        )
        .expect("write project settings");

        let untrusted =
            load_session_config(&cwd, &global_dir, None, false).expect("load untrusted config");
        assert_eq!(untrusted.default_thinking_level.as_deref(), Some("low"));

        let trusted =
            load_session_config(&cwd, &global_dir, None, true).expect("load trusted config");
        assert_eq!(trusted.default_thinking_level.as_deref(), Some("high"));
    }

    /// Configured secrets settings govern an SDK-built session (the default
    /// FTUI stack): user patterns are obfuscated and block mode refuses,
    /// where the session used to run the built-in default regardless.
    #[test]
    fn sessions_apply_the_configured_secrets_settings() {
        let session_with = |settings: &str| {
            let tmp = tempdir().expect("tempdir");
            std::fs::create_dir_all(tmp.path().join(".pi")).expect("create project config dir");
            std::fs::write(tmp.path().join(".pi/settings.json"), settings)
                .expect("write project settings");
            let mut options = hermetic_session_options(tmp.path());
            options.workspace_trusted = true;
            (
                tmp,
                run_async(create_agent_session(options)).expect("create session"),
            )
        };

        let (_tmp, mut handle) =
            session_with(r#"{"secrets":{"extra_patterns":["ACME-[0-9]{6}"]}}"#);
        let out = handle
            .session_mut()
            .agent
            .secrets_transform_outbound_text("token ACME-123456 here")
            .expect("obfuscate mode");
        assert!(!out.contains("ACME-123456"), "user pattern applied: {out}");

        let (_tmp, mut handle) =
            session_with(r#"{"secrets":{"mode":"block","extra_patterns":["ACME-[0-9]{6}"]}}"#);
        assert!(
            handle
                .session_mut()
                .agent
                .secrets_transform_outbound_text("token ACME-123456 here")
                .is_err(),
            "block mode refuses"
        );
    }

    /// The host's skills block reaches the session's system prompt; without
    /// one (an embedder that loads no skills) nothing is listed.
    #[test]
    fn sessions_list_the_host_provided_skills() {
        let block =
            "\n\n<available_skills>\n  <skill><name>demo-skill</name></skill>\n</available_skills>";
        let prompt_with = |skills_prompt: Option<String>| {
            let tmp = tempdir().expect("tempdir");
            let mut options = hermetic_session_options(tmp.path());
            options.skills_prompt = skills_prompt;
            let handle = run_async(create_agent_session(options)).expect("create session");
            handle
                .session()
                .agent
                .system_prompt()
                .unwrap_or_default()
                .to_string()
        };
        assert!(prompt_with(Some(block.to_string())).contains("demo-skill"));
        assert!(!prompt_with(None).contains("available_skills"));
    }

    #[test]
    fn sessions_use_the_configured_queue_modes() {
        let modes_for = |mode: &str| {
            let tmp = tempdir().expect("tempdir");
            std::fs::create_dir_all(tmp.path().join(".pi")).expect("create project config dir");
            std::fs::write(
                tmp.path().join(".pi/settings.json"),
                format!(r#"{{"steeringMode":"{mode}","followUpMode":"{mode}"}}"#),
            )
            .expect("write project settings");
            let mut options = hermetic_session_options(tmp.path());
            options.workspace_trusted = true;
            let handle = run_async(create_agent_session(options)).expect("create session");
            handle.session().agent.queue_modes()
        };
        assert_eq!(modes_for("all"), (QueueMode::All, QueueMode::All));
        assert_eq!(
            modes_for("one-at-a-time"),
            (QueueMode::OneAtATime, QueueMode::OneAtATime)
        );
    }

    /// A fixture extension that observes and does nothing else.
    fn observing_extension(dir: &Path) -> PathBuf {
        let path = dir.join("observe.mjs");
        std::fs::write(
            &path,
            "export default function (pi) {\n  pi.on(\"message_update\", async () => {});\n}\n",
        )
        .expect("write fixture extension");
        path
    }

    /// An embedder that loads extensions and supplies no runtime gets one
    /// built for the session (bd-8rvry).
    ///
    /// Without it the session works and its extensions observe nothing: the
    /// coalescer `make_combined_callback` installs has nothing to spawn onto,
    /// so `message_*` and `tool_execution_*` are never delivered, with no error
    /// anywhere. That is bd-82331's failure moved from the in-tree surfaces,
    /// which all supply a runtime now, to everyone outside this repo.
    #[test]
    fn extensions_without_a_supplied_runtime_get_one_built_for_them() {
        let tmp = tempdir().expect("tempdir");
        let mut options = hermetic_session_options(tmp.path());
        options.extension_paths = vec![observing_extension(tmp.path())];

        let handle = run_async(create_agent_session(options)).expect("create session");

        assert!(
            handle.session.extensions.is_some(),
            "the fixture extension must load, or this test is asserting nothing"
        );
        assert!(
            handle.event_runtime.is_some(),
            "a session that loads extensions and was given no runtime must build one and own it"
        );
        assert!(
            handle.session.runtime_handle().is_some(),
            "the built runtime must be installed on the session, not merely held: the coalescer \
             reads it from there"
        );
    }

    /// A supplied runtime is used as-is; no second one appears (bd-8rvry).
    #[test]
    fn a_supplied_runtime_is_not_replaced_by_a_built_one() {
        let tmp = tempdir().expect("tempdir");
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime for handle");
        let mut options = hermetic_session_options(tmp.path());
        options.extension_paths = vec![observing_extension(tmp.path())];
        options.runtime_handle = Some(runtime.handle());

        let handle = run_async(create_agent_session(options)).expect("create session");

        assert!(
            handle.event_runtime.is_none(),
            "an embedder that supplied a runtime must not have a second one started behind its \
             back: four worker threads appearing unasked is worse than the problem it solves"
        );
        assert!(
            handle.session.runtime_handle().is_some(),
            "the supplied handle must still be installed"
        );
    }

    /// No extensions, no runtime, nothing allocated (bd-8rvry).
    #[test]
    fn a_session_without_extensions_builds_no_runtime() {
        let tmp = tempdir().expect("tempdir");
        let handle = run_async(create_agent_session(hermetic_session_options(tmp.path())))
            .expect("create session");

        assert!(
            handle.session.extensions.is_none(),
            "the hermetic options must not load extensions, or this test asserts nothing"
        );
        assert!(
            handle.event_runtime.is_none(),
            "a session with nothing to observe must not start worker threads for observation"
        );
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

    /// bd-jtehj: a RELATIVE `SessionOptions::package_dir` must resolve
    /// against the SESSION's working directory rather than the ambient
    /// process cwd, so prompt-advertised paths are the same absolute
    /// locations the session tools read. Mutation-sensitive: before the fix,
    /// prompt discovery ran from the process cwd — where `relpkgs` does not
    /// exist — and the docs block was silently omitted.
    #[test]
    fn create_agent_session_resolves_relative_package_dir_against_working_directory() {
        let _lock = current_dir_lock();
        // Ambient process cwd differs from the session root on purpose.
        let process_cwd = tempdir().expect("process cwd");
        let sdk_cwd = tempdir().expect("sdk cwd");
        let _guard = CurrentDirGuard::new(process_cwd.path());

        let pkg_root = sdk_cwd.path().join("relpkgs");
        std::fs::create_dir_all(pkg_root.join("docs")).expect("mkdir relpkgs/docs");
        std::fs::write(pkg_root.join("README.md"), "# pi\n").expect("write readme");
        std::fs::write(pkg_root.join("docs").join("extensions.md"), "#\n")
            .expect("write extensions.md");

        let handle = run_async(create_agent_session(SessionOptions {
            provider: Some("openai".to_string()),
            model: Some("gpt-4o".to_string()),
            api_key: Some("dummy-key".to_string()),
            working_directory: Some(sdk_cwd.path().to_path_buf()),
            no_session: true,
            // Deliberately relative: stabilization must anchor it to sdk_cwd.
            package_dir: Some(PathBuf::from("relpkgs")),
            ..SessionOptions::default()
        }))
        .expect("create session");
        let system_prompt = handle
            .session()
            .agent
            .system_prompt()
            .expect("default system prompt present")
            .to_string();
        assert!(system_prompt.contains("Pi documentation"));
        let advertised_readme = pkg_root.join("README.md");
        assert!(
            system_prompt.contains(&advertised_readme.display().to_string()),
            "prompt must advertise the session-root-resolved README: {system_prompt}"
        );
        // The prompt's marker paths must be real files reachable through the
        // same root the tools read from.
        assert!(advertised_readme.is_file());
        assert!(pkg_root.join("docs").join("extensions.md").is_file());
        assert!(system_prompt.contains("extensions (docs/extensions.md)"));
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
    fn cycle_thinking_level_walks_the_models_own_levels_and_wraps() {
        let tmp = tempdir().expect("tempdir");
        let options = SessionOptions {
            provider: Some("anthropic".to_string()),
            model: Some("claude-sonnet-4-6".to_string()),
            api_key: Some("dummy-key".to_string()),
            working_directory: Some(tmp.path().to_path_buf()),
            no_session: true,
            ..SessionOptions::default()
        };
        let mut handle = run_async(create_agent_session(options)).expect("create session");
        let levels = handle
            .session()
            .current_model_entry()
            .expect("registry resolves the fixture model")
            .available_thinking_levels();
        assert!(
            levels.len() > 1,
            "fixture must be a reasoning model, got {levels:?}"
        );

        let start = handle.thinking_level().unwrap_or_default();
        let start_index = levels
            .iter()
            .position(|level| *level == start)
            .expect("starting level is one the model offers");

        // The levels a full lap should visit, in order, starting one past the
        // current one and wrapping back around to it.
        let lap = levels
            .iter()
            .copied()
            .cycle()
            .skip(start_index + 1)
            .take(levels.len())
            .collect::<Vec<_>>();
        assert_eq!(lap.len(), levels.len());

        // Every step lands on the next level in the model's own list, the last
        // wraps to the first, and the runtime agrees each time.
        for (step, expected) in lap.into_iter().enumerate() {
            let reported = run_async(handle.cycle_thinking_level()).expect("cycle");
            assert_eq!(reported, Some(expected), "step {step}");
            assert_eq!(handle.thinking_level(), Some(expected), "step {step}");
        }
        assert_eq!(
            handle.thinking_level(),
            Some(start),
            "a full lap must return to where it started"
        );
    }

    #[test]
    fn cycle_thinking_level_reports_nothing_to_cycle_rather_than_moving() {
        // gpt-4o does not reason, so `Off` is the only level it offers. The
        // caller has to be able to tell that apart from a successful cycle —
        // the UI prints "does not support thinking" on `None` — so this must
        // not silently no-op and report a level.
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

        assert_eq!(
            run_async(handle.cycle_thinking_level()).expect("cycle"),
            None
        );
        assert_eq!(
            handle.session().agent.stream_options().thinking_level,
            Some(crate::model::ThinkingLevel::Off),
            "a model with nothing to cycle must be left where it was"
        );
    }

    #[test]
    fn sdk_stream_options_enable_prompt_caching_by_default() {
        let config = crate::config::Config::default();
        let session = crate::session::Session::in_memory();
        let selection = app::ModelSelection {
            model_entry: ModelEntry {
                model: Model {
                    id: "plain-model".to_string(),
                    name: "Plain Model".to_string(),
                    api: "anthropic-messages".to_string(),
                    provider: "anthropic".to_string(),
                    base_url: "https://api.anthropic.com/v1/messages".to_string(),
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
            },
            thinking_level: crate::model::ThinkingLevel::Off,
            scoped_models: Vec::new(),
            fallback_message: None,
        };

        let options = build_stream_options_with_optional_key(&config, None, &selection, &session);

        // SDK embedders must get the same prompt-caching default as the CLI
        // path (`app::build_stream_options`); the assertion tracks the pure
        // env resolver so it stays correct if PI_CACHE_RETENTION is set in
        // the environment running the tests.
        assert_eq!(
            options.cache_retention,
            app::cache_retention_from_env(std::env::var("PI_CACHE_RETENTION").ok().as_deref())
        );
        if std::env::var("PI_CACHE_RETENTION").is_err() {
            assert_eq!(
                options.cache_retention,
                crate::provider::CacheRetention::Short
            );
        }
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

    #[test]
    fn session_options_new_fields_default_to_current_behavior() {
        let options = SessionOptions::default();
        assert!(options.extension_ui_handler.is_none());
        assert!(options.persist_extension_permissions);
        assert!(options.compaction_settings.is_none());
        assert!(options.mcp.is_none());
        assert!(options.extension_flags.is_empty());
    }

    #[test]
    fn create_agent_session_uses_compaction_override_verbatim() {
        let tmp = tempdir().expect("tempdir");
        let custom = ResolvedCompactionSettings {
            enabled: false,
            context_window_tokens: 55_555,
            reserve_tokens: 1_234,
            keep_recent_tokens: 4_321,
            mode: crate::compaction::AutoCompactionMode::default(),
            render_mode: crate::compaction::CompactionRenderMode::default(),
        };
        let options = SessionOptions {
            compaction_settings: Some(custom.clone()),
            ..hermetic_session_options(tmp.path())
        };

        let handle = run_async(create_agent_session(options)).expect("create session");
        let resolved = handle.compaction_settings();
        assert!(!resolved.enabled);
        assert_eq!(resolved.context_window_tokens, custom.context_window_tokens);
        assert_eq!(resolved.reserve_tokens, custom.reserve_tokens);
        assert_eq!(resolved.keep_recent_tokens, custom.keep_recent_tokens);
    }

    #[test]
    fn create_agent_session_derives_compaction_without_override() {
        let tmp = tempdir().expect("tempdir");
        let options = hermetic_session_options(tmp.path());

        let handle = run_async(create_agent_session(options)).expect("create session");
        let resolved = handle.compaction_settings();
        assert!(
            resolved.context_window_tokens > 0,
            "derived settings must resolve a context window"
        );
    }

    #[test]
    fn replacement_mcp_activation_requires_proven_predecessor_release() {
        let no_predecessor_manager = SessionResourceShutdown::default();
        assert!(no_predecessor_manager.permits_replacement_mcp_activation());

        let released = SessionResourceShutdown {
            mcp: McpShutdownOutcome::Released,
            ..Default::default()
        };
        assert!(released.permits_replacement_mcp_activation());

        let mut indeterminate = SessionResourceShutdown {
            mcp: McpShutdownOutcome::Indeterminate,
            ..Default::default()
        };
        indeterminate.fail(String::from("MCP shutdown timed out"));
        assert!(!indeterminate.permits_replacement_mcp_activation());
        assert!(!indeterminate.completed_cleanly());
    }

    #[test]
    fn shutdown_skips_autosave_for_ephemeral_session() {
        let tmp = tempdir().expect("tempdir");
        let warnings = run_async(async {
            let handle = create_agent_session(hermetic_session_options(tmp.path()))
                .await
                .expect("create session");
            let cx = crate::agent_cx::AgentCx::for_request();
            {
                let mut session = handle
                    .session
                    .session
                    .lock(cx.cx())
                    .await
                    .expect("lock session");
                session
                    .set_autosave_durability_mode(crate::session::AutosaveDurabilityMode::Strict);
                session.append_session_info(Some("ephemeral mutation".to_string()));
            }
            handle.shutdown_owned_resources().await
        });

        assert!(
            warnings.is_empty(),
            "ephemeral sessions have no persistence path and must not attempt autosave: {warnings:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_stops_background_jobs_owned_by_the_session() {
        let tmp = tempdir().expect("tempdir");
        let (warnings, session_id, job_id) = run_async(async {
            let handle = create_agent_session(hermetic_session_options(tmp.path()))
                .await
                .expect("create session");
            let cx = crate::agent_cx::AgentCx::for_request();
            let session_id = handle
                .session
                .session
                .lock(cx.cx())
                .await
                .expect("lock session")
                .header
                .id
                .clone();
            let job = crate::jobs::spawn_background(
                &session_id,
                tmp.path(),
                None,
                None,
                "sleep 300",
                Some(300),
                None,
            )
            .expect("spawn session-owned background job");
            let warnings = handle.shutdown_owned_resources().await;
            (warnings, session_id, job.id)
        });

        assert!(
            warnings.is_empty(),
            "clean session shutdown must not report warnings: {warnings:?}"
        );
        let jobs = crate::jobs::list(&session_id).expect("list session jobs");
        assert!(jobs.iter().any(|job| {
            job.id == job_id && job.status == crate::jobs::JobStatus::Killed.as_str()
        }));
        let _ = crate::jobs::take_completion_notices(&session_id);
    }

    struct SdkRecordingUiHandler {
        prompts: Mutex<Vec<ExtensionUiRequest>>,
    }

    #[async_trait::async_trait]
    impl ExtensionUiHandler for SdkRecordingUiHandler {
        async fn request_ui(
            &self,
            request: ExtensionUiRequest,
        ) -> Result<Option<ExtensionUiResponse>> {
            let id = request.id.clone();
            self.prompts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request);
            Ok(Some(ExtensionUiResponse {
                id,
                value: Some(Value::Bool(true)),
                cancelled: false,
            }))
        }
    }

    #[test]
    fn create_agent_session_wires_extension_ui_handler_before_startup() {
        let tmp = tempdir().expect("tempdir");
        let ext_path = tmp.path().join("noop_ext.js");
        std::fs::write(
            &ext_path,
            r#"
export default function init(pi) {
    pi.on("startup", async (_event, ctx) => {
        await ctx.ui.confirm("Startup prompt", "Allow startup initialization?");
    });
}
"#,
        )
        .expect("write extension");

        let handler = Arc::new(SdkRecordingUiHandler {
            prompts: Mutex::new(Vec::new()),
        });
        let options = SessionOptions {
            extension_paths: vec![ext_path],
            extension_ui_handler: Some(handler.clone()),
            persist_extension_permissions: false,
            ..hermetic_session_options(tmp.path())
        };

        let handle = run_async(create_agent_session(options)).expect("create session");
        let manager = handle.extension_manager().expect("extensions loaded");
        assert!(
            !manager.policy_prompt_persistence(),
            "persist_extension_permissions: false must scope decisions to the session"
        );

        {
            let prompts = handler
                .prompts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(
                prompts.len(),
                1,
                "the UI bridge must be installed before the startup hook runs"
            );
            assert_eq!(prompts[0].method, "confirm");
            assert_eq!(prompts[0].payload["title"], "Startup prompt");
        }

        let response = run_async(async {
            manager
                .request_ui(ExtensionUiRequest::new(
                    "",
                    "confirm",
                    serde_json::json!({ "title": "Allow?", "message": "capability" }),
                ))
                .await
        })
        .expect("request_ui")
        .expect("handler response");
        assert_eq!(response.value, Some(Value::Bool(true)));

        let prompts = handler
            .prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(prompts.len(), 2, "handler must receive both UI requests");
        assert_eq!(prompts[1].method, "confirm");
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
    #[test]
    fn session_without_model_ask_still_exposes_host_picker() {
        let tmp = tempdir().expect("tempdir");
        let mut options = hermetic_session_options(tmp.path());
        options.enabled_tools = Some(vec!["read".to_string()]);
        let handle = run_async(create_agent_session(options)).expect("create session");
        assert!(!handle.has_tool("ask"));
        assert!(handle.ask_tool().is_some());
    }
}
