//! Integration tests for configuration precedence and patching invariants.

mod common;

use common::TestHarness;
use pi::config::{Config, SettingsScope};
use serde_json::json;
use std::env;
use std::ffi::OsString;
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

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = env::var_os(key);
        env::set_var(key, value);
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = env::var_os(key);
        env::remove_var(key);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => env::set_var(self.key, value),
            None => env::remove_var(self.key),
        }
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
    let _global_guard = EnvVarGuard::set("PI_CODING_AGENT_DIR", &global_dir);
    let _override_guard = EnvVarGuard::set("PI_CONFIG_PATH", &override_path);

    let config = Config::load().expect("load config");
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
    let _global_guard = EnvVarGuard::set("PI_CODING_AGENT_DIR", &global_dir);
    let _override_guard = EnvVarGuard::remove("PI_CONFIG_PATH");

    let config = Config::load().expect("load config");
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
fn config_dirs_respect_pi_env_overrides() {
    let _lock = config_lock();
    let harness = TestHarness::new("config_dirs_respect_pi_env_overrides");

    let agent_dir = harness.create_dir("agent-root");
    let sessions_dir = harness.create_dir("sessions-custom");
    let packages_dir = harness.create_dir("packages-custom");

    let _global_guard = EnvVarGuard::set("PI_CODING_AGENT_DIR", &agent_dir);
    let _sessions_guard = EnvVarGuard::remove("PI_SESSIONS_DIR");
    let _packages_guard = EnvVarGuard::remove("PI_PACKAGE_DIR");

    assert_eq!(Config::global_dir(), agent_dir);
    assert_eq!(Config::sessions_dir(), agent_dir.join("sessions"));
    assert_eq!(Config::package_dir(), agent_dir.join("packages"));
    assert_eq!(Config::auth_path(), agent_dir.join("auth.json"));

    let _sessions_guard = EnvVarGuard::set("PI_SESSIONS_DIR", &sessions_dir);
    let _packages_guard = EnvVarGuard::set("PI_PACKAGE_DIR", &packages_dir);

    assert_eq!(Config::sessions_dir(), sessions_dir);
    assert_eq!(Config::package_dir(), packages_dir);
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
fn config_load_pi_config_path_invalid_json_falls_back_to_defaults() {
    let _lock = config_lock();
    let harness =
        TestHarness::new("config_load_pi_config_path_invalid_json_falls_back_to_defaults");

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
    let _global_guard = EnvVarGuard::set("PI_CODING_AGENT_DIR", &global_dir);
    let _override_guard = EnvVarGuard::set("PI_CONFIG_PATH", &override_path);

    let config = Config::load().expect("load config");
    harness.log().info_ctx("config", "Loaded config", |ctx| {
        ctx.push((
            "theme".to_string(),
            config.theme.as_deref().unwrap_or("<none>").to_string(),
        ));
    });

    assert!(config.theme.is_none());
    assert!(config.default_provider.is_none());
}
