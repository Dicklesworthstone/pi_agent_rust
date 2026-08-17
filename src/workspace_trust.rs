//! Workspace trust: a trust-on-first-use gate for project-local configuration
//! that can execute code (GitHub #151).
//!
//! A repository can select Git/npm packages via `.pi/settings.json` (whose npm
//! lifecycle scripts run as the local user during install) and can auto-load
//! JavaScript from `.pi/extensions/` (which may call `pi.exec` during session
//! startup). Neither is model-generated content — it is deterministic
//! configuration execution controlled by whoever authored the repository.
//!
//! This module decides, before any of that resolution runs, whether the
//! current workspace is trusted:
//!
//! - The decision is keyed to the canonical workspace path **and** a content
//!   digest over the project-controlled surfaces (`.pi/settings.json` plus
//!   every file under `.pi/extensions/`). Any content change re-prompts.
//! - Interactive launches prompt once and persist the answer (trusted or
//!   untrusted) in `<global_dir>/workspace-trust.json`.
//! - Non-interactive launches (RPC, `--print`, piped stdin) fail closed:
//!   project-local configuration is skipped for that run with a warning, and
//!   nothing is persisted, so a later interactive launch still prompts.
//! - `--trust` (or `trustAllWorkspaces` in the *global* settings, or the
//!   `PI_WORKSPACE_TRUST` env var) covers automation.
//!
//! Explicit CLI resource paths (`-e/--extension`, `--skill`, …) are user
//! consent and are deliberately not gated here.

use crate::config::Config;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// File name of the trust store inside the global configuration directory.
pub const TRUST_STORE_FILE: &str = "workspace-trust.json";

/// Environment override for automation: `trusted` (or `1`) forces trust for
/// this run, `untrusted` (or `0`) forces the fail-closed path. Neither value
/// is persisted.
pub const TRUST_ENV_VAR: &str = "PI_WORKSPACE_TRUST";

const TRUST_STORE_VERSION: u32 = 1;

/// A recorded (or requested) trust decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustDecision {
    Trusted,
    Untrusted,
}

/// What the workspace declares that could execute local code.
#[derive(Debug, Clone)]
pub struct WorkspaceTrustSurface {
    /// True when `.pi/settings.json` exists.
    pub has_project_settings: bool,
    /// Number of `packages` entries declared in `.pi/settings.json`.
    pub package_count: usize,
    /// Files under `.pi/extensions/`, relative to the workspace root, sorted.
    pub extension_entries: Vec<String>,
    /// Hex sha256 over the canonical surface manifest.
    pub digest: String,
}

impl WorkspaceTrustSurface {
    /// Scan `cwd` for project-controlled executable surfaces.
    ///
    /// Returns `Ok(None)` when the workspace declares nothing to trust
    /// (no `.pi/settings.json` and no `.pi/extensions/` entries).
    pub fn scan(cwd: &Path) -> Result<Option<Self>> {
        let project_dir = cwd.join(Config::project_dir());
        let settings_path = project_dir.join("settings.json");
        let extensions_dir = project_dir.join("extensions");

        let settings_bytes = match std::fs::read(&settings_path) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(Error::config(format!(
                    "Failed to read {}: {err}",
                    settings_path.display()
                )));
            }
        };

        let mut extension_files = Vec::new();
        collect_files_recursive(&extensions_dir, &extensions_dir, &mut extension_files)?;
        extension_files.sort_by(|a, b| a.relative.cmp(&b.relative));

        if settings_bytes.is_none() && extension_files.is_empty() {
            return Ok(None);
        }

        let package_count = settings_bytes
            .as_deref()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok())
            .and_then(|value| {
                value
                    .get("packages")
                    .and_then(|packages| packages.as_array().map(Vec::len))
            })
            .unwrap_or(0);

        // Canonical manifest: one line per surface file, sorted, tab-separated
        // relative path and content sha256. The digest is over the manifest.
        let mut manifest = String::new();
        if let Some(bytes) = &settings_bytes {
            manifest.push_str(".pi/settings.json\t");
            manifest.push_str(&crate::package_manager::hex_encode(&Sha256::digest(bytes)));
            manifest.push('\n');
        }
        let mut extension_entries = Vec::with_capacity(extension_files.len());
        for found in &extension_files {
            let bytes = std::fs::read(&found.absolute).map_err(|err| {
                Error::config(format!(
                    "Failed to read {}: {err}",
                    found.absolute.display()
                ))
            })?;
            let display = format!(".pi/extensions/{}", found.relative.display());
            manifest.push_str(&display);
            manifest.push('\t');
            manifest.push_str(&crate::package_manager::hex_encode(&Sha256::digest(&bytes)));
            manifest.push('\n');
            extension_entries.push(display);
        }

        Ok(Some(Self {
            has_project_settings: settings_bytes.is_some(),
            package_count,
            extension_entries,
            digest: crate::package_manager::hex_encode(&Sha256::digest(manifest.as_bytes())),
        }))
    }
}

/// A surface file discovered under `.pi/extensions/`: the absolute path comes
/// straight from directory traversal (never re-derived by joining), and the
/// relative path is only used for display and manifest keys.
struct FoundSurfaceFile {
    relative: PathBuf,
    absolute: PathBuf,
}

fn collect_files_recursive(root: &Path, dir: &Path, out: &mut Vec<FoundSurfaceFile>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(Error::config(format!(
                "Failed to scan {}: {err}",
                dir.display()
            )));
        }
    };
    for entry in entries {
        let entry = entry
            .map_err(|err| Error::config(format!("Failed to scan {}: {err}", dir.display())))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| Error::config(format!("Failed to inspect {}: {err}", path.display())))?;
        if file_type.is_dir() {
            collect_files_recursive(root, &path, out)?;
        } else if file_type.is_file() || file_type.is_symlink() {
            // Symlinked entries hash their target content via fs::read later;
            // a broken symlink surfaces as a read error, which fails closed.
            if let Ok(relative) = path.strip_prefix(root) {
                let relative = relative.to_path_buf();
                out.push(FoundSurfaceFile {
                    relative,
                    absolute: path,
                });
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrustRecord {
    digest: String,
    decision: TrustDecision,
    #[serde(default)]
    updated_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustStoreFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    workspaces: BTreeMap<String, TrustRecord>,
}

/// Persistent map of workspace path -> (surface digest, decision).
#[derive(Debug)]
pub struct WorkspaceTrustStore {
    path: PathBuf,
    data: TrustStoreFile,
}

impl WorkspaceTrustStore {
    /// Default store location inside the global configuration directory.
    #[must_use]
    pub fn default_path() -> PathBuf {
        Config::global_dir().join(TRUST_STORE_FILE)
    }

    /// Load the store, tolerating a missing or corrupt file (corrupt files
    /// are treated as empty so a damaged store can never grant trust).
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let data = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<TrustStoreFile>(&bytes).ok())
            .unwrap_or_default();
        Self {
            path: path.to_path_buf(),
            data,
        }
    }

    /// The stored decision for `workspace`, if its digest still matches.
    #[must_use]
    pub fn decision(&self, workspace: &str, digest: &str) -> Option<TrustDecision> {
        self.data
            .workspaces
            .get(workspace)
            .filter(|record| record.digest == digest)
            .map(|record| record.decision)
    }

    /// Record a decision for `workspace` at `digest` and save the store.
    pub fn record(&mut self, workspace: &str, digest: &str, decision: TrustDecision) -> Result<()> {
        self.data.version = TRUST_STORE_VERSION;
        self.data.workspaces.insert(
            workspace.to_string(),
            TrustRecord {
                digest: digest.to_string(),
                decision,
                updated_at: chrono::Utc::now().to_rfc3339(),
            },
        );
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                Error::config(format!("Failed to create {}: {err}", parent.display()))
            })?;
        }
        let json = serde_json::to_string_pretty(&self.data)
            .map_err(|err| Error::config(format!("Failed to encode trust store: {err}")))?;
        std::fs::write(&self.path, json).map_err(|err| {
            Error::config(format!("Failed to write {}: {err}", self.path.display()))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

/// How the effective trust decision was reached (for logs and warnings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustSource {
    /// The workspace declares nothing that could execute code.
    NoSurface,
    /// `--trust` on the command line (persisted).
    CliFlag,
    /// `trustAllWorkspaces` in the global settings (not persisted).
    TrustAllConfig,
    /// `PI_WORKSPACE_TRUST` environment override (not persisted).
    EnvOverride,
    /// A stored decision whose digest still matches.
    Store,
    /// The interactive first-use prompt (persisted).
    Prompt,
    /// Non-interactive launch with no stored decision: fail closed.
    NonInteractive,
}

/// The effective trust state for this launch.
#[derive(Debug)]
pub struct WorkspaceTrustState {
    pub trusted: bool,
    pub source: TrustSource,
    pub surface: Option<WorkspaceTrustSurface>,
}

/// Inputs to [`establish`] that the caller resolves from CLI/config/env.
#[derive(Debug, Clone)]
pub struct TrustInputs {
    /// `--trust` was passed on the command line.
    pub cli_trust: bool,
    /// `trustAllWorkspaces` from the *global* settings file.
    pub trust_all_workspaces: bool,
    /// Value of [`TRUST_ENV_VAR`], if set.
    pub env_override: Option<String>,
    /// Whether an interactive first-use prompt may be shown.
    pub interactive: bool,
}

/// Canonical store key for a workspace path.
#[must_use]
pub fn workspace_key(cwd: &Path) -> String {
    std::fs::canonicalize(cwd)
        .unwrap_or_else(|_| cwd.to_path_buf())
        .display()
        .to_string()
}

/// Decide whether the workspace at `cwd` is trusted, prompting via `prompt`
/// when allowed and necessary. `prompt` returns `Ok(true)` to trust.
pub fn establish(
    cwd: &Path,
    store_path: &Path,
    inputs: &TrustInputs,
    prompt: impl FnOnce(&WorkspaceTrustSurface) -> Result<bool>,
) -> Result<WorkspaceTrustState> {
    let Some(surface) = WorkspaceTrustSurface::scan(cwd)? else {
        return Ok(WorkspaceTrustState {
            trusted: true,
            source: TrustSource::NoSurface,
            surface: None,
        });
    };

    let key = workspace_key(cwd);
    let mut store = WorkspaceTrustStore::load(store_path);

    if let Some(value) = inputs.env_override.as_deref() {
        let trusted = match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "trusted" => true,
            "0" | "false" | "untrusted" => false,
            other => {
                return Err(Error::config(format!(
                    "Invalid {TRUST_ENV_VAR} value '{other}': expected trusted or untrusted"
                )));
            }
        };
        return Ok(WorkspaceTrustState {
            trusted,
            source: TrustSource::EnvOverride,
            surface: Some(surface),
        });
    }

    if inputs.cli_trust {
        store.record(&key, &surface.digest, TrustDecision::Trusted)?;
        return Ok(WorkspaceTrustState {
            trusted: true,
            source: TrustSource::CliFlag,
            surface: Some(surface),
        });
    }

    if inputs.trust_all_workspaces {
        return Ok(WorkspaceTrustState {
            trusted: true,
            source: TrustSource::TrustAllConfig,
            surface: Some(surface),
        });
    }

    match store.decision(&key, &surface.digest) {
        Some(TrustDecision::Trusted) => Ok(WorkspaceTrustState {
            trusted: true,
            source: TrustSource::Store,
            surface: Some(surface),
        }),
        Some(TrustDecision::Untrusted) => Ok(WorkspaceTrustState {
            trusted: false,
            source: TrustSource::Store,
            surface: Some(surface),
        }),
        None if inputs.interactive => {
            let granted = prompt(&surface)?;
            let decision = if granted {
                TrustDecision::Trusted
            } else {
                TrustDecision::Untrusted
            };
            store.record(&key, &surface.digest, decision)?;
            Ok(WorkspaceTrustState {
                trusted: granted,
                source: TrustSource::Prompt,
                surface: Some(surface),
            })
        }
        None => Ok(WorkspaceTrustState {
            trusted: false,
            source: TrustSource::NonInteractive,
            surface: Some(surface),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, content).expect("write fixture");
    }

    fn no_prompt(_: &WorkspaceTrustSurface) -> Result<bool> {
        // Surfaces as an `establish` Err, failing the test's expect().
        Err(Error::config("prompt must not run in this scenario"))
    }

    fn inputs() -> TrustInputs {
        TrustInputs {
            cli_trust: false,
            trust_all_workspaces: false,
            env_override: None,
            interactive: false,
        }
    }

    #[test]
    fn scan_returns_none_without_project_surfaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            WorkspaceTrustSurface::scan(dir.path())
                .expect("scan")
                .is_none()
        );
        // An unrelated .pi file (e.g. session artifacts) still yields None.
        write(&dir.path().join(".pi/notes.txt"), "not a surface");
        assert!(
            WorkspaceTrustSurface::scan(dir.path())
                .expect("scan")
                .is_none()
        );
    }

    #[test]
    fn scan_digest_is_stable_and_tracks_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &dir.path().join(".pi/settings.json"),
            r#"{"packages":["npm:left-pad"],"theme":"dark"}"#,
        );
        write(&dir.path().join(".pi/extensions/hook.js"), "export {}\n");
        write(
            &dir.path().join(".pi/extensions/nested/util.ts"),
            "export const x = 1\n",
        );

        let first = WorkspaceTrustSurface::scan(dir.path())
            .expect("scan")
            .expect("surface");
        let second = WorkspaceTrustSurface::scan(dir.path())
            .expect("scan")
            .expect("surface");
        assert_eq!(first.digest, second.digest, "digest must be deterministic");
        assert!(first.has_project_settings);
        assert_eq!(first.package_count, 1);
        assert_eq!(
            first.extension_entries,
            vec![
                ".pi/extensions/hook.js".to_string(),
                ".pi/extensions/nested/util.ts".to_string(),
            ]
        );

        // Settings edit changes the digest.
        write(
            &dir.path().join(".pi/settings.json"),
            r#"{"packages":["npm:left-pad","npm:evil"],"theme":"dark"}"#,
        );
        let settings_changed = WorkspaceTrustSurface::scan(dir.path())
            .expect("scan")
            .expect("surface");
        assert_ne!(first.digest, settings_changed.digest);
        assert_eq!(settings_changed.package_count, 2);

        // Extension content edit changes the digest.
        write(
            &dir.path().join(".pi/extensions/hook.js"),
            "export const changed = true\n",
        );
        let extension_changed = WorkspaceTrustSurface::scan(dir.path())
            .expect("scan")
            .expect("surface");
        assert_ne!(settings_changed.digest, extension_changed.digest);

        // A new extension file changes the digest.
        write(&dir.path().join(".pi/extensions/new.js"), "export {}\n");
        let extension_added = WorkspaceTrustSurface::scan(dir.path())
            .expect("scan")
            .expect("surface");
        assert_ne!(extension_changed.digest, extension_added.digest);
    }

    #[test]
    fn store_roundtrip_and_digest_mismatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store_path = dir.path().join("workspace-trust.json");

        let mut store = WorkspaceTrustStore::load(&store_path);
        assert!(store.decision("/ws", "d1").is_none());
        store
            .record("/ws", "d1", TrustDecision::Trusted)
            .expect("record");

        let reloaded = WorkspaceTrustStore::load(&store_path);
        assert_eq!(reloaded.decision("/ws", "d1"), Some(TrustDecision::Trusted));
        assert_eq!(
            reloaded.decision("/ws", "d2"),
            None,
            "a digest change must invalidate the stored decision"
        );
        assert_eq!(reloaded.decision("/other", "d1"), None);
    }

    #[test]
    fn corrupt_store_is_treated_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store_path = dir.path().join("workspace-trust.json");
        write(&store_path, "{not json");
        let store = WorkspaceTrustStore::load(&store_path);
        assert!(store.decision("/ws", "d1").is_none());
    }

    fn seeded_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &dir.path().join(".pi/settings.json"),
            r#"{"packages":["npm:left-pad"]}"#,
        );
        write(&dir.path().join(".pi/extensions/hook.js"), "export {}\n");
        dir
    }

    #[test]
    fn establish_trivially_trusts_workspaces_without_surfaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store_path = dir.path().join("store.json");
        let state = establish(dir.path(), &store_path, &inputs(), no_prompt).expect("establish");
        assert!(state.trusted);
        assert_eq!(state.source, TrustSource::NoSurface);
        assert!(!store_path.exists(), "no-surface runs must not persist");
    }

    #[test]
    fn establish_cli_flag_trusts_and_persists() {
        let dir = seeded_workspace();
        let store_path = dir.path().join("store.json");
        let state = establish(
            dir.path(),
            &store_path,
            &TrustInputs {
                cli_trust: true,
                ..inputs()
            },
            no_prompt,
        )
        .expect("establish");
        assert!(state.trusted);
        assert_eq!(state.source, TrustSource::CliFlag);

        // A later plain run reuses the stored decision.
        let followup = establish(dir.path(), &store_path, &inputs(), no_prompt).expect("establish");
        assert!(followup.trusted);
        assert_eq!(followup.source, TrustSource::Store);
    }

    #[test]
    fn establish_trust_all_config_trusts_without_persisting() {
        let dir = seeded_workspace();
        let store_path = dir.path().join("store.json");
        let state = establish(
            dir.path(),
            &store_path,
            &TrustInputs {
                trust_all_workspaces: true,
                ..inputs()
            },
            no_prompt,
        )
        .expect("establish");
        assert!(state.trusted);
        assert_eq!(state.source, TrustSource::TrustAllConfig);
        assert!(!store_path.exists());
    }

    #[test]
    fn establish_env_override_wins_without_persisting() {
        let dir = seeded_workspace();
        let store_path = dir.path().join("store.json");
        for (value, expected) in [("trusted", true), ("0", false)] {
            let state = establish(
                dir.path(),
                &store_path,
                &TrustInputs {
                    env_override: Some(value.to_string()),
                    // Env must win even over --trust.
                    cli_trust: true,
                    ..inputs()
                },
                no_prompt,
            )
            .expect("establish");
            assert_eq!(state.trusted, expected, "env value {value}");
            assert_eq!(state.source, TrustSource::EnvOverride);
        }
        assert!(!store_path.exists());

        let err = establish(
            dir.path(),
            &store_path,
            &TrustInputs {
                env_override: Some("maybe".to_string()),
                ..inputs()
            },
            no_prompt,
        )
        .expect_err("invalid env value must fail");
        assert!(err.to_string().contains(TRUST_ENV_VAR));
    }

    #[test]
    fn establish_prompt_answers_persist_both_ways() {
        for (answer, expected) in [(true, true), (false, false)] {
            let dir = seeded_workspace();
            let store_path = dir.path().join("store.json");
            let state = establish(
                dir.path(),
                &store_path,
                &TrustInputs {
                    interactive: true,
                    ..inputs()
                },
                |_| Ok(answer),
            )
            .expect("establish");
            assert_eq!(state.trusted, expected);
            assert_eq!(state.source, TrustSource::Prompt);

            // The decision is remembered; the prompt must not run again.
            let followup =
                establish(dir.path(), &store_path, &inputs(), no_prompt).expect("establish");
            assert_eq!(followup.trusted, expected);
            assert_eq!(followup.source, TrustSource::Store);
        }
    }

    #[test]
    fn establish_reprompts_after_content_change() {
        let dir = seeded_workspace();
        let store_path = dir.path().join("store.json");
        establish(
            dir.path(),
            &store_path,
            &TrustInputs {
                interactive: true,
                ..inputs()
            },
            |_| Ok(true),
        )
        .expect("establish");

        write(
            &dir.path().join(".pi/extensions/hook.js"),
            "export const changed = 1\n",
        );
        let state = establish(
            dir.path(),
            &store_path,
            &TrustInputs {
                interactive: true,
                ..inputs()
            },
            |_| Ok(false),
        )
        .expect("establish");
        assert!(!state.trusted, "digest change must invalidate stored trust");
        assert_eq!(state.source, TrustSource::Prompt);
    }

    #[test]
    fn establish_non_interactive_fails_closed_without_persisting() {
        let dir = seeded_workspace();
        let store_path = dir.path().join("store.json");
        let state = establish(dir.path(), &store_path, &inputs(), no_prompt).expect("establish");
        assert!(!state.trusted);
        assert_eq!(state.source, TrustSource::NonInteractive);
        assert!(
            !store_path.exists(),
            "non-interactive denial must stay ephemeral so a later interactive run prompts"
        );
    }
}
