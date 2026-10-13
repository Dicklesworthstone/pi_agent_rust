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
    /// Number of legacy session files moved or safely copied into `sessions/<encoded-cwd>/`.
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
    let mut oauth_snapshot = None;
    let mut settings_update = None;

    if oauth_path.exists() {
        match fs::read_to_string(&oauth_path) {
            Ok(content) => match serde_json::from_str::<Value>(&content) {
                Ok(Value::Object(entries)) => {
                    parsed_oauth = true;
                    oauth_snapshot = Some(content);
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
                                Ok(updated) => settings_update = Some((content, updated)),
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

    AuthMigration {
        credentials: migrated,
        providers,
        settings_update,
        oauth_snapshot: if parsed_oauth && !oauth_has_unmigrated_entries {
            oauth_snapshot
        } else {
            None
        },
    }
    .commit(agent_dir, warnings)
}

/// Preparation is read-only. A durable, private, no-clobber auth publication
/// must precede any cleanup of either legacy source. An unsuccessful cleanup
/// leaves duplicate credentials, never the only copy of a credential.
struct AuthMigration {
    credentials: Map<String, Value>,
    providers: BTreeSet<String>,
    settings_update: Option<(String, String)>,
    oauth_snapshot: Option<String>,
}

impl AuthMigration {
    fn commit(self, agent_dir: &Path, warnings: &mut Vec<String>) -> Vec<String> {
        if !self.credentials.is_empty() {
            let contents = match serde_json::to_string_pretty(&self.credentials) {
                Ok(contents) => contents,
                Err(err) => {
                    warnings.push(format!("could not serialize migrated auth.json: {err}"));
                    return Vec::new();
                }
            };
            if let Err(err) =
                publish_private_migration_file(&agent_dir.join("auth.json"), &contents)
            {
                warnings.push(format!(
                    "could not write auth.json during migration; legacy credentials retained: {err}"
                ));
                return Vec::new();
            }
        }

        if let Some((original, updated)) = self.settings_update {
            let path = agent_dir.join("settings.json");
            if let Err(err) = replace_unchanged_migration_file(&path, &original, &updated) {
                warnings.push(format!(
                    "could not persist settings.json after apiKeys migration (auth.json is already saved): {err}"
                ));
            }
        }
        if let Some(original) = self.oauth_snapshot {
            let path = agent_dir.join("oauth.json");
            let archived = path.with_extension("json.migrated");
            // Never replace an older recovery copy or archive a new credential
            // that appeared after preparation. Publication syncs the backup
            // before removing the original name.
            let result = require_unchanged_migration_file(&path, &original)
                .and_then(|()| publish_private_migration_file(&archived, &original))
                .and_then(|()| require_unchanged_migration_file(&path, &original))
                .and_then(|()| fs::remove_file(&path))
                .and_then(|()| sync_migration_directory(agent_dir));
            if let Err(err) = result {
                warnings.push(format!(
                    "could not archive oauth.json after migration: {err}"
                ));
            }
        }
        self.providers.into_iter().collect()
    }
}

fn stage_private_migration_file(
    path: &Path,
    content: &str,
) -> std::io::Result<tempfile::NamedTempFile> {
    use std::io::Write as _;

    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("missing migration parent"))?;
    let mut staged = tempfile::Builder::new()
        .prefix(".pi-migration-")
        .tempfile_in(parent)?;
    set_owner_only_permissions(staged.path())?;
    staged.write_all(content.as_bytes())?;
    staged.as_file().sync_all()?;
    Ok(staged)
}

fn publish_private_migration_file(path: &Path, content: &str) -> std::io::Result<()> {
    let staged = stage_private_migration_file(path, content)?;
    // A concurrent startup/login, existing directory, or dangling symlink
    // must not be overwritten, even if the earlier exists() check was false.
    drop(
        staged
            .persist_noclobber(path)
            .map_err(|error| error.error)?,
    );
    sync_migration_directory(path.parent().expect("staging checked parent"))
}

fn require_unchanged_migration_file(path: &Path, original: &str) -> std::io::Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_file() || fs::read_to_string(path)? != original {
        return Err(std::io::Error::other(
            "legacy source changed during migration; leaving it in place",
        ));
    }
    Ok(())
}

fn replace_unchanged_migration_file(
    path: &Path,
    original: &str,
    updated: &str,
) -> std::io::Result<()> {
    require_unchanged_migration_file(path, original)?;
    let staged = stage_private_migration_file(path, updated)?;
    require_unchanged_migration_file(path, original)?;
    drop(staged.persist(path).map_err(|error| error.error)?);
    sync_migration_directory(path.parent().expect("staging checked parent"))
}

// Directory durability is available through the standard library on Unix.
// Files themselves are synced on every platform before publication.
#[cfg_attr(
    not(unix),
    allow(clippy::unnecessary_wraps, clippy::missing_const_for_fn)
)]
fn sync_migration_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
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
        // Attachment references are relative to the complete JSONL filename.
        // Never move that filename away from its authoritative media directory.
        // A blob-backed migration publishes a validated copy and retains the
        // entire original bundle for recovery, including on publication failure.
        let sidecar = crate::session::attachments::sidecar_path(&source_path);
        let result = match fs::symlink_metadata(&sidecar) {
            Ok(_) => session_bundle::copy(&source_path, &target_path).map(|()| true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                fs::rename(&source_path, &target_path).map(|()| false)
            }
            Err(err) => Err(err),
        };
        match result {
            Ok(retained) => {
                migrated_count += 1;
                if retained {
                    warnings.push(format!(
                        "migrated media session {}; original transcript and attachments retained as a recovery copy at {}",
                        target_path.display(),
                        source_path.display()
                    ));
                }
            }
            Err(err) => warnings.push(format!(
                "could not migrate session file {} to {}: {err}",
                source_path.display(),
                target_path.display()
            )),
        }
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

// The Unix arm really can fail, so the fallible signature has to stay even
// though the `cfg(not(unix))` arm below cannot.
#[cfg_attr(
    not(unix),
    allow(clippy::unnecessary_wraps, clippy::missing_const_for_fn)
)]
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

    fn prepared_auth_migration(agent_dir: &std::path::Path) -> super::AuthMigration {
        let settings = r#"{"apiKeys":{"openai":"legacy-key"},"theme":"dark"}"#;
        let oauth = r#"{"anthropic":{"access_token":"legacy-token"}}"#;
        write(&agent_dir.join("settings.json"), settings);
        write(&agent_dir.join("oauth.json"), oauth);
        super::AuthMigration {
            credentials: serde_json::json!({
                "openai": {"type": "api_key", "key": "legacy-key"},
                "anthropic": {"type": "oauth", "access_token": "legacy-token"},
            })
            .as_object()
            .expect("object")
            .clone(),
            providers: std::collections::BTreeSet::from([
                "anthropic".to_string(),
                "openai".to_string(),
            ]),
            settings_update: Some((settings.to_string(), r#"{"theme":"dark"}"#.to_string())),
            oauth_snapshot: Some(oauth.to_string()),
        }
    }

    #[test]
    fn failed_auth_publication_keeps_both_legacy_sources_and_reports_no_migration() {
        let temp = TempDir::new().expect("tempdir");
        let agent_dir = temp.path();
        let migration = prepared_auth_migration(agent_dir);
        let settings = fs::read(agent_dir.join("settings.json")).expect("settings");
        let oauth = fs::read(agent_dir.join("oauth.json")).expect("oauth");
        // A real publication failure, after preparation, independent of uid.
        fs::create_dir(agent_dir.join("auth.json")).expect("block destination");
        let mut warnings = Vec::new();

        assert!(migration.commit(agent_dir, &mut warnings).is_empty());
        assert_eq!(fs::read(agent_dir.join("settings.json")).unwrap(), settings);
        assert_eq!(fs::read(agent_dir.join("oauth.json")).unwrap(), oauth);
        assert!(agent_dir.join("auth.json").is_dir());
        assert!(!agent_dir.join("oauth.json.migrated").exists());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("legacy credentials retained"))
        );
        assert_eq!(
            fs::read_dir(agent_dir).unwrap().count(),
            3,
            "no staging debris"
        );
    }

    #[test]
    fn concurrent_auth_publication_is_not_overwritten_or_used_to_authorize_cleanup() {
        let temp = TempDir::new().expect("tempdir");
        let migration = prepared_auth_migration(temp.path());
        let settings = fs::read(temp.path().join("settings.json")).unwrap();
        let oauth = fs::read(temp.path().join("oauth.json")).unwrap();
        let current = r#"{"openai":{"type":"api_key","key":"new-login"}}"#;
        write(&temp.path().join("auth.json"), current);
        let mut warnings = Vec::new();

        assert!(migration.commit(temp.path(), &mut warnings).is_empty());
        assert_eq!(
            fs::read_to_string(temp.path().join("auth.json")).unwrap(),
            current
        );
        assert_eq!(
            fs::read(temp.path().join("settings.json")).unwrap(),
            settings
        );
        assert_eq!(fs::read(temp.path().join("oauth.json")).unwrap(), oauth);
        assert!(!warnings.is_empty());
    }

    #[test]
    fn changed_legacy_sources_are_retained_after_auth_is_published() {
        let temp = TempDir::new().expect("tempdir");
        let migration = prepared_auth_migration(temp.path());
        let settings = r#"{"apiKeys":{"openai":"new-key"},"theme":"light"}"#;
        let oauth = r#"{"anthropic":{"access_token":"new-token"}}"#;
        write(&temp.path().join("settings.json"), settings);
        write(&temp.path().join("oauth.json"), oauth);
        let mut warnings = Vec::new();

        assert_eq!(migration.commit(temp.path(), &mut warnings).len(), 2);
        let auth: Value =
            serde_json::from_str(&fs::read_to_string(temp.path().join("auth.json")).unwrap())
                .unwrap();
        assert_eq!(auth["openai"]["key"], "legacy-key");
        assert_eq!(auth["anthropic"]["access_token"], "legacy-token");
        assert_eq!(
            fs::read_to_string(temp.path().join("settings.json")).unwrap(),
            settings
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("oauth.json")).unwrap(),
            oauth
        );
        assert!(!temp.path().join("oauth.json.migrated").exists());
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn existing_oauth_recovery_copy_is_never_replaced() {
        let temp = TempDir::new().expect("tempdir");
        let migration = prepared_auth_migration(temp.path());
        let original = fs::read(temp.path().join("oauth.json")).unwrap();
        write(
            &temp.path().join("oauth.json.migrated"),
            "old recovery copy",
        );
        let mut warnings = Vec::new();

        assert_eq!(migration.commit(temp.path(), &mut warnings).len(), 2);
        assert_eq!(fs::read(temp.path().join("oauth.json")).unwrap(), original);
        assert_eq!(
            fs::read_to_string(temp.path().join("oauth.json.migrated")).unwrap(),
            "old recovery copy"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("could not archive"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn auth_migration_refuses_dangling_destination_without_losing_legacy_keys() {
        let temp = TempDir::new().expect("tempdir");
        let _migration = prepared_auth_migration(temp.path());
        let settings = fs::read(temp.path().join("settings.json")).unwrap();
        let oauth = fs::read(temp.path().join("oauth.json")).unwrap();
        let target = temp.path().join("missing-auth");
        let auth_path = temp.path().join("auth.json");
        std::os::unix::fs::symlink(&target, &auth_path).expect("symlink");
        let mut warnings = Vec::new();

        assert!(super::migrate_auth_to_auth_json(temp.path(), &mut warnings).is_empty());
        assert_eq!(fs::read_link(&auth_path).unwrap(), target);
        assert_eq!(
            fs::read(temp.path().join("settings.json")).unwrap(),
            settings
        );
        assert_eq!(fs::read(temp.path().join("oauth.json")).unwrap(), oauth);
        assert!(!temp.path().join("oauth.json.migrated").exists());
        assert!(!warnings.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn auth_migration_never_opens_predictable_legacy_temporary_paths() {
        let temp = TempDir::new().expect("tempdir");
        let migration = prepared_auth_migration(temp.path());
        let victim = temp.path().join("unrelated");
        write(&victim, "do not truncate");
        for name in ["auth.json.tmp", "settings.json.tmp"] {
            std::os::unix::fs::symlink(&victim, temp.path().join(name)).expect("symlink");
        }
        let mut warnings = Vec::new();

        assert_eq!(migration.commit(temp.path(), &mut warnings).len(), 2);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(fs::read_to_string(victim).unwrap(), "do not truncate");
        assert!(temp.path().join("oauth.json.migrated").exists());
        assert!(!temp.path().join("oauth.json").exists());
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

        let escaped_cwd = cwd.to_string_lossy().replace('\\', "\\\\");
        write(
            &agent_dir.join("legacy-session.jsonl"),
            &format!(
                "{{\"type\":\"session\",\"cwd\":\"{escaped_cwd}\",\"id\":\"abc\"}}\n{{\"type\":\"message\"}}\n"
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
        let escaped_cwd = cwd.to_string_lossy().replace('\\', "\\\\");
        write(
            &agent_dir.join("legacy.jsonl"),
            &format!("{{\"type\":\"session\",\"cwd\":\"{escaped_cwd}\"}}\n"),
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
                // warning + guide URL + docs URLs
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

mod session_bundle {
    //! Copy blob-backed legacy sessions without splitting their authoritative data.
    //!
    //! The original transcript and attachment directory are deliberately retained.
    //! Two independent renames cannot atomically move a transcript and its blobs.
    //! Publish a complete destination snapshot instead, under both persistence
    //! locks, and leave the source usable for recovery even after an I/O failure.

    use crate::file_identity::FileIdentity;
    use crate::session::attachments::{decode_stored_entry, sidecar_path};
    use crate::session_store_v2::{self, ArtifactWriteMode};
    use sha2::{Digest as _, Sha256};
    use std::fs::{self, File};
    use std::io::{self, BufRead as _, BufReader, Read as _, Seek as _, SeekFrom, Write as _};
    use std::path::{Path, PathBuf};

    const MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;
    const COPY_BUFFER_BYTES: usize = 16 * 1024;
    // Match native session admission, without retaining the whole transcript.
    const MAX_JSONL_LINE_BYTES: usize = 100 * 1024 * 1024;

    fn migration_error(error: impl std::fmt::Display) -> io::Error {
        io::Error::other(error.to_string())
    }

    fn require_absent(path: &Path) -> io::Result<()> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "session migration destination is already occupied",
            )),
        }
    }

    fn persistence_lock(session: &Path) -> io::Result<File> {
        let mut path = session.as_os_str().to_os_string();
        path.push(".lock");
        let file = session_store_v2::open_regular_file_for_write(
            Path::new(&path),
            true,
            ArtifactWriteMode::Preserve,
        )
        .map_err(migration_error)?;
        // Startup must not block behind an active writer. This is the same
        // <session>.lock regular-file protocol used by JSONL persistence. A
        // directory lock, symlink or non-private lock file is refused by the opener.
        fs4::FileExt::try_lock(&file).map_err(migration_error)?;
        Ok(file)
    }

    struct PinnedDirectory {
        path: PathBuf,
        handle: File,
        identity: FileIdentity,
    }

    impl PinnedDirectory {
        fn open(path: PathBuf, create: bool) -> io::Result<Self> {
            let handle =
                session_store_v2::open_private_directory(&path, create).map_err(migration_error)?;
            let identity = FileIdentity::of_open_file(&handle)?;
            let directory = Self {
                path,
                handle,
                identity,
            };
            directory.validate()?;
            Ok(directory)
        }

        fn validate(&self) -> io::Result<()> {
            check_identity(&self.path, &self.handle, &self.identity)
        }

        fn sync(&self) -> io::Result<()> {
            self.validate()?;
            super::sync_migration_directory(&self.path)?;
            self.validate()
        }
    }

    fn check_identity(path: &Path, file: &File, identity: &FileIdentity) -> io::Result<()> {
        if &FileIdentity::of_open_file(file)? != identity
            || &FileIdentity::of_path_nofollow(path)? != identity
        {
            return Err(io::Error::other(
                "session migration source or directory changed",
            ));
        }
        Ok(())
    }

    fn open_source(path: &Path) -> io::Result<File> {
        session_store_v2::open_regular_file_for_read(path)
            .map_err(migration_error)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "session source is missing"))
    }

    /// Copy a verified content-addressed blob using a fixed-size buffer. Check
    /// the hash before publication; filename shape alone never authenticates data.
    fn copy_verified_blob(source: &Path, name: &str, target: &PinnedDirectory) -> io::Result<()> {
        let mut input = open_source(source)?;
        let identity = FileIdentity::of_open_file(&input)?;
        let length = input.metadata()?.len();
        if length == 0 || length > MAX_BLOB_BYTES {
            return Err(io::Error::other(
                "session attachment exceeds migration byte limits",
            ));
        }
        target.validate()?;
        let mut staged = tempfile::Builder::new()
            .prefix(".pi-migration-")
            .tempfile_in(&target.path)?;
        let mut hasher = Sha256::new();
        let mut count = 0_u64;
        let mut buffer = [0_u8; COPY_BUFFER_BYTES];
        loop {
            let read = match input.read(&mut buffer) {
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            if read == 0 {
                break;
            }
            count += read as u64;
            if count > length {
                return Err(io::Error::other("session attachment grew during migration"));
            }
            hasher.update(&buffer[..read]);
            staged.write_all(&buffer[..read])?;
        }
        if count != length || crate::package_manager::hex_encode(&hasher.finalize()) != name {
            return Err(io::Error::other(
                "session attachment digest or byte count does not match",
            ));
        }
        check_identity(source, &input, &identity)?;
        staged.as_file().sync_all()?;
        target.validate()?;
        let destination = target.path.join(name);
        match staged.persist_noclobber(&destination) {
            Ok(file) => drop(file),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                // A previous interrupted migration may have copied this digest.
                // Reuse only a private, regular, byte-identical file. Never truncate
                // an existing blob and never treat a partial copy as success.
                let mut existing = open_source(&destination)?;
                let existing_identity = FileIdentity::of_open_file(&existing)?;
                if existing.metadata()?.len() != length {
                    return Err(io::Error::other(
                        "existing migrated attachment has wrong size",
                    ));
                }
                let mut hasher = Sha256::new();
                let mut bounded = (&mut existing).take(length + 1);
                let mut count = 0_u64;
                loop {
                    let read = match bounded.read(&mut buffer) {
                        Ok(read) => read,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error),
                    };
                    if read == 0 {
                        break;
                    }
                    count += read as u64;
                    hasher.update(&buffer[..read]);
                }
                if count != length || crate::package_manager::hex_encode(&hasher.finalize()) != name {
                    return Err(io::Error::other("existing migrated attachment is corrupt"));
                }
                check_identity(&destination, &existing, &existing_identity)?;
                existing.sync_all()?;
            }
            Err(error) => return Err(error.error),
        }
        target.validate()
    }

    /// Apply native reference/schema validation against the destination before
    /// publishing its transcript. Copying the directory alone cannot establish
    /// that every reference exists, has the declared size, or uses valid encoding.
    /// Native validation streams blob hashes instead of hydrating base64 payloads.
    fn validate_transcript(file: &mut File, target: &Path) -> io::Result<()> {
        file.seek(SeekFrom::Start(0))?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut header = true;
        loop {
            line.clear();
            let read = (&mut reader)
                .take(MAX_JSONL_LINE_BYTES as u64 + 2)
                .read_line(&mut line)?;
            if read == 0 {
                break;
            }
            if line.strip_suffix('\n').unwrap_or(&line).len() > MAX_JSONL_LINE_BYTES {
                return Err(io::Error::other(
                    "session migration JSONL line exceeds byte limit",
                ));
            }
            if header {
                // Startup has already admitted the legacy header's cwd. Keep its
                // bytes/legacy version untouched instead of rewriting the format.
                let value: serde_json::Value =
                    serde_json::from_str(&line).map_err(migration_error)?;
                if value["type"] != "session" || !value["cwd"].is_string() {
                    return Err(io::Error::other("invalid legacy session header"));
                }
                header = false;
            } else if !line.trim().is_empty() {
                decode_stored_entry(Some(target), &line).map_err(migration_error)?;
            }
        }
        if header {
            return Err(io::Error::other("legacy session is empty"));
        }
        Ok(())
    }

    /// The destination transcript is the commit point. On failure no source data
    /// is removed; already-copied immutable blobs are safe to verify and reuse on
    /// a later attempt. Keep the original bundle even after success, rather than
    /// introducing a second, non-atomic cleanup transaction at startup.
    pub(super) fn copy(source: &Path, target: &Path) -> io::Result<()> {
        require_absent(target)?;
        let _source_lock = persistence_lock(source)?;
        let _target_lock = persistence_lock(target)?;
        require_absent(target)?;
        let mut input = open_source(source)?;
        let identity = FileIdentity::of_open_file(&input)?;
        let metadata = input.metadata()?;
        let length = metadata.len();
        let modified = metadata.modified()?;
        let source_blobs = PinnedDirectory::open(sidecar_path(source), false)?;
        let target_blobs = PinnedDirectory::open(sidecar_path(target), true)?;
        let parent = target
            .parent()
            .ok_or_else(|| io::Error::other("session destination has no parent"))?;
        let mut transcript = tempfile::Builder::new()
            .prefix(".pi-migration-")
            .tempfile_in(parent)?;
        let limit = length
            .checked_add(1)
            .ok_or_else(|| io::Error::other("session transcript is too large"))?;
        if io::copy(&mut (&mut input).take(limit), &mut transcript)? != length {
            return Err(io::Error::other(
                "session transcript changed during migration",
            ));
        }

        for entry in fs::read_dir(&source_blobs.path)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| io::Error::other("invalid session attachment filename"))?;
            // Unpublished persistence staging files cannot be referenced by JSONL.
            if name.starts_with(".pending-") {
                continue;
            }
            if name.len() != 64
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(io::Error::other("invalid session attachment digest filename"));
            }
            source_blobs.validate()?;
            copy_verified_blob(&entry.path(), name, &target_blobs)?;
        }
        source_blobs.validate()?;
        target_blobs.sync()?;
        // Make the newly created sidecar directory durable before publishing a
        // transcript that refers to it, including when it has no blobs yet.
        super::sync_migration_directory(parent)?;
        check_identity(source, &input, &identity)?;
        let current = input.metadata()?;
        if current.len() != length || current.modified()? != modified {
            return Err(io::Error::other(
                "session transcript changed during migration",
            ));
        }
        validate_transcript(transcript.as_file_mut(), target)?;
        transcript.as_file().sync_all()?;
        target_blobs.validate()?;
        drop(
            transcript
                .persist_noclobber(target)
                .map_err(|error| error.error)?,
        );
        super::sync_migration_directory(parent)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use base64::Engine as _;
        use serde_json::json;

        fn fixture(root: &Path) -> (PathBuf, PathBuf, String, Vec<u8>) {
            let source = root.join("legacy.jsonl");
            let target = root.join("sessions/workspace/legacy.jsonl");
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            let bytes = vec![0x61; 70 * 1024];
            let name = crate::package_manager::hex_encode(&Sha256::digest(&bytes));
            let directory = PinnedDirectory::open(sidecar_path(&source), true).unwrap();
            let mut blob = session_store_v2::open_regular_file_for_write(
                &directory.path.join(&name),
                true,
                ArtifactWriteMode::CreateNew,
            )
            .unwrap();
            blob.write_all(&bytes).unwrap();
            let mut session = session_store_v2::open_regular_file_for_write(
                &source,
                true,
                ArtifactWriteMode::CreateNew,
            )
            .unwrap();
            writeln!(
                session,
                "{}",
                json!({"type":"session","version":3,"id":"legacy","cwd":"/workspace"})
            )
            .unwrap();
            writeln!(
                session,
                "{}",
                json!({
                    "type":"message","id":"m1","parentId":null,
                    "timestamp":"2026-10-13T00:00:00.000Z",
                    "message":{"role":"user","timestamp":1,"content":[{
                        "type":"image","mimeType":"image/png",
                        "data":{"$piBlob":format!("sha256:{name}"),"sizeBytes":bytes.len(),"encoding":"base64"}
                    }]}
                })
            )
            .unwrap();
            (source, target, name, bytes)
        }

        #[test]
        fn copies_exact_transcript_and_media_and_retains_the_complete_source() {
            let temp = tempfile::tempdir().unwrap();
            let (source, target, name, bytes) = fixture(temp.path());
            let transcript = fs::read(&source).unwrap();
            copy(&source, &target).unwrap();
            assert_eq!(fs::read(&target).unwrap(), transcript);
            assert_eq!(fs::read(&source).unwrap(), transcript);
            assert_eq!(fs::read(sidecar_path(&target).join(&name)).unwrap(), bytes);
            assert_eq!(fs::read(sidecar_path(&source).join(&name)).unwrap(), bytes);
            let line = String::from_utf8(transcript).unwrap();
            let line = line.lines().nth(1).unwrap();
            // Exercise native hydration, not just the byte-copy helper.
            let entry = crate::session::attachments::decode_entry(Some(&target), line).unwrap();
            let value = serde_json::to_value(entry).unwrap();
            let encoded = value["message"]["content"][0]["data"].as_str().unwrap();
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .unwrap(),
                bytes
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                for path in [
                    &target,
                    &sidecar_path(&target),
                    &sidecar_path(&target).join(&name),
                ] {
                    assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
                }
            }
        }

        #[test]
        fn corrupt_source_never_publishes_a_transcript_or_removes_original_data() {
            let temp = tempfile::tempdir().unwrap();
            let (source, target, name, bytes) = fixture(temp.path());
            let blob = sidecar_path(&source).join(&name);
            fs::write(&blob, vec![0x62; bytes.len()]).unwrap();
            let transcript = fs::read(&source).unwrap();
            assert!(copy(&source, &target).is_err());
            assert!(!target.exists());
            assert_eq!(fs::read(&source).unwrap(), transcript);
            assert_eq!(fs::read(&blob).unwrap(), vec![0x62; bytes.len()]);
        }

        #[test]
        fn missing_or_malformed_references_never_publish_a_transcript() {
            for bad_size in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let (source, target, name, _) = fixture(temp.path());
                if bad_size {
                    let text = fs::read_to_string(&source).unwrap();
                    let mut lines = text.lines();
                    let header = lines.next().unwrap();
                    let mut entry: serde_json::Value =
                        serde_json::from_str(lines.next().unwrap()).unwrap();
                    entry["message"]["content"][0]["data"]["sizeBytes"] = json!(1);
                    fs::write(&source, format!("{header}\n{entry}\n")).unwrap();
                } else {
                    fs::remove_file(sidecar_path(&source).join(&name)).unwrap();
                }
                let transcript = fs::read(&source).unwrap();
                assert!(copy(&source, &target).is_err());
                assert!(!target.exists());
                assert_eq!(fs::read(&source).unwrap(), transcript);
            }
        }

        #[test]
        fn retries_reuse_only_verified_copies_and_never_replace_existing_blobs() {
            let temp = tempfile::tempdir().unwrap();
            let (source, target, name, bytes) = fixture(temp.path());
            let target_blobs = PinnedDirectory::open(sidecar_path(&target), true).unwrap();
            copy_verified_blob(&sidecar_path(&source).join(&name), &name, &target_blobs).unwrap();
            copy(&source, &target).unwrap();
            assert_eq!(fs::read(sidecar_path(&target).join(&name)).unwrap(), bytes);

            let temp = tempfile::tempdir().unwrap();
            let (source, target, name, bytes) = fixture(temp.path());
            let directory = PinnedDirectory::open(sidecar_path(&target), true).unwrap();
            let mut blob = session_store_v2::open_regular_file_for_write(
                &directory.path.join(&name),
                true,
                ArtifactWriteMode::CreateNew,
            )
            .unwrap();
            let corrupt = vec![0x62; bytes.len()];
            blob.write_all(&corrupt).unwrap();
            drop(blob);
            assert!(copy(&source, &target).is_err());
            assert!(!target.exists());
            assert_eq!(fs::read(directory.path.join(&name)).unwrap(), corrupt);
            assert_eq!(fs::read(sidecar_path(&source).join(&name)).unwrap(), bytes);
        }

        #[test]
        fn occupied_destination_and_native_persistence_locks_are_respected() {
            let temp = tempfile::tempdir().unwrap();
            let (source, target, _, _) = fixture(temp.path());
            // Hold the actual persistence guard, not our helper, to establish that
            // a running native session and startup migration mutually exclude.
            let guard = crate::session::lock_session_persistence(&source).unwrap();
            assert!(copy(&source, &target).is_err());
            assert!(!target.exists());
            drop(guard);
            let guard = crate::session::lock_session_persistence(&target).unwrap();
            assert!(copy(&source, &target).is_err());
            assert!(!target.exists());
            drop(guard);
            fs::write(&target, b"other session").unwrap();
            assert!(copy(&source, &target).is_err());
            assert_eq!(fs::read(&target).unwrap(), b"other session");
            assert!(source.exists());
        }

        #[cfg(unix)]
        #[test]
        fn symlinked_attachment_or_destination_is_never_followed() {
            let temp = tempfile::tempdir().unwrap();
            let (source, target, name, bytes) = fixture(temp.path());
            let original_blob = sidecar_path(&source).join(&name);
            let moved_blob = temp.path().join("outside");
            fs::rename(&original_blob, &moved_blob).unwrap();
            std::os::unix::fs::symlink(&moved_blob, &original_blob).unwrap();
            assert!(copy(&source, &target).is_err());
            assert!(!target.exists());
            assert_eq!(fs::read(&moved_blob).unwrap(), bytes);
            assert!(source.exists());

            let temp = tempfile::tempdir().unwrap();
            let (source, target, _, _) = fixture(temp.path());
            let missing = temp.path().join("missing");
            std::os::unix::fs::symlink(&missing, &target).unwrap();
            assert!(copy(&source, &target).is_err());
            assert_eq!(fs::read_link(&target).unwrap(), missing);
            assert!(!missing.exists());
            assert!(source.exists());
        }

        #[test]
        fn startup_migrates_blob_backed_sessions_once_and_retains_a_recovery_copy() {
            let temp = tempfile::tempdir().unwrap();
            let (source, _, name, bytes) = fixture(temp.path());
            let cwd = Path::new("/workspace");
            let target = temp
                .path()
                .join("sessions")
                .join(crate::session::encode_cwd(cwd))
                .join("legacy.jsonl");
            let original = fs::read(&source).unwrap();
            let first = super::super::run_startup_migrations_with_agent_dir(temp.path(), cwd);
            assert_eq!(first.migrated_session_files, 1, "{:?}", first.warnings);
            assert!(
                first
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("recovery copy"))
            );
            assert_eq!(fs::read(&target).unwrap(), original);
            assert_eq!(fs::read(&source).unwrap(), original);
            assert_eq!(fs::read(sidecar_path(&target).join(&name)).unwrap(), bytes);
            let second = super::super::run_startup_migrations_with_agent_dir(temp.path(), cwd);
            assert_eq!(second.migrated_session_files, 0);
            assert_eq!(fs::read(&target).unwrap(), original);
            assert_eq!(fs::read(sidecar_path(&source).join(&name)).unwrap(), bytes);
        }
    }
}
