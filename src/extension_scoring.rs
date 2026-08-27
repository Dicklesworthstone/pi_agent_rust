use chrono::{DateTime, Duration, Utc};
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
    // Log-linear: 10 * ln(1 + stars) / ln(1 + 5000), clamped to [0, 10]
    #[allow(clippy::cast_precision_loss)]
    let score = 10.0 * (stars as f64).ln_1p() / 5000_f64.ln_1p();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        (score.round() as u32).min(10)
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
    // Log-linear: 8 * ln(1 + downloads) / ln(1 + 50_000), clamped to [0, 8]
    #[allow(clippy::cast_precision_loss)]
    let score = 8.0 * (downloads as f64).ln_1p() / 50_000_f64.ln_1p();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        (score.round() as u32).min(8)
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
    if updated_at > as_of {
        missing.insert("recency.updated_at".to_string());
        return 0;
    }
    #[allow(clippy::cast_precision_loss)]
    let days = (as_of - updated_at).num_days() as f64;
    // Exponential decay: 15 * exp(-ln(2) * days / half_life), half_life = 180 days
    let half_life = 180.0_f64;
    let score = 15.0 * (-std::f64::consts::LN_2 * days / half_life).exp();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        (score.round() as u32).min(15)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiSkipReason {
    Disabled,
    MissingTelemetry,
    StaleEvidence,
    BudgetExceeded,
    BelowUtilityFloor,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiCandidate {
    pub id: String,
    pub utility_score: f64,
    pub estimated_overhead_ms: u32,
    #[serde(default)]
    pub last_seen_at: Option<String>,
    #[serde(default = "default_voi_candidate_enabled")]
    pub enabled: bool,
}

const fn default_voi_candidate_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiPlannerConfig {
    pub enabled: bool,
    pub overhead_budget_ms: u32,
    #[serde(default)]
    pub max_candidates: Option<usize>,
    #[serde(default)]
    pub stale_after_minutes: Option<i64>,
    #[serde(default)]
    pub min_utility_score: Option<f64>,
}

impl Default for VoiPlannerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            overhead_budget_ms: 100,
            max_candidates: None,
            stale_after_minutes: Some(120),
            min_utility_score: Some(0.0),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiPlannedCandidate {
    pub id: String,
    pub utility_score: f64,
    pub estimated_overhead_ms: u32,
    pub utility_per_ms: f64,
    pub cumulative_overhead_ms: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiSkippedCandidate {
    pub id: String,
    pub reason: VoiSkipReason,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiPlan {
    pub selected: Vec<VoiPlannedCandidate>,
    pub skipped: Vec<VoiSkippedCandidate>,
    pub used_overhead_ms: u32,
    pub remaining_overhead_ms: u32,
}

pub fn plan_voi_candidates(
    candidates: &[VoiCandidate],
    now: DateTime<Utc>,
    config: &VoiPlannerConfig,
) -> VoiPlan {
    let mut selected = Vec::new();
    let mut skipped = Vec::new();
    if !config.enabled {
        skipped.extend(candidates.iter().map(|candidate| VoiSkippedCandidate {
            id: candidate.id.clone(),
            reason: VoiSkipReason::Disabled,
        }));
        return VoiPlan {
            selected,
            skipped,
            used_overhead_ms: 0,
            remaining_overhead_ms: config.overhead_budget_ms,
        };
    }

    let max_candidates = config.max_candidates.unwrap_or(usize::MAX);
    let stale_after_minutes = config.stale_after_minutes.unwrap_or(120).max(0);
    let min_utility_score = config.min_utility_score.unwrap_or(0.0).max(0.0);

    let mut ranked = candidates.to_vec();
    ranked.sort_by(compare_voi_candidates_desc);
    let mut eligible = Vec::new();
    for candidate in ranked {
        if !candidate.enabled {
            skipped.push(VoiSkippedCandidate {
                id: candidate.id,
                reason: VoiSkipReason::Disabled,
            });
            continue;
        }
        if normalized_utility(candidate.utility_score) < min_utility_score {
            skipped.push(VoiSkippedCandidate {
                id: candidate.id,
                reason: VoiSkipReason::BelowUtilityFloor,
            });
            continue;
        }
        if let Some(reason) = evaluate_candidate_freshness(
            candidate.last_seen_at.as_deref(),
            now,
            stale_after_minutes,
        ) {
            skipped.push(VoiSkippedCandidate {
                id: candidate.id,
                reason,
            });
            continue;
        }
        eligible.push(candidate);
    }

    let selected_indices = select_voi_candidate_indices(
        &eligible,
        config.overhead_budget_ms,
        max_candidates.min(eligible.len()),
    );
    let mut used_overhead_ms = 0_u32;

    for (index, candidate) in eligible.into_iter().enumerate() {
        if !selected_indices.contains(&index) {
            skipped.push(VoiSkippedCandidate {
                id: candidate.id,
                reason: VoiSkipReason::BudgetExceeded,
            });
            continue;
        }
        used_overhead_ms = used_overhead_ms.saturating_add(candidate.estimated_overhead_ms);
        let upm = utility_per_ms(&candidate);
        let score = normalized_utility(candidate.utility_score);
        let id = candidate.id;
        selected.push(VoiPlannedCandidate {
            id,
            utility_score: score,
            estimated_overhead_ms: candidate.estimated_overhead_ms,
            utility_per_ms: upm,
            cumulative_overhead_ms: used_overhead_ms,
        });
    }

    VoiPlan {
        selected,
        skipped,
        used_overhead_ms,
        remaining_overhead_ms: config.overhead_budget_ms.saturating_sub(used_overhead_ms),
    }
}

#[derive(Debug, Clone)]
struct VoiSelectionState {
    utility_score: f64,
    overhead_ms: u32,
    selected_indices: Vec<usize>,
}

fn select_voi_candidate_indices(
    eligible: &[VoiCandidate],
    overhead_budget_ms: u32,
    max_candidates: usize,
) -> BTreeSet<usize> {
    if eligible.is_empty() || max_candidates == 0 {
        return BTreeSet::new();
    }

    let mut frontiers = vec![BTreeMap::<u32, VoiSelectionState>::new(); max_candidates + 1];
    frontiers[0].insert(
        0,
        VoiSelectionState {
            utility_score: 0.0,
            overhead_ms: 0,
            selected_indices: Vec::new(),
        },
    );

    for (candidate_index, candidate) in eligible.iter().enumerate() {
        let candidate_utility = normalized_utility(candidate.utility_score);
        for count in (0..max_candidates).rev() {
            let states = frontiers[count].values().cloned().collect::<Vec<_>>();
            for state in states {
                let Some(overhead_ms) = state
                    .overhead_ms
                    .checked_add(candidate.estimated_overhead_ms)
                else {
                    continue;
                };
                if overhead_ms > overhead_budget_ms {
                    continue;
                }
                let utility_score = state.utility_score + candidate_utility;
                let mut selected_indices = state.selected_indices;
                selected_indices.push(candidate_index);
                insert_voi_selection_state(
                    &mut frontiers[count + 1],
                    VoiSelectionState {
                        utility_score,
                        overhead_ms,
                        selected_indices,
                    },
                );
            }
        }
    }

    frontiers
        .into_iter()
        .flat_map(BTreeMap::into_values)
        .max_by(compare_voi_selection_states)
        .map(|state| state.selected_indices.into_iter().collect())
        .unwrap_or_default()
}

fn insert_voi_selection_state(
    frontier: &mut BTreeMap<u32, VoiSelectionState>,
    candidate: VoiSelectionState,
) {
    if frontier
        .values()
        .any(|existing| voi_selection_state_dominates(existing, &candidate))
    {
        return;
    }

    let dominated_overheads = frontier
        .iter()
        .filter_map(|(overhead, existing)| {
            voi_selection_state_dominates(&candidate, existing).then_some(*overhead)
        })
        .collect::<Vec<_>>();
    for overhead in dominated_overheads {
        frontier.remove(&overhead);
    }

    match frontier.get(&candidate.overhead_ms) {
        Some(existing)
            if compare_voi_selection_states(&candidate, existing)
                != std::cmp::Ordering::Greater =>
        {
            return;
        }
        _ => {}
    }
    frontier.insert(candidate.overhead_ms, candidate);
}

fn voi_selection_state_dominates(left: &VoiSelectionState, right: &VoiSelectionState) -> bool {
    const EPSILON: f64 = 1.0e-9;

    left.overhead_ms <= right.overhead_ms
        && (left.utility_score > right.utility_score + EPSILON
            || ((left.utility_score - right.utility_score).abs() <= EPSILON
                && (left.overhead_ms < right.overhead_ms
                    || left.selected_indices <= right.selected_indices)))
}

fn compare_voi_selection_states(
    left: &VoiSelectionState,
    right: &VoiSelectionState,
) -> std::cmp::Ordering {
    left.utility_score
        .partial_cmp(&right.utility_score)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| right.overhead_ms.cmp(&left.overhead_ms))
        .then_with(|| right.selected_indices.cmp(&left.selected_indices))
}

fn compare_voi_candidates_desc(left: &VoiCandidate, right: &VoiCandidate) -> std::cmp::Ordering {
    utility_per_ms(right)
        .partial_cmp(&utility_per_ms(left))
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| {
            normalized_utility(right.utility_score)
                .partial_cmp(&normalized_utility(left.utility_score))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| left.estimated_overhead_ms.cmp(&right.estimated_overhead_ms))
        .then_with(|| left.id.cmp(&right.id))
}

fn evaluate_candidate_freshness(
    last_seen_at: Option<&str>,
    now: DateTime<Utc>,
    stale_after_minutes: i64,
) -> Option<VoiSkipReason> {
    let Some(raw) = last_seen_at else {
        return Some(VoiSkipReason::MissingTelemetry);
    };
    let Ok(parsed) = DateTime::parse_from_rfc3339(raw) else {
        return Some(VoiSkipReason::MissingTelemetry);
    };
    let observed_at = parsed.with_timezone(&Utc);
    if observed_at > now {
        return Some(VoiSkipReason::MissingTelemetry);
    }
    let age = now.signed_duration_since(observed_at);
    let Some(staleness_window) = Duration::try_minutes(stale_after_minutes) else {
        return Some(VoiSkipReason::StaleEvidence);
    };
    if age > staleness_window {
        Some(VoiSkipReason::StaleEvidence)
    } else {
        None
    }
}

const fn normalized_utility(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn utility_per_ms(candidate: &VoiCandidate) -> f64 {
    if candidate.estimated_overhead_ms == 0 {
        0.0
    } else {
        normalized_utility(candidate.utility_score) / f64::from(candidate.estimated_overhead_ms)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeanFieldShardObservation {
    pub shard_id: String,
    pub queue_pressure: f64,
    pub tail_latency_ratio: f64,
    pub starvation_risk: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeanFieldShardState {
    pub shard_id: String,
    pub routing_weight: f64,
    pub batch_budget: u32,
    pub help_factor: f64,
    pub backoff_factor: f64,
    #[serde(default)]
    pub last_routing_delta: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeanFieldControllerConfig {
    pub queue_gain: f64,
    pub latency_gain: f64,
    pub starvation_gain: f64,
    pub damping: f64,
    pub max_step: f64,
    pub min_routing_weight: f64,
    pub max_routing_weight: f64,
    pub min_batch_budget: u32,
    pub max_batch_budget: u32,
    pub min_help_factor: f64,
    pub max_help_factor: f64,
    pub min_backoff_factor: f64,
    pub max_backoff_factor: f64,
    pub convergence_epsilon: f64,
}

impl Default for MeanFieldControllerConfig {
    fn default() -> Self {
        Self {
            queue_gain: 0.55,
            latency_gain: 0.35,
            starvation_gain: 0.50,
            damping: 0.60,
            max_step: 0.20,
            min_routing_weight: 0.10,
            max_routing_weight: 3.00,
            min_batch_budget: 1,
            max_batch_budget: 64,
            min_help_factor: 0.50,
            max_help_factor: 2.50,
            min_backoff_factor: 1.00,
            max_backoff_factor: 3.50,
            convergence_epsilon: 0.02,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeanFieldShardControl {
    pub shard_id: String,
    pub routing_weight: f64,
    pub batch_budget: u32,
    pub help_factor: f64,
    pub backoff_factor: f64,
    pub routing_delta: f64,
    pub stability_margin: f64,
    pub clipped: bool,
    pub oscillation_guarded: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeanFieldControllerReport {
    pub global_pressure: f64,
    pub converged: bool,
    pub controls: Vec<MeanFieldShardControl>,
    pub clipped_count: usize,
    pub oscillation_guard_count: usize,
}

pub fn compute_mean_field_controls(
    observations: &[MeanFieldShardObservation],
    previous: &[MeanFieldShardState],
    config: &MeanFieldControllerConfig,
) -> MeanFieldControllerReport {
    let mut previous_by_shard = BTreeMap::new();
    for state in previous {
        previous_by_shard.insert(state.shard_id.clone(), state.clone());
    }

    let sanitized = observations
        .iter()
        .map(|observation| {
            (
                observation.shard_id.clone(),
                sanitize_observation(observation),
            )
        })
        .collect::<Vec<_>>();
    let sanitized_config = sanitize_mean_field_config(config);
    let global_pressure = if sanitized.is_empty() {
        0.0
    } else {
        sanitized
            .iter()
            .map(|(_, obs)| obs.composite_pressure)
            .sum::<f64>()
            / usize_to_f64(sanitized.len())
    };

    let mut controls = Vec::with_capacity(sanitized.len());
    let mut clipped_count = 0_usize;
    let mut oscillation_guard_count = 0_usize;
    let mut total_absolute_delta = 0.0;

    for (shard_id, observation) in sanitized {
        let baseline = previous_by_shard
            .get(&shard_id)
            .cloned()
            .unwrap_or_else(|| default_mean_field_state(&shard_id, &sanitized_config));
        let baseline = sanitize_mean_field_state(baseline, &sanitized_config);

        let control = compute_control_for_shard(
            &shard_id,
            observation,
            &baseline,
            global_pressure,
            &sanitized_config,
        );
        if control.clipped {
            clipped_count = clipped_count.saturating_add(1);
        }
        if control.oscillation_guarded {
            oscillation_guard_count = oscillation_guard_count.saturating_add(1);
        }
        total_absolute_delta += control.routing_delta.abs();
        controls.push(control);
    }

    controls.sort_by(|left, right| left.shard_id.cmp(&right.shard_id));
    let converged = if controls.is_empty() {
        true
    } else {
        (total_absolute_delta / usize_to_f64(controls.len()))
            <= sanitized_config.convergence_epsilon
    };

    MeanFieldControllerReport {
        global_pressure,
        converged,
        controls,
        clipped_count,
        oscillation_guard_count,
    }
}

#[derive(Debug, Clone, Copy)]
struct SanitizedMeanFieldObservation {
    queue_pressure: f64,
    latency_pressure: f64,
    starvation_risk: f64,
    composite_pressure: f64,
}

#[derive(Debug, Clone, Copy)]
struct SanitizedMeanFieldConfig {
    queue_gain: f64,
    latency_gain: f64,
    starvation_gain: f64,
    damping: f64,
    max_step: f64,
    min_routing_weight: f64,
    max_routing_weight: f64,
    min_batch_budget: u32,
    max_batch_budget: u32,
    min_help_factor: f64,
    max_help_factor: f64,
    min_backoff_factor: f64,
    max_backoff_factor: f64,
    convergence_epsilon: f64,
}

fn sanitize_observation(observation: &MeanFieldShardObservation) -> SanitizedMeanFieldObservation {
    let queue_pressure = non_negative_finite(observation.queue_pressure).clamp(0.0, 1.0);
    let latency_pressure =
        non_negative_finite(observation.tail_latency_ratio - 1.0).clamp(0.0, 1.0);
    let starvation_risk = non_negative_finite(observation.starvation_risk).clamp(0.0, 1.0);
    let composite_pressure = (queue_pressure + latency_pressure + starvation_risk) / 3.0;
    SanitizedMeanFieldObservation {
        queue_pressure,
        latency_pressure,
        starvation_risk,
        composite_pressure,
    }
}

fn sanitize_mean_field_config(config: &MeanFieldControllerConfig) -> SanitizedMeanFieldConfig {
    let min_routing_weight = non_negative_finite(config.min_routing_weight);
    let max_routing_weight = config.max_routing_weight.max(min_routing_weight);
    let min_batch_budget = config.min_batch_budget.min(config.max_batch_budget);
    let max_batch_budget = config.max_batch_budget.max(min_batch_budget);
    let min_help_factor = non_negative_finite(config.min_help_factor);
    let max_help_factor = config.max_help_factor.max(min_help_factor);
    let min_backoff_factor = non_negative_finite(config.min_backoff_factor);
    let max_backoff_factor = config.max_backoff_factor.max(min_backoff_factor);
    SanitizedMeanFieldConfig {
        queue_gain: non_negative_finite(config.queue_gain),
        latency_gain: non_negative_finite(config.latency_gain),
        starvation_gain: non_negative_finite(config.starvation_gain),
        damping: non_negative_finite(config.damping).min(1.0),
        max_step: non_negative_finite(config.max_step),
        min_routing_weight,
        max_routing_weight,
        min_batch_budget,
        max_batch_budget,
        min_help_factor,
        max_help_factor,
        min_backoff_factor,
        max_backoff_factor,
        convergence_epsilon: non_negative_finite(config.convergence_epsilon),
    }
}

fn sanitize_mean_field_state(
    state: MeanFieldShardState,
    config: &SanitizedMeanFieldConfig,
) -> MeanFieldShardState {
    MeanFieldShardState {
        shard_id: state.shard_id,
        routing_weight: finite_or_zero(state.routing_weight)
            .clamp(config.min_routing_weight, config.max_routing_weight),
        batch_budget: state
            .batch_budget
            .clamp(config.min_batch_budget, config.max_batch_budget),
        help_factor: finite_or_zero(state.help_factor)
            .clamp(config.min_help_factor, config.max_help_factor),
        backoff_factor: finite_or_zero(state.backoff_factor)
            .clamp(config.min_backoff_factor, config.max_backoff_factor),
        last_routing_delta: finite_or_zero(state.last_routing_delta),
    }
}

fn compute_control_for_shard(
    shard_id: &str,
    observation: SanitizedMeanFieldObservation,
    baseline: &MeanFieldShardState,
    global_pressure: f64,
    config: &SanitizedMeanFieldConfig,
) -> MeanFieldShardControl {
    let pressure_offset = observation.composite_pressure - global_pressure;
    let instability = config.starvation_gain.mul_add(
        -observation.starvation_risk,
        config.latency_gain.mul_add(
            observation.latency_pressure,
            config
                .queue_gain
                .mul_add(observation.queue_pressure, pressure_offset),
        ),
    );
    let target_routing = baseline.routing_weight - instability;
    let mut routing_delta = target_routing - baseline.routing_weight;
    let oscillation_guarded = if baseline.last_routing_delta.abs() > f64::EPSILON
        && routing_delta.abs() > f64::EPSILON
        && baseline.last_routing_delta.signum() != routing_delta.signum()
    {
        routing_delta *= 0.5;
        true
    } else {
        false
    };

    let clipped_delta = routing_delta.clamp(-config.max_step, config.max_step);
    let step_clipped = (clipped_delta - routing_delta).abs() > f64::EPSILON;
    let damped_routing = clipped_delta.mul_add(config.damping, baseline.routing_weight);
    let bounded_routing =
        damped_routing.clamp(config.min_routing_weight, config.max_routing_weight);
    let routing_boundary_clipped = (bounded_routing - damped_routing).abs() > f64::EPSILON;
    let routing_clipped = step_clipped || routing_boundary_clipped;

    let normalized_batch = (-0.4_f64)
        .mul_add(
            observation.latency_pressure,
            (-0.6_f64).mul_add(observation.queue_pressure, 1.0),
        )
        .clamp(0.0, 1.0);
    let desired_batch = f64::from(config.max_batch_budget) * normalized_batch;
    let batch_budget = quantize_batch_budget(
        desired_batch,
        config.min_batch_budget,
        config.max_batch_budget,
    );

    let help_factor = config
        .starvation_gain
        .mul_add(observation.starvation_risk, 1.0)
        .clamp(config.min_help_factor, config.max_help_factor);
    let backoff_factor = config
        .latency_gain
        .mul_add(
            observation.latency_pressure,
            config.queue_gain.mul_add(observation.queue_pressure, 1.0),
        )
        .clamp(config.min_backoff_factor, config.max_backoff_factor);
    let stability_margin = (config.max_step - routing_delta.abs()).max(0.0);

    MeanFieldShardControl {
        shard_id: shard_id.to_string(),
        routing_weight: bounded_routing,
        batch_budget,
        help_factor,
        backoff_factor,
        routing_delta: bounded_routing - baseline.routing_weight,
        stability_margin,
        clipped: routing_clipped,
        oscillation_guarded,
    }
}

fn quantize_batch_budget(desired_batch: f64, min_batch_budget: u32, max_batch_budget: u32) -> u32 {
    if min_batch_budget >= max_batch_budget {
        return min_batch_budget;
    }

    let lower_bound = f64::from(min_batch_budget);
    let upper_bound = f64::from(max_batch_budget);
    let desired = if desired_batch.is_finite() {
        desired_batch.clamp(lower_bound, upper_bound)
    } else {
        lower_bound
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lower = desired.floor() as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let upper = desired.ceil() as u32;
    if lower == upper {
        return lower.clamp(min_batch_budget, max_batch_budget);
    }

    let lower = lower.clamp(min_batch_budget, max_batch_budget);
    let upper = upper.clamp(min_batch_budget, max_batch_budget);
    let lower_distance = (desired - f64::from(lower)).abs();
    let upper_distance = (f64::from(upper) - desired).abs();
    if upper_distance < lower_distance {
        upper
    } else {
        lower
    }
}

fn default_mean_field_state(
    shard_id: &str,
    config: &SanitizedMeanFieldConfig,
) -> MeanFieldShardState {
    MeanFieldShardState {
        shard_id: shard_id.to_string(),
        routing_weight: 1.0,
        batch_budget: u32::midpoint(config.min_batch_budget, config.max_batch_budget),
        help_factor: 1.0,
        backoff_factor: 1.0,
        last_routing_delta: 0.0,
    }
}

fn usize_to_f64(value: usize) -> f64 {
    let bounded = u32::try_from(value).unwrap_or(u32::MAX);
    f64::from(bounded)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpeGateReason {
    Approved,
    NoValidSamples,
    InsufficientSupport,
    HighUncertainty,
    ExcessiveRegret,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpeTraceSample {
    pub action: String,
    pub behavior_propensity: f64,
    pub target_propensity: f64,
    pub outcome: f64,
    #[serde(default)]
    pub baseline_outcome: Option<f64>,
    #[serde(default)]
    pub direct_method_prediction: Option<f64>,
    #[serde(default)]
    pub context_lineage: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpeEvaluatorConfig {
    pub max_importance_weight: f64,
    pub min_effective_sample_size: f64,
    pub max_standard_error: f64,
    pub confidence_z: f64,
    pub max_regret_delta: f64,
}

impl Default for OpeEvaluatorConfig {
    fn default() -> Self {
        Self {
            max_importance_weight: 25.0,
            min_effective_sample_size: 8.0,
            max_standard_error: 0.25,
            confidence_z: 1.96,
            max_regret_delta: 0.05,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpeEstimatorSummary {
    pub estimate: f64,
    pub variance: f64,
    pub standard_error: f64,
    pub ci_low: f64,
    pub ci_high: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpeDiagnostics {
    pub total_samples: usize,
    pub valid_samples: usize,
    pub skipped_invalid_samples: usize,
    pub direct_method_fallback_samples: usize,
    pub clipped_weight_samples: usize,
    pub sum_importance_weight: f64,
    pub max_importance_weight: f64,
    pub effective_sample_size: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpeGateDecision {
    pub passed: bool,
    pub reason: OpeGateReason,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpeEvaluationReport {
    pub ips: OpeEstimatorSummary,
    pub wis: OpeEstimatorSummary,
    pub doubly_robust: OpeEstimatorSummary,
    pub baseline_mean: f64,
    pub estimated_regret_delta: f64,
    pub diagnostics: OpeDiagnostics,
    pub gate: OpeGateDecision,
}

#[derive(Debug, Clone, Copy)]
struct NormalizedOpeSample {
    importance_weight: f64,
    outcome: f64,
    baseline: f64,
    direct_method: f64,
}

#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
pub fn evaluate_off_policy(
    samples: &[OpeTraceSample],
    config: &OpeEvaluatorConfig,
) -> OpeEvaluationReport {
    let max_importance_weight = non_negative_finite(config.max_importance_weight);
    let min_effective_sample_size = non_negative_finite(config.min_effective_sample_size);
    let max_standard_error = non_negative_finite(config.max_standard_error);
    let confidence_z = positive_finite_or(config.confidence_z, 1.96);
    let max_regret_delta = non_negative_finite(config.max_regret_delta);

    let mut normalized_samples = Vec::with_capacity(samples.len());
    let mut skipped_invalid_samples = 0_usize;
    let mut direct_method_fallback_samples = 0_usize;
    let mut clipped_weight_samples = 0_usize;

    for sample in samples {
        if let Some((normalized, clipped_weight, fallback_direct_method)) =
            normalize_ope_sample(sample, max_importance_weight)
        {
            if clipped_weight {
                clipped_weight_samples = clipped_weight_samples.saturating_add(1);
            }
            if fallback_direct_method {
                direct_method_fallback_samples = direct_method_fallback_samples.saturating_add(1);
            }
            normalized_samples.push(normalized);
        } else {
            skipped_invalid_samples = skipped_invalid_samples.saturating_add(1);
        }
    }

    let valid_samples = normalized_samples.len();
    let mut sum_importance_weight: f64 = 0.0;
    let mut max_seen_importance_weight: f64 = 0.0;
    for sample in &normalized_samples {
        sum_importance_weight = saturating_add(sum_importance_weight, sample.importance_weight);
        max_seen_importance_weight = max_seen_importance_weight.max(sample.importance_weight);
    }
    let mut sum_importance_weight_scaled: f64 = 0.0;
    let mut sum_importance_weight_sq_scaled: f64 = 0.0;
    if max_seen_importance_weight > 0.0 {
        for sample in &normalized_samples {
            let scaled_weight = sample.importance_weight / max_seen_importance_weight;
            sum_importance_weight_scaled += scaled_weight;
            sum_importance_weight_sq_scaled =
                scaled_weight.mul_add(scaled_weight, sum_importance_weight_sq_scaled);
        }
    }
    let effective_sample_size = if sum_importance_weight_sq_scaled > 0.0 {
        (sum_importance_weight_scaled * sum_importance_weight_scaled)
            / sum_importance_weight_sq_scaled
    } else {
        0.0
    };

    let valid_samples_f64 = valid_samples as f64;
    let mut ips_effects = Vec::with_capacity(valid_samples);
    let mut wis_effects = Vec::with_capacity(valid_samples);
    let mut doubly_robust_effects = Vec::with_capacity(valid_samples);
    let mut baseline_values = Vec::with_capacity(valid_samples);

    for sample in &normalized_samples {
        ips_effects.push(sample.importance_weight * sample.outcome);
        doubly_robust_effects.push(
            sample
                .importance_weight
                .mul_add(sample.outcome - sample.direct_method, sample.direct_method),
        );
        baseline_values.push(sample.baseline);
    }
    if sum_importance_weight_scaled > 0.0 && max_seen_importance_weight > 0.0 {
        for sample in &normalized_samples {
            let scaled_weight = sample.importance_weight / max_seen_importance_weight;
            wis_effects.push(
                (scaled_weight * sample.outcome * valid_samples_f64) / sum_importance_weight_scaled,
            );
        }
    } else {
        wis_effects.resize(valid_samples, 0.0);
    }

    let ips = summarize_estimator(&ips_effects, confidence_z);
    let wis = summarize_estimator(&wis_effects, confidence_z);
    let doubly_robust = summarize_estimator(&doubly_robust_effects, confidence_z);
    let baseline_mean = arithmetic_mean(&baseline_values);
    let estimated_regret_delta = saturating_difference(baseline_mean, doubly_robust.estimate);
    let numerically_unstable = estimator_has_unbounded_uncertainty(&ips)
        || estimator_has_unbounded_uncertainty(&wis)
        || estimator_has_unbounded_uncertainty(&doubly_robust);

    let gate = if valid_samples == 0 {
        OpeGateDecision {
            passed: false,
            reason: OpeGateReason::NoValidSamples,
        }
    } else if effective_sample_size < min_effective_sample_size {
        OpeGateDecision {
            passed: false,
            reason: OpeGateReason::InsufficientSupport,
        }
    } else if numerically_unstable || doubly_robust.standard_error > max_standard_error {
        OpeGateDecision {
            passed: false,
            reason: OpeGateReason::HighUncertainty,
        }
    } else if estimated_regret_delta > max_regret_delta {
        OpeGateDecision {
            passed: false,
            reason: OpeGateReason::ExcessiveRegret,
        }
    } else {
        OpeGateDecision {
            passed: true,
            reason: OpeGateReason::Approved,
        }
    };

    OpeEvaluationReport {
        ips,
        wis,
        doubly_robust,
        baseline_mean,
        estimated_regret_delta,
        diagnostics: OpeDiagnostics {
            total_samples: samples.len(),
            valid_samples,
            skipped_invalid_samples,
            direct_method_fallback_samples,
            clipped_weight_samples,
            sum_importance_weight,
            max_importance_weight: max_seen_importance_weight,
            effective_sample_size,
        },
        gate,
    }
}

fn normalize_ope_sample(
    sample: &OpeTraceSample,
    max_importance_weight: f64,
) -> Option<(NormalizedOpeSample, bool, bool)> {
    if !sample.outcome.is_finite()
        || !sample.behavior_propensity.is_finite()
        || sample.behavior_propensity <= 0.0
        || !sample.target_propensity.is_finite()
        || sample.target_propensity < 0.0
    {
        return None;
    }
    let raw_weight = sample.target_propensity / sample.behavior_propensity;
    if !raw_weight.is_finite() || raw_weight < 0.0 {
        return None;
    }
    let clipped_weight = raw_weight.min(max_importance_weight);
    let clipped = clipped_weight < raw_weight;
    let baseline = sample
        .baseline_outcome
        .filter(|value| value.is_finite())
        .unwrap_or(sample.outcome);
    let (direct_method, fallback_direct_method) = match sample.direct_method_prediction {
        Some(value) if value.is_finite() => (value, false),
        _ => (0.0, true),
    };
    let ips_effect = clipped_weight * sample.outcome;
    let doubly_robust_effect =
        clipped_weight.mul_add(sample.outcome - direct_method, direct_method);
    if !ips_effect.is_finite() || !doubly_robust_effect.is_finite() {
        return None;
    }
    Some((
        NormalizedOpeSample {
            importance_weight: clipped_weight,
            outcome: sample.outcome,
            baseline,
            direct_method,
        },
        clipped,
        fallback_direct_method,
    ))
}

#[allow(clippy::cast_precision_loss)]
fn summarize_estimator(effects: &[f64], confidence_z: f64) -> OpeEstimatorSummary {
    if effects.is_empty() {
        return OpeEstimatorSummary {
            estimate: 0.0,
            variance: 0.0,
            standard_error: 0.0,
            ci_low: 0.0,
            ci_high: 0.0,
        };
    }
    if effects.iter().any(|value| !value.is_finite()) {
        return unbounded_uncertainty_summary(0.0);
    }
    let sample_count = effects.len() as f64;
    let estimate = arithmetic_mean(effects);
    let scale = effects
        .iter()
        .fold(0.0_f64, |acc, value| acc.max(value.abs()));
    if scale == 0.0 {
        return OpeEstimatorSummary {
            estimate: 0.0,
            variance: 0.0,
            standard_error: 0.0,
            ci_low: 0.0,
            ci_high: 0.0,
        };
    }
    let variance = if effects.len() > 1 {
        effects
            .iter()
            .map(|value| {
                let centered = (*value / scale) - (estimate / scale);
                centered * centered
            })
            .sum::<f64>()
            * scale
            * scale
            / (sample_count - 1.0)
    } else {
        0.0
    };
    let variance = if variance.is_finite() {
        variance
    } else {
        f64::MAX
    };
    let standard_error = if is_saturated_f64(variance) {
        f64::MAX
    } else {
        let standard_error = (variance / sample_count).sqrt();
        if standard_error.is_finite() {
            standard_error
        } else {
            f64::MAX
        }
    };
    let margin = if is_saturated_f64(standard_error) {
        f64::MAX
    } else {
        let margin = confidence_z * standard_error;
        if margin.is_finite() { margin } else { f64::MAX }
    };
    OpeEstimatorSummary {
        estimate,
        variance,
        standard_error,
        ci_low: saturating_difference(estimate, margin),
        ci_high: saturating_add(estimate, margin),
    }
}

#[allow(clippy::cast_precision_loss)]
fn arithmetic_mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        let scale = values
            .iter()
            .fold(0.0_f64, |acc, value| acc.max(value.abs()));
        if scale == 0.0 {
            0.0
        } else {
            let sample_count = values.len() as f64;
            scale * (values.iter().map(|value| *value / scale).sum::<f64>() / sample_count)
        }
    }
}

const fn non_negative_finite(value: f64) -> f64 {
    if value.is_finite() {
        if value > 0.0 { value } else { 0.0 }
    } else {
        0.0
    }
}

const fn finite_or_zero(value: f64) -> f64 {
    if value.is_finite() { value } else { 0.0 }
}

fn positive_finite_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}

fn unbounded_uncertainty_summary(estimate: f64) -> OpeEstimatorSummary {
    OpeEstimatorSummary {
        estimate,
        variance: f64::MAX,
        standard_error: f64::MAX,
        ci_low: -f64::MAX,
        ci_high: f64::MAX,
    }
}

const fn estimator_has_unbounded_uncertainty(summary: &OpeEstimatorSummary) -> bool {
    is_saturated_f64(summary.standard_error)
}

fn saturating_add(left: f64, right: f64) -> f64 {
    let sum = left + right;
    if sum.is_finite() {
        sum
    } else if left.is_sign_negative() == right.is_sign_negative() {
        if left.is_sign_negative() {
            -f64::MAX
        } else {
            f64::MAX
        }
    } else {
        0.0
    }
}

fn saturating_difference(left: f64, right: f64) -> f64 {
    saturating_add(left, -right)
}

const fn is_saturated_f64(value: f64) -> bool {
    value >= f64::MAX / 2.0
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InterferenceMatrixCompletenessReport {
    pub expected_pairs: usize,
    pub observed_pairs: usize,
    pub missing_pairs: Vec<String>,
    pub duplicate_pairs: Vec<String>,
    pub unknown_pairs: Vec<String>,
    pub complete: bool,
}

fn normalize_interference_lever(raw: &str) -> Option<String> {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn canonicalize_interference_pair(first: &str, second: &str) -> Option<(String, String)> {
    let left = normalize_interference_lever(first)?;
    let right = normalize_interference_lever(second)?;
    if left <= right {
        Some((left, right))
    } else {
        Some((right, left))
    }
}

pub fn parse_interference_pair_key(key: &str) -> Option<(String, String)> {
    let mut parts = key.split('+');
    let first = parts.next()?;
    let second = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    canonicalize_interference_pair(first, second)
}

pub fn format_interference_pair_key(first: &str, second: &str) -> Option<String> {
    let (left, right) = canonicalize_interference_pair(first, second)?;
    Some(format!("{left}+{right}"))
}

pub fn evaluate_interference_matrix_completeness(
    levers: &[String],
    observed_pair_keys: &[String],
) -> InterferenceMatrixCompletenessReport {
    let ordered_levers: Vec<String> = levers
        .iter()
        .filter_map(|lever| normalize_interference_lever(lever))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let mut expected_pairs = BTreeSet::new();
    for (idx, left) in ordered_levers.iter().enumerate() {
        for right in ordered_levers.iter().skip(idx + 1) {
            expected_pairs.insert(format!("{left}+{right}"));
        }
    }

    let mut seen_pairs = BTreeSet::new();
    let mut duplicate_pairs = BTreeSet::new();
    let mut unknown_pairs = BTreeSet::new();
    let mut observed_pairs = BTreeSet::new();

    for raw_key in observed_pair_keys {
        let Some((left, right)) = parse_interference_pair_key(raw_key) else {
            unknown_pairs.insert(raw_key.clone());
            continue;
        };

        let key = format!("{left}+{right}");
        if !seen_pairs.insert(key.clone()) {
            duplicate_pairs.insert(key.clone());
            continue;
        }

        if expected_pairs.contains(&key) {
            observed_pairs.insert(key);
        } else {
            unknown_pairs.insert(key);
        }
    }

    let missing_pairs = expected_pairs
        .difference(&observed_pairs)
        .cloned()
        .collect::<Vec<_>>();
    let duplicate_pairs = duplicate_pairs.into_iter().collect::<Vec<_>>();
    let unknown_pairs = unknown_pairs.into_iter().collect::<Vec<_>>();

    InterferenceMatrixCompletenessReport {
        expected_pairs: expected_pairs.len(),
        observed_pairs: observed_pairs.len(),
        missing_pairs: missing_pairs.clone(),
        duplicate_pairs: duplicate_pairs.clone(),
        unknown_pairs: unknown_pairs.clone(),
        complete: missing_pairs.is_empty()
            && duplicate_pairs.is_empty()
            && unknown_pairs.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn empty_signals() -> Signals {
        Signals::default()
    }

    fn empty_tags() -> Tags {
        Tags::default()
    }

    fn minimal_candidate(id: &str) -> CandidateInput {
        CandidateInput {
            id: id.to_string(),
            name: None,
            source_tier: None,
            signals: Signals::default(),
            tags: Tags::default(),
            recency: Recency::default(),
            compat: Compatibility::default(),
            license: LicenseInfo::default(),
            gates: Gates::default(),
            risk: RiskInfo::default(),
            manual_override: None,
        }
    }

    #[test]
    fn parse_interference_pair_key_normalizes_order_and_case() {
        let parsed = parse_interference_pair_key("Queue+marshal").expect("pair must parse");
        assert_eq!(parsed.0, "marshal");
        assert_eq!(parsed.1, "queue");

        let formatted =
            format_interference_pair_key(" Queue ", "marshal ").expect("pair must format");
        assert_eq!(formatted, "marshal+queue");
    }

    #[test]
    fn parse_interference_pair_key_rejects_invalid_shapes() {
        assert!(parse_interference_pair_key("").is_none());
        assert!(parse_interference_pair_key("queue").is_none());
        assert!(parse_interference_pair_key("a+b+c").is_none());
        assert!(parse_interference_pair_key(" + ").is_none());
    }

    #[test]
    fn interference_matrix_completeness_detects_missing_duplicate_and_unknown_pairs() {
        let levers = vec![
            "queue".to_string(),
            "policy".to_string(),
            "execute".to_string(),
        ];
        let observed = vec![
            "queue+policy".to_string(),
            "policy+queue".to_string(),  // duplicate (canonicalized)
            "queue+marshal".to_string(), // unknown pair
            "broken".to_string(),        // malformed key
        ];

        let report = evaluate_interference_matrix_completeness(&levers, &observed);
        assert_eq!(report.expected_pairs, 3);
        assert_eq!(report.observed_pairs, 1);
        assert_eq!(
            report.missing_pairs,
            vec!["execute+policy".to_string(), "execute+queue".to_string()]
        );
        assert_eq!(report.duplicate_pairs, vec!["policy+queue".to_string()]);
        assert_eq!(
            report.unknown_pairs,
            vec!["broken".to_string(), "marshal+queue".to_string()]
        );
        assert!(!report.complete);
    }

    #[test]
    fn interference_matrix_completeness_passes_with_full_unique_matrix() {
        let levers = vec![
            "marshal".to_string(),
            "queue".to_string(),
            "schedule".to_string(),
            "policy".to_string(),
        ];
        let observed = vec![
            "marshal+queue".to_string(),
            "marshal+schedule".to_string(),
            "marshal+policy".to_string(),
            "queue+schedule".to_string(),
            "queue+policy".to_string(),
            "schedule+policy".to_string(),
        ];

        let report = evaluate_interference_matrix_completeness(&levers, &observed);
        assert_eq!(report.expected_pairs, 6);
        assert_eq!(report.observed_pairs, 6);
        assert!(report.missing_pairs.is_empty());
        assert!(report.duplicate_pairs.is_empty());
        assert!(report.unknown_pairs.is_empty());
        assert!(report.complete);
    }

    // =========================================================================
    // score_github_stars
    // =========================================================================

    #[test]
    fn github_stars_none_returns_zero_and_records_missing() {
        let mut missing = BTreeSet::new();
        let signals = empty_signals();
        assert_eq!(score_github_stars(&signals, &mut missing), 0);
        assert!(missing.contains("signals.github_stars"));
    }

    #[test]
    fn github_stars_tiers() {
        let cases = [
            (0, 0),
            (49, 5),
            (50, 5),
            (199, 6),
            (200, 6),
            (499, 7),
            (500, 7),
            (999, 8),
            (1_000, 8),
            (1_999, 9),
            (2_000, 9),
            (4_999, 10),
            (5_000, 10),
            (100_000, 10),
        ];
        for (stars, expected) in cases {
            let mut missing = BTreeSet::new();
            let signals = Signals {
                github_stars: Some(stars),
                ..Default::default()
            };
            assert_eq!(
                score_github_stars(&signals, &mut missing),
                expected,
                "stars={stars}"
            );
        }
    }

    // =========================================================================
    // score_npm_downloads
    // =========================================================================

    #[test]
    fn npm_downloads_none_returns_zero() {
        let mut missing = BTreeSet::new();
        assert_eq!(score_npm_downloads(&empty_signals(), &mut missing), 0);
        assert!(missing.contains("signals.npm_downloads_month"));
    }

    #[test]
    fn npm_downloads_tiers() {
        let cases = [
            (0, 0),
            (499, 5),
            (500, 5),
            (2_000, 6),
            (10_000, 7),
            (50_000, 8),
        ];
        for (dl, expected) in cases {
            let mut missing = BTreeSet::new();
            let signals = Signals {
                npm_downloads_month: Some(dl),
                ..Default::default()
            };
            assert_eq!(
                score_npm_downloads(&signals, &mut missing),
                expected,
                "downloads={dl}"
            );
        }
    }

    // =========================================================================
    // score_marketplace_installs
    // =========================================================================

    #[test]
    fn marketplace_installs_no_marketplace_records_missing() {
        let mut missing = BTreeSet::new();
        assert_eq!(
            score_marketplace_installs(&empty_signals(), &mut missing),
            0
        );
        assert!(missing.contains("signals.marketplace.installs_month"));
    }

    #[test]
    fn marketplace_installs_none_records_missing() {
        let mut missing = BTreeSet::new();
        let signals = Signals {
            marketplace: Some(MarketplaceSignals::default()),
            ..Default::default()
        };
        assert_eq!(score_marketplace_installs(&signals, &mut missing), 0);
        assert!(missing.contains("signals.marketplace.installs_month"));
    }

    #[test]
    fn marketplace_installs_tiers() {
        let cases = [(0, 0), (99, 0), (100, 1), (500, 2), (2_000, 4), (10_000, 5)];
        for (installs, expected) in cases {
            let mut missing = BTreeSet::new();
            let signals = Signals {
                marketplace: Some(MarketplaceSignals {
                    installs_month: Some(installs),
                    ..Default::default()
                }),
                ..Default::default()
            };
            assert_eq!(
                score_marketplace_installs(&signals, &mut missing),
                expected,
                "installs={installs}"
            );
        }
    }

    // =========================================================================
    // score_forks
    // =========================================================================

    #[test]
    fn forks_none_returns_zero() {
        let mut missing = BTreeSet::new();
        assert_eq!(score_forks(&empty_signals(), &mut missing), 0);
        assert!(missing.contains("signals.github_forks"));
    }

    #[test]
    fn forks_tiers() {
        let cases = [(0, 0), (49, 0), (50, 1), (200, 1), (500, 2), (10_000, 2)];
        for (f, expected) in cases {
            let mut missing = BTreeSet::new();
            let signals = Signals {
                github_forks: Some(f),
                ..Default::default()
            };
            assert_eq!(score_forks(&signals, &mut missing), expected, "forks={f}");
        }
    }

    // =========================================================================
    // score_references
    // =========================================================================

    #[test]
    fn references_empty() {
        let signals = empty_signals();
        assert_eq!(score_references(&signals), 0);
    }

    #[test]
    fn references_deduplicates_trimmed() {
        let signals = Signals {
            references: vec![" ref1 ".to_string(), "ref1".to_string(), "ref2".to_string()],
            ..Default::default()
        };
        assert_eq!(score_references(&signals), 2); // 2 unique => score 2
    }

    #[test]
    fn references_tiers() {
        let make = |n: usize| -> Signals {
            Signals {
                references: (0..n).map(|i| format!("ref-{i}")).collect(),
                ..Default::default()
            }
        };
        assert_eq!(score_references(&make(1)), 0);
        assert_eq!(score_references(&make(2)), 2);
        assert_eq!(score_references(&make(5)), 3);
        assert_eq!(score_references(&make(10)), 4);
        assert_eq!(score_references(&make(20)), 4);
    }

    #[test]
    fn references_ignores_empty_and_whitespace() {
        let signals = Signals {
            references: vec![String::new(), "  ".to_string(), "real".to_string()],
            ..Default::default()
        };
        assert_eq!(score_references(&signals), 0); // 1 unique => 0
    }

    // =========================================================================
    // score_official_visibility
    // =========================================================================

    #[test]
    fn official_visibility_all_none_records_missing() {
        let mut missing = BTreeSet::new();
        assert_eq!(score_official_visibility(&empty_signals(), &mut missing), 0);
        assert!(missing.contains("signals.official_visibility"));
    }

    #[test]
    fn official_visibility_listing_highest() {
        let mut missing = BTreeSet::new();
        let signals = Signals {
            official_listing: Some(true),
            pi_mono_example: Some(true),
            badlogic_gist: Some(true),
            ..Default::default()
        };
        assert_eq!(score_official_visibility(&signals, &mut missing), 10);
    }

    #[test]
    fn official_visibility_example_mid() {
        let mut missing = BTreeSet::new();
        let signals = Signals {
            pi_mono_example: Some(true),
            ..Default::default()
        };
        assert_eq!(score_official_visibility(&signals, &mut missing), 8);
    }

    #[test]
    fn official_visibility_gist_lowest() {
        let mut missing = BTreeSet::new();
        let signals = Signals {
            badlogic_gist: Some(true),
            ..Default::default()
        };
        assert_eq!(score_official_visibility(&signals, &mut missing), 6);
    }

    // =========================================================================
    // score_marketplace_visibility
    // =========================================================================

    #[test]
    fn marketplace_visibility_no_marketplace() {
        let mut missing = BTreeSet::new();
        assert_eq!(
            score_marketplace_visibility(&empty_signals(), &mut missing),
            0
        );
        assert!(missing.contains("signals.marketplace.rank"));
        assert!(missing.contains("signals.marketplace.featured"));
    }

    #[test]
    fn marketplace_visibility_rank_tiers() {
        let cases = [
            (5, 6),
            (10, 6),
            (30, 4),
            (50, 4),
            (80, 2),
            (100, 2),
            (200, 0),
        ];
        for (rank, expected) in cases {
            let mut missing = BTreeSet::new();
            let signals = Signals {
                marketplace: Some(MarketplaceSignals {
                    rank: Some(rank),
                    ..Default::default()
                }),
                ..Default::default()
            };
            assert_eq!(
                score_marketplace_visibility(&signals, &mut missing),
                expected,
                "rank={rank}"
            );
        }
    }

    #[test]
    fn marketplace_visibility_featured_adds_2_capped_at_6() {
        let mut missing = BTreeSet::new();
        let signals = Signals {
            marketplace: Some(MarketplaceSignals {
                rank: Some(5),
                featured: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        // rank=5 -> 6, featured -> +2, capped at 6
        assert_eq!(score_marketplace_visibility(&signals, &mut missing), 6);
    }

    // =========================================================================
    // score_runtime_tier
    // =========================================================================

    #[test]
    fn runtime_tier_values() {
        let cases = [
            (None, 0),
            (Some("pkg-with-deps"), 6),
            (Some("provider-ext"), 6),
            (Some("multi-file"), 4),
            (Some("legacy-js"), 2),
            (Some("unknown-tier"), 0),
        ];
        for (runtime, expected) in cases {
            let tags = Tags {
                runtime: runtime.map(String::from),
                ..Default::default()
            };
            assert_eq!(score_runtime_tier(&tags), expected, "runtime={runtime:?}");
        }
    }

    // =========================================================================
    // score_interaction
    // =========================================================================

    #[test]
    fn interaction_empty() {
        assert_eq!(score_interaction(&empty_tags()), 0);
    }

    #[test]
    fn interaction_all_tags() {
        let tags = Tags {
            interaction: vec![
                "provider".to_string(),
                "ui_integration".to_string(),
                "event_hook".to_string(),
                "slash_command".to_string(),
                "tool_only".to_string(),
            ],
            ..Default::default()
        };
        // 3+2+2+1+1 = 9, capped at 8
        assert_eq!(score_interaction(&tags), 8);
    }

    #[test]
    fn interaction_single_provider() {
        let tags = Tags {
            interaction: vec!["provider".to_string()],
            ..Default::default()
        };
        assert_eq!(score_interaction(&tags), 3);
    }

    // =========================================================================
    // score_hostcalls
    // =========================================================================

    #[test]
    fn hostcalls_empty() {
        assert_eq!(score_hostcalls(&empty_tags()), 0);
    }

    #[test]
    fn hostcalls_all_capabilities() {
        let tags = Tags {
            capabilities: vec![
                "exec".to_string(),
                "http".to_string(),
                "read".to_string(),
                "ui".to_string(),
                "session".to_string(),
            ],
            ..Default::default()
        };
        // 2+2+1+1+1 = 7, capped at 6
        assert_eq!(score_hostcalls(&tags), 6);
    }

    #[test]
    fn hostcalls_write_and_edit_count_as_one() {
        let tags = Tags {
            capabilities: vec!["write".to_string(), "edit".to_string()],
            ..Default::default()
        };
        // write matches the read|write|edit arm => 1, edit also matches => still 1
        assert_eq!(score_hostcalls(&tags), 1);
    }

    // =========================================================================
    // score_activity
    // =========================================================================

    #[test]
    fn activity_none_returns_zero() {
        let mut missing = BTreeSet::new();
        let as_of = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(score_activity(&Recency::default(), as_of, &mut missing), 0);
        assert!(missing.contains("recency.updated_at"));
    }

    #[test]
    fn activity_recent_gets_max() {
        let mut missing = BTreeSet::new();
        let as_of = Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap();
        let recency = Recency {
            updated_at: Some("2026-01-15T00:00:00Z".to_string()),
        };
        assert_eq!(score_activity(&recency, as_of, &mut missing), 14);
    }

    #[test]
    fn activity_tiers() {
        let as_of = Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap();
        let cases = [
            ("2026-06-15T00:00:00Z", 14), // 16 days
            ("2026-04-15T00:00:00Z", 11), // ~77 days
            ("2026-02-01T00:00:00Z", 8),  // ~150 days
            ("2025-10-01T00:00:00Z", 5),  // ~273 days
            ("2025-01-01T00:00:00Z", 2),  // ~547 days
            ("2023-01-01T00:00:00Z", 0),  // >730 days
        ];
        for (date, expected) in cases {
            let mut missing = BTreeSet::new();
            let recency = Recency {
                updated_at: Some(date.to_string()),
            };
            assert_eq!(
                score_activity(&recency, as_of, &mut missing),
                expected,
                "date={date}"
            );
        }
    }

    #[test]
    fn activity_invalid_date_returns_zero() {
        let mut missing = BTreeSet::new();
        let as_of = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let recency = Recency {
            updated_at: Some("not-a-date".to_string()),
        };
        assert_eq!(score_activity(&recency, as_of, &mut missing), 0);
        assert!(missing.contains("recency.updated_at"));
    }

    #[test]
    fn activity_future_date_returns_zero() {
        let mut missing = BTreeSet::new();
        let as_of = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let recency = Recency {
            updated_at: Some("2026-01-01T00:00:01Z".to_string()),
        };
        assert_eq!(score_activity(&recency, as_of, &mut missing), 0);
        assert!(missing.contains("recency.updated_at"));
    }

    // =========================================================================
    // score_compatibility
    // =========================================================================

    #[test]
    fn compatibility_unmodified_is_max() {
        let compat = Compatibility {
            status: Some(CompatStatus::Unmodified),
            ..Default::default()
        };
        assert_eq!(score_compatibility(&compat), 20);
    }

    #[test]
    fn compatibility_requires_shims() {
        let compat = Compatibility {
            status: Some(CompatStatus::RequiresShims),
            ..Default::default()
        };
        assert_eq!(score_compatibility(&compat), 15);
    }

    #[test]
    fn compatibility_runtime_gap() {
        let compat = Compatibility {
            status: Some(CompatStatus::RuntimeGap),
            ..Default::default()
        };
        assert_eq!(score_compatibility(&compat), 10);
    }

    #[test]
    fn compatibility_blocked_is_zero() {
        let compat = Compatibility {
            status: Some(CompatStatus::Blocked),
            ..Default::default()
        };
        assert_eq!(score_compatibility(&compat), 0);
    }

    #[test]
    fn compatibility_adjustment_positive() {
        let compat = Compatibility {
            status: Some(CompatStatus::RequiresShims),
            adjustment: Some(3),
            ..Default::default()
        };
        assert_eq!(score_compatibility(&compat), 18);
    }

    #[test]
    fn compatibility_adjustment_negative_clamped() {
        let compat = Compatibility {
            status: Some(CompatStatus::RuntimeGap),
            adjustment: Some(-15),
            ..Default::default()
        };
        // 10 + (-15) = -5, clamped to 0
        assert_eq!(score_compatibility(&compat), 0);
    }

    #[test]
    fn compatibility_adjustment_capped_at_20() {
        let compat = Compatibility {
            status: Some(CompatStatus::Unmodified),
            adjustment: Some(10),
            ..Default::default()
        };
        // 20 + 10 = 30, capped at 20
        assert_eq!(score_compatibility(&compat), 20);
    }

    // =========================================================================
    // score_risk
    // =========================================================================

    #[test]
    fn risk_none_is_zero() {
        assert_eq!(score_risk(&RiskInfo::default()), 0);
    }

    #[test]
    fn risk_penalty_override() {
        let risk = RiskInfo {
            penalty: Some(7),
            level: Some(RiskLevel::Critical),
            ..Default::default()
        };
        // Explicit penalty overrides level
        assert_eq!(score_risk(&risk), 7);
    }

    #[test]
    fn risk_penalty_capped_at_15() {
        let risk = RiskInfo {
            penalty: Some(50),
            ..Default::default()
        };
        assert_eq!(score_risk(&risk), 15);
    }

    #[test]
    fn risk_level_tiers() {
        let cases = [
            (RiskLevel::Low, 0),
            (RiskLevel::Moderate, 5),
            (RiskLevel::High, 10),
            (RiskLevel::Critical, 15),
        ];
        for (level, expected) in cases {
            let risk = RiskInfo {
                level: Some(level),
                ..Default::default()
            };
            assert_eq!(score_risk(&risk), expected, "level={level:?}");
        }
    }

    // =========================================================================
    // compute_gates
    // =========================================================================

    #[test]
    fn gates_all_false_by_default() {
        let candidate = minimal_candidate("test");
        let gates = compute_gates(&candidate);
        assert!(!gates.provenance_pinned);
        assert!(!gates.license_ok);
        assert!(!gates.deterministic);
        assert!(!gates.unmodified); // Unknown status -> not unmodified
        assert!(!gates.passes);
    }

    #[test]
    fn gates_all_pass() {
        let mut candidate = minimal_candidate("test");
        candidate.gates.provenance_pinned = Some(true);
        candidate.gates.deterministic = Some(true);
        candidate.license.redistribution = Some(Redistribution::Ok);
        candidate.compat.status = Some(CompatStatus::Unmodified);
        let gates = compute_gates(&candidate);
        assert!(gates.passes);
    }

    #[test]
    fn gates_license_restricted_counts_as_ok() {
        let mut candidate = minimal_candidate("test");
        candidate.gates.provenance_pinned = Some(true);
        candidate.gates.deterministic = Some(true);
        candidate.license.redistribution = Some(Redistribution::Restricted);
        candidate.compat.status = Some(CompatStatus::RequiresShims);
        let gates = compute_gates(&candidate);
        assert!(gates.license_ok);
        assert!(gates.passes);
    }

    #[test]
    fn gates_license_exclude_fails() {
        let mut candidate = minimal_candidate("test");
        candidate.license.redistribution = Some(Redistribution::Exclude);
        let gates = compute_gates(&candidate);
        assert!(!gates.license_ok);
    }

    #[test]
    fn gates_unmodified_includes_requires_shims_and_runtime_gap() {
        for status in [
            CompatStatus::Unmodified,
            CompatStatus::RequiresShims,
            CompatStatus::RuntimeGap,
        ] {
            let mut candidate = minimal_candidate("test");
            candidate.compat.status = Some(status);
            let gates = compute_gates(&candidate);
            assert!(gates.unmodified, "status={status:?} should be unmodified");
        }
    }

    // =========================================================================
    // compute_tier
    // =========================================================================

    #[test]
    fn tier_official_is_tier_0() {
        let mut candidate = minimal_candidate("test");
        candidate.signals.pi_mono_example = Some(true);
        let gates = compute_gates(&candidate);
        assert_eq!(compute_tier(&candidate, &gates, 0), "tier-0");
    }

    #[test]
    fn tier_official_source_tier_is_tier_0() {
        let mut candidate = minimal_candidate("test");
        candidate.source_tier = Some("official-pi-mono".to_string());
        let gates = compute_gates(&candidate);
        assert_eq!(compute_tier(&candidate, &gates, 0), "tier-0");
    }

    #[test]
    fn tier_manual_override_on_official() {
        let mut candidate = minimal_candidate("test");
        candidate.signals.pi_mono_example = Some(true);
        candidate.manual_override = Some(ManualOverride {
            reason: "special".to_string(),
            tier: Some("tier-1".to_string()),
        });
        let gates = compute_gates(&candidate);
        assert_eq!(compute_tier(&candidate, &gates, 0), "tier-1");
    }

    #[test]
    fn tier_excluded_when_gates_fail() {
        let candidate = minimal_candidate("test");
        let gates = compute_gates(&candidate); // gates fail
        assert_eq!(compute_tier(&candidate, &gates, 100), "excluded");
    }

    #[test]
    fn tier_1_at_70_plus() {
        let mut candidate = minimal_candidate("test");
        candidate.gates.provenance_pinned = Some(true);
        candidate.gates.deterministic = Some(true);
        candidate.license.redistribution = Some(Redistribution::Ok);
        candidate.compat.status = Some(CompatStatus::Unmodified);
        let gates = compute_gates(&candidate);
        assert_eq!(compute_tier(&candidate, &gates, 70), "tier-1");
        assert_eq!(compute_tier(&candidate, &gates, 100), "tier-1");
    }

    #[test]
    fn tier_2_at_50_to_69() {
        let mut candidate = minimal_candidate("test");
        candidate.gates.provenance_pinned = Some(true);
        candidate.gates.deterministic = Some(true);
        candidate.license.redistribution = Some(Redistribution::Ok);
        candidate.compat.status = Some(CompatStatus::Unmodified);
        let gates = compute_gates(&candidate);
        assert_eq!(compute_tier(&candidate, &gates, 50), "tier-2");
        assert_eq!(compute_tier(&candidate, &gates, 69), "tier-2");
    }

    #[test]
    fn tier_excluded_below_50() {
        let mut candidate = minimal_candidate("test");
        candidate.gates.provenance_pinned = Some(true);
        candidate.gates.deterministic = Some(true);
        candidate.license.redistribution = Some(Redistribution::Ok);
        candidate.compat.status = Some(CompatStatus::Unmodified);
        let gates = compute_gates(&candidate);
        assert_eq!(compute_tier(&candidate, &gates, 49), "excluded");
    }

    // =========================================================================
    // compare_scored
    // =========================================================================

    #[test]
    fn compare_scored_by_final_total() {
        let as_of = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let mut a = score_candidate(&minimal_candidate("a"), as_of);
        let mut b = score_candidate(&minimal_candidate("b"), as_of);
        a.score.final_total = 80;
        b.score.final_total = 60;
        assert_eq!(compare_scored(&a, &b), std::cmp::Ordering::Greater);
    }

    #[test]
    fn compare_scored_tiebreak_by_id() {
        let as_of = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let a = score_candidate(&minimal_candidate("alpha"), as_of);
        let b = score_candidate(&minimal_candidate("beta"), as_of);
        // Same scores, tiebreak by id
        assert_eq!(
            compare_scored(&a, &b),
            std::cmp::Ordering::Less // "alpha" < "beta"
        );
    }

    // =========================================================================
    // build_histogram
    // =========================================================================

    #[test]
    fn histogram_empty() {
        let histogram = build_histogram(&[]);
        assert_eq!(histogram.len(), 11);
        for bucket in &histogram {
            assert_eq!(bucket.count, 0);
        }
    }

    #[test]
    fn histogram_ranges_correct() {
        let histogram = build_histogram(&[]);
        assert_eq!(histogram[0].range, "0-9");
        assert_eq!(histogram[5].range, "50-59");
        assert_eq!(histogram[10].range, "100-100");
    }

    #[test]
    fn histogram_counts_correctly() {
        let as_of = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let mut items: Vec<ScoredCandidate> = (0..3)
            .map(|i| score_candidate(&minimal_candidate(&format!("c{i}")), as_of))
            .collect();
        items[0].score.final_total = 15; // bucket 1 (10-19)
        items[1].score.final_total = 15; // bucket 1 (10-19)
        items[2].score.final_total = 75; // bucket 7 (70-79)
        let histogram = build_histogram(&items);
        assert_eq!(histogram[1].count, 2);
        assert_eq!(histogram[7].count, 1);
    }

    // =========================================================================
    // score_popularity (composite)
    // =========================================================================

    #[test]
    fn popularity_capped_at_30() {
        let mut missing = BTreeSet::new();
        let signals = Signals {
            official_listing: Some(true), // 10
            github_stars: Some(10_000),   // 10
            marketplace: Some(MarketplaceSignals {
                rank: Some(1),        // 6
                featured: Some(true), // +2 (capped at 6)
                ..Default::default()
            }),
            references: (0..20).map(|i| format!("ref-{i}")).collect(), // 4
            ..Default::default()
        };
        let (total, _) = score_popularity(&signals, &mut missing);
        assert_eq!(total, 30); // capped
    }

    // =========================================================================
    // score_adoption (composite)
    // =========================================================================

    #[test]
    fn adoption_capped_at_15() {
        let mut missing = BTreeSet::new();
        let signals = Signals {
            npm_downloads_month: Some(100_000), // 8
            github_forks: Some(1_000),          // 2
            marketplace: Some(MarketplaceSignals {
                installs_month: Some(50_000), // 5
                ..Default::default()
            }),
            ..Default::default()
        };
        let (total, _) = score_adoption(&signals, &mut missing);
        assert_eq!(total, 15); // capped
    }

    // =========================================================================
    // score_coverage (composite)
    // =========================================================================

    #[test]
    fn coverage_capped_at_20() {
        let tags = Tags {
            runtime: Some("pkg-with-deps".to_string()), // 6
            interaction: vec![
                "provider".to_string(),
                "ui_integration".to_string(),
                "event_hook".to_string(),
                "slash_command".to_string(),
                "tool_only".to_string(),
            ], // 8
            capabilities: vec![
                "exec".to_string(),
                "http".to_string(),
                "read".to_string(),
                "ui".to_string(),
                "session".to_string(),
            ], // 6
        };
        let (total, _) = score_coverage(&tags);
        assert_eq!(total, 20); // capped
    }

    // =========================================================================
    // score_candidates (integration)
    // =========================================================================

    #[test]
    fn score_candidates_ranks_correctly() {
        let as_of = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let generated_at = as_of;
        let mut high = minimal_candidate("high");
        high.signals.github_stars = Some(10_000);
        high.signals.pi_mono_example = Some(true);
        high.compat.status = Some(CompatStatus::Unmodified);
        high.recency.updated_at = Some("2025-12-15T00:00:00Z".to_string());

        let low = minimal_candidate("low");

        let report = score_candidates(&[high, low], as_of, generated_at, 5);
        assert_eq!(report.schema, "pi.ext.scoring.v1");
        assert_eq!(report.items.len(), 2);
        assert_eq!(report.items[0].rank, 1);
        assert_eq!(report.items[1].rank, 2);
        assert!(report.items[0].score.final_total >= report.items[1].score.final_total);
    }

    #[test]
    fn score_candidates_empty_input() {
        let as_of = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let report = score_candidates(&[], as_of, as_of, 5);
        assert!(report.items.is_empty());
        assert!(report.summary.top_overall.is_empty());
    }

    #[test]
    fn score_candidates_do_not_reward_future_activity_timestamps() {
        let as_of = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let generated_at = as_of;
        let mut future = minimal_candidate("future");
        future.compat.status = Some(CompatStatus::Unmodified);
        future.gates.provenance_pinned = Some(true);
        future.gates.deterministic = Some(true);
        future.license.redistribution = Some(Redistribution::Ok);
        future.recency.updated_at = Some("2026-01-01T00:00:01Z".to_string());

        let mut present = future.clone();
        present.id = "present".to_string();
        present.recency.updated_at = Some("2025-12-31T23:59:59Z".to_string());

        let report = score_candidates(&[future, present], as_of, generated_at, 5);
        let future_item = report
            .items
            .iter()
            .find(|item| item.id == "future")
            .expect("future candidate present");
        let present_item = report
            .items
            .iter()
            .find(|item| item.id == "present")
            .expect("present candidate present");
        assert_eq!(future_item.score.activity, 0);
        assert!(
            future_item
                .missing_signals
                .contains(&"recency.updated_at".to_string())
        );
        assert!(present_item.score.activity > future_item.score.activity);
        assert!(present_item.score.final_total > future_item.score.final_total);
    }

    // =========================================================================
    // Serde round-trips
    // =========================================================================

    #[test]
    fn compat_status_serde_roundtrip() {
        for status in [
            CompatStatus::Unmodified,
            CompatStatus::RequiresShims,
            CompatStatus::RuntimeGap,
            CompatStatus::Blocked,
            CompatStatus::Unknown,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: CompatStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
    }

    #[test]
    fn redistribution_serde_roundtrip() {
        for red in [
            Redistribution::Ok,
            Redistribution::Restricted,
            Redistribution::Exclude,
            Redistribution::Unknown,
        ] {
            let json = serde_json::to_string(&red).unwrap();
            let back: Redistribution = serde_json::from_str(&json).unwrap();
            assert_eq!(back, red);
        }
    }

    #[test]
    fn risk_level_serde_roundtrip() {
        for level in [
            RiskLevel::Low,
            RiskLevel::Moderate,
            RiskLevel::High,
            RiskLevel::Critical,
        ] {
            let json = serde_json::to_string(&level).unwrap();
            let back: RiskLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(back, level);
        }
    }

    #[test]
    fn scoring_report_serde_roundtrip() {
        let as_of = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let mut candidate = minimal_candidate("test-ext");
        candidate.signals.github_stars = Some(500);
        candidate.compat.status = Some(CompatStatus::Unmodified);
        let report = score_candidates(&[candidate], as_of, as_of, 5);
        let json = serde_json::to_string(&report).unwrap();
        let back: ScoringReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.items.len(), 1);
        assert_eq!(back.items[0].id, "test-ext");
    }

    // =========================================================================
    // Missing signals tracking
    // =========================================================================

    #[test]
    fn missing_signals_collected_for_empty_candidate() {
        let as_of = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let candidate = minimal_candidate("bare");
        let scored = score_candidate(&candidate, as_of);
        assert!(!scored.missing_signals.is_empty());
        assert!(
            scored
                .missing_signals
                .contains(&"signals.github_stars".to_string())
        );
        assert!(
            scored
                .missing_signals
                .contains(&"signals.github_forks".to_string())
        );
        assert!(
            scored
                .missing_signals
                .contains(&"recency.updated_at".to_string())
        );
    }

    // =========================================================================
    // VOI planner
    // =========================================================================

    fn voi_candidate(
        id: &str,
        utility_score: f64,
        estimated_overhead_ms: u32,
        last_seen_at: Option<&str>,
    ) -> VoiCandidate {
        VoiCandidate {
            id: id.to_string(),
            utility_score,
            estimated_overhead_ms,
            last_seen_at: last_seen_at.map(std::string::ToString::to_string),
            enabled: true,
        }
    }

    #[test]
    fn voi_planner_budget_feasible_selection_and_skip_reason() {
        let now = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
        let config = VoiPlannerConfig {
            enabled: true,
            overhead_budget_ms: 9,
            max_candidates: None,
            stale_after_minutes: Some(180),
            min_utility_score: Some(0.0),
        };
        let candidates = vec![
            voi_candidate("fast-high", 9.0, 3, Some("2026-01-09T23:00:00Z")),
            voi_candidate("expensive", 12.0, 7, Some("2026-01-09T23:00:00Z")),
            voi_candidate("small", 4.0, 2, Some("2026-01-09T23:00:00Z")),
        ];

        let plan = plan_voi_candidates(&candidates, now, &config);
        assert_eq!(
            plan.selected
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec!["small", "expensive"]
        );
        assert_eq!(plan.used_overhead_ms, 9);
        assert_eq!(plan.remaining_overhead_ms, 0);
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].id, "fast-high");
        assert_eq!(plan.skipped[0].reason, VoiSkipReason::BudgetExceeded);
    }

    #[test]
    fn voi_planner_is_deterministic_across_input_order() {
        let now = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
        let config = VoiPlannerConfig {
            enabled: true,
            overhead_budget_ms: 20,
            max_candidates: Some(3),
            stale_after_minutes: Some(180),
            min_utility_score: Some(0.0),
        };
        let a = voi_candidate("a", 8.0, 2, Some("2026-01-09T23:00:00Z"));
        let b = voi_candidate("b", 8.0, 2, Some("2026-01-09T23:00:00Z"));
        let c = voi_candidate("c", 8.0, 2, Some("2026-01-09T23:00:00Z"));

        let plan_1 = plan_voi_candidates(&[a.clone(), b.clone(), c.clone()], now, &config);
        let plan_2 = plan_voi_candidates(&[c, a, b], now, &config);

        let ids_1 = plan_1
            .selected
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        let ids_2 = plan_2
            .selected
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(ids_1, ids_2);
    }

    #[test]
    fn voi_planner_rejects_stale_and_below_floor_candidates() {
        let now = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
        let config = VoiPlannerConfig {
            enabled: true,
            overhead_budget_ms: 20,
            max_candidates: None,
            stale_after_minutes: Some(60),
            min_utility_score: Some(5.0),
        };
        let candidates = vec![
            voi_candidate("fresh-good", 6.0, 2, Some("2026-01-09T23:30:00Z")),
            voi_candidate("stale", 7.0, 2, Some("2026-01-08T00:00:00Z")),
            voi_candidate("low-utility", 2.0, 1, Some("2026-01-09T23:30:00Z")),
            voi_candidate("missing-telemetry", 7.0, 1, None),
        ];

        let plan = plan_voi_candidates(&candidates, now, &config);
        assert_eq!(plan.selected.len(), 1);
        assert_eq!(plan.selected[0].id, "fresh-good");
        assert_eq!(plan.skipped.len(), 3);
        assert!(
            plan.skipped
                .iter()
                .any(|entry| entry.id == "stale" && entry.reason == VoiSkipReason::StaleEvidence)
        );
        assert!(plan.skipped.iter().any(|entry| {
            entry.id == "low-utility" && entry.reason == VoiSkipReason::BelowUtilityFloor
        }));
        assert!(plan.skipped.iter().any(|entry| {
            entry.id == "missing-telemetry" && entry.reason == VoiSkipReason::MissingTelemetry
        }));
    }

    #[test]
    fn voi_planner_rejects_future_telemetry_timestamps() {
        let now = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
        let config = VoiPlannerConfig {
            enabled: true,
            overhead_budget_ms: 20,
            max_candidates: None,
            stale_after_minutes: Some(60),
            min_utility_score: Some(0.0),
        };
        let candidates = vec![
            voi_candidate("fresh", 6.0, 2, Some("2026-01-09T23:30:00Z")),
            voi_candidate("future", 99.0, 1, Some("2026-01-10T01:00:00Z")),
        ];

        let plan = plan_voi_candidates(&candidates, now, &config);
        assert_eq!(plan.selected.len(), 1);
        assert_eq!(plan.selected[0].id, "fresh");
        assert!(plan.skipped.iter().any(|entry| {
            entry.id == "future" && entry.reason == VoiSkipReason::MissingTelemetry
        }));
    }

    #[test]
    fn voi_planner_applies_staleness_at_second_precision() {
        let now = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
        let config = VoiPlannerConfig {
            enabled: true,
            overhead_budget_ms: 20,
            max_candidates: None,
            stale_after_minutes: Some(1),
            min_utility_score: Some(0.0),
        };
        let candidates = vec![
            voi_candidate("fresh", 6.0, 2, Some("2026-01-09T23:59:00Z")),
            voi_candidate("stale-by-seconds", 7.0, 2, Some("2026-01-09T23:58:59Z")),
        ];

        let plan = plan_voi_candidates(&candidates, now, &config);
        assert_eq!(plan.selected.len(), 1);
        assert_eq!(plan.selected[0].id, "fresh");
        assert!(plan.skipped.iter().any(|entry| {
            entry.id == "stale-by-seconds" && entry.reason == VoiSkipReason::StaleEvidence
        }));
    }

    #[test]
    fn voi_planner_fails_closed_when_staleness_window_overflows() {
        let now = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
        let config = VoiPlannerConfig {
            enabled: true,
            overhead_budget_ms: 20,
            max_candidates: None,
            stale_after_minutes: Some(i64::MAX),
            min_utility_score: Some(0.0),
        };
        let candidates = vec![voi_candidate("fresh", 6.0, 2, Some("2026-01-09T23:59:00Z"))];

        let plan = plan_voi_candidates(&candidates, now, &config);
        assert!(plan.selected.is_empty());
        assert!(
            plan.skipped.iter().any(|entry| {
                entry.id == "fresh" && entry.reason == VoiSkipReason::StaleEvidence
            })
        );
    }

    #[test]
    fn voi_planner_disabled_returns_no_selection() {
        let now = Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap();
        let config = VoiPlannerConfig {
            enabled: false,
            overhead_budget_ms: 20,
            max_candidates: None,
            stale_after_minutes: Some(120),
            min_utility_score: Some(0.0),
        };
        let candidates = vec![voi_candidate("a", 8.0, 2, Some("2026-01-09T23:00:00Z"))];
        let plan = plan_voi_candidates(&candidates, now, &config);
        assert!(plan.selected.is_empty());
        assert_eq!(plan.used_overhead_ms, 0);
        assert_eq!(plan.remaining_overhead_ms, 20);
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].reason, VoiSkipReason::Disabled);
    }

    fn ope_sample(
        action: &str,
        behavior_propensity: f64,
        target_propensity: f64,
        outcome: f64,
    ) -> OpeTraceSample {
        OpeTraceSample {
            action: action.to_string(),
            behavior_propensity,
            target_propensity,
            outcome,
            baseline_outcome: Some(outcome),
            direct_method_prediction: Some(outcome),
            context_lineage: Some(format!("ctx:{action}")),
        }
    }

    fn assert_close(left: f64, right: f64, epsilon: f64) {
        assert!(
            (left - right).abs() <= epsilon,
            "values differ: left={left}, right={right}, epsilon={epsilon}"
        );
    }

    #[test]
    fn ope_matches_ground_truth_when_behavior_equals_target() {
        let config = OpeEvaluatorConfig {
            max_importance_weight: 50.0,
            min_effective_sample_size: 1.0,
            max_standard_error: 10.0,
            confidence_z: 1.96,
            max_regret_delta: 1.0,
        };
        let samples = vec![
            ope_sample("a", 0.5, 0.5, 1.0),
            ope_sample("a", 0.5, 0.5, 0.0),
            ope_sample("a", 0.5, 0.5, 1.0),
            ope_sample("a", 0.5, 0.5, 1.0),
            ope_sample("a", 0.5, 0.5, 0.0),
            ope_sample("a", 0.5, 0.5, 1.0),
        ];

        let report = evaluate_off_policy(&samples, &config);
        let expected_mean = 4.0 / 6.0;
        assert_close(report.ips.estimate, expected_mean, 1e-9);
        assert_close(report.wis.estimate, expected_mean, 1e-9);
        assert_close(report.doubly_robust.estimate, expected_mean, 1e-9);
        assert_eq!(report.gate.reason, OpeGateReason::Approved);
        assert!(report.gate.passed);
    }

    #[test]
    fn ope_fails_closed_under_extreme_propensity_skew() {
        let config = OpeEvaluatorConfig {
            max_importance_weight: 100.0,
            min_effective_sample_size: 4.0,
            max_standard_error: 10.0,
            confidence_z: 1.96,
            max_regret_delta: 10.0,
        };
        let mut samples = vec![ope_sample("candidate", 0.02, 1.0, 0.0)];
        for _ in 0..9 {
            samples.push(ope_sample("candidate", 1.0, 0.02, 1.0));
        }

        let report = evaluate_off_policy(&samples, &config);
        assert!(report.diagnostics.effective_sample_size < 2.0);
        assert_eq!(report.gate.reason, OpeGateReason::InsufficientSupport);
        assert!(!report.gate.passed);
    }

    #[test]
    fn ope_fails_closed_when_no_valid_samples_exist() {
        let config = OpeEvaluatorConfig::default();
        let samples = vec![
            ope_sample("invalid", 0.0, 0.5, 1.0),
            ope_sample("invalid", -1.0, 0.5, 1.0),
        ];

        let report = evaluate_off_policy(&samples, &config);
        assert_eq!(report.diagnostics.valid_samples, 0);
        assert_eq!(report.gate.reason, OpeGateReason::NoValidSamples);
        assert!(!report.gate.passed);
    }

    #[test]
    fn ope_skips_samples_whose_derived_effects_overflow() {
        let config = OpeEvaluatorConfig {
            max_importance_weight: 1.0,
            min_effective_sample_size: 1.0,
            max_standard_error: 10.0,
            confidence_z: 1.96,
            max_regret_delta: 10.0,
        };
        let samples = vec![OpeTraceSample {
            action: "overflow".to_string(),
            behavior_propensity: 1.0,
            target_propensity: 1.0,
            outcome: 1.0e308,
            baseline_outcome: Some(1.0e308),
            direct_method_prediction: Some(-1.0e308),
            context_lineage: Some("ctx:overflow".to_string()),
        }];

        let report = evaluate_off_policy(&samples, &config);
        assert_eq!(report.diagnostics.valid_samples, 0);
        assert_eq!(report.diagnostics.skipped_invalid_samples, 1);
        assert_eq!(report.gate.reason, OpeGateReason::NoValidSamples);
        assert!(!report.gate.passed);
        serde_json::to_string(&report).expect("report must remain serializable");
    }

    #[test]
    fn ope_is_stable_across_input_order() {
        let config = OpeEvaluatorConfig {
            max_importance_weight: 50.0,
            min_effective_sample_size: 1.0,
            max_standard_error: 10.0,
            confidence_z: 1.96,
            max_regret_delta: 10.0,
        };
        let samples = vec![
            ope_sample("a", 0.40, 0.30, 0.2),
            ope_sample("a", 0.50, 0.60, 0.8),
            ope_sample("a", 0.70, 0.20, 0.1),
            ope_sample("a", 0.30, 0.50, 0.7),
        ];
        let mut reversed = samples.clone();
        reversed.reverse();

        let original = evaluate_off_policy(&samples, &config);
        let swapped = evaluate_off_policy(&reversed, &config);
        assert_close(original.ips.estimate, swapped.ips.estimate, 1e-12);
        assert_close(original.wis.estimate, swapped.wis.estimate, 1e-12);
        assert_close(
            original.doubly_robust.estimate,
            swapped.doubly_robust.estimate,
            1e-12,
        );
        assert_eq!(original.gate.reason, swapped.gate.reason);
        assert_close(
            original.diagnostics.effective_sample_size,
            swapped.diagnostics.effective_sample_size,
            1e-12,
        );
    }

    #[test]
    fn ope_fails_closed_when_wis_effects_overflow() {
        let config = OpeEvaluatorConfig {
            max_importance_weight: 1.0,
            min_effective_sample_size: 1.0,
            max_standard_error: 10.0,
            confidence_z: 1.96,
            max_regret_delta: 10.0,
        };
        let samples = vec![
            OpeTraceSample {
                action: "dominant".to_string(),
                behavior_propensity: 1.0,
                target_propensity: 1.0,
                outcome: 1.0e308,
                baseline_outcome: Some(1.0e308),
                direct_method_prediction: Some(1.0e308),
                context_lineage: Some("ctx:dominant".to_string()),
            },
            OpeTraceSample {
                action: "tiny".to_string(),
                behavior_propensity: 1.0,
                target_propensity: 1.0e-308,
                outcome: 0.0,
                baseline_outcome: Some(0.0),
                direct_method_prediction: Some(0.0),
                context_lineage: Some("ctx:tiny".to_string()),
            },
        ];

        let report = evaluate_off_policy(&samples, &config);
        assert_eq!(report.gate.reason, OpeGateReason::HighUncertainty);
        assert!(!report.gate.passed);
        assert!(report.wis.standard_error >= f64::MAX / 2.0);
        serde_json::to_string(&report).expect("report must remain serializable");
    }

    fn mean_field_observation(
        shard_id: &str,
        queue_pressure: f64,
        tail_latency_ratio: f64,
        starvation_risk: f64,
    ) -> MeanFieldShardObservation {
        MeanFieldShardObservation {
            shard_id: shard_id.to_string(),
            queue_pressure,
            tail_latency_ratio,
            starvation_risk,
        }
    }

    fn mean_field_state(
        shard_id: &str,
        routing_weight: f64,
        batch_budget: u32,
        last_routing_delta: f64,
    ) -> MeanFieldShardState {
        MeanFieldShardState {
            shard_id: shard_id.to_string(),
            routing_weight,
            batch_budget,
            help_factor: 1.0,
            backoff_factor: 1.0,
            last_routing_delta,
        }
    }

    #[test]
    fn mean_field_controls_are_deterministic_across_input_order() {
        let config = MeanFieldControllerConfig::default();
        let observations = vec![
            mean_field_observation("shard-b", 0.7, 1.3, 0.1),
            mean_field_observation("shard-a", 0.3, 1.1, 0.2),
            mean_field_observation("shard-c", 0.5, 1.0, 0.4),
        ];
        let previous = vec![
            mean_field_state("shard-c", 1.2, 24, 0.0),
            mean_field_state("shard-a", 0.9, 18, 0.0),
            mean_field_state("shard-b", 1.1, 20, 0.0),
        ];

        let baseline = compute_mean_field_controls(&observations, &previous, &config);
        let reversed_observations = observations.iter().rev().cloned().collect::<Vec<_>>();
        let reversed_previous = previous.iter().rev().cloned().collect::<Vec<_>>();
        let reversed =
            compute_mean_field_controls(&reversed_observations, &reversed_previous, &config);

        assert_eq!(
            baseline
                .controls
                .iter()
                .map(|control| control.shard_id.as_str())
                .collect::<Vec<_>>(),
            vec!["shard-a", "shard-b", "shard-c"]
        );
        assert_eq!(
            baseline
                .controls
                .iter()
                .map(|control| control.shard_id.clone())
                .collect::<Vec<_>>(),
            reversed
                .controls
                .iter()
                .map(|control| control.shard_id.clone())
                .collect::<Vec<_>>()
        );
        assert_close(baseline.global_pressure, reversed.global_pressure, 1e-12);
    }

    #[test]
    fn mean_field_clips_unstable_steps_to_max_step() {
        let config = MeanFieldControllerConfig {
            max_step: 0.05,
            ..MeanFieldControllerConfig::default()
        };
        let observations = vec![mean_field_observation("shard-a", 1.0, 2.0, 0.0)];
        let previous = vec![mean_field_state("shard-a", 1.0, 32, 0.0)];

        let report = compute_mean_field_controls(&observations, &previous, &config);
        assert_eq!(report.controls.len(), 1);
        let control = &report.controls[0];
        assert!(control.clipped);
        assert!(control.routing_delta.abs() <= config.max_step + 1e-12);
        assert!(control.stability_margin <= config.max_step + 1e-12);
    }

    #[test]
    fn mean_field_oscillation_guard_reduces_sign_flip_delta() {
        let config = MeanFieldControllerConfig {
            max_step: 0.30,
            ..MeanFieldControllerConfig::default()
        };
        let observations = vec![mean_field_observation("shard-a", 1.0, 1.7, 0.0)];
        let previous = vec![mean_field_state("shard-a", 1.2, 20, 0.12)];

        let report = compute_mean_field_controls(&observations, &previous, &config);
        let control = &report.controls[0];
        assert!(control.oscillation_guarded);
        assert!(report.oscillation_guard_count >= 1);
    }

    #[test]
    fn mean_field_marks_converged_for_small_average_delta() {
        let config = MeanFieldControllerConfig {
            queue_gain: 0.0,
            latency_gain: 0.0,
            starvation_gain: 0.0,
            damping: 1.0,
            convergence_epsilon: 0.05,
            ..MeanFieldControllerConfig::default()
        };
        let observations = vec![
            mean_field_observation("shard-a", 0.40, 1.0, 0.1),
            mean_field_observation("shard-b", 0.42, 1.0, 0.1),
        ];
        let previous = vec![
            mean_field_state("shard-a", 1.0, 24, 0.0),
            mean_field_state("shard-b", 1.0, 24, 0.0),
        ];

        let report = compute_mean_field_controls(&observations, &previous, &config);
        assert!(report.converged);
    }

    #[test]
    fn mean_field_sanitizes_nonfinite_previous_state() {
        let config = MeanFieldControllerConfig::default();
        let observations = vec![mean_field_observation("shard-a", 0.8, 1.5, 0.2)];
        let previous = vec![MeanFieldShardState {
            shard_id: "shard-a".to_string(),
            routing_weight: f64::NAN,
            batch_budget: 0,
            help_factor: f64::INFINITY,
            backoff_factor: f64::NEG_INFINITY,
            last_routing_delta: f64::NAN,
        }];

        let report = compute_mean_field_controls(&observations, &previous, &config);
        let control = &report.controls[0];
        assert!(report.global_pressure.is_finite());
        assert!(control.routing_weight.is_finite());
        assert!(control.routing_delta.is_finite());
        assert!(control.stability_margin.is_finite());
        assert!(control.routing_weight >= config.min_routing_weight);
        assert!(control.routing_weight <= config.max_routing_weight);
        assert!(control.batch_budget >= config.min_batch_budget);
        assert!(control.batch_budget <= config.max_batch_budget);
    }

    #[test]
    fn mean_field_quantizes_wide_batch_budget_range_without_scanning() {
        assert_eq!(quantize_batch_budget(10.49, 1, u32::MAX), 10);
        assert_eq!(quantize_batch_budget(10.50, 1, u32::MAX), 10);
        assert_eq!(quantize_batch_budget(10.51, 1, u32::MAX), 11);
        assert_eq!(quantize_batch_budget(f64::INFINITY, 7, u32::MAX), 7);
    }

    // ── Property tests ──

    mod proptest_scoring {
        use super::*;
        use proptest::prelude::*;

        fn arb_signals() -> impl Strategy<Value = Signals> {
            (
                any::<Option<bool>>(),
                any::<Option<bool>>(),
                any::<Option<bool>>(),
                prop::option::of(0..100_000u64),
                prop::option::of(0..10_000u64),
                prop::option::of(0..1_000_000u64),
            )
                .prop_map(|(listing, example, gist, stars, forks, npm)| Signals {
                    official_listing: listing,
                    pi_mono_example: example,
                    badlogic_gist: gist,
                    github_stars: stars,
                    github_forks: forks,
                    npm_downloads_month: npm,
                    references: Vec::new(),
                    marketplace: None,
                })
        }

        fn arb_tags() -> impl Strategy<Value = Tags> {
            let runtime = prop::option::of(prop::sample::select(vec![
                "pkg-with-deps".to_string(),
                "provider-ext".to_string(),
                "multi-file".to_string(),
                "legacy-js".to_string(),
            ]));
            runtime.prop_map(|rt| Tags {
                runtime: rt,
                interaction: Vec::new(),
                capabilities: Vec::new(),
            })
        }

        fn arb_compat_status() -> impl Strategy<Value = CompatStatus> {
            prop::sample::select(vec![
                CompatStatus::Unmodified,
                CompatStatus::RequiresShims,
                CompatStatus::RuntimeGap,
                CompatStatus::Blocked,
                CompatStatus::Unknown,
            ])
        }

        fn arb_risk_level() -> impl Strategy<Value = RiskLevel> {
            prop::sample::select(vec![
                RiskLevel::Low,
                RiskLevel::Moderate,
                RiskLevel::High,
                RiskLevel::Critical,
            ])
        }

        proptest! {
            #[test]
            fn score_github_stars_bounded(stars in 0..10_000_000u64) {
                let signals = Signals {
                    github_stars: Some(stars),
                    ..Signals::default()
                };
                let mut missing = BTreeSet::new();
                let score = score_github_stars(&signals, &mut missing);
                assert!(score <= 10, "github_stars score {score} exceeds max 10");
            }

            #[test]
            fn score_github_stars_monotonic(a in 0..100_000u64, b in 0..100_000u64) {
                let mut missing = BTreeSet::new();
                let sig_a = Signals { github_stars: Some(a), ..Signals::default() };
                let sig_b = Signals { github_stars: Some(b), ..Signals::default() };
                let score_a = score_github_stars(&sig_a, &mut missing);
                let score_b = score_github_stars(&sig_b, &mut missing);
                if a <= b {
                    assert!(
                        score_a <= score_b,
                        "monotonicity: stars {a} → {score_a}, {b} → {score_b}"
                    );
                }
            }

            #[test]
            fn score_npm_downloads_bounded(downloads in 0..10_000_000u64) {
                let signals = Signals {
                    npm_downloads_month: Some(downloads),
                    ..Signals::default()
                };
                let mut missing = BTreeSet::new();
                let score = score_npm_downloads(&signals, &mut missing);
                assert!(score <= 8, "npm_downloads score {score} exceeds max 8");
            }

            #[test]
            fn score_compatibility_bounded(
                status in arb_compat_status(),
                adjustment in -20..20i8,
            ) {
                let compat = Compatibility {
                    status: Some(status),
                    blocked_reasons: Vec::new(),
                    required_shims: Vec::new(),
                    adjustment: Some(adjustment),
                };
                let score = score_compatibility(&compat);
                assert!(score <= 20, "compatibility score {score} exceeds max 20");
            }

            #[test]
            fn score_risk_bounded(
                level in prop::option::of(arb_risk_level()),
                penalty in prop::option::of(0..100u8),
            ) {
                let risk = RiskInfo { level, penalty, flags: Vec::new() };
                let score = score_risk(&risk);
                assert!(score <= 15, "risk penalty {score} exceeds max 15");
            }

            #[test]
            fn normalized_utility_nonnegative(value in prop::num::f64::ANY) {
                let result = normalized_utility(value);
                assert!(
                    result >= 0.0,
                    "normalized_utility({value}) = {result} must be >= 0.0"
                );
            }

            #[test]
            fn normalized_utility_handles_special_floats(
                value in prop::sample::select(vec![
                    f64::NAN, f64::INFINITY, f64::NEG_INFINITY,
                    0.0, -0.0, f64::MIN, f64::MAX, f64::MIN_POSITIVE,
                ]),
            ) {
                let result = normalized_utility(value);
                assert!(
                    result.is_finite(),
                    "normalized_utility({value}) = {result} must be finite"
                );
                assert!(result >= 0.0, "must be non-negative");
            }

            #[test]
            fn normalized_utility_idempotent(value in prop::num::f64::ANY) {
                let once = normalized_utility(value);
                let twice = normalized_utility(once);
                assert!(
                    (once - twice).abs() < f64::EPSILON || (once.is_nan() && twice.is_nan()),
                    "normalized_utility must be idempotent: {once} vs {twice}"
                );
            }

            #[test]
            fn base_total_is_sum_of_components(
                signals in arb_signals(),
                tags in arb_tags(),
            ) {
                let mut missing = BTreeSet::new();
                let (popularity, _) = score_popularity(&signals, &mut missing);
                let (adoption, _) = score_adoption(&signals, &mut missing);
                let (coverage, _) = score_coverage(&tags);
                // Each clamped component is bounded
                assert!(popularity <= 30, "popularity {popularity} > 30");
                assert!(adoption <= 15, "adoption {adoption} > 15");
                assert!(coverage <= 20, "coverage {coverage} > 20");
            }

            #[test]
            fn gates_passes_iff_all_true(
                prov in any::<bool>(),
                det in any::<bool>(),
                license_ok in any::<bool>(),
                unmod in any::<bool>(),
            ) {
                let expected = prov && det && license_ok && unmod;
                let gates = GateStatus {
                    provenance_pinned: prov,
                    license_ok,
                    deterministic: det,
                    unmodified: unmod,
                    passes: expected,
                };
                assert!(
                    gates.passes == expected,
                    "gates.passes should be AND of all flags"
                );
            }
        }
    }
}
