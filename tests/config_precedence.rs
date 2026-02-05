//! Integration tests for configuration precedence and patching invariants.

mod common;

use common::TestHarness;
use pi::config::{Config, SettingsScope, TerminalSettings};
use serde_json::json;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

fn config_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().expect("lock")
}

struct CurrentDirGuard {
    previous: PathBuf,
}

impl CurrentDirGuard {
    fn new(path: &Path) -> Self {
        let previous = env::current_dir().expect("read current dir");
        env::set_current_dir(path).expect("set current dir");
        Self { previous }
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = env::set_current_dir(&self.previous);
    }
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dir");
    }
    std::fs::write(path, contents).expect("write file");
}

#[test]
fn config_load_pi_config_path_override_beats_project_and_global() {
    let _lock = config_lock();
    let harness = TestHarness::new("config_load_pi_config_path_override_beats_project_and_global");

    let cwd = harness.create_dir("cwd");
    let global_dir = harness.create_dir("global");
    let override_path = harness.create_file(
        "override.json",
        br#"{ "theme": "override", "default_provider": "openai" }"#,
    );

    write_file(
        &global_dir.join("settings.json"),
        r#"{ "theme": "global", "default_provider": "anthropic" }"#,
    );
    write_file(
        &cwd.join(".pi/settings.json"),
        r#"{ "theme": "project", "default_provider": "google" }"#,
    );

    let _cwd_guard = CurrentDirGuard::new(&cwd);
    let config =
        Config::load_with_roots(Some(&override_path), &global_dir, &cwd).expect("load config");
    harness.log().info_ctx("config", "Loaded config", |ctx| {
        ctx.push((
            "theme".to_string(),
            config.theme.as_deref().unwrap_or("<none>").to_string(),
        ));
        ctx.push((
            "default_provider".to_string(),
            config
                .default_provider
                .as_deref()
                .unwrap_or("<none>")
                .to_string(),
        ));
    });

    assert_eq!(config.theme.as_deref(), Some("override"));
    assert_eq!(config.default_provider.as_deref(), Some("openai"));
}

#[test]
fn config_load_merges_project_over_global_when_no_override() {
    let _lock = config_lock();
    let harness = TestHarness::new("config_load_merges_project_over_global_when_no_override");

    let cwd = harness.create_dir("cwd");
    let global_dir = harness.create_dir("global");

    write_file(
        &global_dir.join("settings.json"),
        r#"{ "theme": "global", "default_model": "global" }"#,
    );
    write_file(
        &cwd.join(".pi/settings.json"),
        r#"{ "default_model": "project" }"#,
    );

    let _cwd_guard = CurrentDirGuard::new(&cwd);
    let config = Config::load_with_roots(None, &global_dir, &cwd).expect("load config");
    harness.log().info_ctx("config", "Loaded config", |ctx| {
        ctx.push((
            "theme".to_string(),
            config.theme.as_deref().unwrap_or("<none>").to_string(),
        ));
        ctx.push((
            "default_model".to_string(),
            config
                .default_model
                .as_deref()
                .unwrap_or("<none>")
                .to_string(),
        ));
    });

    assert_eq!(config.theme.as_deref(), Some("global"));
    assert_eq!(config.default_model.as_deref(), Some("project"));
}

#[test]
fn config_dirs_use_explicit_roots() {
    let _lock = config_lock();
    let harness = TestHarness::new("config_dirs_use_explicit_roots");

    let cwd = harness.create_dir("cwd");
    let global_dir = harness.create_dir("global");

    assert_eq!(
        Config::settings_path_with_roots(SettingsScope::Global, &global_dir, &cwd),
        global_dir.join("settings.json")
    );
    assert_eq!(
        Config::settings_path_with_roots(SettingsScope::Project, &global_dir, &cwd),
        cwd.join(".pi/settings.json")
    );
}

#[test]
fn patch_settings_is_deep_merge_and_writes_restrictive_permissions() {
    let harness =
        TestHarness::new("patch_settings_is_deep_merge_and_writes_restrictive_permissions");

    let cwd = harness.create_dir("cwd");
    let global_dir = harness.create_dir("global");
    let settings_path = Config::settings_path_with_roots(SettingsScope::Project, &global_dir, &cwd);

    harness.log().info_ctx("setup", "settings_path", |ctx| {
        ctx.push(("path".to_string(), settings_path.display().to_string()));
    });

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
        serde_json::from_str(&std::fs::read_to_string(&settings_path).expect("read settings"))
            .expect("parse settings");
    assert_eq!(stored["theme"], json!("dark"));
    assert_eq!(stored["compaction"]["reserve_tokens"], json!(111));
    assert_eq!(stored["compaction"]["enabled"], json!(false));

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
fn config_load_pi_config_path_invalid_json_returns_error() {
    let _lock = config_lock();
    let harness = TestHarness::new("config_load_pi_config_path_invalid_json_returns_error");

    let cwd = harness.create_dir("cwd");
    let global_dir = harness.create_dir("global");
    let override_path = harness.create_file("override.json", b"not json");

    write_file(
        &global_dir.join("settings.json"),
        r#"{ "theme": "global", "default_provider": "anthropic" }"#,
    );
    write_file(
        &cwd.join(".pi/settings.json"),
        r#"{ "theme": "project", "default_provider": "google" }"#,
    );

    let _cwd_guard = CurrentDirGuard::new(&cwd);
    let result = Config::load_with_roots(Some(&override_path), &global_dir, &cwd);
    harness
        .log()
        .info("config", format!("Loaded config result: {result:?}"));
    assert!(result.is_err());
}

#[test]
fn config_terminal_defaults_and_overrides() {
    let _lock = config_lock();
    let config = Config::default();
    assert!(config.terminal_show_images());
    assert!(!config.terminal_clear_on_shrink());

    let config = Config {
        terminal: Some(TerminalSettings {
            show_images: Some(false),
            clear_on_shrink: Some(true),
        }),
        ..Config::default()
    };
    assert!(!config.terminal_show_images());
    assert!(config.terminal_clear_on_shrink());
}
