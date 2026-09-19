//! Complete, bounded MCP tool catalogs on a single transport generation.
//!
//! `nextCursor` is opaque, including an empty string. No schemas become
//! mountable until every page succeeds. The request timeout is shared by the
//! whole traversal; a server cannot multiply it by returning more pages.

use std::collections::HashSet;
use std::io::Write;

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

impl McpManager {
    pub(super) async fn collect_tool_catalog(
        &self,
        entry: &Arc<ServerEntry>,
        transport: &Arc<dyn McpTransport>,
    ) -> Result<Vec<McpToolMeta>> {
        let deadline = Instant::now() + DEFAULT_MCP_TIMEOUT;
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
            let result = transport.request("tools/list", params, remaining).await;
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
                futures::future::pending::<()>().await;
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
        let (manager, entry, old) = fixture(
            &temp,
            vec![json!({"tools":[tool("obsolete")]})],
            None,
        );
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
        let (manager, entry, transport) = fixture(
            &temp,
            vec![json!({"tools":[tool("must-not-mount")]})],
            None,
        );
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
}
