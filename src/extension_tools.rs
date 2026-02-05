//! Extension tools integration.
//!
//! This module provides adapters that allow JavaScript extension-registered tools to be used as
//! normal Rust `Tool` implementations inside the agent tool registry.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
#[cfg(feature = "wasm-host")]
use std::time::Duration;

use crate::error::{Error, Result};
#[cfg(feature = "wasm-host")]
use crate::extensions::WasmExtensionHandle;
use crate::extensions::{ExtensionManager, JsExtensionRuntimeHandle};
use crate::extensions_js::ExtensionToolDef;
use crate::tools::{Tool, ToolOutput, ToolUpdate};
#[cfg(feature = "wasm-host")]
use asupersync::time::{timeout, wall_now};

const DEFAULT_EXTENSION_TOOL_TIMEOUT_MS: u64 = 60_000;

/// Wraps a JS extension-registered tool so it can be used as a Rust [`Tool`].
///
/// Note: This wrapper uses [`JsExtensionRuntimeHandle`] rather than [`crate::extensions_js::PiJsRuntime`]
/// so it remains `Send + Sync` and can be stored in the shared tool registry.
pub struct ExtensionToolWrapper {
    def: ExtensionToolDef,
    runtime: JsExtensionRuntimeHandle,
    ctx_payload: Value,
    timeout_ms: u64,
}

impl ExtensionToolWrapper {
    #[must_use]
    pub fn new(def: ExtensionToolDef, runtime: JsExtensionRuntimeHandle) -> Self {
        Self {
            def,
            runtime,
            ctx_payload: Value::Object(serde_json::Map::new()),
            timeout_ms: DEFAULT_EXTENSION_TOOL_TIMEOUT_MS,
        }
    }

    #[must_use]
    pub fn with_ctx_payload(mut self, ctx_payload: Value) -> Self {
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

    let mut wrappers: Vec<Box<dyn Tool>> = Vec::new();
    let mut seen = HashSet::new();

    if let Some(runtime) = manager.js_runtime() {
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
                    .with_ctx_payload(ctx_payload.clone()),
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
            if let Some(active) = active.as_ref() {
                if !active.contains(&def.name) {
                    continue;
                }
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
            .execute_tool(
                self.def.name.clone(),
                tool_call_id.to_string(),
                input,
                self.ctx_payload.clone(),
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
            context: &Context,
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
                    other => panic!("Expected single text content block, got: {other:?}"),
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
            let mut agent_session = AgentSession::new(agent, session, false);
            let message = agent_session
                .run_text("hi".to_string(), |_event: AgentEvent| {})
                .await
                .expect("run_text");

            match message.content.as_slice() {
                [ContentBlock::Text(text)] => assert_eq!(text.text, "done"),
                other => panic!("Expected single text content block, got: {other:?}"),
            }
        });
    }
}
