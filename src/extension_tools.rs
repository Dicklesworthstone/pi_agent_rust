//! Extension tools integration.
//!
//! This module provides adapters that allow JavaScript extension-registered tools to be used as
//! normal Rust `Tool` implementations inside the agent tool registry.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::extensions::JsExtensionRuntimeHandle;
use crate::extensions_js::ExtensionToolDef;
use crate::tools::{Tool, ToolOutput, ToolUpdate};

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

#[async_trait]
impl Tool for ExtensionToolWrapper {
    fn name(&self) -> &str {
        &self.def.name
    }

    fn label(&self) -> &str {
        &self.def.label
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::extensions::{ExtensionManager, JsExtensionLoadSpec};
    use crate::extensions_js::PiJsRuntimeConfig;
    use crate::model::ContentBlock;
    use crate::tools::ToolRegistry;
    use asupersync::runtime::RuntimeBuilder;
    use serde_json::json;
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

            let def = ExtensionToolDef {
                name: "hello_tool".to_string(),
                label: "hello_tool".to_string(),
                description: "test tool".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": { "name": { "type": "string" } }
                }),
            };

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
                other => panic!("Expected single text content block, got: {other:?}"),
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
}
