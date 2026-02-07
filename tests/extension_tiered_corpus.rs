//! Integration tests for the tiered corpus selection pipeline.
//!
//! Tests the end-to-end flow: validated candidates + license report + pool → scored + tiered corpus.

use pi::extension_license::{ScreeningInput, VerdictStatus, screen_extensions};
use pi::extension_scoring::{
    CandidateInput, CompatStatus, Compatibility, Gates, LicenseInfo, MarketplaceSignals, Recency,
    Redistribution, RiskInfo, Signals, Tags, score_candidates,
};

use chrono::Utc;

/// Helper: build a minimal `CandidateInput` with common defaults.
fn make_input(id: &str) -> CandidateInput {
    CandidateInput {
        id: id.into(),
        name: Some(id.into()),
        source_tier: Some("community".into()),
        signals: Signals::default(),
        tags: Tags::default(),
        recency: Recency::default(),
        compat: Compatibility {
            status: Some(CompatStatus::Unmodified),
            ..Compatibility::default()
        },
        license: LicenseInfo {
            spdx: Some("MIT".into()),
            redistribution: Some(Redistribution::Ok),
            notes: None,
        },
        gates: Gates {
            provenance_pinned: Some(true),
            deterministic: Some(true),
        },
        risk: RiskInfo::default(),
        manual_override: None,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Scoring integration tests
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn tier0_for_official_extensions() {
    let mut input = make_input("official-ext");
    input.source_tier = Some("official-pi-mono".into());
    input.signals.pi_mono_example = Some(true);
    input.signals.official_listing = Some(true);
    input.tags.runtime = Some("legacy-js".into());
    input.tags.interaction = vec!["tool_only".into()];
    input.tags.capabilities = vec!["exec".into()];
    input.recency.updated_at = Some("2026-01-01T00:00:00Z".into());

    let report = score_candidates(&[input], Utc::now(), Utc::now(), 10);
    assert_eq!(report.items.len(), 1);
    assert_eq!(report.items[0].tier, "tier-0");
}

#[test]
fn tier1_for_high_score_passing_gates() {
    let mut input = make_input("popular-ext");
    input.signals = Signals {
        github_stars: Some(5000),
        github_forks: Some(500),
        npm_downloads_month: Some(50_000),
        references: vec!["blog".into(), "docs".into(), "tutorial".into()],
        marketplace: Some(MarketplaceSignals {
            rank: Some(5),
            installs_month: Some(10_000),
            featured: Some(true),
        }),
        ..Signals::default()
    };
    input.tags = Tags {
        runtime: Some("pkg-with-deps".into()),
        interaction: vec!["provider".into(), "ui_integration".into(), "event_hook".into()],
        capabilities: vec!["exec".into(), "http".into(), "ui".into()],
    };
    input.recency.updated_at = Some("2026-01-15T00:00:00Z".into());

    let report = score_candidates(&[input], Utc::now(), Utc::now(), 10);
    assert_eq!(report.items.len(), 1);
    assert_eq!(report.items[0].tier, "tier-1");
    assert!(report.items[0].score.final_total >= 70);
}

#[test]
fn excluded_for_gate_failure() {
    let mut input = make_input("no-provenance");
    input.source_tier = Some("third-party-github".into());
    input.license.spdx = None;
    input.license.redistribution = Some(Redistribution::Unknown);
    input.gates.provenance_pinned = Some(false);

    let report = score_candidates(&[input], Utc::now(), Utc::now(), 10);
    assert_eq!(report.items[0].tier, "excluded");
}

#[test]
fn tier2_for_moderate_score_passing_gates() {
    let mut input = make_input("moderate-ext");
    input.signals.github_stars = Some(500);
    input.signals.npm_downloads_month = Some(2000);
    input.tags = Tags {
        runtime: Some("pkg-with-deps".into()),
        interaction: vec!["provider".into(), "slash_command".into(), "event_hook".into()],
        capabilities: vec!["http".into(), "session".into(), "exec".into()],
    };
    input.recency.updated_at = Some("2026-01-01T00:00:00Z".into());

    let report = score_candidates(&[input], Utc::now(), Utc::now(), 10);
    let score = report.items[0].score.final_total;
    assert!(
        score >= 50 && score < 70,
        "Expected tier-2 score range (50-69), got {score}"
    );
    assert_eq!(report.items[0].tier, "tier-2");
}

// ────────────────────────────────────────────────────────────────────────────
// Coverage verification tests
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn mixed_tiers_across_extension_types() {
    let mut official = make_input("official-tool");
    official.source_tier = Some("official-pi-mono".into());
    official.signals.pi_mono_example = Some(true);
    official.signals.official_listing = Some(true);
    official.tags.runtime = Some("legacy-js".into());
    official.tags.interaction = vec!["tool_only".into()];
    official.tags.capabilities = vec!["exec".into()];
    official.recency.updated_at = Some("2026-01-01T00:00:00Z".into());

    let mut provider = make_input("community-provider");
    provider.signals.github_stars = Some(100);
    provider.signals.npm_downloads_month = Some(5000);
    provider.tags = Tags {
        runtime: Some("provider-ext".into()),
        interaction: vec!["provider".into()],
        capabilities: vec!["http".into()],
    };
    provider.recency.updated_at = Some("2026-01-01T00:00:00Z".into());

    let mut npm_cmd = make_input("npm-command");
    npm_cmd.source_tier = Some("npm-registry".into());
    npm_cmd.signals.npm_downloads_month = Some(2000);
    npm_cmd.tags = Tags {
        runtime: Some("pkg-with-deps".into()),
        interaction: vec!["slash_command".into()],
        capabilities: vec!["session".into()],
    };
    npm_cmd.recency.updated_at = Some("2025-06-01T00:00:00Z".into());

    let report = score_candidates(&[official, provider, npm_cmd], Utc::now(), Utc::now(), 10);
    assert_eq!(report.items.len(), 3);

    let off = report
        .items
        .iter()
        .find(|i| i.id == "official-tool")
        .unwrap();
    assert_eq!(off.tier, "tier-0");

    // Verify histogram has entries.
    assert!(report.summary.histogram.iter().any(|b| b.count > 0));
}

// ────────────────────────────────────────────────────────────────────────────
// License + scoring integration
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn license_verdict_affects_gate() {
    let mut input = make_input("copyleft-ext");
    input.signals.github_stars = Some(500);
    input.tags = Tags {
        runtime: Some("legacy-js".into()),
        interaction: vec!["tool_only".into()],
        ..Tags::default()
    };
    input.recency.updated_at = Some("2026-01-01T00:00:00Z".into());
    input.license.spdx = Some("GPL-3.0".into());
    input.license.redistribution = Some(Redistribution::Restricted);

    let report = score_candidates(&[input], Utc::now(), Utc::now(), 10);
    // Restricted license → license_ok = true (gate passes).
    assert!(report.items[0].gates.license_ok);
}

#[test]
fn screening_report_integrates_with_scoring() {
    let screening_inputs = vec![
        ScreeningInput {
            canonical_id: "ext-a".into(),
            known_license: Some("MIT".into()),
            source_tier: Some("community".into()),
        },
        ScreeningInput {
            canonical_id: "ext-b".into(),
            known_license: None,
            source_tier: Some("third-party-github".into()),
        },
    ];
    let screening = screen_extensions(&screening_inputs, "test");

    let a = screening
        .verdicts
        .iter()
        .find(|v| v.canonical_id == "ext-a")
        .unwrap();
    assert_eq!(a.verdict, VerdictStatus::Pass);

    let b = screening
        .verdicts
        .iter()
        .find(|v| v.canonical_id == "ext-b")
        .unwrap();
    assert_eq!(b.verdict, VerdictStatus::NeedsReview);
}

// ────────────────────────────────────────────────────────────────────────────
// Golden corpus test
// ────────────────────────────────────────────────────────────────────────────

#[test]
#[allow(clippy::cast_possible_truncation)]
fn golden_tiered_corpus() {
    let corpus_path = "docs/extension-tiered-corpus.json";
    if !std::path::Path::new(corpus_path).exists() {
        eprintln!("Skipping golden test: tiered corpus not found");
        return;
    }

    let text = std::fs::read_to_string(corpus_path).unwrap();
    let report: pi::extension_scoring::ScoringReport = serde_json::from_str(&text).unwrap();

    // Basic sanity.
    assert!(
        report.items.len() >= 300,
        "Expected >=300 items, got {}",
        report.items.len()
    );

    // Tier distribution.
    let tier0 = report.items.iter().filter(|i| i.tier == "tier-0").count();
    let tier1 = report.items.iter().filter(|i| i.tier == "tier-1").count();
    let tier2 = report.items.iter().filter(|i| i.tier == "tier-2").count();
    let excluded = report
        .items
        .iter()
        .filter(|i| i.tier == "excluded")
        .count();

    assert!(tier0 >= 50, "Expected >=50 tier-0, got {tier0}");
    assert_eq!(
        tier0 + tier1 + tier2 + excluded,
        report.items.len(),
        "Tier counts don't sum"
    );

    // All items should have non-empty id.
    for item in &report.items {
        assert!(!item.id.is_empty(), "Empty id found");
    }

    // Items should be ranked sequentially.
    for (i, item) in report.items.iter().enumerate() {
        assert_eq!(
            item.rank,
            (i + 1) as u32,
            "Item {} has rank {} but expected {}",
            item.id,
            item.rank,
            i + 1
        );
    }

    // Scores should be monotonically non-increasing.
    for w in report.items.windows(2) {
        assert!(
            w[0].score.final_total >= w[1].score.final_total,
            "Items not sorted by score: {} ({}) > {} ({})",
            w[0].id,
            w[0].score.final_total,
            w[1].id,
            w[1].score.final_total
        );
    }

    eprintln!(
        "Golden corpus: {} items, T0={tier0} T1={tier1} T2={tier2} Excl={excluded}",
        report.items.len()
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Registration type to interaction mapping
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn registrations_affect_coverage_score() {
    let mut rich = make_input("rich");
    rich.tags = Tags {
        runtime: Some("pkg-with-deps".into()),
        interaction: vec![
            "provider".into(),
            "ui_integration".into(),
            "event_hook".into(),
            "slash_command".into(),
        ],
        capabilities: vec![
            "exec".into(),
            "http".into(),
            "ui".into(),
            "session".into(),
        ],
    };
    rich.recency.updated_at = Some("2026-01-01T00:00:00Z".into());

    let mut sparse = make_input("sparse");
    sparse.tags = Tags {
        runtime: Some("legacy-js".into()),
        interaction: vec!["tool_only".into()],
        capabilities: vec![],
    };
    sparse.recency.updated_at = Some("2026-01-01T00:00:00Z".into());

    let report = score_candidates(&[rich, sparse], Utc::now(), Utc::now(), 10);
    let rich_score = report
        .items
        .iter()
        .find(|i| i.id == "rich")
        .unwrap()
        .score
        .coverage;
    let sparse_score = report
        .items
        .iter()
        .find(|i| i.id == "sparse")
        .unwrap()
        .score
        .coverage;
    assert!(
        rich_score > sparse_score,
        "Rich extension ({rich_score}) should score higher coverage than sparse ({sparse_score})"
    );
}

#[test]
fn summary_has_histogram_and_top_entries() {
    let inputs: Vec<CandidateInput> = (0..5)
        .map(|i| {
            let mut inp = make_input(&format!("ext-{i}"));
            inp.source_tier = Some("official-pi-mono".into());
            inp.signals.pi_mono_example = Some(true);
            inp.signals.official_listing = Some(true);
            inp.tags.runtime = Some("legacy-js".into());
            inp.tags.interaction = vec!["tool_only".into()];
            inp.recency.updated_at = Some("2026-01-01T00:00:00Z".into());
            inp
        })
        .collect();

    let report = score_candidates(&inputs, Utc::now(), Utc::now(), 3);

    // Histogram should have 11 buckets.
    assert_eq!(report.summary.histogram.len(), 11);

    // Top overall should have at most top_n entries.
    assert!(report.summary.top_overall.len() <= 3);
}

// ────────────────────────────────────────────────────────────────────────────
// Edge cases
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn empty_input_produces_empty_report() {
    let report = score_candidates(&[], Utc::now(), Utc::now(), 10);
    assert!(report.items.is_empty());
    assert!(report.summary.top_overall.is_empty());
}

#[test]
fn single_extension_gets_rank_1() {
    let input = make_input("solo");
    let report = score_candidates(&[input], Utc::now(), Utc::now(), 10);
    assert_eq!(report.items.len(), 1);
    assert_eq!(report.items[0].rank, 1);
}
