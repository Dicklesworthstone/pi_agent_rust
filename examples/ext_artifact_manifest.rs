//! Generate extension artifact provenance manifests (bd-3uvd).
//!
//! This is a small, deterministic generator that:
//! - reads `docs/extension-master-catalog.json` (the authoritative index),
//! - infers best-effort provenance fields from `tests/ext_conformance/artifacts/`,
//! - writes `docs/extension-artifact-provenance.json`.
//!
//! The intent is auditability + reproducible refreshes when artifacts are updated.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::{Deserialize, Serialize};

const PI_MONO_REPO: &str = "https://github.com/badlogic/pi-mono";

#[derive(Debug, Parser)]
#[command(name = "ext_artifact_manifest")]
#[command(about = "Generate artifact provenance manifest JSON", long_about = None)]
struct Args {
    /// Path to `docs/extension-master-catalog.json`.
    #[arg(long, default_value = "docs/extension-master-catalog.json")]
    master_catalog: PathBuf,

    /// Root directory containing vendored extension artifacts.
    #[arg(long, default_value = "tests/ext_conformance/artifacts")]
    artifacts_dir: PathBuf,

    /// Output path for the generated provenance manifest.
    #[arg(long, default_value = "docs/extension-artifact-provenance.json")]
    out: PathBuf,

    /// Only verify output is up-to-date; do not write.
    #[arg(long, default_value_t = false)]
    check: bool,
}

#[derive(Debug, Deserialize)]
struct MasterCatalog {
    generated: String,
    extensions: Vec<MasterCatalogExtension>,
}

#[derive(Debug, Deserialize)]
struct MasterCatalogExtension {
    id: String,
    directory: String,
    source_tier: String,
    extension_files: Vec<String>,
    checksum: String,
}

#[derive(Debug, Deserialize)]
struct PackageJson {
    name: Option<String>,
    version: Option<String>,
    license: Option<String>,
    repository: Option<serde_json::Value>,
    homepage: Option<String>,
}

#[derive(Debug, Default)]
struct PackageInventory {
    root: Option<PackageJson>,
    nested: Vec<PackageJson>,
}

impl PackageInventory {
    fn all(&self) -> impl Iterator<Item = &PackageJson> {
        self.root.iter().chain(self.nested.iter())
    }
}

#[derive(Debug, Serialize)]
struct ProvenanceManifest {
    #[serde(rename = "$schema")]
    schema: &'static str,
    generated: String,
    artifact_root: String,
    items: Vec<ProvenanceItem>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProvenanceItem {
    id: String,
    directory: String,
    retrieved: String,
    checksum: Sha256Checksum,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    license: String,
    source: ProvenanceSource,
}

#[derive(Debug, Serialize)]
struct Sha256Checksum {
    sha256: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ProvenanceSource {
    Git {
        repo: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Npm {
        package: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        url: String,
    },
    Url {
        url: String,
    },
    Unknown {
        note: String,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();
    let manifest = build_manifest(&args)?;
    let json = serde_json::to_string_pretty(&manifest).context("serialize manifest")?;
    let json = format!("{json}\n");

    if args.check {
        match fs::read_to_string(&args.out) {
            Ok(existing) => {
                if existing != json {
                    bail!("Generated manifest differs from {}", args.out.display());
                }
            }
            Err(_) => bail!("Missing output file: {}", args.out.display()),
        }
        return Ok(());
    }

    fs::write(&args.out, json).with_context(|| format!("write {}", args.out.display()))?;
    Ok(())
}

fn build_manifest(args: &Args) -> Result<ProvenanceManifest> {
    let bytes = fs::read(&args.master_catalog)
        .with_context(|| format!("read master catalog: {}", args.master_catalog.display()))?;
    let catalog: MasterCatalog =
        serde_json::from_slice(&bytes).context("parse docs/extension-master-catalog.json")?;

    let mut items = catalog
        .extensions
        .iter()
        .map(|ext| build_item(ext, &catalog.generated, &args.artifacts_dir))
        .collect::<Result<Vec<_>>>()?;
    items.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(ProvenanceManifest {
        schema: "pi.ext.artifact_provenance.v1",
        generated: catalog.generated,
        artifact_root: args.artifacts_dir.to_string_lossy().to_string(),
        items,
    })
}

fn build_item(
    ext: &MasterCatalogExtension,
    retrieved: &str,
    artifacts_dir: &Path,
) -> Result<ProvenanceItem> {
    let dir = artifacts_dir.join(&ext.directory);
    let packages = read_package_inventory(&dir)?;

    let name = packages
        .root
        .as_ref()
        .and_then(|p| p.name.clone())
        .or_else(|| ext.id.rsplit('/').next().map(ToString::to_string));

    let version = packages.root.as_ref().and_then(|p| p.version.clone());
    let license = infer_license(&ext.source_tier, &packages, &dir);
    let source = infer_source(ext, &packages);

    Ok(ProvenanceItem {
        id: ext.id.clone(),
        directory: ext.directory.clone(),
        retrieved: retrieved.to_string(),
        checksum: Sha256Checksum {
            sha256: ext.checksum.clone(),
        },
        name,
        version,
        license,
        source,
    })
}

fn read_package_json_file(path: &Path) -> Result<PackageJson> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn read_package_json(dir: &Path) -> Result<Option<PackageJson>> {
    let path = dir.join("package.json");
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(read_package_json_file(&path)?))
}

fn read_package_inventory(dir: &Path) -> Result<PackageInventory> {
    let root = read_package_json(dir)?;
    let mut package_json_paths = Vec::new();
    collect_nested_package_json_paths(dir, dir, &mut package_json_paths)?;
    package_json_paths.sort();

    let mut nested = Vec::new();
    for path in package_json_paths {
        nested.push(read_package_json_file(&path)?);
    }

    Ok(PackageInventory { root, nested })
}

fn collect_nested_package_json_paths(
    root: &Path,
    dir: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("read directory {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("read directory entries {}", dir.display()))?;
    entries.sort_by_key(std::fs::DirEntry::path);

    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("read file type {}", path.display()))?;
        if file_type.is_dir() {
            if entry.file_name().to_string_lossy().as_ref() == "node_modules" {
                continue;
            }
            collect_nested_package_json_paths(root, &path, out)?;
            continue;
        }

        if !file_type.is_file() || entry.file_name().to_string_lossy().as_ref() != "package.json" {
            continue;
        }

        if path != root.join("package.json") {
            out.push(path);
        }
    }

    Ok(())
}

fn infer_license(source_tier: &str, packages: &PackageInventory, dir: &Path) -> String {
    if source_tier == "official-pi-mono" || source_tier == "community" {
        return "MIT".to_string();
    }

    if let Some(license) = packages
        .all()
        .filter_map(|pkg| pkg.license.as_deref())
        .map(str::trim)
        .find(|license| !license.is_empty())
    {
        return license.to_string();
    }

    if let Some(detected) = detect_license_file(dir) {
        return detected;
    }

    "UNKNOWN".to_string()
}

fn detect_license_file(dir: &Path) -> Option<String> {
    let candidates = [
        "LICENSE",
        "LICENSE.md",
        "LICENSE.txt",
        "COPYING",
        "COPYING.md",
        "COPYING.txt",
    ];

    for name in candidates {
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(spdx) = detect_spdx_from_text(&text) {
            return Some(spdx.to_string());
        }
        return Some("SEE_LICENSE".to_string());
    }

    None
}

fn detect_spdx_from_text(text: &str) -> Option<&'static str> {
    let upper = text.to_ascii_uppercase();
    if upper.contains("MIT LICENSE") {
        return Some("MIT");
    }
    if upper.contains("APACHE LICENSE") && upper.contains("VERSION 2.0") {
        return Some("Apache-2.0");
    }
    if upper.contains("GNU GENERAL PUBLIC LICENSE") {
        if upper.contains("VERSION 3") {
            return Some("GPL-3.0");
        }
        if upper.contains("VERSION 2") {
            return Some("GPL-2.0");
        }
        return Some("GPL");
    }
    if upper.contains("GNU LESSER GENERAL PUBLIC LICENSE") {
        if upper.contains("VERSION 3") {
            return Some("LGPL-3.0");
        }
        if upper.contains("VERSION 2.1") {
            return Some("LGPL-2.1");
        }
        return Some("LGPL");
    }
    None
}

fn infer_source(ext: &MasterCatalogExtension, packages: &PackageInventory) -> ProvenanceSource {
    if ext.source_tier == "official-pi-mono" {
        let file = ext.extension_files.first().cloned();
        let path = file.map(|file| format!("packages/coding-agent/examples/extensions/{file}"));
        return ProvenanceSource::Git {
            repo: PI_MONO_REPO.to_string(),
            path,
        };
    }

    if ext.source_tier == "community" {
        let author = community_author_from_directory(&ext.directory);
        let file = ext.extension_files.first().cloned();
        let path = match (author, file) {
            (Some(author), Some(file)) => {
                Some(format!("packages/coding-agent/community/{author}/{file}"))
            }
            _ => None,
        };
        return ProvenanceSource::Git {
            repo: PI_MONO_REPO.to_string(),
            path,
        };
    }

    if let Some(package) = ext.id.strip_prefix("npm/") {
        let url = format!("https://www.npmjs.com/package/{package}");
        return ProvenanceSource::Npm {
            package: package.to_string(),
            version: packages.root.as_ref().and_then(|p| p.version.clone()),
            url,
        };
    }

    if let Some(url) = unique_repository_or_homepage_url(packages) {
        return ProvenanceSource::Url { url };
    }

    // Best-effort fallbacks for known directory naming patterns.
    if let Some(ownerish) = ext.directory.strip_prefix("agents-") {
        let owner = ownerish.split('/').next().unwrap_or(ownerish);
        return ProvenanceSource::Git {
            repo: format!("https://github.com/{owner}/agents"),
            path: None,
        };
    }

    if let Some(url) = infer_third_party_repo_url(&ext.id) {
        return ProvenanceSource::Url { url };
    }

    ProvenanceSource::Unknown {
        note: "No repository metadata detected".to_string(),
    }
}

fn community_author_from_directory(directory: &str) -> Option<String> {
    let slug = directory.strip_prefix("community/")?;
    let (author, _) = slug.split_once('-')?;
    if author.trim().is_empty() {
        None
    } else {
        Some(author.to_string())
    }
}

fn extract_repository_url(pkg: &PackageJson) -> Option<String> {
    let value = pkg.repository.as_ref()?;
    match value {
        serde_json::Value::String(s) => normalize_repo_url(s),
        serde_json::Value::Object(map) => map
            .get("url")
            .and_then(|v| v.as_str())
            .and_then(normalize_repo_url),
        _ => None,
    }
}

fn normalize_repo_url(raw: &str) -> Option<String> {
    let mut value = raw.trim().to_string();
    if value.is_empty() {
        return None;
    }
    if let Some(stripped) = value.strip_prefix("git+") {
        value = stripped.to_string();
    }
    if let Some((stripped, _)) = value.split_once('#') {
        value = stripped.to_string();
    }
    value = value.trim_end_matches('/').to_string();

    if let Some(normalized) = normalize_hosted_repo_reference(&value) {
        value = normalized;
    }

    if let Some(stripped) = value.strip_suffix(".git") {
        value = stripped.to_string();
    }

    Some(value.trim_end_matches('/').to_string())
}

fn normalize_hosted_repo_reference(value: &str) -> Option<String> {
    for (prefix, base) in [
        ("git@github.com:", "https://github.com"),
        ("ssh://git@github.com/", "https://github.com"),
        ("git://github.com/", "https://github.com"),
        ("github.com/", "https://github.com"),
        ("git@gitlab.com:", "https://gitlab.com"),
        ("ssh://git@gitlab.com/", "https://gitlab.com"),
        ("git://gitlab.com/", "https://gitlab.com"),
        ("gitlab.com/", "https://gitlab.com"),
        ("git@bitbucket.org:", "https://bitbucket.org"),
        ("ssh://git@bitbucket.org/", "https://bitbucket.org"),
        ("git://bitbucket.org/", "https://bitbucket.org"),
        ("bitbucket.org/", "https://bitbucket.org"),
    ] {
        if let Some(path) = value.strip_prefix(prefix) {
            return normalize_hosted_repo_path(base, path);
        }
    }

    if let Some((host, path)) = value.split_once(':') {
        let base = match host {
            "github" => "https://github.com",
            "gitlab" => "https://gitlab.com",
            "bitbucket" => "https://bitbucket.org",
            _ => return None,
        };
        return normalize_hosted_repo_path(base, path);
    }

    if value.contains("://")
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with('/')
        || value.matches('/').count() != 1
    {
        return None;
    }

    normalize_hosted_repo_path("https://github.com", value)
}

fn normalize_hosted_repo_path(base: &str, path: &str) -> Option<String> {
    let path = path.trim_matches('/');
    if path.is_empty() {
        return None;
    }

    let mut segments = path.split('/');
    let owner = segments.next()?;
    let remainder = segments.collect::<Vec<_>>();
    if owner.is_empty()
        || remainder.is_empty()
        || !is_valid_repo_path_segment(owner)
        || !remainder
            .iter()
            .all(|segment| is_valid_repo_path_segment(segment))
    {
        return None;
    }

    Some(format!("{base}/{path}"))
}

fn is_valid_repo_path_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn unique_repository_or_homepage_url(packages: &PackageInventory) -> Option<String> {
    let repository_urls = packages
        .all()
        .filter_map(extract_repository_url)
        .collect::<BTreeSet<_>>();
    if repository_urls.len() == 1 {
        return repository_urls.into_iter().next();
    }

    let homepages = packages
        .all()
        .filter_map(|pkg| pkg.homepage.as_deref())
        .map(str::trim)
        .filter(|homepage| !homepage.is_empty())
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>();
    if homepages.len() == 1 {
        return homepages.into_iter().next();
    }

    None
}

fn infer_third_party_repo_url(id: &str) -> Option<String> {
    let slug = id.strip_prefix("third-party/")?;
    let (owner, repo) = split_third_party_slug(slug)?;
    Some(format!("https://github.com/{owner}/{repo}"))
}

fn split_third_party_slug(slug: &str) -> Option<(String, String)> {
    let slug = slug.trim_matches('/');
    if slug.is_empty() {
        return None;
    }

    for (marker, repo_prefix) in [
        ("-pi-", "pi-"),
        ("-agent-", "agent-"),
        ("-agents-", "agents-"),
    ] {
        if let Some((owner, rest)) = slug.split_once(marker)
            && !owner.is_empty()
            && !rest.is_empty()
        {
            return Some((owner.to_string(), format!("{repo_prefix}{rest}")));
        }
    }

    if let Some(owner) = slug.strip_suffix("-agents")
        && !owner.is_empty()
    {
        return Some((owner.to_string(), "agents".to_string()));
    }

    if slug.matches('-').count() >= 2
        && slug.len() >= 3
        && slug.as_bytes().get(1).copied() == Some(b'-')
    {
        let (owner, repo) = slug.rsplit_once('-')?;
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        return Some((owner.to_string(), repo.to_string()));
    }

    let (owner, repo) = slug.split_once('-')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn third_party_ext(id: &str) -> MasterCatalogExtension {
        MasterCatalogExtension {
            id: id.to_string(),
            directory: id.to_string(),
            source_tier: "third-party-github".to_string(),
            extension_files: vec!["index.ts".to_string()],
            checksum: "deadbeef".to_string(),
        }
    }

    fn package_inventory_with_repository(repository: serde_json::Value) -> PackageInventory {
        PackageInventory {
            root: Some(PackageJson {
                name: None,
                version: None,
                license: None,
                repository: Some(repository),
                homepage: None,
            }),
            nested: Vec::new(),
        }
    }

    #[test]
    fn build_item_uses_unique_nested_repository_metadata_for_monorepos() {
        let temp = tempdir().expect("tempdir");
        let artifacts_dir = temp.path();
        let dir = artifacts_dir.join("third-party/ben-vargas-pi-packages");
        fs::create_dir_all(dir.join("pi-ancestor-discovery")).expect("create nested package dir");
        fs::create_dir_all(dir.join("pi-cut-stack")).expect("create nested package dir");

        fs::write(
            dir.join("pi-ancestor-discovery/package.json"),
            r#"{
                "name": "@benvargas/pi-ancestor-discovery",
                "license": "MIT",
                "repository": {
                    "type": "git",
                    "url": "git+https://github.com/ben-vargas/pi-packages.git",
                    "directory": "packages/pi-ancestor-discovery"
                }
            }"#,
        )
        .expect("write package.json");
        fs::write(
            dir.join("pi-cut-stack/package.json"),
            r#"{
                "name": "@benvargas/pi-cut-stack",
                "license": "MIT",
                "repository": {
                    "type": "git",
                    "url": "git+https://github.com/ben-vargas/pi-packages.git",
                    "directory": "packages/pi-cut-stack"
                }
            }"#,
        )
        .expect("write package.json");

        let item = build_item(
            &third_party_ext("third-party/ben-vargas-pi-packages"),
            "2026-03-15T00:00:00Z",
            artifacts_dir,
        )
        .expect("build item");

        assert_eq!(item.license, "MIT");
        assert_eq!(item.name.as_deref(), Some("ben-vargas-pi-packages"));
        match item.source {
            ProvenanceSource::Url { url } => {
                assert_eq!(url, "https://github.com/ben-vargas/pi-packages");
            }
            other => panic!("expected Url source, got {other:?}"),
        }
    }

    #[test]
    fn infer_source_falls_back_to_valid_github_url_for_ambiguous_third_party_slug() {
        let source = infer_source(
            &third_party_ext("third-party/w-winter-dot314"),
            &PackageInventory::default(),
        );

        match source {
            ProvenanceSource::Url { url } => {
                assert_eq!(url, "https://github.com/w-winter/dot314");
            }
            other => panic!("expected Url source, got {other:?}"),
        }
    }

    #[test]
    fn infer_source_prefers_first_split_for_non_prefixed_multi_hyphen_slug() {
        let source = infer_source(
            &third_party_ext("third-party/rytswd-slow-mode"),
            &PackageInventory::default(),
        );

        match source {
            ProvenanceSource::Url { url } => {
                assert_eq!(url, "https://github.com/rytswd/slow-mode");
            }
            other => panic!("expected Url source, got {other:?}"),
        }
    }

    #[test]
    fn infer_source_normalizes_bare_repository_shorthand_to_https_github_url() {
        let source = infer_source(
            &third_party_ext("third-party/custom"),
            &package_inventory_with_repository(serde_json::json!("w-winter/dot314")),
        );

        match source {
            ProvenanceSource::Url { url } => {
                assert_eq!(url, "https://github.com/w-winter/dot314");
            }
            other => panic!("expected Url source, got {other:?}"),
        }
    }

    #[test]
    fn infer_source_normalizes_github_repository_shorthand_with_ref_to_https_url() {
        let source = infer_source(
            &third_party_ext("third-party/custom"),
            &package_inventory_with_repository(serde_json::json!("github:w-winter/dot314#main")),
        );

        match source {
            ProvenanceSource::Url { url } => {
                assert_eq!(url, "https://github.com/w-winter/dot314");
            }
            other => panic!("expected Url source, got {other:?}"),
        }
    }

    #[test]
    fn infer_source_normalizes_github_ssh_repository_object_to_https_url() {
        let source = infer_source(
            &third_party_ext("third-party/custom"),
            &package_inventory_with_repository(serde_json::json!({
                "type": "git",
                "url": "git+ssh://git@github.com/w-winter/dot314.git"
            })),
        );

        match source {
            ProvenanceSource::Url { url } => {
                assert_eq!(url, "https://github.com/w-winter/dot314");
            }
            other => panic!("expected Url source, got {other:?}"),
        }
    }

    #[test]
    fn infer_source_normalizes_gitlab_ssh_repository_object_to_https_url() {
        let source = infer_source(
            &third_party_ext("third-party/custom"),
            &package_inventory_with_repository(serde_json::json!({
                "type": "git",
                "url": "git+ssh://git@gitlab.com/group/subgroup/repo.git"
            })),
        );

        match source {
            ProvenanceSource::Url { url } => {
                assert_eq!(url, "https://gitlab.com/group/subgroup/repo");
            }
            other => panic!("expected Url source, got {other:?}"),
        }
    }

    #[test]
    fn infer_source_normalizes_bitbucket_scp_repository_object_to_https_url() {
        let source = infer_source(
            &third_party_ext("third-party/custom"),
            &package_inventory_with_repository(serde_json::json!({
                "type": "git",
                "url": "git@bitbucket.org:workspace/repo.git"
            })),
        );

        match source {
            ProvenanceSource::Url { url } => {
                assert_eq!(url, "https://bitbucket.org/workspace/repo");
            }
            other => panic!("expected Url source, got {other:?}"),
        }
    }
}
