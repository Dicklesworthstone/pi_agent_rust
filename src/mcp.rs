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
use std::path::{Path, PathBuf};

use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};

/// Build an MCP manager while enforcing the established workspace-trust
/// decision at discovery time.
///
/// Denied workspaces never open project-native or foreign project configs;
/// explicit CLI files and global Pi configuration remain eligible.
pub fn bootstrap_with_project_trust(
    cwd: &Path,
    global_dir: &Path,
    cli_paths: &[PathBuf],
    project_trusted: bool,
) -> crate::error::Result<McpManager> {
    let discovery =
        config::discover_with_project_trust(cwd, global_dir, cli_paths, project_trusted);
    Ok(McpManager::new(cwd, global_dir, discovery))
}

/// Mounted tool name cap (provider schemas reject longer names).
const MAX_MOUNTED_NAME: usize = 64;

/// Build the mounted tool name for a server tool: sanitized, length-capped
/// with a stable hash suffix on overflow.
#[must_use]
pub fn mounted_name(server: &str, tool: &str) -> String {
    use sha2::{Digest as _, Sha256};

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
    let sanitized_server = sanitize(server);
    let sanitized_tool = sanitize(tool);
    let full = format!("mcp__{sanitized_server}__{sanitized_tool}");
    let lossy = sanitized_server != server || sanitized_tool != tool;
    let reserved_hash_suffix = full
        .as_bytes()
        .get(full.len().saturating_sub(25)..)
        .is_some_and(|suffix| {
            suffix.len() == 25 && suffix[0] == b'_' && suffix[1..].iter().all(u8::is_ascii_hexdigit)
        });
    // `__` is the component delimiter. Hash any raw component containing it,
    // and reserve the generated suffix shape, so a literal tool name cannot
    // deliberately impersonate the mounted name of a lossy/long input.
    let ambiguous = server.contains("__") || tool.contains("__") || reserved_hash_suffix;
    if !lossy && !ambiguous && full.len() <= MAX_MOUNTED_NAME {
        return full;
    }

    // Sanitization is many-to-one (`do.thing` and `do_thing` would otherwise
    // collide), and `DefaultHasher` is not a cross-version persistence
    // contract. Bind the original length-framed names with a stable digest.
    let mut hasher = Sha256::new();
    hasher.update(b"pi_agent_rust:mcp-mounted-tool:v1\0");
    for part in [server, tool] {
        hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = crate::package_manager::hex_encode(&hasher.finalize());
    let suffix = &digest[..24];
    let keep = MAX_MOUNTED_NAME - suffix.len() - 1;
    let truncated = &full[..full.len().min(keep)];
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

/// Mount cached wrappers for one server only.
///
/// Runtime trust/test flows use this targeted form so a newly available
/// server does not re-append wrappers for every server that was already
/// mounted during startup (bd-vjfol).
#[must_use]
pub fn mount_server_tools(
    manager: &std::sync::Arc<McpManager>,
    server_name: &str,
) -> Vec<Box<dyn Tool>> {
    let Some((server, metas)) = manager
        .mounted_tool_metas()
        .into_iter()
        .find(|(server, _)| server == server_name)
    else {
        return Vec::new();
    };
    metas
        .into_iter()
        .map(|meta| Box::new(McpTool::new(&server, &meta, manager.clone())) as Box<dyn Tool>)
        .collect()
}

/// Connect every acknowledged server, then snapshot its cached tools as
/// first-class wrappers.
///
/// Call this once after native, foreign, CLI, and
/// extension-provided server definitions have all been registered.
#[must_use]
pub async fn connect_trusted_and_mount_tools(
    manager: &std::sync::Arc<McpManager>,
) -> Vec<Box<dyn Tool>> {
    manager.connect_trusted().await;
    mount_tools(manager)
}

/// Bring MCP servers that extensions registered after startup into a live
/// agent (bd-8m21l).
///
/// Startup copies the extension-registered server definitions into the MCP
/// manager once; a `registerMcpServer` call from a later extension callback
/// only updates the extension manager's snapshot. This drains that snapshot:
/// every definition whose name the manager does not know yet is registered
/// under the same trust gate as at startup, and when anything was new the
/// trusted servers are connected and only tool names the agent does not
/// already have are mounted. Returns the number of newly registered
/// definitions; cheap when nothing changed, so callers run it at every turn
/// start (SDK/FrankenTUI prompts and the classic TUI's turn task).
pub async fn sync_extension_registrations(
    manager: &std::sync::Arc<McpManager>,
    extensions: &crate::extensions::ExtensionManager,
    agent: &mut crate::agent::Agent,
) -> usize {
    let specs = extensions.extension_mcp_servers();
    if specs.is_empty() {
        return 0;
    }
    let known: std::collections::HashSet<String> = manager
        .list()
        .into_iter()
        .map(|server| server.name)
        .collect();
    let mut registered = 0usize;
    for spec in specs {
        let name = spec
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if name.is_empty() || known.contains(name) {
            continue;
        }
        manager.register_extension_server(name, &spec);
        registered += 1;
    }
    if registered > 0 {
        tracing::info!(
            event = "pi.mcp.extension_registrations_synced",
            registered,
            "registered extension MCP servers contributed after startup"
        );
        let mut wrappers = connect_trusted_and_mount_tools(manager).await;
        wrappers.retain(|tool| !agent.has_tool(tool.name()));
        agent.extend_tools(wrappers);
    }
    registered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounted_name_sanitizes_and_preserves() {
        assert_eq!(mounted_name("docs", "search"), "mcp__docs__search");
        let sanitized = mounted_name("my-server", "do.thing");
        assert!(sanitized.starts_with("mcp__my-server__do_thing_"));
        assert_ne!(sanitized, mounted_name("my-server", "do_thing"));
        assert_eq!(sanitized, mounted_name("my-server", "do.thing"));

        assert_ne!(
            mounted_name("a__b", "c"),
            mounted_name("a", "b__c"),
            "length-framed hashing must disambiguate raw component boundaries"
        );
        let impersonating_tool = sanitized
            .strip_prefix("mcp__my-server__")
            .expect("mounted prefix");
        assert_ne!(
            sanitized,
            mounted_name("my-server", impersonating_tool),
            "the generated hash suffix namespace must be reserved"
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
