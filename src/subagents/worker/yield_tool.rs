//! Native, ordered receipts for early terminal workpool item results.
//!
//! Only an explicitly configured RPC worker installs this helper. The parent
//! independently authenticates the RPC event and applies its frozen contract;
//! a successful local call acknowledges submission, not parent acceptance.

use super::output_contract;
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{SharedToolRegistry, Tool, ToolEffects, ToolOutput, ToolUpdate};
use serde_json::{Value, json};

struct YieldTool;

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for YieldTool {
    fn name(&self) -> &str {
        "yield"
    }

    fn label(&self) -> &str {
        "yield"
    }

    fn description(&self) -> &str {
        "Submit one completed item from the current grouped workpool assignment \
         to the parent immediately. Copy requestId and key exactly from that \
         assignment; supply either data satisfying the item's output contract \
         or a nonempty error, never both. Submit each key once, then continue \
         working on the remaining items. This is a terminal item result, not a \
         progress message. Omit submitted keys from the final items envelope."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "requestId": {"type": "string", "minLength": 1, "maxLength": 256},
                "key": {"type": "integer", "minimum": 1, "maximum": 32},
                "data": {},
                "error": {"type": "string", "minLength": 1, "maxLength": 4096}
            },
            "required": ["requestId", "key"],
            "oneOf": [{"required": ["data"]}, {"required": ["error"]}],
            "additionalProperties": false
        })
    }

    fn effects(&self) -> ToolEffects {
        // Publication order matters, but this helper grants no filesystem or
        // process authority and is usable by analysis-only workers.
        ToolEffects::read().with_ordered_execution()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let receipt = output_contract::yield_receipt(tool_call_id, input)?;
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(
                "Result submitted to the parent workpool.",
            ))],
            details: Some(receipt),
            is_error: false,
        })
    }
}

pub(in crate::subagents) fn install(tools: &SharedToolRegistry) -> Result<()> {
    let mut result = Ok(());
    tools.update(|registry| {
        if registry.get("yield").is_some() {
            result = Err(Error::validation(
                "Worker yield tool conflicts with an existing tool named yield",
            ));
            return;
        }
        registry.push(Box::new(YieldTool));
        if registry.get("yield").is_none() {
            result = Err(Error::validation(
                "Worker yield tool is disabled by tools.loadMode.yield",
            ));
            return;
        }
        // Respect an explicit off mode above, then expose the transport helper
        // directly even when its configured tier would require discovery.
        registry.promote_tools(&["yield"]);
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::tools::ToolRegistry;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn receipt_uses_native_identity_and_only_the_final_tool_result() {
        let updates = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&updates);
        let output = futures::executor::block_on(YieldTool.execute(
            "native-call-7",
            json!({"requestId":"warm-2", "key":7, "data":{"answer":42}}),
            Some(Box::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            })),
        ))
        .expect("valid yield receipt");
        assert!(!output.is_error);
        assert_eq!(updates.load(Ordering::SeqCst), 0);
        let receipt = output.details.expect("receipt details");
        assert_eq!(receipt["schema"], "pi.subagent.worker.yield.v1");
        assert_eq!(receipt["toolCallId"], "native-call-7");
        assert_eq!(receipt["requestId"], "warm-2");
        assert_eq!(receipt["key"], 7);
        assert_eq!(receipt["data"]["answer"], 42);
    }

    #[test]
    fn guest_cannot_supply_receipt_identity_or_ambiguous_results() {
        for input in [
            json!({"requestId":"warm-2", "key":1, "data":null, "toolCallId":"forged"}),
            json!({"requestId":"warm-2", "key":1, "data":null, "error":"failed"}),
            json!({"requestId":"warm-2", "key":1, "error":""}),
            json!({"requestId":"warm-2", "key":0, "data":null}),
        ] {
            assert!(futures::executor::block_on(YieldTool.execute("native", input, None)).is_err());
        }
    }

    #[test]
    fn installation_adds_only_the_private_transport_helper() {
        let tools = SharedToolRegistry::new(ToolRegistry::without_builtins(None));
        install(&tools).expect("worker helper admitted");
        let snapshot = tools.snapshot();
        assert_eq!(snapshot.tools().len(), 1);
        assert!(snapshot.get("yield").is_some());
        assert!(!snapshot.is_discoverable("yield"));
        assert!(install(&tools).is_err(), "existing tools must not be replaced");
        assert_eq!(tools.snapshot().tools().len(), 1);
    }

    #[test]
    fn configured_off_is_not_overridden_by_worker_registration() {
        let config: Config = serde_json::from_value(json!({
            "tools":{"loadMode":{"yield":"off"}}
        }))
        .expect("valid configuration");
        let tools = SharedToolRegistry::new(ToolRegistry::without_builtins(Some(&config)));
        assert!(install(&tools).is_err());
        assert!(tools.snapshot().get("yield").is_none());
        assert!(tools.snapshot().tools().is_empty());
    }

    #[test]
    fn discoverable_configuration_exposes_receipts_without_extra_tools() {
        let config: Config = serde_json::from_value(json!({
            "tools":{"loadMode":{"yield":"discoverable"}}
        }))
        .expect("valid configuration");
        let tools = SharedToolRegistry::new(ToolRegistry::without_builtins(Some(&config)));
        install(&tools).expect("worker helper admitted");
        let snapshot = tools.snapshot();
        assert!(snapshot.get("yield").is_some());
        assert!(!snapshot.is_discoverable("yield"));
    }
}
