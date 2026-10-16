//! Workspace-installed ESM packages over the existing file/linker boundary.
//!
//! This is a resolver, not a package manager or Node compatibility runtime.
//! Package metadata is an immutable, bounded snapshot of this retained realm.
//! Conditional exports retain JSON insertion order; serde_json's default map
//! ordering must not change which branch a package author selected.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde::Deserialize;
use serde::de::{MapAccess, SeqAccess, Visitor};

use super::{Admission, ModuleFiles, MAX_PATH_BYTES, module_language, source, valid_spelling};

const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_MANIFEST_LOOKUPS: usize = 128;
const MAX_METADATA_BYTES: usize = 2 * 1024 * 1024;
const MAX_ANCESTORS: usize = 64;
const MAX_MAP_NODES: usize = 2048;
const MAX_MAP_DEPTH: usize = 32;

#[derive(Default)]
pub(super) struct Catalog {
    manifests: BTreeMap<PathBuf, Option<Rc<Manifest>>>,
    roots: BTreeSet<PathBuf>,
    lookups: usize,
    bytes: usize,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(rename = "type")]
    kind: Option<String>,
    main: Option<String>,
    // Absent exports allow legacy entry paths. Explicit null seals a package.
    #[serde(default, deserialize_with = "present_exports")]
    exports: Option<Export>,
}

#[derive(Debug)]
enum Export {
    Blocked,
    Target(String),
    Conditions(Vec<(String, Self)>),
    Alternatives(Vec<Self>),
}

fn present_exports<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Export>, D::Error> {
    Export::deserialize(deserializer).map(Some)
}

impl<'de> Deserialize<'de> for Export {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ExportVisitor;
        impl<'de> Visitor<'de> for ExportVisitor {
            type Value = Export;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a package target, null, ordered condition map, or array")
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Export, E> {
                Ok(Export::Blocked)
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Export, E> {
                Ok(Export::Target(value.to_owned()))
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Export, E> {
                Ok(Export::Target(value))
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Export, M::Error> {
                let mut entries = Vec::new();
                let mut names = BTreeSet::new();
                while let Some((name, value)) = map.next_entry::<String, Export>()? {
                    if entries.len() >= MAX_MAP_NODES || !names.insert(name.clone()) {
                        return Err(serde::de::Error::custom("duplicate or excessive package map keys"));
                    }
                    entries.push((name, value));
                }
                Ok(Export::Conditions(entries))
            }
            fn visit_seq<S: SeqAccess<'de>>(self, mut sequence: S) -> Result<Export, S::Error> {
                let mut entries = Vec::new();
                while let Some(value) = sequence.next_element::<Export>()? {
                    if entries.len() >= MAX_MAP_NODES {
                        return Err(serde::de::Error::custom("excessive package alternatives"));
                    }
                    entries.push(value);
                }
                Ok(Export::Alternatives(entries))
            }
        }
        deserializer.deserialize_any(ExportVisitor)
    }
}

fn error(code: &str, message: &str) -> String {
    // Package resolution is part of module admission. Keep its established
    // failure family as well as the more specific package diagnostic.
    format!("EVAL_MODULE: EVAL_PACKAGE_{code}: {message}")
}

fn check_map(value: &Export, depth: usize, remaining: &mut usize) -> Admission<()> {
    if depth > MAX_MAP_DEPTH || *remaining == 0 {
        return Err(error("LIMIT", "package map nesting or node budget exceeded"));
    }
    *remaining -= 1;
    match value {
        Export::Conditions(entries) => {
            for (_, value) in entries { check_map(value, depth + 1, remaining)?; }
        }
        Export::Alternatives(entries) => {
            for value in entries { check_map(value, depth + 1, remaining)?; }
        }
        _ => {}
    }
    Ok(())
}

fn manifest(files: &ModuleFiles, root: &Path, path: &Path) -> Admission<Option<Rc<Manifest>>> {
    {
        let mut state = files.0.borrow_mut();
        state.check()?;
        if let Some(cached) = state.packages.manifests.get(path) { return Ok(cached.clone()); }
        if state.packages.lookups >= MAX_MANIFEST_LOOKUPS {
            return Err(error("LIMIT", "package metadata catalog exhausted; reset JavaScript"));
        }
        // Failed and missing reads consume admission too, not just successes.
        state.packages.lookups += 1;
    }
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {
            // A broken manifest symlink is not an absent manifest.
            if std::fs::symlink_metadata(path).is_ok() {
                return Err(error("MANIFEST", "package metadata target is unavailable"));
            }
            None
        }
        Err(_) => return Err(error("MANIFEST", "package metadata is unreadable")),
    };
    let value = if let Some(metadata) = metadata {
        if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES as u64 {
            return Err(error("LIMIT", "package.json must be a regular file of at most 64 KiB"));
        }
        // The same bounded reader protects code and metadata, including UTF-8,
        // canonical workspace containment and observed open/read identity races.
        let loaded = source::load_sync(root, path)
            .map_err(|_| error("MANIFEST", "package metadata failed workspace file admission"))?;
        {
            let mut state = files.0.borrow_mut();
            state.check()?;
            if loaded.code.len() > MAX_MANIFEST_BYTES
                || loaded.code.len() > MAX_METADATA_BYTES.saturating_sub(state.packages.bytes)
            {
                return Err(error("LIMIT", "package metadata exceeds the realm byte budget"));
            }
            state.packages.bytes += loaded.code.len();
        }
        let parsed: Manifest = serde_json::from_str(&loaded.code)
            .map_err(|_| error("MANIFEST", "invalid package.json; source content is not included"))?;
        if let Some(exports) = &parsed.exports {
            let mut remaining = MAX_MAP_NODES;
            check_map(exports, 0, &mut remaining)?;
        }
        Some(Rc::new(parsed))
    } else { None };
    let mut state = files.0.borrow_mut();
    state.check()?;
    state.packages.manifests.insert(path.to_path_buf(), value.clone());
    Ok(value)
}

fn valid_segments(path: &str) -> bool {
    !path.is_empty() && valid_spelling(path) && !path.contains(['%', '*'])
        && path.split('/').all(|segment| {
            !segment.is_empty() && !matches!(segment, "." | "..")
                && !segment.eq_ignore_ascii_case("node_modules")
                && !segment.ends_with(['.', ' '])
        })
}

fn split_package(name: &str) -> Admission<(&str, &str)> {
    if !valid_segments(name) || name.starts_with(['/', '.']) {
        return Err(error("SPECIFIER", "use a package name with an optional explicit subpath"));
    }
    let end = if name.starts_with('@') {
        let scope = name.find('/').ok_or_else(|| error("SPECIFIER", "scoped package requires a name"))?;
        name[scope + 1..].find('/').map_or(name.len(), |end| scope + 1 + end)
    } else { name.find('/').unwrap_or(name.len()) };
    let package = &name[..end];
    let parts = package.strip_prefix('@').unwrap_or(package);
    if package.len() > 214 || parts.split('/').any(|part| {
        part.is_empty() || part.starts_with('.')
            || !part.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    }) {
        return Err(error("SPECIFIER", "package name is not portable ASCII or exceeds 214 bytes"));
    }
    Ok((package, &name[end..]))
}

#[derive(Debug, PartialEq, Eq)]
enum Selection { Unmatched, Blocked, Path(String) }

fn select(value: &Export) -> Admission<Selection> {
    match value {
        Export::Blocked => Ok(Selection::Blocked),
        Export::Target(path) => Ok(Selection::Path(path.clone())),
        Export::Conditions(entries) => {
            if entries.iter().any(|(condition, _)| {
                condition.starts_with('.') || condition.parse::<u32>().is_ok()
            }) {
                return Err(error("EXPORTS", "invalid conditional-export key"));
            }
            for (condition, value) in entries {
                // Neither Node nor browser globals are supplied by this realm.
                // Evaluate matching conditions in author order, not key order.
                if matches!(condition.as_str(), "import" | "default") {
                    match select(value)? {
                        Selection::Unmatched => {}
                        selected => return Ok(selected),
                    }
                }
            }
            Ok(Selection::Unmatched)
        }
        Export::Alternatives(_) => Err(error("EXPORTS", "array export alternatives are not supported")),
    }
}

fn exported_target(exports: &Export, subpath: &str) -> Admission<String> {
    let selected = if let Export::Conditions(entries) = exports
        && entries.iter().any(|(key, _)| key.starts_with('.'))
    {
        if entries.iter().any(|(key, _)| key != "." && !key.starts_with("./")) {
            return Err(error("EXPORTS", "exports cannot mix subpaths with conditions"));
        }
        match entries.iter().find(|(key, _)| key == subpath) {
            Some((_, value)) => select(value)?,
            None => Selection::Unmatched,
        }
    } else if subpath == "." { select(exports)? } else { Selection::Unmatched };
    match selected {
        Selection::Path(path) => Ok(path),
        _ => Err(error("NOT_EXPORTED", "package does not export this entry for import/default conditions")),
    }
}

fn target_path(root: &Path, target: &str) -> Admission<PathBuf> {
    let relative = target.strip_prefix("./")
        .filter(|path| valid_segments(path))
        .ok_or_else(|| error("TARGET", "package targets must be explicit ./ paths without traversal or URL syntax"))?;
    if module_language(Path::new(relative)).is_none() {
        return Err(error("FORMAT", "package target must have a supported ESM, TypeScript or data suffix"));
    }
    let path = root.join(relative);
    if path.to_str().is_none_or(|path| path.len() > MAX_PATH_BYTES) {
        return Err(error("LIMIT", "package target path exceeds bounded UTF-8"));
    }
    Ok(path)
}

/// Resolve only installed packages found at or below the workspace boundary.
/// An existing nearer package is authoritative even when broken or sealed.
pub(super) fn resolve(files: &ModuleFiles, root: &Path, parent: &Path, name: &str) -> Admission<(PathBuf, PathBuf)> {
    let (package, tail) = split_package(name)?;
    let subpath = format!(".{tail}");
    let mut directory = parent;
    for _ in 0..MAX_ANCESTORS {
        files.0.borrow().check()?;
        if directory.file_name().is_none_or(|name| name != "node_modules") {
            let candidate = directory.join("node_modules").join(package);
            match std::fs::symlink_metadata(&candidate) {
                Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(error("PATH", "installed package is unreadable")),
                Ok(_) => {
                    let package_root = std::fs::canonicalize(&candidate)
                        .map_err(|_| error("PATH", "installed package target is unavailable"))?;
                    if !package_root.starts_with(root) || !package_root.is_dir() {
                        return Err(error("PATH", "installed package must be a directory inside the workspace"));
                    }
                    let manifest = manifest(files, root, &package_root.join("package.json"))?
                        .ok_or_else(|| error("MANIFEST", "installed package requires package.json"))?;
                    let target = if let Some(exports) = &manifest.exports {
                        exported_target(exports, &subpath)?
                    } else if tail.is_empty() {
                        let main = manifest.main.as_deref().unwrap_or("./index.js");
                        if main.starts_with("./") { main.to_owned() } else { format!("./{main}") }
                    } else { subpath.clone() };
                    let path = target_path(&package_root, &target)?;
                    let canonical = std::fs::canonicalize(&path)
                        .map_err(|_| error("PATH", "exported package file is unavailable"))?;
                    if !canonical.starts_with(&package_root) {
                        return Err(error("PATH", "exported file escapes its package directory"));
                    }
                    let mut state = files.0.borrow_mut();
                    state.check()?;
                    state.packages.roots.insert(package_root.clone());
                    return Ok((path, package_root));
                }
            }
        }
        if directory == root { return Err(error("NOT_FOUND", "package is not installed inside this workspace")); }
        directory = directory.parent().filter(|parent| parent.starts_with(root))
            .ok_or_else(|| error("PATH", "package search reached the workspace boundary"))?;
    }
    Err(error("LIMIT", "package ancestor search exceeds 64 directories"))
}

/// A package .js file needs an explicit ESM scope, including relative edges
/// within symlinked workspace packages. Ordinary workspace scripts are unchanged.
pub(super) fn validate_javascript(files: &ModuleFiles, root: &Path, path: &Path) -> Admission<()> {
    if path.extension().is_none_or(|extension| extension != "js") { return Ok(()); }
    let is_package = path.components().any(|part| part.as_os_str() == "node_modules")
        || files.0.borrow().packages.roots.iter().any(|root| path.starts_with(root));
    if !is_package { return Ok(()); }
    let mut directory = path.parent().ok_or_else(|| error("PATH", "package file has no directory"))?;
    for _ in 0..MAX_ANCESTORS {
        files.0.borrow().check()?;
        if directory.file_name().is_some_and(|name| name == "node_modules") { break; }
        if let Some(manifest) = manifest(files, root, &directory.join("package.json"))? {
            return if manifest.kind.as_deref() == Some("module") { Ok(()) } else {
                Err(error("COMMONJS", "package .js requires type:module; CommonJS is not executed"))
            };
        }
        if directory == root { break; }
        directory = directory.parent().filter(|parent| parent.starts_with(root))
            .ok_or_else(|| error("PATH", "package scope escaped the workspace"))?;
    }
    Err(error("COMMONJS", "package .js has no explicit type:module scope; use ESM"))
}

#[cfg(test)]
mod tests;
