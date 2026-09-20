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

impl McpManager {
    /// Call one tool on one trusted server, retaining the calling owner's
    /// authority and cancellation through connection setup and execution.
    ///
    /// An uncertain call is never replayed. Transport failures may reconnect
    /// for subsequent calls; user cancellation does not start recovery work.
    ///
    /// # Errors
    /// Returns capability, trust, cancellation, transport, or server errors.
    /// An abandoned/superseded call may already have had remote side effects.
    pub async fn call_tool(&self, server: &str, tool: &str, arguments: Value) -> Result<Value> {
        let owner = AgentCx::for_current_or_request();
        // Reject an attenuated/cancelled caller before setup can resolve
        // secrets, run a command, or open a connection.
        catalog::check_request_owner(&owner)?;
        owner
            .with_current(async {
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
                    Err(error) if is_indeterminate_call_delivery(&error) => {
                        let recovery = self
                            .recover_after_indeterminate_call(&entry, &transport, &error)
                            .await;
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

    async fn call_on_transport_with_timeout(
        &self,
        entry: &Arc<ServerEntry>,
        transport: &Arc<dyn McpTransport>,
        tool: &str,
        arguments: &Value,
        timeout: Duration,
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
        // This generation-checked operation is the success linearization
        // point. Ignoring its bool would let an old response escape during a
        // concurrent reconnect and reset the replacement's failure budget.
        if !Self::record_operational_success(entry, transport) {
            return Err(superseded_call());
        }
        guard.armed = false;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use std::future::{Future, poll_fn};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    use async_trait::async_trait;
    use asupersync::{Budget, Cx};
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
            assert_eq!(method, "tools/call", "no hidden replay or reconnect request");
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
        let owner = AgentCx::from_cx(
            runtime.request_cx_with_budget(Budget::new().with_poll_quota(1000)),
        );
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
            owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel tools/call"));
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
            Err(tool_err("MCP_SERVER_ERROR", "server error -32602: invalid arguments")),
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
}
