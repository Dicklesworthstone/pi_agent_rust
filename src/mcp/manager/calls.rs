//! Owned MCP tool execution. A cancelled or superseded call is never replayed.
//!
//! The connection lane only serializes setup/recovery, not tool execution.
//! Every in-flight call owns cleanup for its exact transport generation; it
//! cannot clear a replacement's cache or return an old generation's result.

use super::{
    Arc, DEFAULT_MCP_TIMEOUT, Duration, Error, McpManager, McpTransport, Result, ServerEntry,
    Value, catalog, is_indeterminate_call_delivery, tool_err,
};
use crate::agent_cx::AgentCx;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

/// Setup and recovery include more than JSON-RPC requests: lock admission,
/// secret resolution, initialization notifications and receive activation can
/// all suspend. Keep their entire future (including Drop) under one owner.
struct OwnedPhase<'a, F> {
    owner: &'a AgentCx,
    future: Option<Pin<Box<F>>>,
}

impl<F: Future> Future for OwnedPhase<'_, F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let _guard = this.owner.cx().clone().set_current_restricted();
        this.future
            .as_mut()
            .expect("active phase")
            .as_mut()
            .poll(task)
    }
}

impl<F> Drop for OwnedPhase<'_, F> {
    fn drop(&mut self) {
        let _guard = self.owner.cx().clone().set_current_restricted();
        drop(self.future.take());
    }
}

fn phase_timeout(phase: &str) -> Error {
    tool_err(
        "MCP_TIMEOUT",
        format!("MCP {phase} exceeded its manager deadline"),
    )
}

async fn phase_cancellation(owner: &AgentCx) {
    let (sender, mut receiver) = asupersync::channel::oneshot::channel::<()>();
    let _ = receiver.recv(owner.cx()).await;
    drop(sender);
}

/// The absolute deadline is shared across lock admission and all setup
/// stages. No stage can buy itself another thirty seconds. This is an async
/// bound, not preemption of blocking OS calls; construction's existing
/// abandonment guard rejects and aborts any late blocking-pool result.
async fn run_phase<T>(
    owner: &AgentCx,
    operation: impl Future<Output = Result<T>>,
    deadline: Instant,
    phase: &str,
) -> Result<T> {
    OwnedPhase {
        owner,
        future: Some(Box::pin(async {
            catalog::check_request_owner(owner)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(phase_timeout(phase));
            }
            let now = owner
                .cx()
                .timer_driver()
                .map_or_else(asupersync::time::wall_now, |timer| timer.now());
            let mut timer = std::pin::pin!(asupersync::time::sleep(now, remaining));
            let mut cancellation = std::pin::pin!(phase_cancellation(owner));
            let mut operation = std::pin::pin!(operation);
            poll_fn(|task| {
                if let Err(error) = catalog::check_request_owner(owner) {
                    return Poll::Ready(Err(error));
                }
                if cancellation.as_mut().poll(task).is_ready() {
                    return Poll::Ready(Err(tool_err("MCP_CANCELLED", "MCP phase cancelled")));
                }
                if Instant::now() >= deadline || timer.as_mut().poll(task).is_ready() {
                    return Poll::Ready(Err(phase_timeout(phase)));
                }
                let result = operation.as_mut().poll(task);
                if result.is_ready() {
                    if let Err(error) = catalog::check_request_owner(owner) {
                        return Poll::Ready(Err(error));
                    }
                    if Instant::now() >= deadline {
                        return Poll::Ready(Err(phase_timeout(phase)));
                    }
                }
                result
            })
            .await
        })),
    }
    .await
}

/// The caller retains the connection lane while this guard is armed. A
/// cancelled setup may finish publishing in the same poll as cancellation;
/// retire that new publication, never a healthy connection merely borrowed
/// by the attempt. Other setup attempts cannot publish until the lane drops.
struct SetupPublicationGuard {
    entry: Arc<ServerEntry>,
    original: Option<Arc<dyn McpTransport>>,
    armed: bool,
}

impl Drop for SetupPublicationGuard {
    // Publication and its derived state must be cleared under the same lock.
    #[allow(clippy::significant_drop_tightening)]
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let retired = {
            let mut current = McpManager::lock(&self.entry.transport);
            let borrowed = match (current.as_ref(), self.original.as_ref()) {
                (Some(current), Some(original)) => Arc::ptr_eq(current, original),
                _ => false,
            };
            if borrowed || current.is_none() {
                None
            } else {
                let retired = current.take();
                McpManager::lock(&self.entry.tools_cache).take();
                *McpManager::lock(&self.entry.health) = super::ServerHealth::NotStarted;
                retired
            }
        };
        if let Some(transport) = retired {
            transport.abort();
        }
    }
}

struct ToolCallGuard {
    entry: Arc<ServerEntry>,
    transport: Arc<dyn McpTransport>,
    armed: bool,
}

impl ToolCallGuard {
    fn retire(&mut self, error: &Error) {
        if self.armed {
            McpManager::detach_failed_call_transport(&self.entry, &self.transport, error);
            self.armed = false;
        }
    }
}

impl Drop for ToolCallGuard {
    fn drop(&mut self) {
        if self.armed {
            self.retire(&tool_err(
                "MCP_DELIVERY_INDETERMINATE",
                "tools/call was abandoned; remote effects may already have occurred and were not replayed",
            ));
        }
    }
}

fn is_current(entry: &Arc<ServerEntry>, transport: &Arc<dyn McpTransport>) -> bool {
    McpManager::lock(&entry.transport)
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, transport))
}

fn superseded_call() -> Error {
    tool_err(
        "MCP_DELIVERY_INDETERMINATE",
        "connection changed while tools/call was in flight; its result was discarded, remote effects may already have occurred, and the call was not replayed",
    )
}

fn is_cancelled(error: &Error) -> bool {
    matches!(error, Error::Tool { tool, message }
        if tool == "mcp" && message.starts_with("[MCP_CANCELLED] "))
}

/// Snapshot one advertised contract under the transport->cache publication
/// lock order. Missing metadata is NOT an absent output schema: discovery must
/// finish first, or the call must fail before dispatch.
#[allow(clippy::significant_drop_tightening)]
fn output_schema_for_call(
    entry: &Arc<ServerEntry>,
    transport: &Arc<dyn McpTransport>,
    tool: &str,
) -> Result<Option<super::McpOutputSchema>> {
    let current = McpManager::lock(&entry.transport);
    if !current
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, transport))
    {
        drop(current);
        transport.abort();
        return Err(tool_err(
            "MCP_TRANSPORT_SUPERSEDED",
            "connection changed before output-contract capture; the call was not sent",
        ));
    }
    let cache = McpManager::lock(&entry.tools_cache);
    let (_, tools) = cache.as_ref().ok_or_else(|| {
        tool_err(
            "MCP_CATALOG_UNAVAILABLE",
            "tool metadata is unavailable; the call was not sent",
        )
    })?;
    let metadata = tools.iter().find(|metadata| metadata.name == tool).ok_or_else(|| {
        tool_err(
            "MCP_UNKNOWN_TOOL",
            "the requested tool is not in the server's admitted catalog; the call was not sent",
        )
    })?;
    Ok(metadata.output_schema.clone())
}

impl McpManager {
    /// Bound on-demand setup, including waiting for another connection
    /// attempt. Existing private-handshake and blocking-construction guards
    /// own unpublished work; this guard owns only a new publication.
    pub(super) async fn connect_for_request(
        &self,
        owner: &AgentCx,
        entry: &Arc<ServerEntry>,
        timeout: Duration,
    ) -> Result<()> {
        catalog::check_request_owner(owner)?;
        self.check_running()?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| phase_timeout("connection setup"))?;
        let (mut publication, lane) = run_phase(
            owner,
            async {
                let lane = asupersync::sync::OwnedMutexGuard::lock(
                    Arc::clone(&entry.connect_lane),
                    owner.cx(),
                )
                .await
                .map_err(|_| tool_err("MCP_CANCELLED", "cancelled while connecting server"))?;
                let publication = SetupPublicationGuard {
                    entry: Arc::clone(entry),
                    original: Self::lock(&entry.transport).clone(),
                    armed: true,
                };
                self.ensure_ready_in_lane(entry).await?;
                // Keep cleanup armed across run_phase's final cancellation
                // and deadline checks. Tuple fields drop in this order:
                // publication first, lane second, even on rejected success.
                Ok((publication, lane))
            },
            deadline,
            "connection setup",
        )
        .await?;
        publication.armed = false;
        // The publication guard must finish BEFORE another connector can
        // take the lane, including when this whole future is abandoned.
        drop(publication);
        drop(lane);
        Ok(())
    }

    /// Capture a connection and its admitted tool contract before releasing
    /// the setup lane. Cold calls and expired/cleared catalogs must discover
    /// metadata, not silently bypass validation. Execution itself stays outside
    /// the lane, so independent tool calls remain concurrent.
    async fn prepare_tool_call(
        &self,
        owner: &AgentCx,
        entry: &Arc<ServerEntry>,
        tool: &str,
        timeout: Duration,
    ) -> Result<(Arc<dyn McpTransport>, Option<super::McpOutputSchema>)> {
        catalog::check_request_owner(owner)?;
        self.check_running()?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| phase_timeout("tool preparation"))?;
        let (mut publication, lane, transport, output_schema) = run_phase(
            owner,
            async {
                let lane = asupersync::sync::OwnedMutexGuard::lock(
                    Arc::clone(&entry.connect_lane),
                    owner.cx(),
                )
                .await
                .map_err(|_| tool_err("MCP_CANCELLED", "cancelled while preparing tool call"))?;
                let publication = SetupPublicationGuard {
                    entry: Arc::clone(entry),
                    original: Self::lock(&entry.transport).clone(),
                    armed: true,
                };
                self.ensure_ready_in_lane(entry).await?;
                let transport = Self::lock(&entry.transport).clone().ok_or_else(|| {
                    tool_err(
                        "MCP_TRANSPORT_UNAVAILABLE",
                        "the connection disappeared before tools/call was dispatched",
                    )
                })?;
                let catalog_is_fresh = publication
                    .original
                    .as_ref()
                    .is_some_and(|original| Arc::ptr_eq(original, &transport))
                    && Self::lock(&entry.tools_cache)
                        .as_ref()
                        .is_some_and(|(at, _)| at.elapsed() <= super::TOOL_CACHE_TTL);
                if !catalog_is_fresh {
                    self.list_and_cache_tools_in_lane(entry).await?;
                }
                let output_schema = output_schema_for_call(entry, &transport, tool)?;
                // Retain lane ownership until run_phase has accepted the
                // result; rejected completion drops publication before lane.
                Ok((publication, lane, transport, output_schema))
            },
            deadline,
            "tool preparation",
        )
        .await?;
        publication.armed = false;
        drop(publication);
        drop(lane);
        Ok((transport, output_schema))
    }

    /// Call one tool on one trusted server, retaining the calling owner's
    /// authority and cancellation through connection setup and execution.
    ///
    /// An uncertain call is never replayed. Transport failures may reconnect
    /// for subsequent calls; user cancellation does not start recovery work.
    ///
    /// # Errors
    /// Returns capability, trust, cancellation, catalog, output-contract,
    /// transport, or server errors. An abandoned/superseded call may already
    /// have had remote side effects; output-contract errors are not replayed.
    pub async fn call_tool(&self, server: &str, tool: &str, arguments: Value) -> Result<Value> {
        let owner = AgentCx::for_current_or_request();
        // Reject an attenuated/cancelled caller before setup can resolve
        // secrets, run a command, or open a connection.
        catalog::check_request_owner(&owner)?;
        owner
            .with_current(async {
                let entry = self.entry(server)?;
                let (transport, output_schema) = self
                    .prepare_tool_call(&owner, &entry, tool, DEFAULT_MCP_TIMEOUT)
                    .await?;
                match self
                    .execute_tool_call(
                        &entry,
                        &transport,
                        tool,
                        &arguments,
                        DEFAULT_MCP_TIMEOUT,
                        output_schema,
                    )
                    .await
                {
                    Ok(value) => Ok(value),
                    Err(error) if is_indeterminate_call_delivery(&error) => {
                        let deadline = Instant::now()
                            .checked_add(DEFAULT_MCP_TIMEOUT)
                            .unwrap_or_else(Instant::now);
                        let recovery = run_phase(
                            &owner,
                            async {
                                Ok(self
                                    .recover_after_indeterminate_call(&entry, &transport, &error)
                                    .await)
                            },
                            deadline,
                            "connection recovery",
                        )
                        .await
                        .unwrap_or_else(|recovery| {
                            format!("recovery stopped without replaying the call: {recovery}")
                        });
                        Err(tool_err(
                            "MCP_DELIVERY_INDETERMINATE",
                            format!(
                                "server {:?} lost its transport during tools/call; the request may have completed and was not retried; {recovery}",
                                entry.config.name
                            ),
                        ))
                    }
                    Err(error) => Err(error),
                }
            })
            .await
    }

    // Lifecycle fixtures can exercise a transport directly without discovery.
    // Production callers always enter through prepare_tool_call above.
    #[cfg(test)]
    pub(super) async fn call_on_transport(
        &self,
        entry: &Arc<ServerEntry>,
        transport: &Arc<dyn McpTransport>,
        tool: &str,
        arguments: &Value,
    ) -> Result<Value> {
        self.call_on_transport_with_timeout(entry, transport, tool, arguments, DEFAULT_MCP_TIMEOUT)
            .await
    }

    #[cfg(test)]
    async fn call_on_transport_with_timeout(
        &self,
        entry: &Arc<ServerEntry>,
        transport: &Arc<dyn McpTransport>,
        tool: &str,
        arguments: &Value,
        timeout: Duration,
    ) -> Result<Value> {
        self.execute_tool_call(entry, transport, tool, arguments, timeout, None)
            .await
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_tool_call(
        &self,
        entry: &Arc<ServerEntry>,
        transport: &Arc<dyn McpTransport>,
        tool: &str,
        arguments: &Value,
        timeout: Duration,
        output_schema: Option<super::McpOutputSchema>,
    ) -> Result<Value> {
        let owner = AgentCx::for_current_or_request();
        catalog::check_request_owner(&owner)?;
        self.check_running()?;
        if let Err(error) = self.check_trust(entry) {
            Self::close_revoked_transport(entry, transport).await;
            return Err(error);
        }
        if !is_current(entry, transport) {
            transport.abort();
            return Err(tool_err(
                "MCP_TRANSPORT_SUPERSEDED",
                "connection changed before tools/call dispatch; the call was not sent",
            ));
        }
        if !transport.is_alive() {
            let error = tool_err(
                "MCP_TRANSPORT_CLOSED",
                "connection closed before tools/call dispatch",
            );
            Self::detach_failed_call_transport(entry, transport, &error);
            return Err(error);
        }

        // The contract was captured in the setup lane. Never re-read the
        // mutable catalog here or after dispatch: a refresh may have cleared it.
        let mut guard = ToolCallGuard {
            entry: Arc::clone(entry),
            transport: Arc::clone(transport),
            armed: true,
        };
        let result = catalog::request_with_owner(
            &owner,
            transport,
            "tools/call",
            serde_json::json!({"name": tool, "arguments": arguments}),
            timeout,
        )
        .await;
        // Retire cancellation and timeout before any asynchronous revocation
        // cleanup. An uncooperative close must not delay owner cancellation.
        if !is_current(entry, transport) {
            return Err(superseded_call());
        }
        if let Err(error) = catalog::check_request_owner(&owner) {
            guard.retire(&error);
            return Err(error);
        }
        let result = match result {
            Err(error)
                if is_indeterminate_call_delivery(&error)
                    || is_cancelled(&error)
                    || !transport.is_alive() =>
            {
                guard.retire(&error);
                return Err(error);
            }
            result => result,
        };
        // Keep the guard armed through result validation. Early returns,
        // including shutdown and trust revocation, cannot leak a live call.
        self.check_running()?;
        if let Err(error) = self.check_trust(entry) {
            Self::close_revoked_transport(entry, transport).await;
            guard.armed = false;
            return Err(error);
        }
        if let Err(error) = catalog::check_request_owner(&owner) {
            guard.retire(&error);
            return Err(error);
        }
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                // A complete JSON-RPC server error is a definite answer,
                // not an uncertain delivery or a broken connection.
                if !is_current(entry, transport) {
                    return Err(superseded_call());
                }
                guard.armed = false;
                return Err(error);
            }
        };
        // A schema mismatch is a definite response, not transport loss. Keep
        // validation's error until the generation has been checked and the
        // delivery guard disarmed; `?` before that point would retire a healthy
        // connection and incorrectly classify the completed call as abandoned.
        let output_validation = output_schema
            .as_ref()
            .map_or(Ok(()), |schema| schema.validate_result(&value));
        if let Err(error) = catalog::check_request_owner(&owner) {
            guard.retire(&error);
            return Err(error);
        }
        self.check_running()?;
        if let Err(error) = self.check_trust(entry) {
            Self::close_revoked_transport(entry, transport).await;
            guard.armed = false;
            return Err(error);
        }
        // This generation-checked operation is the success linearization
        // point. Ignoring its bool would let an old response escape during a
        // concurrent reconnect and reset the replacement's failure budget.
        if !Self::record_operational_success(entry, transport) {
            return Err(superseded_call());
        }
        guard.armed = false;
        output_validation?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use std::future::{Future, poll_fn};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    use asupersync::{Budget, Cx};
    use async_trait::async_trait;
    use serde_json::json;

    use super::super::{
        ConfiguredServer, Instant, McpDiscovery, McpToolMeta, Provenance, ServerHealth, TrustStore,
    };
    use super::*;

    type DispatchHook = dyn Fn() + Send + Sync;

    struct CallTransport {
        reply: Mutex<Option<Result<Value>>>,
        requests: Mutex<Vec<(String, Value)>>,
        pending: AtomicBool,
        closed: AtomicBool,
        hook: Mutex<Option<Arc<DispatchHook>>>,
    }

    impl CallTransport {
        fn new(reply: Result<Value>) -> Self {
            Self {
                reply: Mutex::new(Some(reply)),
                requests: Mutex::new(Vec::new()),
                pending: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                hook: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl McpTransport for CallTransport {
        async fn request(&self, method: &str, params: Value, _timeout: Duration) -> Result<Value> {
            assert_eq!(
                method, "tools/call",
                "no hidden replay or reconnect request"
            );
            McpManager::lock(&self.requests).push((method.to_string(), params));
            let hook = McpManager::lock(&self.hook).clone();
            if let Some(hook) = hook {
                hook();
            }
            if self.pending.load(Ordering::Acquire) {
                let budget = Cx::current().expect("request owner").budget();
                // Deliberately ignore the timeout and register no transport
                // wake-up. Only the manager's owner/timer can end this call.
                poll_fn(|_| {
                    assert_eq!(Cx::current().expect("owner on every poll").budget(), budget);
                    Poll::<()>::Pending
                })
                .await;
            }
            McpManager::lock(&self.reply)
                .take()
                .expect("tools/call must not be replayed")
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

    fn runtime() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
    }

    fn install(entry: &Arc<ServerEntry>, transport: &Arc<CallTransport>, name: &str) {
        let erased: Arc<dyn McpTransport> = transport.clone();
        *McpManager::lock(&entry.transport) = Some(erased);
        *McpManager::lock(&entry.tools_cache) = Some((
            Instant::now(),
            vec![McpToolMeta {
                name: name.to_string(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
                output_schema: None,
            }],
        ));
        *McpManager::lock(&entry.health) = ServerHealth::Ready { tools: 1 };
    }

    fn fixture(
        temp: &tempfile::TempDir,
        reply: Result<Value>,
    ) -> (McpManager, Arc<ServerEntry>, Arc<CallTransport>) {
        let manager = McpManager::new(
            temp.path(),
            temp.path(),
            McpDiscovery {
                servers: vec![ConfiguredServer {
                    name: "exec".to_string(),
                    command: None,
                    args: Vec::new(),
                    env: Vec::new(),
                    url: Some("https://execution.invalid/mcp".to_string()),
                    headers: Vec::new(),
                    transport_hint: Some("http".to_string()),
                    provenance: Provenance::ProjectPi,
                    source_file: temp.path().join("mcp.json"),
                }],
                warnings: Vec::new(),
            },
        );
        let entry = manager.entry("exec").expect("server");
        TrustStore::load(&manager.inner.trust_path)
            .expect("trust store")
            .acknowledge("exec", &manager.trust_fingerprint_for(&entry), "operator")
            .expect("trust server");
        let transport = Arc::new(CallTransport::new(reply));
        install(&entry, &transport, "execute");
        (manager, entry, transport)
    }

    // Install through the discovery parser, not an independently constructed
    // validator, so these tests cover catalog-to-execution contract routing.
    fn install_output_contract(entry: &Arc<ServerEntry>, schema: Value) {
        let mut tool = json!({"name":"execute", "inputSchema":{"type":"object"}});
        tool["outputSchema"] = schema;
        let tools = super::super::parse_tool_list(&json!({"tools":[tool]}))
            .expect("admitted output schema");
        *McpManager::lock(&entry.tools_cache) = Some((Instant::now(), tools));
    }

    fn count_output_schema(kind: &str) -> Value {
        json!({
            "type":"object",
            "properties":{"count":{"type":kind}},
            "required":["count"],
            "additionalProperties":false
        })
    }

    #[test]
    fn advertised_output_schema_is_enforced_on_the_public_call_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let response = json!({
            "content":[{"type":"text", "text":"one item"}],
            "structuredContent":{"count":1},
            "_meta":{"private":"client-only"}
        });
        let (manager, entry, transport) = fixture(&temp, Ok(response.clone()));
        install_output_contract(&entry, count_output_schema("integer"));
        let result = runtime()
            .block_on(manager.call_tool("exec", "execute", json!({})))
            .expect("valid structured output");
        assert_eq!(result, response);
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
        assert!(!transport.closed.load(Ordering::Acquire));
        assert_eq!(manager.mounted_tool_metas().len(), 1);
    }

    #[test]
    fn invalid_structured_output_is_not_replayed_or_treated_as_transport_loss() {
        for response in [
            json!({"content":[{"type":"text", "text":"{\"count\":1}"}]}),
            json!({"content":[], "structuredContent":{"count":"private-wrong-type"}}),
            json!({"content":[], "structuredContent":null}),
            json!({"content":[], "structuredContent":[]}),
            json!({"isError":"true", "content":[]}),
            json!({"isError":false, "content":[], "structuredContent":{}}),
        ] {
            let temp = tempfile::tempdir().expect("tempdir");
            let (manager, entry, transport) = fixture(&temp, Ok(response));
            install_output_contract(&entry, count_output_schema("integer"));
            let error = runtime()
                .block_on(manager.call_tool("exec", "execute", json!({})))
                .expect_err("invalid output is not a successful tool call");
            let message = error.to_string();
            assert!(message.contains("MCP_OUTPUT_INVALID"), "{message}");
            assert!(message.contains("not replayed"));
            assert!(!message.contains("private-wrong-type"));
            assert!(!is_indeterminate_call_delivery(&error));
            assert_eq!(McpManager::lock(&transport.requests).len(), 1);
            assert!(!transport.closed.load(Ordering::Acquire));
            assert!(McpManager::lock(&entry.transport).is_some());
            assert_eq!(manager.mounted_tool_metas().len(), 1);
            assert_eq!(McpManager::lock(&entry.restarts).count, 0);
        }
    }

    #[test]
    fn output_contract_failure_does_not_poison_the_next_explicit_call() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(
            &temp,
            Ok(json!({"content":[], "structuredContent":{"count":"wrong"}})),
        );
        install_output_contract(&entry, count_output_schema("integer"));
        let runtime = runtime();
        let error = runtime
            .block_on(manager.call_tool("exec", "execute", json!({})))
            .expect_err("first response violates contract");
        assert!(error.to_string().contains("MCP_OUTPUT_INVALID"));
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
        let response = json!({"content":[], "structuredContent":{"count":2}});
        *McpManager::lock(&transport.reply) = Some(Ok(response.clone()));
        assert_eq!(
            runtime
                .block_on(manager.call_tool("exec", "execute", json!({})))
                .expect("subsequent explicit call uses healthy connection"),
            response
        );
        assert_eq!(McpManager::lock(&transport.requests).len(), 2);
        assert!(!transport.closed.load(Ordering::Acquire));
    }

    #[test]
    fn tool_execution_errors_bypass_the_success_output_contract() {
        let temp = tempfile::tempdir().expect("tempdir");
        let response = json!({
            "isError":true,
            "content":[{"type":"text", "text":"rate limited"}]
        });
        let (manager, entry, transport) = fixture(&temp, Ok(response.clone()));
        install_output_contract(&entry, count_output_schema("integer"));
        assert_eq!(
            runtime()
                .block_on(manager.call_tool("exec", "execute", json!({})))
                .expect("keep server's execution error"),
            response
        );
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
        assert!(!transport.closed.load(Ordering::Acquire));
    }

    #[test]
    fn output_contract_does_not_replace_a_definite_jsonrpc_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(
            &temp,
            Err(tool_err("MCP_SERVER_ERROR", "server rejected the request")),
        );
        install_output_contract(&entry, count_output_schema("integer"));
        let error = runtime()
            .block_on(manager.call_tool("exec", "execute", json!({})))
            .expect_err("definite server error");
        assert!(error.to_string().contains("MCP_SERVER_ERROR"));
        assert!(!error.to_string().contains("MCP_OUTPUT_INVALID"));
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
        assert!(!transport.closed.load(Ordering::Acquire));
    }

    #[test]
    fn unstructured_tool_calls_keep_their_existing_response_behavior() {
        let temp = tempfile::tempdir().expect("tempdir");
        let response = json!({"content":[{"type":"text", "text":"plain output"}]});
        let (manager, _entry, transport) = fixture(&temp, Ok(response.clone()));
        assert_eq!(
            runtime()
                .block_on(manager.call_tool("exec", "execute", json!({})))
                .expect("no output schema was advertised"),
            response
        );
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
    }

    #[test]
    fn catalog_refresh_cannot_change_an_in_flight_output_contract() {
        for (count, expected_valid) in [(json!(7), true), (json!("seven"), false)] {
            let temp = tempfile::tempdir().expect("tempdir");
            let response = json!({"content":[], "structuredContent":{"count":count}});
            let (manager, entry, transport) = fixture(&temp, Ok(response.clone()));
            install_output_contract(&entry, count_output_schema("integer"));
            let weak = Arc::downgrade(&entry);
            *McpManager::lock(&transport.hook) = Some(Arc::new(move || {
                let entry = weak.upgrade().expect("live entry");
                install_output_contract(&entry, count_output_schema("string"));
            }));
            let result = runtime().block_on(manager.call_tool("exec", "execute", json!({})));
            if expected_valid {
                assert_eq!(result.expect("original integer contract"), response);
            } else {
                assert!(result.expect_err("replacement string contract must not be borrowed")
                    .to_string().contains("MCP_OUTPUT_INVALID"));
            }
            assert_eq!(McpManager::lock(&transport.requests).len(), 1);
            assert!(!transport.closed.load(Ordering::Acquire));
        }
    }

    #[test]
    fn valid_structured_output_from_an_obsolete_generation_cannot_escape() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, old) = fixture(
            &temp,
            Ok(json!({"content":[], "structuredContent":{"count":1}})),
        );
        install_output_contract(&entry, count_output_schema("integer"));
        let replacement = Arc::new(CallTransport::new(Ok(json!({"content":[]}))));
        let replacement_for_hook = Arc::clone(&replacement);
        let weak = Arc::downgrade(&entry);
        *McpManager::lock(&old.hook) = Some(Arc::new(move || {
            let entry = weak.upgrade().expect("live entry");
            install(&entry, &replacement_for_hook, "execute");
            install_output_contract(&entry, count_output_schema("string"));
            McpManager::lock(&entry.restarts).count = 2;
        }));
        let error = runtime()
            .block_on(manager.call_tool("exec", "execute", json!({})))
            .expect_err("obsolete generation response must not escape");
        assert!(error.to_string().contains("MCP_DELIVERY_INDETERMINATE"));
        assert!(!error.to_string().contains("MCP_OUTPUT_INVALID"));
        assert_eq!(McpManager::lock(&old.requests).len(), 1);
        assert!(old.closed.load(Ordering::Acquire));
        assert!(!replacement.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&replacement.requests).is_empty());
        assert_eq!(McpManager::lock(&entry.restarts).count, 2);
    }

    #[test]
    fn dropping_pending_tool_execution_retires_transport_and_cached_tools() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, Ok(json!({"content": []})));
        transport.pending.store(true, Ordering::Release);
        runtime().block_on(async {
            let mut call = Box::pin(manager.call_tool("exec", "execute", json!({})));
            assert!(futures::poll!(call.as_mut()).is_pending());
            assert_eq!(McpManager::lock(&transport.requests).len(), 1);
            drop(call);
        });
        assert!(transport.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert!(McpManager::lock(&entry.tools_cache).is_none());
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
        assert!(manager.mounted_tool_metas().is_empty());
        assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_ok());
    }

    #[test]
    fn dropping_an_unpolled_tool_call_has_no_transport_effects() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, Ok(json!({"content": []})));
        drop(Box::pin(manager.call_tool("exec", "execute", json!({}))));
        assert!(!transport.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&transport.requests).is_empty());
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
        assert_eq!(manager.mounted_tool_metas().len(), 1);
    }

    #[test]
    fn abandoned_old_call_cannot_retire_a_replacement_or_clear_its_catalog() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, old) = fixture(&temp, Ok(json!({"content": []})));
        old.pending.store(true, Ordering::Release);
        let replacement = Arc::new(CallTransport::new(Ok(json!({"content": []}))));
        runtime().block_on(async {
            let mut call = Box::pin(manager.call_tool("exec", "execute", json!({})));
            assert!(futures::poll!(call.as_mut()).is_pending());
            install(&entry, &replacement, "replacement");
            McpManager::lock(&entry.restarts).count = 2;
            drop(call);
        });
        assert!(old.closed.load(Ordering::Acquire));
        assert!(!replacement.closed.load(Ordering::Acquire));
        assert_eq!(McpManager::lock(&entry.restarts).count, 2);
        assert_eq!(manager.mounted_tool_metas()[0].1[0].name, "replacement");
        assert!(McpManager::lock(&replacement.requests).is_empty());
    }

    #[test]
    fn obsolete_success_is_not_returned_and_does_not_reset_replacement_failures() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, old) = fixture(
            &temp,
            Ok(json!({"content": [{"type": "text", "text": "obsolete-private-result"}]})),
        );
        let replacement = Arc::new(CallTransport::new(Ok(json!({"content": []}))));
        let weak = Arc::downgrade(&entry);
        let replacement_for_hook = Arc::clone(&replacement);
        *McpManager::lock(&old.hook) = Some(Arc::new(move || {
            let entry = weak.upgrade().expect("live entry");
            install(&entry, &replacement_for_hook, "replacement");
            McpManager::lock(&entry.restarts).count = 2;
        }));
        let error = runtime()
            .block_on(manager.call_tool("exec", "execute", json!({})))
            .expect_err("old result must not escape");
        assert!(error.to_string().contains("MCP_DELIVERY_INDETERMINATE"));
        assert!(!error.to_string().contains("obsolete-private-result"));
        assert!(old.closed.load(Ordering::Acquire));
        assert!(!replacement.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&replacement.requests).is_empty());
        assert_eq!(McpManager::lock(&entry.restarts).count, 2);
        assert_eq!(manager.mounted_tool_metas()[0].1[0].name, "replacement");
    }

    struct WakeCounter(AtomicUsize);

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn cancellation_wakes_idle_execution_under_its_original_owner_without_recovery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, Ok(json!({"content": []})));
        transport.pending.store(true, Ordering::Release);
        let runtime = runtime();
        let owner =
            AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new().with_poll_quota(1000)));
        runtime.block_on(async {
            let parent = Cx::current().expect("parent");
            let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
            let waker = Waker::from(Arc::clone(&counter));
            let mut task = Context::from_waker(&waker);
            let mut call = Box::pin(manager.call_tool("exec", "execute", json!({})));
            {
                let _guard = owner.cx().clone().set_current_restricted();
                assert!(call.as_mut().poll(&mut task).is_pending());
            }
            assert!(call.as_mut().poll(&mut task).is_pending());
            let before = counter.0.load(Ordering::SeqCst);
            owner.cancel_with(
                asupersync::types::CancelKind::User,
                Some("cancel tools/call"),
            );
            assert!(counter.0.load(Ordering::SeqCst) > before);
            let Poll::Ready(Err(error)) = call.as_mut().poll(&mut task) else {
                panic!("cancelled call must finish without a transport reply");
            };
            assert!(error.to_string().contains("MCP_CANCELLED"));
            assert!(!parent.is_cancel_requested());
            assert_eq!(Cx::current().unwrap().budget(), parent.budget());
        });
        assert!(transport.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
    }

    #[test]
    fn execution_deadline_is_enforced_even_when_transport_ignores_it() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, Ok(json!({"content": []})));
        transport.pending.store(true, Ordering::Release);
        let erased: Arc<dyn McpTransport> = transport.clone();
        let error = runtime()
            .block_on(manager.call_on_transport_with_timeout(
                &entry,
                &erased,
                "execute",
                &json!({}),
                Duration::from_millis(20),
            ))
            .expect_err("manager deadline");
        assert!(error.to_string().contains("MCP_TIMEOUT"));
        assert!(transport.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert!(McpManager::lock(&entry.tools_cache).is_none());
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
    }

    #[test]
    fn cancellation_racing_a_definite_response_never_returns_success() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, Ok(json!({"content": []})));
        let runtime = runtime();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        let owner_for_hook = owner.clone();
        *McpManager::lock(&transport.hook) = Some(Arc::new(move || {
            owner_for_hook.cancel_with(asupersync::types::CancelKind::User, Some("response race"));
        }));
        let error = runtime
            .block_on(owner.with_current(manager.call_tool("exec", "execute", json!({}))))
            .expect_err("cancellation wins");
        assert!(error.to_string().contains("MCP_CANCELLED"));
        assert!(transport.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
    }

    #[test]
    fn pre_cancelled_or_restricted_callers_cannot_dispatch_or_retire_a_connection() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, Ok(json!({"content": []})));
        let cancelled = AgentCx::for_request();
        cancelled.cancel_with(asupersync::types::CancelKind::User, Some("before dispatch"));
        let restricted = {
            let _guard = Cx::for_request()
                .restrict::<asupersync::cx::cap::None>()
                .set_current_restricted();
            AgentCx::for_current_or_request()
        };
        for (owner, code) in [
            (cancelled, "MCP_CANCELLED"),
            (restricted, "MCP_CAPABILITY_DENIED"),
        ] {
            let error = runtime()
                .block_on(owner.with_current(manager.call_tool("exec", "execute", json!({}))))
                .expect_err("owner rejects dispatch");
            assert!(error.to_string().contains(code), "{error}");
        }
        assert!(McpManager::lock(&transport.requests).is_empty());
        assert!(!transport.closed.load(Ordering::Acquire));
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
    }

    #[test]
    fn a_definite_server_error_preserves_the_connection_and_is_not_replayed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(
            &temp,
            Err(tool_err(
                "MCP_SERVER_ERROR",
                "server error -32602: invalid arguments",
            )),
        );
        McpManager::lock(&entry.restarts).count = 2;
        let error = runtime()
            .block_on(manager.call_tool("exec", "execute", json!({})))
            .expect_err("definite server error");
        assert!(error.to_string().contains("MCP_SERVER_ERROR"));
        assert!(!error.to_string().contains("MCP_DELIVERY_INDETERMINATE"));
        assert!(!transport.closed.load(Ordering::Acquire));
        assert_eq!(McpManager::lock(&entry.restarts).count, 2);
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
        assert_eq!(manager.mounted_tool_metas().len(), 1);
    }

    #[test]
    fn a_complete_tool_error_result_is_returned_unchanged_without_replay() {
        let temp = tempfile::tempdir().expect("tempdir");
        let result = json!({
            "content": [{"type": "text", "text": "execution failed"}],
            "structuredContent": {"exitCode": 7}, "isError": true
        });
        let arguments = json!({"path": "日本語/file", "command": "printf '%s' value"});
        let (manager, entry, transport) = fixture(&temp, Ok(result.clone()));
        McpManager::lock(&entry.restarts).count = 2;
        let returned = runtime()
            .block_on(manager.call_tool("exec", "execute", arguments.clone()))
            .expect("complete tool result");
        assert_eq!(returned, result);
        assert!(!transport.closed.load(Ordering::Acquire));
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
        assert_eq!(
            *McpManager::lock(&transport.requests),
            vec![(
                "tools/call".to_string(),
                json!({"name": "execute", "arguments": arguments})
            )]
        );
    }

    struct SetupState {
        stall: &'static str,
        reached: AtomicBool,
        aborted: AtomicBool,
        tool_calls: AtomicUsize,
        list_calls: AtomicUsize,
        catalog: Mutex<Value>,
        reply: Mutex<Value>,
    }

    struct SetupTransport(Arc<SetupState>);

    impl SetupTransport {
        async fn stage(&self, stage: &str) {
            if self.0.stall == stage {
                self.0.reached.store(true, Ordering::Release);
                // No cancellation registration, deadline, or cooperative
                // transport wake-up: setup itself must supply all three.
                futures::future::pending::<()>().await;
            }
        }
    }

    #[async_trait]
    impl McpTransport for SetupTransport {
        async fn request(&self, method: &str, _params: Value, _timeout: Duration) -> Result<Value> {
            match method {
                "initialize" => {
                    self.stage("initialize").await;
                    if self.0.stall == "close" {
                        Err(tool_err("MCP_SERVER_ERROR", "initialization refused"))
                    } else {
                        Ok(json!({}))
                    }
                }
                "tools/call" => {
                    self.0.tool_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(McpManager::lock(&self.0.reply).clone())
                }
                "tools/list" => {
                    self.0.list_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(McpManager::lock(&self.0.catalog).clone())
                }
                _ => panic!("unexpected setup method"),
            }
        }

        async fn notify(&self, method: &str, _params: Value) -> Result<()> {
            assert_eq!(method, "notifications/initialized");
            self.stage("initialized").await;
            Ok(())
        }

        async fn activate(self: Arc<Self>) -> Result<()> {
            self.stage("activate").await;
            Ok(())
        }

        fn is_alive(&self) -> bool {
            !self.0.aborted.load(Ordering::Acquire)
        }

        fn abort(&self) {
            self.0.aborted.store(true, Ordering::Release);
        }

        async fn close(&self) {
            self.stage("close").await;
            self.abort();
        }

        fn diagnostics_tail(&self) -> String {
            String::new()
        }
    }

    fn setup_factory(manager: &McpManager, stall: &'static str) -> Arc<SetupState> {
        let state = Arc::new(SetupState {
            stall,
            reached: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
            tool_calls: AtomicUsize::new(0),
            list_calls: AtomicUsize::new(0),
            catalog: Mutex::new(json!({"tools":[{
                "name":"execute", "inputSchema":{"type":"object"}
            }]})),
            reply: Mutex::new(json!({"content":[]})),
        });
        let state_for_factory = Arc::clone(&state);
        *McpManager::lock(&manager.inner.transport_factory) = Some(Arc::new(move || {
            Box::new(SetupTransport(Arc::clone(&state_for_factory))) as Box<dyn McpTransport>
        }));
        state
    }

    /// A broken deadline must fail a test rather than strand its process.
    async fn watchdog<T>(future: impl Future<Output = T>) -> T {
        let watch = async {
            let owner = AgentCx::for_current_or_request();
            owner.time().sleep(Duration::from_secs(2)).await;
        };
        match futures::future::select(Box::pin(future), Box::pin(watch)).await {
            futures::future::Either::Left((result, _)) => result,
            futures::future::Either::Right(((), pending)) => {
                drop(pending);
                panic!("MCP lifecycle test exceeded its independent watchdog");
            }
        }
    }

    #[test]
    fn cancelled_connection_waiter_does_not_retire_the_lane_holders_transport() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, Ok(json!({"content": []})));
        let lane = Arc::clone(&entry.connect_lane)
            .try_lock_owned()
            .expect("hold lane");
        let runtime = runtime();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        runtime.block_on(async {
            let mut call =
                Box::pin(owner.with_current(manager.call_tool("exec", "execute", json!({}))));
            assert!(futures::poll!(call.as_mut()).is_pending());
            owner.cancel_with(
                asupersync::types::CancelKind::User,
                Some("cancel lane waiter"),
            );
            let Poll::Ready(Err(error)) = futures::poll!(call.as_mut()) else {
                panic!("cancelled admission must finish before the holder releases its lane");
            };
            assert!(error.to_string().contains("MCP_CANCELLED"));
            assert!(!transport.closed.load(Ordering::Acquire));
            assert!(McpManager::lock(&transport.requests).is_empty());
            assert_eq!(McpManager::lock(&entry.restarts).count, 0);
        });
        drop(lane);
        assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_ok());
        assert_eq!(manager.mounted_tool_metas().len(), 1);
    }

    #[test]
    fn connection_admission_spends_the_setup_deadline_without_mutating_the_holder() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, Ok(json!({"content": []})));
        let lane = Arc::clone(&entry.connect_lane)
            .try_lock_owned()
            .expect("hold lane");
        let runtime = runtime();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        let error = runtime
            .block_on(watchdog(manager.connect_for_request(
                &owner,
                &entry,
                Duration::from_millis(20),
            )))
            .expect_err("admission deadline");
        assert!(error.to_string().contains("MCP_TIMEOUT"));
        assert!(error.to_string().contains("connection setup"));
        assert!(!transport.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&transport.requests).is_empty());
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
        drop(lane);
        assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_ok());
    }

    #[test]
    fn cancellation_retires_private_setup_at_every_async_handshake_stage() {
        for stage in ["initialize", "initialized", "activate", "close"] {
            let temp = tempfile::tempdir().expect("tempdir");
            let (manager, entry, _) = fixture(&temp, Ok(json!({"content": []})));
            McpManager::lock(&entry.transport).take();
            let state = setup_factory(&manager, stage);
            let runtime = runtime();
            let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
            runtime.block_on(async {
                let parent = Cx::current().expect("parent");
                let mut call =
                    Box::pin(owner.with_current(manager.call_tool("exec", "execute", json!({}))));
                watchdog(poll_fn(|task| {
                    assert!(call.as_mut().poll(task).is_pending(), "stage {stage}");
                    if state.reached.load(Ordering::Acquire) {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                }))
                .await;
                owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel setup"));
                let Poll::Ready(Err(error)) = futures::poll!(call.as_mut()) else {
                    panic!("{stage} did not observe owner cancellation");
                };
                assert!(error.to_string().contains("MCP_CANCELLED"), "{error}");
                assert!(!parent.is_cancel_requested());
                assert_eq!(Cx::current().unwrap().budget(), parent.budget());
            });
            assert!(state.aborted.load(Ordering::Acquire), "stage {stage}");
            assert_eq!(state.tool_calls.load(Ordering::SeqCst), 0);
            assert!(McpManager::lock(&entry.transport).is_none());
            assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_ok());
        }
    }

    #[test]
    fn deadlines_bound_private_initialization_notification_activation_and_close() {
        for stage in ["initialize", "initialized", "activate", "close"] {
            let temp = tempfile::tempdir().expect("tempdir");
            let (manager, entry, _) = fixture(&temp, Ok(json!({"content": []})));
            McpManager::lock(&entry.transport).take();
            let state = setup_factory(&manager, stage);
            let runtime = runtime();
            let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
            let error = runtime
                .block_on(watchdog(manager.connect_for_request(
                    &owner,
                    &entry,
                    Duration::from_millis(100),
                )))
                .expect_err("whole setup deadline");
            assert!(
                error.to_string().contains("MCP_TIMEOUT"),
                "{stage}: {error}"
            );
            assert!(state.reached.load(Ordering::Acquire), "stage {stage}");
            assert!(state.aborted.load(Ordering::Acquire), "stage {stage}");
            assert_eq!(state.tool_calls.load(Ordering::SeqCst), 0);
            assert!(McpManager::lock(&entry.transport).is_none());
            assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_ok());
        }
    }

    #[test]
    fn recovery_cancellation_preserves_uncertain_delivery_and_never_replays_the_tool() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, old) =
            fixture(&temp, Err(tool_err("MCP_TRANSPORT_IO", "lost tool reply")));
        let state = setup_factory(&manager, "activate");
        let runtime = runtime();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        runtime.block_on(async {
            let mut call =
                Box::pin(owner.with_current(manager.call_tool("exec", "execute", json!({}))));
            watchdog(poll_fn(|task| {
                assert!(call.as_mut().poll(task).is_pending());
                if state.reached.load(Ordering::Acquire) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }))
            .await;
            owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel recovery"));
            let Poll::Ready(Err(error)) = futures::poll!(call.as_mut()) else {
                panic!("recovery must not suppress owner cancellation");
            };
            assert!(error.to_string().contains("MCP_DELIVERY_INDETERMINATE"));
            assert!(error.to_string().contains("MCP_CANCELLED"));
        });
        assert_eq!(McpManager::lock(&old.requests).len(), 1);
        assert!(old.closed.load(Ordering::Acquire));
        assert!(state.aborted.load(Ordering::Acquire));
        assert_eq!(state.tool_calls.load(Ordering::SeqCst), 0);
        assert!(McpManager::lock(&entry.transport).is_none());
        assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_ok());
    }

    #[test]
    fn timely_setup_publishes_once_and_dispatches_the_tool_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, _) = fixture(&temp, Ok(json!({"content": []})));
        McpManager::lock(&entry.transport).take();
        let state = setup_factory(&manager, "none");
        let returned = runtime()
            .block_on(watchdog(manager.call_tool("exec", "execute", json!({}))))
            .expect("successful setup and execution");
        assert_eq!(returned, json!({"content": []}));
        assert_eq!(state.tool_calls.load(Ordering::SeqCst), 1);
        assert!(!state.aborted.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_some());
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
    }

    fn setup_output_catalog(state: &SetupState, schema: Value) {
        *McpManager::lock(&state.catalog) = json!({"tools":[{
            "name":"execute", "inputSchema":{"type":"object"}, "outputSchema":schema
        }]});
    }

    #[test]
    fn cold_tool_call_discovers_and_enforces_output_contract_before_dispatch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, _) = fixture(&temp, Ok(json!({"content":[]})));
        McpManager::lock(&entry.transport).take();
        let state = setup_factory(&manager, "none");
        setup_output_catalog(&state, count_output_schema("integer"));
        *McpManager::lock(&state.reply) = json!({"structuredContent":{"count":"wrong"}});
        let error = runtime()
            .block_on(watchdog(manager.call_tool("exec", "execute", json!({}))))
            .expect_err("cold calls must not bypass the advertised output contract");
        assert!(error.to_string().contains("MCP_OUTPUT_INVALID"));
        assert_eq!(state.list_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.tool_calls.load(Ordering::SeqCst), 1);
        assert!(!state.aborted.load(Ordering::Acquire));
    }

    #[test]
    fn missing_or_expired_catalog_is_refreshed_before_later_tool_calls() {
        for expire in [false, true] {
            let temp = tempfile::tempdir().expect("tempdir");
            let (manager, entry, _) = fixture(&temp, Ok(json!({"content":[]})));
            McpManager::lock(&entry.transport).take();
            let state = setup_factory(&manager, "none");
            setup_output_catalog(&state, count_output_schema("integer"));
            *McpManager::lock(&state.reply) = json!({"structuredContent":{"count":1}});
            let runtime = runtime();
            runtime
                .block_on(watchdog(manager.call_tool("exec", "execute", json!({}))))
                .expect("first call establishes the catalog");
            if expire {
                McpManager::lock(&entry.tools_cache)
                    .as_mut()
                    .expect("cached catalog")
                    .0 = Instant::now()
                    .checked_sub(super::super::TOOL_CACHE_TTL + Duration::from_secs(1))
                    .expect("representable expired instant");
            } else {
                McpManager::lock(&entry.tools_cache).take();
            }
            *McpManager::lock(&state.reply) = json!({"structuredContent":{"count":"wrong"}});
            let error = runtime
                .block_on(watchdog(manager.call_tool("exec", "execute", json!({}))))
                .expect_err("missing metadata must never become a missing contract");
            assert!(error.to_string().contains("MCP_OUTPUT_INVALID"));
            assert_eq!(state.list_calls.load(Ordering::SeqCst), 2);
            assert_eq!(state.tool_calls.load(Ordering::SeqCst), 2);
            assert!(!state.aborted.load(Ordering::Acquire));
        }
    }

    #[test]
    fn undiscovered_tool_names_cannot_skip_output_contract_admission() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, Ok(json!({"content":[]})));
        let error = runtime()
            .block_on(manager.call_tool("exec", "not-advertised", json!({})))
            .expect_err("unadvertised tools must not bypass admission");
        assert!(error.to_string().contains("MCP_UNKNOWN_TOOL"));
        assert!(McpManager::lock(&transport.requests).is_empty());
        assert!(!transport.closed.load(Ordering::Acquire));
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
    }

    #[test]
    fn cleared_catalog_capture_fails_before_dispatch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (_manager, entry, transport) = fixture(&temp, Ok(json!({"content":[]})));
        let erased: Arc<dyn McpTransport> = transport.clone();
        McpManager::lock(&entry.tools_cache).take();
        let error = output_schema_for_call(&entry, &erased, "execute")
            .expect_err("metadata absence is not an optional output schema");
        assert!(error.to_string().contains("MCP_CATALOG_UNAVAILABLE"));
        assert!(McpManager::lock(&transport.requests).is_empty());
        assert!(!transport.closed.load(Ordering::Acquire));
    }

    #[test]
    fn malformed_output_catalog_blocks_a_cold_call_before_remote_effects() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, _) = fixture(&temp, Ok(json!({"content":[]})));
        McpManager::lock(&entry.transport).take();
        let state = setup_factory(&manager, "none");
        setup_output_catalog(&state, json!({"type":7}));
        let error = runtime()
            .block_on(watchdog(manager.call_tool("exec", "execute", json!({}))))
            .expect_err("invalid schema prevents dispatch");
        assert!(error.to_string().contains("MCP_PROTOCOL"));
        assert_eq!(state.list_calls.load(Ordering::SeqCst), 1);
        assert_eq!(state.tool_calls.load(Ordering::SeqCst), 0);
        assert!(state.aborted.load(Ordering::Acquire));
        assert!(manager.mounted_tool_metas().is_empty());
    }
}
