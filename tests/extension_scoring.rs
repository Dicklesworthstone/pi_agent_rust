#![forbid(unsafe_code)]

mod common;

use chrono::{DateTime, TimeZone, Utc};
use common::TestHarness;
use pi::extension_scoring::{
    CandidateInput, CompatStatus, Compatibility, Gates, LicenseInfo, MarketplaceSignals, Recency,
    Redistribution, RiskInfo, RiskLevel, Signals, Tags, score_candidates,
};
use std::collections::HashMap;

fn fixed_as_of() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0)
        .single()
        .expect("valid timestamp")
}

fn load_fixture_candidates() -> Vec<CandidateInput> {
    serde_json::from_str(include_str!("fixtures/extension_scoring_candidates.json"))
        .expect("fixture parses")
}

#[test]
fn scoring_examples_match_rubric() {
    let as_of = fixed_as_of();
    let report = score_candidates(&load_fixture_candidates(), as_of, as_of, 10);
    let by_id: HashMap<&str, &pi::extension_scoring::ScoredCandidate> = report
        .items
        .iter()
        .map(|item| (item.id.as_str(), item))
        .collect();

    let openclaw = by_id
        .get("openclaw-featured-tool")
        .expect("openclaw candidate");
    assert_eq!(openclaw.score.final_total, 71);
    assert_eq!(openclaw.tier, "tier-1");
    assert_eq!(openclaw.score.popularity, 17);
    assert_eq!(openclaw.score.adoption, 12);
    assert_eq!(openclaw.score.coverage, 17);
    assert_eq!(openclaw.score.activity, 15);
    assert_eq!(openclaw.score.compatibility, 15);
    assert_eq!(openclaw.score.risk_penalty, 5);
    assert_eq!(
        openclaw.score.components.popularity.marketplace_visibility,
        6
    );
    assert_eq!(openclaw.score.components.adoption.marketplace_installs, 5);

    let niche = by_id.get("niche-github-script").expect("niche candidate");
    assert_eq!(niche.score.final_total, 26);
    assert_eq!(niche.tier, "excluded");

    let official = by_id
        .get("official-pi-mono-example")
        .expect("official candidate");
    assert_eq!(official.score.final_total, 69);
    assert_eq!(official.tier, "tier-0");
}

#[test]
fn missing_metrics_are_reported() {
    let as_of = fixed_as_of();
    let mut candidate = CandidateInput {
        id: "missing-metrics".to_string(),
        name: None,
        source_tier: None,
        signals: Signals::default(),
        tags: Tags::default(),
        recency: Recency::default(),
        compat: Compatibility {
            status: Some(CompatStatus::Blocked),
            ..Default::default()
        },
        license: LicenseInfo {
            spdx: None,
            redistribution: Some(Redistribution::Ok),
            notes: None,
        },
        gates: Gates {
            provenance_pinned: Some(true),
            deterministic: Some(true),
        },
        risk: RiskInfo {
            level: Some(RiskLevel::Low),
            penalty: None,
            flags: Vec::new(),
        },
        manual_override: None,
    };
    candidate.signals.references = vec![];

    let report = score_candidates(&[candidate], as_of, as_of, 5);
    let missing = &report.items[0].missing_signals;
    assert!(missing.contains(&"signals.github_stars".to_string()));
    assert!(missing.contains(&"signals.github_forks".to_string()));
    assert!(missing.contains(&"signals.npm_downloads_month".to_string()));
    assert!(missing.contains(&"signals.marketplace.rank".to_string()));
    assert!(missing.contains(&"signals.marketplace.featured".to_string()));
    assert!(missing.contains(&"signals.marketplace.installs_month".to_string()));
    assert!(missing.contains(&"recency.updated_at".to_string()));
}

#[test]
fn tie_breaker_prefers_higher_coverage() {
    let as_of = fixed_as_of();
    let candidate_a = CandidateInput {
        id: "coverage-wins".to_string(),
        name: None,
        source_tier: None,
        signals: Signals {
            github_stars: Some(5_000),
            ..Default::default()
        },
        tags: Tags {
            runtime: Some("pkg-with-deps".to_string()),
            interaction: vec!["ui_integration".to_string(), "event_hook".to_string()],
            capabilities: Vec::new(),
        },
        recency: Recency::default(),
        compat: Compatibility {
            status: Some(CompatStatus::Blocked),
            ..Default::default()
        },
        license: LicenseInfo {
            spdx: None,
            redistribution: Some(Redistribution::Ok),
            notes: None,
        },
        gates: Gates {
            provenance_pinned: Some(true),
            deterministic: Some(true),
        },
        risk: RiskInfo::default(),
        manual_override: None,
    };

    let candidate_b = CandidateInput {
        id: "popularity-ties".to_string(),
        name: None,
        source_tier: None,
        signals: Signals {
            github_stars: Some(1_000),
            references: vec![
                "r1".to_string(),
                "r2".to_string(),
                "r3".to_string(),
                "r4".to_string(),
                "r5".to_string(),
            ],
            marketplace: Some(MarketplaceSignals {
                rank: Some(50),
                installs_month: None,
                featured: Some(false),
            }),
            ..Default::default()
        },
        tags: Tags {
            runtime: Some("legacy-js".to_string()),
            interaction: vec!["tool_only".to_string()],
            capabilities: vec!["exec".to_string()],
        },
        recency: Recency::default(),
        compat: Compatibility {
            status: Some(CompatStatus::Blocked),
            ..Default::default()
        },
        license: LicenseInfo {
            spdx: None,
            redistribution: Some(Redistribution::Ok),
            notes: None,
        },
        gates: Gates {
            provenance_pinned: Some(true),
            deterministic: Some(true),
        },
        risk: RiskInfo::default(),
        manual_override: None,
    };

    let report = score_candidates(&[candidate_b, candidate_a], as_of, as_of, 10);
    assert_eq!(report.items[0].id, "coverage-wins");
    assert_eq!(report.items[1].id, "popularity-ties");
    assert_eq!(
        report.items[0].score.final_total,
        report.items[1].score.final_total
    );
    assert!(
        report.items[0].score.coverage > report.items[1].score.coverage,
        "coverage tie-breaker should rank higher coverage first"
    );
}

#[test]
fn e2e_scoring_fixture_outputs_artifacts() {
    let harness = TestHarness::new("e2e_extension_scoring_fixture");
    let candidates = load_fixture_candidates();
    let as_of = fixed_as_of();

    harness
        .log()
        .info_ctx("scoring", "Loaded fixture candidates", |ctx| {
            ctx.push(("count".into(), candidates.len().to_string()));
            ctx.push(("as_of".into(), as_of.to_rfc3339()));
        });

    let report = score_candidates(&candidates, as_of, as_of, 5);
    let report_path = harness.temp_path("extension_scoring_report.json");
    std::fs::write(
        &report_path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&report).expect("serialize report")
        ),
    )
    .expect("write report");
    harness.record_artifact("scoring_report", &report_path);

    let summary_path = harness.temp_path("extension_scoring_summary.json");
    std::fs::write(
        &summary_path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&report.summary).expect("serialize summary")
        ),
    )
    .expect("write summary");
    harness.record_artifact("scoring_summary", &summary_path);

    let top_ids = report
        .summary
        .top_overall
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<Vec<_>>()
        .join(", ");
    harness
        .log()
        .info("scoring", format!("Top overall: {top_ids}"));

    let logs_path = harness.temp_path("test_logs.jsonl");
    if let Err(e) = harness.write_jsonl_logs_normalized(&logs_path) {
        harness
            .log()
            .warn("jsonl", format!("Failed to write JSONL logs: {e}"));
    } else {
        harness.record_artifact("jsonl_logs", &logs_path);
    }

    let index_path = harness.temp_path("artifact_index.jsonl");
    if let Err(e) = harness.write_artifact_index_jsonl_normalized(&index_path) {
        harness
            .log()
            .warn("jsonl", format!("Failed to write artifact index: {e}"));
    } else {
        harness.record_artifact("artifact_index", &index_path);
    }
}
