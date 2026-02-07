//! Final extension inclusion list + version pins (bd-3vb8).
//!
//! Produces the authoritative inclusion list for acquisition and conformance work.
//! Cross-references vendored artifacts, scoring data, provenance, risk review,
//! and master catalog to produce a single, comprehensive output at
//! `docs/extension-inclusion-list.json`.
//!
//! Tiering:
//! - Tier-1 (MUST PASS): vendored artifacts with clear permissive licenses
//! - Tier-1-review: vendored artifacts with unknown licenses (need manual review)
//! - Tier-2 (STRETCH): scored candidates not yet vendored

use pi::conformance::snapshot::SourceTier;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

// ── Input types ────────────────────────────────────────────────────────

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
    #[serde(default)]
    source: Option<serde_json::Value>,
    checksum: ProvenanceChecksum,
}

#[derive(Debug, Deserialize)]
struct ProvenanceChecksum {
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct MasterCatalog {
    extensions: Vec<MasterCatalogExt>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct MasterCatalogExt {
    id: String,
    #[serde(default)]
    source_tier: Option<String>,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    file_count: usize,
}

#[derive(Debug, Deserialize)]
struct TieredCorpus {
    items: Vec<TieredItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TieredItem {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    source_tier: Option<String>,
    #[serde(default)]
    score: Option<TieredScore>,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    gates: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TieredScore {
    #[serde(default)]
    final_total: f64,
}

#[derive(Debug, Deserialize)]
struct RiskReview {
    artifacts: Vec<RiskArtifact>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct RiskArtifact {
    id: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    risk_level: Option<String>,
    #[serde(default)]
    security_severity: Option<String>,
    #[serde(default)]
    has_npm_deps: bool,
    #[serde(default)]
    npm_deps: Vec<String>,
}

// ── Output types ───────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct InclusionList {
    schema: &'static str,
    generated_at: String,
    summary: InclusionSummary,
    tier1: Vec<InclusionEntry>,
    tier1_review: Vec<InclusionEntry>,
    tier2: Vec<InclusionEntry>,
    coverage: CoverageMap,
    exclusion_notes: Vec<ExclusionNote>,
}

#[derive(Debug, Serialize)]
struct InclusionSummary {
    tier1_count: usize,
    tier1_review_count: usize,
    tier2_count: usize,
    total_must_pass: usize,
    total_stretch: usize,
    capability_coverage: BTreeMap<String, usize>,
    source_tier_distribution: BTreeMap<String, usize>,
}

#[derive(Debug, Serialize)]
struct InclusionEntry {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    directory: String,
    source_tier: String,
    license: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<f64>,
    provenance: ProvenancePin,
    capabilities: Vec<String>,
    category: String,
    risk_level: String,
    inclusion_rationale: String,
}

#[derive(Debug, Serialize)]
struct ProvenancePin {
    checksum_sha256: String,
    source_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    npm_package: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    npm_version: Option<String>,
}

#[derive(Debug, Serialize)]
struct CoverageMap {
    by_capability: BTreeMap<String, Vec<String>>,
    by_category: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Serialize)]
struct ExclusionNote {
    id: String,
    score: f64,
    reason: String,
}

// ── Helpers ────────────────────────────────────────────────────────────

fn categorize_extension(capabilities: &[String]) -> String {
    if capabilities.iter().any(|c| c == "registerProvider" || c == "streamSimple") {
        return "provider".to_string();
    }
    if capabilities.iter().any(|c| c == "registerTool") {
        return "tool".to_string();
    }
    if capabilities.iter().any(|c| c == "ui_header" || c == "ui_overlay") {
        return "ui".to_string();
    }
    if capabilities.iter().any(|c| c == "event_hook") {
        return "event-hook".to_string();
    }
    if capabilities.iter().any(|c| c == "registerShortcut") {
        return "shortcut".to_string();
    }
    if capabilities.iter().any(|c| c == "registerFlag") {
        return "flag".to_string();
    }
    if capabilities.iter().any(|c| c == "exec_api") {
        return "exec".to_string();
    }
    if capabilities.iter().any(|c| c == "session_api") {
        return "session".to_string();
    }
    "basic".to_string()
}

fn extract_provenance_pin(source: Option<&serde_json::Value>, checksum: &str) -> ProvenancePin {
    let mut pin = ProvenancePin {
        checksum_sha256: checksum.to_string(),
        source_type: "unknown".to_string(),
        source_repo: None,
        source_path: None,
        npm_package: None,
        npm_version: None,
    };

    if let Some(src) = source {
        if let Some(src_type) = src.get("type").and_then(serde_json::Value::as_str) {
            pin.source_type = src_type.to_string();
        }
        if let Some(repo) = src.get("repo").and_then(serde_json::Value::as_str) {
            pin.source_repo = Some(repo.to_string());
        }
        if let Some(path) = src.get("path").and_then(serde_json::Value::as_str) {
            pin.source_path = Some(path.to_string());
        }
        if let Some(pkg) = src.get("package").and_then(serde_json::Value::as_str) {
            pin.npm_package = Some(pkg.to_string());
        }
        if let Some(ver) = src.get("version").and_then(serde_json::Value::as_str) {
            pin.npm_version = Some(ver.to_string());
        }
    }

    pin
}

fn rationale_for(source_tier: &str, license: &str, score: Option<f64>) -> String {
    let score_note = score.map_or(String::new(), |s| format!(", score={s:.0}"));
    match source_tier {
        "official-pi-mono" => format!(
            "Official pi-mono extension ({license} license{score_note}); \
             canonical reference for conformance"
        ),
        "community" => format!(
            "Community extension ({license} license{score_note}); \
             broadens coverage of real-world patterns"
        ),
        "npm-registry" => format!(
            "npm-published extension ({license} license{score_note}); \
             validates npm packaging and dependency resolution"
        ),
        "third-party-github" => format!(
            "Third-party GitHub extension ({license} license{score_note}); \
             exercises diverse coding patterns"
        ),
        _ => format!(
            "{source_tier} extension ({license} license{score_note})"
        ),
    }
}

// ── Main test ──────────────────────────────────────────────────────────

#[test]
#[allow(clippy::too_many_lines)]
fn generate_inclusion_list() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));

    // Load all data sources
    let provenance: ProvenanceManifest = serde_json::from_slice(
        &fs::read(repo_root.join("docs/extension-artifact-provenance.json")).unwrap(),
    )
    .unwrap();

    let master: MasterCatalog = serde_json::from_slice(
        &fs::read(repo_root.join("docs/extension-master-catalog.json")).unwrap(),
    )
    .unwrap();

    let tiered: TieredCorpus = serde_json::from_slice(
        &fs::read(repo_root.join("docs/extension-tiered-corpus.json")).unwrap(),
    )
    .unwrap();

    let risk: RiskReview = serde_json::from_slice(
        &fs::read(repo_root.join("tests/ext_conformance/artifacts/RISK_REVIEW.json")).unwrap(),
    )
    .unwrap();

    // Index all data by ID
    let master_map: BTreeMap<String, &MasterCatalogExt> = master
        .extensions
        .iter()
        .map(|e| (e.id.clone(), e))
        .collect();

    let tiered_map: BTreeMap<String, &TieredItem> = tiered
        .items
        .iter()
        .map(|e| (e.id.clone(), e))
        .collect();

    let risk_map: BTreeMap<String, &RiskArtifact> = risk
        .artifacts
        .iter()
        .map(|e| (e.id.clone(), e))
        .collect();

    let mut tier1 = Vec::new();
    let mut tier1_review = Vec::new();
    let mut capability_coverage: BTreeMap<String, usize> = BTreeMap::new();
    let mut source_dist: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_capability: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut by_category: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut included_ids: BTreeSet<String> = BTreeSet::new();

    // Process all 208 vendored artifacts
    for item in &provenance.items {
        let license = item
            .license
            .as_deref()
            .unwrap_or("UNKNOWN")
            .to_string();
        let is_clear = license != "UNKNOWN" && !license.is_empty();

        let source_tier_str = SourceTier::from_directory(&item.directory);
        let source_tier = match source_tier_str {
            SourceTier::OfficialPiMono => "official-pi-mono",
            SourceTier::Community => "community",
            SourceTier::NpmRegistry => "npm-registry",
            SourceTier::ThirdPartyGithub => "third-party-github",
            SourceTier::AgentsMikeastock => "agents-mikeastock",
            SourceTier::Templates => "templates",
        };

        *source_dist.entry(source_tier.to_string()).or_insert(0) += 1;

        let capabilities = master_map
            .get(&item.id)
            .map(|e| e.capabilities.clone())
            .unwrap_or_default();

        for cap in &capabilities {
            *capability_coverage.entry(cap.clone()).or_insert(0) += 1;
            by_capability
                .entry(cap.clone())
                .or_default()
                .push(item.id.clone());
        }

        let category = categorize_extension(&capabilities);
        by_category
            .entry(category.clone())
            .or_default()
            .push(item.id.clone());

        let score = tiered_map
            .get(&item.id)
            .and_then(|t| t.score.as_ref())
            .map(|s| s.final_total);

        let name = tiered_map
            .get(&item.id)
            .and_then(|t| t.name.clone());

        let risk_level = risk_map
            .get(&item.id)
            .and_then(|r| r.risk_level.clone())
            .unwrap_or_else(|| "unknown".to_string());

        let pin = extract_provenance_pin(&item.source, &item.checksum.sha256);

        let entry = InclusionEntry {
            id: item.id.clone(),
            name,
            directory: item.directory.clone(),
            source_tier: source_tier.to_string(),
            license: license.clone(),
            score,
            provenance: pin,
            capabilities,
            category,
            risk_level,
            inclusion_rationale: rationale_for(source_tier, &license, score),
        };

        included_ids.insert(item.id.clone());

        if is_clear {
            tier1.push(entry);
        } else {
            tier1_review.push(entry);
        }
    }

    // Tier-2: scored candidates not yet vendored
    let mut tier2 = Vec::new();
    let mut exclusion_notes = Vec::new();

    for item in &tiered.items {
        if included_ids.contains(&item.id) {
            continue;
        }

        let tier = item.tier.as_deref().unwrap_or("excluded");
        let score = item
            .score
            .as_ref()
            .map_or(0.0, |s| s.final_total);

        if tier == "tier-2" || (tier != "excluded" && score >= 50.0) {
            tier2.push(InclusionEntry {
                id: item.id.clone(),
                name: item.name.clone(),
                directory: String::new(),
                source_tier: item.source_tier.clone().unwrap_or_default(),
                license: "UNKNOWN".to_string(),
                score: Some(score),
                provenance: ProvenancePin {
                    checksum_sha256: String::new(),
                    source_type: "not-yet-vendored".to_string(),
                    source_repo: None,
                    source_path: None,
                    npm_package: None,
                    npm_version: None,
                },
                capabilities: Vec::new(),
                category: "unknown".to_string(),
                risk_level: "unknown".to_string(),
                inclusion_rationale: format!(
                    "High-scoring candidate (score={score:.0}); \
                     not yet vendored, awaiting acquisition"
                ),
            });
        } else if score >= 40.0 {
            exclusion_notes.push(ExclusionNote {
                id: item.id.clone(),
                score,
                reason: format!(
                    "Score {score:.0} but not vendored; \
                     missing quality gates or license info"
                ),
            });
        }
    }

    // Sort everything for stable output
    tier1.sort_by(|a, b| a.id.cmp(&b.id));
    tier1_review.sort_by(|a, b| a.id.cmp(&b.id));
    tier2.sort_by(|a, b| {
        b.score
            .unwrap_or(0.0)
            .partial_cmp(&a.score.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    exclusion_notes.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let total_must_pass = tier1.len() + tier1_review.len();

    let inclusion_list = InclusionList {
        schema: "pi.ext.inclusion_list.v1",
        generated_at: chrono::Utc::now().to_rfc3339(),
        summary: InclusionSummary {
            tier1_count: tier1.len(),
            tier1_review_count: tier1_review.len(),
            tier2_count: tier2.len(),
            total_must_pass,
            total_stretch: tier2.len(),
            capability_coverage,
            source_tier_distribution: source_dist,
        },
        tier1,
        tier1_review,
        tier2,
        coverage: CoverageMap {
            by_capability,
            by_category,
        },
        exclusion_notes,
    };

    // Write output
    let json =
        serde_json::to_string_pretty(&inclusion_list).expect("serialize inclusion list");
    let output_path = repo_root.join("docs/extension-inclusion-list.json");
    fs::write(&output_path, &json).expect("write inclusion list");

    // Print summary
    eprintln!(
        "\n=== Extension Inclusion List ===\n\
         Tier-1 (MUST PASS, clear license): {}\n\
         Tier-1 (MUST PASS, license review): {}\n\
         Total MUST PASS: {}\n\
         Tier-2 (STRETCH): {}\n\
         Exclusion notes: {}\n\
         Output: {}\n",
        inclusion_list.summary.tier1_count,
        inclusion_list.summary.tier1_review_count,
        total_must_pass,
        inclusion_list.summary.tier2_count,
        inclusion_list.exclusion_notes.len(),
        output_path.display()
    );

    // Assertions
    assert!(
        total_must_pass >= 200,
        "Tier-1 (MUST PASS) should have >= 200 extensions, got {total_must_pass}"
    );

    // Every Tier-1 entry must have a provenance pin
    for entry in &inclusion_list.tier1 {
        assert!(
            !entry.provenance.checksum_sha256.is_empty(),
            "Tier-1 entry {} missing checksum",
            entry.id
        );
        assert!(
            entry.provenance.source_type != "unknown",
            "Tier-1 entry {} missing source type",
            entry.id
        );
    }

    // Every Tier-1 entry must have a license
    for entry in &inclusion_list.tier1 {
        assert!(
            entry.license != "UNKNOWN",
            "Tier-1 clear entry {} has UNKNOWN license",
            entry.id
        );
    }

    // Capability coverage: at least one extension for each major capability
    let required_capabilities = [
        "registerTool",
        "event_hook",
        "registerProvider",
        "ui_header",
        "registerShortcut",
        "registerFlag",
    ];
    for cap in &required_capabilities {
        assert!(
            inclusion_list
                .summary
                .capability_coverage
                .contains_key(*cap),
            "Missing coverage for capability: {cap}"
        );
    }
}
