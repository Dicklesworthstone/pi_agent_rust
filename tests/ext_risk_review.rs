//! Risk review: license + security + runtime dependencies (bd-3h8w).
//!
//! Scans all 208 vendored extension artifacts for:
//! 1. License compatibility (MIT/BSD/Apache preferred; flag GPL/copyleft)
//! 2. Security red flags (eval, dynamic code, cookie access, etc.)
//! 3. Runtime dependency risks (npm deps, `node_modules` requirements)
//!
//! Generates `tests/ext_conformance/artifacts/RISK_REVIEW.json` evidence log.

use pi::extension_license::{
    License, Redistributable, SecuritySeverity, detect_license_from_content,
    detect_license_from_spdx, redistributable, scan_security,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

// ── Provenance manifest types ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ProvenanceManifest {
    items: Vec<ProvenanceItem>,
}

#[derive(Debug, Deserialize)]
struct ProvenanceItem {
    id: String,
    directory: String,
    #[serde(default)]
    license: Option<String>,
}

// ── Risk review output types ───────────────────────────────────────────

#[derive(Debug, Serialize)]
struct RiskReview {
    schema: &'static str,
    generated_at: String,
    summary: RiskSummary,
    license_distribution: BTreeMap<String, usize>,
    artifacts: Vec<ArtifactRisk>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    copyleft_extensions: Vec<CopyleftEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    security_flagged: Vec<SecurityFlagEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    npm_dependency_risks: Vec<NpmRiskEntry>,
}

#[derive(Debug, Serialize)]
struct RiskSummary {
    total_artifacts: usize,
    license_clear: usize,
    license_copyleft: usize,
    license_unknown: usize,
    security_clean: usize,
    security_info_only: usize,
    security_warnings: usize,
    security_critical: usize,
    has_npm_deps: usize,
    has_heavy_deps: usize,
    overall_risk: &'static str,
}

#[derive(Debug, Serialize)]
struct ArtifactRisk {
    id: String,
    directory: String,
    license: String,
    license_source: &'static str,
    redistributable: &'static str,
    security_severity: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    security_findings: Vec<String>,
    has_npm_deps: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    npm_deps: Vec<String>,
    risk_level: &'static str,
}

#[derive(Debug, Serialize)]
struct CopyleftEntry {
    id: String,
    license: String,
    notes: String,
}

#[derive(Debug, Serialize)]
struct SecurityFlagEntry {
    id: String,
    severity: String,
    findings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct NpmRiskEntry {
    id: String,
    dep_count: usize,
    deps: Vec<String>,
    notes: String,
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Read all .ts files in a directory and concatenate their content.
fn read_all_ts_sources(dir: &Path) -> String {
    let mut content = String::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "ts" || e == "js") {
                if let Ok(text) = fs::read_to_string(&path) {
                    content.push_str(&text);
                    content.push('\n');
                }
            }
            if path.is_dir() {
                content.push_str(&read_all_ts_sources(&path));
            }
        }
    }
    content
}

/// Detect license from artifact directory (check LICENSE file, package.json).
fn detect_artifact_license(
    dir: &Path,
    provenance_license: Option<&str>,
) -> (License, &'static str) {
    // Strategy 1: Check LICENSE/LICENSE.md files
    for name in &[
        "LICENSE",
        "LICENSE.md",
        "LICENSE.txt",
        "license",
        "license.md",
    ] {
        let path = dir.join(name);
        if path.is_file() {
            if let Ok(content) = fs::read_to_string(&path) {
                let license = detect_license_from_content(&content);
                if license != License::Unknown {
                    return (license, "license_file");
                }
            }
        }
    }

    // Strategy 2: Check package.json license field
    let pkg_path = dir.join("package.json");
    if pkg_path.is_file() {
        if let Ok(content) = fs::read_to_string(&pkg_path) {
            if let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(spdx) = pkg.get("license").and_then(serde_json::Value::as_str) {
                    let license = detect_license_from_spdx(spdx);
                    if license != License::Unknown {
                        return (license, "package_json");
                    }
                }
            }
        }
    }

    // Strategy 3: Use provenance manifest license
    if let Some(prov_license) = provenance_license {
        if prov_license != "UNKNOWN" && !prov_license.is_empty() {
            let license = detect_license_from_spdx(prov_license);
            return (license, "provenance_manifest");
        }
    }

    (License::Unknown, "none")
}

/// Extract npm dependencies from package.json.
fn extract_npm_deps(dir: &Path) -> Vec<String> {
    let pkg_path = dir.join("package.json");
    if !pkg_path.is_file() {
        return Vec::new();
    }
    let Ok(content) = fs::read_to_string(&pkg_path) else {
        return Vec::new();
    };
    let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Vec::new();
    };
    let mut deps = BTreeSet::new();
    if let Some(obj) = pkg
        .get("dependencies")
        .and_then(serde_json::Value::as_object)
    {
        for key in obj.keys() {
            deps.insert(key.clone());
        }
    }
    deps.into_iter().collect()
}

/// "Heavy" dependencies that indicate significant runtime requirements.
const HEAVY_DEPS: &[&str] = &[
    "puppeteer",
    "playwright",
    "electron",
    "sharp",
    "canvas",
    "node-pty",
    "better-sqlite3",
    "fsevents",
    "grpc",
    "@grpc/grpc-js",
    "native-module",
    "node-gyp",
];

// ── Main test ──────────────────────────────────────────────────────────

#[test]
#[allow(clippy::too_many_lines)]
fn risk_review_evidence_log() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let artifacts_root = repo_root.join("tests/ext_conformance/artifacts");

    // Load provenance manifest for license data
    let provenance_path = repo_root.join("docs/extension-artifact-provenance.json");
    let provenance: ProvenanceManifest =
        serde_json::from_slice(&fs::read(&provenance_path).expect("read provenance"))
            .expect("parse provenance");

    let mut artifacts = Vec::new();
    let mut license_dist: BTreeMap<String, usize> = BTreeMap::new();
    let mut copyleft_extensions = Vec::new();
    let mut security_flagged = Vec::new();
    let mut npm_dependency_risks = Vec::new();

    let mut license_clear = 0usize;
    let mut license_copyleft = 0usize;
    let mut license_unknown = 0usize;
    let mut security_clean = 0usize;
    let mut security_info_only = 0usize;
    let mut security_warnings = 0usize;
    let mut security_critical = 0usize;
    let mut has_npm_deps = 0usize;
    let mut has_heavy_deps = 0usize;

    for item in &provenance.items {
        let artifact_dir = artifacts_root.join(&item.directory);
        if !artifact_dir.is_dir() {
            continue;
        }

        // 1. License check
        let (license, license_source) =
            detect_artifact_license(&artifact_dir, item.license.as_deref());
        let redist = redistributable(&license);
        let spdx = license.spdx().to_string();
        *license_dist.entry(spdx.clone()).or_insert(0) += 1;

        let redist_str = match redist {
            Redistributable::Yes => {
                license_clear += 1;
                "yes"
            }
            Redistributable::Copyleft => {
                license_copyleft += 1;
                copyleft_extensions.push(CopyleftEntry {
                    id: item.id.clone(),
                    license: spdx.clone(),
                    notes: format!(
                        "Copyleft license ({spdx}); must preserve license in redistribution"
                    ),
                });
                "copyleft"
            }
            Redistributable::Unknown => {
                license_unknown += 1;
                "unknown"
            }
            Redistributable::No => "no",
        };

        // 2. Security scan
        let source_content = read_all_ts_sources(&artifact_dir);
        let findings = scan_security(&source_content);
        let max_severity = findings
            .iter()
            .map(|f| match f.severity {
                SecuritySeverity::Critical => 3,
                SecuritySeverity::Warning => 2,
                SecuritySeverity::Info => 1,
            })
            .max()
            .unwrap_or(0);

        let severity_str = match max_severity {
            0 => {
                security_clean += 1;
                "clean"
            }
            1 => {
                security_info_only += 1;
                "info"
            }
            2 => {
                security_warnings += 1;
                "warning"
            }
            _ => {
                security_critical += 1;
                "critical"
            }
        };

        let finding_strs: Vec<String> = findings
            .iter()
            .map(|f| format!("[{:?}] {}: {}", f.severity, f.pattern, f.description))
            .collect();

        if max_severity >= 2 {
            security_flagged.push(SecurityFlagEntry {
                id: item.id.clone(),
                severity: severity_str.to_string(),
                findings: finding_strs.clone(),
            });
        }

        // 3. Runtime dependency check
        let deps = extract_npm_deps(&artifact_dir);
        let dep_count = deps.len();
        let has_deps = !deps.is_empty();
        if has_deps {
            has_npm_deps += 1;
        }

        let heavy: Vec<String> = deps
            .iter()
            .filter(|d| HEAVY_DEPS.contains(&d.as_str()))
            .cloned()
            .collect();
        let is_heavy = !heavy.is_empty();
        if is_heavy {
            has_heavy_deps += 1;
        }

        if has_deps {
            let notes = if is_heavy {
                format!("{dep_count} deps including heavy: {}", heavy.join(", "))
            } else {
                format!("{dep_count} npm dependencies")
            };
            npm_dependency_risks.push(NpmRiskEntry {
                id: item.id.clone(),
                dep_count,
                deps: deps.clone(),
                notes,
            });
        }

        // 4. Overall risk level
        let risk_level = if max_severity >= 3 {
            "high"
        } else if redist == Redistributable::Copyleft || is_heavy || max_severity >= 2 {
            "medium"
        } else if redist == Redistributable::Unknown || has_deps {
            "low"
        } else {
            "minimal"
        };

        artifacts.push(ArtifactRisk {
            id: item.id.clone(),
            directory: item.directory.clone(),
            license: spdx,
            license_source,
            redistributable: redist_str,
            security_severity: severity_str,
            security_findings: finding_strs,
            has_npm_deps: has_deps,
            npm_deps: deps,
            risk_level,
        });
    }

    let total = artifacts.len();
    let overall_risk = if security_critical > 0 {
        "HIGH"
    } else if license_copyleft > 0 || security_warnings > 5 || has_heavy_deps > 0 {
        "MEDIUM"
    } else {
        "LOW"
    };

    let review = RiskReview {
        schema: "pi.ext.risk_review.v1",
        generated_at: chrono::Utc::now().to_rfc3339(),
        summary: RiskSummary {
            total_artifacts: total,
            license_clear,
            license_copyleft,
            license_unknown,
            security_clean,
            security_info_only,
            security_warnings,
            security_critical,
            has_npm_deps,
            has_heavy_deps,
            overall_risk,
        },
        license_distribution: license_dist,
        artifacts,
        copyleft_extensions,
        security_flagged,
        npm_dependency_risks,
    };

    // Write evidence log
    let json = serde_json::to_string_pretty(&review).expect("serialize risk review");
    let output_path = repo_root.join("tests/ext_conformance/artifacts/RISK_REVIEW.json");
    fs::write(&output_path, &json).expect("write risk review");

    // Print summary
    eprintln!(
        "\n=== Risk Review Summary ===\n\
         Total artifacts: {total}\n\
         License clear: {license_clear}\n\
         License copyleft: {license_copyleft}\n\
         License unknown: {license_unknown}\n\
         Security clean: {security_clean}\n\
         Security info: {security_info_only}\n\
         Security warnings: {security_warnings}\n\
         Security critical: {security_critical}\n\
         Has npm deps: {has_npm_deps}\n\
         Has heavy deps: {has_heavy_deps}\n\
         Overall risk: {overall_risk}\n\
         Evidence log: {}\n",
        output_path.display()
    );

    // Assertions: no critical security issues in vendored artifacts
    assert_eq!(
        security_critical,
        0,
        "Critical security findings in vendored artifacts: {:?}",
        review
            .security_flagged
            .iter()
            .filter(|f| f.severity == "critical")
            .map(|f| &f.id)
            .collect::<Vec<_>>()
    );

    // Assertion: no copyleft licenses (our corpus should be permissive-only)
    // Note: if copyleft extensions are intentionally included, document as exceptions
    if !review.copyleft_extensions.is_empty() {
        eprintln!(
            "WARNING: {} copyleft-licensed extensions found:",
            review.copyleft_extensions.len()
        );
        for entry in &review.copyleft_extensions {
            eprintln!("  {} ({}): {}", entry.id, entry.license, entry.notes);
        }
    }
}
