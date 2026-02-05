use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoringReport {
    pub schema: String,
    pub generated_at: String,
    pub as_of: String,
    pub summary: ScoringSummary,
    pub items: Vec<ScoredCandidate>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoringSummary {
    pub histogram: Vec<ScoreHistogramBucket>,
    pub top_overall: Vec<RankedEntry>,
    pub top_by_source_tier: BTreeMap<String, Vec<RankedEntry>>,
    pub manual_overrides: Vec<ManualOverrideEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoreHistogramBucket {
    pub range: String,
    pub count: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RankedEntry {
    pub id: String,
    pub score: u32,
    pub tier: String,
    pub rank: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManualOverrideEntry {
    pub id: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateInput {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub source_tier: Option<String>,
    #[serde(default)]
    pub signals: Signals,
    #[serde(default)]
    pub tags: Tags,
    #[serde(default)]
    pub recency: Recency,
    #[serde(default)]
    pub compat: Compatibility,
    #[serde(default)]
    pub license: LicenseInfo,
    #[serde(default)]
    pub gates: Gates,
    #[serde(default)]
    pub risk: RiskInfo,
    #[serde(default)]
    pub manual_override: Option<ManualOverride>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Signals {
    #[serde(default)]
    pub official_listing: Option<bool>,
    #[serde(default)]
    pub pi_mono_example: Option<bool>,
    #[serde(default)]
    pub badlogic_gist: Option<bool>,
    #[serde(default)]
    pub github_stars: Option<u64>,
    #[serde(default)]
    pub github_forks: Option<u64>,
    #[serde(default)]
    pub npm_downloads_month: Option<u64>,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub marketplace: Option<MarketplaceSignals>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceSignals {
    #[serde(default)]
    pub rank: Option<u32>,
    #[serde(default)]
    pub installs_month: Option<u64>,
    #[serde(default)]
    pub featured: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tags {
    #[serde(default)]
    pub runtime: Option<String>,
    #[serde(default)]
    pub interaction: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Recency {
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Compatibility {
    #[serde(default)]
    pub status: Option<CompatStatus>,
    #[serde(default)]
    pub blocked_reasons: Vec<String>,
    #[serde(default)]
    pub required_shims: Vec<String>,
    #[serde(default)]
    pub adjustment: Option<i8>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatStatus {
    Unmodified,
    RequiresShims,
    RuntimeGap,
    Blocked,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LicenseInfo {
    #[serde(default)]
    pub spdx: Option<String>,
    #[serde(default)]
    pub redistribution: Option<Redistribution>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Redistribution {
    Ok,
    Restricted,
    Exclude,
    Unknown,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Gates {
    #[serde(default)]
    pub provenance_pinned: Option<bool>,
    #[serde(default)]
    pub deterministic: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskInfo {
    #[serde(default)]
    pub level: Option<RiskLevel>,
    #[serde(default)]
    pub penalty: Option<u8>,
    #[serde(default)]
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Moderate,
    High,
    Critical,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManualOverride {
    pub reason: String,
    #[serde(default)]
    pub tier: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoredCandidate {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_tier: Option<String>,
    pub score: ScoreBreakdown,
    pub tier: String,
    pub rank: u32,
    pub gates: GateStatus,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub missing_signals: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manual_override: Option<ManualOverride>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoreBreakdown {
    pub popularity: u32,
    pub adoption: u32,
    pub coverage: u32,
    pub activity: u32,
    pub compatibility: u32,
    pub risk_penalty: u32,
    pub base_total: u32,
    pub final_total: u32,
    pub components: ScoreComponents,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScoreComponents {
    pub popularity: PopularityComponents,
    pub adoption: AdoptionComponents,
    pub coverage: CoverageComponents,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PopularityComponents {
    pub official_visibility: u32,
    pub github_stars: u32,
    pub marketplace_visibility: u32,
    pub references: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AdoptionComponents {
    pub npm_downloads: u32,
    pub marketplace_installs: u32,
    pub forks: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CoverageComponents {
    pub runtime_tier: u32,
    pub interaction: u32,
    pub hostcalls: u32,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GateStatus {
    pub provenance_pinned: bool,
    pub license_ok: bool,
    pub deterministic: bool,
    pub unmodified: bool,
    pub passes: bool,
}

pub fn score_candidates(
    inputs: &[CandidateInput],
    as_of: DateTime<Utc>,
    generated_at: DateTime<Utc>,
    top_n: usize,
) -> ScoringReport {
    let mut scored = inputs
        .iter()
        .map(|candidate| score_candidate(candidate, as_of))
        .collect::<Vec<_>>();

    scored.sort_by(|left, right| compare_scored(right, left));
    for (idx, item) in scored.iter_mut().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        {
            item.rank = (idx + 1) as u32;
        }
    }

    let summary = build_summary(&scored, top_n);

    ScoringReport {
        schema: "pi.ext.scoring.v1".to_string(),
        generated_at: generated_at.to_rfc3339(),
        as_of: as_of.to_rfc3339(),
        summary,
        items: scored,
    }
}

fn compare_scored(left: &ScoredCandidate, right: &ScoredCandidate) -> std::cmp::Ordering {
    left.score
        .final_total
        .cmp(&right.score.final_total)
        .then_with(|| left.score.coverage.cmp(&right.score.coverage))
        .then_with(|| left.score.popularity.cmp(&right.score.popularity))
        .then_with(|| left.score.activity.cmp(&right.score.activity))
        .then_with(|| left.id.cmp(&right.id))
}

fn score_candidate(candidate: &CandidateInput, as_of: DateTime<Utc>) -> ScoredCandidate {
    let mut missing = BTreeSet::new();
    let (popularity, popularity_components) = score_popularity(&candidate.signals, &mut missing);
    let (adoption, adoption_components) = score_adoption(&candidate.signals, &mut missing);
    let (coverage, coverage_components) = score_coverage(&candidate.tags);
    let activity = score_activity(&candidate.recency, as_of, &mut missing);
    let compatibility = score_compatibility(&candidate.compat);
    let risk_penalty = score_risk(&candidate.risk);
    let base_total = popularity + adoption + coverage + activity + compatibility;
    let final_total = base_total.saturating_sub(risk_penalty);
    let gates = compute_gates(candidate);
    let tier = compute_tier(candidate, &gates, final_total);
    let missing_signals = missing.into_iter().collect::<Vec<_>>();
    let components = ScoreComponents {
        popularity: popularity_components,
        adoption: adoption_components,
        coverage: coverage_components,
    };
    let score = ScoreBreakdown {
        popularity,
        adoption,
        coverage,
        activity,
        compatibility,
        risk_penalty,
        base_total,
        final_total,
        components,
    };
    let manual_override = candidate.manual_override.clone();
    ScoredCandidate {
        id: candidate.id.clone(),
        name: candidate.name.clone(),
        source_tier: candidate.source_tier.clone(),
        score,
        tier,
        rank: 0,
        gates,
        missing_signals,
        manual_override,
    }
}

fn score_popularity(
    signals: &Signals,
    missing: &mut BTreeSet<String>,
) -> (u32, PopularityComponents) {
    let official_visibility = score_official_visibility(signals, missing);
    let github_stars = score_github_stars(signals, missing);
    let marketplace_visibility = score_marketplace_visibility(signals, missing);
    let references = score_references(signals);
    let total = (official_visibility + github_stars + marketplace_visibility + references).min(30);
    (
        total,
        PopularityComponents {
            official_visibility,
            github_stars,
            marketplace_visibility,
            references,
        },
    )
}

fn score_adoption(signals: &Signals, missing: &mut BTreeSet<String>) -> (u32, AdoptionComponents) {
    let npm_downloads = score_npm_downloads(signals, missing);
    let marketplace_installs = score_marketplace_installs(signals, missing);
    let forks = score_forks(signals, missing);
    let total = (npm_downloads + marketplace_installs + forks).min(15);
    (
        total,
        AdoptionComponents {
            npm_downloads,
            marketplace_installs,
            forks,
        },
    )
}

fn score_coverage(tags: &Tags) -> (u32, CoverageComponents) {
    let runtime_tier = score_runtime_tier(tags);
    let interaction = score_interaction(tags);
    let hostcalls = score_hostcalls(tags);
    let total = (runtime_tier + interaction + hostcalls).min(20);
    (
        total,
        CoverageComponents {
            runtime_tier,
            interaction,
            hostcalls,
        },
    )
}

fn score_official_visibility(signals: &Signals, missing: &mut BTreeSet<String>) -> u32 {
    let listing = signals.official_listing;
    let example = signals.pi_mono_example;
    let gist = signals.badlogic_gist;
    if listing.is_none() && example.is_none() && gist.is_none() {
        missing.insert("signals.official_visibility".to_string());
    }
    if listing.unwrap_or(false) {
        10
    } else if example.unwrap_or(false) {
        8
    } else if gist.unwrap_or(false) {
        6
    } else {
        0
    }
}

fn score_github_stars(signals: &Signals, missing: &mut BTreeSet<String>) -> u32 {
    let Some(stars) = signals.github_stars else {
        missing.insert("signals.github_stars".to_string());
        return 0;
    };
    match stars {
        s if s >= 5_000 => 10,
        s if s >= 2_000 => 9,
        s if s >= 1_000 => 8,
        s if s >= 500 => 6,
        s if s >= 200 => 4,
        s if s >= 50 => 2,
        _ => 0,
    }
}

fn score_marketplace_visibility(signals: &Signals, missing: &mut BTreeSet<String>) -> u32 {
    let Some(marketplace) = signals.marketplace.as_ref() else {
        missing.insert("signals.marketplace.rank".to_string());
        missing.insert("signals.marketplace.featured".to_string());
        return 0;
    };
    let rank_points = match marketplace.rank {
        Some(rank) if rank <= 10 => 6,
        Some(rank) if rank <= 50 => 4,
        Some(rank) if rank <= 100 => 2,
        Some(_) => 0,
        None => {
            missing.insert("signals.marketplace.rank".to_string());
            0
        }
    };
    let featured_points = if marketplace.featured.unwrap_or(false) {
        2
    } else {
        0
    };
    (rank_points + featured_points).min(6)
}

fn score_references(signals: &Signals) -> u32 {
    let unique = signals
        .references
        .iter()
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .collect::<BTreeSet<_>>()
        .len();
    match unique {
        n if n >= 10 => 4,
        n if n >= 5 => 3,
        n if n >= 2 => 2,
        _ => 0,
    }
}

fn score_npm_downloads(signals: &Signals, missing: &mut BTreeSet<String>) -> u32 {
    let Some(downloads) = signals.npm_downloads_month else {
        missing.insert("signals.npm_downloads_month".to_string());
        return 0;
    };
    match downloads {
        d if d >= 50_000 => 8,
        d if d >= 10_000 => 6,
        d if d >= 2_000 => 4,
        d if d >= 500 => 2,
        _ => 0,
    }
}

fn score_marketplace_installs(signals: &Signals, missing: &mut BTreeSet<String>) -> u32 {
    let Some(marketplace) = signals.marketplace.as_ref() else {
        missing.insert("signals.marketplace.installs_month".to_string());
        return 0;
    };
    let Some(installs) = marketplace.installs_month else {
        missing.insert("signals.marketplace.installs_month".to_string());
        return 0;
    };
    match installs {
        d if d >= 10_000 => 5,
        d if d >= 2_000 => 4,
        d if d >= 500 => 2,
        d if d >= 100 => 1,
        _ => 0,
    }
}

fn score_forks(signals: &Signals, missing: &mut BTreeSet<String>) -> u32 {
    let Some(forks) = signals.github_forks else {
        missing.insert("signals.github_forks".to_string());
        return 0;
    };
    match forks {
        f if f >= 500 => 2,
        f if f >= 200 => 1,
        f if f >= 50 => 1,
        _ => 0,
    }
}

fn score_runtime_tier(tags: &Tags) -> u32 {
    let Some(runtime) = tags.runtime.as_deref() else {
        return 0;
    };
    match runtime {
        "pkg-with-deps" | "provider-ext" => 6,
        "multi-file" => 4,
        "legacy-js" => 2,
        _ => 0,
    }
}

fn score_interaction(tags: &Tags) -> u32 {
    let mut score = 0;
    if tags.interaction.iter().any(|tag| tag == "provider") {
        score += 3;
    }
    if tags.interaction.iter().any(|tag| tag == "ui_integration") {
        score += 2;
    }
    if tags.interaction.iter().any(|tag| tag == "event_hook") {
        score += 2;
    }
    if tags.interaction.iter().any(|tag| tag == "slash_command") {
        score += 1;
    }
    if tags.interaction.iter().any(|tag| tag == "tool_only") {
        score += 1;
    }
    score.min(8)
}

fn score_hostcalls(tags: &Tags) -> u32 {
    let mut score = 0;
    if tags.capabilities.iter().any(|cap| cap == "exec") {
        score += 2;
    }
    if tags.capabilities.iter().any(|cap| cap == "http") {
        score += 2;
    }
    if tags
        .capabilities
        .iter()
        .any(|cap| matches!(cap.as_str(), "read" | "write" | "edit"))
    {
        score += 1;
    }
    if tags.capabilities.iter().any(|cap| cap == "ui") {
        score += 1;
    }
    if tags.capabilities.iter().any(|cap| cap == "session") {
        score += 1;
    }
    score.min(6)
}

fn score_activity(recency: &Recency, as_of: DateTime<Utc>, missing: &mut BTreeSet<String>) -> u32 {
    let Some(updated_at) = recency.updated_at.as_deref() else {
        missing.insert("recency.updated_at".to_string());
        return 0;
    };
    let Ok(parsed) = DateTime::parse_from_rfc3339(updated_at) else {
        missing.insert("recency.updated_at".to_string());
        return 0;
    };
    let updated_at = parsed.with_timezone(&Utc);
    let days = (as_of - updated_at).num_days();
    match days {
        d if d <= 30 => 15,
        d if d <= 90 => 12,
        d if d <= 180 => 9,
        d if d <= 365 => 6,
        d if d <= 730 => 3,
        _ => 0,
    }
}

fn score_compatibility(compat: &Compatibility) -> u32 {
    let base = match compat.status.unwrap_or_default() {
        CompatStatus::Unmodified => 20,
        CompatStatus::RequiresShims => 15,
        CompatStatus::RuntimeGap => 10,
        CompatStatus::Blocked | CompatStatus::Unknown => 0,
    };
    let adjustment = compat.adjustment.unwrap_or(0);
    let adjusted = base + i32::from(adjustment);
    #[allow(clippy::cast_sign_loss)]
    {
        adjusted.clamp(0, 20) as u32
    }
}

fn score_risk(risk: &RiskInfo) -> u32 {
    if let Some(penalty) = risk.penalty {
        return u32::from(penalty.min(15));
    }
    match risk.level {
        Some(RiskLevel::Low) | None => 0,
        Some(RiskLevel::Moderate) => 5,
        Some(RiskLevel::High) => 10,
        Some(RiskLevel::Critical) => 15,
    }
}

fn compute_gates(candidate: &CandidateInput) -> GateStatus {
    let provenance_pinned = candidate.gates.provenance_pinned.unwrap_or(false);
    let deterministic = candidate.gates.deterministic.unwrap_or(false);
    let license_ok = matches!(
        candidate.license.redistribution,
        Some(Redistribution::Ok | Redistribution::Restricted)
    );
    let unmodified = matches!(
        candidate.compat.status.unwrap_or_default(),
        CompatStatus::Unmodified | CompatStatus::RequiresShims | CompatStatus::RuntimeGap
    );
    let passes = provenance_pinned && deterministic && license_ok && unmodified;
    GateStatus {
        provenance_pinned,
        license_ok,
        deterministic,
        unmodified,
        passes,
    }
}

fn compute_tier(candidate: &CandidateInput, gates: &GateStatus, final_total: u32) -> String {
    let is_official = candidate.signals.pi_mono_example.unwrap_or(false)
        || matches!(candidate.source_tier.as_deref(), Some("official-pi-mono"));
    if is_official {
        if let Some(override_tier) = candidate
            .manual_override
            .as_ref()
            .and_then(|override_spec| override_spec.tier.clone())
        {
            return override_tier;
        }
        return "tier-0".to_string();
    }
    if let Some(override_tier) = candidate
        .manual_override
        .as_ref()
        .and_then(|override_spec| override_spec.tier.clone())
    {
        return override_tier;
    }
    if !gates.passes {
        return "excluded".to_string();
    }
    if final_total >= 70 {
        return "tier-1".to_string();
    }
    if final_total >= 50 {
        return "tier-2".to_string();
    }
    "excluded".to_string()
}

fn build_summary(items: &[ScoredCandidate], top_n: usize) -> ScoringSummary {
    let histogram = build_histogram(items);
    let top_overall = items
        .iter()
        .take(top_n)
        .map(|item| RankedEntry {
            id: item.id.clone(),
            score: item.score.final_total,
            tier: item.tier.clone(),
            rank: item.rank,
        })
        .collect::<Vec<_>>();

    let mut by_tier = BTreeMap::<String, Vec<&ScoredCandidate>>::new();
    for item in items {
        let tier = item
            .source_tier
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        by_tier.entry(tier).or_default().push(item);
    }

    let mut top_by_source_tier = BTreeMap::new();
    for (tier, entries) in by_tier {
        let mut top_entries = entries
            .into_iter()
            .take(top_n)
            .map(|item| RankedEntry {
                id: item.id.clone(),
                score: item.score.final_total,
                tier: item.tier.clone(),
                rank: item.rank,
            })
            .collect::<Vec<_>>();
        top_entries.sort_by_key(|entry| entry.rank);
        top_by_source_tier.insert(tier, top_entries);
    }

    let manual_overrides = items
        .iter()
        .filter_map(|item| {
            item.manual_override
                .as_ref()
                .map(|override_spec| ManualOverrideEntry {
                    id: item.id.clone(),
                    reason: override_spec.reason.clone(),
                    tier: override_spec.tier.clone(),
                })
        })
        .collect::<Vec<_>>();

    ScoringSummary {
        histogram,
        top_overall,
        top_by_source_tier,
        manual_overrides,
    }
}

fn build_histogram(items: &[ScoredCandidate]) -> Vec<ScoreHistogramBucket> {
    let mut buckets = BTreeMap::<u32, u32>::new();
    for item in items {
        let bucket = item.score.final_total / 10;
        *buckets.entry(bucket).or_insert(0) += 1;
    }
    (0..=10)
        .map(|bucket| {
            let start = bucket * 10;
            let end = if bucket == 10 { 100 } else { start + 9 };
            ScoreHistogramBucket {
                range: format!("{start}-{end}"),
                count: buckets.get(&bucket).copied().unwrap_or(0),
            }
        })
        .collect()
}
