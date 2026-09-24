//! Offline, bounded inventory of locked public-registry package versions.
//!
//! No package-manager subprocess, manifest resolution, registry guessing or
//! source upload. The inventory describes lockfiles, not installed/reachable code.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

pub const INVENTORY_SCHEMA: &str = "pi.security-dependency-inventory/v1";
const MAX_LOCKFILES: usize = 32;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;
const MAX_PACKAGES: usize = 10_000;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Input {
    #[serde(rename = "op")]
    pub _op: String,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    pub lockfile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    pub ecosystem: String,
    pub name: String,
    pub version: String,
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Excluded {
    pub location: Location,
    /// An exclusion category, never an untrusted source URL or its credentials.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Lockfile {
    pub path: String,
    pub format: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    pub schema: String,
    pub scope: String,
    pub lockfiles: Vec<Lockfile>,
    pub packages: Vec<Package>,
    pub excluded: Vec<Excluded>,
}

#[derive(Default)]
struct Builder {
    packages: BTreeMap<(String, String, String), BTreeSet<Location>>,
    excluded: Vec<Excluded>,
    entries: usize,
}

fn error(message: impl Into<String>) -> Error {
    Error::tool(
        "security_scan",
        format!("[DEPENDENCY_INVENTORY] {}", message.into()),
    )
}

impl Builder {
    fn entry(&mut self) -> Result<()> {
        self.entries += 1;
        if self.entries > MAX_ENTRIES {
            return Err(error("lockfile entries exceed 20000"));
        }
        Ok(())
    }

    fn exclude(&mut self, location: Location, reason: &str) {
        self.excluded.push(Excluded {
            location,
            reason: reason.to_string(),
        });
    }

    fn add(
        &mut self,
        ecosystem: &str,
        name: &str,
        version: &str,
        location: Location,
    ) -> Result<()> {
        if !valid_name(ecosystem, name)
            || version.len() > 256
            || semver::Version::parse(version).is_err()
        {
            self.exclude(location, "invalid_or_unpinned_package_identity");
            return Ok(());
        }
        let key = (ecosystem.to_string(), name.to_string(), version.to_string());
        if !self.packages.contains_key(&key) && self.packages.len() >= MAX_PACKAGES {
            return Err(error("unique public package/version pairs exceed 10000"));
        }
        self.packages.entry(key).or_default().insert(location);
        Ok(())
    }

    fn finish(self, lockfiles: Vec<Lockfile>) -> Inventory {
        let packages = self
            .packages
            .into_iter()
            .map(|((ecosystem, name, version), locations)| Package {
                ecosystem,
                name,
                version,
                locations: locations.into_iter().collect(),
            })
            .collect();
        Inventory {
            schema: INVENTORY_SCHEMA.to_string(),
            scope: "exact public-registry versions in the selected lockfiles; no installed-code or reachability claim".to_string(),
            lockfiles, packages, excluded: self.excluded,
        }
    }
}

pub(super) fn valid_name(ecosystem: &str, name: &str) -> bool {
    let component = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    if name.is_empty() || name.len() > 256 {
        return false;
    }
    if ecosystem == "crates.io" {
        return name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b));
    }
    if let Some(scoped) = name.strip_prefix('@') {
        return scoped
            .split_once('/')
            .is_some_and(|(scope, package)| component(scope) && component(package));
    }
    component(name)
}

fn normalized_path(raw: &str) -> Result<String> {
    if raw.is_empty()
        || raw.len() > 4096
        || raw.contains('\\')
        || raw.contains(':')
        || raw.chars().any(char::is_control)
    {
        return Err(error(
            "lockfile paths must be relative, NUL-free forward-slash paths",
        ));
    }
    let mut parts = Vec::new();
    for component in Path::new(raw).components() {
        match component {
            Component::Normal(part) => parts.push(
                part.to_str()
                    .ok_or_else(|| error("lockfile path must be UTF-8"))?,
            ),
            Component::CurDir => {}
            _ => return Err(error("lockfile path cannot escape the project")),
        }
    }
    if parts.is_empty() {
        return Err(error("expected a lockfile, not a directory"));
    }
    let result = parts.join("/");
    format_for(&result)?;
    Ok(result)
}

fn format_for(path: &str) -> Result<&'static str> {
    match Path::new(path).file_name().and_then(|name| name.to_str()) {
        Some("Cargo.lock") => Ok("cargo"),
        Some("package-lock.json" | "npm-shrinkwrap.json") => Ok("npm"),
        _ => Err(error(
            "supported lockfiles are Cargo.lock, package-lock.json and npm-shrinkwrap.json",
        )),
    }
}

/// Build a deterministic inventory. Empty paths inspect the two conventional
/// root lockfiles only. Nested workspaces must supply their lockfiles explicitly.
pub fn inventory(cwd: &Path, paths: &[String]) -> Result<Inventory> {
    if paths.len() > MAX_LOCKFILES {
        return Err(error("at most 32 lockfiles may be selected"));
    }
    let defaults = paths.is_empty();
    let selected = if defaults {
        vec!["Cargo.lock".to_string(), "package-lock.json".to_string()]
    } else {
        paths.to_vec()
    };
    let selected = selected
        .iter()
        .map(|path| normalized_path(path))
        .collect::<Result<BTreeSet<_>>>()?;
    let root = Root::open(cwd)?;
    let mut builder = Builder::default();
    let mut lockfiles = Vec::new();
    let mut total = 0;
    for path in selected {
        let bytes = match root.read(&path) {
            Ok(bytes) => bytes,
            Err(failure) if defaults && failure.kind() == std::io::ErrorKind::NotFound => continue,
            Err(failure) => {
                return Err(error(format!(
                    "cannot read selected lockfile {path}: {failure}"
                )));
            }
        };
        total += bytes.len();
        if total > MAX_TOTAL_BYTES {
            return Err(error("selected lockfiles exceed 16 MiB combined"));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| error(format!("lockfile {path} is not UTF-8")))?;
        let format = format_for(&path)?;
        match format {
            "cargo" => parse_cargo(text, &path, &mut builder)?,
            "npm" => parse_npm(text, &path, &mut builder)?,
            _ => unreachable!("format_for validates format"),
        }
        lockfiles.push(Lockfile {
            path,
            format: format.to_string(),
            sha256: digest(&bytes),
        });
    }
    if lockfiles.is_empty() {
        return Err(error(
            "no supported root lockfiles found; supply explicit paths for nested projects",
        ));
    }
    Ok(builder.finish(lockfiles))
}

fn digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        })
}

#[derive(Deserialize)]
struct CargoLock {
    version: Option<u32>,
    package: Vec<CargoPackage>,
}
#[derive(Deserialize)]
struct CargoPackage {
    name: String,
    version: String,
    source: Option<String>,
}

fn parse_cargo(text: &str, path: &str, builder: &mut Builder) -> Result<()> {
    // Do not echo a parser diagnostic: it can include private lockfile lines.
    let parsed: CargoLock =
        toml::from_str(text).map_err(|_| error(format!("malformed Cargo lockfile {path}")))?;
    if parsed
        .version
        .is_some_and(|version| !(1..=4).contains(&version))
    {
        return Err(error("unsupported Cargo lockfile version"));
    }
    for package in parsed.package {
        builder.entry()?;
        let location = Location {
            lockfile: path.to_string(),
            package_path: None,
        };
        match package.source.as_deref() {
            Some(
                "registry+https://github.com/rust-lang/crates.io-index"
                | "sparse+https://index.crates.io/",
            ) => {
                builder.add("crates.io", &package.name, &package.version, location)?;
            }
            None => builder.exclude(location, "local_or_workspace_package"),
            Some(_) => builder.exclude(location, "git_or_nonpublic_registry_source"),
        }
    }
    Ok(())
}

fn parse_npm(text: &str, path: &str, builder: &mut Builder) -> Result<()> {
    let parsed: Value =
        serde_json::from_str(text).map_err(|_| error(format!("malformed npm lockfile {path}")))?;
    if !matches!(parsed["lockfileVersion"].as_u64(), Some(2 | 3)) {
        return Err(error(
            "npm lockfileVersion must be 2 or 3; legacy v1 is not silently approximated",
        ));
    }
    let packages = parsed["packages"]
        .as_object()
        .ok_or_else(|| error("npm lockfile has no packages map"))?;
    for (package_path, package) in packages {
        if package_path.is_empty() {
            continue;
        } // root project, not a dependency
        builder.entry()?;
        if package_path.len() > 4096
            || package_path.chars().any(char::is_control)
            || !package.is_object()
        {
            return Err(error("invalid npm package location or descriptor"));
        }
        let location = Location {
            lockfile: path.to_string(),
            package_path: Some(package_path.clone()),
        };
        if package.get("link").is_some_and(|link| !link.is_boolean()) {
            return Err(error("invalid npm link flag"));
        }
        if package["link"] == true {
            builder.exclude(location, "local_link");
            continue;
        }
        let installed_name = package_path
            .rsplit_once("node_modules/")
            .filter(|(prefix, _)| prefix.is_empty() || prefix.ends_with('/'))
            .map(|(_, name)| name);
        let Some(installed_name) = installed_name else {
            builder.exclude(location, "workspace_package");
            continue;
        };
        let Some(resolved) = package.get("resolved").and_then(Value::as_str) else {
            builder.exclude(location, "registry_origin_not_recorded");
            continue;
        };
        let public = url::Url::parse(resolved).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str() == Some("registry.npmjs.org")
                && url.port_or_known_default() == Some(443)
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        });
        if !public {
            builder.exclude(location, "git_local_tarball_or_nonpublic_registry_source");
            continue;
        }
        // npm aliases record their actual registry identity in `name`.
        let name = match package.get("name") {
            None => installed_name,
            Some(Value::String(name)) => name,
            Some(_) => return Err(error("invalid npm package name")),
        };
        let Some(version) = package.get("version").and_then(Value::as_str) else {
            builder.exclude(location, "version_not_recorded");
            continue;
        };
        builder.add("npm", name, version, location)?;
    }
    Ok(())
}

// Descriptor-relative traversal prevents an ancestor symlink swap from turning
// an approved workspace lockfile into an arbitrary local file upload source.
// The root itself is trusted host input, opened once per inventory.
#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
struct Root(std::os::fd::OwnedFd);
#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
impl Root {
    fn open(path: &Path) -> Result<Self> {
        use rustix::fs::{Mode, OFlags};
        rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(Self)
        .map_err(|_| error("cannot open project directory"))
    }
    fn read(&self, path: &str) -> std::io::Result<Vec<u8>> {
        use rustix::fs::{Mode, OFlags};
        use std::io::Read as _;
        let mut parent = None;
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            let mut flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
            if parts.peek().is_some() {
                flags |= OFlags::DIRECTORY;
            }
            let next = rustix::fs::openat(
                parent.as_ref().unwrap_or(&self.0),
                part,
                flags,
                Mode::empty(),
            )?;
            if parts.peek().is_none() {
                let file = std::fs::File::from(next);
                let metadata = file.metadata()?;
                if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
                    return Err(std::io::Error::other(
                        "lockfile must be regular and at most 4 MiB",
                    ));
                }
                let mut bytes = Vec::new();
                file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_FILE_BYTES {
                    return Err(std::io::Error::other("lockfile grew beyond 4 MiB"));
                }
                return Ok(bytes);
            }
            parent = Some(next);
        }
        Err(std::io::Error::other("empty lockfile path"))
    }
}

#[cfg(not(all(unix, not(any(target_os = "espidf", target_os = "redox")))))]
struct Root;
#[cfg(not(all(unix, not(any(target_os = "espidf", target_os = "redox")))))]
impl Root {
    fn open(_path: &Path) -> Result<Self> {
        Err(error(
            "confined dependency lockfile reads require a supported Unix descriptor backend",
        ))
    }
    // Same signature as the Unix reader, which does use `self`.
    #[allow(clippy::unused_self)]
    fn read(&self, _path: &str) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "unsupported descriptor backend",
        ))
    }
}

#[cfg(test)]
mod tests;
