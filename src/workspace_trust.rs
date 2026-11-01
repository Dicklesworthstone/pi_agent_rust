//! Workspace trust: a trust-on-first-use gate for project-local configuration
//! that can execute code (GitHub #151).
//!
//! A repository can select Git/npm packages via `.pi/settings.json` (whose npm
//! lifecycle scripts run as the local user during install) and can auto-load
//! JavaScript from `.pi/extensions/` (which may call `pi.exec` during session
//! startup). Project agent definitions can also select a child process's tool
//! allowlist, skills and prompt. These are repository-authored capabilities,
//! even when the model selects the profile by name.
//!
//! This module decides, before any of that resolution runs, whether the
//! current workspace is trusted:
//!
//! - The decision is keyed to the canonical workspace path **and** a content
//!   digest over the project-controlled surfaces (`.pi/settings.json`, every
//!   file under `.pi/extensions/`, project-local MCP configuration, and the
//!   nearest `.pi/agents/` profiles). Any content change re-prompts.
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
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

/// File name of the trust store inside the global configuration directory.
pub const TRUST_STORE_FILE: &str = "workspace-trust.json";

/// Environment override for automation: `trusted` (or `1`) forces trust for
/// this run, `untrusted` (or `0`) forces the fail-closed path. Neither value
/// is persisted.
pub const TRUST_ENV_VAR: &str = "PI_WORKSPACE_TRUST";

const TRUST_STORE_VERSION: u32 = 1;
const TRUST_SURFACE_DIGEST_DOMAIN: &[u8] = b"pi_agent_rust:workspace-trust-surface:v2";
const MAX_TRUST_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_TRUST_EXTENSION_BYTES: usize = 16 * 1024 * 1024;
// Match the bounded Markdown definition reader in subagents::definition.
const MAX_TRUST_AGENT_DEFINITIONS: usize = 256;
const MAX_TRUST_AGENT_BYTES: usize = 1024 * 1024;

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
    /// Escaped canonical workspace path suitable for an untrusted terminal
    /// prompt. The trust-store key retains the underlying canonical path.
    pub workspace_display: String,
    /// True when `.pi/settings.json` exists.
    pub has_project_settings: bool,
    /// Number of `packages` entries declared in `.pi/settings.json`.
    pub package_count: usize,
    /// Files under `.pi/extensions/`, relative to the workspace root, sorted.
    pub extension_entries: Vec<String>,
    /// Project-local MCP configuration files, relative to the workspace root,
    /// sorted. Explicit CLI and global MCP files are deliberate operator
    /// inputs and are not workspace trust surfaces.
    pub mcp_config_entries: Vec<String>,
    /// Markdown profiles in the nearest `.pi/agents/` directory. Descendant
    /// paths are relative to the workspace; inherited ancestor paths retain
    /// their full origin. Entries are sorted and escaped for display.
    pub agent_definition_entries: Vec<String>,
    /// Hex sha256 over the canonical surface manifest.
    pub digest: String,
}

impl WorkspaceTrustSurface {
    /// Scan `cwd` for project-controlled executable surfaces.
    ///
    /// Returns `Ok(None)` when the workspace declares nothing to trust
    /// (no `.pi/settings.json`, no `.pi/extensions/` entries, no project-local
    /// MCP configuration, and no effective project agent definitions).
    pub fn scan(cwd: &Path) -> Result<Option<Self>> {
        const PROJECT_MCP_CONFIGS: &[&str] = &[
            ".agents/mcp.json",
            ".claude/mcp.json",
            ".codex/config.toml",
            ".cursor/mcp.json",
            ".gemini/settings.json",
            ".pi/mcp.json",
            ".windsurf/mcp.json",
        ];

        let project_dir = cwd.join(Config::project_dir());
        let settings_path = project_dir.join("settings.json");
        let extensions_dir = project_dir.join("extensions");

        let settings_bytes = read_bounded_regular_file(&settings_path, MAX_TRUST_CONFIG_BYTES)?;

        let mut extension_files = Vec::new();
        collect_files_recursive(&extensions_dir, &extensions_dir, &mut extension_files)?;
        extension_files.sort_by(|a, b| a.relative.cmp(&b.relative));

        let mut mcp_config_files = Vec::new();
        for relative in PROJECT_MCP_CONFIGS {
            let absolute = cwd.join(relative);
            if let Some(bytes) = read_bounded_regular_file(&absolute, MAX_TRUST_CONFIG_BYTES)? {
                mcp_config_files.push((PathBuf::from(*relative), bytes));
            }
        }

        let agent_directory = nearest_project_agents_dir(cwd);
        let agent_files = agent_directory
            .as_deref()
            .map(collect_agent_definition_files)
            .transpose()?
            .unwrap_or_default();

        if settings_bytes.is_none()
            && extension_files.is_empty()
            && mcp_config_files.is_empty()
            && agent_files.is_empty()
        {
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

        // Versioned, domain-separated records use raw path bytes and explicit
        // lengths, so control characters or non-UTF-8 names cannot make two
        // different surface sets hash to the same manifest.
        let mut surface_hasher = Sha256::new();
        surface_hasher.update(TRUST_SURFACE_DIGEST_DOMAIN);
        if let Some(bytes) = &settings_bytes {
            hash_surface_record(
                &mut surface_hasher,
                b"settings",
                Path::new(".pi/settings.json"),
                bytes,
            );
        }
        let mut extension_entries = Vec::with_capacity(extension_files.len());
        for found in &extension_files {
            let bytes = read_bounded_regular_file(&found.absolute, MAX_TRUST_EXTENSION_BYTES)?
                .ok_or_else(|| {
                    Error::config(format!(
                        "Trust surface disappeared while scanning {}",
                        escaped_path(&found.absolute)
                    ))
                })?;
            let relative = Path::new(".pi/extensions").join(&found.relative);
            hash_surface_record(&mut surface_hasher, b"extension", &relative, &bytes);
            extension_entries.push(escaped_path(&relative));
        }
        let mut mcp_config_entries = Vec::with_capacity(mcp_config_files.len());
        for (relative, bytes) in mcp_config_files {
            hash_surface_record(&mut surface_hasher, b"mcp", &relative, &bytes);
            mcp_config_entries.push(escaped_path(&relative));
        }
        let agent_definition_entries = hash_agent_definitions(
            &mut surface_hasher,
            cwd,
            agent_directory.as_deref(),
            &agent_files,
        )?;

        Ok(Some(Self {
            workspace_display: escaped_path(
                &std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()),
            ),
            has_project_settings: settings_bytes.is_some(),
            package_count,
            extension_entries,
            mcp_config_entries,
            agent_definition_entries,
            digest: crate::package_manager::hex_encode(&surface_hasher.finalize()),
        }))
    }
}

/// Select the same nearest authored directory for trust admission and loading.
/// An empty nearer directory intentionally shadows an ancestor's definitions.
pub(crate) fn nearest_project_agents_dir(cwd: &Path) -> Option<PathBuf> {
    let mut current = std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    loop {
        let candidate = current.join(Config::project_dir()).join("agents");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn collect_agent_definition_files(directory: &Path) -> Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir(directory).map_err(|err| {
        Error::config(format!(
            "Failed to scan agent directory {}: {err}",
            escaped_path(directory)
        ))
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|err| Error::config(format!("Failed to enumerate agent definitions: {err}")))?
            .path();
        if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        {
            if paths.len() >= MAX_TRUST_AGENT_DEFINITIONS {
                return Err(Error::config(
                    "PI_AGENT_DEFINITION_LIMIT: too many agent definitions in one directory",
                ));
            }
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn hash_agent_definitions(
    hasher: &mut Sha256,
    cwd: &Path,
    directory: Option<&Path>,
    files: &[PathBuf],
) -> Result<Vec<String>> {
    let mut entries = Vec::with_capacity(files.len());
    if !files.is_empty()
        && let Some(directory) = directory
    {
        // Both the selected logical directory and its resolved origin matter:
        // identical profiles at another root can select different relative skills.
        let canonical = std::fs::canonicalize(directory).map_err(|err| {
            Error::config(format!(
                "Failed to resolve agent directory {}: {err}",
                escaped_path(directory)
            ))
        })?;
        hash_surface_record(
            hasher,
            b"agent-directory",
            directory,
            canonical.as_os_str().as_encoded_bytes(),
        );
    }
    for path in files {
        let bytes = read_bounded_regular_file(path, MAX_TRUST_AGENT_BYTES)?.ok_or_else(|| {
            Error::config(format!(
                "Agent definition disappeared while scanning {}",
                escaped_path(path)
            ))
        })?;
        hash_surface_record(hasher, b"agent-definition", path, &bytes);
        entries.push(escaped_path(path.strip_prefix(cwd).unwrap_or(path)));
    }
    Ok(entries)
}

fn read_bounded_regular_file(path: &Path, max_bytes: usize) -> Result<Option<Vec<u8>>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        // A missing path and a regular file where a directory was expected
        // (`.codex` as a file makes `.codex/config.toml` ENOTDIR) both mean
        // the surface does not exist; neither is a reason to refuse startup.
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(err) => {
            return Err(Error::config(format!(
                "Failed to inspect {}: {err}",
                escaped_path(path)
            )));
        }
    };
    if !metadata.is_file() {
        return Err(Error::config(format!(
            "Trust surface is not a regular file: {}",
            escaped_path(path)
        )));
    }
    if metadata.len() > max_bytes as u64 {
        return Err(Error::config(format!(
            "Trust surface exceeds {max_bytes} bytes: {}",
            escaped_path(path)
        )));
    }

    let file = File::open(path)
        .map_err(|err| Error::config(format!("Failed to read {}: {err}", escaped_path(path))))?;
    // Capacity hint only: the value is clamped to `max_bytes`, and a wrapped
    // cast on a 32-bit target could at worst under-reserve.
    #[allow(clippy::cast_possible_truncation)]
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(max_bytes));
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| Error::config(format!("Failed to read {}: {err}", escaped_path(path))))?;
    if bytes.len() > max_bytes {
        return Err(Error::config(format!(
            "Trust surface exceeds {max_bytes} bytes: {}",
            escaped_path(path)
        )));
    }
    Ok(Some(bytes))
}

fn hash_surface_record(hasher: &mut Sha256, kind: &[u8], path: &Path, bytes: &[u8]) {
    hash_length_prefixed(hasher, kind);
    hash_length_prefixed(hasher, path.as_os_str().as_encoded_bytes());
    hash_length_prefixed(hasher, bytes);
}

fn hash_length_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn escaped_path(path: &Path) -> String {
    path.as_os_str()
        .to_string_lossy()
        .chars()
        .flat_map(char::escape_default)
        .collect()
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

/// Launch trust retained by CLI-owned sessions that reuse resolved resources.
///
/// A rebuilt session must not treat previously discovered project extensions
/// as newly authorized explicit paths. Keep the original decision and surface
/// digest, then recheck policy before any configuration or resource reload.
/// Programmatic SDK callers can omit this guard and keep their explicit trust
/// decision.
#[derive(Debug, Clone)]
pub struct WorkspaceTrustGuard {
    origin: PathBuf,
    origin_key: String,
    trusted: bool,
    source: TrustSource,
    surface_digest: Option<String>,
    global_dir: PathBuf,
    config_override: Option<PathBuf>,
    env_override: Option<String>,
}

impl WorkspaceTrustGuard {
    /// Capture the decision that actually admitted the launch resources.
    /// This does not scan again, grant trust, or retain `--trust` for replay.
    #[must_use]
    pub fn from_established(
        cwd: &Path,
        state: &WorkspaceTrustState,
        inputs: &TrustInputs,
        global_dir: &Path,
        config_override: Option<&Path>,
    ) -> Self {
        let origin = std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let global_dir =
            std::path::absolute(global_dir).unwrap_or_else(|_| global_dir.to_path_buf());
        let config_override = config_override.map(|path| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                origin.join(path)
            }
        });
        Self {
            origin_key: workspace_key(&origin),
            origin,
            trusted: state.trusted,
            source: state.source,
            surface_digest: state.surface.as_ref().map(|surface| surface.digest.clone()),
            global_dir,
            config_override,
            env_override: inputs.env_override.clone(),
        }
    }

    /// The original decision; revalidation never promotes a denied launch.
    #[must_use]
    pub const fn trusted(&self) -> bool {
        self.trusted
    }

    #[must_use]
    pub(crate) fn global_dir(&self) -> &Path {
        &self.global_dir
    }

    #[must_use]
    pub(crate) fn config_override(&self) -> Option<&Path> {
        self.config_override.as_deref()
    }

    /// Verify that a replacement still uses the admitted launch workspace.
    ///
    /// # Errors
    /// Returns an actionable restart error if the workspace or trust changed.
    pub fn validate_workspace(&self, cwd: &Path) -> Result<()> {
        if workspace_key(cwd) != self.origin_key {
            return Err(self
                .restart_required("the requested workspace differs from the launch workspace"));
        }
        self.validate_origin()
    }

    /// Revalidate the origin of cached project configuration and resources.
    ///
    /// # Errors
    /// Returns an actionable restart error if the original grant is no longer
    /// valid, its scoped surface changed, or current policy cannot be read.
    pub fn validate_origin(&self) -> Result<()> {
        if workspace_key(&self.origin) != self.origin_key {
            return Err(
                self.restart_required("the launch workspace path now resolves elsewhere"),
            );
        }
        if !self.trusted {
            return Ok(());
        }

        let global = Config::load_with_roots_and_project_trust(
            self.config_override.as_deref(),
            &self.global_dir,
            &self.origin,
            false,
        )
        .map_err(|error| {
            self.restart_required(&format!("cannot reload operator settings: {error}"))
        })?;
        let inputs = TrustInputs {
            cli_trust: false,
            trust_all_workspaces: global.trust_all_workspaces.unwrap_or(false),
            env_override: self.env_override.clone(),
            interactive: false,
        };
        let current = establish(
            &self.origin,
            &self.global_dir.join(TRUST_STORE_FILE),
            &inputs,
            |_| Ok(false),
        )
        .map_err(|error| {
            self.restart_required(&format!("cannot revalidate workspace trust: {error}"))
        })?;
        if !current.trusted {
            return Err(self.restart_required("workspace trust is no longer granted"));
        }

        // `establish` returns NoSurface before consulting universal policy.
        // A captured trusted environment override cannot change in this guard;
        // global policy is read above on every validation.
        let universal = matches!(
            current.source,
            TrustSource::EnvOverride | TrustSource::TrustAllConfig
        ) || (current.source == TrustSource::NoSurface
            && (self.source == TrustSource::EnvOverride
                || (inputs.env_override.is_none() && inputs.trust_all_workspaces)));
        if !universal
            && current.surface.as_ref().map(|surface| surface.digest.as_str())
                != self.surface_digest.as_deref()
        {
            return Err(self.restart_required(
                "the project configuration or executable resources changed",
            ));
        }
        Ok(())
    }

    fn restart_required(&self, reason: &str) -> Error {
        Error::config(format!(
            "PI_WORKSPACE_TRUST_RESTART_REQUIRED: {reason} for {}; restart Pi and re-establish workspace trust before rebuilding the session or reloading resources",
            escaped_path(&self.origin)
        ))
    }
}

#[cfg(unix)]
fn encoded_workspace_key(canonical: &Path) -> String {
    // The tag is part of the persistence contract. Changing it deliberately
    // invalidates older decisions and causes a safe re-prompt instead of
    // interpreting path bytes with a different platform/toolchain encoding.
    use std::os::unix::ffi::OsStrExt as _;

    format!(
        "path-v3:unix:{}",
        crate::package_manager::hex_encode(canonical.as_os_str().as_bytes())
    )
}

#[cfg(windows)]
fn encoded_workspace_key(canonical: &Path) -> String {
    use std::os::windows::ffi::OsStrExt as _;

    let bytes = canonical
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    format!(
        "path-v3:windows-utf16le:{}",
        crate::package_manager::hex_encode(&bytes)
    )
}

#[cfg(not(any(unix, windows)))]
fn encoded_workspace_key(canonical: &Path) -> String {
    format!(
        "path-v3:platform-encoded:{}",
        crate::package_manager::hex_encode(canonical.as_os_str().as_encoded_bytes())
    )
}

/// Canonical store key for a workspace path.
#[must_use]
pub fn workspace_key(cwd: &Path) -> String {
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    encoded_workspace_key(&canonical)
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
    use clap::Parser;

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

    fn guard_for(cwd: &Path, global_dir: &Path, trust_inputs: &TrustInputs) -> WorkspaceTrustGuard {
        let state = establish(
            cwd,
            &global_dir.join(TRUST_STORE_FILE),
            trust_inputs,
            no_prompt,
        )
        .expect("establish launch trust");
        WorkspaceTrustGuard::from_established(cwd, &state, trust_inputs, global_dir, None)
    }

    fn assert_restart_required(result: Result<()>) {
        let error = result.expect_err("changed launch authority must fail closed");
        assert!(
            error
                .to_string()
                .contains("PI_WORKSPACE_TRUST_RESTART_REQUIRED"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn reload_guard_keeps_the_admitted_digest_and_never_replays_cli_trust() {
        let workspace = tempfile::tempdir().expect("workspace");
        let global = tempfile::tempdir().expect("global");
        let settings = workspace.path().join(".pi/settings.json");
        write(
            &settings,
            r#"{"tools":{"enable_eval":false,"enable_github":false,"enable_security_scan":false,"enable_subagent":false}}"#,
        );
        let trust_inputs = TrustInputs {
            cli_trust: true,
            ..inputs()
        };
        let store_path = global.path().join(TRUST_STORE_FILE);
        let admitted = establish(workspace.path(), &store_path, &trust_inputs, no_prompt)
            .expect("admit original settings");
        let guard = WorkspaceTrustGuard::from_established(
            workspace.path(),
            &admitted,
            &trust_inputs,
            global.path(),
            None,
        );
        assert!(guard.trusted());
        guard
            .validate_workspace(workspace.path())
            .expect("unchanged workspace");

        write(
            &settings,
            r#"{"tools":{"enable_eval":true,"enable_github":true,"enable_security_scan":true,"enable_subagent":true}}"#,
        );
        // Capturing after a mutation must still retain the digest from the
        // actual admission, rather than scanning and granting the new bytes.
        let late_guard = WorkspaceTrustGuard::from_established(
            workspace.path(),
            &admitted,
            &trust_inputs,
            global.path(),
            None,
        );
        assert_restart_required(guard.validate_origin());
        assert_restart_required(late_guard.validate_origin());

        // A separate process can approve the new surface, but this session's
        // cached resources still require resolution in a fresh launch.
        let readmitted = establish(workspace.path(), &store_path, &trust_inputs, no_prompt)
            .expect("admit changed settings separately");
        assert_restart_required(guard.validate_origin());
        let fresh_guard = WorkspaceTrustGuard::from_established(
            workspace.path(),
            &readmitted,
            &trust_inputs,
            global.path(),
            None,
        );
        fresh_guard
            .validate_origin()
            .expect("freshly resolved launch");

        let other = tempfile::tempdir().expect("other workspace");
        write(&other.path().join(".pi/settings.json"), r#"{}"#);
        assert_restart_required(fresh_guard.validate_workspace(other.path()));
    }

    #[test]
    fn reload_guard_rejects_mutated_extensions_and_project_mcp() {
        for (relative, before, after) in [
            (
                ".pi/extensions/hook.ts",
                "export const value = 1;\n",
                "export const value = 2;\n",
            ),
            (
                ".agents/mcp.json",
                r#"{"mcpServers":{}}"#,
                r#"{"mcpServers":{"new":{"command":"new-server"}}}"#,
            ),
        ] {
            let workspace = tempfile::tempdir().expect("workspace");
            let global = tempfile::tempdir().expect("global");
            let path = workspace.path().join(relative);
            write(&path, before);
            let guard = guard_for(
                workspace.path(),
                global.path(),
                &TrustInputs {
                    cli_trust: true,
                    ..inputs()
                },
            );
            guard.validate_origin().expect("unchanged surface");
            write(&path, after);
            assert_restart_required(guard.validate_origin());
        }
    }

    #[test]
    fn reload_guard_rejects_new_surface_after_empty_workspace_admission() {
        let workspace = tempfile::tempdir().expect("workspace");
        let global = tempfile::tempdir().expect("global");
        let guard = guard_for(workspace.path(), global.path(), &inputs());
        assert!(guard.trusted());
        guard.validate_origin().expect("still empty");
        write(
            &workspace.path().join(".pi/settings.json"),
            r#"{"tools":{"enable_eval":true,"enable_github":true,"enable_security_scan":true,"enable_subagent":true}}"#,
        );
        assert_restart_required(guard.validate_origin());
    }

    #[test]
    fn reload_guard_honors_current_universal_policy_including_empty_surfaces() {
        for via_env in [false, true] {
            for initially_empty in [false, true] {
                let workspace = tempfile::tempdir().expect("workspace");
                let global = tempfile::tempdir().expect("global");
                let settings = workspace.path().join(".pi/settings.json");
                if !initially_empty {
                    write(&settings, r#"{}"#);
                }
                let trust_inputs = if via_env {
                    TrustInputs {
                        env_override: Some(" TrUsTeD ".to_string()),
                        ..inputs()
                    }
                } else {
                    write(
                        &global.path().join("settings.json"),
                        r#"{"trustAllWorkspaces":true}"#,
                    );
                    TrustInputs {
                        trust_all_workspaces: true,
                        ..inputs()
                    }
                };
                let guard = guard_for(workspace.path(), global.path(), &trust_inputs);
                write(&settings, r#"{"tools":{"enable_subagent":true}}"#);
                guard
                    .validate_origin()
                    .expect("universal policy remains active");
                std::fs::rename(&settings, workspace.path().join("old-settings.json"))
                    .expect("remove the last executable surface");
                guard
                    .validate_origin()
                    .expect("universal policy also allows removal");
            }
        }
    }

    #[test]
    fn reload_guard_observes_policy_revocation_and_malformed_operator_settings() {
        let workspace = tempfile::tempdir().expect("workspace");
        let global = tempfile::tempdir().expect("global");
        write(&workspace.path().join(".pi/settings.json"), r#"{}"#);
        let global_settings = global.path().join("settings.json");
        write(&global_settings, r#"{"trustAllWorkspaces":true}"#);
        let global_guard = guard_for(
            workspace.path(),
            global.path(),
            &TrustInputs {
                trust_all_workspaces: true,
                ..inputs()
            },
        );
        write(&global_settings, r#"{"trustAllWorkspaces":false}"#);
        assert_restart_required(global_guard.validate_origin());

        let trust_inputs = TrustInputs {
            cli_trust: true,
            ..inputs()
        };
        let scoped_guard = guard_for(workspace.path(), global.path(), &trust_inputs);
        let surface = WorkspaceTrustSurface::scan(workspace.path())
            .expect("scan")
            .expect("surface");
        WorkspaceTrustStore::load(&global.path().join(TRUST_STORE_FILE))
            .record(
                &workspace_key(workspace.path()),
                &surface.digest,
                TrustDecision::Untrusted,
            )
            .expect("revoke stored trust");
        assert_restart_required(scoped_guard.validate_origin());

        let trusted_guard = guard_for(workspace.path(), global.path(), &trust_inputs);
        write(&global_settings, r#"{"tools":{"enable_eval":"yes"}}"#);
        assert_restart_required(trusted_guard.validate_origin());
    }

    #[test]
    fn reload_guard_uses_the_anchored_operator_override() {
        let workspace = tempfile::tempdir().expect("workspace");
        let global = tempfile::tempdir().expect("global");
        write(&workspace.path().join(".pi/settings.json"), r#"{}"#);
        write(
            &global.path().join("settings.json"),
            r#"{"trustAllWorkspaces":true}"#,
        );
        let operator_settings = workspace.path().join("operator.json");
        write(&operator_settings, r#"{"trustAllWorkspaces":true}"#);
        let trust_inputs = TrustInputs {
            trust_all_workspaces: true,
            ..inputs()
        };
        let state = establish(
            workspace.path(),
            &global.path().join(TRUST_STORE_FILE),
            &trust_inputs,
            no_prompt,
        )
        .expect("admit operator policy");
        let guard = WorkspaceTrustGuard::from_established(
            workspace.path(),
            &state,
            &trust_inputs,
            global.path(),
            Some(Path::new("operator.json")),
        );
        write(
            &workspace.path().join(".pi/settings.json"),
            r#"{"tools":{"enable_eval":true}}"#,
        );
        guard
            .validate_origin()
            .expect("relative override resolved at origin");
        write(&operator_settings, r#"{"trustAllWorkspaces":false}"#);
        assert_restart_required(guard.validate_origin());
    }

    #[test]
    fn reload_guard_never_promotes_an_originally_untrusted_workspace() {
        let workspace = tempfile::tempdir().expect("workspace");
        let global = tempfile::tempdir().expect("global");
        write(&workspace.path().join(".pi/settings.json"), r#"{}"#);
        let guard = guard_for(workspace.path(), global.path(), &inputs());
        assert!(!guard.trusted());
        write(
            &workspace.path().join(".pi/settings.json"),
            "malformed project configuration",
        );
        write(
            &global.path().join("settings.json"),
            r#"{"trustAllWorkspaces":true}"#,
        );
        guard
            .validate_origin()
            .expect("project configuration stays excluded");
        assert!(!guard.trusted());
        let other = tempfile::tempdir().expect("other workspace");
        assert_restart_required(guard.validate_workspace(other.path()));
    }

    #[test]
    fn scan_treats_a_file_where_a_surface_directory_is_expected_as_absent() {
        // A stray regular file named `.codex` (this repository carried one)
        // used to abort startup with "Failed to inspect .codex/config.toml:
        // Not a directory"; a file cannot contain a trust surface.
        let dir = tempfile::tempdir().expect("tempdir"); // ubs:ignore test fixture
        std::fs::write(dir.path().join(".codex"), b"").expect("write .codex file"); // ubs:ignore test fixture
        assert!(
            WorkspaceTrustSurface::scan(dir.path())
                .expect("scan") // ubs:ignore test assertion
                .is_none()
        );
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
        assert!(first.mcp_config_entries.is_empty());

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
    fn scan_tracks_project_mcp_configuration_as_executable_surface() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project_mcp = dir.path().join(".pi/mcp.json");
        write(
            &project_mcp,
            r#"{"mcpServers":{"local":{"command":"first"}}}"#,
        );

        let first = WorkspaceTrustSurface::scan(dir.path())
            .expect("scan")
            .expect("MCP-only workspace must have a trust surface");
        assert!(!first.has_project_settings);
        assert!(first.extension_entries.is_empty());
        assert_eq!(first.mcp_config_entries, vec![".pi/mcp.json"]);

        write(
            &project_mcp,
            r#"{"mcpServers":{"local":{"command":"second"}}}"#,
        );
        let changed = WorkspaceTrustSurface::scan(dir.path())
            .expect("scan")
            .expect("surface");
        assert_ne!(
            first.digest, changed.digest,
            "changing an MCP execution target must invalidate workspace trust"
        );

        write(
            &dir.path().join(".codex/config.toml"),
            "[mcp_servers.foreign]\ncommand = \"foreign\"\n",
        );
        let foreign_added = WorkspaceTrustSurface::scan(dir.path())
            .expect("scan")
            .expect("surface");
        assert_eq!(
            foreign_added.mcp_config_entries,
            vec![".codex/config.toml", ".pi/mcp.json"]
        );
        assert_ne!(changed.digest, foreign_added.digest);
    }

    #[test]
    fn scan_tracks_agent_profile_bytes_names_and_nearest_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join("project");
        let cwd = project.join("nested");
        std::fs::create_dir_all(&cwd).expect("workspace");
        let ancestor = project.join(".pi/agents/scout.md");
        write(&ancestor, "read-only profile");
        let first = WorkspaceTrustSurface::scan(&cwd)
            .expect("scan")
            .expect("inherited profiles are a trust surface");
        assert!(!first.has_project_settings);
        assert!(first.extension_entries.is_empty());
        assert!(first.mcp_config_entries.is_empty());
        assert_eq!(
            first.agent_definition_entries,
            vec![escaped_path(&ancestor)]
        );

        write(&project.join(".pi/agents/notes.txt"), "ignored");
        write(&project.join(".pi/agents/nested/ignored.md"), "ignored");
        assert_eq!(
            first.digest,
            WorkspaceTrustSurface::scan(&cwd)
                .expect("scan")
                .expect("surface")
                .digest,
            "discovery is nonrecursive and Markdown-only"
        );

        write(&ancestor, "privileged profile");
        let changed = WorkspaceTrustSurface::scan(&cwd)
            .expect("scan")
            .expect("surface");
        assert_ne!(first.digest, changed.digest);
        let renamed = project.join(".pi/agents/reviewer.MD");
        std::fs::rename(&ancestor, &renamed).expect("rename fixture");
        let renamed_surface = WorkspaceTrustSurface::scan(&cwd)
            .expect("scan")
            .expect("case-insensitive Markdown selection");
        assert_ne!(changed.digest, renamed_surface.digest);
        assert_eq!(
            renamed_surface.agent_definition_entries,
            vec![escaped_path(&renamed)]
        );

        // The loader stops at the nearest directory, even when it is empty.
        let nearest = cwd.join(".pi/agents");
        std::fs::create_dir_all(&nearest).expect("empty nearest directory");
        assert!(WorkspaceTrustSurface::scan(&cwd).expect("scan").is_none());
        write(&nearest.join("reviewer.MD"), "privileged profile");
        let nearest_surface = WorkspaceTrustSurface::scan(&cwd)
            .expect("scan")
            .expect("nearest surface");
        assert_eq!(
            nearest_surface.agent_definition_entries,
            vec![".pi/agents/reviewer.MD"]
        );
        assert_ne!(renamed_surface.digest, nearest_surface.digest);
        write(&renamed, "ignored ancestor mutation");
        assert_eq!(
            nearest_surface.digest,
            WorkspaceTrustSurface::scan(&cwd)
                .expect("scan")
                .expect("surface")
                .digest
        );
    }

    #[test]
    fn agent_only_workspaces_require_admission_and_late_profiles_invalidate_empty_launches() {
        let workspace = tempfile::tempdir().expect("workspace");
        let global = tempfile::tempdir().expect("global");
        let initially_empty = guard_for(workspace.path(), global.path(), &inputs());
        assert!(initially_empty.trusted());
        write(
            &workspace.path().join(".pi/agents/scout.md"),
            "---\nname: scout\ndescription: project\ntools: eval,github,security_scan,subagent\n---\nproject prompt",
        );
        assert_restart_required(initially_empty.validate_origin());
        let denied = establish(
            workspace.path(),
            &global.path().join(TRUST_STORE_FILE),
            &inputs(),
            no_prompt,
        )
        .expect("noninteractive admission");
        assert!(!denied.trusted);
        assert_eq!(denied.source, TrustSource::NonInteractive);
        let admitted = guard_for(
            workspace.path(),
            global.path(),
            &TrustInputs {
                cli_trust: true,
                ..inputs()
            },
        );
        admitted.validate_origin().expect("admitted profile");
        assert_restart_required(initially_empty.validate_origin());
    }

    #[test]
    fn scan_enforces_agent_definition_file_and_directory_bounds() {
        let non_regular = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(non_regular.path().join(".pi/agents/directory.md"))
            .expect("non-regular definition");
        let error = WorkspaceTrustSurface::scan(non_regular.path())
            .expect_err("non-regular definitions must fail closed");
        assert!(error.to_string().contains("not a regular file"));

        let oversized = tempfile::tempdir().expect("tempdir");
        write(
            &oversized.path().join(".pi/agents/scout.md"),
            &"x".repeat(MAX_TRUST_AGENT_BYTES + 1),
        );
        let error = WorkspaceTrustSurface::scan(oversized.path())
            .expect_err("oversized definition must fail closed");
        assert!(error.to_string().contains("exceeds 1048576 bytes"));

        let many = tempfile::tempdir().expect("tempdir");
        for index in 0..MAX_TRUST_AGENT_DEFINITIONS {
            write(
                &many.path().join(format!(".pi/agents/{index}.md")),
                "profile",
            );
        }
        let accepted = WorkspaceTrustSurface::scan(many.path())
            .expect("bounded scan")
            .expect("profiles");
        assert_eq!(
            accepted.agent_definition_entries.len(),
            MAX_TRUST_AGENT_DEFINITIONS
        );
        write(&many.path().join(".pi/agents/overflow.md"), "profile");
        let error = WorkspaceTrustSurface::scan(many.path())
            .expect_err("definition count is bounded before file reads");
        assert!(error.to_string().contains("PI_AGENT_DEFINITION_LIMIT"));
    }

    #[cfg(unix)]
    #[test]
    fn scan_binds_agent_directory_symlink_origin_and_escapes_profile_names() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("workspace");
        std::fs::create_dir_all(cwd.join(".pi")).expect("workspace");
        let first_root = temp.path().join("profiles-first");
        let second_root = temp.path().join("profiles-second");
        let filename = "scout\n\u{1b}[2J.md";
        for root in [&first_root, &second_root] {
            write(&root.join(filename), "identical profile bytes");
        }
        let selected = cwd.join(".pi/agents");
        symlink(&first_root, &selected).expect("first origin");
        let first = WorkspaceTrustSurface::scan(&cwd)
            .expect("scan")
            .expect("surface");
        let displayed = first.agent_definition_entries.join(" ");
        assert!(!displayed.contains('\n'));
        assert!(!displayed.contains('\u{1b}'));
        assert!(displayed.contains("\\n"));
        assert!(displayed.contains("\\u{1b}"));
        std::fs::rename(&selected, cwd.join(".pi/agents.previous")).expect("retain old link");
        symlink(&second_root, &selected).expect("second origin");
        let second = WorkspaceTrustSurface::scan(&cwd)
            .expect("scan")
            .expect("surface");
        assert_ne!(
            first.digest, second.digest,
            "origin changes must invalidate trust"
        );

        symlink("/dev/zero", second_root.join("device.md")).expect("non-regular target");
        let error = WorkspaceTrustSurface::scan(&cwd)
            .expect_err("agent device symlink must fail without reading");
        assert!(error.to_string().contains("not a regular file"));
    }

    #[test]
    fn scan_rejects_non_regular_and_oversized_trust_surfaces() {
        let non_regular = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(non_regular.path().join(".pi/mcp.json"))
            .expect("create non-regular MCP surface");
        let err = WorkspaceTrustSurface::scan(non_regular.path())
            .expect_err("directories must not be read as trust surfaces");
        assert!(err.to_string().contains("not a regular file"));

        let oversized = tempfile::tempdir().expect("tempdir");
        write(
            &oversized.path().join(".pi/mcp.json"),
            &"x".repeat(MAX_TRUST_CONFIG_BYTES + 1),
        );
        let err = WorkspaceTrustSurface::scan(oversized.path())
            .expect_err("oversized configs must fail before allocation grows unbounded");
        assert!(err.to_string().contains("exceeds 1048576 bytes"));
    }

    #[cfg(unix)]
    #[test]
    fn scan_rejects_device_symlinks_and_escapes_untrusted_names() {
        use std::os::unix::fs::symlink;

        let device = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(device.path().join(".pi")).expect("create project dir");
        symlink("/dev/zero", device.path().join(".pi/mcp.json")).expect("create device symlink");
        let err = WorkspaceTrustSurface::scan(device.path())
            .expect_err("device symlinks must be rejected before opening");
        assert!(err.to_string().contains("not a regular file"));

        let controls = tempfile::tempdir().expect("tempdir");
        write(
            &controls.path().join(".pi/extensions/evil\n\u{1b}[2J.js"),
            "export {}\n",
        );
        let surface = WorkspaceTrustSurface::scan(controls.path())
            .expect("scan")
            .expect("surface");
        let displayed = surface.extension_entries.join(" ");
        assert!(!displayed.contains('\n'));
        assert!(!displayed.contains('\u{1b}'));
        assert!(displayed.contains("\\n"));
        assert!(displayed.contains("\\u{1b}"));
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

    #[cfg(unix)]
    #[test]
    fn workspace_keys_preserve_non_utf8_canonical_path_identity() -> std::io::Result<()> {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir
            .path()
            .join(OsString::from_vec(vec![b'w', b's', b'-', 0x80]));
        let second = dir
            .path()
            .join(OsString::from_vec(vec![b'w', b's', b'-', 0x81]));
        for workspace in [&first, &second] {
            match std::fs::create_dir(workspace) {
                Ok(()) => {}
                Err(error)
                    if error.raw_os_error() == Some(rustix::io::Errno::ILSEQ.raw_os_error()) =>
                {
                    eprintln!(
                        "SKIP non-UTF-8 workspace {workspace:?}: filesystem returned {error}"
                    );
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
        }
        eprintln!("Exercising distinct non-UTF-8 workspaces: {first:?}, {second:?}");

        let first_key = workspace_key(&first);
        let second_key = workspace_key(&second);
        assert_ne!(
            first_key, second_key,
            "distinct raw canonical paths must never share trust decisions"
        );
        assert!(first_key.starts_with("path-v3:unix:"));
        assert!(first_key.is_ascii(), "store keys must remain JSON-safe");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn workspace_keys_use_tagged_utf16le_path_encoding() {
        use std::os::windows::ffi::OsStrExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = std::fs::canonicalize(dir.path()).expect("canonical workspace");
        let expected_bytes = canonical
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        assert_eq!(
            workspace_key(dir.path()),
            format!(
                "path-v3:windows-utf16le:{}",
                crate::package_manager::hex_encode(&expected_bytes)
            )
        );
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
    fn establish_non_interactive_mcp_only_workspace_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &dir.path().join(".agents/mcp.json"),
            r#"{"mcpServers":{"project":{"command":"project-server"}}}"#,
        );
        let store_path = dir.path().join("store.json");

        let state = establish(dir.path(), &store_path, &inputs(), no_prompt).expect("establish");
        assert!(!state.trusted);
        assert_eq!(state.source, TrustSource::NonInteractive);
        assert_eq!(
            state.surface.as_ref().expect("surface").mcp_config_entries,
            vec![".agents/mcp.json"]
        );
        assert!(!store_path.exists());
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
    fn optional_tool_activation_changes_require_fresh_workspace_trust() {
        let cli = crate::cli::Cli::try_parse_from(["pi"]).expect("default CLI");
        for (name, setting) in [
            ("eval", "enableEval"),
            ("github", "enableGithub"),
            ("security_scan", "enableSecurityScan"),
            ("subagent", "enableSubagent"),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let cwd = dir.path().join("workspace");
            let global_dir = dir.path().join("global");
            let store_path = dir.path().join("trust.json");
            let settings_path = cwd.join(".pi/settings.json");
            write(
                &settings_path,
                &serde_json::json!({"tools":{(setting):false}}).to_string(),
            );
            let original = establish(
                &cwd,
                &store_path,
                &TrustInputs {
                    cli_trust: true,
                    ..inputs()
                },
                no_prompt,
            )
            .expect("trust original settings");
            assert!(original.trusted);
            let remembered = establish(&cwd, &store_path, &inputs(), no_prompt)
                .expect("unchanged settings retain trust");
            assert!(remembered.trusted);
            assert_eq!(remembered.source, TrustSource::Store);

            write(
                &settings_path,
                &serde_json::json!({"tools":{(setting):true}}).to_string(),
            );
            let changed = establish(&cwd, &store_path, &inputs(), no_prompt)
                .expect("changed settings must fail closed noninteractively");
            assert!(!changed.trusted, "enabling {name} must invalidate trust");
            assert_eq!(changed.source, TrustSource::NonInteractive);
            assert_ne!(
                original.surface.as_ref().expect("original surface").digest,
                changed.surface.as_ref().expect("changed surface").digest
            );
            let ignored = Config::load_with_roots_and_project_trust(
                None,
                &global_dir,
                &cwd,
                changed.trusted,
            )
            .expect("load without untrusted project settings");
            assert!(!cli.enabled_tools(&ignored).contains(&name));

            let approved = establish(
                &cwd,
                &store_path,
                &TrustInputs {
                    interactive: true,
                    ..inputs()
                },
                |_| Ok(true),
            )
            .expect("approve changed settings");
            assert!(approved.trusted);
            assert_eq!(approved.source, TrustSource::Prompt);
            let accepted = Config::load_with_roots_and_project_trust(
                None,
                &global_dir,
                &cwd,
                approved.trusted,
            )
            .expect("load newly trusted project settings");
            assert!(cli.enabled_tools(&accepted).contains(&name));
        }
    }

    #[test]
    fn optional_tool_settings_cannot_grant_their_own_workspace_trust() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().join("workspace");
        let global_dir = dir.path().join("global");
        let store_path = dir.path().join("trust.json");
        write(
            &cwd.join(".pi/settings.json"),
            r#"{"trustAllWorkspaces":true,"tools":{"enableEval":true,"enableGithub":true,"enableSecurityScan":true,"enableSubagent":true}}"#,
        );
        // Startup reads trustAllWorkspaces from operator-controlled settings
        // before allowing any project settings into the effective config.
        let global =
            Config::load_with_roots_and_project_trust(None, &global_dir, &cwd, false)
                .expect("load operator-controlled settings");
        let state = establish(
            &cwd,
            &store_path,
            &TrustInputs {
                trust_all_workspaces: global.trust_all_workspaces.unwrap_or(false),
                ..inputs()
            },
            no_prompt,
        )
        .expect("establish trust");
        assert!(!state.trusted);
        assert_eq!(state.source, TrustSource::NonInteractive);
        assert!(!store_path.exists());
        let config =
            Config::load_with_roots_and_project_trust(None, &global_dir, &cwd, state.trusted)
                .expect("load untrusted workspace config");
        let cli = crate::cli::Cli::try_parse_from(["pi"]).expect("default CLI");
        let enabled = cli.enabled_tools(&config);
        for name in ["eval", "github", "security_scan", "subagent"] {
            assert!(!enabled.contains(&name), "project must not grant {name}");
        }
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
