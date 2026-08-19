//! MCP client: mounts configured MCP servers' tools as first-class
//! `mcp__<server>_<tool>` agent tools (bd-cv653.6.1).
//!
//! One registry ([`McpManager`]) unifies three config sources — native files
//! (`.pi/mcp.json`, `.agents/mcp.json`, `~/.pi/agent/mcp.json`, `--mcp-config`),
//! foreign files (`.claude/`, `.cursor/`, ...), and extension-registered
//! specs — with one trust gate, one spawn path, and one `/mcp` view showing
//! all three provenances.

pub mod config;
pub mod manager;
pub mod transport;
pub mod trust;

pub use config::{ConfiguredServer, McpDiscovery, Provenance};
pub use manager::{McpManager, McpToolMeta, ServerHealth, ServerInfo};
pub use trust::{TrustDecision, TrustStore};

use async_trait::async_trait;
use serde_json::Value;

use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};

/// Mounted tool name cap (provider schemas reject longer names).
const MAX_MOUNTED_NAME: usize = 64;

/// Build the mounted tool name for a server tool: sanitized, length-capped
/// with a stable hash suffix on overflow.
#[must_use]
pub fn mounted_name(server: &str, tool: &str) -> String {
    use std::hash::{Hash, Hasher};

    let sanitize = |raw: &str| -> String {
        raw.chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                    ch
                } else {
                    '_'
                }
            })
            .collect()
    };
    let full = format!("mcp__{}__{}", sanitize(server), sanitize(tool));
    if full.chars().count() <= MAX_MOUNTED_NAME {
        return full;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    full.hash(&mut hasher);
    let suffix = format!("{:08x}", u32::try_from(hasher.finish()).unwrap_or(u32::MAX));
    let keep = MAX_MOUNTED_NAME - suffix.len() - 1;
    let truncated: String = full.chars().take(keep).collect();
    format!("{truncated}_{suffix}")
}

/// One mounted MCP server tool.
pub struct McpTool {
    server: String,
    tool_name: String,
    mounted: String,
    description: String,
    schema: Value,
    manager: std::sync::Arc<McpManager>,
}

impl McpTool {
    #[must_use]
    pub fn new(server: &str, meta: &McpToolMeta, manager: std::sync::Arc<McpManager>) -> Self {
        let description = if meta.description.is_empty() {
            format!("MCP tool {} from server {}", meta.name, server)
        } else {
            format!("{} (MCP server: {})", meta.description, server)
        };
        Self {
            server: server.to_string(),
            tool_name: meta.name.clone(),
            mounted: mounted_name(server, &meta.name),
            description,
            schema: meta.input_schema.clone(),
            manager,
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.mounted
    }

    fn label(&self) -> &str {
        &self.mounted
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.schema.clone()
    }

    fn effects(&self) -> ToolEffects {
        // MCP servers are external processes/network endpoints; calls may
        // mutate remote state, so they are scheduling barriers.
        ToolEffects::network().union(ToolEffects::process())
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> crate::error::Result<ToolOutput> {
        let result = self
            .manager
            .call_tool(&self.server, &self.tool_name, input)
            .await?;
        Ok(mcp_result_to_output(&result))
    }
}

/// Shape an MCP `tools/call` result into a ToolOutput: text content blocks
/// join into the text payload; structured content lands in details;
/// `isError` propagates.
fn mcp_result_to_output(result: &Value) -> ToolOutput {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut texts = Vec::new();
    let mut non_text = Vec::new();
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        for block in content {
            let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
            if kind == "text" {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    texts.push(text.to_string());
                }
            } else {
                non_text.push(block.clone());
            }
        }
    }
    let structured = result.get("structuredContent").cloned();
    let text = if texts.is_empty() {
        // No text blocks: fall back to a JSON rendering so the model sees
        // the result instead of an empty payload.
        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "<unserializable>".to_string())
    } else {
        texts.join("\n")
    };
    let mut details = serde_json::json!({
        "mcp": true,
        "nonTextBlocks": non_text.len(),
    });
    if let Some(structured) = structured {
        details["structuredContent"] = structured;
    }
    if !non_text.is_empty() {
        details["nonText"] = Value::Array(non_text);
    }
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(details),
        is_error,
    }
}

/// Mount every cached server tool as a first-class tool wrapper.
#[must_use]
pub fn mount_tools(manager: &std::sync::Arc<McpManager>) -> Vec<Box<dyn Tool>> {
    let mut out: Vec<Box<dyn Tool>> = Vec::new();
    for (server, metas) in manager.mounted_tool_metas() {
        for meta in metas {
            out.push(Box::new(McpTool::new(&server, &meta, manager.clone())));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounted_name_sanitizes_and_preserves() {
        assert_eq!(mounted_name("docs", "search"), "mcp__docs__search");
        assert_eq!(
            mounted_name("my-server", "do.thing"),
            "mcp__my-server__do_thing"
        );
    }

    #[test]
    fn mounted_name_caps_with_stable_hash() {
        let long_server = "s".repeat(40);
        let long_tool = "t".repeat(40);
        let name = mounted_name(&long_server, &long_tool);
        assert!(name.chars().count() <= MAX_MOUNTED_NAME);
        assert!(name.starts_with("mcp__"));
        // Stable across calls.
        assert_eq!(name, mounted_name(&long_server, &long_tool));
    }

    #[test]
    fn result_shaping_text_and_error() {
        let out = mcp_result_to_output(&serde_json::json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "text", "text": "world"}
            ],
            "isError": false
        }));
        assert!(!out.is_error);
        let text = out.content.first().and_then(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        });
        assert_eq!(text, Some("hello\nworld"));
    }

    #[test]
    fn result_shaping_error_and_nontext_fallback() {
        let out = mcp_result_to_output(&serde_json::json!({
            "content": [{"type": "image", "data": "..."}],
            "isError": true
        }));
        assert!(out.is_error);
        // No text blocks → JSON fallback rendering.
        let text = out.content.first().and_then(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        });
        assert!(text.is_some_and(|t| t.contains("image")));
        assert_eq!(out.details.as_ref().unwrap()["nonTextBlocks"], 1);
    }
}
