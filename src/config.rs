//! Configuration loading and management.

use crate::agent::QueueMode;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write as _;
use std::path::{Path, PathBuf};
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

    // Model Configuration
    #[serde(alias = "defaultProvider")]
    pub default_provider: Option<String>,
    #[serde(alias = "defaultModel")]
    pub default_model: Option<String>,
    #[serde(alias = "defaultThinkingLevel")]
    pub default_thinking_level: Option<String>,
    #[serde(alias = "enabledModels")]
    pub enabled_models: Option<Vec<String>>,

    // Message Handling
    #[serde(alias = "steeringMode")]
    pub steering_mode: Option<String>,
    #[serde(alias = "followUpMode")]
    pub follow_up_mode: Option<String>,

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

    // Extension Policy
    #[serde(alias = "extensionPolicy")]
    pub extension_policy: Option<ExtensionPolicyConfig>,
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
///     "profile": "safe",
///     "allowDangerous": false
///   }
/// }
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtensionPolicyConfig {
    /// Policy profile: "safe", "standard" (default), or "permissive".
    pub profile: Option<String>,
    /// Allow dangerous capabilities (exec, env). Overrides profile's deny list.
    #[serde(alias = "allowDangerous")]
    pub allow_dangerous: Option<bool>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsScope {
    Global,
    Project,
}

impl Config {
    /// Load configuration from global and project settings.
    pub fn load() -> Result<Self> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let config_path = std::env::var_os("PI_CONFIG_PATH").map(PathBuf::from);
        Self::load_with_roots(config_path.as_deref(), &Self::global_dir(), &cwd)
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
            let config = Self::load_from_path(path)?;
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
            last_changelog_version: other.last_changelog_version.or(base.last_changelog_version),
            double_escape_action: other.double_escape_action.or(base.double_escape_action),
            editor_padding_x: other.editor_padding_x.or(base.editor_padding_x),
            autocomplete_max_visible: other
                .autocomplete_max_visible
                .or(base.autocomplete_max_visible),
            session_picker_input: other.session_picker_input.or(base.session_picker_input),
            session_store: other.session_store.or(base.session_store),

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

            // Extension Policy
            extension_policy: merge_extension_policy(
                base.extension_policy,
                other.extension_policy,
            ),
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

    pub fn terminal_show_images(&self) -> bool {
        self.terminal
            .as_ref()
            .and_then(|t| t.show_images)
            .unwrap_or(true)
    }

    pub fn terminal_clear_on_shrink(&self) -> bool {
        self.terminal
            .as_ref()
            .and_then(|t| t.clear_on_shrink)
            .unwrap_or(false)
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

    /// Resolve the extension policy from config, CLI override, and env var.
    ///
    /// Resolution order (highest precedence first):
    /// 1. `cli_override` (from `--extension-policy` flag)
    /// 2. `PI_EXTENSION_POLICY` environment variable
    /// 3. `extension_policy.profile` from settings.json
    /// 4. Default: "standard"
    ///
    /// If `allow_dangerous` is true (from config or env), exec/env are removed
    /// from the policy's deny list.
    pub fn resolve_extension_policy(
        &self,
        cli_override: Option<&str>,
    ) -> crate::extensions::ExtensionPolicy {
        use crate::extensions::PolicyProfile;

        // Determine profile name: CLI > env > config > default
        let profile_name = cli_override
            .map(ToString::to_string)
            .or_else(|| std::env::var("PI_EXTENSION_POLICY").ok())
            .or_else(|| {
                self.extension_policy
                    .as_ref()
                    .and_then(|p| p.profile.clone())
            })
            .unwrap_or_else(|| "standard".to_string());

        let profile = match profile_name.to_ascii_lowercase().as_str() {
            "safe" => PolicyProfile::Safe,
            "permissive" => PolicyProfile::Permissive,
            _ => PolicyProfile::Standard,
        };

        let mut policy = profile.to_policy();

        // Check allow_dangerous: config setting or PI_EXTENSION_ALLOW_DANGEROUS env
        let allow_dangerous = self
            .extension_policy
            .as_ref()
            .and_then(|p| p.allow_dangerous)
            .unwrap_or(false)
            || std::env::var("PI_EXTENSION_ALLOW_DANGEROUS")
                .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

        if allow_dangerous {
            policy.deny_caps.retain(|cap| cap != "exec" && cap != "env");
        }

        policy
    }

    fn emit_queue_mode_diagnostics(&self) {
        emit_queue_mode_diagnostic("steering_mode", self.steering_mode.as_deref());
        emit_queue_mode_diagnostic("follow_up_mode", self.follow_up_mode.as_deref());
    }
}

fn env_lookup(var: &str) -> Option<String> {
    std::env::var(var).ok()
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
    match mode.map(str::trim) {
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
            allow_dangerous: other.allow_dangerous.or(base.allow_dangerous),
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

    Ok(())
}

fn patch_settings_file(path: &Path, patch: Value) -> Result<Value> {
    let mut settings = load_settings_json_object(path)?;
    deep_merge_settings_value(&mut settings, patch)?;
    write_settings_json_atomic(path, &settings)?;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::{
        Config, SettingsScope, extension_index_path_from_env, global_dir_from_env,
        package_dir_from_env, sessions_dir_from_env,
    };
    use crate::agent::QueueMode;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;
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
    fn patch_settings_writes_with_restrictive_permissions() {
        let temp = TempDir::new().expect("create tempdir");
        let cwd = temp.path().join("cwd");
        let global_dir = temp.path().join("global");
        let settings_path =
            Config::settings_path_with_roots(SettingsScope::Project, &global_dir, &cwd);

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
            let mode = std::fs::metadata(&settings_path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
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
        assert_eq!(config.thinking_budget("xhigh"), u32::MAX);
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
            }),
            ..Config::default()
        };
        assert_eq!(config.thinking_budget("minimal"), 100);
        assert_eq!(config.thinking_budget("low"), 200);
        assert_eq!(config.thinking_budget("medium"), 300);
        assert_eq!(config.thinking_budget("high"), 400);
        assert_eq!(config.thinking_budget("xhigh"), 500);
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
                "sessionPickerInput": 2
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
}
