//! MCP client: mounts configured MCP servers' tools as first-class
//! `mcp__<server>_<tool>` agent tools (bd-cv653.6.1).
//!
//! One registry ([`McpManager`]) unifies three config sources — native files
//! (`.pi/mcp.json`, `.agents/mcp.json`, `~/.pi/agent/mcp.json`, `--mcp-config`),
//! foreign files (`.claude/`, `.cursor/`, ...), and extension-registered
//! specs — with one trust gate, one spawn path, and one `/mcp` view showing
//! all three provenances.

pub mod config;
mod content;
pub mod manager;
pub mod transport;
pub mod trust;

pub use config::{ConfiguredServer, McpDiscovery, Provenance};
pub use manager::{McpManager, McpToolMeta, ServerHealth, ServerInfo};
pub use trust::{TrustDecision, TrustStore};

use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[cfg(test)]
use crate::model::ContentBlock;
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

/// Preserve native media and ordered mixed content, expose embedded documents
/// and structured results, and keep client metadata out of prompt text.
fn mcp_result_to_output(result: &Value) -> ToolOutput {
    content::tool_output(result)
}

/// Resource access for one trusted server. This namespace is disjoint from
/// server-advertised `mcp__...` tools, even for hostile or ambiguous names.
pub struct McpContextTool {
    server: String,
    mounted: String,
    description: String,
    manager: std::sync::Arc<McpManager>,
}

impl McpContextTool {
    #[must_use]
    pub fn new(server: &str, manager: std::sync::Arc<McpManager>) -> Self {
        use sha2::{Digest as _, Sha256};

        let readable: String = server
            .chars()
            .take(24)
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        let mut hasher = Sha256::new();
        hasher.update(b"pi_agent_rust:mcp-context:v1\0");
        hasher.update(server.as_bytes());
        let hash = crate::package_manager::hex_encode(&hasher.finalize());
        Self {
            server: server.to_string(),
            mounted: format!("mcp_context_{readable}_{}", &hash[..24]),
            description: format!(
                "Browse resource and URI-template catalogs and read resources from MCP server {server:?}. \
                 Listings return one page: pass nextCursor unchanged to continue. Read resource URIs \
                 through this tool, not local file or web tools. Unsupported methods return an error."
            ),
            manager,
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum McpContextAction {
    ListResources { cursor: Option<String> },
    ListResourceTemplates { cursor: Option<String> },
    ReadResource { uri: String },
}

/// Only public catalog fields enter model context. In particular, `_meta`
/// remains in details; it must not become an extra instruction channel.
fn resource_catalog_output(result: Value, templates: bool) -> ToolOutput {
    let (field, uri_field) = if templates {
        ("resourceTemplates", "uriTemplate")
    } else {
        ("resources", "uri")
    };
    let entries: Vec<Value> = result[field]
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| {
            let mut public = serde_json::Map::new();
            for key in ["name", "title", uri_field, "description", "mimeType"] {
                if let Some(value) = entry.get(key) {
                    public.insert(key.to_string(), value.clone());
                }
            }
            if !templates && let Some(size) = entry.get("size") {
                public.insert("size".to_string(), size.clone());
            }
            Value::Object(public)
        })
        .collect();
    let mut public = serde_json::json!({field: entries});
    if let Some(cursor) = result.get("nextCursor") {
        public["nextCursor"] = cursor.clone();
    }
    ToolOutput {
        content: vec![crate::model::ContentBlock::Text(
            crate::model::TextContent::new(public.to_string()),
        )],
        details: Some(result),
        is_error: false,
    }
}

fn resource_read_output(result: &Value) -> ToolOutput {
    let blocks: Vec<Value> = result["contents"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|resource| serde_json::json!({"type": "resource", "resource": resource}))
        .collect();
    let mut output = content::tool_output(&serde_json::json!({"content": blocks}));
    let shaping = output.details.take();
    output.details = Some(serde_json::json!({"result": result, "shaping": shaping}));
    output
}

#[async_trait]
impl Tool for McpContextTool {
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
        serde_json::json!({
            "type": "object",
            "required": ["action"],
            "additionalProperties": false,
            "properties": {
                "action": {"type": "string", "enum": ["list_resources", "list_resource_templates", "read_resource"]},
                "cursor": {"type": "string", "description": "Opaque nextCursor from this server's previous page, including empty strings"},
                "uri": {"type": "string", "description": "Exact resource URI to read through this MCP server"}
            }
        })
    }

    fn effects(&self) -> ToolEffects {
        // External server requests remain barriers, even for read-shaped RPC.
        ToolEffects::network().union(ToolEffects::process())
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> crate::error::Result<ToolOutput> {
        let action: McpContextAction = serde_json::from_value(input).map_err(|_| {
            crate::error::Error::tool(
                "mcp",
                "[MCP_REQUEST_INVALID] expected a resource action with its cursor or URI",
            )
        })?;
        match action {
            McpContextAction::ListResources { cursor } => {
                let result = self
                    .manager
                    .list_resources(&self.server, cursor.as_deref())
                    .await?;
                Ok(resource_catalog_output(result, false))
            }
            McpContextAction::ListResourceTemplates { cursor } => {
                let result = self
                    .manager
                    .list_resource_templates(&self.server, cursor.as_deref())
                    .await?;
                Ok(resource_catalog_output(result, true))
            }
            McpContextAction::ReadResource { uri } => {
                let result = self.manager.read_resource(&self.server, &uri).await?;
                Ok(resource_read_output(&result))
            }
        }
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
    for server in manager.context_server_names(None) {
        out.push(Box::new(McpContextTool::new(&server, manager.clone())));
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
    let mut out: Vec<Box<dyn Tool>> = manager
        .mounted_tool_metas()
        .into_iter()
        .find(|(server, _)| server == server_name)
        .into_iter()
        .flat_map(|(server, metas)| {
            metas.into_iter().map(move |meta| {
                Box::new(McpTool::new(&server, &meta, manager.clone())) as Box<dyn Tool>
            })
        })
        .collect();
    for server in manager.context_server_names(Some(server_name)) {
        out.push(Box::new(McpContextTool::new(&server, manager.clone())));
    }
    out
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
    fn result_shaping_error_and_invalid_media() {
        let out = mcp_result_to_output(&serde_json::json!({
            "content": [{"type": "image", "data": "..."}],
            "isError": true
        }));
        assert!(out.is_error);
        // A malformed media result is explicit, never a raw base64 dump.
        let text = out.content.first().and_then(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        });
        assert!(text.is_some_and(|t| t.contains("MCP_CONTENT_INVALID")));
        assert_eq!(out.details.as_ref().unwrap()["nonTextBlocks"], 1);
    }
}
