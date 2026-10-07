use super::*;
use crate::agent::{Agent, AgentConfig};
use crate::model::{AssistantMessage, ContentBlock, StopReason, StreamEvent, TextContent, ToolCall};
use crate::provider::{Context, Provider, StreamOptions, ToolDef};
use crate::tools::{SharedToolRegistry, Tool, ToolOrigin, ToolOutput, ToolRegistry, ToolUpdate};
use async_trait::async_trait;
use serde_json::json;
use std::sync::atomic::AtomicUsize;

#[derive(Default)]
struct CatalogState {
    catalog: Mutex<Value>,
    lists: AtomicUsize,
    starts: AtomicUsize,
    calls: Mutex<Vec<Value>>,
    hold_list: AtomicBool,
    list_started: Mutex<Option<futures::channel::oneshot::Sender<()>>>,
}

struct CatalogTransport {
    state: Arc<CatalogState>,
    closed: AtomicBool,
}

#[async_trait]
impl McpTransport for CatalogTransport {
    async fn request(&self, method: &str, params: Value, _timeout: Duration) -> Result<Value> {
        match method {
            "initialize" => Ok(json!({})),
            "tools/list" => {
                self.state.lists.fetch_add(1, Ordering::SeqCst);
                if self.state.hold_list.load(Ordering::SeqCst) {
                    if let Some(started) = McpManager::lock(&self.state.list_started).take() {
                        let _ = started.send(());
                    }
                    return futures::future::pending().await;
                }
                Ok(McpManager::lock(&self.state.catalog).clone())
            }
            "tools/call" => {
                McpManager::lock(&self.state.calls).push(params);
                Ok(json!({"content":[{"type":"text","text":"called current catalog"}]}))
            }
            _ => Err(tool_err("MCP_PROTOCOL", "unexpected fixture method")),
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

#[derive(Default)]
struct CatalogProvider {
    contexts: Mutex<Vec<Vec<ToolDef>>>,
    call_next: AtomicBool,
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for CatalogProvider {
    fn name(&self) -> &str {
        "catalog-fixture"
    }

    fn api(&self) -> &str {
        "catalog-fixture"
    }

    fn model_id(&self) -> &str {
        "catalog-fixture"
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
        McpManager::lock(&self.contexts).push(context.tools.to_vec());
        let call = self.call_next.swap(false, Ordering::SeqCst);
        let reason = if call {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
        let content = if call {
            vec![ContentBlock::ToolCall(ToolCall {
                id: "catalog-call".into(),
                name: "mcp__fixture__echo".into(),
                arguments: json!({"new_argument":"accepted"}),
                thought_signature: None,
            })]
        } else {
            vec![ContentBlock::Text(TextContent::new("done"))]
        };
        Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Done {
            reason,
            message: AssistantMessage {
                content,
                stop_reason: reason,
                ..Default::default()
            },
        })])))
    }
}

fn meta(name: &str, argument: &str) -> Value {
    json!({"name":name,"description":format!("uses {argument}"),"inputSchema":{
        "type":"object","properties":{argument:{"type":"string"}},
        "required":[argument],"additionalProperties":false
    }})
}

fn fixture(
    temp: &tempfile::TempDir,
    catalog: Value,
) -> (Arc<McpManager>, Arc<ServerEntry>, Arc<CatalogState>) {
    let (manager, entry) = super::tests::trusted_fixture_manager(temp);
    let state = Arc::new(CatalogState {
        catalog: Mutex::new(catalog),
        ..Default::default()
    });
    let factory_state = Arc::clone(&state);
    *McpManager::lock(&manager.inner.transport_factory) =
        Some(Arc::new(move || -> Box<dyn McpTransport> {
            factory_state.starts.fetch_add(1, Ordering::SeqCst);
            Box::new(CatalogTransport {
                state: Arc::clone(&factory_state),
                closed: AtomicBool::new(false),
            })
        }));
    (Arc::new(manager), entry, state)
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime")
}

fn agent(provider: &Arc<CatalogProvider>) -> Agent {
    let provider: Arc<dyn Provider> = provider.clone();
    Agent::new(
        provider,
        ToolRegistry::from_tools(Vec::new()),
        AgentConfig::default(),
    )
}

#[test]
fn live_agent_reconciles_refreshed_schemas_and_removed_tools_before_provider_dispatch() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (manager, _, state) = fixture(
        &temp,
        json!({"tools":[meta("echo","old_argument"),meta("removed","old")]}),
    );
    let provider = Arc::new(CatalogProvider::default());
    let mut agent = agent(&provider);
    let registry = agent.shared_tools();
    runtime().block_on(async {
        manager.test("fixture").await.expect("initial catalog");
        crate::mcp::reconcile_tools(&manager, &registry);
        agent.run("first request", |_| {}).await.expect("first turn");
        let old = registry.snapshot();
        let version = registry.version();
        *McpManager::lock(&state.catalog) =
            json!({"tools":[meta("echo","new_argument"),meta("added","new")]});
        manager
            .test("fixture")
            .await
            .expect("explicit server refresh");
        provider.call_next.store(true, Ordering::SeqCst);
        agent
            .run("use refreshed tool", |_| {})
            .await
            .expect("tool turn");
        assert!(registry.version() > version);
        let contexts = McpManager::lock(&provider.contexts);
        assert_eq!(
            contexts[0]
                .iter()
                .find(|tool| tool.name == "mcp__fixture__echo")
                .expect("old tool")
                .parameters["required"],
            json!(["old_argument"])
        );
        for context in &contexts[1..] {
            assert!(!context.iter().any(|tool| tool.name == "mcp__fixture__removed"));
            assert!(context.iter().any(|tool| tool.name == "mcp__fixture__added"));
            assert_eq!(
                context
                    .iter()
                    .find(|tool| tool.name == "mcp__fixture__echo")
                    .expect("new tool")
                    .parameters["required"],
                json!(["new_argument"])
            );
        }
        drop(contexts);
        assert_eq!(
            McpManager::lock(&state.calls).as_slice(),
            &[json!({"name":"echo","arguments":{"new_argument":"accepted"}})]
        );
        assert_eq!(
            old.get("mcp__fixture__echo")
                .expect("retained snapshot")
                .parameters()["required"],
            json!(["old_argument"])
        );
        assert!(old.get("mcp__fixture__removed").is_some());
        let stable = registry.version();
        agent.run("unchanged", |_| {}).await.expect("unchanged turn");
        assert_eq!(
            registry.version(),
            stable,
            "no schema change means no invalidation"
        );
        assert_eq!(
            state.lists.load(Ordering::SeqCst),
            2,
            "fresh catalogs do no discovery"
        );
    });
}

#[test]
fn live_agent_refreshes_empty_expired_and_reconnected_catalogs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (manager, entry, state) = fixture(&temp, json!({"tools":[]}));
    let provider = Arc::new(CatalogProvider::default());
    let mut agent = agent(&provider);
    let registry = agent.shared_tools();
    runtime().block_on(async {
        manager.test("fixture").await.expect("empty catalog");
        crate::mcp::reconcile_tools(&manager, &registry);
        *McpManager::lock(&state.catalog) = json!({"tools":[meta("late","value")]});
        let expired_at = Instant::now() // ubs:ignore[rust.ownership.unwrap-expect] -- This expiry fixture requires a representable past instant.
            .checked_sub(TOOL_CACHE_TTL)
            .and_then(|at| at.checked_sub(Duration::from_secs(1)))
            .expect("test clock can represent an expired catalog");
        McpManager::lock(&entry.tools_cache).as_mut().expect("cache").0 = expired_at;
        agent
            .run("discover newly available tool", |_| {})
            .await
            .expect("TTL refresh");
        assert!(agent.has_tool("mcp__fixture__late"));
        let old_transport = McpManager::lock(&entry.transport)
            .clone()
            .expect("transport");
        old_transport.abort();
        *McpManager::lock(&state.catalog) = json!({"tools":[meta("replacement","value")]});
        agent
            .run("recover server", |_| {})
            .await
            .expect("reconnected turn");
        assert!(agent.has_tool("mcp__fixture__replacement"));
        assert!(!agent.has_tool("mcp__fixture__late"));
        assert_eq!(state.starts.load(Ordering::SeqCst), 2);
        assert_eq!(state.lists.load(Ordering::SeqCst), 3);
    });
}

#[test]
fn live_agent_withdraws_failed_catalogs_and_denied_servers_without_restarting_them() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (manager, _, state) = fixture(&temp, json!({"tools":[meta("echo","value")]}));
    let provider = Arc::new(CatalogProvider::default());
    let mut agent = agent(&provider);
    let registry = agent.shared_tools();
    runtime().block_on(async {
        manager.test("fixture").await.expect("catalog");
        crate::mcp::reconcile_tools(&manager, &registry);
        *McpManager::lock(&state.catalog) = json!({"tools":[{"name":"malformed"}]});
        manager
            .test("fixture")
            .await
            .expect_err("bad metadata rejected");
        agent
            .run("failed discovery", |_| {})
            .await
            .expect("turn without stale tools");
        assert!(!agent.has_tool("mcp__fixture__echo"));
        *McpManager::lock(&state.catalog) = json!({"tools":[meta("recovered","value")]});
        manager.test("fixture").await.expect("operator recovery");
        manager.deny("fixture").await.expect("deny");
        agent.run("denied catalog", |_| {}).await.expect("denied turn");
        assert!(registry.snapshot().tools().is_empty());
        let starts = state.starts.load(Ordering::SeqCst);
        let lists = state.lists.load(Ordering::SeqCst);
        agent.run("still denied", |_| {}).await.expect("no restart");
        assert_eq!(state.starts.load(Ordering::SeqCst), starts);
        assert_eq!(state.lists.load(Ordering::SeqCst), lists);
        // A shallow copy made while no wrappers exist must retain the owner
        // binding, rather than rediscovering managers only from live tools.
        let copied = SharedToolRegistry::from_arc(registry.snapshot());
        let copied_provider: Arc<dyn Provider> = provider.clone();
        agent = Agent::with_shared_tools(copied_provider, copied, AgentConfig::default());
        *McpManager::lock(&state.catalog) = json!({"tools":[meta("restored","value")]});
        manager.trust("fixture").await.expect("trust again");
        agent
            .run("empty binding survived denial", |_| {})
            .await
            .expect("restored turn");
        assert!(agent.has_tool("mcp__fixture__restored"));
    });
}

#[test]
fn aborting_catalog_refresh_never_dispatches_the_provider() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (manager, entry, state) = fixture(&temp, json!({"tools":[]}));
    let provider = Arc::new(CatalogProvider::default());
    let mut agent = agent(&provider);
    runtime().block_on(async {
        manager.test("fixture").await.expect("initial catalog");
        crate::mcp::reconcile_tools(&manager, &agent.shared_tools());
        let expired_at = Instant::now() // ubs:ignore[rust.ownership.unwrap-expect] -- Cancellation must exercise a real expired catalog.
            .checked_sub(TOOL_CACHE_TTL)
            .and_then(|at| at.checked_sub(Duration::from_secs(1)))
            .expect("test clock can represent an expired catalog");
        McpManager::lock(&entry.tools_cache).as_mut().expect("cache").0 = expired_at;
        state.hold_list.store(true, Ordering::SeqCst);
        let (started, receive) = futures::channel::oneshot::channel();
        *McpManager::lock(&state.list_started) = Some(started);
        let (abort, signal) = crate::agent::AbortHandle::new();
        let cancel = async {
            receive.await.expect("refresh reached tools/list");
            abort.abort();
        };
        let (result, ()) = futures::join!(
            agent.run_with_abort("cancel during discovery", Some(signal), |_| {}),
            cancel,
        );
        assert_eq!(result.expect("aborted turn").stop_reason, StopReason::Aborted);
        assert!(McpManager::lock(&provider.contexts).is_empty());
        assert!(McpManager::lock(&entry.tools_cache).is_none());
    });
}

struct ExtensionCollision {
    name: String,
}

#[async_trait]
impl Tool for ExtensionCollision {
    fn name(&self) -> &str {
        &self.name
    }

    fn label(&self) -> &str {
        &self.name
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "extension owns this name"
    }

    fn parameters(&self) -> Value {
        json!({"type":"object"})
    }

    fn origin(&self) -> ToolOrigin {
        ToolOrigin::Extension
    }

    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        Ok(ToolOutput {
            content: Vec::new(),
            details: None,
            is_error: false,
        })
    }
}

#[test]
fn reconciliation_preserves_other_owners_and_shelved_extension_collisions() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (manager, _, state) = fixture(
        &temp,
        json!({"tools":[meta("same","value"),meta("shelved","value")]}),
    );
    let registry = SharedToolRegistry::new(ToolRegistry::from_tools(vec![
        Box::new(ExtensionCollision {
            name: "mcp__fixture__same".into(),
        }),
        Box::new(ExtensionCollision {
            name: "mcp__fixture__shelved".into(),
        }),
    ]));
    registry.update(|tools| tools.set_active_extension_tools(&["mcp__fixture__same".into()]));
    runtime().block_on(async {
        manager.test("fixture").await.expect("catalog");
        crate::mcp::reconcile_tools(&manager, &registry);
        assert_eq!(
            registry
                .snapshot()
                .get("mcp__fixture__same")
                .expect("extension")
                .description(),
            "extension owns this name"
        );
        assert!(registry.snapshot().get("mcp__fixture__shelved").is_none());
        let other = Arc::new(McpManager::new(
            temp.path(),
            temp.path(),
            McpDiscovery {
                servers: Vec::new(),
                warnings: Vec::new(),
            },
        ));
        let foreign = super::parse_tool_list(&json!({"tools":[meta("foreign","value")]}))
            .expect("metadata");
        registry.update(|tools| {
            tools.push(Box::new(crate::mcp::McpTool::new(
                "fixture",
                &foreign[0],
                Arc::clone(&other),
            )));
        });
        *McpManager::lock(&state.catalog) = json!({"tools":[]});
        manager
            .test("fixture")
            .await
            .expect("authoritative empty catalog");
        crate::mcp::reconcile_tools(&manager, &registry);
        let snapshot = registry.snapshot();
        assert!(snapshot.get("mcp__fixture__foreign").is_some());
        assert_eq!(snapshot.inactive_tools().len(), 1);
        assert_eq!(snapshot.inactive_tools()[0].name(), "mcp__fixture__shelved");
        assert_eq!(
            snapshot
                .get("mcp__fixture__same")
                .expect("extension retained")
                .origin(),
            ToolOrigin::Extension
        );
    });
}
