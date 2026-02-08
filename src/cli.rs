//! CLI argument parsing using Clap.

use clap::{Parser, Subcommand};

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
    /// LLM provider (e.g., anthropic, openai, google)
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

    // === Thinking/Reasoning ===
    /// Extended thinking level
    #[arg(long, value_parser = ["off", "minimal", "low", "medium", "high", "xhigh"])]
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

    // === Mode & Output ===
    /// Output mode for print mode (text, json, rpc)
    #[arg(long, value_parser = ["text", "json", "rpc"])]
    pub mode: Option<String>,

    /// Non-interactive mode (process & exit)
    #[arg(short = 'p', long)]
    pub print: bool,

    /// Force verbose startup
    #[arg(long)]
    pub verbose: bool,

    // === Tools ===
    /// Disable all built-in tools
    #[arg(long)]
    pub no_tools: bool,

    /// Specific tools to enable (comma-separated: read,bash,edit,write,grep,find,ls)
    #[arg(long, default_value = "read,bash,edit,write")]
    pub tools: String,

    // === Extensions ===
    /// Load extension file (can use multiple times)
    #[arg(short = 'e', long, action = clap::ArgAction::Append)]
    pub extension: Vec<String>,

    /// Disable extension discovery
    #[arg(long)]
    pub no_extensions: bool,

    /// Extension capability policy: safe, standard, or permissive
    #[arg(long, value_name = "PROFILE")]
    pub extension_policy: Option<String>,

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

    // === Export & Listing ===
    /// Export session file to HTML
    #[arg(long)]
    pub export: Option<String>,

    /// List available models (optional fuzzy search pattern)
    #[arg(long)]
    #[allow(clippy::option_option)]
    // This is intentional: None = not set, Some(None) = set without value, Some(Some(x)) = set with value
    pub list_models: Option<Option<String>>,

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
    use super::{Cli, Commands};
    use clap::Parser;

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
        for level in &["off", "minimal", "low", "medium", "high", "xhigh"] {
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
    fn parse_install_subcommand() {
        let cli = Cli::parse_from(["pi", "install", "npm:@org/pkg"]);
        match cli.command {
            Some(Commands::Install { source, local }) => {
                assert_eq!(source, "npm:@org/pkg");
                assert!(!local);
            }
            other => panic!("expected Install, got {other:?}"),
        }
    }

    #[test]
    fn parse_install_local_flag() {
        let cli = Cli::parse_from(["pi", "install", "--local", "git:https://example.com"]);
        match cli.command {
            Some(Commands::Install { source, local }) => {
                assert_eq!(source, "git:https://example.com");
                assert!(local);
            }
            other => panic!("expected Install --local, got {other:?}"),
        }
    }

    #[test]
    fn parse_install_local_short_flag() {
        let cli = Cli::parse_from(["pi", "install", "-l", "./local-ext"]);
        match cli.command {
            Some(Commands::Install { local, .. }) => assert!(local),
            other => panic!("expected Install -l, got {other:?}"),
        }
    }

    #[test]
    fn parse_remove_subcommand() {
        let cli = Cli::parse_from(["pi", "remove", "npm:pkg"]);
        match cli.command {
            Some(Commands::Remove { source, local }) => {
                assert_eq!(source, "npm:pkg");
                assert!(!local);
            }
            other => panic!("expected Remove, got {other:?}"),
        }
    }

    #[test]
    fn parse_remove_local_flag() {
        let cli = Cli::parse_from(["pi", "remove", "--local", "npm:pkg"]);
        match cli.command {
            Some(Commands::Remove { local, .. }) => assert!(local),
            other => panic!("expected Remove --local, got {other:?}"),
        }
    }

    #[test]
    fn parse_update_with_source() {
        let cli = Cli::parse_from(["pi", "update", "npm:pkg"]);
        match cli.command {
            Some(Commands::Update { source }) => {
                assert_eq!(source.as_deref(), Some("npm:pkg"));
            }
            other => panic!("expected Update with source, got {other:?}"),
        }
    }

    #[test]
    fn parse_update_all() {
        let cli = Cli::parse_from(["pi", "update"]);
        match cli.command {
            Some(Commands::Update { source }) => assert!(source.is_none()),
            other => panic!("expected Update (all), got {other:?}"),
        }
    }

    #[test]
    fn parse_list_subcommand() {
        let cli = Cli::parse_from(["pi", "list"]);
        assert!(matches!(cli.command, Some(Commands::List)));
    }

    #[test]
    fn parse_config_subcommand() {
        let cli = Cli::parse_from(["pi", "config"]);
        assert!(matches!(cli.command, Some(Commands::Config)));
    }

    #[test]
    fn parse_update_index_subcommand() {
        let cli = Cli::parse_from(["pi", "update-index"]);
        assert!(matches!(cli.command, Some(Commands::UpdateIndex)));
    }

    #[test]
    fn parse_info_subcommand() {
        let cli = Cli::parse_from(["pi", "info", "auto-commit-on-exit"]);
        match cli.command {
            Some(Commands::Info { name }) => {
                assert_eq!(name, "auto-commit-on-exit");
            }
            other => panic!("expected Info, got {other:?}"),
        }
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
    fn list_models_with_pattern() {
        let cli = Cli::parse_from(["pi", "--list-models", "claude*"]);
        match cli.list_models {
            Some(Some(ref pat)) => assert_eq!(pat, "claude*"),
            other => panic!("expected Some(Some(\"claude*\")), got {other:?}"),
        }
    }

    // ── 6. enabled_tools() method ────────────────────────────────────

    #[test]
    fn default_tools() {
        let cli = Cli::parse_from(["pi"]);
        assert_eq!(cli.enabled_tools(), vec!["read", "bash", "edit", "write"]);
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
        assert_eq!(cli.tools, "read,bash,edit,write");
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
    Config,
}

impl Cli {
    /// Get file arguments (prefixed with @)
    pub fn file_args(&self) -> Vec<&str> {
        self.args
            .iter()
            .filter(|a| a.starts_with('@'))
            .map(|a| a.trim_start_matches('@'))
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
            self.tools.split(',').map(str::trim).collect()
        }
    }
}
