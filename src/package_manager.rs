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
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use tracing::{info, warn};

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
}

impl ResolveRoots {
    /// Build roots using the default Pi settings locations (env + cwd).
    #[must_use]
    pub fn from_env(cwd: &Path) -> Self {
        Self {
            global_settings_path: global_settings_path(),
            project_settings_path: project_settings_path(cwd),
            global_base_dir: Config::global_dir(),
            project_base_dir: cwd.join(Config::project_dir()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PackageManager {
    cwd: PathBuf,
}

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
        let (tx, rx) = oneshot::channel();

        thread::spawn(move || {
            let res = this.install_sync(&source, scope);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        rx.recv(cx.cx())
            .await
            .map_err(|_| Error::tool("package_manager", "Install task cancelled"))?
    }

    fn install_sync(&self, source: &str, scope: PackageScope) -> Result<()> {
        let parsed = parse_source(source, &self.cwd);
        match parsed {
            ParsedSource::Npm { spec, .. } => self.install_npm(&spec, scope),
            ParsedSource::Git {
                repo,
                host,
                path,
                r#ref,
                ..
            } => self.install_git(&repo, &host, &path, r#ref.as_deref(), scope),
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
        }
    }

    pub async fn remove(&self, source: &str, scope: PackageScope) -> Result<()> {
        let this = self.clone();
        let source = source.to_string();
        let (tx, rx) = oneshot::channel();

        thread::spawn(move || {
            let res = this.remove_sync(&source, scope);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        rx.recv(cx.cx())
            .await
            .map_err(|_| Error::tool("package_manager", "Remove task cancelled"))?
    }

    fn remove_sync(&self, source: &str, scope: PackageScope) -> Result<()> {
        let parsed = parse_source(source, &self.cwd);
        match parsed {
            ParsedSource::Npm { name, .. } => self.uninstall_npm(&name, scope),
            ParsedSource::Git { host, path, .. } => self.remove_git(&host, &path, scope),
            ParsedSource::Local { .. } => Ok(()),
        }
    }

    pub async fn update_source(&self, source: &str, scope: PackageScope) -> Result<()> {
        let this = self.clone();
        let source = source.to_string();
        let (tx, rx) = oneshot::channel();

        thread::spawn(move || {
            let res = this.update_source_sync(&source, scope);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        rx.recv(cx.cx())
            .await
            .map_err(|_| Error::tool("package_manager", "Update task cancelled"))?
    }

    fn update_source_sync(&self, source: &str, scope: PackageScope) -> Result<()> {
        let parsed = parse_source(source, &self.cwd);
        match parsed {
            ParsedSource::Npm { spec, pinned, .. } => {
                if pinned {
                    return Ok(());
                }
                self.install_npm(&spec, scope)
            }
            ParsedSource::Git {
                repo,
                host,
                path,
                pinned,
                ..
            } => {
                if pinned {
                    return Ok(());
                }
                self.update_git(&repo, &host, &path, scope)
            }
            ParsedSource::Local { .. } => Ok(()),
        }
    }

    pub async fn installed_path(
        &self,
        source: &str,
        scope: PackageScope,
    ) -> Result<Option<PathBuf>> {
        let this = self.clone();
        let source = source.to_string();
        let (tx, rx) = oneshot::channel();

        thread::spawn(move || {
            let res = this.installed_path_sync(&source, scope);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        rx.recv(cx.cx())
            .await
            .map_err(|_| Error::tool("package_manager", "Installed path lookup cancelled"))?
    }

    fn installed_path_sync(&self, source: &str, scope: PackageScope) -> Result<Option<PathBuf>> {
        let parsed = parse_source(source, &self.cwd);
        Ok(match parsed {
            ParsedSource::Npm { name, .. } => self.npm_install_path(&name, scope)?,
            ParsedSource::Git { host, path, .. } => {
                Some(self.git_install_path(&host, &path, scope))
            }
            ParsedSource::Local { path } => Some(path),
        })
    }

    pub async fn list_packages(&self) -> Result<Vec<PackageEntry>> {
        let this = self.clone();
        let (tx, rx) = oneshot::channel();

        thread::spawn(move || {
            let res = this.list_packages_sync();
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        rx.recv(cx.cx())
            .await
            .map_err(|_| Error::tool("package_manager", "List packages task cancelled"))?
    }

    fn list_packages_sync(&self) -> Result<Vec<PackageEntry>> {
        let global = list_packages_in_settings(&global_settings_path())?
            .into_iter()
            .map(|mut p| {
                p.scope = PackageScope::User;
                p
            });
        let project = list_packages_in_settings(&project_settings_path(&self.cwd))?
            .into_iter()
            .map(|mut p| {
                p.scope = PackageScope::Project;
                p
            });
        Ok(global.chain(project).collect())
    }

    /// Ensure all packages in settings are installed.
    /// Returns the list of packages that were newly installed.
    pub async fn ensure_packages_installed(&self) -> Result<Vec<PackageEntry>> {
        // This method combines multiple async calls, so we don't need to wrap it in spawn
        // assuming list_packages and install are properly offloaded.
        // However, iterating and installing sequentially might be slow.
        // For now, simple sequential await is fine.

        let packages = self.list_packages().await?;
        let mut installed = Vec::new();

        for entry in packages {
            // Check if already installed
            if let Ok(Some(path)) = self.installed_path(&entry.source, entry.scope).await {
                if path.exists() {
                    continue;
                }
            }

            // Install the package
            if self.install(&entry.source, entry.scope).await.is_ok() {
                installed.push(entry);
            }
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
        let (tx, rx) = oneshot::channel();

        // Offload the heavy lifting (sync I/O) to a thread
        thread::spawn(move || {
            let res: Result<(SettingsSnapshot, SettingsSnapshot, Vec<ScopedPackage>)> = (|| {
                let global = read_settings_snapshot(&roots_for_setup.global_settings_path)?;
                let project = read_settings_snapshot(&roots_for_setup.project_settings_path)?;

                // 1) Package resources (global + project, deduped; project wins)
                let mut all_packages: Vec<ScopedPackage> = Vec::new();
                all_packages.extend(global.packages.iter().cloned().map(|pkg| ScopedPackage {
                    pkg,
                    scope: PackageScope::User,
                }));
                all_packages.extend(project.packages.iter().cloned().map(|pkg| ScopedPackage {
                    pkg,
                    scope: PackageScope::Project,
                }));
                let package_sources = this_for_setup.dedupe_packages(all_packages);
                Ok((global, project, package_sources))
            })(
            );
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        let (global, project, package_sources) = rx
            .recv(cx.cx())
            .await
            .map_err(|_| Error::tool("package_manager", "Resolve setup task cancelled"))??;

        let mut accumulator = ResourceAccumulator::new();

        // This part is async (network calls for NPM)
        Box::pin(self.resolve_package_sources(&package_sources, &mut accumulator)).await?;

        // Offload the rest of sync resolution
        let this = self.clone();
        let roots = roots.clone();
        let (tx, rx) = oneshot::channel();
        let accumulator = std::sync::Mutex::new(accumulator);

        thread::spawn(move || {
            let mut accumulator = accumulator.lock().unwrap();

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

            // 3) Auto-discovered resources from standard dirs (global and project)
            this.add_auto_discovered_resources(
                &mut accumulator,
                &global,
                &project,
                &roots.global_base_dir,
                &roots.project_base_dir,
            );

            let resolved = accumulator.clone().into_resolved_paths();
            drop(accumulator);
            maybe_emit_compat_ledgers(&resolved.extensions);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), Ok(resolved));
        });

        let cx = AgentCx::for_request();
        rx.recv(cx.cx())
            .await
            .map_err(|_| Error::tool("package_manager", "Resolve processing task cancelled"))?
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
            .map(|source| ScopedPackage {
                pkg: PackageSpec {
                    source: source.clone(),
                    filter: None,
                },
                scope,
            })
            .collect::<Vec<_>>();

        Box::pin(self.resolve_package_sources(&package_sources, &mut accumulator)).await?;

        let (tx, rx) = oneshot::channel();
        let accumulator = std::sync::Mutex::new(accumulator);

        thread::spawn(move || {
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
        rx.recv(cx.cx())
            .await
            .map_err(|_| Error::tool("package_manager", "Resolve extensions task cancelled"))?
    }

    pub async fn add_package_source(&self, source: &str, scope: PackageScope) -> Result<()> {
        let this = self.clone();
        let source = source.to_string();
        let (tx, rx) = oneshot::channel();

        thread::spawn(move || {
            let res = this.add_package_source_sync(&source, scope);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        rx.recv(cx.cx())
            .await
            .map_err(|_| Error::tool("package_manager", "Add source task cancelled"))?
    }

    fn add_package_source_sync(&self, source: &str, scope: PackageScope) -> Result<()> {
        let path = match scope {
            PackageScope::User => global_settings_path(),
            PackageScope::Project => project_settings_path(&self.cwd),
            PackageScope::Temporary => {
                return Err(Error::config(
                    "Temporary packages cannot be persisted to settings".to_string(),
                ));
            }
        };
        update_package_sources(&path, source, UpdateAction::Add)
    }

    pub async fn remove_package_source(&self, source: &str, scope: PackageScope) -> Result<()> {
        let this = self.clone();
        let source = source.to_string();
        let (tx, rx) = oneshot::channel();

        thread::spawn(move || {
            let res = this.remove_package_source_sync(&source, scope);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        rx.recv(cx.cx())
            .await
            .map_err(|_| Error::tool("package_manager", "Remove source task cancelled"))?
    }

    fn remove_package_source_sync(&self, source: &str, scope: PackageScope) -> Result<()> {
        let path = match scope {
            PackageScope::User => global_settings_path(),
            PackageScope::Project => project_settings_path(&self.cwd),
            PackageScope::Temporary => {
                return Err(Error::config(
                    "Temporary packages cannot be persisted to settings".to_string(),
                ));
            }
        };
        update_package_sources(&path, source, UpdateAction::Remove)
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

    fn npm_install_path(&self, name: &str, scope: PackageScope) -> Result<Option<PathBuf>> {
        Ok(match scope {
            PackageScope::Temporary => {
                Some(temporary_dir("npm", None).join("node_modules").join(name))
            }
            PackageScope::Project => Some(self.project_npm_root().join("node_modules").join(name)),
            PackageScope::User => Some(self.global_npm_root()?.join(name)),
        })
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

    fn install_npm(&self, spec: &str, scope: PackageScope) -> Result<()> {
        let (name, _) = parse_npm_spec(spec);
        match scope {
            PackageScope::User => run_command("npm", ["install", "-g", spec], None)?,
            PackageScope::Project | PackageScope::Temporary => {
                let install_root = match scope {
                    PackageScope::Project => self.project_npm_root(),
                    PackageScope::Temporary => temporary_dir("npm", None),
                    PackageScope::User => unreachable!("handled above"),
                };
                ensure_npm_project(&install_root)?;
                run_command(
                    "npm",
                    [
                        "install",
                        spec,
                        "--prefix",
                        install_root.to_string_lossy().as_ref(),
                    ],
                    None,
                )?;
            }
        }

        // Basic sanity: installed path exists
        if let Some(installed) = self.npm_install_path(&name, scope)? {
            if !installed.exists() {
                return Err(Error::tool(
                    "npm",
                    format!(
                        "npm install succeeded but '{}' is missing",
                        installed.display()
                    ),
                ));
            }
        }

        Ok(())
    }

    fn uninstall_npm(&self, name: &str, scope: PackageScope) -> Result<()> {
        if scope == PackageScope::User {
            run_command("npm", ["uninstall", "-g", name], None)?;
            return Ok(());
        }

        let install_root = match scope {
            PackageScope::Project => self.project_npm_root(),
            PackageScope::Temporary => temporary_dir("npm", None),
            PackageScope::User => unreachable!("handled above"),
        };
        if !install_root.exists() {
            return Ok(());
        }
        run_command(
            "npm",
            [
                "uninstall",
                name,
                "--prefix",
                install_root.to_string_lossy().as_ref(),
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
        let target_dir = self.git_install_path(host, repo_path, scope);
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
            ["clone", &clone_url, target_dir.to_string_lossy().as_ref()],
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

        let target_dir = self.git_install_path(host, repo_path, scope);
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
        let target_dir = self.git_install_path(host, repo_path, scope);
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

#[derive(Debug, Clone)]
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
        return Some(PackageSpec {
            source: s.to_string(),
            filter: None,
        });
    }

    let obj = value.as_object()?;
    let source = obj.get("source")?.as_str()?.to_string();

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
            let identity = self.package_identity(&entry.pkg.source);
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
                    );
                }
                ParsedSource::Npm { spec, name, pinned } => {
                    // Offload installed_path check
                    let installed_path = self
                        .installed_path(&format!("npm:{name}"), entry.scope)
                        .await?
                        .unwrap_or_else(|| self.cwd.join("node_modules").join(&name));

                    let needs_install = !installed_path.exists()
                        || Box::pin(self.npm_needs_update(&spec, pinned, &installed_path)).await;
                    if needs_install {
                        self.install(source_str, entry.scope).await?;
                    }

                    metadata.base_dir = Some(installed_path.clone());
                    Self::collect_package_resources(
                        &installed_path,
                        accumulator,
                        entry.pkg.filter.as_ref(),
                        &metadata,
                    );
                }
                ParsedSource::Git {
                    repo: _,
                    host,
                    path,
                    r#ref: _,
                    ..
                } => {
                    // Offload git_install_path
                    let installed_path = self.git_install_path(&host, &path, entry.scope);

                    if !installed_path.exists() {
                        self.install(source_str, entry.scope).await?;
                    }

                    metadata.base_dir = Some(installed_path.clone());
                    Self::collect_package_resources(
                        &installed_path,
                        accumulator,
                        entry.pkg.filter.as_ref(),
                        &metadata,
                    );
                }
            }
        }

        Ok(())
    }

    async fn npm_needs_update(&self, spec: &str, pinned: bool, installed_path: &Path) -> bool {
        let installed_version = read_installed_npm_version(installed_path);
        let Some(installed_version) = installed_version else {
            return true;
        };

        let (_, pinned_version) = parse_npm_spec(spec);
        if pinned {
            return pinned_version.is_some_and(|pv| pv != installed_version);
        }

        Box::pin(get_latest_npm_version(installed_path, spec))
            .await
            .is_ok_and(|latest| latest != installed_version)
    }

    fn resolve_local_extension_source(
        resolved: &Path,
        accumulator: &mut ResourceAccumulator,
        filter: Option<&PackageFilter>,
        metadata: &mut PathMetadata,
    ) {
        if !resolved.exists() {
            return;
        }

        let Ok(stats) = fs::metadata(resolved) else {
            return;
        };

        if stats.is_file() {
            metadata.base_dir = resolved.parent().map(Path::to_path_buf);
            accumulator
                .extensions
                .add(resolved.to_path_buf(), metadata, true);
            return;
        }

        if !stats.is_dir() {
            return;
        }

        metadata.base_dir = Some(resolved.to_path_buf());
        let had_any = Self::collect_package_resources(resolved, accumulator, filter, metadata);
        if !had_any {
            accumulator
                .extensions
                .add(resolved.to_path_buf(), metadata, true);
        }
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
                let enabled = is_enabled_by_overrides(&path, project_overrides, project_base_dir);
                target.add(path, &project_metadata, enabled);
            }
        }
    }

    fn collect_package_resources(
        package_root: &Path,
        accumulator: &mut ResourceAccumulator,
        filter: Option<&PackageFilter>,
        metadata: &PathMetadata,
    ) -> bool {
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
                    );
                } else {
                    Self::collect_default_resources(package_root, resource_type, target, metadata);
                }
            }
            return true;
        }

        if let Some(manifest) = read_pi_manifest(package_root) {
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
            return true;
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

        has_any_dir
    }

    fn collect_default_resources(
        package_root: &Path,
        resource_type: ResourceType,
        target: &mut ResourceList,
        metadata: &PathMetadata,
    ) {
        if let Some(manifest) = read_pi_manifest(package_root) {
            let entries = manifest.entries_for(resource_type);
            if entries.as_ref().is_some_and(|e| !e.is_empty()) {
                Self::add_manifest_entries(
                    entries.as_deref(),
                    package_root,
                    resource_type,
                    target,
                    metadata,
                );
                return;
            }
        }

        let dir = package_root.join(resource_type.as_str());
        if dir.exists() {
            let files = collect_resource_files(&dir, resource_type);
            for f in files {
                target.add(f, metadata, true);
            }
        }
    }

    fn apply_package_filter(
        package_root: &Path,
        user_patterns: &[String],
        resource_type: ResourceType,
        target: &mut ResourceList,
        metadata: &PathMetadata,
    ) {
        let (all_files, _) = Self::collect_manifest_files(package_root, resource_type);

        if user_patterns.is_empty() {
            for f in all_files {
                target.add(f, metadata, false);
            }
            return;
        }

        let enabled_by_user = apply_patterns(&all_files, user_patterns, package_root);
        for f in all_files {
            let enabled = enabled_by_user.contains(&f);
            target.add(f, metadata, enabled);
        }
    }

    fn collect_manifest_files(
        package_root: &Path,
        resource_type: ResourceType,
    ) -> (Vec<PathBuf>, std::collections::HashSet<PathBuf>) {
        if let Some(manifest) = read_pi_manifest(package_root) {
            let entries = manifest.entries_for(resource_type);
            if let Some(entries) = entries {
                if !entries.is_empty() {
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
                    return (enabled_vec, enabled_by_manifest);
                }
            }
        }

        let convention_dir = package_root.join(resource_type.as_str());
        if !convention_dir.exists() {
            return (Vec::new(), std::collections::HashSet::new());
        }
        let all_files = collect_resource_files(&convention_dir, resource_type);
        let set = all_files.iter().cloned().collect();
        (all_files, set)
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

fn read_pi_manifest(package_root: &Path) -> Option<PiManifest> {
    let package_json = package_root.join("package.json");
    if !package_json.exists() {
        return None;
    }
    let raw = fs::read_to_string(package_json).ok()?;
    let json: Value = serde_json::from_str(&raw).ok()?;
    let pi = json.get("pi")?;
    let obj = pi.as_object()?;

    Some(PiManifest {
        extensions: obj.get("extensions").and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        }),
        skills: obj.get("skills").and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        }),
        prompts: obj.get("prompts").and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        }),
        themes: obj.get("themes").and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        }),
    })
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
    glob::Pattern::new(&normalized_pattern)
        .ok()
        .is_some_and(|p| p.matches(&candidate))
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

fn collect_skill_entries(dir: &Path) -> Vec<PathBuf> {
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
        if !path.is_file() {
            continue;
        }
        let rel = path.strip_prefix(dir).unwrap_or(path);
        let depth = rel.components().count();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        if depth == 1 {
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                out.push(path.to_path_buf());
            }
        } else if name == "SKILL.md" {
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

fn resolve_extension_entries(dir: &Path) -> Option<Vec<PathBuf>> {
    match load_extension_manifest(dir) {
        Ok(Some(_)) => {
            return Some(vec![dir.to_path_buf()]);
        }
        Ok(None) => {}
        Err(err) => {
            warn!(path = %dir.display(), "Invalid extension manifest: {err}");
        }
    }

    let package_json_path = dir.join("package.json");
    if package_json_path.exists() {
        let manifest = read_pi_manifest(dir);
        if let Some(manifest) = manifest {
            if let Some(exts) = manifest.extensions {
                let mut entries = Vec::new();
                for ext_path in exts {
                    let resolved = dir.join(ext_path);
                    if resolved.exists() {
                        entries.push(resolved);
                    }
                }
                if !entries.is_empty() {
                    return Some(entries);
                }
            }
        }
    }

    let index_ts = dir.join("index.ts");
    if index_ts.exists() {
        return Some(vec![index_ts]);
    }
    let index_js = dir.join("index.js");
    if index_js.exists() {
        return Some(vec![index_js]);
    }
    None
}

fn collect_auto_extension_entries(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }

    let mut out = Vec::new();
    if let Some(entries) = resolve_extension_entries(dir) {
        out.extend(entries);
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
            #[allow(clippy::case_sensitive_file_extension_comparisons)]
            // .ts/.js are conventionally lowercase
            let is_ext_file = path
                .extension()
                .is_some_and(|ext| ext == "ts" || ext == "js");
            if is_ext_file {
                out.push(path);
            }
            continue;
        }
        if stats.is_dir() {
            if let Some(entries) = resolve_extension_entries(&path) {
                out.extend(entries);
            }
        }
    }
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
            pinned: version.is_some(),
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

fn parse_git_source(spec: &str, cwd: &Path) -> ParsedSource {
    let mut parts = spec.split('@');
    let repo_raw = parts.next().unwrap_or("").trim();
    let r#ref = parts
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let pinned = r#ref.is_some();

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
        let normalized = repo_raw
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches(".git")
            .to_string();

        let mut segments = normalized.split('/').collect::<Vec<_>>();
        let host = segments.first().copied().unwrap_or("").to_string();
        let path = if segments.len() >= 2 {
            segments.remove(0);
            segments.join("/")
        } else {
            String::new()
        };

        (normalized, host, path)
    };

    ParsedSource::Git {
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

fn looks_like_local_path(spec: &str) -> bool {
    let spec = spec.trim();
    spec.starts_with("file://")
        || spec.starts_with('/')
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('~')
}

fn local_path_from_spec(spec: &str, cwd: &Path) -> PathBuf {
    // `git clone` supports local paths and file:// URLs. We normalize both to a local PathBuf so
    // tests can create deterministic offline git fixtures.
    let spec = spec.trim();
    if let Some(rest) = spec.strip_prefix("file://") {
        // Keep the triple-slash form (`file:///abs/path`) working by stripping only the scheme.
        return resolve_local_path(rest, cwd);
    }
    resolve_local_path(spec, cwd)
}

fn resolve_local_path(input: &str, cwd: &Path) -> PathBuf {
    let trimmed = input.trim();
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
        if dir == root {
            break;
        }
        let Ok(canon) = dir.canonicalize() else { break };
        if !canon.starts_with(&root) {
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

fn global_settings_path() -> PathBuf {
    if let Ok(path) = std::env::var("PI_CONFIG_PATH") {
        return PathBuf::from(path);
    }
    Config::global_dir().join("settings.json")
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

fn update_package_sources(path: &Path, source: &str, action: UpdateAction) -> Result<()> {
    let mut root = read_settings_json(path)?;
    if !root.is_object() {
        root = serde_json::json!({});
    }

    let packages_value = root.get_mut("packages");
    let packages = match packages_value {
        Some(Value::Array(arr)) => arr,
        Some(_) => {
            *packages_value.unwrap() = Value::Array(Vec::new());
            root.get_mut("packages")
                .and_then(Value::as_array_mut)
                .unwrap()
        }
        None => {
            root["packages"] = Value::Array(Vec::new());
            root.get_mut("packages")
                .and_then(Value::as_array_mut)
                .unwrap()
        }
    };

    match action {
        UpdateAction::Add => {
            let exists = packages.iter().any(|existing| {
                extract_package_source(existing).is_some_and(|(s, _)| sources_match(&s, source))
            });
            if !exists {
                packages.push(Value::String(source.to_string()));
            }
        }
        UpdateAction::Remove => {
            packages.retain(|existing| {
                !extract_package_source(existing).is_some_and(|(s, _)| sources_match(&s, source))
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

fn sources_match(a: &str, b: &str) -> bool {
    normalize_source(a).is_some_and(|left| normalize_source(b).is_some_and(|right| left == right))
}

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
        let repo = rest.trim().split('@').next().unwrap_or("");
        let normalized = repo
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches(".git");
        return Some(NormalizedSource {
            kind: NormalizedKind::Git,
            key: normalized.to_string(),
        });
    }
    if looks_like_git_url(source) || source.starts_with("https://") || source.starts_with("http://")
    {
        let repo = source.split('@').next().unwrap_or("");
        let normalized = repo
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches(".git");
        return Some(NormalizedSource {
            kind: NormalizedKind::Git,
            key: normalized.to_string(),
        });
    }
    Some(NormalizedSource {
        kind: NormalizedKind::Local,
        key: source.to_string(),
    })
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
        assert!(!sources_match("npm:foo", "npm:bar"));
        assert!(!sources_match("git:github.com/a/b", "git:github.com/a/c"));
    }

    #[test]
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
            other => panic!("expected local source, got {other:?}"),
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
    fn test_project_settings_override_global_package_filters() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let project_root = temp_dir.path().join("project");
            fs::create_dir_all(project_root.join(".pi")).expect("create project settings dir");

            let package_root = temp_dir.path().join("pkg");
            fs::create_dir_all(package_root.join("extensions")).expect("create extensions dir");
            fs::write(package_root.join("extensions/a.js"), "a").expect("write a.js");
            fs::write(package_root.join("extensions/b.js"), "b").expect("write b.js");

            let global_settings_path = temp_dir.path().join("global-settings.json");
            let project_settings_path = project_root.join(".pi/settings.json");

            let global_settings = json!({
                "packages": [{
                    "source": package_root.to_string_lossy(),
                    "extensions": ["extensions/a.js"]
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
                    "extensions": ["extensions/b.js"]
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
            let expected_path = package_root.join("extensions/b.js");
            assert_eq!(enabled_extensions[0].path, expected_path);
            assert_eq!(enabled_extensions[0].metadata.scope, PackageScope::Project);

            let disabled = resolved
                .extensions
                .iter()
                .find(|entry| entry.path == package_root.join("extensions/a.js"))
                .expect("a.js entry");
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
    fn test_resolve_extension_sources_uses_temporary_scope() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let extension_path = temp_dir.path().join("ext.js");
            fs::write(&extension_path, "export default function() {}").expect("write extension");

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
    fn test_resolve_local_path_normalizes_dot_segments() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let resolved = resolve_local_path("./foo/../bar", temp_dir.path());
        assert_eq!(resolved, temp_dir.path().join("bar"));
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_local_extension_source_accepts_symlink() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let extension_path = temp_dir.path().join("ext.js");
        fs::write(&extension_path, "export default function() {}").expect("write extension");

        let symlink_path = temp_dir.path().join("ext-link.js");
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
        );

        assert_eq!(accumulator.extensions.items.len(), 1);
        assert_eq!(accumulator.extensions.items[0].path, symlink_path);
    }

    #[test]
    fn test_manifest_extensions_resolve_with_patterns() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let package_root = temp_dir.path().join("pkg");
            let extensions_dir = package_root.join("extensions");
            fs::create_dir_all(&extensions_dir).expect("create extensions dir");
            fs::write(extensions_dir.join("a.js"), "a").expect("write a.js");
            fs::write(extensions_dir.join("b.js"), "b").expect("write b.js");

            let manifest = json!({
                "name": "pkg",
                "version": "1.0.0",
                "pi": {
                    "extensions": ["extensions", "!extensions/b.js"]
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
            assert!(paths.contains(&package_root.join("extensions/a.js")));
            assert!(!paths.contains(&package_root.join("extensions/b.js")));
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
                "runtime": "js",
                "entrypoint": "index.js",
                "capabilities": []
            }))
            .expect("serialize extension manifest"),
        )
        .expect("write extension manifest");
        fs::write(
            extension_dir.join("index.js"),
            "export default function() {}",
        )
        .expect("write extension entry");

        let entries = resolve_extension_entries(&extension_dir).expect("entries");
        assert_eq!(entries, vec![extension_dir]);
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
        assert!(looks_like_local_path("./relative"));
        assert!(looks_like_local_path("../parent"));
        assert!(looks_like_local_path("/absolute"));
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

        update_package_sources(&path, "npm:foo", UpdateAction::Add).expect("add");
        let settings = read_settings_json(&path).expect("read");
        let packages = settings["packages"].as_array().expect("packages array");
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0], json!("npm:foo"));

        // Adding same source again should not duplicate
        update_package_sources(&path, "npm:foo@2.0", UpdateAction::Add).expect("add again");
        let settings = read_settings_json(&path).expect("read");
        let packages = settings["packages"].as_array().expect("packages array");
        assert_eq!(packages.len(), 1, "duplicate source should not be added");

        update_package_sources(&path, "npm:foo", UpdateAction::Remove).expect("remove");
        let settings = read_settings_json(&path).expect("read");
        let packages = settings["packages"].as_array().expect("packages array");
        assert!(packages.is_empty());
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
        let result = read_pi_manifest(dir.path());
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
    fn read_pi_manifest_no_pi_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("package.json"),
            r#"{"name": "test", "version": "1.0.0"}"#,
        )
        .expect("write");
        assert!(read_pi_manifest(dir.path()).is_none());
    }

    #[test]
    fn read_pi_manifest_no_package_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_pi_manifest(dir.path()).is_none());
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
        match parse_source("npm:@scope/pkg@1.0", dir.path()) {
            ParsedSource::Npm { spec, name, pinned } => {
                assert_eq!(spec, "@scope/pkg@1.0");
                assert_eq!(name, "@scope/pkg");
                assert!(pinned);
            }
            other => panic!("expected Npm, got {other:?}"),
        }
    }

    #[test]
    fn parse_source_npm_unpinned() {
        let dir = tempfile::tempdir().expect("tempdir");
        match parse_source("npm:express", dir.path()) {
            ParsedSource::Npm { pinned, .. } => {
                assert!(!pinned);
            }
            other => panic!("expected Npm, got {other:?}"),
        }
    }

    #[test]
    fn parse_source_git_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        match parse_source("git:github.com/user/repo@v2", dir.path()) {
            ParsedSource::Git {
                repo,
                host,
                path,
                r#ref,
                pinned,
            } => {
                assert_eq!(repo, "github.com/user/repo");
                assert_eq!(host, "github.com");
                assert_eq!(path, "user/repo");
                assert_eq!(r#ref, Some("v2".to_string()));
                assert!(pinned);
            }
            other => panic!("expected Git, got {other:?}"),
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
            other => panic!("expected Git, got {other:?}"),
        }
    }

    #[test]
    fn parse_source_local_relative() {
        let dir = tempfile::tempdir().expect("tempdir");
        match parse_source("./my-ext", dir.path()) {
            ParsedSource::Local { path } => {
                assert_eq!(path, dir.path().join("my-ext"));
            }
            other => panic!("expected Local, got {other:?}"),
        }
    }

    #[test]
    fn parse_source_local_absolute() {
        let dir = tempfile::tempdir().expect("tempdir");
        match parse_source("/abs/my-ext", dir.path()) {
            ParsedSource::Local { path } => {
                assert_eq!(path, PathBuf::from("/abs/my-ext"));
            }
            other => panic!("expected Local, got {other:?}"),
        }
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
            (ParsedSource::Git { path: p1, .. }, ParsedSource::Git { path: p2, .. }) => {
                assert_eq!(p1, p2, "same local source should produce same hash");
            }
            _ => panic!("expected Git for both"),
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
    fn collect_auto_extension_entries_finds_js_ts_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("extensions");
        fs::create_dir_all(&ext_dir).expect("create dir");
        fs::write(ext_dir.join("a.js"), "a").expect("write");
        fs::write(ext_dir.join("b.ts"), "b").expect("write");
        fs::write(ext_dir.join("c.md"), "c").expect("write");

        let entries = collect_auto_extension_entries(&ext_dir);
        assert!(entries.len() >= 2);
        let has_js = entries.iter().any(|p| p.file_name().unwrap() == "a.js");
        let has_ts = entries.iter().any(|p| p.file_name().unwrap() == "b.ts");
        let has_md = entries.iter().any(|p| p.file_name().unwrap() == "c.md");
        assert!(has_js, "should find .js files");
        assert!(has_ts, "should find .ts files");
        assert!(!has_md, "should not find .md files");
    }

    // ======================================================================
    // resolve_extension_entries with index.ts / index.js
    // ======================================================================

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
                "runtime": "js",
                "entrypoint": "main.js",
                "capabilities": []
            }))
            .unwrap(),
        )
        .expect("write manifest");
        fs::write(ext_dir.join("main.js"), "main").expect("write main");
        fs::write(ext_dir.join("index.ts"), "index").expect("write index");

        let entries = resolve_extension_entries(&ext_dir).expect("entries");
        // Manifest presence should return the directory itself, not index.ts
        assert_eq!(entries, vec![ext_dir]);
    }

    #[test]
    fn resolve_extension_entries_empty_dir_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ext_dir = dir.path().join("ext");
        fs::create_dir_all(&ext_dir).expect("create dir");
        assert!(resolve_extension_entries(&ext_dir).is_none());
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
}
