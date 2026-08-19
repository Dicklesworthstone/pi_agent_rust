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
use super::trust::{TrustDecision, TrustStore};
use crate::error::{Error, Result};

/// Global deadline for eager startup connects (all trusted servers in
/// parallel; stragglers land as `Unhealthy` and are retried via `/mcp test`).
const STARTUP_CONNECT_BUDGET: Duration = Duration::from_secs(8);
/// Tool-list cache TTL.
const TOOL_CACHE_TTL: Duration = Duration::from_secs(300);
/// Max automatic restarts after a crash before the server is `Failed`.
const MAX_RESTARTS: u32 = 3;

fn tool_err(code: &str, message: impl Into<String>) -> Error {
    Error::tool("mcp", format!("[{code}] {}", message.into()))
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
                let decision = store.decision(&config.name, &config.fingerprint());
                let trust = match decision {
                    TrustDecision::Acknowledged => "acknowledged",
                    TrustDecision::Pending => "pending",
                    TrustDecision::Denied => "denied",
                };
                let health = Self::lock(&entry.health).clone();
                let tools = Self::lock(&entry.tools_cache)
                    .as_ref()
                    .map_or(0, |(_, tools)| tools.len());
                let target = config
                    .command
                    .clone()
                    .or_else(|| config.url.clone())
                    .unwrap_or_else(|| "<none>".to_string());
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
        let fingerprint = entry.config.fingerprint();
        {
            let _guard = Self::lock(&self.inner.trust_lock);
            let mut store = TrustStore::load(&self.inner.trust_path)?;
            store.acknowledge(name, &fingerprint, "operator")?;
        }
        self.connect_and_list(&entry).await
    }

    /// Deny a server (fail-closed; kills any live connection).
    ///
    /// # Errors
    ///
    /// Fails when the server is unknown or the store cannot persist.
    pub async fn deny(&self, name: &str) -> Result<()> {
        let entry = self.entry(name)?;
        let fingerprint = entry.config.fingerprint();
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
        self.connect_and_list(&entry).await
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
        // Fresh tools/list on an explicit test/trust path.
        let transport = Self::lock(&entry.transport).clone();
        let transport =
            transport.ok_or_else(|| tool_err("MCP_TRANSPORT_CLOSED", "not connected"))?;
        let tools = Self::fetch_tools(&transport).await?;
        *Self::lock(&entry.tools_cache) = Some((Instant::now(), tools.clone()));
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
            .decision(&entry.config.name, &entry.config.fingerprint());
        match decision {
            TrustDecision::Acknowledged => Ok(()),
            TrustDecision::Pending => Err(tool_err(
                "MCP_TRUST_PENDING",
                format!(
                    "server {:?} is pending trust; run /mcp trust {} to allow spawning it ({}:{})",
                    entry.config.name,
                    entry.config.name,
                    entry.config.command.as_deref().unwrap_or(""),
                    entry.config.args.join(" "),
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
        let existing = { Self::lock(&entry.transport).clone() };
        if let Some(transport) = existing.as_ref()
            && transport.is_alive()
        {
            return Ok(());
        }
        let crashed = existing.is_some();

        self.check_trust(entry)?;
        Self::check_restart_budget(entry)?;

        if crashed {
            // Close out the dead transport before respawning.
            if let Some(dead) = existing {
                dead.close().await;
            }
            Self::lock(&entry.transport).take();
        }

        let transport = match self.spawn_transport(entry).await {
            Ok(transport) => transport,
            Err(err) => {
                Self::record_failure(entry, &err);
                return Err(err);
            }
        };
        *Self::lock(&entry.transport) = Some(Arc::from(transport));

        // MCP handshake against the stored transport.
        let handshake = {
            let transport = Self::lock(&entry.transport).clone();
            let transport =
                transport.ok_or_else(|| tool_err("MCP_TRANSPORT_CLOSED", "not connected"))?;
            transport
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
                .map(|_result| transport)
        };
        let transport = match handshake {
            Ok(transport) => transport,
            Err(err) => {
                let dead = { Self::lock(&entry.transport).take() };
                if let Some(dead) = dead {
                    dead.close().await;
                }
                Self::record_failure(entry, &err);
                return Err(err);
            }
        };
        transport
            .notify("notifications/initialized", serde_json::json!({}))
            .await?;
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
        if config.is_http() {
            let url = config.url.clone().ok_or_else(|| {
                tool_err(
                    "MCP_CONFIG_INVALID",
                    format!("server {:?} is http-shaped but has no url", config.name),
                )
            })?;
            let headers = resolve_secrets(&config.headers)?;
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
        let env = resolve_secrets(&config.env)?;
        let transport =
            super::transport::StdioTransport::spawn(&command, &config.args, &env, &self.inner.cwd)?;
        Ok(Box::new(transport))
    }

    /// `tools/list` against a live transport.
    async fn fetch_tools(transport: &Arc<dyn McpTransport>) -> Result<Vec<McpToolMeta>> {
        let result = transport
            .request("tools/list", serde_json::json!({}), DEFAULT_MCP_TIMEOUT)
            .await?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::with_capacity(tools.len());
        for tool in tools {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            out.push(McpToolMeta {
                name: name.to_string(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                input_schema: tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"type": "object"})),
            });
        }
        Ok(out)
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
        let transport = { Self::lock(&entry.transport).clone() };
        let transport =
            transport.ok_or_else(|| tool_err("MCP_TRANSPORT_CLOSED", "not connected"))?;
        transport
            .request(
                "tools/call",
                serde_json::json!({ "name": tool, "arguments": arguments }),
                DEFAULT_MCP_TIMEOUT,
            )
            .await
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
        let servers = Self::lock(&self.inner.servers).clone();
        servers
            .values()
            .filter_map(|entry| {
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
                store.decision(&entry.config.name, &entry.config.fingerprint())
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
        let config = ConfiguredServer {
            name: name.to_string(),
            command: spec
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_string),
            args: spec
                .get("args")
                .and_then(Value::as_array)
                .map(|args| {
                    args.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            env: spec
                .get("env")
                .and_then(Value::as_object)
                .map(|env| {
                    env.iter()
                        .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                        .collect()
                })
                .unwrap_or_default(),
            url: spec.get("url").and_then(Value::as_str).map(str::to_string),
            headers: spec
                .get("headers")
                .and_then(Value::as_object)
                .map(|headers| {
                    headers
                        .iter()
                        .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                        .collect()
                })
                .unwrap_or_default(),
            transport_hint: spec.get("type").and_then(Value::as_str).map(str::to_string),
            provenance: Provenance::Extension,
            source_file: spec
                .get("extension_id")
                .and_then(Value::as_str)
                .map_or_else(
                    || PathBuf::from("<extension>"),
                    |id| PathBuf::from(format!("extension:{id}")),
                ),
        };
        let entry = Arc::new(ServerEntry {
            config,
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
    let message = err.to_string();
    message.contains("MCP_TRANSPORT_CLOSED") || message.contains("MCP_TRANSPORT_IO")
}

/// Resolve `$ENV:`/`$CMD:` secret references in env/header values.
fn resolve_secrets(entries: &[(String, String)]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::with_capacity(entries.len());
    for (name, raw) in entries {
        match crate::auth::resolve_secret_reference(raw) {
            Ok(Some(resolved)) => out.push((name.clone(), resolved)),
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
