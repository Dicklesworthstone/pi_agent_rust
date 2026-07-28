//! Configuration loading and management.

use crate::agent::QueueMode;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tempfile::NamedTempFile;

/// Main configuration structure.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    // Appearance
    pub theme: Option<String>,
    #[serde(alias = "hideThinkingBlock")]
    pub hide_thinking_block: Option<bool>,
    #[serde(alias = "showHardwareCursor")]
    pub show_hardware_cursor: Option<bool>,
    /// Disable terminal mouse capture in the interactive TUI.
    ///
    /// When `true`, the TUI does not call `with_mouse_all_motion`, so the
    /// terminal's native click-to-select / right-click-paste / Shift-Insert
    /// behaviour keeps working — at the cost of in-app mouse-wheel scrolling.
    /// Default `false` preserves the existing behaviour.
    ///
    /// Motivated by Windows users (CMD.exe + Windows Terminal) where mouse
    /// capture blocks copy/paste — particularly the OAuth flow's ~600-char
    /// authorization URL, which becomes effectively impossible to copy out
    /// when the TUI captures every mouse event. See pi_agent_rust#78.
    ///
    /// Env override: `PI_NO_MOUSE_CAPTURE=1`.
    #[serde(alias = "disableMouseCapture", alias = "noMouseCapture")]
    pub disable_mouse_capture: Option<bool>,

    // Model Configuration
    #[serde(alias = "defaultProvider")]
    pub default_provider: Option<String>,
    #[serde(alias = "defaultModel")]
    pub default_model: Option<String>,
    #[serde(alias = "defaultThinkingLevel")]
    pub default_thinking_level: Option<String>,
    #[serde(alias = "enabledModels")]
    pub enabled_models: Option<Vec<String>>,

    /// HTTP request timeout in seconds for provider API calls.
    ///
    /// Bounds connect + request + first-response-header latency for each
    /// provider request. `0` disables the timeout entirely (unbounded).
    ///
    /// When unset, the default is provider-aware: 60s for cloud providers and
    /// 600s for local providers (Ollama, LM Studio) where the first request can
    /// block while the model loads into memory. Overridden by the
    /// `--request-timeout` CLI flag / `PI_HTTP_REQUEST_TIMEOUT_SECS` env var.
    /// See pi_agent_rust#90.
    #[serde(alias = "requestTimeoutSecs", alias = "requestTimeoutSeconds")]
    pub request_timeout_secs: Option<u64>,

    // Message Handling
    #[serde(alias = "steeringMode", alias = "queueMode")]
    pub steering_mode: Option<String>,
    #[serde(alias = "followUpMode")]
    pub follow_up_mode: Option<String>,

    // Version check
    #[serde(alias = "checkForUpdates")]
    pub check_for_updates: Option<bool>,

    // Terminal Behavior
    #[serde(alias = "quietStartup")]
    pub quiet_startup: Option<bool>,
    #[serde(alias = "collapseChangelog")]
    pub collapse_changelog: Option<bool>,
    #[serde(alias = "lastChangelogVersion")]
    pub last_changelog_version: Option<String>,
    #[serde(alias = "doubleEscapeAction")]
    pub double_escape_action: Option<String>,
    #[serde(alias = "editorPaddingX")]
    pub editor_padding_x: Option<u32>,
    #[serde(alias = "autocompleteMaxVisible")]
    pub autocomplete_max_visible: Option<u32>,
    /// Non-interactive session picker selection (1-based index).
    #[serde(alias = "sessionPickerInput")]
    pub session_picker_input: Option<u32>,
    /// Session persistence backend: `jsonl` (default) or `sqlite` (requires `sqlite-sessions`).
    #[serde(alias = "sessionStore", alias = "sessionBackend")]
    pub session_store: Option<String>,
    /// Session durability mode: `strict`, `balanced` (default), or `throughput`.
    #[serde(alias = "sessionDurability")]
    pub session_durability: Option<String>,

    // Compaction
    pub compaction: Option<CompactionSettings>,

    // Branch Summarization
    #[serde(alias = "branchSummary")]
    pub branch_summary: Option<BranchSummarySettings>,

    // Retry Configuration
    pub retry: Option<RetrySettings>,

    // Shell
    #[serde(alias = "shellPath")]
    pub shell_path: Option<String>,
    #[serde(alias = "shellCommandPrefix")]
    pub shell_command_prefix: Option<String>,
    /// Override path to GitHub CLI (`gh`) for features like `/share`.
    #[serde(alias = "ghPath")]
    pub gh_path: Option<String>,

    // Images
    pub images: Option<ImageSettings>,

    // Markdown rendering
    pub markdown: Option<MarkdownSettings>,

    // Terminal Display
    pub terminal: Option<TerminalSettings>,

    // Thinking Budgets
    #[serde(alias = "thinkingBudgets")]
    pub thinking_budgets: Option<ThinkingBudgets>,

    // Extensions/Skills/etc.
    pub packages: Option<Vec<PackageSource>>,
    pub extensions: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub prompts: Option<Vec<String>>,
    pub themes: Option<Vec<String>>,
    #[serde(alias = "enableSkillCommands")]
    pub enable_skill_commands: Option<bool>,

    // Extension tool hook behavior
    #[serde(alias = "failClosedHooks")]
    pub fail_closed_hooks: Option<bool>,

    // Extension Policy
    #[serde(alias = "extensionPolicy")]
    pub extension_policy: Option<ExtensionPolicyConfig>,

    // Repair Policy
    #[serde(alias = "repairPolicy")]
    pub repair_policy: Option<RepairPolicyConfig>,

    // Runtime Risk Controller
    #[serde(alias = "extensionRisk")]
    pub extension_risk: Option<ExtensionRiskConfig>,
}

/// Extension capability policy configuration.
///
/// Controls which dangerous capabilities (exec, env) are available to extensions.
/// Can be set in `settings.json` or via the `--extension-policy` CLI flag.
///
/// # Example (settings.json)
///
/// ```json
/// {
///   "extensionPolicy": {
///     "defaultPermissive": true,
///     "allowDangerous": false
///   }
/// }
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtensionPolicyConfig {
    /// Policy profile: "safe", "balanced", or "permissive".
    /// Legacy alias "standard" is also accepted.
    pub profile: Option<String>,
    /// Toggle the fallback profile when `profile` is omitted.
    #[serde(alias = "defaultPermissive")]
    pub default_permissive: Option<bool>,
    /// Allow dangerous capabilities (exec, env). Overrides profile's deny list.
    #[serde(alias = "allowDangerous")]
    pub allow_dangerous: Option<bool>,
}

/// Repair policy configuration.
///
/// Controls how the agent handles broken or incompatible extensions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RepairPolicyConfig {
    /// Repair mode: "off", "suggest" (default), "auto-safe", "auto-strict".
    pub mode: Option<String>,
}

/// Runtime risk controller configuration for extension hostcalls.
///
/// Deterministic, non-LLM controls for dynamic hardening/denial decisions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtensionRiskConfig {
    /// Enable runtime risk controller.
    pub enabled: Option<bool>,
    /// Type-I error target for sequential detector (0 < alpha < 1).
    pub alpha: Option<f64>,
    /// Sliding window size for residual/drift checks.
    #[serde(alias = "windowSize")]
    pub window_size: Option<u32>,
    /// Max in-memory risk ledger entries.
    #[serde(alias = "ledgerLimit")]
    pub ledger_limit: Option<u32>,
    /// Max budget per risk decision in milliseconds.
    #[serde(alias = "decisionTimeoutMs")]
    pub decision_timeout_ms: Option<u64>,
    /// Fail closed when controller evaluation errors or exceeds budget.
    #[serde(alias = "failClosed")]
    pub fail_closed: Option<bool>,
    /// Enforcement mode: `true` = enforce risk decisions, `false` = shadow
    /// mode (score-only, no blocking).  Defaults to `true` when risk is
    /// enabled.
    pub enforce: Option<bool>,
}

/// Resolved extension policy plus explainability metadata.
#[derive(Debug, Clone)]
pub struct ResolvedExtensionPolicy {
    /// Raw profile token selected by precedence resolution.
    pub requested_profile: String,
    /// Effective normalized profile name after fallback.
    pub effective_profile: String,
    /// Source of the selected profile token: cli, env, config, or default.
    pub profile_source: &'static str,
    /// Whether dangerous capabilities were explicitly enabled.
    pub allow_dangerous: bool,
    /// Final effective policy used by runtime components.
    pub policy: crate::extensions::ExtensionPolicy,
    /// Audit trail for dangerous-capability opt-in, if `allow_dangerous`
    /// was true and modified the policy. `None` when no opt-in occurred.
    pub dangerous_opt_in_audit: Option<crate::extensions::DangerousOptInAuditEntry>,
}

/// Resolved repair policy plus explainability metadata.
#[derive(Debug, Clone)]
pub struct ResolvedRepairPolicy {
    /// Raw mode token selected by precedence resolution.
    pub requested_mode: String,
    /// Effective mode after normalization.
    pub effective_mode: crate::extensions::RepairPolicyMode,
    /// Source of the selected mode token: cli, env, config, or default.
    pub source: &'static str,
}

/// Resolved runtime risk settings plus source metadata.
#[derive(Debug, Clone)]
pub struct ResolvedExtensionRisk {
    /// Source of the resolved settings: env, config, or default.
    pub source: &'static str,
    /// Effective settings used by the extension runtime.
    pub settings: crate::extensions::RuntimeRiskConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionSettings {
    pub enabled: Option<bool>,
    #[serde(alias = "reserveTokens")]
    pub reserve_tokens: Option<u32>,
    #[serde(alias = "keepRecentTokens")]
    pub keep_recent_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BranchSummarySettings {
    #[serde(alias = "reserveTokens")]
    pub reserve_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RetrySettings {
    pub enabled: Option<bool>,
    #[serde(alias = "maxRetries")]
    pub max_retries: Option<u32>,
    #[serde(alias = "baseDelayMs")]
    pub base_delay_ms: Option<u32>,
    #[serde(alias = "maxDelayMs")]
    pub max_delay_ms: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageSettings {
    #[serde(alias = "autoResize")]
    pub auto_resize: Option<bool>,
    #[serde(alias = "blockImages")]
    pub block_images: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MarkdownSettings {
    /// Indentation (in spaces) applied to code blocks in rendered output.
    #[serde(
        alias = "codeBlockIndent",
        deserialize_with = "deserialize_code_block_indent_option"
    )]
    pub code_block_indent: Option<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    #[serde(alias = "showImages")]
    pub show_images: Option<bool>,
    #[serde(alias = "clearOnShrink")]
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
    pub max: Option<u32>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingsScope {
    Global,
    Project,
}

/// Map a [`PolicyProfile`] to its normalized string name.
const fn effective_profile_str(profile: crate::extensions::PolicyProfile) -> &'static str {
    match profile {
        crate::extensions::PolicyProfile::Safe => "safe",
        crate::extensions::PolicyProfile::Standard => "balanced",
        crate::extensions::PolicyProfile::Permissive => "permissive",
    }
}

impl Config {
    /// Load configuration from global and project settings.
    pub fn load() -> Result<Self> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let config_path = Self::config_path_override_from_env(&cwd);
        Self::load_with_roots(config_path.as_deref(), &Self::global_dir(), &cwd)
    }

    /// Resolve a config override path relative to the supplied cwd.
    #[must_use]
    pub(crate) fn resolve_config_override_path(path: &Path, cwd: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }
    }

    /// Resolve the `PI_CONFIG_PATH` override relative to the supplied cwd.
    #[must_use]
    pub fn config_path_override_from_env(cwd: &Path) -> Option<PathBuf> {
        std::env::var_os("PI_CONFIG_PATH")
            .map(PathBuf::from)
            .map(|path| Self::resolve_config_override_path(&path, cwd))
    }

    /// Get the global configuration directory.
    pub fn global_dir() -> PathBuf {
        global_dir_from_env(env_lookup)
    }

    /// Get the project configuration directory.
    pub fn project_dir() -> PathBuf {
        PathBuf::from(".pi")
    }

    /// Get the sessions directory.
    pub fn sessions_dir() -> PathBuf {
        let global_dir = Self::global_dir();
        sessions_dir_from_env(env_lookup, &global_dir)
    }

    /// Get the package directory.
    pub fn package_dir() -> PathBuf {
        let global_dir = Self::global_dir();
        package_dir_from_env(env_lookup, &global_dir)
    }

    /// Get the extension index cache file path.
    pub fn extension_index_path() -> PathBuf {
        let global_dir = Self::global_dir();
        extension_index_path_from_env(env_lookup, &global_dir)
    }

    /// Get the auth file path.
    pub fn auth_path() -> PathBuf {
        Self::global_dir().join("auth.json")
    }

    /// Get the extension permissions file path.
    pub fn permissions_path() -> PathBuf {
        Self::global_dir().join("extension-permissions.json")
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
    fn load_from_path(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(path)?;
        if content.trim().is_empty() {
            return Ok(Self::default());
        }

        let config: Self = serde_json::from_str(&content).map_err(|e| {
            Error::config(format!(
                "Failed to parse settings file {}: {e}",
                path.display()
            ))
        })?;
        Ok(config)
    }

    pub fn load_with_roots(
        config_path: Option<&std::path::Path>,
        global_dir: &std::path::Path,
        cwd: &std::path::Path,
    ) -> Result<Self> {
        if let Some(path) = config_path {
            let config = Self::load_from_path(&Self::resolve_config_override_path(path, cwd))?;
            config.emit_queue_mode_diagnostics();
            return Ok(config);
        }

        let global = Self::load_from_path(&global_dir.join("settings.json"))?;
        let project = Self::load_from_path(&cwd.join(Self::project_dir()).join("settings.json"))?;
        let merged = Self::merge(global, project);
        merged.emit_queue_mode_diagnostics();
        Ok(merged)
    }

    pub fn settings_path_with_roots(
        scope: SettingsScope,
        global_dir: &Path,
        cwd: &Path,
    ) -> PathBuf {
        match scope {
            SettingsScope::Global => global_dir.join("settings.json"),
            SettingsScope::Project => cwd.join(Self::project_dir()).join("settings.json"),
        }
    }

    pub fn patch_settings_with_roots(
        scope: SettingsScope,
        global_dir: &Path,
        cwd: &Path,
        patch: Value,
    ) -> Result<PathBuf> {
        let path = Self::settings_path_with_roots(scope, global_dir, cwd);
        patch_settings_file(&path, patch)?;
        Ok(path)
    }

    pub fn patch_settings_to_path(path: &Path, patch: Value) -> Result<PathBuf> {
        patch_settings_file(path, patch)?;
        Ok(path.to_path_buf())
    }

    /// Merge two configurations, with `other` taking precedence.
    pub fn merge(base: Self, other: Self) -> Self {
        Self {
            // Appearance
            theme: other.theme.or(base.theme),
            hide_thinking_block: other.hide_thinking_block.or(base.hide_thinking_block),
            show_hardware_cursor: other.show_hardware_cursor.or(base.show_hardware_cursor),
            disable_mouse_capture: other.disable_mouse_capture.or(base.disable_mouse_capture),

            // Model Configuration
            default_provider: other.default_provider.or(base.default_provider),
            default_model: other.default_model.or(base.default_model),
            default_thinking_level: other.default_thinking_level.or(base.default_thinking_level),
            enabled_models: other.enabled_models.or(base.enabled_models),
            request_timeout_secs: other.request_timeout_secs.or(base.request_timeout_secs),

            // Message Handling
            steering_mode: other.steering_mode.or(base.steering_mode),
            follow_up_mode: other.follow_up_mode.or(base.follow_up_mode),

            // Version check
            check_for_updates: other.check_for_updates.or(base.check_for_updates),

            // Terminal Behavior
            quiet_startup: other.quiet_startup.or(base.quiet_startup),
            collapse_changelog: other.collapse_changelog.or(base.collapse_changelog),
            last_changelog_version: other.last_changelog_version.or(base.last_changelog_version),
            double_escape_action: other.double_escape_action.or(base.double_escape_action),
            editor_padding_x: other.editor_padding_x.or(base.editor_padding_x),
            autocomplete_max_visible: other
                .autocomplete_max_visible
                .or(base.autocomplete_max_visible),
            session_picker_input: other.session_picker_input.or(base.session_picker_input),
            session_store: other.session_store.or(base.session_store),
            session_durability: other.session_durability.or(base.session_durability),

            // Compaction
            compaction: merge_compaction(base.compaction, other.compaction),

            // Branch Summarization
            branch_summary: merge_branch_summary(base.branch_summary, other.branch_summary),

            // Retry Configuration
            retry: merge_retry(base.retry, other.retry),

            // Shell
            shell_path: other.shell_path.or(base.shell_path),
            shell_command_prefix: other.shell_command_prefix.or(base.shell_command_prefix),
            gh_path: other.gh_path.or(base.gh_path),

            // Images
            images: merge_images(base.images, other.images),

            // Markdown rendering
            markdown: merge_markdown(base.markdown, other.markdown),

            // Terminal Display
            terminal: merge_terminal(base.terminal, other.terminal),

            // Thinking Budgets
            thinking_budgets: merge_thinking_budgets(base.thinking_budgets, other.thinking_budgets),

            // Extensions/Skills/etc.
            packages: other.packages.or(base.packages),
            extensions: other.extensions.or(base.extensions),
            skills: other.skills.or(base.skills),
            prompts: other.prompts.or(base.prompts),
            themes: other.themes.or(base.themes),
            enable_skill_commands: other.enable_skill_commands.or(base.enable_skill_commands),
            fail_closed_hooks: other.fail_closed_hooks.or(base.fail_closed_hooks),

            // Extension Policy
            extension_policy: merge_extension_policy(base.extension_policy, other.extension_policy),

            // Repair Policy
            repair_policy: merge_repair_policy(base.repair_policy, other.repair_policy),

            // Runtime Risk Controller
            extension_risk: merge_extension_risk(base.extension_risk, other.extension_risk),
        }
    }

    // === Accessor methods with defaults ===

    pub fn compaction_enabled(&self) -> bool {
        self.compaction
            .as_ref()
            .and_then(|c| c.enabled)
            .unwrap_or(true)
    }

    pub fn steering_queue_mode(&self) -> QueueMode {
        parse_queue_mode_or_default(self.steering_mode.as_deref())
    }

    pub fn follow_up_queue_mode(&self) -> QueueMode {
        parse_queue_mode_or_default(self.follow_up_mode.as_deref())
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

    /// Whether to check for version updates on startup (default: true).
    pub fn should_check_for_updates(&self) -> bool {
        self.check_for_updates.unwrap_or(true)
    }

    pub fn image_block_images(&self) -> bool {
        self.images
            .as_ref()
            .and_then(|i| i.block_images)
            .unwrap_or(false)
    }

    pub fn terminal_show_images(&self) -> bool {
        self.terminal
            .as_ref()
            .and_then(|t| t.show_images)
            .unwrap_or(true)
    }

    pub fn terminal_clear_on_shrink(&self) -> bool {
        self.terminal_clear_on_shrink_with_lookup(env_lookup)
    }

    fn terminal_clear_on_shrink_with_lookup<F>(&self, get_env: F) -> bool
    where
        F: Fn(&str) -> Option<String>,
    {
        if let Some(value) = self.terminal.as_ref().and_then(|t| t.clear_on_shrink) {
            return value;
        }
        get_env("PI_CLEAR_ON_SHRINK").is_some_and(|value| value == "1")
    }

    pub fn thinking_budget(&self, level: &str) -> u32 {
        let budgets = self.thinking_budgets.as_ref();
        match level {
            "minimal" => budgets.and_then(|b| b.minimal).unwrap_or(1024),
            "low" => budgets.and_then(|b| b.low).unwrap_or(2048),
            "medium" => budgets.and_then(|b| b.medium).unwrap_or(8192),
            "high" => budgets.and_then(|b| b.high).unwrap_or(16384),
            "xhigh" => budgets.and_then(|b| b.xhigh).unwrap_or(32768),
            "max" => budgets.and_then(|b| b.max).unwrap_or(65536),
            _ => 0,
        }
    }

    pub fn markdown_code_block_indent(&self) -> u8 {
        self.markdown
            .as_ref()
            .and_then(|m| m.code_block_indent)
            .unwrap_or(2)
    }

    pub fn enable_skill_commands(&self) -> bool {
        self.enable_skill_commands.unwrap_or(true)
    }

    pub fn fail_closed_hooks(&self) -> bool {
        if let Some(value) = parse_env_bool("PI_EXTENSION_HOOKS_FAIL_CLOSED") {
            return value;
        }
        self.fail_closed_hooks.unwrap_or(false)
    }

    /// Resolve the extension policy from config, CLI override, and env var.
    ///
    /// Resolution order (highest precedence first):
    /// 1. `cli_override` (from `--extension-policy` flag)
    /// 2. `PI_EXTENSION_POLICY` environment variable
    /// 3. `extension_policy.profile` from settings.json
    /// 4. `extension_policy.default_permissive` from settings.json
    /// 5. Default: "permissive"
    ///
    /// If `allow_dangerous` is true (from config or env), exec/env are removed
    /// from the policy's deny list.
    pub fn resolve_extension_policy_with_metadata(
        &self,
        cli_override: Option<&str>,
    ) -> ResolvedExtensionPolicy {
        use crate::extensions::PolicyProfile;

        // Determine profile name with source: CLI > env > config > default
        let (requested_profile, profile_source) = cli_override.map_or_else(
            || {
                std::env::var("PI_EXTENSION_POLICY").map_or_else(
                    |_| {
                        self.extension_policy
                            .as_ref()
                            .and_then(|p| p.profile.clone())
                            .map_or_else(
                                || {
                                    self.extension_policy
                                        .as_ref()
                                        .and_then(|p| p.default_permissive)
                                        .map_or_else(
                                            || ("permissive".to_string(), "default"),
                                            |default_permissive| {
                                                (
                                                    if default_permissive {
                                                        "permissive"
                                                    } else {
                                                        "safe"
                                                    }
                                                    .to_string(),
                                                    "config",
                                                )
                                            },
                                        )
                                },
                                |value| (value, "config"),
                            )
                    },
                    |value| (value, "env"),
                )
            },
            |value| (value.to_string(), "cli"),
        );

        let normalized_profile = requested_profile.to_ascii_lowercase();
        let profile = if normalized_profile == "safe" {
            PolicyProfile::Safe
        } else if normalized_profile == "permissive" {
            PolicyProfile::Permissive
        } else if normalized_profile == "balanced" || normalized_profile == "standard" {
            // "balanced" (and legacy "standard") map to the standard policy.
            PolicyProfile::Standard
        } else {
            // Unknown values fail closed to the safe profile.
            tracing::warn!(
                requested = %normalized_profile,
                fallback = "safe",
                "Unknown extension policy profile; falling back to safe"
            );
            PolicyProfile::Safe
        };

        let mut policy = profile.to_policy();

        // Check allow_dangerous: config setting or PI_EXTENSION_ALLOW_DANGEROUS env
        let config_allows = self
            .extension_policy
            .as_ref()
            .and_then(|p| p.allow_dangerous)
            .unwrap_or(false);
        let env_allows = std::env::var("PI_EXTENSION_ALLOW_DANGEROUS")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        let allow_dangerous = config_allows || env_allows;

        // Build audit trail before mutating deny_caps.
        let dangerous_opt_in_audit = if allow_dangerous {
            let source = if env_allows { "env" } else { "config" }.to_string();
            let unblocked: Vec<String> = policy
                .deny_caps
                .iter()
                .filter(|cap| *cap == "exec" || *cap == "env")
                .cloned()
                .collect();
            if !unblocked.is_empty() {
                tracing::warn!(
                    source = %source,
                    profile = %effective_profile_str(profile),
                    capabilities = ?unblocked,
                    "Dangerous capabilities explicitly unblocked via allow_dangerous"
                );
            }
            Some(crate::extensions::DangerousOptInAuditEntry {
                source,
                profile: effective_profile_str(profile).to_string(),
                capabilities_unblocked: unblocked,
            })
        } else {
            None
        };

        if allow_dangerous {
            policy.deny_caps.retain(|cap| cap != "exec" && cap != "env");
        }

        let effective_profile = effective_profile_str(profile);

        ResolvedExtensionPolicy {
            requested_profile,
            effective_profile: effective_profile.to_string(),
            profile_source,
            allow_dangerous,
            policy,
            dangerous_opt_in_audit,
        }
    }

    pub fn resolve_extension_policy(
        &self,
        cli_override: Option<&str>,
    ) -> crate::extensions::ExtensionPolicy {
        self.resolve_extension_policy_with_metadata(cli_override)
            .policy
    }

    /// Resolve the repair policy from config, CLI override, and env var.
    ///
    /// Resolution order (highest precedence first):
    /// 1. `cli_override` (from `--repair-policy` flag)
    /// 2. `PI_REPAIR_POLICY` environment variable
    /// 3. `repair_policy.mode` from settings.json
    /// 4. Default: "suggest"
    pub fn resolve_repair_policy_with_metadata(
        &self,
        cli_override: Option<&str>,
    ) -> ResolvedRepairPolicy {
        use crate::extensions::RepairPolicyMode;

        // Determine mode string with source: CLI > env > config > default
        let (requested_mode, source) = cli_override.map_or_else(
            || {
                std::env::var("PI_REPAIR_POLICY").map_or_else(
                    |_| {
                        self.repair_policy
                            .as_ref()
                            .and_then(|p| p.mode.clone())
                            .map_or_else(
                                || ("suggest".to_string(), "default"),
                                |value| (value, "config"),
                            )
                    },
                    |value| (value, "env"),
                )
            },
            |value| (value.to_string(), "cli"),
        );

        let effective_mode = match requested_mode.trim().to_ascii_lowercase().as_str() {
            "off" => RepairPolicyMode::Off,
            "auto-safe" => RepairPolicyMode::AutoSafe,
            "auto-strict" => RepairPolicyMode::AutoStrict,
            _ => RepairPolicyMode::Suggest, // Fallback to safe default
        };

        ResolvedRepairPolicy {
            requested_mode,
            effective_mode,
            source,
        }
    }

    pub fn resolve_repair_policy(
        &self,
        cli_override: Option<&str>,
    ) -> crate::extensions::RepairPolicyMode {
        self.resolve_repair_policy_with_metadata(cli_override)
            .effective_mode
    }

    /// Resolve runtime risk controller settings from config and environment.
    ///
    /// Resolution order (highest precedence first):
    /// 1. `PI_EXTENSION_RISK_*` env vars
    /// 2. `extensionRisk` config
    /// 3. deterministic defaults
    pub fn resolve_extension_risk_with_metadata(&self) -> ResolvedExtensionRisk {
        fn parse_env_f64(name: &str) -> Option<f64> {
            std::env::var(name).ok().and_then(|v| v.trim().parse().ok())
        }

        const fn sanitize_alpha(alpha: f64) -> Option<f64> {
            if alpha.is_finite() {
                Some(alpha.clamp(1.0e-6, 0.5))
            } else {
                None
            }
        }

        fn parse_env_u32(name: &str) -> Option<u32> {
            std::env::var(name).ok().and_then(|v| v.trim().parse().ok())
        }

        fn parse_env_u64(name: &str) -> Option<u64> {
            std::env::var(name).ok().and_then(|v| v.trim().parse().ok())
        }

        let mut settings = crate::extensions::RuntimeRiskConfig::default();
        let mut source = "default";

        if let Some(cfg) = self.extension_risk.as_ref() {
            if let Some(enabled) = cfg.enabled {
                settings.enabled = enabled;
                source = "config";
            }
            if let Some(alpha) = cfg.alpha.and_then(sanitize_alpha) {
                settings.alpha = alpha;
                source = "config";
            }
            if let Some(window_size) = cfg.window_size {
                settings.window_size = window_size.clamp(8, 4096) as usize;
                source = "config";
            }
            if let Some(ledger_limit) = cfg.ledger_limit {
                settings.ledger_limit = ledger_limit.clamp(32, 20_000) as usize;
                source = "config";
            }
            if let Some(timeout_ms) = cfg.decision_timeout_ms {
                settings.decision_timeout_ms = timeout_ms.clamp(1, 2_000);
                source = "config";
            }
            if let Some(fail_closed) = cfg.fail_closed {
                settings.fail_closed = fail_closed;
                source = "config";
            }
            if let Some(enforce) = cfg.enforce {
                settings.enforce = enforce;
                source = "config";
            }
        }

        if let Some(enabled) = parse_env_bool("PI_EXTENSION_RISK_ENABLED") {
            settings.enabled = enabled;
            source = "env";
        }
        if let Some(alpha) = parse_env_f64("PI_EXTENSION_RISK_ALPHA").and_then(sanitize_alpha) {
            settings.alpha = alpha;
            source = "env";
        }
        if let Some(window_size) = parse_env_u32("PI_EXTENSION_RISK_WINDOW") {
            settings.window_size = window_size.clamp(8, 4096) as usize;
            source = "env";
        }
        if let Some(ledger_limit) = parse_env_u32("PI_EXTENSION_RISK_LEDGER_LIMIT") {
            settings.ledger_limit = ledger_limit.clamp(32, 20_000) as usize;
            source = "env";
        }
        if let Some(timeout_ms) = parse_env_u64("PI_EXTENSION_RISK_DECISION_TIMEOUT_MS") {
            settings.decision_timeout_ms = timeout_ms.clamp(1, 2_000);
            source = "env";
        }
        if let Some(fail_closed) = parse_env_bool("PI_EXTENSION_RISK_FAIL_CLOSED") {
            settings.fail_closed = fail_closed;
            source = "env";
        }
        if let Some(enforce) = parse_env_bool("PI_EXTENSION_RISK_ENFORCE") {
            settings.enforce = enforce;
            source = "env";
        }

        ResolvedExtensionRisk { source, settings }
    }

    pub fn resolve_extension_risk(&self) -> crate::extensions::RuntimeRiskConfig {
        self.resolve_extension_risk_with_metadata().settings
    }

    fn emit_queue_mode_diagnostics(&self) {
        emit_queue_mode_diagnostic("steering_mode", self.steering_mode.as_deref());
        emit_queue_mode_diagnostic("follow_up_mode", self.follow_up_mode.as_deref());
    }
}

fn env_lookup(var: &str) -> Option<String> {
    std::env::var(var).ok()
}

fn parse_env_bool(name: &str) -> Option<bool> {
    std::env::var(name).ok().and_then(|v| {
        let t = v.trim();
        if t.eq_ignore_ascii_case("1")
            || t.eq_ignore_ascii_case("true")
            || t.eq_ignore_ascii_case("yes")
            || t.eq_ignore_ascii_case("on")
        {
            Some(true)
        } else if t.eq_ignore_ascii_case("0")
            || t.eq_ignore_ascii_case("false")
            || t.eq_ignore_ascii_case("no")
            || t.eq_ignore_ascii_case("off")
        {
            Some(false)
        } else {
            None
        }
    })
}

fn global_dir_from_env<F>(get_env: F) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    get_env("PI_CODING_AGENT_DIR").map_or_else(
        || {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".pi")
                .join("agent")
        },
        PathBuf::from,
    )
}

fn sessions_dir_from_env<F>(get_env: F, global_dir: &Path) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    get_env("PI_SESSIONS_DIR").map_or_else(|| global_dir.join("sessions"), PathBuf::from)
}

fn package_dir_from_env<F>(get_env: F, global_dir: &Path) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    get_env("PI_PACKAGE_DIR").map_or_else(|| global_dir.join("packages"), PathBuf::from)
}

fn extension_index_path_from_env<F>(get_env: F, global_dir: &Path) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    get_env("PI_EXTENSION_INDEX_PATH")
        .map_or_else(|| global_dir.join("extension-index.json"), PathBuf::from)
}

pub(crate) fn parse_queue_mode(mode: Option<&str>) -> Option<QueueMode> {
    match mode.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("all") => Some(QueueMode::All),
        Some("one-at-a-time") => Some(QueueMode::OneAtATime),
        _ => None,
    }
}

pub(crate) fn parse_queue_mode_or_default(mode: Option<&str>) -> QueueMode {
    parse_queue_mode(mode).unwrap_or(QueueMode::OneAtATime)
}

fn emit_queue_mode_diagnostic(setting: &'static str, mode: Option<&str>) {
    let Some(mode) = mode else {
        return;
    };

    let trimmed = mode.trim();
    if parse_queue_mode(Some(trimmed)).is_some() {
        return;
    }

    tracing::warn!(
        setting,
        value = trimmed,
        "Unknown queue mode; falling back to one-at-a-time"
    );
}

fn merge_compaction(
    base: Option<CompactionSettings>,
    other: Option<CompactionSettings>,
) -> Option<CompactionSettings> {
    match (base, other) {
        (Some(base), Some(other)) => Some(CompactionSettings {
            enabled: other.enabled.or(base.enabled),
            reserve_tokens: other.reserve_tokens.or(base.reserve_tokens),
            keep_recent_tokens: other.keep_recent_tokens.or(base.keep_recent_tokens),
        }),
        (None, Some(other)) => Some(other),
        (Some(base), None) => Some(base),
        (None, None) => None,
    }
}

fn merge_branch_summary(
    base: Option<BranchSummarySettings>,
    other: Option<BranchSummarySettings>,
) -> Option<BranchSummarySettings> {
    match (base, other) {
        (Some(base), Some(other)) => Some(BranchSummarySettings {
            reserve_tokens: other.reserve_tokens.or(base.reserve_tokens),
        }),
        (None, Some(other)) => Some(other),
        (Some(base), None) => Some(base),
        (None, None) => None,
    }
}

fn merge_retry(base: Option<RetrySettings>, other: Option<RetrySettings>) -> Option<RetrySettings> {
    match (base, other) {
        (Some(base), Some(other)) => Some(RetrySettings {
            enabled: other.enabled.or(base.enabled),
            max_retries: other.max_retries.or(base.max_retries),
            base_delay_ms: other.base_delay_ms.or(base.base_delay_ms),
            max_delay_ms: other.max_delay_ms.or(base.max_delay_ms),
        }),
        (None, Some(other)) => Some(other),
        (Some(base), None) => Some(base),
        (None, None) => None,
    }
}

fn merge_markdown(
    base: Option<MarkdownSettings>,
    other: Option<MarkdownSettings>,
) -> Option<MarkdownSettings> {
    match (base, other) {
        (Some(base), Some(other)) => Some(MarkdownSettings {
            code_block_indent: other.code_block_indent.or(base.code_block_indent),
        }),
        (None, Some(other)) => Some(other),
        (Some(base), None) => Some(base),
        (None, None) => None,
    }
}

fn deserialize_code_block_indent_option<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(number)) => number
            .as_u64()
            .and_then(|value| u8::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("markdown.codeBlockIndent must fit in u8")),
        Some(serde_json::Value::String(indent)) => u8::try_from(indent.chars().count())
            .map(Some)
            .map_err(|_| serde::de::Error::custom("markdown.codeBlockIndent string is too long")),
        Some(_) => Err(serde::de::Error::custom(
            "markdown.codeBlockIndent must be a string or integer",
        )),
    }
}

fn merge_images(
    base: Option<ImageSettings>,
    other: Option<ImageSettings>,
) -> Option<ImageSettings> {
    match (base, other) {
        (Some(base), Some(other)) => Some(ImageSettings {
            auto_resize: other.auto_resize.or(base.auto_resize),
            block_images: other.block_images.or(base.block_images),
        }),
        (None, Some(other)) => Some(other),
        (Some(base), None) => Some(base),
        (None, None) => None,
    }
}

fn merge_terminal(
    base: Option<TerminalSettings>,
    other: Option<TerminalSettings>,
) -> Option<TerminalSettings> {
    match (base, other) {
        (Some(base), Some(other)) => Some(TerminalSettings {
            show_images: other.show_images.or(base.show_images),
            clear_on_shrink: other.clear_on_shrink.or(base.clear_on_shrink),
        }),
        (None, Some(other)) => Some(other),
        (Some(base), None) => Some(base),
        (None, None) => None,
    }
}

fn merge_thinking_budgets(
    base: Option<ThinkingBudgets>,
    other: Option<ThinkingBudgets>,
) -> Option<ThinkingBudgets> {
    match (base, other) {
        (Some(base), Some(other)) => Some(ThinkingBudgets {
            minimal: other.minimal.or(base.minimal),
            low: other.low.or(base.low),
            medium: other.medium.or(base.medium),
            high: other.high.or(base.high),
            xhigh: other.xhigh.or(base.xhigh),
            max: other.max.or(base.max),
        }),
        (None, Some(other)) => Some(other),
        (Some(base), None) => Some(base),
        (None, None) => None,
    }
}

fn merge_extension_policy(
    base: Option<ExtensionPolicyConfig>,
    other: Option<ExtensionPolicyConfig>,
) -> Option<ExtensionPolicyConfig> {
    match (base, other) {
        (Some(base), Some(other)) => Some(ExtensionPolicyConfig {
            profile: other.profile.or(base.profile),
            default_permissive: other.default_permissive.or(base.default_permissive),
            allow_dangerous: other.allow_dangerous.or(base.allow_dangerous),
        }),
        (None, Some(other)) => Some(other),
        (Some(base), None) => Some(base),
        (None, None) => None,
    }
}

fn merge_repair_policy(
    base: Option<RepairPolicyConfig>,
    other: Option<RepairPolicyConfig>,
) -> Option<RepairPolicyConfig> {
    match (base, other) {
        (Some(base), Some(other)) => Some(RepairPolicyConfig {
            mode: other.mode.or(base.mode),
        }),
        (None, Some(other)) => Some(other),
        (Some(base), None) => Some(base),
        (None, None) => None,
    }
}

fn merge_extension_risk(
    base: Option<ExtensionRiskConfig>,
    other: Option<ExtensionRiskConfig>,
) -> Option<ExtensionRiskConfig> {
    match (base, other) {
        (Some(base), Some(other)) => Some(ExtensionRiskConfig {
            enabled: other.enabled.or(base.enabled),
            alpha: other.alpha.or(base.alpha),
            window_size: other.window_size.or(base.window_size),
            ledger_limit: other.ledger_limit.or(base.ledger_limit),
            decision_timeout_ms: other.decision_timeout_ms.or(base.decision_timeout_ms),
            fail_closed: other.fail_closed.or(base.fail_closed),
            enforce: other.enforce.or(base.enforce),
        }),
        (None, Some(other)) => Some(other),
        (Some(base), None) => Some(base),
        (None, None) => None,
    }
}

fn load_settings_json_object(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(serde_json::Map::new()));
    }

    let content = std::fs::read_to_string(path)?;
    if content.trim().is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    let value: Value = serde_json::from_str(&content)?;
    if !value.is_object() {
        return Err(Error::config(format!(
            "Settings file is not a JSON object: {}",
            path.display()
        )));
    }
    Ok(value)
}

fn deep_merge_settings_value(dst: &mut Value, patch: Value) -> Result<()> {
    let Value::Object(patch) = patch else {
        return Err(Error::validation("Settings patch must be a JSON object"));
    };

    let dst_obj = dst.as_object_mut().ok_or_else(|| {
        Error::config("Internal error: settings root unexpectedly not a JSON object")
    })?;

    for (key, value) in patch {
        if value.is_null() {
            dst_obj.remove(&key);
            continue;
        }

        match (dst_obj.get_mut(&key), value) {
            (Some(Value::Object(dst_child)), Value::Object(patch_child)) => {
                let mut child = Value::Object(std::mem::take(dst_child));
                deep_merge_settings_value(&mut child, Value::Object(patch_child))?;
                dst_obj.insert(key, child);
            }
            (_, other) => {
                dst_obj.insert(key, other);
            }
        }
    }
    Ok(())
}

fn write_settings_json_atomic(path: &Path, value: &Value) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.as_os_str().is_empty() {
        std::fs::create_dir_all(parent)?;
    }

    let mut contents = serde_json::to_string_pretty(value)?;
    contents.push('\n');

    let mut tmp = NamedTempFile::new_in(parent)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let perms = std::fs::Permissions::from_mode(0o600);
        tmp.as_file().set_permissions(perms)?;
    }

    tmp.write_all(contents.as_bytes())?;
    tmp.as_file().sync_all()?;

    tmp.persist(path).map_err(|err| {
        Error::config(format!(
            "Failed to persist settings file to {}: {}",
            path.display(),
            err.error
        ))
    })?;
    sync_settings_parent_dir(path)?;

    Ok(())
}

fn patch_settings_file(path: &Path, patch: Value) -> Result<Value> {
    let _process_guard = settings_persist_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Directory-based lock compatible with upstream TS pi's `proper-lockfile`
    // (see `crate::file_lock`); the in-process `settings_persist_lock` above still
    // serializes threads within this process.
    let _file_guard = crate::file_lock::DirLock::acquire_for(path, Duration::from_secs(30))
        .map_err(|e| Error::config(format!("settings lock: {e}")))?;
    let mut settings = load_settings_json_object(path)?;
    deep_merge_settings_value(&mut settings, patch)?;
    write_settings_json_atomic(path, &settings)?;
    Ok(settings)
}

fn settings_persist_lock() -> &'static Mutex<()> {
    static PERSIST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    PERSIST_LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(unix)]
fn sync_settings_parent_dir(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_settings_parent_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        BranchSummarySettings, CompactionSettings, Config, ExtensionPolicyConfig,
        ExtensionRiskConfig, ImageSettings, RepairPolicyConfig, RetrySettings, SettingsScope,
        TerminalSettings, ThinkingBudgets, deep_merge_settings_value,
        extension_index_path_from_env, global_dir_from_env, merge_branch_summary, merge_compaction,
        merge_extension_policy, merge_extension_risk, merge_images, merge_repair_policy,
        merge_retry, merge_terminal, merge_thinking_budgets, package_dir_from_env,
        sessions_dir_from_env,
    };
    use crate::agent::QueueMode;
    use proptest::prelude::*;
    use proptest::string::string_regex;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};
    use tempfile::TempDir;

    fn write_file(path: &std::path::Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(path, contents).expect("write file");
    }

    #[test]
    fn load_returns_defaults_when_missing() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load config");
        assert!(config.theme.is_none());
        assert!(config.default_provider.is_none());
        assert!(config.default_model.is_none());
    }

    #[test]
    fn load_respects_pi_config_path_override() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "theme": "global", "default_provider": "anthropic" }"#,
        );
        write_file(
            &cwd.join(".pi/settings.json"),
            r#"{ "theme": "project", "default_provider": "google" }"#,
        );

        let override_path = temp.path().join("override.json");
        write_file(
            &override_path,
            r#"{ "theme": "override", "default_provider": "openai" }"#,
        );

        let config =
            Config::load_with_roots(Some(&override_path), &global_dir, &cwd).expect("load config");
        assert_eq!(config.theme.as_deref(), Some("override"));
        assert_eq!(config.default_provider.as_deref(), Some("openai"));
    }

    #[test]
    fn resolve_config_override_path_anchors_relative_paths_to_supplied_cwd() {
        let cwd = PathBuf::from("/tmp/pi-agent");
        let relative = PathBuf::from("config/override.json");
        let absolute = PathBuf::from("/etc/pi/settings.json");

        assert_eq!(
            Config::resolve_config_override_path(&relative, &cwd),
            cwd.join("config/override.json")
        );
        assert_eq!(
            Config::resolve_config_override_path(&absolute, &cwd),
            absolute
        );
    }

    #[test]
    fn load_with_roots_resolves_relative_override_against_supplied_cwd() {
        let temp = TempDir::new().expect("create tempdir");
        let unrelated = temp.path().join("unrelated");
        std::fs::create_dir_all(&unrelated).expect("create unrelated dir");

        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        let override_dir = cwd.join("config");
        std::fs::create_dir_all(&override_dir).expect("create override dir");
        write_file(
            &override_dir.join("override.json"),
            r#"{ "theme": "override", "default_provider": "openai" }"#,
        );

        let config = Config::load_with_roots(
            Some(std::path::Path::new("config/override.json")),
            &global_dir,
            &cwd,
        )
        .expect("load config");

        assert_eq!(config.theme.as_deref(), Some("override"));
        assert_eq!(config.default_provider.as_deref(), Some("openai"));
    }

    #[test]
    fn load_merges_project_over_global() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "default_provider": "anthropic", "default_model": "global", "theme": "global" }"#,
        );
        write_file(
            &cwd.join(".pi/settings.json"),
            r#"{ "default_model": "project" }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load config");
        assert_eq!(config.default_provider.as_deref(), Some("anthropic"));
        assert_eq!(config.default_model.as_deref(), Some("project"));
        assert_eq!(config.theme.as_deref(), Some("global"));
    }

    #[test]
    fn load_merges_nested_structs_instead_of_overriding() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "compaction": { "enabled": true, "reserve_tokens": 1234, "keep_recent_tokens": 5678 } }"#,
        );
        write_file(
            &cwd.join(".pi/settings.json"),
            r#"{ "compaction": { "enabled": false } }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load config");
        assert!(!config.compaction_enabled());
        assert_eq!(config.compaction_reserve_tokens(), 1234);
        assert_eq!(config.compaction_keep_recent_tokens(), 5678);
    }

    #[test]
    fn load_parses_retry_images_terminal_and_shell_fields() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{
                "compaction": { "enabled": false, "reserve_tokens": 4444, "keep_recent_tokens": 5555 },
                "retry": { "enabled": false, "max_retries": 9, "base_delay_ms": 101, "max_delay_ms": 202 },
                "images": { "auto_resize": false, "block_images": true },
                "terminal": { "show_images": false, "clear_on_shrink": true },
                "shell_path": "/bin/zsh",
                "shell_command_prefix": "set -euo pipefail"
            }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load config");
        assert!(!config.compaction_enabled());
        assert_eq!(config.compaction_reserve_tokens(), 4444);
        assert_eq!(config.compaction_keep_recent_tokens(), 5555);
        assert!(!config.retry_enabled());
        assert_eq!(config.retry_max_retries(), 9);
        assert_eq!(config.retry_base_delay_ms(), 101);
        assert_eq!(config.retry_max_delay_ms(), 202);
        assert!(!config.image_auto_resize());
        assert!(!config.terminal_show_images());
        assert!(config.terminal_clear_on_shrink());
        assert_eq!(config.shell_path.as_deref(), Some("/bin/zsh"));
        assert_eq!(
            config.shell_command_prefix.as_deref(),
            Some("set -euo pipefail")
        );
    }

    #[test]
    fn accessors_use_expected_defaults() {
        let config = Config::default();
        assert!(config.compaction_enabled());
        assert_eq!(config.compaction_reserve_tokens(), 16384);
        assert_eq!(config.compaction_keep_recent_tokens(), 20000);
        assert!(config.retry_enabled());
        assert_eq!(config.retry_max_retries(), 3);
        assert_eq!(config.retry_base_delay_ms(), 2000);
        assert_eq!(config.retry_max_delay_ms(), 60000);
        assert!(config.image_auto_resize());
        assert!(config.terminal_show_images());
        assert!(!config.terminal_clear_on_shrink());
        assert!(config.shell_path.is_none());
        assert!(config.shell_command_prefix.is_none());
    }

    #[test]
    fn directory_helpers_honor_environment_overrides() {
        let env = HashMap::from([
            ("PI_CODING_AGENT_DIR".to_string(), "env-root".to_string()),
            ("PI_SESSIONS_DIR".to_string(), "env-sessions".to_string()),
            ("PI_PACKAGE_DIR".to_string(), "env-packages".to_string()),
            (
                "PI_EXTENSION_INDEX_PATH".to_string(),
                "env-extension-index.json".to_string(),
            ),
        ]);

        let global = global_dir_from_env(|key| env.get(key).cloned());
        let sessions = sessions_dir_from_env(|key| env.get(key).cloned(), &global);
        let package = package_dir_from_env(|key| env.get(key).cloned(), &global);
        let extension_index = extension_index_path_from_env(|key| env.get(key).cloned(), &global);

        assert_eq!(global, PathBuf::from("env-root"));
        assert_eq!(sessions, PathBuf::from("env-sessions"));
        assert_eq!(package, PathBuf::from("env-packages"));
        assert_eq!(extension_index, PathBuf::from("env-extension-index.json"));
    }

    #[test]
    fn directory_helpers_fall_back_to_global_subdirs_when_unset() {
        let env = HashMap::from([("PI_CODING_AGENT_DIR".to_string(), "root-dir".to_string())]);
        let global = global_dir_from_env(|key| env.get(key).cloned());
        let sessions = sessions_dir_from_env(|key| env.get(key).cloned(), &global);
        let package = package_dir_from_env(|key| env.get(key).cloned(), &global);
        let extension_index = extension_index_path_from_env(|key| env.get(key).cloned(), &global);

        assert_eq!(global, PathBuf::from("root-dir"));
        assert_eq!(sessions, PathBuf::from("root-dir").join("sessions"));
        assert_eq!(package, PathBuf::from("root-dir").join("packages"));
        assert_eq!(
            extension_index,
            PathBuf::from("root-dir").join("extension-index.json")
        );
    }

    #[test]
    fn patch_settings_deep_merges_and_preserves_other_fields() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        let settings_path =
            Config::settings_path_with_roots(SettingsScope::Project, &global_dir, &cwd);

        write_file(
            &settings_path,
            r#"{ "theme": "dark", "compaction": { "reserve_tokens": 111 } }"#,
        );

        let updated = Config::patch_settings_with_roots(
            SettingsScope::Project,
            &global_dir,
            &cwd,
            json!({ "compaction": { "enabled": false } }),
        )
        .expect("patch settings");

        assert_eq!(updated, settings_path);

        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).expect("read"))
                .expect("parse");
        assert_eq!(stored["theme"], json!("dark"));
        assert_eq!(stored["compaction"]["reserve_tokens"], json!(111));
        assert_eq!(stored["compaction"]["enabled"], json!(false));
    }

    #[test]
    fn patch_settings_serializes_concurrent_updates() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        let settings_path =
            Config::settings_path_with_roots(SettingsScope::Project, &global_dir, &cwd);

        write_file(&settings_path, r#"{ "theme": "dark" }"#);

        let barrier = Arc::new(Barrier::new(12));
        let mut handles = Vec::new();

        for idx in 0..12 {
            let barrier = Arc::clone(&barrier);
            let cwd = cwd.clone();
            let global_dir = global_dir.clone();
            handles.push(std::thread::spawn(move || {
                let mut patch = serde_json::Map::new();
                patch.insert(format!("concurrent_{idx}"), json!(idx));
                barrier.wait();
                Config::patch_settings_with_roots(
                    SettingsScope::Project,
                    &global_dir,
                    &cwd,
                    Value::Object(patch),
                )
                .expect("patch settings")
            }));
        }

        for handle in handles {
            handle.join().expect("join patch thread");
        }

        let stored: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).expect("read settings"))
                .expect("parse settings");
        assert_eq!(stored["theme"], json!("dark"));
        for idx in 0..12 {
            let key = format!("concurrent_{idx}");
            let expected = json!(idx);
            assert_eq!(stored.get(&key), Some(&expected));
        }
    }

    #[test]
    fn patch_settings_writes_with_restrictive_permissions() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        Config::patch_settings_with_roots(
            SettingsScope::Project,
            &global_dir,
            &cwd,
            json!({ "default_provider": "anthropic" }),
        )
        .expect("patch settings");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let settings_path =
                Config::settings_path_with_roots(SettingsScope::Project, &global_dir, &cwd);
            let mode = std::fs::metadata(&settings_path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn patch_settings_to_path_updates_explicit_file() {
        let temp = TempDir::new().expect("create tempdir");
        let path = temp.path().join("override").join("settings.json");

        let updated =
            Config::patch_settings_to_path(&path, json!({ "default_provider": "anthropic" }))
                .expect("patch settings");

        assert_eq!(updated, path);

        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(stored["default_provider"], json!("anthropic"));
    }

    #[test]
    fn patch_settings_applies_theme_and_queue_modes() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");

        Config::patch_settings_with_roots(
            SettingsScope::Project,
            &global_dir,
            &cwd,
            json!({
                "theme": "solarized",
                "steeringMode": "all",
                "followUpMode": "one-at-a-time",
                "editor_padding_x": 4,
                "show_hardware_cursor": true,
            }),
        )
        .expect("patch settings");

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load config");
        assert_eq!(config.theme.as_deref(), Some("solarized"));
        assert_eq!(config.steering_queue_mode(), QueueMode::All);
        assert_eq!(config.follow_up_queue_mode(), QueueMode::OneAtATime);
        assert_eq!(config.editor_padding_x, Some(4));
        assert_eq!(config.show_hardware_cursor, Some(true));
    }

    #[test]
    fn load_with_invalid_pi_config_path_json_returns_error() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");

        let override_path = temp.path().join("override.json");
        write_file(&override_path, "not json");

        let result = Config::load_with_roots(Some(&override_path), &global_dir, &cwd);
        assert!(result.is_err());
    }

    #[test]
    fn load_with_missing_pi_config_path_file_falls_back_to_defaults() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");

        let missing_path = temp.path().join("missing.json");
        let config =
            Config::load_with_roots(Some(&missing_path), &global_dir, &cwd).expect("load config");
        assert!(config.theme.is_none());
        assert!(config.default_provider.is_none());
        assert!(config.default_model.is_none());
    }

    #[test]
    fn queue_mode_accessors_parse_values_and_aliases() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "steeringMode": "all", "followUpMode": "one-at-a-time" }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load config");
        assert_eq!(config.steering_queue_mode(), QueueMode::All);
        assert_eq!(config.follow_up_queue_mode(), QueueMode::OneAtATime);
    }

    #[test]
    fn queue_mode_accessors_default_on_unknown() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "steering_mode": "not-a-real-mode" }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load config");
        assert_eq!(config.steering_queue_mode(), QueueMode::OneAtATime);
        assert_eq!(config.follow_up_queue_mode(), QueueMode::OneAtATime);
    }

    // ── thinking_budget accessor ───────────────────────────────────────

    #[test]
    fn thinking_budget_returns_defaults_when_unset() {
        let config = Config::default();
        assert_eq!(config.thinking_budget("minimal"), 1024);
        assert_eq!(config.thinking_budget("low"), 2048);
        assert_eq!(config.thinking_budget("medium"), 8192);
        assert_eq!(config.thinking_budget("high"), 16384);
        assert_eq!(config.thinking_budget("xhigh"), 32768);
        assert_eq!(config.thinking_budget("max"), 65536);
        assert_eq!(config.thinking_budget("unknown-level"), 0);
    }

    #[test]
    fn thinking_budget_uses_custom_values() {
        let config = Config {
            thinking_budgets: Some(super::ThinkingBudgets {
                minimal: Some(100),
                low: Some(200),
                medium: Some(300),
                high: Some(400),
                xhigh: Some(500),
                max: Some(600),
            }),
            ..Config::default()
        };
        assert_eq!(config.thinking_budget("minimal"), 100);
        assert_eq!(config.thinking_budget("low"), 200);
        assert_eq!(config.thinking_budget("medium"), 300);
        assert_eq!(config.thinking_budget("high"), 400);
        assert_eq!(config.thinking_budget("xhigh"), 500);
        assert_eq!(config.thinking_budget("max"), 600);
    }

    // ── enable_skill_commands ──────────────────────────────────────────

    #[test]
    fn enable_skill_commands_defaults_to_true() {
        let config = Config::default();
        assert!(config.enable_skill_commands());
    }

    #[test]
    fn enable_skill_commands_can_be_disabled() {
        let config = Config {
            enable_skill_commands: Some(false),
            ..Config::default()
        };
        assert!(!config.enable_skill_commands());
    }

    // ── branch_summary_reserve_tokens ──────────────────────────────────

    #[test]
    fn branch_summary_reserve_tokens_falls_back_to_compaction() {
        let config = Config {
            compaction: Some(super::CompactionSettings {
                reserve_tokens: Some(9999),
                ..Default::default()
            }),
            ..Config::default()
        };
        assert_eq!(config.branch_summary_reserve_tokens(), 9999);
    }

    #[test]
    fn branch_summary_reserve_tokens_uses_own_value() {
        let config = Config {
            compaction: Some(super::CompactionSettings {
                reserve_tokens: Some(9999),
                ..Default::default()
            }),
            branch_summary: Some(super::BranchSummarySettings {
                reserve_tokens: Some(1111),
            }),
            ..Config::default()
        };
        assert_eq!(config.branch_summary_reserve_tokens(), 1111);
    }

    // ── deep_merge_settings_value ──────────────────────────────────────

    #[test]
    fn deep_merge_null_value_removes_key() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        let settings_path =
            Config::settings_path_with_roots(SettingsScope::Project, &global_dir, &cwd);

        write_file(
            &settings_path,
            r#"{ "theme": "dark", "default_provider": "anthropic" }"#,
        );

        Config::patch_settings_with_roots(
            SettingsScope::Project,
            &global_dir,
            &cwd,
            json!({ "theme": null }),
        )
        .expect("patch");

        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).expect("read"))
                .expect("parse");
        assert!(stored.get("theme").is_none());
        assert_eq!(stored["default_provider"], json!("anthropic"));
    }

    // ── parse_queue_mode ───────────────────────────────────────────────

    #[test]
    fn parse_queue_mode_parses_known_values() {
        assert_eq!(super::parse_queue_mode(Some("all")), Some(QueueMode::All));
        assert_eq!(
            super::parse_queue_mode(Some("one-at-a-time")),
            Some(QueueMode::OneAtATime)
        );
        assert_eq!(super::parse_queue_mode(Some("unknown")), None);
        assert_eq!(super::parse_queue_mode(None), None);
    }

    // ── PackageSource serde ────────────────────────────────────────────

    #[test]
    fn package_source_serde_string_variant() {
        let parsed: super::PackageSource =
            serde_json::from_value(json!("npm:my-ext@1.0")).expect("parse");
        assert!(matches!(parsed, super::PackageSource::String(s) if s == "npm:my-ext@1.0"));
    }

    #[test]
    fn package_source_serde_detailed_variant() {
        let parsed: super::PackageSource = serde_json::from_value(json!({
            "source": "git:org/repo",
            "local": true,
            "kind": "extension"
        }))
        .expect("parse");
        assert!(matches!(
            parsed,
            super::PackageSource::Detailed { source, local: Some(true), kind: Some(_) } if source == "git:org/repo"
        ));
    }

    // ── settings_path_with_roots ───────────────────────────────────────

    #[test]
    fn settings_path_global_and_project_differ() {
        let global_path = Config::settings_path_with_roots(
            SettingsScope::Global,
            std::path::Path::new("/global"),
            std::path::Path::new("/project"),
        );
        let project_path = Config::settings_path_with_roots(
            SettingsScope::Project,
            std::path::Path::new("/global"),
            std::path::Path::new("/project"),
        );
        assert_ne!(global_path, project_path);
        assert!(global_path.starts_with("/global"));
        assert!(project_path.starts_with("/project"));
    }

    // ── SettingsScope equality ──────────────────────────────────────────

    #[test]
    fn settings_scope_equality() {
        assert_eq!(SettingsScope::Global, SettingsScope::Global);
        assert_eq!(SettingsScope::Project, SettingsScope::Project);
        assert_ne!(SettingsScope::Global, SettingsScope::Project);
    }

    // ── camelCase alias fields ─────────────────────────────────────────

    #[test]
    fn camel_case_aliases_are_parsed() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{
                "hideThinkingBlock": true,
                "showHardwareCursor": true,
                "quietStartup": true,
                "collapseChangelog": true,
                "doubleEscapeAction": "quit",
                "editorPaddingX": 5,
                "autocompleteMaxVisible": 15,
                "sessionPickerInput": 2,
                "sessionDurability": "throughput"
            }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load config");
        assert_eq!(config.hide_thinking_block, Some(true));
        assert_eq!(config.show_hardware_cursor, Some(true));
        assert_eq!(config.quiet_startup, Some(true));
        assert_eq!(config.collapse_changelog, Some(true));
        assert_eq!(config.double_escape_action.as_deref(), Some("quit"));
        assert_eq!(config.editor_padding_x, Some(5));
        assert_eq!(config.autocomplete_max_visible, Some(15));
        assert_eq!(config.session_picker_input, Some(2));
        assert_eq!(config.session_durability.as_deref(), Some("throughput"));
    }

    #[test]
    fn camel_case_nested_aliases_are_parsed() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{
                "queueMode": "all",
                "compaction": { "enabled": false, "reserveTokens": 1234, "keepRecentTokens": 5678 },
                "branchSummary": { "reserveTokens": 2222 },
                "retry": { "enabled": false, "maxRetries": 9, "baseDelayMs": 101, "maxDelayMs": 202 },
                "images": { "autoResize": false, "blockImages": true },
                "terminal": { "showImages": false, "clearOnShrink": true }
            }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load config");
        assert_eq!(config.steering_mode.as_deref(), Some("all"));
        assert_eq!(config.steering_queue_mode(), QueueMode::All);
        assert!(!config.compaction_enabled());
        assert_eq!(config.compaction_reserve_tokens(), 1234);
        assert_eq!(config.compaction_keep_recent_tokens(), 5678);
        assert_eq!(config.branch_summary_reserve_tokens(), 2222);
        assert!(!config.retry_enabled());
        assert_eq!(config.retry_max_retries(), 9);
        assert_eq!(config.retry_base_delay_ms(), 101);
        assert_eq!(config.retry_max_delay_ms(), 202);
        assert!(!config.image_auto_resize());
        assert!(!config.terminal_show_images());
        assert!(config.terminal_clear_on_shrink());
    }

    #[test]
    fn terminal_clear_on_shrink_uses_env_when_unset() {
        let config = Config::default();
        assert!(config.terminal_clear_on_shrink_with_lookup(|name| {
            if name == "PI_CLEAR_ON_SHRINK" {
                Some("1".to_string())
            } else {
                None
            }
        }));
        assert!(!config.terminal_clear_on_shrink_with_lookup(|_| None));
    }

    #[test]
    fn terminal_clear_on_shrink_settings_take_precedence_over_env() {
        let config = Config {
            terminal: Some(TerminalSettings {
                clear_on_shrink: Some(false),
                ..TerminalSettings::default()
            }),
            ..Config::default()
        };
        assert!(!config.terminal_clear_on_shrink_with_lookup(|name| {
            if name == "PI_CLEAR_ON_SHRINK" {
                Some("1".to_string())
            } else {
                None
            }
        }));
    }

    // ── Config serde roundtrip ─────────────────────────────────────────

    #[test]
    fn config_serde_roundtrip() {
        let config = Config {
            theme: Some("dark".to_string()),
            default_provider: Some("anthropic".to_string()),
            compaction: Some(super::CompactionSettings {
                enabled: Some(true),
                reserve_tokens: Some(1000),
                keep_recent_tokens: Some(2000),
            }),
            ..Config::default()
        };
        let json = serde_json::to_string(&config).expect("serialize");
        let deserialized: Config = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.theme.as_deref(), Some("dark"));
        assert_eq!(deserialized.default_provider.as_deref(), Some("anthropic"));
        assert!(deserialized.compaction_enabled());
    }

    // ── merge thinking budgets ─────────────────────────────────────────

    #[test]
    fn load_handles_empty_file_as_default() {
        let temp = TempDir::new().expect("create tempdir");
        let path = temp.path().join("empty.json");
        write_file(&path, "");

        let config = Config::load_from_path(&path).expect("load config");
        // Should return default config, not error
        assert!(config.theme.is_none());
    }

    #[test]
    fn merge_thinking_budgets_combines_values() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "thinking_budgets": { "minimal": 100, "low": 200 } }"#,
        );
        write_file(
            &cwd.join(".pi/settings.json"),
            r#"{ "thinking_budgets": { "minimal": 999 } }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load");
        assert_eq!(config.thinking_budget("minimal"), 999);
        assert_eq!(config.thinking_budget("low"), 200);
    }

    #[test]
    fn merge_extension_risk_combines_global_and_project_values() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{
                "extensionRisk": {
                    "enabled": true,
                    "alpha": 0.2,
                    "windowSize": 128,
                    "ledgerLimit": 500,
                    "decisionTimeoutMs": 100,
                    "failClosed": false
                }
            }"#,
        );
        write_file(
            &cwd.join(".pi/settings.json"),
            r#"{
                "extensionRisk": {
                    "alpha": 0.05,
                    "windowSize": 256,
                    "failClosed": true
                }
            }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load");
        let risk = config.extension_risk.expect("merged extension risk");
        assert_eq!(risk.enabled, Some(true));
        assert_eq!(risk.alpha, Some(0.05));
        assert_eq!(risk.window_size, Some(256));
        assert_eq!(risk.ledger_limit, Some(500));
        assert_eq!(risk.decision_timeout_ms, Some(100));
        assert_eq!(risk.fail_closed, Some(true));
    }

    #[test]
    fn merge_extension_risk_empty_project_object_keeps_global_values() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{
                "extensionRisk": {
                    "enabled": true,
                    "alpha": 0.1,
                    "windowSize": 64,
                    "ledgerLimit": 200,
                    "decisionTimeoutMs": 75,
                    "failClosed": true
                }
            }"#,
        );
        write_file(&cwd.join(".pi/settings.json"), r#"{ "extensionRisk": {} }"#);

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load");
        let risk = config.extension_risk.expect("merged extension risk");
        assert_eq!(risk.enabled, Some(true));
        assert_eq!(risk.alpha, Some(0.1));
        assert_eq!(risk.window_size, Some(64));
        assert_eq!(risk.ledger_limit, Some(200));
        assert_eq!(risk.decision_timeout_ms, Some(75));
        assert_eq!(risk.fail_closed, Some(true));
    }

    #[test]
    fn extension_risk_defaults_fail_closed() {
        let config = Config::default();
        let resolved = config.resolve_extension_risk_with_metadata();
        assert_eq!(resolved.source, "default");
        assert!(resolved.settings.fail_closed);
    }

    #[test]
    fn extension_risk_config_can_disable_fail_closed_explicitly() {
        let config = Config {
            extension_risk: Some(ExtensionRiskConfig {
                enabled: Some(true),
                fail_closed: Some(false),
                ..ExtensionRiskConfig::default()
            }),
            ..Config::default()
        };
        let resolved = config.resolve_extension_risk_with_metadata();
        assert_eq!(resolved.source, "config");
        assert!(!resolved.settings.fail_closed);
    }

    // ====================================================================
    // Extension Policy Config
    // ====================================================================

    #[test]
    fn extension_policy_defaults_to_permissive_behavior() {
        let config = Config::default();
        let policy = config.resolve_extension_policy(None);
        assert_eq!(
            policy.mode,
            crate::extensions::ExtensionPolicyMode::Permissive
        );
        assert!(policy.deny_caps.is_empty());
    }

    #[test]
    fn extension_policy_metadata_reports_cli_source() {
        let config = Config::default();
        let resolved = config.resolve_extension_policy_with_metadata(Some("safe"));
        assert_eq!(resolved.profile_source, "cli");
        assert_eq!(resolved.requested_profile, "safe");
        assert_eq!(resolved.effective_profile, "safe");
        assert_eq!(
            resolved.policy.mode,
            crate::extensions::ExtensionPolicyMode::Strict
        );
    }

    #[test]
    fn extension_policy_metadata_unknown_profile_falls_back_to_safe() {
        let config = Config::default();
        let resolved = config.resolve_extension_policy_with_metadata(Some("unknown-value"));
        assert_eq!(resolved.requested_profile, "unknown-value");
        assert_eq!(resolved.effective_profile, "safe");
        assert_eq!(
            resolved.policy.mode,
            crate::extensions::ExtensionPolicyMode::Strict
        );
    }

    #[test]
    fn extension_policy_metadata_balanced_profile_maps_to_prompt_mode() {
        let config = Config::default();
        let resolved = config.resolve_extension_policy_with_metadata(Some("balanced"));
        assert_eq!(resolved.requested_profile, "balanced");
        assert_eq!(resolved.effective_profile, "balanced");
        assert_eq!(
            resolved.policy.mode,
            crate::extensions::ExtensionPolicyMode::Prompt
        );
    }

    #[test]
    fn extension_policy_metadata_legacy_standard_alias_maps_to_balanced() {
        let config = Config::default();
        let resolved = config.resolve_extension_policy_with_metadata(Some("standard"));
        assert_eq!(resolved.requested_profile, "standard");
        assert_eq!(resolved.effective_profile, "balanced");
        assert_eq!(
            resolved.policy.mode,
            crate::extensions::ExtensionPolicyMode::Prompt
        );
    }

    #[test]
    fn extension_policy_default_permissive_toggle_false_restores_safe_behavior() {
        let config = Config {
            extension_policy: Some(ExtensionPolicyConfig {
                profile: None,
                default_permissive: Some(false),
                allow_dangerous: None,
            }),
            ..Default::default()
        };
        let resolved = config.resolve_extension_policy_with_metadata(None);
        assert_eq!(resolved.profile_source, "config");
        assert_eq!(resolved.requested_profile, "safe");
        assert_eq!(resolved.effective_profile, "safe");
        assert_eq!(
            resolved.policy.mode,
            crate::extensions::ExtensionPolicyMode::Strict
        );
    }

    #[test]
    fn extension_policy_cli_override_safe() {
        let config = Config::default();
        let policy = config.resolve_extension_policy(Some("safe"));
        assert_eq!(policy.mode, crate::extensions::ExtensionPolicyMode::Strict);
        assert!(policy.deny_caps.contains(&"exec".to_string()));
    }

    #[test]
    fn extension_policy_cli_override_permissive() {
        let config = Config::default();
        let policy = config.resolve_extension_policy(Some("permissive"));
        assert_eq!(
            policy.mode,
            crate::extensions::ExtensionPolicyMode::Permissive
        );
    }

    #[test]
    fn extension_policy_from_settings_json() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "extensionPolicy": { "profile": "safe" } }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load");
        let policy = config.resolve_extension_policy(None);
        assert_eq!(policy.mode, crate::extensions::ExtensionPolicyMode::Strict);
    }

    #[test]
    fn extension_policy_cli_overrides_config() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "extensionPolicy": { "profile": "safe" } }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load");
        // CLI says permissive, config says safe → CLI wins
        let policy = config.resolve_extension_policy(Some("permissive"));
        assert_eq!(
            policy.mode,
            crate::extensions::ExtensionPolicyMode::Permissive
        );
    }

    #[test]
    fn extension_policy_allow_dangerous_removes_deny() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "extensionPolicy": { "defaultPermissive": false, "allowDangerous": true } }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load");
        let policy = config.resolve_extension_policy(None);
        // Safe fallback still drops explicit deny-caps when allowDangerous=true.
        assert!(!policy.deny_caps.contains(&"exec".to_string()));
        assert!(!policy.deny_caps.contains(&"env".to_string()));
    }

    #[test]
    fn extension_policy_project_overrides_global() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "extensionPolicy": { "profile": "safe" } }"#,
        );
        write_file(
            &cwd.join(".pi/settings.json"),
            r#"{ "extensionPolicy": { "profile": "permissive" } }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load");
        let policy = config.resolve_extension_policy(None);
        assert_eq!(
            policy.mode,
            crate::extensions::ExtensionPolicyMode::Permissive
        );
    }

    #[test]
    fn extension_policy_unknown_profile_defaults_to_safe() {
        let config = Config::default();
        let policy = config.resolve_extension_policy(Some("unknown-value"));
        assert_eq!(policy.mode, crate::extensions::ExtensionPolicyMode::Strict);
    }

    #[test]
    fn extension_policy_deserializes_camel_case() {
        let json = r#"{ "extensionPolicy": { "profile": "safe", "defaultPermissive": false, "allowDangerous": false } }"#;
        let config: Config = serde_json::from_str(json).expect("parse");
        assert_eq!(
            config.extension_policy.as_ref().unwrap().profile.as_deref(),
            Some("safe")
        );
        assert_eq!(
            config.extension_policy.as_ref().unwrap().default_permissive,
            Some(false)
        );
        assert_eq!(
            config.extension_policy.as_ref().unwrap().allow_dangerous,
            Some(false)
        );
    }

    #[test]
    fn extension_policy_merge_project_overrides_global_partial() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        // Global sets profile=safe
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "extensionPolicy": { "profile": "safe" } }"#,
        );
        // Project sets allowDangerous=true but not profile
        write_file(
            &cwd.join(".pi/settings.json"),
            r#"{ "extensionPolicy": { "allowDangerous": true } }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load");
        // Profile from global, allowDangerous from project
        let ext_config = config.extension_policy.as_ref().unwrap();
        assert_eq!(ext_config.profile.as_deref(), Some("safe"));
        assert_eq!(ext_config.allow_dangerous, Some(true));
    }

    // ====================================================================
    // SEC-4.4: Dangerous opt-in audit and profile transition tests
    // ====================================================================

    #[test]
    fn dangerous_opt_in_audit_present_when_allow_dangerous() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "extensionPolicy": { "profile": "safe", "allowDangerous": true } }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load");
        let resolved = config.resolve_extension_policy_with_metadata(None);
        assert!(resolved.allow_dangerous);
        let audit = resolved
            .dangerous_opt_in_audit
            .expect("audit entry must be present");
        assert_eq!(audit.source, "config");
        assert_eq!(audit.profile, "safe");
        assert!(audit.capabilities_unblocked.contains(&"exec".to_string()));
        assert!(audit.capabilities_unblocked.contains(&"env".to_string()));
    }

    #[test]
    fn dangerous_opt_in_audit_absent_when_not_opted_in() {
        let config = Config::default();
        let resolved = config.resolve_extension_policy_with_metadata(None);
        assert!(!resolved.allow_dangerous);
        assert!(resolved.dangerous_opt_in_audit.is_none());
    }

    #[test]
    fn dangerous_opt_in_audit_empty_unblocked_when_permissive() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "extensionPolicy": { "profile": "permissive", "allowDangerous": true } }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load");
        let resolved = config.resolve_extension_policy_with_metadata(None);
        let audit = resolved
            .dangerous_opt_in_audit
            .expect("audit entry must be present");
        assert!(
            audit.capabilities_unblocked.is_empty(),
            "permissive has no deny_caps to remove"
        );
    }

    #[test]
    fn profile_downgrade_safe_roundtrip_verifiable() {
        let config = Config::default();
        let permissive = config.resolve_extension_policy(Some("permissive"));
        let safe = config.resolve_extension_policy(Some("safe"));

        assert_eq!(
            permissive.evaluate("exec").decision,
            crate::extensions::PolicyDecision::Allow
        );
        assert_eq!(
            safe.evaluate("exec").decision,
            crate::extensions::PolicyDecision::Deny
        );

        let check = crate::extensions::ExtensionPolicy::is_valid_downgrade(&permissive, &safe);
        assert!(check.is_valid_downgrade);
    }

    #[test]
    fn profile_upgrade_safe_to_permissive_not_downgrade() {
        let config = Config::default();
        let safe = config.resolve_extension_policy(Some("safe"));
        let permissive = config.resolve_extension_policy(Some("permissive"));

        let check = crate::extensions::ExtensionPolicy::is_valid_downgrade(&safe, &permissive);
        assert!(!check.is_valid_downgrade);
    }

    #[test]
    fn profile_metadata_includes_audit_for_balanced_allow_dangerous() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "extensionPolicy": { "profile": "balanced", "allowDangerous": true } }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load");
        let resolved = config.resolve_extension_policy_with_metadata(None);
        assert_eq!(resolved.effective_profile, "balanced");
        assert!(resolved.allow_dangerous);
        let audit = resolved.dangerous_opt_in_audit.unwrap();
        assert_eq!(audit.source, "config");
        assert_eq!(audit.profile, "balanced");
        assert!(audit.capabilities_unblocked.contains(&"exec".to_string()));
    }

    #[test]
    fn explain_policy_runtime_callable_from_config() {
        let config = Config::default();
        let policy = config.resolve_extension_policy(Some("safe"));
        let explanation = policy.explain_effective_policy(None);
        assert_eq!(
            explanation.mode,
            crate::extensions::ExtensionPolicyMode::Strict
        );
        assert!(!explanation.dangerous_denied.is_empty());
        assert!(explanation.dangerous_allowed.is_empty());
    }

    // ====================================================================
    // Repair Policy Config
    // ====================================================================

    #[test]
    fn repair_policy_defaults_to_suggest() {
        let config = Config::default();
        let policy = config.resolve_repair_policy(None);
        assert_eq!(policy, crate::extensions::RepairPolicyMode::Suggest);
    }

    #[test]
    fn repair_policy_metadata_reports_cli_source() {
        let config = Config::default();
        let resolved = config.resolve_repair_policy_with_metadata(Some("off"));
        assert_eq!(resolved.source, "cli");
        assert_eq!(resolved.requested_mode, "off");
        assert_eq!(
            resolved.effective_mode,
            crate::extensions::RepairPolicyMode::Off
        );
    }

    #[test]
    fn repair_policy_metadata_unknown_mode_defaults_to_suggest() {
        let config = Config::default();
        let resolved = config.resolve_repair_policy_with_metadata(Some("unknown"));
        assert_eq!(resolved.requested_mode, "unknown");
        assert_eq!(
            resolved.effective_mode,
            crate::extensions::RepairPolicyMode::Suggest
        );
    }

    #[test]
    fn repair_policy_from_settings_json() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "repairPolicy": { "mode": "auto-safe" } }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load");
        let policy = config.resolve_repair_policy(None);
        assert_eq!(policy, crate::extensions::RepairPolicyMode::AutoSafe);
    }

    #[test]
    fn repair_policy_cli_overrides_config() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "repairPolicy": { "mode": "off" } }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load");
        let policy = config.resolve_repair_policy(Some("auto-strict"));
        assert_eq!(policy, crate::extensions::RepairPolicyMode::AutoStrict);
    }

    #[test]
    fn repair_policy_project_overrides_global() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        write_file(
            &global_dir.join("settings.json"),
            r#"{ "repairPolicy": { "mode": "off" } }"#,
        );
        write_file(
            &cwd.join(".pi/settings.json"),
            r#"{ "repairPolicy": { "mode": "auto-safe" } }"#,
        );

        let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load");
        let policy = config.resolve_repair_policy(None);
        assert_eq!(policy, crate::extensions::RepairPolicyMode::AutoSafe);
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 128, .. ProptestConfig::default() })]

        #[test]
        fn proptest_config_merge_prefers_other_for_scalar_fields(
            base_theme in prop::option::of(string_regex("[A-Za-z0-9_-]{1,16}").unwrap()),
            other_theme in prop::option::of(string_regex("[A-Za-z0-9_-]{1,16}").unwrap()),
            base_provider in prop::option::of(string_regex("[A-Za-z0-9_-]{1,16}").unwrap()),
            other_provider in prop::option::of(string_regex("[A-Za-z0-9_-]{1,16}").unwrap()),
            base_hide_thinking in prop::option::of(any::<bool>()),
            other_hide_thinking in prop::option::of(any::<bool>()),
            base_autocomplete in prop::option::of(0u16..512u16),
            other_autocomplete in prop::option::of(0u16..512u16),
        ) {
            let base = Config {
                theme: base_theme.clone(),
                default_provider: base_provider.clone(),
                hide_thinking_block: base_hide_thinking,
                autocomplete_max_visible: base_autocomplete.map(u32::from),
                ..Config::default()
            };
            let other = Config {
                theme: other_theme.clone(),
                default_provider: other_provider.clone(),
                hide_thinking_block: other_hide_thinking,
                autocomplete_max_visible: other_autocomplete.map(u32::from),
                ..Config::default()
            };

            let merged = Config::merge(base, other);
            prop_assert_eq!(merged.theme, other_theme.or(base_theme));
            prop_assert_eq!(merged.default_provider, other_provider.or(base_provider));
            prop_assert_eq!(
                merged.hide_thinking_block,
                other_hide_thinking.or(base_hide_thinking)
            );
            prop_assert_eq!(
                merged.autocomplete_max_visible,
                other_autocomplete
                    .map(u32::from)
                    .or_else(|| base_autocomplete.map(u32::from))
            );
        }

        #[test]
        fn proptest_merge_extension_risk_prefers_other_fields_when_present(
            base_present in any::<bool>(),
            other_present in any::<bool>(),
            base_enabled in prop::option::of(any::<bool>()),
            other_enabled in prop::option::of(any::<bool>()),
            base_alpha in prop::option::of(-1.0e6f64..1.0e6f64),
            other_alpha in prop::option::of(-1.0e6f64..1.0e6f64),
            base_window in prop::option::of(1u16..1024u16),
            other_window in prop::option::of(1u16..1024u16),
            base_ledger_limit in prop::option::of(1u16..2048u16),
            other_ledger_limit in prop::option::of(1u16..2048u16),
            base_timeout_ms in prop::option::of(1u16..5000u16),
            other_timeout_ms in prop::option::of(1u16..5000u16),
            base_fail_closed in prop::option::of(any::<bool>()),
            other_fail_closed in prop::option::of(any::<bool>()),
            base_enforce in prop::option::of(any::<bool>()),
            other_enforce in prop::option::of(any::<bool>()),
        ) {
            let base = base_present.then_some(ExtensionRiskConfig {
                enabled: base_enabled,
                alpha: base_alpha,
                window_size: base_window.map(u32::from),
                ledger_limit: base_ledger_limit.map(u32::from),
                decision_timeout_ms: base_timeout_ms.map(u64::from),
                fail_closed: base_fail_closed,
                enforce: base_enforce,
            });
            let other = other_present.then_some(ExtensionRiskConfig {
                enabled: other_enabled,
                alpha: other_alpha,
                window_size: other_window.map(u32::from),
                ledger_limit: other_ledger_limit.map(u32::from),
                decision_timeout_ms: other_timeout_ms.map(u64::from),
                fail_closed: other_fail_closed,
                enforce: other_enforce,
            });

            let merged = super::merge_extension_risk(base.clone(), other.clone());
            match (base, other, merged) {
                (None, None, None) => {}
                (Some(base), None, Some(merged)) => {
                    prop_assert_eq!(merged.enabled, base.enabled);
                    prop_assert_eq!(merged.alpha, base.alpha);
                    prop_assert_eq!(merged.window_size, base.window_size);
                    prop_assert_eq!(merged.ledger_limit, base.ledger_limit);
                    prop_assert_eq!(merged.decision_timeout_ms, base.decision_timeout_ms);
                    prop_assert_eq!(merged.fail_closed, base.fail_closed);
                    prop_assert_eq!(merged.enforce, base.enforce);
                }
                (None, Some(other), Some(merged)) => {
                    prop_assert_eq!(merged.enabled, other.enabled);
                    prop_assert_eq!(merged.alpha, other.alpha);
                    prop_assert_eq!(merged.window_size, other.window_size);
                    prop_assert_eq!(merged.ledger_limit, other.ledger_limit);
                    prop_assert_eq!(merged.decision_timeout_ms, other.decision_timeout_ms);
                    prop_assert_eq!(merged.fail_closed, other.fail_closed);
                    prop_assert_eq!(merged.enforce, other.enforce);
                }
                (Some(base), Some(other), Some(merged)) => {
                    prop_assert_eq!(merged.enabled, other.enabled.or(base.enabled));
                    prop_assert_eq!(merged.alpha, other.alpha.or(base.alpha));
                    prop_assert_eq!(merged.window_size, other.window_size.or(base.window_size));
                    prop_assert_eq!(merged.ledger_limit, other.ledger_limit.or(base.ledger_limit));
                    prop_assert_eq!(
                        merged.decision_timeout_ms,
                        other.decision_timeout_ms.or(base.decision_timeout_ms)
                    );
                    prop_assert_eq!(merged.fail_closed, other.fail_closed.or(base.fail_closed));
                    prop_assert_eq!(merged.enforce, other.enforce.or(base.enforce));
                }
                _ => assert!(false, "merge_extension_risk must preserve Option-shape semantics"),
            }
        }

        #[test]
        fn proptest_deep_merge_settings_value_scalar_and_null_patch_semantics(
            base_entries in prop::collection::hash_map(
                string_regex("[a-z][a-z0-9_]{0,10}").unwrap(),
                any::<i64>(),
                0..16
            ),
            patch_entries in prop::collection::hash_map(
                string_regex("[a-z][a-z0-9_]{0,10}").unwrap(),
                prop::option::of(any::<i64>()),
                0..16
            ),
        ) {
            let mut dst = Value::Object(
                base_entries
                    .iter()
                    .map(|(key, value)| (key.clone(), json!(*value)))
                    .collect(),
            );
            let patch = Value::Object(
                patch_entries
                    .iter()
                    .map(|(key, value)| {
                        (
                            key.clone(),
                            value.map_or(Value::Null, |number| json!(number)),
                        )
                    })
                    .collect(),
            );

            super::deep_merge_settings_value(&mut dst, patch).expect("merge should succeed");
            let dst_obj = dst.as_object().expect("merged value should stay an object");

            let mut expected = base_entries;
            for (key, value) in &patch_entries {
                match value {
                    Some(number) => {
                        expected.insert(key.clone(), *number);
                    }
                    None => {
                        expected.remove(key);
                    }
                }
            }

            prop_assert_eq!(dst_obj.len(), expected.len());
            for (key, expected_value) in expected {
                prop_assert_eq!(dst_obj.get(&key), Some(&json!(expected_value)));
            }
        }

        #[test]
        fn proptest_deep_merge_settings_value_nested_object_patch_semantics(
            base_nested in prop::collection::hash_map(
                string_regex("[a-z][a-z0-9_]{0,10}").unwrap(),
                any::<i64>(),
                0..12
            ),
            patch_nested in prop::collection::hash_map(
                string_regex("[a-z][a-z0-9_]{0,10}").unwrap(),
                prop::option::of(any::<i64>()),
                0..12
            ),
            preserve_value in any::<i64>(),
        ) {
            let mut dst = json!({
                "nested": Value::Object(
                    base_nested
                        .iter()
                        .map(|(key, value)| (key.clone(), json!(*value)))
                        .collect()
                ),
                "preserve": preserve_value
            });

            let patch = json!({
                "nested": Value::Object(
                    patch_nested
                        .iter()
                        .map(|(key, value)| {
                            (
                                key.clone(),
                                value.map_or(Value::Null, |number| json!(number)),
                            )
                        })
                        .collect()
                )
            });

            super::deep_merge_settings_value(&mut dst, patch).expect("nested merge should succeed");

            let mut expected_nested = base_nested;
            for (key, value) in &patch_nested {
                match value {
                    Some(number) => {
                        expected_nested.insert(key.clone(), *number);
                    }
                    None => {
                        expected_nested.remove(key);
                    }
                }
            }

            let nested = dst
                .get("nested")
                .and_then(Value::as_object)
                .expect("nested key should stay an object");
            prop_assert_eq!(nested.len(), expected_nested.len());
            for (key, expected_value) in expected_nested {
                prop_assert_eq!(nested.get(&key), Some(&json!(expected_value)));
            }
            prop_assert_eq!(dst.get("preserve"), Some(&json!(preserve_value)));
        }

        #[test]
        fn proptest_deep_merge_settings_value_rejects_non_object_patch(
            patch in prop_oneof![
                any::<bool>().prop_map(Value::Bool),
                any::<i64>().prop_map(Value::from),
                Just(Value::Null),
                prop::collection::vec(any::<i64>(), 0..8).prop_map(|values| json!(values)),
            ],
        ) {
            let mut dst = json!({});
            let err = super::deep_merge_settings_value(&mut dst, patch)
                .expect_err("non-object patch must fail closed");
            prop_assert!(
                err.to_string().contains("Settings patch must be a JSON object"),
                "unexpected error: {err}"
            );
        }

        #[test]
        fn proptest_extension_risk_alpha_finite_values_clamp(alpha in -1.0e6f64..1.0e6f64) {
            let config = Config {
                extension_risk: Some(ExtensionRiskConfig {
                    alpha: Some(alpha),
                    ..ExtensionRiskConfig::default()
                }),
                ..Config::default()
            };

            let resolved = config.resolve_extension_risk_with_metadata();
            let env_alpha = std::env::var("PI_EXTENSION_RISK_ALPHA")
                .ok()
                .and_then(|raw| raw.trim().parse::<f64>().ok())
                .and_then(|parsed| parsed.is_finite().then_some(parsed.clamp(1.0e-6, 0.5)));

            // Only PI_EXTENSION_RISK_ALPHA should override config alpha.
            let expected_alpha = env_alpha.unwrap_or_else(|| alpha.clamp(1.0e-6, 0.5));
            prop_assert!((resolved.settings.alpha - expected_alpha).abs() <= f64::EPSILON);
            if env_alpha.is_some() {
                prop_assert_eq!(resolved.source, "env");
            }
        }

        #[test]
        fn proptest_config_deserializes_extension_risk_alpha_values(alpha in -1.0e6f64..1.0e6f64) {
            let parsed: Config = serde_json::from_value(json!({
                "extensionRisk": {
                    "alpha": alpha
                }
            }))
            .expect("config with finite alpha should deserialize");

            prop_assert_eq!(
                parsed.extension_risk.as_ref().and_then(|risk| risk.alpha),
                Some(alpha)
            );
        }

        #[test]
        fn proptest_extension_risk_alpha_non_finite_values_are_ignored(
            alpha in prop_oneof![Just(f64::NAN), Just(f64::INFINITY), Just(f64::NEG_INFINITY)]
        ) {
            let config = Config {
                extension_risk: Some(ExtensionRiskConfig {
                    alpha: Some(alpha),
                    ..ExtensionRiskConfig::default()
                }),
                ..Config::default()
            };

            let baseline = Config::default().resolve_extension_risk_with_metadata();
            let resolved = config.resolve_extension_risk_with_metadata();
            // Non-finite config alpha must be ignored, so result should match
            // baseline resolution under the same environment.
            prop_assert!((resolved.settings.alpha - baseline.settings.alpha).abs() <= f64::EPSILON);
            prop_assert_eq!(resolved.source, baseline.source);
        }

        #[test]
        fn proptest_parse_queue_mode_unknown_values_return_none(raw in string_regex("[A-Za-z0-9_-]{1,24}").unwrap()) {
            let lowered = raw.to_ascii_lowercase();
            prop_assume!(lowered != "all" && lowered != "one-at-a-time");
            prop_assert_eq!(super::parse_queue_mode(Some(&raw)), None);
        }

        #[test]
        fn proptest_extension_policy_unknown_profile_fails_closed(raw in string_regex("[A-Za-z0-9_-]{1,24}").unwrap()) {
            let lowered = raw.to_ascii_lowercase();
            prop_assume!(
                lowered != "safe"
                    && lowered != "balanced"
                    && lowered != "standard"
                    && lowered != "permissive"
            );

            let config: Config = serde_json::from_value(json!({
                "extensionPolicy": {
                    "profile": raw
                }
            }))
            .expect("config should deserialize");
            // Use CLI override so test remains deterministic even when env
            // policy variables are present in the runner.
            let resolved = config.resolve_extension_policy_with_metadata(Some(&raw));
            prop_assert_eq!(resolved.effective_profile, "safe");
            prop_assert_eq!(
                resolved.policy.mode,
                crate::extensions::ExtensionPolicyMode::Strict
            );
        }
    }

    // ── markdown.codeBlockIndent config ───────────────────────────────

    #[test]
    fn markdown_code_block_indent_deserializes() {
        let json = r#"{"markdown":{"codeBlockIndent":4}}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.markdown.as_ref().unwrap().code_block_indent, Some(4));
    }

    #[test]
    fn markdown_code_block_indent_accepts_legacy_string() {
        let json = r#"{"markdown":{"codeBlockIndent":"    "}}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.markdown.as_ref().unwrap().code_block_indent, Some(4));
    }

    #[test]
    fn markdown_code_block_indent_snake_case_alias() {
        let json = r#"{"markdown":{"code_block_indent":6}}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.markdown.as_ref().unwrap().code_block_indent, Some(6));
    }

    #[test]
    fn markdown_code_block_indent_absent() {
        let json = r"{}";
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.markdown.is_none());
        assert_eq!(config.markdown_code_block_indent(), 2);
    }

    #[test]
    fn markdown_code_block_indent_zero() {
        let json = r#"{"markdown":{"codeBlockIndent":0}}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.markdown.as_ref().unwrap().code_block_indent, Some(0));
    }

    #[test]
    fn markdown_merge_prefers_other() {
        let base: Config = serde_json::from_str(r#"{"markdown":{"codeBlockIndent":2}}"#).unwrap();
        let other: Config = serde_json::from_str(r#"{"markdown":{"codeBlockIndent":4}}"#).unwrap();
        let merged = Config::merge(base, other);
        assert_eq!(merged.markdown.as_ref().unwrap().code_block_indent, Some(4));
    }

    // ── check_for_updates config ──────────────────────────────────────

    #[test]
    fn check_for_updates_default_is_true() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(config.should_check_for_updates());
    }

    #[test]
    fn check_for_updates_explicit_false() {
        let json = r#"{"checkForUpdates": false}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(!config.should_check_for_updates());
    }

    #[test]
    fn check_for_updates_explicit_true() {
        let json = r#"{"check_for_updates": true}"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.should_check_for_updates());
    }

    // ── merge function property tests ──────────────────────────────────

    mod merge_proptests {
        use super::*;

        // All merge functions share the same pattern:
        //   (None, None)    → None
        //   (Some, None)    → Some(base)
        //   (None, Some)    → Some(other)
        //   (Some, Some)    → Some(field-by-field other.or(base))

        proptest! {
            // ================================================================
            // merge_compaction
            // ================================================================

            #[test]
            fn compaction_none_none_is_none(() in Just(())) {
                assert!(merge_compaction(None, None).is_none());
            }

            #[test]
            fn compaction_right_identity(
                enabled in prop::option::of(any::<bool>()),
                reserve in prop::option::of(1u32..100_000),
                keep in prop::option::of(1u32..100_000),
            ) {
                let base = CompactionSettings { enabled, reserve_tokens: reserve, keep_recent_tokens: keep };
                let result = merge_compaction(Some(base.clone()), None).unwrap();
                assert_eq!(result.enabled, base.enabled);
                assert_eq!(result.reserve_tokens, base.reserve_tokens);
                assert_eq!(result.keep_recent_tokens, base.keep_recent_tokens);
            }

            #[test]
            fn compaction_left_identity(
                enabled in prop::option::of(any::<bool>()),
                reserve in prop::option::of(1u32..100_000),
                keep in prop::option::of(1u32..100_000),
            ) {
                let other = CompactionSettings { enabled, reserve_tokens: reserve, keep_recent_tokens: keep };
                let result = merge_compaction(None, Some(other.clone())).unwrap();
                assert_eq!(result.enabled, other.enabled);
                assert_eq!(result.reserve_tokens, other.reserve_tokens);
                assert_eq!(result.keep_recent_tokens, other.keep_recent_tokens);
            }

            #[test]
            fn compaction_other_overrides_base(
                b_en in prop::option::of(any::<bool>()),
                b_res in prop::option::of(1u32..100_000),
                o_en in prop::option::of(any::<bool>()),
                o_res in prop::option::of(1u32..100_000),
            ) {
                let base = CompactionSettings { enabled: b_en, reserve_tokens: b_res, keep_recent_tokens: None };
                let other = CompactionSettings { enabled: o_en, reserve_tokens: o_res, keep_recent_tokens: None };
                let result = merge_compaction(Some(base), Some(other)).unwrap();
                assert_eq!(result.enabled, o_en.or(b_en));
                assert_eq!(result.reserve_tokens, o_res.or(b_res));
            }

            // ================================================================
            // merge_branch_summary
            // ================================================================

            #[test]
            fn branch_summary_none_none_is_none(() in Just(())) {
                assert!(merge_branch_summary(None, None).is_none());
            }

            #[test]
            fn branch_summary_other_overrides(
                b_res in prop::option::of(1u32..100_000),
                o_res in prop::option::of(1u32..100_000),
            ) {
                let base = BranchSummarySettings { reserve_tokens: b_res };
                let other = BranchSummarySettings { reserve_tokens: o_res };
                let result = merge_branch_summary(Some(base), Some(other)).unwrap();
                assert_eq!(result.reserve_tokens, o_res.or(b_res));
            }

            // ================================================================
            // merge_retry
            // ================================================================

            #[test]
            fn retry_none_none_is_none(() in Just(())) {
                assert!(merge_retry(None, None).is_none());
            }

            #[test]
            fn retry_other_overrides(
                b_en in prop::option::of(any::<bool>()),
                b_max in prop::option::of(1u32..10),
                o_en in prop::option::of(any::<bool>()),
                o_base_delay in prop::option::of(100u32..5000),
            ) {
                let base = RetrySettings { enabled: b_en, max_retries: b_max, base_delay_ms: None, max_delay_ms: None };
                let other = RetrySettings { enabled: o_en, max_retries: None, base_delay_ms: o_base_delay, max_delay_ms: None };
                let result = merge_retry(Some(base), Some(other)).unwrap();
                assert_eq!(result.enabled, o_en.or(b_en));
                assert_eq!(result.max_retries, b_max); // other had None, base passes through
                assert_eq!(result.base_delay_ms, o_base_delay); // other had Some, overrides
            }

            // ================================================================
            // merge_images
            // ================================================================

            #[test]
            fn images_none_none_is_none(() in Just(())) {
                assert!(merge_images(None, None).is_none());
            }

            #[test]
            fn images_other_overrides(
                b_resize in prop::option::of(any::<bool>()),
                b_block in prop::option::of(any::<bool>()),
                o_resize in prop::option::of(any::<bool>()),
                o_block in prop::option::of(any::<bool>()),
            ) {
                let base = ImageSettings { auto_resize: b_resize, block_images: b_block };
                let other = ImageSettings { auto_resize: o_resize, block_images: o_block };
                let result = merge_images(Some(base), Some(other)).unwrap();
                assert_eq!(result.auto_resize, o_resize.or(b_resize));
                assert_eq!(result.block_images, o_block.or(b_block));
            }

            // ================================================================
            // merge_terminal
            // ================================================================

            #[test]
            fn terminal_none_none_is_none(() in Just(())) {
                assert!(merge_terminal(None, None).is_none());
            }

            #[test]
            fn terminal_other_overrides(
                b_show in prop::option::of(any::<bool>()),
                b_clear in prop::option::of(any::<bool>()),
                o_show in prop::option::of(any::<bool>()),
                o_clear in prop::option::of(any::<bool>()),
            ) {
                let base = TerminalSettings { show_images: b_show, clear_on_shrink: b_clear };
                let other = TerminalSettings { show_images: o_show, clear_on_shrink: o_clear };
                let result = merge_terminal(Some(base), Some(other)).unwrap();
                assert_eq!(result.show_images, o_show.or(b_show));
                assert_eq!(result.clear_on_shrink, o_clear.or(b_clear));
            }

            // ================================================================
            // merge_thinking_budgets
            // ================================================================

            #[test]
            fn thinking_budgets_none_none_is_none(() in Just(())) {
                assert!(merge_thinking_budgets(None, None).is_none());
            }

            #[test]
            fn thinking_budgets_other_overrides(
                b_min in prop::option::of(1u32..65536),
                b_low in prop::option::of(1u32..65536),
                o_med in prop::option::of(1u32..65536),
                o_high in prop::option::of(1u32..65536),
            ) {
                let base = ThinkingBudgets { minimal: b_min, low: b_low, medium: None, high: None, xhigh: None, max: None };
                let other = ThinkingBudgets { minimal: None, low: None, medium: o_med, high: o_high, xhigh: None, max: None };
                let result = merge_thinking_budgets(Some(base), Some(other)).unwrap();
                assert_eq!(result.minimal, b_min); // only in base
                assert_eq!(result.low, b_low); // only in base
                assert_eq!(result.medium, o_med); // only in other
                assert_eq!(result.high, o_high); // only in other
                assert_eq!(result.xhigh, None); // neither
            }

            // ================================================================
            // merge_extension_policy
            // ================================================================

            #[test]
            fn extension_policy_none_none_is_none(() in Just(())) {
                assert!(merge_extension_policy(None, None).is_none());
            }

            #[test]
            fn extension_policy_other_overrides(
                b_profile in prop::option::of(string_regex("[a-z]{3,10}").unwrap()),
                b_default_permissive in prop::option::of(any::<bool>()),
                b_danger in prop::option::of(any::<bool>()),
                o_profile in prop::option::of(string_regex("[a-z]{3,10}").unwrap()),
                o_default_permissive in prop::option::of(any::<bool>()),
                o_danger in prop::option::of(any::<bool>()),
            ) {
                let base = ExtensionPolicyConfig {
                    profile: b_profile.clone(),
                    default_permissive: b_default_permissive,
                    allow_dangerous: b_danger,
                };
                let other = ExtensionPolicyConfig {
                    profile: o_profile.clone(),
                    default_permissive: o_default_permissive,
                    allow_dangerous: o_danger,
                };
                let result = merge_extension_policy(Some(base), Some(other)).unwrap();
                assert_eq!(result.profile, o_profile.or(b_profile));
                assert_eq!(
                    result.default_permissive,
                    o_default_permissive.or(b_default_permissive)
                );
                assert_eq!(result.allow_dangerous, o_danger.or(b_danger));
            }

            // ================================================================
            // merge_repair_policy
            // ================================================================

            #[test]
            fn repair_policy_none_none_is_none(() in Just(())) {
                assert!(merge_repair_policy(None, None).is_none());
            }

            #[test]
            fn repair_policy_other_overrides(
                b_mode in prop::option::of(string_regex("[a-z-]{3,12}").unwrap()),
                o_mode in prop::option::of(string_regex("[a-z-]{3,12}").unwrap()),
            ) {
                let base = RepairPolicyConfig { mode: b_mode.clone() };
                let other = RepairPolicyConfig { mode: o_mode.clone() };
                let result = merge_repair_policy(Some(base), Some(other)).unwrap();
                assert_eq!(result.mode, o_mode.or(b_mode));
            }

            // ================================================================
            // merge_extension_risk
            // ================================================================

            #[test]
            fn extension_risk_none_none_is_none(() in Just(())) {
                assert!(merge_extension_risk(None, None).is_none());
            }

            #[test]
            fn extension_risk_other_overrides(
                b_en in prop::option::of(any::<bool>()),
                b_window in prop::option::of(1u32..1000),
                o_en in prop::option::of(any::<bool>()),
                o_timeout in prop::option::of(1u64..60_000),
            ) {
                let base = ExtensionRiskConfig {
                    enabled: b_en, alpha: None, window_size: b_window,
                    ledger_limit: None, decision_timeout_ms: None,
                    fail_closed: None, enforce: None,
                };
                let other = ExtensionRiskConfig {
                    enabled: o_en, alpha: None, window_size: None,
                    ledger_limit: None, decision_timeout_ms: o_timeout,
                    fail_closed: None, enforce: None,
                };
                let result = merge_extension_risk(Some(base), Some(other)).unwrap();
                assert_eq!(result.enabled, o_en.or(b_en));
                assert_eq!(result.window_size, b_window); // only in base
                assert_eq!(result.decision_timeout_ms, o_timeout); // only in other
            }
        }

        // ================================================================
        // deep_merge_settings_value
        // ================================================================

        proptest! {
            #[test]
            fn deep_merge_null_deletes_key(key in "[a-z]{1,8}", val in "[a-z]{1,12}") {
                let mut dst = json!({ &key: val });
                deep_merge_settings_value(&mut dst, json!({ &key: null })).unwrap();
                assert!(dst.get(&key).is_none());
            }

            #[test]
            fn deep_merge_leaf_replaces(key in "[a-z]{1,8}", old in 0i64..100, new in 100i64..200) {
                let mut dst = json!({ &key: old });
                deep_merge_settings_value(&mut dst, json!({ &key: new })).unwrap();
                assert_eq!(dst[&key], json!(new));
            }

            #[test]
            fn deep_merge_nested_preserves_siblings(
                parent in "[a-z]{1,6}",
                child_a in "[a-z]{1,6}",
                child_b in "[a-z]{1,6}",
                val_a in 0i64..100,
                val_b in 0i64..100,
                val_new in 100i64..200,
            ) {
                if child_a != child_b {
                    let mut dst = json!({ &parent: { &child_a: val_a, &child_b: val_b } });
                    deep_merge_settings_value(
                        &mut dst,
                        json!({ &parent: { &child_a: val_new } }),
                    ).unwrap();
                    assert_eq!(dst[&parent][&child_a], json!(val_new));
                    assert_eq!(dst[&parent][&child_b], json!(val_b));
                }
            }

            #[test]
            fn deep_merge_non_object_patch_rejected(val in 0i64..1000) {
                let mut dst = json!({});
                assert!(deep_merge_settings_value(&mut dst, json!(val)).is_err());
            }

            #[test]
            fn deep_merge_idempotent(key in "[a-z]{1,6}", val in "[a-z]{1,10}") {
                let patch = json!({ &key: &val });
                let mut dst1 = json!({});
                let mut dst2 = json!({});
                deep_merge_settings_value(&mut dst1, patch.clone()).unwrap();
                deep_merge_settings_value(&mut dst2, patch.clone()).unwrap();
                deep_merge_settings_value(&mut dst2, patch).unwrap();
                assert_eq!(dst1, dst2);
            }
        }
    }
}
