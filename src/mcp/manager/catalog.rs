//! Complete, bounded MCP tool catalogs on a single transport generation.
//!
//! `nextCursor` is opaque, including an empty string. No schemas become
//! mountable until every page succeeds. The request timeout is shared by the
//! whole traversal; a server cannot multiply it by returning more pages.

use std::collections::HashSet;
use std::future::{Future, poll_fn};
use std::io::Write;
use std::task::Poll;

use serde_json::json;

use super::{
    Arc, DEFAULT_MCP_TIMEOUT, Duration, Instant, MAX_SERVER_TOOLS, McpManager, McpToolMeta,
    McpTransport, Result, ServerEntry, Value, parse_tool_list, tool_err,
};

mod context;

const MAX_CATALOG_PAGES: usize = 128;
const MAX_CURSOR_BYTES: usize = 4096;
const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;

/// Count the serialized catalog without allocating another copy of its
/// potentially large schemas. Each page spends the remaining shared budget.
struct CatalogByteBudget {
    remaining: usize,
}

impl Write for CatalogByteBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.remaining = self.remaining.checked_sub(bytes.len()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "catalog byte limit exceeded",
            )
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Catalog {
    tools: Vec<McpToolMeta>,
    names: HashSet<String>,
    cursors: HashSet<String>,
    pages: usize,
    bytes: CatalogByteBudget,
}

impl Catalog {
    fn new() -> Self {
        Self {
            tools: Vec::new(),
            names: HashSet::new(),
            cursors: HashSet::new(),
            pages: 0,
            bytes: CatalogByteBudget {
                remaining: MAX_CATALOG_BYTES,
            },
        }
    }

    fn append_page(&mut self, result: &Value) -> Result<Option<String>> {
        if self.pages >= MAX_CATALOG_PAGES {
            return Err(tool_err(
                "MCP_PROTOCOL",
                "tools/list exceeded the page limit",
            ));
        }
        serde_json::to_writer(&mut self.bytes, result).map_err(|_| {
            tool_err(
                "MCP_PROTOCOL",
                "tools/list exceeded the aggregate catalog byte limit",
            )
        })?;
        let tools = parse_tool_list(result)?;
        if tools.len() > MAX_SERVER_TOOLS.saturating_sub(self.tools.len()) {
            return Err(tool_err(
                "MCP_PROTOCOL",
                "tools/list exceeded the aggregate tool limit",
            ));
        }
        for tool in &tools {
            if !self.names.insert(tool.name.clone()) {
                return Err(tool_err(
                    "MCP_PROTOCOL",
                    "tools/list repeated a tool name across pages",
                ));
            }
        }
        let next = match result.get("nextCursor") {
            None => None,
            Some(Value::String(cursor)) => {
                if cursor.len() > MAX_CURSOR_BYTES {
                    return Err(tool_err(
                        "MCP_PROTOCOL",
                        "tools/list cursor exceeded the byte limit",
                    ));
                }
                if !self.cursors.insert(cursor.clone()) {
                    return Err(tool_err(
                        "MCP_PROTOCOL",
                        "tools/list repeated a pagination cursor",
                    ));
                }
                Some(cursor.clone())
            }
            Some(_) => {
                return Err(tool_err(
                    "MCP_PROTOCOL",
                    "tools/list nextCursor must be a string",
                ));
            }
        };
        self.pages += 1;
        if next.is_some() && self.pages == MAX_CATALOG_PAGES {
            return Err(tool_err(
                "MCP_PROTOCOL",
                "tools/list exceeded the page limit",
            ));
        }
        self.tools.extend(tools);
        Ok(next)
    }
}

fn remaining_budget(deadline: Instant, now: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(now);
    if remaining.is_zero() {
        Err(tool_err(
            "MCP_TIMEOUT",
            "tools/list exhausted its total catalog deadline",
        ))
    } else {
        Ok(remaining)
    }
}

/// Own the exact generation while a page is in flight. Dropping discovery
/// (including the outer startup timeout) must retire a request whose result
/// will never be consumed, without touching a concurrently installed server.
struct CatalogRequestGuard {
    entry: Arc<ServerEntry>,
    transport: Arc<dyn McpTransport>,
    armed: bool,
}

impl Drop for CatalogRequestGuard {
    fn drop(&mut self) {
        if self.armed {
            McpManager::fail_transport_generation(
                &self.entry,
                &self.transport,
                &tool_err(
                    "MCP_CANCELLED",
                    "tools/list was cancelled before its response was consumed",
                ),
            );
        }
    }
}

fn check_catalog_generation(
    entry: &Arc<ServerEntry>,
    transport: &Arc<dyn McpTransport>,
) -> Result<()> {
    let is_current = McpManager::lock(&entry.transport)
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, transport));
    if !is_current {
        transport.abort();
        return Err(tool_err(
            "MCP_TRANSPORT_SUPERSEDED",
            "connection changed during tools/list traversal",
        ));
    }
    if !transport.is_alive() {
        let error = tool_err(
            "MCP_TRANSPORT_CLOSED",
            "connection closed before tools/list completed",
        );
        McpManager::fail_transport_generation(entry, transport, &error);
        return Err(error);
    }
    Ok(())
}

pub(super) fn check_request_owner(owner: &crate::agent_cx::AgentCx) -> Result<()> {
    if !owner.capabilities().io || !owner.capabilities().time {
        return Err(tool_err(
            "MCP_CAPABILITY_DENIED",
            "MCP requests require the owner's I/O and timer capabilities",
        ));
    }
    owner.checkpoint().map_err(|_| request_cancelled())
}

fn request_cancelled() -> crate::error::Error {
    tool_err(
        "MCP_CANCELLED",
        "MCP request cancelled; delivery may already have occurred and was not retried",
    )
}

fn request_timed_out() -> crate::error::Error {
    tool_err("MCP_TIMEOUT", "MCP operation exceeded its manager deadline")
}

async fn wait_for_owner_cancellation(owner: crate::agent_cx::AgentCx) {
    let (sender, mut receiver) = asupersync::channel::oneshot::channel::<()>();
    // Keeping the sender alive means only owner cancellation can finish this
    // receive. Its registration is retired with the request, not a new task.
    let _ = receiver.recv(owner.cx()).await;
    drop(sender);
}

/// Enforce the manager's lifetime even when a transport never wakes, ignores
/// its timeout, or is polled later by a task with different ambient authority.
/// Cancellation and expiry win over a response arriving in the same poll.
pub(super) async fn request_with_owner(
    owner: &crate::agent_cx::AgentCx,
    transport: &Arc<dyn McpTransport>,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value> {
    // Construct the transport request only after admission and under its
    // captured owner, just like polling and cancellation cleanup below.
    operation_with_owner(
        owner,
        async { transport.request(method, params, timeout).await },
        timeout,
    )
    .await?
}

/// Drop pending work under its owner even when the caller abandons the entire
/// enclosing future from a different task or thread.
struct OwnedOperation<'a, F> {
    owner: &'a crate::agent_cx::AgentCx,
    future: Option<std::pin::Pin<Box<F>>>,
}

impl<F> Drop for OwnedOperation<'_, F> {
    fn drop(&mut self) {
        let _guard = self.owner.cx().clone().set_current_restricted();
        drop(self.future.take());
    }
}

/// One cancellation/deadline boundary for requests and connection setup.
/// The nested result distinguishes a returned operation error (which may
/// already have been accounted for) from owner cancellation or expiry.
pub(super) async fn operation_with_owner<F: Future>(
    owner: &crate::agent_cx::AgentCx,
    future: F,
    timeout: Duration,
) -> Result<F::Output> {
    let mut operation = OwnedOperation {
        owner,
        future: Some(Box::pin(future)),
    };
    check_request_owner(owner)?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(request_timed_out)?;
    if timeout.is_zero() {
        return Err(request_timed_out());
    }
    let now = owner
        .cx()
        .timer_driver()
        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
    let mut timer = Box::pin(asupersync::time::sleep(now, timeout));
    let mut cancellation = Box::pin(wait_for_owner_cancellation(owner.clone()));
    let result = poll_fn(|task| {
        let _guard = owner.cx().clone().set_current_restricted();
        if let Err(error) = check_request_owner(owner) {
            return Poll::Ready(Err(error));
        }
        if cancellation.as_mut().poll(task).is_ready() {
            return Poll::Ready(Err(request_cancelled()));
        }
        if Instant::now() >= deadline || timer.as_mut().poll(task).is_ready() {
            return Poll::Ready(Err(request_timed_out()));
        }
        let result = operation
            .future
            .as_mut()
            .expect("active operation")
            .as_mut()
            .poll(task);
        if result.is_ready() {
            if let Err(error) = check_request_owner(owner) {
                return Poll::Ready(Err(error));
            }
            if Instant::now() >= deadline {
                return Poll::Ready(Err(request_timed_out()));
            }
        }
        result.map(Ok)
    })
    .await;
    let _guard = owner.cx().clone().set_current_restricted();
    drop(operation);
    drop(cancellation);
    drop(timer);
    result
}

impl McpManager {
    /// Discover the server's current tool catalog, including tools added or
    /// changed since startup. Publish only a complete, validated catalog.
    ///
    /// Unlike `/mcp test`, discovery does not reset the restart budget or
    /// acknowledge trust. Admission, connection setup and all cursor pages
    /// share one owner and one outer deadline; no tool is executed here.
    ///
    /// # Errors
    /// Returns trust, capability, cancellation, timeout, restart-budget or
    /// protocol errors. A failed traversal never returns a partial catalog.
    pub async fn refresh_tools(&self, server: &str) -> Result<Vec<McpToolMeta>> {
        self.refresh_tools_with_timeout(server, DEFAULT_MCP_TIMEOUT)
            .await
    }

    async fn refresh_tools_with_timeout(
        &self,
        server: &str,
        timeout: Duration,
    ) -> Result<Vec<McpToolMeta>> {
        let owner = crate::agent_cx::AgentCx::for_current_or_request();
        check_request_owner(&owner)?;
        self.check_running()?;
        let entry = self.entry(server)?;
        // connect_and_list retains the connection lane through publication.
        // The outer owner boundary also covers waiting for that lane and
        // private initialization, not just individual tools/list requests.
        operation_with_owner(&owner, self.connect_and_list(&entry), timeout).await?
    }

    pub(super) async fn collect_tool_catalog(
        &self,
        entry: &Arc<ServerEntry>,
        transport: &Arc<dyn McpTransport>,
    ) -> Result<Vec<McpToolMeta>> {
        self.collect_tool_catalog_with_timeout(entry, transport, DEFAULT_MCP_TIMEOUT)
            .await
    }

    async fn collect_tool_catalog_with_timeout(
        &self,
        entry: &Arc<ServerEntry>,
        transport: &Arc<dyn McpTransport>,
        timeout: Duration,
    ) -> Result<Vec<McpToolMeta>> {
        let owner = crate::agent_cx::AgentCx::for_current_or_request();
        check_request_owner(&owner)?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(request_timed_out)?;
        let mut catalog = Catalog::new();
        let mut params = json!({});
        loop {
            self.check_running()?;
            if let Err(error) = self.check_trust(entry) {
                Self::close_revoked_transport(entry, transport).await;
                return Err(error);
            }
            check_catalog_generation(entry, transport)?;
            let remaining = match remaining_budget(deadline, Instant::now()) {
                Ok(remaining) => remaining,
                Err(error) => {
                    Self::fail_transport_generation(entry, transport, &error);
                    return Err(error);
                }
            };
            let mut request_guard = CatalogRequestGuard {
                entry: Arc::clone(entry),
                transport: Arc::clone(transport),
                armed: true,
            };
            let result =
                request_with_owner(&owner, transport, "tools/list", params, remaining).await;
            // A returned error is handled by the existing failure taxonomy;
            // only abandonment of the pending future belongs to the guard.
            request_guard.armed = false;
            let result = match result {
                Ok(result) => result,
                // Tools are optional in MCP. A resource-only server may
                // explicitly reject the initial tools/list method. Do not
                // mistake that for a crashed connection, or accept a failure
                // on a later page as a successfully completed partial catalog.
                Err(error)
                    if catalog.pages == 0
                        && transport.is_alive()
                        && context::is_method_not_found(&error) =>
                {
                    json!({"tools": []})
                }
                Err(error) => {
                    Self::fail_transport_generation(entry, transport, &error);
                    return Err(error);
                }
            };
            self.check_running()?;
            if let Err(error) = self.check_trust(entry) {
                Self::close_revoked_transport(entry, transport).await;
                return Err(error);
            }
            check_catalog_generation(entry, transport)?;
            // A transport that returns after its allotted deadline cannot
            // publish a late success or buy a fresh timeout for another page.
            if let Err(error) = remaining_budget(deadline, Instant::now()) {
                Self::fail_transport_generation(entry, transport, &error);
                return Err(error);
            }
            let next = match catalog.append_page(&result) {
                Ok(next) => next,
                Err(error) => {
                    Self::fail_transport_generation(entry, transport, &error);
                    return Err(error);
                }
            };
            if let Err(error) = remaining_budget(deadline, Instant::now()) {
                Self::fail_transport_generation(entry, transport, &error);
                return Err(error);
            }
            if let Err(error) = check_request_owner(&owner) {
                Self::fail_transport_generation(entry, transport, &error);
                return Err(error);
            }
            match next {
                Some(cursor) => params = json!({"cursor": cursor}),
                None => return Ok(catalog.tools),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;

    use super::super::{ConfiguredServer, McpDiscovery, Provenance, ServerHealth, TrustStore};
    use super::*;

    type PageHook = dyn Fn(usize) + Send + Sync;

    struct PagedTransport {
        pages: Mutex<VecDeque<Value>>,
        requests: Mutex<Vec<(Value, Duration)>>,
        closed: AtomicBool,
        after_page: Mutex<Option<Arc<PageHook>>>,
        pause_at: Mutex<Option<usize>>,
    }

    #[async_trait]
    impl McpTransport for PagedTransport {
        async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
            assert_eq!(
                method, "tools/list",
                "a catalog traversal must not call tools"
            );
            let index = {
                let mut requests = McpManager::lock(&self.requests);
                requests.push((params, timeout));
                requests.len()
            };
            let page = McpManager::lock(&self.pages)
                .pop_front()
                .ok_or_else(|| tool_err("MCP_PROTOCOL", "unexpected extra catalog request"))?;
            let hook = McpManager::lock(&self.after_page).clone();
            if let Some(hook) = hook {
                hook(index);
            }
            let pause = *McpManager::lock(&self.pause_at) == Some(index);
            if pause {
                let budget = asupersync::Cx::current().map(|cx| cx.budget());
                poll_fn(|_| {
                    assert_eq!(asupersync::Cx::current().map(|cx| cx.budget()), budget);
                    Poll::<()>::Pending
                })
                .await;
            }
            Ok(page)
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

    fn tool(name: &str) -> Value {
        json!({"name":name,"description":"fixture","inputSchema":{"type":"object"}})
    }

    fn fixture(
        temp: &tempfile::TempDir,
        pages: Vec<Value>,
        hook: Option<Arc<PageHook>>,
    ) -> (McpManager, Arc<ServerEntry>, Arc<PagedTransport>) {
        let config = ConfiguredServer {
            name: "catalog".to_string(),
            command: None,
            args: Vec::new(),
            env: Vec::new(),
            url: Some("https://catalog.invalid/mcp".to_string()),
            headers: Vec::new(),
            transport_hint: Some("http".to_string()),
            provenance: Provenance::ProjectPi,
            source_file: temp.path().join("mcp.json"),
        };
        let manager = McpManager::new(
            temp.path(),
            temp.path(),
            McpDiscovery {
                servers: vec![config],
                warnings: Vec::new(),
            },
        );
        let entry = manager.entry("catalog").expect("fixture server");
        TrustStore::load(&manager.inner.trust_path)
            .expect("trust store")
            .acknowledge(
                "catalog",
                &manager.trust_fingerprint_for(&entry),
                "operator",
            )
            .expect("trust fixture");
        let transport = Arc::new(PagedTransport {
            pages: Mutex::new(pages.into()),
            requests: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            after_page: Mutex::new(hook),
            pause_at: Mutex::new(None),
        });
        let erased: Arc<dyn McpTransport> = transport.clone();
        *McpManager::lock(&entry.transport) = Some(erased);
        *McpManager::lock(&entry.health) = ServerHealth::Ready { tools: 0 };
        (manager, entry, transport)
    }

    #[test]
    fn every_page_reaches_the_mountable_catalog_in_server_order() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(
            &temp,
            vec![
                json!({"tools":[tool("first")],"nextCursor":"opaque +/="}),
                json!({"tools":[tool("second")],"nextCursor":""}),
                json!({"tools":[tool("third")]}),
            ],
            None,
        );
        let weak_entry = Arc::downgrade(&entry);
        *McpManager::lock(&transport.after_page) = Some(Arc::new(move |_| {
            let entry = weak_entry.upgrade().expect("entry remains alive");
            assert!(
                McpManager::lock(&entry.tools_cache).is_none(),
                "no partial catalog may be published during page traversal"
            );
        }));
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let tools = runtime
            .block_on(manager.list_and_cache_tools(&entry))
            .expect("whole catalog");
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second", "third"]
        );
        assert_eq!(manager.mounted_tool_metas()[0].1.len(), 3);
        let (params, all_within_timeout, monotonic) = {
            let requests = McpManager::lock(&transport.requests);
            (
                requests
                    .iter()
                    .map(|(params, _)| params.clone())
                    .collect::<Vec<_>>(),
                requests
                    .iter()
                    .all(|(_, timeout)| *timeout <= DEFAULT_MCP_TIMEOUT),
                requests.windows(2).all(|pair| pair[1].1 <= pair[0].1),
            )
        };
        assert_eq!(
            params,
            vec![
                json!({}),
                json!({"cursor":"opaque +/="}),
                json!({"cursor":""})
            ]
        );
        assert!(all_within_timeout);
        assert!(
            monotonic,
            "later pages must spend the same timeout, not reset it"
        );
        assert!(!transport.closed.load(Ordering::Acquire));
    }

    #[test]
    fn empty_pages_with_a_cursor_do_not_end_discovery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, _) = fixture(
            &temp,
            vec![
                json!({"tools":[],"nextCursor":"more"}),
                json!({"tools":[tool("found")]}),
            ],
            None,
        );
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let tools = runtime
            .block_on(manager.list_and_cache_tools(&entry))
            .expect("later tool");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "found");
    }

    #[test]
    fn malformed_later_pages_invalidate_the_whole_catalog() {
        for second in [
            json!({"tools":[tool("duplicate")]}),
            json!({"tools":[],"nextCursor":"again"}),
            json!({"tools":[],"nextCursor":42}),
            json!({"tools":[],"nextCursor":null}),
            json!({"tools":[{"name":"broken"}]}),
        ] {
            let temp = tempfile::tempdir().expect("tempdir");
            let (manager, entry, transport) = fixture(
                &temp,
                vec![
                    json!({"tools":[tool("duplicate")],"nextCursor":"again"}),
                    second,
                ],
                None,
            );
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("runtime");
            let error = runtime
                .block_on(manager.list_and_cache_tools(&entry))
                .expect_err("no partial catalog");
            assert!(error.to_string().contains("MCP_PROTOCOL"), "{error}");
            assert!(manager.mounted_tool_metas().is_empty());
            assert!(McpManager::lock(&entry.tools_cache).is_none());
            assert!(transport.closed.load(Ordering::Acquire));
            assert_eq!(McpManager::lock(&transport.requests).len(), 2);
        }
    }

    #[test]
    fn aggregate_tool_limit_applies_across_individually_valid_pages() {
        let mut catalog = Catalog::new();
        let first: Vec<Value> = (0..MAX_SERVER_TOOLS)
            .map(|index| tool(&format!("tool-{index}")))
            .collect();
        assert_eq!(
            catalog
                .append_page(&json!({"tools":first,"nextCursor":"more"}))
                .expect("first page"),
            Some("more".to_string())
        );
        let error = catalog
            .append_page(&json!({"tools":[tool("extra")]}))
            .expect_err("aggregate limit");
        assert!(error.to_string().contains("aggregate tool limit"));
    }

    #[test]
    fn unique_empty_pages_cannot_extend_discovery_without_bound() {
        let mut catalog = Catalog::new();
        for index in 0..MAX_CATALOG_PAGES - 1 {
            catalog
                .append_page(&json!({"tools":[],"nextCursor":format!("page-{index}")}))
                .expect("within page budget");
        }
        let error = catalog
            .append_page(&json!({"tools":[],"nextCursor":"one-more"}))
            .expect_err("stop before dispatching page beyond cap");
        assert!(error.to_string().contains("page limit"));
    }

    #[test]
    fn a_final_page_at_the_page_limit_is_accepted() {
        let mut catalog = Catalog::new();
        for index in 0..MAX_CATALOG_PAGES - 1 {
            catalog
                .append_page(&json!({"tools":[],"nextCursor":index.to_string()}))
                .expect("page");
        }
        assert_eq!(
            catalog
                .append_page(&json!({"tools":[tool("last")]}))
                .expect("last page"),
            None
        );
        assert_eq!(catalog.tools[0].name, "last");
    }

    #[test]
    fn cursor_cycles_and_oversized_tokens_fail_without_echoing_tokens() {
        let mut catalog = Catalog::new();
        for cursor in ["secret-cursor-a", "secret-cursor-b"] {
            catalog
                .append_page(&json!({"tools":[],"nextCursor":cursor}))
                .expect("distinct cursor");
        }
        let error = catalog
            .append_page(&json!({"tools":[],"nextCursor":"secret-cursor-a"}))
            .expect_err("cycle");
        assert!(!error.to_string().contains("secret-cursor"));
        let error = Catalog::new()
            .append_page(&json!({"tools":[],"nextCursor":"x".repeat(MAX_CURSOR_BYTES + 1)}))
            .expect_err("cursor bound");
        assert!(error.to_string().contains("cursor exceeded"));
    }

    #[test]
    fn serialized_byte_budget_is_shared_and_counts_schema_escaping() {
        let page = json!({"tools":[tool("quoted\"name")],"nextCursor":"next"});
        let mut catalog = Catalog::new();
        catalog.bytes.remaining = serde_json::to_vec(&page).expect("encode").len();
        catalog.append_page(&page).expect("exact byte bound");
        assert_eq!(catalog.bytes.remaining, 0);
        let error = catalog
            .append_page(&json!({"tools":[]}))
            .expect_err("aggregate byte bound");
        assert!(error.to_string().contains("aggregate catalog byte limit"));
    }

    #[test]
    fn deadline_accounting_is_total_and_expiry_is_terminal() {
        let start = Instant::now();
        let deadline = start + DEFAULT_MCP_TIMEOUT;
        assert_eq!(
            remaining_budget(deadline, start).expect("full budget"),
            DEFAULT_MCP_TIMEOUT
        );
        assert_eq!(
            remaining_budget(deadline, start + Duration::from_secs(5)).expect("remaining"),
            DEFAULT_MCP_TIMEOUT
                .checked_sub(Duration::from_secs(5))
                .expect("valid timeout")
        );
        for now in [deadline, deadline + Duration::from_secs(1)] {
            assert!(
                remaining_budget(deadline, now)
                    .expect_err("expired")
                    .to_string()
                    .contains("MCP_TIMEOUT")
            );
        }
    }

    #[test]
    fn revocation_between_pages_stops_before_another_request() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(
            &temp,
            vec![
                json!({"tools":[tool("first")],"nextCursor":"more"}),
                json!({"tools":[tool("must-not-fetch")]}),
            ],
            None,
        );
        let path = manager.inner.trust_path.clone();
        let fingerprint = manager.trust_fingerprint_for(&entry);
        let hook: Arc<PageHook> = Arc::new(move |_| {
            TrustStore::load(&path)
                .expect("reload trust")
                .deny("catalog", &fingerprint, "operator")
                .expect("revoke during response");
        });
        *McpManager::lock(&transport.after_page) = Some(hook);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(manager.list_and_cache_tools(&entry))
            .expect_err("revoked catalog");
        assert!(error.to_string().contains("MCP_TRUST_DENIED"), "{error}");
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
        assert!(transport.closed.load(Ordering::Acquire));
        assert!(manager.mounted_tool_metas().is_empty());
    }

    #[test]
    fn replacement_between_pages_never_inherits_an_old_cursor_or_partial_catalog() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, old) = fixture(
            &temp,
            vec![
                json!({"tools":[tool("obsolete")],"nextCursor":"old-server-cursor"}),
                json!({"tools":[tool("must-not-request")]}),
            ],
            None,
        );
        let replacement = Arc::new(PagedTransport {
            pages: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            after_page: Mutex::new(None),
            pause_at: Mutex::new(None),
        });
        let weak_entry = Arc::downgrade(&entry);
        let replacement_for_hook: Arc<dyn McpTransport> = replacement.clone();
        *McpManager::lock(&old.after_page) = Some(Arc::new(move |index| {
            assert_eq!(
                index, 1,
                "old transport must not receive another page request"
            );
            let entry = weak_entry.upgrade().expect("entry remains alive");
            *McpManager::lock(&entry.transport) = Some(Arc::clone(&replacement_for_hook));
            *McpManager::lock(&entry.tools_cache) = Some((
                Instant::now(),
                vec![McpToolMeta {
                    name: "replacement".to_string(),
                    description: String::new(),
                    input_schema: json!({}),
                    output_schema: None,
                }],
            ));
            *McpManager::lock(&entry.health) = ServerHealth::Ready { tools: 1 };
        }));
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(manager.list_and_cache_tools(&entry))
            .expect_err("superseded catalog");
        assert!(
            error.to_string().contains("MCP_TRANSPORT_SUPERSEDED"),
            "{error}"
        );
        assert_eq!(McpManager::lock(&old.requests).len(), 1);
        assert!(McpManager::lock(&replacement.requests).is_empty());
        assert!(old.closed.load(Ordering::Acquire));
        assert!(!replacement.closed.load(Ordering::Acquire));
        let mounted = manager.mounted_tool_metas();
        assert_eq!(mounted.len(), 1);
        assert_eq!(mounted[0].1.len(), 1);
        assert_eq!(mounted[0].1[0].name, "replacement");
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
    }

    #[test]
    fn dropped_discovery_retires_first_or_later_pending_page_and_releases_lane() {
        for pause_at in [1, 2] {
            let temp = tempfile::tempdir().expect("tempdir");
            let (manager, entry, transport) = fixture(
                &temp,
                vec![
                    json!({"tools":[tool("first")],"nextCursor":"more"}),
                    json!({"tools":[tool("last")]}),
                ],
                None,
            );
            *McpManager::lock(&transport.pause_at) = Some(pause_at);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("runtime");
            runtime.block_on(async {
                let mut discovery = Box::pin(manager.list_and_cache_tools(&entry));
                assert!(futures::poll!(discovery.as_mut()).is_pending());
                assert_eq!(McpManager::lock(&transport.requests).len(), pause_at);
                assert!(!transport.closed.load(Ordering::Acquire));
                drop(discovery);
                assert!(transport.closed.load(Ordering::Acquire));
                assert!(McpManager::lock(&entry.transport).is_none());
                assert!(McpManager::lock(&entry.tools_cache).is_none());
                assert!(manager.mounted_tool_metas().is_empty());
                assert_eq!(McpManager::lock(&entry.restarts).count, 1);
                assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_ok());
            });
        }
    }

    #[test]
    fn dropping_unpolled_discovery_does_not_retire_a_healthy_connection() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, vec![], None);
        drop(Box::pin(manager.list_and_cache_tools(&entry)));
        assert!(!transport.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&transport.requests).is_empty());
        assert!(McpManager::lock(&entry.transport).is_some());
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
    }

    #[test]
    fn cancellation_of_an_old_catalog_preserves_replacement_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, old) = fixture(&temp, vec![json!({"tools":[tool("obsolete")]})], None);
        *McpManager::lock(&old.pause_at) = Some(1);
        let replacement = Arc::new(PagedTransport {
            pages: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            after_page: Mutex::new(None),
            pause_at: Mutex::new(None),
        });
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let mut discovery = Box::pin(manager.list_and_cache_tools(&entry));
            assert!(futures::poll!(discovery.as_mut()).is_pending());
            assert_eq!(McpManager::lock(&old.requests).len(), 1);
            let erased: Arc<dyn McpTransport> = replacement.clone();
            *McpManager::lock(&entry.transport) = Some(erased);
            *McpManager::lock(&entry.tools_cache) = Some((
                Instant::now(),
                vec![McpToolMeta {
                    name: "replacement".to_string(),
                    description: String::new(),
                    input_schema: json!({}),
                    output_schema: None,
                }],
            ));
            *McpManager::lock(&entry.health) = ServerHealth::Ready { tools: 1 };
            drop(discovery);
            assert!(old.closed.load(Ordering::Acquire));
            assert!(!replacement.closed.load(Ordering::Acquire));
            let mounted = manager.mounted_tool_metas();
            assert_eq!(mounted.len(), 1);
            assert_eq!(mounted[0].1[0].name, "replacement");
            assert_eq!(McpManager::lock(&entry.restarts).count, 0);
            assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_ok());
        });
    }

    #[test]
    fn a_final_page_from_a_dead_transport_is_not_publishable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) =
            fixture(&temp, vec![json!({"tools":[tool("must-not-mount")]})], None);
        let weak = Arc::downgrade(&transport);
        *McpManager::lock(&transport.after_page) = Some(Arc::new(move |_| {
            weak.upgrade().expect("transport alive").abort();
        }));
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(manager.list_and_cache_tools(&entry))
            .expect_err("dead generation");
        assert!(error.to_string().contains("MCP_TRANSPORT_CLOSED"));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert!(manager.mounted_tool_metas().is_empty());
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
    }

    struct WakeCounter(std::sync::atomic::AtomicUsize);

    impl std::task::Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn owner_cancellation_wakes_idle_discovery_without_transport_cooperation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) =
            fixture(&temp, vec![json!({"tools":[tool("never-returned")]})], None);
        *McpManager::lock(&transport.pause_at) = Some(1);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let budget = asupersync::Budget::new().with_poll_quota(1000);
        let owner = crate::agent_cx::AgentCx::from_cx(runtime.request_cx_with_budget(budget));
        runtime.block_on(async {
            let parent = asupersync::Cx::current().expect("runtime caller");
            let counter = Arc::new(WakeCounter(std::sync::atomic::AtomicUsize::new(0)));
            let waker = std::task::Waker::from(Arc::clone(&counter));
            let mut task = std::task::Context::from_waker(&waker);
            let mut discovery = Box::pin(manager.list_and_cache_tools(&entry));
            {
                let _guard = owner.cx().clone().set_current_restricted();
                assert!(discovery.as_mut().poll(&mut task).is_pending());
            }
            // Re-poll under another caller: the transport fixture asserts
            // every poll still runs with its initially captured owner budget.
            assert!(discovery.as_mut().poll(&mut task).is_pending());
            assert_eq!(asupersync::Cx::current().unwrap().budget(), parent.budget());
            let wakes_before = counter.0.load(Ordering::SeqCst);
            owner.cancel_with(
                asupersync::types::CancelKind::User,
                Some("cancel discovery"),
            );
            assert!(counter.0.load(Ordering::SeqCst) > wakes_before);
            let Poll::Ready(Err(error)) = discovery.as_mut().poll(&mut task) else {
                panic!("cancellation must finish without a network response");
            };
            assert!(error.to_string().contains("MCP_CANCELLED"));
            assert!(transport.closed.load(Ordering::Acquire));
            assert!(McpManager::lock(&entry.transport).is_none());
            assert!(McpManager::lock(&entry.tools_cache).is_none());
            assert_eq!(McpManager::lock(&transport.requests).len(), 1);
            assert_eq!(McpManager::lock(&entry.restarts).count, 1);
            assert!(!parent.is_cancel_requested());
            assert_eq!(asupersync::Cx::current().unwrap().budget(), parent.budget());
            assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_ok());
        });
    }

    #[test]
    fn manager_deadline_terminates_a_transport_that_ignores_its_timeout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) =
            fixture(&temp, vec![json!({"tools":[tool("never-returned")]})], None);
        *McpManager::lock(&transport.pause_at) = Some(1);
        let erased: Arc<dyn McpTransport> = transport.clone();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(manager.collect_tool_catalog_with_timeout(
                &entry,
                &erased,
                Duration::from_millis(10),
            ))
            .expect_err("manager enforces deadline");
        assert!(error.to_string().contains("MCP_TIMEOUT"));
        assert!(transport.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert!(manager.mounted_tool_metas().is_empty());
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
    }

    #[test]
    fn cancellation_racing_a_page_response_cannot_publish_or_fetch_another_page() {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let owner = crate::agent_cx::AgentCx::from_cx(
            runtime.request_cx_with_budget(asupersync::Budget::new()),
        );
        let owner_for_hook = owner.clone();
        let (manager, entry, transport) = fixture(
            &temp,
            vec![
                json!({"tools":[tool("first")],"nextCursor":"more"}),
                json!({"tools":[tool("must-not-fetch")]}),
            ],
            Some(Arc::new(move |_| {
                owner_for_hook.cancel_with(
                    asupersync::types::CancelKind::User,
                    Some("cancel as response arrives"),
                );
            })),
        );
        runtime.block_on(async {
            let mut discovery = Box::pin(manager.list_and_cache_tools(&entry));
            let result = {
                let _guard = owner.cx().clone().set_current_restricted();
                futures::poll!(discovery.as_mut())
            };
            let Poll::Ready(Err(error)) = result else {
                panic!("cancellation must beat the simultaneous response");
            };
            assert!(error.to_string().contains("MCP_CANCELLED"));
        });
        assert!(transport.closed.load(Ordering::Acquire));
        assert!(manager.mounted_tool_metas().is_empty());
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
    }

    #[test]
    fn pre_cancelled_or_restricted_owner_cannot_dispatch_a_page() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (_manager, _entry, transport) = fixture(&temp, vec![], None);
        let erased: Arc<dyn McpTransport> = transport.clone();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let cancelled = crate::agent_cx::AgentCx::for_request();
        cancelled.cancel_with(asupersync::types::CancelKind::User, Some("before dispatch"));
        let restricted = {
            let _guard = asupersync::Cx::for_request()
                .restrict::<asupersync::cx::cap::None>()
                .set_current_restricted();
            crate::agent_cx::AgentCx::for_current_or_request()
        };
        for (owner, code) in [
            (cancelled, "MCP_CANCELLED"),
            (restricted, "MCP_CAPABILITY_DENIED"),
        ] {
            let error = runtime
                .block_on(request_with_owner(
                    &owner,
                    &erased,
                    "tools/list",
                    json!({}),
                    DEFAULT_MCP_TIMEOUT,
                ))
                .expect_err("owner denies dispatch");
            assert!(error.to_string().contains(code), "{error}");
        }
        assert!(McpManager::lock(&transport.requests).is_empty());
    }

    #[test]
    fn expired_request_budget_prevents_dispatch_and_late_success() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (_manager, _entry, transport) = fixture(
            &temp,
            vec![json!({"tools":[]})],
            Some(Arc::new(|_| std::thread::sleep(Duration::from_millis(100)))),
        );
        let erased: Arc<dyn McpTransport> = transport.clone();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let owner = crate::agent_cx::AgentCx::from_cx(
            runtime.request_cx_with_budget(asupersync::Budget::new()),
        );
        let error = runtime
            .block_on(request_with_owner(
                &owner,
                &erased,
                "tools/list",
                json!({}),
                Duration::ZERO,
            ))
            .expect_err("zero budget");
        assert!(error.to_string().contains("MCP_TIMEOUT"));
        assert!(McpManager::lock(&transport.requests).is_empty());
        let error = runtime
            .block_on(request_with_owner(
                &owner,
                &erased,
                "tools/list",
                json!({}),
                Duration::from_millis(50),
            ))
            .expect_err("late success is not success");
        assert!(error.to_string().contains("MCP_TIMEOUT"));
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
    }

    fn refresh_log(case: &str, requests: usize, code: &str) {
        eprintln!(
            "{}",
            json!({
                "schema":"pi.mcp.live_catalog.test.v1", "case":case,
                "requests":requests, "outcome":code, "rawSecretBytesEmitted":0
            })
        );
    }

    #[test]
    fn public_refresh_replaces_a_fresh_startup_catalog_and_retains_contracts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let schema = json!({"type":"object", "required":["count"]});
        let mut added = tool("added-after-startup");
        added["outputSchema"] = schema.clone();
        let (manager, entry, transport) = fixture(
            &temp,
            vec![
                json!({"tools":[tool("changed")], "nextCursor":"next"}),
                json!({"tools":[added]}),
            ],
            None,
        );
        *McpManager::lock(&entry.tools_cache) = Some((
            Instant::now(),
            parse_tool_list(&json!({"tools":[tool("removed")]})).unwrap(),
        ));
        McpManager::lock(&entry.restarts).count = 2;
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let tools = runtime
            .block_on(manager.refresh_tools("catalog"))
            .expect("fresh complete catalog");
        refresh_log(
            "fresh-catalog-replacement",
            McpManager::lock(&transport.requests).len(),
            "ok",
        );
        assert_eq!(
            tools
                .iter()
                .map(|meta| meta.name.as_str())
                .collect::<Vec<_>>(),
            vec!["changed", "added-after-startup"]
        );
        assert_eq!(tools[1].output_schema.as_ref().unwrap().schema(), &schema);
        assert_eq!(manager.mounted_tool_metas()[0].1.len(), 2);
        assert_eq!(
            McpManager::lock(&entry.restarts).count,
            2,
            "discovery must not reset crash history"
        );
        assert_eq!(McpManager::lock(&transport.requests).len(), 2);
        assert!(!transport.closed.load(Ordering::Acquire));
    }

    #[test]
    fn public_refresh_accepts_removal_of_every_previously_advertised_tool() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, vec![json!({"tools":[]})], None);
        *McpManager::lock(&entry.tools_cache) = Some((
            Instant::now(),
            parse_tool_list(&json!({"tools":[tool("removed")]})).unwrap(),
        ));
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let tools = runtime
            .block_on(manager.refresh_tools("catalog"))
            .expect("empty catalog");
        refresh_log(
            "empty-replacement",
            McpManager::lock(&transport.requests).len(),
            "ok",
        );
        assert!(tools.is_empty());
        assert!(
            McpManager::lock(&entry.tools_cache)
                .as_ref()
                .unwrap()
                .1
                .is_empty()
        );
        assert!(matches!(
            *McpManager::lock(&entry.health),
            ServerHealth::Ready { tools: 0 }
        ));
        assert!(!transport.closed.load(Ordering::Acquire));
    }

    #[test]
    fn public_refresh_rejects_partial_catalogs_instead_of_returning_cached_tools() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(
            &temp,
            vec![
                json!({"tools":[tool("first")], "nextCursor":"next"}),
                json!({"tools":[{"name":"invalid", "inputSchema":{}}], "nextCursor":7}),
            ],
            None,
        );
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(manager.refresh_tools("catalog"))
            .expect_err("whole catalog required");
        refresh_log(
            "partial-catalog",
            McpManager::lock(&transport.requests).len(),
            "MCP_PROTOCOL",
        );
        assert!(error.to_string().contains("MCP_PROTOCOL"));
        assert!(McpManager::lock(&entry.tools_cache).is_none());
        assert!(transport.closed.load(Ordering::Acquire));
        assert_eq!(McpManager::lock(&transport.requests).len(), 2);
    }

    #[test]
    fn public_refresh_does_not_reset_an_exhausted_server() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, vec![], None);
        McpManager::lock(&entry.transport).take();
        McpManager::lock(&entry.restarts).count = super::super::MAX_RESTARTS;
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(manager.refresh_tools("catalog"))
            .expect_err("explicit recovery required");
        refresh_log(
            "restart-budget",
            McpManager::lock(&transport.requests).len(),
            "MCP_RESTART_EXHAUSTED",
        );
        assert!(error.to_string().contains("MCP_RESTART_EXHAUSTED"));
        assert_eq!(
            McpManager::lock(&entry.restarts).count,
            super::super::MAX_RESTARTS
        );
        assert!(McpManager::lock(&transport.requests).is_empty());
        assert!(McpManager::lock(&entry.transport).is_none());
    }

    #[test]
    fn public_refresh_pre_cancelled_or_restricted_owners_cannot_dispatch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, vec![], None);
        let cancelled = crate::agent_cx::AgentCx::for_request();
        cancelled.cancel_with(asupersync::types::CancelKind::User, Some("before refresh"));
        let restricted = {
            let _guard = asupersync::Cx::for_request()
                .restrict::<asupersync::cx::cap::None>()
                .set_current_restricted();
            crate::agent_cx::AgentCx::for_current_or_request()
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        for (owner, code) in [
            (cancelled, "MCP_CANCELLED"),
            (restricted, "MCP_CAPABILITY_DENIED"),
        ] {
            let error = runtime
                .block_on(owner.with_current(manager.refresh_tools("catalog")))
                .expect_err("refresh owner rejected");
            refresh_log(
                "owner-admission",
                McpManager::lock(&transport.requests).len(),
                code,
            );
            assert!(error.to_string().contains(code), "{error}");
        }
        assert!(McpManager::lock(&transport.requests).is_empty());
        assert!(!transport.closed.load(Ordering::Acquire));
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
    }

    #[test]
    fn public_refresh_deadline_includes_waiting_for_the_connection_lane() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, vec![], None);
        let lane = Arc::clone(&entry.connect_lane)
            .try_lock_owned()
            .expect("hold lane");
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(manager.refresh_tools_with_timeout("catalog", Duration::from_millis(10)))
            .expect_err("lane admission spends the deadline");
        refresh_log(
            "admission-deadline",
            McpManager::lock(&transport.requests).len(),
            "MCP_TIMEOUT",
        );
        assert!(error.to_string().contains("MCP_TIMEOUT"));
        assert!(McpManager::lock(&transport.requests).is_empty());
        assert!(!transport.closed.load(Ordering::Acquire));
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
        drop(lane);
        assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_ok());
    }

    #[test]
    fn public_refresh_deadline_retires_a_pending_page_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) =
            fixture(&temp, vec![json!({"tools":[tool("late")]})], None);
        *McpManager::lock(&transport.pause_at) = Some(1);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(manager.refresh_tools_with_timeout("catalog", Duration::from_millis(10)))
            .expect_err("outer refresh deadline");
        refresh_log(
            "pending-page-deadline",
            McpManager::lock(&transport.requests).len(),
            "MCP_TIMEOUT",
        );
        assert!(error.to_string().contains("MCP_TIMEOUT"));
        assert!(transport.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert!(McpManager::lock(&entry.tools_cache).is_none());
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
        assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_ok());
    }

    #[test]
    fn public_refresh_abandonment_cleans_up_without_replaying_discovery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, transport) = fixture(&temp, vec![json!({"tools":[]})], None);
        *McpManager::lock(&transport.pause_at) = Some(1);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let mut refresh = Box::pin(manager.refresh_tools("catalog"));
            assert!(futures::poll!(refresh.as_mut()).is_pending());
            drop(refresh);
        });
        refresh_log(
            "abandoned-refresh",
            McpManager::lock(&transport.requests).len(),
            "cancelled",
        );
        assert_eq!(McpManager::lock(&transport.requests).len(), 1);
        assert!(transport.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
        assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_ok());
    }
}
