//! Package management: install/remove/update/list.
//!
//! This is a Rust port of pi-mono's package manager concepts:
//! - Sources: `npm:pkg`, `git:host/owner/repo[@ref]`, local paths
//! - Scopes: user (global) and project (local)
//! - Global npm installs use `npm install -g` (npm-managed global root)
//! - Git installs are under Pi's agent/project directories (`~/.pi/agent/git`, `./.pi/git`)

use crate::agent_cx::AgentCx;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::extension_index::ExtensionIndexStore;
use crate::extensions::{CompatibilityScanner, load_extension_manifest};
use asupersync::channel::oneshot;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use tracing::{info, warn};

fn finish_package_task<T, E>(
    handle: thread::JoinHandle<()>,
    recv_result: std::result::Result<Result<T>, E>,
    cancelled_message: &'static str,
) -> Result<T> {
    if let Err(panic_payload) = handle.join() {
        std::panic::resume_unwind(panic_payload);
    }
    recv_result.map_err(|_| Error::tool("package_manager", cancelled_message))?
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageScope {
    User,
    Project,
    Temporary,
}

#[derive(Debug, Clone)]
pub struct PackageEntry {
    pub scope: PackageScope,
    pub source: String,
    pub filter: Option<PackageFilter>,
}

/// Optional per-resource filters for packages in settings.
///
/// Mirrors pi-mono's `PackageSource` object form:
/// `{ source, extensions?, skills?, prompts?, themes? }`.
#[derive(Debug, Clone, Default)]
pub struct PackageFilter {
    pub extensions: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub prompts: Option<Vec<String>>,
    pub themes: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct PathMetadata {
    pub source: String,
    pub scope: PackageScope,
    pub origin: ResourceOrigin,
    pub base_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceOrigin {
    Package,
    TopLevel,
}

#[derive(Debug, Clone)]
pub struct ResolvedResource {
    pub path: PathBuf,
    pub enabled: bool,
    pub metadata: PathMetadata,
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedPaths {
    pub extensions: Vec<ResolvedResource>,
    pub skills: Vec<ResolvedResource>,
    pub prompts: Vec<ResolvedResource>,
    pub themes: Vec<ResolvedResource>,
}

/// Explicit roots for resource resolution (settings + auto-discovery base dirs).
///
/// This exists primarily to make `PackageManager::resolve()` testable without
/// mutating process-wide environment variables (Rust 2024 makes `set_var` unsafe).
#[derive(Debug, Clone)]
pub struct ResolveRoots {
    pub global_settings_path: PathBuf,
    pub project_settings_path: PathBuf,
    pub global_base_dir: PathBuf,
    pub project_base_dir: PathBuf,
    pub project_settings_enabled: bool,
}

impl ResolveRoots {
    fn from_override(cwd: &Path, config_override_path: Option<&Path>) -> Self {
        Self {
            global_settings_path: config_override_path.map_or_else(
                || Config::global_dir().join("settings.json"),
                std::path::Path::to_path_buf,
            ),
            project_settings_path: project_settings_path(cwd),
            global_base_dir: Config::global_dir(),
            project_base_dir: cwd.join(Config::project_dir()),
            project_settings_enabled: config_override_path.is_none(),
        }
    }

    /// Build roots using the default Pi settings locations (env + cwd).
    #[must_use]
    pub fn from_env(cwd: &Path) -> Self {
        Self::from_override(cwd, Config::config_path_override_from_env(cwd).as_deref())
    }
}

#[derive(Debug, Clone)]
pub struct PackageManager {
    cwd: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageLockAction {
    Install,
    Update,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageLockfile {
    pub schema: String,
    #[serde(default)]
    pub entries: Vec<PackageLockEntry>,
}

impl Default for PackageLockfile {
    fn default() -> Self {
        Self {
            schema: PACKAGE_LOCK_SCHEMA.to_string(),
            entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageLockEntry {
    pub identity: String,
    pub source: String,
    pub source_kind: PackageSourceKind,
    pub resolved: PackageResolvedProvenance,
    pub digest_sha256: String,
    pub trust_state: PackageEntryTrustState,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PackageSourceKind {
    Npm,
    Git,
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PackageResolvedProvenance {
    Npm {
        name: String,
        requested_spec: String,
        requested_version: Option<String>,
        installed_version: String,
        pinned: bool,
    },
    Git {
        repo: String,
        host: String,
        path: String,
        requested_ref: Option<String>,
        resolved_commit: String,
        origin_url: Option<String>,
        pinned: bool,
    },
    Local {
        resolved_path: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PackageEntryTrustState {
    Trusted,
    Rejected,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageTrustAuditEvent {
    pub schema: &'static str,
    pub timestamp: String,
    pub action: String,
    pub scope: String,
    pub source: String,
    pub identity: String,
    pub from_state: String,
    pub to_state: String,
    pub reason_codes: Vec<String>,
    pub remediation: Option<String>,
    pub details: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct LockTransitionPlan {
    pub reason_codes: Vec<String>,
    pub from_state: String,
    pub to_state: String,
}

#[derive(Debug, Clone)]
pub struct PackageLockMismatch {
    pub code: &'static str,
    pub reason: String,
    pub remediation: String,
}

pub const PACKAGE_LOCK_SCHEMA: &str = "pi.package_lock.v1";
pub const PACKAGE_TRUST_AUDIT_SCHEMA: &str = "pi.package_trust_audit.v1";

impl PackageManager {
    pub const fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }

    /// Resolve a shorthand source (`id`/`name`) via the local extension index when possible.
    ///
    /// Returns the original source when no unique alias mapping exists.
    pub fn resolve_install_source_alias(&self, source: &str) -> String {
        let source = source.trim();
        resolve_install_source_alias(source, &self.cwd).unwrap_or_else(|| source.to_string())
    }

    /// Get a stable identity for a package source, ignoring version/ref.
    ///
    /// Mirrors pi-mono's `getPackageIdentity()`:
    /// - npm: `npm:<name>`
    /// - git: `git:<repo>` (normalized host/path, no ref)
    /// - local: `local:<resolved-absolute-path>`
    pub fn package_identity(&self, source: &str) -> String {
        match parse_source(source, &self.cwd) {
            ParsedSource::Npm { name, .. } => format!("npm:{name}"),
            ParsedSource::Git { repo, .. } => format!("git:{repo}"),
            ParsedSource::Local { path } => format!("local:{}", path.display()),
        }
    }

    pub async fn install(&self, source: &str, scope: PackageScope) -> Result<()> {
        let this = self.clone();
        let source = source.to_string();
        let (tx, mut rx) = oneshot::channel();

        let handle = thread::spawn(move || {
            let res = this.install_sync(&source, scope);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        let recv_result = rx.recv(cx.cx()).await;
        finish_package_task(handle, recv_result, "Install task cancelled")
    }

    /// Synchronous variant of [`Self::install`] for startup subcommand fast paths.
    pub fn install_blocking(&self, source: &str, scope: PackageScope) -> Result<()> {
        self.install_sync(source, scope)
    }

    fn install_sync(&self, source: &str, scope: PackageScope) -> Result<()> {
        let source = validate_non_empty_source(source, "Package source")?;
        let parsed = parse_source(source, &self.cwd);
        match parsed {
            ParsedSource::Npm { spec, .. } => self.install_npm(&spec, scope),
            ParsedSource::Git {
                clone_source,
                host,
                path,
                r#ref,
                ..
            } => self.install_git(&clone_source, &host, &path, r#ref.as_deref(), scope),
            ParsedSource::Local { path } => {
                if path.exists() {
                    Ok(())
                } else {
                    Err(Error::config(format!(
                        "Local package path does not exist: {}",
                        path.display()
                    )))
                }
            }
        }?;

        self.verify_and_record_lock(source, scope, PackageLockAction::Install)
    }

    pub async fn remove(&self, source: &str, scope: PackageScope) -> Result<()> {
        let this = self.clone();
        let source = source.to_string();
        let (tx, mut rx) = oneshot::channel();

        let handle = thread::spawn(move || {
            let res = this.remove_sync(&source, scope);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        let recv_result = rx.recv(cx.cx()).await;
        finish_package_task(handle, recv_result, "Remove task cancelled")
    }

    /// Synchronous variant of [`Self::remove`] for startup subcommand fast paths.
    pub fn remove_blocking(&self, source: &str, scope: PackageScope) -> Result<()> {
        self.remove_sync(source, scope)
    }

    fn remove_sync(&self, source: &str, scope: PackageScope) -> Result<()> {
        let source = validate_non_empty_source(source, "Package source")?;
        let parsed = parse_source(source, &self.cwd);
        match parsed {
            ParsedSource::Npm { name, .. } => self.uninstall_npm(&name, scope),
            ParsedSource::Git { host, path, .. } => self.remove_git(&host, &path, scope),
            ParsedSource::Local { .. } => Ok(()),
        }?;

        self.remove_lock_entry(source, scope)
    }

    pub async fn update_source(&self, source: &str, scope: PackageScope) -> Result<()> {
        let this = self.clone();
        let source = source.to_string();
        let (tx, mut rx) = oneshot::channel();

        let handle = thread::spawn(move || {
            let res = this.update_source_sync(&source, scope);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        let recv_result = rx.recv(cx.cx()).await;
        finish_package_task(handle, recv_result, "Update task cancelled")
    }

    /// Synchronous variant of [`Self::update_source`] for startup subcommand fast paths.
    pub fn update_source_blocking(&self, source: &str, scope: PackageScope) -> Result<()> {
        self.update_source_sync(source, scope)
    }

    fn update_source_sync(&self, source: &str, scope: PackageScope) -> Result<()> {
        let source = validate_non_empty_source(source, "Package source")?;
        let parsed = parse_source(source, &self.cwd);
        match parsed {
            ParsedSource::Npm { spec, pinned, .. } => {
                if !pinned {
                    self.install_npm(&spec, scope)?;
                }
            }
            ParsedSource::Git {
                clone_source,
                host,
                path,
                pinned,
                ..
            } => {
                if !pinned {
                    self.update_git(&clone_source, &host, &path, scope)?;
                }
            }
            ParsedSource::Local { .. } => {}
        }

        self.verify_and_record_lock(source, scope, PackageLockAction::Update)
    }

    pub async fn installed_path(
        &self,
        source: &str,
        scope: PackageScope,
    ) -> Result<Option<PathBuf>> {
        let this = self.clone();
        let source = source.to_string();
        let (tx, mut rx) = oneshot::channel();

        let handle = thread::spawn(move || {
            let res = this.installed_path_sync(&source, scope);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        let recv_result = rx.recv(cx.cx()).await;
        finish_package_task(handle, recv_result, "Installed path lookup cancelled")
    }

    /// Synchronous variant of [`Self::installed_path`] for startup fast paths.
    pub fn installed_path_blocking(
        &self,
        source: &str,
        scope: PackageScope,
    ) -> Result<Option<PathBuf>> {
        self.installed_path_sync(source, scope)
    }

    fn installed_path_sync(&self, source: &str, scope: PackageScope) -> Result<Option<PathBuf>> {
        let source = validate_non_empty_source(source, "Package source")?;
        let parsed = parse_source(source, &self.cwd);
        Ok(match parsed {
            ParsedSource::Npm { name, .. } => Some(self.npm_install_path(&name, scope)?),
            ParsedSource::Git { host, path, .. } => {
                Some(self.checked_git_install_path(&host, &path, scope)?)
            }
            ParsedSource::Local { path } => Some(path),
        })
    }

    pub async fn list_packages(&self) -> Result<Vec<PackageEntry>> {
        let this = self.clone();
        let (tx, mut rx) = oneshot::channel();

        let handle = thread::spawn(move || {
            let res = this.list_packages_sync();
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        let recv_result = rx.recv(cx.cx()).await;
        finish_package_task(handle, recv_result, "List packages task cancelled")
    }

    /// Synchronous variant of [`Self::list_packages`] for startup fast paths.
    pub fn list_packages_blocking(&self) -> Result<Vec<PackageEntry>> {
        let roots = ResolveRoots::from_env(&self.cwd);
        Self::list_packages_with_roots(&roots)
    }

    fn list_packages_sync(&self) -> Result<Vec<PackageEntry>> {
        self.list_packages_blocking()
    }

    fn list_packages_with_roots(roots: &ResolveRoots) -> Result<Vec<PackageEntry>> {
        let global = list_packages_in_settings(&roots.global_settings_path)?
            .into_iter()
            .map(|mut p| {
                p.scope = PackageScope::User;
                p
            });
        let project = roots
            .project_settings_enabled
            .then(|| list_packages_in_settings(&roots.project_settings_path))
            .transpose()?
            .unwrap_or_default()
            .into_iter()
            .map(|mut p| {
                p.scope = PackageScope::Project;
                p
            });
        Ok(global.chain(project).collect())
    }

    /// Best-effort synchronous package resource resolution for startup/config fast paths.
    ///
    /// Returns `Ok(Some(...))` when all package resources can be resolved from local state
    /// without any install/update work. Returns `Ok(None)` when a package source would require
    /// install/update behavior (for example, missing npm/git package files), allowing callers
    /// to fall back to the canonical async `resolve()` path.
    pub fn resolve_package_resources_blocking(&self) -> Result<Option<ResolvedPaths>> {
        let roots = ResolveRoots::from_env(&self.cwd);
        self.resolve_package_resources_with_roots_blocking(&roots)
    }

    fn resolve_package_resources_with_roots_blocking(
        &self,
        roots: &ResolveRoots,
    ) -> Result<Option<ResolvedPaths>> {
        let global = read_settings_snapshot(&roots.global_settings_path)?;
        let project = read_project_settings_snapshot(roots)?;

        let mut all_packages: Vec<ScopedPackage> = Vec::new();
        all_packages.extend(global.packages.iter().cloned().map(|pkg| ScopedPackage {
            pkg,
            scope: PackageScope::User,
        }));
        all_packages.extend(project.packages.iter().cloned().map(|pkg| ScopedPackage {
            pkg,
            scope: PackageScope::Project,
        }));
        let package_sources = self.dedupe_packages(all_packages);
        let global_npm_root = self.pass_global_npm_root_blocking(&package_sources)?;

        let mut accumulator = ResourceAccumulator::new();

        for entry in package_sources {
            let source_str = entry.pkg.source.trim();
            if source_str.is_empty() {
                continue;
            }

            let parsed = parse_source(source_str, &self.cwd);
            let mut metadata = PathMetadata {
                source: source_str.to_string(),
                scope: entry.scope,
                origin: ResourceOrigin::Package,
                base_dir: None,
            };

            match parsed {
                ParsedSource::Local { path } => {
                    Self::resolve_local_extension_source(
                        &path,
                        &mut accumulator,
                        entry.pkg.filter.as_ref(),
                        &mut metadata,
                        entry.scope == PackageScope::Temporary,
                    )?;
                }
                ParsedSource::Npm { name, .. } => {
                    let installed_path =
                        self.npm_install_dir(&name, entry.scope, global_npm_root.as_deref())?;

                    if !installed_path.exists() {
                        return Ok(None);
                    }

                    metadata.base_dir = Some(installed_path.clone());
                    Self::collect_package_resources(
                        &installed_path,
                        &mut accumulator,
                        entry.pkg.filter.as_ref(),
                        &metadata,
                    )?;
                }
                ParsedSource::Git { host, path, .. } => {
                    let installed_path =
                        self.checked_git_install_path(&host, &path, entry.scope)?;
                    if !installed_path.exists() {
                        return Ok(None);
                    }

                    metadata.base_dir = Some(installed_path.clone());
                    Self::collect_package_resources(
                        &installed_path,
                        &mut accumulator,
                        entry.pkg.filter.as_ref(),
                        &metadata,
                    )?;
                }
            }
        }

        Ok(Some(accumulator.into_resolved_paths()))
    }

    /// Ensure all packages in settings are installed.
    /// Returns the list of packages that were newly installed.
    pub async fn ensure_packages_installed(&self) -> Result<Vec<PackageEntry>> {
        let packages = self.list_packages().await?;
        let installed = self.ensure_package_entries_installed(packages).await?;

        // Prune lockfile entries whose settings.json source is gone.
        // Reconciliation failure is logged but never blocks startup or
        // installation — stale entries are inert.
        if let Err(err) = self.reconcile_all_lockfiles().await {
            tracing::warn!(
                event = "pkg.lockfile.reconcile.failed",
                error = %err,
                "Lockfile reconciliation failed during ensure_packages_installed"
            );
        }

        Ok(installed)
    }

    /// Prune stale lockfile entries across the user and project scopes.
    ///
    /// See [`Self::reconcile_lockfile_sync`] for semantics. Each pruned
    /// entry is recorded in `package-trust-audit.jsonl` with
    /// `reason_codes: ["reconciled"]`. The two scopes are reconciled
    /// independently — a failure in one (e.g. a corrupt lockfile) does
    /// not block the other.
    pub async fn reconcile_all_lockfiles(&self) -> Result<Vec<PackageLockEntry>> {
        let this = self.clone();
        let (tx, mut rx) = oneshot::channel();

        let handle = thread::spawn(move || {
            let mut pruned = Vec::new();
            for scope in [PackageScope::User, PackageScope::Project] {
                match this.reconcile_lockfile_sync(scope) {
                    Ok(mut entries) => pruned.append(&mut entries),
                    Err(err) => tracing::warn!(
                        event = "pkg.lockfile.reconcile.scope_failed",
                        scope = scope_label(scope),
                        error = %err,
                        "Lockfile reconciliation failed for this scope; other scopes will still be reconciled"
                    ),
                }
            }
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), Ok::<_, Error>(pruned));
        });

        let cx = AgentCx::for_request();
        let recv_result = rx.recv(cx.cx()).await;
        finish_package_task(handle, recv_result, "Lockfile reconcile task cancelled")
    }

    async fn ensure_package_entries_installed(
        &self,
        packages: Vec<PackageEntry>,
    ) -> Result<Vec<PackageEntry>> {
        let mut installed = Vec::new();

        for entry in packages {
            match self.installed_path(&entry.source, entry.scope).await? {
                Some(path) if path.exists() => continue,
                _ => {}
            }

            self.install(&entry.source, entry.scope).await?;
            installed.push(entry);
        }

        Ok(installed)
    }

    /// Resolve all resources (extensions/skills/prompts/themes) from:
    /// - packages in global + project settings (deduped by identity; project wins)
    /// - local resource entries from settings (with pattern filtering)
    /// - auto-discovered resources from standard directories (with override patterns)
    ///
    /// This matches pi-mono's `DefaultPackageManager.resolve()` semantics.
    pub async fn resolve(&self) -> Result<ResolvedPaths> {
        let roots = ResolveRoots::from_env(&self.cwd);
        self.resolve_with_roots(&roots).await
    }

    pub async fn resolve_with_roots(&self, roots: &ResolveRoots) -> Result<ResolvedPaths> {
        let this_for_setup = self.clone();
        let roots_for_setup = roots.clone();
        let (tx, mut rx) = oneshot::channel();

        // Offload the heavy lifting (sync I/O) to a thread
        let handle = thread::spawn(move || {
            let res: Result<(SettingsSnapshot, SettingsSnapshot, Vec<ScopedPackage>)> = (|| {
                let global = read_settings_snapshot(&roots_for_setup.global_settings_path)?;
                let project = read_project_settings_snapshot(&roots_for_setup)?;

                // 1) Package resources (global + project, deduped; project wins)
                let mut all_packages: Vec<ScopedPackage> = Vec::new();
                all_packages.extend(global.packages.iter().cloned().map(|pkg| ScopedPackage {
                    pkg,
                    scope: PackageScope::User,
                }));
                if roots_for_setup.project_settings_enabled {
                    all_packages.extend(project.packages.iter().cloned().map(|pkg| {
                        ScopedPackage {
                            pkg,
                            scope: PackageScope::Project,
                        }
                    }));
                }
                let package_sources = this_for_setup.dedupe_packages(all_packages);
                Ok((global, project, package_sources))
            })(
            );
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        let recv_result = rx.recv(cx.cx()).await;
        let (global, project, package_sources) =
            finish_package_task(handle, recv_result, "Resolve setup task cancelled")?;

        // One `npm root -g` subprocess per resolution pass, not one per package.
        let global_npm_root = self.pass_global_npm_root(&package_sources).await?;

        let mut accumulator = ResourceAccumulator::new();

        // This part may be async (install work for missing/outdated NPM packages)
        Box::pin(self.resolve_package_sources(
            &package_sources,
            global_npm_root.as_deref(),
            &mut accumulator,
        ))
        .await?;

        // Offload the rest of sync resolution
        let this = self.clone();
        let roots = roots.clone();
        let (tx, mut rx) = oneshot::channel();
        let accumulator = std::sync::Mutex::new(accumulator);

        let handle = thread::spawn(move || {
            let mut accumulator = accumulator
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            // 2) Local entries from settings (global and project)
            for resource_type in ResourceType::all() {
                let target = accumulator.target_mut(resource_type);
                Self::resolve_local_entries(
                    global.entries_for(resource_type),
                    resource_type,
                    target,
                    &PathMetadata {
                        source: "local".to_string(),
                        scope: PackageScope::User,
                        origin: ResourceOrigin::TopLevel,
                        base_dir: Some(roots.global_base_dir.clone()),
                    },
                    &roots.global_base_dir,
                );

                if roots.project_settings_enabled {
                    Self::resolve_local_entries(
                        project.entries_for(resource_type),
                        resource_type,
                        target,
                        &PathMetadata {
                            source: "local".to_string(),
                            scope: PackageScope::Project,
                            origin: ResourceOrigin::TopLevel,
                            base_dir: Some(roots.project_base_dir.clone()),
                        },
                        &roots.project_base_dir,
                    );
                }
            }

            // 3) Auto-discovered resources from standard dirs (global and project)
            this.add_auto_discovered_resources(
                &mut accumulator,
                &global,
                &project,
                &roots.global_base_dir,
                &roots.project_base_dir,
                roots.project_settings_enabled,
            );

            let resolved = accumulator.clone().into_resolved_paths();
            drop(accumulator);
            maybe_emit_compat_ledgers(&resolved.extensions);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), Ok(resolved));
        });

        let cx = AgentCx::for_request();
        let recv_result = rx.recv(cx.cx()).await;
        finish_package_task(handle, recv_result, "Resolve processing task cancelled")
    }

    /// Resolve resources for extension sources specified via CLI `-e/--extension`.
    ///
    /// Mirrors pi-mono's `resolveExtensionSources(..., { temporary: true })`.
    pub async fn resolve_extension_sources(
        &self,
        sources: &[String],
        options: ResolveExtensionSourcesOptions,
    ) -> Result<ResolvedPaths> {
        let scope = if options.temporary {
            PackageScope::Temporary
        } else if options.local {
            PackageScope::Project
        } else {
            PackageScope::User
        };

        let mut accumulator = ResourceAccumulator::new();
        let package_sources = sources
            .iter()
            .map(|source| {
                Ok(ScopedPackage {
                    pkg: PackageSpec {
                        source: validate_non_empty_source(source, "Extension source")?.to_string(),
                        filter: None,
                    },
                    scope,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let global_npm_root = self.pass_global_npm_root(&package_sources).await?;
        Box::pin(self.resolve_package_sources(
            &package_sources,
            global_npm_root.as_deref(),
            &mut accumulator,
        ))
        .await?;

        let (tx, mut rx) = oneshot::channel();
        let accumulator = std::sync::Mutex::new(accumulator);

        let handle = thread::spawn(move || {
            let resolved = {
                let accumulator = accumulator
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                accumulator.clone().into_resolved_paths()
            };
            maybe_emit_compat_ledgers(&resolved.extensions);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), Ok(resolved));
        });

        let cx = AgentCx::for_request();
        let recv_result = rx.recv(cx.cx()).await;
        finish_package_task(handle, recv_result, "Resolve extensions task cancelled")
    }

    pub async fn add_package_source(&self, source: &str, scope: PackageScope) -> Result<()> {
        let this = self.clone();
        let source = source.to_string();
        let (tx, mut rx) = oneshot::channel();

        let handle = thread::spawn(move || {
            let res = this.add_package_source_sync(&source, scope);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        let recv_result = rx.recv(cx.cx()).await;
        finish_package_task(handle, recv_result, "Add source task cancelled")
    }

    /// Synchronous variant of [`Self::add_package_source`] for startup subcommand fast paths.
    pub fn add_package_source_blocking(&self, source: &str, scope: PackageScope) -> Result<()> {
        self.add_package_source_sync(source, scope)
    }

    fn add_package_source_sync(&self, source: &str, scope: PackageScope) -> Result<()> {
        let path = match scope {
            PackageScope::User => global_settings_path(&self.cwd),
            PackageScope::Project => project_settings_path(&self.cwd),
            PackageScope::Temporary => {
                return Err(Error::config(
                    "Temporary packages cannot be persisted to settings".to_string(),
                ));
            }
        };
        update_package_sources(&path, source, UpdateAction::Add, &self.cwd)
    }

    pub async fn remove_package_source(&self, source: &str, scope: PackageScope) -> Result<()> {
        let this = self.clone();
        let source = source.to_string();
        let (tx, mut rx) = oneshot::channel();

        let handle = thread::spawn(move || {
            let res = this.remove_package_source_sync(&source, scope);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        let recv_result = rx.recv(cx.cx()).await;
        finish_package_task(handle, recv_result, "Remove source task cancelled")
    }

    /// Synchronous variant of [`Self::remove_package_source`] for startup subcommand fast paths.
    pub fn remove_package_source_blocking(&self, source: &str, scope: PackageScope) -> Result<()> {
        self.remove_package_source_sync(source, scope)
    }

    fn remove_package_source_sync(&self, source: &str, scope: PackageScope) -> Result<()> {
        let path = match scope {
            PackageScope::User => global_settings_path(&self.cwd),
            PackageScope::Project => project_settings_path(&self.cwd),
            PackageScope::Temporary => {
                return Err(Error::config(
                    "Temporary packages cannot be persisted to settings".to_string(),
                ));
            }
        };
        update_package_sources(&path, source, UpdateAction::Remove, &self.cwd)
    }

    fn lockfile_path_for_scope(&self, scope: PackageScope) -> Option<PathBuf> {
        match scope {
            PackageScope::User => Some(Config::global_dir().join("packages.lock.json")),
            PackageScope::Project => Some(
                self.cwd
                    .join(Config::project_dir())
                    .join("packages.lock.json"),
            ),
            PackageScope::Temporary => None,
        }
    }

    fn trust_audit_path_for_scope(&self, scope: PackageScope) -> Option<PathBuf> {
        match scope {
            PackageScope::User => Some(Config::global_dir().join("package-trust-audit.jsonl")),
            PackageScope::Project => Some(
                self.cwd
                    .join(Config::project_dir())
                    .join("package-trust-audit.jsonl"),
            ),
            PackageScope::Temporary => None,
        }
    }

    fn verify_and_record_lock(
        &self,
        source: &str,
        scope: PackageScope,
        action: PackageLockAction,
    ) -> Result<()> {
        let Some(lockfile_path) = self.lockfile_path_for_scope(scope) else {
            return Ok(());
        };

        let candidate = self.build_lock_entry(source, scope)?;
        let mut lockfile = read_package_lockfile(&lockfile_path)?;
        let existing_idx = lockfile
            .entries
            .iter()
            .position(|entry| entry.identity == candidate.identity);
        let existing = existing_idx.and_then(|idx| lockfile.entries.get(idx).cloned());

        match evaluate_lock_transition(existing.as_ref(), &candidate, action) {
            Ok(transition) => {
                if let Some(idx) = existing_idx {
                    lockfile.entries[idx] = candidate.clone();
                } else {
                    lockfile.entries.push(candidate.clone());
                }
                sort_lock_entries(&mut lockfile.entries);
                write_package_lockfile_atomic(&lockfile_path, &lockfile)?;

                let event = PackageTrustAuditEvent {
                    schema: PACKAGE_TRUST_AUDIT_SCHEMA,
                    timestamp: chrono::Utc::now()
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    action: match action {
                        PackageLockAction::Install => "install",
                        PackageLockAction::Update => "update",
                    }
                    .to_string(),
                    scope: scope_label(scope).to_string(),
                    source: source.to_string(),
                    identity: candidate.identity.clone(),
                    from_state: transition.from_state,
                    to_state: transition.to_state,
                    reason_codes: transition.reason_codes,
                    remediation: None,
                    details: serde_json::to_value(&candidate)
                        .unwrap_or_else(|_| serde_json::json!({})),
                };
                self.append_trust_audit_event(scope, &event)?;
                Ok(())
            }
            Err(mismatch) => {
                let from_state = existing.as_ref().map_or_else(
                    || "untracked".to_string(),
                    |entry| trust_state_label(entry.trust_state).to_string(),
                );
                let event = PackageTrustAuditEvent {
                    schema: PACKAGE_TRUST_AUDIT_SCHEMA,
                    timestamp: chrono::Utc::now()
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    action: match action {
                        PackageLockAction::Install => "install",
                        PackageLockAction::Update => "update",
                    }
                    .to_string(),
                    scope: scope_label(scope).to_string(),
                    source: source.to_string(),
                    identity: candidate.identity.clone(),
                    from_state,
                    to_state: "rejected".to_string(),
                    reason_codes: vec![mismatch.code.to_string()],
                    remediation: Some(mismatch.remediation.clone()),
                    details: serde_json::to_value(&candidate)
                        .unwrap_or_else(|_| serde_json::json!({})),
                };
                self.append_trust_audit_event(scope, &event)?;

                Err(verification_error(
                    mismatch.code,
                    &mismatch.reason,
                    &mismatch.remediation,
                ))
            }
        }
    }

    /// Prune lockfile entries whose identity no longer appears in the
    /// current `settings.json` package list for the given scope.
    ///
    /// Stale entries are inert for resolution (`resolve_package_sources()`
    /// reads settings, not the lockfile), but they accumulate over time
    /// and confuse operators inspecting the file. This method runs on
    /// startup as an implicit reconciliation so `remove_lock_entry` is
    /// effectively called whenever an extension is removed from
    /// `settings.json`, not only during an explicit `remove_sync()`.
    ///
    /// Each pruned entry is recorded as a `remove` trust-audit event with
    /// `reason_codes: ["reconciled"]` so the prune is auditable.
    ///
    /// Failures are intentionally non-fatal: if the reconcile cannot
    /// succeed (corrupt lockfile, trust-audit write failure, etc.) we
    /// log and return `Ok(vec![])` so the startup path is not blocked.
    /// The stale-entry fallback is always safe — entries never cause
    /// resolution errors.
    fn reconcile_lockfile_sync(&self, scope: PackageScope) -> Result<Vec<PackageLockEntry>> {
        let Some(lockfile_path) = self.lockfile_path_for_scope(scope) else {
            return Ok(Vec::new());
        };
        if !lockfile_path.exists() {
            return Ok(Vec::new());
        }

        let roots = ResolveRoots::from_env(&self.cwd);
        let settings_path = match scope {
            PackageScope::User => &roots.global_settings_path,
            PackageScope::Project => {
                if !roots.project_settings_enabled {
                    // A config override is active, so project-level
                    // settings are intentionally bypassed. We don't
                    // have an authoritative "live" set for the project
                    // scope — pruning based on an empty set would wipe
                    // the project lockfile, which is exactly the wrong
                    // outcome. Skip reconciliation for this scope.
                    return Ok(Vec::new());
                }
                &roots.project_settings_path
            }
            PackageScope::Temporary => return Ok(Vec::new()),
        };
        // `read_settings_json` returns `Ok({})` for missing files, which would
        // otherwise make this function prune every entry in the lockfile.
        // Treat a missing settings file as "no authoritative info" and skip
        // reconciliation for the scope — the user hasn't told us yet what
        // should be live, so leave the lockfile intact.
        if !settings_path.exists() {
            return Ok(Vec::new());
        }
        let live_sources: Vec<PackageEntry> = list_packages_in_settings(settings_path)?;

        let live_identities: std::collections::HashSet<String> = live_sources
            .iter()
            .map(|entry| self.package_identity(&entry.source))
            .collect();

        let mut lockfile = read_package_lockfile(&lockfile_path)?;
        let mut pruned = Vec::new();
        lockfile.entries.retain(|entry| {
            if live_identities.contains(&entry.identity) {
                true
            } else {
                pruned.push(entry.clone());
                false
            }
        });

        if pruned.is_empty() {
            return Ok(Vec::new());
        }

        sort_lock_entries(&mut lockfile.entries);
        write_package_lockfile_atomic(&lockfile_path, &lockfile)?;

        for removed in &pruned {
            let event = PackageTrustAuditEvent {
                schema: PACKAGE_TRUST_AUDIT_SCHEMA,
                timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                action: "remove".to_string(),
                scope: scope_label(scope).to_string(),
                source: removed.source.clone(),
                identity: removed.identity.clone(),
                from_state: trust_state_label(removed.trust_state).to_string(),
                to_state: "removed".to_string(),
                reason_codes: vec!["reconciled".to_string()],
                remediation: None,
                details: serde_json::to_value(removed).unwrap_or_else(|_| serde_json::json!({})),
            };
            if let Err(err) = self.append_trust_audit_event(scope, &event) {
                tracing::warn!(
                    event = "pkg.lockfile.reconcile.audit_failed",
                    scope = scope_label(scope),
                    identity = %removed.identity,
                    error = %err,
                    "Failed to append trust-audit event for reconciled lockfile entry"
                );
            }
        }

        Ok(pruned)
    }

    fn remove_lock_entry(&self, source: &str, scope: PackageScope) -> Result<()> {
        let Some(lockfile_path) = self.lockfile_path_for_scope(scope) else {
            return Ok(());
        };

        let identity = self.package_identity(source);
        let mut lockfile = read_package_lockfile(&lockfile_path)?;
        let Some(idx) = lockfile
            .entries
            .iter()
            .position(|entry| entry.identity == identity)
        else {
            return Ok(());
        };

        let removed = lockfile.entries.remove(idx);
        sort_lock_entries(&mut lockfile.entries);
        write_package_lockfile_atomic(&lockfile_path, &lockfile)?;

        let event = PackageTrustAuditEvent {
            schema: PACKAGE_TRUST_AUDIT_SCHEMA,
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            action: "remove".to_string(),
            scope: scope_label(scope).to_string(),
            source: source.to_string(),
            identity,
            from_state: trust_state_label(removed.trust_state).to_string(),
            to_state: "removed".to_string(),
            reason_codes: vec!["removed".to_string()],
            remediation: None,
            details: serde_json::to_value(&removed).unwrap_or_else(|_| serde_json::json!({})),
        };
        self.append_trust_audit_event(scope, &event)?;
        Ok(())
    }

    fn append_trust_audit_event(
        &self,
        scope: PackageScope,
        event: &PackageTrustAuditEvent,
    ) -> Result<()> {
        let Some(path) = self.trust_audit_path_for_scope(scope) else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let payload = serde_json::to_string(event)?;
        let mut opts = fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(path)?;
        writeln!(file, "{payload}")?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn build_lock_entry(&self, source: &str, scope: PackageScope) -> Result<PackageLockEntry> {
        let source = validate_non_empty_source(source, "Package source")?;
        let parsed = parse_source(source, &self.cwd);
        match parsed {
            ParsedSource::Npm { spec, name, pinned } => {
                let installed_path = self.npm_install_path(&name, scope)?;
                if !installed_path.exists() {
                    return Err(Error::tool(
                        "package_manager",
                        format!(
                            "Installed npm package path is missing for lock verification: {}",
                            installed_path.display()
                        ),
                    ));
                }

                let (_, requested_version) = parse_npm_spec(&spec);
                let installed_version = read_installed_npm_version(&installed_path).ok_or_else(|| {
                    verification_error(
                        "npm_version_missing",
                        &format!(
                            "Missing package.json version for installed npm package at {}",
                            installed_path.display()
                        ),
                        "Reinstall the package (`pi remove <source>` then `pi install <source>`) and retry.",
                    )
                })?;

                if let Some(expected) = requested_version.as_deref().and_then(npm_exact_version) {
                    // npm normalizes exact specs (`v1.2.3` installs as `1.2.3`)
                    // and ignores build metadata, so compare parsed versions by
                    // precedence rather than raw strings.
                    let installed_matches = npm_exact_version(&installed_version)
                        .is_some_and(|installed| installed.cmp_precedence(&expected).is_eq());
                    if !installed_matches {
                        return Err(verification_error(
                            "npm_version_mismatch",
                            &format!(
                                "Pinned npm version mismatch for {name}: expected {expected}, got {installed_version}",
                            ),
                            "Pin the intended version explicitly and reinstall to refresh trusted provenance.",
                        ));
                    }
                }

                let digest_sha256 = digest_package_path(&installed_path)?;
                Ok(PackageLockEntry {
                    identity: self.package_identity(source),
                    source: source.to_string(),
                    source_kind: PackageSourceKind::Npm,
                    resolved: PackageResolvedProvenance::Npm {
                        name,
                        requested_spec: spec,
                        requested_version,
                        installed_version,
                        pinned,
                    },
                    digest_sha256,
                    trust_state: PackageEntryTrustState::Trusted,
                })
            }
            ParsedSource::Git {
                repo,
                host,
                path,
                r#ref,
                pinned,
                ..
            } => {
                let installed_path = self.checked_git_install_path(&host, &path, scope)?;
                if !installed_path.exists() {
                    return Err(Error::tool(
                        "package_manager",
                        format!(
                            "Installed git package path is missing for lock verification: {}",
                            installed_path.display()
                        ),
                    ));
                }

                let resolved_commit =
                    run_command_capture("git", ["rev-parse", "HEAD"], Some(&installed_path))?;
                if let Some(expected_ref) = r#ref.as_ref() {
                    let expected_commit = run_command_capture(
                        "git",
                        ["rev-parse", expected_ref.as_str()],
                        Some(&installed_path),
                    )?;
                    if expected_commit != resolved_commit {
                        return Err(verification_error(
                            "git_ref_mismatch",
                            &format!(
                                "Pinned git ref mismatch for {repo}: ref {expected_ref} resolved to {expected_commit}, but HEAD is {resolved_commit}",
                            ),
                            "Fetch/reset the repo and reinstall from the intended pinned ref.",
                        ));
                    }
                }

                let origin_url = run_command_capture(
                    "git",
                    ["config", "--get", "remote.origin.url"],
                    Some(&installed_path),
                )
                .ok()
                .filter(|value| !value.trim().is_empty());
                let digest_sha256 = digest_package_path(&installed_path)?;

                Ok(PackageLockEntry {
                    identity: self.package_identity(source),
                    source: source.to_string(),
                    source_kind: PackageSourceKind::Git,
                    resolved: PackageResolvedProvenance::Git {
                        repo,
                        host,
                        path,
                        requested_ref: r#ref,
                        resolved_commit,
                        origin_url,
                        pinned,
                    },
                    digest_sha256,
                    trust_state: PackageEntryTrustState::Trusted,
                })
            }
            ParsedSource::Local { path } => {
                if !path.exists() {
                    return Err(Error::config(format!(
                        "Local package path does not exist: {}",
                        path.display()
                    )));
                }

                let digest_sha256 = digest_package_path(&path)?;
                let resolved_path = path
                    .canonicalize()
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();
                Ok(PackageLockEntry {
                    identity: self.package_identity(source),
                    source: source.to_string(),
                    source_kind: PackageSourceKind::Local,
                    resolved: PackageResolvedProvenance::Local { resolved_path },
                    digest_sha256,
                    trust_state: PackageEntryTrustState::Trusted,
                })
            }
        }
    }

    fn project_npm_root(&self) -> PathBuf {
        self.cwd.join(Config::project_dir()).join("npm")
    }

    fn project_git_root(&self) -> PathBuf {
        self.cwd.join(Config::project_dir()).join("git")
    }

    #[allow(clippy::unused_self)]
    fn global_git_root(&self) -> PathBuf {
        Config::global_dir().join("git")
    }

    #[allow(clippy::unused_self)]
    fn global_npm_root(&self) -> Result<PathBuf> {
        let output = Command::new("npm")
            .args(["root", "-g"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| Error::tool("npm", format!("Failed to spawn npm: {e}")))?;

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut msg = String::from("npm root -g failed");
            if let Some(code) = output.status.code() {
                let _ = write!(msg, " (exit {code})");
            }
            if !stdout.trim().is_empty() {
                let _ = write!(msg, "\nstdout:\n{stdout}");
            }
            if !stderr.trim().is_empty() {
                let _ = write!(msg, "\nstderr:\n{stderr}");
            }
            return Err(Error::tool("npm", msg));
        }

        let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if root.is_empty() {
            return Err(Error::tool("npm", "npm root -g returned empty output"));
        }

        Ok(PathBuf::from(root))
    }

    fn npm_prefix_root(&self, scope: PackageScope) -> Option<PathBuf> {
        match scope {
            PackageScope::Project => Some(self.project_npm_root()),
            PackageScope::Temporary => Some(temporary_dir("npm", None)),
            PackageScope::User => None,
        }
    }

    /// Whether resolving this source's install path requires the global npm
    /// root (i.e. it is an npm source installed into npm's global prefix).
    fn source_needs_global_npm_root(&self, entry: &ScopedPackage) -> bool {
        self.npm_prefix_root(entry.scope).is_none()
            && matches!(
                parse_source(entry.pkg.source.trim(), &self.cwd),
                ParsedSource::Npm { .. }
            )
    }

    /// Resolve the global npm root once for a resolution pass.
    ///
    /// Returns `Ok(None)` without spawning any subprocess when no source in
    /// the pass installs into npm's global prefix. Callers hand the result to
    /// [`Self::npm_install_dir`] so a pass runs `npm root -g` at most once
    /// instead of once per package.
    fn pass_global_npm_root_blocking(&self, sources: &[ScopedPackage]) -> Result<Option<PathBuf>> {
        if sources
            .iter()
            .any(|entry| self.source_needs_global_npm_root(entry))
        {
            self.global_npm_root().map(Some)
        } else {
            Ok(None)
        }
    }

    /// Async variant of [`Self::pass_global_npm_root_blocking`] that offloads
    /// the `npm root -g` subprocess (and source parsing I/O) to a thread.
    async fn pass_global_npm_root(&self, sources: &[ScopedPackage]) -> Result<Option<PathBuf>> {
        let this = self.clone();
        let sources = sources.to_vec();
        let (tx, mut rx) = oneshot::channel();

        let handle = thread::spawn(move || {
            let res = this.pass_global_npm_root_blocking(&sources);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        let recv_result = rx.recv(cx.cx()).await;
        finish_package_task(handle, recv_result, "Global npm root task cancelled")
    }

    /// Install directory for an npm package.
    ///
    /// `global_npm_root` is a pre-resolved `npm root -g` result (see
    /// [`Self::pass_global_npm_root_blocking`]); supplying it avoids spawning
    /// the subprocess per package. When absent, the root is resolved on
    /// demand.
    fn npm_install_dir(
        &self,
        name: &str,
        scope: PackageScope,
        global_npm_root: Option<&Path>,
    ) -> Result<PathBuf> {
        if let Some(prefix_root) = self.npm_prefix_root(scope) {
            return Ok(prefix_root.join("node_modules").join(name));
        }
        match global_npm_root {
            Some(root) => Ok(root.join(name)),
            None => Ok(self.global_npm_root()?.join(name)),
        }
    }

    fn npm_install_path(&self, name: &str, scope: PackageScope) -> Result<PathBuf> {
        self.npm_install_dir(name, scope, None)
    }

    fn git_root(&self, scope: PackageScope) -> Option<PathBuf> {
        match scope {
            PackageScope::Temporary => None,
            PackageScope::User => Some(self.global_git_root()),
            PackageScope::Project => Some(self.project_git_root()),
        }
    }

    fn git_install_path(&self, host: &str, repo_path: &str, scope: PackageScope) -> PathBuf {
        match scope {
            PackageScope::Temporary => temporary_dir(&format!("git-{host}"), Some(repo_path)),
            PackageScope::User => self.global_git_root().join(host).join(repo_path),
            PackageScope::Project => self.project_git_root().join(host).join(repo_path),
        }
    }

    fn checked_git_install_path(
        &self,
        host: &str,
        repo_path: &str,
        scope: PackageScope,
    ) -> Result<PathBuf> {
        if host.trim().is_empty() || repo_path.trim().is_empty() {
            return Err(Error::tool(
                "package_manager",
                "Invalid git package source: remote repositories must include both a host and repository path",
            ));
        }

        Ok(self.git_install_path(host, repo_path, scope))
    }

    fn install_npm(&self, spec: &str, scope: PackageScope) -> Result<()> {
        let (name, _) = parse_npm_spec(spec);
        if let Some(install_root) = self.npm_prefix_root(scope) {
            ensure_npm_project(&install_root)?;
            run_command(
                "npm",
                [
                    "install",
                    "--prefix",
                    install_root.to_string_lossy().as_ref(),
                    "--",
                    spec,
                ],
                None,
            )?;
        } else {
            run_command("npm", ["install", "-g", spec], None)?;
        }

        // Basic sanity: installed path exists
        let installed = self.npm_install_path(&name, scope)?;
        if !installed.exists() {
            return Err(Error::tool(
                "npm",
                format!(
                    "npm install succeeded but '{}' is missing",
                    installed.display()
                ),
            ));
        }

        Ok(())
    }

    fn uninstall_npm(&self, name: &str, scope: PackageScope) -> Result<()> {
        let Some(install_root) = self.npm_prefix_root(scope) else {
            run_command("npm", ["uninstall", "-g", "--", name], None)?;
            return Ok(());
        };
        if !install_root.exists() {
            return Ok(());
        }
        run_command(
            "npm",
            [
                "uninstall",
                "--prefix",
                install_root.to_string_lossy().as_ref(),
                "--",
                name,
            ],
            None,
        )?;
        Ok(())
    }

    fn install_git(
        &self,
        repo: &str,
        host: &str,
        repo_path: &str,
        r#ref: Option<&str>,
        scope: PackageScope,
    ) -> Result<()> {
        let target_dir = self.checked_git_install_path(host, repo_path, scope)?;
        if target_dir.exists() {
            return Ok(());
        }

        if let Some(root) = self.git_root(scope) {
            ensure_git_ignore(&root)?;
        }
        if let Some(parent) = target_dir.parent() {
            fs::create_dir_all(parent)?;
        }

        let clone_url = if repo.starts_with("http://") || repo.starts_with("https://") {
            repo.to_string()
        } else if looks_like_local_path(repo) {
            // Allow offline installs from local repos: `git:./repo`, `git:/abs/repo`, `git:file:///repo`.
            repo.to_string()
        } else {
            format!("https://{repo}")
        };

        run_command(
            "git",
            [
                "clone",
                "--",
                &clone_url,
                target_dir.to_string_lossy().as_ref(),
            ],
            None,
        )?;

        if let Some(r#ref) = r#ref {
            run_command("git", ["checkout", r#ref], Some(&target_dir))?;
        }

        if target_dir.join("package.json").exists() {
            run_command("npm", ["install"], Some(&target_dir))?;
        }

        Ok(())
    }

    fn update_git(
        &self,
        repo: &str,
        host: &str,
        repo_path: &str,
        scope: PackageScope,
    ) -> Result<()> {
        if scope == PackageScope::Temporary {
            // Temporary installs are ephemeral; callers should reinstall if needed.
            return Ok(());
        }

        let target_dir = self.checked_git_install_path(host, repo_path, scope)?;
        if !target_dir.exists() {
            return self.install_git(repo, host, repo_path, None, scope);
        }

        run_command("git", ["fetch", "--prune", "origin"], Some(&target_dir))?;
        run_command("git", ["reset", "--hard", "@{upstream}"], Some(&target_dir))?;
        run_command("git", ["clean", "-fdx"], Some(&target_dir))?;

        if target_dir.join("package.json").exists() {
            run_command("npm", ["install"], Some(&target_dir))?;
        }

        Ok(())
    }

    fn remove_git(&self, host: &str, repo_path: &str, scope: PackageScope) -> Result<()> {
        let target_dir = self.checked_git_install_path(host, repo_path, scope)?;
        if !target_dir.exists() {
            return Ok(());
        }

        fs::remove_dir_all(&target_dir)?;
        if let Some(root) = self.git_root(scope) {
            prune_empty_git_parents(&target_dir, &root);
        }
        Ok(())
    }
}

// ============================================================================
// Resource resolution (pi-mono parity)
// ============================================================================

#[derive(Debug, Clone, Default)]
pub struct ResolveExtensionSourcesOptions {
    pub local: bool,
    pub temporary: bool,
}

#[derive(Debug, Clone)]
struct PackageSpec {
    source: String,
    filter: Option<PackageFilter>,
}

#[derive(Debug, Clone, Default)]
struct SettingsSnapshot {
    packages: Vec<PackageSpec>,
    extensions: Vec<String>,
    skills: Vec<String>,
    prompts: Vec<String>,
    themes: Vec<String>,
}

impl SettingsSnapshot {
    fn entries_for(&self, resource_type: ResourceType) -> &[String] {
        match resource_type {
            ResourceType::Extensions => &self.extensions,
            ResourceType::Skills => &self.skills,
            ResourceType::Prompts => &self.prompts,
            ResourceType::Themes => &self.themes,
        }
    }
}

fn read_settings_snapshot(path: &Path) -> Result<SettingsSnapshot> {
    let value = read_settings_json(path)?;
    let packages_value = value
        .get("packages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut packages = Vec::new();
    for pkg in &packages_value {
        if let Some(spec) = extract_package_spec(pkg) {
            packages.push(spec);
        }
    }

    Ok(SettingsSnapshot {
        packages,
        extensions: extract_string_array(value.get("extensions")),
        skills: extract_string_array(value.get("skills")),
        prompts: extract_string_array(value.get("prompts")),
        themes: extract_string_array(value.get("themes")),
    })
}

fn read_project_settings_snapshot(roots: &ResolveRoots) -> Result<SettingsSnapshot> {
    if roots.project_settings_enabled {
        read_settings_snapshot(&roots.project_settings_path)
    } else {
        Ok(SettingsSnapshot::default())
    }
}

fn extract_string_array(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn extract_package_spec(value: &Value) -> Option<PackageSpec> {
    if let Some(s) = value.as_str() {
        let source = s.trim();
        if source.is_empty() {
            return None;
        }
        return Some(PackageSpec {
            source: source.to_string(),
            filter: None,
        });
    }

    let obj = value.as_object()?;
    let source = obj.get("source")?.as_str()?.trim().to_string();
    if source.is_empty() {
        return None;
    }

    let filter = PackageFilter {
        extensions: extract_filter_field(obj, "extensions"),
        skills: extract_filter_field(obj, "skills"),
        prompts: extract_filter_field(obj, "prompts"),
        themes: extract_filter_field(obj, "themes"),
    };

    Some(PackageSpec {
        source,
        filter: Some(filter),
    })
}

fn extract_filter_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<Vec<String>> {
    if !obj.contains_key(key) {
        return None;
    }

    match obj.get(key) {
        Some(Value::String(s)) => Some(vec![s.clone()]),
        Some(Value::Array(arr)) => Some(
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
        ),
        _ => Some(Vec::new()),
    }
}

#[derive(Debug, Clone)]
struct ScopedPackage {
    pkg: PackageSpec,
    scope: PackageScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceType {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

impl ResourceType {
    const fn all() -> [Self; 4] {
        [Self::Extensions, Self::Skills, Self::Prompts, Self::Themes]
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Extensions => "extensions",
            Self::Skills => "skills",
            Self::Prompts => "prompts",
            Self::Themes => "themes",
        }
    }
}

#[derive(Debug, Default, Clone)]
struct ResourceAccumulator {
    extensions: ResourceList,
    skills: ResourceList,
    prompts: ResourceList,
    themes: ResourceList,
}

impl ResourceAccumulator {
    fn new() -> Self {
        Self::default()
    }

    #[allow(clippy::missing_const_for_fn)] // const fn with &mut is unstable
    fn target_mut(&mut self, resource_type: ResourceType) -> &mut ResourceList {
        match resource_type {
            ResourceType::Extensions => &mut self.extensions,
            ResourceType::Skills => &mut self.skills,
            ResourceType::Prompts => &mut self.prompts,
            ResourceType::Themes => &mut self.themes,
        }
    }

    fn into_resolved_paths(mut self) -> ResolvedPaths {
        for items in [
            &mut self.extensions.items,
            &mut self.skills.items,
            &mut self.prompts.items,
            &mut self.themes.items,
        ] {
            items.sort_by(|a, b| a.path.to_string_lossy().cmp(&b.path.to_string_lossy()));
        }

        ResolvedPaths {
            extensions: self.extensions.items,
            skills: self.skills.items,
            prompts: self.prompts.items,
            themes: self.themes.items,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct ResourceList {
    seen: std::collections::HashSet<String>,
    items: Vec<ResolvedResource>,
}

impl ResourceList {
    fn add(&mut self, path: PathBuf, metadata: &PathMetadata, enabled: bool) {
        let key = path.to_string_lossy().to_string();
        if !self.seen.insert(key) {
            return;
        }
        self.items.push(ResolvedResource {
            path,
            enabled,
            metadata: metadata.clone(),
        });
    }
}

impl PackageManager {
    fn dedupe_packages(&self, packages: Vec<ScopedPackage>) -> Vec<ScopedPackage> {
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut out: Vec<ScopedPackage> = Vec::new();

        for entry in packages {
            let source = entry.pkg.source.trim();
            if source.is_empty() {
                continue;
            }
            let identity = self.package_identity(source);
            if let Some(&idx) = seen.get(&identity) {
                let existing_scope = out[idx].scope;
                if entry.scope == PackageScope::Project && existing_scope == PackageScope::User {
                    out[idx] = entry;
                }
                continue;
            }

            seen.insert(identity, out.len());
            out.push(entry);
        }

        out
    }

    async fn resolve_package_sources(
        &self,
        sources: &[ScopedPackage],
        global_npm_root: Option<&Path>,
        accumulator: &mut ResourceAccumulator,
    ) -> Result<()> {
        for entry in sources {
            let source_str = entry.pkg.source.trim();
            if source_str.is_empty() {
                continue;
            }

            let parsed = parse_source(source_str, &self.cwd);
            let mut metadata = PathMetadata {
                source: source_str.to_string(),
                scope: entry.scope,
                origin: ResourceOrigin::Package,
                base_dir: None,
            };

            match parsed {
                ParsedSource::Local { path } => {
                    Self::resolve_local_extension_source(
                        &path,
                        accumulator,
                        entry.pkg.filter.as_ref(),
                        &mut metadata,
                        entry.scope == PackageScope::Temporary,
                    )?;
                }
                ParsedSource::Npm { spec, name, .. } => {
                    let installed_path =
                        self.npm_install_dir(&name, entry.scope, global_npm_root)?;

                    let needs_install = !installed_path.exists()
                        || Box::pin(npm_needs_update(&spec, &installed_path)).await;
                    if needs_install {
                        self.install(source_str, entry.scope).await?;
                    }

                    metadata.base_dir = Some(installed_path.clone());
                    Self::collect_package_resources(
                        &installed_path,
                        accumulator,
                        entry.pkg.filter.as_ref(),
                        &metadata,
                    )?;
                }
                ParsedSource::Git {
                    repo: _,
                    host,
                    path,
                    ..
                } => {
                    // Offload git_install_path
                    let installed_path =
                        self.checked_git_install_path(&host, &path, entry.scope)?;

                    if !installed_path.exists() {
                        self.install(source_str, entry.scope).await?;
                    }

                    metadata.base_dir = Some(installed_path.clone());
                    Self::collect_package_resources(
                        &installed_path,
                        accumulator,
                        entry.pkg.filter.as_ref(),
                        &metadata,
                    )?;
                }
            }
        }

        Ok(())
    }

    fn resolve_local_extension_source(
        resolved: &Path,
        accumulator: &mut ResourceAccumulator,
        filter: Option<&PackageFilter>,
        metadata: &mut PathMetadata,
        strict: bool,
    ) -> Result<()> {
        if !resolved.exists() {
            if strict {
                return Err(Error::config(format!(
                    "Extension source '{}' does not exist",
                    resolved.display()
                )));
            }
            return Ok(());
        }

        let stats = match fs::metadata(resolved) {
            Ok(stats) => stats,
            Err(err) => {
                if strict {
                    return Err(Error::config(format!(
                        "Failed to inspect extension source '{}': {err}",
                        resolved.display()
                    )));
                }
                return Ok(());
            }
        };

        if stats.is_file() {
            if !is_supported_extension_file(resolved) {
                let message = format!(
                    "Unsupported extension source file '{}'; use extension.json, JS/TS entrypoints, *.native.json, or *.wasm",
                    resolved.display()
                );
                if strict {
                    return Err(Error::config(message));
                }
                warn!(path = %resolved.display(), "{message}");
                return Ok(());
            }
            metadata.base_dir = resolved.parent().map(Path::to_path_buf);
            accumulator
                .extensions
                .add(resolved.to_path_buf(), metadata, true);
            return Ok(());
        }

        if !stats.is_dir() {
            if strict {
                return Err(Error::config(format!(
                    "Extension source '{}' is neither a file nor a directory",
                    resolved.display()
                )));
            }
            return Ok(());
        }

        metadata.base_dir = Some(resolved.to_path_buf());
        let had_any = Self::collect_package_resources(resolved, accumulator, filter, metadata)?;
        if !had_any {
            accumulator
                .extensions
                .add(resolved.to_path_buf(), metadata, true);
        }
        Ok(())
    }

    fn resolve_local_entries(
        entries: &[String],
        resource_type: ResourceType,
        target: &mut ResourceList,
        metadata: &PathMetadata,
        base_dir: &Path,
    ) {
        if entries.is_empty() {
            return;
        }

        let (plain, patterns) = split_patterns(entries);
        let resolved_plain = plain
            .iter()
            .map(|p| resolve_path_from_base(p, base_dir))
            .collect::<Vec<_>>();
        let all_files = collect_files_from_paths(&resolved_plain, resource_type);
        let enabled_paths = apply_patterns(&all_files, &patterns, base_dir);

        for f in all_files {
            let enabled = enabled_paths.contains(&f);
            target.add(f, metadata, enabled);
        }
    }

    #[allow(clippy::unused_self)]
    fn add_auto_discovered_resources(
        &self,
        accumulator: &mut ResourceAccumulator,
        global: &SettingsSnapshot,
        project: &SettingsSnapshot,
        global_base_dir: &Path,
        project_base_dir: &Path,
        project_settings_enabled: bool,
    ) {
        let user_metadata = PathMetadata {
            source: "auto".to_string(),
            scope: PackageScope::User,
            origin: ResourceOrigin::TopLevel,
            base_dir: Some(global_base_dir.to_path_buf()),
        };
        let project_metadata = PathMetadata {
            source: "auto".to_string(),
            scope: PackageScope::Project,
            origin: ResourceOrigin::TopLevel,
            base_dir: Some(project_base_dir.to_path_buf()),
        };

        let user_dirs = AutoDirs::new(global_base_dir);
        let project_dirs = AutoDirs::new(project_base_dir);

        for resource_type in ResourceType::all() {
            let target = accumulator.target_mut(resource_type);
            let (user_paths, user_overrides) = match resource_type {
                ResourceType::Extensions => (
                    collect_auto_extension_entries(&user_dirs.extensions),
                    &global.extensions,
                ),
                ResourceType::Skills => (
                    collect_auto_skill_entries(&user_dirs.skills),
                    &global.skills,
                ),
                ResourceType::Prompts => (
                    collect_auto_prompt_entries(&user_dirs.prompts),
                    &global.prompts,
                ),
                ResourceType::Themes => (
                    collect_auto_theme_entries(&user_dirs.themes),
                    &global.themes,
                ),
            };
            for path in user_paths {
                let enabled = is_enabled_by_overrides(&path, user_overrides, global_base_dir);
                target.add(path, &user_metadata, enabled);
            }

            if project_settings_enabled {
                let (project_paths, project_overrides) = match resource_type {
                    ResourceType::Extensions => (
                        collect_auto_extension_entries(&project_dirs.extensions),
                        &project.extensions,
                    ),
                    ResourceType::Skills => (
                        collect_auto_skill_entries(&project_dirs.skills),
                        &project.skills,
                    ),
                    ResourceType::Prompts => (
                        collect_auto_prompt_entries(&project_dirs.prompts),
                        &project.prompts,
                    ),
                    ResourceType::Themes => (
                        collect_auto_theme_entries(&project_dirs.themes),
                        &project.themes,
                    ),
                };
                for path in project_paths {
                    let enabled =
                        is_enabled_by_overrides(&path, project_overrides, project_base_dir);
                    target.add(path, &project_metadata, enabled);
                }
            }
        }
    }

    fn collect_package_resources(
        package_root: &Path,
        accumulator: &mut ResourceAccumulator,
        filter: Option<&PackageFilter>,
        metadata: &PathMetadata,
    ) -> Result<bool> {
        if let Some(filter) = filter {
            for resource_type in ResourceType::all() {
                let target = accumulator.target_mut(resource_type);
                let patterns = match resource_type {
                    ResourceType::Extensions => filter.extensions.as_ref(),
                    ResourceType::Skills => filter.skills.as_ref(),
                    ResourceType::Prompts => filter.prompts.as_ref(),
                    ResourceType::Themes => filter.themes.as_ref(),
                };

                if let Some(patterns) = patterns {
                    Self::apply_package_filter(
                        package_root,
                        patterns,
                        resource_type,
                        target,
                        metadata,
                    )?;
                } else {
                    Self::collect_default_resources(package_root, resource_type, target, metadata)?;
                }
            }
            return Ok(true);
        }

        if let Some(manifest) = read_pi_manifest(package_root)? {
            for resource_type in ResourceType::all() {
                let entries = manifest.entries_for(resource_type);
                Self::add_manifest_entries(
                    entries.as_deref(),
                    package_root,
                    resource_type,
                    accumulator.target_mut(resource_type),
                    metadata,
                );
            }
            return Ok(true);
        }

        let mut has_any_dir = false;
        for resource_type in ResourceType::all() {
            let dir = package_root.join(resource_type.as_str());
            if dir.exists() {
                let files = collect_resource_files(&dir, resource_type);
                let target = accumulator.target_mut(resource_type);
                for f in files {
                    target.add(f, metadata, true);
                }
                has_any_dir = true;
            }
        }

        Ok(has_any_dir)
    }

    fn collect_default_resources(
        package_root: &Path,
        resource_type: ResourceType,
        target: &mut ResourceList,
        metadata: &PathMetadata,
    ) -> Result<()> {
        if let Some(manifest) = read_pi_manifest(package_root)?
            && let Some(entries) = manifest.entries_for(resource_type)
        {
            Self::add_manifest_entries(
                Some(&entries),
                package_root,
                resource_type,
                target,
                metadata,
            );
            return Ok(());
        }

        let dir = package_root.join(resource_type.as_str());
        if dir.exists() {
            let files = collect_resource_files(&dir, resource_type);
            for f in files {
                target.add(f, metadata, true);
            }
        }
        Ok(())
    }

    fn apply_package_filter(
        package_root: &Path,
        user_patterns: &[String],
        resource_type: ResourceType,
        target: &mut ResourceList,
        metadata: &PathMetadata,
    ) -> Result<()> {
        let (all_files, _) = Self::collect_manifest_files(package_root, resource_type)?;

        if user_patterns.is_empty() {
            for f in all_files {
                target.add(f, metadata, false);
            }
            return Ok(());
        }

        let enabled_by_user = apply_patterns(&all_files, user_patterns, package_root);
        for f in all_files {
            let enabled = enabled_by_user.contains(&f);
            target.add(f, metadata, enabled);
        }
        Ok(())
    }

    fn collect_manifest_files(
        package_root: &Path,
        resource_type: ResourceType,
    ) -> Result<(Vec<PathBuf>, std::collections::HashSet<PathBuf>)> {
        if let Some(manifest) = read_pi_manifest(package_root)?
            && let Some(entries) = manifest.entries_for(resource_type)
        {
            if entries.is_empty() {
                return Ok((Vec::new(), std::collections::HashSet::new()));
            }
            let all_files =
                collect_files_from_manifest_entries(&entries, package_root, resource_type);
            let patterns = entries
                .iter()
                .filter(|e| is_pattern(e))
                .cloned()
                .collect::<Vec<_>>();
            let enabled_by_manifest = if patterns.is_empty() {
                all_files
                    .iter()
                    .cloned()
                    .collect::<std::collections::HashSet<_>>()
            } else {
                apply_patterns(&all_files, &patterns, package_root)
            };
            let mut enabled_vec = enabled_by_manifest.iter().cloned().collect::<Vec<_>>();
            enabled_vec.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
            return Ok((enabled_vec, enabled_by_manifest));
        }

        let convention_dir = package_root.join(resource_type.as_str());
        if !convention_dir.exists() {
            return Ok((Vec::new(), std::collections::HashSet::new()));
        }
        let all_files = collect_resource_files(&convention_dir, resource_type);
        let set = all_files.iter().cloned().collect();
        Ok((all_files, set))
    }

    fn add_manifest_entries(
        entries: Option<&[String]>,
        root: &Path,
        resource_type: ResourceType,
        target: &mut ResourceList,
        metadata: &PathMetadata,
    ) {
        let Some(entries) = entries else {
            return;
        };
        if entries.is_empty() {
            return;
        }

        let all_files = collect_files_from_manifest_entries(entries, root, resource_type);
        let patterns = entries
            .iter()
            .filter(|e| is_pattern(e))
            .cloned()
            .collect::<Vec<_>>();
        let enabled_paths = apply_patterns(&all_files, &patterns, root);

        for f in all_files {
            if enabled_paths.contains(&f) {
                target.add(f, metadata, true);
            }
        }
    }
}

#[derive(Debug, Default)]
struct AutoDirs {
    extensions: PathBuf,
    skills: PathBuf,
    prompts: PathBuf,
    themes: PathBuf,
}

impl AutoDirs {
    fn new(base_dir: &Path) -> Self {
        Self {
            extensions: base_dir.join("extensions"),
            skills: base_dir.join("skills"),
            prompts: base_dir.join("prompts"),
            themes: base_dir.join("themes"),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct PiManifest {
    extensions: Option<Vec<String>>,
    skills: Option<Vec<String>>,
    prompts: Option<Vec<String>>,
    themes: Option<Vec<String>>,
}

impl PiManifest {
    fn entries_for(&self, resource_type: ResourceType) -> Option<Vec<String>> {
        match resource_type {
            ResourceType::Extensions => self.extensions.clone(),
            ResourceType::Skills => self.skills.clone(),
            ResourceType::Prompts => self.prompts.clone(),
            ResourceType::Themes => self.themes.clone(),
        }
    }
}

fn parse_manifest_entries_field(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    manifest_path: &Path,
    package_root: &Path,
) -> Result<Option<Vec<String>>> {
    let Some(value) = obj.get(key) else {
        return Ok(None);
    };

    match value {
        Value::String(entry) => Ok(Some(vec![validate_manifest_entry(
            package_root,
            manifest_path,
            key,
            entry,
        )?])),
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for entry in arr {
                let Some(entry) = entry.as_str() else {
                    return Err(Error::config(format!(
                        "Invalid package manifest {}: `pi.{key}` must be a string or array of strings",
                        manifest_path.display()
                    )));
                };
                out.push(validate_manifest_entry(
                    package_root,
                    manifest_path,
                    key,
                    entry,
                )?);
            }

            Ok(Some(out))
        }
        _ => Err(Error::config(format!(
            "Invalid package manifest {}: `pi.{key}` must be a string or array of strings",
            manifest_path.display()
        ))),
    }
}

fn validate_manifest_entry(
    package_root: &Path,
    manifest_path: &Path,
    field_name: &str,
    entry: &str,
) -> Result<String> {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return Err(Error::config(format!(
            "Invalid package manifest {}: `pi.{field_name}` entries must be non-empty paths",
            manifest_path.display()
        )));
    }

    let path_part = trimmed
        .strip_prefix('!')
        .or_else(|| trimmed.strip_prefix('+'))
        .or_else(|| trimmed.strip_prefix('-'))
        .unwrap_or(trimmed);
    if path_part.is_empty() {
        return Err(Error::config(format!(
            "Invalid package manifest {}: `pi.{field_name}` entries must be non-empty paths",
            manifest_path.display()
        )));
    }

    let relative = Path::new(path_part);
    if relative.is_absolute() {
        return Err(Error::config(format!(
            "Invalid package manifest {}: `pi.{field_name}` paths must stay within the package root",
            manifest_path.display()
        )));
    }

    let mut depth = 0usize;
    for component in relative.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(_) => depth = depth.saturating_add(1),
            std::path::Component::ParentDir => {
                if depth == 0 {
                    return Err(Error::config(format!(
                        "Invalid package manifest {}: `pi.{field_name}` paths must stay within the package root",
                        manifest_path.display()
                    )));
                }
                depth -= 1;
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(Error::config(format!(
                    "Invalid package manifest {}: `pi.{field_name}` paths must stay within the package root",
                    manifest_path.display()
                )));
            }
        }
    }

    let resolved = package_root.join(relative);
    if resolved.exists() && !manifest_path_within_root(&resolved, package_root) {
        return Err(Error::config(format!(
            "Invalid package manifest {}: `pi.{field_name}` paths must stay within the package root",
            manifest_path.display()
        )));
    }

    Ok(trimmed.to_string())
}

fn manifest_path_within_root(target: &Path, root: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    let Ok(target) = target.canonicalize() else {
        return false;
    };
    target == root || target.starts_with(&root)
}

fn read_pi_manifest(package_root: &Path) -> Result<Option<PiManifest>> {
    let manifest_path = package_root.join("package.json");
    if !manifest_path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&manifest_path).map_err(|err| {
        Error::config(format!(
            "Failed to read package manifest {}: {err}",
            manifest_path.display()
        ))
    })?;
    let json: Value = serde_json::from_str(&raw).map_err(|err| {
        Error::config(format!(
            "Failed to parse package manifest {}: {err}",
            manifest_path.display()
        ))
    })?;
    let Some(pi) = json.get("pi") else {
        return Ok(None);
    };
    let Some(obj) = pi.as_object() else {
        return Err(Error::config(format!(
            "Invalid package manifest {}: `pi` must be an object",
            manifest_path.display()
        )));
    };

    Ok(Some(PiManifest {
        extensions: parse_manifest_entries_field(obj, "extensions", &manifest_path, package_root)?,
        skills: parse_manifest_entries_field(obj, "skills", &manifest_path, package_root)?,
        prompts: parse_manifest_entries_field(obj, "prompts", &manifest_path, package_root)?,
        themes: parse_manifest_entries_field(obj, "themes", &manifest_path, package_root)?,
    }))
}

fn temporary_dir(prefix: &str, suffix: Option<&str>) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(format!("{prefix}-{}", suffix.unwrap_or("")));
    let digest = hasher.finalize();
    let short = hex_encode(&digest)[..8].to_string();

    let mut dir = std::env::temp_dir()
        .join("pi-extensions")
        .join(prefix)
        .join(short);
    if let Some(suffix) = suffix {
        dir = dir.join(suffix);
    }
    dir
}

fn hex_encode(bytes: &[u8]) -> String {
    const LUT: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for &b in bytes {
        out.push(LUT[(b >> 4) as usize] as char);
        out.push(LUT[(b & 0x0f) as usize] as char);
    }
    out
}

fn resolve_path_from_base(input: &str, base_dir: &Path) -> PathBuf {
    let trimmed = input.trim();
    if trimmed == "~" {
        return dirs::home_dir().unwrap_or_else(|| base_dir.to_path_buf());
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return dirs::home_dir()
            .unwrap_or_else(|| base_dir.to_path_buf())
            .join(rest);
    }
    if trimmed.starts_with('~') {
        return dirs::home_dir()
            .unwrap_or_else(|| base_dir.to_path_buf())
            .join(trimmed.trim_start_matches('~'));
    }

    let p = Path::new(trimmed);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    base_dir.join(p)
}

fn is_pattern(s: &str) -> bool {
    s.starts_with('!')
        || s.starts_with('+')
        || s.starts_with('-')
        || s.contains('*')
        || s.contains('?')
}

fn split_patterns(entries: &[String]) -> (Vec<String>, Vec<String>) {
    let mut plain = Vec::new();
    let mut patterns = Vec::new();
    for entry in entries {
        if is_pattern(entry) {
            patterns.push(entry.clone());
        } else {
            plain.push(entry.clone());
        }
    }
    (plain, patterns)
}

fn posix_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn relative_posix(base: &Path, path: &Path) -> String {
    let base_components = base.components().collect::<Vec<_>>();
    let path_components = path.components().collect::<Vec<_>>();

    let mut i = 0usize;
    while i < base_components.len()
        && i < path_components.len()
        && base_components[i] == path_components[i]
    {
        i += 1;
    }

    if i == 0 {
        return posix_string(path);
    }

    let mut rel = PathBuf::new();
    for _ in i..base_components.len() {
        rel.push("..");
    }
    for comp in path_components.iter().skip(i) {
        rel.push(comp.as_os_str());
    }
    posix_string(&rel)
}

fn normalize_exact_pattern(pattern: &str) -> &str {
    pattern
        .strip_prefix("./")
        .or_else(|| pattern.strip_prefix(".\\"))
        .unwrap_or(pattern)
}

fn pattern_matches(pattern: &str, candidate: &str) -> bool {
    let normalized_pattern = pattern.replace('\\', "/");
    let candidate = candidate.replace('\\', "/");
    glob::Pattern::new(&normalized_pattern).is_ok_and(|p| p.matches(&candidate))
}

fn matches_any_pattern(file_path: &Path, patterns: &[String], base_dir: &Path) -> bool {
    let rel = relative_posix(base_dir, file_path);
    let name = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let file_str = posix_string(file_path);

    let is_skill_file = name == "SKILL.md";
    let parent_dir = is_skill_file.then(|| file_path.parent().unwrap_or_else(|| Path::new(".")));
    let parent_dir_str = parent_dir.map(posix_string);
    let parent_rel = parent_dir.map(|p| relative_posix(base_dir, p));
    let parent_name = parent_dir
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str());

    for pattern in patterns {
        if pattern_matches(pattern, &rel)
            || pattern_matches(pattern, name)
            || pattern_matches(pattern, &file_str)
        {
            return true;
        }
        if !is_skill_file {
            continue;
        }
        if parent_rel
            .as_ref()
            .is_some_and(|s| pattern_matches(pattern, s))
        {
            return true;
        }
        if parent_name.is_some_and(|s| pattern_matches(pattern, s)) {
            return true;
        }
        if parent_dir_str
            .as_ref()
            .is_some_and(|s| pattern_matches(pattern, s))
        {
            return true;
        }
    }
    false
}

fn matches_any_exact_pattern(file_path: &Path, patterns: &[String], base_dir: &Path) -> bool {
    if patterns.is_empty() {
        return false;
    }

    let rel = relative_posix(base_dir, file_path);
    let file_str = posix_string(file_path);

    let name = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let is_skill_file = name == "SKILL.md";
    let parent_dir = is_skill_file.then(|| file_path.parent().unwrap_or_else(|| Path::new(".")));
    let parent_dir_str = parent_dir.map(posix_string);
    let parent_rel = parent_dir.map(|p| relative_posix(base_dir, p));

    patterns.iter().any(|pattern| {
        let normalized = normalize_exact_pattern(pattern);
        if normalized == rel || normalized == file_str {
            return true;
        }
        if !is_skill_file {
            return false;
        }
        parent_rel.as_ref().is_some_and(|p| normalized == p)
            || parent_dir_str.as_ref().is_some_and(|p| normalized == p)
    })
}

fn get_override_patterns(entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .filter(|p| p.starts_with('!') || p.starts_with('+') || p.starts_with('-'))
        .cloned()
        .collect()
}

fn is_enabled_by_overrides(path: &Path, patterns: &[String], base_dir: &Path) -> bool {
    let overrides = get_override_patterns(patterns);
    let excludes = overrides
        .iter()
        .filter_map(|p| p.strip_prefix('!').map(str::to_string))
        .collect::<Vec<_>>();
    let force_includes = overrides
        .iter()
        .filter_map(|p| p.strip_prefix('+').map(str::to_string))
        .collect::<Vec<_>>();
    let force_excludes = overrides
        .iter()
        .filter_map(|p| p.strip_prefix('-').map(str::to_string))
        .collect::<Vec<_>>();

    // Priority: force_excludes > force_includes > excludes
    if !force_excludes.is_empty() && matches_any_exact_pattern(path, &force_excludes, base_dir) {
        false
    } else if !force_includes.is_empty()
        && matches_any_exact_pattern(path, &force_includes, base_dir)
    {
        true
    } else {
        excludes.is_empty() || !matches_any_pattern(path, &excludes, base_dir)
    }
}

fn apply_patterns(
    all_paths: &[PathBuf],
    patterns: &[String],
    base_dir: &Path,
) -> std::collections::HashSet<PathBuf> {
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    let mut force_includes = Vec::new();
    let mut force_excludes = Vec::new();

    for p in patterns {
        if let Some(rest) = p.strip_prefix('+') {
            force_includes.push(rest.to_string());
        } else if let Some(rest) = p.strip_prefix('-') {
            force_excludes.push(rest.to_string());
        } else if let Some(rest) = p.strip_prefix('!') {
            excludes.push(rest.to_string());
        } else {
            includes.push(p.clone());
        }
    }

    let mut result: Vec<PathBuf> = if includes.is_empty() {
        all_paths.to_vec()
    } else {
        all_paths
            .iter()
            .filter(|p| matches_any_pattern(p, &includes, base_dir))
            .cloned()
            .collect()
    };

    if !excludes.is_empty() {
        result.retain(|p| !matches_any_pattern(p, &excludes, base_dir));
    }

    if !force_includes.is_empty() {
        for p in all_paths {
            if !result.contains(p) && matches_any_exact_pattern(p, &force_includes, base_dir) {
                result.push(p.clone());
            }
        }
    }

    if !force_excludes.is_empty() {
        result.retain(|p| !matches_any_exact_pattern(p, &force_excludes, base_dir));
    }

    result.into_iter().collect()
}

fn collect_resource_files(dir: &Path, resource_type: ResourceType) -> Vec<PathBuf> {
    match resource_type {
        ResourceType::Skills => collect_skill_entries(dir),
        ResourceType::Extensions => collect_auto_extension_entries(dir),
        ResourceType::Prompts => collect_files_recursive(dir, "md"),
        ResourceType::Themes => collect_files_recursive(dir, "json"),
    }
}

fn collect_files_from_paths(paths: &[PathBuf], resource_type: ResourceType) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        if !p.exists() {
            continue;
        }
        let Ok(stats) = fs::metadata(p) else {
            continue;
        };
        if stats.is_file() {
            if resource_type == ResourceType::Extensions && !is_supported_extension_file(p) {
                warn!(
                    path = %p.display(),
                    "Ignoring unsupported extension file entry; use extension.json, JS/TS entrypoints, *.native.json, or *.wasm"
                );
                continue;
            }
            out.push(p.clone());
        } else if stats.is_dir() {
            out.extend(collect_resource_files(p, resource_type));
        }
    }
    out
}

fn collect_files_from_manifest_entries(
    entries: &[String],
    root: &Path,
    resource_type: ResourceType,
) -> Vec<PathBuf> {
    if resource_type == ResourceType::Extensions {
        return collect_extension_manifest_entries(entries, root);
    }

    let plain = entries
        .iter()
        .filter(|e| !is_pattern(e))
        .cloned()
        .collect::<Vec<_>>();
    let resolved = plain
        .iter()
        .map(|entry| {
            let p = Path::new(entry);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                root.join(entry)
            }
        })
        .collect::<Vec<_>>();

    collect_files_from_paths(&resolved, resource_type)
}

fn collect_extension_manifest_entries(entries: &[String], root: &Path) -> Vec<PathBuf> {
    let plain = entries
        .iter()
        .filter(|entry| !is_pattern(entry))
        .map(|entry| root.join(entry))
        .collect::<Vec<_>>();

    let mut out = Vec::new();
    let root_identity = canonical_identity_path(root);
    for path in plain {
        if !path.exists() {
            continue;
        }

        let Ok(stats) = fs::metadata(&path) else {
            continue;
        };
        if stats.is_file() {
            if !is_supported_extension_file(&path) {
                warn!(
                    path = %path.display(),
                    "Ignoring unsupported package.json#pi.extensions entry; use extension.json, JS/TS entrypoints, *.native.json, or *.wasm"
                );
                continue;
            }
            out.push(path);
            continue;
        }

        if !stats.is_dir() {
            continue;
        }

        if canonical_identity_path(&path) == root_identity {
            out.push(path);
            continue;
        }

        out.extend(collect_auto_extension_entries(&path));
    }

    out
}

fn collect_files_recursive(dir: &Path, ext: &str) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }

    let mut builder = ignore::WalkBuilder::new(dir);
    builder
        .hidden(true)
        .follow_links(true)
        .git_global(false)
        .git_exclude(false)
        .add_custom_ignore_filename(".fdignore")
        .filter_entry(|e| e.file_name() != std::ffi::OsStr::new("node_modules"));

    let mut out = Vec::new();
    for entry in builder.build().filter_map(std::result::Result::ok) {
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case(ext))
        {
            out.push(path.to_path_buf());
        }
    }
    out
}

fn canonical_identity_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn collect_skill_entries(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }

    let visited_dirs = std::sync::Mutex::new(std::collections::HashSet::new());
    let mut out = Vec::new();
    let mut seen_files = std::collections::HashSet::new();

    let mut builder = ignore::WalkBuilder::new(dir);
    builder
        .hidden(true)
        .follow_links(true)
        .git_global(false)
        .git_exclude(false)
        .add_custom_ignore_filename(".fdignore")
        .filter_entry(move |entry| {
            let name = entry.file_name().to_string_lossy();
            if name == "node_modules" {
                return false;
            }

            if !entry.path().is_dir() {
                return true;
            }

            let canonical_dir = canonical_identity_path(entry.path());
            visited_dirs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(canonical_dir)
        });

    for entry in builder.build().filter_map(std::result::Result::ok) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let rel = path.strip_prefix(dir).unwrap_or(path);
        let depth = rel.components().count();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let is_top_level_md = depth == 1 && path.extension().and_then(|e| e.to_str()) == Some("md");
        let is_nested_skill = depth > 1 && name == "SKILL.md";
        if !is_top_level_md && !is_nested_skill {
            continue;
        }

        let canonical_file = canonical_identity_path(path);
        if seen_files.insert(canonical_file) {
            out.push(path.to_path_buf());
        }
    }

    out
}

fn collect_auto_skill_entries(dir: &Path) -> Vec<PathBuf> {
    collect_skill_entries(dir)
}

fn collect_auto_prompt_entries(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !dir.exists() {
        return out;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let Ok(stats) = fs::metadata(&path) else {
            continue;
        };
        if stats.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn collect_auto_theme_entries(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !dir.exists() {
        return out;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let Ok(stats) = fs::metadata(&path) else {
            continue;
        };
        if stats.is_file() && path.extension().and_then(|e| e.to_str()) == Some("json") {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn is_supported_extension_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    if name.eq_ignore_ascii_case("extension.json") || name.ends_with(".native.json") {
        return true;
    }

    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };

    if ext.eq_ignore_ascii_case("wasm") {
        return true;
    }

    ["ts", "tsx", "js", "mjs", "cjs", "mts", "cts"]
        .iter()
        .any(|candidate| ext.eq_ignore_ascii_case(candidate))
}

fn resolve_extension_entries(dir: &Path) -> Option<Vec<PathBuf>> {
    match load_extension_manifest(dir) {
        Ok(Some(_)) => {
            return Some(vec![dir.to_path_buf()]);
        }
        Ok(None) => {}
        Err(err) => {
            warn!(path = %dir.display(), "Invalid extension manifest: {err}");
            return None;
        }
    }

    let package_json_path = dir.join("package.json");
    if package_json_path.exists() {
        match read_pi_manifest(dir) {
            Ok(Some(manifest)) => {
                if let Some(exts) = manifest.extensions {
                    let all_files = collect_extension_manifest_entries(&exts, dir);
                    let patterns = exts
                        .iter()
                        .filter(|entry| is_pattern(entry))
                        .cloned()
                        .collect::<Vec<_>>();
                    let mut entries = if patterns.is_empty() {
                        all_files
                    } else {
                        let enabled = apply_patterns(&all_files, &patterns, dir);
                        enabled.into_iter().collect::<Vec<_>>()
                    };
                    entries.sort();
                    entries.dedup();
                    return Some(entries);
                }
            }
            Ok(None) => {}
            Err(err) => {
                warn!(path = %package_json_path.display(), "Invalid package manifest: {err}");
                return None;
            }
        }
    }

    let index_native = dir.join("index.native.json");
    if index_native.exists() {
        return Some(vec![index_native]);
    }

    for index_name in [
        "index.ts",
        "index.tsx",
        "index.js",
        "index.mjs",
        "index.cjs",
        "index.mts",
        "index.cts",
    ] {
        let candidate = dir.join(index_name);
        if candidate.exists() {
            return Some(vec![candidate]);
        }
    }

    None
}

fn suppress_root_extension_walk(dir: &Path) -> bool {
    match load_extension_manifest(dir) {
        Ok(Some(_)) | Err(_) => return true,
        Ok(None) => {}
    }

    let package_json_path = dir.join("package.json");
    if !package_json_path.exists() {
        return false;
    }

    match read_pi_manifest(dir) {
        Ok(Some(manifest)) => manifest.extensions.is_some(),
        Ok(None) => false,
        Err(_) => true,
    }
}

fn collect_auto_extension_entries(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let suppress_root_walk = suppress_root_extension_walk(dir);
    if let Some(entries) = resolve_extension_entries(dir) {
        out.extend(entries);
    }

    if suppress_root_walk {
        out.sort();
        out.dedup();
        return out;
    }

    let mut builder = ignore::WalkBuilder::new(dir);
    builder
        .hidden(true)
        .follow_links(true)
        .max_depth(Some(1))
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .add_custom_ignore_filename(".fdignore")
        .filter_entry(|e| e.file_name() != std::ffi::OsStr::new("node_modules"));

    for entry in builder.build().skip(1).filter_map(std::result::Result::ok) {
        let path = entry.path().to_path_buf();
        let Ok(stats) = fs::metadata(&path) else {
            continue;
        };
        if stats.is_file() {
            if is_supported_extension_file(&path) {
                out.push(path);
            }
            continue;
        }
        if stats.is_dir()
            && let Some(entries) = resolve_extension_entries(&path)
        {
            out.extend(entries);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn read_installed_npm_version(installed_path: &Path) -> Option<String> {
    let package_json = installed_path.join("package.json");
    let raw = fs::read_to_string(package_json).ok()?;
    let json: Value = serde_json::from_str(&raw).ok()?;
    json.get("version")
        .and_then(Value::as_str)
        .map(str::to_string)
}

async fn get_latest_npm_version(installed_path: &Path, spec: &str) -> Result<String> {
    let (name, _) = parse_npm_spec(spec);
    let url = format!("https://registry.npmjs.org/{name}/latest");
    let client = crate::http::client::Client::new();
    let response = Box::pin(client.get(&url).send()).await.map_err(|e| {
        Error::tool(
            "npm",
            format!(
                "Failed to fetch npm registry for {}: {e}",
                installed_path.display()
            ),
        )
    })?;

    let status = response.status();
    let body = response.text().await.map_err(|e| {
        Error::tool(
            "npm",
            format!(
                "Failed to read npm registry response for {}: {e}",
                installed_path.display()
            ),
        )
    })?;

    if !(200..300).contains(&status) {
        return Err(Error::tool(
            "npm",
            format!("npm registry error (HTTP {status}): {body}"),
        ));
    }

    let data: Value = serde_json::from_str(&body).map_err(|e| {
        Error::tool(
            "npm",
            format!(
                "Failed to parse npm registry response for {}: {e}",
                installed_path.display()
            ),
        )
    })?;
    data.get("version")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::tool("npm", "Registry response missing version"))
}

#[derive(Debug, Clone)]
enum ParsedSource {
    Npm {
        spec: String,
        name: String,
        pinned: bool,
    },
    Git {
        clone_source: String,
        repo: String,
        host: String,
        path: String,
        r#ref: Option<String>,
        pinned: bool,
    },
    Local {
        path: PathBuf,
    },
}

fn parse_source(source: &str, cwd: &Path) -> ParsedSource {
    let source = source.trim();
    if let Some(rest) = source.strip_prefix("npm:") {
        let spec = rest.trim().to_string();
        let (name, version) = parse_npm_spec(&spec);
        return ParsedSource::Npm {
            spec,
            name,
            pinned: version.as_deref().is_some_and(npm_version_pins_source),
        };
    }

    if let Some(rest) = source.strip_prefix("git:") {
        return parse_git_source(rest.trim(), cwd);
    }

    if looks_like_git_url(source) || source.starts_with("https://") || source.starts_with("http://")
    {
        return parse_git_source(source, cwd);
    }

    if let Some(resolved) = resolve_install_source_alias(source, cwd) {
        return parse_source(&resolved, cwd);
    }

    ParsedSource::Local {
        path: resolve_local_path(source, cwd),
    }
}

fn validate_non_empty_source<'a>(source: &'a str, label: &str) -> Result<&'a str> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(Error::config(format!("{label} must be non-empty")));
    }
    Ok(trimmed)
}

fn resolve_install_source_alias(source: &str, cwd: &Path) -> Option<String> {
    if source.is_empty() || looks_like_local_path(source) {
        return None;
    }

    // Preserve local-path behavior for existing relative paths like `foo/bar`.
    if resolve_local_path(source, cwd).exists() {
        return None;
    }

    match ExtensionIndexStore::default_store().resolve_install_source(source) {
        Ok(Some(resolved)) if resolved != source => Some(resolved),
        Ok(_) => None,
        Err(err) => {
            tracing::debug!(
                "failed to resolve install source alias via extension index (using source as-is): {err}"
            );
            None
        }
    }
}

fn git_clone_source(source: &str, cwd: &Path) -> String {
    let spec = source.trim().strip_prefix("git:").unwrap_or(source).trim();
    let (repo_raw, _) = split_git_spec_ref(spec);
    if looks_like_local_path(repo_raw) {
        local_path_from_spec(repo_raw, cwd)
            .to_string_lossy()
            .to_string()
    } else {
        repo_raw.to_string()
    }
}

// Trailing `@ref` is part of Pi's git source syntax, but authenticated HTTPS
// URLs and SCP-like SSH specs also contain `@` in the authority section.
fn split_git_spec_ref(spec: &str) -> (&str, Option<&str>) {
    let spec = spec.trim();
    if spec.is_empty() {
        return ("", None);
    }

    if looks_like_local_path(spec) {
        let mut parts = spec.splitn(2, '@');
        let repo = parts.next().unwrap_or("").trim();
        let r#ref = parts.next().map(str::trim).filter(|part| !part.is_empty());
        return (repo, r#ref);
    }

    let reserved_prefix_end = git_ref_reserved_prefix_end(spec);
    for (idx, _) in spec.match_indices('@').rev() {
        if idx < reserved_prefix_end {
            continue;
        }
        let repo = spec[..idx].trim();
        let r#ref = spec[idx + 1..].trim();
        if !repo.is_empty() && !r#ref.is_empty() {
            return (repo, Some(r#ref));
        }
    }

    (spec, None)
}

fn git_ref_reserved_prefix_end(spec: &str) -> usize {
    if let Some(scheme_idx) = spec.find("://") {
        let authority_start = scheme_idx + 3;
        return spec[authority_start..]
            .find('/')
            .map_or(spec.len(), |idx| authority_start + idx);
    }

    if let Some(at_idx) = spec.find('@') {
        let tail = &spec[at_idx + 1..];
        if let Some(colon_idx) = tail.find(':') {
            let slash_idx = tail.find('/');
            if slash_idx.is_none_or(|slash| colon_idx < slash) {
                return at_idx + 1 + colon_idx;
            }
        }
    }

    0
}

// Normalize clone sources into a stable repo identity without leaking URL
// userinfo into install paths, trust records, or source matching.
fn normalize_remote_git_repo(repo_raw: &str) -> (String, String, String) {
    let repo_raw = repo_raw.trim();

    if let Ok(url) = url::Url::parse(repo_raw) {
        let host = url.host_str().unwrap_or("").to_string();
        let path = url
            .path()
            .trim_matches('/')
            .trim_end_matches(".git")
            .to_string();
        let repo = if path.is_empty() {
            host.clone()
        } else {
            format!("{host}/{path}")
        };
        return (repo, host, path);
    }

    if !repo_raw.contains("://") {
        let first_slash = repo_raw.find('/');
        if let Some(colon_idx) = repo_raw.find(':')
            && first_slash.is_none_or(|slash| colon_idx < slash)
            && let Some(at_idx) = repo_raw[..colon_idx].rfind('@')
        {
            let host = repo_raw[at_idx + 1..colon_idx].trim().to_string();
            let path = repo_raw[colon_idx + 1..]
                .trim()
                .trim_matches('/')
                .trim_end_matches(".git")
                .split('/')
                .filter(|s| !s.is_empty() && *s != "." && *s != "..")
                .collect::<Vec<_>>()
                .join("/");
            let repo = if path.is_empty() {
                host.clone()
            } else {
                format!("{host}/{path}")
            };
            return (repo, host, path);
        }
    }

    let normalized = repo_raw
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches(".git")
        .to_string();

    let segments = normalized
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != "." && *segment != "..")
        .collect::<Vec<_>>();

    let host = segments
        .first()
        .copied()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("")
        .to_string();
    let path = if segments.len() >= 2 {
        segments[1..].join("/")
    } else {
        String::new()
    };
    let repo = if path.is_empty() {
        host.clone()
    } else {
        format!("{host}/{path}")
    };

    (repo, host, path)
}

#[cfg(test)]
fn normalized_git_repo_key(spec: &str) -> String {
    let (repo_raw, _) = split_git_spec_ref(spec);
    if looks_like_local_path(repo_raw) {
        repo_raw.to_string()
    } else {
        let (repo, _, _) = normalize_remote_git_repo(repo_raw);
        repo
    }
}

fn parse_git_source(spec: &str, cwd: &Path) -> ParsedSource {
    let (repo_raw, parsed_ref) = split_git_spec_ref(spec);
    let r#ref = parsed_ref.map(str::to_string);
    let pinned = r#ref.is_some();
    // Derive the actual clone target from the already-resolved git spec so
    // extension-index aliases and authenticated URLs keep their install-time source.
    let clone_source = git_clone_source(spec, cwd);

    let (repo, host, path) = if looks_like_local_path(repo_raw) {
        let repo_path = local_path_from_spec(repo_raw, cwd);

        // Use a short stable hash for the on-disk install directory to avoid embedding absolute
        // paths (slashes, drive letters) into `.pi/git/**` paths.
        let mut hasher = Sha256::new();
        hasher.update(repo_path.to_string_lossy().as_bytes());
        let digest = hasher.finalize();
        let key = hex_encode(&digest)[..16].to_string();

        (
            repo_path.to_string_lossy().to_string(),
            "local".to_string(),
            key,
        )
    } else {
        normalize_remote_git_repo(repo_raw)
    };

    ParsedSource::Git {
        clone_source,
        repo,
        host,
        path,
        r#ref,
        pinned,
    }
}

fn looks_like_git_url(source: &str) -> bool {
    const HOSTS: [&str; 4] = ["github.com", "gitlab.com", "bitbucket.org", "codeberg.org"];
    let normalized = source
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    HOSTS
        .iter()
        .any(|host| normalized.starts_with(&format!("{host}/")))
}

const fn looks_like_windows_drive_absolute_path(spec: &str) -> bool {
    let bytes = spec.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
}

fn looks_like_local_path(spec: &str) -> bool {
    let spec = spec.trim();
    spec == "."
        || spec == ".."
        || spec.starts_with("file://")
        || spec.starts_with("\\\\")
        || spec.starts_with('/')
        || spec.starts_with(".\\")
        || spec.starts_with("..\\")
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('~')
        || looks_like_windows_drive_absolute_path(spec)
}

fn local_path_from_spec(spec: &str, cwd: &Path) -> PathBuf {
    // `git clone` supports local paths and file:// URLs. We normalize both to a local PathBuf so
    // tests can create deterministic offline git fixtures.
    let spec = spec.trim();
    if let Some(rest) = spec.strip_prefix("file://") {
        // Keep the triple-slash form (`file:///abs/path`) working by stripping only the scheme.
        return resolve_local_path(&file_url_local_path(rest), cwd);
    }
    resolve_local_path(spec, cwd)
}

fn file_url_local_path(path: &str) -> String {
    let path = path.trim();

    if looks_like_windows_drive_absolute_path(path) {
        return path.to_string();
    }

    if let Some(stripped) = path
        .strip_prefix('/')
        .filter(|stripped| looks_like_windows_drive_absolute_path(stripped))
    {
        return stripped.to_string();
    }

    if path.eq_ignore_ascii_case("localhost") {
        return "/".to_string();
    }
    if let Some((host, stripped)) = path.split_once('/')
        && host.eq_ignore_ascii_case("localhost")
    {
        if looks_like_windows_drive_absolute_path(stripped) {
            return stripped.to_string();
        }
        if let Some(drive_path) = stripped
            .strip_prefix('/')
            .filter(|drive_path| looks_like_windows_drive_absolute_path(drive_path))
        {
            return drive_path.to_string();
        }
        return format!("/{stripped}");
    }

    if !path.is_empty() && !path.starts_with('/') {
        return format!("//{path}");
    }

    path.to_string()
}

fn resolve_local_path(input: &str, cwd: &Path) -> PathBuf {
    let trimmed = input.trim();
    if spec_is_platform_absolute(trimmed) {
        return PathBuf::from(trimmed);
    }
    if trimmed == "~" {
        return normalize_dot_segments(&dirs::home_dir().unwrap_or_else(|| cwd.to_path_buf()));
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return normalize_dot_segments(
            &dirs::home_dir()
                .unwrap_or_else(|| cwd.to_path_buf())
                .join(rest),
        );
    }
    if trimmed.starts_with('~') {
        return normalize_dot_segments(
            &dirs::home_dir()
                .unwrap_or_else(|| cwd.to_path_buf())
                .join(trimmed.trim_start_matches('~')),
        );
    }
    normalize_dot_segments(&cwd.join(trimmed))
}

fn spec_is_platform_absolute(spec: &str) -> bool {
    Path::new(spec).is_absolute()
        || spec.starts_with("\\\\")
        || looks_like_windows_drive_absolute_path(spec)
}

fn normalize_dot_segments(path: &Path) -> PathBuf {
    use std::ffi::{OsStr, OsString};
    use std::path::Component;

    let mut out = PathBuf::new();
    let mut normals: Vec<OsString> = Vec::new();
    let mut has_prefix = false;
    let mut has_root = false;

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                out.push(prefix.as_os_str());
                has_prefix = true;
            }
            Component::RootDir => {
                out.push(component.as_os_str());
                has_root = true;
            }
            Component::CurDir => {}
            Component::ParentDir => match normals.last() {
                Some(last) if last.as_os_str() != OsStr::new("..") => {
                    normals.pop();
                }
                _ => {
                    if !has_root && !has_prefix {
                        normals.push(OsString::from(".."));
                    }
                }
            },
            Component::Normal(part) => normals.push(part.to_os_string()),
        }
    }

    for part in normals {
        out.push(part);
    }

    out
}

fn parse_npm_spec(spec: &str) -> (String, Option<String>) {
    let spec = spec.trim();
    if spec.is_empty() {
        return (String::new(), None);
    }

    let at_pos = spec
        .strip_prefix('@')
        .map_or_else(|| spec.find('@'), |rest| rest.rfind('@').map(|idx| idx + 1));

    match at_pos {
        Some(pos) if pos + 1 < spec.len() => {
            (spec[..pos].to_string(), Some(spec[pos + 1..].to_string()))
        }
        _ => (spec.to_string(), None),
    }
}

fn pinned_npm_version_needs_update(requested_version: &str, installed_version: &str) -> bool {
    !is_local_npm_reference(requested_version) && requested_version != installed_version
}

/// Offline verdict on whether an installed npm package needs install work.
#[derive(Debug, PartialEq, Eq)]
enum NpmLocalDecision {
    /// Installed metadata is missing/unreadable or the installed version
    /// fails a locally decidable exact/range spec.
    NeedsInstall,
    /// The installed copy satisfies its spec; skip registry and installer.
    Satisfied,
    /// The spec form cannot be decided locally (bare spec); callers keep the
    /// historical latest-version registry probe.
    CheckRegistry { installed_version: String },
}

/// Decide, without any network or subprocess work, whether the package at
/// `installed_path` needs install work for `spec`.
///
/// Exact versions and conventional ranges are settled here so startup does
/// not pay registry latency for already-satisfied installs. Dist-tags, local
/// file/link references, and unrecognized spec forms keep their historical
/// behavior; bare specs defer to the registry probe.
fn npm_local_update_decision(spec: &str, installed_path: &Path) -> NpmLocalDecision {
    let Some(installed_version) = read_installed_npm_version(installed_path) else {
        return NpmLocalDecision::NeedsInstall;
    };

    let (_, requested_version) = parse_npm_spec(spec);
    let Some(requested) = requested_version else {
        return NpmLocalDecision::CheckRegistry { installed_version };
    };

    NpmVersionConstraint::parse(&requested).map_or_else(
        || {
            if pinned_npm_version_needs_update(&requested, &installed_version) {
                NpmLocalDecision::NeedsInstall
            } else {
                NpmLocalDecision::Satisfied
            }
        },
        |constraint| {
            if constraint.is_satisfied_by(&installed_version) {
                NpmLocalDecision::Satisfied
            } else {
                NpmLocalDecision::NeedsInstall
            }
        },
    )
}

/// Whether the installed package at `installed_path` needs install/update
/// work for `spec` during startup resolution.
///
/// Decided locally whenever possible (see [`npm_local_update_decision`]);
/// only bare specs still consult the registry, preserving their historical
/// latest-version probe.
async fn npm_needs_update(spec: &str, installed_path: &Path) -> bool {
    match npm_local_update_decision(spec, installed_path) {
        NpmLocalDecision::NeedsInstall => true,
        NpmLocalDecision::Satisfied => false,
        NpmLocalDecision::CheckRegistry { installed_version } => {
            Box::pin(get_latest_npm_version(installed_path, spec))
                .await
                .is_ok_and(|latest| latest != installed_version)
        }
    }
}

/// A conservatively parsed npm version constraint that can be evaluated
/// without consulting npm or the registry.
///
/// Only single-token forms whose npm semantics the `semver` crate reproduces
/// are recognized: exact versions (`1.2.3`, `v1.2.3`), x-ranges (`1`, `1.2`,
/// `1.x`, `1.2.*`, `*`), and caret/tilde/comparator ranges (`^1.2`, `~0.4.1`,
/// `>=2`, `=1.2.3`). Dist-tags, unions (`||`), space-separated intersections,
/// hyphen ranges, and local references all yield `None` so callers fall back
/// to their existing behavior.
#[derive(Debug)]
enum NpmVersionConstraint {
    Exact(semver::Version),
    Range(semver::VersionReq),
}

impl NpmVersionConstraint {
    fn parse(raw: &str) -> Option<Self> {
        let value = raw.trim();
        if value.is_empty()
            || value.contains(char::is_whitespace)
            || value.contains("||")
            || value.contains(',')
        {
            return None;
        }

        if let Some(version) = npm_exact_version(value) {
            return Some(Self::Exact(version));
        }

        if let Some(canonical) = canonicalize_npm_x_range(value) {
            return semver::VersionReq::parse(&canonical).ok().map(Self::Range);
        }

        // Single-comparator ranges. The `semver` crate treats missing or
        // wildcard components the way npm does for these operators, but a
        // bare partial like `1.2` would get Cargo's caret default, so
        // operator-less values are only handled by the branches above.
        if value.starts_with(['^', '~', '=', '>', '<']) {
            return semver::VersionReq::parse(value).ok().map(Self::Range);
        }

        None
    }

    fn is_satisfied_by(&self, installed_version: &str) -> bool {
        let Some(installed) = npm_exact_version(installed_version) else {
            return false;
        };
        match self {
            // npm ignores build metadata when comparing versions.
            Self::Exact(expected) => installed.cmp_precedence(expected).is_eq(),
            Self::Range(range) => range.matches(&installed),
        }
    }
}

/// Canonicalize npm x-range shorthand (`1`, `1.2`, `1.x`, `1.2.X`, `1.*`,
/// `*`) into the wildcard form the `semver` crate parses with npm semantics.
/// Returns `None` for anything that is not a pure x-range.
fn canonicalize_npm_x_range(value: &str) -> Option<String> {
    let parts: Vec<&str> = value.split('.').collect();
    if parts.len() > 3 {
        return None;
    }

    let mut numeric: Vec<u64> = Vec::new();
    let mut saw_wildcard = false;
    for part in parts {
        if matches!(part, "x" | "X" | "*") {
            saw_wildcard = true;
            continue;
        }
        // A numeric component after a wildcard (`1.x.3`) has no conventional
        // meaning; npm rejects leading zeros.
        if saw_wildcard
            || part.is_empty()
            || (part.len() > 1 && part.starts_with('0'))
            || !part.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        numeric.push(part.parse().ok()?);
    }

    match numeric.as_slice() {
        [] => Some("*".to_string()),
        [major] => Some(format!("{major}.*")),
        [major, minor] => Some(format!("{major}.{minor}.*")),
        // A fully numeric `X.Y.Z` is an exact version, not an x-range.
        _ => None,
    }
}

/// Parse a string as a single npm version, tolerating surrounding whitespace
/// and npm's optional leading `v` (`v1.2.3` installs as `1.2.3`).
///
/// Used both to classify exact (pinning) spec fragments and to parse
/// installed `package.json` versions.
fn npm_exact_version(value: &str) -> Option<semver::Version> {
    let value = value.trim();
    let value = value.strip_prefix(['v', 'V']).unwrap_or(value);
    semver::Version::parse(value).ok()
}

/// Whether an npm spec's version fragment pins the source.
///
/// Mirrors upstream pi's `isExactNpmVersion`: only exact versions pin, so
/// `pi update` refreshes ranges and dist-tags while leaving pins alone.
/// Local file/link/workspace references are additionally treated as pinned
/// so update keeps leaving them untouched (unchanged behavior).
fn npm_version_pins_source(version: &str) -> bool {
    is_local_npm_reference(version) || npm_exact_version(version).is_some()
}

fn is_local_npm_reference(reference: &str) -> bool {
    let reference = reference.trim();
    reference.starts_with("file:")
        || reference.starts_with("link:")
        || reference.starts_with("workspace:")
}

fn ensure_npm_project(root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    ensure_git_ignore(root)?;
    let package_json = root.join("package.json");
    if !package_json.exists() {
        let value = serde_json::json!({ "name": "pi-packages", "private": true });
        fs::write(&package_json, serde_json::to_string_pretty(&value)?)?;
    }
    Ok(())
}

fn ensure_git_ignore(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    let ignore_path = dir.join(".gitignore");
    if !ignore_path.exists() {
        fs::write(ignore_path, "*\n!.gitignore\n")?;
    }
    Ok(())
}

fn prune_empty_git_parents(target_dir: &Path, root: &Path) {
    let Ok(root) = root.canonicalize() else {
        return;
    };
    let mut current = target_dir.parent().map(PathBuf::from);

    while let Some(dir) = current {
        let Ok(canon) = dir.canonicalize() else { break };
        if canon == root || !canon.starts_with(&root) {
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            break;
        };
        if entries.into_iter().next().is_some() {
            break;
        }
        let _ = fs::remove_dir(&dir);
        current = dir.parent().map(PathBuf::from);
    }
}

fn run_command<I, S>(program: &str, args: I, cwd: Option<&Path>) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }

    let output = cmd
        .output()
        .map_err(|e| Error::tool(program, format!("Failed to spawn {program}: {e}")))?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut msg = format!("Command failed: {program}");
        if let Some(code) = output.status.code() {
            let _ = write!(msg, " (exit {code})");
        }
        if !stdout.trim().is_empty() {
            let _ = write!(msg, "\nstdout:\n{stdout}");
        }
        if !stderr.trim().is_empty() {
            let _ = write!(msg, "\nstderr:\n{stderr}");
        }
        return Err(Error::tool(program, msg));
    }

    Ok(())
}

fn run_command_capture<I, S>(program: &str, args: I, cwd: Option<&Path>) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }

    let output = cmd
        .output()
        .map_err(|e| Error::tool(program, format!("Failed to spawn {program}: {e}")))?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut msg = format!("Command failed: {program}");
        if let Some(code) = output.status.code() {
            let _ = write!(msg, " (exit {code})");
        }
        if !stdout.trim().is_empty() {
            let _ = write!(msg, "\nstdout:\n{stdout}");
        }
        if !stderr.trim().is_empty() {
            let _ = write!(msg, "\nstderr:\n{stderr}");
        }
        return Err(Error::tool(program, msg));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

const fn scope_label(scope: PackageScope) -> &'static str {
    match scope {
        PackageScope::User => "user",
        PackageScope::Project => "project",
        PackageScope::Temporary => "temporary",
    }
}

const fn trust_state_label(state: PackageEntryTrustState) -> &'static str {
    match state {
        PackageEntryTrustState::Trusted => "trusted",
        PackageEntryTrustState::Rejected => "rejected",
    }
}

fn verification_error(code: &str, reason: &str, remediation: &str) -> Error {
    Error::tool(
        "package_manager",
        format!(
            "Package lock/provenance verification failed [{code}]: {reason}\nRemediation: {remediation}"
        ),
    )
}

pub fn evaluate_lock_transition(
    existing: Option<&PackageLockEntry>,
    candidate: &PackageLockEntry,
    action: PackageLockAction,
) -> std::result::Result<LockTransitionPlan, PackageLockMismatch> {
    let Some(existing) = existing else {
        return Ok(LockTransitionPlan {
            reason_codes: vec!["first_seen".to_string()],
            from_state: "untracked".to_string(),
            to_state: "trusted".to_string(),
        });
    };

    let allow_mutation = allow_lock_entry_update(candidate, action);

    if (existing.source_kind != candidate.source_kind || existing.source != candidate.source)
        && !allow_mutation
    {
        return Err(PackageLockMismatch {
            code: "provenance_mismatch",
            reason: format!(
                "source identity changed for {}: previous='{}' ({:?}), current='{}' ({:?})",
                candidate.identity,
                existing.source,
                existing.source_kind,
                candidate.source,
                candidate.source_kind
            ),
            remediation: format!(
                "Review the source change, then run `pi remove {}` and `pi install {}` to re-establish trust.",
                candidate.source, candidate.source
            ),
        });
    }

    if existing.resolved != candidate.resolved
        && !allow_mutation
        && !resolved_differs_only_in_pinned(&existing.resolved, &candidate.resolved)
    {
        return Err(PackageLockMismatch {
            code: "provenance_mismatch",
            reason: format!(
                "resolved provenance changed for {} while source is immutable in this operation",
                candidate.identity
            ),
            remediation: format!(
                "Use `pi update {}` for unpinned sources, or reinstall after intentional provenance changes.",
                candidate.source
            ),
        });
    }

    if existing.digest_sha256 != candidate.digest_sha256 && !allow_mutation {
        return Err(PackageLockMismatch {
            code: "digest_mismatch",
            reason: format!(
                "digest changed for {}: expected {}, got {}",
                candidate.identity, existing.digest_sha256, candidate.digest_sha256
            ),
            remediation: format!(
                "Inspect upstream changes. If expected, run `pi remove {}` then `pi install {}` to trust the new digest.",
                candidate.source, candidate.source
            ),
        });
    }

    let mut reason_codes = Vec::new();
    if existing.resolved != candidate.resolved {
        reason_codes.push("provenance_changed".to_string());
    }
    if existing.digest_sha256 != candidate.digest_sha256 {
        reason_codes.push("digest_changed".to_string());
    }
    if reason_codes.is_empty() {
        reason_codes.push("verified".to_string());
    }

    Ok(LockTransitionPlan {
        reason_codes,
        from_state: trust_state_label(existing.trust_state).to_string(),
        to_state: "trusted".to_string(),
    })
}

/// Whether two resolved provenances are identical except for the `pinned`
/// classification flag.
///
/// `pinned` is a derived property of the *spec* (exact version or local ref),
/// not of what was installed. When its definition changes — as when range and
/// dist-tag specs stopped counting as pinned — a pre-existing lock entry
/// carries the old classification for the same installed artifact. Failing
/// verification for that alone would hard-block startup until a manual
/// remove/install cycle, so the flag-only delta is tolerated and the entry
/// rotates to the new classification on the next lock write.
fn resolved_differs_only_in_pinned(
    existing: &PackageResolvedProvenance,
    candidate: &PackageResolvedProvenance,
) -> bool {
    match (existing, candidate) {
        (
            PackageResolvedProvenance::Npm {
                name: en,
                requested_spec: es,
                requested_version: ev,
                installed_version: ei,
                pinned: _,
            },
            PackageResolvedProvenance::Npm {
                name: cn,
                requested_spec: cs,
                requested_version: cv,
                installed_version: ci,
                pinned: _,
            },
        ) => en == cn && es == cs && ev == cv && ei == ci,
        (
            PackageResolvedProvenance::Git {
                repo: er,
                host: eh,
                path: ep,
                requested_ref: erf,
                resolved_commit: ec,
                origin_url: eo,
                pinned: _,
            },
            PackageResolvedProvenance::Git {
                repo: cr,
                host: ch,
                path: cp,
                requested_ref: crf,
                resolved_commit: cc,
                origin_url: co,
                pinned: _,
            },
        ) => er == cr && eh == ch && ep == cp && erf == crf && ec == cc && eo == co,
        _ => false,
    }
}

const fn allow_lock_entry_update(candidate: &PackageLockEntry, action: PackageLockAction) -> bool {
    match action {
        PackageLockAction::Install => false,
        PackageLockAction::Update => match candidate.resolved {
            PackageResolvedProvenance::Npm { pinned, .. }
            | PackageResolvedProvenance::Git { pinned, .. } => !pinned,
            PackageResolvedProvenance::Local { .. } => false,
        },
    }
}

pub fn sort_lock_entries(entries: &mut [PackageLockEntry]) {
    entries.sort_by(|left, right| {
        left.identity
            .cmp(&right.identity)
            .then_with(|| left.source.cmp(&right.source))
    });
}

pub fn read_package_lockfile(path: &Path) -> Result<PackageLockfile> {
    if !path.exists() {
        return Ok(PackageLockfile::default());
    }

    let content = fs::read_to_string(path)?;
    let mut lockfile: PackageLockfile = serde_json::from_str(&content).map_err(|err| {
        Error::config(format!(
            "Invalid package lockfile JSON in {}: {err}",
            path.display()
        ))
    })?;
    if lockfile.schema.trim().is_empty() {
        lockfile.schema = PACKAGE_LOCK_SCHEMA.to_string();
    }
    sort_lock_entries(&mut lockfile.entries);
    Ok(lockfile)
}

pub fn write_package_lockfile_atomic(path: &Path, lockfile: &PackageLockfile) -> Result<()> {
    let value = serde_json::to_value(lockfile)?;
    write_settings_json_atomic(path, &value)
}

pub fn digest_package_path(path: &Path) -> Result<String> {
    if path.is_file() {
        let mut hasher = Sha256::new();
        hasher.update(b"file\0");
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("entry");
        hasher.update(file_name.as_bytes());
        hasher.update(b"\0");
        let bytes = fs::read(path)?
            .into_iter()
            .filter(|byte| *byte != b'\r')
            .collect::<Vec<_>>();
        hasher.update(&bytes);
        hasher.update(b"\0");
        return Ok(hex_encode(hasher.finalize().as_slice()));
    }

    if !path.is_dir() {
        return Err(Error::tool(
            "package_manager",
            format!(
                "Cannot compute digest for non-file/non-directory path: {}",
                path.display()
            ),
        ));
    }

    let mut files = Vec::new();
    collect_digest_files_recursive(path, path, &mut files)?;
    files.sort_by_key(|(_, relative)| relative.clone());

    let mut hasher = Sha256::new();
    for (full_path, relative) in files {
        hasher.update(b"file\0");
        hasher.update(relative.as_bytes());
        hasher.update(b"\0");
        let bytes = fs::read(full_path)?
            .into_iter()
            .filter(|byte| *byte != b'\r')
            .collect::<Vec<_>>();
        hasher.update(&bytes);
        hasher.update(b"\0");
    }

    Ok(hex_encode(hasher.finalize().as_slice()))
}

fn collect_digest_files_recursive(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(PathBuf, String)>,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        if name == OsStr::new(".git") {
            continue;
        }

        if file_type.is_dir() {
            collect_digest_files_recursive(root, &path, out)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        out.push((path.clone(), relative_posix(root, &path)));
    }
    Ok(())
}

fn global_settings_path(cwd: &Path) -> PathBuf {
    Config::config_path_override_from_env(cwd)
        .unwrap_or_else(|| Config::global_dir().join("settings.json"))
}

fn project_settings_path(cwd: &Path) -> PathBuf {
    cwd.join(Config::project_dir()).join("settings.json")
}

#[derive(Debug, Clone, Copy)]
enum UpdateAction {
    Add,
    Remove,
}

fn list_packages_in_settings(path: &Path) -> Result<Vec<PackageEntry>> {
    let value = read_settings_json(path)?;
    let packages = value
        .get("packages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::new();
    for pkg in packages {
        if let Some(spec) = extract_package_spec(&pkg) {
            out.push(PackageEntry {
                scope: PackageScope::User, // caller overrides
                source: spec.source,
                filter: spec.filter,
            });
        }
    }
    Ok(out)
}

fn update_package_sources(
    path: &Path,
    source: &str,
    action: UpdateAction,
    cwd: &Path,
) -> Result<()> {
    let source = source.trim();
    if source.is_empty() {
        return Err(Error::Config(
            "settings package source cannot be empty".to_string(),
        ));
    }

    let mut root = read_settings_json(path)?;
    if !root.is_object() {
        root = serde_json::json!({});
    }

    if !matches!(root.get("packages"), Some(Value::Array(_))) {
        root["packages"] = Value::Array(Vec::new());
    }

    let packages = root
        .get_mut("packages")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            Error::Config("failed to initialize settings 'packages' as an array".to_string())
        })?;

    match action {
        UpdateAction::Add => {
            let exists = packages.iter().any(|existing| {
                extract_package_source(existing)
                    .is_some_and(|(s, _)| sources_match_in_dir(&s, source, cwd))
            });
            if !exists {
                packages.push(Value::String(source.to_string()));
            }
        }
        UpdateAction::Remove => {
            packages.retain(|existing| {
                !extract_package_source(existing)
                    .is_some_and(|(s, _)| sources_match_in_dir(&s, source, cwd))
            });
        }
    }

    write_settings_json_atomic(path, &root)
}

fn extract_package_source(value: &Value) -> Option<(String, bool)> {
    if let Some(s) = value.as_str() {
        return Some((s.to_string(), false));
    }
    let obj = value.as_object()?;
    let source = obj.get("source")?.as_str()?.to_string();
    Some((source, true))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NormalizedKind {
    Npm,
    Git,
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedSource {
    kind: NormalizedKind,
    key: String,
}

#[cfg(test)]
fn sources_match(a: &str, b: &str) -> bool {
    normalize_source(a).is_some_and(|left| normalize_source(b).is_some_and(|right| left == right))
}

fn sources_match_in_dir(a: &str, b: &str, cwd: &Path) -> bool {
    normalize_source_in_dir(a, cwd)
        .is_some_and(|left| normalize_source_in_dir(b, cwd).is_some_and(|right| left == right))
}

#[cfg(test)]
fn normalize_source(source: &str) -> Option<NormalizedSource> {
    let source = source.trim();
    if source.is_empty() {
        return None;
    }
    if let Some(rest) = source.strip_prefix("npm:") {
        let spec = rest.trim();
        let (name, _) = parse_npm_spec(spec);
        return Some(NormalizedSource {
            kind: NormalizedKind::Npm,
            key: name,
        });
    }
    if let Some(rest) = source.strip_prefix("git:") {
        return Some(NormalizedSource {
            kind: NormalizedKind::Git,
            key: normalized_git_repo_key(rest.trim()),
        });
    }
    if looks_like_git_url(source) || source.starts_with("https://") || source.starts_with("http://")
    {
        return Some(NormalizedSource {
            kind: NormalizedKind::Git,
            key: normalized_git_repo_key(source),
        });
    }
    Some(NormalizedSource {
        kind: NormalizedKind::Local,
        key: source.to_string(),
    })
}

fn normalize_source_in_dir(source: &str, cwd: &Path) -> Option<NormalizedSource> {
    let source = source.trim();
    if source.is_empty() {
        return None;
    }

    // Settings dedupe/removal should follow install-time source resolution so equivalent local
    // paths and shorthand aliases collapse to the same package entry.
    match parse_source(source, cwd) {
        ParsedSource::Npm { name, .. } => Some(NormalizedSource {
            kind: NormalizedKind::Npm,
            key: name,
        }),
        ParsedSource::Git { repo, .. } => Some(NormalizedSource {
            kind: NormalizedKind::Git,
            key: repo,
        }),
        ParsedSource::Local { path } => Some(NormalizedSource {
            kind: NormalizedKind::Local,
            key: path.to_string_lossy().to_string(),
        }),
    }
}

fn read_settings_json(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let content = fs::read_to_string(path)?;
    serde_json::from_str(&content).map_err(|e| {
        Error::config(format!(
            "Invalid JSON in settings file {}: {e}",
            path.display()
        ))
    })
}

fn write_settings_json_atomic(path: &Path, value: &Value) -> Result<()> {
    let data = serde_json::to_string_pretty(value)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    fs::write(tmp.path(), data)?;
    let tmp_path = tmp.into_temp_path();
    tmp_path
        .persist(path)
        .map_err(|e| Error::Io(Box::new(e.error)))?;
    Ok(())
}

fn compat_scan_enabled() -> bool {
    let value = std::env::var("PI_EXT_COMPAT_SCAN").unwrap_or_default();
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn maybe_emit_compat_ledgers(extensions: &[ResolvedResource]) {
    if !compat_scan_enabled() {
        return;
    }

    let mut enabled = extensions.iter().filter(|r| r.enabled).collect::<Vec<_>>();
    enabled.sort_by(|left, right| left.path.cmp(&right.path));

    for resource in enabled {
        let root = if resource.path.is_dir() {
            resource.path.clone()
        } else {
            resource
                .path
                .parent()
                .map_or_else(|| resource.path.clone(), Path::to_path_buf)
        };
        let scanner = CompatibilityScanner::new(root);
        let ledger = match scanner.scan_path(&resource.path) {
            Ok(ledger) => ledger,
            Err(err) => {
                warn!(event = "ext.compat_ledger_error", error = %err);
                continue;
            }
        };

        if ledger.is_empty() {
            continue;
        }

        match serde_json::to_string(&ledger) {
            Ok(ledger_json) => {
                info!(
                    event = "ext.compat_ledger",
                    schema = %ledger.schema,
                    ledger = %ledger_json
                );
            }
            Err(err) => {
                warn!(event = "ext.compat_ledger_serialize_error", error = %err);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use serde_json::json;
    use std::fs;
    use std::future::Future;

    fn run_async<T>(future: impl Future<Output = T>) -> T {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        runtime.block_on(future)
    }

    #[test]
    fn test_finish_package_task_propagates_panic_before_cancellation() {
        let handle = std::thread::spawn(|| -> () {
            panic!("package manager worker panic");
        });

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<()> = finish_package_task(handle, Err(()), "Install task cancelled");
        }));

        assert!(
            panic.is_err(),
            "worker panic should not be masked as cancellation"
        );
    }

    #[test]
    fn test_finish_package_task_maps_nonpanic_cancellation_to_tool_error() {
        let handle = std::thread::spawn(|| {});

        let err = finish_package_task::<(), _>(handle, Err(()), "Install task cancelled")
            .expect_err("error");

        assert!(
            err.to_string().contains("Install task cancelled"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_finish_package_task_returns_success_payload() {
        let handle = std::thread::spawn(|| {});

        let value =
            finish_package_task::<usize, ()>(handle, Ok(Ok(7usize)), "task cancelled").unwrap();

        assert_eq!(value, 7);
    }

    #[test]
    fn test_parse_npm_spec_scoped_and_unscoped() {
        assert_eq!(parse_npm_spec("foo"), ("foo".to_string(), None));
        assert_eq!(
            parse_npm_spec("foo@1.2.3"),
            ("foo".to_string(), Some("1.2.3".to_string()))
        );
        assert_eq!(
            parse_npm_spec("@scope/name@1.2.3"),
            ("@scope/name".to_string(), Some("1.2.3".to_string()))
        );
        assert_eq!(
            parse_npm_spec("@scope/name"),
            ("@scope/name".to_string(), None)
        );
    }

    #[test]
    fn test_sources_match_normalization() {
        assert!(sources_match("npm:foo@1", "npm:foo@2"));
        assert!(sources_match(
            "git:github.com/a/b@v1",
            "git:github.com/a/b@v2"
        ));
        assert!(sources_match(
            "https://github.com/a/b.git@v1",
            "github.com/a/b"
        ));
        assert!(sources_match(
            "git:https://token-a@github.com/a/b.git@v1",
            "git:https://token-b@github.com/a/b.git@v2"
        ));
        assert!(!sources_match("npm:foo", "npm:bar"));
        assert!(!sources_match("git:github.com/a/b", "git:github.com/a/c"));
    }

    #[test]
    #[cfg(unix)]
    fn test_package_identity_matches_pi_mono() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = PackageManager::new(dir.path().to_path_buf());

        assert_eq!(
            manager.package_identity("npm:@scope/name@1.2.3"),
            "npm:@scope/name"
        );
        assert_eq!(
            manager.package_identity("git:https://github.com/a/b.git@v1"),
            "git:github.com/a/b"
        );
        assert_eq!(
            manager.package_identity("git:https://token@github.com/a/b.git@v1"),
            "git:github.com/a/b"
        );

        let identity = manager.package_identity("./foo/../bar");
        let expected_suffix = format!("{}/bar", dir.path().display());
        assert!(identity.ends_with(&expected_suffix), "{identity}");
    }

    #[test]
    fn parse_source_prefers_existing_local_paths_over_index_aliases() {
        let dir = tempfile::tempdir().expect("tempdir");
        let local = dir.path().join("checkpoint-pi");
        fs::create_dir_all(&local).expect("create local path");

        match parse_source("checkpoint-pi", dir.path()) {
            ParsedSource::Local { path } => assert_eq!(path, local),
            other => panic!("Unexpected parsed source: {:?}", other),
        }
    }

    #[test]
    fn test_installed_path_project_scope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = PackageManager::new(dir.path().to_path_buf());

        // Note: we can't easily test async methods in sync test without runtime.
        // We test the private sync methods which logic relies on.
        // But we made them private.
        // For unit tests in the same module, we can access private methods.

        let npm = manager
            .installed_path_sync("npm:foo@1.2.3", PackageScope::Project)
            .expect("installed_path")
            .expect("path");
        assert_eq!(
            npm,
            dir.path()
                .join(Config::project_dir())
                .join("npm")
                .join("node_modules")
                .join("foo")
        );

        let git = manager
            .installed_path_sync("git:github.com/user/repo@v1", PackageScope::Project)
            .expect("installed_path")
            .expect("path");
        assert_eq!(
            git,
            dir.path()
                .join(Config::project_dir())
                .join("git")
                .join("github.com")
                .join("user/repo")
        );

        let git_with_auth = manager
            .installed_path_sync(
                "git:https://token@github.com/user/repo.git@main",
                PackageScope::Project,
            )
            .expect("installed_path")
            .expect("path");
        assert_eq!(git_with_auth, git);
    }

    #[test]
    fn test_installed_path_project_scope_local_git_hashes_absolute_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = PackageManager::new(dir.path().to_path_buf());

        let repo_path = dir.path().join("repo");
        fs::create_dir_all(&repo_path).expect("create local repo dir");

        let mut hasher = Sha256::new();
        hasher.update(repo_path.to_string_lossy().as_bytes());
        let digest = hasher.finalize();
        let key = hex_encode(&digest)[..16].to_string();

        let local = manager
            .installed_path_sync("git:./repo", PackageScope::Project)
            .expect("installed_path")
            .expect("path");
        assert_eq!(
            local,
            dir.path()
                .join(Config::project_dir())
                .join("git")
                .join("local")
                .join(key),
            "local git sources should map to a stable hashed install directory",
        );
    }

    #[test]
    fn installed_path_sync_rejects_blank_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = PackageManager::new(dir.path().to_path_buf());

        let err = manager
            .installed_path_sync("   ", PackageScope::Project)
            .expect_err("blank package source should fail");
        assert!(
            err.to_string().contains("Package source must be non-empty"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn remove_sync_rejects_git_host_only_source_without_deleting_host_bucket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = PackageManager::new(dir.path().to_path_buf());
        let host_bucket = dir
            .path()
            .join(Config::project_dir())
            .join("git")
            .join("github.com");
        let repo_dir = host_bucket.join("user").join("repo");
        fs::create_dir_all(&repo_dir).expect("create repo dir");
        fs::write(repo_dir.join("package.json"), "{}").expect("write sentinel");

        let err = manager
            .remove_sync("git:github.com", PackageScope::Project)
            .expect_err("host-only git source should be rejected");

        assert!(
            err.to_string().contains("Invalid git package source"),
            "unexpected error: {err}"
        );
        assert!(host_bucket.exists(), "host bucket should be preserved");
        assert!(repo_dir.exists(), "nested repo should be preserved");
    }

    #[test]
    fn test_project_settings_override_global_package_filters() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let project_root = temp_dir.path().join("project");
            fs::create_dir_all(project_root.join(".pi")).expect("create project settings dir");

            let package_root = temp_dir.path().join("pkg");
            fs::create_dir_all(package_root.join("extensions")).expect("create extensions dir");
            fs::write(package_root.join("extensions/a.native.json"), "{}")
                .expect("write a.native.json");
            fs::write(package_root.join("extensions/b.native.json"), "{}")
                .expect("write b.native.json");

            let global_settings_path = temp_dir.path().join("global-settings.json");
            let project_settings_path = project_root.join(".pi/settings.json");

            let global_settings = json!({
                "packages": [{
                    "source": package_root.to_string_lossy(),
                    "extensions": ["extensions/a.native.json"]
                }]
            });
            fs::write(
                &global_settings_path,
                serde_json::to_string_pretty(&global_settings).expect("serialize global settings"),
            )
            .expect("write global settings");

            let project_settings = json!({
                "packages": [{
                    "source": package_root.to_string_lossy(),
                    "extensions": ["extensions/b.native.json"]
                }]
            });
            fs::write(
                &project_settings_path,
                serde_json::to_string_pretty(&project_settings)
                    .expect("serialize project settings"),
            )
            .expect("write project settings");

            let roots = ResolveRoots {
                global_settings_path: global_settings_path.clone(),
                project_settings_path: project_settings_path.clone(),
                global_base_dir: temp_dir.path().join("global-base"),
                project_base_dir: project_root.join(".pi"),
                project_settings_enabled: true,
            };
            fs::create_dir_all(&roots.global_base_dir).expect("create global base dir");

            let manager = PackageManager::new(project_root);
            let resolved = manager.resolve_with_roots(&roots).await.expect("resolve");

            let enabled_extensions = resolved
                .extensions
                .iter()
                .filter(|entry| entry.enabled)
                .collect::<Vec<_>>();
            assert_eq!(enabled_extensions.len(), 1);
            let expected_path = package_root.join("extensions/b.native.json");
            assert_eq!(enabled_extensions[0].path, expected_path);
            assert_eq!(enabled_extensions[0].metadata.scope, PackageScope::Project);

            let disabled = resolved
                .extensions
                .iter()
                .find(|entry| entry.path == package_root.join("extensions/a.native.json"))
                .expect("a.native.json entry");
            assert!(!disabled.enabled);
            assert!(
                resolved
                    .extensions
                    .iter()
                    .all(|entry| entry.metadata.scope == PackageScope::Project)
            );
        });
    }

    #[test]
    fn test_list_packages_with_override_roots_ignores_project_settings() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let project_root = temp_dir.path().join("project");
        fs::create_dir_all(project_root.join(".pi")).expect("create project settings dir");

        let override_settings_path = temp_dir.path().join("override-settings.json");
        let project_settings_path = project_root.join(".pi/settings.json");

        fs::write(
            &override_settings_path,
            serde_json::to_string_pretty(&json!({
                "packages": ["npm:override-only"]
            }))
            .expect("serialize override settings"),
        )
        .expect("write override settings");
        fs::write(
            &project_settings_path,
            serde_json::to_string_pretty(&json!({
                "packages": ["npm:project-leak"]
            }))
            .expect("serialize project settings"),
        )
        .expect("write project settings");

        let roots = ResolveRoots::from_override(&project_root, Some(&override_settings_path));
        let manager = PackageManager::new(project_root);
        let packages = PackageManager::list_packages_with_roots(&roots).expect("list packages");

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].source, "npm:override-only");
        assert_eq!(packages[0].scope, PackageScope::User);
        assert!(
            !roots.project_settings_enabled,
            "full config override should disable project settings"
        );
    }

    #[test]
    fn test_config_override_roots_ignore_project_package_filters() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let project_root = temp_dir.path().join("project");
            fs::create_dir_all(project_root.join(".pi")).expect("create project settings dir");
            fs::create_dir_all(project_root.join(".pi/extensions"))
                .expect("create project extension dir");

            let package_root = temp_dir.path().join("pkg");
            fs::create_dir_all(package_root.join("extensions")).expect("create extensions dir");
            fs::write(package_root.join("extensions/a.native.json"), "{}")
                .expect("write a.native.json");

            let package_root2 = temp_dir.path().join("pkg2");
            fs::create_dir_all(package_root2.join("extensions")).expect("create extensions dir");
            fs::write(package_root2.join("extensions/b.native.json"), "{}")
                .expect("write b.native.json");
            let project_local_extension =
                project_root.join(".pi/extensions/project-local.native.json");
            let project_auto_extension =
                project_root.join(".pi/extensions/project-auto.native.json");
            fs::write(&project_local_extension, "{}").expect("write project local extension");
            fs::write(&project_auto_extension, "{}").expect("write project auto extension");

            let override_settings_path = temp_dir.path().join("override-settings.json");
            let project_settings_path = project_root.join(".pi/settings.json");

            let override_settings = json!({
                "packages": [{
                    "source": package_root.to_string_lossy(),
                    "extensions": ["extensions/a.native.json"]
                }]
            });
            fs::write(
                &override_settings_path,
                serde_json::to_string_pretty(&override_settings)
                    .expect("serialize override settings"),
            )
            .expect("write override settings");

            let project_settings = json!({
                "extensions": ["extensions/project-local.native.json"],
                "packages": [{
                    "source": package_root2.to_string_lossy(),
                    "extensions": ["extensions/b.native.json"]
                }]
            });
            fs::write(
                &project_settings_path,
                serde_json::to_string_pretty(&project_settings)
                    .expect("serialize project settings"),
            )
            .expect("write project settings");

            let roots = ResolveRoots {
                global_settings_path: override_settings_path.clone(),
                project_settings_path: project_settings_path.clone(),
                global_base_dir: temp_dir.path().join("global-base"),
                project_base_dir: project_root.join(".pi"),
                project_settings_enabled: false,
            };
            fs::create_dir_all(&roots.global_base_dir).expect("create global base dir");

            let manager = PackageManager::new(project_root);
            let resolved = manager.resolve_with_roots(&roots).await.expect("resolve");

            let enabled_extensions = resolved
                .extensions
                .iter()
                .filter(|entry| entry.enabled)
                .collect::<Vec<_>>();
            assert_eq!(enabled_extensions.len(), 1);
            assert_eq!(
                enabled_extensions[0].path,
                package_root.join("extensions/a.native.json")
            );
            assert_eq!(enabled_extensions[0].metadata.scope, PackageScope::User);
            assert!(
                resolved
                    .extensions
                    .iter()
                    .all(|entry| entry.path != package_root2.join("extensions/b.native.json")),
                "project package resources should be ignored when a full config override is active"
            );
            assert!(
                resolved
                    .extensions
                    .iter()
                    .all(|entry| entry.path != project_local_extension),
                "project local settings entries should be ignored when a full config override is active"
            );
            assert!(
                resolved
                    .extensions
                    .iter()
                    .all(|entry| entry.path != project_auto_extension),
                "project auto-discovered resources should be ignored when a full config override is active"
            );
        });
    }

    #[test]
    fn test_resolve_extension_sources_uses_temporary_scope() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let extension_path = temp_dir.path().join("ext.native.json");
            fs::write(&extension_path, "{}").expect("write extension");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let sources = vec![extension_path.to_string_lossy().to_string()];
            let resolved = manager
                .resolve_extension_sources(
                    &sources,
                    ResolveExtensionSourcesOptions {
                        local: false,
                        temporary: true,
                    },
                )
                .await
                .expect("resolve extension sources");

            assert_eq!(resolved.extensions.len(), 1);
            let entry = &resolved.extensions[0];
            assert!(entry.enabled);
            assert_eq!(entry.path, extension_path);
            assert_eq!(entry.metadata.scope, PackageScope::Temporary);
            assert_eq!(entry.metadata.origin, ResourceOrigin::Package);
            assert_eq!(entry.metadata.source, sources[0]);
        });
    }

    #[test]
    fn test_resolve_extension_sources_rejects_missing_local_path() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let missing_path = temp_dir.path().join("missing.native.json");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let err = manager
                .resolve_extension_sources(
                    &[missing_path.to_string_lossy().to_string()],
                    ResolveExtensionSourcesOptions {
                        local: false,
                        temporary: true,
                    },
                )
                .await
                .expect_err("missing CLI extension path should fail");

            assert!(
                err.to_string().contains("does not exist"),
                "unexpected error: {err}"
            );
        });
    }

    #[test]
    fn test_resolve_extension_sources_rejects_unsupported_local_file() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let unsupported_path = temp_dir.path().join("notes.txt");
            fs::write(&unsupported_path, "not an extension").expect("write unsupported file");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let err = manager
                .resolve_extension_sources(
                    &[unsupported_path.to_string_lossy().to_string()],
                    ResolveExtensionSourcesOptions {
                        local: false,
                        temporary: true,
                    },
                )
                .await
                .expect_err("unsupported CLI extension file should fail");

            assert!(
                err.to_string()
                    .contains("Unsupported extension source file"),
                "unexpected error: {err}"
            );
        });
    }

    #[test]
    fn test_resolve_local_path_normalizes_dot_segments() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let resolved = resolve_local_path("./foo/../bar", temp_dir.path());
        assert_eq!(resolved, temp_dir.path().join("bar"));
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_local_extension_source_accepts_symlink() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let extension_path = temp_dir.path().join("ext.native.json");
        fs::write(&extension_path, "{}").expect("write extension");

        let symlink_path = temp_dir.path().join("ext-link.native.json");
        std::os::unix::fs::symlink(&extension_path, &symlink_path).expect("create symlink");

        let mut accumulator = ResourceAccumulator::new();
        let mut metadata = PathMetadata {
            source: symlink_path.to_string_lossy().to_string(),
            scope: PackageScope::Temporary,
            origin: ResourceOrigin::Package,
            base_dir: None,
        };

        PackageManager::resolve_local_extension_source(
            &symlink_path,
            &mut accumulator,
            None,
            &mut metadata,
            true,
        )
        .expect("resolve symlink extension source");

        assert_eq!(accumulator.extensions.items.len(), 1);
        assert_eq!(accumulator.extensions.items[0].path, symlink_path);
    }

    #[test]
    fn test_resolve_extension_sources_rejects_blank_source() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let err = manager
                .resolve_extension_sources(
                    &["   ".to_string()],
                    ResolveExtensionSourcesOptions {
                        local: false,
                        temporary: true,
                    },
                )
                .await
                .expect_err("blank extension source should fail");

            assert!(
                err.to_string()
                    .contains("Extension source must be non-empty"),
                "unexpected error: {err}"
            );
        });
    }

    #[test]
    fn test_manifest_extensions_resolve_with_patterns() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let package_root = temp_dir.path().join("pkg");
            let extensions_dir = package_root.join("extensions");
            fs::create_dir_all(&extensions_dir).expect("create extensions dir");
            fs::write(extensions_dir.join("a.native.json"), "{}").expect("write a.native.json");
            fs::write(extensions_dir.join("b.native.json"), "{}").expect("write b.native.json");

            let manifest = json!({
                "name": "pkg",
                "version": "1.0.0",
                "pi": {
                    "extensions": ["extensions", "!extensions/b.native.json"]
                }
            });
            fs::write(
                package_root.join("package.json"),
                serde_json::to_string_pretty(&manifest).expect("serialize manifest"),
            )
            .expect("write manifest");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let sources = vec![package_root.to_string_lossy().to_string()];
            let resolved = manager
                .resolve_extension_sources(
                    &sources,
                    ResolveExtensionSourcesOptions {
                        local: false,
                        temporary: true,
                    },
                )
                .await
                .expect("resolve extension sources");

            let paths = resolved
                .extensions
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>();
            assert!(paths.contains(&package_root.join("extensions/a.native.json")));
            assert!(!paths.contains(&package_root.join("extensions/b.native.json")));
        });
    }

    #[test]
    fn test_resolve_extension_sources_accepts_single_string_manifest_extension() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let package_root = temp_dir.path().join("pkg");
            let entry_path = package_root.join("extensions").join("index.ts");
            fs::create_dir_all(entry_path.parent().expect("entry parent")).expect("create dir");
            fs::write(&entry_path, "export default {}").expect("write index.ts");
            fs::write(
                package_root.join("package.json"),
                serde_json::to_string_pretty(&json!({
                    "name": "pkg",
                    "version": "1.0.0",
                    "pi": {
                        "extensions": "extensions/index.ts"
                    }
                }))
                .expect("serialize manifest"),
            )
            .expect("write manifest");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let sources = vec![package_root.to_string_lossy().to_string()];
            let resolved = manager
                .resolve_extension_sources(
                    &sources,
                    ResolveExtensionSourcesOptions {
                        local: false,
                        temporary: true,
                    },
                )
                .await
                .expect("resolve extension sources");

            assert_eq!(resolved.extensions.len(), 1);
            let entry = &resolved.extensions[0];
            assert_eq!(entry.path, entry_path);
            assert!(entry.enabled);
            assert_eq!(entry.metadata.source, sources[0]);
        });
    }

    #[test]
    fn test_resolve_extension_sources_errors_on_malformed_package_manifest() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let package_root = temp_dir.path().join("pkg");
            let extensions_dir = package_root.join("extensions");
            fs::create_dir_all(&extensions_dir).expect("create extensions dir");
            fs::write(extensions_dir.join("a.native.json"), "{}").expect("write extension");
            fs::write(package_root.join("package.json"), "{ not valid json")
                .expect("write malformed package.json");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let err = manager
                .resolve_extension_sources(
                    &[package_root.to_string_lossy().to_string()],
                    ResolveExtensionSourcesOptions {
                        local: false,
                        temporary: true,
                    },
                )
                .await
                .expect_err("malformed package manifest must fail closed");

            let message = err.to_string();
            assert!(message.contains("Failed to parse package manifest"));
            assert!(message.contains(&package_root.join("package.json").display().to_string()));
        });
    }

    #[test]
    fn test_resolve_extension_sources_errors_on_outside_root_manifest_path() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let package_root = temp_dir.path().join("pkg");
            let escaped_dir = temp_dir.path().join("escaped");
            fs::create_dir_all(&package_root).expect("create package root");
            fs::create_dir_all(&escaped_dir).expect("create escaped dir");
            fs::write(escaped_dir.join("index.ts"), "export default {};")
                .expect("write escaped extension");
            fs::write(
                package_root.join("package.json"),
                serde_json::to_string_pretty(&json!({
                    "name": "pkg",
                    "pi": {
                        "extensions": ["../escaped/index.ts"]
                    }
                }))
                .expect("serialize package.json"),
            )
            .expect("write package.json");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let err = manager
                .resolve_extension_sources(
                    &[package_root.to_string_lossy().to_string()],
                    ResolveExtensionSourcesOptions {
                        local: false,
                        temporary: true,
                    },
                )
                .await
                .expect_err("outside-root manifest entries must fail closed");

            let message = err.to_string();
            assert!(message.contains("`pi.extensions` paths must stay within the package root"));
            assert!(message.contains(&package_root.join("package.json").display().to_string()));
        });
    }

    #[test]
    fn test_extension_manifest_directory_detected() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let extension_dir = temp_dir.path().join("ext");
        fs::create_dir_all(&extension_dir).expect("create extension dir");
        fs::write(
            extension_dir.join("extension.json"),
            serde_json::to_string_pretty(&json!({
                "schema": "pi.ext.manifest.v1",
                "extension_id": "test.ext",
                "name": "Test Extension",
                "version": "0.1.0",
                "api_version": "1.0",
                "runtime": "native-rust",
                "entrypoint": "index.native.json",
                "capabilities": []
            }))
            .expect("serialize extension manifest"),
        )
        .expect("write extension manifest");
        fs::write(extension_dir.join("index.native.json"), "{}").expect("write extension entry");

        let entries = resolve_extension_entries(&extension_dir).expect("entries");
        assert_eq!(entries, vec![extension_dir]);
    }

    #[test]
    fn test_resolve_extension_entries_skips_invalid_package_manifest() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let extension_dir = temp_dir.path().join("ext");
        fs::create_dir_all(&extension_dir).expect("create extension dir");
        fs::write(extension_dir.join("package.json"), "{ not valid json")
            .expect("write malformed package.json");
        fs::write(extension_dir.join("index.ts"), "export {};\n").expect("write fallback entry");

        assert!(
            resolve_extension_entries(&extension_dir).is_none(),
            "invalid package.json should not fall back to index.* entrypoints"
        );
    }

    #[test]
    fn test_resolve_extension_entries_skips_invalid_extension_manifest() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let extension_dir = temp_dir.path().join("ext");
        fs::create_dir_all(&extension_dir).expect("create extension dir");
        fs::write(extension_dir.join("extension.json"), "{ not valid json")
            .expect("write malformed extension.json");
        fs::write(extension_dir.join("index.ts"), "export {};\n").expect("write fallback entry");

        assert!(
            resolve_extension_entries(&extension_dir).is_none(),
            "invalid extension.json should not fall back to index.* entrypoints"
        );
    }

    // ======================================================================
    // is_pattern / split_patterns
    // ======================================================================

    #[test]
    fn is_pattern_detects_all_prefix_operators() {
        assert!(is_pattern("!exclude_me"));
        assert!(is_pattern("+force_include"));
        assert!(is_pattern("-force_exclude"));
        assert!(is_pattern("*.js"));
        assert!(is_pattern("foo?bar"));
        assert!(!is_pattern("plain_entry"));
        assert!(!is_pattern("extensions/a.js"));
        assert!(!is_pattern(""));
    }

    #[test]
    fn split_patterns_separates_plain_from_operators() {
        let entries = vec![
            "a.js".to_string(),
            "!b.js".to_string(),
            "c.js".to_string(),
            "+d.js".to_string(),
            "-e.js".to_string(),
            "*.ts".to_string(),
        ];
        let (plain, patterns) = split_patterns(&entries);
        assert_eq!(plain, vec!["a.js", "c.js"]);
        assert_eq!(patterns, vec!["!b.js", "+d.js", "-e.js", "*.ts"]);
    }

    #[test]
    fn split_patterns_empty_input() {
        let (plain, patterns) = split_patterns(&[]);
        assert!(plain.is_empty());
        assert!(patterns.is_empty());
    }

    // ======================================================================
    // posix_string / relative_posix
    // ======================================================================

    #[test]
    fn posix_string_normalizes_separators() {
        assert_eq!(posix_string(Path::new("a/b/c")), "a/b/c");
        assert_eq!(posix_string(Path::new("/abs/path")), "/abs/path");
    }

    #[test]
    fn relative_posix_computes_relative_path() {
        let base = Path::new("/home/user/project");
        let path = Path::new("/home/user/project/src/main.rs");
        assert_eq!(relative_posix(base, path), "src/main.rs");
    }

    #[test]
    fn relative_posix_with_parent_traversal() {
        let base = Path::new("/home/user/project/src");
        let path = Path::new("/home/user/project/tests/foo.rs");
        assert_eq!(relative_posix(base, path), "../tests/foo.rs");
    }

    #[test]
    fn relative_posix_no_common_prefix() {
        let base = Path::new("/a/b");
        let path = Path::new("/c/d");
        let result = relative_posix(base, path);
        assert_eq!(result, "../../c/d");
    }

    // ======================================================================
    // normalize_exact_pattern
    // ======================================================================

    #[test]
    fn normalize_exact_pattern_strips_dot_slash() {
        assert_eq!(normalize_exact_pattern("./foo.js"), "foo.js");
        assert_eq!(normalize_exact_pattern("foo.js"), "foo.js");
        assert_eq!(normalize_exact_pattern(""), "");
    }

    // ======================================================================
    // pattern_matches
    // ======================================================================

    #[test]
    fn pattern_matches_simple_glob() {
        assert!(pattern_matches("*.js", "foo.js"));
        assert!(pattern_matches("*.js", "bar.js"));
        assert!(!pattern_matches("*.js", "foo.ts"));
    }

    #[test]
    fn pattern_matches_exact() {
        assert!(pattern_matches("foo.js", "foo.js"));
        assert!(!pattern_matches("foo.js", "bar.js"));
    }

    #[test]
    fn pattern_matches_question_mark() {
        assert!(pattern_matches("?.js", "a.js"));
        assert!(!pattern_matches("?.js", "ab.js"));
    }

    // ======================================================================
    // looks_like_git_url / looks_like_local_path
    // ======================================================================

    #[test]
    fn looks_like_git_url_recognizes_known_hosts() {
        assert!(looks_like_git_url("github.com/user/repo"));
        assert!(looks_like_git_url("https://github.com/user/repo"));
        assert!(looks_like_git_url("gitlab.com/user/repo"));
        assert!(looks_like_git_url("bitbucket.org/user/repo"));
        assert!(looks_like_git_url("codeberg.org/user/repo"));
        assert!(!looks_like_git_url("example.com/user/repo"));
        assert!(!looks_like_git_url("npm:foo"));
        assert!(!looks_like_git_url("./local"));
    }

    #[test]
    fn looks_like_local_path_various_forms() {
        assert!(looks_like_local_path("."));
        assert!(looks_like_local_path(".."));
        assert!(looks_like_local_path("./relative"));
        assert!(looks_like_local_path("../parent"));
        assert!(looks_like_local_path("/absolute"));
        assert!(looks_like_local_path("C:/absolute"));
        assert!(looks_like_local_path("C:\\absolute"));
        assert!(looks_like_local_path("\\\\server\\share"));
        assert!(looks_like_local_path("~/home_relative"));
        assert!(looks_like_local_path("file:///abs/path"));
        assert!(!looks_like_local_path("npm:foo"));
        assert!(!looks_like_local_path("github.com/user/repo"));
    }

    // ======================================================================
    // hex_encode
    // ======================================================================

    #[test]
    fn hex_encode_correctness() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0xab, 0x12]), "00ffab12");
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    // ======================================================================
    // normalize_dot_segments
    // ======================================================================

    #[test]
    fn normalize_dot_segments_removes_current_dir() {
        let result = normalize_dot_segments(Path::new("/a/./b/./c"));
        assert_eq!(result, PathBuf::from("/a/b/c"));
    }

    #[test]
    fn normalize_dot_segments_resolves_parent_dir() {
        let result = normalize_dot_segments(Path::new("/a/b/../c"));
        assert_eq!(result, PathBuf::from("/a/c"));
    }

    #[test]
    fn normalize_dot_segments_multiple_parents() {
        let result = normalize_dot_segments(Path::new("/a/b/c/../../d"));
        assert_eq!(result, PathBuf::from("/a/d"));
    }

    #[test]
    fn normalize_dot_segments_cannot_go_above_root() {
        let result = normalize_dot_segments(Path::new("/a/../.."));
        assert_eq!(result, PathBuf::from("/"));
    }

    #[test]
    fn normalize_dot_segments_relative_path_keeps_parents() {
        let result = normalize_dot_segments(Path::new("a/../../b"));
        assert_eq!(result, PathBuf::from("../b"));
    }

    // ======================================================================
    // resolve_path_from_base
    // ======================================================================

    #[test]
    fn resolve_path_from_base_absolute_path() {
        let result = resolve_path_from_base("/abs/path", Path::new("/base"));
        assert_eq!(result, PathBuf::from("/abs/path"));
    }

    #[test]
    fn resolve_path_from_base_relative_path() {
        let result = resolve_path_from_base("foo/bar", Path::new("/base"));
        assert_eq!(result, PathBuf::from("/base/foo/bar"));
    }

    #[test]
    fn resolve_path_from_base_tilde_expansion() {
        let result = resolve_path_from_base("~/docs", Path::new("/base"));
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/base"));
        assert_eq!(result, home.join("docs"));
    }

    #[test]
    fn resolve_path_from_base_bare_tilde() {
        let result = resolve_path_from_base("~", Path::new("/base"));
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/base"));
        assert_eq!(result, home);
    }

    // ======================================================================
    // extract_string_array
    // ======================================================================

    #[test]
    fn extract_string_array_from_string() {
        let val = json!("single");
        let result = extract_string_array(Some(&val));
        assert_eq!(result, vec!["single"]);
    }

    #[test]
    fn extract_string_array_from_array() {
        let val = json!(["a", "b", "c"]);
        let result = extract_string_array(Some(&val));
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn extract_string_array_from_null() {
        let result = extract_string_array(None);
        assert!(result.is_empty());
    }

    #[test]
    fn extract_string_array_filters_non_strings() {
        let val = json!(["a", 42, "b", null, "c"]);
        let result = extract_string_array(Some(&val));
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    // ======================================================================
    // extract_package_spec / extract_filter_field
    // ======================================================================

    #[test]
    fn extract_package_spec_from_string() {
        let spec = extract_package_spec(&json!("npm:foo@1.0"));
        assert!(spec.is_some());
        let spec = spec.unwrap();
        assert_eq!(spec.source, "npm:foo@1.0");
        assert!(spec.filter.is_none());
    }

    #[test]
    fn extract_package_spec_from_object() {
        let val = json!({
            "source": "npm:bar",
            "extensions": ["a.js", "b.js"],
            "skills": "my-skill"
        });
        let spec = extract_package_spec(&val);
        assert!(spec.is_some());
        let spec = spec.unwrap();
        assert_eq!(spec.source, "npm:bar");
        let filter = spec.filter.unwrap();
        assert_eq!(
            filter.extensions,
            Some(vec!["a.js".to_string(), "b.js".to_string()])
        );
        assert_eq!(filter.skills, Some(vec!["my-skill".to_string()]));
        assert!(filter.prompts.is_none());
        assert!(filter.themes.is_none());
    }

    #[test]
    fn extract_package_spec_from_object_missing_source() {
        let val = json!({"extensions": ["a.js"]});
        assert!(extract_package_spec(&val).is_none());
    }

    #[test]
    fn extract_package_spec_from_non_string_non_object() {
        assert!(extract_package_spec(&json!(42)).is_none());
        assert!(extract_package_spec(&json!(null)).is_none());
        assert!(extract_package_spec(&json!(true)).is_none());
    }

    #[test]
    fn extract_package_spec_rejects_blank_sources() {
        assert!(extract_package_spec(&json!("   ")).is_none());
        assert!(extract_package_spec(&json!({"source": " \t "})).is_none());
    }

    #[test]
    fn extract_filter_field_absent_key() {
        let obj = serde_json::Map::new();
        assert!(extract_filter_field(&obj, "extensions").is_none());
    }

    #[test]
    fn extract_filter_field_string_value() {
        let mut obj = serde_json::Map::new();
        obj.insert("skills".to_string(), json!("my-skill"));
        let result = extract_filter_field(&obj, "skills");
        assert_eq!(result, Some(vec!["my-skill".to_string()]));
    }

    #[test]
    fn extract_filter_field_array_value() {
        let mut obj = serde_json::Map::new();
        obj.insert("themes".to_string(), json!(["dark", "light"]));
        let result = extract_filter_field(&obj, "themes");
        assert_eq!(result, Some(vec!["dark".to_string(), "light".to_string()]));
    }

    #[test]
    fn extract_filter_field_non_string_array_or_null() {
        let mut obj = serde_json::Map::new();
        obj.insert("prompts".to_string(), json!(42));
        let result = extract_filter_field(&obj, "prompts");
        assert_eq!(result, Some(Vec::<String>::new()));
    }

    // ======================================================================
    // is_enabled_by_overrides
    // ======================================================================

    #[test]
    fn is_enabled_by_overrides_no_overrides_enables_all() {
        let path = Path::new("/base/extensions/foo.js");
        let patterns: Vec<String> = vec!["extensions/foo.js".to_string()];
        assert!(is_enabled_by_overrides(path, &patterns, Path::new("/base")));
    }

    #[test]
    fn is_enabled_by_overrides_exclude_disables() {
        let path = Path::new("/base/extensions/foo.js");
        let patterns = vec!["!*.js".to_string()];
        assert!(!is_enabled_by_overrides(
            path,
            &patterns,
            Path::new("/base")
        ));
    }

    #[test]
    fn is_enabled_by_overrides_force_include_overrides_exclude() {
        let path = Path::new("/base/extensions/foo.js");
        let patterns = vec!["!*.js".to_string(), "+extensions/foo.js".to_string()];
        assert!(is_enabled_by_overrides(path, &patterns, Path::new("/base")));
    }

    #[test]
    fn is_enabled_by_overrides_force_exclude_overrides_force_include() {
        let path = Path::new("/base/extensions/foo.js");
        let patterns = vec![
            "+extensions/foo.js".to_string(),
            "-extensions/foo.js".to_string(),
        ];
        assert!(!is_enabled_by_overrides(
            path,
            &patterns,
            Path::new("/base")
        ));
    }

    // ======================================================================
    // apply_patterns
    // ======================================================================

    #[test]
    fn apply_patterns_include_glob() {
        let base = Path::new("/base");
        let paths = vec![
            PathBuf::from("/base/a.js"),
            PathBuf::from("/base/b.ts"),
            PathBuf::from("/base/c.js"),
        ];
        let patterns = vec!["*.js".to_string()];
        let result = apply_patterns(&paths, &patterns, base);
        assert!(result.contains(&PathBuf::from("/base/a.js")));
        assert!(result.contains(&PathBuf::from("/base/c.js")));
        assert!(!result.contains(&PathBuf::from("/base/b.ts")));
    }

    #[test]
    fn apply_patterns_exclude_removes_from_includes() {
        let base = Path::new("/base");
        let paths = vec![
            PathBuf::from("/base/a.js"),
            PathBuf::from("/base/b.js"),
            PathBuf::from("/base/c.js"),
        ];
        let patterns = vec!["*.js".to_string(), "!b.js".to_string()];
        let result = apply_patterns(&paths, &patterns, base);
        assert!(result.contains(&PathBuf::from("/base/a.js")));
        assert!(!result.contains(&PathBuf::from("/base/b.js")));
        assert!(result.contains(&PathBuf::from("/base/c.js")));
    }

    #[test]
    fn apply_patterns_no_patterns_returns_all() {
        let base = Path::new("/base");
        let paths = vec![PathBuf::from("/base/a.js"), PathBuf::from("/base/b.js")];
        let result = apply_patterns(&paths, &[], base);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn apply_patterns_force_include_adds_excluded() {
        let base = Path::new("/base");
        let paths = vec![PathBuf::from("/base/a.js"), PathBuf::from("/base/b.js")];
        let patterns = vec!["a.js".to_string(), "+b.js".to_string()];
        let result = apply_patterns(&paths, &patterns, base);
        assert!(result.contains(&PathBuf::from("/base/a.js")));
        assert!(result.contains(&PathBuf::from("/base/b.js")));
    }

    #[test]
    fn apply_patterns_force_exclude_removes_everything() {
        let base = Path::new("/base");
        let paths = vec![PathBuf::from("/base/a.js"), PathBuf::from("/base/b.js")];
        let patterns = vec!["-a.js".to_string()];
        let result = apply_patterns(&paths, &patterns, base);
        assert!(!result.contains(&PathBuf::from("/base/a.js")));
        assert!(result.contains(&PathBuf::from("/base/b.js")));
    }

    // ======================================================================
    // normalize_source / sources_match (additional coverage)
    // ======================================================================

    #[test]
    fn normalize_source_empty_returns_none() {
        assert!(normalize_source("").is_none());
        assert!(normalize_source("  ").is_none());
    }

    #[test]
    fn normalize_source_npm() {
        let result = normalize_source("npm:@scope/pkg@2.0.0").unwrap();
        assert_eq!(result.kind, NormalizedKind::Npm);
        assert_eq!(result.key, "@scope/pkg");
    }

    #[test]
    fn normalize_source_git() {
        let result = normalize_source("git:github.com/user/repo@v1").unwrap();
        assert_eq!(result.kind, NormalizedKind::Git);
        assert_eq!(result.key, "github.com/user/repo");
    }

    #[test]
    fn normalize_source_https_git_url() {
        let result = normalize_source("https://github.com/user/repo.git@v2").unwrap();
        assert_eq!(result.kind, NormalizedKind::Git);
        assert_eq!(result.key, "github.com/user/repo");
    }

    #[test]
    fn normalize_source_git_https_with_userinfo_and_ref() {
        let result = normalize_source("git:https://token@github.com/user/repo.git@v2").unwrap();
        assert_eq!(result.kind, NormalizedKind::Git);
        assert_eq!(result.key, "github.com/user/repo");
    }

    #[test]
    fn normalize_source_local() {
        let result = normalize_source("my-local-package").unwrap();
        assert_eq!(result.kind, NormalizedKind::Local);
        assert_eq!(result.key, "my-local-package");
    }

    // ======================================================================
    // parse_npm_spec (additional edge cases)
    // ======================================================================

    #[test]
    fn parse_npm_spec_empty() {
        assert_eq!(parse_npm_spec(""), (String::new(), None));
    }

    #[test]
    fn parse_npm_spec_whitespace() {
        assert_eq!(parse_npm_spec("  foo  "), ("foo".to_string(), None));
    }

    #[test]
    fn parse_npm_spec_scoped_with_version() {
        let (name, version) = parse_npm_spec("@org/pkg@^3.0.0");
        assert_eq!(name, "@org/pkg");
        assert_eq!(version, Some("^3.0.0".to_string()));
    }

    #[test]
    fn parse_npm_spec_trailing_at() {
        let (name, version) = parse_npm_spec("foo@");
        assert_eq!(name, "foo@");
        assert!(version.is_none());
    }

    // ======================================================================
    // ResourceList dedup
    // ======================================================================

    #[test]
    fn resource_list_deduplicates_by_path() {
        let mut list = ResourceList::default();
        let metadata = PathMetadata {
            source: "test".to_string(),
            scope: PackageScope::User,
            origin: ResourceOrigin::Package,
            base_dir: None,
        };
        list.add(PathBuf::from("/a"), &metadata, true);
        list.add(PathBuf::from("/a"), &metadata, true);
        list.add(PathBuf::from("/b"), &metadata, false);
        assert_eq!(list.items.len(), 2);
        assert_eq!(list.items[0].path, PathBuf::from("/a"));
        assert_eq!(list.items[1].path, PathBuf::from("/b"));
    }

    // ======================================================================
    // ResourceAccumulator into_resolved_paths
    // ======================================================================

    #[test]
    fn resource_accumulator_sorts_by_path() {
        let mut acc = ResourceAccumulator::new();
        let metadata = PathMetadata {
            source: "test".to_string(),
            scope: PackageScope::User,
            origin: ResourceOrigin::Package,
            base_dir: None,
        };
        acc.extensions.add(PathBuf::from("/z/ext"), &metadata, true);
        acc.extensions.add(PathBuf::from("/a/ext"), &metadata, true);
        acc.skills.add(PathBuf::from("/z/skill"), &metadata, true);
        acc.skills.add(PathBuf::from("/a/skill"), &metadata, true);

        let resolved = acc.into_resolved_paths();
        assert_eq!(resolved.extensions[0].path, PathBuf::from("/a/ext"));
        assert_eq!(resolved.extensions[1].path, PathBuf::from("/z/ext"));
        assert_eq!(resolved.skills[0].path, PathBuf::from("/a/skill"));
        assert_eq!(resolved.skills[1].path, PathBuf::from("/z/skill"));
    }

    // ======================================================================
    // SettingsSnapshot::entries_for
    // ======================================================================

    #[test]
    fn settings_snapshot_entries_for_returns_correct_type() {
        let snapshot = SettingsSnapshot {
            packages: vec![],
            extensions: vec!["ext".to_string()],
            skills: vec!["skill".to_string()],
            prompts: vec!["prompt".to_string()],
            themes: vec!["theme".to_string()],
        };
        assert_eq!(snapshot.entries_for(ResourceType::Extensions), &["ext"]);
        assert_eq!(snapshot.entries_for(ResourceType::Skills), &["skill"]);
        assert_eq!(snapshot.entries_for(ResourceType::Prompts), &["prompt"]);
        assert_eq!(snapshot.entries_for(ResourceType::Themes), &["theme"]);
    }

    // ======================================================================
    // read_settings_json / read_settings_snapshot (filesystem tests)
    // ======================================================================

    #[test]
    fn read_settings_json_missing_file_returns_empty_object() {
        let result = read_settings_json(Path::new("/nonexistent/path/settings.json"));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), json!({}));
    }

    #[test]
    fn read_settings_json_valid_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"foo": "bar"}"#).expect("write");
        let result = read_settings_json(&path).expect("read");
        assert_eq!(result, json!({"foo": "bar"}));
    }

    #[test]
    fn read_settings_json_invalid_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, "not json").expect("write");
        assert!(read_settings_json(&path).is_err());
    }

    #[test]
    fn read_settings_snapshot_with_packages_and_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        let settings = json!({
            "packages": ["npm:foo", {"source": "npm:bar", "extensions": ["a.js"]}],
            "extensions": ["ext1.js"],
            "skills": "my-skill",
            "themes": ["dark.json", "light.json"]
        });
        fs::write(&path, serde_json::to_string(&settings).unwrap()).expect("write");
        let snapshot = read_settings_snapshot(&path).expect("read");
        assert_eq!(snapshot.packages.len(), 2);
        assert_eq!(snapshot.packages[0].source, "npm:foo");
        assert_eq!(snapshot.packages[1].source, "npm:bar");
        assert!(snapshot.packages[1].filter.is_some());
        assert_eq!(snapshot.extensions, vec!["ext1.js"]);
        assert_eq!(snapshot.skills, vec!["my-skill"]);
        assert_eq!(snapshot.themes, vec!["dark.json", "light.json"]);
        assert!(snapshot.prompts.is_empty());
    }

    // ======================================================================
    // write_settings_json_atomic / update_package_sources
    // ======================================================================

    #[test]
    fn write_settings_json_atomic_creates_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sub/settings.json");
        let value = json!({"test": true});
        write_settings_json_atomic(&path, &value).expect("write");
        let content = fs::read_to_string(&path).expect("read");
        let parsed: Value = serde_json::from_str(&content).expect("parse");
        assert_eq!(parsed, json!({"test": true}));
    }

    #[test]
    fn update_package_sources_add_and_remove() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, "{}").expect("write initial");

        update_package_sources(&path, "npm:foo", UpdateAction::Add, dir.path()).expect("add");
        let settings = read_settings_json(&path).expect("read");
        let packages = settings["packages"].as_array().expect("packages array");
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0], json!("npm:foo"));

        // Adding same source again should not duplicate
        update_package_sources(&path, "npm:foo@2.0", UpdateAction::Add, dir.path())
            .expect("add again");
        let settings = read_settings_json(&path).expect("read");
        let packages = settings["packages"].as_array().expect("packages array");
        assert_eq!(packages.len(), 1, "duplicate source should not be added");

        update_package_sources(&path, "npm:foo", UpdateAction::Remove, dir.path()).expect("remove");
        let settings = read_settings_json(&path).expect("read");
        let packages = settings["packages"].as_array().expect("packages array");
        assert!(packages.is_empty());
    }

    #[test]
    fn update_package_sources_normalizes_non_array_packages() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        let malformed = json!({
            "packages": { "source": "npm:legacy" }
        });
        fs::write(
            &path,
            serde_json::to_string(&malformed).expect("serialize malformed settings"),
        )
        .expect("write malformed settings");

        update_package_sources(&path, "npm:foo", UpdateAction::Add, dir.path()).expect("add");
        let settings = read_settings_json(&path).expect("read");
        let packages = settings["packages"].as_array().expect("packages array");
        assert_eq!(packages, &vec![json!("npm:foo")]);
    }

    #[test]
    fn update_package_sources_normalizes_non_object_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, "[]").expect("write malformed root");

        update_package_sources(&path, "npm:foo", UpdateAction::Add, dir.path()).expect("add");
        let settings = read_settings_json(&path).expect("read");
        let packages = settings["packages"].as_array().expect("packages array");
        assert_eq!(packages, &vec![json!("npm:foo")]);
    }

    #[test]
    fn update_package_sources_rejects_empty_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, "{}").expect("write initial");

        let err = update_package_sources(&path, "   ", UpdateAction::Add, dir.path())
            .expect_err("must fail");
        assert!(
            err.to_string()
                .contains("settings package source cannot be empty"),
            "unexpected error: {err}"
        );

        let settings = read_settings_json(&path).expect("read");
        assert!(
            settings.get("packages").is_none(),
            "failed update must not mutate packages"
        );
    }

    #[test]
    fn update_package_sources_deduplicates_equivalent_local_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, "{}").expect("write initial");

        update_package_sources(&path, "./foo/../bar", UpdateAction::Add, dir.path()).expect("add");
        update_package_sources(&path, "./bar", UpdateAction::Add, dir.path()).expect("add again");

        let settings = read_settings_json(&path).expect("read");
        let packages = settings["packages"].as_array().expect("packages array");
        assert_eq!(
            packages.len(),
            1,
            "equivalent local paths should deduplicate"
        );
        assert_eq!(packages[0], json!("./foo/../bar"));
    }

    #[test]
    fn update_package_sources_remove_matches_equivalent_local_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        fs::write(&path, "{}").expect("write initial");

        update_package_sources(&path, "./foo/../bar", UpdateAction::Add, dir.path()).expect("add");
        update_package_sources(&path, "./bar", UpdateAction::Remove, dir.path()).expect("remove");

        let settings = read_settings_json(&path).expect("read");
        let packages = settings["packages"].as_array().expect("packages array");
        assert!(packages.is_empty(), "equivalent local paths should remove");
    }

    // ======================================================================
    // list_packages_in_settings
    // ======================================================================

    #[test]
    fn list_packages_in_settings_reads_all_formats() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        let settings = json!({
            "packages": [
                "npm:foo",
                {"source": "git:github.com/user/repo", "extensions": ["a.js"]}
            ]
        });
        fs::write(&path, serde_json::to_string(&settings).unwrap()).expect("write");
        let packages = list_packages_in_settings(&path).expect("list");
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].source, "npm:foo");
        assert!(packages[0].filter.is_none());
        assert_eq!(packages[1].source, "git:github.com/user/repo");
        assert!(packages[1].filter.is_some());
    }

    // ======================================================================
    // read_pi_manifest
    // ======================================================================

    #[test]
    fn read_pi_manifest_with_pi_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = json!({
            "name": "test-pkg",
            "version": "1.0.0",
            "pi": {
                "extensions": ["ext/a.js", "ext/b.js"],
                "skills": ["skills/foo.md"]
            }
        });
        fs::write(
            dir.path().join("package.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .expect("write");
        let result = read_pi_manifest(dir.path()).expect("read manifest");
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(
            result.extensions,
            Some(vec!["ext/a.js".to_string(), "ext/b.js".to_string()])
        );
        assert_eq!(result.skills, Some(vec!["skills/foo.md".to_string()]));
        assert!(result.prompts.is_none());
        assert!(result.themes.is_none());
    }

    #[test]
    fn read_pi_manifest_accepts_single_string_resource_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = json!({
            "name": "test-pkg",
            "version": "1.0.0",
            "pi": {
                "extensions": "ext/a.js",
                "skills": "skills/foo.md",
                "prompts": "prompts/intro.md",
                "themes": "themes/default.json"
            }
        });
        fs::write(
            dir.path().join("package.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .expect("write");

        let result = read_pi_manifest(dir.path()).expect("read manifest");
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.extensions, Some(vec!["ext/a.js".to_string()]));
        assert_eq!(result.skills, Some(vec!["skills/foo.md".to_string()]));
        assert_eq!(result.prompts, Some(vec!["prompts/intro.md".to_string()]));
        assert_eq!(result.themes, Some(vec!["themes/default.json".to_string()]));
    }

    #[test]
    fn read_pi_manifest_no_pi_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("package.json"),
            r#"{"name": "test", "version": "1.0.0"}"#,
        )
        .expect("write");
        assert!(
            read_pi_manifest(dir.path())
                .expect("read manifest")
                .is_none()
        );
    }

    #[test]
    fn read_pi_manifest_no_package_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            read_pi_manifest(dir.path())
                .expect("read manifest")
                .is_none()
        );
    }

    #[test]
    fn read_pi_manifest_errors_on_malformed_package_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_path = dir.path().join("package.json");
        fs::write(&manifest_path, "{ not valid json").expect("write malformed package.json");

        let err = read_pi_manifest(dir.path()).expect_err("malformed package.json must error");
        let message = err.to_string();
        assert!(message.contains("Failed to parse package manifest"));
        assert!(message.contains(&manifest_path.display().to_string()));
    }

    #[test]
    fn read_pi_manifest_errors_when_pi_field_is_not_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_path = dir.path().join("package.json");
        fs::write(&manifest_path, r#"{"name":"pkg","pi":"not-an-object"}"#)
            .expect("write invalid pi manifest");

        let err = read_pi_manifest(dir.path()).expect_err("non-object `pi` field must error");
        let message = err.to_string();
        assert!(message.contains("Invalid package manifest"));
        assert!(message.contains("`pi` must be an object"));
        assert!(message.contains(&manifest_path.display().to_string()));
    }

    #[test]
    fn read_pi_manifest_errors_when_resource_entries_are_not_string_lists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_path = dir.path().join("package.json");
        fs::write(
            &manifest_path,
            r#"{"name":"pkg","pi":{"extensions":["ok",7]}}"#,
        )
        .expect("write invalid pi manifest");

        let err = read_pi_manifest(dir.path()).expect_err("non-string manifest entries must error");
        let message = err.to_string();
        assert!(message.contains("Invalid package manifest"));
        assert!(message.contains("`pi.extensions` must be a string or array of strings"));
        assert!(message.contains(&manifest_path.display().to_string()));
    }

    #[test]
    fn read_pi_manifest_errors_when_resource_entry_escapes_package_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_path = dir.path().join("package.json");
        fs::write(
            &manifest_path,
            r#"{"name":"pkg","pi":{"extensions":["../outside/index.ts"]}}"#,
        )
        .expect("write invalid pi manifest");

        let err =
            read_pi_manifest(dir.path()).expect_err("outside-root manifest entries must error");
        let message = err.to_string();
        assert!(message.contains("Invalid package manifest"));
        assert!(message.contains("`pi.extensions` paths must stay within the package root"));
        assert!(message.contains(&manifest_path.display().to_string()));
    }

    #[test]
    fn read_pi_manifest_errors_when_resource_pattern_escapes_package_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_path = dir.path().join("package.json");
        fs::write(
            &manifest_path,
            r#"{"name":"pkg","pi":{"extensions":["extensions","+../outside/index.ts"]}}"#,
        )
        .expect("write invalid pi manifest");

        let err =
            read_pi_manifest(dir.path()).expect_err("outside-root override patterns must error");
        let message = err.to_string();
        assert!(message.contains("Invalid package manifest"));
        assert!(message.contains("`pi.extensions` paths must stay within the package root"));
        assert!(message.contains(&manifest_path.display().to_string()));
    }

    // ======================================================================
    // temporary_dir
    // ======================================================================

    #[test]
    fn temporary_dir_stable_hash() {
        let a = temporary_dir("npm", None);
        let b = temporary_dir("npm", None);
        assert_eq!(a, b, "same inputs should produce same path");

        let c = temporary_dir("npm", Some("foo"));
        assert_ne!(a, c, "different suffix should produce different path");
    }

    #[test]
    fn temporary_dir_includes_prefix() {
        let result = temporary_dir("git-github.com", Some("user/repo"));
        let path_str = result.to_string_lossy();
        assert!(path_str.contains("pi-extensions"));
        assert!(path_str.contains("git-github.com"));
    }

    #[test]
    fn npm_prefix_root_matches_scope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = PackageManager::new(dir.path().to_path_buf());

        assert_eq!(
            manager.npm_prefix_root(PackageScope::Project),
            Some(dir.path().join(Config::project_dir()).join("npm"))
        );
        assert_eq!(
            manager.npm_prefix_root(PackageScope::Temporary),
            Some(temporary_dir("npm", None))
        );
        assert_eq!(manager.npm_prefix_root(PackageScope::User), None);
    }

    // ======================================================================
    // compat_scan_enabled
    // ======================================================================

    #[test]
    fn compat_scan_enabled_recognizes_truthy_values() {
        // Test the parsing logic inline (cannot mutate env in safe Rust 2024)
        let truthy = ["1", "true", "yes", "on", "TRUE", "Yes", "ON"];
        for val in truthy {
            let lower = val.trim().to_ascii_lowercase();
            assert!(
                matches!(lower.as_str(), "1" | "true" | "yes" | "on"),
                "{val} should be truthy"
            );
        }
        let falsy = ["0", "false", "no", "off", "", "random"];
        for val in falsy {
            let lower = val.trim().to_ascii_lowercase();
            assert!(
                !matches!(lower.as_str(), "1" | "true" | "yes" | "on"),
                "{val} should be falsy"
            );
        }
    }

    // ======================================================================
    // parse_source (additional cases)
    // ======================================================================

    #[test]
    fn parse_source_npm_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        match parse_source("npm:@scope/pkg@1.0.0", dir.path()) {
            ParsedSource::Npm { spec, name, pinned } => {
                assert_eq!(spec, "@scope/pkg@1.0.0");
                assert_eq!(name, "@scope/pkg");
                assert!(pinned);
            }
            other => panic!("Unexpected parsed source: {:?}", other),
        }
    }

    #[test]
    fn parse_source_npm_pins_only_exact_versions() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (source, expected_pinned) in [
            // Exact versions pin (upstream `isExactNpmVersion` semantics).
            ("npm:pkg@1.2.3", true),
            ("npm:pkg@v1.2.3", true),
            ("npm:@scope/pkg@2.0.0-beta.1", true),
            // Local references stay pinned so `pi update` leaves them alone.
            ("npm:pkg@file:../pkg", true),
            ("npm:pkg@link:../pkg", true),
            // Ranges, partial versions, and dist-tags float.
            ("npm:pkg@^1.2.0", false),
            ("npm:pkg@~1.2", false),
            ("npm:@scope/pkg@1.0", false),
            ("npm:pkg@1.x", false),
            ("npm:pkg@>=2.0.0", false),
            ("npm:pkg@latest", false),
            ("npm:express", false),
        ] {
            let ParsedSource::Npm { pinned, .. } = parse_source(source, dir.path()) else {
                panic!("Unexpected parsed source for {source}");
            };
            assert_eq!(pinned, expected_pinned, "pinned mismatch for {source}");
        }
    }

    #[test]
    fn parse_source_npm_unpinned() {
        let dir = tempfile::tempdir().expect("tempdir");
        match parse_source("npm:express", dir.path()) {
            ParsedSource::Npm { pinned, .. } => {
                assert!(!pinned);
            }
            other => panic!("Unexpected parsed source: {:?}", other),
        }
    }

    #[test]
    fn parse_source_git_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        match parse_source("git:github.com/user/repo@v2", dir.path()) {
            ParsedSource::Git {
                clone_source,
                repo,
                host,
                path,
                r#ref,
                pinned,
            } => {
                assert_eq!(clone_source, "github.com/user/repo");
                assert_eq!(repo, "github.com/user/repo");
                assert_eq!(host, "github.com");
                assert_eq!(path, "user/repo");
                assert_eq!(r#ref, Some("v2".to_string()));
                assert!(pinned);
            }
            other => panic!("Unexpected parsed source: {:?}", other),
        }
    }

    #[test]
    fn parse_source_https_github_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        match parse_source("https://github.com/user/repo.git", dir.path()) {
            ParsedSource::Git { repo, host, .. } => {
                assert_eq!(repo, "github.com/user/repo");
                assert_eq!(host, "github.com");
            }
            other => panic!(),
        }
    }

    #[test]
    fn parse_source_https_github_url_with_userinfo_and_ref() {
        let dir = tempfile::tempdir().expect("tempdir");
        match parse_source(
            "git:https://token@github.com/user/repo.git@main",
            dir.path(),
        ) {
            ParsedSource::Git {
                clone_source,
                repo,
                host,
                path,
                r#ref,
                pinned,
            } => {
                assert_eq!(clone_source, "https://token@github.com/user/repo.git");
                assert_eq!(repo, "github.com/user/repo");
                assert_eq!(host, "github.com");
                assert_eq!(path, "user/repo");
                assert_eq!(r#ref, Some("main".to_string()));
                assert!(pinned);
            }
            other => panic!("Unexpected parsed source: {:?}", other),
        }
    }

    #[test]
    fn parse_source_local_relative() {
        let dir = tempfile::tempdir().expect("tempdir");
        match parse_source("./my-ext", dir.path()) {
            ParsedSource::Local { path } => {
                assert_eq!(path, dir.path().join("my-ext"));
            }
            other => panic!(),
        }
    }

    #[test]
    #[cfg(unix)]
    fn parse_source_local_absolute() {
        let dir = tempfile::tempdir().expect("tempdir");
        match parse_source("/abs/my-ext", dir.path()) {
            ParsedSource::Local { path } => {
                assert_eq!(path, PathBuf::from("/abs/my-ext"));
            }
            other => panic!(),
        }
    }

    #[test]
    fn parse_source_windows_drive_absolute_stays_absolute() {
        let dir = tempfile::tempdir().expect("tempdir");
        match parse_source("C:\\packages\\demo", dir.path()) {
            ParsedSource::Local { path } => {
                assert_eq!(path, PathBuf::from("C:\\packages\\demo"));
            }
            other => panic!("Unexpected parsed source: {other:?}"),
        }

        match parse_source("C:/packages/demo", dir.path()) {
            ParsedSource::Local { path } => {
                assert_eq!(path, PathBuf::from("C:/packages/demo"));
            }
            other => panic!("Unexpected parsed source: {other:?}"),
        }
    }

    #[test]
    fn parse_git_source_windows_drive_path_treated_as_local_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        match parse_source("git:C:\\packages\\demo@main", dir.path()) {
            ParsedSource::Git {
                clone_source,
                repo,
                host,
                path,
                r#ref,
                pinned,
            } => {
                assert_eq!(clone_source, "C:\\packages\\demo");
                assert_eq!(repo, "C:\\packages\\demo");
                assert_eq!(host, "local");
                assert_eq!(path.len(), 16);
                assert_eq!(r#ref, Some("main".to_string()));
                assert!(pinned);
            }
            other => panic!("Unexpected parsed source: {other:?}"),
        }
    }

    #[test]
    fn local_path_from_file_url_keeps_windows_drive_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            local_path_from_spec("file:///C:/packages/demo", dir.path()),
            PathBuf::from("C:/packages/demo")
        );
        assert_eq!(
            local_path_from_spec("file:///C:\\packages\\demo", dir.path()),
            PathBuf::from("C:\\packages\\demo")
        );
        assert_eq!(
            local_path_from_spec("file://C:/packages/demo", dir.path()),
            PathBuf::from("C:/packages/demo")
        );
        assert_eq!(
            local_path_from_spec("file://C:\\packages\\demo", dir.path()),
            PathBuf::from("C:\\packages\\demo")
        );
    }

    #[test]
    fn local_path_from_file_url_localhost_keeps_absolute_root() {
        let cwd = Path::new("/home/user/project");
        assert_eq!(
            local_path_from_spec("file://localhost/tmp/repo", cwd),
            PathBuf::from("/tmp/repo")
        );
        assert_eq!(
            local_path_from_spec("file://localhost", cwd),
            PathBuf::from("/")
        );
        assert_eq!(
            local_path_from_spec("file://localhost/C:/packages/demo", cwd),
            PathBuf::from("C:/packages/demo")
        );
        assert_eq!(
            local_path_from_spec("file://localhost/C:\\packages\\demo", cwd),
            PathBuf::from("C:\\packages\\demo")
        );
    }

    #[test]
    fn local_path_from_file_url_network_share_keeps_network_root() {
        let cwd = Path::new("/home/user/project");
        assert_eq!(
            local_path_from_spec("file://server/share/repo", cwd),
            PathBuf::from("//server/share/repo")
        );
    }

    // ======================================================================
    // parse_git_source local paths
    // ======================================================================

    #[test]
    fn parse_git_source_local_path_hashes_deterministically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result1 = parse_git_source("./local-repo", dir.path());
        let result2 = parse_git_source("./local-repo", dir.path());
        match (&result1, &result2) {
            (
                ParsedSource::Git {
                    clone_source: clone_a,
                    path: p1,
                    ..
                },
                ParsedSource::Git {
                    clone_source: clone_b,
                    path: p2,
                    ..
                },
            ) => {
                assert_eq!(clone_a, &dir.path().join("local-repo").to_string_lossy());
                assert_eq!(clone_b, &dir.path().join("local-repo").to_string_lossy());
                assert_eq!(p1, p2, "same local source should produce same hash");
            }
            _ => panic!(),
        }
    }

    // ======================================================================
    // PackageManager::dedupe_packages
    // ======================================================================

    #[test]
    fn dedupe_packages_project_wins_over_user() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = PackageManager::new(dir.path().to_path_buf());

        let packages = vec![
            ScopedPackage {
                pkg: PackageSpec {
                    source: "npm:foo@1.0".to_string(),
                    filter: None,
                },
                scope: PackageScope::User,
            },
            ScopedPackage {
                pkg: PackageSpec {
                    source: "npm:foo@2.0".to_string(),
                    filter: None,
                },
                scope: PackageScope::Project,
            },
        ];

        let deduped = manager.dedupe_packages(packages);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].scope, PackageScope::Project);
        assert_eq!(deduped[0].pkg.source, "npm:foo@2.0");
    }

    #[test]
    fn dedupe_packages_user_does_not_override_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = PackageManager::new(dir.path().to_path_buf());

        let packages = vec![
            ScopedPackage {
                pkg: PackageSpec {
                    source: "npm:bar@1.0".to_string(),
                    filter: None,
                },
                scope: PackageScope::Project,
            },
            ScopedPackage {
                pkg: PackageSpec {
                    source: "npm:bar@2.0".to_string(),
                    filter: None,
                },
                scope: PackageScope::User,
            },
        ];

        let deduped = manager.dedupe_packages(packages);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].scope, PackageScope::Project);
        assert_eq!(deduped[0].pkg.source, "npm:bar@1.0");
    }

    #[test]
    fn dedupe_packages_skips_blank_sources() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = PackageManager::new(dir.path().to_path_buf());

        let packages = vec![
            ScopedPackage {
                pkg: PackageSpec {
                    source: "   ".to_string(),
                    filter: None,
                },
                scope: PackageScope::User,
            },
            ScopedPackage {
                pkg: PackageSpec {
                    source: "npm:bar@1.0".to_string(),
                    filter: None,
                },
                scope: PackageScope::Project,
            },
        ];

        let deduped = manager.dedupe_packages(packages);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].pkg.source, "npm:bar@1.0");
    }

    #[test]
    fn dedupe_packages_different_names_preserved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = PackageManager::new(dir.path().to_path_buf());

        let packages = vec![
            ScopedPackage {
                pkg: PackageSpec {
                    source: "npm:foo".to_string(),
                    filter: None,
                },
                scope: PackageScope::User,
            },
            ScopedPackage {
                pkg: PackageSpec {
                    source: "npm:bar".to_string(),
                    filter: None,
                },
                scope: PackageScope::User,
            },
        ];

        let deduped = manager.dedupe_packages(packages);
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn ensure_packages_installed_propagates_install_errors() {
        run_async(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let manager = PackageManager::new(dir.path().to_path_buf());
            let missing_local_package = dir.path().join("missing-package");

            let err = manager
                .ensure_package_entries_installed(vec![PackageEntry {
                    scope: PackageScope::Project,
                    source: missing_local_package.to_string_lossy().into_owned(),
                    filter: None,
                }])
                .await
                .expect_err("missing package install should fail");

            assert!(
                err.to_string().contains("does not exist"),
                "unexpected error: {err}"
            );
        });
    }

    // ======================================================================
    // collect_auto_prompt_entries / collect_auto_theme_entries
    // ======================================================================

    #[test]
    fn collect_auto_prompt_entries_finds_md_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prompts_dir = dir.path().join("prompts");
        fs::create_dir_all(&prompts_dir).expect("create dir");
        fs::write(prompts_dir.join("hello.md"), "# Hello").expect("write");
        fs::write(prompts_dir.join("world.md"), "# World").expect("write");
        fs::write(prompts_dir.join("notmd.txt"), "text").expect("write");
        fs::write(prompts_dir.join(".hidden.md"), "hidden").expect("write");

        let entries = collect_auto_prompt_entries(&prompts_dir);
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|p| p.extension().unwrap() == "md"));
    }

    #[test]
    fn collect_auto_prompt_entries_nonexistent_dir() {
        let entries = collect_auto_prompt_entries(Path::new("/nonexistent"));
        assert!(entries.is_empty());
    }

    #[test]
    fn collect_auto_theme_entries_finds_json_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let themes_dir = dir.path().join("themes");
        fs::create_dir_all(&themes_dir).expect("create dir");
        fs::write(themes_dir.join("dark.json"), "{}").expect("write");
        fs::write(themes_dir.join("light.json"), "{}").expect("write");
        fs::write(themes_dir.join("readme.md"), "text").expect("write");

        let entries = collect_auto_theme_entries(&themes_dir);
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|p| p.extension().unwrap() == "json"));
    }

    // ======================================================================
    // collect_auto_extension_entries
    // ======================================================================

    #[test]
    fn collect_auto_extension_entries_finds_native_descriptors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("extensions");
        fs::create_dir_all(&ext_dir).expect("create dir");
        fs::write(ext_dir.join("a.native.json"), "{}").expect("write");
        fs::write(ext_dir.join("b.native.json"), "{}").expect("write");
        fs::write(ext_dir.join("c.md"), "c").expect("write");

        let entries = collect_auto_extension_entries(&ext_dir);
        assert!(entries.len() >= 2);
        let has_a = entries
            .iter()
            .any(|p| p.file_name().unwrap() == "a.native.json");
        let has_b = entries
            .iter()
            .any(|p| p.file_name().unwrap() == "b.native.json");
        let has_md = entries.iter().any(|p| p.file_name().unwrap() == "c.md");
        assert!(has_a, "should find native descriptor files");
        assert!(has_b, "should find native descriptor files");
        assert!(!has_md, "should not find .md files");
    }

    #[test]
    fn collect_auto_extension_entries_finds_root_js_entry_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("extensions");
        fs::create_dir_all(&ext_dir).expect("create dir");
        fs::write(ext_dir.join("my_extension.ts"), "export default {}").expect("write");

        let entries = collect_auto_extension_entries(&ext_dir);
        assert!(
            entries
                .iter()
                .any(|p| p.file_name().unwrap() == "my_extension.ts")
        );
    }

    #[test]
    fn collect_auto_extension_entries_deduplicates_index_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("extensions");
        fs::create_dir_all(&ext_dir).expect("create dir");
        fs::write(ext_dir.join("index.ts"), "export default {}").expect("write");

        let entries = collect_auto_extension_entries(&ext_dir);
        let count = entries
            .iter()
            .filter(|p| p.file_name().unwrap() == "index.ts")
            .count();
        assert_eq!(count, 1, "index.ts should only be present once");
    }

    #[test]
    fn collect_auto_extension_entries_fail_closed_on_malformed_root_package_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("extensions");
        fs::create_dir_all(&ext_dir).expect("create dir");
        fs::write(ext_dir.join("package.json"), "{ not valid json")
            .expect("write malformed package.json");
        fs::write(ext_dir.join("index.ts"), "export default {}").expect("write index.ts");

        let entries = collect_auto_extension_entries(&ext_dir);
        assert!(
            entries.is_empty(),
            "malformed root package.json must not fall back to conventional entrypoints"
        );
    }

    #[test]
    fn collect_auto_extension_entries_fail_closed_on_malformed_root_extension_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("extensions");
        fs::create_dir_all(&ext_dir).expect("create dir");
        fs::write(ext_dir.join("extension.json"), "{ not valid json")
            .expect("write malformed extension.json");
        fs::write(ext_dir.join("index.ts"), "export default {}").expect("write index.ts");

        let entries = collect_auto_extension_entries(&ext_dir);
        assert!(
            entries.is_empty(),
            "malformed root extension.json must not fall back to conventional entrypoints"
        );
    }

    #[test]
    fn collect_auto_extension_entries_respects_empty_root_manifest_extensions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("extensions");
        fs::create_dir_all(&ext_dir).expect("create dir");
        fs::write(
            ext_dir.join("package.json"),
            serde_json::to_string_pretty(&json!({
                "name": "test-pkg",
                "pi": {
                    "extensions": []
                }
            }))
            .expect("serialize package.json"),
        )
        .expect("write package.json");
        fs::write(ext_dir.join("index.ts"), "export default {}").expect("write index.ts");

        let entries = collect_auto_extension_entries(&ext_dir);
        assert!(
            entries.is_empty(),
            "explicit empty root package.json#pi.extensions must disable conventional fallback"
        );
    }

    #[test]
    fn collect_auto_extension_entries_respects_missing_root_manifest_targets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("extensions");
        fs::create_dir_all(&ext_dir).expect("create dir");
        fs::write(
            ext_dir.join("package.json"),
            serde_json::to_string_pretty(&json!({
                "name": "test-pkg",
                "pi": {
                    "extensions": ["missing/index.ts"]
                }
            }))
            .expect("serialize package.json"),
        )
        .expect("write package.json");
        fs::write(ext_dir.join("index.ts"), "export default {}").expect("write index.ts");

        let entries = collect_auto_extension_entries(&ext_dir);
        assert!(
            entries.is_empty(),
            "missing root package.json#pi.extensions targets must disable conventional fallback"
        );
    }

    // ======================================================================
    // resolve_extension_entries with index.native.json / legacy JS fallback
    // ======================================================================

    #[test]
    fn resolve_extension_entries_finds_index_native_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("ext");
        fs::create_dir_all(&ext_dir).expect("create dir");
        fs::write(ext_dir.join("index.native.json"), "{}").expect("write");

        let entries = resolve_extension_entries(&ext_dir).expect("entries");
        assert_eq!(entries, vec![ext_dir.join("index.native.json")]);
    }

    #[test]
    fn resolve_extension_entries_finds_index_ts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("ext");
        fs::create_dir_all(&ext_dir).expect("create dir");
        fs::write(ext_dir.join("index.ts"), "export default {}").expect("write");

        let entries = resolve_extension_entries(&ext_dir).expect("entries");
        assert_eq!(entries, vec![ext_dir.join("index.ts")]);
    }

    #[test]
    fn resolve_extension_entries_finds_index_js() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("ext");
        fs::create_dir_all(&ext_dir).expect("create dir");
        fs::write(ext_dir.join("index.js"), "export default {}").expect("write");

        let entries = resolve_extension_entries(&ext_dir).expect("entries");
        assert_eq!(entries, vec![ext_dir.join("index.js")]);
    }

    #[test]
    fn resolve_extension_entries_prefers_manifest_over_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("ext");
        fs::create_dir_all(&ext_dir).expect("create dir");
        fs::write(
            ext_dir.join("extension.json"),
            serde_json::to_string_pretty(&json!({
                "schema": "pi.ext.manifest.v1",
                "extension_id": "test.ext",
                "name": "Test",
                "version": "0.1.0",
                "api_version": "1.0",
                "runtime": "native-rust",
                "entrypoint": "main.native.json",
                "capabilities": []
            }))
            .unwrap(),
        )
        .expect("write manifest");
        fs::write(ext_dir.join("main.native.json"), "{}").expect("write main");
        fs::write(ext_dir.join("index.ts"), "index").expect("write index");

        let entries = resolve_extension_entries(&ext_dir).expect("entries");
        // Manifest presence should return the directory itself, not index.ts
        assert_eq!(entries, vec![ext_dir]);
    }

    #[test]
    fn resolve_extension_entries_empty_manifest_extensions_fail_closed_without_index_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("ext");
        fs::create_dir_all(&ext_dir).expect("create dir");
        fs::write(
            ext_dir.join("package.json"),
            serde_json::to_string_pretty(&json!({
                "name": "test-pkg",
                "pi": {
                    "extensions": []
                }
            }))
            .expect("serialize package.json"),
        )
        .expect("write package.json");
        fs::write(ext_dir.join("index.ts"), "export default {}").expect("write index.ts");

        let entries = resolve_extension_entries(&ext_dir).expect("entries");
        assert!(
            entries.is_empty(),
            "explicit empty package.json#pi.extensions must not fall back to index.*"
        );
    }

    #[test]
    fn resolve_extension_entries_missing_manifest_targets_fail_closed_without_index_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("ext");
        fs::create_dir_all(&ext_dir).expect("create dir");
        fs::write(
            ext_dir.join("package.json"),
            serde_json::to_string_pretty(&json!({
                "name": "test-pkg",
                "pi": {
                    "extensions": ["extensions/missing.js"]
                }
            }))
            .expect("serialize package.json"),
        )
        .expect("write package.json");
        fs::write(ext_dir.join("index.ts"), "export default {}").expect("write index.ts");

        let entries = resolve_extension_entries(&ext_dir).expect("entries");
        assert!(
            entries.is_empty(),
            "missing package.json#pi.extensions targets must not fall back to index.*"
        );
    }

    #[test]
    fn resolve_extension_entries_applies_manifest_pattern_overrides() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("ext");
        let nested_dir = ext_dir.join("extensions");
        fs::create_dir_all(&nested_dir).expect("create nested dir");
        fs::write(nested_dir.join("a.native.json"), "{}").expect("write a.native.json");
        fs::write(nested_dir.join("b.native.json"), "{}").expect("write b.native.json");
        fs::write(
            ext_dir.join("package.json"),
            serde_json::to_string_pretty(&json!({
                "name": "test-pkg",
                "pi": {
                    "extensions": ["extensions", "!extensions/b.native.json"]
                }
            }))
            .expect("serialize package.json"),
        )
        .expect("write package.json");

        let entries = resolve_extension_entries(&ext_dir).expect("entries");
        assert_eq!(entries, vec![nested_dir.join("a.native.json")]);
    }

    #[test]
    fn collect_auto_extension_entries_fail_closed_on_outside_root_manifest_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("ext");
        let escaped_dir = dir.path().join("escaped");
        fs::create_dir_all(&ext_dir).expect("create ext dir");
        fs::create_dir_all(&escaped_dir).expect("create escaped dir");
        fs::write(escaped_dir.join("index.ts"), "export default {};").expect("write escaped entry");
        fs::write(ext_dir.join("index.ts"), "export default {};").expect("write fallback entry");
        fs::write(
            ext_dir.join("package.json"),
            serde_json::to_string_pretty(&json!({
                "name": "test-pkg",
                "pi": {
                    "extensions": ["../escaped/index.ts"]
                }
            }))
            .expect("serialize package.json"),
        )
        .expect("write package.json");

        let entries = collect_auto_extension_entries(&ext_dir);
        assert!(
            entries.is_empty(),
            "outside-root package.json#pi.extensions must disable conventional fallback"
        );
    }

    #[test]
    fn resolve_extension_entries_empty_dir_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("ext");
        fs::create_dir_all(&ext_dir).expect("create dir");
        assert!(resolve_extension_entries(&ext_dir).is_none());
    }

    #[test]
    fn collect_default_resources_respects_empty_manifest_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let package_root = dir.path().join("pkg");
        let skills_dir = package_root.join("skills").join("my-skill");
        fs::create_dir_all(&skills_dir).expect("create skills dir");
        fs::write(skills_dir.join("SKILL.md"), "# Skill").expect("write skill");
        fs::write(
            package_root.join("package.json"),
            serde_json::to_string_pretty(&json!({
                "name": "pkg",
                "pi": {
                    "skills": []
                }
            }))
            .expect("serialize package.json"),
        )
        .expect("write package.json");

        let mut target = ResourceList::default();
        let metadata = PathMetadata {
            source: package_root.display().to_string(),
            scope: PackageScope::Project,
            origin: ResourceOrigin::Package,
            base_dir: Some(package_root.clone()),
        };

        PackageManager::collect_default_resources(
            &package_root,
            ResourceType::Skills,
            &mut target,
            &metadata,
        )
        .expect("collect default resources");

        assert!(
            target.items.is_empty(),
            "explicit empty package.json#pi.skills must disable convention fallback"
        );
    }

    #[test]
    fn collect_manifest_files_respects_empty_manifest_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let package_root = dir.path().join("pkg");
        let extensions_dir = package_root.join("extensions");
        fs::create_dir_all(&extensions_dir).expect("create extensions dir");
        fs::write(extensions_dir.join("index.ts"), "export default {}").expect("write index.ts");
        fs::write(
            package_root.join("package.json"),
            serde_json::to_string_pretty(&json!({
                "name": "pkg",
                "pi": {
                    "extensions": []
                }
            }))
            .expect("serialize package.json"),
        )
        .expect("write package.json");

        let (all_files, enabled) =
            PackageManager::collect_manifest_files(&package_root, ResourceType::Extensions)
                .expect("collect manifest files");

        assert!(all_files.is_empty());
        assert!(enabled.is_empty());
    }

    // ======================================================================
    // collect_skill_entries
    // ======================================================================

    #[test]
    fn collect_skill_entries_finds_skill_md_in_subdirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skills_dir = dir.path().join("skills");
        fs::create_dir_all(skills_dir.join("my-skill")).expect("create dir");
        fs::write(skills_dir.join("my-skill/SKILL.md"), "# Skill").expect("write skill");
        fs::write(skills_dir.join("top-level.md"), "# Top").expect("write top");
        fs::write(skills_dir.join("readme.txt"), "text").expect("write txt");

        let entries = collect_skill_entries(&skills_dir);
        assert!(entries.iter().any(|p| p.file_name().unwrap() == "SKILL.md"));
        assert!(
            entries
                .iter()
                .any(|p| p.file_name().unwrap() == "top-level.md")
        );
        assert!(
            !entries
                .iter()
                .any(|p| p.file_name().unwrap() == "readme.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn collect_skill_entries_ignores_symlink_cycles() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("my-skill");
        fs::create_dir_all(&skill_dir).expect("create dir");
        fs::write(skill_dir.join("SKILL.md"), "# Skill").expect("write skill");

        let loop_link = skill_dir.join("loop");
        std::os::unix::fs::symlink(&skill_dir, &loop_link).expect("create symlink loop");

        let entries = collect_skill_entries(&skills_dir);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], skill_dir.join("SKILL.md"));
    }

    #[cfg(unix)]
    #[test]
    fn collect_skill_entries_dedupes_alias_symlink_to_same_skill_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skills_dir = dir.path().join("skills");
        let real_root = skills_dir.join("real");
        let skill_dir = real_root.join("my-skill");
        fs::create_dir_all(&skill_dir).expect("create dir");
        fs::write(skill_dir.join("SKILL.md"), "# Skill").expect("write skill");

        std::os::unix::fs::symlink(&real_root, skills_dir.join("alias"))
            .expect("create alias symlink");

        let entries = collect_skill_entries(&skills_dir);
        assert_eq!(entries.len(), 1);
        let canonical_entries = entries
            .iter()
            .map(|path| canonical_identity_path(path))
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(canonical_entries.len(), 1);
        assert!(canonical_entries.contains(&canonical_identity_path(&skill_dir.join("SKILL.md"))));
    }

    // ======================================================================
    // prune_empty_git_parents
    // ======================================================================

    #[test]
    fn prune_empty_git_parents_removes_empty_ancestors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("git");
        let deep = root.join("github.com/user/repo");
        fs::create_dir_all(&deep).expect("create dirs");

        // Remove the deepest dir first
        fs::remove_dir(&deep).expect("remove repo dir");

        prune_empty_git_parents(&deep, &root);

        // Both github.com/user and github.com should be pruned
        assert!(!root.join("github.com/user").exists());
        assert!(!root.join("github.com").exists());
        // Root itself should remain
        assert!(root.exists());
    }

    // ======================================================================
    // ensure_npm_project / ensure_git_ignore
    // ======================================================================

    #[test]
    fn ensure_npm_project_creates_package_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("npm");
        ensure_npm_project(&root).expect("ensure");
        assert!(root.join("package.json").exists());
        assert!(root.join(".gitignore").exists());

        let content = fs::read_to_string(root.join("package.json")).expect("read");
        let json: Value = serde_json::from_str(&content).expect("parse");
        assert_eq!(json["name"], "pi-packages");
        assert_eq!(json["private"], true);
    }

    #[test]
    fn ensure_npm_project_does_not_overwrite_existing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("npm");
        fs::create_dir_all(&root).expect("create dir");
        fs::write(root.join("package.json"), r#"{"name":"existing"}"#).expect("write");
        ensure_npm_project(&root).expect("ensure");
        let content = fs::read_to_string(root.join("package.json")).expect("read");
        assert!(content.contains("existing"), "should not overwrite");
    }

    #[test]
    fn ensure_git_ignore_creates_gitignore() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("git");
        ensure_git_ignore(&root).expect("ensure");
        let content = fs::read_to_string(root.join(".gitignore")).expect("read");
        assert!(content.contains('*'));
        assert!(content.contains("!.gitignore"));
    }

    // ======================================================================
    // PiManifest::entries_for
    // ======================================================================

    #[test]
    fn pi_manifest_entries_for_returns_cloned_vectors() {
        let manifest = PiManifest {
            extensions: Some(vec!["a.js".to_string()]),
            skills: None,
            prompts: Some(vec!["p.md".to_string()]),
            themes: None,
        };
        assert_eq!(
            manifest.entries_for(ResourceType::Extensions),
            Some(vec!["a.js".to_string()])
        );
        assert!(manifest.entries_for(ResourceType::Skills).is_none());
        assert_eq!(
            manifest.entries_for(ResourceType::Prompts),
            Some(vec!["p.md".to_string()])
        );
        assert!(manifest.entries_for(ResourceType::Themes).is_none());
    }

    // ======================================================================
    // ResourceType::all / as_str
    // ======================================================================

    #[test]
    fn resource_type_all_and_as_str() {
        let all = ResourceType::all();
        assert_eq!(all.len(), 4);
        assert_eq!(ResourceType::Extensions.as_str(), "extensions");
        assert_eq!(ResourceType::Skills.as_str(), "skills");
        assert_eq!(ResourceType::Prompts.as_str(), "prompts");
        assert_eq!(ResourceType::Themes.as_str(), "themes");
    }

    // ======================================================================
    // read_installed_npm_version
    // ======================================================================

    #[test]
    fn read_installed_npm_version_parses_package_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("package.json"),
            r#"{"name":"foo","version":"1.2.3"}"#,
        )
        .expect("write");
        let version = read_installed_npm_version(dir.path());
        assert_eq!(version, Some("1.2.3".to_string()));
    }

    #[test]
    fn read_installed_npm_version_missing_version_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("package.json"), r#"{"name":"foo"}"#).expect("write");
        assert!(read_installed_npm_version(dir.path()).is_none());
    }

    #[test]
    fn read_installed_npm_version_no_package_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_installed_npm_version(dir.path()).is_none());
    }

    // ======================================================================
    // NpmVersionConstraint / npm_local_update_decision
    // ======================================================================

    #[test]
    fn npm_version_constraint_recognizes_conventional_specs() {
        for spec in [
            "1.2.3",
            "v1.2.3",
            " 1.2.3 ",
            "1.2.3-beta.1",
            "1.2.3+build.5",
            "1",
            "1.2",
            "1.x",
            "1.X",
            "1.2.*",
            "*",
            "x",
            "^1.2.0",
            "~1.2",
            ">=2",
            ">1.2.3",
            "<=2.4.0",
            "<2",
            "=1.2.3",
        ] {
            assert!(
                NpmVersionConstraint::parse(spec).is_some(),
                "{spec} should be locally decidable"
            );
        }
    }

    #[test]
    fn npm_version_constraint_rejects_unconventional_specs() {
        for spec in [
            // Dist-tags and garbage.
            "latest",
            "beta",
            "next",
            "1.2.3alpha",
            "01.2.3",
            "01.x",
            "",
            "^",
            // Local references.
            "file:../demo",
            "link:../demo",
            "workspace:*",
            // Multi-token range language we deliberately leave to npm.
            ">=1.0.0 <2.0.0",
            "1.2.3 || >=2",
            "1.2.0 - 1.4.0",
            ">=1.0.0, <2.0.0",
            "1.x.3",
        ] {
            assert!(
                NpmVersionConstraint::parse(spec).is_none(),
                "{spec} should fall back to historical behavior"
            );
        }
    }

    #[test]
    fn npm_version_constraint_satisfaction_follows_npm_semantics() {
        for (spec, installed, expected) in [
            // Exact versions: normalized, build metadata ignored.
            ("1.2.3", "1.2.3", true),
            ("v1.2.3", "1.2.3", true),
            ("1.2.3", "v1.2.3", true),
            ("1.2.3+build.5", "1.2.3", true),
            ("1.2.3", "1.2.4", false),
            ("1.2.3-beta.1", "1.2.3-beta.1", true),
            ("1.2.3-beta.1", "1.2.3", false),
            // Caret.
            ("^1.2.0", "1.9.9", true),
            ("^1.2.0", "1.1.0", false),
            ("^1.2.0", "2.0.0", false),
            ("^0.2.3", "0.2.9", true),
            ("^0.2.3", "0.3.0", false),
            // Tilde (npm: `~1.2` pins the minor, unlike Cargo's bare `1.2`).
            ("~1.2.3", "1.2.9", true),
            ("~1.2.3", "1.3.0", false),
            ("~1.2", "1.2.9", true),
            ("~1.2", "1.3.0", false),
            // X-ranges and partial versions (npm: `1.2` means `1.2.x`).
            ("1", "1.9.9", true),
            ("1", "2.0.0", false),
            ("1.2", "1.2.9", true),
            ("1.2", "1.3.0", false),
            ("1.x", "1.9.9", true),
            ("1.x", "2.0.0", false),
            ("1.2.*", "1.2.9", true),
            ("1.2.*", "1.3.0", false),
            ("*", "0.0.1", true),
            // Comparators, including partial-version forms.
            (">=2", "2.0.0", true),
            (">=2", "1.9.9", false),
            (">1.2", "1.3.0", true),
            (">1.2", "1.2.9", false),
            ("<=1.2", "1.2.9", true),
            ("<=1.2", "1.3.0", false),
            ("=1.2.3", "1.2.3", true),
            ("=1.2.3", "1.2.4", false),
            // Pre-release installs never satisfy plain ranges.
            ("^1.2.0", "1.3.0-beta.1", false),
            // Unparseable installed versions never satisfy.
            ("^1.2.0", "not-a-version", false),
        ] {
            let constraint = NpmVersionConstraint::parse(spec)
                .unwrap_or_else(|| panic!("{spec} should parse as a local constraint"));
            assert_eq!(
                constraint.is_satisfied_by(installed),
                expected,
                "satisfaction mismatch for spec {spec} with installed {installed}"
            );
        }
    }

    #[test]
    fn npm_local_update_decision_skips_satisfied_installs_offline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let installed = dir.path().join("demo");
        fs::create_dir_all(&installed).expect("create installed package");

        let write_version = |version: &str| {
            fs::write(
                installed.join("package.json"),
                format!(r#"{{"name":"demo","version":"{version}"}}"#),
            )
            .expect("write package.json");
        };

        // Missing metadata always installs.
        assert_eq!(
            npm_local_update_decision("demo@^1.0.0", &dir.path().join("missing")),
            NpmLocalDecision::NeedsInstall
        );

        write_version("1.2.3");
        // Exact and range specs are decided locally.
        assert_eq!(
            npm_local_update_decision("demo@1.2.3", &installed),
            NpmLocalDecision::Satisfied
        );
        assert_eq!(
            npm_local_update_decision("demo@v1.2.3", &installed),
            NpmLocalDecision::Satisfied
        );
        assert_eq!(
            npm_local_update_decision("demo@1.2.4", &installed),
            NpmLocalDecision::NeedsInstall
        );
        assert_eq!(
            npm_local_update_decision("demo@^1.0.0", &installed),
            NpmLocalDecision::Satisfied
        );
        assert_eq!(
            npm_local_update_decision("demo@^2.0.0", &installed),
            NpmLocalDecision::NeedsInstall
        );
        assert_eq!(
            npm_local_update_decision("demo@1.x", &installed),
            NpmLocalDecision::Satisfied
        );
        // Bare specs keep the historical registry probe.
        assert_eq!(
            npm_local_update_decision("demo", &installed),
            NpmLocalDecision::CheckRegistry {
                installed_version: "1.2.3".to_string()
            }
        );
        // Dist-tags keep the historical string comparison (always reinstall).
        assert_eq!(
            npm_local_update_decision("demo@latest", &installed),
            NpmLocalDecision::NeedsInstall
        );
        // Local references are never reinstalled at startup.
        assert_eq!(
            npm_local_update_decision("demo@file:../demo", &installed),
            NpmLocalDecision::Satisfied
        );
        // Multi-token ranges fall back to the historical string comparison.
        assert_eq!(
            npm_local_update_decision("demo@>=1.0.0 <2.0.0", &installed),
            NpmLocalDecision::NeedsInstall
        );

        write_version("");
        assert_eq!(
            npm_local_update_decision("demo@^1.0.0", &installed),
            NpmLocalDecision::NeedsInstall
        );
    }

    #[test]
    fn npm_version_pins_source_matches_upstream_exactness() {
        for (version, expected) in [
            ("1.2.3", true),
            ("v1.2.3", true),
            ("1.2.3-rc.1", true),
            ("file:../demo", true),
            ("link:../demo", true),
            ("workspace:*", true),
            ("^1.2.3", false),
            ("~1.2", false),
            ("1.2", false),
            ("1.x", false),
            ("latest", false),
        ] {
            assert_eq!(
                npm_version_pins_source(version),
                expected,
                "pin mismatch for {version}"
            );
        }
    }

    #[test]
    fn source_needs_global_npm_root_only_for_user_scoped_npm() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = PackageManager::new(dir.path().to_path_buf());

        let scoped = |source: &str, scope: PackageScope| ScopedPackage {
            pkg: PackageSpec {
                source: source.to_string(),
                filter: None,
            },
            scope,
        };

        assert!(
            manager.source_needs_global_npm_root(&scoped("npm:demo@^1.0.0", PackageScope::User))
        );
        assert!(
            !manager
                .source_needs_global_npm_root(&scoped("npm:demo@^1.0.0", PackageScope::Project))
        );
        assert!(
            !manager
                .source_needs_global_npm_root(&scoped("npm:demo@^1.0.0", PackageScope::Temporary))
        );
        assert!(
            !manager.source_needs_global_npm_root(&scoped(
                "git:github.com/user/repo",
                PackageScope::User
            ))
        );
        assert!(!manager.source_needs_global_npm_root(&scoped("./local-pkg", PackageScope::User)));

        // No user-scoped npm sources means no `npm root -g` subprocess at all.
        let sources = vec![
            scoped("npm:demo@^1.0.0", PackageScope::Project),
            scoped("./local-pkg", PackageScope::User),
        ];
        assert_eq!(
            manager
                .pass_global_npm_root_blocking(&sources)
                .expect("no npm subprocess should be needed"),
            None
        );
    }

    #[test]
    fn resolve_with_roots_skips_install_for_satisfied_npm_ranges() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let project_root = temp_dir.path().join("project");
            fs::create_dir_all(project_root.join(".pi")).expect("create project settings dir");

            // Pre-install a project-scoped npm package satisfying its range.
            let installed = project_root
                .join(".pi")
                .join("npm")
                .join("node_modules")
                .join("pi-test-preinstalled-pkg");
            fs::create_dir_all(installed.join("extensions")).expect("create installed package");
            fs::write(
                installed.join("package.json"),
                r#"{"name":"pi-test-preinstalled-pkg","version":"1.4.2"}"#,
            )
            .expect("write package.json");
            fs::write(installed.join("extensions/a.native.json"), "{}").expect("write extension");

            let project_settings_path = project_root.join(".pi/settings.json");
            fs::write(
                &project_settings_path,
                serde_json::to_string_pretty(&json!({
                    "packages": ["npm:pi-test-preinstalled-pkg@^1.2.0"]
                }))
                .expect("serialize project settings"),
            )
            .expect("write project settings");

            let roots = ResolveRoots {
                global_settings_path: temp_dir.path().join("global-settings.json"),
                project_settings_path,
                global_base_dir: temp_dir.path().join("global-base"),
                project_base_dir: project_root.join(".pi"),
                project_settings_enabled: true,
            };
            fs::create_dir_all(&roots.global_base_dir).expect("create global base dir");

            // If resolution wrongly decided the satisfied range needed
            // install/update work, it would shell out to npm (and fail for
            // this nonexistent package name) instead of resolving locally.
            let manager = PackageManager::new(project_root);
            let resolved = manager
                .resolve_with_roots(&roots)
                .await
                .expect("satisfied installed range must resolve without npm work");

            assert!(
                resolved
                    .extensions
                    .iter()
                    .any(|entry| entry.path == installed.join("extensions/a.native.json")),
                "installed package resources should be resolved"
            );
        });
    }

    // ======================================================================
    // PackageScope / extract_package_source
    // ======================================================================

    #[test]
    fn extract_package_source_string_value() {
        let (source, is_obj) = extract_package_source(&json!("npm:foo")).unwrap();
        assert_eq!(source, "npm:foo");
        assert!(!is_obj);
    }

    #[test]
    fn extract_package_source_object_value() {
        let val = json!({"source": "git:repo"});
        let (source, is_obj) = extract_package_source(&val).unwrap();
        assert_eq!(source, "git:repo");
        assert!(is_obj);
    }

    #[test]
    fn extract_package_source_invalid_returns_none() {
        assert!(extract_package_source(&json!(42)).is_none());
        assert!(extract_package_source(&json!(null)).is_none());
    }

    // ======================================================================
    // AutoDirs
    // ======================================================================

    #[test]
    fn auto_dirs_constructs_correct_paths() {
        let base = Path::new("/home/user/.pi/agent");
        let dirs = AutoDirs::new(base);
        assert_eq!(dirs.extensions, base.join("extensions"));
        assert_eq!(dirs.skills, base.join("skills"));
        assert_eq!(dirs.prompts, base.join("prompts"));
        assert_eq!(dirs.themes, base.join("themes"));
    }

    // ======================================================================
    // get_override_patterns
    // ======================================================================

    #[test]
    fn get_override_patterns_filters_correctly() {
        let entries = vec![
            "a.js".to_string(),
            "!excluded.js".to_string(),
            "+forced.js".to_string(),
            "-removed.js".to_string(),
            "b.js".to_string(),
        ];
        let overrides = get_override_patterns(&entries);
        assert_eq!(overrides.len(), 3);
        assert!(overrides.contains(&"!excluded.js".to_string()));
        assert!(overrides.contains(&"+forced.js".to_string()));
        assert!(overrides.contains(&"-removed.js".to_string()));
    }

    // ======================================================================
    // local_path_from_spec
    // ======================================================================

    #[test]
    fn local_path_from_spec_file_url() {
        let cwd = Path::new("/home/user/project");
        let result = local_path_from_spec("file:///abs/repo", cwd);
        assert_eq!(result, PathBuf::from("/abs/repo"));
    }

    #[test]
    fn local_path_from_spec_relative() {
        let cwd = Path::new("/home/user/project");
        let result = local_path_from_spec("./my-repo", cwd);
        assert_eq!(result, PathBuf::from("/home/user/project/my-repo"));
    }

    #[test]
    fn parse_git_source_sanitizes_paths() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Case 1: Traversal in host/path — production strips "git:" prefix before
        // calling parse_git_source (see parse_source line ~2139), so pass without it.
        match parse_git_source("../../evil/repo", dir.path()) {
            ParsedSource::Git { host, .. } => {
                // "../../evil/repo" -> looks_like_local_path -> local hash path
                // After sanitization, traversal segments are filtered out.
                assert_eq!(host, "local");
            }
            other => panic!(),
        }

        // Case 2: Traversal in middle
        match parse_git_source("github.com/../../user/repo", dir.path()) {
            ParsedSource::Git { host, path, .. } => {
                // "github.com/../../user/repo" -> segments filter ".." -> "github.com/user/repo"
                assert_eq!(host, "github.com");
                assert_eq!(path, "user/repo");
            }
            other => panic!(),
        }

        // Case 3: Just dots
        match parse_git_source("..", dir.path()) {
            ParsedSource::Git { host, .. } => {
                // ".." starts with ".." -> looks_like_local_path -> local hash
                assert_eq!(host, "local");
            }
            other => panic!(),
        }

        // Case 4: SCP-like path with traversal segments must be sanitized
        let (_, host, path) = normalize_remote_git_repo("git@evil.com:../../etc/passwd");
        assert_eq!(host, "evil.com");
        assert_eq!(
            path, "etc/passwd",
            "dot-dot segments must be stripped from SCP-like paths"
        );
        assert!(
            !path.contains(".."),
            "path must not contain traversal segments"
        );

        // Case 5: SCP-like path with single traversal
        let (_, _, path) = normalize_remote_git_repo("git@github.com:../user/repo");
        assert!(!path.contains(".."));
        assert_eq!(path, "user/repo");
    }

    fn sample_npm_lock_entry(
        source: &str,
        requested_spec: &str,
        requested_version: Option<&str>,
        installed_version: &str,
        digest: &str,
        pinned: bool,
    ) -> PackageLockEntry {
        PackageLockEntry {
            identity: "npm:demo-pkg".to_string(),
            source: source.to_string(),
            source_kind: PackageSourceKind::Npm,
            resolved: PackageResolvedProvenance::Npm {
                name: "demo-pkg".to_string(),
                requested_spec: requested_spec.to_string(),
                requested_version: requested_version.map(str::to_string),
                installed_version: installed_version.to_string(),
                pinned,
            },
            digest_sha256: digest.to_string(),
            trust_state: PackageEntryTrustState::Trusted,
        }
    }

    #[test]
    fn evaluate_lock_transition_rejects_install_digest_mismatch() {
        let existing = sample_npm_lock_entry(
            "npm:demo-pkg@1.0.0",
            "demo-pkg@1.0.0",
            Some("1.0.0"),
            "1.0.0",
            "aaaaaaaa",
            true,
        );
        let candidate = sample_npm_lock_entry(
            "npm:demo-pkg@1.0.0",
            "demo-pkg@1.0.0",
            Some("1.0.0"),
            "1.0.0",
            "bbbbbbbb",
            true,
        );

        let mismatch =
            evaluate_lock_transition(Some(&existing), &candidate, PackageLockAction::Install)
                .expect_err("install should fail closed on digest mismatch");
        assert_eq!(mismatch.code, "digest_mismatch");
    }

    #[test]
    fn evaluate_lock_transition_allows_unpinned_update_changes() {
        let existing =
            sample_npm_lock_entry("npm:demo-pkg", "demo-pkg", None, "1.0.0", "aaaaaaaa", false);
        let candidate =
            sample_npm_lock_entry("npm:demo-pkg", "demo-pkg", None, "1.1.0", "bbbbbbbb", false);

        let transition =
            evaluate_lock_transition(Some(&existing), &candidate, PackageLockAction::Update)
                .expect("unpinned update should permit provenance/digest rotation");
        assert!(
            transition
                .reason_codes
                .contains(&"provenance_changed".to_string()),
            "expected provenance_changed reason code"
        );
        assert!(
            transition
                .reason_codes
                .contains(&"digest_changed".to_string()),
            "expected digest_changed reason code"
        );
    }

    #[test]
    fn digest_package_path_ignores_git_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let package_root = dir.path().join("pkg");
        fs::create_dir_all(package_root.join(".git")).expect("create .git dir");
        fs::write(package_root.join("index.js"), "export const ok = true;\n")
            .expect("write index.js");
        fs::write(package_root.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("write .git/HEAD");

        let digest_before = digest_package_path(&package_root).expect("digest before");

        fs::write(package_root.join(".git/HEAD"), "ref: refs/heads/feature\n")
            .expect("rewrite .git/HEAD");
        fs::write(
            package_root.join(".git/config"),
            "[core]\nrepositoryformatversion = 0\n",
        )
        .expect("write .git/config");

        let digest_after = digest_package_path(&package_root).expect("digest after");
        assert_eq!(
            digest_before, digest_after,
            "digest should ignore .git metadata"
        );
    }

    #[test]
    fn verify_and_record_lock_is_deterministic_for_same_inputs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();
        let pkg = cwd.join("local-pkg");
        fs::create_dir_all(&pkg).expect("create local package dir");
        fs::write(pkg.join("index.js"), "export const stable = true;\n")
            .expect("write extension file");

        let manager = PackageManager::new(cwd.clone());
        manager
            .verify_and_record_lock(
                "./local-pkg",
                PackageScope::Project,
                PackageLockAction::Install,
            )
            .expect("first lock verification");

        let lockfile_path = cwd.join(".pi").join("packages.lock.json");
        let first = fs::read_to_string(&lockfile_path).expect("read first lockfile");

        manager
            .verify_and_record_lock(
                "./local-pkg",
                PackageScope::Project,
                PackageLockAction::Install,
            )
            .expect("second lock verification");
        let second = fs::read_to_string(&lockfile_path).expect("read second lockfile");

        assert_eq!(
            first, second,
            "same inputs should produce identical lockfile artifacts"
        );
    }

    #[test]
    fn resolved_pinned_flag_only_delta_is_tolerated() {
        // A pre-change lock entry recorded `pinned: true` for a range spec;
        // the reclassification alone must not fail Install verification
        // (it would hard-block startup until manual remove/install).
        let existing = PackageResolvedProvenance::Npm {
            name: "pkg".to_string(),
            requested_spec: "npm:pkg@^1.2.0".to_string(),
            requested_version: Some("^1.2.0".to_string()),
            installed_version: "1.2.5".to_string(),
            pinned: true,
        };
        let candidate = PackageResolvedProvenance::Npm {
            name: "pkg".to_string(),
            requested_spec: "npm:pkg@^1.2.0".to_string(),
            requested_version: Some("^1.2.0".to_string()),
            installed_version: "1.2.5".to_string(),
            pinned: false,
        };
        assert!(resolved_differs_only_in_pinned(&existing, &candidate));

        // Any substantive delta (here: installed version) still mismatches.
        let drifted = PackageResolvedProvenance::Npm {
            name: "pkg".to_string(),
            requested_spec: "npm:pkg@^1.2.0".to_string(),
            requested_version: Some("^1.2.0".to_string()),
            installed_version: "1.3.0".to_string(),
            pinned: false,
        };
        assert!(!resolved_differs_only_in_pinned(&existing, &drifted));

        // Cross-kind comparison never matches.
        let local = PackageResolvedProvenance::Local {
            resolved_path: "/tmp/x".to_string(),
        };
        assert!(!resolved_differs_only_in_pinned(&existing, &local));
    }

    #[test]
    fn reconcile_lockfile_prunes_entries_missing_from_settings() {
        // #49: a lockfile entry whose source was removed from settings.json
        // should be pruned on reconcile, and the prune should be recorded
        // in the trust-audit log with reason_codes=["reconciled"].
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();

        // Two local packages. Both get locked; then we drop pkg2 from
        // settings to simulate the user removing it.
        for name in ["pkg1", "pkg2"] {
            let pkg = cwd.join(name);
            fs::create_dir_all(&pkg).expect("create pkg dir");
            fs::write(pkg.join("index.js"), "export const ok = true;\n").expect("write entry");
        }

        let settings_path = cwd.join(".pi").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).expect("mkdir .pi");
        fs::write(
            &settings_path,
            json!({ "packages": ["./pkg1", "./pkg2"] }).to_string(),
        )
        .expect("seed settings");

        let manager = PackageManager::new(cwd.clone());
        manager
            .verify_and_record_lock("./pkg1", PackageScope::Project, PackageLockAction::Install)
            .expect("lock pkg1");
        manager
            .verify_and_record_lock("./pkg2", PackageScope::Project, PackageLockAction::Install)
            .expect("lock pkg2");

        let lockfile_path = cwd.join(".pi").join("packages.lock.json");
        let before = read_package_lockfile(&lockfile_path).expect("read pre-reconcile lockfile");
        assert_eq!(
            before.entries.len(),
            2,
            "both pkgs should be locked initially"
        );

        // Drop pkg2 from settings — this is the scenario where
        // `remove_lock_entry` was never called because the user edited
        // settings.json directly or via another tool.
        fs::write(
            &settings_path,
            json!({ "packages": ["./pkg1"] }).to_string(),
        )
        .expect("rewrite settings");

        let pruned = manager
            .reconcile_lockfile_sync(PackageScope::Project)
            .expect("reconcile");

        assert_eq!(pruned.len(), 1, "exactly one entry should be pruned");
        assert_eq!(
            pruned[0].source, "./pkg2",
            "pruned entry must be pkg2 (the one removed from settings), not pkg1"
        );
        assert!(
            pruned[0].identity.ends_with("pkg2"),
            "pruned identity should be the pkg2 absolute path, got {:?}",
            pruned[0].identity
        );

        let after = read_package_lockfile(&lockfile_path).expect("read post-reconcile lockfile");
        assert_eq!(
            after.entries.len(),
            1,
            "lockfile should have one remaining entry"
        );
        assert!(
            after.entries.iter().any(|e| e.source == "./pkg1"),
            "pkg1 should remain after reconcile"
        );

        // Re-running reconcile is a no-op: nothing to prune.
        let again = manager
            .reconcile_lockfile_sync(PackageScope::Project)
            .expect("reconcile idempotent");
        assert!(again.is_empty(), "second reconcile should prune nothing");

        // Audit event should record the reconciled prune.
        let audit_path = cwd.join(".pi").join("package-trust-audit.jsonl");
        let audit = fs::read_to_string(&audit_path).expect("read audit log");
        assert!(
            audit.lines().any(|line| line.contains("\"reconciled\"")),
            "trust-audit log should contain a reconciled remove event, got:\n{audit}"
        );
    }

    #[test]
    fn reconcile_lockfile_noop_when_lockfile_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = PackageManager::new(dir.path().to_path_buf());
        let pruned = manager
            .reconcile_lockfile_sync(PackageScope::Project)
            .expect("reconcile should not error on missing lockfile");
        assert!(pruned.is_empty());
    }

    #[test]
    fn reconcile_lockfile_preserves_entries_when_settings_file_is_missing() {
        // Regression: read_settings_json returns Ok({}) for a missing file,
        // which would otherwise make reconcile interpret "no settings file"
        // as "zero live packages" and wipe the lockfile. A missing settings
        // file must be treated as "no authoritative info" — preserve the
        // existing lockfile rather than pruning it.
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();

        // Create a local package and lock it while settings.json exists,
        // so we have a real non-trivial lockfile to protect.
        let pkg = cwd.join("keep-me");
        fs::create_dir_all(&pkg).expect("mkdir keep-me");
        fs::write(pkg.join("index.js"), "export const ok = true;\n").expect("write entry");

        let settings_path = cwd.join(".pi").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).expect("mkdir .pi");
        fs::write(
            &settings_path,
            json!({ "packages": ["./keep-me"] }).to_string(),
        )
        .expect("seed settings");

        let manager = PackageManager::new(cwd.clone());
        manager
            .verify_and_record_lock(
                "./keep-me",
                PackageScope::Project,
                PackageLockAction::Install,
            )
            .expect("lock keep-me");

        let lockfile_path = cwd.join(".pi").join("packages.lock.json");
        let before_len = read_package_lockfile(&lockfile_path)
            .expect("read lockfile")
            .entries
            .len();
        assert_eq!(before_len, 1, "precondition: one locked entry");

        // Simulate the "settings file went missing" case (manual edit,
        // accidental delete, fresh clone without settings yet) and
        // reconcile.
        fs::remove_file(&settings_path).expect("delete settings");

        let pruned = manager
            .reconcile_lockfile_sync(PackageScope::Project)
            .expect("reconcile should not error on missing settings file");
        assert!(
            pruned.is_empty(),
            "reconcile must NOT prune entries when settings.json is missing — we have no authoritative live set to compare against"
        );

        let after_len = read_package_lockfile(&lockfile_path)
            .expect("read lockfile post-reconcile")
            .entries
            .len();
        assert_eq!(
            after_len, before_len,
            "lockfile must be preserved when settings file is missing"
        );
    }

    mod proptest_package_manager {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// `parse_npm_spec` never panics on arbitrary input.
            #[test]
            fn parse_npm_spec_never_panics(s in ".*") {
                let _ = parse_npm_spec(&s);
            }

            /// Unscoped name without '@' has no version.
            #[test]
            fn unscoped_no_at_returns_no_version(name in "[a-z][a-z0-9-]{0,20}") {
                let (parsed_name, version) = parse_npm_spec(&name);
                assert_eq!(parsed_name, name);
                assert!(version.is_none());
            }

            /// Scoped package without version has no version.
            #[test]
            fn scoped_no_version(scope in "[a-z]{1,10}", pkg in "[a-z]{1,10}") {
                let input = format!("@{scope}/{pkg}");
                let (parsed_name, version) = parse_npm_spec(&input);
                assert_eq!(parsed_name, input);
                assert!(version.is_none());
            }

            /// Scoped package with version is parsed correctly.
            #[test]
            fn scoped_with_version(
                scope in "[a-z]{1,10}",
                pkg in "[a-z]{1,10}",
                ver in "[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}",
            ) {
                let input = format!("@{scope}/{pkg}@{ver}");
                let (parsed_name, version) = parse_npm_spec(&input);
                assert_eq!(parsed_name, format!("@{scope}/{pkg}"));
                assert_eq!(version, Some(ver));
            }

            /// `is_pattern` detects all pattern prefixes.
            #[test]
            fn is_pattern_detects_prefix(
                prefix_idx in 0..3usize,
                suffix in "[a-z.]{1,20}",
            ) {
                let prefix = ["!", "+", "-"][prefix_idx];
                let input = format!("{prefix}{suffix}");
                assert!(is_pattern(&input));
            }

            /// `is_pattern` detects wildcard characters.
            #[test]
            fn is_pattern_detects_wildcards(
                prefix in "[a-z]{0,5}",
                wild_idx in 0..2usize,
                suffix in "[a-z]{0,5}",
            ) {
                let wild = ["*", "?"][wild_idx];
                let input = format!("{prefix}{wild}{suffix}");
                assert!(is_pattern(&input));
            }

            /// Plain alphanumeric strings are not patterns.
            #[test]
            fn plain_strings_not_patterns(s in "[a-z0-9]{1,20}") {
                assert!(!is_pattern(&s));
            }

            /// `split_patterns` partitions correctly.
            #[test]
            fn split_patterns_partition_is_complete(
                plains in prop::collection::vec("[a-z]{1,10}", 0..5),
                patterns in prop::collection::vec("[!+*][a-z]{1,10}", 0..5),
            ) {
                let mut all = plains;
                all.extend(patterns);
                let (split_plain, split_patterns) = split_patterns(&all);
                assert_eq!(
                    split_plain.len() + split_patterns.len(),
                    all.len(),
                    "partition should be complete"
                );
                for p in &split_patterns {
                    assert!(is_pattern(p));
                }
            }

            /// `hex_encode` output length is always 2x input length.
            #[test]
            fn hex_encode_output_length(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
                let hex = hex_encode(&bytes);
                assert_eq!(hex.len(), bytes.len() * 2);
            }

            /// `hex_encode` output is lowercase hex.
            #[test]
            fn hex_encode_is_lowercase_hex(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
                let hex = hex_encode(&bytes);
                assert!(hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
            }

            /// `posix_string` output never contains backslashes.
            #[test]
            fn posix_string_no_backslashes(segments in prop::collection::vec("[a-z]{1,5}", 1..5)) {
                let path = PathBuf::from(segments.join("/"));
                let result = posix_string(&path);
                assert!(!result.contains('\\'));
            }

            /// `posix_string` is idempotent on already-posix paths.
            #[test]
            fn posix_string_idempotent(segments in prop::collection::vec("[a-z]{1,5}", 1..5)) {
                let path = PathBuf::from(segments.join("/"));
                let first = posix_string(&path);
                let second = posix_string(&PathBuf::from(&first));
                assert_eq!(first, second);
            }

            /// `normalize_exact_pattern` strips leading "./" only.
            #[test]
            fn normalize_exact_pattern_strips_dot_slash(suffix in "[a-z]{1,15}") {
                let with_prefix = format!("./{suffix}");
                assert_eq!(normalize_exact_pattern(&with_prefix), suffix.as_str());
            }

            /// `normalize_exact_pattern` strips at most one leading "./".
            #[test]
            fn normalize_exact_pattern_strips_at_most_one(suffix in "[a-z]{1,15}") {
                let input = format!("./{suffix}");
                let result = normalize_exact_pattern(&input);
                assert_eq!(result, suffix.as_str());
                // Without prefix, function is identity
                assert_eq!(normalize_exact_pattern(&suffix), suffix.as_str());
            }

            /// `looks_like_git_url` detects known git hosts.
            #[test]
            fn looks_like_git_url_known_hosts(
                host_idx in 0..4usize,
                path in "[a-z]{1,10}/[a-z]{1,10}",
            ) {
                let host = ["github.com", "gitlab.com", "bitbucket.org", "codeberg.org"][host_idx];
                let url = format!("{host}/{path}");
                assert!(looks_like_git_url(&url));
            }

            /// `looks_like_git_url` rejects plain names.
            #[test]
            fn looks_like_git_url_rejects_plain(name in "[a-z]{1,15}") {
                assert!(!looks_like_git_url(&name));
            }

            /// `looks_like_local_path` detects relative and tilde paths.
            #[test]
            fn looks_like_local_path_detects_relative(suffix in "[a-z]{1,10}") {
                assert!(looks_like_local_path(&format!("./{suffix}")));
                assert!(looks_like_local_path(&format!("../{suffix}")));
                assert!(looks_like_local_path(&format!("~/{suffix}")));
            }

            /// `normalize_dot_segments` is idempotent.
            #[test]
            fn normalize_dot_segments_idempotent(
                segments in prop::collection::vec("[a-z]{1,5}", 1..5),
            ) {
                let path = PathBuf::from(segments.join("/"));
                let first = normalize_dot_segments(&path);
                let second = normalize_dot_segments(&first);
                assert_eq!(first, second);
            }

            /// `normalize_source` returns None for empty/whitespace.
            #[test]
            fn normalize_source_empty_returns_none(s in "[ \\t]{0,10}") {
                assert!(normalize_source(&s).is_none());
            }

            /// `sources_match` is reflexive.
            #[test]
            fn sources_match_reflexive(source in "[a-z]{1,15}") {
                assert!(sources_match(&source, &source));
            }

            /// `sources_match` is symmetric.
            #[test]
            fn sources_match_symmetric(a in "[a-z]{1,10}", b in "[a-z]{1,10}") {
                assert_eq!(sources_match(&a, &b), sources_match(&b, &a));
            }

            /// `sort_lock_entries` produces sorted output.
            #[test]
            fn sort_lock_entries_produces_sorted(
                identities in prop::collection::vec("[a-z]{1,10}", 1..10),
            ) {
                let mut entries: Vec<PackageLockEntry> = identities
                    .iter()
                    .map(|id| PackageLockEntry {
                        identity: id.clone(),
                        source: format!("npm:{id}"),
                        source_kind: PackageSourceKind::Npm,
                        resolved: PackageResolvedProvenance::Npm {
                            name: id.clone(),
                            requested_spec: id.clone(),
                            requested_version: None,
                            installed_version: "1.0.0".to_string(),
                            pinned: false,
                        },
                        digest_sha256: "abcd".to_string(),
                        trust_state: PackageEntryTrustState::Trusted,
                    })
                    .collect();
                sort_lock_entries(&mut entries);
                for pair in entries.windows(2) {
                    assert!(pair[0].identity <= pair[1].identity);
                }
            }
        }
    }

    #[test]
    fn verify_and_record_lock_fails_closed_on_local_digest_mismatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();
        let pkg = cwd.join("local-pkg");
        fs::create_dir_all(&pkg).expect("create local package dir");
        let file_path = pkg.join("index.js");
        fs::write(&file_path, "export const version = 1;\n").expect("write extension file");

        let manager = PackageManager::new(cwd);
        manager
            .verify_and_record_lock(
                "./local-pkg",
                PackageScope::Project,
                PackageLockAction::Install,
            )
            .expect("initial lock verification");

        fs::write(&file_path, "export const version = 2;\n").expect("tamper package file");

        let err = manager
            .verify_and_record_lock(
                "./local-pkg",
                PackageScope::Project,
                PackageLockAction::Install,
            )
            .expect_err("install verification should fail on digest mismatch");
        let message = err.to_string();
        assert!(
            message.contains("digest_mismatch"),
            "expected digest_mismatch error, got: {message}"
        );
    }
}
