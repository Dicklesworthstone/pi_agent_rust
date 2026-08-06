//! CLI argument parsing using Clap.

use clap::error::ErrorKind;
use clap::{Parser, Subcommand};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionCliFlag {
    pub name: String,
    pub value: Option<String>,
}

impl ExtensionCliFlag {
    pub fn display_name(&self) -> String {
        format!("--{}", self.name)
    }
}

#[derive(Debug)]
pub struct ParsedCli {
    pub cli: Cli,
    pub extension_flags: Vec<ExtensionCliFlag>,
}

#[derive(Debug, Clone, Copy)]
struct LongOptionSpec {
    takes_value: bool,
    optional_value: bool,
}

const ROOT_SUBCOMMANDS: &[&str] = &[
    "install",
    "remove",
    "update",
    "update-index",
    "context-preview",
    "swarm-progress",
    "swarm-replay-preview",
    "validation-broker",
    "search",
    "info",
    "list",
    "config",
    "doctor",
    "migrate",
];

fn known_long_option(name: &str) -> Option<LongOptionSpec> {
    let (takes_value, optional_value) = match name {
        "version"
        | "continue"
        | "resume"
        | "no-session"
        | "no-migrations"
        | "no-mouse-capture"
        | "print"
        | "rpc"
        | "acp"
        | "verbose"
        | "no-tools"
        | "no-extensions"
        | "explain-extension-policy"
        | "explain-repair-policy"
        | "no-skills"
        | "no-prompt-templates"
        | "no-themes"
        | "list-providers"
        | "refresh-models"
        | "persist-models"
        | "hide-cwd-in-prompt" => (false, false),
        "provider"
        | "model"
        | "api-key"
        | "models"
        | "thinking"
        | "system-prompt"
        | "append-system-prompt"
        | "session"
        | "session-dir"
        | "session-durability"
        | "mode"
        | "tools"
        | "extension"
        | "extension-policy"
        | "repair-policy"
        | "skill"
        | "prompt-template"
        | "theme"
        | "theme-path"
        | "max-tool-iterations"
        | "request-timeout"
        | "export"
        | "fetch-models" => (true, false),
        "list-models" => (true, true),
        _ => return None,
    };
    Some(LongOptionSpec {
        takes_value,
        optional_value,
    })
}

fn is_known_short_flag(token: &str) -> bool {
    if !token.starts_with('-') || token.starts_with("--") {
        return false;
    }
    let body = &token[1..];
    if body.is_empty() {
        return false;
    }
    body.chars()
        .all(|ch| matches!(ch, 'v' | 'c' | 'r' | 'p' | 'e'))
}

fn short_flag_expects_value(token: &str) -> bool {
    if !is_known_short_flag(token) {
        return false;
    }

    let body = &token[1..];
    body.find('e')
        .is_some_and(|index| index.eq(&(body.len() - 1)))
}

fn is_negative_numeric_token(token: &str) -> bool {
    if !token.starts_with('-') || token.eq("-") || token.starts_with("--") {
        return false;
    }
    token.parse::<i64>().is_ok() || token.parse::<f64>().is_ok_and(f64::is_finite)
}

#[allow(clippy::too_many_lines)] // Argument normalization needs single-pass stateful parsing.
fn preprocess_extension_flags(raw_args: &[String]) -> (Vec<String>, Vec<ExtensionCliFlag>) {
    if raw_args.is_empty() {
        return (vec!["pi".to_string()], Vec::new());
    }
    let mut filtered = Vec::with_capacity(raw_args.len());
    filtered.push(raw_args[0].clone());
    let mut extracted = Vec::new();
    let mut expecting_value = false;
    let mut in_subcommand = false;
    let mut index = 1usize;
    while index < raw_args.len() {
        let token = &raw_args[index];
        if token.eq("--") {
            filtered.extend(raw_args[index..].iter().cloned());
            break;
        }
        if expecting_value {
            filtered.push(token.clone());
            expecting_value = false;
            index += 1;
            continue;
        }
        if in_subcommand {
            filtered.push(token.clone());
            index += 1;
            continue;
        }
        if token.starts_with("--") && token.len() > 2 {
            let without_prefix = &token[2..];
            let (name, has_inline_value) = without_prefix
                .split_once('=')
                .map_or((without_prefix, false), |(name, _)| (name, true));
            if let Some(spec) = known_long_option(name) {
                filtered.push(token.clone());
                if spec.takes_value && !has_inline_value && !spec.optional_value {
                    expecting_value = true;
                } else if spec.takes_value && !has_inline_value && spec.optional_value {
                    let has_value = raw_args
                        .get(index + 1)
                        .is_some_and(|next| !next.starts_with('-') || next.eq("-"));
                    expecting_value = has_value;
                }
                index += 1;
                continue;
            }
            let (name, inline_value) = without_prefix
                .split_once('=')
                .map_or((without_prefix, None), |(name, value)| {
                    (name, Some(value.to_string()))
                });
            if name.is_empty() {
                filtered.push(token.clone());
                index += 1;
                continue;
            }
            let mut value = inline_value;
            if value.is_none() {
                let next = raw_args.get(index + 1);
                if let Some(next) = next
                    && next.ne("--")
                    && (!next.starts_with('-') || next.eq("-") || is_negative_numeric_token(next))
                {
                    value = Some(next.clone());
                    index += 1;
                }
            }
            extracted.push(ExtensionCliFlag {
                name: name.to_string(),
                value,
            });
            index += 1;
            continue;
        }
        if token.eq("-e") {
            filtered.push(token.clone());
            expecting_value = true;
            index += 1;
            continue;
        }
        if is_known_short_flag(token) {
            filtered.push(token.clone());
            expecting_value = short_flag_expects_value(token);
            index += 1;
            continue;
        }
        if token.starts_with('-') {
            filtered.push(token.clone());
            index += 1;
            continue;
        }
        if ROOT_SUBCOMMANDS.contains(&token.as_str()) {
            in_subcommand = true;
        }
        filtered.push(token.clone());
        index += 1;
    }
    (filtered, extracted)
}

pub fn parse_with_extension_flags(raw_args: Vec<String>) -> Result<ParsedCli, clap::Error> {
    if raw_args.is_empty() {
        let cli = Cli::try_parse_from(["pi"])?;
        return Ok(ParsedCli {
            cli,
            extension_flags: Vec::new(),
        });
    }

    match Cli::try_parse_from(raw_args.clone()) {
        Ok(_) => {
            // We do NOT return early here because `Cli` has trailing varargs for `message`.
            // If the user provided `pi hello --unknown flag`, clap might happily parse
            // `--unknown flag` into `message`. We must preprocess extension flags first!
        }
        Err(err) => {
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                return Err(err);
            }
        }
    }

    let (filtered_args, extension_flags) = preprocess_extension_flags(&raw_args);
    if extension_flags.is_empty() {
        let cli = Cli::try_parse_from(raw_args)?;
        return Ok(ParsedCli {
            cli,
            extension_flags: Vec::new(),
        });
    }

    let cli = Cli::try_parse_from(filtered_args)?;
    Ok(ParsedCli {
        cli,
        extension_flags,
    })
}

/// Pi - AI coding agent CLI
#[derive(Parser, Debug)]
#[allow(clippy::struct_excessive_bools)] // CLI flags are naturally boolean
#[command(name = "pi")]
#[command(version, about, long_about = None, disable_version_flag = true)]
#[command(after_help = "Examples:
  pi \"explain this code\"              Start new session with message
  pi @file.rs \"review this\"           Include file in context
  pi -c                                Continue previous session
  pi -r                                Resume from session picker
  pi -p \"what is 2+2\"                 Print mode (non-interactive)
  pi --model claude-opus-4 \"help\"     Use specific model
")]
pub struct Cli {
    // === Help & Version ===
    /// Print version information
    #[arg(short = 'v', long)]
    pub version: bool,

    // === Model Configuration ===
    /// LLM provider (e.g., anthropic, openai, google).
    /// Run --list-providers for canonical IDs + aliases.
    #[arg(long, env = "PI_PROVIDER")]
    pub provider: Option<String>,

    /// Model ID (e.g., claude-opus-4, gpt-4o)
    #[arg(long, env = "PI_MODEL")]
    pub model: Option<String>,

    /// API key (overrides environment variable)
    #[arg(long)]
    pub api_key: Option<String>,

    /// Model patterns for Ctrl+P cycling (comma-separated, supports globs)
    #[arg(long)]
    pub models: Option<String>,

    /// HTTP request timeout in seconds for provider API calls.
    ///
    /// Bounds connect + request + first-response-header latency for each
    /// provider request. `0` disables the timeout entirely (unbounded).
    ///
    /// When unset, the default is provider-aware: 60s for cloud providers and
    /// 600s (10 minutes) for local providers (Ollama, LM Studio) where the
    /// first request can block while the model loads into memory. Raise this if
    /// a local model's cold start exceeds the default. Equivalent to the
    /// `PI_HTTP_REQUEST_TIMEOUT_SECS` env var and the `requestTimeoutSecs`
    /// setting. See pi_agent_rust#90.
    #[arg(long, value_name = "SECONDS", env = "PI_HTTP_REQUEST_TIMEOUT_SECS")]
    pub request_timeout: Option<u64>,

    // === Thinking/Reasoning ===
    /// Extended thinking level
    #[arg(long, value_parser = ["off", "minimal", "low", "medium", "high", "xhigh", "max"])]
    pub thinking: Option<String>,

    // === System Prompt ===
    /// Override system prompt
    #[arg(long)]
    pub system_prompt: Option<String>,

    /// Append to system prompt (text or file path)
    #[arg(long)]
    pub append_system_prompt: Option<String>,

    // === Session Management ===
    /// Continue previous session
    #[arg(short = 'c', long)]
    pub r#continue: bool,

    /// Select session from picker UI
    #[arg(short = 'r', long)]
    pub resume: bool,

    /// Use specific session file path
    #[arg(long)]
    pub session: Option<String>,

    /// Directory for session storage/lookup
    #[arg(long)]
    pub session_dir: Option<String>,

    /// Don't save session (ephemeral)
    #[arg(long)]
    pub no_session: bool,

    /// Session durability mode: strict, balanced, or throughput
    #[arg(
        long,
        value_parser = ["strict", "balanced", "throughput"]
    )]
    pub session_durability: Option<String>,

    /// Skip startup migrations for legacy config/session/layout paths
    #[arg(long)]
    pub no_migrations: bool,

    /// Disable terminal mouse capture in the interactive TUI.
    ///
    /// Pi normally captures all mouse motion to enable in-app wheel scrolling.
    /// On Windows / CMD.exe / Windows Terminal that capture blocks the
    /// terminal-native click-to-select / right-click-paste / Shift-Insert
    /// behaviour, making it effectively impossible to copy out the OAuth
    /// authorization URL (which is ~600 characters). Setting this flag (or
    /// `disable_mouse_capture: true` in settings, or `PI_NO_MOUSE_CAPTURE=1`)
    /// turns the capture off so terminal-native copy/paste keeps working.
    /// In-app mouse wheel scrolling is sacrificed; users can still scroll
    /// with Page Up/Down or arrow keys.
    ///
    /// Note: the env-var path is intentionally read in `run_interactive`
    /// (not via `#[arg(env = "...")]` here) so the truthiness semantics
    /// stay "only `=1` is truthy", matching how `PI_HARDWARE_CURSOR`
    /// behaves and avoiding clap's bool-env ambiguity where `=0` /
    /// `=false` may otherwise set the flag to true.
    #[arg(long)]
    pub no_mouse_capture: bool,

    // === Mode & Output ===
    /// Output mode for print mode (text, json, rpc)
    #[arg(long, value_parser = ["text", "json", "rpc"])]
    pub mode: Option<String>,

    /// Non-interactive mode (process & exit)
    #[arg(short = 'p', long)]
    pub print: bool,

    /// Start in RPC mode (alias for --mode rpc)
    #[arg(long, conflicts_with_all = ["mode", "print"])]
    pub rpc: bool,

    /// Start in ACP (Agent Client Protocol) mode for Zed editor integration.
    /// Reads JSON-RPC 2.0 requests from stdin and writes responses to stdout.
    #[arg(long)]
    pub acp: bool,

    /// Force verbose startup
    #[arg(long)]
    pub verbose: bool,

    // === Tools ===
    /// Disable all built-in tools
    #[arg(long)]
    pub no_tools: bool,

    /// Specific tools to enable (comma-separated: read,write,edit,bash,grep,find,ls,hashline_edit,subagent)
    #[arg(
        long,
        default_value = "read,bash,edit,write,grep,find,ls,hashline_edit"
    )]
    pub tools: String,

    // === Extensions ===
    /// Load extension file (can use multiple times)
    #[arg(short = 'e', long, action = clap::ArgAction::Append)]
    pub extension: Vec<String>,

    /// Disable extension discovery
    #[arg(long)]
    pub no_extensions: bool,

    /// Extension capability policy: safe, balanced, or permissive (legacy alias: standard)
    #[arg(long, value_name = "PROFILE")]
    pub extension_policy: Option<String>,

    /// Print the resolved extension policy with per-capability decisions and exit
    #[arg(long)]
    pub explain_extension_policy: bool,

    /// Repair policy mode: off, suggest, auto-safe, or auto-strict
    #[arg(long, value_name = "MODE")]
    pub repair_policy: Option<String>,

    /// Print the resolved repair policy and exit
    #[arg(long)]
    pub explain_repair_policy: bool,

    // === Skills ===
    /// Load skill file/directory (can use multiple times)
    #[arg(long, action = clap::ArgAction::Append)]
    pub skill: Vec<String>,

    /// Disable skill discovery
    #[arg(long)]
    pub no_skills: bool,

    // === Prompt Templates ===
    /// Load prompt template file/directory (can use multiple times)
    #[arg(long, action = clap::ArgAction::Append)]
    pub prompt_template: Vec<String>,

    /// Disable prompt template discovery
    #[arg(long)]
    pub no_prompt_templates: bool,

    // === Themes ===
    /// Select active theme (built-in name, discovered theme name, or theme JSON path)
    #[arg(long)]
    pub theme: Option<String>,

    /// Add theme file/directory to discovery (can use multiple times)
    #[arg(long = "theme-path", action = clap::ArgAction::Append)]
    pub theme_path: Vec<String>,

    /// Disable theme discovery
    #[arg(long)]
    pub no_themes: bool,

    // === System prompt modifiers ===
    /// Hide the current working directory from the system prompt.
    #[arg(long, env = "PI_HIDE_CWD_IN_PROMPT")]
    pub hide_cwd_in_prompt: bool,

    /// Maximum tool-call iterations per agent turn before stopping.
    /// Default: 50. Clamped to [1, 1000]; values outside the range fall back
    /// to 50 with a warning. Pairs with the iteration-aware-handoff protocol —
    /// at 80% of the cap, a one-shot steering message is injected so the agent
    /// can begin a graceful handoff rather than being silently killed at the
    /// ceiling. Override per-invocation via this flag, or globally via the
    /// `PI_MAX_TOOL_ITERATIONS` env var (read at agent start; invalid values
    /// fall back to the default with a warning, never abort startup).
    //
    // NOTE: `env =` is intentionally NOT set here. Clap's env wiring is strict
    // (an unparseable value aborts startup with a clap error), which would
    // defeat the lenient resolver semantics expected for this knob. The env
    // var is read inside `resolve_max_tool_iterations` instead, where bad
    // values warn-and-fall-back rather than fail the run.
    #[arg(long, value_name = "N")]
    pub max_tool_iterations: Option<usize>,

    // === Export & Listing ===
    /// Export session file to HTML
    #[arg(long)]
    pub export: Option<String>,

    /// List available models (optional fuzzy search pattern)
    #[arg(long)]
    #[allow(clippy::option_option)]
    // This is intentional: None = not set, Some(None) = set without value, Some(Some(x)) = set with value
    pub list_models: Option<Option<String>>,

    /// List all supported providers with aliases and auth env keys
    #[arg(long)]
    pub list_providers: bool,

    /// Fetch the live model catalog from a provider's `/v1/models` endpoint
    /// (OpenAI-compatible providers only). Falls back to the static registry
    /// when the live call fails. Long-lived library callers reuse successful
    /// results in-process for 5 minutes; separate CLI invocations do not share
    /// that cache. Set `PI_DISABLE_MODEL_CACHE=1` to bypass it.
    #[arg(long, value_name = "PROVIDER")]
    pub fetch_models: Option<String>,

    /// When used with `--fetch-models`, ignore any cached entry and require a
    /// successful fresh network call. Live-refresh failures are reported
    /// instead of being disguised as static-registry results.
    #[arg(long, requires = "fetch_models")]
    pub refresh_models: bool,

    /// Persist a verified live or same-process cached `--fetch-models` catalog
    /// to `models.fetched.json`. Static fallback results are never persisted.
    #[arg(long, requires = "fetch_models")]
    pub persist_models: bool,

    // === Subcommands ===
    #[command(subcommand)]
    pub command: Option<Commands>,

    // === Positional Arguments ===
    /// Messages and @file references
    #[arg(trailing_var_arg = true)]
    pub args: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::{Cli, Commands, ROOT_SUBCOMMANDS, parse_with_extension_flags};
    use clap::{CommandFactory, Parser, error::ErrorKind};

    // ── 1. Basic flag parsing ────────────────────────────────────────

    #[test]
    fn parse_resource_flags_and_mode() {
        let cli = Cli::parse_from([
            "pi",
            "--mode",
            "rpc",
            "--models",
            "gpt-4*,claude*",
            "--extension",
            "ext1",
            "--skill",
            "skill.md",
            "--prompt-template",
            "prompt.md",
            "--theme",
            "dark",
            "--theme-path",
            "dark.ini",
            "--no-themes",
        ]);

        assert_eq!(cli.mode.as_deref(), Some("rpc"));
        assert_eq!(cli.models.as_deref(), Some("gpt-4*,claude*"));
        assert_eq!(cli.extension, vec!["ext1".to_string()]);
        assert_eq!(cli.skill, vec!["skill.md".to_string()]);
        assert_eq!(cli.prompt_template, vec!["prompt.md".to_string()]);
        assert_eq!(cli.theme.as_deref(), Some("dark"));
        assert_eq!(cli.theme_path, vec!["dark.ini".to_string()]);
        assert!(cli.no_themes);
    }

    #[test]
    fn parse_continue_short_flag() {
        let cli = Cli::parse_from(["pi", "-c"]);
        assert!(cli.r#continue);
        assert!(!cli.resume);
        assert!(!cli.print);
    }

    #[test]
    fn parse_continue_long_flag() {
        let cli = Cli::parse_from(["pi", "--continue"]);
        assert!(cli.r#continue);
    }

    #[test]
    fn parse_resume_short_flag() {
        let cli = Cli::parse_from(["pi", "-r"]);
        assert!(cli.resume);
        assert!(!cli.r#continue);
    }

    #[test]
    fn parse_session_path() {
        let cli = Cli::parse_from(["pi", "--session", "/tmp/session.jsonl"]);
        assert_eq!(cli.session.as_deref(), Some("/tmp/session.jsonl"));
    }

    #[test]
    fn parse_session_dir() {
        let cli = Cli::parse_from(["pi", "--session-dir", "/tmp/sessions"]);
        assert_eq!(cli.session_dir.as_deref(), Some("/tmp/sessions"));
    }

    #[test]
    fn parse_no_session() {
        let cli = Cli::parse_from(["pi", "--no-session"]);
        assert!(cli.no_session);
    }

    #[test]
    fn parse_session_durability() {
        let cli = Cli::parse_from(["pi", "--session-durability", "throughput"]);
        assert_eq!(cli.session_durability.as_deref(), Some("throughput"));
    }

    #[test]
    fn parse_no_migrations() {
        let cli = Cli::parse_from(["pi", "--no-migrations"]);
        assert!(cli.no_migrations);
    }

    #[test]
    fn parse_print_short_flag() {
        let cli = Cli::parse_from(["pi", "-p", "what is 2+2"]);
        assert!(cli.print);
        assert_eq!(cli.message_args(), vec!["what is 2+2"]);
    }

    #[test]
    fn parse_print_long_flag() {
        let cli = Cli::parse_from(["pi", "--print", "question"]);
        assert!(cli.print);
    }

    #[test]
    fn parse_rpc_alias_sets_rpc_flag() {
        let cli = Cli::parse_from(["pi", "--rpc"]);
        assert!(cli.rpc);
    }

    #[test]
    fn parse_rpc_alias_conflicts_with_print() {
        let result = Cli::try_parse_from(["pi", "--rpc", "--print", "question"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_model_flag() {
        let cli = Cli::parse_from(["pi", "--model", "claude-opus-4"]);
        assert_eq!(cli.model.as_deref(), Some("claude-opus-4"));
    }

    #[test]
    fn parse_provider_flag() {
        let cli = Cli::parse_from(["pi", "--provider", "openai"]);
        assert_eq!(cli.provider.as_deref(), Some("openai"));
    }

    #[test]
    fn parse_api_key_flag() {
        let cli = Cli::parse_from(["pi", "--api-key", "sk-ant-test123"]);
        assert_eq!(cli.api_key.as_deref(), Some("sk-ant-test123"));
    }

    #[test]
    fn parse_version_short_flag() {
        let cli = Cli::parse_from(["pi", "-v"]);
        assert!(cli.version);
    }

    #[test]
    fn parse_version_long_flag() {
        let cli = Cli::parse_from(["pi", "--version"]);
        assert!(cli.version);
    }

    #[test]
    fn parse_with_extension_flags_preserves_help_error() {
        let err = parse_with_extension_flags(vec!["pi".into(), "--help".into()])
            .expect_err("`--help` should stay a clap help path");
        assert!(matches!(err.kind(), clap::error::ErrorKind::DisplayHelp));
    }

    #[test]
    fn parse_verbose_flag() {
        let cli = Cli::parse_from(["pi", "--verbose"]);
        assert!(cli.verbose);
    }

    #[test]
    fn parse_system_prompt_flags() {
        let cli = Cli::parse_from([
            "pi",
            "--system-prompt",
            "You are a helper",
            "--append-system-prompt",
            "Be concise",
        ]);
        assert_eq!(cli.system_prompt.as_deref(), Some("You are a helper"));
        assert_eq!(cli.append_system_prompt.as_deref(), Some("Be concise"));
    }

    #[test]
    fn parse_export_flag() {
        let cli = Cli::parse_from(["pi", "--export", "output.html"]);
        assert_eq!(cli.export.as_deref(), Some("output.html"));
    }

    // ── 2. Thinking level parsing ────────────────────────────────────

    #[test]
    fn parse_all_thinking_levels() {
        for level in &["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let cli = Cli::parse_from(["pi", "--thinking", level]);
            assert_eq!(cli.thinking.as_deref(), Some(*level));
        }
    }

    #[test]
    fn invalid_thinking_level_rejected() {
        let result = Cli::try_parse_from(["pi", "--thinking", "ultra"]);
        assert!(result.is_err());
    }

    // ── 3. @file expansion ───────────────────────────────────────────

    #[test]
    fn file_and_message_args_split() {
        let cli = Cli::parse_from(["pi", "@a.txt", "hello", "@b.md", "world"]);
        assert_eq!(cli.file_args(), vec!["a.txt", "b.md"]);
        assert_eq!(cli.message_args(), vec!["hello", "world"]);
    }

    #[test]
    fn file_args_empty_when_none() {
        let cli = Cli::parse_from(["pi", "hello", "world"]);
        assert!(cli.file_args().is_empty());
        assert_eq!(cli.message_args(), vec!["hello", "world"]);
    }

    #[test]
    fn message_args_empty_when_only_files() {
        let cli = Cli::parse_from(["pi", "@src/main.rs", "@Cargo.toml"]);
        assert_eq!(cli.file_args(), vec!["src/main.rs", "Cargo.toml"]);
        assert!(cli.message_args().is_empty());
    }

    #[test]
    fn no_positional_args_yields_empty() {
        let cli = Cli::parse_from(["pi"]);
        assert!(cli.file_args().is_empty());
        assert!(cli.message_args().is_empty());
    }

    #[test]
    fn at_prefix_stripped_from_file_paths() {
        let cli = Cli::parse_from(["pi", "@/absolute/path.rs"]);
        assert_eq!(cli.file_args(), vec!["/absolute/path.rs"]);
    }

    // ── 4. Subcommand parsing ────────────────────────────────────────

    #[test]
    fn parse_install_subcommand() -> Result<(), String> {
        let cli = Cli::parse_from(["pi", "install", "npm:@org/pkg"]);
        let Some(Commands::Install { source, local }) = cli.command else {
            return Err(format!("unexpected command: {:?}", cli.command));
        };
        assert_eq!(source, "npm:@org/pkg");
        assert!(!local);
        Ok(())
    }

    #[test]
    fn parse_install_local_flag() -> Result<(), String> {
        let cli = Cli::parse_from(["pi", "install", "--local", "git:https://example.com"]);
        let Some(Commands::Install { source, local }) = cli.command else {
            return Err(format!("unexpected command: {:?}", cli.command));
        };
        assert_eq!(source, "git:https://example.com");
        assert!(local);
        Ok(())
    }

    #[test]
    fn parse_install_local_short_flag() -> Result<(), String> {
        let cli = Cli::parse_from(["pi", "install", "-l", "./local-ext"]);
        let Some(Commands::Install { local, .. }) = cli.command else {
            return Err(format!("unexpected command: {:?}", cli.command));
        };
        assert!(local);
        Ok(())
    }

    #[test]
    fn parse_remove_subcommand() -> Result<(), String> {
        let cli = Cli::parse_from(["pi", "remove", "npm:pkg"]);
        let Some(Commands::Remove { source, local }) = cli.command else {
            return Err(format!("unexpected command: {:?}", cli.command));
        };
        assert_eq!(source, "npm:pkg");
        assert!(!local);
        Ok(())
    }

    #[test]
    fn parse_remove_local_flag() -> Result<(), String> {
        let cli = Cli::parse_from(["pi", "remove", "--local", "npm:pkg"]);
        let Some(Commands::Remove { local, .. }) = cli.command else {
            return Err(format!("unexpected command: {:?}", cli.command));
        };
        assert!(local);
        Ok(())
    }

    #[test]
    fn parse_update_with_source() -> Result<(), String> {
        let cli = Cli::parse_from(["pi", "update", "npm:pkg"]);
        let Some(Commands::Update { source }) = cli.command else {
            return Err(format!("unexpected command: {:?}", cli.command));
        };
        assert_eq!(source.as_deref(), Some("npm:pkg"));
        Ok(())
    }

    #[test]
    fn parse_update_all() -> Result<(), String> {
        let cli = Cli::parse_from(["pi", "update"]);
        let Some(Commands::Update { source }) = cli.command else {
            return Err(format!("unexpected command: {:?}", cli.command));
        };
        assert!(source.is_none());
        Ok(())
    }

    #[test]
    fn parse_list_subcommand() {
        let cli = Cli::parse_from(["pi", "list"]);
        assert!(matches!(cli.command, Some(Commands::List)));
    }

    #[test]
    fn parse_config_subcommand() -> Result<(), String> {
        let cli = Cli::parse_from(["pi", "config"]);
        let Some(Commands::Config { show, paths, json }) = cli.command else {
            return Err(format!("unexpected command: {:?}", cli.command));
        };
        assert!(!show);
        assert!(!paths);
        assert!(!json);
        Ok(())
    }

    #[test]
    fn parse_config_show_flag() -> Result<(), String> {
        let cli = Cli::parse_from(["pi", "config", "--show"]);
        let Some(Commands::Config { show, paths, json }) = cli.command else {
            return Err(format!("unexpected command: {:?}", cli.command));
        };
        assert!(show);
        assert!(!paths);
        assert!(!json);
        Ok(())
    }

    #[test]
    fn parse_config_paths_flag() -> Result<(), String> {
        let cli = Cli::parse_from(["pi", "config", "--paths"]);
        let Some(Commands::Config { show, paths, json }) = cli.command else {
            return Err(format!("unexpected command: {:?}", cli.command));
        };
        assert!(!show);
        assert!(paths);
        assert!(!json);
        Ok(())
    }

    #[test]
    fn parse_config_json_flag() -> Result<(), String> {
        let cli = Cli::parse_from(["pi", "config", "--json"]);
        let Some(Commands::Config { show, paths, json }) = cli.command else {
            return Err(format!("unexpected command: {:?}", cli.command));
        };
        assert!(!show);
        assert!(!paths);
        assert!(json);
        Ok(())
    }

    #[test]
    fn parse_update_index_subcommand() {
        let cli = Cli::parse_from(["pi", "update-index"]);
        assert!(matches!(cli.command, Some(Commands::UpdateIndex)));
    }

    #[test]
    fn parse_validation_broker_plan_subcommand() -> Result<(), String> {
        let cli = Cli::parse_from([
            "pi",
            "validation-broker",
            "plan",
            "--request",
            "request.json",
            "--inputs",
            "inputs.json",
            "--store",
            "slots.jsonl",
            "--format",
            "json",
        ]);
        let Some(Commands::ValidationBroker {
            command:
                super::ValidationBrokerCommand::Plan {
                    request,
                    inputs,
                    store,
                    format,
                    ..
                },
        }) = cli.command
        else {
            return Err(format!("unexpected command: {:?}", cli.command));
        };
        assert_eq!(request, "request.json");
        assert_eq!(inputs, "inputs.json");
        assert_eq!(store, "slots.jsonl");
        assert_eq!(format, "json");
        Ok(())
    }

    #[test]
    fn parse_swarm_progress_subcommand() -> Result<(), String> {
        let cli = Cli::parse_from([
            "pi",
            "swarm-progress",
            "--input",
            "progress-input.json",
            "--since",
            "HEAD~1",
            "--format",
            "json",
            "--out-json",
            "progress.json",
        ]);
        let Some(Commands::SwarmProgress {
            input,
            since,
            format,
            out_json,
            out_text,
        }) = cli.command
        else {
            return Err(format!("unexpected command: {:?}", cli.command));
        };
        assert_eq!(input, "progress-input.json");
        assert_eq!(since.as_deref(), Some("HEAD~1"));
        assert_eq!(format, "json");
        assert_eq!(out_json.as_deref(), Some("progress.json"));
        assert!(out_text.is_none());
        Ok(())
    }

    #[test]
    fn parse_info_subcommand() -> Result<(), String> {
        let cli = Cli::parse_from(["pi", "info", "auto-commit-on-exit"]);
        let Some(Commands::Info { name }) = cli.command else {
            return Err(format!("unexpected command: {:?}", cli.command));
        };
        assert_eq!(name, "auto-commit-on-exit");
        Ok(())
    }

    #[test]
    fn no_subcommand_when_only_message() {
        let cli = Cli::parse_from(["pi", "hello"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.message_args(), vec!["hello"]);
    }

    // ── 5. --list-models (Option<Option<String>>) ────────────────────

    #[test]
    fn list_models_not_set() {
        let cli = Cli::parse_from(["pi"]);
        assert!(cli.list_models.is_none());
    }

    #[test]
    fn list_models_without_pattern() {
        let cli = Cli::parse_from(["pi", "--list-models"]);
        assert!(matches!(cli.list_models, Some(None)));
    }

    #[test]
    fn list_models_with_pattern() -> Result<(), String> {
        let cli = Cli::parse_from(["pi", "--list-models", "claude*"]);
        let Some(Some(ref pat)) = cli.list_models else {
            return Err(format!("unexpected list_models: {:?}", cli.list_models));
        };
        assert_eq!(pat, "claude*");
        Ok(())
    }

    #[test]
    fn fetch_models_flags_survive_extension_flag_preprocessing() {
        let parsed = parse_with_extension_flags(
            [
                "pi",
                "--fetch-models",
                "openrouter",
                "--refresh-models",
                "--persist-models",
                "--request-timeout",
                "17",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        )
        .expect("parse fetch-models flags");

        assert_eq!(parsed.cli.fetch_models.as_deref(), Some("openrouter"));
        assert!(parsed.cli.refresh_models);
        assert!(parsed.cli.persist_models);
        assert_eq!(parsed.cli.request_timeout, Some(17));
        assert!(
            parsed.extension_flags.is_empty(),
            "built-in model flags must not be reclassified as extension flags"
        );
    }

    #[test]
    fn persist_models_requires_fetch_models() {
        let error = Cli::try_parse_from(["pi", "--persist-models"])
            .expect_err("persist-models without fetch-models must be rejected");
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    // ── 5b. --list-providers (bool) ────────────────────────────────────

    #[test]
    fn list_providers_not_set() {
        let cli = Cli::parse_from(["pi"]);
        assert!(!cli.list_providers);
    }

    #[test]
    fn list_providers_set() {
        let cli = Cli::parse_from(["pi", "--list-providers"]);
        assert!(cli.list_providers);
    }

    // ── 6. enabled_tools() method ────────────────────────────────────

    #[test]
    fn default_tools() {
        let cli = Cli::parse_from(["pi"]);
        assert_eq!(
            cli.enabled_tools(),
            vec![
                "read",
                "bash",
                "edit",
                "write",
                "grep",
                "find",
                "ls",
                "hashline_edit",
            ]
        );
    }

    #[test]
    fn custom_tools_list() {
        let cli = Cli::parse_from(["pi", "--tools", "read,grep,find,ls"]);
        assert_eq!(cli.enabled_tools(), vec!["read", "grep", "find", "ls"]);
    }

    #[test]
    fn no_tools_flag_returns_empty() {
        let cli = Cli::parse_from(["pi", "--no-tools"]);
        assert!(cli.enabled_tools().is_empty());
    }

    #[test]
    fn tools_with_spaces_trimmed() {
        let cli = Cli::parse_from(["pi", "--tools", "read, bash, edit"]);
        assert_eq!(cli.enabled_tools(), vec!["read", "bash", "edit"]);
    }

    #[test]
    fn tools_ignore_empty_entries_and_duplicates() {
        let cli = Cli::parse_from(["pi", "--tools", "read,, bash,read, ,grep,bash"]);
        assert_eq!(cli.enabled_tools(), vec!["read", "bash", "grep"]);
    }

    // ── 7. Invalid inputs ────────────────────────────────────────────

    #[test]
    fn unknown_flag_rejected() {
        let result = Cli::try_parse_from(["pi", "--nonexistent"]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_mode_rejected() {
        let result = Cli::try_parse_from(["pi", "--mode", "xml"]);
        assert!(result.is_err());
    }

    #[test]
    fn install_without_source_rejected() {
        let result = Cli::try_parse_from(["pi", "install"]);
        assert!(result.is_err());
    }

    #[test]
    fn remove_without_source_rejected() {
        let result = Cli::try_parse_from(["pi", "remove"]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_subcommand_option_rejected() {
        let result = Cli::try_parse_from(["pi", "install", "--bogus", "npm:pkg"]);
        assert!(result.is_err());
    }

    #[test]
    fn extension_flags_are_extracted_in_second_pass_parse() {
        let parsed = parse_with_extension_flags(vec![
            "pi".to_string(),
            "--plan".to_string(),
            "ship it".to_string(),
            "--model".to_string(),
            "gpt-4o".to_string(),
        ])
        .expect("parse with extension flags");

        assert_eq!(parsed.cli.model.as_deref(), Some("gpt-4o"));
        assert_eq!(parsed.extension_flags.len(), 1);
        assert_eq!(parsed.extension_flags[0].name, "plan");
        assert_eq!(parsed.extension_flags[0].value.as_deref(), Some("ship it"));
    }

    #[test]
    fn extension_bool_flag_without_value_is_supported() {
        let parsed = parse_with_extension_flags(vec![
            "pi".to_string(),
            "--dry-run".to_string(),
            "--print".to_string(),
            "hello".to_string(),
        ])
        .expect("parse extension bool flag");

        assert!(parsed.cli.print);
        assert_eq!(parsed.extension_flags.len(), 1);
        assert_eq!(parsed.extension_flags[0].name, "dry-run");
        assert!(parsed.extension_flags[0].value.is_none());
    }

    #[test]
    fn extension_flag_accepts_negative_integer_value() {
        let parsed = parse_with_extension_flags(vec![
            "pi".to_string(),
            "--temperature".to_string(),
            "-1".to_string(),
            "--print".to_string(),
            "hello".to_string(),
        ])
        .expect("parse negative integer value");

        assert!(parsed.cli.print);
        assert_eq!(parsed.extension_flags.len(), 1);
        assert_eq!(parsed.extension_flags[0].name, "temperature");
        assert_eq!(parsed.extension_flags[0].value.as_deref(), Some("-1"));
    }

    #[test]
    fn extension_flag_accepts_negative_float_value() {
        let parsed = parse_with_extension_flags(vec![
            "pi".to_string(),
            "--temperature".to_string(),
            "-0.25".to_string(),
            "--print".to_string(),
            "hello".to_string(),
        ])
        .expect("parse negative float value");

        assert!(parsed.cli.print);
        assert_eq!(parsed.extension_flags.len(), 1);
        assert_eq!(parsed.extension_flags[0].name, "temperature");
        assert_eq!(parsed.extension_flags[0].value.as_deref(), Some("-0.25"));
    }

    #[test]
    fn parse_with_extension_flags_recognizes_session_durability_as_builtin() {
        let parsed = parse_with_extension_flags(vec![
            "pi".to_string(),
            "--session-durability".to_string(),
            "throughput".to_string(),
            "--print".to_string(),
            "hello".to_string(),
        ])
        .expect("parse with session durability");

        assert_eq!(parsed.cli.session_durability.as_deref(), Some("throughput"));
        assert!(parsed.extension_flags.is_empty());
        assert!(parsed.cli.print);
    }

    #[test]
    fn parse_with_extension_flags_recognizes_no_mouse_capture_as_builtin() {
        let parsed = parse_with_extension_flags(vec![
            "pi".to_string(),
            "--no-mouse-capture".to_string(),
            "--plan".to_string(),
            "ship-it".to_string(),
            "--print".to_string(),
            "hello".to_string(),
        ])
        .expect("parse with no-mouse-capture and extension flag");

        assert!(parsed.cli.no_mouse_capture);
        assert!(parsed.cli.print);
        assert_eq!(parsed.cli.message_args(), vec!["hello"]);
        assert_eq!(parsed.extension_flags.len(), 1);
        assert_eq!(parsed.extension_flags[0].name, "plan");
        assert_eq!(parsed.extension_flags[0].value.as_deref(), Some("ship-it"));
    }

    #[test]
    fn extension_flag_parser_does_not_bypass_subcommand_validation() {
        let result = parse_with_extension_flags(vec![
            "pi".to_string(),
            "install".to_string(),
            "--bogus".to_string(),
            "pkg".to_string(),
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn extension_flags_survive_short_cluster_ending_in_e() {
        let parsed = parse_with_extension_flags(vec![
            "pi".to_string(),
            "-pe".to_string(),
            "ext.js".to_string(),
            "--plan".to_string(),
            "ship-it".to_string(),
            "hello".to_string(),
        ])
        .expect("parse short cluster with extension");

        assert!(parsed.cli.print);
        assert_eq!(parsed.cli.extension, vec!["ext.js".to_string()]);
        assert_eq!(parsed.cli.message_args(), vec!["hello"]);
        assert_eq!(parsed.extension_flags.len(), 1);
        assert_eq!(parsed.extension_flags[0].name, "plan");
        assert_eq!(parsed.extension_flags[0].value.as_deref(), Some("ship-it"));
    }

    #[test]
    fn extension_flags_after_message_args_are_extracted() {
        let parsed = parse_with_extension_flags(vec![
            "pi".to_string(),
            "hello".to_string(),
            "--plan".to_string(),
            "ship-it".to_string(),
        ])
        .expect("parse extension flag after message");

        assert_eq!(parsed.cli.message_args(), vec!["hello"]);
        assert_eq!(parsed.extension_flags.len(), 1);
        assert_eq!(parsed.extension_flags[0].name, "plan");
        assert_eq!(parsed.extension_flags[0].value.as_deref(), Some("ship-it"));
    }

    #[test]
    fn extension_flag_inline_value_matches_separate_value() {
        let separate = parse_with_extension_flags(vec![
            "pi".to_string(),
            "--plan".to_string(),
            "ship-it".to_string(),
            "--print".to_string(),
            "hello".to_string(),
        ])
        .expect("parse separate extension flag");

        let inline = parse_with_extension_flags(vec![
            "pi".to_string(),
            "--plan=ship-it".to_string(),
            "--print".to_string(),
            "hello".to_string(),
        ])
        .expect("parse inline extension flag");

        assert_eq!(separate.cli.print, inline.cli.print);
        assert_eq!(separate.cli.message_args(), inline.cli.message_args());
        assert_eq!(separate.extension_flags, inline.extension_flags);
    }

    #[test]
    fn root_subcommands_constant_matches_clap_parser() {
        let mut actual = Cli::command()
            .get_subcommands()
            .map(|command| command.get_name().to_string())
            .collect::<Vec<_>>();
        actual.sort();

        let mut expected = ROOT_SUBCOMMANDS
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>();
        expected.sort();

        assert_eq!(expected, actual);
    }

    // ── 8. Multiple append flags ─────────────────────────────────────

    #[test]
    fn multiple_extensions() {
        let cli = Cli::parse_from([
            "pi",
            "--extension",
            "ext1.js",
            "-e",
            "ext2.js",
            "--extension",
            "ext3.js",
        ]);
        assert_eq!(
            cli.extension,
            vec!["ext1.js", "ext2.js", "ext3.js"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn multiple_skills() {
        let cli = Cli::parse_from(["pi", "--skill", "a.md", "--skill", "b.md"]);
        assert_eq!(
            cli.skill,
            vec!["a.md", "b.md"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn multiple_theme_paths() {
        let cli = Cli::parse_from(["pi", "--theme-path", "a/", "--theme-path", "b/"]);
        assert_eq!(
            cli.theme_path,
            vec!["a/", "b/"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    // ── 9. Disable-discovery flags ───────────────────────────────────

    #[test]
    fn no_extensions_flag() {
        let cli = Cli::parse_from(["pi", "--no-extensions"]);
        assert!(cli.no_extensions);
    }

    #[test]
    fn no_skills_flag() {
        let cli = Cli::parse_from(["pi", "--no-skills"]);
        assert!(cli.no_skills);
    }

    #[test]
    fn no_prompt_templates_flag() {
        let cli = Cli::parse_from(["pi", "--no-prompt-templates"]);
        assert!(cli.no_prompt_templates);
    }

    // ── 10. Defaults ─────────────────────────────────────────────────

    #[test]
    fn bare_invocation_defaults() {
        let cli = Cli::parse_from(["pi"]);
        assert!(!cli.version);
        assert!(!cli.r#continue);
        assert!(!cli.resume);
        assert!(!cli.print);
        assert!(!cli.verbose);
        assert!(!cli.no_session);
        assert!(!cli.no_migrations);
        assert!(!cli.no_tools);
        assert!(!cli.no_extensions);
        assert!(!cli.no_skills);
        assert!(!cli.no_prompt_templates);
        assert!(!cli.no_themes);
        assert!(cli.provider.is_none());
        assert!(cli.model.is_none());
        assert!(cli.api_key.is_none());
        assert!(cli.thinking.is_none());
        assert!(cli.session.is_none());
        assert!(cli.session_dir.is_none());
        assert!(cli.mode.is_none());
        assert!(cli.export.is_none());
        assert!(cli.system_prompt.is_none());
        assert!(cli.append_system_prompt.is_none());
        assert!(cli.list_models.is_none());
        assert!(cli.command.is_none());
        assert!(cli.args.is_empty());
        assert_eq!(cli.tools, "read,bash,edit,write,grep,find,ls,hashline_edit");
    }

    // ── 11. Combined flags ───────────────────────────────────────────

    #[test]
    fn print_mode_with_model_and_thinking() {
        let cli = Cli::parse_from([
            "pi",
            "-p",
            "--model",
            "gpt-4o",
            "--thinking",
            "high",
            "solve this problem",
        ]);
        assert!(cli.print);
        assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
        assert_eq!(cli.thinking.as_deref(), Some("high"));
        assert_eq!(cli.message_args(), vec!["solve this problem"]);
    }

    // ── 12. Extension policy flag ───────────────────────────────────

    #[test]
    fn extension_policy_flag_parses() {
        let cli = Cli::parse_from(["pi", "--extension-policy", "safe"]);
        assert_eq!(cli.extension_policy.as_deref(), Some("safe"));
    }

    #[test]
    fn extension_policy_flag_permissive() {
        let cli = Cli::parse_from(["pi", "--extension-policy", "permissive"]);
        assert_eq!(cli.extension_policy.as_deref(), Some("permissive"));
    }

    #[test]
    fn extension_policy_flag_balanced() {
        let cli = Cli::parse_from(["pi", "--extension-policy", "balanced"]);
        assert_eq!(cli.extension_policy.as_deref(), Some("balanced"));
    }

    #[test]
    fn extension_policy_flag_absent() {
        let cli = Cli::parse_from(["pi"]);
        assert!(cli.extension_policy.is_none());
    }

    #[test]
    fn explain_extension_policy_flag_parses() {
        let cli = Cli::parse_from(["pi", "--explain-extension-policy"]);
        assert!(cli.explain_extension_policy);
    }

    // ── 13. Repair policy flag ──────────────────────────────────────

    #[test]
    fn repair_policy_flag_parses() {
        let cli = Cli::parse_from(["pi", "--repair-policy", "auto-safe"]);
        assert_eq!(cli.repair_policy.as_deref(), Some("auto-safe"));
    }

    #[test]
    fn repair_policy_flag_off() {
        let cli = Cli::parse_from(["pi", "--repair-policy", "off"]);
        assert_eq!(cli.repair_policy.as_deref(), Some("off"));
    }

    #[test]
    fn repair_policy_flag_absent() {
        let cli = Cli::parse_from(["pi"]);
        assert!(cli.repair_policy.is_none());
    }

    #[test]
    fn explain_repair_policy_flag_parses() {
        let cli = Cli::parse_from(["pi", "--explain-repair-policy"]);
        assert!(cli.explain_repair_policy);
    }

    // ── 14. CLI parity: every TS flag is parseable ──────────────────
    //
    // Reference: legacy_pi_mono_code/.../cli/args.ts
    // This test validates that all flags from the TypeScript CLI are
    // accepted by the Rust CLI parser (DROPIN-141 / bd-3meug).

    #[test]
    fn ts_parity_all_shared_flags_parse() {
        // Every flag from the TS args.ts that Rust must support.
        let cli = Cli::parse_from([
            "pi",
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet-4-5",
            "--api-key",
            "sk-test",
            "--system-prompt",
            "You are helpful.",
            "--append-system-prompt",
            "Extra context.",
            "--continue",
            "--session",
            "/tmp/sess",
            "--session-dir",
            "/tmp/sessdir",
            "--no-session",
            "--mode",
            "json",
            "--print",
            "--verbose",
            "--no-tools",
            "--tools",
            "read,bash",
            "--thinking",
            "high",
            "--extension",
            "ext.js",
            "--no-extensions",
            "--skill",
            "skill.md",
            "--no-skills",
            "--prompt-template",
            "tmpl.md",
            "--no-prompt-templates",
            "--theme",
            "dark",
            "--no-themes",
            "--export",
            "/tmp/out.html",
            "--models",
            "claude*,gpt*",
        ]);

        assert_eq!(cli.provider.as_deref(), Some("anthropic"));
        assert_eq!(cli.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(cli.api_key.as_deref(), Some("sk-test"));
        assert_eq!(cli.system_prompt.as_deref(), Some("You are helpful."));
        assert_eq!(cli.append_system_prompt.as_deref(), Some("Extra context."));
        assert!(cli.r#continue);
        assert_eq!(cli.session.as_deref(), Some("/tmp/sess"));
        assert_eq!(cli.session_dir.as_deref(), Some("/tmp/sessdir"));
        assert!(cli.no_session);
        assert_eq!(cli.mode.as_deref(), Some("json"));
        assert!(cli.print);
        assert!(cli.verbose);
        assert!(cli.no_tools);
        assert_eq!(cli.tools, "read,bash");
        assert_eq!(cli.thinking.as_deref(), Some("high"));
        assert_eq!(cli.extension, vec!["ext.js"]);
        assert!(cli.no_extensions);
        assert_eq!(cli.skill, vec!["skill.md"]);
        assert!(cli.no_skills);
        assert_eq!(cli.prompt_template, vec!["tmpl.md"]);
        assert!(cli.no_prompt_templates);
        assert_eq!(cli.theme.as_deref(), Some("dark"));
        assert!(cli.no_themes);
        assert_eq!(cli.export.as_deref(), Some("/tmp/out.html"));
        assert_eq!(cli.models.as_deref(), Some("claude*,gpt*"));
    }

    #[test]
    fn ts_parity_short_flags_match() {
        // TS short flags: -c (continue), -r (resume), -p (print),
        // -e (extension), -v (version), -h (help)
        let cli = Cli::parse_from(["pi", "-c", "-p", "-e", "ext.js"]);
        assert!(cli.r#continue);
        assert!(cli.print);
        assert_eq!(cli.extension, vec!["ext.js"]);

        let cli2 = Cli::parse_from(["pi", "-r"]);
        assert!(cli2.resume);
    }

    #[test]
    fn ts_parity_subcommands() {
        // TS subcommands: install, remove, update, list, config
        let cli = Cli::parse_from(["pi", "install", "npm:my-ext"]);
        assert!(matches!(cli.command, Some(Commands::Install { .. })));

        let cli = Cli::parse_from(["pi", "remove", "npm:my-ext"]);
        assert!(matches!(cli.command, Some(Commands::Remove { .. })));

        let cli = Cli::parse_from(["pi", "update"]);
        assert!(matches!(cli.command, Some(Commands::Update { .. })));

        let cli = Cli::parse_from(["pi", "list"]);
        assert!(matches!(cli.command, Some(Commands::List)));

        let cli = Cli::parse_from(["pi", "config"]);
        assert!(matches!(cli.command, Some(Commands::Config { .. })));
    }

    #[test]
    fn ts_parity_at_file_expansion() {
        let cli = Cli::parse_from(["pi", "-p", "@readme.md", "summarize this"]);
        assert_eq!(cli.file_args(), vec!["readme.md"]);
        assert_eq!(cli.message_args(), vec!["summarize this"]);
    }

    #[test]
    fn ts_parity_list_models_optional_search() {
        // --list-models with optional search term (TS parity)
        let cli = Cli::parse_from(["pi", "--list-models"]);
        assert_eq!(cli.list_models, Some(None));

        let cli = Cli::parse_from(["pi", "--list-models", "sonnet"]);
        assert_eq!(cli.list_models, Some(Some("sonnet".to_string())));
    }

    // ── Property tests ──────────────────────────────────────────────────

    mod proptest_cli {
        use crate::cli::{
            ExtensionCliFlag, ROOT_SUBCOMMANDS, is_known_short_flag, is_negative_numeric_token,
            known_long_option, preprocess_extension_flags, short_flag_expects_value,
        };
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn is_known_short_flag_accepts_known_char_combos(
                combo in prop::sample::select(vec![
                    "-v", "-c", "-r", "-p", "-e",
                    "-vc", "-vp", "-cr", "-vcr", "-vcrpe",
                ]),
            ) {
                assert!(
                    is_known_short_flag(combo),
                    "'{combo}' should be a known short flag"
                );
            }

            #[test]
            fn is_known_short_flag_rejects_unknown_chars(
                c in prop::sample::select(vec!['a', 'b', 'd', 'f', 'g', 'h', 'x', 'z']),
            ) {
                let token  = format!("-{c}");
                assert!(
                    !is_known_short_flag(&token),
                    "'-{c}' should not be a known short flag"
                );
            }

            #[test]
            fn is_known_short_flag_rejects_non_dash_prefix(
                body in "[a-z]{1,5}",
            ) {
                assert!(
                    !is_known_short_flag(&body),
                    "'{body}' without dash should not be a short flag"
                );
            }

            #[test]
            fn is_known_short_flag_rejects_double_dash(
                body in "[vcr]{1,5}",
            ) {
                let token  = format!("--{body}");
                assert!(
                    !is_known_short_flag(&token),
                    "'--{body}' should not be a short flag"
                );
            }

            #[test]
            fn short_flag_expects_value_when_cluster_ends_with_e(
                prefix in prop::sample::select(vec!["", "p", "c", "vp"]),
            ) {
                let token = format!("-{prefix}e");
                assert!(
                    short_flag_expects_value(&token),
                    "'{token}' should expect a following value"
                );
            }

            #[test]
            fn short_flag_does_not_expect_value_when_e_has_inline_value(
                suffix in prop::sample::select(vec!["v", "c", "r", "p", "vc"]),
            ) {
                let token = format!("-e{suffix}");
                assert!(
                    !short_flag_expects_value(&token),
                    "'{token}' should treat '{suffix}' as the inline -e value"
                );
            }

            #[test]
            fn is_negative_numeric_token_accepts_negative_integers(
                n in 1..10_000i64,
            ) {
                let token  = format!("-{n}");
                assert!(
                    is_negative_numeric_token(&token),
                    "'{token}' should be a negative numeric token"
                );
            }

            #[test]
            fn is_negative_numeric_token_accepts_negative_floats(
                whole in 0..100u32,
                frac in 1..100u32,
            ) {
                let token  = format!("-{whole}.{frac}");
                assert!(
                    is_negative_numeric_token(&token),
                    "'{token}' should be a negative numeric token"
                );
            }

            #[test]
            fn is_negative_numeric_token_rejects_positive_numbers(
                n in 0..10_000u64,
            ) {
                let token  = n.to_string();
                assert!(
                    !is_negative_numeric_token(&token),
                    "'{token}' (positive) should not be a negative numeric token"
                );
            }

            #[test]
            fn is_negative_numeric_token_rejects_non_numeric(
                s in "[a-z]{1,5}",
            ) {
                let token  = format!("-{s}");
                assert!(
                    !is_negative_numeric_token(&token),
                    "'-{s}' should not be a negative numeric token"
                );
            }

            #[test]
            fn preprocess_empty_returns_pi_program_name(_dummy in Just(())) {
                let result = preprocess_extension_flags(&[]);
                assert_eq!(result.0, vec!["pi"]);
                let extracted: &[ExtensionCliFlag] = &result.1;
                assert!(extracted.is_empty());
            }

            #[test]
            fn preprocess_known_flags_never_extracted(
                flag in prop::sample::select(vec![
                    "--version", "--verbose", "--print", "--no-tools",
                    "--no-extensions", "--no-skills", "--no-prompt-templates",
                    "--no-mouse-capture", "--rpc", "--list-providers",
                ]),
            ) {
                let args: Vec<String> = vec!["pi".to_string(), flag.to_string()];
                let result = preprocess_extension_flags(&args);
                let extracted: &[ExtensionCliFlag] = &result.1;
                assert!(
                    extracted.is_empty(),
                    "known flag '{flag}' should not be extracted"
                );
                assert!(
                    result.0.contains(&flag.to_string()),
                    "known flag '{flag}' should be in filtered"
                );
            }

            #[test]
            fn preprocess_unknown_flags_are_extracted(
                name in "[a-z]{3,10}".prop_filter(
                    "must not be a known option",
                    |n| known_long_option(n).is_none()
                        && !ROOT_SUBCOMMANDS.contains(&n.as_str()),
                ),
            ) {
                let flag = format!("--{name}");
                let args: Vec<String> = vec!["pi".to_string(), flag.clone()];
                let result = preprocess_extension_flags(&args);
                assert!(
                    !result.0.contains(&flag),
                    "unknown flag '{flag}' should not be in filtered"
                );
                assert_eq!(
                    result.1.len(), 1,
                    "should extract exactly one extension flag"
                );
                assert_eq!(result.1[0].name, name);
            }

            #[test]
            fn preprocess_double_dash_terminates(
                tail_count in 0..5usize,
                tail_token in "[a-z]{1,5}",
            ) {
                let mut args = vec!["pi".to_string(), "--".to_string()];
                for i in 0..tail_count {
                    args.push(format!("--{tail_token}{i}"));
                }
                let result = preprocess_extension_flags(&args);
                let extracted: &[ExtensionCliFlag] = &result.1;
                assert!(
                    extracted.is_empty(),
                    "after --, nothing should be extracted"
                );
                // All tokens should be in filtered
                assert_eq!(result.0.len(), args.len());
            }

            #[test]
            fn preprocess_subcommand_barrier(
                subcommand in prop::sample::select(vec![
                    "install", "remove", "update", "search", "info", "list", "config", "doctor",
                    "migrate", "swarm-progress",
                ]),
            ) {
                let args: Vec<String> = vec![
                    "pi".to_string(),
                    subcommand.to_string(),
                    "--unknown-flag".to_string(),
                ];
                let result = preprocess_extension_flags(&args);
                let extracted: &[ExtensionCliFlag] = &result.1;
                assert!(
                    extracted.is_empty(),
                    "after subcommand '{subcommand}', flags should not be extracted"
                );
                assert_eq!(result.0.len(), 3);
            }

            #[test]
            fn extension_flag_display_name_format(
                name in "[a-z]{1,10}",
            ) {
                let flag = ExtensionCliFlag {
                    name: name.clone(),
                    value: None,
                };
                assert_eq!(
                    flag.display_name(),
                    format!("--{name}"),
                    "display_name should be --name"
                );
            }
        }
    }
}

/// Package management subcommands
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Install extension/skill/prompt/theme from source
    Install {
        /// Package source (npm:pkg, git:url, or local path)
        source: String,
        /// Install locally (project) instead of globally
        #[arg(short = 'l', long)]
        local: bool,
    },

    /// Remove package from settings
    Remove {
        /// Package source to remove
        source: String,
        /// Remove from local (project) settings
        #[arg(short = 'l', long)]
        local: bool,
    },

    /// Update packages
    Update {
        /// Specific source to update (or all if omitted)
        source: Option<String>,
    },

    /// Refresh extension index cache from remote sources
    #[command(name = "update-index")]
    UpdateIndex,

    /// Preview the semantic context bundle Pi would use for a task
    #[command(name = "context-preview")]
    ContextPreview {
        /// Output format: text (default) or json
        #[arg(long, default_value = "text", value_parser = ["text", "json"])]
        format: String,
        /// Bead ID to anchor the preview around
        #[arg(long)]
        bead: Option<String>,
        /// Changed path to anchor related context; repeatable
        #[arg(long = "changed-path", action = clap::ArgAction::Append)]
        changed_paths: Vec<String>,
        /// Failing command to match validation context
        #[arg(long = "failing-command")]
        failing_command: Option<String>,
        /// Maximum selected bundle items
        #[arg(long, default_value_t = 24)]
        max_items: usize,
        /// Maximum selected bundle bytes
        #[arg(long, default_value_t = 32 * 1024)]
        max_bytes: u64,
        /// Task query text used to score candidate context
        #[arg(trailing_var_arg = true)]
        query: Vec<String>,
    },

    /// Evaluate a normalized swarm progress SLO snapshot without live mutations
    #[command(name = "swarm-progress")]
    SwarmProgress {
        /// Normalized ProgressSloEvaluationInput JSON to evaluate
        #[arg(long)]
        input: String,
        /// Optional operator baseline; must match input.time_window.comparison_baseline
        #[arg(long)]
        since: Option<String>,
        /// Output format for stdout when no output path is supplied
        #[arg(long, default_value = "text", value_parser = ["text", "json"])]
        format: String,
        /// Write schema-governed progress SLO JSON; refuses to overwrite
        #[arg(long = "out-json")]
        out_json: Option<String>,
        /// Write concise progress SLO text; refuses to overwrite
        #[arg(long = "out-text")]
        out_text: Option<String>,
    },

    /// Preview an offline swarm replay trace and policy comparison
    #[command(name = "swarm-replay-preview")]
    SwarmReplayPreview {
        /// Normalized pi.swarm.replay_trace.v1 JSON to replay
        #[arg(long)]
        trace: String,
        /// Baseline policy to compare; repeatable, defaults to all built-in policies
        #[arg(long = "policy", action = clap::ArgAction::Append)]
        policies: Vec<String>,
        /// Output format for stdout when no output path is supplied
        #[arg(long, default_value = "text", value_parser = ["text", "json"])]
        format: String,
        /// Write schema-governed preview JSON; refuses to overwrite
        #[arg(long = "out-json")]
        out_json: Option<String>,
        /// Write concise preview text; refuses to overwrite
        #[arg(long = "out-text")]
        out_text: Option<String>,
        /// Override generation timestamp for deterministic fixtures
        #[arg(long = "generated-at")]
        generated_at: Option<String>,
    },

    /// Inspect and mutate validation-broker slot leases
    #[command(name = "validation-broker")]
    ValidationBroker {
        #[command(subcommand)]
        command: ValidationBrokerCommand,
    },

    /// Show detailed information about an extension
    Info {
        /// Extension name or id to look up
        name: String,
    },

    /// Search available extensions by keyword
    Search {
        /// Search query (e.g. "git", "auto commit")
        query: String,
        /// Filter results by tag
        #[arg(long)]
        tag: Option<String>,
        /// Sort results: relevance, name
        #[arg(long, default_value = "relevance")]
        sort: String,
        /// Maximum number of results
        #[arg(long, default_value = "25")]
        limit: usize,
    },

    /// List installed packages
    List,

    /// Open configuration UI
    Config {
        /// Print configuration summary as text (non-interactive)
        #[arg(long)]
        show: bool,
        /// Print path and precedence details only
        #[arg(long)]
        paths: bool,
        /// Print configuration details as JSON
        #[arg(long)]
        json: bool,
    },

    /// Diagnose environment health and extension compatibility
    Doctor {
        /// Extension path to check (omit to run all environment checks)
        path: Option<String>,
        /// Output format: text (default), json, markdown
        #[arg(long, default_value = "text")]
        format: String,
        /// Extension policy profile to check against
        #[arg(long)]
        policy: Option<String>,
        /// Automatically fix safe issues (missing dirs, permissions)
        #[arg(long)]
        fix: bool,
        /// Run specific categories: config,dirs,auth,shell,sessions,swarm,extensions
        #[arg(long)]
        only: Option<String>,
    },

    /// Migrate session files from JSONL v1 to v2 segment format
    Migrate {
        /// Path to specific session JSONL file (or directory to migrate all)
        path: String,
        /// Dry-run: validate migration without persisting changes
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ValidationBrokerCommand {
    /// Print current slot-store status without mutating it
    Status {
        /// Append-only validation slot JSONL store
        #[arg(long)]
        store: String,
        /// Output format when no output path is supplied
        #[arg(long, default_value = "text", value_parser = ["text", "json"])]
        format: String,
        /// Write schema-governed JSON; refuses to overwrite
        #[arg(long = "out-json")]
        out_json: Option<String>,
        /// Write concise text; refuses to overwrite
        #[arg(long = "out-text")]
        out_text: Option<String>,
        /// Override report timestamp for deterministic fixtures
        #[arg(long = "generated-at")]
        generated_at: Option<String>,
    },

    /// Plan whether to run, narrow, wait, coalesce, or surface a blocker
    Plan {
        /// ValidationAdmissionRequestContext JSON
        #[arg(long)]
        request: String,
        /// ValidationBrokerInputSnapshot JSON
        #[arg(long)]
        inputs: String,
        /// Append-only validation slot JSONL store to inspect
        #[arg(long)]
        store: String,
        /// Optional ValidationAdmissionPolicy JSON
        #[arg(long)]
        policy: Option<String>,
        /// Output format when no output path is supplied
        #[arg(long, default_value = "text", value_parser = ["text", "json"])]
        format: String,
        /// Write schema-governed JSON; refuses to overwrite
        #[arg(long = "out-json")]
        out_json: Option<String>,
        /// Write concise text; refuses to overwrite
        #[arg(long = "out-text")]
        out_text: Option<String>,
        /// Override report timestamp for deterministic fixtures
        #[arg(long = "generated-at")]
        generated_at: Option<String>,
    },

    /// Acquire a slot by appending an active lease record
    Acquire {
        /// ValidationSlotRequest JSON
        #[arg(long)]
        request: String,
        /// Append-only validation slot JSONL store
        #[arg(long)]
        store: String,
        /// Lease start timestamp in UTC RFC3339
        #[arg(long = "started-at")]
        started_at: String,
        /// Lease expiry timestamp in UTC RFC3339
        #[arg(long = "expires-at")]
        expires_at: String,
        /// Output format when no output path is supplied
        #[arg(long, default_value = "text", value_parser = ["text", "json"])]
        format: String,
        /// Write schema-governed JSON; refuses to overwrite
        #[arg(long = "out-json")]
        out_json: Option<String>,
        /// Write concise text; refuses to overwrite
        #[arg(long = "out-text")]
        out_text: Option<String>,
    },

    /// Renew a slot owned by the caller
    Renew {
        /// Append-only validation slot JSONL store
        #[arg(long)]
        store: String,
        /// Slot ID to renew
        #[arg(long = "slot-id")]
        slot_id: String,
        /// Owning agent name
        #[arg(long)]
        owner: String,
        /// New heartbeat timestamp in UTC RFC3339
        #[arg(long = "heartbeat-at")]
        heartbeat_at: String,
        /// New expiry timestamp in UTC RFC3339
        #[arg(long = "expires-at")]
        expires_at: String,
        /// Output format when no output path is supplied
        #[arg(long, default_value = "text", value_parser = ["text", "json"])]
        format: String,
        /// Write schema-governed JSON; refuses to overwrite
        #[arg(long = "out-json")]
        out_json: Option<String>,
        /// Write concise text; refuses to overwrite
        #[arg(long = "out-text")]
        out_text: Option<String>,
    },

    /// Release a slot owned by the caller
    Release {
        /// Append-only validation slot JSONL store
        #[arg(long)]
        store: String,
        /// Slot ID to release
        #[arg(long = "slot-id")]
        slot_id: String,
        /// Owning agent name
        #[arg(long)]
        owner: String,
        /// Release timestamp in UTC RFC3339
        #[arg(long)]
        at: String,
        /// Release reason
        #[arg(long)]
        reason: String,
        /// Output format when no output path is supplied
        #[arg(long, default_value = "text", value_parser = ["text", "json"])]
        format: String,
        /// Write schema-governed JSON; refuses to overwrite
        #[arg(long = "out-json")]
        out_json: Option<String>,
        /// Write concise text; refuses to overwrite
        #[arg(long = "out-text")]
        out_text: Option<String>,
    },
}

impl Cli {
    /// Get file arguments (prefixed with @)
    pub fn file_args(&self) -> Vec<&str> {
        self.args
            .iter()
            .filter(|a| a.starts_with('@'))
            .map(|a| a.strip_prefix('@').unwrap_or(a))
            .collect()
    }

    /// Get message arguments (not prefixed with @)
    pub fn message_args(&self) -> Vec<&str> {
        self.args
            .iter()
            .filter(|a| !a.starts_with('@'))
            .map(String::as_str)
            .collect()
    }

    /// Get enabled tools as a list
    pub fn enabled_tools(&self) -> Vec<&str> {
        if self.no_tools {
            vec![]
        } else {
            let mut seen = HashSet::new();
            self.tools
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .filter(|name| seen.insert(*name))
                .collect()
        }
    }
}
