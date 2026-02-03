//! Configuration loading and management.

use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Main configuration structure.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    // Appearance
    pub theme: Option<String>,
    pub hide_thinking_block: Option<bool>,
    pub show_hardware_cursor: Option<bool>,

    // Model Configuration
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub default_thinking_level: Option<String>,
    pub enabled_models: Option<Vec<String>>,

    // Message Handling
    pub steering_mode: Option<String>,
    pub follow_up_mode: Option<String>,

    // Terminal Behavior
    pub quiet_startup: Option<bool>,
    pub collapse_changelog: Option<bool>,
    pub double_escape_action: Option<String>,
    pub editor_padding_x: Option<u32>,
    pub autocomplete_max_visible: Option<u32>,

    // Compaction
    pub compaction: Option<CompactionSettings>,

    // Branch Summarization
    pub branch_summary: Option<BranchSummarySettings>,

    // Retry Configuration
    pub retry: Option<RetrySettings>,

    // Shell
    pub shell_path: Option<String>,
    pub shell_command_prefix: Option<String>,

    // Images
    pub images: Option<ImageSettings>,

    // Terminal Display
    pub terminal: Option<TerminalSettings>,

    // Thinking Budgets
    pub thinking_budgets: Option<ThinkingBudgets>,

    // Extensions/Skills/etc.
    pub packages: Option<Vec<PackageSource>>,
    pub extensions: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub prompts: Option<Vec<String>>,
    pub themes: Option<Vec<String>>,
    pub enable_skill_commands: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionSettings {
    pub enabled: Option<bool>,
    pub reserve_tokens: Option<u32>,
    pub keep_recent_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BranchSummarySettings {
    pub reserve_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RetrySettings {
    pub enabled: Option<bool>,
    pub max_retries: Option<u32>,
    pub base_delay_ms: Option<u32>,
    pub max_delay_ms: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageSettings {
    pub auto_resize: Option<bool>,
    pub block_images: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    pub show_images: Option<bool>,
    pub clear_on_shrink: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ThinkingBudgets {
    pub minimal: Option<u32>,
    pub low: Option<u32>,
    pub medium: Option<u32>,
    pub high: Option<u32>,
    pub xhigh: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PackageSource {
    String(String),
    Detailed {
        source: String,
        #[serde(default)]
        local: Option<bool>,
        #[serde(default)]
        kind: Option<String>,
    },
}

impl Config {
    /// Load configuration from global and project settings.
    pub fn load() -> Result<Self> {
        if let Ok(path) = std::env::var("PI_CONFIG_PATH") {
            return Self::load_from_path(&PathBuf::from(path));
        }
        let global = Self::load_global()?;
        let project = Self::load_project()?;

        // Merge project settings over global
        Ok(Self::merge(global, project))
    }

    /// Get the global configuration directory.
    pub fn global_dir() -> PathBuf {
        std::env::var("PI_CODING_AGENT_DIR").map_or_else(
            |_| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".pi")
                    .join("agent")
            },
            PathBuf::from,
        )
    }

    /// Get the project configuration directory.
    pub fn project_dir() -> PathBuf {
        PathBuf::from(".pi")
    }

    /// Get the sessions directory.
    pub fn sessions_dir() -> PathBuf {
        if let Ok(path) = std::env::var("PI_SESSIONS_DIR") {
            return PathBuf::from(path);
        }
        Self::global_dir().join("sessions")
    }

    /// Get the package directory.
    pub fn package_dir() -> PathBuf {
        if let Ok(path) = std::env::var("PI_PACKAGE_DIR") {
            return PathBuf::from(path);
        }
        Self::global_dir().join("packages")
    }

    /// Get the auth file path.
    pub fn auth_path() -> PathBuf {
        Self::global_dir().join("auth.json")
    }

    /// Load global settings.
    fn load_global() -> Result<Self> {
        let path = Self::global_dir().join("settings.json");
        Self::load_from_path(&path)
    }

    /// Load project settings.
    fn load_project() -> Result<Self> {
        let path = Self::project_dir().join("settings.json");
        Self::load_from_path(&path)
    }

    /// Load settings from a specific path.
    fn load_from_path(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(path)?;
        let config: Self = serde_json::from_str(&content).unwrap_or_default();
        Ok(config)
    }

    /// Merge two configurations, with `other` taking precedence.
    fn merge(base: Self, other: Self) -> Self {
        Self {
            // Appearance
            theme: other.theme.or(base.theme),
            hide_thinking_block: other.hide_thinking_block.or(base.hide_thinking_block),
            show_hardware_cursor: other.show_hardware_cursor.or(base.show_hardware_cursor),

            // Model Configuration
            default_provider: other.default_provider.or(base.default_provider),
            default_model: other.default_model.or(base.default_model),
            default_thinking_level: other.default_thinking_level.or(base.default_thinking_level),
            enabled_models: other.enabled_models.or(base.enabled_models),

            // Message Handling
            steering_mode: other.steering_mode.or(base.steering_mode),
            follow_up_mode: other.follow_up_mode.or(base.follow_up_mode),

            // Terminal Behavior
            quiet_startup: other.quiet_startup.or(base.quiet_startup),
            collapse_changelog: other.collapse_changelog.or(base.collapse_changelog),
            double_escape_action: other.double_escape_action.or(base.double_escape_action),
            editor_padding_x: other.editor_padding_x.or(base.editor_padding_x),
            autocomplete_max_visible: other
                .autocomplete_max_visible
                .or(base.autocomplete_max_visible),

            // Compaction
            compaction: other.compaction.or(base.compaction),

            // Branch Summarization
            branch_summary: other.branch_summary.or(base.branch_summary),

            // Retry Configuration
            retry: other.retry.or(base.retry),

            // Shell
            shell_path: other.shell_path.or(base.shell_path),
            shell_command_prefix: other.shell_command_prefix.or(base.shell_command_prefix),

            // Images
            images: other.images.or(base.images),

            // Terminal Display
            terminal: other.terminal.or(base.terminal),

            // Thinking Budgets
            thinking_budgets: other.thinking_budgets.or(base.thinking_budgets),

            // Extensions/Skills/etc.
            packages: other.packages.or(base.packages),
            extensions: other.extensions.or(base.extensions),
            skills: other.skills.or(base.skills),
            prompts: other.prompts.or(base.prompts),
            themes: other.themes.or(base.themes),
            enable_skill_commands: other.enable_skill_commands.or(base.enable_skill_commands),
        }
    }

    // === Accessor methods with defaults ===

    pub fn compaction_enabled(&self) -> bool {
        self.compaction
            .as_ref()
            .and_then(|c| c.enabled)
            .unwrap_or(true)
    }

    pub fn compaction_reserve_tokens(&self) -> u32 {
        self.compaction
            .as_ref()
            .and_then(|c| c.reserve_tokens)
            .unwrap_or(16384)
    }

    pub fn compaction_keep_recent_tokens(&self) -> u32 {
        self.compaction
            .as_ref()
            .and_then(|c| c.keep_recent_tokens)
            .unwrap_or(20000)
    }

    pub fn branch_summary_reserve_tokens(&self) -> u32 {
        self.branch_summary
            .as_ref()
            .and_then(|b| b.reserve_tokens)
            .unwrap_or_else(|| self.compaction_reserve_tokens())
    }

    pub fn retry_enabled(&self) -> bool {
        self.retry.as_ref().and_then(|r| r.enabled).unwrap_or(true)
    }

    pub fn retry_max_retries(&self) -> u32 {
        self.retry.as_ref().and_then(|r| r.max_retries).unwrap_or(3)
    }

    pub fn retry_base_delay_ms(&self) -> u32 {
        self.retry
            .as_ref()
            .and_then(|r| r.base_delay_ms)
            .unwrap_or(2000)
    }

    pub fn retry_max_delay_ms(&self) -> u32 {
        self.retry
            .as_ref()
            .and_then(|r| r.max_delay_ms)
            .unwrap_or(60000)
    }

    pub fn image_auto_resize(&self) -> bool {
        self.images
            .as_ref()
            .and_then(|i| i.auto_resize)
            .unwrap_or(true)
    }

    pub fn thinking_budget(&self, level: &str) -> u32 {
        let budgets = self.thinking_budgets.as_ref();
        match level {
            "minimal" => budgets.and_then(|b| b.minimal).unwrap_or(1024),
            "low" => budgets.and_then(|b| b.low).unwrap_or(2048),
            "medium" => budgets.and_then(|b| b.medium).unwrap_or(8192),
            "high" => budgets.and_then(|b| b.high).unwrap_or(16384),
            "xhigh" => budgets.and_then(|b| b.xhigh).unwrap_or(u32::MAX),
            _ => 0,
        }
    }

    pub fn enable_skill_commands(&self) -> bool {
        self.enable_skill_commands.unwrap_or(true)
    }
}
