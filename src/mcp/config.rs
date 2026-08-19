//! MCP server configuration: discovery, parsing, merge, and precedence.
//!
//! Sources in precedence order (highest first, bd-cv653.6.1):
//!
//! 1. `--mcp-config <path>` (CLI, repeatable)
//! 2. `.pi/mcp.json` (project native)
//! 3. `.agents/mcp.json` (project cross-agent convention)
//! 4. `~/.pi/agent/mcp.json` (global native)
//! 5. Foreign files (`.claude/mcp.json`, `.cursor/mcp.json`,
//!    `.windsurf/mcp.json`, `.gemini/settings.json`, `.codex/config.toml`
//!    under the project) — marked `provenance=foreign`.
//!
//! Merge semantics: per server name, the highest-precedence source wins the
//! whole definition; every server records where it came from. Malformed
//! entries are skipped with a warning record — they never abort the load.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

/// Where a server definition came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// `--mcp-config` CLI file.
    Cli,
    /// `.pi/mcp.json` in the project.
    ProjectPi,
    /// `.agents/mcp.json` in the project.
    ProjectAgents,
    /// `~/.pi/agent/mcp.json`.
    GlobalPi,
    /// A foreign tool's config file (`.claude/`, `.cursor/`, ...).
    Foreign,
    /// Contributed by an installed extension via `registerMcpServer`.
    Extension,
}

impl Provenance {
    /// Display label for the `/mcp` view.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::ProjectPi => ".pi",
            Self::ProjectAgents => ".agents",
            Self::GlobalPi => "global",
            Self::Foreign => "foreign",
            Self::Extension => "extension",
        }
    }

    /// Whether this provenance is one of pi's native files.
    #[must_use]
    pub const fn is_native(self) -> bool {
        !matches!(self, Self::Foreign | Self::Extension)
    }
}

/// One server definition after merging.
#[derive(Debug, Clone)]
pub struct ConfiguredServer {
    /// Server name (config map key).
    pub name: String,
    /// Spawn command (stdio servers).
    pub command: Option<String>,
    /// argv for the command.
    pub args: Vec<String>,
    /// Extra environment entries (values may use `$ENV:`/`$CMD:`).
    pub env: Vec<(String, String)>,
    /// Endpoint URL (HTTP servers).
    pub url: Option<String>,
    /// Extra HTTP headers (values may use `$ENV:`/`$CMD:`).
    pub headers: Vec<(String, String)>,
    /// Explicit transport hint (`"stdio"` / `"http"` / `"sse"`).
    pub transport_hint: Option<String>,
    /// Where the definition came from.
    pub provenance: Provenance,
    /// Source file it was read from.
    pub source_file: PathBuf,
}

impl ConfiguredServer {
    /// Stable fingerprint of the spawn target: a trust decision binds to
    /// this, so any config change re-prompts.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.command.hash(&mut hasher);
        self.args.hash(&mut hasher);
        self.url.hash(&mut hasher);
        self.transport_hint.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    /// Whether this is an HTTP(-family) server.
    #[must_use]
    pub fn is_http(&self) -> bool {
        self.url.is_some()
            || matches!(
                self.transport_hint.as_deref(),
                Some("http" | "sse" | "streamable-http")
            )
    }
}

/// A skipped entry, surfaced in `/mcp` and logs instead of aborting.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigWarning {
    pub source_file: PathBuf,
    pub entry: String,
    pub reason: String,
}

/// The raw file shape: `{"mcpServers": {...}}` or a bare server map.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpFile {
    #[serde(default)]
    mcp_servers: HashMap<String, Value>,
}

/// One raw server entry (tolerant: unknown fields ignored).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawServer {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    env: Option<HashMap<String, String>>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: Option<HashMap<String, String>>,
    #[serde(default, rename = "type")]
    transport: Option<String>,
}

/// Parse one server entry; `Err` carries the skip reason.
fn parse_server(name: &str, raw: &Value) -> std::result::Result<RawServer, String> {
    serde_json::from_value(raw.clone()).map_err(|err| format!("entry {name:?}: {err}"))
}

/// Load one config file. Missing file → empty; malformed JSON → one warning.
fn load_file(
    path: &Path,
    provenance: Provenance,
    out: &mut Vec<(String, RawServer, Provenance, PathBuf)>,
    warnings: &mut Vec<ConfigWarning>,
) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return; // absent files are normal
    };
    // TOML is only supported for .codex/config.toml via a minimal parse: the
    // [mcp_servers.NAME] tables. Everything else is JSON.
    let parsed: std::result::Result<Value, String> =
        if path.extension().is_some_and(|e| e == "toml") {
            Ok(parse_codex_toml(&content))
        } else {
            serde_json::from_str(&content).map_err(|err| format!("invalid JSON: {err}"))
        };
    let value = match parsed {
        Ok(value) => value,
        Err(reason) => {
            warnings.push(ConfigWarning {
                source_file: path.to_path_buf(),
                entry: "<file>".to_string(),
                reason,
            });
            return;
        }
    };
    // Accept both `{"mcpServers": {...}}` and a bare `{name: {...}}` map.
    let servers: HashMap<String, Value> = serde_json::from_value::<McpFile>(value.clone())
        .map(|file| file.mcp_servers)
        .unwrap_or_default();
    let servers = if servers.is_empty() {
        // Bare-map form: values that look like server objects.
        value
            .as_object()
            .map(|map| {
                map.iter()
                    .filter(|(_, v)| v.get("command").is_some() || v.get("url").is_some())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        servers
    };
    for (name, raw) in servers {
        match parse_server(&name, &raw) {
            Ok(server) => out.push((name, server, provenance, path.to_path_buf())),
            Err(reason) => warnings.push(ConfigWarning {
                source_file: path.to_path_buf(),
                entry: name,
                reason,
            }),
        }
    }
}

/// Minimal TOML extraction for `.codex/config.toml` `[mcp_servers.NAME]`
/// tables (string values and string arrays only — the MCP surface).
/// Never fails: unrecognized lines are ignored.
fn parse_codex_toml(content: &str) -> Value {
    let mut servers = serde_json::Map::new();
    let mut current: Option<(String, serde_json::Map<String, Value>)> = None;
    let flush = |current: Option<(String, serde_json::Map<String, Value>)>,
                 servers: &mut serde_json::Map<String, Value>| {
        if let Some((name, table)) = current {
            servers.insert(name, Value::Object(table));
        }
    };
    for line in content.lines() {
        let line = line.trim();
        if let Some(name) = line
            .strip_prefix("[mcp_servers.")
            .and_then(|rest| rest.strip_suffix(']'))
        {
            let finished = current.take();
            flush(finished, &mut servers);
            current = Some((name.trim_matches('"').to_string(), serde_json::Map::new()));
            continue;
        }
        if line.starts_with('[') {
            let finished = current.take();
            flush(finished, &mut servers);
            continue;
        }
        if let (Some((_, table)), Some((key, value))) = (current.as_mut(), line.split_once('=')) {
            let key = key.trim().to_string();
            let value = value.trim();
            let parsed = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .map_or_else(
                    || {
                        if value.starts_with('[') {
                            serde_json::from_str(&value.replace('\'', "\"")).unwrap_or(Value::Null)
                        } else {
                            Value::String(value.trim_matches('"').to_string())
                        }
                    },
                    |stripped| Value::String(stripped.to_string()),
                );
            table.insert(key, parsed);
        }
    }
    flush(current, &mut servers);
    Value::Object(serde_json::Map::from_iter([(
        "mcpServers".to_string(),
        Value::Object(servers),
    )]))
}

/// Foreign discovery candidates under the project root.
const FOREIGN_PROJECT_FILES: &[&str] = &[
    ".claude/mcp.json",
    ".cursor/mcp.json",
    ".windsurf/mcp.json",
    ".gemini/settings.json",
    ".codex/config.toml",
];

/// The merged discovery result.
#[derive(Debug, Default)]
pub struct McpDiscovery {
    /// Servers keyed by name, highest-precedence definition per name.
    pub servers: Vec<ConfiguredServer>,
    /// Non-fatal load problems (malformed entries/files).
    pub warnings: Vec<ConfigWarning>,
}

/// Discover and merge MCP server configs.
///
/// `cli_paths`: `--mcp-config` files (repeatable, highest precedence).
/// `global_dir`: the pi global agent dir (`~/.pi/agent`).
#[must_use]
pub fn discover(cwd: &Path, global_dir: &Path, cli_paths: &[PathBuf]) -> McpDiscovery {
    let mut layered: Vec<(String, RawServer, Provenance, PathBuf)> = Vec::new();
    let mut warnings = Vec::new();

    // Precedence high → low. Later layers only fill names not already set.
    for path in cli_paths {
        load_file(path, Provenance::Cli, &mut layered, &mut warnings);
    }
    load_file(
        &cwd.join(".pi/mcp.json"),
        Provenance::ProjectPi,
        &mut layered,
        &mut warnings,
    );
    load_file(
        &cwd.join(".agents/mcp.json"),
        Provenance::ProjectAgents,
        &mut layered,
        &mut warnings,
    );
    load_file(
        &global_dir.join("mcp.json"),
        Provenance::GlobalPi,
        &mut layered,
        &mut warnings,
    );
    for foreign in FOREIGN_PROJECT_FILES {
        load_file(
            &cwd.join(foreign),
            Provenance::Foreign,
            &mut layered,
            &mut warnings,
        );
    }

    // First occurrence wins (layers were loaded high → low precedence).
    let mut seen = std::collections::HashSet::new();
    let mut servers = Vec::new();
    for (name, raw, provenance, source_file) in layered {
        if !seen.insert(name.clone()) {
            continue;
        }
        servers.push(ConfiguredServer {
            name,
            command: raw.command,
            args: raw.args.unwrap_or_default(),
            env: raw
                .env
                .map(|env| env.into_iter().collect())
                .unwrap_or_default(),
            url: raw.url,
            headers: raw
                .headers
                .map(|headers| headers.into_iter().collect())
                .unwrap_or_default(),
            transport_hint: raw.transport,
            provenance,
            source_file,
        });
    }
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    McpDiscovery { servers, warnings }
}

/// Write-side view of a project-native config file (for `/mcp add|remove`):
/// read-modify-write `.pi/mcp.json` preserving unrelated content.
///
/// # Errors
///
/// Returns an error when the file exists but is not valid JSON.
pub fn read_project_config(path: &Path) -> Result<Value, crate::error::Error> {
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).map_err(|err| {
            crate::error::Error::tool(
                "mcp",
                format!("[MCP_CONFIG_INVALID] {}: {err}", path.display()),
            )
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(serde_json::json!({ "mcpServers": {} }))
        }
        Err(err) => Err(crate::error::Error::tool(
            "mcp",
            format!("[MCP_CONFIG_IO] cannot read {}: {err}", path.display()),
        )),
    }
}

/// Write the project config back (pretty JSON, parent dirs created).
///
/// # Errors
///
/// Returns an error on I/O failure.
pub fn write_project_config(path: &Path, value: &Value) -> Result<(), crate::error::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            crate::error::Error::tool(
                "mcp",
                format!("[MCP_CONFIG_IO] cannot create {}: {err}", parent.display()),
            )
        })?;
    }
    let rendered = serde_json::to_string_pretty(value).map_err(|err| {
        crate::error::Error::tool("mcp", format!("[MCP_CONFIG_IO] serialize failed: {err}"))
    })?;
    std::fs::write(path, format!("{rendered}\n")).map_err(|err| {
        crate::error::Error::tool(
            "mcp",
            format!("[MCP_CONFIG_IO] cannot write {}: {err}", path.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("dirs");
        }
        std::fs::write(path, content).expect("write");
    }

    #[test]
    fn project_beats_global_and_foreign_fills_gaps() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        let global = temp.path().join("global");
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"mcpServers": {"shared": {"command": "project-cmd"}, "only_project": {"command": "p2"}}}"#,
        );
        write(
            &global.join("mcp.json"),
            r#"{"mcpServers": {"shared": {"command": "global-cmd"}, "only_global": {"command": "g2"}}}"#,
        );
        write(
            &cwd.join(".claude/mcp.json"),
            r#"{"mcpServers": {"foreign_one": {"command": "f1"}, "only_project": {"command": "shadowed"}}}"#,
        );
        let discovery = discover(&cwd, &global, &[]);
        let by_name: HashMap<_, _> = discovery
            .servers
            .iter()
            .map(|s| (s.name.as_str(), s))
            .collect();
        assert_eq!(
            by_name["shared"].command.as_deref(),
            Some("project-cmd"),
            "project must beat global"
        );
        assert_eq!(by_name["shared"].provenance, Provenance::ProjectPi);
        assert_eq!(by_name["only_global"].command.as_deref(), Some("g2"));
        assert_eq!(by_name["foreign_one"].provenance, Provenance::Foreign);
        // Native definition shadows the foreign duplicate entirely.
        assert_eq!(
            by_name["only_project"].command.as_deref(),
            Some("p2"),
            "native must shadow foreign"
        );
    }

    #[test]
    fn cli_beats_everything() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        let global = temp.path().join("global");
        let cli = temp.path().join("cli.json");
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"mcpServers": {"s": {"command": "project"}}}"#,
        );
        write(&cli, r#"{"mcpServers": {"s": {"command": "cli"}}}"#);
        let discovery = discover(&cwd, &global, &[cli]);
        assert_eq!(discovery.servers.len(), 1);
        assert_eq!(discovery.servers[0].command.as_deref(), Some("cli"));
        assert_eq!(discovery.servers[0].provenance, Provenance::Cli);
    }

    #[test]
    fn malformed_entries_skip_and_warn_never_abort() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        let global = temp.path().join("global");
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"mcpServers": {"good": {"command": "ok"}, "bad": 42}}"#,
        );
        let discovery = discover(&cwd, &global, &[]);
        assert_eq!(discovery.servers.len(), 1, "good entry survives");
        assert_eq!(discovery.warnings.len(), 1, "bad entry warned");
        assert!(discovery.warnings[0].reason.contains("\"bad\""));
    }

    #[test]
    fn malformed_file_warns_without_losing_other_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        let global = temp.path().join("global");
        write(&cwd.join(".pi/mcp.json"), "{not json");
        write(
            &global.join("mcp.json"),
            r#"{"mcpServers": {"g": {"command": "ok"}}}"#,
        );
        let discovery = discover(&cwd, &global, &[]);
        assert_eq!(discovery.servers.len(), 1);
        assert_eq!(discovery.warnings.len(), 1);
    }

    #[test]
    fn bare_map_form_accepted() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"myserver": {"command": "bare-form"}}"#,
        );
        let discovery = discover(&cwd, &temp.path().join("g"), &[]);
        assert_eq!(discovery.servers.len(), 1);
        assert_eq!(discovery.servers[0].command.as_deref(), Some("bare-form"));
    }

    #[test]
    fn codex_toml_tables_parsed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        write(
            &cwd.join(".codex/config.toml"),
            "[mcp_servers.docs]\ncommand = \"docs-mcp\"\nargs = [\"--port\", \"8080\"]\n",
        );
        let discovery = discover(&cwd, &temp.path().join("g"), &[]);
        assert_eq!(discovery.servers.len(), 1);
        let server = &discovery.servers[0];
        assert_eq!(server.name, "docs");
        assert_eq!(server.command.as_deref(), Some("docs-mcp"));
        assert_eq!(server.args, vec!["--port", "8080"]);
        assert_eq!(server.provenance, Provenance::Foreign);
    }

    #[test]
    fn http_shape_detected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"mcpServers": {"remote": {"url": "https://mcp.example.com/sse", "headers": {"Authorization": "$ENV:MCP_TOKEN"}}}}"#,
        );
        let discovery = discover(&cwd, &temp.path().join("g"), &[]);
        assert!(discovery.servers[0].is_http());
        assert_eq!(discovery.servers[0].headers.len(), 1);
    }

    #[test]
    fn fingerprint_changes_with_target() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"mcpServers": {"s": {"command": "a"}}}"#,
        );
        let first = discover(&cwd, &temp.path().join("g"), &[]).servers[0].fingerprint();
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"mcpServers": {"s": {"command": "b"}}}"#,
        );
        let second = discover(&cwd, &temp.path().join("g"), &[]).servers[0].fingerprint();
        assert_ne!(first, second, "trust fingerprint must track the target");
    }

    #[test]
    fn write_then_read_project_config_roundtrip() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(".pi/mcp.json");
        let mut value = read_project_config(&path).expect("read absent");
        value["mcpServers"]["added"] = serde_json::json!({"command": "new-cmd"});
        write_project_config(&path, &value).expect("write");
        let reread = read_project_config(&path).expect("reread");
        assert_eq!(
            reread["mcpServers"]["added"]["command"].as_str(),
            Some("new-cmd")
        );
    }
}
