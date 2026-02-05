//! Generate extension artifact provenance manifests (bd-3uvd).
//!
//! This is a small, deterministic generator that:
//! - reads `docs/extension-master-catalog.json` (the authoritative index),
//! - infers best-effort provenance fields from `tests/ext_conformance/artifacts/`,
//! - writes `docs/extension-artifact-provenance.json`.
//!
//! The intent is auditability + reproducible refreshes when artifacts are updated.

#![forbid(unsafe_code)]

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
    let package_json = read_package_json(&dir)?;

    let name = package_json
        .as_ref()
        .and_then(|p| p.name.clone())
        .or_else(|| ext.id.rsplit('/').next().map(ToString::to_string));

    let version = package_json.as_ref().and_then(|p| p.version.clone());
    let license = infer_license(&ext.source_tier, package_json.as_ref(), &dir);
    let source = infer_source(ext, package_json.as_ref());

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

fn read_package_json(dir: &Path) -> Result<Option<PackageJson>> {
    let path = dir.join("package.json");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let pkg: PackageJson =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(pkg))
}

fn infer_license(source_tier: &str, pkg: Option<&PackageJson>, dir: &Path) -> String {
    if source_tier == "official-pi-mono" || source_tier == "community" {
        return "MIT".to_string();
    }

    if let Some(license) = pkg.and_then(|p| p.license.as_deref()) {
        let trimmed = license.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
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

fn infer_source(ext: &MasterCatalogExtension, pkg: Option<&PackageJson>) -> ProvenanceSource {
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
            version: pkg.and_then(|p| p.version.clone()),
            url,
        };
    }

    if let Some(url) = pkg
        .and_then(extract_repository_url)
        .or_else(|| pkg.and_then(|p| p.homepage.clone()))
    {
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

    if let Some(ownerish) = ext.id.strip_prefix("third-party/") {
        return ProvenanceSource::Url {
            url: format!("https://github.com/{ownerish}"),
        };
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
    if std::path::Path::new(&value)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("git"))
    {
        value.truncate(value.len().saturating_sub(4));
    }
    Some(value)
}
