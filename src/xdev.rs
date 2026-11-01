//! Tool load modes + the `xdev` dispatcher (bd-cv653.1.6).
//!
//! As the built-in tool surface grows, sending every tool's full JSON schema
//! on every provider request costs thousands of tokens per turn and measurably
//! degrades model tool-selection. Load modes split the surface in two:
//!
//! - **Essential** tools are always in the schema (read/write/edit/bash/
//!   grep/find/ls/hashline_edit/ask/todo/xdev).
//! - **Discoverable** tools stay out of the schema; a compact index in the
//!   system prompt (name + one-line purpose) advertises them, and the single
//!   `xdev` dispatcher tool lists, describes, runs, and promotes them.
//!
//! `xdev run` delegates through the agent's normal execution path (effects,
//! approval, logging all preserved — the agent intercepts the dispatch);
//! `xdev promote <name>` moves a discoverable tool into the live schema
//! mid-session without a restart. `--tools` pins the exact set and wins over
//! every tier default.

use crate::config::Config;
use crate::error::Error;
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolOutput, ToolUpdate};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::Path;
use std::path::PathBuf;

/// Load tier for a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadMode {
    /// Always present in the provider schema.
    Essential,
    /// Hidden from the schema; reachable via `xdev` and promotable.
    Discoverable,
    /// Not enabled at all.
    Off,
}

impl LoadMode {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "essential" => Some(Self::Essential),
            "discoverable" => Some(Self::Discoverable),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Essential => "essential",
            Self::Discoverable => "discoverable",
            Self::Off => "off",
        }
    }
}

/// Tools that are always in the schema by default.
const ESSENTIAL_DEFAULTS: &[&str] = &[
    "read",
    "write",
    "edit",
    "bash",
    "grep",
    "find",
    "ls",
    "hashline_edit",
    "ask",
    "todo",
    "xdev",
    // Daily-driver research tool (bd-cv653.2.1).
    "web_search",
    // Tiny schema, required the moment plan mode activates (bd-cv653.3.5).
    "submit_plan",
    // Zero-parameter clock read (gh #207, #103); cheaper to keep in the schema than
    // to make the model discover it through xdev before every date/time question.
    "current_time",
];

/// Tools that are opt-in ONLY (never in the default enabled set, never
/// discoverable-by-default): they spawn additional agent processes.
const OPT_IN_ONLY: &[&str] = &["subagent"];

/// The default tier for a tool name (before config overrides).
#[must_use]
pub fn default_tier(name: &str) -> LoadMode {
    if ESSENTIAL_DEFAULTS.contains(&name) {
        return LoadMode::Essential;
    }
    if OPT_IN_ONLY.contains(&name) {
        return LoadMode::Off;
    }
    LoadMode::Discoverable
}

/// Explicit `tools.loadMode.<name>` override, without the default tier.
///
/// An absent or unknown override returns `None`. This distinction matters for
/// opt-in tools: their default `Off` tier must not veto an explicit activation.
#[must_use]
pub fn configured_tier(name: &str, config: Option<&Config>) -> Option<LoadMode> {
    config
        .and_then(|c| c.tools.as_ref())
        .and_then(|t| t.load_mode.as_ref())
        .and_then(|modes| modes.get(name))
        .and_then(|raw| LoadMode::from_name(raw))
}

/// Effective tier for a tool: `tools.loadMode.<name>` override > default
/// table. Unknown override values are ignored (a warning is the caller's job).
#[must_use]
pub fn tier_for(name: &str, config: Option<&Config>) -> LoadMode {
    configured_tier(name, config).unwrap_or_else(|| default_tier(name))
}

/// Names of built-in tools enabled by default when the user passes no
/// `--tools`: the essential set plus the discoverable set. Opt-in-only tools
/// (subagent) stay out.
#[must_use]
pub fn default_enabled_tools() -> Vec<&'static str> {
    vec![
        "read",
        "bash",
        "edit",
        "write",
        "grep",
        "find",
        "ls",
        "hashline_edit",
        "web_search",
        "ast_grep",
        "ast_edit",
        "lsp",
        "debug",
        "ask",
        "todo",
        "submit_plan",
        "jobs",
        "hub",
        "current_time",
    ]
}

/// Curated one-line purposes for built-in tools, used for the system-prompt
/// discoverable index (bd-cv653.1.6). Kept honest by a unit test that
/// compares each entry against the live tool description.
#[must_use]
pub fn builtin_one_liner(name: &str) -> Option<&'static str> {
    Some(match name {
        "read" => {
            "Read local files, document URLs, ZIP archive members, images (jpg, png, gif, webp), and v…"
        }
        "write" => "Write content to a file, creating parent directories as needed",
        "edit" => "Edit a file by replacing text",
        "bash" => "Execute a bash command",
        "grep" => {
            "Search files, internal documents (agent://, skill://, local://, pr://, and other read URL…"
        }
        "find" => "Search for files by glob pattern",
        "ls" => "List directory contents",
        "hashline_edit" => {
            "Apply precise file edits using LINE#HASH tags from a prior read with hashline=true"
        }
        "eval" => "Run a Python or JavaScript cell in a persistent kernel",
        "github" => {
            "Structured GitHub operations via the gh CLI: pr_view, issue_view, pr_diff, search (code/i…"
        }
        "security_scan" => "Review local source with plan/run/disposition/compare",
        "web_search" => "Search the web",
        "ast_grep" => "Structural code search using tree-sitter AST patterns (ast-grep syntax)",
        "ast_edit" => {
            "Staged structural code rewrite using tree-sitter AST patterns (ast-grep syntax)"
        }
        "lsp" => {
            "IDE-grade code intelligence via language servers: diagnostics, definition, references, ho…"
        }
        "debug" => {
            "Drive a real native/Python/Go debugger: launch/attach, retained breakpoints, thread-aware…"
        }
        "jobs" => {
            "Manage this session's background bash jobs, async subagent tasks and retained eval model …"
        }
        "hub" => "Supervise long-running processes and manage background jobs",
        "subagent" => "Delegate an isolated task to a named Pi child agent",
        "ask" => "Ask the user structured questions mid-turn instead of guessing",
        "todo" => "Maintain the session task list",
        "xdev" => "Discover and run rarely-used tools that are not in the live schema",
        "current_time" => {
            "Return the host's current wall-clock time: UTC and local ISO-8601 timestamps, UTC offset,…"
        }
        _ => return None,
    })
}

/// The system-prompt discoverable index: `(name, one-liner)` for tools that
/// are enabled AND discoverable-tier AND known built-ins. Extension/custom
/// tools still surface via `xdev list` at runtime. When the dispatcher is
/// disabled, there is no discovery route to advertise.
#[must_use]
pub fn prompt_index_for(enabled: &[&str], config: Option<&Config>) -> Vec<(String, String)> {
    if tier_for("xdev", config) == LoadMode::Off {
        return Vec::new();
    }
    enabled
        .iter()
        .filter(|name| tier_for(name, config) == LoadMode::Discoverable)
        .filter_map(|name| builtin_one_liner(name).map(|line| (name.to_string(), line.to_string())))
        .collect()
}

/// One-line summary of a tool for the system-prompt discoverable index.
#[must_use]
pub fn one_liner(description: &str) -> String {
    const MAX: usize = 90;
    let first = description
        .split(['.', '\n'])
        .next()
        .unwrap_or(description)
        .trim();
    if first.chars().count() <= MAX {
        first.to_string()
    } else {
        let mut out: String = first.chars().take(MAX - 1).collect();
        out.push('…');
        out
    }
}

fn text_output(text: &str, is_error: bool) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: None,
        is_error,
    }
}

/// Snapshot of a discoverable tool for the xdev dispatcher's list/describe.
#[derive(Debug, Clone)]
pub struct DiscoverableToolInfo {
    pub name: String,
    pub one_liner: String,
    pub description: String,
    pub parameters: Value,
}

/// The `xdev` dispatcher tool.
///
/// `list` and `describe` are handled inline from the snapshot; `run` and
/// `promote` are intercepted by the agent executor so the inner tool executes
/// through the normal path (approval, effects, logs). If an `xdev` run/promote
/// call reaches this `execute` directly (a host that did not install the
/// interception), it fails loudly instead of silently doing nothing.
pub struct XdevTool {
    #[allow(dead_code)]
    cwd: PathBuf,
    discoverable: Vec<DiscoverableToolInfo>,
}

impl XdevTool {
    #[must_use]
    pub fn new(cwd: &Path, discoverable: Vec<DiscoverableToolInfo>) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            discoverable,
        }
    }

    #[must_use]
    pub fn discoverable(&self) -> &[DiscoverableToolInfo] {
        &self.discoverable
    }

    fn list_output(&self) -> ToolOutput {
        let mut lines = String::from(
            "Discoverable tools (not in the live schema; use xdev run to call one, xdev promote to add it to the schema):\n",
        );
        for info in &self.discoverable {
            let _ = std::fmt::Write::write_fmt(
                &mut lines,
                format_args!("- {}: {}\n", info.name, info.one_liner),
            );
        }
        if self.discoverable.is_empty() {
            lines.push_str("(none)\n");
        }
        text_output(&lines, false)
    }

    fn describe_output(&self, name: &str) -> ToolOutput {
        self.discoverable
            .iter()
            .find(|info| info.name == name)
            .map_or_else(
                || {
                    text_output(
                        &format!(
                            "No discoverable tool named {name:?}. Use xdev list to see available tools."
                        ),
                        true,
                    )
                },
                |info| {
                    text_output(
                        &json!({
                            "name": info.name,
                            "description": info.description,
                            "parameters": info.parameters,
                            "tier": LoadMode::Discoverable.as_str(),
                        })
                        .to_string(),
                        false,
                    )
                },
            )
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for XdevTool {
    fn name(&self) -> &str {
        "xdev"
    }

    fn label(&self) -> &str {
        "xdev"
    }

    fn description(&self) -> &str {
        "Discover and run rarely-used tools that are not in the live schema. \
         actions: list (show discoverable tools with one-line purposes), \
         describe <name> (full description + JSON schema), run <name> <args> \
         (execute the tool through the normal path with its own validation), \
         promote <name> (add the tool to the live schema for the rest of the \
         session). Prefer direct tool calls for essential tools; use xdev for \
         anything not in your schema."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "describe", "run", "promote"],
                    "description": "Dispatcher action"
                },
                "name": {
                    "type": "string",
                    "description": "Target tool name (required for describe/run/promote)"
                },
                "args": {
                    "type": "object",
                    "description": "Arguments for the target tool (required for run)"
                }
            },
            "required": ["action"]
        })
    }

    fn effects(&self) -> crate::tools::ToolEffects {
        // Conservative union: a discoverable tool may do anything, so xdev
        // advertises the full effect set (approval gates stay honest).
        crate::tools::ToolEffects::read()
            .union(crate::tools::ToolEffects::write())
            .union(crate::tools::ToolEffects::append())
            .union(crate::tools::ToolEffects::network())
            .union(crate::tools::ToolEffects::process())
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> crate::error::Result<ToolOutput> {
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("list");
        match action {
            "list" => Ok(self.list_output()),
            "describe" => {
                let name = input.get("name").and_then(Value::as_str).unwrap_or("");
                if name.is_empty() {
                    return Ok(text_output("xdev describe requires a `name`", true));
                }
                Ok(self.describe_output(name))
            }
            "run" | "promote" => Err(Error::tool(
                "xdev",
                "xdev run/promote must be dispatched by the agent executor; \
                 this host did not install the interception"
                    .to_string(),
            )),
            other => Ok(text_output(
                &format!("Unknown xdev action {other:?}; expected list|describe|run|promote"),
                true,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;
    use std::collections::HashMap;

    #[test]
    fn every_tiered_name_is_a_name_the_tools_flag_knows_about() {
        // AGENTS.md already requires the README inventory and these two
        // constants to stay in step. This makes the `--tools` vocabulary the
        // third: a name tiered here but absent from both lists in tools.rs is
        // a tool the flag reports as nonexistent, which is what
        // bd-tools-known-names-missing-seven-mbo3g was about. `xdev` itself
        // was in exactly that state while sitting in ESSENTIAL_DEFAULTS.
        use crate::tools::ToolRegistry;
        let mut missing = Vec::new();
        for name in ESSENTIAL_DEFAULTS.iter().chain(OPT_IN_ONLY) {
            let known = ToolRegistry::KNOWN_TOOL_NAMES.contains(name);
            let unselectable = ToolRegistry::TOOLS_NOT_SELECTED_BY_FLAG
                .iter()
                .any(|(listed, _)| listed == name);
            if !known && !unselectable {
                missing.push(*name);
            }
        }
        assert!(
            missing.is_empty(),
            "tiered here but unknown to --tools: {missing:?}; add each to \
             ToolRegistry::KNOWN_TOOL_NAMES, or to TOOLS_NOT_SELECTED_BY_FLAG \
             with a line saying what does turn it on"
        );
    }

    #[test]
    fn default_tiers_cover_core_and_opt_in() {
        assert_eq!(default_tier("read"), LoadMode::Essential);
        assert_eq!(default_tier("bash"), LoadMode::Essential);
        assert_eq!(default_tier("xdev"), LoadMode::Essential);
        assert_eq!(default_tier("ask"), LoadMode::Essential);
        assert_eq!(default_tier("subagent"), LoadMode::Off);
        assert_eq!(default_tier("web_search"), LoadMode::Essential);
        assert_eq!(default_tier("ast_grep"), LoadMode::Discoverable);
        assert_eq!(default_tier("lsp"), LoadMode::Discoverable);
    }

    #[test]
    fn config_override_beats_default() {
        let mut modes = HashMap::new();
        modes.insert("ast_grep".to_string(), "essential".to_string());
        modes.insert("bash".to_string(), "discoverable".to_string());
        modes.insert("find".to_string(), "off".to_string());
        let config = Config {
            tools: Some(crate::config::ToolSettings {
                load_mode: Some(modes),
                ..Default::default()
            }),
            ..Config::default()
        };
        assert_eq!(tier_for("ast_grep", Some(&config)), LoadMode::Essential);
        assert_eq!(tier_for("bash", Some(&config)), LoadMode::Discoverable);
        assert_eq!(tier_for("find", Some(&config)), LoadMode::Off);
        // Unoverridden names keep the default.
        assert_eq!(tier_for("read", Some(&config)), LoadMode::Essential);
        // Unknown override values are ignored.
        let mut bad = HashMap::new();
        bad.insert("grep".to_string(), "sometimes".to_string());
        let config = Config {
            tools: Some(crate::config::ToolSettings {
                load_mode: Some(bad),
                ..Default::default()
            }),
            ..Config::default()
        };
        assert_eq!(tier_for("grep", Some(&config)), LoadMode::Essential);
    }

    #[test]
    fn configured_tier_distinguishes_missing_invalid_and_explicit_off() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "tools": {
                "loadMode": {
                    "eval": " OFF ",
                    "bash": "DISCOVERABLE",
                    "subagent": "not-a-load-mode"
                }
            }
        }))
        .expect("load mode config");
        assert_eq!(configured_tier("eval", Some(&config)), Some(LoadMode::Off));
        assert_eq!(
            configured_tier("bash", Some(&config)),
            Some(LoadMode::Discoverable)
        );
        assert_eq!(configured_tier("subagent", Some(&config)), None);
        assert_eq!(configured_tier("read", Some(&config)), None);
        assert_eq!(configured_tier("subagent", None), None);
        assert_eq!(tier_for("subagent", Some(&config)), LoadMode::Off);
    }

    #[test]
    fn one_liner_truncates_at_sentence_and_cap() {
        assert_eq!(one_liner("Short one."), "Short one");
        assert_eq!(one_liner("First sentence. Second."), "First sentence");
        let long = "x".repeat(200);
        let out = one_liner(&long);
        assert!(out.chars().count() <= 90);
    }

    #[test]
    fn default_enabled_excludes_opt_in_only() {
        let enabled = default_enabled_tools();
        assert!(!enabled.contains(&"subagent"));
        assert!(enabled.contains(&"read"));
        assert!(enabled.contains(&"ast_grep"));
    }

    const OPTIONAL_TOOL_SETTINGS: [(&str, &str); 4] = [
        ("enableEval", "eval"),
        ("enableGithub", "github"),
        ("enableSecurityScan", "security_scan"),
        ("enableSubagent", "subagent"),
    ];

    fn optional_tool_config(enabled: bool) -> Config {
        serde_json::from_value(json!({
            "tools": {
                "enableEval": enabled,
                "enableGithub": enabled,
                "enableSecurityScan": enabled,
                "enableSubagent": enabled
            }
        }))
        .expect("optional tool settings")
    }

    fn selected_registry_and_prompt(
        cli: &crate::cli::Cli,
        config: &Config,
        cwd: &Path,
    ) -> (crate::tools::ToolRegistry, String) {
        let enabled = cli.enabled_tools(config);
        let registry = if cli.builtin_tools_disabled() {
            crate::tools::ToolRegistry::without_builtins(Some(config))
        } else {
            crate::tools::ToolRegistry::new(&enabled, cwd, Some(config))
        };
        let prompt = crate::app::build_system_prompt(
            cli, cwd, &enabled, None, cwd, cwd, true, false, None, config,
        )
        .expect("native system prompt");
        (registry, prompt)
    }

    #[test]
    fn settings_enabled_optional_tools_reach_the_native_prompt() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = crate::cli::Cli::parse_from(["pi"]);
        for (setting, name) in OPTIONAL_TOOL_SETTINGS {
            let config: Config = serde_json::from_value(json!({
                "tools": { (setting): true }
            }))
            .expect("optional tool settings");
            let (registry, prompt) = selected_registry_and_prompt(&cli, &config, temp.path());
            let tool = registry.get(name).expect("configured optional tool");
            let live_line = one_liner(tool.description());
            assert_eq!(
                builtin_one_liner(name),
                Some(live_line.as_str()),
                "one-liner drift for {name}"
            );
            assert!(
                prompt.contains(&format!("- {name}:")),
                "{name} is advertised"
            );
            if name == "subagent" {
                assert!(!registry.is_discoverable(name));
                assert!(
                    !prompt_index_for(&cli.enabled_tools(&config), Some(&config))
                        .iter()
                        .any(|(listed, _)| listed == name)
                );
            } else {
                assert!(registry.is_discoverable(name));
                assert!(registry.get("xdev").is_some());
                assert!(
                    prompt.contains(&format!("- {name}: {live_line}\n")),
                    "discovery must advertise the enabled {name}"
                );
            }
        }
    }

    #[test]
    fn optional_tool_discovery_respects_defaults_and_explicit_selection() {
        let temp = tempfile::tempdir().expect("tempdir");
        let assert_absent = |cli: &crate::cli::Cli, config: &Config| {
            let (registry, prompt) = selected_registry_and_prompt(cli, config, temp.path());
            for (_, name) in OPTIONAL_TOOL_SETTINGS {
                assert!(registry.get(name).is_none(), "unexpected {name}");
                assert!(
                    !prompt.contains(&format!("- {name}:")),
                    "unexpected {name} advert"
                );
            }
        };
        let cli = crate::cli::Cli::parse_from(["pi"]);
        for config in [Config::default(), optional_tool_config(false)] {
            assert_absent(&cli, &config);
        }

        let configured = optional_tool_config(true);
        let defaults = default_enabled_tools().join(",");
        for tools in ["read", defaults.as_str(), "", " , , "] {
            let cli = crate::cli::Cli::parse_from(["pi", "--tools", tools]);
            assert_absent(&cli, &configured);
        }
        let cli = crate::cli::Cli::parse_from(["pi", "--no-tools"]);
        assert_absent(&cli, &configured);

        let cli = crate::cli::Cli::parse_from([
            "pi",
            "--tools",
            "eval,github,security_scan,subagent",
        ]);
        let (registry, prompt) =
            selected_registry_and_prompt(&cli, &optional_tool_config(false), temp.path());
        for (_, name) in OPTIONAL_TOOL_SETTINGS {
            assert!(registry.get(name).is_some(), "explicit {name}");
            assert!(
                prompt.contains(&format!("- {name}:")),
                "explicit {name} advert"
            );
        }
    }

    #[test]
    fn optional_tool_discovery_follows_load_mode_without_enabling_tools() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = crate::cli::Cli::parse_from(["pi"]);
        for (setting, name) in OPTIONAL_TOOL_SETTINGS {
            for mode in ["essential", "discoverable", "off"] {
                for enabled in [false, true] {
                    let config: Config = serde_json::from_value(json!({
                        "tools": { (setting): enabled, "loadMode": { (name): mode } }
                    }))
                    .expect("optional tool load mode");
                    let (registry, prompt) =
                        selected_registry_and_prompt(&cli, &config, temp.path());
                    let registered = enabled && mode != "off";
                    let discoverable = registered && mode == "discoverable";
                    assert_eq!(registry.get(name).is_some(), registered, "{name} {mode}");
                    assert_eq!(registry.is_discoverable(name), discoverable, "{name} {mode}");
                    assert_eq!(
                        prompt_index_for(&cli.enabled_tools(&config), Some(&config))
                            .iter()
                            .any(|(listed, _)| listed == name),
                        discoverable,
                        "{name} {mode}"
                    );
                    if discoverable {
                        let line = one_liner(
                            registry.get(name).expect("discoverable tool").description(),
                        );
                        assert!(prompt.contains(&format!("- {name}: {line}\n")));
                    } else if !registered {
                        assert!(!prompt.contains(&format!("- {name}:")));
                    }
                }
            }

            // An explicit tool list also overrides a configured Off entry.
            let config: Config = serde_json::from_value(json!({
                "tools": { (setting): false, "loadMode": { (name): "off" } }
            }))
            .expect("explicit optional tool settings");
            let cli = crate::cli::Cli::parse_from(["pi", "--tools", name]);
            let (registry, _) = selected_registry_and_prompt(&cli, &config, temp.path());
            assert!(registry.get(name).is_some(), "explicit {name}");
            assert!(
                !registry.is_discoverable(name),
                "explicit off-tier {name} stays live"
            );
        }
    }

    #[test]
    fn disabled_dispatcher_is_not_advertised_for_enabled_optional_tools() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = optional_tool_config(true);
        config.tools.as_mut().expect("tool settings").load_mode = Some(HashMap::from([(
            "xdev".to_string(),
            " OFF ".to_string(),
        )]));
        for args in [
            vec!["pi"],
            vec!["pi", "--tools", "eval,github,security_scan,subagent"],
        ] {
            let cli = crate::cli::Cli::parse_from(args);
            let (registry, prompt) = selected_registry_and_prompt(&cli, &config, temp.path());
            for (_, name) in OPTIONAL_TOOL_SETTINGS {
                assert!(registry.get(name).is_some(), "{name} remains selected");
            }
            assert!(registry.is_discoverable("eval"));
            assert!(registry.get("xdev").is_none());
            assert!(prompt_index_for(&cli.enabled_tools(&config), Some(&config)).is_empty());
            assert!(!prompt.contains("Additional tools are available via the `xdev` dispatcher"));
            assert!(!prompt.contains("Use `xdev` with action"));
        }
    }

    #[test]
    fn one_liner_table_matches_live_descriptions() {
        // Anti-drift gate: every built-in that exists in a default registry
        // must have its curated one-liner equal the live description's first
        // sentence (so the prompt index can never lie about a tool).
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = crate::tools::ToolRegistry::new(&default_enabled_tools(), temp.path(), None);
        let mut checked = 0;
        for tool in registry.tools() {
            if let Some(line) = builtin_one_liner(tool.name()) {
                assert_eq!(
                    line,
                    one_liner(tool.description()),
                    "one-liner table drifted from live description for {}",
                    tool.name()
                );
                checked += 1;
            }
        }
        assert!(
            checked >= 10,
            "expected to check the built-ins, got {checked}"
        );
    }
}
