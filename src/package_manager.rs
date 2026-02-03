//! Package management: install/remove/update/list.
//!
//! This is a Rust port of pi-mono's package manager concepts:
//! - Sources: `npm:pkg`, `git:host/owner/repo[@ref]`, local paths
//! - Scopes: user (global) and project (local)
//! - Global npm installs use `npm install -g` (npm-managed global root)
//! - Git installs are under Pi's agent/project directories (`~/.pi/agent/git`, `./.pi/git`)

use crate::config::Config;
use crate::error::{Error, Result};
use serde_json::Value;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageScope {
    User,
    Project,
}

#[derive(Debug, Clone)]
pub struct PackageEntry {
    pub scope: PackageScope,
    pub source: String,
    pub filtered: bool,
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

    pub fn install(&self, source: &str, scope: PackageScope) -> Result<()> {
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
            ParsedSource::Local { .. } => Err(Error::config(format!(
                "Unsupported install source: {source}"
            ))),
        }
    }

    pub fn remove(&self, source: &str, scope: PackageScope) -> Result<()> {
        let parsed = parse_source(source, &self.cwd);
        match parsed {
            ParsedSource::Npm { name, .. } => self.uninstall_npm(&name, scope),
            ParsedSource::Git { host, path, .. } => self.remove_git(&host, &path, scope),
            ParsedSource::Local { .. } => Err(Error::config(format!(
                "Unsupported remove source: {source}"
            ))),
        }
    }

    pub fn update_source(&self, source: &str, scope: PackageScope) -> Result<()> {
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

    pub fn installed_path(&self, source: &str, scope: PackageScope) -> Result<Option<PathBuf>> {
        let parsed = parse_source(source, &self.cwd);
        Ok(match parsed {
            ParsedSource::Npm { name, .. } => self.npm_install_path(&name, scope)?,
            ParsedSource::Git { host, path, .. } => {
                Some(self.git_install_path(&host, &path, scope))
            }
            ParsedSource::Local { path } => Some(path),
        })
    }

    pub fn list_packages(&self) -> Result<Vec<PackageEntry>> {
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
    pub fn ensure_packages_installed(&self) -> Result<Vec<PackageEntry>> {
        let packages = self.list_packages()?;
        let mut installed = Vec::new();

        for entry in packages {
            if entry.filtered {
                continue;
            }

            // Check if already installed
            if let Ok(Some(path)) = self.installed_path(&entry.source, entry.scope) {
                if path.exists() {
                    continue;
                }
            }

            // Install the package
            if self.install(&entry.source, entry.scope).is_ok() {
                installed.push(entry);
            }
        }

        Ok(installed)
    }

    /// Resolve a package resource path.
    /// Given a package source, returns paths to resources of the given type.
    pub fn resolve_package_resources(
        &self,
        source: &str,
        scope: PackageScope,
        resource_type: &str,
    ) -> Result<Vec<PathBuf>> {
        let install_path = self
            .installed_path(source, scope)?
            .ok_or_else(|| Error::config(format!("Package not found: {source}")))?;

        if !install_path.exists() {
            return Ok(Vec::new());
        }

        // Look for resources in standard locations
        let candidates = [
            install_path.join(resource_type),
            install_path.join(format!("{resource_type}s")),
            install_path.join("resources").join(resource_type),
        ];

        let mut resources = Vec::new();
        for candidate in candidates {
            if candidate.is_dir() {
                for entry in fs::read_dir(&candidate)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path.is_file() || path.is_dir() {
                        resources.push(path);
                    }
                }
            } else if candidate.is_file() {
                resources.push(candidate);
            }
        }

        Ok(resources)
    }

    /// Get all resource paths of a given type from all installed packages.
    pub fn all_package_resources(&self, resource_type: &str) -> Result<Vec<PathBuf>> {
        let packages = self.list_packages()?;
        let mut all_resources = Vec::new();

        for entry in packages {
            if entry.filtered {
                continue;
            }
            if let Ok(resources) =
                self.resolve_package_resources(&entry.source, entry.scope, resource_type)
            {
                all_resources.extend(resources);
            }
        }

        Ok(all_resources)
    }

    pub fn add_package_source(&self, source: &str, scope: PackageScope) -> Result<()> {
        let path = match scope {
            PackageScope::User => global_settings_path(),
            PackageScope::Project => project_settings_path(&self.cwd),
        };
        update_package_sources(&path, source, UpdateAction::Add)
    }

    pub fn remove_package_source(&self, source: &str, scope: PackageScope) -> Result<()> {
        let path = match scope {
            PackageScope::User => global_settings_path(),
            PackageScope::Project => project_settings_path(&self.cwd),
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
            PackageScope::Project => Some(self.project_npm_root().join("node_modules").join(name)),
            PackageScope::User => Some(self.global_npm_root()?.join(name)),
        })
    }

    fn git_root(&self, scope: PackageScope) -> PathBuf {
        match scope {
            PackageScope::User => self.global_git_root(),
            PackageScope::Project => self.project_git_root(),
        }
    }

    fn git_install_path(&self, host: &str, repo_path: &str, scope: PackageScope) -> PathBuf {
        self.git_root(scope).join(host).join(repo_path)
    }

    fn install_npm(&self, spec: &str, scope: PackageScope) -> Result<()> {
        let (name, _) = parse_npm_spec(spec);
        match scope {
            PackageScope::User => {
                run_command("npm", ["install", "-g", spec], None)?;
            }
            PackageScope::Project => {
                let install_root = self.project_npm_root();
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

        let install_root = self.project_npm_root();
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

        let root = self.git_root(scope);
        ensure_git_ignore(&root)?;
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
        prune_empty_git_parents(&target_dir, &self.git_root(scope));
        Ok(())
    }
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
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
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
        if let Some((source, filtered)) = extract_package_source(&pkg) {
            out.push(PackageEntry {
                scope: PackageScope::User, // caller overrides
                source,
                filtered,
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
    tmp_path.persist(path).map_err(|e| Error::Io(e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let npm = manager
            .installed_path("npm:foo@1.2.3", PackageScope::Project)
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
            .installed_path("git:github.com/user/repo@v1", PackageScope::Project)
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
}
