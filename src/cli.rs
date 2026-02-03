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
    /// Load theme file/directory (can use multiple times)
    #[arg(long, action = clap::ArgAction::Append)]
    pub theme: Vec<String>,

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
    use super::Cli;
    use clap::Parser;

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
            "dark.ini",
            "--no-themes",
        ]);

        assert_eq!(cli.mode.as_deref(), Some("rpc"));
        assert_eq!(cli.models.as_deref(), Some("gpt-4*,claude*"));
        assert_eq!(cli.extension, vec!["ext1".to_string()]);
        assert_eq!(cli.skill, vec!["skill.md".to_string()]);
        assert_eq!(cli.prompt_template, vec!["prompt.md".to_string()]);
        assert_eq!(cli.theme, vec!["dark.ini".to_string()]);
        assert!(cli.no_themes);
    }

    #[test]
    fn file_and_message_args_split() {
        let cli = Cli::parse_from(["pi", "@a.txt", "hello", "@b.md", "world"]);
        assert_eq!(cli.file_args(), vec!["a.txt", "b.md"]);
        assert_eq!(cli.message_args(), vec!["hello", "world"]);
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
