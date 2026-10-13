//! Python lockfile inventories, not Python environment resolution.
//!
//! All locked marker/group variants are included. Source metadata, rather than
//! the package's name, determines whether an identity may be sent to PyPI/OSV.
//! No wheel, sdist, index, requirements include or project code is opened.

use super::{Builder, Location, Result, error};
use serde::Deserialize;
use std::sync::LazyLock;

// This parses public PEP 440 identifiers only, not constraints or local builds.
// Normalize decimal strings without machine-integer conversions: epochs and
// release components need not fit u64. Retain the release component count.
static PUBLIC_VERSION: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?ix)\A v?
        (?:(?P<epoch>[0-9]+)!)?
        (?P<release>[0-9]+(?:\.[0-9]+)*)
        (?:[-_.]?(?P<pre>alpha|beta|preview|pre|rc|a|b|c)[-_.]?(?P<pre_n>[0-9]+)?)?
        (?:(?:-(?P<post_short>[0-9]+))|(?:[-_.]?(?P<post>post|rev|r)[-_.]?(?P<post_n>[0-9]+)?))?
        (?:[-_.]?(?P<dev>dev)[-_.]?(?P<dev_n>[0-9]+)?)?
        \z",
    )
    .expect("constant public Python version expression")
});

fn decimal(value: &str) -> &str {
    let value = value.trim_start_matches('0');
    if value.is_empty() { "0" } else { value }
}

pub(super) fn version(raw: &str) -> Option<String> {
    if raw.len() > 256 || !raw.is_ascii() {
        return None;
    }
    let captures = PUBLIC_VERSION.captures(raw.trim())?;
    let number = |field: &str| captures.name(field).map_or("0", |value| decimal(value.as_str()));
    let mut result = String::new();
    let epoch = number("epoch");
    if epoch != "0" {
        result.push_str(epoch);
        result.push('!');
    }
    for (index, part) in captures.name("release")?.as_str().split('.').enumerate() {
        if index > 0 {
            result.push('.');
        }
        result.push_str(decimal(part));
    }
    if let Some(pre) = captures.name("pre") {
        let pre = match pre.as_str().to_ascii_lowercase().as_str() {
            "alpha" | "a" => "a",
            "beta" | "b" => "b",
            _ => "rc",
        };
        result.push_str(pre);
        result.push_str(number("pre_n"));
    }
    if captures.name("post").is_some() || captures.name("post_short").is_some() {
        result.push_str(".post");
        result.push_str(if captures.name("post_short").is_some() {
            number("post_short")
        } else {
            number("post_n")
        });
    }
    if captures.name("dev").is_some() {
        result.push_str(".dev");
        result.push_str(number("dev_n"));
    }
    // Adding implicit numbers and separators can increase the length slightly.
    (result.len() <= 256).then_some(result)
}

pub(super) fn name(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    if raw.len() > 256
        || !bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        || !bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        || !bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(byte))
    {
        return None;
    }
    let mut result = String::with_capacity(raw.len());
    let mut separator = false;
    for byte in bytes {
        if b"._-".contains(byte) {
            if !separator {
                result.push('-');
            }
            separator = true;
        } else {
            result.push(char::from(byte.to_ascii_lowercase()));
            separator = false;
        }
    }
    Some(result)
}

#[derive(Deserialize)]
struct UvLock {
    version: u64,
    // Revisions are documented additive changes; the schema version is the gate.
    #[serde(rename = "revision")]
    _revision: Option<u64>,
    package: Vec<LockedPackage>,
}

#[derive(Deserialize)]
struct PoetryLock {
    metadata: PoetryMetadata,
    package: Vec<LockedPackage>,
}

#[derive(Deserialize)]
struct PoetryMetadata {
    #[serde(rename = "lock-version")]
    lock_version: String,
}

#[derive(Deserialize)]
struct LockedPackage {
    name: String,
    version: Option<String>,
    source: Option<toml::Table>,
    develop: Option<bool>,
}

fn public_index(raw: &str) -> bool {
    if raw.trim() != raw || raw.chars().any(char::is_control) {
        return false;
    }
    url::Url::parse(raw).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("pypi.org")
            && url.port_or_known_default() == Some(443)
            && matches!(url.path(), "/simple" | "/simple/")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

fn source_text<'a>(source: &'a toml::Table, field: &str) -> Result<Option<&'a str>> {
    source.get(field).map(|value| {
        value.as_str().ok_or_else(|| error("invalid Python lockfile source descriptor"))
    }).transpose()
}

fn uv_public(source: Option<&toml::Table>) -> Result<bool> {
    let Some(source) = source else { return Ok(false) };
    let registry = source_text(source, "registry")?;
    // A registry plus a direct/path/git source is ambiguous, never public by
    // precedence. Unknown future source shapes remain explicit exclusions.
    Ok(source.len() == 1 && registry.is_some_and(public_index))
}

fn poetry_public(source: Option<&toml::Table>) -> Result<bool> {
    // Poetry's locker omits `source` for default PyPI packages. Unlike uv, this
    // omission has defined semantics; an explicit empty/unknown source does not.
    let Some(source) = source else { return Ok(true) };
    let kind = source_text(source, "type")?;
    let url = source_text(source, "url")?;
    source_text(source, "reference")?;
    Ok(kind == Some("legacy")
        && url.is_some_and(public_index)
        && source.keys().all(|key| matches!(key.as_str(), "type" | "url" | "reference")))
}

pub(super) fn parse(text: &str, path: &str, format: &str, builder: &mut Builder) -> Result<()> {
    let packages = match format {
        "uv" => {
            let lock: UvLock = toml::from_str(text)
                .map_err(|_| error("malformed uv lockfile; parser contents are not exposed"))?;
            if lock.version != 1 {
                return Err(error("unsupported uv lockfile schema; expected version 1"));
            }
            lock.package
        }
        "poetry" => {
            let lock: PoetryLock = toml::from_str(text)
                .map_err(|_| error("malformed Poetry lockfile; parser contents are not exposed"))?;
            if !matches!(lock.metadata.lock_version.as_str(), "1.1" | "2.0" | "2.1") {
                return Err(error("unsupported Poetry lock version; expected 1.1, 2.0 or 2.1"));
            }
            lock.package
        }
        _ => return Err(error("unsupported Python lockfile format")),
    };
    for (index, package) in packages.into_iter().enumerate() {
        builder.entry()?;
        // The ordinal identifies the TOML table, not an installed directory.
        // Even exclusions therefore reveal neither private names nor sources.
        let location = Location {
            lockfile: path.to_string(),
            package_path: Some(format!("package[{index}]")),
        };
        if package.develop == Some(true) {
            builder.exclude(location, "local_or_editable_python_package");
            continue;
        }
        let public = if format == "uv" {
            uv_public(package.source.as_ref())?
        } else {
            poetry_public(package.source.as_ref())?
        };
        if !public {
            builder.exclude(location, "python_origin_not_public_pypi");
        } else if let Some(version) = &package.version {
            if version.contains('+') {
                builder.exclude(location, "python_local_version_not_public_release");
            } else {
                builder.add("PyPI", &package.name, version, location)?;
            }
        } else {
            builder.exclude(location, "version_not_recorded");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
