//! Extension tools integration.
//!
//! This module provides adapters that allow JavaScript extension-registered tools to be used as
//! normal Rust `Tool` implementations inside the agent tool registry.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, Weak};
#[cfg(feature = "wasm-host")]
use std::time::Duration;

use crate::error::{Error, Result};
#[cfg(test)]
use crate::extensions::JsExtensionRuntimeHandle;
#[cfg(feature = "wasm-host")]
use crate::extensions::WasmExtensionHandle;
use crate::extensions::{ExtensionManager, ExtensionRuntimeHandle, ExtensionToolRegistration};
use crate::extensions_js::ExtensionToolDef;
use crate::tools::{SharedToolRegistry, Tool, ToolOutput, ToolRegistry, ToolUpdate};
#[cfg(feature = "wasm-host")]
use asupersync::time::{timeout, wall_now};

const DEFAULT_EXTENSION_TOOL_TIMEOUT_MS: u64 = 60_000;

/// Wraps a JS extension-registered tool so it can be used as a Rust [`Tool`].
///
/// Note: This wrapper uses [`ExtensionRuntimeHandle`] rather than
/// [`crate::extensions_js::PiJsRuntime`] so it remains `Send + Sync` and can be
/// stored in the shared tool registry.
pub struct ExtensionToolWrapper {
    def: ExtensionToolDef,
    registration: Option<Arc<ExtensionToolRegistration>>,
    runtime: ExtensionRuntimeHandle,
    ctx_payload: Arc<Value>,
    timeout_ms: u64,
}

impl ExtensionToolWrapper {
    #[must_use]
    pub fn new<R>(def: ExtensionToolDef, runtime: R) -> Self
    where
        R: Into<ExtensionRuntimeHandle>,
    {
        Self {
            def,
            registration: None,
            runtime: runtime.into(),
            ctx_payload: Arc::new(Value::Object(serde_json::Map::new())),
            timeout_ms: DEFAULT_EXTENSION_TOOL_TIMEOUT_MS,
        }
    }

    fn from_registration(
        registration: ExtensionToolRegistration,
        runtime: ExtensionRuntimeHandle,
    ) -> Self {
        let mut wrapper = Self::new(registration.definition.clone(), runtime);
        wrapper.registration = Some(Arc::new(registration));
        wrapper
    }

    #[must_use]
    pub fn with_ctx_payload(mut self, ctx_payload: Value) -> Self {
        self.ctx_payload = Arc::new(ctx_payload);
        self
    }

    #[must_use]
    pub fn with_ctx_payload_shared(mut self, ctx_payload: Arc<Value>) -> Self {
        self.ctx_payload = ctx_payload;
        self
    }

    #[must_use]
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms.max(1);
        self
    }
}

#[cfg(feature = "wasm-host")]
pub struct WasmExtensionToolWrapper {
    def: ExtensionToolDef,
    handle: WasmExtensionHandle,
    timeout_ms: u64,
}

#[cfg(feature = "wasm-host")]
impl WasmExtensionToolWrapper {
    #[must_use]
    pub const fn new(def: ExtensionToolDef, handle: WasmExtensionHandle) -> Self {
        Self {
            def,
            handle,
            timeout_ms: DEFAULT_EXTENSION_TOOL_TIMEOUT_MS,
        }
    }

    #[must_use]
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms.max(1);
        self
    }
}

/// Collect all registered extension tools and wrap them as Rust [`Tool`]s.
///
/// This is intended to be called after extensions are loaded/activated so the returned tool list
/// can be injected into the agent [`crate::tools::ToolRegistry`].
pub async fn collect_extension_tool_wrappers(
    manager: &ExtensionManager,
    ctx_payload: Value,
) -> Result<Vec<Box<dyn Tool>>> {
    let active = manager
        .active_tools()
        .map(|tools| tools.into_iter().collect::<HashSet<_>>());
    let mut wrappers =
        collect_all_extension_tool_wrappers(manager, manager.runtime(), ctx_payload).await?;
    if let Some(active) = active {
        wrappers.retain(|tool| active.contains(tool.name()));
    }
    Ok(wrappers)
}

/// Agent-owned publication state for deferred extension tool registration.
/// Exact Arc identities distinguish our mounts from later same-name host
/// overrides without adding a manager/runtime ownership cycle.
pub(crate) struct ExtensionToolCatalog {
    ctx_payload: Value,
    published: Vec<Arc<dyn Tool>>,
    registrations: Vec<ExtensionToolRegistration>,
    active: Option<Vec<String>>,
    published_snapshot: Option<Weak<ToolRegistry>>,
    initialized: bool,
}

impl ExtensionToolCatalog {
    pub(crate) const fn new(ctx_payload: Value) -> Self {
        Self {
            ctx_payload,
            published: Vec::new(),
            registrations: Vec::new(),
            active: None,
            published_snapshot: None,
            initialized: false,
        }
    }

    pub(crate) async fn refresh(
        &mut self,
        manager: &ExtensionManager,
        tools: &SharedToolRegistry,
    ) -> Result<()> {
        // Collection asks the runtime for its authoritative, owner-validated
        // snapshot. Errors leave the published registry untouched.
        let runtime = manager.runtime_for_tools();
        let registrations = match &runtime {
            Some(runtime) => runtime.get_tool_registrations().await?,
            None => Vec::new(),
        };
        let active = manager.active_tools();
        // WASM instances already bind directly to their executable owner;
        // their catalog retains the existing refresh path. JS/native tokens
        // let unchanged loops retain every wrapper and the shared snapshot.
        if !cfg!(feature = "wasm-host")
            && self.initialized
            && self.registrations == registrations
            && self.active == active
            && self
                .published_snapshot
                .as_ref()
                .is_some_and(|previous| Weak::ptr_eq(previous, &Arc::downgrade(&tools.snapshot())))
        {
            return Ok(());
        }
        let replacements = wrap_extension_tool_registrations(
            manager,
            runtime,
            self.ctx_payload.clone(),
            registrations.clone(),
        );
        let (published, published_snapshot) = tools.reconcile_extension_tools(
            &self.published,
            replacements,
            active.as_deref(),
            !self.initialized,
        );
        self.published = published;
        self.registrations = registrations;
        self.active = active;
        self.published_snapshot = Some(published_snapshot);
        self.initialized = true;
        Ok(())
    }

    pub(crate) fn clear(&mut self, tools: &SharedToolRegistry) {
        self.published = tools
            .reconcile_extension_tools(&self.published, Vec::new(), None, false)
            .0;
        self.registrations.clear();
        self.active = None;
        self.published_snapshot = None;
        self.initialized = false;
    }
}

async fn collect_all_extension_tool_wrappers(
    manager: &ExtensionManager,
    runtime: Option<ExtensionRuntimeHandle>,
    ctx_payload: Value,
) -> Result<Vec<Box<dyn Tool>>> {
    let registrations = match &runtime {
        Some(runtime) => runtime.get_tool_registrations().await?,
        None => Vec::new(),
    };
    Ok(wrap_extension_tool_registrations(
        manager,
        runtime,
        ctx_payload,
        registrations,
    ))
}

fn wrap_extension_tool_registrations(
    manager: &ExtensionManager,
    runtime: Option<ExtensionRuntimeHandle>,
    ctx_payload: Value,
    mut registrations: Vec<ExtensionToolRegistration>,
) -> Vec<Box<dyn Tool>> {
    #[cfg(not(feature = "wasm-host"))]
    let _ = manager;
    let shared_ctx_payload = Arc::new(ctx_payload);

    let mut wrappers: Vec<Box<dyn Tool>> = Vec::new();
    let mut seen = HashSet::new();

    if let Some(runtime) = runtime {
        registrations.sort_by(|a, b| a.definition.name.cmp(&b.definition.name));
        for registration in registrations {
            if !seen.insert(registration.definition.name.clone()) {
                tracing::warn!(tool = %registration.definition.name, "Duplicate extension tool name; ignoring");
                continue;
            }

            wrappers.push(Box::new(
                ExtensionToolWrapper::from_registration(registration, runtime.clone())
                    .with_ctx_payload_shared(Arc::clone(&shared_ctx_payload)),
            ));
        }
    }

    #[cfg(feature = "wasm-host")]
    {
        let mut wasm_defs: Vec<(ExtensionToolDef, WasmExtensionHandle)> = Vec::new();
        for handle in manager.wasm_extensions() {
            for def in handle.tool_defs() {
                wasm_defs.push((def.clone(), handle.clone()));
            }
        }

        wasm_defs.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        for (def, handle) in wasm_defs {
            if !seen.insert(def.name.clone()) {
                tracing::warn!(tool = %def.name, "Duplicate extension tool name; ignoring");
                continue;
            }

            wrappers.push(Box::new(WasmExtensionToolWrapper::new(def, handle)));
        }
    }

    wrappers
}

#[async_trait]
impl Tool for ExtensionToolWrapper {
    fn name(&self) -> &str {
        &self.def.name
    }

    fn label(&self) -> &str {
        self.def.label.as_deref().unwrap_or(&self.def.name)
    }

    fn description(&self) -> &str {
        &self.def.description
    }

    fn parameters(&self) -> Value {
        self.def.parameters.clone()
    }

    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Extension
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        input: Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let result = self
            .runtime
            .execute_tool_with_registration_ref(
                &self.def.name,
                tool_call_id,
                input,
                Arc::clone(&self.ctx_payload),
                self.timeout_ms,
                on_update,
                self.registration.clone(),
            )
            .await
            .map_err(|err| Error::tool(self.name(), err.to_string()))?;

        serde_json::from_value(result).map_err(|err| {
            Error::tool(
                self.name(),
                format!("Invalid extension tool output (expected ToolOutput JSON): {err}"),
            )
        })
    }
}

#[cfg(feature = "wasm-host")]
#[async_trait]
impl Tool for WasmExtensionToolWrapper {
    fn name(&self) -> &str {
        &self.def.name
    }

    fn label(&self) -> &str {
        self.def.label.as_deref().unwrap_or(&self.def.name)
    }

    fn description(&self) -> &str {
        &self.def.description
    }

    fn parameters(&self) -> Value {
        self.def.parameters.clone()
    }

    fn origin(&self) -> crate::tools::ToolOrigin {
        crate::tools::ToolOrigin::Extension
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let fut = self.handle.handle_tool(&self.def.name, &input);
        let output_json = if self.timeout_ms > 0 {
            match timeout(
                wall_now(),
                Duration::from_millis(self.timeout_ms),
                Box::pin(fut),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    return Err(Error::tool(
                        self.name(),
                        format!(
                            "WASM tool '{}' timed out after {}ms",
                            self.name(),
                            self.timeout_ms
                        ),
                    ));
                }
            }
        } else {
            fut.await
        }
        .map_err(|err| Error::tool(self.name(), err.to_string()))?;

        serde_json::from_str(&output_json).map_err(|err| {
            Error::tool(
                self.name(),
                format!("Invalid WASM tool output (expected ToolOutput JSON): {err}"),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::agent::{Agent, AgentConfig, AgentEvent, AgentSession};
    use crate::extensions::{
        ExtensionManager, JsExtensionLoadSpec, NativeRustExtensionLoadSpec,
        NativeRustExtensionRuntimeHandle,
    };
    use crate::extensions_js::{PiJsRuntimeConfig, PiJsRuntimeLimits};
    use crate::model::{
        AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ToolCall,
        Usage,
    };
    use crate::provider::{Context, Provider, StreamOptions};
    use crate::session::Session;
    use crate::tools::ToolRegistry;
    use asupersync::runtime::RuntimeBuilder;
    use asupersync::sync::Mutex;
    use async_trait::async_trait;
    use futures::Stream;
    use serde_json::json;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex, PoisonError};
    use std::time::Duration;

    async fn setup_js_tool(
        source: &str,
        tool_name: &str,
    ) -> (
        tempfile::TempDir,
        ExtensionManager,
        JsExtensionRuntimeHandle,
        ExtensionToolDef,
    ) {
        setup_js_tool_with_native_tools(source, tool_name, Vec::new()).await
    }

    async fn setup_js_tool_with_native_tools(
        source: &str,
        tool_name: &str,
        native_tools: Vec<Box<dyn Tool>>,
    ) -> (
        tempfile::TempDir,
        ExtensionManager,
        JsExtensionRuntimeHandle,
        ExtensionToolDef,
    ) {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let entry_path = temp_dir.path().join("ext.mjs");
        std::fs::write(&entry_path, source).expect("write extension entry");

        let manager = ExtensionManager::new();
        let mut tools = ToolRegistry::new(&[], temp_dir.path(), None);
        for tool in native_tools {
            tools.push(tool);
        }
        let tools = Arc::new(tools);
        let js_runtime = JsExtensionRuntimeHandle::start(
            PiJsRuntimeConfig {
                cwd: temp_dir.path().display().to_string(),
                ..Default::default()
            },
            Arc::clone(&tools),
            manager.clone(),
        )
        .await
        .expect("start js runtime");
        manager.set_js_runtime(js_runtime.clone());

        let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("spec");
        manager
            .load_js_extensions(vec![spec])
            .await
            .expect("load js extensions");

        let def = js_runtime
            .get_registered_tools()
            .await
            .expect("get registered tools")
            .into_iter()
            .find(|tool| tool.name == tool_name)
            .expect("tool registered");

        (temp_dir, manager, js_runtime, def)
    }

    const TOOL_INVOCATION_FIXTURE: &str = r"
        export default function init(pi) {
          let abortCount = 0;
          let savedUpdate;
          const output = (text, details = {}) => ({
            content: [{ type: 'text', text }], details
          });
          pi.registerTool({
            name: 'invocation_tool', description: 'Invocation lifecycle fixture',
            parameters: { type: 'object', properties: { mode: { type: 'string' } } },
            async execute(callId, input, signal, onUpdate) {
              if (!(signal instanceof AbortSignal) || signal.aborted) {
                throw new Error('tool must receive its own live AbortSignal');
              }
              const update = typeof onUpdate === 'function' ? onUpdate : () => {};
              savedUpdate = update;
              if (input.mode === 'complete') {
                if (typeof onUpdate !== 'function') throw new Error('missing update callback');
                update(output('started', { phase: 'started', callId }));
                // The native read fixture cannot return until the update
                // callback releases it. Buffering updates until completion
                // therefore deadlocks and fails the outer test timeout.
                await pi.tool('read', {});
                return output('finished', { phase: 'finished', callId });
              }
              const pending = new Promise((resolve) => {
                signal.addEventListener('abort', () => {
                  abortCount++;
                  try { update(output('late abort update')); } catch (_) {}
                  if (input.mode !== 'ignore') resolve(output('cancelled'));
                }, { once: true });
              });
              update(output('started', { phase: 'started', callId }));
              return pending;
            }
          });
          pi.registerTool({
            name: 'probe_tool', description: 'Observe the prior invocation',
            parameters: { type: 'object' },
            async execute(_callId, _input, signal, onUpdate) {
              if (savedUpdate) {
                try { savedUpdate(output('stale update')); } catch (_) {}
              }
              if (typeof onUpdate === 'function') {
                onUpdate(output('probe progress', { phase: 'probe' }));
              }
              return output('probe', {
                abortCount, freshSignal: signal instanceof AbortSignal && !signal.aborted
              });
            }
          });
        }
    ";

    struct ProgressGateTool {
        release: StdMutex<Option<futures::channel::oneshot::Receiver<()>>>,
    }

    #[async_trait]
    impl Tool for ProgressGateTool {
        fn name(&self) -> &str {
            "read"
        }

        fn label(&self) -> &str {
            "Progress gate"
        }

        fn description(&self) -> &str {
            "Test fixture released by the native progress callback"
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object" })
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> Result<ToolOutput> {
            let release = self
                .release
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .ok_or_else(|| Error::tool("read", "progress gate entered more than once"))?;
            release
                .await
                .map_err(|_| Error::tool("read", "progress callback was abandoned"))?;
            Ok(ToolOutput {
                content: Vec::new(),
                details: None,
                is_error: false,
            })
        }
    }

    type TestUpdateCallback = Box<dyn Fn(ToolUpdate) + Send + Sync>;

    fn first_update_channel() -> (
        futures::channel::oneshot::Receiver<ToolUpdate>,
        TestUpdateCallback,
    ) {
        let (sender, receiver) = futures::channel::oneshot::channel();
        let sender = StdMutex::new(Some(sender));
        let callback = Box::new(move |update| {
            let sender = sender.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(sender) = sender {
                let _ = sender.send(update);
            }
        });
        (receiver, callback)
    }

    async fn abandon_started_tool(wrapper: &ExtensionToolWrapper, mode: &str) {
        let (update, callback) = first_update_channel();
        let execution = wrapper.execute("abandoned-call", json!({ "mode": mode }), Some(callback));
        let first = asupersync::time::timeout(
            asupersync::time::wall_now(),
            Duration::from_secs(5),
            futures::future::select(update, execution),
        )
        .await
        .expect("tool must deliver progress before its execution deadline");
        match first {
            futures::future::Either::Left((update, pending)) => {
                // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression assertion; absent from production builds.
                assert_eq!(
                    update
                        .expect("initial progress")
                        .details
                        .expect("progress details")["phase"],
                    "started",
                );
                drop(pending);
            }
            futures::future::Either::Right((result, _)) => {
                panic!("tool must stay pending until cancellation, got {result:?}");
            }
        }
    }

    async fn probe_invocation(runtime: &JsExtensionRuntimeHandle) -> Result<Value> {
        asupersync::time::timeout(
            asupersync::time::wall_now(),
            Duration::from_secs(5),
            runtime.execute_tool(
                "probe_tool".to_string(),
                "probe-call".to_string(),
                json!({}),
                Arc::new(json!({})),
                5_000,
            ),
        )
        .await
        .expect("the next request must not wait for the abandoned tool's 60-second budget")
    }

    #[test]
    fn extension_tool_updates_release_work_before_final_output_and_ignore_late_callbacks() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let (release, wait) = futures::channel::oneshot::channel();
            let gate = ProgressGateTool {
                release: StdMutex::new(Some(wait)),
            };
            let (_directory, _manager, js_runtime, def) = setup_js_tool_with_native_tools(
                TOOL_INVOCATION_FIXTURE,
                "invocation_tool",
                vec![Box::new(gate)],
            )
            .await;
            let wrapper = ExtensionToolWrapper::new(def, js_runtime.clone());
            let updates = Arc::new(StdMutex::new(Vec::new()));
            let observed = Arc::clone(&updates);
            let release = StdMutex::new(Some(release));
            let output = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_secs(5),
                wrapper.execute(
                    "progress-call",
                    json!({ "mode": "complete" }),
                    Some(Box::new(move |update| {
                        observed
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(update);
                        let release = release
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .take();
                        if let Some(release) = release {
                            release.send(()).expect("release the native tool");
                        }
                    })),
                ),
            )
            .await
            .expect("progress must be delivered while the JS tool waits for host work")
            .expect("completed extension tool");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression assertion; absent from production builds.
            assert_eq!(output.details.as_ref().unwrap()["phase"], "finished");
            {
                let updates = updates.lock().unwrap_or_else(PoisonError::into_inner);
                // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression assertion; absent from production builds.
                assert_eq!(updates.len(), 1);
                // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
                assert_eq!(updates[0].details.as_ref().unwrap()["phase"], "started");
                // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
                assert_eq!(
                    updates[0].details.as_ref().unwrap()["callId"],
                    "progress-call"
                );
            }

            let probe_updates = Arc::new(StdMutex::new(Vec::new()));
            let observed_probe = Arc::clone(&probe_updates);
            let probe = js_runtime
                .execute_tool_with_updates(
                    "probe_tool".to_string(),
                    // Reusing a model-supplied call ID must not reuse the
                    // previous invocation's signal or progress recipient.
                    "progress-call".to_string(),
                    json!({}),
                    Arc::new(json!({})),
                    5_000,
                    Some(Box::new(move |update| {
                        observed_probe
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(update);
                    })),
                )
                .await
                .expect("healthy runtime");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert_eq!(probe["details"]["abortCount"], 0);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert_eq!(probe["details"]["freshSignal"], true);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert_eq!(
                updates.lock().unwrap_or_else(PoisonError::into_inner).len(),
                1
            );
            let probe_updates = probe_updates.lock().unwrap_or_else(PoisonError::into_inner);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert_eq!(probe_updates.len(), 1);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert_eq!(probe_updates[0].details.as_ref().unwrap()["phase"], "probe");
        });
    }

    #[test]
    fn extension_tool_caller_abandonment_aborts_only_its_invocation() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let (_directory, _manager, js_runtime, def) =
                setup_js_tool(TOOL_INVOCATION_FIXTURE, "invocation_tool").await;
            let wrapper = ExtensionToolWrapper::new(def, js_runtime.clone());
            abandon_started_tool(&wrapper, "wait").await;
            let probe = probe_invocation(&js_runtime)
                .await
                .expect("cooperative cancellation must retain a healthy realm");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert_eq!(probe["details"]["abortCount"], 1);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert_eq!(probe["details"]["freshSignal"], true);
        });
    }

    #[test]
    fn extension_tool_deadline_aborts_cooperative_work_without_quarantining_the_realm() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let (_directory, _manager, js_runtime, def) =
                setup_js_tool(TOOL_INVOCATION_FIXTURE, "invocation_tool").await;
            let wrapper = ExtensionToolWrapper::new(def, js_runtime.clone()).with_timeout_ms(500);
            let failure = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_secs(5),
                wrapper.execute("deadline-call", json!({ "mode": "wait" }), None),
            )
            .await
            .expect("execution deadline must be enforced")
            .expect_err("a completed abort cleanup must not become a successful tool result");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert!(failure.to_string().contains("timed out"), "{failure}");
            let probe = probe_invocation(&js_runtime)
                .await
                .expect("cooperative deadline cleanup must retain a healthy realm");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert_eq!(probe["details"]["abortCount"], 1);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert_eq!(probe["details"]["freshSignal"], true);
        });
    }

    #[test]
    fn extension_tool_noncooperative_cancellation_quarantines_its_realm() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let (_directory, _manager, js_runtime, def) =
                setup_js_tool(TOOL_INVOCATION_FIXTURE, "invocation_tool").await;
            let wrapper = ExtensionToolWrapper::new(def, js_runtime.clone());
            abandon_started_tool(&wrapper, "ignore").await;
            let error = probe_invocation(&js_runtime)
                .await
                .expect_err("a tool that ignores abort cannot keep its realm live indefinitely");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert!(error.to_string().contains("quarantin"), "{error}");
        });
    }

    #[test]
    fn extension_tool_cpu_deadline_quarantines_only_the_offending_realm() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let directory = tempfile::tempdir().expect("separate extension directories");
            let cpu_directory = directory.path().join("cpu");
            let healthy_directory = directory.path().join("healthy");
            std::fs::create_dir(&cpu_directory).expect("cpu directory");
            std::fs::create_dir(&healthy_directory).expect("healthy directory");
            let cpu_path = cpu_directory.join("index.mjs");
            let healthy_path = healthy_directory.join("index.mjs");
            std::fs::write(
                &cpu_path,
                r"
                export default function init(pi) {
                  pi.registerTool({
                    name: 'cpu_tool', description: 'Never yields to the event loop',
                    parameters: { type: 'object' },
                    execute() { while (true) {} }
                  });
                }
                ",
            )
            .expect("cpu extension");
            std::fs::write(
                &healthy_path,
                r"
                export default function init(pi) {
                  pi.registerTool({
                    name: 'healthy_tool', description: 'Independent extension',
                    parameters: { type: 'object' },
                    execute(_callId, _input, signal, onUpdate) {
                      return {
                        content: [{ type: 'text', text: 'healthy' }],
                        details: {
                          freshSignal: signal instanceof AbortSignal && !signal.aborted,
                          updatesOmitted: onUpdate === undefined
                        }
                      };
                    }
                  });
                }
                ",
            )
            .expect("healthy extension");
            let manager = ExtensionManager::new();
            let native_tools = Arc::new(ToolRegistry::new(&[], directory.path(), None));
            let js_runtime = JsExtensionRuntimeHandle::start(
                PiJsRuntimeConfig {
                    cwd: directory.path().display().to_string(),
                    limits: PiJsRuntimeLimits {
                        // This must be stopped by the per-invocation deadline,
                        // not a global instruction-count budget.
                        interrupt_budget: None,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                native_tools,
                manager.clone(),
            )
            .await
            .expect("start separate realms");
            manager.set_js_runtime(js_runtime.clone());
            manager
                .load_js_extensions(vec![
                    JsExtensionLoadSpec::from_entry_path(&cpu_path).expect("cpu spec"),
                    JsExtensionLoadSpec::from_entry_path(&healthy_path).expect("healthy spec"),
                ])
                .await
                .expect("load independent extensions");
            let def = js_runtime
                .get_registered_tools()
                .await
                .expect("registered tools")
                .into_iter()
                .find(|tool| tool.name == "cpu_tool")
                .expect("cpu tool");
            let cpu = ExtensionToolWrapper::new(def, js_runtime.clone()).with_timeout_ms(500);
            let failure = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_secs(5),
                cpu.execute("cpu-call", json!({}), None),
            )
            .await
            .expect("CPU-bound JavaScript must not prevent deadline enforcement")
            .expect_err("CPU-bound JavaScript must be interrupted");
            let failure = failure.to_string();
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert!(
                failure.contains("timed out") || failure.contains("interrupt"),
                "{failure}"
            );
            let healthy = js_runtime
                .execute_tool(
                    "healthy_tool".to_string(),
                    "healthy-call".to_string(),
                    json!({}),
                    Arc::new(json!({})),
                    5_000,
                )
                .await
                .expect("the interrupted realm must not poison an independent extension");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert_eq!(healthy["content"][0]["text"], "healthy");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert_eq!(healthy["details"]["freshSignal"], true);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert_eq!(healthy["details"]["updatesOmitted"], true);
            let quarantined = cpu
                .execute("cpu-retry", json!({}), None)
                .await
                .expect_err("an interrupted realm must remain quarantined");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert!(
                quarantined.to_string().contains("quarantin"),
                "{quarantined}"
            );
        });
    }

    #[test]
    fn native_descriptor_tools_keep_legacy_results_without_synthetic_updates() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let directory = tempfile::tempdir().expect("native descriptor directory");
            let path = directory.path().join("fixture.native.json");
            let expected = json!({
                "content": [{ "type": "text", "text": "native result" }],
                "details": { "runtime": "native-rust" }
            });
            std::fs::write(
                &path,
                json!({
                    "tools": [{
                        "name": "native_tool", "description": "Native result fixture",
                        "parameters": { "type": "object" }
                    }],
                    "toolOutputs": { "native_tool": expected }
                })
                .to_string(),
            )
            .expect("write native descriptor");
            let native = NativeRustExtensionRuntimeHandle::start()
                .await
                .expect("native runtime");
            let manager = ExtensionManager::new();
            manager.set_native_runtime(native.clone());
            manager
                .load_native_extensions(vec![
                    NativeRustExtensionLoadSpec::from_entry_path(&path).expect("native spec"),
                ])
                .await
                .expect("load native descriptor");
            let def = native
                .get_registered_tools()
                .await
                .expect("native tools")
                .pop()
                .expect("native tool");
            let handle = ExtensionRuntimeHandle::from(native);
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert_eq!(
                handle
                    .execute_tool_ref(
                        "native_tool",
                        "legacy",
                        json!({}),
                        Arc::new(json!({})),
                        5_000
                    )
                    .await
                    .expect("legacy native execution"),
                expected,
            );
            let count = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&count);
            let output = ExtensionToolWrapper::new(def, handle)
                .execute(
                    "with-updates",
                    json!({}),
                    Some(Box::new(move |_| {
                        observed.fetch_add(1, Ordering::SeqCst);
                    })),
                )
                .await
                .expect("native execution with an update callback");
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert_eq!(
                serde_json::to_value(output).expect("native result"),
                expected
            );
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert_eq!(count.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn extension_tool_wrapper_executes_registered_tool() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "hello_tool",
                    label: "hello_tool",
                    description: "test tool",
                    parameters: { type: "object", properties: { name: { type: "string" } } },
                    execute: async (_callId, input, _onUpdate, _abort, ctx) => {
                      const who = input && input.name ? String(input.name) : "world";
                      const cwd = ctx && ctx.cwd ? String(ctx.cwd) : "";
                      return {
                        content: [{ type: "text", text: `hello ${who}` }],
                        details: { from: "extension", cwd: cwd },
                        isError: false
                      };
                    }
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let manager = ExtensionManager::new();
            let tools = Arc::new(ToolRegistry::new(&[], temp_dir.path(), None));
            let js_runtime = JsExtensionRuntimeHandle::start(
                PiJsRuntimeConfig {
                    cwd: temp_dir.path().display().to_string(),
                    ..Default::default()
                },
                Arc::clone(&tools),
                manager.clone(),
            )
            .await
            .expect("start js runtime");
            manager.set_js_runtime(js_runtime.clone());

            let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("spec");
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load js extensions");

            let tool_defs = js_runtime
                .get_registered_tools()
                .await
                .expect("get registered tools");
            let def = tool_defs
                .into_iter()
                .find(|tool| tool.name == "hello_tool")
                .expect("hello_tool registered");

            let wrapper = ExtensionToolWrapper::new(def, js_runtime).with_ctx_payload(json!({
                "cwd": temp_dir.path().display().to_string()
            }));

            let output = wrapper
                .execute("call-1", json!({ "name": "pi" }), None)
                .await
                .expect("execute tool");

            assert!(!output.is_error);

            match output.content.as_slice() {
                [ContentBlock::Text(text)] => assert_eq!(text.text, "hello pi"),
                other => assert!(
                    matches!(other, [ContentBlock::Text(_)]),
                    "Expected single text content block, got: {other:?}"
                ),
            }

            let details = output.details.expect("details present");
            assert_eq!(
                details.get("from").and_then(Value::as_str),
                Some("extension")
            );
            let cwd = temp_dir.path().display().to_string();
            assert_eq!(
                details.get("cwd").and_then(Value::as_str),
                Some(cwd.as_str())
            );
        });
    }

    #[test]
    fn extension_tool_wrapper_metadata_and_timeout_clamp() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let source = r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "meta_tool",
                    label: "Meta Tool",
                    description: "metadata test tool",
                    parameters: { type: "object", properties: { x: { type: "number" } } },
                    execute: async (_callId, _input, _onUpdate, _abort, _ctx) => ({
                      content: [{ type: "text", text: "ok" }],
                      isError: false
                    })
                  });
                }
                "#;
            let (_temp_dir, _manager, js_runtime, def) = setup_js_tool(source, "meta_tool").await;

            let wrapper = ExtensionToolWrapper::new(def.clone(), js_runtime.clone())
                .with_timeout_ms(0)
                .with_ctx_payload(json!({"cwd": "/tmp"}));
            assert_eq!(wrapper.timeout_ms, 1);
            assert_eq!(wrapper.name(), "meta_tool");
            assert_eq!(wrapper.label(), "Meta Tool");
            assert_eq!(wrapper.description(), "metadata test tool");
            assert_eq!(
                wrapper.parameters(),
                json!({ "type": "object", "properties": { "x": { "type": "number" } } })
            );

            let mut no_label = def;
            no_label.label = None;
            let fallback = ExtensionToolWrapper::new(no_label, js_runtime).with_timeout_ms(25);
            assert_eq!(fallback.timeout_ms, 25);
            assert_eq!(fallback.label(), "meta_tool");
        });
    }

    #[test]
    fn extension_tool_wrapper_maps_invalid_output_to_tool_error() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let source = r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "broken_tool",
                    label: "broken_tool",
                    description: "returns invalid output payload",
                    parameters: { type: "object", properties: {} },
                    execute: async (_callId, _input, _onUpdate, _abort, _ctx) => ({
                      nope: true
                    })
                  });
                }
                "#;
            let (_temp_dir, _manager, js_runtime, def) = setup_js_tool(source, "broken_tool").await;

            let wrapper = ExtensionToolWrapper::new(def, js_runtime);
            let err = wrapper
                .execute("call-1", json!({}), None)
                .await
                .expect_err("invalid tool output should fail");

            match err {
                Error::Tool { tool, message } => {
                    assert_eq!(tool, "broken_tool");
                    assert!(message.contains("Invalid extension tool output"));
                }
                other => panic!(),
            }
        });
    }

    #[derive(Debug)]
    struct ToolCallingProvider;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for ToolCallingProvider {
        fn name(&self) -> &str {
            "test-provider"
        }

        fn api(&self) -> &str {
            "test-api"
        }

        fn model_id(&self) -> &str {
            "test-model"
        }

        async fn stream(
            &self,
            context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            fn assistant_message(content: Vec<ContentBlock>) -> AssistantMessage {
                AssistantMessage {
                    content,
                    api: "test-api".to_string(),
                    provider: "test-provider".to_string(),
                    model: "test-model".to_string(),
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    stop_details: None,
                    error_message: None,
                    timestamp: 0,
                }
            }

            let tool_def_present = context.tools.iter().any(|tool| tool.name == "hello_tool");
            let tool_result = context.messages.iter().find_map(|message| match message {
                Message::ToolResult(result) if result.tool_name == "hello_tool" => Some(result),
                _ => None,
            });

            if let Some(result) = tool_result {
                match result.content.as_slice() {
                    [ContentBlock::Text(text)] => assert_eq!(text.text, "hello pi"),
                    other => panic!(),
                }

                let events = vec![
                    Ok(StreamEvent::Start {
                        partial: assistant_message(Vec::new()),
                    }),
                    Ok(StreamEvent::Done {
                        reason: StopReason::Stop,
                        message: assistant_message(vec![ContentBlock::Text(TextContent::new(
                            "done",
                        ))]),
                    }),
                ];
                return Ok(Box::pin(futures::stream::iter(events)));
            }

            assert!(
                tool_def_present,
                "Expected extension tool to be present in provider tool defs"
            );

            let tool_call = ToolCall {
                id: "call-1".to_string(),
                name: "hello_tool".to_string(),
                arguments: json!({ "name": "pi" }),
                thought_signature: None,
            };

            let events = vec![
                Ok(StreamEvent::Start {
                    partial: assistant_message(Vec::new()),
                }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: assistant_message(vec![ContentBlock::ToolCall(tool_call)]),
                }),
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[test]
    fn agent_executes_extension_tool_registered_via_js() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "hello_tool",
                    label: "hello_tool",
                    description: "test tool",
                    parameters: { type: "object", properties: { name: { type: "string" } } },
                    execute: async (_callId, input, _onUpdate, _abort, _ctx) => {
                      const who = input && input.name ? String(input.name) : "world";
                      return {
                        content: [{ type: "text", text: `hello ${who}` }],
                        details: { from: "extension" },
                        isError: false
                      };
                    }
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let manager = ExtensionManager::new();
            let tools_for_runtime = Arc::new(ToolRegistry::new(&[], temp_dir.path(), None));
            let js_runtime = JsExtensionRuntimeHandle::start(
                PiJsRuntimeConfig {
                    cwd: temp_dir.path().display().to_string(),
                    ..Default::default()
                },
                Arc::clone(&tools_for_runtime),
                manager.clone(),
            )
            .await
            .expect("start js runtime");
            manager.set_js_runtime(js_runtime.clone());

            let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("spec");
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load js extensions");

            let wrappers = collect_extension_tool_wrappers(
                &manager,
                json!({ "cwd": temp_dir.path().display().to_string() }),
            )
            .await
            .expect("collect wrappers");
            assert_eq!(wrappers.len(), 1);

            let provider = Arc::new(ToolCallingProvider);
            let tools = ToolRegistry::new(&[], temp_dir.path(), None);
            let mut agent = Agent::new(provider, tools, AgentConfig::default());
            agent.extend_tools(wrappers);

            let session = Arc::new(Mutex::new(Session::in_memory()));
            let mut agent_session = AgentSession::new(
                agent,
                session,
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            let message = agent_session
                .run_text("hi".to_string(), |_event: AgentEvent| {})
                .await
                .expect("run_text");

            match message.content.as_slice() {
                [ContentBlock::Text(text)] => assert_eq!(text.text, "done"),
                other => panic!(),
            }
        });
    }

    /// Tool the agent mounts after the JS runtime has already booted.
    struct LateEchoTool;

    #[async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl crate::tools::Tool for LateEchoTool {
        fn name(&self) -> &str {
            "late_echo"
        }
        fn label(&self) -> &str {
            "late_echo"
        }
        fn description(&self) -> &str {
            "echoes its input; mounted after the extension runtime started"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({ "type": "object", "properties": { "text": { "type": "string" } } })
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            input: serde_json::Value,
            _on_update: Option<Box<dyn Fn(crate::tools::ToolUpdate) + Send + Sync>>,
        ) -> crate::error::Result<crate::tools::ToolOutput> {
            let text = input
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            Ok(crate::tools::ToolOutput {
                content: vec![crate::model::ContentBlock::Text(
                    crate::model::TextContent::new(format!("echo:{text}")),
                )],
                details: None,
                is_error: false,
            })
        }
    }

    /// Boot a JS runtime on a fresh shared registry and load `source`. The
    /// `tool` capability needs an approval prompt under the default policy,
    /// and tests cannot answer one; the permissive profile grants it, which
    /// is what the classic startup path does for trusted workspaces.
    async fn start_extension_on_shared_registry(
        source: &str,
    ) -> (
        tempfile::TempDir,
        ExtensionManager,
        JsExtensionRuntimeHandle,
        crate::tools::SharedToolRegistry,
    ) {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let entry_path = temp_dir.path().join("ext.mjs");
        std::fs::write(&entry_path, source).expect("write extension entry");
        let manager = ExtensionManager::new();
        let shared =
            crate::tools::SharedToolRegistry::new(ToolRegistry::new(&[], temp_dir.path(), None));
        let js_runtime = JsExtensionRuntimeHandle::start_with_policy(
            PiJsRuntimeConfig {
                cwd: temp_dir.path().display().to_string(),
                ..Default::default()
            },
            shared.clone(),
            manager.clone(),
            crate::extensions::ExtensionPolicy::from_profile(
                crate::extensions::PolicyProfile::Permissive,
            ),
        )
        .await
        .expect("start js runtime");
        manager.set_js_runtime(js_runtime.clone());
        let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("spec");
        manager
            .load_js_extensions(vec![spec])
            .await
            .expect("load js extensions");
        (temp_dir, manager, js_runtime, shared)
    }

    #[test]
    fn deferred_catalog_preserves_host_overrides_and_replacement_owners() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let (temp_dir, manager, _js_runtime, shared) = start_extension_on_shared_registry(
                r#"
                export default function(pi) {
                    for (const name of ["late_echo", "survivor"]) {
                        pi.registerTool({
                            name, description: "extension " + name,
                            parameters: { type: "object" },
                            execute: async () => ({
                                content: [{ type: "text", text: "extension:" + name }],
                            }),
                        });
                    }
                }
                "#,
            ).await;
            shared.update(|registry| registry.push(Box::new(LateEchoTool)));
            let ctx = json!({ "cwd": temp_dir.path().display().to_string() });
            manager.set_active_tools(vec!["survivor".to_string()]);
            let mut filtered = ExtensionToolCatalog::new(ctx.clone());
            filtered.refresh(&manager, &shared).await.unwrap();
            assert_eq!(shared.snapshot().get("late_echo").unwrap().origin(), crate::tools::ToolOrigin::BuiltIn);
            assert!(shared.snapshot().get("survivor").is_some());
            filtered.clear(&shared);
            manager.set_active_tools(vec!["late_echo".to_string(), "survivor".to_string()]);
            let mut catalog = ExtensionToolCatalog::new(ctx.clone());
            catalog.refresh(&manager, &shared).await.unwrap();
            let original = shared.snapshot();
            assert_eq!(original.get("late_echo").unwrap().origin(), crate::tools::ToolOrigin::Extension);
            let version = shared.version();
            catalog.refresh(&manager, &shared).await.unwrap();
            assert_eq!(shared.version(), version, "unchanged schemas retain the provider cache");
            #[cfg(not(feature = "wasm-host"))]
            assert!(Arc::ptr_eq(&original, &shared.snapshot()), "unchanged registrations retain the executable snapshot too");

            manager.set_active_tools(vec!["survivor".to_string()]);
            catalog.refresh(&manager, &shared).await.unwrap();
            assert!(shared.snapshot().get("late_echo").is_none());
            assert!(shared.snapshot().inactive_tools().iter().any(|tool| tool.name() == "late_echo"));

            // A later host mount can share a shelved registration's name.
            // Refresh must neither erase that host tool nor revive the old
            // inactive wrapper over it.
            shared.update(|registry| registry.push(Box::new(LateEchoTool)));
            catalog.refresh(&manager, &shared).await.unwrap();
            manager.set_active_tools(vec!["late_echo".to_string(), "survivor".to_string()]);
            catalog.refresh(&manager, &shared).await.unwrap();
            let snapshot = shared.snapshot();
            assert_eq!(snapshot.get("late_echo").unwrap().origin(), crate::tools::ToolOrigin::BuiltIn);
            assert!(!snapshot.inactive_tools().iter().any(|tool| tool.name() == "late_echo"));
            let output = snapshot.get("late_echo").unwrap()
                .execute("native-override", json!({ "text": "retained" }), None).await.unwrap();
            assert!(matches!(&output.content[0], ContentBlock::Text(text) if text.text == "echo:retained"));
            let old_output = original.get("late_echo").unwrap()
                .execute("retained-snapshot", json!({}), None).await.unwrap();
            assert!(matches!(&old_output.content[0], ContentBlock::Text(text) if text.text == "extension:late_echo"));

            // Reload publishes a fresh owner before the former one is
            // cleared. Both sets have Extension origin and identical names.
            let mut replacement = ExtensionToolCatalog::new(ctx);
            replacement.refresh(&manager, &shared).await.unwrap();
            catalog.clear(&shared);
            assert_eq!(shared.snapshot().get("late_echo").unwrap().origin(), crate::tools::ToolOrigin::Extension);
            assert!(shared.snapshot().get("survivor").is_some());
            shared.update(|registry| registry.push(Box::new(LateEchoTool)));
            replacement.clear(&shared);
            assert_eq!(shared.snapshot().tools().len(), 1);
            assert_eq!(shared.snapshot().get("late_echo").unwrap().origin(), crate::tools::ToolOrigin::BuiltIn);
            assert!(manager.shutdown(std::time::Duration::from_secs(5)).await);
        });
    }

    #[test]
    fn selected_tool_schema_cannot_invoke_a_replaced_handler_or_mutated_schema() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let (temp_dir, manager, js_runtime, shared) = start_extension_on_shared_registry(
                r#"
                export default function(pi) {
                    let calls = 0;
                    const parameters = { type: "object", properties: { value: { type: "string" } } };
                    const install = (label) => pi.registerTool({
                        name: "binding_probe", description: "stable description", parameters,
                        execute: async () => {
                            calls++;
                            return { content: [{ type: "text", text: label }] };
                        },
                    });
                    install("original");
                    pi.registerCommand("replace", { handler: async () => { install("replacement"); } });
                    pi.registerCommand("mutate", { handler: async () => { parameters.required = ["value"]; } });
                    pi.registerCommand("calls", { handler: async () => ({ calls }) });
                }
                "#,
            ).await;
            let mut catalog = ExtensionToolCatalog::new(json!({ "cwd": temp_dir.path() }));
            catalog.refresh(&manager, &shared).await.unwrap();
            let original = shared.snapshot();
            let first = original.get("binding_probe").unwrap()
                .execute("first", json!({}), None).await.unwrap();
            assert!(matches!(&first.content[0], ContentBlock::Text(text) if text.text == "original"));

            let ctx = Arc::new(json!({}));
            js_runtime.execute_command("replace".into(), String::new(), Arc::clone(&ctx), 5_000)
                .await.unwrap();
            let error = original.get("binding_probe").unwrap()
                .execute("stale-handler", json!({}), None).await.unwrap_err();
            assert!(error.to_string().contains("PI_EXTENSION_TOOL_STALE"), "{error}");
            assert_eq!(js_runtime.execute_command("calls".into(), String::new(), Arc::clone(&ctx), 5_000)
                .await.unwrap(), json!({"calls":1}), "the replacement handler must not run under its predecessor's schema");

            catalog.refresh(&manager, &shared).await.unwrap();
            let replacement = shared.snapshot();
            let second = replacement.get("binding_probe").unwrap()
                .execute("replacement", json!({}), None).await.unwrap();
            assert!(matches!(&second.content[0], ContentBlock::Text(text) if text.text == "replacement"));
            let error = original.get("binding_probe").unwrap()
                .execute("retained-after-refresh", json!({}), None).await.unwrap_err();
            assert!(error.to_string().contains("PI_EXTENSION_TOOL_STALE"));

            // Mutating the originally supplied parameters object changes the
            // executable schema without calling registerTool a second time.
            js_runtime.execute_command("mutate".into(), String::new(), Arc::clone(&ctx), 5_000)
                .await.unwrap();
            let error = replacement.get("binding_probe").unwrap()
                .execute("stale-schema", json!({"value":"valid for the new schema"}), None)
                .await.unwrap_err();
            assert!(error.to_string().contains("PI_EXTENSION_TOOL_STALE"), "{error}");
            catalog.refresh(&manager, &shared).await.unwrap();
            let fresh = shared.snapshot();
            assert_eq!(fresh.get("binding_probe").unwrap().parameters()["required"], json!(["value"]));
            fresh.get("binding_probe").unwrap()
                .execute("fresh-schema", json!({"value":"accepted"}), None).await.unwrap();
            assert_eq!(js_runtime.execute_command("calls".into(), String::new(), ctx, 5_000)
                .await.unwrap(), json!({"calls":3}));
            assert!(manager.shutdown(std::time::Duration::from_secs(5)).await);
        });
    }

    #[test]
    fn selected_tool_validates_its_published_schema_without_guest_accessors() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let (temp_dir, manager, _js_runtime, shared) = start_extension_on_shared_registry(
                r#"
                export default function(pi) {
                    const parameters = { type: "object", properties: {} };
                    Object.defineProperty(parameters, "required", {
                        get() { return ["never advertised"]; },
                    });
                    pi.registerTool({
                        name: "binding_probe", description: "projected schema", parameters,
                        execute: async () => ({ content: [{ type: "text", text: "admitted" }] }),
                    });
                }
                "#,
            )
            .await;
            let mut catalog = ExtensionToolCatalog::new(json!({ "cwd": temp_dir.path() }));
            catalog.refresh(&manager, &shared).await.unwrap();
            let selected = shared.snapshot();
            let tool = selected.get("binding_probe").unwrap();
            assert!(tool.parameters().get("required").is_none());
            let output = tool
                .execute("projected-input", json!({}), None)
                .await
                .unwrap();
            assert!(
                matches!(&output.content[0], ContentBlock::Text(text) if text.text == "admitted")
            );
            assert!(manager.shutdown(std::time::Duration::from_secs(5)).await);
        });
    }

    #[test]
    fn selected_tool_owner_cannot_survive_same_owner_or_cross_owner_cold_reload() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        runtime.block_on(async {
            let source = r#"
                export default function(pi) {
                    pi.registerTool({ name: "binding_probe", description: "same schema",
                        parameters: { type: "object" },
                        execute: async () => ({ content: [{ type: "text", text: "fresh" }] }),
                    });
                }
            "#;
            let (temp_dir, manager, _js_runtime, shared) =
                start_extension_on_shared_registry(source).await;
            let mut catalog = ExtensionToolCatalog::new(json!({ "cwd": temp_dir.path() }));
            catalog.refresh(&manager, &shared).await.unwrap();
            for filename in ["ext.mjs", "new-owner.mjs"] {
                let selected = shared.snapshot();
                let path = temp_dir.path().join(filename);
                std::fs::write(&path, source).unwrap();
                manager
                    .load_js_extensions(vec![JsExtensionLoadSpec::from_entry_path(&path).unwrap()])
                    .await
                    .unwrap();
                let error = selected
                    .get("binding_probe")
                    .unwrap()
                    .execute("retained-owner", json!({}), None)
                    .await
                    .unwrap_err();
                assert!(
                    error.to_string().contains("PI_EXTENSION_TOOL_STALE"),
                    "{filename}: {error}"
                );
                catalog.refresh(&manager, &shared).await.unwrap();
                let fresh = shared
                    .snapshot()
                    .get("binding_probe")
                    .unwrap()
                    .execute("fresh-owner", json!({}), None)
                    .await
                    .unwrap();
                assert!(
                    matches!(&fresh.content[0], ContentBlock::Text(text) if text.text == "fresh")
                );
            }
            manager.load_js_extensions(Vec::new()).await.unwrap();
            catalog.refresh(&manager, &shared).await.unwrap();
            assert!(shared.snapshot().get("binding_probe").is_none());
            assert!(manager.shutdown(std::time::Duration::from_secs(5)).await);
        });
    }

    /// Production-path check for the shared registry (bd-4t6oz): the JS
    /// runtime boots on the same [`crate::tools::SharedToolRegistry`] the
    /// agent is built over, a Rust tool is mounted through the agent AFTER
    /// boot, and a `pi.tool` hostcall issued by an extension tool reaches it.
    /// Under the previous split registry the runtime kept its pre-warm copy
    /// and answered "Unknown tool: late_echo".
    #[test]
    fn hostcall_sees_tool_mounted_after_runtime_start() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            let (temp_dir, manager, _js_runtime, shared) = start_extension_on_shared_registry(
                r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "call_late_tool",
                    description: "calls a tool the agent mounted after boot",
                    parameters: { type: "object" },
                    execute: async () => {
                      const result = await pi.tool("late_echo", { text: "mounted-late" });
                      return {
                        content: [{ type: "text", text: JSON.stringify(result) }],
                        isError: false
                      };
                    }
                  });
                }
                "#,
            )
            .await;

            // The agent shares the registry handle and mounts a tool now that
            // the runtime is already up.
            let provider = Arc::new(ToolCallingProvider);
            let mut agent =
                Agent::with_shared_tools(provider, shared.clone(), AgentConfig::default());
            assert!(!agent.has_tool("late_echo"));
            agent.extend_tools(vec![Box::new(LateEchoTool) as Box<dyn crate::tools::Tool>]);
            assert!(agent.has_tool("late_echo"));

            let wrappers = collect_extension_tool_wrappers(
                &manager,
                json!({ "cwd": temp_dir.path().display().to_string() }),
            )
            .await
            .expect("collect wrappers");
            let wrapper = wrappers
                .into_iter()
                .find(|wrapper| wrapper.name() == "call_late_tool")
                .expect("extension tool wrapper");
            let output = wrapper
                .execute("call-late", json!({}), None)
                .await
                .expect("extension tool executes");
            let text = output
                .content
                .iter()
                .filter_map(|block| match block {
                    crate::model::ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !output.is_error && text.contains("echo:mounted-late"),
                "the hostcall must reach the tool mounted after boot: is_error={} text={text}",
                output.is_error
            );
        });
    }

    /// bd-4t6oz slice 3: `pi.setActiveTools` changes what the live agent can
    /// see and execute in one published registry swap. Before, the hostcall
    /// only rewrote extension-manager metadata consulted when wrappers were
    /// collected, so an already-mounted tool stayed callable and in the
    /// provider schema.
    #[test]
    fn set_active_tools_shelves_and_restores_mounted_extension_tools() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        runtime.block_on(async {
            // Extension tool `execute` receives (callId, input, signal,
            // onUpdate, ctx); the active list travels in `input.names`.
            let (temp_dir, manager, _js_runtime, shared) = start_extension_on_shared_registry(
                r#"
                export default function init(pi) {
                  const ok = (text) => ({ content: [{ type: "text", text }], isError: false });
                  pi.registerTool({
                    name: "keep_tool",
                    description: "stays active",
                    parameters: { type: "object" },
                    execute: async () => ok("keep")
                  });
                  pi.registerTool({
                    name: "drop_tool",
                    description: "gets shelved",
                    parameters: { type: "object" },
                    execute: async () => ok("drop")
                  });
                  pi.registerTool({
                    name: "toggle_tool",
                    description: "applies an active-tool list",
                    parameters: { type: "object", properties: { names: { type: "array" } } },
                    execute: async (_callId, input) => {
                      await pi.setActiveTools(input.names);
                      return ok("toggled");
                    }
                  });
                }
                "#,
            )
            .await;

            let provider = Arc::new(ToolCallingProvider);
            let mut agent =
                Agent::with_shared_tools(provider, shared.clone(), AgentConfig::default());
            let wrappers = collect_extension_tool_wrappers(
                &manager,
                json!({ "cwd": temp_dir.path().display().to_string() }),
            )
            .await
            .expect("collect wrappers");
            assert_eq!(wrappers.len(), 3);
            agent.extend_tools(wrappers);
            assert!(agent.has_tool("keep_tool") && agent.has_tool("drop_tool"));
            let version_before = shared.version();

            // The extension narrows its active set: the shelved tool leaves
            // the live registry in the same swap, the others stay.
            let registry = shared.snapshot();
            let toggle = registry.get("toggle_tool").expect("toggle_tool mounted");
            let output = toggle
                .execute(
                    "toggle-1",
                    json!({ "names": ["keep_tool", "toggle_tool"] }),
                    None,
                )
                .await
                .expect("toggle executes");
            assert!(!output.is_error, "toggle must succeed: {output:?}");
            assert!(
                shared.version() > version_before,
                "the swap must be published"
            );
            assert!(agent.has_tool("keep_tool"));
            assert!(
                !agent.has_tool("drop_tool"),
                "a de-activated extension tool must be unreachable for the agent"
            );
            assert_eq!(
                shared
                    .snapshot()
                    .inactive_tools()
                    .iter()
                    .map(|tool| tool.name().to_string())
                    .collect::<Vec<_>>(),
                vec!["drop_tool".to_string()]
            );

            // Naming it again restores it; built-ins are never part of this.
            let registry = shared.snapshot();
            let toggle = registry
                .get("toggle_tool")
                .expect("toggle_tool still mounted");
            toggle
                .execute(
                    "toggle-2",
                    json!({ "names": ["keep_tool", "drop_tool", "toggle_tool"] }),
                    None,
                )
                .await
                .expect("toggle executes again");
            assert!(
                agent.has_tool("drop_tool"),
                "a re-activated tool must come back"
            );
            assert!(shared.snapshot().inactive_tools().is_empty());
        });
    }

    // -- Constructor & builder tests --

    #[test]
    fn extension_tool_wrapper_default_timeout() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let source = r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "t",
                    description: "d",
                    parameters: { type: "object" },
                    execute: async () => ({ content: [], isError: false })
                  });
                }
            "#;
            let (_temp_dir, _manager, js_runtime, def) = setup_js_tool(source, "t").await;
            let wrapper = ExtensionToolWrapper::new(def, js_runtime);
            assert_eq!(wrapper.timeout_ms, DEFAULT_EXTENSION_TOOL_TIMEOUT_MS);
            assert_eq!(wrapper.timeout_ms, 60_000);
        });
    }

    #[test]
    fn extension_tool_wrapper_timeout_clamp_boundary() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let source = r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "t",
                    description: "d",
                    parameters: { type: "object" },
                    execute: async () => ({ content: [], isError: false })
                  });
                }
            "#;
            let (_temp_dir, _manager, js_runtime, def) = setup_js_tool(source, "t").await;

            // timeout=0 clamped to 1
            let w0 = ExtensionToolWrapper::new(def.clone(), js_runtime.clone()).with_timeout_ms(0);
            assert_eq!(w0.timeout_ms, 1);

            // timeout=1 stays 1
            let w1 = ExtensionToolWrapper::new(def.clone(), js_runtime.clone()).with_timeout_ms(1);
            assert_eq!(w1.timeout_ms, 1);

            // timeout=u64::MAX stays u64::MAX
            let wmax = ExtensionToolWrapper::new(def, js_runtime).with_timeout_ms(u64::MAX);
            assert_eq!(wmax.timeout_ms, u64::MAX);
        });
    }

    #[test]
    fn extension_tool_wrapper_ctx_payload_default_empty() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let source = r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "t",
                    description: "d",
                    parameters: { type: "object" },
                    execute: async () => ({ content: [], isError: false })
                  });
                }
            "#;
            let (_temp_dir, _manager, js_runtime, def) = setup_js_tool(source, "t").await;
            let wrapper = ExtensionToolWrapper::new(def, js_runtime);
            assert_eq!(wrapper.ctx_payload.as_ref(), &json!({}));
        });
    }

    #[test]
    fn extension_tool_wrapper_ctx_payload_override() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let source = r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "t",
                    description: "d",
                    parameters: { type: "object" },
                    execute: async () => ({ content: [], isError: false })
                  });
                }
            "#;
            let (_temp_dir, _manager, js_runtime, def) = setup_js_tool(source, "t").await;
            let custom_ctx = json!({"cwd": "/tmp", "user": "test"});
            let wrapper =
                ExtensionToolWrapper::new(def, js_runtime).with_ctx_payload(custom_ctx.clone());
            assert_eq!(wrapper.ctx_payload.as_ref(), &custom_ctx);
        });
    }

    // -- collect_extension_tool_wrappers tests --

    #[test]
    fn collect_wrappers_no_js_runtime_returns_empty() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let manager = ExtensionManager::new();
            let wrappers = collect_extension_tool_wrappers(&manager, json!({}))
                .await
                .expect("collect wrappers");
            assert!(wrappers.is_empty());
        });
    }

    #[test]
    fn collect_wrappers_multiple_tools_from_one_extension() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "tool_alpha",
                    description: "first tool",
                    parameters: { type: "object" },
                    execute: async () => ({ content: [{ type: "text", text: "alpha" }], isError: false })
                  });
                  pi.registerTool({
                    name: "tool_beta",
                    description: "second tool",
                    parameters: { type: "object" },
                    execute: async () => ({ content: [{ type: "text", text: "beta" }], isError: false })
                  });
                }
                "#,
            )
            .expect("write extension");

            let manager = ExtensionManager::new();
            let tools = Arc::new(ToolRegistry::new(&[], temp_dir.path(), None));
            let js_runtime = JsExtensionRuntimeHandle::start(
                PiJsRuntimeConfig {
                    cwd: temp_dir.path().display().to_string(),
                    ..Default::default()
                },
                Arc::clone(&tools),
                manager.clone(),
            )
            .await
            .expect("start js runtime");
            manager.set_js_runtime(js_runtime.clone());

            let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("spec");
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load js extensions");

            let wrappers = collect_extension_tool_wrappers(&manager, json!({}))
                .await
                .expect("collect wrappers");
            assert_eq!(wrappers.len(), 2);

            // Sorted alphabetically
            assert_eq!(wrappers[0].name(), "tool_alpha");
            assert_eq!(wrappers[1].name(), "tool_beta");
        });
    }

    #[test]
    fn collect_wrappers_respects_active_tools_filter() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let entry_path = temp_dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "tool_keep",
                    description: "kept",
                    parameters: { type: "object" },
                    execute: async () => ({ content: [], isError: false })
                  });
                  pi.registerTool({
                    name: "tool_skip",
                    description: "skipped",
                    parameters: { type: "object" },
                    execute: async () => ({ content: [], isError: false })
                  });
                }
                "#,
            )
            .expect("write extension");

            let manager = ExtensionManager::new();
            let tools = Arc::new(ToolRegistry::new(&[], temp_dir.path(), None));
            let js_runtime = JsExtensionRuntimeHandle::start(
                PiJsRuntimeConfig {
                    cwd: temp_dir.path().display().to_string(),
                    ..Default::default()
                },
                Arc::clone(&tools),
                manager.clone(),
            )
            .await
            .expect("start js runtime");
            manager.set_js_runtime(js_runtime.clone());

            let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("spec");
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load js extensions");

            // Set active_tools to only include tool_keep
            manager.set_active_tools(vec!["tool_keep".to_string()]);

            let wrappers = collect_extension_tool_wrappers(&manager, json!({}))
                .await
                .expect("collect wrappers");
            assert_eq!(wrappers.len(), 1);
            assert_eq!(wrappers[0].name(), "tool_keep");
        });
    }

    #[test]
    fn extension_tool_wrapper_js_error_maps_to_tool_error() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let source = r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "throwing_tool",
                    description: "throws an error",
                    parameters: { type: "object" },
                    execute: async () => { throw new Error("boom!"); }
                  });
                }
            "#;
            let (_temp_dir, _manager, js_runtime, def) =
                setup_js_tool(source, "throwing_tool").await;

            let wrapper = ExtensionToolWrapper::new(def, js_runtime);
            let err = wrapper
                .execute("call-1", json!({}), None)
                .await
                .expect_err("throwing tool should fail");

            match err {
                Error::Tool { tool, message } => {
                    assert_eq!(tool, "throwing_tool");
                    assert!(
                        message.contains("boom") || message.contains("error"),
                        "Expected error message to reference the thrown error, got: {message}"
                    );
                }
                other => panic!(),
            }
        });
    }

    #[test]
    fn extension_tool_wrapper_empty_content_result() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let source = r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "empty_tool",
                    description: "returns empty content",
                    parameters: { type: "object" },
                    execute: async () => ({
                      content: [],
                      isError: false
                    })
                  });
                }
            "#;
            let (_temp_dir, _manager, js_runtime, def) = setup_js_tool(source, "empty_tool").await;

            let wrapper = ExtensionToolWrapper::new(def, js_runtime);
            let output = wrapper
                .execute("call-1", json!({}), None)
                .await
                .expect("execute tool");

            assert!(!output.is_error);
            assert!(output.content.is_empty());
        });
    }

    #[test]
    fn extension_tool_wrapper_is_error_flag() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let source = r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "error_tool",
                    description: "returns error flag",
                    parameters: { type: "object" },
                    execute: async () => ({
                      content: [{ type: "text", text: "something went wrong" }],
                      isError: true
                    })
                  });
                }
            "#;
            let (_temp_dir, _manager, js_runtime, def) = setup_js_tool(source, "error_tool").await;

            let wrapper = ExtensionToolWrapper::new(def, js_runtime);
            let output = wrapper
                .execute("call-1", json!({}), None)
                .await
                .expect("execute tool");

            assert!(output.is_error);
            match output.content.as_slice() {
                [ContentBlock::Text(text)] => {
                    assert_eq!(text.text, "something went wrong");
                }
                other => panic!(),
            }
        });
    }

    #[test]
    fn extension_tool_wrapper_passes_input_to_handler() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async {
            let source = r#"
                export default function init(pi) {
                  pi.registerTool({
                    name: "echo_tool",
                    description: "echoes input",
                    parameters: { type: "object", properties: { msg: { type: "string" } } },
                    execute: async (_callId, input) => ({
                      content: [{ type: "text", text: JSON.stringify(input) }],
                      isError: false
                    })
                  });
                }
            "#;
            let (_temp_dir, _manager, js_runtime, def) = setup_js_tool(source, "echo_tool").await;

            let wrapper = ExtensionToolWrapper::new(def, js_runtime);
            let output = wrapper
                .execute("call-1", json!({"msg": "hello world"}), None)
                .await
                .expect("execute tool");

            assert!(!output.is_error);
            match output.content.as_slice() {
                [ContentBlock::Text(text)] => {
                    let parsed: serde_json::Value =
                        serde_json::from_str(&text.text).expect("parse JSON");
                    assert_eq!(parsed["msg"], "hello world");
                }
                other => panic!(),
            }
        });
    }
}
