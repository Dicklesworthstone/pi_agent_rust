//! Startup migrations for legacy Pi layouts and config formats.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::config::Config;
use crate::session::encode_cwd;

const MIGRATION_GUIDE_URL: &str = "https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/CHANGELOG.md#extensions-migration";
const EXTENSIONS_DOC_URL: &str =
    "https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/docs/extensions.md";

const MANAGED_TOOL_BINARIES: &[&str] = &["fd", "rg", "fd.exe", "rg.exe"];

/// Summary of startup migration actions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Providers migrated into `auth.json`.
    pub migrated_auth_providers: Vec<String>,
    /// Number of session files moved from `~/.pi/agent/*.jsonl` to `sessions/<encoded-cwd>/`.
    pub migrated_session_files: usize,
    /// Directories where `commands/` was renamed to `prompts/`.
    pub migrated_commands_dirs: Vec<PathBuf>,
    /// Managed binaries moved from `tools/` to `bin/`.
    pub migrated_tool_binaries: Vec<String>,
    /// Deprecated layout warnings (hooks/tools).
    pub deprecation_warnings: Vec<String>,
    /// Non-fatal migration execution warnings.
    pub warnings: Vec<String>,
}

impl MigrationReport {
    #[must_use]
    pub fn messages(&self) -> Vec<String> {
        let mut messages = Vec::new();

        if !self.migrated_auth_providers.is_empty() {
            messages.push(format!(
                "Migrated legacy credentials into auth.json for providers: {}",
                self.migrated_auth_providers.join(", ")
            ));
        }
        if self.migrated_session_files > 0 {
            messages.push(format!(
                "Migrated {} legacy session file(s) into sessions/<encoded-cwd>/",
                self.migrated_session_files
            ));
        }
        if !self.migrated_commands_dirs.is_empty() {
            let dirs = self
                .migrated_commands_dirs
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            messages.push(format!("Migrated commands/ -> prompts/ at: {dirs}"));
        }
        if !self.migrated_tool_binaries.is_empty() {
            messages.push(format!(
                "Migrated managed binaries tools/ -> bin/: {}",
                self.migrated_tool_binaries.join(", ")
            ));
        }

        for warning in &self.warnings {
            messages.push(format!("Warning: {warning}"));
        }
        for warning in &self.deprecation_warnings {
            messages.push(format!("Warning: {warning}"));
        }

        if !self.deprecation_warnings.is_empty() {
            messages.push(format!("Migration guide: {MIGRATION_GUIDE_URL}"));
            messages.push(format!("Extensions docs: {EXTENSIONS_DOC_URL}"));
        }

        messages
    }
}

/// Run one-time startup migrations against the global agent directory.
#[must_use]
pub fn run_startup_migrations(cwd: &Path) -> MigrationReport {
    run_startup_migrations_with_agent_dir(&Config::global_dir(), cwd)
}

fn run_startup_migrations_with_agent_dir(agent_dir: &Path, cwd: &Path) -> MigrationReport {
    let mut report = MigrationReport::default();

    report.migrated_auth_providers = migrate_auth_to_auth_json(agent_dir, &mut report.warnings);
    report.migrated_session_files =
        migrate_sessions_from_agent_root(agent_dir, &mut report.warnings);
    report.migrated_tool_binaries = migrate_tools_to_bin(agent_dir, &mut report.warnings);

    if migrate_commands_to_prompts(agent_dir, &mut report.warnings) {
        report
            .migrated_commands_dirs
            .push(agent_dir.join("prompts"));
    }
    let project_dir = cwd.join(Config::project_dir());
    if migrate_commands_to_prompts(&project_dir, &mut report.warnings) {
        report
            .migrated_commands_dirs
            .push(project_dir.join("prompts"));
    }

    report
        .deprecation_warnings
        .extend(check_deprecated_extension_dirs(agent_dir, "Global"));
    report
        .deprecation_warnings
        .extend(check_deprecated_extension_dirs(&project_dir, "Project"));

    report
}

#[allow(clippy::too_many_lines)]
fn migrate_auth_to_auth_json(agent_dir: &Path, warnings: &mut Vec<String>) -> Vec<String> {
    let auth_path = agent_dir.join("auth.json");
    if auth_path.exists() {
        return Vec::new();
    }

    let oauth_path = agent_dir.join("oauth.json");
    let settings_path = agent_dir.join("settings.json");
    let mut migrated = Map::new();
    let mut providers = BTreeSet::new();
    let mut parsed_oauth = false;
    let mut oauth_has_unmigrated_entries = false;

    if oauth_path.exists() {
        match fs::read_to_string(&oauth_path) {
            Ok(content) => match serde_json::from_str::<Value>(&content) {
                Ok(Value::Object(entries)) => {
                    parsed_oauth = true;
                    for (provider, credential) in entries {
                        if let Value::Object(mut object) = credential {
                            object.insert("type".to_string(), Value::String("oauth".to_string()));
                            migrated.insert(provider.clone(), Value::Object(object));
                            providers.insert(provider);
                        } else {
                            oauth_has_unmigrated_entries = true;
                            warnings.push(format!(
                                "oauth.json entry for provider {provider} is not an object; leaving oauth.json in place"
                            ));
                        }
                    }
                }
                Ok(_) => warnings
                    .push("oauth.json is not an object; skipping OAuth migration".to_string()),
                Err(err) => warnings.push(format!(
                    "could not parse oauth.json; skipping OAuth migration: {err}"
                )),
            },
            Err(err) => warnings.push(format!(
                "could not read oauth.json; skipping OAuth migration: {err}"
            )),
        }
    }

    if settings_path.exists() {
        match fs::read_to_string(&settings_path) {
            Ok(content) => match serde_json::from_str::<Value>(&content) {
                Ok(mut settings_value) => {
                    if let Some(api_keys) = settings_value
                        .get("apiKeys")
                        .and_then(Value::as_object)
                        .cloned()
                    {
                        let mut remaining_api_keys = Map::new();
                        let mut settings_changed = false;
                        for (provider, key_value) in api_keys {
                            let Some(key) = key_value.as_str() else {
                                warnings.push(format!(
                                        "settings.json apiKeys.{provider} is not a string; leaving it in place"
                                    ));
                                remaining_api_keys.insert(provider, key_value);
                                continue;
                            };
                            settings_changed = true;
                            if migrated.contains_key(&provider) {
                                continue;
                            }
                            migrated.insert(
                                provider.clone(),
                                serde_json::json!({
                                    "type": "api_key",
                                    "key": key,
                                }),
                            );
                            providers.insert(provider);
                        }
                        if settings_changed {
                            if let Value::Object(settings_obj) = &mut settings_value {
                                if remaining_api_keys.is_empty() {
                                    settings_obj.remove("apiKeys");
                                } else {
                                    settings_obj.insert(
                                        "apiKeys".to_string(),
                                        Value::Object(remaining_api_keys),
                                    );
                                }
                            }
                            match serde_json::to_string_pretty(&settings_value) {
                                    Ok(updated) => {
                                        let tmp = settings_path.with_extension("json.tmp");
                                        let mut opts = fs::OpenOptions::new();
                                        opts.write(true).create(true).truncate(true);
                                        #[cfg(unix)]
                                        {
                                            use std::os::unix::fs::OpenOptionsExt;
                                            opts.mode(0o600);
                                        }
                                        let res = opts.open(&tmp).and_then(|mut f| {
                                            use std::io::Write;
                                            f.write_all(updated.as_bytes())?;
                                            f.sync_all()
                                        }).and_then(|()| fs::rename(&tmp, &settings_path));

                                        if let Err(err) = res {
                                            warnings.push(format!(
                                                "could not persist settings.json after apiKeys migration: {err}"
                                            ));
                                        }
                                    }
                                    Err(err) => warnings.push(format!(
                                        "could not serialize settings.json after apiKeys migration: {err}"
                                    )),
                                }
                        }
                    }
                }
                Err(err) => warnings.push(format!(
                    "could not parse settings.json for apiKeys migration: {err}"
                )),
            },
            Err(err) => warnings.push(format!(
                "could not read settings.json for apiKeys migration: {err}"
            )),
        }
    }

    let mut auth_persisted = migrated.is_empty();
    if !migrated.is_empty() {
        if let Err(err) = fs::create_dir_all(agent_dir) {
            warnings.push(format!(
                "could not create agent dir for auth.json migration: {err}"
            ));
            return providers.into_iter().collect();
        }

        match serde_json::to_string_pretty(&Value::Object(migrated)) {
            Ok(contents) => {
                let tmp = auth_path.with_extension("json.tmp");
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create(true).truncate(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }

                let res = options
                    .open(&tmp)
                    .and_then(|mut f| {
                        use std::io::Write;
                        f.write_all(contents.as_bytes())?;
                        f.sync_all()
                    })
                    .and_then(|()| fs::rename(&tmp, &auth_path));

                if let Err(err) = res {
                    warnings.push(format!("could not write auth.json during migration: {err}"));
                } else if let Err(err) = set_owner_only_permissions(&auth_path) {
                    warnings.push(format!("could not set auth.json permissions to 600: {err}"));
                } else {
                    auth_persisted = true;
                }
            }
            Err(err) => warnings.push(format!("could not serialize migrated auth.json: {err}")),
        }
    }

    if parsed_oauth && !oauth_has_unmigrated_entries && auth_persisted && oauth_path.exists() {
        let migrated_path = oauth_path.with_extension("json.migrated");
        if let Err(err) = fs::rename(&oauth_path, migrated_path) {
            warnings.push(format!(
                "could not rename oauth.json after migration: {err}"
            ));
        }
    }

    providers.into_iter().collect()
}

fn migrate_sessions_from_agent_root(agent_dir: &Path, warnings: &mut Vec<String>) -> usize {
    let Ok(read_dir) = fs::read_dir(agent_dir) else {
        return 0;
    };

    let mut migrated_count = 0usize;

    for entry in read_dir.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let source_path = entry.path();
        if source_path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }

        let Some(cwd) = session_cwd_from_header(&source_path) else {
            continue;
        };
        let encoded = encode_cwd(Path::new(&cwd));
        let target_dir = agent_dir.join("sessions").join(encoded);
        if let Err(err) = fs::create_dir_all(&target_dir) {
            warnings.push(format!(
                "could not create session migration target dir {}: {err}",
                target_dir.display()
            ));
            continue;
        }
        let Some(file_name) = source_path.file_name() else {
            continue;
        };
        let target_path = target_dir.join(file_name);
        if target_path.exists() {
            continue;
        }
        if let Err(err) = fs::rename(&source_path, &target_path) {
            warnings.push(format!(
                "could not migrate session file {} to {}: {err}",
                source_path.display(),
                target_path.display()
            ));
            continue;
        }
        migrated_count += 1;
    }

    migrated_count
}

fn session_cwd_from_header(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let header: Value = serde_json::from_str(line.trim()).ok()?;
    if header.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    header
        .get("cwd")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn migrate_commands_to_prompts(base_dir: &Path, warnings: &mut Vec<String>) -> bool {
    let commands_dir = base_dir.join("commands");
    let prompts_dir = base_dir.join("prompts");
    if !commands_dir.exists() || prompts_dir.exists() {
        return false;
    }

    match fs::rename(&commands_dir, &prompts_dir) {
        Ok(()) => true,
        Err(err) => {
            warnings.push(format!(
                "could not migrate commands/ to prompts/ in {}: {err}",
                base_dir.display()
            ));
            false
        }
    }
}

fn migrate_tools_to_bin(agent_dir: &Path, warnings: &mut Vec<String>) -> Vec<String> {
    let tools_dir = agent_dir.join("tools");
    if !tools_dir.exists() {
        return Vec::new();
    }
    let bin_dir = agent_dir.join("bin");
    let mut moved = Vec::new();

    for binary in MANAGED_TOOL_BINARIES {
        let old_path = tools_dir.join(binary);
        if !old_path.exists() {
            continue;
        }

        if let Err(err) = fs::create_dir_all(&bin_dir) {
            warnings.push(format!("could not create bin/ directory: {err}"));
            break;
        }

        let new_path = bin_dir.join(binary);
        if new_path.exists() {
            if let Err(err) = fs::remove_file(&old_path) {
                warnings.push(format!(
                    "could not remove legacy managed binary {} after migration: {err}",
                    old_path.display()
                ));
            }
            continue;
        }

        match fs::rename(&old_path, &new_path) {
            Ok(()) => moved.push((*binary).to_string()),
            Err(err) => warnings.push(format!(
                "could not move managed binary {} to {}: {err}",
                old_path.display(),
                new_path.display()
            )),
        }
    }

    moved
}

fn check_deprecated_extension_dirs(base_dir: &Path, label: &str) -> Vec<String> {
    let mut warnings = Vec::new();

    let hooks_dir = base_dir.join("hooks");
    if hooks_dir.exists() {
        warnings.push(format!(
            "{label} hooks/ directory found. Hooks have been renamed to extensions/"
        ));
    }

    let tools_dir = base_dir.join("tools");
    if tools_dir.exists() {
        match fs::read_dir(&tools_dir) {
            Ok(entries) => {
                let custom_entries = entries
                    .flatten()
                    .filter(|entry| {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name.starts_with('.') {
                            return false;
                        }
                        !MANAGED_TOOL_BINARIES.iter().any(|managed| *managed == name)
                    })
                    .count();
                if custom_entries > 0 {
                    warnings.push(format!(
                        "{label} tools/ directory contains custom files. Custom tools should live under extensions/"
                    ));
                }
            }
            Err(err) => warnings.push(format!(
                "could not inspect deprecated tools/ directory at {}: {err}",
                tools_dir.display()
            )),
        }
    }

    warnings
}

fn set_owner_only_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::run_startup_migrations_with_agent_dir;
    use crate::session::encode_cwd;
    use serde_json::Value;
    use std::fs;
    use tempfile::TempDir;

    fn write(path: &std::path::Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(path, content).expect("write fixture file");
    }

    #[test]
    fn migrate_auth_from_oauth_and_settings_api_keys() {
        let temp = TempDir::new().expect("tempdir");
        let agent_dir = temp.path().join("agent");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::create_dir_all(&cwd).expect("create cwd");

        write(
            &agent_dir.join("oauth.json"),
            r#"{"anthropic":{"access_token":"a","refresh_token":"r","expires":1}}"#,
        );
        write(
            &agent_dir.join("settings.json"),
            r#"{"apiKeys":{"openai":"sk-openai","anthropic":"ignored"},"theme":"dark"}"#,
        );

        let report = run_startup_migrations_with_agent_dir(&agent_dir, &cwd);
        assert_eq!(
            report.migrated_auth_providers,
            vec!["anthropic".to_string(), "openai".to_string()]
        );

        let auth_value: Value = serde_json::from_str(
            &fs::read_to_string(agent_dir.join("auth.json")).expect("read auth"),
        )
        .expect("parse auth");
        assert_eq!(auth_value["anthropic"]["type"], "oauth");
        assert_eq!(auth_value["openai"]["type"], "api_key");
        assert_eq!(auth_value["openai"]["key"], "sk-openai");

        let settings_value: Value = serde_json::from_str(
            &fs::read_to_string(agent_dir.join("settings.json")).expect("read settings"),
        )
        .expect("parse settings");
        assert!(settings_value.get("apiKeys").is_none());
        assert!(agent_dir.join("oauth.json.migrated").exists());
    }

    #[test]
    fn migrate_auth_preserves_malformed_oauth_entries() {
        let temp = TempDir::new().expect("tempdir");
        let agent_dir = temp.path().join("agent");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::create_dir_all(&cwd).expect("create cwd");

        write(
            &agent_dir.join("oauth.json"),
            r#"{"anthropic":{"access_token":"a","refresh_token":"r","expires":1},"broken":"oops"}"#,
        );

        let report = run_startup_migrations_with_agent_dir(&agent_dir, &cwd);
        assert_eq!(
            report.migrated_auth_providers,
            vec!["anthropic".to_string()]
        );
        assert!(report.warnings.iter().any(|warning| {
            warning.contains("oauth.json entry for provider broken is not an object")
        }));

        let auth_value: Value = serde_json::from_str(
            &fs::read_to_string(agent_dir.join("auth.json")).expect("read auth"),
        )
        .expect("parse auth");
        assert_eq!(auth_value["anthropic"]["type"], "oauth");
        assert!(agent_dir.join("oauth.json").exists());
        assert!(!agent_dir.join("oauth.json.migrated").exists());
    }

    #[test]
    fn migrate_auth_preserves_invalid_settings_api_keys() {
        let temp = TempDir::new().expect("tempdir");
        let agent_dir = temp.path().join("agent");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::create_dir_all(&cwd).expect("create cwd");

        write(
            &agent_dir.join("settings.json"),
            r#"{"apiKeys":{"openai":"sk-openai","broken":123},"theme":"dark"}"#,
        );

        let report = run_startup_migrations_with_agent_dir(&agent_dir, &cwd);
        assert_eq!(report.migrated_auth_providers, vec!["openai".to_string()]);
        assert!(
            report.warnings.iter().any(|warning| {
                warning.contains("settings.json apiKeys.broken is not a string")
            })
        );

        let auth_value: Value = serde_json::from_str(
            &fs::read_to_string(agent_dir.join("auth.json")).expect("read auth"),
        )
        .expect("parse auth");
        assert_eq!(auth_value["openai"]["type"], "api_key");
        assert_eq!(auth_value["openai"]["key"], "sk-openai");

        let settings_value: Value = serde_json::from_str(
            &fs::read_to_string(agent_dir.join("settings.json")).expect("read settings"),
        )
        .expect("parse settings");
        assert_eq!(settings_value["theme"], "dark");
        assert_eq!(settings_value["apiKeys"]["broken"], 123);
        assert!(settings_value["apiKeys"].get("openai").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn migrate_auth_sets_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("tempdir");
        let agent_dir = temp.path().join("agent");
        let cwd = temp.path().join("project");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::create_dir_all(&cwd).expect("create cwd");

        write(
            &agent_dir.join("settings.json"),
            r#"{"apiKeys":{"openai":"sk-test"}}"#,
        );

        let _report = run_startup_migrations_with_agent_dir(&agent_dir, &cwd);

        let auth_path = agent_dir.join("auth.json");
        assert!(auth_path.exists(), "auth.json should be created");
        let mode = fs::metadata(&auth_path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "auth.json should have 0o600 permissions, got {mode:#o}"
        );
    }

    #[test]
    fn migrate_sessions_from_agent_root_to_encoded_project_dir() {
        let temp = TempDir::new().expect("tempdir");
        let agent_dir = temp.path().join("agent");
        let cwd = temp.path().join("workspace");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::create_dir_all(&cwd).expect("create cwd");

        write(
            &agent_dir.join("legacy-session.jsonl"),
            &format!(
                "{{\"type\":\"session\",\"cwd\":\"{}\",\"id\":\"abc\"}}\n{{\"type\":\"message\"}}\n",
                cwd.display()
            ),
        );

        let report = run_startup_migrations_with_agent_dir(&agent_dir, &cwd);
        assert_eq!(report.migrated_session_files, 1);

        let expected = agent_dir
            .join("sessions")
            .join(encode_cwd(&cwd))
            .join("legacy-session.jsonl");
        assert!(expected.exists());
        assert!(!agent_dir.join("legacy-session.jsonl").exists());
    }

    #[test]
    fn migrate_commands_and_managed_tools() {
        let temp = TempDir::new().expect("tempdir");
        let agent_dir = temp.path().join("agent");
        let cwd = temp.path().join("workspace");
        let project_dir = cwd.join(".pi");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::create_dir_all(&project_dir).expect("create project dir");

        write(&agent_dir.join("commands/global.md"), "# global");
        write(&project_dir.join("commands/project.md"), "# project");
        write(&agent_dir.join("tools/fd"), "fd-binary");
        write(&agent_dir.join("tools/rg"), "rg-binary");

        let report = run_startup_migrations_with_agent_dir(&agent_dir, &cwd);

        assert!(agent_dir.join("prompts/global.md").exists());
        assert!(project_dir.join("prompts/project.md").exists());
        assert!(agent_dir.join("bin/fd").exists());
        assert!(agent_dir.join("bin/rg").exists());
        assert!(!agent_dir.join("tools/fd").exists());
        assert!(!agent_dir.join("tools/rg").exists());
        assert_eq!(report.migrated_tool_binaries.len(), 2);
        assert_eq!(report.migrated_commands_dirs.len(), 2);
    }

    #[test]
    fn managed_tool_cleanup_when_target_exists() {
        let temp = TempDir::new().expect("tempdir");
        let agent_dir = temp.path().join("agent");
        let cwd = temp.path().join("workspace");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::create_dir_all(&cwd).expect("create cwd");

        write(&agent_dir.join("tools/fd"), "legacy-fd");
        write(&agent_dir.join("bin/fd"), "existing-fd");

        let report = run_startup_migrations_with_agent_dir(&agent_dir, &cwd);
        assert!(report.migrated_tool_binaries.is_empty());
        assert!(!agent_dir.join("tools/fd").exists());
        assert_eq!(
            fs::read_to_string(agent_dir.join("bin/fd")).expect("read existing bin/fd"),
            "existing-fd"
        );
    }

    #[test]
    fn warns_for_deprecated_hooks_and_custom_tools() {
        let temp = TempDir::new().expect("tempdir");
        let agent_dir = temp.path().join("agent");
        let cwd = temp.path().join("workspace");
        let project_dir = cwd.join(".pi");
        fs::create_dir_all(agent_dir.join("hooks")).expect("create global hooks");
        fs::create_dir_all(project_dir.join("hooks")).expect("create project hooks");
        write(&agent_dir.join("tools/custom.sh"), "#!/bin/sh\necho hi\n");

        let report = run_startup_migrations_with_agent_dir(&agent_dir, &cwd);
        assert!(!report.deprecation_warnings.is_empty());
        assert!(
            report
                .messages()
                .iter()
                .any(|line| line.contains("Migration guide: "))
        );
    }

    #[test]
    fn migration_is_idempotent() {
        let temp = TempDir::new().expect("tempdir");
        let agent_dir = temp.path().join("agent");
        let cwd = temp.path().join("workspace");
        fs::create_dir_all(&agent_dir).expect("create agent dir");
        fs::create_dir_all(&cwd).expect("create cwd");

        write(
            &agent_dir.join("oauth.json"),
            r#"{"anthropic":{"access_token":"a","refresh_token":"r","expires":1}}"#,
        );
        write(
            &agent_dir.join("legacy.jsonl"),
            &format!("{{\"type\":\"session\",\"cwd\":\"{}\"}}\n", cwd.display()),
        );
        write(&agent_dir.join("commands/hello.md"), "# hello");
        write(&agent_dir.join("tools/fd"), "fd-binary");

        let first = run_startup_migrations_with_agent_dir(&agent_dir, &cwd);
        assert!(!first.migrated_auth_providers.is_empty());
        assert!(first.migrated_session_files > 0);

        let second = run_startup_migrations_with_agent_dir(&agent_dir, &cwd);
        assert!(second.migrated_auth_providers.is_empty());
        assert_eq!(second.migrated_session_files, 0);
        assert!(second.migrated_commands_dirs.is_empty());
        assert!(second.migrated_tool_binaries.is_empty());
    }

    #[test]
    fn empty_layout_is_noop() {
        let temp = TempDir::new().expect("tempdir");
        let agent_dir = temp.path().join("agent");
        let cwd = temp.path().join("workspace");
        fs::create_dir_all(&cwd).expect("create cwd");

        let report = run_startup_migrations_with_agent_dir(&agent_dir, &cwd);
        assert!(report.migrated_auth_providers.is_empty());
        assert_eq!(report.migrated_session_files, 0);
        assert!(report.migrated_commands_dirs.is_empty());
        assert!(report.migrated_tool_binaries.is_empty());
        assert!(report.deprecation_warnings.is_empty());
        assert!(report.warnings.is_empty());
    }

    mod proptest_migrations {
        use crate::migrations::{MigrationReport, session_cwd_from_header};
        use proptest::prelude::*;

        proptest! {
            /// Empty `MigrationReport` produces empty messages.
            #[test]
            fn empty_report_no_messages(_dummy in 0..1u8) {
                let report = MigrationReport::default();
                assert!(report.messages().is_empty());
            }

            /// Auth provider migration message includes all provider names.
            #[test]
            fn messages_include_providers(
                p1 in "[a-z]{3,8}",
                p2 in "[a-z]{3,8}"
            ) {
                let report = MigrationReport {
                    migrated_auth_providers: vec![p1.clone(), p2.clone()],
                    ..Default::default()
                };
                let msgs = report.messages();
                assert_eq!(msgs.len(), 1);
                assert!(msgs[0].contains(&p1));
                assert!(msgs[0].contains(&p2));
            }

            /// Session migration message includes count.
            #[test]
            fn messages_include_session_count(count in 1..100usize) {
                let report = MigrationReport {
                    migrated_session_files: count,
                    ..Default::default()
                };
                let msgs = report.messages();
                assert_eq!(msgs.len(), 1);
                assert!(msgs[0].contains(&count.to_string()));
            }

            /// Warnings are prefixed with "Warning: ".
            #[test]
            fn messages_prefix_warnings(warning in "[a-z ]{5,20}") {
                let report = MigrationReport {
                    warnings: vec![warning.clone()],
                    ..Default::default()
                };
                let msgs = report.messages();
                assert_eq!(msgs.len(), 1);
                assert!(msgs[0].starts_with("Warning: "));
                assert!(msgs[0].contains(&warning));
            }

            /// Deprecation warnings add guide/docs URLs.
            #[test]
            fn messages_deprecation_adds_urls(warning in "[a-z ]{5,20}") {
                let report = MigrationReport {
                    deprecation_warnings: vec![warning],
                    ..Default::default()
                };
                let msgs = report.messages();
                // warning + guide URL + docs URL
                assert_eq!(msgs.len(), 3);
                assert!(msgs[1].contains("Migration guide:"));
                assert!(msgs[2].contains("Extensions docs:"));
            }

            /// `session_cwd_from_header` extracts cwd from valid session header.
            #[test]
            fn session_cwd_extraction(cwd in "[/a-z]{3,20}") {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("test.jsonl");
                let header = serde_json::json!({
                    "type": "session",
                    "cwd": cwd,
                    "id": "test"
                });
                std::fs::write(&path, serde_json::to_string(&header).unwrap()).unwrap();
                assert_eq!(session_cwd_from_header(&path), Some(cwd));
            }

            /// `session_cwd_from_header` returns None for wrong type.
            #[test]
            fn session_cwd_wrong_type(type_val in "[a-z]{3,10}") {
                prop_assume!(type_val != "session");
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("test.jsonl");
                let header = serde_json::json!({
                    "type": type_val,
                    "cwd": "/test"
                });
                std::fs::write(&path, serde_json::to_string(&header).unwrap()).unwrap();
                assert_eq!(session_cwd_from_header(&path), None);
            }

            /// `session_cwd_from_header` returns None for empty file.
            #[test]
            fn session_cwd_empty_file(_dummy in 0..1u8) {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("empty.jsonl");
                std::fs::write(&path, "").unwrap();
                assert_eq!(session_cwd_from_header(&path), None);
            }

            /// `session_cwd_from_header` returns None for invalid JSON.
            #[test]
            fn session_cwd_invalid_json(s in "[a-z]{5,20}") {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("bad.jsonl");
                std::fs::write(&path, &s).unwrap();
                assert_eq!(session_cwd_from_header(&path), None);
            }

            /// Message count equals sum of non-empty field contributions.
            #[test]
            fn messages_count_additive(
                n_providers in 0..3usize,
                sessions in 0..5usize,
                n_warnings in 0..3usize,
                n_deprecations in 0..3usize
            ) {
                let report = MigrationReport {
                    migrated_auth_providers: (0..n_providers).map(|i| format!("p{i}")).collect(),
                    migrated_session_files: sessions,
                    migrated_commands_dirs: Vec::new(),
                    migrated_tool_binaries: Vec::new(),
                    warnings: (0..n_warnings).map(|i| format!("w{i}")).collect(),
                    deprecation_warnings: (0..n_deprecations).map(|i| format!("d{i}")).collect(),
                };
                let msgs = report.messages();
                let mut expected = 0;
                if n_providers > 0 { expected += 1; }
                if sessions > 0 { expected += 1; }
                expected += n_warnings;
                expected += n_deprecations;
                if n_deprecations > 0 { expected += 2; } // guide + docs URLs
                assert_eq!(msgs.len(), expected);
            }
        }
    }
}
