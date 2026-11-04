// Integration tests are separate crates, so src/lib.rs's `recursion_limit`
// does not reach here; with the `wasm-host` feature, proving `Send` for the
// extension-loading futures exceeds the default 128.
#![recursion_limit = "256"]
#![allow(clippy::redundant_clone)]
//! Tests for extension event hook cancellation semantics (bd-2p8).
//!
//! Tests cover:
//! - `dispatch_cancellable_event` with `{cancelled: true}`, `{cancel: true}`, and `false`
//! - Session lifecycle before-hooks: switch, fork, compact
//! - Session lifecycle after-events fire-and-forget
//! - Cancellation payloads are forwarded to hooks
//! - No cancellation when no hooks registered

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::{Value, json};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_js_extension(harness: &common::TestHarness, source: &str) -> ExtensionManager {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_entry_path = harness.create_file("extensions/ext.mjs", source.as_bytes());
    let spec = JsExtensionLoadSpec::from_entry_path(&ext_entry_path).expect("load spec");

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start js runtime")
        }
    });
    manager.set_js_runtime(runtime);

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load extension");
        }
    });

    manager
}

// ---------------------------------------------------------------------------
// Extension sources
// ---------------------------------------------------------------------------

/// Extension that cancels `session_before_switch` via `{cancelled: true}`,
/// `session_before_fork` via `{cancel: true}`, and `session_before_compact` via `false`.
const SESSION_CANCEL_EXT: &str = r#"
export default function init(pi) {
    pi.on("session_before_switch", (event, ctx) => {
        return { cancelled: true, reason: "Extension vetoed switch" };
    });

    pi.on("session_before_fork", (event, ctx) => {
        return { cancel: true };
    });

    pi.on("session_before_compact", (event, ctx) => {
        return false;
    });

    pi.on("session_switch", (event, ctx) => {
        return null;
    });

    pi.on("session_fork", (event, ctx) => {
        return null;
    });

    pi.on("session_compact", (event, ctx) => {
        return null;
    });
}
"#;

/// Extension that does NOT cancel any session lifecycle events.
const SESSION_ALLOW_EXT: &str = r#"
export default function init(pi) {
    pi.on("session_before_switch", (event, ctx) => {
        return { cancelled: false };
    });

    pi.on("session_before_fork", (event, ctx) => {
        return null;
    });

    pi.on("session_before_compact", (event, ctx) => {
        return true;
    });
}
"#;

/// Extension with NO event hooks.
const NO_HOOKS_EXT: &str = r#"
export default function init(pi) {
    pi.registerCommand("noop", {
        description: "No-op command",
        handler: async () => null
    });
}
"#;

// ---------------------------------------------------------------------------
// Tests: cancellation via {cancelled: true}
// ---------------------------------------------------------------------------

#[test]
fn session_before_switch_cancelled_via_cancelled_object() {
    let harness = common::TestHarness::new("session_before_switch_cancelled_via_cancelled_object");
    let manager = load_js_extension(&harness, SESSION_CANCEL_EXT);

    let cancelled = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_cancellable_event(
                    ExtensionEventName::SessionBeforeSwitch,
                    Some(json!({
                        "reason": "resume",
                        "targetSessionFile": "/tmp/test-session.jsonl",
                    })),
                    5000,
                )
                .await
                .expect("dispatch cancellable")
        }
    });

    assert!(
        cancelled,
        "Expected cancellation when handler returns {{cancelled: true}}"
    );
}

// ---------------------------------------------------------------------------
// Tests: cancellation via {cancel: true}
// ---------------------------------------------------------------------------

#[test]
fn session_before_fork_cancelled_via_cancel_object() {
    let harness = common::TestHarness::new("session_before_fork_cancelled_via_cancel_object");
    let manager = load_js_extension(&harness, SESSION_CANCEL_EXT);

    let cancelled = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_cancellable_event(
                    ExtensionEventName::SessionBeforeFork,
                    Some(json!({
                        "entryId": "entry-123",
                        "summary": "test fork",
                    })),
                    5000,
                )
                .await
                .expect("dispatch cancellable")
        }
    });

    assert!(
        cancelled,
        "Expected cancellation when handler returns {{cancel: true}}"
    );
}

// ---------------------------------------------------------------------------
// Tests: cancellation via returning false
// ---------------------------------------------------------------------------

#[test]
fn session_before_compact_cancelled_via_false() {
    let harness = common::TestHarness::new("session_before_compact_cancelled_via_false");
    let manager = load_js_extension(&harness, SESSION_CANCEL_EXT);

    let cancelled = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_cancellable_event(
                    ExtensionEventName::SessionBeforeCompact,
                    Some(json!({ "sessionId": "session-abc" })),
                    5000,
                )
                .await
                .expect("dispatch cancellable")
        }
    });

    assert!(
        cancelled,
        "Expected cancellation when handler returns false"
    );
}

// ---------------------------------------------------------------------------
// Tests: no cancellation when hook allows the action
// ---------------------------------------------------------------------------

#[test]
fn session_before_switch_not_cancelled_when_allowed() {
    let harness = common::TestHarness::new("session_before_switch_not_cancelled_when_allowed");
    let manager = load_js_extension(&harness, SESSION_ALLOW_EXT);

    let cancelled = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_cancellable_event(
                    ExtensionEventName::SessionBeforeSwitch,
                    Some(json!({ "targetSessionFile": "/tmp/test.jsonl" })),
                    5000,
                )
                .await
                .expect("dispatch")
        }
    });

    assert!(
        !cancelled,
        "Should not cancel when handler returns {{cancelled: false}}"
    );
}

#[test]
fn session_before_fork_not_cancelled_when_null() {
    let harness = common::TestHarness::new("session_before_fork_not_cancelled_when_null");
    let manager = load_js_extension(&harness, SESSION_ALLOW_EXT);

    let cancelled = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_cancellable_event(
                    ExtensionEventName::SessionBeforeFork,
                    Some(json!({ "entryId": "e1" })),
                    5000,
                )
                .await
                .expect("dispatch")
        }
    });

    assert!(!cancelled, "Should not cancel when handler returns null");
}

#[test]
fn session_before_compact_not_cancelled_when_true() {
    let harness = common::TestHarness::new("session_before_compact_not_cancelled_when_true");
    let manager = load_js_extension(&harness, SESSION_ALLOW_EXT);

    let cancelled = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_cancellable_event(
                    ExtensionEventName::SessionBeforeCompact,
                    Some(json!({ "sessionId": "s1" })),
                    5000,
                )
                .await
                .expect("dispatch")
        }
    });

    assert!(!cancelled, "Should not cancel when handler returns true");
}

// ---------------------------------------------------------------------------
// Tests: after-events (fire-and-forget)
// ---------------------------------------------------------------------------

#[test]
fn session_after_events_dispatch_successfully() {
    let harness = common::TestHarness::new("session_after_events_dispatch_successfully");
    let manager = load_js_extension(&harness, SESSION_CANCEL_EXT);

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event(
                    ExtensionEventName::SessionSwitch,
                    Some(json!({
                        "reason": "resume",
                        "targetSessionFile": "/tmp/target.jsonl",
                        "sessionId": "new-session-id",
                    })),
                )
                .await
                .expect("session_switch after-event");

            manager
                .dispatch_event(
                    ExtensionEventName::SessionFork,
                    Some(json!({
                        "entryId": "e1",
                        "summary": "forked here",
                        "newSessionId": "fork-session-id",
                    })),
                )
                .await
                .expect("session_fork after-event");

            manager
                .dispatch_event(
                    ExtensionEventName::SessionCompact,
                    Some(json!({
                        "tokensBefore": 50000,
                        "firstKeptEntryId": "entry-42",
                    })),
                )
                .await
                .expect("session_compact after-event");
        }
    });
}

// ---------------------------------------------------------------------------
// Tests: payload forwarding
// ---------------------------------------------------------------------------

#[test]
fn session_before_hook_receives_payload() {
    let harness = common::TestHarness::new("session_before_hook_receives_payload");

    let echo_ext = r#"
export default function init(pi) {
    pi.on("session_before_switch", (event, ctx) => {
        return { received: event, cancelled: false };
    });
}
"#;
    let manager = load_js_extension(&harness, echo_ext);

    let response: Option<Value> = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::SessionBeforeSwitch,
                    Some(json!({
                        "reason": "new_session",
                        "targetSessionFile": "/sessions/new.jsonl",
                    })),
                    5000,
                )
                .await
                .expect("dispatch with response")
        }
    });

    let response = response.expect("should get a response");
    let received = response
        .get("received")
        .expect("should have received field");
    assert_eq!(
        received.get("reason").and_then(Value::as_str),
        Some("new_session")
    );
    assert_eq!(
        received.get("targetSessionFile").and_then(Value::as_str),
        Some("/sessions/new.jsonl")
    );
}

// ---------------------------------------------------------------------------
// Tests: no hooks registered
// ---------------------------------------------------------------------------

#[test]
fn no_cancellation_when_no_hooks_registered() {
    let harness = common::TestHarness::new("no_cancellation_when_no_hooks_registered");
    let manager = load_js_extension(&harness, NO_HOOKS_EXT);

    for event_name in [
        ExtensionEventName::SessionBeforeSwitch,
        ExtensionEventName::SessionBeforeFork,
        ExtensionEventName::SessionBeforeCompact,
    ] {
        let cancelled = common::run_async({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch_cancellable_event(event_name, None, 5000)
                    .await
                    .expect("dispatch cancellable")
            }
        });

        assert!(
            !cancelled,
            "Should not cancel when no hooks registered for {event_name:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests: ExtensionEvent serialization for session lifecycle types
// ---------------------------------------------------------------------------

#[test]
fn session_before_switch_event_serialization() {
    use pi::extension_events::ExtensionEvent;

    let event = ExtensionEvent::SessionBeforeSwitch {
        current_session: Some("session-1".to_string()),
        target_session: "session-2".to_string(),
    };

    assert_eq!(event.event_name(), "session_before_switch");

    let value = serde_json::to_value(&event).expect("serialize");
    assert_eq!(
        value.get("type").and_then(Value::as_str),
        Some("session_before_switch")
    );
    assert_eq!(
        value.get("currentSession").and_then(Value::as_str),
        Some("session-1")
    );
    assert_eq!(
        value.get("targetSession").and_then(Value::as_str),
        Some("session-2")
    );
}

#[test]
fn session_before_fork_event_serialization() {
    use pi::extension_events::ExtensionEvent;

    let event = ExtensionEvent::SessionBeforeFork {
        current_session: None,
        fork_entry_id: "entry-42".to_string(),
    };

    assert_eq!(event.event_name(), "session_before_fork");

    let value = serde_json::to_value(&event).expect("serialize");
    assert_eq!(
        value.get("type").and_then(Value::as_str),
        Some("session_before_fork")
    );
    assert!(value.get("currentSession").unwrap().is_null());
    assert_eq!(
        value.get("forkEntryId").and_then(Value::as_str),
        Some("entry-42")
    );
}

/// Request lifetime cancellation is separate from a hook vetoing a session
/// operation. These tests exercise the actor and real owner-isolated realms.
#[cfg(test)]
mod runtime_lifetime {
    use asupersync::runtime::RuntimeBuilder;
    use asupersync::time::{timeout, wall_now};
    use futures::channel::oneshot;
    use futures::future::{Either, select};
    use pi::error::{Error, Result};
    use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
    use pi::extensions_js::{PiJsRuntimeConfig, PiJsRuntimeLimits};
    use pi::tools::{Tool, ToolOutput, ToolRegistry, ToolUpdate};
    use serde_json::{Value, json};
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    const COMPLETED_OWNER: &str = r#"
        export default function(pi) {
            let starts = 0;
            pi.on('session_start', () => { starts++; });
            pi.registerTool({
                name: 'completed_probe', description: 'Completed event owner',
                parameters: { type: 'object' },
                execute() { return { content: [], details: { starts } }; }
            });
        }
    "#;

    const ACTIVE_OWNER: &str = r#"
        export default function(pi) {
            let starts = 0;
            pi.registerTool({
                name: 'active_probe', description: 'Active event owner',
                parameters: { type: 'object' },
                execute(_callId, input) {
                    if (input.poisonSetup) {
                        const has = Map.prototype.has;
                        Map.prototype.has = function(key) {
                            if (typeof key === 'string' && key.startsWith('event-batch-context')) {
                                while (true) {}
                            }
                            return has.call(this, key);
                        };
                    }
                    return { content: [], details: { starts } };
                }
            });
            pi.on('session_start', async event => {
                starts++;
                if (event.mode === 'cpu_sync') { while (true) {} }
                if (event.mode === 'poison_cleanup') {
                    const pending = pi.tool('read', { mode: 'blocked' });
                    // Batch cleanup uses this prototype in the bridge. A
                    // quarantined realm must never be re-entered to run it.
                    Map.prototype.delete = () => { while (true) {} };
                    return pending;
                }
                await pi.tool('read', { mode: event.mode });
                if (event.mode === 'pending') return new Promise(() => {});
                if (event.mode === 'cpu') { while (true) {} }
                if (event.mode === 'poison_successful_cleanup') {
                    const remove = Map.prototype.delete;
                    Map.prototype.delete = function(key) {
                        if (typeof key === 'string' && key.startsWith('event-batch-context')) {
                            while (true) {}
                        }
                        return remove.call(this, key);
                    };
                }
                if (event.mode === 'deferred') {
                    pi.registerTool({
                        name: 'deferred_probe', description: 'Registered during startup',
                        parameters: { type: 'object' },
                        execute() {
                            return { content: [], details: { registeredAfterHostcall: true } };
                        }
                    });
                }
                return null;
            });
        }
    "#;

    const UNTOUCHED_PEER: &str = r#"
        export default function(pi) {
            let starts = 0;
            let queued = 0;
            pi.on('session_start', () => { starts++; });
            pi.on('session_switch', () => { queued++; });
            pi.registerTool({
                name: 'peer_probe', description: 'Unaffected event owner',
                parameters: { type: 'object' },
                execute() { return { content: [], details: { starts, queued } }; }
            });
        }
    "#;

    #[derive(Default)]
    struct NativeCalls {
        entries: AtomicUsize,
        completions: AtomicUsize,
        drops: AtomicUsize,
    }

    struct NativeCallGuard(Arc<NativeCalls>);

    impl Drop for NativeCallGuard {
        fn drop(&mut self) {
            self.0.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct EventGate {
        started: Mutex<Option<oneshot::Sender<()>>>,
        release: Mutex<Option<oneshot::Receiver<()>>>,
        calls: Arc<NativeCalls>,
    }

    #[async_trait::async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Tool for EventGate {
        fn name(&self) -> &str {
            "read"
        }

        fn label(&self) -> &str {
            "Event admission gate"
        }

        fn description(&self) -> &str {
            "Observe and hold a real native dispatch issued by an event hook"
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object", "properties": { "mode": { "type": "string" } } })
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            input: Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> Result<ToolOutput> {
            let _guard = NativeCallGuard(Arc::clone(&self.calls));
            self.calls.entries.fetch_add(1, Ordering::SeqCst);
            let started = self
                .started
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .ok_or_else(|| Error::tool("read", "event gate entered more than once"))?;
            let _ = started.send(());
            match input["mode"].as_str() {
                Some("blocked") => std::future::pending::<()>().await,
                Some("gate" | "deferred") => {
                    let release = self
                        .release
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .take()
                        .ok_or_else(|| Error::tool("read", "event gate already consumed"))?;
                    release
                        .await
                        .map_err(|_| Error::tool("read", "event gate release abandoned"))?;
                }
                _ => {}
            }
            self.calls.completions.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput {
                content: Vec::new(),
                details: None,
                is_error: false,
            })
        }
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        manager: ExtensionManager,
        runtime: JsExtensionRuntimeHandle,
        started: Option<oneshot::Receiver<()>>,
        release: Option<oneshot::Sender<()>>,
        calls: Arc<NativeCalls>,
    }

    async fn fixture() -> Fixture {
        let directory = tempfile::tempdir().expect("event fixture directory");
        let completed = directory.path().join("completed.mjs");
        let active = directory.path().join("active.mjs");
        let peer = directory.path().join("peer.mjs");
        std::fs::write(&completed, COMPLETED_OWNER).expect("completed owner source");
        std::fs::write(&active, ACTIVE_OWNER).expect("active owner source");
        std::fs::write(&peer, UNTOUCHED_PEER).expect("peer source");
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let calls = Arc::new(NativeCalls::default());
        let tools = Arc::new(ToolRegistry::from_tools(vec![Box::new(EventGate {
            started: Mutex::new(Some(started_tx)),
            release: Mutex::new(Some(release_rx)),
            calls: Arc::clone(&calls),
        })]));
        let manager = ExtensionManager::new();
        let runtime = JsExtensionRuntimeHandle::start(
            PiJsRuntimeConfig {
                cwd: directory.path().display().to_string(),
                limits: PiJsRuntimeLimits {
                    // CPU tests must exercise the request's deadline fence,
                    // without help from a global instruction-count limit.
                    interrupt_budget: None,
                    ..Default::default()
                },
                ..Default::default()
            },
            tools,
            manager.clone(),
        )
        .await
        .expect("event runtime");
        manager.set_js_runtime(runtime.clone());
        manager
            .load_js_extensions(vec![
                JsExtensionLoadSpec::from_entry_path(completed).expect("completed spec"),
                JsExtensionLoadSpec::from_entry_path(active).expect("active spec"),
                JsExtensionLoadSpec::from_entry_path(peer).expect("peer spec"),
            ])
            .await
            .expect("three independent event owners");
        Fixture {
            _directory: directory,
            manager,
            runtime,
            started: Some(started_rx),
            release: Some(release_tx),
            calls,
        }
    }

    async fn after_native_entry<F: Future + Unpin>(
        started: oneshot::Receiver<()>,
        dispatch: F,
    ) -> F {
        let first = timeout(
            wall_now(),
            Duration::from_secs(5),
            select(started, dispatch),
        )
        .await
        .expect("event must enter its native hostcall promptly");
        let pending = match first {
            Either::Left((started, pending)) => {
                started.expect("native hostcall admission");
                Some(pending)
            }
            Either::Right(_) => None,
        };
        pending.expect("event must remain in flight until its gate or cancellation resolves")
    }

    async fn probe(runtime: &JsExtensionRuntimeHandle, name: &str) -> Result<Value> {
        timeout(
            wall_now(),
            Duration::from_secs(5),
            runtime.execute_tool(
                name.to_string(),
                format!("{name}-call"),
                json!({}),
                Arc::new(json!({})),
                2_000,
            ),
        )
        .await
        .expect("a follow-up request must not wait for the abandoned 60-second event")
    }

    async fn assert_only_active_owner_quarantined(fixture: &Fixture) {
        let completed = probe(&fixture.runtime, "completed_probe")
            .await
            .expect("a completed owner must remain callable");
        let peer = probe(&fixture.runtime, "peer_probe")
            .await
            .expect("the untouched peer must remain callable");
        let active = probe(&fixture.runtime, "active_probe")
            .await
            .expect_err("the unfinished owner must be quarantined");
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) owner-isolation invariant.
        assert_eq!(completed["details"]["starts"], 1);
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) owner-isolation invariant.
        assert_eq!(peer["details"], json!({ "starts": 0, "queued": 0 }));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) quarantine regression invariant.
        assert!(active.to_string().contains("quarantin"), "{active}");
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) teardown regression invariant.
        assert!(fixture.manager.shutdown(Duration::from_secs(2)).await);
    }

    #[test]
    fn dropping_a_pending_event_quarantines_only_its_unfinished_owner() {
        let executor = RuntimeBuilder::current_thread().build().expect("runtime");
        executor.block_on(async {
            let mut fixture = fixture().await;
            let dispatch = Box::pin(fixture.runtime.dispatch_event(
                "session_start".to_string(),
                json!({ "mode": "pending" }),
                Arc::new(json!({})),
                60_000,
            ));
            let pending =
                after_native_entry(fixture.started.take().expect("admission"), dispatch).await;
            drop(pending);
            assert_only_active_owner_quarantined(&fixture).await;
        });
    }

    #[test]
    fn blocked_native_hostcall_cannot_hide_event_cancellation() {
        let executor = RuntimeBuilder::current_thread().build().expect("runtime");
        executor.block_on(async {
            let mut fixture = fixture().await;
            let dispatch = Box::pin(fixture.runtime.dispatch_event(
                "session_start".to_string(),
                json!({ "mode": "blocked" }),
                Arc::new(json!({})),
                60_000,
            ));
            let pending =
                after_native_entry(fixture.started.take().expect("admission"), dispatch).await;
            drop(pending);
            // Take the counters before shutdown: shutdown itself must not be
            // responsible for releasing the abandoned dispatch's native work.
            let peer = probe(&fixture.runtime, "peer_probe")
                .await
                .expect("blocked host work cannot pin the actor after caller drop");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) owner-isolation invariant.
            assert_eq!(peer["details"]["starts"], 0);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) single native-admission invariant.
            assert_eq!(fixture.calls.entries.load(Ordering::SeqCst), 1);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) cancellation must not fake completion.
            assert_eq!(fixture.calls.completions.load(Ordering::SeqCst), 0);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) retained dispatch must be dropped once.
            assert_eq!(fixture.calls.drops.load(Ordering::SeqCst), 1);
            assert_only_active_owner_quarantined(&fixture).await;
        });
    }

    #[test]
    fn cpu_bound_event_obeys_its_explicit_deadline_and_keeps_peers_healthy() {
        let executor = RuntimeBuilder::current_thread().build().expect("runtime");
        executor.block_on(async {
            let mut fixture = fixture().await;
            let dispatch = Box::pin(fixture.runtime.dispatch_event(
                "session_start".to_string(),
                json!({ "mode": "cpu" }),
                Arc::new(json!({})),
                500,
            ));
            let pending =
                after_native_entry(fixture.started.take().expect("admission"), dispatch).await;
            let failure = timeout(wall_now(), Duration::from_secs(5), pending)
                .await
                .expect("CPU-bound JavaScript must not defeat the request deadline")
                .expect_err("CPU-bound event must time out or be interrupted");
            let message = failure.to_string();
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) explicit deadline regression invariant.
            assert!(
                message.contains("timed out") || message.contains("interrupt"),
                "{message}"
            );
            assert_only_active_owner_quarantined(&fixture).await;
        });
    }

    #[test]
    fn synchronous_cpu_event_is_interrupted_before_it_can_return_a_promise() {
        let executor = RuntimeBuilder::current_thread().build().expect("runtime");
        executor.block_on(async {
            let fixture = fixture().await;
            timeout(
                wall_now(),
                Duration::from_secs(5),
                fixture.runtime.dispatch_event(
                    "session_start".to_string(),
                    json!({ "mode": "cpu_sync" }),
                    Arc::new(json!({})),
                    500,
                ),
            )
            .await
            .expect("the initial JS call must be covered by the deadline fence")
            .expect_err("synchronous CPU work must not complete successfully");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) synchronous execution must precede hostcall entry.
            assert_eq!(fixture.calls.entries.load(Ordering::SeqCst), 0);
            assert_only_active_owner_quarantined(&fixture).await;
        });
    }

    #[test]
    fn an_event_expiring_in_the_actor_queue_never_invokes_its_hook() {
        let executor = RuntimeBuilder::current_thread().build().expect("runtime");
        executor.block_on(async {
            let mut fixture = fixture().await;
            let dispatch = Box::pin(fixture.runtime.dispatch_event(
                "session_start".to_string(),
                json!({ "mode": "gate" }),
                Arc::new(json!({})),
                60_000,
            ));
            let active =
                after_native_entry(fixture.started.take().expect("admission"), dispatch).await;
            let queued = Box::pin(fixture.runtime.dispatch_event(
                "session_switch".to_string(),
                json!({}),
                Arc::new(json!({})),
                100,
            ));
            let first = timeout(wall_now(), Duration::from_secs(5), select(queued, active))
                .await
                .expect("queued request must use its own deadline");
            let active = match first {
                Either::Left((expired, active)) => {
                    expired.expect_err("queued request must expire before the actor is released");
                    Some(active)
                }
                Either::Right(_) => None,
            }
            .expect("gated event must remain active while the second request expires");
            fixture
                .release
                .take()
                .expect("release sender")
                .send(())
                .expect("release gated hostcall");
            timeout(wall_now(), Duration::from_secs(5), active)
                .await
                .expect("first event must finish after its gate opens")
                .expect("successful first event");
            let peer = probe(&fixture.runtime, "peer_probe")
                .await
                .expect("peer remains healthy");
            let active = probe(&fixture.runtime, "active_probe")
                .await
                .expect("successful owner remains healthy");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) queued event must never execute.
            assert_eq!(peer["details"], json!({ "starts": 1, "queued": 0 }));
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) queued expiry must not poison active work.
            assert_eq!(active["details"]["starts"], 1);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) teardown regression invariant.
            assert!(fixture.manager.shutdown(Duration::from_secs(2)).await);
        });
    }

    #[test]
    fn successful_async_startup_publishes_deferred_tools_and_preserves_owner_health() {
        let executor = RuntimeBuilder::current_thread().build().expect("runtime");
        executor.block_on(async {
            let mut fixture = fixture().await;
            let dispatch = Box::pin(fixture.runtime.dispatch_event(
                "session_start".to_string(),
                json!({ "mode": "deferred" }),
                Arc::new(json!({})),
                60_000,
            ));
            let active =
                after_native_entry(fixture.started.take().expect("admission"), dispatch).await;
            fixture
                .release
                .take()
                .expect("release sender")
                .send(())
                .expect("release startup hostcall");
            timeout(wall_now(), Duration::from_secs(5), active)
                .await
                .expect("startup must complete after native work settles")
                .expect("successful startup event");
            let deferred = probe(&fixture.runtime, "deferred_probe")
                .await
                .expect("newly registered tool must be routed to its owner");
            let peer = probe(&fixture.runtime, "peer_probe")
                .await
                .expect("later owner executed normally");
            let active = probe(&fixture.runtime, "active_probe")
                .await
                .expect("successful owner stays healthy");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) deferred registration regression invariant.
            assert_eq!(deferred["details"]["registeredAfterHostcall"], true);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) successful event ordering invariant.
            assert_eq!(peer["details"]["starts"], 1);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) successful event owner invariant.
            assert_eq!(active["details"]["starts"], 1);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) teardown regression invariant.
            assert!(fixture.manager.shutdown(Duration::from_secs(2)).await);
        });
    }

    #[test]
    fn dropping_an_event_batch_stops_later_events_and_only_quarantines_the_active_owner() {
        let executor = RuntimeBuilder::current_thread().build().expect("runtime");
        executor.block_on(async {
            let mut fixture = fixture().await;
            let dispatch = Box::pin(fixture.runtime.dispatch_event_batch(
                vec![
                    ("session_start".to_string(), json!({ "mode": "blocked" })),
                    ("session_switch".to_string(), json!({})),
                ],
                Arc::new(json!({})),
                60_000,
            ));
            let pending =
                after_native_entry(fixture.started.take().expect("admission"), dispatch).await;
            drop(pending);
            let peer = probe(&fixture.runtime, "peer_probe")
                .await
                .expect("batch abandonment must release the actor");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) batch cancellation must stop future owners/events.
            assert_eq!(peer["details"], json!({ "starts": 0, "queued": 0 }));
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) batch cancellation drops retained host work.
            assert_eq!(fixture.calls.drops.load(Ordering::SeqCst), 1);
            assert_only_active_owner_quarantined(&fixture).await;
        });
    }

    #[test]
    fn cancelled_batch_cleanup_never_reenters_a_quarantined_realm() {
        let executor = RuntimeBuilder::current_thread().build().expect("runtime");
        executor.block_on(async {
            let mut fixture = fixture().await;
            let dispatch = Box::pin(fixture.runtime.dispatch_event_batch(
                vec![(
                    "session_start".to_string(),
                    json!({ "mode": "poison_cleanup" }),
                )],
                Arc::new(json!({})),
                60_000,
            ));
            let pending =
                after_native_entry(fixture.started.take().expect("admission"), dispatch).await;
            drop(pending);
            let peer = probe(&fixture.runtime, "peer_probe")
                .await
                .expect("poisoned cleanup in the cancelled realm must not block a healthy peer");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) cancelled owner must not advance later hooks.
            assert_eq!(peer["details"], json!({ "starts": 0, "queued": 0 }));
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) native cancellation precedes cleanup.
            assert_eq!(fixture.calls.drops.load(Ordering::SeqCst), 1);
            assert_only_active_owner_quarantined(&fixture).await;
        });
    }

    #[test]
    fn successful_batch_cleanup_bounds_guest_execution_without_poisoning_later_peers() {
        let executor = RuntimeBuilder::current_thread().build().expect("runtime");
        executor.block_on(async {
            let fixture = fixture().await;
            timeout(
                wall_now(),
                Duration::from_secs(5),
                fixture.runtime.dispatch_event_batch(
                    vec![(
                        "session_start".to_string(),
                        json!({ "mode": "poison_successful_cleanup" }),
                    )],
                    Arc::new(json!({})),
                    5_000,
                ),
            )
            .await
            .expect("batch cleanup must have its own bounded guest-execution fence")
            .expect_err("the owner with nonterminating cleanup must be quarantined");
            let completed = probe(&fixture.runtime, "completed_probe")
                .await
                .expect("already cleaned owner stays healthy");
            let peer = probe(&fixture.runtime, "peer_probe")
                .await
                .expect("a later healthy owner must receive its own cleanup budget");
            let failed = probe(&fixture.runtime, "active_probe")
                .await
                .expect_err("only the poisoned cleanup owner is quarantined");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) successful hooks precede cleanup failure.
            assert_eq!(completed["details"]["starts"], 1);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) later healthy owner remains independent.
            assert_eq!(peer["details"], json!({ "starts": 1, "queued": 0 }));
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) cleanup failure must quarantine its owner.
            assert!(failed.to_string().contains("quarantin"), "{failed}");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) cleanup failure follows successful native work.
            assert_eq!(fixture.calls.completions.load(Ordering::SeqCst), 1);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) teardown regression invariant.
            assert!(fixture.manager.shutdown(Duration::from_secs(2)).await);
        });
    }

    #[test]
    fn batch_context_setup_is_deadline_bound_before_any_event_hook_runs() {
        let executor = RuntimeBuilder::current_thread().build().expect("runtime");
        executor.block_on(async {
            let fixture = fixture().await;
            fixture
                .runtime
                .execute_tool(
                    "active_probe".to_string(),
                    "install-setup-probe".to_string(),
                    json!({ "poisonSetup": true }),
                    Arc::new(json!({})),
                    2_000,
                )
                .await
                .expect("install setup-specific prototype override without poisoning other work");
            timeout(
                wall_now(),
                Duration::from_secs(5),
                fixture.runtime.dispatch_event_batch(
                    vec![("session_start".to_string(), json!({}))],
                    Arc::new(json!({})),
                    500,
                ),
            )
            .await
            .expect("batch context setup must obey the original request deadline")
            .expect_err("nonterminating context setup must fail");
            let completed = probe(&fixture.runtime, "completed_probe")
                .await
                .expect("previously created context must be cleaned without losing its owner");
            let peer = probe(&fixture.runtime, "peer_probe")
                .await
                .expect("a realm not entered during failed setup stays callable");
            let failed = probe(&fixture.runtime, "active_probe")
                .await
                .expect_err("the setup offender must be quarantined");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) setup failure precedes all event hooks.
            assert_eq!(completed["details"]["starts"], 0);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) failed setup cannot invoke peer hooks.
            assert_eq!(peer["details"], json!({ "starts": 0, "queued": 0 }));
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) setup failure must quarantine its owner.
            assert!(failed.to_string().contains("quarantin"), "{failed}");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) setup failure cannot admit hook hostcalls.
            assert_eq!(fixture.calls.entries.load(Ordering::SeqCst), 0);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) teardown regression invariant.
            assert!(fixture.manager.shutdown(Duration::from_secs(2)).await);
        });
    }
}
