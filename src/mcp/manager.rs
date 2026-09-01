//! MCP client manager: one registry unifying file-configured, foreign, and
//! extension-registered servers; trust-gated connections; bounded restart
//! with backoff; tool-list caching (bd-cv653.6.1).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::config::{ConfiguredServer, McpDiscovery, Provenance};
use super::transport::{DEFAULT_MCP_TIMEOUT, MCP_PROTOCOL_VERSION, McpTransport};
use super::trust::{TrustDecision, TrustStore, TrustWriteGuard};
use crate::error::{Error, Result};

#[cfg(test)]
type TestTransportFactory = dyn Fn() -> Box<dyn McpTransport> + Send + Sync;
#[cfg(test)]
type StartupAfterGenerationHook = dyn Fn() + Send + Sync;

/// Global deadline for eager startup connects (all trusted servers in
/// parallel; stragglers land as `Unhealthy` and are retried via `/mcp test`).
const STARTUP_CONNECT_BUDGET: Duration = Duration::from_secs(8);
/// Tool-list cache TTL.
const TOOL_CACHE_TTL: Duration = Duration::from_secs(300);
/// Max automatic restarts after a crash before the server is `Failed`.
const MAX_RESTARTS: u32 = 3;
/// Bound untrusted `tools/list` metadata before it reaches provider schemas.
const MAX_SERVER_TOOLS: usize = 1024;
const MAX_TOOL_NAME_BYTES: usize = 1024;
const MAX_TOOL_DESCRIPTION_BYTES: usize = 64 * 1024;

fn tool_err(code: &str, message: impl Into<String>) -> Error {
    Error::tool("mcp", format!("[{code}] {}", message.into()))
}

fn optional_string(spec: &Value, field: &str) -> std::result::Result<Option<String>, String> {
    spec.get(field)
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("field {field:?} must be a string"))
        })
        .transpose()
}

fn optional_string_array(spec: &Value, field: &str) -> std::result::Result<Vec<String>, String> {
    let Some(value) = spec.get(field) else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| format!("field {field:?} must be an array of strings"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("field {field:?} must contain only strings"))
        })
        .collect()
}

fn optional_string_map(
    spec: &Value,
    field: &str,
) -> std::result::Result<Vec<(String, String)>, String> {
    let Some(value) = spec.get(field) else {
        return Ok(Vec::new());
    };
    let values = value
        .as_object()
        .ok_or_else(|| format!("field {field:?} must be an object of string values"))?;
    values
        .iter()
        .map(|(name, value)| {
            value
                .as_str()
                .map(|value| (name.clone(), value.to_string()))
                .ok_or_else(|| format!("field {field:?} entry {name:?} must be a string"))
        })
        .collect()
}

fn parse_tool_list(result: &Value) -> Result<Vec<McpToolMeta>> {
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            tool_err(
                "MCP_PROTOCOL",
                "tools/list result must contain a tools array",
            )
        })?;
    if tools.len() > MAX_SERVER_TOOLS {
        return Err(tool_err(
            "MCP_PROTOCOL",
            format!(
                "tools/list returned {} tools; at most {MAX_SERVER_TOOLS} are accepted",
                tools.len()
            ),
        ));
    }

    let mut names = std::collections::HashSet::with_capacity(tools.len());
    let mut parsed = Vec::with_capacity(tools.len());
    for (index, tool) in tools.iter().enumerate() {
        let tool = tool.as_object().ok_or_else(|| {
            tool_err(
                "MCP_PROTOCOL",
                format!("tools/list entry {index} must be an object"),
            )
        })?;
        let name = tool.get("name").and_then(Value::as_str).ok_or_else(|| {
            tool_err(
                "MCP_PROTOCOL",
                format!("tools/list entry {index} must have a string name"),
            )
        })?;
        if name.is_empty() || name.len() > MAX_TOOL_NAME_BYTES {
            return Err(tool_err(
                "MCP_PROTOCOL",
                format!(
                    "tools/list entry {index} name must contain 1 to {MAX_TOOL_NAME_BYTES} bytes"
                ),
            ));
        }
        if !names.insert(name) {
            return Err(tool_err(
                "MCP_PROTOCOL",
                format!("tools/list contains duplicate tool name {name:?}"),
            ));
        }
        let description = match tool.get("description") {
            None => "",
            Some(description) => description.as_str().ok_or_else(|| {
                tool_err(
                    "MCP_PROTOCOL",
                    format!("tools/list entry {index} description must be a string"),
                )
            })?,
        };
        if description.len() > MAX_TOOL_DESCRIPTION_BYTES {
            return Err(tool_err(
                "MCP_PROTOCOL",
                format!(
                    "tools/list entry {index} description exceeds {MAX_TOOL_DESCRIPTION_BYTES} bytes"
                ),
            ));
        }
        let input_schema = tool
            .get("inputSchema")
            .filter(|schema| schema.is_object())
            .cloned()
            .ok_or_else(|| {
                tool_err(
                    "MCP_PROTOCOL",
                    format!("tools/list entry {index} must have an object inputSchema"),
                )
            })?;
        parsed.push(McpToolMeta {
            name: name.to_string(),
            description: description.to_string(),
            input_schema,
        });
    }
    Ok(parsed)
}

/// One server's advertised tool (from `tools/list`).
#[derive(Debug, Clone)]
pub struct McpToolMeta {
    /// Tool name as the server calls it.
    pub name: String,
    /// Server-provided description.
    pub description: String,
    /// JSON Schema for the tool input.
    pub input_schema: Value,
}

/// Runtime health for the `/mcp` view.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ServerHealth {
    /// Never connected this session.
    NotStarted,
    /// Connected and tool list cached.
    Ready { tools: usize },
    /// Crashed; will retry after the backoff elapses.
    Unhealthy { reason: String, retries: u32 },
    /// Exceeded restart budget; manual `/mcp test` revives.
    Failed { reason: String },
}

impl ServerHealth {
    fn label(&self) -> String {
        match self {
            Self::NotStarted => "not started".to_string(),
            Self::Ready { tools } => format!("ready ({tools} tools)"),
            Self::Unhealthy { reason, retries } => {
                format!("unhealthy (retry {retries}/{MAX_RESTARTS}): {reason}")
            }
            Self::Failed { reason } => format!("failed: {reason}"),
        }
    }
}

/// Restart bookkeeping for one server.
#[derive(Debug, Default)]
struct RestartState {
    count: u32,
    next_retry_at: Option<Instant>,
}

/// One registered server.
struct ServerEntry {
    config: ConfiguredServer,
    connect_lane: Arc<asupersync::sync::Mutex<()>>,
    transport: Mutex<Option<Arc<dyn McpTransport>>>,
    tools_cache: Mutex<Option<(Instant, Vec<McpToolMeta>)>>,
    health: Mutex<ServerHealth>,
    restarts: Mutex<RestartState>,
    /// bd-hyik7: extension-supplied working-directory override for this
    /// server; relative spec values anchor to the manager cwd at
    /// registration so the bound identity stays deterministic.
    cwd_override: Option<PathBuf>,
}

impl ServerEntry {
    /// The working directory every execution-relevant decision (fingerprint,
    /// identity, spawn) must use for this entry (bd-hyik7).
    fn effective_cwd(&self, manager_default: &Path) -> PathBuf {
        self.cwd_override
            .clone()
            .unwrap_or_else(|| manager_default.to_path_buf())
    }
}

/// Owns a transport until its initialize handshake is published. Dropping a
/// cancelled handshake must synchronously abort the private child/request;
/// otherwise a startup deadline could return while leaving work unreachable
/// from the manager's published transport slot.
struct PrivateHandshakeTransport {
    transport: Arc<dyn McpTransport>,
    armed: bool,
}

impl PrivateHandshakeTransport {
    fn new(transport: Arc<dyn McpTransport>) -> Self {
        Self {
            transport,
            armed: true,
        }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PrivateHandshakeTransport {
    fn drop(&mut self) {
        if self.armed {
            self.transport.abort();
        }
    }
}

/// Marks synchronous transport construction as abandoned when the async
/// caller is cancelled. Blocking work cannot be forcibly stopped, but it can
/// be prevented from spawning or publishing a transport after its deadline.
struct TransportConstructionAttempt {
    abandoned: Arc<AtomicBool>,
    armed: bool,
}

impl TransportConstructionAttempt {
    const fn new(abandoned: Arc<AtomicBool>) -> Self {
        Self {
            abandoned,
            armed: true,
        }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TransportConstructionAttempt {
    fn drop(&mut self) {
        if self.armed {
            self.abandoned.store(true, Ordering::Release);
        }
    }
}

/// A `/mcp list` row.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub name: String,
    pub target: String,
    pub provenance: String,
    pub trust: String,
    pub health: String,
    pub tools: usize,
    pub source_file: PathBuf,
}

/// The client registry. Cheap to clone (shared inner state).
pub struct McpManager {
    inner: Arc<McpManagerInner>,
}

struct McpManagerInner {
    cwd: PathBuf,
    servers: Mutex<HashMap<String, Arc<ServerEntry>>>,
    trust_path: PathBuf,
    trust_lock: Mutex<()>,
    shutting_down: AtomicBool,
    warnings: Vec<super::config::ConfigWarning>,
    #[cfg(test)]
    transport_factory: Mutex<Option<Arc<TestTransportFactory>>>,
    #[cfg(test)]
    startup_after_generation_hook: Mutex<Option<Arc<StartupAfterGenerationHook>>>,
}

impl McpManager {
    /// Build from discovery (no connections yet).
    #[must_use]
    pub fn new(cwd: &Path, global_dir: &Path, discovery: McpDiscovery) -> Self {
        let servers = discovery
            .servers
            .into_iter()
            .map(|config| {
                let entry = Arc::new(ServerEntry {
                    config,
                    cwd_override: None,
                    connect_lane: Arc::new(asupersync::sync::Mutex::new(())),
                    transport: Mutex::new(None),
                    tools_cache: Mutex::new(None),
                    health: Mutex::new(ServerHealth::NotStarted),
                    restarts: Mutex::new(RestartState::default()),
                });
                (entry.config.name.clone(), entry)
            })
            .collect();
        Self {
            inner: Arc::new(McpManagerInner {
                cwd: cwd.to_path_buf(),
                servers: Mutex::new(servers),
                trust_path: global_dir.join("mcp-trust.json"),
                trust_lock: Mutex::new(()),
                shutting_down: AtomicBool::new(false),
                warnings: discovery.warnings,
                #[cfg(test)]
                transport_factory: Mutex::new(None),
                #[cfg(test)]
                startup_after_generation_hook: Mutex::new(None),
            }),
        }
    }

    /// Discover + build in one step.
    ///
    /// # Errors
    ///
    /// Never fails on discovery problems (warnings are collected); the
    /// `Result` is for forward compatibility.
    pub fn bootstrap(cwd: &Path, global_dir: &Path, cli_paths: &[PathBuf]) -> Result<Self> {
        let discovery = super::config::discover(cwd, global_dir, cli_paths);
        Ok(Self::new(cwd, global_dir, discovery))
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn trust_store(&self) -> Result<TrustStore> {
        let _guard = Self::lock(&self.inner.trust_lock);
        TrustStore::load(&self.inner.trust_path)
    }

    fn trust_fingerprint(&self, config: &ConfiguredServer) -> String {
        config.fingerprint(&self.inner.cwd)
    }

    /// bd-hyik7: fingerprint honoring a per-entry cwd override (extension
    /// servers may carry their own working directory; the fingerprint binds
    /// every execution-relevant field, cwd included). Prefer this whenever
    /// the caller holds the [`ServerEntry`].
    fn trust_fingerprint_for(&self, entry: &Arc<ServerEntry>) -> String {
        entry
            .config
            .fingerprint(&entry.effective_cwd(&self.inner.cwd))
    }

    fn check_running(&self) -> Result<()> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            Err(tool_err(
                "MCP_MANAGER_SHUTDOWN",
                "this MCP session is shutting down and cannot start or use transports",
            ))
        } else {
            Ok(())
        }
    }

    /// Config warnings collected during discovery (for `/mcp`).
    #[must_use]
    pub fn warnings(&self) -> &[super::config::ConfigWarning] {
        &self.inner.warnings
    }

    /// Current listing (sync; never connects).
    #[must_use]
    pub fn list(&self) -> Vec<ServerInfo> {
        let store = TrustStore::load(&self.inner.trust_path).unwrap_or_else(|_| {
            TrustStore::load(Path::new("/nonexistent-mcp-trust")).expect("empty store")
        });
        let servers = Self::lock(&self.inner.servers).clone();
        let mut rows: Vec<ServerInfo> = servers
            .values()
            .map(|entry| {
                let config = &entry.config;
                let fingerprint = self.trust_fingerprint_for(entry);
                let decision = store.decision(&config.name, &fingerprint);
                let trust = match decision {
                    TrustDecision::Acknowledged => "acknowledged",
                    TrustDecision::Pending => "pending",
                    TrustDecision::Denied => "denied",
                };
                let health = Self::lock(&entry.health).clone();
                let tools = Self::lock(&entry.tools_cache)
                    .as_ref()
                    .map_or(0, |(_, tools)| tools.len());
                // Targets are untrusted configuration and may contain literal
                // credentials in argv, URL userinfo, or query parameters. The
                // trust fingerprint binds the exact bytes; the status surface
                // needs only the transport shape.
                let target = if config.is_http() {
                    "<http>"
                } else if config.command.is_some() {
                    "<stdio>"
                } else {
                    "<none>"
                }
                .to_string();
                ServerInfo {
                    name: config.name.clone(),
                    target,
                    provenance: config.provenance.label().to_string(),
                    trust: trust.to_string(),
                    health: health.label(),
                    tools,
                    source_file: config.source_file.clone(),
                }
            })
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// Acknowledge a server (operator trust decision, audited) and eagerly
    /// connect so its tools are usable immediately.
    ///
    /// # Errors
    ///
    /// Fails when the server is unknown, the trust store cannot persist, or
    /// the eager connect fails (the trust decision still stands).
    pub async fn trust(&self, name: &str) -> Result<Vec<McpToolMeta>> {
        let entry = self.entry(name)?;
        let fingerprint = self.trust_fingerprint_for(&entry);
        {
            let _guard = Self::lock(&self.inner.trust_lock);
            // bd-sp5o3: resolve and bind the CANONICAL executable identity
            // the operator is approving — never ack on the raw string alone.
            let identity = entry
                .config
                .execution_identity(&entry.effective_cwd(&self.inner.cwd))
                .map_err(|err| {
                    tool_err(
                        "MCP_TRUST_UNRESOLVED",
                        format!(
                            "cannot bind an executable for {:?}: {err}; \
                             fix the command, then /mcp trust {name} again",
                            entry.config.name
                        ),
                    )
                })?;
            let mut store = TrustStore::load(&self.inner.trust_path)?;
            if let Some(execution) = &identity {
                store.acknowledge_execution(name, &fingerprint, "operator", execution.clone())?;
            } else {
                // HTTP transport: nothing local executes.
                store.acknowledge(name, &fingerprint, "operator")?;
            }
        }
        self.connect_and_list(&entry).await
    }

    /// Deny a server (fail-closed; kills this manager's live connection).
    ///
    /// The durable store is shared across processes, but transport handles are
    /// process-local. Peer managers therefore re-read trust before every spawn,
    /// handshake publication, tools/list, tools/call, and cache exposure. A
    /// request already accepted by a server in the unavoidable interval between
    /// its final pre-request check and the persisted denial cannot be recalled;
    /// its response is rejected by the post-request check and that manager's
    /// transport is closed.
    ///
    /// # Errors
    ///
    /// Fails when the server is unknown or the store cannot persist.
    pub async fn deny(&self, name: &str) -> Result<()> {
        let entry = self.entry(name)?;
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let _connect_guard =
            asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&entry.connect_lane), cx.cx())
                .await
                .map_err(|_| tool_err("MCP_CANCELLED", "cancelled while denying server"))?;
        let fingerprint = self.trust_fingerprint_for(&entry);
        {
            let _guard = Self::lock(&self.inner.trust_lock);
            let mut store = TrustStore::load(&self.inner.trust_path)?;
            store.deny(name, &fingerprint, "operator")?;
        }
        let transport = { Self::lock(&entry.transport).take() };
        if let Some(transport) = transport {
            transport.close().await;
        }
        *Self::lock(&entry.health) = ServerHealth::NotStarted;
        Self::lock(&entry.tools_cache).take();
        Ok(())
    }

    /// bd-vjfol: tear down every live transport and await child exit.
    ///
    /// FTUI `/new` and `/resume` must guarantee singleton shutdown before
    /// swapping manager handles: dropping the old manager only aborts
    /// transports without awaiting stdio children. This terminally seals the
    /// manager, synchronously aborts every published transport, waits for each
    /// private connection attempt to leave its connection lane, then awaits
    /// all published `close()` futures together. One slow child therefore
    /// cannot serialize unrelated close operations, and no private handshake
    /// can outlive the returned future.
    ///
    /// Runtime-only teardown: persisted trust, definitions, provenance, and
    /// on-disk caches are untouched; per-entry slots reset to `NotStarted`
    /// exactly like [`Self::deny`] does for a single server. Returns the
    /// names of transports that were actually closed (empty when idle). A
    /// manager cannot be restarted after this call; construct a replacement
    /// session manager instead.
    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_in_scrutinee)]
    pub async fn shutdown_all(&self) -> Vec<String> {
        self.inner.shutting_down.store(true, Ordering::Release);
        let servers = Self::lock(&self.inner.servers).clone();
        let mut detached: Vec<(String, Arc<dyn McpTransport>)> = Vec::new();
        for (name, entry) in &servers {
            if let Some(transport) = Self::lock(&entry.transport).take() {
                // Abort immediately so in-flight stdio/HTTP requests observe
                // shutdown while connection lanes drain below.
                transport.abort();
                detached.push((name.clone(), transport));
            }
            *Self::lock(&entry.health) = ServerHealth::NotStarted;
            Self::lock(&entry.tools_cache).take();
        }

        // A connection under construction stays private until its handshake
        // succeeds, so it cannot be detached above. Every constructor holds
        // this lane through publication; terminal state prevents publication,
        // and this barrier waits until the private transport has closed.
        let lane_guards = futures::future::join_all(servers.values().map(|entry| {
            let lane = Arc::clone(&entry.connect_lane);
            async move {
                let cx = crate::agent_cx::AgentCx::for_request();
                asupersync::sync::OwnedMutexGuard::lock(lane, cx.cx()).await
            }
        }))
        .await;

        // Reset every entry, including an inconsistent cache-only state. The
        // terminal publication check makes a transport here impossible in
        // production, but taking it keeps teardown fail-closed under races.
        for (name, entry) in &servers {
            if let Some(transport) = Self::lock(&entry.transport).take() {
                transport.abort();
                detached.push((name.clone(), transport));
            }
            *Self::lock(&entry.health) = ServerHealth::NotStarted;
            Self::lock(&entry.tools_cache).take();
        }
        drop(lane_guards);

        futures::future::join_all(detached.into_iter().map(|(name, transport)| async move {
            transport.close().await;
            name
        }))
        .await
    }

    /// Ping + tool list (the `/mcp test` surface).
    ///
    /// # Errors
    ///
    /// Fails closed on trust denial/pending or any transport error.
    pub async fn test(&self, name: &str) -> Result<Vec<McpToolMeta>> {
        self.check_running()?;
        let entry = self.entry(name)?;
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let connect_guard =
            asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&entry.connect_lane), cx.cx())
                .await
                .map_err(|_| tool_err("MCP_CANCELLED", "cancelled while testing server"))?;
        *Self::lock(&entry.restarts) = RestartState::default();
        *Self::lock(&entry.health) = ServerHealth::NotStarted;
        self.ensure_ready_in_lane(&entry).await?;
        let result = self.list_and_cache_tools_in_lane(&entry).await;
        drop(connect_guard);
        result
    }

    fn entry(&self, name: &str) -> Result<Arc<ServerEntry>> {
        Self::lock(&self.inner.servers)
            .get(name)
            .cloned()
            .ok_or_else(|| {
                tool_err(
                    "MCP_UNKNOWN_SERVER",
                    format!("no MCP server named {name:?} (see /mcp list)"),
                )
            })
    }

    /// Connect (trust-gated, restart-budgeted) and return the tool list.
    async fn connect_and_list(&self, entry: &Arc<ServerEntry>) -> Result<Vec<McpToolMeta>> {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let connect_guard =
            asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&entry.connect_lane), cx.cx())
                .await
                .map_err(|_| tool_err("MCP_CANCELLED", "cancelled while connecting server"))?;
        self.ensure_ready_in_lane(entry).await?;
        let result = self.list_and_cache_tools_in_lane(entry).await;
        drop(connect_guard);
        result
    }

    #[cfg(test)]
    async fn list_and_cache_tools(&self, entry: &Arc<ServerEntry>) -> Result<Vec<McpToolMeta>> {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let connect_guard =
            asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&entry.connect_lane), cx.cx())
                .await
                .map_err(|_| tool_err("MCP_CANCELLED", "cancelled while listing server tools"))?;
        let result = self.list_and_cache_tools_in_lane(entry).await;
        drop(connect_guard);
        result
    }

    async fn list_and_cache_tools_in_lane(
        &self,
        entry: &Arc<ServerEntry>,
    ) -> Result<Vec<McpToolMeta>> {
        self.check_running()?;
        self.check_trust(entry)?;
        let transport = Self::lock(&entry.transport)
            .clone()
            .ok_or_else(|| tool_err("MCP_TRANSPORT_CLOSED", "not connected"))?;
        if !Self::clear_tools_cache_if_current(entry, &transport) {
            return Err(tool_err(
                "MCP_TRANSPORT_SUPERSEDED",
                "connection changed before tools/list dispatch",
            ));
        }
        let result = match transport
            .request("tools/list", serde_json::json!({}), DEFAULT_MCP_TIMEOUT)
            .await
        {
            Ok(result) => result,
            Err(err) => {
                Self::fail_transport_generation(entry, &transport, &err);
                return Err(err);
            }
        };
        self.check_running()?;
        if let Err(err) = self.check_trust(entry) {
            Self::close_revoked_transport(entry, &transport).await;
            return Err(err);
        }
        let tools = match parse_tool_list(&result) {
            Ok(tools) => tools,
            Err(err) => {
                Self::fail_transport_generation(entry, &transport, &err);
                return Err(err);
            }
        };
        // Narrow the final cross-process revocation window immediately before
        // publishing schemas, then bind publication to this exact connection.
        if let Err(err) = self.check_trust(entry) {
            Self::close_revoked_transport(entry, &transport).await;
            return Err(err);
        }
        if !self.publish_tools_if_current(entry, &transport, &tools) {
            transport.abort();
            return Err(tool_err(
                "MCP_TRANSPORT_SUPERSEDED",
                "connection changed before tools/list publication",
            ));
        }
        Ok(tools)
    }

    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn clear_tools_cache_if_current(
        entry: &Arc<ServerEntry>,
        transport: &Arc<dyn McpTransport>,
    ) -> bool {
        let current = Self::lock(&entry.transport);
        if !current
            .as_ref()
            .is_some_and(|candidate| Arc::ptr_eq(candidate, transport))
        {
            return false;
        }
        Self::lock(&entry.tools_cache).take();
        true
    }

    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn publish_tools_if_current(
        &self,
        entry: &Arc<ServerEntry>,
        transport: &Arc<dyn McpTransport>,
        tools: &[McpToolMeta],
    ) -> bool {
        let current = Self::lock(&entry.transport);
        if self.inner.shutting_down.load(Ordering::Acquire)
            || !current
                .as_ref()
                .is_some_and(|candidate| Arc::ptr_eq(candidate, transport))
        {
            return false;
        }
        *Self::lock(&entry.tools_cache) = Some((Instant::now(), tools.to_vec()));
        *Self::lock(&entry.health) = ServerHealth::Ready { tools: tools.len() };
        true
    }

    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn fail_transport_generation(
        entry: &Arc<ServerEntry>,
        transport: &Arc<dyn McpTransport>,
        failure: &Error,
    ) -> bool {
        let removed = {
            let mut current = Self::lock(&entry.transport);
            if current
                .as_ref()
                .is_some_and(|candidate| Arc::ptr_eq(candidate, transport))
            {
                let removed = current.take();
                Self::record_failure(entry, failure);
                removed
            } else {
                None
            }
        };
        transport.abort();
        removed.is_some()
    }

    /// Ensure a live, initialized transport for the server.
    ///
    /// Restart discipline: a transport failure reconnects for subsequent
    /// calls, but an in-flight `tools/call` is never replayed because delivery
    /// may already have occurred. A crash loop engages the budget: failed
    /// spawn/handshake attempts back off immediately, while runtime crashes
    /// allow one immediate reconnect before backoff. Only a successful
    /// `tools/call` resets the consecutive-failure counter; `MAX_RESTARTS`
    /// failures mark the server `Failed` until `/mcp test` revives it.
    /// Trust gate (fail-closed with a named remedy).
    fn check_trust(&self, entry: &Arc<ServerEntry>) -> Result<()> {
        let decision = self
            .trust_store()?
            .decision(&entry.config.name, &self.trust_fingerprint_for(entry));
        match decision {
            TrustDecision::Acknowledged => Ok(()),
            TrustDecision::Pending => Err(tool_err(
                "MCP_TRUST_PENDING",
                format!(
                    "server {:?} is pending trust; inspect its source config, then run /mcp trust {} to allow it",
                    entry.config.name, entry.config.name,
                ),
            )),
            TrustDecision::Denied => Err(tool_err(
                "MCP_TRUST_DENIED",
                format!(
                    "server {:?} was denied by the operator and will never spawn; /mcp trust {} after editing resets the decision",
                    entry.config.name, entry.config.name
                ),
            )),
        }
    }

    fn lock_trust_for_execution(
        config: &ConfiguredServer,
        cwd: &Path,
        trust_path: &Path,
    ) -> Result<TrustWriteGuard> {
        let fingerprint = config.fingerprint(cwd);
        let mut store = TrustStore::load(trust_path)?;
        let (decision, guard) = store.locked_decision(&config.name, &fingerprint)?;

        // bd-sp5o3: the acknowledged decision covers the RESOLVED executable,
        // not the raw command string. At this final pre-spawn seam, under the
        // cross-process trust lock and BEFORE any process is created, re-
        // derive the identity and compare. PATH swaps, symlink retargets, or
        // in-place content replacement all fail closed here instead of
        // executing substituted code.
        if matches!(decision, TrustDecision::Acknowledged) && config.command.is_some() {
            let current = config.execution_identity(cwd).map_err(|err| {
                tool_err(
                    "MCP_TRUST_IDENTITY_UNRESOLVED",
                    format!(
                        "server {:?} can no longer be resolved to its trusted \
                         executable before spawn: {err}",
                        config.name
                    ),
                )
            })?;
            let stored = store.acknowledged_execution(&config.name, &fingerprint);
            // stdio under an Acknowledged decision always yields Some here;
            // anything else is a vanished surface, not a bypass.
            let Some(current) = current else {
                return Err(tool_err(
                    "MCP_TRUST_IDENTITY_UNRESOLVED",
                    format!(
                        "server {:?} resolved to no executable right before spawn",
                        config.name
                    ),
                ));
            };
            match stored {
                Some(bound) if bound == current => {}
                Some(bound) => {
                    return Err(tool_err(
                        "MCP_TRUST_IDENTITY_MISMATCH",
                        format!(
                            "server {:?} now resolves to a different executable \
                             than the operator approved; refusing to spawn.\n  \
                             approved: {bound}\n  current:  {current}",
                            config.name
                        ),
                    ));
                }
                None => {
                    return Err(tool_err(
                        "MCP_TRUST_IDENTITY_MISSING",
                        format!(
                            "server {:?} has an acknowledgement that predates \
                             execution binding (or lost it); run /mcp trust {} \
                             again after inspecting its source config",
                            config.name, config.name
                        ),
                    ));
                }
            }
        }

        match decision {
            TrustDecision::Acknowledged => Ok(guard),
            TrustDecision::Pending => Err(tool_err(
                "MCP_TRUST_PENDING",
                format!(
                    "server {:?} became pending before local execution; inspect its source config, then run /mcp trust {} to allow it",
                    config.name, config.name,
                ),
            )),
            TrustDecision::Denied => Err(tool_err(
                "MCP_TRUST_DENIED",
                format!(
                    "server {:?} was denied before local execution and will not run",
                    config.name
                ),
            )),
        }
    }

    /// Restart budget: refuse inside the backoff window or when exhausted.
    /// The first-ever connect has no state and proceeds.
    fn check_restart_budget(entry: &Arc<ServerEntry>) -> Result<()> {
        let (count, next_retry_at) = {
            let restarts = Self::lock(&entry.restarts);
            (restarts.count, restarts.next_retry_at)
        };
        if count >= MAX_RESTARTS {
            *Self::lock(&entry.health) = ServerHealth::Failed {
                reason: format!("exceeded {MAX_RESTARTS} consecutive failures"),
            };
            return Err(tool_err(
                "MCP_RESTART_EXHAUSTED",
                format!(
                    "server {:?} failed {} times in a row; fix it, then /mcp test {}",
                    entry.config.name, count, entry.config.name
                ),
            ));
        }
        if let Some(next) = next_retry_at {
            let now = Instant::now();
            if now < next {
                *Self::lock(&entry.health) = ServerHealth::Unhealthy {
                    reason: "in restart backoff".to_string(),
                    retries: count,
                };
                return Err(tool_err(
                    "MCP_BACKOFF",
                    format!(
                        "server {:?} is in restart backoff for {:.0}s more",
                        entry.config.name,
                        (next - now).as_secs_f32()
                    ),
                ));
            }
        }
        Ok(())
    }

    async fn ensure_ready(&self, entry: &Arc<ServerEntry>) -> Result<()> {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let _connect_guard =
            asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&entry.connect_lane), cx.cx())
                .await
                .map_err(|_| tool_err("MCP_CANCELLED", "cancelled while connecting server"))?;

        self.ensure_ready_in_lane(entry).await
    }

    #[allow(clippy::too_many_lines)]
    async fn ensure_ready_in_lane(&self, entry: &Arc<ServerEntry>) -> Result<()> {
        self.check_running()?;
        if let Err(err) = self.check_trust(entry) {
            let transport = { Self::lock(&entry.transport).take() };
            if let Some(transport) = transport {
                transport.close().await;
            }
            *Self::lock(&entry.health) = ServerHealth::NotStarted;
            Self::lock(&entry.tools_cache).take();
            return Err(err);
        }

        let existing = { Self::lock(&entry.transport).clone() };
        if let Some(transport) = existing.as_ref()
            && transport.is_alive()
        {
            return Ok(());
        }
        if let Some(dead) = existing {
            let failure = tool_err(
                "MCP_TRANSPORT_CLOSED",
                format!(
                    "server {:?} exited before the next request",
                    entry.config.name
                ),
            );
            Self::detach_failed_call_transport(entry, &dead, &failure);
        }
        Self::check_restart_budget(entry)?;

        // Secret resolution, restart bookkeeping, and dead-transport cleanup
        // can all take time. Re-read the shared store at the last feasible seam
        // before transport construction or process creation.
        self.check_trust(entry)?;
        let transport: Arc<dyn McpTransport> = match self.spawn_transport(entry).await {
            Ok(transport) => Arc::from(transport),
            Err(err) => {
                Self::record_failure(entry, &err);
                return Err(err);
            }
        };
        let mut private_transport = PrivateHandshakeTransport::new(Arc::clone(&transport));
        if let Err(err) = self.check_running() {
            transport.close().await;
            return Err(err);
        }
        if let Err(err) = self.check_trust(entry) {
            transport.close().await;
            *Self::lock(&entry.health) = ServerHealth::NotStarted;
            Self::lock(&entry.tools_cache).take();
            return Err(err);
        }

        // Keep the new transport private until its full handshake succeeds.
        if let Err(err) = transport
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "pi_agent_rust",
                        "version": crate::platform::VERSION,
                    },
                }),
                DEFAULT_MCP_TIMEOUT,
            )
            .await
        {
            transport.close().await;
            Self::record_failure(entry, &err);
            return Err(err);
        }
        if let Err(err) = self.check_running() {
            transport.close().await;
            return Err(err);
        }
        if let Err(err) = transport
            .notify("notifications/initialized", serde_json::json!({}))
            .await
        {
            transport.close().await;
            Self::record_failure(entry, &err);
            return Err(err);
        }
        if let Err(err) = self.check_trust(entry) {
            transport.close().await;
            *Self::lock(&entry.health) = ServerHealth::NotStarted;
            Self::lock(&entry.tools_cache).take();
            return Err(err);
        }
        if let Err(err) = Arc::clone(&transport).activate().await {
            transport.close().await;
            Self::record_failure(entry, &err);
            return Err(err);
        }
        if !transport.is_alive() {
            let err = tool_err(
                "MCP_TRANSPORT_CLOSED",
                format!(
                    "server {:?} failed while activating its receive channel",
                    entry.config.name
                ),
            );
            transport.close().await;
            Self::record_failure(entry, &err);
            return Err(err);
        }
        let published = {
            let mut current = Self::lock(&entry.transport);
            if self.inner.shutting_down.load(Ordering::Acquire) {
                false
            } else {
                *current = Some(Arc::clone(&transport));
                true
            }
        };
        if !published {
            transport.close().await;
            return Err(tool_err(
                "MCP_MANAGER_SHUTDOWN",
                "MCP session shutdown won the transport publication race",
            ));
        }
        private_transport.disarm();
        // A denial can race the small check-to-publication interval above.
        // Recheck before returning the transport to any caller; on revocation,
        // remove exactly the transport this connect attempt published.
        if let Err(err) = self.check_trust(entry) {
            Self::close_revoked_transport(entry, &transport).await;
            return Err(err);
        }
        {
            let current = Self::lock(&entry.transport);
            if self.inner.shutting_down.load(Ordering::Acquire)
                || !current
                    .as_ref()
                    .is_some_and(|candidate| Arc::ptr_eq(candidate, &transport))
            {
                drop(current);
                transport.abort();
                return Err(tool_err(
                    "MCP_MANAGER_SHUTDOWN",
                    "MCP session shut down before transport activation completed",
                ));
            }
            *Self::lock(&entry.health) = ServerHealth::Ready {
                tools: Self::lock(&entry.tools_cache)
                    .as_ref()
                    .map_or(0, |(_, tools)| tools.len()),
            };
        }
        Ok(())
    }

    /// Record a failed spawn/handshake: increment the counter and arm the
    /// exponential backoff.
    fn record_failure(entry: &Arc<ServerEntry>, err: &Error) {
        Self::lock(&entry.tools_cache).take();
        let mut restarts = Self::lock(&entry.restarts);
        restarts.count = restarts.count.saturating_add(1);
        let backoff = Duration::from_secs(1 << restarts.count.min(3));
        restarts.next_retry_at = Some(Instant::now() + backoff);
        *Self::lock(&entry.health) = if restarts.count >= MAX_RESTARTS {
            ServerHealth::Failed {
                reason: err.to_string(),
            }
        } else {
            ServerHealth::Unhealthy {
                reason: err.to_string(),
                retries: restarts.count,
            }
        };
    }

    async fn spawn_transport(&self, entry: &Arc<ServerEntry>) -> Result<Box<dyn McpTransport>> {
        let config = entry.config.clone();
        let cwd = entry.effective_cwd(&self.inner.cwd);
        let trust_path = self.inner.trust_path.clone();
        #[cfg(test)]
        let factory = Self::lock(&self.inner.transport_factory).clone();
        let abandoned = Arc::new(AtomicBool::new(false));
        let abandoned_worker = Arc::clone(&abandoned);
        let mut attempt = TransportConstructionAttempt::new(abandoned);

        // Secret resolution and process construction are synchronous. Keep
        // them off the async worker so the startup deadline can still be
        // polled. The production runtime supplies a bounded blocking pool.
        let result = asupersync::runtime::spawn_blocking(move || {
            let ensure_active = || {
                if abandoned_worker.load(Ordering::Acquire) {
                    Err(tool_err(
                        "MCP_STARTUP_CANCELLED",
                        format!("server {:?} startup was cancelled", config.name),
                    ))
                } else {
                    Ok(())
                }
            };
            ensure_active()?;

            // Hold the cross-process trust-store lock across every local
            // execution effect ($CMD resolution and stdio process creation).
            // A concurrent deny/reset either linearizes before this guard and
            // blocks execution, or waits and linearizes after it.
            let _trust_execution_guard =
                Self::lock_trust_for_execution(&config, &cwd, &trust_path)?;
            ensure_active()?;

            #[cfg(test)]
            if let Some(factory) = factory {
                let transport = factory();
                if let Err(error) = ensure_active() {
                    transport.abort();
                    return Err(error);
                }
                return Ok(transport);
            }
            if config.is_http() {
                let url = config.url.clone().ok_or_else(|| {
                    tool_err(
                        "MCP_CONFIG_INVALID",
                        format!("server {:?} is http-shaped but has no url", config.name),
                    )
                })?;
                let headers =
                    resolve_secrets(&config.headers, super::config::validate_http_header_value)?;
                ensure_active()?;
                return Ok(
                    Box::new(super::transport::HttpTransport::new(&url, headers)?)
                        as Box<dyn McpTransport>,
                );
            }
            let command = config.command.clone().ok_or_else(|| {
                tool_err(
                    "MCP_CONFIG_INVALID",
                    format!("server {:?} has no command or url", config.name),
                )
            })?;
            let env = resolve_secrets(&config.env, super::config::validate_env_value)?;
            ensure_active()?;
            let transport =
                super::transport::StdioTransport::spawn(&command, &config.args, &env, &cwd)?;
            if let Err(error) = ensure_active() {
                transport.abort();
                return Err(error);
            }
            Ok(Box::new(transport) as Box<dyn McpTransport>)
        })
        .await;
        attempt.disarm();
        result
    }

    /// Call one tool on one server.
    ///
    /// Trust-gated. When the transport dies mid-call, the server is reconnected
    /// for later calls, but the failed call is not replayed: the server may
    /// have performed a side effect before its response was lost.
    ///
    /// # Errors
    ///
    /// Trust-gated; transport and server errors carry taxonomy codes.
    pub async fn call_tool(&self, server: &str, tool: &str, arguments: Value) -> Result<Value> {
        let entry = self.entry(server)?;
        self.ensure_ready(&entry).await?;
        let transport = Self::lock(&entry.transport).clone().ok_or_else(|| {
            tool_err(
                "MCP_TRANSPORT_UNAVAILABLE",
                "the connection disappeared before tools/call was dispatched",
            )
        })?;
        match self
            .call_on_transport(&entry, &transport, tool, &arguments)
            .await
        {
            Ok(value) => Ok(value),
            Err(err) if is_indeterminate_call_delivery(&err) => {
                let recovery = self
                    .recover_after_indeterminate_call(&entry, &transport, &err)
                    .await;
                Err(tool_err(
                    "MCP_DELIVERY_INDETERMINATE",
                    format!(
                        "server {:?} lost its transport during tools/call; the request may have completed and was not retried; {recovery}",
                        entry.config.name
                    ),
                ))
            }
            Err(err) => Err(err),
        }
    }

    async fn call_on_transport(
        &self,
        entry: &Arc<ServerEntry>,
        transport: &Arc<dyn McpTransport>,
        tool: &str,
        arguments: &Value,
    ) -> Result<Value> {
        // The connection lane intentionally does not span a potentially long
        // tool call. Re-authorize at the request boundary so a trust decision
        // changed by another manager cannot be bypassed by an already-live
        // transport.
        self.check_running()?;
        self.check_trust(entry)?;
        let result = transport
            .request(
                "tools/call",
                serde_json::json!({ "name": tool, "arguments": arguments }),
                DEFAULT_MCP_TIMEOUT,
            )
            .await;
        self.check_running()?;
        let result = result?;
        if let Err(err) = self.check_trust(entry) {
            Self::close_revoked_transport(entry, transport).await;
            return Err(err);
        }
        Self::record_operational_success(entry, transport);
        Ok(result)
    }

    async fn recover_after_indeterminate_call(
        &self,
        entry: &Arc<ServerEntry>,
        failed_transport: &Arc<dyn McpTransport>,
        failure: &Error,
    ) -> String {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let connect_guard =
            asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&entry.connect_lane), cx.cx()).await;
        let Ok(connect_guard) = connect_guard else {
            failed_transport.abort();
            return "recovery was cancelled before the connection lane became available"
                .to_string();
        };

        Self::detach_failed_call_transport(entry, failed_transport, failure);

        let recovery = match self.ensure_ready_in_lane(entry).await {
            Ok(()) => self.list_and_cache_tools_in_lane(entry).await.map(|_| ()),
            Err(err) => Err(err),
        };
        drop(connect_guard);
        match recovery {
            Ok(()) => "the server was reconnected for subsequent calls".to_string(),
            Err(recovery) => {
                format!("the server could not be reconnected for subsequent calls: {recovery}")
            }
        }
    }

    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn detach_failed_call_transport(
        entry: &Arc<ServerEntry>,
        failed_transport: &Arc<dyn McpTransport>,
        failure: &Error,
    ) -> bool {
        let removed_failed_transport = {
            let mut current = Self::lock(&entry.transport);
            if current
                .as_ref()
                .is_some_and(|candidate| Arc::ptr_eq(candidate, failed_transport))
            {
                let removed = current.take();
                // Keep transport ownership and its derived state in one
                // generation-checked critical section. Otherwise a replacement
                // could publish between `take` and `mark_unhealthy` and then be
                // poisoned by the stale failure.
                Self::mark_unhealthy(entry, failure);
                removed
            } else {
                None
            }
        };
        failed_transport.abort();
        removed_failed_transport.is_some()
    }

    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    async fn close_revoked_transport(entry: &Arc<ServerEntry>, transport: &Arc<dyn McpTransport>) {
        let removed = {
            let mut current = Self::lock(&entry.transport);
            if current
                .as_ref()
                .is_some_and(|candidate| Arc::ptr_eq(candidate, transport))
            {
                let removed = current.take();
                // Health and tool metadata belong to this exact transport
                // generation. Clear them before releasing the generation lock
                // so a concurrently published replacement cannot be erased.
                *Self::lock(&entry.health) = ServerHealth::NotStarted;
                Self::lock(&entry.tools_cache).take();
                removed
            } else {
                None
            }
        };
        if let Some(removed) = removed {
            removed.close().await;
        } else {
            // This request still owns an obsolete revoked transport even when
            // a replacement is current. Abort only the obsolete generation.
            transport.abort();
        }
    }

    /// Record a transport death: restart count + health state. One isolated
    /// crash receives an immediate reconnect; another crash before a
    /// successful tool call arms backoff and consumes the remaining budget.
    fn mark_unhealthy(entry: &Arc<ServerEntry>, err: &Error) {
        Self::lock(&entry.tools_cache).take();
        let mut restarts = Self::lock(&entry.restarts);
        restarts.count = restarts.count.saturating_add(1);
        // Preserve one immediate recovery for an isolated runtime crash. A
        // second crash before a successful tools/call proves a flap and arms
        // the same exponential backoff used for spawn/handshake failures.
        if restarts.count > 1 {
            let backoff = Duration::from_secs(1 << restarts.count.min(3));
            restarts.next_retry_at = Some(Instant::now() + backoff);
        } else {
            restarts.next_retry_at = None;
        }
        *Self::lock(&entry.health) = ServerHealth::Unhealthy {
            reason: err.to_string(),
            retries: restarts.count,
        };
        if restarts.count >= MAX_RESTARTS {
            *Self::lock(&entry.health) = ServerHealth::Failed {
                reason: err.to_string(),
            };
        }
    }

    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn record_operational_success(
        entry: &Arc<ServerEntry>,
        transport: &Arc<dyn McpTransport>,
    ) -> bool {
        let current = Self::lock(&entry.transport);
        if !current
            .as_ref()
            .is_some_and(|candidate| Arc::ptr_eq(candidate, transport))
        {
            return false;
        }
        *Self::lock(&entry.restarts) = RestartState::default();
        true
    }

    /// Tool metadata snapshot of every server with a fresh cache (for
    /// mounting).
    #[must_use]
    pub fn mounted_tool_metas(&self) -> Vec<(String, Vec<McpToolMeta>)> {
        let Ok(store) = self.trust_store() else {
            return Vec::new();
        };
        let servers = Self::lock(&self.inner.servers).clone();
        servers
            .values()
            .filter_map(|entry| {
                if store.decision(&entry.config.name, &self.trust_fingerprint_for(entry))
                    != TrustDecision::Acknowledged
                {
                    return None;
                }
                if !matches!(&*Self::lock(&entry.health), ServerHealth::Ready { .. }) {
                    return None;
                }
                let transport = Self::lock(&entry.transport).clone()?;
                if !transport.is_alive() {
                    return None;
                }
                let tools = Self::lock(&entry.tools_cache).clone()?;
                let (cached_at, tools) = tools;
                if cached_at.elapsed() > TOOL_CACHE_TTL {
                    return None;
                }
                Some((entry.config.name.clone(), tools))
            })
            .collect()
    }

    /// Server diagnostics tail (stderr for stdio, endpoint for HTTP) — the
    /// `/mcp` diagnostics surface.
    #[must_use]
    pub fn server_diagnostics(&self, name: &str) -> Option<String> {
        let entry = Self::lock(&self.inner.servers).get(name).cloned()?;
        let transport = Self::lock(&entry.transport).clone();
        transport.map(|t| t.diagnostics_tail())
    }

    /// Eagerly connect every acknowledged server (startup path): parallel,
    /// bounded by a global budget; stragglers/failures land Unhealthy and
    /// never block startup.
    pub async fn connect_trusted(&self) {
        self.connect_trusted_with_budget(STARTUP_CONNECT_BUDGET)
            .await;
    }

    fn fail_timed_out_startup_attempt(
        entry: &Arc<ServerEntry>,
        observed_transport: Option<Arc<dyn McpTransport>>,
    ) -> bool {
        let error = tool_err(
            "MCP_STARTUP_TIMEOUT",
            format!(
                "server {:?} did not finish initialization and tools/list within the startup budget",
                entry.config.name
            ),
        );
        let (owns_state, owned_transport) = {
            let mut current = Self::lock(&entry.transport);
            match observed_transport.as_ref() {
                Some(observed)
                    if current
                        .as_ref()
                        .is_some_and(|candidate| Arc::ptr_eq(candidate, observed)) =>
                {
                    let owned = current.take();
                    Self::record_failure(entry, &error);
                    (true, owned)
                }
                None if current.is_none() => {
                    Self::record_failure(entry, &error);
                    (true, None)
                }
                _ => (false, None),
            }
        };
        if !owns_state {
            if let Some(observed) = observed_transport {
                observed.abort();
            }
            return false;
        }
        if let Some(transport) = owned_transport.or(observed_transport) {
            transport.abort();
        }
        true
    }

    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_in_scrutinee)]
    async fn connect_trusted_with_budget(&self, budget: Duration) {
        use std::sync::atomic::{AtomicBool, Ordering};

        let Ok(store) = self.trust_store() else {
            return;
        };
        let servers = Self::lock(&self.inner.servers).clone();
        let mut tracked = Vec::new();
        let mut pending = Vec::new();
        for entry in servers.values().filter(|entry| {
            store.decision(&entry.config.name, &self.trust_fingerprint_for(entry))
                == TrustDecision::Acknowledged
        }) {
            let entry = Arc::clone(entry);
            let completed = Arc::new(AtomicBool::new(false));
            let observed_transport = Arc::new(Mutex::new(None));
            tracked.push((
                Arc::clone(&entry),
                Arc::clone(&completed),
                Arc::clone(&observed_transport),
            ));
            pending.push(async move {
                let cx = crate::agent_cx::AgentCx::for_current_or_request();
                let connect_guard = asupersync::sync::OwnedMutexGuard::lock(
                    Arc::clone(&entry.connect_lane),
                    cx.cx(),
                )
                .await;
                if let Ok(connect_guard) = connect_guard {
                    if self.ensure_ready_in_lane(&entry).await.is_ok() {
                        // Capture and list one exact generation while retaining
                        // the same lane. Timeout cleanup can then safely compare
                        // this snapshot with the transport whose request hung.
                        let current_generation = Self::lock(&entry.transport).clone();
                        *Self::lock(&observed_transport) = current_generation;
                        #[cfg(test)]
                        if let Some(hook) =
                            Self::lock(&self.inner.startup_after_generation_hook).clone()
                        {
                            hook();
                        }
                        let _ = self.list_and_cache_tools_in_lane(&entry).await;
                    }
                    drop(connect_guard);
                }
                completed.store(true, Ordering::Release);
            });
        }
        if pending.is_empty() {
            return;
        }
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let now = cx
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        let all = Box::pin(futures::future::join_all(pending));
        let deadline = Box::pin(asupersync::time::sleep(now, budget));
        if let futures::future::Either::Right(((), unfinished)) =
            futures::future::select(all, deadline).await
        {
            // Cancellation first: this drops connection-lane guards and the
            // private-handshake guard before cleanup inspects shared state.
            drop(unfinished);
            for (entry, completed, observed_transport) in tracked {
                if completed.load(Ordering::Acquire) {
                    continue;
                }
                let observed_transport = Self::lock(&observed_transport).clone();
                let Ok(_connect_guard) = Arc::clone(&entry.connect_lane).try_lock_owned() else {
                    if let Some(transport) = observed_transport {
                        transport.abort();
                    }
                    tracing::debug!(
                        event = "pi.mcp.startup_cleanup_superseded",
                        server = %entry.config.name,
                        "another connection attempt owns the lane; timeout cleanup will not mutate its state"
                    );
                    continue;
                };
                if !Self::fail_timed_out_startup_attempt(&entry, observed_transport) {
                    tracing::debug!(
                        event = "pi.mcp.startup_cleanup_superseded",
                        server = %entry.config.name,
                        "a replacement transport superseded the timed-out startup attempt"
                    );
                }
            }
            tracing::info!(
                event = "pi.mcp.startup_budget_exhausted",
                "MCP startup connects exceeded the global budget; stragglers stay Unhealthy"
            );
        }
    }

    /// Register an extension-contributed server spec (`registerMcpServer`).
    /// Same registry, same trust gate: the spec flows through the identical
    /// spawn path as file-configured servers, with `provenance=extension`.
    /// Name collisions with existing entries are ignored (file config wins).
    #[allow(clippy::too_many_lines)]
    pub fn register_extension_server(&self, name: &str, spec: &Value) {
        if let Err(reason) = super::config::validate_server_name(name) {
            tracing::warn!(
                event = "pi.mcp.extension_config_rejected",
                server = name,
                %reason,
                "extension MCP server configuration rejected"
            );
            return;
        }
        if !spec.is_object() {
            tracing::warn!(
                event = "pi.mcp.extension_config_rejected",
                server = name,
                reason = "server specification must be an object",
                "extension MCP server configuration rejected"
            );
            return;
        }
        let parsed = (|| -> std::result::Result<ConfiguredServer, String> {
            let extension_id = optional_string(spec, "extension_id")?;
            let type_hint = optional_string(spec, "type")?;
            let transport_hint = optional_string(spec, "transport")?;
            let transport_hint = match (type_hint, transport_hint) {
                (Some(left), Some(right)) if left != right => {
                    return Err(format!(
                        "fields \"type\" and \"transport\" disagree ({left:?} versus {right:?})"
                    ));
                }
                (Some(value), _) | (_, Some(value)) => Some(value),
                (None, None) => None,
            };
            Ok(ConfiguredServer {
                name: name.to_string(),
                command: optional_string(spec, "command")?,
                args: optional_string_array(spec, "args")?,
                env: optional_string_map(spec, "env")?,
                url: optional_string(spec, "url")?,
                headers: optional_string_map(spec, "headers")?,
                transport_hint,
                provenance: Provenance::Extension,
                source_file: extension_id.map_or_else(
                    || PathBuf::from("<extension>"),
                    |id| PathBuf::from(format!("extension:{id}")),
                ),
            })
        })();
        let mut config = match parsed {
            Ok(config) => config,
            Err(reason) => {
                tracing::warn!(
                    event = "pi.mcp.extension_config_rejected",
                    server = name,
                    %reason,
                    "extension MCP server configuration rejected"
                );
                return;
            }
        };
        config.env = match super::config::normalize_env(config.env) {
            Ok(env) => env,
            Err(reason) => {
                tracing::warn!(
                    event = "pi.mcp.extension_config_rejected",
                    server = name,
                    %reason,
                    "extension MCP server configuration rejected"
                );
                return;
            }
        };
        config.headers = match super::config::normalize_http_headers(config.headers) {
            Ok(headers) => headers,
            Err(reason) => {
                tracing::warn!(
                    event = "pi.mcp.extension_config_rejected",
                    server = name,
                    %reason,
                    "extension MCP server configuration rejected"
                );
                return;
            }
        };
        if let Err(reason) = super::config::validate_transport_shape(&config) {
            tracing::warn!(
                event = "pi.mcp.extension_config_rejected",
                server = name,
                %reason,
                "extension MCP server configuration rejected"
            );
            return;
        }
        // bd-hyik7: preserve the spec's working-directory intent, anchored
        // against the manager cwd when relative so the bound identity and
        // spawn environment stay deterministic.
        let cwd_override = match optional_string(spec, "cwd") {
            Ok(Some(raw)) => {
                let raw_path = PathBuf::from(raw.trim());
                let anchored = if raw_path.is_absolute() {
                    raw_path
                } else {
                    self.inner.cwd.join(raw_path)
                };
                std::fs::canonicalize(&anchored).unwrap_or(anchored).into()
            }
            Ok(None) => None,
            Err(reason) => {
                tracing::warn!(
                    event = "pi.mcp.extension_config_rejected",
                    server = name,
                    %reason,
                    "extension MCP server configuration rejected"
                );
                return;
            }
        };
        let entry = Arc::new(ServerEntry {
            config,
            cwd_override,
            connect_lane: Arc::new(asupersync::sync::Mutex::new(())),
            transport: Mutex::new(None),
            tools_cache: Mutex::new(None),
            health: Mutex::new(ServerHealth::NotStarted),
            restarts: Mutex::new(RestartState::default()),
        });
        Self::lock(&self.inner.servers)
            .entry(name.to_string())
            .or_insert(entry);
    }
}

/// Whether a `tools/call` failure happened after request delivery may have
/// occurred. These failures must never trigger an automatic replay.
fn is_indeterminate_call_delivery(err: &Error) -> bool {
    matches!(
        err,
        Error::Tool { tool, message }
            if tool == "mcp"
                && [
                    "[MCP_TRANSPORT_CLOSED] ",
                    "[MCP_TRANSPORT_IO] ",
                    "[MCP_PROTOCOL] ",
                    "[MCP_TIMEOUT] ",
                ]
                    .iter()
                    .any(|prefix| message.starts_with(prefix))
    )
}

/// Resolve `$ENV:`/`$CMD:` secret references in env/header values.
fn resolve_secrets(
    entries: &[(String, String)],
    validate_value: fn(&str) -> std::result::Result<(), String>,
) -> Result<Vec<(String, String)>> {
    resolve_secrets_with(entries, validate_value, |raw| {
        crate::auth::resolve_secret_reference(raw)
    })
}

fn resolve_secrets_with<F>(
    entries: &[(String, String)],
    validate_value: fn(&str) -> std::result::Result<(), String>,
    mut resolve: F,
) -> Result<Vec<(String, String)>>
where
    F: FnMut(&str) -> std::result::Result<Option<String>, String>,
{
    let mut out = Vec::with_capacity(entries.len());
    for (name, raw) in entries {
        match resolve(raw) {
            Ok(Some(resolved)) => {
                validate_value(&resolved).map_err(|reason| {
                    tool_err(
                        "MCP_SECRET_INVALID",
                        format!("resolved value for {name:?} is invalid: {reason}"),
                    )
                })?;
                out.push((name.clone(), resolved));
            }
            Ok(None) => {
                return Err(tool_err(
                    "MCP_SECRET_UNRESOLVED",
                    format!("{name}: reference resolved to empty (unset env var or empty output)"),
                ));
            }
            Err(reason) => {
                return Err(tool_err(
                    "MCP_SECRET_UNRESOLVED",
                    format!("{name}: {reason}"),
                ));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Condvar, mpsc};

    use async_trait::async_trait;

    use super::*;

    #[test]
    fn tool_list_parser_rejects_malformed_or_ambiguous_metadata() {
        let valid = serde_json::json!({
            "tools": [{
                "name": "echo",
                "description": "Echo text",
                "inputSchema": {"type": "object"}
            }]
        });
        let parsed = parse_tool_list(&valid).expect("valid tool list");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "echo");

        for malformed in [
            serde_json::json!({}),
            serde_json::json!({"tools": {}}),
            serde_json::json!({"tools": [null]}),
            serde_json::json!({"tools": [{"name": "echo"}]}),
            serde_json::json!({
                "tools": [
                    {"name": "echo", "inputSchema": {}},
                    {"name": "echo", "inputSchema": {}}
                ]
            }),
            serde_json::json!({
                "tools": [
                    {"name": "valid", "inputSchema": {}},
                    {"name": 7, "inputSchema": {}}
                ]
            }),
        ] {
            let error =
                parse_tool_list(&malformed).expect_err("malformed tools/list must fail as a whole");
            assert!(error.to_string().contains("MCP_PROTOCOL"), "{error}");
        }
    }

    #[test]
    fn extension_registration_rejects_ambiguous_transport_shapes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let manager = McpManager::new(
            temp.path(),
            &temp.path().join("global"),
            McpDiscovery::default(),
        );
        for (name, spec) in [
            (
                "both",
                serde_json::json!({"command":"server","url":"https://example.invalid"}),
            ),
            (
                "url-stdio",
                serde_json::json!({"url":"https://example.invalid","transport":"stdio"}),
            ),
            (
                "command-http",
                serde_json::json!({"command":"server","type":"http"}),
            ),
            (
                "conflicting-hints",
                serde_json::json!({
                    "command":"server",
                    "type":"stdio",
                    "transport":"http"
                }),
            ),
        ] {
            manager.register_extension_server(name, &spec);
            assert!(
                manager.entry(name).is_err(),
                "invalid extension server {name:?} must not enter the registry"
            );
        }

        manager.register_extension_server(
            "valid",
            &serde_json::json!({"command":"server","transport":"stdio"}),
        );
        assert_eq!(
            manager
                .entry("valid")
                .expect("valid extension server")
                .config
                .transport_hint
                .as_deref(),
            Some("stdio")
        );
    }

    #[test]
    fn resolved_secret_values_are_revalidated_before_transport_use() {
        let entries = vec![("X-Token".to_string(), "$ENV:TOKEN".to_string())];
        let error = resolve_secrets_with(
            &entries,
            super::super::config::validate_http_header_value,
            |_| Ok(Some("safe\r\nX-Forged: yes".to_string())),
        )
        .expect_err("resolved header controls must fail before transport construction");
        let message = error.to_string();
        assert!(message.contains("MCP_SECRET_INVALID"), "{message}");
        assert!(!message.contains("X-Forged"), "{message}");
        assert!(!message.contains('\r'), "{message:?}");
        assert!(!message.contains('\n'), "{message:?}");
    }

    struct MalformedToolsTransport {
        closed: AtomicBool,
    }

    struct HangingToolsTransport {
        started: Mutex<Option<mpsc::Sender<()>>>,
        closed: AtomicBool,
    }

    struct HangingInitializeState {
        started: Mutex<Option<mpsc::Sender<()>>>,
        closed: AtomicBool,
    }

    struct HangingInitializeTransport {
        state: Arc<HangingInitializeState>,
    }

    struct FlappingCallTransport {
        closed: AtomicBool,
    }

    struct FailingInitializeTransport {
        closed: AtomicBool,
    }

    struct SuccessfulTransport {
        closed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl McpTransport for HangingToolsTransport {
        // Guard scope is deliberate; tightening drops would change lock-hold semantics.
        #[allow(clippy::significant_drop_in_scrutinee)]
        async fn request(&self, method: &str, _params: Value, _timeout: Duration) -> Result<Value> {
            if method == "tools/list" {
                if let Some(started) = McpManager::lock(&self.started).take() {
                    let _ = started.send(());
                }
                return futures::future::pending::<Result<Value>>().await;
            }
            Ok(serde_json::json!({}))
        }

        async fn notify(&self, _method: &str, _params: Value) -> Result<()> {
            Ok(())
        }

        fn is_alive(&self) -> bool {
            !self.closed.load(Ordering::Acquire)
        }

        fn abort(&self) {
            self.closed.store(true, Ordering::Release);
        }

        async fn close(&self) {
            self.abort();
        }

        fn diagnostics_tail(&self) -> String {
            String::new()
        }
    }

    #[async_trait]
    impl McpTransport for HangingInitializeTransport {
        // Guard scope is deliberate; tightening drops would change lock-hold semantics.
        #[allow(clippy::significant_drop_in_scrutinee)]
        async fn request(&self, method: &str, _params: Value, _timeout: Duration) -> Result<Value> {
            if method == "initialize" {
                if let Some(started) = McpManager::lock(&self.state.started).take() {
                    let _ = started.send(());
                }
                return futures::future::pending::<Result<Value>>().await;
            }
            Ok(serde_json::json!({}))
        }

        async fn notify(&self, _method: &str, _params: Value) -> Result<()> {
            Ok(())
        }

        fn is_alive(&self) -> bool {
            !self.state.closed.load(Ordering::Acquire)
        }

        fn abort(&self) {
            self.state.closed.store(true, Ordering::Release);
        }

        async fn close(&self) {
            self.abort();
        }

        fn diagnostics_tail(&self) -> String {
            String::new()
        }
    }

    #[async_trait]
    impl McpTransport for FlappingCallTransport {
        async fn request(&self, method: &str, _params: Value, _timeout: Duration) -> Result<Value> {
            match method {
                "initialize" => Ok(serde_json::json!({})),
                "tools/list" => Ok(serde_json::json!({
                    "tools": [{
                        "name": "echo",
                        "description": "fixture",
                        "inputSchema": {"type": "object"}
                    }]
                })),
                "tools/call" => {
                    self.closed.store(true, Ordering::Release);
                    Err(tool_err(
                        "MCP_TRANSPORT_IO",
                        "fixture transport crashed during tools/call",
                    ))
                }
                other => Err(tool_err(
                    "MCP_PROTOCOL",
                    format!("unexpected fixture method {other:?}"),
                )),
            }
        }

        async fn notify(&self, _method: &str, _params: Value) -> Result<()> {
            Ok(())
        }

        fn is_alive(&self) -> bool {
            !self.closed.load(Ordering::Acquire)
        }

        fn abort(&self) {
            self.closed.store(true, Ordering::Release);
        }

        async fn close(&self) {
            self.abort();
        }

        fn diagnostics_tail(&self) -> String {
            String::new()
        }
    }

    #[async_trait]
    impl McpTransport for FailingInitializeTransport {
        async fn request(&self, method: &str, _params: Value, _timeout: Duration) -> Result<Value> {
            if method == "initialize" {
                return Err(tool_err("MCP_TRANSPORT_IO", "fixture initialize failure"));
            }
            Err(tool_err(
                "MCP_PROTOCOL",
                format!("unexpected fixture method {method:?}"),
            ))
        }

        async fn notify(&self, _method: &str, _params: Value) -> Result<()> {
            Ok(())
        }

        fn is_alive(&self) -> bool {
            !self.closed.load(Ordering::Acquire)
        }

        fn abort(&self) {
            self.closed.store(true, Ordering::Release);
        }

        async fn close(&self) {
            self.abort();
        }

        fn diagnostics_tail(&self) -> String {
            String::new()
        }
    }

    #[async_trait]
    impl McpTransport for SuccessfulTransport {
        async fn request(&self, method: &str, _params: Value, _timeout: Duration) -> Result<Value> {
            match method {
                "initialize" => Ok(serde_json::json!({})),
                "tools/list" => Ok(serde_json::json!({"tools": []})),
                "tools/call" => Ok(serde_json::json!({"content": []})),
                other => Err(tool_err(
                    "MCP_PROTOCOL",
                    format!("unexpected fixture method {other:?}"),
                )),
            }
        }

        async fn notify(&self, _method: &str, _params: Value) -> Result<()> {
            Ok(())
        }

        fn is_alive(&self) -> bool {
            !self.closed.load(Ordering::Acquire)
        }

        fn abort(&self) {
            self.closed.store(true, Ordering::Release);
        }

        async fn close(&self) {
            self.abort();
        }

        fn diagnostics_tail(&self) -> String {
            String::new()
        }
    }

    fn injected_transport_command(temp: &tempfile::TempDir) -> String {
        let command = temp.path().join("injected-mcp-transport");
        std::fs::write(&command, b"injected transport fixture")
            .expect("write injected transport fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let mut permissions = std::fs::metadata(&command)
                .expect("injected transport metadata")
                .permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&command, permissions)
                .expect("make injected transport executable");
        }
        command
            .to_str()
            .expect("injected transport path must be UTF-8")
            .to_string()
    }

    fn acknowledge_test_stdio_server(manager: &McpManager, entry: &Arc<ServerEntry>) {
        let fingerprint = manager.trust_fingerprint_for(entry);
        let execution = entry
            .config
            .execution_identity(&entry.effective_cwd(&manager.inner.cwd))
            .expect("resolve injected transport executable")
            .expect("stdio fixture must have an execution identity");
        TrustStore::load(&manager.inner.trust_path)
            .expect("load trust")
            .acknowledge_execution(&entry.config.name, &fingerprint, "operator", execution)
            .expect("acknowledge fixture execution");
    }

    fn trusted_fixture_manager(temp: &tempfile::TempDir) -> (McpManager, Arc<ServerEntry>) {
        let cwd = temp.path().join("project");
        let global = temp.path().join("global");
        std::fs::create_dir_all(&cwd).expect("project directory");
        std::fs::create_dir_all(&global).expect("global directory");
        let config = ConfiguredServer {
            name: "fixture".to_string(),
            command: Some(injected_transport_command(temp)),
            args: Vec::new(),
            env: Vec::new(),
            url: None,
            headers: Vec::new(),
            transport_hint: Some("stdio".to_string()),
            provenance: Provenance::ProjectPi,
            source_file: cwd.join(".pi/mcp.json"),
        };
        let manager = McpManager::new(
            &cwd,
            &global,
            McpDiscovery {
                servers: vec![config],
                warnings: Vec::new(),
            },
        );
        let entry = manager.entry("fixture").expect("fixture entry");
        acknowledge_test_stdio_server(&manager, &entry);
        (manager, entry)
    }

    #[test]
    fn shutdown_all_awaits_and_resets_every_live_transport() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("project");
        let global = temp.path().join("global");
        std::fs::create_dir_all(&cwd).expect("project directory");
        std::fs::create_dir_all(&global).expect("global directory");
        let servers = ["first", "second"]
            .into_iter()
            .map(|name| ConfiguredServer {
                name: name.to_string(),
                command: Some(format!("unused-{name}")),
                args: Vec::new(),
                env: Vec::new(),
                url: None,
                headers: Vec::new(),
                transport_hint: Some("stdio".to_string()),
                provenance: Provenance::ProjectPi,
                source_file: cwd.join(".pi/mcp.json"),
            })
            .collect();
        let manager = McpManager::new(
            &cwd,
            &global,
            McpDiscovery {
                servers,
                warnings: Vec::new(),
            },
        );

        let mut observed = Vec::new();
        for name in ["first", "second"] {
            let entry = manager.entry(name).expect("fixture entry");
            let closed = Arc::new(AtomicBool::new(false));
            let transport: Arc<dyn McpTransport> = Arc::new(SuccessfulTransport {
                closed: Arc::clone(&closed),
            });
            *McpManager::lock(&entry.transport) = Some(transport);
            *McpManager::lock(&entry.tools_cache) = Some((
                Instant::now(),
                vec![McpToolMeta {
                    name: format!("{name}-tool"),
                    description: String::new(),
                    input_schema: serde_json::json!({}),
                }],
            ));
            *McpManager::lock(&entry.health) = ServerHealth::Ready { tools: 1 };
            observed.push((entry, closed));
        }

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let mut closed_names = runtime.block_on(manager.shutdown_all());
        closed_names.sort();
        assert_eq!(closed_names, vec!["first", "second"]);
        for (entry, closed) in observed {
            assert!(
                closed.load(Ordering::Acquire),
                "shutdown must await transport close"
            );
            assert!(McpManager::lock(&entry.transport).is_none());
            assert!(McpManager::lock(&entry.tools_cache).is_none());
            assert!(matches!(
                &*McpManager::lock(&entry.health),
                ServerHealth::NotStarted
            ));
        }

        let terminal_entry = manager.entry("first").expect("fixture entry");
        let error = runtime
            .block_on(manager.connect_and_list(&terminal_entry))
            .expect_err("a shut-down manager must never reconnect");
        assert!(
            error.to_string().contains("MCP_MANAGER_SHUTDOWN"),
            "{error}"
        );
        assert!(McpManager::lock(&terminal_entry.transport).is_none());
    }

    #[test]
    fn shutdown_waits_for_a_private_handshake_to_leave_its_connection_lane() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry) = trusted_fixture_manager(&temp);
        let manager = Arc::new(manager);
        let (started_tx, started_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let factory_release = Arc::clone(&release);
        let private_closed = Arc::new(AtomicBool::new(false));
        let factory_closed = Arc::clone(&private_closed);
        *McpManager::lock(&manager.inner.transport_factory) =
            Some(Arc::new(move || -> Box<dyn McpTransport> {
                Box::new(HeldRequestTransport {
                    started: Mutex::new(Some(started_tx.clone())),
                    release: Arc::clone(&factory_release),
                    response: serde_json::json!({}),
                    closed: Arc::clone(&factory_closed),
                })
            }));

        let connecting_manager = Arc::clone(&manager);
        let connecting_entry = Arc::clone(&entry);
        let connecting = std::thread::spawn(move || {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("runtime");
            runtime.block_on(connecting_manager.connect_and_list(&connecting_entry))
        });
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("initialize reached the controlled private handshake");

        let shutdown_manager = Arc::clone(&manager);
        let (shutdown_done_tx, shutdown_done_rx) = mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("runtime");
            let closed = runtime.block_on(shutdown_manager.shutdown_all());
            shutdown_done_tx.send(()).expect("shutdown completion");
            closed
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !manager.inner.shutting_down.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "shutdown never sealed the manager"
            );
            std::thread::yield_now();
        }
        let shutdown_returned_before_release = shutdown_done_rx
            .recv_timeout(Duration::from_millis(50))
            .is_ok();

        // Always release and join both helpers before asserting the intended
        // red condition so a broken barrier cannot strand a test process.
        let (released, wake) = &*release;
        *McpManager::lock(released) = true;
        wake.notify_all();

        let connect_result = connecting.join().expect("connect thread");
        let closed_names = shutdown.join().expect("shutdown thread");
        assert!(
            !shutdown_returned_before_release,
            "shutdown returned while a private handshake still held the connection lane"
        );
        let connect_error =
            connect_result.expect_err("terminal shutdown must reject handshake publication");
        assert!(
            connect_error.to_string().contains("MCP_MANAGER_SHUTDOWN"),
            "{connect_error}"
        );
        assert!(closed_names.is_empty());
        assert!(
            private_closed.load(Ordering::Acquire),
            "shutdown must wait for the private transport to close"
        );
        assert!(McpManager::lock(&entry.transport).is_none());
    }

    #[async_trait]
    impl McpTransport for MalformedToolsTransport {
        async fn request(
            &self,
            _method: &str,
            _params: Value,
            _timeout: Duration,
        ) -> Result<Value> {
            Ok(serde_json::json!({
                "tools": [{"name": "broken"}],
                "diagnostic": "MCP_TRUST_PENDING"
            }))
        }

        async fn notify(&self, _method: &str, _params: Value) -> Result<()> {
            Ok(())
        }

        fn is_alive(&self) -> bool {
            !self.closed.load(Ordering::Acquire)
        }

        fn abort(&self) {
            self.closed.store(true, Ordering::Release);
        }

        async fn close(&self) {
            self.abort();
        }

        fn diagnostics_tail(&self) -> String {
            String::new()
        }
    }

    #[test]
    fn malformed_tool_list_closes_transport_and_marks_server_unhealthy() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("project");
        let global = temp.path().join("global");
        std::fs::create_dir_all(&cwd).expect("project directory");
        std::fs::create_dir_all(&global).expect("global directory");
        let config = ConfiguredServer {
            name: "fixture".to_string(),
            command: None,
            args: Vec::new(),
            env: Vec::new(),
            url: Some("https://fixture.invalid/mcp".to_string()),
            headers: Vec::new(),
            transport_hint: Some("http".to_string()),
            provenance: Provenance::ProjectPi,
            source_file: cwd.join(".pi/mcp.json"),
        };
        let manager = McpManager::new(
            &cwd,
            &global,
            McpDiscovery {
                servers: vec![config],
                warnings: Vec::new(),
            },
        );
        let entry = manager.entry("fixture").expect("fixture entry");
        let fingerprint = manager.trust_fingerprint_for(&entry);
        let mut trust = TrustStore::load(&global.join("mcp-trust.json")).expect("load trust");
        trust
            .acknowledge("fixture", &fingerprint, "operator")
            .expect("acknowledge fixture");
        let malformed = Arc::new(MalformedToolsTransport {
            closed: AtomicBool::new(false),
        });
        let transport: Arc<dyn McpTransport> = malformed.clone();
        *McpManager::lock(&entry.transport) = Some(transport);
        *McpManager::lock(&entry.tools_cache) = Some((
            Instant::now(),
            vec![McpToolMeta {
                name: "stale".to_string(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            }],
        ));
        *McpManager::lock(&entry.health) = ServerHealth::Ready { tools: 1 };
        assert_eq!(manager.mounted_tool_metas().len(), 1);

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(manager.list_and_cache_tools(&entry))
            .expect_err("malformed tools/list must fail");
        assert!(error.to_string().contains("MCP_PROTOCOL"), "{error}");
        assert!(malformed.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert!(McpManager::lock(&entry.tools_cache).is_none());
        assert!(manager.mounted_tool_metas().is_empty());
        assert!(matches!(
            &*McpManager::lock(&entry.health),
            ServerHealth::Unhealthy { .. }
        ));

        let remote_transport_words = Error::tool(
            "mcp",
            "[MCP_REMOTE_ERROR] server mentioned MCP_TRANSPORT_CLOSED",
        );
        assert!(
            !is_indeterminate_call_delivery(&remote_transport_words),
            "remote prose must not trigger a duplicate tool call retry"
        );
        assert!(is_indeterminate_call_delivery(&Error::tool(
            "mcp",
            "[MCP_TIMEOUT] request timed out after 1 ms",
        )));
        assert!(is_indeterminate_call_delivery(&Error::tool(
            "mcp",
            "[MCP_PROTOCOL] malformed response after tools/call dispatch",
        )));
        assert!(!is_indeterminate_call_delivery(&Error::tool(
            "mcp",
            "[MCP_TRANSPORT_UNAVAILABLE] request was not dispatched",
        )));
    }

    #[test]
    fn repeated_runtime_crashes_back_off_and_exhaust_the_restart_budget() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry) = trusted_fixture_manager(&temp);

        let constructions = Arc::new(AtomicUsize::new(0));
        let factory_constructions = Arc::clone(&constructions);
        *McpManager::lock(&manager.inner.transport_factory) =
            Some(Arc::new(move || -> Box<dyn McpTransport> {
                factory_constructions.fetch_add(1, Ordering::AcqRel);
                Box::new(FlappingCallTransport {
                    closed: AtomicBool::new(false),
                })
            }));

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime
            .block_on(manager.connect_and_list(&entry))
            .expect("initial handshake and list");
        assert_eq!(constructions.load(Ordering::Acquire), 1);

        let first = runtime
            .block_on(manager.call_tool("fixture", "echo", serde_json::json!({})))
            .expect_err("first runtime crash is delivery-indeterminate");
        assert!(first.to_string().contains("MCP_DELIVERY_INDETERMINATE"));
        assert_eq!(
            constructions.load(Ordering::Acquire),
            2,
            "one isolated crash receives one immediate recovery"
        );

        let second = runtime
            .block_on(manager.call_tool("fixture", "echo", serde_json::json!({})))
            .expect_err("second consecutive runtime crash must enter backoff");
        assert!(second.to_string().contains("MCP_BACKOFF"), "{second}");
        assert_eq!(
            constructions.load(Ordering::Acquire),
            2,
            "backoff must prevent an immediate third construction"
        );
        let blocked = runtime
            .block_on(manager.call_tool("fixture", "echo", serde_json::json!({})))
            .expect_err("calls inside the restart window must fail fast");
        assert!(blocked.to_string().contains("MCP_BACKOFF"), "{blocked}");

        // Advance only the backoff seam, without sleeping, so the next crash
        // can prove terminal budget exhaustion deterministically.
        McpManager::lock(&entry.restarts).next_retry_at = None;
        let third = runtime
            .block_on(manager.call_tool("fixture", "echo", serde_json::json!({})))
            .expect_err("third consecutive runtime crash must exhaust the budget");
        assert!(
            third.to_string().contains("MCP_RESTART_EXHAUSTED"),
            "{third}"
        );
        assert_eq!(constructions.load(Ordering::Acquire), 3);
        let exhausted = runtime
            .block_on(manager.call_tool("fixture", "echo", serde_json::json!({})))
            .expect_err("exhausted server must require explicit /mcp test");
        assert!(
            exhausted.to_string().contains("MCP_RESTART_EXHAUSTED"),
            "{exhausted}"
        );
        assert_eq!(constructions.load(Ordering::Acquire), 3);
        assert_eq!(McpManager::lock(&entry.restarts).count, MAX_RESTARTS);
        assert!(matches!(
            &*McpManager::lock(&entry.health),
            ServerHealth::Failed { .. }
        ));
    }

    #[test]
    fn terminal_initialize_failure_immediately_marks_the_server_failed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry) = trusted_fixture_manager(&temp);
        let constructions = Arc::new(AtomicUsize::new(0));
        let factory_constructions = Arc::clone(&constructions);
        *McpManager::lock(&manager.inner.transport_factory) =
            Some(Arc::new(move || -> Box<dyn McpTransport> {
                factory_constructions.fetch_add(1, Ordering::AcqRel);
                Box::new(FailingInitializeTransport {
                    closed: AtomicBool::new(false),
                })
            }));
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");

        for failure_number in 1..=MAX_RESTARTS {
            McpManager::lock(&entry.restarts).next_retry_at = None;
            let error = runtime
                .block_on(manager.ensure_ready(&entry))
                .expect_err("fixture initialize must fail");
            assert!(error.to_string().contains("MCP_TRANSPORT_IO"), "{error}");
            assert_eq!(McpManager::lock(&entry.restarts).count, failure_number);
        }
        assert_eq!(
            constructions.load(Ordering::Acquire),
            usize::try_from(MAX_RESTARTS).expect("restart budget fits usize")
        );
        assert!(matches!(
            &*McpManager::lock(&entry.health),
            ServerHealth::Failed { reason } if reason.contains("fixture initialize failure")
        ));
        let exhausted = runtime
            .block_on(manager.ensure_ready(&entry))
            .expect_err("terminal failure must block further automatic construction");
        assert!(
            exhausted.to_string().contains("MCP_RESTART_EXHAUSTED"),
            "{exhausted}"
        );
        assert_eq!(
            constructions.load(Ordering::Acquire),
            usize::try_from(MAX_RESTARTS).expect("restart budget fits usize")
        );
    }

    #[test]
    fn only_current_generation_tool_success_resets_the_restart_budget() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry) = trusted_fixture_manager(&temp);
        let current: Arc<dyn McpTransport> = Arc::new(SuccessfulTransport {
            closed: Arc::new(AtomicBool::new(false)),
        });
        *McpManager::lock(&entry.transport) = Some(Arc::clone(&current));
        *McpManager::lock(&entry.restarts) = RestartState {
            count: 2,
            next_retry_at: Some(Instant::now() + Duration::from_secs(10)),
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime
            .block_on(manager.call_on_transport(&entry, &current, "echo", &serde_json::json!({})))
            .expect("current-generation tool call");
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
        assert!(McpManager::lock(&entry.restarts).next_retry_at.is_none());

        let replacement: Arc<dyn McpTransport> = Arc::new(SuccessfulTransport {
            closed: Arc::new(AtomicBool::new(false)),
        });
        let stale: Arc<dyn McpTransport> = Arc::new(SuccessfulTransport {
            closed: Arc::new(AtomicBool::new(false)),
        });
        *McpManager::lock(&entry.transport) = Some(Arc::clone(&replacement));
        *McpManager::lock(&entry.restarts) = RestartState {
            count: 2,
            next_retry_at: Some(Instant::now() + Duration::from_secs(10)),
        };
        runtime
            .block_on(manager.call_on_transport(&entry, &stale, "echo", &serde_json::json!({})))
            .expect("stale transport response is still a completed call");
        assert_eq!(
            McpManager::lock(&entry.restarts).count,
            2,
            "stale success must not reset replacement generation state"
        );
        assert!(McpManager::lock(&entry.restarts).next_retry_at.is_some());
        let published = McpManager::lock(&entry.transport)
            .clone()
            .expect("replacement remains published");
        assert!(Arc::ptr_eq(&published, &replacement));
    }

    #[test]
    fn startup_budget_closes_and_marks_unfinished_servers_unhealthy() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("project");
        let global = temp.path().join("global");
        std::fs::create_dir_all(&cwd).expect("project directory");
        std::fs::create_dir_all(&global).expect("global directory");
        let config = ConfiguredServer {
            name: "fixture".to_string(),
            command: Some("unused-fixture".to_string()),
            args: Vec::new(),
            env: Vec::new(),
            url: None,
            headers: Vec::new(),
            transport_hint: Some("stdio".to_string()),
            provenance: Provenance::ProjectPi,
            source_file: cwd.join(".pi/mcp.json"),
        };
        let manager = McpManager::new(
            &cwd,
            &global,
            McpDiscovery {
                servers: vec![config],
                warnings: Vec::new(),
            },
        );
        let entry = manager.entry("fixture").expect("fixture entry");
        let fingerprint = manager.trust_fingerprint_for(&entry);
        let mut trust = TrustStore::load(&global.join("mcp-trust.json")).expect("load trust");
        trust
            .acknowledge("fixture", &fingerprint, "operator")
            .expect("acknowledge fixture");
        let (started_tx, started_rx) = mpsc::channel();
        let hanging = Arc::new(HangingToolsTransport {
            started: Mutex::new(Some(started_tx)),
            closed: AtomicBool::new(false),
        });
        let transport: Arc<dyn McpTransport> = hanging.clone();
        *McpManager::lock(&entry.transport) = Some(transport);
        *McpManager::lock(&entry.health) = ServerHealth::Ready { tools: 0 };

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(manager.connect_trusted_with_budget(Duration::from_millis(20)));

        started_rx
            .recv_timeout(Duration::from_millis(50))
            .expect("tools/list must enter its pending state before cleanup");
        assert!(hanging.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert!(McpManager::lock(&entry.tools_cache).is_none());
        assert!(matches!(
            &*McpManager::lock(&entry.health),
            ServerHealth::Unhealthy { reason, .. } if reason.contains("MCP_STARTUP_TIMEOUT")
        ));
    }

    #[test]
    fn startup_holds_one_lane_through_generation_capture_and_tool_list() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("project");
        let global = temp.path().join("global");
        std::fs::create_dir_all(&cwd).expect("project directory");
        std::fs::create_dir_all(&global).expect("global directory");
        let config = ConfiguredServer {
            name: "fixture".to_string(),
            command: Some("unused-fixture".to_string()),
            args: Vec::new(),
            env: Vec::new(),
            url: None,
            headers: Vec::new(),
            transport_hint: Some("stdio".to_string()),
            provenance: Provenance::ProjectPi,
            source_file: cwd.join(".pi/mcp.json"),
        };
        let manager = McpManager::new(
            &cwd,
            &global,
            McpDiscovery {
                servers: vec![config],
                warnings: Vec::new(),
            },
        );
        let entry = manager.entry("fixture").expect("fixture entry");
        let fingerprint = manager.trust_fingerprint_for(&entry);
        let mut trust = TrustStore::load(&global.join("mcp-trust.json")).expect("load trust");
        trust
            .acknowledge("fixture", &fingerprint, "operator")
            .expect("acknowledge fixture");

        let (started_tx, started_rx) = mpsc::channel();
        let original = Arc::new(HangingToolsTransport {
            started: Mutex::new(Some(started_tx)),
            closed: AtomicBool::new(false),
        });
        let original_erased: Arc<dyn McpTransport> = original.clone();
        *McpManager::lock(&entry.transport) = Some(original_erased);
        *McpManager::lock(&entry.health) = ServerHealth::Ready { tools: 0 };

        let replacement = Arc::new(HangingToolsTransport {
            started: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        let replacement_erased: Arc<dyn McpTransport> = replacement.clone();
        let contender_acquired = Arc::new(AtomicBool::new(false));
        let contender_acquired_worker = Arc::clone(&contender_acquired);
        let contender_entry = Arc::clone(&entry);
        let (contender_go_tx, contender_go_rx) = mpsc::channel();
        let (contender_done_tx, contender_done_rx) = mpsc::channel();
        let contender = std::thread::spawn(move || {
            contender_go_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("startup generation hook");
            if let Ok(_guard) = Arc::clone(&contender_entry.connect_lane).try_lock_owned() {
                contender_acquired_worker.store(true, Ordering::Release);
                *McpManager::lock(&contender_entry.transport) = Some(replacement_erased);
                *McpManager::lock(&contender_entry.tools_cache) = Some((
                    Instant::now(),
                    vec![McpToolMeta {
                        name: "replacement-tool".to_string(),
                        description: String::new(),
                        input_schema: serde_json::json!({}),
                    }],
                ));
                *McpManager::lock(&contender_entry.health) = ServerHealth::Ready { tools: 1 };
            }
            contender_done_tx.send(()).expect("contender completion");
        });
        let contender_done_rx = Arc::new(Mutex::new(contender_done_rx));
        *McpManager::lock(&manager.inner.startup_after_generation_hook) =
            Some(Arc::new(move || {
                contender_go_tx.send(()).expect("start lane contender");
                McpManager::lock(&contender_done_rx)
                    .recv_timeout(Duration::from_secs(1))
                    .expect("lane contender completion");
            }));

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(manager.connect_trusted_with_budget(Duration::from_millis(20)));
        contender.join().expect("lane contender");

        started_rx
            .recv_timeout(Duration::from_millis(50))
            .expect("the captured transport must receive tools/list");
        assert!(
            !contender_acquired.load(Ordering::Acquire),
            "startup must not release the lane between generation capture and tools/list"
        );
        assert!(original.closed.load(Ordering::Acquire));
        assert!(!replacement.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert!(matches!(
            &*McpManager::lock(&entry.health),
            ServerHealth::Unhealthy { reason, .. } if reason.contains("MCP_STARTUP_TIMEOUT")
        ));
    }

    #[test]
    fn private_handshake_transport_aborts_when_its_future_is_dropped() {
        let transport = Arc::new(HangingToolsTransport {
            started: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        let erased: Arc<dyn McpTransport> = transport.clone();
        {
            let _private = PrivateHandshakeTransport::new(erased);
        }
        assert!(
            transport.closed.load(Ordering::Acquire),
            "dropping a cancelled private handshake must abort its transport"
        );
    }

    #[test]
    fn startup_budget_aborts_a_private_transport_stalled_in_initialize() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry) = trusted_fixture_manager(&temp);

        let (started_tx, started_rx) = mpsc::channel();
        let state = Arc::new(HangingInitializeState {
            started: Mutex::new(Some(started_tx)),
            closed: AtomicBool::new(false),
        });
        let factory_state = Arc::clone(&state);
        *McpManager::lock(&manager.inner.transport_factory) =
            Some(Arc::new(move || -> Box<dyn McpTransport> {
                Box::new(HangingInitializeTransport {
                    state: Arc::clone(&factory_state),
                })
            }));

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(manager.connect_trusted_with_budget(Duration::from_millis(20)));

        started_rx
            .recv_timeout(Duration::from_millis(50))
            .expect("initialize must enter its pending state before timeout cleanup");
        assert!(
            state.closed.load(Ordering::Acquire),
            "dropping the full manager startup path must abort its private transport"
        );
        assert!(McpManager::lock(&entry.transport).is_none());
        assert!(matches!(
            &*McpManager::lock(&entry.health),
            ServerHealth::Unhealthy { reason, .. } if reason.contains("MCP_STARTUP_TIMEOUT")
        ));
    }

    #[test]
    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    fn startup_budget_preempts_blocked_synchronous_transport_construction() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry) = trusted_fixture_manager(&temp);
        let manager = Arc::new(manager);

        let (factory_started_tx, factory_started_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let factory_release = Arc::clone(&release);
        let closed = Arc::new(AtomicBool::new(false));
        let factory_closed = Arc::clone(&closed);
        *McpManager::lock(&manager.inner.transport_factory) =
            Some(Arc::new(move || -> Box<dyn McpTransport> {
                factory_started_tx
                    .send(())
                    .expect("report blocked transport construction");
                let (released, wake) = &*factory_release;
                let mut released = McpManager::lock(released);
                while !*released {
                    released = wake
                        .wait(released)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                Box::new(SuccessfulTransport {
                    closed: Arc::clone(&factory_closed),
                })
            }));

        let connecting_manager = Arc::clone(&manager);
        let (completed_tx, completed_rx) = mpsc::channel();
        let connecting = std::thread::spawn(move || {
            let runtime = asupersync::runtime::RuntimeBuilder::new()
                .worker_threads(1)
                .blocking_threads(1, 2)
                .build()
                .expect("runtime");
            runtime.block_on(
                connecting_manager.connect_trusted_with_budget(Duration::from_millis(50)),
            );
            let _ = completed_tx.send(());
        });
        factory_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("factory reached the controlled blocking seam");
        let completed_within_budget = completed_rx
            .recv_timeout(Duration::from_millis(500))
            .is_ok();

        // Always release and join before asserting, so regressing construction
        // to the async worker produces a bounded red test rather than a hang.
        let (released, wake) = &*release;
        *McpManager::lock(released) = true;
        wake.notify_all();
        if !completed_within_budget {
            completed_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("connection worker finishes after controlled release");
        }
        connecting.join().expect("connection worker");
        assert!(
            completed_within_budget,
            "startup budget must remain pollable during synchronous construction"
        );
        assert!(McpManager::lock(&entry.transport).is_none());
        assert!(matches!(
            &*McpManager::lock(&entry.health),
            ServerHealth::Unhealthy { reason, .. } if reason.contains("MCP_STARTUP_TIMEOUT")
        ));

        let cleanup_deadline = Instant::now() + Duration::from_secs(1);
        while !closed.load(Ordering::Acquire) {
            assert!(
                Instant::now() < cleanup_deadline,
                "late transport construction was not aborted after cancellation"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            McpManager::lock(&entry.transport).is_none(),
            "an abandoned construction must never publish a transport"
        );
    }

    #[test]
    fn startup_timeout_cleanup_does_not_poison_a_replacement_transport() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = ConfiguredServer {
            name: "fixture".to_string(),
            command: Some("unused-fixture".to_string()),
            args: Vec::new(),
            env: Vec::new(),
            url: None,
            headers: Vec::new(),
            transport_hint: Some("stdio".to_string()),
            provenance: Provenance::ProjectPi,
            source_file: temp.path().join("mcp.json"),
        };
        let manager = McpManager::new(
            temp.path(),
            &temp.path().join("global"),
            McpDiscovery {
                servers: vec![config],
                warnings: Vec::new(),
            },
        );
        let entry = manager.entry("fixture").expect("fixture entry");
        let timed_out = Arc::new(HangingToolsTransport {
            started: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        let replacement = Arc::new(HangingToolsTransport {
            started: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        let timed_out_erased: Arc<dyn McpTransport> = timed_out.clone();
        let replacement_erased: Arc<dyn McpTransport> = replacement.clone();
        *McpManager::lock(&entry.transport) = Some(Arc::clone(&replacement_erased));
        *McpManager::lock(&entry.tools_cache) = Some((
            Instant::now(),
            vec![McpToolMeta {
                name: "replacement-tool".to_string(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            }],
        ));
        *McpManager::lock(&entry.health) = ServerHealth::Ready { tools: 1 };

        assert!(
            !McpManager::fail_timed_out_startup_attempt(&entry, Some(timed_out_erased),),
            "generation mismatch must leave replacement state untouched"
        );
        assert!(timed_out.closed.load(Ordering::Acquire));
        assert!(!replacement.closed.load(Ordering::Acquire));
        let current = McpManager::lock(&entry.transport)
            .clone()
            .expect("replacement remains published");
        assert!(Arc::ptr_eq(&current, &replacement_erased));
        assert!(McpManager::lock(&entry.tools_cache).is_some());
        assert!(matches!(
            &*McpManager::lock(&entry.health),
            ServerHealth::Ready { tools: 1 }
        ));

        let revoked = Arc::new(HangingToolsTransport {
            started: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        let revoked_erased: Arc<dyn McpTransport> = revoked.clone();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(McpManager::close_revoked_transport(&entry, &revoked_erased));
        assert!(
            revoked.closed.load(Ordering::Acquire),
            "the obsolete revoked generation must be aborted"
        );
        assert!(!replacement.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.tools_cache).is_some());
        assert!(matches!(
            &*McpManager::lock(&entry.health),
            ServerHealth::Ready { tools: 1 }
        ));

        let failed_call = Arc::new(HangingToolsTransport {
            started: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        let failed_call_erased: Arc<dyn McpTransport> = failed_call.clone();
        let failure = tool_err("MCP_TRANSPORT_IO", "lost old connection");
        assert!(
            !McpManager::detach_failed_call_transport(&entry, &failed_call_erased, &failure,),
            "a stale call failure must not detach or poison its replacement"
        );
        assert!(failed_call.closed.load(Ordering::Acquire));
        assert!(!replacement.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.tools_cache).is_some());
        assert!(matches!(
            &*McpManager::lock(&entry.health),
            ServerHealth::Ready { tools: 1 }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn command_secret_resolution_holds_the_manager_execution_lock() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("project");
        let global = temp.path().join("global");
        std::fs::create_dir_all(&cwd).expect("project directory");
        std::fs::create_dir_all(&global).expect("global directory");
        let started = temp.path().join("resolver-started");
        let release = temp.path().join("resolver-release");
        let command = format!(
            "$CMD:printf started > '{}'; while [ ! -e '{}' ]; do sleep 0.01; done; printf token",
            started.display(),
            release.display()
        );
        let config = ConfiguredServer {
            name: "fixture".to_string(),
            command: None,
            args: Vec::new(),
            env: Vec::new(),
            url: Some("http://127.0.0.1:1/mcp".to_string()),
            headers: vec![("Authorization".to_string(), command)],
            transport_hint: Some("http".to_string()),
            provenance: Provenance::ProjectPi,
            source_file: cwd.join(".pi/mcp.json"),
        };
        let manager = Arc::new(McpManager::new(
            &cwd,
            &global,
            McpDiscovery {
                servers: vec![config],
                warnings: Vec::new(),
            },
        ));
        let entry = manager.entry("fixture").expect("fixture entry");
        let fingerprint = manager.trust_fingerprint_for(&entry);
        let trust_path = global.join("mcp-trust.json");
        TrustStore::load(&trust_path)
            .expect("load trust")
            .acknowledge("fixture", &fingerprint, "operator")
            .expect("acknowledge fixture");

        let connecting_manager = Arc::clone(&manager);
        let connecting_entry = Arc::clone(&entry);
        let connecting = std::thread::spawn(move || {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("runtime");
            runtime.block_on(connecting_manager.ensure_ready(&connecting_entry))
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !started.exists() {
            assert!(Instant::now() < deadline, "secret resolver never started");
            std::thread::sleep(Duration::from_millis(10));
        }

        let lock_attempt = super::super::trust::acquire_global_trust_lock_for(
            &trust_path,
            Duration::from_millis(25),
        );
        // Always release and join the helper before asserting the intended
        // red condition, so a failing mutation cannot strand the resolver.
        std::fs::write(&release, b"release").expect("release secret resolver");
        let connecting_result = connecting.join().expect("connecting thread");
        let error = lock_attempt
            .expect_err("manager must retain the execution lock through command resolution");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            connecting_result.is_err(),
            "the loopback endpoint intentionally has no MCP server"
        );
    }

    struct HeldRequestTransport {
        started: Mutex<Option<mpsc::Sender<()>>>,
        release: Arc<(Mutex<bool>, Condvar)>,
        response: Value,
        closed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl McpTransport for HeldRequestTransport {
        // Guard scope is deliberate; tightening drops would change lock-hold semantics.
        #[allow(
            clippy::significant_drop_in_scrutinee,
            clippy::significant_drop_tightening
        )]
        async fn request(
            &self,
            _method: &str,
            _params: Value,
            _timeout: Duration,
        ) -> Result<Value> {
            if let Some(started) = McpManager::lock(&self.started).take() {
                let _ = started.send(());
            }
            let (released, wake) = &*self.release;
            let mut released = McpManager::lock(released);
            while !*released {
                released = wake
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            Ok(self.response.clone())
        }

        async fn notify(&self, _method: &str, _params: Value) -> Result<()> {
            Ok(())
        }

        fn is_alive(&self) -> bool {
            !self.closed.load(Ordering::Acquire)
        }

        fn abort(&self) {
            self.closed.store(true, Ordering::Release);
        }

        async fn close(&self) {
            self.abort();
        }

        fn diagnostics_tail(&self) -> String {
            String::new()
        }
    }

    fn assert_stale_tools_list_preserves_replacement(response: Value, expected_error_code: &str) {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("project");
        let global = temp.path().join("global");
        std::fs::create_dir_all(&cwd).expect("project directory");
        std::fs::create_dir_all(&global).expect("global directory");
        let config = ConfiguredServer {
            name: "fixture".to_string(),
            command: Some("unused-fixture".to_string()),
            args: Vec::new(),
            env: Vec::new(),
            url: None,
            headers: Vec::new(),
            transport_hint: Some("stdio".to_string()),
            provenance: Provenance::ProjectPi,
            source_file: cwd.join(".pi/mcp.json"),
        };
        let manager = Arc::new(McpManager::new(
            &cwd,
            &global,
            McpDiscovery {
                servers: vec![config],
                warnings: Vec::new(),
            },
        ));
        let entry = manager.entry("fixture").expect("fixture entry");
        let fingerprint = manager.trust_fingerprint_for(&entry);
        let mut trust = TrustStore::load(&global.join("mcp-trust.json")).expect("load trust");
        trust
            .acknowledge("fixture", &fingerprint, "operator")
            .expect("acknowledge fixture");

        let (started_tx, started_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let stale = Arc::new(HeldRequestTransport {
            started: Mutex::new(Some(started_tx)),
            release: Arc::clone(&release),
            response,
            closed: Arc::new(AtomicBool::new(false)),
        });
        let stale_erased: Arc<dyn McpTransport> = stale.clone();
        *McpManager::lock(&entry.transport) = Some(stale_erased);

        let listing_manager = Arc::clone(&manager);
        let listing_entry = Arc::clone(&entry);
        let listing = std::thread::spawn(move || {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("runtime");
            runtime.block_on(listing_manager.list_and_cache_tools(&listing_entry))
        });
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("tools/list reached controlled in-flight seam");

        let replacement = Arc::new(HangingToolsTransport {
            started: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        let replacement_erased: Arc<dyn McpTransport> = replacement.clone();
        *McpManager::lock(&entry.transport) = Some(Arc::clone(&replacement_erased));
        *McpManager::lock(&entry.tools_cache) = Some((
            Instant::now(),
            vec![McpToolMeta {
                name: "replacement-tool".to_string(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            }],
        ));
        *McpManager::lock(&entry.health) = ServerHealth::Ready { tools: 1 };
        let (released, wake) = &*release;
        *McpManager::lock(released) = true;
        wake.notify_all();

        let error = listing
            .join()
            .expect("tools/list thread")
            .expect_err("stale tools/list generation must not publish");
        assert!(
            error.to_string().contains(expected_error_code),
            "unexpected stale tools/list error: {error}"
        );
        assert!(stale.closed.load(Ordering::Acquire));
        assert!(!replacement.closed.load(Ordering::Acquire));
        let current = McpManager::lock(&entry.transport)
            .clone()
            .expect("replacement remains published");
        assert!(Arc::ptr_eq(&current, &replacement_erased));
        let cached = McpManager::lock(&entry.tools_cache)
            .clone()
            .expect("replacement tools remain cached");
        assert_eq!(cached.1[0].name, "replacement-tool");
        assert!(matches!(
            &*McpManager::lock(&entry.health),
            ServerHealth::Ready { tools: 1 }
        ));
    }

    #[test]
    fn stale_successful_tools_list_cannot_publish_over_replacement() {
        assert_stale_tools_list_preserves_replacement(
            serde_json::json!({
                "tools": [{"name": "stale-tool", "inputSchema": {}}]
            }),
            "MCP_TRANSPORT_SUPERSEDED",
        );
    }

    #[test]
    fn stale_malformed_tools_list_cannot_poison_replacement() {
        assert_stale_tools_list_preserves_replacement(
            serde_json::json!({"tools": [{"name": "broken"}]}),
            "MCP_PROTOCOL",
        );
    }

    #[test]
    fn denial_during_request_rejects_response_and_closes_transport() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("project");
        let global = temp.path().join("global");
        std::fs::create_dir_all(&cwd).expect("project directory");
        std::fs::create_dir_all(&global).expect("global directory");
        let config = ConfiguredServer {
            name: "fixture".to_string(),
            command: Some("unused-fixture".to_string()),
            args: Vec::new(),
            env: Vec::new(),
            url: None,
            headers: Vec::new(),
            transport_hint: Some("stdio".to_string()),
            provenance: Provenance::ProjectPi,
            source_file: cwd.join(".pi/mcp.json"),
        };
        let manager = Arc::new(McpManager::new(
            &cwd,
            &global,
            McpDiscovery {
                servers: vec![config],
                warnings: Vec::new(),
            },
        ));
        let entry = manager.entry("fixture").expect("fixture entry");
        let fingerprint = manager.trust_fingerprint_for(&entry);
        let mut trust = TrustStore::load(&global.join("mcp-trust.json")).expect("load trust");
        trust
            .acknowledge("fixture", &fingerprint, "operator")
            .expect("acknowledge fixture");

        let (started_tx, started_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let fake = Arc::new(HeldRequestTransport {
            started: Mutex::new(Some(started_tx)),
            release: Arc::clone(&release),
            response: serde_json::json!({"content": []}),
            closed: Arc::new(AtomicBool::new(false)),
        });
        let transport: Arc<dyn McpTransport> = fake.clone();
        *McpManager::lock(&entry.transport) = Some(Arc::clone(&transport));

        let caller_manager = Arc::clone(&manager);
        let caller_entry = Arc::clone(&entry);
        let caller = std::thread::spawn(move || {
            let runtime = asupersync::runtime::RuntimeBuilder::new()
                .enable_parking(false)
                .worker_threads(1)
                .blocking_threads(1, 2)
                .build()
                .expect("runtime");
            runtime.block_on(caller_manager.call_on_transport(
                &caller_entry,
                &transport,
                "echo",
                &serde_json::json!({"text": "must not escape"}),
            ))
        });
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("request reached controlled in-flight seam");

        let mut denying_store =
            TrustStore::load(&global.join("mcp-trust.json")).expect("reload shared trust");
        denying_store
            .deny("fixture", &fingerprint, "operator")
            .expect("persist concurrent denial");
        let (released, wake) = &*release;
        *McpManager::lock(released) = true;
        wake.notify_all();

        let error = caller
            .join()
            .expect("request thread")
            .expect_err("response after denial must be rejected");
        assert!(error.to_string().contains("MCP_TRUST_DENIED"), "{error}");
        assert!(
            fake.closed.load(Ordering::Acquire),
            "the manager observing revocation must close its transport"
        );
        assert!(McpManager::lock(&entry.transport).is_none());
    }

    // ===== bd-hyik7: extension spec field preservation =====

    /// Authenticated HTTP extension servers keep their configured headers
    /// through normalization.
    #[test]
    fn extension_registration_preserves_http_headers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _entry_unused) = trusted_fixture_manager(&temp);
        manager.register_extension_server(
            "auth-http",
            &serde_json::json!({
                "url": "https://example.invalid/mcp",
                "transport": "http",
                "headers": {"Authorization": "Bearer abc"}
            }),
        );
        let entry = manager.entry("auth-http").expect("registered");
        assert_eq!(
            entry.config.headers,
            vec![("Authorization".to_string(), "Bearer abc".to_string())]
        );
    }

    /// stdio cwd intent is honored: the per-entry override anchors relative
    /// commands, and the bound fingerprint changes with it.
    #[test]
    fn extension_registration_honors_cwd_override() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, fixture_entry) = trusted_fixture_manager(&temp);
        let helper_dir = temp.path().join("ext-cwd");
        std::fs::create_dir_all(&helper_dir).expect("mkdir ext cwd");

        manager.register_extension_server(
            "cwd-srv",
            &serde_json::json!({
                "command": "./serve.sh",
                "type": "stdio",
                "cwd": helper_dir.display().to_string()
            }),
        );
        let entry = manager.entry("cwd-srv").expect("registered cwd server");
        assert_eq!(
            entry.cwd_override.as_deref(),
            Some(helper_dir.as_path()),
            "cwd override stored (canonicalized)"
        );

        let with_override = manager.trust_fingerprint_for(&entry);
        let without_override = manager.trust_fingerprint(&entry.config);
        assert_ne!(
            with_override, without_override,
            "fingerprint must bind the per-entry cwd"
        );
        assert_ne!(
            manager.trust_fingerprint_for(&fixture_entry),
            with_override,
            "unrelated entry unaffected"
        );
    }

    /// Malformed header shapes fail closed: registration is rejected and no
    /// entry exists.
    #[test]
    fn extension_registration_rejects_malformed_headers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, _unused) = trusted_fixture_manager(&temp);
        manager.register_extension_server(
            "bad-headers",
            &serde_json::json!({
                "url": "https://example.invalid/mcp",
                "transport": "http",
                "headers": {"Authorization": 42}
            }),
        );
        assert!(manager.entry("bad-headers").is_err());
    }

    /// bd-vjfol (defect b): `/mcp test` deliberately resets the restart
    /// budget, so a server that exhausted MAX_RESTARTS recovers through it.
    /// Mutation-sensitive: removing the reset makes `check_restart_budget`
    /// reject the reconnect with MCP_RESTART_EXHAUSTED.
    #[test]
    fn mcp_test_resets_exhausted_restart_budget_and_recovers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry) = trusted_fixture_manager(&temp);
        let manager = Arc::new(manager);

        // Simulate a fully exhausted budget with an armed backoff window.
        *McpManager::lock(&entry.restarts) = RestartState {
            count: MAX_RESTARTS,
            next_retry_at: Some(Instant::now() + Duration::from_secs(3600)),
        };
        *McpManager::lock(&entry.health) = ServerHealth::Failed {
            reason: "fixture: exhausted".to_string(),
        };

        let closed = Arc::new(AtomicBool::new(false));
        *McpManager::lock(&manager.inner.transport_factory) =
            Some(Arc::new(move || -> Box<dyn McpTransport> {
                Box::new(SuccessfulTransport {
                    closed: Arc::clone(&closed),
                })
            }));

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let tools = runtime
            .block_on(manager.test("fixture"))
            .expect("exhausted server must recover through /mcp test");

        // SuccessfulTransport advertises an empty (but parsed) tool list.
        assert!(tools.is_empty());
        let restarts = McpManager::lock(&entry.restarts);
        assert_eq!(restarts.count, 0, "budget reset before reconnecting");
        assert!(restarts.next_retry_at.is_none());
        drop(restarts);
        assert!(matches!(
            &*McpManager::lock(&entry.health),
            ServerHealth::Ready { .. }
        ));
        assert!(McpManager::lock(&entry.transport).is_some());
    }

    /// bd-vjfol (defect a): the single startup connect-and-mount pass covers
    /// every registered definition — built-in AND extension-provided — and
    /// never double-connects on repeated passes. Late registration after the
    /// pass is a CALLER-ordering contract: it stays unconnected until an
    /// explicit surface (like `/mcp test`) runs, which is exactly why
    /// main.rs/sdk.rs must register extension servers before calling
    /// connect_trusted.
    #[test]
    fn startup_pass_connects_all_acknowledged_exactly_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, fixture_entry) = trusted_fixture_manager(&temp);
        let manager = Arc::new(manager);
        let fixture_command = fixture_entry
            .config
            .command
            .clone()
            .expect("fixture command");

        manager.register_extension_server(
            "ext-srv",
            &serde_json::json!({
                "command": fixture_command,
                "type": "stdio",
                "trust": "acknowledged"
            }),
        );
        let ext_entry = manager.entry("ext-srv").expect("ext entry");
        // Extension-provided definitions reach the SAME trust gate; a spec
        // acknowledged up front participates in the startup pass.
        acknowledge_test_stdio_server(&manager, &ext_entry);

        let factory_calls = Arc::new(AtomicUsize::new(0));
        let calls_for_factory = Arc::clone(&factory_calls);
        let closed = Arc::new(AtomicBool::new(false));
        *McpManager::lock(&manager.inner.transport_factory) =
            Some(Arc::new(move || -> Box<dyn McpTransport> {
                calls_for_factory.fetch_add(1, Ordering::AcqRel);
                Box::new(SuccessfulTransport {
                    closed: Arc::clone(&closed),
                })
            }));

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(manager.connect_trusted_with_budget(Duration::from_secs(2)));

        assert_eq!(
            factory_calls.load(Ordering::Acquire),
            2,
            "one spawn per acknowledged definition (fixture + extension)"
        );
        assert!(McpManager::lock(&fixture_entry.transport).is_some());
        assert!(McpManager::lock(&ext_entry.transport).is_some());

        // A second startup pass must be idempotent: live transports are
        // reused, so no additional spawn (no double connection) occurs and
        // the mounted tool cache is not duplicated.
        runtime.block_on(manager.connect_trusted_with_budget(Duration::from_secs(2)));
        assert_eq!(
            factory_calls.load(Ordering::Acquire),
            2,
            "repeated startup pass must reuse live transports"
        );
        assert!(
            McpManager::lock(&ext_entry.tools_cache).is_some(),
            "extension tools mounted exactly once"
        );

        // Late registration after the pass stays dormant by contract.
        manager.register_extension_server(
            "late-srv",
            &serde_json::json!({"command": "unused-late", "type": "stdio"}),
        );
        let late_entry = manager.entry("late-srv").expect("late entry");
        assert!(
            McpManager::lock(&late_entry.transport).is_none(),
            "post-pass registration must not spontaneously connect"
        );
    }
}
