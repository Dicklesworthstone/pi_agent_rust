//! Extension tools integration.
//!
//! This module provides adapters that allow JavaScript extension-registered tools to be used as
//! normal Rust `Tool` implementations inside the agent tool registry.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
#[cfg(feature = "wasm-host")]
use std::time::Duration;

use crate::error::{Error, Result};
#[cfg(test)]
use crate::extensions::JsExtensionRuntimeHandle;
#[cfg(feature = "wasm-host")]
use crate::extensions::WasmExtensionHandle;
use crate::extensions::{ExtensionManager, ExtensionRuntimeHandle};
use crate::extensions_js::ExtensionToolDef;
use crate::tools::{Tool, ToolOutput, ToolUpdate};
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
            runtime: runtime.into(),
            ctx_payload: Arc::new(Value::Object(serde_json::Map::new())),
            timeout_ms: DEFAULT_EXTENSION_TOOL_TIMEOUT_MS,
        }
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
    let shared_ctx_payload = Arc::new(ctx_payload);
    let active = manager
        .active_tools()
        .map(|tools| tools.into_iter().collect::<HashSet<_>>());

    let mut wrappers: Vec<Box<dyn Tool>> = Vec::new();
    let mut seen = HashSet::new();

    if let Some(runtime) = manager.runtime() {
        let mut defs = runtime.get_registered_tools().await?;
        if let Some(active) = active.as_ref() {
            defs.retain(|def| active.contains(&def.name));
        }

        defs.sort_by(|a, b| a.name.cmp(&b.name));
        for def in defs {
            if !seen.insert(def.name.clone()) {
                tracing::warn!(tool = %def.name, "Duplicate extension tool name; ignoring");
                continue;
            }

            wrappers.push(Box::new(
                ExtensionToolWrapper::new(def, runtime.clone())
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
            if let Some(active) = active.as_ref()
                && !active.contains(&def.name)
            {
                continue;
            }
            if !seen.insert(def.name.clone()) {
                tracing::warn!(tool = %def.name, "Duplicate extension tool name; ignoring");
                continue;
            }

            wrappers.push(Box::new(WasmExtensionToolWrapper::new(def, handle)));
        }
    }

    Ok(wrappers)
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

    async fn execute(
        &self,
        tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let result = self
            .runtime
            .execute_tool_ref(
                &self.def.name,
                tool_call_id,
                input,
                Arc::clone(&self.ctx_payload),
                self.timeout_ms,
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
    use crate::extensions::{ExtensionManager, JsExtensionLoadSpec};
    use crate::extensions_js::PiJsRuntimeConfig;
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
    use std::sync::Arc;

    async fn setup_js_tool(
        source: &str,
        tool_name: &str,
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

        let def = js_runtime
            .get_registered_tools()
            .await
            .expect("get registered tools")
            .into_iter()
            .find(|tool| tool.name == tool_name)
            .expect("tool registered");

        (temp_dir, manager, js_runtime, def)
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
