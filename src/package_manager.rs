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
use crate::extensions::{CompatibilityScanner, load_extension_manifest};
use asupersync::Cx;
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
                let accumulator = accumulator.lock().unwrap();
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
            let cx = Cx::for_request();
            let _ = tx.send(&cx, res);
        });

        let cx = Cx::for_request();
        rx.recv(&cx)
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
        return parse_git_source(rest.trim());
    }

    if looks_like_git_url(source) || source.starts_with("https://") || source.starts_with("http://")
    {
        return parse_git_source(source);
    }

    ParsedSource::Local {
        path: resolve_local_path(source, cwd),
    }
}

fn parse_git_source(spec: &str) -> ParsedSource {
    let mut parts = spec.split('@');
    let repo_raw = parts.next().unwrap_or("").trim();
    let r#ref = parts
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let pinned = r#ref.is_some();

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

    ParsedSource::Git {
        repo: normalized,
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
}
