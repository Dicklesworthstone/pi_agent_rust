//! One owner and one deadline for acquiring and initializing an MCP connection.
//!
//! Request deadlines alone do not bound secret resolution, initialize,
//! initialized notification, or receive-channel activation. The lane remains
//! owned until cleanup has settled, including cancellation racing publication.

use super::{
    Arc, DEFAULT_MCP_TIMEOUT, Duration, Error, Instant, McpManager, McpTransport, Result,
    ServerEntry, ServerHealth, catalog, tool_err,
};
use crate::agent_cx::AgentCx;

/// A server acknowledgement is not authority to borrow the worker's process
/// capability. Both stdio and command-derived HTTP credentials execute locally.
pub(super) fn check_transport_owner(
    owner: &AgentCx,
    config: &super::ConfiguredServer,
) -> Result<()> {
    catalog::check_request_owner(owner)?;
    if !owner.capabilities().spawn && transport_needs_spawn(config) {
        return Err(tool_err(
            "MCP_CAPABILITY_DENIED",
            "MCP server construction requires the owner's process-spawn capability",
        ));
    }
    Ok(())
}

fn transport_needs_spawn(config: &super::ConfiguredServer) -> bool {
    config.command.is_some()
        || config
            .headers
            .iter()
            .any(|(_, value)| value.trim().starts_with("$CMD:"))
}

/// Armed only after trust and restart admission select a new construction.
/// Cached connections and cancelled lane waiters never become owned attempts.
pub(super) struct ConnectionAttempt<'a> {
    manager: &'a McpManager,
    entry: &'a Arc<ServerEntry>,
    transport: Option<Arc<dyn McpTransport>>,
    armed: bool,
}

impl<'a> ConnectionAttempt<'a> {
    fn new(manager: &'a McpManager, entry: &'a Arc<ServerEntry>) -> Self {
        Self {
            manager,
            entry,
            transport: None,
            armed: false,
        }
    }

    pub(super) const fn begin(&mut self) {
        self.armed = true;
    }

    pub(super) fn observe(&mut self, transport: Arc<dyn McpTransport>) {
        self.transport = Some(transport);
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }

    // Generation ownership and its derived state must share one critical section.
    #[allow(clippy::significant_drop_tightening)]
    fn retire(&mut self, failure: Option<&Error>) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let owned = self.transport.take();
        {
            let mut current = McpManager::lock(&self.entry.transport);
            let owns_state = match (owned.as_ref(), current.as_ref()) {
                (_, None) => true,
                (Some(owned), Some(current)) => Arc::ptr_eq(owned, current),
                (None, Some(_)) => false,
            };
            if owns_state
                && !self
                    .manager
                    .inner
                    .shutting_down
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                drop(current.take());
                if let Some(error) = failure {
                    McpManager::record_failure(self.entry, error);
                } else {
                    // Abandonment is not itself proof of a server fault. The
                    // outer startup-budget owner accounts for its timeout;
                    // counting here too would consume two restart attempts.
                    drop(McpManager::lock(&self.entry.tools_cache).take());
                    *McpManager::lock(&self.entry.health) = ServerHealth::NotStarted;
                }
            }
        }
        // Never await close under a cancelled owner. A still-running blocking
        // constructor is separately fenced by TransportConstructionAttempt.
        if let Some(transport) = owned {
            transport.abort();
        }
    }
}

impl Drop for ConnectionAttempt<'_> {
    fn drop(&mut self) {
        self.retire(None);
    }
}

fn setup_timeout() -> Error {
    tool_err(
        "MCP_TIMEOUT",
        "MCP connection setup exhausted its total deadline",
    )
}

impl McpManager {
    pub(super) async fn ensure_ready(&self, entry: &Arc<ServerEntry>) -> Result<()> {
        self.ensure_ready_owned(entry, DEFAULT_MCP_TIMEOUT, true)
            .await
    }

    /// Startup, trust, test, and recovery already hold this connection's lane.
    pub(super) async fn ensure_ready_in_lane(&self, entry: &Arc<ServerEntry>) -> Result<()> {
        self.ensure_ready_owned(entry, DEFAULT_MCP_TIMEOUT, false)
            .await
    }

    async fn ensure_ready_owned(
        &self,
        entry: &Arc<ServerEntry>,
        timeout: Duration,
        acquire_lane: bool,
    ) -> Result<()> {
        let owner = AgentCx::for_current_or_request();
        catalog::check_request_owner(&owner)?;
        self.check_running()?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(setup_timeout)?;

        // Declare the lane first: cancellation drops the operation, then the
        // attempt, then the lane. No replacement constructor can slip between
        // aborting the private operation and retiring its published state.
        let lane = if acquire_lane {
            let acquisition = asupersync::sync::OwnedMutexGuard::lock(
                Arc::clone(&entry.connect_lane),
                owner.cx(),
            );
            Some(
                catalog::operation_with_owner(&owner, acquisition, timeout)
                    .await?
                    .map_err(|_| tool_err("MCP_CANCELLED", "cancelled while connecting server"))?,
            )
        } else {
            None
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(setup_timeout());
        }
        let mut attempt = ConnectionAttempt::new(self, entry);
        let mut inner_failed = false;
        let outcome = catalog::operation_with_owner(
            &owner,
            async {
                let result = self.ensure_ready_in_lane_inner(entry, &mut attempt).await;
                // Existing returned-error paths already close and account for the
                // failure. Do not count twice if cancellation races the same poll.
                inner_failed = result.is_err();
                result
            },
            remaining,
        )
        .await;
        let result = match outcome {
            Ok(result) => {
                attempt.disarm();
                result
            }
            Err(error) => {
                if inner_failed {
                    attempt.disarm();
                } else {
                    attempt.retire(Some(&error));
                }
                Err(error)
            }
        };
        drop(attempt);
        drop(lane);
        result
    }
}

#[cfg(test)]
mod tests {
    use std::future::{Future, poll_fn};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::Poll;

    use asupersync::{Budget, Cx};
    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::super::{ConfiguredServer, McpDiscovery, Provenance, TrustStore};
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Stage {
        Ready,
        Initialize,
        Initialized,
        Activate,
    }

    type StageHook = dyn Fn(Stage) + Send + Sync;

    struct State {
        pause: Stage,
        entered: AtomicBool,
        closed: AtomicBool,
        constructions: AtomicUsize,
        methods: Mutex<Vec<String>>,
        hook: Mutex<Option<Arc<StageHook>>>,
        fail_initialize: bool,
    }

    struct SetupTransport(Arc<State>);

    impl SetupTransport {
        async fn stage(&self, stage: Stage) {
            let hook = McpManager::lock(&self.0.hook).clone();
            if let Some(hook) = hook {
                hook(stage);
            }
            if self.0.pause == stage {
                self.0.entered.store(true, Ordering::Release);
                let budget = Cx::current().expect("setup owner").budget();
                poll_fn(|_| {
                    assert_eq!(Cx::current().expect("owner on every poll").budget(), budget);
                    Poll::<()>::Pending
                })
                .await;
            }
        }
    }

    #[async_trait]
    impl McpTransport for SetupTransport {
        async fn request(&self, method: &str, _params: Value, _timeout: Duration) -> Result<Value> {
            McpManager::lock(&self.0.methods).push(method.to_string());
            match method {
                "initialize" => {
                    self.stage(Stage::Initialize).await;
                    if self.0.fail_initialize {
                        return Err(tool_err("MCP_SERVER_ERROR", "fixture initialize failure"));
                    }
                    Ok(json!({
                        "protocolVersion": super::super::super::transport::MCP_PROTOCOL_VERSION,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "setup-fixture", "version": "1"}
                    }))
                }
                "tools/list" => Ok(json!({"tools": []})),
                "tools/call" => Ok(json!({"content": [{"type": "text", "text": "done"}]})),
                _ => Err(tool_err("MCP_PROTOCOL", "unexpected fixture request")),
            }
        }

        async fn notify(&self, method: &str, _params: Value) -> Result<()> {
            assert_eq!(method, "notifications/initialized");
            McpManager::lock(&self.0.methods).push(method.to_string());
            self.stage(Stage::Initialized).await;
            Ok(())
        }

        async fn activate(self: Arc<Self>) -> Result<()> {
            McpManager::lock(&self.0.methods).push("activate".to_string());
            self.stage(Stage::Activate).await;
            Ok(())
        }

        fn is_alive(&self) -> bool {
            !self.0.closed.load(Ordering::Acquire)
        }

        fn abort(&self) {
            self.0.closed.store(true, Ordering::Release);
        }

        async fn close(&self) {
            self.abort();
        }

        fn diagnostics_tail(&self) -> String {
            String::new()
        }
    }

    fn fixture(
        temp: &tempfile::TempDir,
        pause: Stage,
        fail_initialize: bool,
    ) -> (McpManager, Arc<ServerEntry>, Arc<State>) {
        let config = ConfiguredServer {
            name: "setup".to_string(),
            command: None,
            args: Vec::new(),
            env: Vec::new(),
            url: Some("https://setup.invalid/mcp".to_string()),
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
        let entry = manager.entry("setup").expect("fixture entry");
        TrustStore::load(&manager.inner.trust_path)
            .expect("trust store")
            .acknowledge("setup", &manager.trust_fingerprint_for(&entry), "operator")
            .expect("trust fixture");
        let state = Arc::new(State {
            pause,
            entered: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            constructions: AtomicUsize::new(0),
            methods: Mutex::new(Vec::new()),
            hook: Mutex::new(None),
            fail_initialize,
        });
        let factory_state = Arc::clone(&state);
        *McpManager::lock(&manager.inner.transport_factory) = Some(Arc::new(move || {
            factory_state.constructions.fetch_add(1, Ordering::AcqRel);
            Box::new(SetupTransport(Arc::clone(&factory_state))) as Box<dyn McpTransport>
        }));
        (manager, entry, state)
    }

    fn runtime() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::new()
            .worker_threads(1)
            .blocking_threads(1, 2)
            .build()
            .expect("runtime")
    }

    // Even a regression in manager deadlines must not hang this test suite.
    async fn bounded<F: Future>(future: F) -> F::Output {
        let owner = AgentCx::for_current_or_request();
        let time = owner.time();
        let deadline = Box::pin(time.sleep(Duration::from_secs(5)));
        match futures::future::select(Box::pin(future), deadline).await {
            futures::future::Either::Left((result, _)) => result,
            futures::future::Either::Right(((), pending)) => {
                drop(pending);
                panic!("MCP setup exceeded the test watchdog");
            }
        }
    }

    async fn entered<F: Future>(future: &mut std::pin::Pin<Box<F>>, state: &State) {
        poll_fn(|task| {
            assert!(future.as_mut().poll(task).is_pending());
            if state.entered.load(Ordering::Acquire) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    #[test]
    fn cold_tool_calls_cancel_in_every_handshake_stage_without_dispatching_tools() {
        for stage in [Stage::Initialize, Stage::Initialized, Stage::Activate] {
            let temp = tempfile::tempdir().expect("tempdir");
            let (manager, entry, state) = fixture(&temp, stage, false);
            let runtime = runtime();
            let owner = AgentCx::from_cx(
                runtime.request_cx_with_budget(Budget::new().with_poll_quota(1000)),
            );
            runtime.block_on(bounded(async {
                let parent = Cx::current().expect("parent");
                let mut call = Box::pin(manager.call_tool("setup", "echo", json!({})));
                owner.with_current(entered(&mut call, &state)).await;
                assert!(futures::poll!(call.as_mut()).is_pending());
                owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel setup"));
                let error = call.await.expect_err("cancelled cold call");
                assert!(
                    error.to_string().contains("MCP_CANCELLED"),
                    "{stage:?}: {error}"
                );
                assert!(!parent.is_cancel_requested());
                assert_eq!(Cx::current().unwrap().budget(), parent.budget());
            }));
            assert!(state.closed.load(Ordering::Acquire));
            assert!(McpManager::lock(&entry.transport).is_none());
            assert_eq!(McpManager::lock(&entry.restarts).count, 1);
            assert!(
                !McpManager::lock(&state.methods)
                    .iter()
                    .any(|m| m == "tools/call")
            );
            assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_ok());
        }
    }

    #[test]
    fn manager_deadline_bounds_each_uncooperative_handshake_stage() {
        for stage in [Stage::Initialize, Stage::Initialized, Stage::Activate] {
            let temp = tempfile::tempdir().expect("tempdir");
            let (manager, entry, state) = fixture(&temp, stage, false);
            let error = runtime()
                .block_on(bounded(manager.ensure_ready_owned(
                    &entry,
                    Duration::from_millis(500),
                    true,
                )))
                .expect_err("bounded setup");
            assert!(error.to_string().contains("MCP_TIMEOUT"), "{error}");
            assert!(
                state.entered.load(Ordering::Acquire),
                "fixture reached {stage:?}"
            );
            assert!(state.closed.load(Ordering::Acquire));
            assert!(McpManager::lock(&entry.transport).is_none());
            assert_eq!(McpManager::lock(&entry.restarts).count, 1);
            assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_ok());
        }
    }

    #[test]
    fn lane_wait_is_bounded_without_poisoning_the_lane_owner() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, state) = fixture(&temp, Stage::Ready, false);
        let lane = Arc::clone(&entry.connect_lane)
            .try_lock_owned()
            .expect("hold lane");
        let error = runtime()
            .block_on(bounded(manager.ensure_ready_owned(
                &entry,
                Duration::from_millis(20),
                true,
            )))
            .expect_err("bounded lane wait");
        assert!(error.to_string().contains("MCP_TIMEOUT"));
        assert_eq!(state.constructions.load(Ordering::Acquire), 0);
        assert!(!state.closed.load(Ordering::Acquire));
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
        drop(lane);
    }

    #[test]
    fn dropping_setup_aborts_private_transport_before_releasing_lane() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, state) = fixture(&temp, Stage::Activate, false);
        runtime().block_on(bounded(async {
            let mut setup = Box::pin(manager.ensure_ready(&entry));
            entered(&mut setup, &state).await;
            assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_err());
            drop(setup);
            assert!(state.closed.load(Ordering::Acquire));
            assert!(McpManager::lock(&entry.transport).is_none());
            assert_eq!(McpManager::lock(&entry.restarts).count, 0);
            assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_ok());
        }));
    }

    #[test]
    fn activation_success_racing_cancellation_cannot_leave_a_published_transport() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, state) = fixture(&temp, Stage::Ready, false);
        let runtime = runtime();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        let cancelling = owner.clone();
        *McpManager::lock(&state.hook) = Some(Arc::new(move |stage| {
            if stage == Stage::Activate {
                cancelling
                    .cancel_with(asupersync::types::CancelKind::User, Some("activation race"));
            }
        }));
        let error = runtime
            .block_on(bounded(owner.with_current(manager.call_tool(
                "setup",
                "echo",
                json!({}),
            ))))
            .expect_err("cancelled activation");
        assert!(error.to_string().contains("MCP_CANCELLED"));
        assert!(state.closed.load(Ordering::Acquire));
        assert!(McpManager::lock(&entry.transport).is_none());
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
        assert!(
            !McpManager::lock(&state.methods)
                .iter()
                .any(|m| m == "tools/call")
        );
    }

    #[test]
    fn stale_setup_abandonment_preserves_the_replacement_generation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, old) = fixture(&temp, Stage::Activate, false);
        let other_temp = tempfile::tempdir().expect("replacement tempdir");
        let (_other_manager, _, replacement_state) = fixture(&other_temp, Stage::Ready, false);
        let replacement: Arc<dyn McpTransport> =
            Arc::new(SetupTransport(Arc::clone(&replacement_state)));
        runtime().block_on(bounded(async {
            let mut setup = Box::pin(manager.ensure_ready(&entry));
            entered(&mut setup, &old).await;
            *McpManager::lock(&entry.transport) = Some(Arc::clone(&replacement));
            *McpManager::lock(&entry.health) = ServerHealth::Ready { tools: 7 };
            drop(setup);
            assert!(old.closed.load(Ordering::Acquire));
            assert!(!replacement_state.closed.load(Ordering::Acquire));
            assert_eq!(McpManager::lock(&entry.restarts).count, 0);
            assert!(
                McpManager::lock(&entry.transport)
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &replacement))
            );
            assert!(matches!(
                *McpManager::lock(&entry.health),
                ServerHealth::Ready { tools: 7 }
            ));
        }));
    }

    #[test]
    fn successful_connection_is_reused_without_duplicate_tool_execution() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, state) = fixture(&temp, Stage::Ready, false);
        runtime().block_on(bounded(async {
            for _ in 0..2 {
                let result = manager
                    .call_tool("setup", "echo", json!({}))
                    .await
                    .expect("tool");
                assert_eq!(result["content"][0]["text"], "done");
            }
        }));
        assert_eq!(state.constructions.load(Ordering::Acquire), 1);
        assert_eq!(
            *McpManager::lock(&state.methods),
            [
                "initialize",
                "notifications/initialized",
                "activate",
                "tools/call",
                "tools/call",
            ]
        );
        assert!(!state.closed.load(Ordering::Acquire));
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
    }

    #[test]
    fn returned_handshake_failure_is_counted_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, state) = fixture(&temp, Stage::Ready, true);
        let error = runtime()
            .block_on(bounded(manager.ensure_ready(&entry)))
            .expect_err("handshake failure");
        assert!(error.to_string().contains("fixture initialize failure"));
        assert!(state.closed.load(Ordering::Acquire));
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
    }

    #[test]
    fn outer_startup_timeout_charges_an_abandoned_private_handshake_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, state) = fixture(&temp, Stage::Activate, false);
        runtime().block_on(bounded(
            manager.connect_trusted_with_budget(Duration::from_millis(500)),
        ));
        assert!(state.entered.load(Ordering::Acquire));
        assert!(state.closed.load(Ordering::Acquire));
        assert_eq!(McpManager::lock(&entry.restarts).count, 1);
        assert!(matches!(
            &*McpManager::lock(&entry.health),
            ServerHealth::Unhealthy { reason, .. } if reason.contains("MCP_STARTUP_TIMEOUT")
        ));
    }

    #[test]
    fn denied_or_cancelled_owner_cannot_begin_connection_setup() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, state) = fixture(&temp, Stage::Ready, false);
        let cancelled = AgentCx::for_request();
        cancelled.cancel_with(asupersync::types::CancelKind::User, Some("before setup"));
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
                .block_on(bounded(owner.with_current(manager.ensure_ready(&entry))))
                .expect_err("setup denied");
            assert!(error.to_string().contains(code));
        }
        assert_eq!(state.constructions.load(Ordering::Acquire), 0);
        assert_eq!(McpManager::lock(&entry.restarts).count, 0);
    }

    #[test]
    fn transport_spawn_requirements_cover_stdio_and_command_secrets() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (_manager, entry, _) = fixture(&temp, Stage::Ready, false);
        let mut config = entry.config.clone();
        assert!(!transport_needs_spawn(&config));
        for (value, expected) in [
            ("literal-token", false),
            ("$ENV:PI_TEST_TOKEN", false),
            ("prefix $CMD:literal", false),
            ("$CMD:credential-helper", true),
            ("  $CMD:credential-helper  ", true),
        ] {
            config.headers = vec![("Authorization".to_string(), value.to_string())];
            assert_eq!(transport_needs_spawn(&config), expected);
            config.headers.clear();
            config.env = vec![("TOKEN".to_string(), value.to_string())];
            // HTTP never resolves the stdio environment. Do not demand
            // process authority for a field this transport will not use.
            assert!(!transport_needs_spawn(&config));
            config.env.clear();
        }
        config.command = Some("server".to_string());
        assert!(transport_needs_spawn(&config));
    }

    #[test]
    fn blocking_constructor_runs_under_the_captured_owner() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, state) = fixture(&temp, Stage::Ready, false);
        let runtime = runtime();
        let budget = Budget::new().with_poll_quota(4321);
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(budget));
        *McpManager::lock(&manager.inner.transport_factory) = Some(Arc::new(move || {
            assert_eq!(
                Cx::current().expect("blocking worker owner").budget(),
                budget
            );
            Box::new(SetupTransport(Arc::clone(&state))) as Box<dyn McpTransport>
        }));
        runtime
            .block_on(bounded(owner.with_current(manager.ensure_ready(&entry))))
            .expect("owned construction");
    }

    struct ReleaseWorker(Arc<(Mutex<bool>, std::sync::Condvar)>);

    impl Drop for ReleaseWorker {
        fn drop(&mut self) {
            *McpManager::lock(&self.0.0) = true;
            self.0.1.notify_all();
        }
    }

    #[test]
    fn cancelled_blocking_construction_cannot_handshake_or_publish_when_it_returns() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (manager, entry, state) = fixture(&temp, Stage::Ready, false);
        let runtime = runtime();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        let started = Arc::new(AtomicBool::new(false));
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        // Declared after the runtime: even the outer watchdog's panic releases
        // the worker before runtime shutdown can wait for blocking work.
        let release = ReleaseWorker(Arc::clone(&gate));
        let worker_gate = Arc::clone(&gate);
        let worker_started = Arc::clone(&started);
        let worker_state = Arc::clone(&state);
        *McpManager::lock(&manager.inner.transport_factory) = Some(Arc::new(move || {
            worker_started.store(true, Ordering::Release);
            let (released, wake) = &*worker_gate;
            let mut released = McpManager::lock(released);
            while !*released {
                released = wake
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            drop(released);
            Box::new(SetupTransport(Arc::clone(&worker_state))) as Box<dyn McpTransport>
        }));
        runtime.block_on(bounded(async {
            let parent = AgentCx::for_current_or_request();
            let mut setup = Box::pin(owner.with_current(manager.ensure_ready(&entry)));
            loop {
                assert!(futures::poll!(setup.as_mut()).is_pending());
                if started.load(Ordering::Acquire) {
                    break;
                }
                parent.time().sleep(Duration::from_millis(1)).await;
            }
            owner.cancel_with(
                asupersync::types::CancelKind::User,
                Some("cancel constructor"),
            );
            let result = setup.await;
            drop(release);
            let error = result.expect_err("constructor caller cancelled");
            assert!(error.to_string().contains("MCP_CANCELLED"));
            while !state.closed.load(Ordering::Acquire) {
                parent.time().sleep(Duration::from_millis(1)).await;
            }
            assert!(McpManager::lock(&state.methods).is_empty());
            assert!(McpManager::lock(&entry.transport).is_none());
            assert!(Arc::clone(&entry.connect_lane).try_lock_owned().is_ok());
        }));
    }

    struct DropProbe {
        budget: Budget,
        dropped: Arc<AtomicBool>,
    }

    impl Future for DropProbe {
        type Output = ();

        fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<()> {
            assert_eq!(Cx::current().expect("poll owner").budget(), self.budget);
            Poll::Pending
        }
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            assert_eq!(Cx::current().expect("drop owner").budget(), self.budget);
            self.dropped.store(true, Ordering::Release);
        }
    }

    #[test]
    fn abandoning_an_operation_restores_its_owner_for_the_future_destructor() {
        let runtime = runtime();
        let budget = Budget::new().with_poll_quota(8765);
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(budget));
        let dropped = Arc::new(AtomicBool::new(false));
        runtime.block_on(async {
            let parent = Cx::current().expect("caller");
            let probe = DropProbe {
                budget,
                dropped: Arc::clone(&dropped),
            };
            let mut operation = Box::pin(catalog::operation_with_owner(
                &owner,
                probe,
                Duration::from_secs(1),
            ));
            assert!(futures::poll!(operation.as_mut()).is_pending());
            assert_eq!(Cx::current().unwrap().budget(), parent.budget());
            drop(operation);
            assert!(dropped.load(Ordering::Acquire));
            assert_eq!(Cx::current().unwrap().budget(), parent.budget());
        });
    }
}
