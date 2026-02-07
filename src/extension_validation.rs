//! Deterministic classifier and deduplication engine for Pi extension candidates.
//!
//! This module takes mixed-source research data (GitHub code search, repo search,
//! npm scan, curated lists) and produces a validated, deduplicated candidate set.
//!
//! Each candidate gets:
//! - A `ValidationStatus` (true-extension, mention-only, unknown)
//! - `ValidationEvidence` (which signals matched)
//! - A canonical identity key for deduplication
//!
//! The classifier is intentionally conservative: a candidate must show clear Pi
//! extension API usage to be classified as `TrueExtension`.

use crate::extension_popularity::{
    CandidateItem, CandidatePool, GitHubRepoCandidate, github_repo_candidate_from_url,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ────────────────────────────────────────────────────────────────────────────
// Classification types
// ────────────────────────────────────────────────────────────────────────────

/// Validation status for a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    /// Confirmed Pi extension: has API import + export default or registration calls.
    TrueExtension,
    /// Mentions Pi but does not implement the extension protocol.
    MentionOnly,
    /// Insufficient evidence to classify.
    Unknown,
}

/// Evidence supporting a validation decision.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValidationEvidence {
    /// Has `@mariozechner/pi-coding-agent` or `@mariozechner/pi-ai` import.
    pub has_api_import: bool,
    /// Has `export default` in entrypoint.
    pub has_export_default: bool,
    /// Registration API calls found (e.g. `registerTool`, `registerCommand`).
    pub registrations: Vec<String>,
    /// Sources that contributed to this candidate.
    pub sources: Vec<String>,
    /// Human-readable reason for the classification decision.
    pub reason: String,
}

/// A fully validated candidate with classification and dedup info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatedCandidate {
    /// Canonical identity key (e.g. "nicobailon/pi-messenger" or "npm:@oh-my-pi/lsp").
    pub canonical_id: String,
    /// Display name.
    pub name: String,
    /// Validation status.
    pub status: ValidationStatus,
    /// Evidence for classification.
    pub evidence: ValidationEvidence,
    /// Aliases — other identifiers that map to this canonical entry.
    pub aliases: Vec<String>,
    /// Source tier (official-pi-mono, community, npm-registry, third-party-github).
    pub source_tier: Option<String>,
    /// Repository URL (if known).
    pub repository_url: Option<String>,
    /// npm package name (if known).
    pub npm_package: Option<String>,
}

/// Output of the full validation + dedup pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub generated_at: String,
    pub task: String,
    pub stats: ValidationStats,
    pub candidates: Vec<ValidatedCandidate>,
}

/// Aggregate statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationStats {
    pub total_input_candidates: usize,
    pub after_dedup: usize,
    pub true_extension: usize,
    pub mention_only: usize,
    pub unknown: usize,
    pub sources_merged: usize,
}

// ────────────────────────────────────────────────────────────────────────────
// Research source input types (deserialized from JSON files)
// ────────────────────────────────────────────────────────────────────────────

/// A candidate from the GitHub code search inventory.
#[derive(Debug, Clone, Deserialize)]
pub struct CodeSearchEntry {
    pub repo: String,
    pub path: String,
    #[serde(default)]
    pub all_paths: Vec<String>,
    #[serde(default)]
    pub is_valid_extension: bool,
    #[serde(default)]
    pub has_api_import: bool,
    #[serde(default)]
    pub has_export_default: bool,
    #[serde(default)]
    pub registrations: Vec<String>,
    #[serde(default)]
    pub file_count: usize,
}

/// Wrapper for code search inventory JSON.
#[derive(Debug, Clone, Deserialize)]
pub struct CodeSearchInventory {
    pub meta: serde_json::Value,
    pub extensions: Vec<CodeSearchEntry>,
}

/// A candidate from the GitHub repo search.
#[derive(Debug, Clone, Deserialize)]
pub struct RepoSearchEntry {
    pub repo: String,
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub stars: Option<u64>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub registrations: Vec<String>,
}

/// Wrapper for repo search summary JSON.
#[derive(Debug, Clone, Deserialize)]
pub struct RepoSearchSummary {
    pub repos: Vec<RepoSearchEntry>,
}

/// A candidate from the npm scan.
#[derive(Debug, Clone, Deserialize)]
pub struct NpmScanEntry {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub has_pi_dep: bool,
}

/// Wrapper for npm scan summary JSON.
#[derive(Debug, Clone, Deserialize)]
pub struct NpmScanSummary {
    pub packages: Vec<NpmScanEntry>,
}

/// A candidate from the curated list sweep.
#[derive(Debug, Clone, Deserialize)]
pub struct CuratedListEntry {
    pub name: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

/// Wrapper for curated list summary JSON.
#[derive(Debug, Clone, Deserialize)]
pub struct CuratedListSummary {
    pub candidates: Vec<CuratedListEntry>,
}

// ────────────────────────────────────────────────────────────────────────────
// Canonical ID generation
// ────────────────────────────────────────────────────────────────────────────

/// Normalize a GitHub repo slug to lowercase `owner/repo`.
#[must_use]
pub fn normalize_github_repo(repo: &str) -> String {
    let repo = repo.trim().to_lowercase();
    // Strip trailing .git
    repo.strip_suffix(".git").unwrap_or(&repo).to_string()
}

/// Extract a canonical ID from a GitHub repository URL.
/// Returns `owner/repo` in lowercase, or None if not a GitHub URL.
#[must_use]
pub fn canonical_id_from_repo_url(url: &str) -> Option<String> {
    match github_repo_candidate_from_url(url)? {
        GitHubRepoCandidate::Repo(r) => Some(format!(
            "{}/{}",
            r.owner.to_lowercase(),
            r.repo.to_lowercase()
        )),
        GitHubRepoCandidate::Slug(_) => None,
    }
}

/// Generate a canonical ID from an npm package name.
/// Prefixed with `npm:` to distinguish from GitHub repos.
#[must_use]
pub fn canonical_id_from_npm(package: &str) -> String {
    format!("npm:{}", package.trim().to_lowercase())
}

/// Generate a canonical ID from a GitHub repo slug (e.g. "owner/repo").
#[must_use]
pub fn canonical_id_from_repo_slug(slug: &str) -> String {
    normalize_github_repo(slug)
}

// ────────────────────────────────────────────────────────────────────────────
// Classification logic
// ────────────────────────────────────────────────────────────────────────────

/// Known Pi extension API registration methods.
const REGISTRATION_METHODS: &[&str] = &[
    "registerTool",
    "registerCommand",
    "registerProvider",
    "registerShortcut",
    "registerFlag",
    "registerMessageRenderer",
];

/// Classify a candidate based on code-level evidence.
///
/// A candidate is `TrueExtension` if it has:
/// - An API import (`@mariozechner/pi-coding-agent`, `@mariozechner/pi-ai`, or `ExtensionAPI`)
/// - AND either `export default` or at least one registration call.
///
/// It is `MentionOnly` if it references Pi but lacks the protocol implementation.
/// Otherwise it is `Unknown`.
#[must_use]
pub fn classify_from_evidence(evidence: &ValidationEvidence) -> ValidationStatus {
    let has_registrations = !evidence.registrations.is_empty();

    if evidence.has_api_import && (evidence.has_export_default || has_registrations) {
        ValidationStatus::TrueExtension
    } else if evidence.has_api_import || has_registrations || evidence.has_export_default {
        // Has some signal but not enough for full classification.
        ValidationStatus::MentionOnly
    } else {
        ValidationStatus::Unknown
    }
}

/// Classify extension source content (raw TypeScript/JavaScript).
#[must_use]
pub fn classify_source_content(content: &str) -> (ValidationStatus, ValidationEvidence) {
    let has_api_import = content.contains("@mariozechner/pi-coding-agent")
        || content.contains("@mariozechner/pi-ai")
        || content.contains("ExtensionAPI");

    let has_export_default = content.contains("export default");

    let mut registrations = Vec::new();
    for method in REGISTRATION_METHODS {
        let pattern = format!("{method}(");
        if content.contains(&pattern) {
            registrations.push((*method).to_string());
        }
    }

    let evidence = ValidationEvidence {
        has_api_import,
        has_export_default,
        registrations: registrations.clone(),
        sources: vec!["source_content".to_string()],
        reason: build_classification_reason(has_api_import, has_export_default, &registrations),
    };

    let status = classify_from_evidence(&evidence);
    (status, evidence)
}

/// Build a human-readable reason string.
fn build_classification_reason(
    has_api_import: bool,
    has_export_default: bool,
    registrations: &[String],
) -> String {
    let mut parts = Vec::new();
    if has_api_import {
        parts.push("Pi API import found");
    }
    if has_export_default {
        parts.push("export default present");
    }
    if !registrations.is_empty() {
        parts.push("registration calls detected");
    }
    if parts.is_empty() {
        "no Pi extension signals detected".to_string()
    } else {
        parts.join("; ")
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Deduplication engine
// ────────────────────────────────────────────────────────────────────────────

/// Intermediate merge record used during dedup.
#[derive(Debug, Clone)]
struct MergeRecord {
    canonical_id: String,
    name: String,
    evidence: ValidationEvidence,
    aliases: Vec<String>,
    source_tier: Option<String>,
    repository_url: Option<String>,
    npm_package: Option<String>,
    /// Whether this candidate has a vendored artifact (already confirmed as extension).
    is_vendored: bool,
}

/// Merge map keyed by canonical ID.
type MergeMap = HashMap<String, MergeRecord>;

/// Merge a new candidate into the merge map.
/// If the canonical ID already exists, merge evidence and aliases.
fn merge_into(map: &mut MergeMap, canonical_id: String, record: MergeRecord) {
    if let Some(existing) = map.get_mut(&canonical_id) {
        // Merge evidence: take the union of signals.
        existing.evidence.has_api_import |= record.evidence.has_api_import;
        existing.evidence.has_export_default |= record.evidence.has_export_default;
        for reg in &record.evidence.registrations {
            if !existing.evidence.registrations.contains(reg) {
                existing.evidence.registrations.push(reg.clone());
            }
        }
        for src in &record.evidence.sources {
            if !existing.evidence.sources.contains(src) {
                existing.evidence.sources.push(src.clone());
            }
        }
        // Merge aliases (add any new ones, including the incoming canonical_id if different).
        for alias in &record.aliases {
            if !existing.aliases.contains(alias) && *alias != existing.canonical_id {
                existing.aliases.push(alias.clone());
            }
        }
        // Merge vendored status.
        existing.is_vendored |= record.is_vendored;
        // Prefer more specific/curated source tiers over generic ones.
        // Candidate pool tiers (official-pi-mono, community, npm-registry) are more
        // accurate than research-derived "third-party-github".
        match (&existing.source_tier, &record.source_tier) {
            (None, _) => existing.source_tier = record.source_tier,
            (Some(existing_tier), Some(new_tier))
                if existing_tier == "third-party-github" && is_curated_tier(new_tier) =>
            {
                existing.source_tier = record.source_tier;
            }
            _ => {}
        }
        if existing.repository_url.is_none() {
            existing.repository_url = record.repository_url;
        }
        if existing.npm_package.is_none() {
            existing.npm_package = record.npm_package;
        }
    } else {
        map.insert(canonical_id, record);
    }
}

/// Returns true if the tier name indicates a curated/hand-verified classification
/// (as opposed to the generic "third-party-github" from research).
fn is_curated_tier(tier: &str) -> bool {
    matches!(
        tier,
        "official-pi-mono" | "community" | "npm-registry" | "agents-mikeastock"
    )
}

/// Link an npm package to a GitHub repo canonical ID (via repository URL).
/// Returns the GitHub canonical ID if the npm package's repo URL matches.
fn npm_to_github_canonical(npm_repo_url: &str) -> Option<String> {
    canonical_id_from_repo_url(npm_repo_url)
}

// ────────────────────────────────────────────────────────────────────────────
// Full pipeline
// ────────────────────────────────────────────────────────────────────────────

/// Configuration for the validation pipeline.
pub struct ValidationConfig {
    /// Task ID for provenance tracking.
    pub task_id: String,
}

/// Run the full validation + dedup pipeline on all research sources.
///
/// Steps:
/// 1. Ingest all sources into a merge map keyed by canonical ID.
/// 2. Merge npm packages with their GitHub repos when repository URLs match.
/// 3. Classify each merged candidate.
/// 4. Produce output report.
#[allow(clippy::too_many_lines)]
pub fn run_validation_pipeline(
    code_search: Option<&CodeSearchInventory>,
    repo_search: Option<&RepoSearchSummary>,
    npm_scan: Option<&NpmScanSummary>,
    curated_list: Option<&CuratedListSummary>,
    existing_pool: Option<&CandidatePool>,
    config: &ValidationConfig,
) -> ValidationReport {
    let mut merge_map: MergeMap = HashMap::new();
    let mut total_input = 0usize;

    // Phase 1: Ingest code search results (highest-signal source).
    if let Some(cs) = code_search {
        for entry in &cs.extensions {
            total_input += 1;
            let canonical_id = canonical_id_from_repo_slug(&entry.repo);
            let record = MergeRecord {
                canonical_id: canonical_id.clone(),
                name: entry
                    .repo
                    .split('/')
                    .next_back()
                    .unwrap_or(&entry.repo)
                    .to_string(),
                evidence: ValidationEvidence {
                    has_api_import: entry.has_api_import,
                    has_export_default: entry.has_export_default,
                    registrations: entry.registrations.clone(),
                    sources: vec!["code_search".to_string()],
                    reason: String::new(), // Will be computed later.
                },
                aliases: Vec::new(),
                source_tier: Some("third-party-github".to_string()),
                repository_url: Some(format!("https://github.com/{}", entry.repo)),
                npm_package: None,
                is_vendored: false,
            };
            merge_into(&mut merge_map, canonical_id, record);
        }
    }

    // Phase 2: Ingest repo search results.
    if let Some(rs) = repo_search {
        for entry in &rs.repos {
            total_input += 1;
            let canonical_id = canonical_id_from_repo_slug(&entry.repo);
            let record = MergeRecord {
                canonical_id: canonical_id.clone(),
                name: entry
                    .repo
                    .split('/')
                    .next_back()
                    .unwrap_or(&entry.repo)
                    .to_string(),
                evidence: ValidationEvidence {
                    has_api_import: true, // Repo search already validated these.
                    has_export_default: true,
                    registrations: entry.registrations.clone(),
                    sources: vec!["repo_search".to_string()],
                    reason: String::new(),
                },
                aliases: Vec::new(),
                source_tier: Some("third-party-github".to_string()),
                repository_url: Some(format!("https://github.com/{}", entry.repo)),
                npm_package: None,
                is_vendored: false,
            };
            merge_into(&mut merge_map, canonical_id, record);
        }
    }

    // Phase 3: Ingest npm scan results.
    // First pass: try to link to existing GitHub repo entries.
    if let Some(ns) = npm_scan {
        for entry in &ns.packages {
            total_input += 1;
            let npm_canonical = canonical_id_from_npm(&entry.name);

            // Try to link to GitHub repo via repository URL.
            let github_canonical = entry
                .repository
                .as_deref()
                .and_then(npm_to_github_canonical);

            let target_id = github_canonical
                .clone()
                .unwrap_or_else(|| npm_canonical.clone());

            let mut aliases = vec![npm_canonical.clone()];
            if let Some(ref gc) = github_canonical {
                if *gc != target_id {
                    aliases.push(gc.clone());
                }
            }
            // Remove duplicates with target_id.
            aliases.retain(|a| *a != target_id);

            let record = MergeRecord {
                canonical_id: target_id.clone(),
                name: entry.name.clone(),
                evidence: ValidationEvidence {
                    has_api_import: entry.has_pi_dep,
                    has_export_default: false, // npm metadata doesn't tell us this.
                    registrations: Vec::new(),
                    sources: vec!["npm_scan".to_string()],
                    reason: String::new(),
                },
                aliases,
                source_tier: Some("npm-registry".to_string()),
                repository_url: entry.repository.as_deref().and_then(|u| {
                    canonical_id_from_repo_url(u).map(|slug| format!("https://github.com/{slug}"))
                }),
                npm_package: Some(entry.name.clone()),
                is_vendored: false,
            };
            merge_into(&mut merge_map, target_id, record);
        }
    }

    // Phase 4: Ingest curated list results.
    if let Some(cl) = curated_list {
        for entry in &cl.candidates {
            total_input += 1;
            let canonical_id = if entry.name.contains('/') {
                canonical_id_from_repo_slug(&entry.name)
            } else {
                entry.name.to_lowercase()
            };

            // Curated list entries: use category to determine signal level.
            // "extensions" and "providers" categories indicate human-curated Pi extensions.
            let cat = entry.category.as_deref().unwrap_or("");
            let is_extension_category =
                cat == "extensions" || cat == "providers" || cat == "skills";
            let record = MergeRecord {
                canonical_id: canonical_id.clone(),
                name: entry
                    .name
                    .split('/')
                    .next_back()
                    .unwrap_or(&entry.name)
                    .to_string(),
                evidence: ValidationEvidence {
                    has_api_import: is_extension_category,
                    has_export_default: is_extension_category,
                    registrations: Vec::new(),
                    sources: vec![format!(
                        "curated_list:{}",
                        entry.source.as_deref().unwrap_or("unknown")
                    )],
                    reason: String::new(),
                },
                aliases: Vec::new(),
                source_tier: entry.category.clone(),
                repository_url: if entry.name.contains('/') {
                    Some(format!("https://github.com/{}", entry.name))
                } else {
                    None
                },
                npm_package: None,
                is_vendored: false,
            };
            merge_into(&mut merge_map, canonical_id, record);
        }
    }

    // Phase 5: Ingest existing candidate pool (enriches with vendor/tier info).
    if let Some(pool) = existing_pool {
        for item in &pool.items {
            total_input += 1;
            let canonical_id = item.id.to_lowercase();

            // For candidate pool items, prefer the item's own ID as canonical.
            // Only use the GitHub URL as canonical if the item ID already matches
            // an owner/repo pattern. This prevents monorepo collapse (e.g., all 60
            // official pi-mono extensions sharing badlogic/pi-mono URL).
            let github_canonical = item
                .repository_url
                .as_deref()
                .and_then(canonical_id_from_repo_url);

            let target_id = if canonical_id.contains('/') {
                // Item ID already looks like owner/repo — try GitHub canonical.
                github_canonical.unwrap_or_else(|| canonical_id.clone())
            } else {
                // Item ID is a package name (e.g., "antigravity-image-gen") — keep it.
                canonical_id.clone()
            };

            let mut aliases = vec![canonical_id.clone()];
            for a in &item.aliases {
                aliases.push(a.to_lowercase());
            }
            aliases.retain(|a| *a != target_id);
            aliases.sort();
            aliases.dedup();

            let record = MergeRecord {
                canonical_id: target_id.clone(),
                name: item.name.clone(),
                evidence: ValidationEvidence {
                    has_api_import: false,
                    has_export_default: false,
                    registrations: Vec::new(),
                    sources: vec![format!("candidate_pool:{}", item.source_tier)],
                    reason: String::new(),
                },
                aliases,
                source_tier: Some(item.source_tier.clone()),
                repository_url: item.repository_url.clone(),
                npm_package: extract_npm_package(item),
                is_vendored: item.status == "vendored",
            };
            merge_into(&mut merge_map, target_id, record);
        }
    }

    // Phase 6: Classify all merged candidates.
    let mut candidates: Vec<ValidatedCandidate> = merge_map
        .into_values()
        .map(|mut rec| {
            // Compute the reason string.
            rec.evidence.reason = build_classification_reason(
                rec.evidence.has_api_import,
                rec.evidence.has_export_default,
                &rec.evidence.registrations,
            );
            let mut status = classify_from_evidence(&rec.evidence);
            // Promote vendored candidates: they were already artifact-validated.
            if rec.is_vendored && status != ValidationStatus::TrueExtension {
                status = ValidationStatus::TrueExtension;
                if !rec.evidence.reason.is_empty() {
                    rec.evidence.reason.push_str("; ");
                }
                rec.evidence
                    .reason
                    .push_str("vendored artifact (pre-validated)");
            }
            ValidatedCandidate {
                canonical_id: rec.canonical_id,
                name: rec.name,
                status,
                evidence: rec.evidence,
                aliases: rec.aliases,
                source_tier: rec.source_tier,
                repository_url: rec.repository_url,
                npm_package: rec.npm_package,
            }
        })
        .collect();

    // Sort by canonical_id for stable output.
    candidates.sort_by(|a, b| a.canonical_id.cmp(&b.canonical_id));

    // Compute stats.
    let true_ext = candidates
        .iter()
        .filter(|c| c.status == ValidationStatus::TrueExtension)
        .count();
    let mention = candidates
        .iter()
        .filter(|c| c.status == ValidationStatus::MentionOnly)
        .count();
    let unknown = candidates
        .iter()
        .filter(|c| c.status == ValidationStatus::Unknown)
        .count();

    // Count sources merged (candidates that have >1 source).
    let sources_merged = candidates
        .iter()
        .filter(|c| c.evidence.sources.len() > 1)
        .count();

    ValidationReport {
        generated_at: chrono_now_iso(),
        task: config.task_id.clone(),
        stats: ValidationStats {
            total_input_candidates: total_input,
            after_dedup: candidates.len(),
            true_extension: true_ext,
            mention_only: mention,
            unknown,
            sources_merged,
        },
        candidates,
    }
}

/// Extract npm package name from a `CandidateItem` if its source is npm.
fn extract_npm_package(item: &CandidateItem) -> Option<String> {
    match &item.source {
        crate::extension_popularity::CandidateSource::Npm { package, .. } => Some(package.clone()),
        _ => None,
    }
}

/// Simple ISO timestamp (avoids pulling in chrono).
pub fn chrono_now_iso() -> String {
    // Use a fixed format for determinism in tests, real timestamp in production.
    let now = std::time::SystemTime::now();
    let secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Simple UTC approximation.
    let days = secs / 86400;
    let rem = secs % 86400;
    let hours = rem / 3600;
    let mins = (rem % 3600) / 60;
    let s = rem % 60;
    // Approximate year/month/day from days since epoch.
    // Good enough for a timestamp string.
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{mins:02}:{s:02}Z")
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    // Simplified civil calendar conversion.
    let mut year = 1970;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }
    let month_days: &[u64] = if is_leap(year) {
        &[31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        &[31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1;
    for &md in month_days {
        if days < md {
            break;
        }
        days -= md;
        month += 1;
    }
    (year, month, days + 1)
}

const fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ====================================================================
    // Canonical ID generation
    // ====================================================================

    #[test]
    fn canonical_id_from_repo_url_standard() {
        assert_eq!(
            canonical_id_from_repo_url("https://github.com/Owner/Repo"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn canonical_id_from_repo_url_git_plus() {
        assert_eq!(
            canonical_id_from_repo_url("git+https://github.com/Can1357/oh-my-pi.git"),
            Some("can1357/oh-my-pi".to_string())
        );
    }

    #[test]
    fn canonical_id_from_repo_url_ssh() {
        assert_eq!(
            canonical_id_from_repo_url("git@github.com:zenobi-us/pi-rose-pine.git"),
            Some("zenobi-us/pi-rose-pine".to_string())
        );
    }

    #[test]
    fn canonical_id_from_repo_url_non_github() {
        assert_eq!(canonical_id_from_repo_url("https://gitlab.com/a/b"), None);
    }

    #[test]
    fn canonical_id_from_npm_scoped() {
        assert_eq!(canonical_id_from_npm("@oh-my-pi/lsp"), "npm:@oh-my-pi/lsp");
    }

    #[test]
    fn canonical_id_from_npm_unscoped() {
        assert_eq!(canonical_id_from_npm("mitsupi"), "npm:mitsupi");
    }

    // ====================================================================
    // Classification
    // ====================================================================

    #[test]
    fn classify_true_extension_import_plus_export() {
        let ev = ValidationEvidence {
            has_api_import: true,
            has_export_default: true,
            registrations: Vec::new(),
            ..Default::default()
        };
        assert_eq!(classify_from_evidence(&ev), ValidationStatus::TrueExtension);
    }

    #[test]
    fn classify_true_extension_import_plus_registration() {
        let ev = ValidationEvidence {
            has_api_import: true,
            has_export_default: false,
            registrations: vec!["registerTool".to_string()],
            ..Default::default()
        };
        assert_eq!(classify_from_evidence(&ev), ValidationStatus::TrueExtension);
    }

    #[test]
    fn classify_mention_only_import_only() {
        let ev = ValidationEvidence {
            has_api_import: true,
            has_export_default: false,
            registrations: Vec::new(),
            ..Default::default()
        };
        assert_eq!(classify_from_evidence(&ev), ValidationStatus::MentionOnly);
    }

    #[test]
    fn classify_mention_only_export_only() {
        let ev = ValidationEvidence {
            has_api_import: false,
            has_export_default: true,
            registrations: Vec::new(),
            ..Default::default()
        };
        assert_eq!(classify_from_evidence(&ev), ValidationStatus::MentionOnly);
    }

    #[test]
    fn classify_mention_only_registration_only() {
        let ev = ValidationEvidence {
            has_api_import: false,
            has_export_default: false,
            registrations: vec!["registerCommand".to_string()],
            ..Default::default()
        };
        assert_eq!(classify_from_evidence(&ev), ValidationStatus::MentionOnly);
    }

    #[test]
    fn classify_unknown_no_signals() {
        let ev = ValidationEvidence::default();
        assert_eq!(classify_from_evidence(&ev), ValidationStatus::Unknown);
    }

    // ====================================================================
    // Source content classification
    // ====================================================================

    #[test]
    fn classify_source_basic_extension() {
        let content = r#"
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
export default function init(api: ExtensionAPI) {
    api.registerTool({ name: "mytool", description: "test", handler: () => {} });
}
"#;
        let (status, ev) = classify_source_content(content);
        assert_eq!(status, ValidationStatus::TrueExtension);
        assert!(ev.has_api_import);
        assert!(ev.has_export_default);
        assert!(ev.registrations.contains(&"registerTool".to_string()));
    }

    #[test]
    fn classify_source_pi_ai_import() {
        let content = r#"
import { ExtensionAPI } from "@mariozechner/pi-ai";
export default (api: ExtensionAPI) => { api.registerCommand({ name: "/test" }); };
"#;
        let (status, _ev) = classify_source_content(content);
        assert_eq!(status, ValidationStatus::TrueExtension);
    }

    #[test]
    fn classify_source_mention_only_readme() {
        let content = "This extension works with @mariozechner/pi-coding-agent to provide...";
        let (status, _ev) = classify_source_content(content);
        assert_eq!(status, ValidationStatus::MentionOnly);
    }

    #[test]
    fn classify_source_no_signals() {
        let content = "function hello() { console.log('world'); }";
        let (status, _ev) = classify_source_content(content);
        assert_eq!(status, ValidationStatus::Unknown);
    }

    // ====================================================================
    // Dedup / merge
    // ====================================================================

    #[test]
    fn merge_same_repo_via_code_search_and_npm() {
        let code_search = CodeSearchInventory {
            meta: serde_json::json!({}),
            extensions: vec![CodeSearchEntry {
                repo: "can1357/oh-my-pi".to_string(),
                path: "packages/lsp/src/index.ts".to_string(),
                all_paths: vec![],
                is_valid_extension: true,
                has_api_import: true,
                has_export_default: true,
                registrations: vec!["registerTool".to_string()],
                file_count: 1,
            }],
        };

        let npm_scan = NpmScanSummary {
            packages: vec![NpmScanEntry {
                name: "@oh-my-pi/lsp".to_string(),
                version: Some("1.3.3710".to_string()),
                description: None,
                repository: Some("git+https://github.com/can1357/oh-my-pi.git".to_string()),
                has_pi_dep: false,
            }],
        };

        let config = ValidationConfig {
            task_id: "test".to_string(),
        };

        let report = run_validation_pipeline(
            Some(&code_search),
            None,
            Some(&npm_scan),
            None,
            None,
            &config,
        );

        // Should merge into one entry, not two.
        let matching: Vec<_> = report
            .candidates
            .iter()
            .filter(|c| c.canonical_id.contains("oh-my-pi"))
            .collect();
        assert_eq!(matching.len(), 1, "should merge repo + npm into one");
        assert_eq!(matching[0].status, ValidationStatus::TrueExtension);
        assert!(
            matching[0]
                .evidence
                .sources
                .contains(&"code_search".to_string())
        );
        assert!(
            matching[0]
                .evidence
                .sources
                .contains(&"npm_scan".to_string())
        );
    }

    #[test]
    fn merge_different_repos_stay_separate() {
        let code_search = CodeSearchInventory {
            meta: serde_json::json!({}),
            extensions: vec![
                CodeSearchEntry {
                    repo: "alice/ext-a".to_string(),
                    path: "index.ts".to_string(),
                    all_paths: vec![],
                    is_valid_extension: true,
                    has_api_import: true,
                    has_export_default: true,
                    registrations: vec![],
                    file_count: 1,
                },
                CodeSearchEntry {
                    repo: "bob/ext-b".to_string(),
                    path: "index.ts".to_string(),
                    all_paths: vec![],
                    is_valid_extension: true,
                    has_api_import: true,
                    has_export_default: true,
                    registrations: vec![],
                    file_count: 1,
                },
            ],
        };

        let config = ValidationConfig {
            task_id: "test".to_string(),
        };

        let report = run_validation_pipeline(Some(&code_search), None, None, None, None, &config);

        assert_eq!(report.candidates.len(), 2);
    }

    #[test]
    fn merge_preserves_aliases() {
        let npm_scan = NpmScanSummary {
            packages: vec![NpmScanEntry {
                name: "@oh-my-pi/lsp".to_string(),
                version: Some("1.0.0".to_string()),
                description: None,
                repository: Some("https://github.com/can1357/oh-my-pi".to_string()),
                has_pi_dep: true,
            }],
        };

        let config = ValidationConfig {
            task_id: "test".to_string(),
        };

        let report = run_validation_pipeline(None, None, Some(&npm_scan), None, None, &config);

        let candidate = report
            .candidates
            .iter()
            .find(|c| c.canonical_id == "can1357/oh-my-pi")
            .expect("should use github canonical");

        assert!(
            candidate.aliases.contains(&"npm:@oh-my-pi/lsp".to_string()),
            "npm name should be alias: {:?}",
            candidate.aliases
        );
    }

    // ====================================================================
    // Pipeline stats
    // ====================================================================

    #[test]
    fn pipeline_stats_correct() {
        let code_search = CodeSearchInventory {
            meta: serde_json::json!({}),
            extensions: vec![
                CodeSearchEntry {
                    repo: "a/ext1".to_string(),
                    path: "index.ts".to_string(),
                    all_paths: vec![],
                    is_valid_extension: true,
                    has_api_import: true,
                    has_export_default: true,
                    registrations: vec![],
                    file_count: 1,
                },
                CodeSearchEntry {
                    repo: "b/ext2".to_string(),
                    path: "index.ts".to_string(),
                    all_paths: vec![],
                    is_valid_extension: true,
                    has_api_import: true,
                    has_export_default: false,
                    registrations: vec![],
                    file_count: 1,
                },
            ],
        };

        let config = ValidationConfig {
            task_id: "test".to_string(),
        };

        let report = run_validation_pipeline(Some(&code_search), None, None, None, None, &config);

        assert_eq!(report.stats.total_input_candidates, 2);
        assert_eq!(report.stats.after_dedup, 2);
        assert_eq!(report.stats.true_extension, 1);
        assert_eq!(report.stats.mention_only, 1);
    }

    // ====================================================================
    // Serialization round-trip
    // ====================================================================

    #[test]
    fn validation_status_serde_round_trip() {
        let statuses = [
            ValidationStatus::TrueExtension,
            ValidationStatus::MentionOnly,
            ValidationStatus::Unknown,
        ];
        for status in &statuses {
            let json = serde_json::to_string(status).unwrap();
            let back: ValidationStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(*status, back);
        }
    }

    #[test]
    fn validated_candidate_serde_round_trip() {
        let c = ValidatedCandidate {
            canonical_id: "owner/repo".to_string(),
            name: "repo".to_string(),
            status: ValidationStatus::TrueExtension,
            evidence: ValidationEvidence {
                has_api_import: true,
                has_export_default: true,
                registrations: vec!["registerTool".to_string()],
                sources: vec!["code_search".to_string()],
                reason: "Pi API import found; export default present".to_string(),
            },
            aliases: vec!["npm:@scope/repo".to_string()],
            source_tier: Some("community".to_string()),
            repository_url: Some("https://github.com/owner/repo".to_string()),
            npm_package: Some("@scope/repo".to_string()),
        };
        let json = serde_json::to_string_pretty(&c).unwrap();
        let back: ValidatedCandidate = serde_json::from_str(&json).unwrap();
        assert_eq!(back.canonical_id, "owner/repo");
        assert_eq!(back.status, ValidationStatus::TrueExtension);
        assert_eq!(back.aliases, vec!["npm:@scope/repo"]);
    }

    // ====================================================================
    // Timestamp helpers
    // ====================================================================

    #[test]
    fn days_to_ymd_epoch() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_known_date() {
        // 2026-01-01 = 20454 days since epoch.
        let (y, m, d) = days_to_ymd(20454);
        assert_eq!(y, 2026);
        assert_eq!(m, 1);
        assert_eq!(d, 1);
    }

    // ====================================================================
    // normalize_github_repo
    // ====================================================================

    #[test]
    fn normalize_lowercases_and_strips_git() {
        assert_eq!(normalize_github_repo("Owner/Repo.git"), "owner/repo");
    }

    #[test]
    fn normalize_trims_whitespace() {
        assert_eq!(normalize_github_repo("  owner/repo  "), "owner/repo");
    }
}
