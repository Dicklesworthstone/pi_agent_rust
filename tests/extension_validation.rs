//! Tests for the extension validation + dedup pipeline (bd-28ov).
//!
//! Coverage:
//! - Canonical ID generation (from URLs, npm, slugs)
//! - Classification logic (true-extension, mention-only, unknown)
//! - Source content classification
//! - Dedup: same extension via repo + npm merges correctly
//! - Dedup: forks/mirrors produce alias mapping
//! - Dedup: different extensions with similar names stay separate
//! - Pipeline stats correctness
//! - Vendored promotion
//! - Golden corpus regression (real data)
//! - E2E pipeline (mixed sources → validated output)
//! - Serde round-trips

use pi::extension_popularity::CandidatePool;
use pi::extension_validation::*;
use std::fs;

// ====================================================================
// Canonical ID generation
// ====================================================================

#[test]
fn canonical_id_from_https_url() {
    assert_eq!(
        canonical_id_from_repo_url("https://github.com/Owner/Repo"),
        Some("owner/repo".to_string())
    );
}

#[test]
fn canonical_id_from_git_plus_url() {
    assert_eq!(
        canonical_id_from_repo_url("git+https://github.com/Can1357/oh-my-pi.git"),
        Some("can1357/oh-my-pi".to_string())
    );
}

#[test]
fn canonical_id_from_ssh_url() {
    assert_eq!(
        canonical_id_from_repo_url("git@github.com:zenobi-us/pi-rose-pine.git"),
        Some("zenobi-us/pi-rose-pine".to_string())
    );
}

#[test]
fn canonical_id_from_non_github_returns_none() {
    assert_eq!(canonical_id_from_repo_url("https://gitlab.com/a/b"), None);
}

#[test]
fn canonical_id_from_empty_returns_none() {
    assert_eq!(canonical_id_from_repo_url(""), None);
}

#[test]
fn canonical_id_npm_scoped() {
    assert_eq!(canonical_id_from_npm("@oh-my-pi/lsp"), "npm:@oh-my-pi/lsp");
}

#[test]
fn canonical_id_npm_unscoped() {
    assert_eq!(canonical_id_from_npm("mitsupi"), "npm:mitsupi");
}

#[test]
fn canonical_id_npm_with_whitespace() {
    assert_eq!(canonical_id_from_npm("  mitsupi  "), "npm:mitsupi");
}

#[test]
fn normalize_github_repo_lowercases() {
    assert_eq!(normalize_github_repo("Owner/Repo.git"), "owner/repo");
}

#[test]
fn normalize_github_repo_trims() {
    assert_eq!(normalize_github_repo("  owner/repo  "), "owner/repo");
}

// ====================================================================
// Classification logic
// ====================================================================

#[test]
fn classify_true_ext_import_plus_export() {
    let ev = ValidationEvidence {
        has_api_import: true,
        has_export_default: true,
        registrations: Vec::new(),
        ..Default::default()
    };
    assert_eq!(classify_from_evidence(&ev), ValidationStatus::TrueExtension);
}

#[test]
fn classify_true_ext_import_plus_registration() {
    let ev = ValidationEvidence {
        has_api_import: true,
        has_export_default: false,
        registrations: vec!["registerTool".to_string()],
        ..Default::default()
    };
    assert_eq!(classify_from_evidence(&ev), ValidationStatus::TrueExtension);
}

#[test]
fn classify_true_ext_import_plus_multiple_registrations() {
    let ev = ValidationEvidence {
        has_api_import: true,
        has_export_default: true,
        registrations: vec![
            "registerTool".to_string(),
            "registerCommand".to_string(),
            "registerShortcut".to_string(),
        ],
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
fn classify_mention_only_export_default_only() {
    let ev = ValidationEvidence {
        has_api_import: false,
        has_export_default: true,
        registrations: Vec::new(),
        ..Default::default()
    };
    assert_eq!(classify_from_evidence(&ev), ValidationStatus::MentionOnly);
}

#[test]
fn classify_mention_only_registration_without_import() {
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
fn source_content_full_extension() {
    let content = r#"
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
export default function init(api: ExtensionAPI) {
    api.registerTool({ name: "mytool", description: "test", handler: () => {} });
    api.registerCommand({ name: "/mycommand" });
}
"#;
    let (status, ev) = classify_source_content(content);
    assert_eq!(status, ValidationStatus::TrueExtension);
    assert!(ev.has_api_import);
    assert!(ev.has_export_default);
    assert!(ev.registrations.contains(&"registerTool".to_string()));
    assert!(ev.registrations.contains(&"registerCommand".to_string()));
}

#[test]
fn source_content_pi_ai_import() {
    let content = r#"
import { ExtensionAPI } from "@mariozechner/pi-ai";
export default (api: ExtensionAPI) => {
    api.registerProvider({ name: "custom" });
};
"#;
    let (status, ev) = classify_source_content(content);
    assert_eq!(status, ValidationStatus::TrueExtension);
    assert!(ev.registrations.contains(&"registerProvider".to_string()));
}

#[test]
fn source_content_extension_api_type_reference() {
    let content = r#"
function setup(api: ExtensionAPI) {
    api.registerFlag({ name: "--verbose" });
}
export default setup;
"#;
    let (status, _) = classify_source_content(content);
    assert_eq!(status, ValidationStatus::TrueExtension);
}

#[test]
fn source_content_mention_only() {
    let content = "This uses @mariozechner/pi-coding-agent API for integration.";
    let (status, _) = classify_source_content(content);
    assert_eq!(status, ValidationStatus::MentionOnly);
}

#[test]
fn source_content_no_signals() {
    let content = "function hello() { console.log('world'); }";
    let (status, _) = classify_source_content(content);
    assert_eq!(status, ValidationStatus::Unknown);
}

#[test]
fn source_content_all_registration_types_detected() {
    let content = r#"
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
export default function(api: ExtensionAPI) {
    api.registerTool({});
    api.registerCommand({});
    api.registerProvider({});
    api.registerShortcut({});
    api.registerFlag({});
    api.registerMessageRenderer({});
}
"#;
    let (_, ev) = classify_source_content(content);
    assert_eq!(ev.registrations.len(), 6);
    assert!(
        ev.registrations
            .contains(&"registerMessageRenderer".to_string())
    );
}

// ====================================================================
// Dedup: same extension via repo + npm merges
// ====================================================================

#[test]
fn dedup_same_extension_repo_plus_npm_merge() {
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

    let matching: Vec<_> = report
        .candidates
        .iter()
        .filter(|c| c.canonical_id.contains("oh-my-pi"))
        .collect();
    assert_eq!(matching.len(), 1, "repo + npm should merge into one entry");
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
    assert!(
        matching[0]
            .aliases
            .contains(&"npm:@oh-my-pi/lsp".to_string()),
        "npm alias preserved: {:?}",
        matching[0].aliases
    );
}

// ====================================================================
// Dedup: forks/mirrors produce alias mapping
// ====================================================================

#[test]
fn dedup_fork_via_curated_and_code_search() {
    let code_search = CodeSearchInventory {
        meta: serde_json::json!({}),
        extensions: vec![CodeSearchEntry {
            repo: "nicobailon/pi-messenger".to_string(),
            path: "src/index.ts".to_string(),
            all_paths: vec![],
            is_valid_extension: true,
            has_api_import: true,
            has_export_default: true,
            registrations: vec!["registerTool".to_string()],
            file_count: 1,
        }],
    };

    let curated = CuratedListSummary {
        candidates: vec![CuratedListEntry {
            name: "nicobailon/pi-messenger".to_string(),
            source: Some("awesome-pi-agent".to_string()),
            category: Some("extensions".to_string()),
            status: Some("new".to_string()),
        }],
    };

    let config = ValidationConfig {
        task_id: "test".to_string(),
    };

    let report = run_validation_pipeline(
        Some(&code_search),
        None,
        None,
        Some(&curated),
        None,
        &config,
    );

    let matching: Vec<_> = report
        .candidates
        .iter()
        .filter(|c| c.canonical_id.contains("pi-messenger"))
        .collect();
    assert_eq!(matching.len(), 1);
    assert!(matching[0].evidence.sources.len() >= 2);
}

// ====================================================================
// Dedup: different extensions with similar names stay separate
// ====================================================================

#[test]
fn dedup_different_extensions_stay_separate() {
    let code_search = CodeSearchInventory {
        meta: serde_json::json!({}),
        extensions: vec![
            CodeSearchEntry {
                repo: "alice/pi-tools".to_string(),
                path: "index.ts".to_string(),
                all_paths: vec![],
                is_valid_extension: true,
                has_api_import: true,
                has_export_default: true,
                registrations: vec!["registerTool".to_string()],
                file_count: 1,
            },
            CodeSearchEntry {
                repo: "bob/pi-tools".to_string(),
                path: "src/index.ts".to_string(),
                all_paths: vec![],
                is_valid_extension: true,
                has_api_import: true,
                has_export_default: true,
                registrations: vec!["registerCommand".to_string()],
                file_count: 1,
            },
        ],
    };

    let config = ValidationConfig {
        task_id: "test".to_string(),
    };

    let report = run_validation_pipeline(Some(&code_search), None, None, None, None, &config);

    assert_eq!(
        report.candidates.len(),
        2,
        "different owners with same repo name should not merge"
    );
    assert!(
        report
            .candidates
            .iter()
            .any(|c| c.canonical_id == "alice/pi-tools")
    );
    assert!(
        report
            .candidates
            .iter()
            .any(|c| c.canonical_id == "bob/pi-tools")
    );
}

// ====================================================================
// Vendored promotion
// ====================================================================

#[test]
fn vendored_candidates_promoted_to_true_extension() {
    // Simulate a candidate pool with a vendored item that has no code-level evidence.
    let pool_json = serde_json::json!({
        "$schema": "pi.ext.candidate_pool.v1",
        "generated_at": "2026-02-06T00:00:00Z",
        "source_inputs": {
            "artifact_provenance": "test",
            "artifact_root": "test",
            "extra_npm_packages": []
        },
        "total_candidates": 1,
        "items": [{
            "id": "my-vendored-ext",
            "name": "my-vendored-ext",
            "source_tier": "community",
            "status": "vendored",
            "license": "MIT",
            "retrieved": "2026-02-06",
            "artifact_path": "tests/ext_conformance/artifacts/my-vendored-ext",
            "checksum": { "sha256": "abc123" },
            "source": { "type": "git", "repo": "https://github.com/test/my-vendored-ext", "path": null },
            "repository_url": "https://github.com/test/my-vendored-ext",
            "popularity": {},
            "aliases": [],
            "notes": null
        }],
        "alias_notes": []
    });

    let pool: CandidatePool = serde_json::from_value(pool_json).unwrap();
    let config = ValidationConfig {
        task_id: "test".to_string(),
    };

    let report = run_validation_pipeline(None, None, None, None, Some(&pool), &config);

    let candidate = report
        .candidates
        .iter()
        .find(|c| c.canonical_id.contains("my-vendored-ext"))
        .expect("should find vendored candidate");

    assert_eq!(
        candidate.status,
        ValidationStatus::TrueExtension,
        "vendored items should be promoted to true-extension"
    );
    assert!(
        candidate.evidence.reason.contains("vendored"),
        "reason should mention vendored: {}",
        candidate.evidence.reason
    );
}

#[test]
fn non_vendored_pool_items_not_promoted() {
    let pool_json = serde_json::json!({
        "$schema": "pi.ext.candidate_pool.v1",
        "generated_at": "2026-02-06T00:00:00Z",
        "source_inputs": {
            "artifact_provenance": "test",
            "artifact_root": "test",
            "extra_npm_packages": []
        },
        "total_candidates": 1,
        "items": [{
            "id": "unvalidated-ext",
            "name": "unvalidated-ext",
            "source_tier": "community",
            "status": "pending",
            "license": "UNKNOWN",
            "retrieved": null,
            "artifact_path": null,
            "checksum": null,
            "source": { "type": "git", "repo": "https://github.com/test/unvalidated-ext", "path": null },
            "repository_url": "https://github.com/test/unvalidated-ext",
            "popularity": {},
            "aliases": [],
            "notes": null
        }],
        "alias_notes": []
    });

    let pool: CandidatePool = serde_json::from_value(pool_json).unwrap();
    let config = ValidationConfig {
        task_id: "test".to_string(),
    };

    let report = run_validation_pipeline(None, None, None, None, Some(&pool), &config);

    let candidate = report
        .candidates
        .iter()
        .find(|c| c.canonical_id.contains("unvalidated-ext"))
        .expect("should find unvalidated candidate");

    assert_ne!(
        candidate.status,
        ValidationStatus::TrueExtension,
        "non-vendored items should NOT be promoted"
    );
}

// ====================================================================
// Pipeline stats correctness
// ====================================================================

#[test]
fn pipeline_stats_are_correct() {
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
                has_api_import: false,
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
    assert_eq!(report.stats.unknown, 1);
}

#[test]
fn pipeline_sources_merged_counts_multi_source() {
    let code_search = CodeSearchInventory {
        meta: serde_json::json!({}),
        extensions: vec![CodeSearchEntry {
            repo: "can1357/oh-my-pi".to_string(),
            path: "index.ts".to_string(),
            all_paths: vec![],
            is_valid_extension: true,
            has_api_import: true,
            has_export_default: true,
            registrations: vec![],
            file_count: 1,
        }],
    };

    let npm_scan = NpmScanSummary {
        packages: vec![NpmScanEntry {
            name: "@oh-my-pi/lsp".to_string(),
            version: None,
            description: None,
            repository: Some("https://github.com/can1357/oh-my-pi".to_string()),
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

    assert!(
        report.stats.sources_merged >= 1,
        "should count at least 1 merged source"
    );
}

// ====================================================================
// Empty pipeline
// ====================================================================

#[test]
fn empty_pipeline_produces_empty_report() {
    let config = ValidationConfig {
        task_id: "test".to_string(),
    };

    let report = run_validation_pipeline(None, None, None, None, None, &config);

    assert_eq!(report.stats.total_input_candidates, 0);
    assert_eq!(report.stats.after_dedup, 0);
    assert!(report.candidates.is_empty());
}

// ====================================================================
// Curated list category handling
// ====================================================================

#[test]
fn curated_extensions_category_classified_as_true() {
    let curated = CuratedListSummary {
        candidates: vec![CuratedListEntry {
            name: "nicobailon/pi-messenger".to_string(),
            source: Some("awesome-pi-agent".to_string()),
            category: Some("extensions".to_string()),
            status: None,
        }],
    };

    let config = ValidationConfig {
        task_id: "test".to_string(),
    };

    let report = run_validation_pipeline(None, None, None, Some(&curated), None, &config);

    let c = &report.candidates[0];
    assert_eq!(c.status, ValidationStatus::TrueExtension);
}

#[test]
fn curated_tools_category_classified_as_unknown() {
    let curated = CuratedListSummary {
        candidates: vec![CuratedListEntry {
            name: "kcosr/codemap".to_string(),
            source: Some("awesome-pi-agent".to_string()),
            category: Some("tools".to_string()),
            status: None,
        }],
    };

    let config = ValidationConfig {
        task_id: "test".to_string(),
    };

    let report = run_validation_pipeline(None, None, None, Some(&curated), None, &config);

    let c = &report.candidates[0];
    assert_eq!(
        c.status,
        ValidationStatus::Unknown,
        "tools category should not get extension signals"
    );
}

#[test]
fn curated_providers_category_classified_as_true() {
    let curated = CuratedListSummary {
        candidates: vec![CuratedListEntry {
            name: "aliou/pi-synthetic".to_string(),
            source: Some("awesome-pi-agent".to_string()),
            category: Some("providers".to_string()),
            status: None,
        }],
    };

    let config = ValidationConfig {
        task_id: "test".to_string(),
    };

    let report = run_validation_pipeline(None, None, None, Some(&curated), None, &config);

    let c = &report.candidates[0];
    assert_eq!(c.status, ValidationStatus::TrueExtension);
}

// ====================================================================
// Golden corpus regression: real data files
// ====================================================================

#[test]
fn golden_corpus_real_data_pipeline() {
    // Only run if the real data files exist.
    let code_search_path = "docs/extension-code-search-inventory.json";
    let npm_scan_path = "docs/extension-npm-scan-summary.json";

    if !std::path::Path::new(code_search_path).exists() {
        eprintln!("Skipping golden corpus test: data files not found");
        return;
    }

    let cs_text = fs::read_to_string(code_search_path).unwrap();
    let cs: CodeSearchInventory = serde_json::from_str(&cs_text).unwrap();

    let ns_text = fs::read_to_string(npm_scan_path).unwrap();
    let ns: NpmScanSummary = serde_json::from_str(&ns_text).unwrap();

    let config = ValidationConfig {
        task_id: "golden-test".to_string(),
    };

    let report = run_validation_pipeline(Some(&cs), None, Some(&ns), None, None, &config);

    // Sanity checks on real data.
    assert!(
        report.stats.after_dedup >= 180,
        "should have at least 180 candidates after dedup, got {}",
        report.stats.after_dedup
    );
    assert!(
        report.stats.true_extension >= 150,
        "should have at least 150 true extensions, got {}",
        report.stats.true_extension
    );
    assert!(
        report.stats.sources_merged >= 5,
        "should have at least 5 merged sources, got {}",
        report.stats.sources_merged
    );
}

#[test]
fn golden_corpus_full_pipeline() {
    // Only run if all data files exist.
    let paths = [
        "docs/extension-code-search-inventory.json",
        "docs/extension-repo-search-summary.json",
        "docs/extension-npm-scan-summary.json",
        "docs/extension-curated-list-summary.json",
        "docs/extension-candidate-pool.json",
    ];

    for p in &paths {
        if !std::path::Path::new(p).exists() {
            eprintln!("Skipping full golden corpus test: {p} not found");
            return;
        }
    }

    let cs: CodeSearchInventory =
        serde_json::from_str(&fs::read_to_string(paths[0]).unwrap()).unwrap();
    let rs: RepoSearchSummary =
        serde_json::from_str(&fs::read_to_string(paths[1]).unwrap()).unwrap();
    let ns: NpmScanSummary = serde_json::from_str(&fs::read_to_string(paths[2]).unwrap()).unwrap();
    let cl: CuratedListSummary =
        serde_json::from_str(&fs::read_to_string(paths[3]).unwrap()).unwrap();
    let pool: CandidatePool = serde_json::from_str(&fs::read_to_string(paths[4]).unwrap()).unwrap();

    let config = ValidationConfig {
        task_id: "golden-full".to_string(),
    };

    let report = run_validation_pipeline(
        Some(&cs),
        Some(&rs),
        Some(&ns),
        Some(&cl),
        Some(&pool),
        &config,
    );

    // Core acceptance criteria: >= 95% classification coverage.
    let classified = report.stats.true_extension + report.stats.mention_only;
    let total = report.stats.after_dedup;
    #[allow(clippy::cast_precision_loss)]
    let coverage_pct = classified as f64 / total as f64 * 100.0;

    assert!(
        coverage_pct >= 95.0,
        "classification coverage should be >= 95%, got {coverage_pct:.1}% ({classified}/{total})"
    );

    // Dedup should reduce the 498 inputs significantly.
    assert!(
        report.stats.after_dedup < report.stats.total_input_candidates,
        "dedup should reduce count: {} -> {}",
        report.stats.total_input_candidates,
        report.stats.after_dedup
    );

    // Should have meaningful merge activity.
    assert!(
        report.stats.sources_merged >= 20,
        "should merge at least 20 cross-source candidates, got {}",
        report.stats.sources_merged
    );

    // Output should be deterministic (sorted by canonical_id).
    let ids: Vec<&str> = report
        .candidates
        .iter()
        .map(|c| c.canonical_id.as_str())
        .collect();
    let mut sorted_ids = ids.clone();
    sorted_ids.sort_unstable();
    assert_eq!(ids, sorted_ids, "output should be sorted by canonical_id");
}

// ====================================================================
// Serde round-trips
// ====================================================================

#[test]
fn validation_status_serde() {
    for status in &[
        ValidationStatus::TrueExtension,
        ValidationStatus::MentionOnly,
        ValidationStatus::Unknown,
    ] {
        let json = serde_json::to_string(status).unwrap();
        let back: ValidationStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(*status, back);
    }
}

#[test]
fn validation_report_serde_round_trip() {
    let report = ValidationReport {
        generated_at: "2026-02-07T00:00:00Z".to_string(),
        task: "test".to_string(),
        stats: ValidationStats {
            total_input_candidates: 10,
            after_dedup: 8,
            true_extension: 5,
            mention_only: 2,
            unknown: 1,
            sources_merged: 3,
        },
        candidates: vec![ValidatedCandidate {
            canonical_id: "owner/repo".to_string(),
            name: "repo".to_string(),
            status: ValidationStatus::TrueExtension,
            evidence: ValidationEvidence {
                has_api_import: true,
                has_export_default: true,
                registrations: vec!["registerTool".to_string()],
                sources: vec!["code_search".to_string()],
                reason: "Pi API import found".to_string(),
            },
            aliases: vec!["npm:@scope/repo".to_string()],
            source_tier: Some("community".to_string()),
            repository_url: Some("https://github.com/owner/repo".to_string()),
            npm_package: Some("@scope/repo".to_string()),
        }],
    };

    let json = serde_json::to_string_pretty(&report).unwrap();
    let back: ValidationReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.stats.after_dedup, 8);
    assert_eq!(back.candidates.len(), 1);
    assert_eq!(back.candidates[0].canonical_id, "owner/repo");
}

// ====================================================================
// Edge case: npm package with no repository URL
// ====================================================================

#[test]
fn npm_without_repo_url_uses_npm_canonical() {
    let npm_scan = NpmScanSummary {
        packages: vec![NpmScanEntry {
            name: "orphan-package".to_string(),
            version: Some("1.0.0".to_string()),
            description: None,
            repository: None,
            has_pi_dep: true,
        }],
    };

    let config = ValidationConfig {
        task_id: "test".to_string(),
    };

    let report = run_validation_pipeline(None, None, Some(&npm_scan), None, None, &config);

    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.candidates[0].canonical_id, "npm:orphan-package");
}

#[test]
fn npm_with_empty_repo_url_uses_npm_canonical() {
    let npm_scan = NpmScanSummary {
        packages: vec![NpmScanEntry {
            name: "pi-extensions".to_string(),
            version: Some("0.1.0".to_string()),
            description: None,
            repository: Some(String::new()),
            has_pi_dep: false,
        }],
    };

    let config = ValidationConfig {
        task_id: "test".to_string(),
    };

    let report = run_validation_pipeline(None, None, Some(&npm_scan), None, None, &config);

    assert_eq!(report.candidates[0].canonical_id, "npm:pi-extensions");
}

// ====================================================================
// Multiple npm packages from same repo merge
// ====================================================================

#[test]
fn multiple_npm_packages_from_same_repo_merge() {
    let npm_scan = NpmScanSummary {
        packages: vec![
            NpmScanEntry {
                name: "@oh-my-pi/lsp".to_string(),
                version: Some("1.0.0".to_string()),
                description: None,
                repository: Some("https://github.com/can1357/oh-my-pi".to_string()),
                has_pi_dep: false,
            },
            NpmScanEntry {
                name: "@oh-my-pi/exa".to_string(),
                version: Some("1.0.0".to_string()),
                description: None,
                repository: Some("https://github.com/can1357/oh-my-pi".to_string()),
                has_pi_dep: false,
            },
        ],
    };

    let config = ValidationConfig {
        task_id: "test".to_string(),
    };

    let report = run_validation_pipeline(None, None, Some(&npm_scan), None, None, &config);

    // Both npm packages point to the same GitHub repo, so they should merge.
    let matching: Vec<_> = report
        .candidates
        .iter()
        .filter(|c| c.canonical_id.contains("oh-my-pi"))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "two npm packages from same repo should merge"
    );
    // Both npm aliases should be preserved.
    let aliases = &matching[0].aliases;
    assert!(aliases.contains(&"npm:@oh-my-pi/lsp".to_string()));
    assert!(aliases.contains(&"npm:@oh-my-pi/exa".to_string()));
}
