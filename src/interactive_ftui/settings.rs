//! Settings on the default interactive stack. The driver saves one validated
//! choice before updating its live session; the UI only displays that reply.

use std::path::{Path, PathBuf};

use crate::agent::QueueMode;
use crate::config::{Config, MarkdownSpacing, SettingsScope};
use crate::error::{Error, Result};
use crate::failover::{FallbackOn, RetryPolicy};
use crate::sdk::AgentSessionHandle;

use super::DoubleEscapeAction;

/// Save into a configuration layer that the session will load again. Project
/// preferences are ignored in untrusted workspaces; explicit user overrides
/// remain authoritative regardless of workspace trust.
pub(super) fn settings_destination(
    config_override: Option<&Path>,
    global_dir: &Path,
    cwd: &Path,
    workspace_trusted: bool,
) -> PathBuf {
    if let Some(path) = config_override {
        return Config::resolve_config_override_path(path, cwd);
    }
    let scope = if workspace_trusted {
        SettingsScope::Project
    } else {
        SettingsScope::Global
    };
    Config::settings_path_with_roots(scope, global_dir, cwd)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsKey {
    AutoRetry,
    ModelFallback,
    FallbackOn,
    AutoCompaction,
    SteeringMode,
    FollowUpMode,
    AutoResizeImages,
    HideThinking,
    MarkdownSpacing,
    DoubleEscape,
    AutocompleteRows,
}

impl SettingsKey {
    pub const ALL: [Self; 11] = [
        Self::AutoRetry,
        Self::ModelFallback,
        Self::FallbackOn,
        Self::AutoCompaction,
        Self::SteeringMode,
        Self::FollowUpMode,
        Self::AutoResizeImages,
        Self::HideThinking,
        Self::MarkdownSpacing,
        Self::DoubleEscape,
        Self::AutocompleteRows,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::AutoRetry => "retry",
            Self::ModelFallback => "model-fallback",
            Self::FallbackOn => "fallback-on",
            Self::AutoCompaction => "compaction",
            Self::SteeringMode => "steering",
            Self::FollowUpMode => "follow-up",
            Self::AutoResizeImages => "image-resize",
            Self::HideThinking => "hide-thinking",
            Self::MarkdownSpacing => "spacing",
            Self::DoubleEscape => "double-escape",
            Self::AutocompleteRows => "completion-rows",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.id() == value)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::AutoRetry => "Automatic retry and provider fallback",
            Self::ModelFallback => "Allow recovery to switch models",
            Self::FallbackOn => "Provider fallback trigger",
            Self::AutoCompaction => "Automatic context compaction",
            Self::SteeringMode => "Steering message delivery",
            Self::FollowUpMode => "Follow-up message delivery",
            Self::AutoResizeImages => "Resize attached images",
            Self::HideThinking => "Collapse thinking blocks",
            Self::MarkdownSpacing => "Transcript spacing",
            Self::DoubleEscape => "Double Escape action",
            Self::AutocompleteRows => "Completion menu rows",
        }
    }

    pub const fn choices(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::AutoRetry
            | Self::ModelFallback
            | Self::AutoCompaction
            | Self::AutoResizeImages
            | Self::HideThinking => &[("on", "On"), ("off", "Off")],
            Self::FallbackOn => &[
                ("any", "Any recoverable provider failure"),
                ("usage-limit", "Account usage limits only"),
                (
                    "except-usage-limit",
                    "Failures other than account usage limits",
                ),
            ],
            Self::SteeringMode | Self::FollowUpMode => &[
                ("one-at-a-time", "One message at a time"),
                ("all", "All queued messages together"),
            ],
            Self::MarkdownSpacing => &[("comfortable", "Comfortable"), ("compact", "Compact")],
            Self::DoubleEscape => &[
                ("rewind", "Choose a message to rewind to"),
                ("fork", "Choose a message to fork from"),
                ("tree", "Show the session tree"),
                ("none", "Do nothing"),
            ],
            Self::AutocompleteRows => &[
                ("3", "3"),
                ("5", "5"),
                ("8", "8"),
                ("10", "10"),
                ("12", "12"),
                ("15", "15"),
                ("20", "20"),
            ],
        }
    }

    fn patch(self, value: &str) -> Result<serde_json::Value> {
        if !self.choices().iter().any(|(choice, _)| *choice == value) {
            return Err(Error::validation(format!(
                "Invalid value for {}: {value}",
                self.label()
            )));
        }
        let enabled = value == "on";
        // Remove aliases in the same locked update. Otherwise changing a
        // camelCase settings file can create duplicate serde fields and make
        // the next startup fail. Unrelated fields stay untouched.
        Ok(match self {
            Self::AutoRetry => serde_json::json!({"retry": {"enabled": enabled}}),
            Self::ModelFallback => serde_json::json!({
                "retry": {"model_fallback": enabled, "modelFallback": null},
            }),
            Self::FallbackOn => serde_json::json!({
                "retry": {"fallback_on": value, "fallbackOn": null},
            }),
            Self::AutoCompaction => serde_json::json!({"compaction": {"enabled": enabled}}),
            Self::SteeringMode => serde_json::json!({
                "steering_mode": value, "steeringMode": null, "queueMode": null,
            }),
            Self::FollowUpMode => serde_json::json!({
                "follow_up_mode": value, "followUpMode": null,
            }),
            Self::AutoResizeImages => serde_json::json!({
                "images": {"auto_resize": enabled, "autoResize": null},
            }),
            Self::HideThinking => serde_json::json!({
                "hide_thinking_block": enabled, "hideThinkingBlock": null,
            }),
            Self::MarkdownSpacing => serde_json::json!({"markdown": {"spacing": value}}),
            Self::DoubleEscape => serde_json::json!({
                "double_escape_action": value, "doubleEscapeAction": null,
            }),
            Self::AutocompleteRows => serde_json::json!({
                "autocomplete_max_visible": value.parse::<u32>().map_err(|err| {
                    Error::validation(format!("Invalid completion row count: {err}"))
                })?,
                "autocompleteMaxVisible": null,
            }),
        })
    }

    fn update_config(self, config: &mut Config, value: &str) {
        let enabled = value == "on";
        match self {
            Self::AutoRetry => config.retry.get_or_insert_default().enabled = Some(enabled),
            Self::ModelFallback => {
                config.retry.get_or_insert_default().model_fallback = Some(enabled);
            }
            Self::FallbackOn => {
                config.retry.get_or_insert_default().fallback_on = Some(fallback_on(value));
            }
            Self::AutoCompaction => {
                config.compaction.get_or_insert_default().enabled = Some(enabled);
            }
            Self::SteeringMode => config.steering_mode = Some(value.to_string()),
            Self::FollowUpMode => config.follow_up_mode = Some(value.to_string()),
            Self::AutoResizeImages => {
                config.images.get_or_insert_default().auto_resize = Some(enabled);
            }
            Self::HideThinking => config.hide_thinking_block = Some(enabled),
            Self::MarkdownSpacing => {
                config.markdown.get_or_insert_default().spacing = Some(if value == "compact" {
                    MarkdownSpacing::Compact
                } else {
                    MarkdownSpacing::Comfortable
                });
            }
            Self::DoubleEscape => config.double_escape_action = Some(value.to_string()),
            Self::AutocompleteRows => config.autocomplete_max_visible = value.parse().ok(),
        }
    }
}

/// Effective settings acknowledged by the driver. No credentials or complete
/// configuration document cross the UI event channel.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct SettingsSnapshot {
    pub path: PathBuf,
    pub auto_retry: bool,
    pub model_fallback: bool,
    pub fallback_on: FallbackOn,
    pub auto_compaction: bool,
    pub steering: QueueMode,
    pub follow_up: QueueMode,
    pub auto_resize_images: bool,
    pub hide_thinking: bool,
    pub markdown_spacing: MarkdownSpacing,
    pub double_escape: DoubleEscapeAction,
    pub autocomplete_rows: usize,
}

impl SettingsSnapshot {
    pub fn value(&self, key: SettingsKey) -> String {
        match key {
            SettingsKey::AutoRetry => on_off(self.auto_retry),
            SettingsKey::ModelFallback => on_off(self.model_fallback),
            SettingsKey::FallbackOn => self.fallback_on.as_str(),
            SettingsKey::AutoCompaction => on_off(self.auto_compaction),
            SettingsKey::SteeringMode => self.steering.as_str(),
            SettingsKey::FollowUpMode => self.follow_up.as_str(),
            SettingsKey::AutoResizeImages => on_off(self.auto_resize_images),
            SettingsKey::HideThinking => on_off(self.hide_thinking),
            SettingsKey::MarkdownSpacing => match self.markdown_spacing {
                MarkdownSpacing::Comfortable => "comfortable",
                MarkdownSpacing::Compact => "compact",
            },
            SettingsKey::DoubleEscape => match self.double_escape {
                DoubleEscapeAction::Rewind => "rewind",
                DoubleEscapeAction::Fork => "fork",
                DoubleEscapeAction::Tree => "tree",
                DoubleEscapeAction::None => "none",
            },
            SettingsKey::AutocompleteRows => return self.autocomplete_rows.to_string(),
        }
        .to_string()
    }
}

const fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

/// Values are validated against `SettingsKey::choices` before applying.
fn fallback_on(value: &str) -> FallbackOn {
    match value {
        "usage-limit" => FallbackOn::UsageLimit,
        "except-usage-limit" => FallbackOn::ExceptUsageLimit,
        _ => FallbackOn::Any,
    }
}

pub(super) struct SettingsController {
    config: Config,
    path: PathBuf,
    /// Disabling automatic recovery preserves the configured budgets for the
    /// next enable, including an embedder's explicit SessionOptions override.
    retry_policy: RetryPolicy,
}

impl SettingsController {
    pub(super) fn new(config: Config, path: PathBuf, handle: &AgentSessionHandle) -> Self {
        let retry_policy = handle.retry_policy().unwrap_or_else(|| RetryPolicy {
            max_retries: config.retry_max_retries(),
            max_failovers_per_turn: config.max_failovers_per_turn(),
            model_fallback: config.model_fallback_enabled(),
            fallback_on: config.retry_fallback_on(),
            base_delay_ms: config.retry_base_delay_ms(),
            max_delay_ms: config.retry_max_delay_ms(),
        });
        Self {
            config,
            path,
            retry_policy,
        }
    }

    pub(super) fn snapshot(&self, handle: &AgentSessionHandle) -> SettingsSnapshot {
        let (steering, follow_up) = handle.session().agent.queue_modes();
        let policy = handle.retry_policy().unwrap_or(self.retry_policy);
        SettingsSnapshot {
            path: self.path.clone(),
            auto_retry: handle.retry_policy().is_some(),
            model_fallback: policy.model_fallback,
            fallback_on: policy.fallback_on,
            auto_compaction: handle.compaction_settings().enabled,
            steering,
            follow_up,
            auto_resize_images: self.config.image_auto_resize(),
            hide_thinking: self.config.hide_thinking_block.unwrap_or(false),
            markdown_spacing: self.config.markdown_spacing(),
            double_escape: DoubleEscapeAction::from_setting(
                self.config.double_escape_action.as_deref(),
            ),
            autocomplete_rows: usize::try_from(self.config.autocomplete_max_visible.unwrap_or(8))
                .unwrap_or(8)
                .clamp(1, 20),
        }
    }

    pub(super) fn apply(
        &mut self,
        handle: &mut AgentSessionHandle,
        key: SettingsKey,
        value: &str,
    ) -> Result<SettingsSnapshot> {
        let patch = key.patch(value)?;
        // The existing writer serializes concurrent edits, deep-merges only
        // this field, and syncs an atomic replacement. No live setting is
        // changed if persistence fails.
        Config::patch_settings_to_path(&self.path, patch)?;
        match key {
            SettingsKey::AutoRetry => {
                if let Some(policy) = handle.retry_policy() {
                    self.retry_policy = policy;
                }
                handle.set_retry_policy((value == "on").then_some(self.retry_policy));
            }
            SettingsKey::ModelFallback | SettingsKey::FallbackOn => {
                let active = handle.retry_policy();
                self.retry_policy = active.unwrap_or(self.retry_policy);
                if key == SettingsKey::ModelFallback {
                    self.retry_policy.model_fallback = value == "on";
                } else {
                    self.retry_policy.fallback_on = fallback_on(value);
                }
                // Editing fallback preferences must not enable recovery.
                if active.is_some() {
                    handle.set_retry_policy(Some(self.retry_policy));
                }
            }
            SettingsKey::AutoCompaction => {
                handle.session_mut().set_auto_compaction(value == "on");
            }
            SettingsKey::SteeringMode | SettingsKey::FollowUpMode => {
                let mode = if value == "all" {
                    QueueMode::All
                } else {
                    QueueMode::OneAtATime
                };
                let (steering, follow_up) = handle.session().agent.queue_modes();
                let (steering, follow_up) = if key == SettingsKey::SteeringMode {
                    (mode, follow_up)
                } else {
                    (steering, mode)
                };
                handle.session_mut().set_queue_modes(steering, follow_up);
            }
            _ => {}
        }
        key.update_config(&mut self.config, value);
        Ok(self.snapshot(handle))
    }

    pub(super) const fn config(&self) -> &Config {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn handle() -> AgentSessionHandle {
        let agent = crate::agent::Agent::new(
            Arc::new(crate::providers::anthropic::AnthropicProvider::new(
                "settings-fixture",
            )),
            crate::tools::ToolRegistry::without_builtins(None),
            crate::agent::AgentConfig::default(),
        );
        let session = crate::agent::AgentSession::new(
            agent,
            Arc::new(asupersync::sync::Mutex::new(
                crate::session::Session::in_memory(),
            )),
            false,
            crate::compaction::ResolvedCompactionSettings {
                enabled: true,
                context_window_tokens: 77_777,
                reserve_tokens: 1234,
                keep_recent_tokens: 4321,
                ..Default::default()
            },
        );
        AgentSessionHandle::from_session_with_listeners(
            session,
            crate::sdk::EventListeners::default(),
        )
        .with_retry(Some(RetryPolicy {
            max_retries: 7,
            max_failovers_per_turn: 2,
            model_fallback: true,
            fallback_on: FallbackOn::Any,
            base_delay_ms: 17,
            max_delay_ms: 333,
        }))
    }

    #[test]
    fn saved_preferences_apply_to_the_live_session_without_resetting_budgets() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut handle = handle();
        let path = temp.path().join("settings.json");
        let mut settings = SettingsController::new(Config::default(), path.clone(), &handle);
        let before = settings.snapshot(&handle);
        assert!(before.auto_retry && before.auto_compaction);
        settings
            .apply(&mut handle, SettingsKey::AutoRetry, "off")
            .expect("disable retry");
        assert!(handle.retry_policy().is_none());
        settings
            .apply(&mut handle, SettingsKey::AutoRetry, "on")
            .expect("enable retry");
        let policy = handle.retry_policy().expect("restored policy");
        assert_eq!((policy.max_retries, policy.max_failovers_per_turn), (7, 2));
        assert_eq!((policy.base_delay_ms, policy.max_delay_ms), (17, 333));

        settings
            .apply(&mut handle, SettingsKey::AutoCompaction, "off")
            .expect("disable compaction");
        assert!(!handle.compaction_settings().enabled);
        settings
            .apply(&mut handle, SettingsKey::AutoCompaction, "on")
            .expect("enable compaction");
        assert_eq!(handle.compaction_settings().context_window_tokens, 77_777);
        assert_eq!(handle.compaction_settings().reserve_tokens, 1234);
        assert_eq!(handle.compaction_settings().keep_recent_tokens, 4321);

        settings
            .apply(&mut handle, SettingsKey::SteeringMode, "all")
            .expect("steering");
        assert_eq!(
            handle.session().agent.queue_modes(),
            (QueueMode::All, before.follow_up)
        );
        settings
            .apply(&mut handle, SettingsKey::FollowUpMode, "all")
            .expect("follow up");
        assert_eq!(
            handle.session().agent.queue_modes(),
            (QueueMode::All, QueueMode::All)
        );
        let loaded: Config =
            serde_json::from_str(&std::fs::read_to_string(path).expect("saved file"))
                .expect("valid saved config");
        assert!(loaded.retry_enabled());
        assert!(loaded.compaction_enabled());
        assert_eq!(loaded.steering_queue_mode(), QueueMode::All);
        assert_eq!(loaded.follow_up_queue_mode(), QueueMode::All);
    }

    #[test]
    fn fallback_preferences_apply_live_and_stay_disabled_until_recovery_is_enabled() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("settings.json");
        let mut handle = handle();
        let mut settings = SettingsController::new(Config::default(), path.clone(), &handle);
        settings
            .apply(&mut handle, SettingsKey::ModelFallback, "off")
            .expect("disable model switching");
        let policy = handle
            .retry_policy()
            .expect("same-model retries remain enabled");
        assert!(!policy.model_fallback);
        assert_eq!(policy.max_retries, 7);

        settings
            .apply(&mut handle, SettingsKey::AutoRetry, "off")
            .expect("disable automatic recovery");
        let snapshot = settings
            .apply(&mut handle, SettingsKey::FallbackOn, "usage-limit")
            .expect("retain usage filter");
        assert!(!snapshot.auto_retry);
        assert!(!snapshot.model_fallback);
        assert_eq!(snapshot.fallback_on, FallbackOn::UsageLimit);
        assert!(handle.retry_policy().is_none());

        settings
            .apply(&mut handle, SettingsKey::AutoRetry, "on")
            .expect("enable recovery");
        let policy = handle.retry_policy().expect("restored recovery");
        assert!(!policy.model_fallback);
        assert_eq!(policy.fallback_on, FallbackOn::UsageLimit);
        assert_eq!((policy.max_retries, policy.max_failovers_per_turn), (7, 2));
        assert_eq!((policy.base_delay_ms, policy.max_delay_ms), (17, 333));

        let loaded: Config =
            serde_json::from_str(&std::fs::read_to_string(path).expect("saved settings"))
                .expect("reload preferences");
        assert!(loaded.retry_enabled());
        assert!(!loaded.model_fallback_enabled());
        assert_eq!(loaded.retry_fallback_on(), FallbackOn::UsageLimit);
    }

    #[test]
    fn persistence_failure_and_invalid_choices_do_not_change_live_preferences() {
        let temp = tempfile::tempdir().expect("tempdir");
        // An existing directory cannot be read/replaced as a settings file.
        let blocked = temp.path().join("blocked-settings.json");
        std::fs::create_dir(&blocked).expect("blocking directory");
        let mut handle = handle();
        let mut settings = SettingsController::new(Config::default(), blocked, &handle);
        let before = settings.snapshot(&handle);
        for key in SettingsKey::ALL {
            let value = key
                .choices()
                .iter()
                .find(|(value, _)| *value != before.value(key))
                .map(|(value, _)| *value)
                .expect("alternative setting");
            assert!(settings.apply(&mut handle, key, value).is_err(), "{key:?}");
            assert_eq!(
                settings.snapshot(&handle),
                before,
                "failed {key:?} changed state"
            );
        }
        let missing = temp.path().join("not-created.json");
        let mut settings = SettingsController::new(Config::default(), missing.clone(), &handle);
        for key in SettingsKey::ALL {
            assert!(settings.apply(&mut handle, key, "invalid-value").is_err());
        }
        assert!(
            !missing.exists(),
            "invalid selections must not create configuration"
        );
        assert_eq!(handle.retry_policy().expect("policy kept").max_retries, 7);
    }

    #[test]
    fn saving_image_resize_first_creates_a_reloadable_configuration() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(".pi/settings.json");
        let mut handle = handle();
        let mut settings = SettingsController::new(Config::default(), path.clone(), &handle);

        let snapshot = settings
            .apply(&mut handle, SettingsKey::AutoResizeImages, "off")
            .expect("save first preference");
        assert!(!snapshot.auto_resize_images);
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("saved settings"))
                .expect("JSON");
        assert_eq!(
            raw,
            serde_json::json!({ "images": { "auto_resize": false } })
        );
        let loaded: Config = serde_json::from_value(raw).expect("reload saved configuration");
        assert!(!loaded.image_auto_resize());
    }

    #[test]
    fn saved_preferences_reload_from_the_layer_allowed_by_workspace_trust() {
        for (workspace_trusted, explicit_override) in
            [(true, false), (false, false), (true, true), (false, true)]
        {
            let temp = tempfile::tempdir().expect("tempdir");
            let global_dir = temp.path().join("global");
            let cwd = temp.path().join("workspace");
            let project_path = cwd.join(".pi/settings.json");
            let global_path = global_dir.join("settings.json");
            std::fs::create_dir_all(project_path.parent().expect("project parent"))
                .expect("project directory");
            std::fs::create_dir_all(&global_dir).expect("global directory");
            let project_bytes: &[u8] =
                b"{\"steering_mode\":\"one-at-a-time\",\"hide_thinking_block\":false}\n";
            let global_bytes: &[u8] =
                b"{\"compaction\":{\"enabled\":true},\"images\":{\"auto_resize\":true}}\n";
            std::fs::write(&project_path, project_bytes).expect("project settings");
            std::fs::write(&global_path, global_bytes).expect("global settings");
            // A relative override also exercises the same cwd resolution used
            // by the loader, without changing process environment variables.
            let config_override = explicit_override.then(|| PathBuf::from("custom.json"));
            let path = settings_destination(
                config_override.as_deref(),
                &global_dir,
                &cwd,
                workspace_trusted,
            );
            let expected_path = if explicit_override {
                cwd.join("custom.json")
            } else if workspace_trusted {
                project_path.clone()
            } else {
                global_path.clone()
            };
            assert_eq!(path, expected_path);
            let initial = Config::load_with_roots_and_project_trust(
                config_override.as_deref(),
                &global_dir,
                &cwd,
                workspace_trusted,
            )
            .expect("initial configuration");
            let mut handle = handle();
            let mut settings = SettingsController::new(initial, path, &handle);
            for (key, value) in [
                (SettingsKey::SteeringMode, "all"),
                (SettingsKey::FollowUpMode, "all"),
                (SettingsKey::AutoCompaction, "off"),
                (SettingsKey::HideThinking, "on"),
                (SettingsKey::AutoResizeImages, "off"),
            ] {
                settings
                    .apply(&mut handle, key, value)
                    .expect("save preference");
            }
            let reloaded = Config::load_with_roots_and_project_trust(
                config_override.as_deref(),
                &global_dir,
                &cwd,
                workspace_trusted,
            )
            .expect("reload saved preferences");
            assert_eq!(reloaded.steering_queue_mode(), QueueMode::All);
            assert_eq!(reloaded.follow_up_queue_mode(), QueueMode::All);
            assert!(!reloaded.compaction_enabled());
            assert_eq!(reloaded.hide_thinking_block, Some(true));
            assert!(!reloaded.image_auto_resize());
            if !workspace_trusted || explicit_override {
                assert_eq!(
                    std::fs::read(&project_path).expect("ignored project settings"),
                    project_bytes,
                    "saving must not touch an ignored project configuration"
                );
            }
            if workspace_trusted || explicit_override {
                assert_eq!(
                    std::fs::read(&global_path).expect("unchanged global settings"),
                    global_bytes,
                    "project and explicit saves must not touch global configuration"
                );
            }
        }
    }

    #[test]
    fn explicit_destination_preserves_unrelated_fields_and_removes_changed_aliases() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("custom.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "steeringMode": "one-at-a-time", "followUpMode": "one-at-a-time",
                "hideThinkingBlock": false, "doubleEscapeAction": "tree",
                "autocompleteMaxVisible": 8,
                "images": {"autoResize": true, "blockImages": true},
                "markdown": {"spacing": "comfortable", "codeBlockIndent": 3},
                "retry": {
                    "enabled": true, "maxRetries": 9,
                    "modelFallback": true, "fallbackOn": "any",
                    "fallbackChains": {"default": ["other/model"]},
                },
                "compaction": {"enabled": true, "reserveTokens": 3210},
                "pluginOptions": {"custom": "keep me"},
            })
            .to_string(),
        )
        .expect("settings fixture");
        let config: Config = serde_json::from_str(&std::fs::read_to_string(&path).expect("read"))
            .expect("fixture config");
        let mut handle = handle();
        let mut settings = SettingsController::new(config, path.clone(), &handle);
        for (key, value) in [
            (SettingsKey::AutoRetry, "off"),
            (SettingsKey::ModelFallback, "off"),
            (SettingsKey::FallbackOn, "except-usage-limit"),
            (SettingsKey::AutoCompaction, "off"),
            (SettingsKey::SteeringMode, "all"),
            (SettingsKey::FollowUpMode, "all"),
            (SettingsKey::AutoResizeImages, "off"),
            (SettingsKey::HideThinking, "on"),
            (SettingsKey::MarkdownSpacing, "compact"),
            (SettingsKey::DoubleEscape, "fork"),
            (SettingsKey::AutocompleteRows, "15"),
        ] {
            settings
                .apply(&mut handle, key, value)
                .expect("save setting");
        }
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("saved")).expect("JSON");
        let loaded: Config = serde_json::from_value(raw.clone()).expect("no duplicate aliases");
        assert!(!loaded.retry_enabled() && !loaded.compaction_enabled());
        assert_eq!(loaded.retry_max_retries(), 9);
        assert!(!loaded.model_fallback_enabled());
        assert_eq!(loaded.retry_fallback_on(), FallbackOn::ExceptUsageLimit);
        assert!(raw["retry"].get("modelFallback").is_none());
        assert!(raw["retry"].get("fallbackOn").is_none());
        assert_eq!(loaded.compaction_reserve_tokens(), 3210);
        assert_eq!(raw["retry"]["fallbackChains"]["default"][0], "other/model");
        assert_eq!(raw["pluginOptions"]["custom"], "keep me");
        assert_eq!(raw["images"]["blockImages"], true);
        assert_eq!(raw["markdown"]["codeBlockIndent"], 3);
        assert!(!loaded.image_auto_resize());
        assert_eq!(loaded.hide_thinking_block, Some(true));
        assert_eq!(loaded.markdown_spacing(), MarkdownSpacing::Compact);
        assert_eq!(loaded.double_escape_action.as_deref(), Some("fork"));
        assert_eq!(loaded.autocomplete_max_visible, Some(15));
        assert!(!temp.path().join(".pi/settings.json").exists());
    }
}
