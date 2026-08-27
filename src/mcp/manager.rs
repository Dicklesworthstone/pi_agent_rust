//! MCP client manager: one registry unifying file-configured, foreign, and
//! extension-registered servers; trust-gated connections; bounded restart
//! with backoff; tool-list caching (bd-cv653.6.1).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::config::{ConfiguredServer, McpDiscovery, Provenance};
use super::transport::{DEFAULT_MCP_TIMEOUT, MCP_PROTOCOL_VERSION, McpTransport};
use super::trust::{TrustDecision, TrustStore, TrustWriteGuard};
use crate::error::{Error, Result};

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
        .ok_or_else(|| tool_err("MCP_PROTOCOL", "tools/list result must contain a tools array"))?;
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
    warnings: Vec<super::config::ConfigWarning>,
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
                warnings: discovery.warnings,
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
                let fingerprint = self.trust_fingerprint(config);
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
        let fingerprint = self.trust_fingerprint(&entry.config);
        {
            let _guard = Self::lock(&self.inner.trust_lock);
            let mut store = TrustStore::load(&self.inner.trust_path)?;
            store.acknowledge(name, &fingerprint, "operator")?;
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
        let _connect_guard = asupersync::sync::OwnedMutexGuard::lock(
            Arc::clone(&entry.connect_lane),
            cx.cx(),
        )
        .await
        .map_err(|_| tool_err("MCP_CANCELLED", "cancelled while denying server"))?;
        let fingerprint = self.trust_fingerprint(&entry.config);
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

    /// Ping + tool list (the `/mcp test` surface).
    ///
    /// # Errors
    ///
    /// Fails closed on trust denial/pending or any transport error.
    pub async fn test(&self, name: &str) -> Result<Vec<McpToolMeta>> {
        let entry = self.entry(name)?;
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let connect_guard = asupersync::sync::OwnedMutexGuard::lock(
            Arc::clone(&entry.connect_lane),
            cx.cx(),
        )
        .await
        .map_err(|_| tool_err("MCP_CANCELLED", "cancelled while testing server"))?;
        *Self::lock(&entry.restarts) = RestartState::default();
        *Self::lock(&entry.health) = ServerHealth::NotStarted;
        self.ensure_ready_in_lane(&entry).await?;
        drop(connect_guard);
        self.list_and_cache_tools(&entry).await
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
        self.ensure_ready(entry).await?;
        self.list_and_cache_tools(entry).await
    }

    async fn list_and_cache_tools(&self, entry: &Arc<ServerEntry>) -> Result<Vec<McpToolMeta>> {
        // Fresh tools/list on an explicit test/trust path.
        let tools = match self.fetch_tools(entry).await {
            Ok(tools) => tools,
            Err(err) => {
                if is_trust_error(&err) {
                    let transport = { Self::lock(&entry.transport).clone() };
                    if let Some(transport) = transport {
                        Self::close_revoked_transport(entry, &transport).await;
                    } else {
                        *Self::lock(&entry.health) = ServerHealth::NotStarted;
                        Self::lock(&entry.tools_cache).take();
                    }
                } else {
                    let transport = { Self::lock(&entry.transport).take() };
                    if let Some(transport) = transport {
                        transport.close().await;
                    }
                    Self::record_failure(entry, &err);
                }
                return Err(err);
            }
        };
        // `fetch_tools` checks after the response; narrow the remaining
        // cross-process window again immediately before cache publication.
        if let Err(err) = self.check_trust(entry) {
            let transport = { Self::lock(&entry.transport).clone() };
            if let Some(transport) = transport {
                Self::close_revoked_transport(entry, &transport).await;
            }
            return Err(err);
        }
        *Self::lock(&entry.tools_cache) = Some((Instant::now(), tools.clone()));
        *Self::lock(&entry.health) = ServerHealth::Ready { tools: tools.len() };
        Ok(tools)
    }

    /// Ensure a live, initialized transport for the server.
    ///
    /// Restart discipline: a single crash is transparent (the next call
    /// respawns immediately and retries once, see `call_tool`). A crash
    /// LOOP engages the budget: every failed spawn/handshake increments the
    /// counter and arms an exponential backoff; calls inside the window
    /// fail with `[MCP_BACKOFF]`, and `MAX_RESTARTS` consecutive failures
    /// mark the server `Failed` until `/mcp test` revives it.
    /// Trust gate (fail-closed with a named remedy).
    fn check_trust(&self, entry: &Arc<ServerEntry>) -> Result<()> {
        let decision = self
            .trust_store()?
            .decision(
                &entry.config.name,
                &self.trust_fingerprint(&entry.config),
            );
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

    fn lock_trust_for_execution(&self, entry: &Arc<ServerEntry>) -> Result<TrustWriteGuard> {
        let fingerprint = self.trust_fingerprint(&entry.config);
        let mut store = TrustStore::load(&self.inner.trust_path)?;
        let (decision, guard) = store.locked_decision(&entry.config.name, &fingerprint)?;
        match decision {
            TrustDecision::Acknowledged => Ok(guard),
            TrustDecision::Pending => Err(tool_err(
                "MCP_TRUST_PENDING",
                format!(
                    "server {:?} became pending before local execution; inspect its source config, then run /mcp trust {} to allow it",
                    entry.config.name, entry.config.name,
                ),
            )),
            TrustDecision::Denied => Err(tool_err(
                "MCP_TRUST_DENIED",
                format!(
                    "server {:?} was denied before local execution and will not run",
                    entry.config.name
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
        let _connect_guard = asupersync::sync::OwnedMutexGuard::lock(
            Arc::clone(&entry.connect_lane),
            cx.cx(),
        )
        .await
        .map_err(|_| tool_err("MCP_CANCELLED", "cancelled while connecting server"))?;

        self.ensure_ready_in_lane(entry).await
    }

    async fn ensure_ready_in_lane(&self, entry: &Arc<ServerEntry>) -> Result<()> {
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
        let crashed = existing.is_some();
        Self::check_restart_budget(entry)?;

        if crashed {
            // Close out the dead transport before respawning.
            if let Some(dead) = existing {
                dead.close().await;
            }
            Self::lock(&entry.transport).take();
        }

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
        *Self::lock(&entry.transport) = Some(Arc::clone(&transport));
        // A denial can race the small check-to-publication interval above.
        // Recheck before returning the transport to any caller; on revocation,
        // remove exactly the transport this connect attempt published.
        if let Err(err) = self.check_trust(entry) {
            Self::close_revoked_transport(entry, &transport).await;
            return Err(err);
        }
        *Self::lock(&entry.health) = ServerHealth::Ready {
            tools: Self::lock(&entry.tools_cache)
                .as_ref()
                .map_or(0, |(_, tools)| tools.len()),
        };
        Self::lock(&entry.restarts).count = 0;
        Self::lock(&entry.restarts).next_retry_at = None;
        Ok(())
    }

    /// Record a failed spawn/handshake: increment the counter and arm the
    /// exponential backoff.
    fn record_failure(entry: &Arc<ServerEntry>, err: &Error) {
        let mut restarts = Self::lock(&entry.restarts);
        restarts.count += 1;
        let backoff = Duration::from_secs(1 << restarts.count.min(3));
        restarts.next_retry_at = Some(Instant::now() + backoff);
        *Self::lock(&entry.health) = ServerHealth::Unhealthy {
            reason: err.to_string(),
            retries: restarts.count,
        };
    }

    async fn spawn_transport(&self, entry: &Arc<ServerEntry>) -> Result<Box<dyn McpTransport>> {
        let config = &entry.config;
        // Hold the cross-process trust-store lock across every local execution
        // effect ($CMD resolution and stdio process creation). A concurrent
        // deny/reset either linearizes before this guard and blocks execution,
        // or waits and linearizes after it.
        let _trust_execution_guard = self.lock_trust_for_execution(entry)?;
        if config.is_http() {
            let url = config.url.clone().ok_or_else(|| {
                tool_err(
                    "MCP_CONFIG_INVALID",
                    format!("server {:?} is http-shaped but has no url", config.name),
                )
            })?;
            let headers = resolve_secrets(
                &config.headers,
                super::config::validate_http_header_value,
            )?;
            return Ok(Box::new(super::transport::HttpTransport::new(
                &url, headers,
            )?));
        }
        let command = config.command.clone().ok_or_else(|| {
            tool_err(
                "MCP_CONFIG_INVALID",
                format!("server {:?} has no command or url", config.name),
            )
        })?;
        let env = resolve_secrets(&config.env, super::config::validate_env_value)?;
        let transport =
            super::transport::StdioTransport::spawn(&command, &config.args, &env, &self.inner.cwd)?;
        Ok(Box::new(transport))
    }

    /// `tools/list` against a live transport.
    async fn fetch_tools(&self, entry: &Arc<ServerEntry>) -> Result<Vec<McpToolMeta>> {
        // `ensure_ready` releases its connection lane before this request.
        // Re-read the shared trust store here so a concurrent manager/process
        // cannot revoke trust between connection publication and tools/list.
        self.check_trust(entry)?;
        let transport = Self::lock(&entry.transport)
            .clone()
            .ok_or_else(|| tool_err("MCP_TRANSPORT_CLOSED", "not connected"))?;
        let result = transport
            .request("tools/list", serde_json::json!({}), DEFAULT_MCP_TIMEOUT)
            .await?;
        if let Err(err) = self.check_trust(entry) {
            Self::close_revoked_transport(entry, &transport).await;
            return Err(err);
        }
        parse_tool_list(&result)
    }

    /// Call one tool on one server.
    ///
    /// Trust-gated. When the transport dies mid-call, the server is restarted
    /// under the bounded-restart discipline and the call retried exactly
    /// once — a crash between calls is transparent to the agent, a crash
    /// storm is not.
    ///
    /// # Errors
    ///
    /// Trust-gated; transport and server errors carry taxonomy codes.
    pub async fn call_tool(&self, server: &str, tool: &str, arguments: Value) -> Result<Value> {
        let entry = self.entry(server)?;
        self.ensure_ready(&entry).await?;
        match self.call_on_transport(&entry, tool, &arguments).await {
            Ok(value) => Ok(value),
            Err(err) if is_transport_death(&err) => {
                // Count the crash, then let ensure_ready apply the bounded
                // restart discipline (immediate first restart, backoff after)
                // and retry once.
                Self::mark_unhealthy(&entry, &err);
                self.ensure_ready(&entry).await?;
                self.call_on_transport(&entry, tool, &arguments).await
            }
            Err(err) => Err(err),
        }
    }

    async fn call_on_transport(
        &self,
        entry: &Arc<ServerEntry>,
        tool: &str,
        arguments: &Value,
    ) -> Result<Value> {
        // The connection lane intentionally does not span a potentially long
        // tool call. Re-authorize at the request boundary so a trust decision
        // changed by another manager cannot be bypassed by an already-live
        // transport.
        self.check_trust(entry)?;
        let transport = { Self::lock(&entry.transport).clone() };
        let transport =
            transport.ok_or_else(|| tool_err("MCP_TRANSPORT_CLOSED", "not connected"))?;
        let result = transport
            .request(
                "tools/call",
                serde_json::json!({ "name": tool, "arguments": arguments }),
                DEFAULT_MCP_TIMEOUT,
            )
            .await?;
        if let Err(err) = self.check_trust(entry) {
            Self::close_revoked_transport(entry, &transport).await;
            return Err(err);
        }
        Ok(result)
    }

    async fn close_revoked_transport(
        entry: &Arc<ServerEntry>,
        transport: &Arc<dyn McpTransport>,
    ) {
        let removed = {
            let mut current = Self::lock(&entry.transport);
            if current
                .as_ref()
                .is_some_and(|candidate| Arc::ptr_eq(candidate, transport))
            {
                current.take()
            } else {
                None
            }
        };
        if let Some(removed) = removed {
            removed.close().await;
        }
        *Self::lock(&entry.health) = ServerHealth::NotStarted;
        Self::lock(&entry.tools_cache).take();
    }

    /// Record a transport death: restart count + health state. The backoff
    /// is armed by `ensure_ready` when the restart actually happens.
    fn mark_unhealthy(entry: &Arc<ServerEntry>, err: &Error) {
        let mut restarts = Self::lock(&entry.restarts);
        restarts.count += 1;
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
                if store.decision(
                    &entry.config.name,
                    &self.trust_fingerprint(&entry.config),
                ) != TrustDecision::Acknowledged
                {
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
        use futures::future::FutureExt;
        let Ok(store) = self.trust_store() else {
            return;
        };
        let servers = Self::lock(&self.inner.servers).clone();
        let pending: Vec<_> = servers
            .values()
            .filter(|entry| {
                store.decision(
                    &entry.config.name,
                    &self.trust_fingerprint(&entry.config),
                )
                    == TrustDecision::Acknowledged
            })
            .map(|entry| async move {
                let _ = self.connect_and_list(entry).await;
            })
            .collect();
        if pending.is_empty() {
            return;
        }
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let now = cx
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        let all = futures::future::join_all(pending).fuse();
        let deadline = asupersync::time::sleep(now, STARTUP_CONNECT_BUDGET).fuse();
        futures::pin_mut!(all, deadline);
        futures::select! {
            _ = all => {},
            () = deadline => {
                tracing::info!(
                    event = "pi.mcp.startup_budget_exhausted",
                    "MCP startup connects exceeded the global budget; stragglers stay Unhealthy"
                );
            },
        }
    }

    /// Register an extension-contributed server spec (`registerMcpServer`).
    /// Same registry, same trust gate: the spec flows through the identical
    /// spawn path as file-configured servers, with `provenance=extension`.
    /// Name collisions with existing entries are ignored (file config wins).
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
        let entry = Arc::new(ServerEntry {
            config,
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

/// Whether an error means the transport itself died (server crash), as
/// opposed to a server-level or protocol-level failure.
fn is_transport_death(err: &Error) -> bool {
    matches!(
        err,
        Error::Tool { tool, message }
            if tool == "mcp"
                && ["[MCP_TRANSPORT_CLOSED] ", "[MCP_TRANSPORT_IO] "]
                    .iter()
                    .any(|prefix| message.starts_with(prefix))
    )
}

fn is_trust_error(err: &Error) -> bool {
    matches!(
        err,
        Error::Tool { tool, message }
            if tool == "mcp"
                && [
                    "[MCP_TRUST_PENDING] ",
                    "[MCP_TRUST_DENIED] ",
                    "[MCP_TRUST_IO] ",
                    "[MCP_TRUST_CORRUPT] ",
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
    use std::sync::atomic::{AtomicBool, Ordering};
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
            let error = parse_tool_list(&malformed)
                .expect_err("malformed tools/list must fail as a whole");
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

        async fn close(&self) {
            self.closed.store(true, Ordering::Release);
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
        let fingerprint = manager.trust_fingerprint(&entry.config);
        let mut trust = TrustStore::load(&global.join("mcp-trust.json")).expect("load trust");
        trust
            .acknowledge("fixture", &fingerprint, "operator")
            .expect("acknowledge fixture");
        let malformed = Arc::new(MalformedToolsTransport {
            closed: AtomicBool::new(false),
        });
        let transport: Arc<dyn McpTransport> = malformed.clone();
        *McpManager::lock(&entry.transport) = Some(transport);

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
        assert!(matches!(
            &*McpManager::lock(&entry.health),
            ServerHealth::Unhealthy { .. }
        ));

        let misleading = Error::tool(
            "mcp",
            "[MCP_PROTOCOL] remote diagnostic mentioned MCP_TRUST_PENDING",
        );
        assert!(!is_trust_error(&misleading));
        let remote_transport_words = Error::tool(
            "mcp",
            "[MCP_REMOTE_ERROR] server mentioned MCP_TRANSPORT_CLOSED",
        );
        assert!(
            !is_transport_death(&remote_transport_words),
            "remote prose must not trigger a duplicate tool call retry"
        );
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
        let fingerprint = manager.trust_fingerprint(&entry.config);
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
        closed: AtomicBool,
    }

    #[async_trait]
    impl McpTransport for HeldRequestTransport {
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
            Ok(serde_json::json!({"content": []}))
        }

        async fn notify(&self, _method: &str, _params: Value) -> Result<()> {
            Ok(())
        }

        fn is_alive(&self) -> bool {
            !self.closed.load(Ordering::Acquire)
        }

        async fn close(&self) {
            self.closed.store(true, Ordering::Release);
        }

        fn diagnostics_tail(&self) -> String {
            String::new()
        }
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
        let fingerprint = manager.trust_fingerprint(&entry.config);
        let mut trust = TrustStore::load(&global.join("mcp-trust.json")).expect("load trust");
        trust
            .acknowledge("fixture", &fingerprint, "operator")
            .expect("acknowledge fixture");

        let (started_tx, started_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let fake = Arc::new(HeldRequestTransport {
            started: Mutex::new(Some(started_tx)),
            release: Arc::clone(&release),
            closed: AtomicBool::new(false),
        });
        let transport: Arc<dyn McpTransport> = fake.clone();
        *McpManager::lock(&entry.transport) = Some(transport);

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
}
