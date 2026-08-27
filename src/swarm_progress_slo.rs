//! Deterministic, read-only swarm progress SLO evaluator.
//!
//! The evaluator consumes already-normalized progress sources and emits
//! `pi.swarm.progress_slo.v1`. It never reads files, mutates Beads, sends
//! Agent Mail, starts RCH work, or changes git state.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Schema emitted by progress SLO reports.
pub const SWARM_PROGRESS_SLO_SCHEMA: &str = "pi.swarm.progress_slo.v1";

/// Contract version implemented by this evaluator.
pub const SWARM_PROGRESS_SLO_CONTRACT_VERSION: &str = "1.0.0";

/// Schema emitted by synthetic stress-budget verdicts for the progress SLO evaluator.
pub const SWARM_PROGRESS_SLO_STRESS_BUDGET_SCHEMA: &str = "pi.swarm.progress_slo.stress_budget.v1";

/// Required caveat for synthetic stress-budget evidence.
pub const SWARM_PROGRESS_SLO_STRESS_BUDGET_CAVEAT: &str =
    "engineering_signal_only_not_benchmark_release_or_dropin_claim_support";

const REASON_BEAD_CLOSEOUT: &str = "PROGRESS-SLO-BEAD-CLOSEOUT";
const REASON_GIT_COMMIT_DELTA: &str = "PROGRESS-SLO-GIT-COMMIT-DELTA";
const REASON_NO_READY_WORK: &str = "PROGRESS-SLO-NO-READY-WORK";
const REASON_STALE_IN_PROGRESS: &str = "PROGRESS-SLO-STALE-IN-PROGRESS";
const REASON_AGENT_MAIL_DEGRADED: &str = "PROGRESS-SLO-AGENT-MAIL-DEGRADED";
const REASON_RCH_SATURATED: &str = "PROGRESS-SLO-RCH-SATURATED";
const REASON_VALIDATION_BROKER_SATURATED: &str = "PROGRESS-SLO-VALIDATION-BROKER-SATURATED";
const REASON_MALFORMED_SOURCE: &str = "PROGRESS-SLO-MALFORMED-SOURCE";
const REASON_MISSING_AUTHORITY: &str = "PROGRESS-SLO-MISSING-AUTHORITY";
const REASON_CONVERGED_NO_OPEN_WORK: &str = "PROGRESS-SLO-CONVERGED-NO-OPEN-WORK";
const REASON_STRESS_BUDGET_EXCEEDED: &str = "PROGRESS-SLO-STRESS-BUDGET-EXCEEDED";
const REASON_STRESS_CACHE_MISSING_OR_MISMATCHED: &str =
    "PROGRESS-SLO-STRESS-CACHE-MISSING-OR-MISMATCHED";
const REASON_STRESS_HOST_BELOW_FLOOR: &str = "PROGRESS-SLO-STRESS-HOST-BELOW-FLOOR";
const REASON_STRESS_MISSING_CAVEAT: &str = "PROGRESS-SLO-STRESS-MISSING-CAVEAT";
const REASON_STRESS_MISSING_MEASUREMENT: &str = "PROGRESS-SLO-STRESS-MISSING-MEASUREMENT";
const REASON_STRESS_MISSING_PROVENANCE: &str = "PROGRESS-SLO-STRESS-MISSING-PROVENANCE";
const REASON_STRESS_SOURCE_DEGRADED: &str = "PROGRESS-SLO-STRESS-SOURCE-DEGRADED";

const LARGE_HOST_STRESS_MIN_CPU_CORES: u16 = 64;
const LARGE_HOST_STRESS_MIN_MEMORY_GIB: u16 = 256;
const LARGE_HOST_STRESS_MAX_SOURCE_RECORDS: u64 = 100_000;
const LARGE_HOST_STRESS_MAX_OPEN_BEADS: u64 = 25_000;
const LARGE_HOST_STRESS_MAX_IN_PROGRESS_BEADS: u64 = 5_000;
const LARGE_HOST_STRESS_MAX_READY_BEADS: u64 = 10_000;
const LARGE_HOST_STRESS_MAX_RCH_QUEUE_DEPTH: u64 = 512;
const LARGE_HOST_STRESS_MAX_VALIDATION_SLOTS: u64 = 1_024;
const LARGE_HOST_STRESS_MAX_EVALUATION_BUDGET_UNITS: u64 = 1_000_000;

const REQUIRED_SOURCE_IDS: &[&str] = &[
    "beads_active_delta",
    "beads_closed_delta",
    "git_commit_delta",
    "rch_posture",
    "validation_broker_posture",
    "agent_mail_health",
    "operator_runpack_summary",
    "swarm_autopilot_summary",
    "context_intelligence_summary",
    "operator_time_window",
];

const PROGRESSING_AUTHORITY_SOURCE_IDS: &[&str] = &[
    "operator_time_window",
    "beads_active_delta",
    "beads_closed_delta",
    "git_commit_delta",
];

/// Top-level status for a progress SLO report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressSloStatus {
    Progressing,
    QuietBlocked,
    CoordinationDegraded,
    BuildSaturated,
    Stalled,
    ConvergedNoOpenWork,
    MalformedSourceDegraded,
    InsufficientEvidenceDegraded,
}

/// Availability state for one normalized progress source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAvailability {
    Available,
    Unavailable,
    Partial,
    Malformed,
    Stale,
    NotConfigured,
}

impl SourceAvailability {
    const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }

    const fn is_malformed(self) -> bool {
        matches!(self, Self::Malformed)
    }

    const fn is_degraded(self) -> bool {
        !self.is_available()
    }
}

/// Freshness state for one normalized progress source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessState {
    Current,
    Stale,
    Missing,
    Malformed,
    FreshnessUnknown,
}

impl FreshnessState {
    const fn is_current(self) -> bool {
        matches!(self, Self::Current)
    }

    const fn is_malformed(self) -> bool {
        matches!(self, Self::Malformed)
    }

    const fn is_degraded(self) -> bool {
        !self.is_current()
    }
}

/// Redaction state for one normalized progress source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionState {
    None,
    Redacted,
    SensitiveOmitted,
    UnsafeToEmit,
}

/// Health posture projected from Agent Mail evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMailHealth {
    Green,
    Yellow,
    Red,
    Corrupt,
    Unavailable,
    Unknown,
}

impl AgentMailHealth {
    const fn is_degraded(self) -> bool {
        !matches!(self, Self::Green)
    }
}

/// Posture projected from RCH queue and worker evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RchPosture {
    Green,
    Queueing,
    Saturated,
    FailOpenLocalRisk,
    Unavailable,
    Unknown,
}

impl RchPosture {
    const fn is_saturated(self) -> bool {
        matches!(
            self,
            Self::Saturated | Self::FailOpenLocalRisk | Self::Unavailable
        )
    }
}

/// Posture projected from validation broker evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationBrokerPosture {
    Green,
    Queueing,
    Saturated,
    StaleSlots,
    Unavailable,
    Unknown,
}

impl ValidationBrokerPosture {
    const fn is_saturated(self) -> bool {
        matches!(self, Self::Saturated | Self::StaleSlots | Self::Unavailable)
    }
}

/// Dimension status used by the saturation summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DimensionStatus {
    Green,
    Yellow,
    Red,
    Unknown,
}

/// Advisory operator posture for the next action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedOperatorPosture {
    ContinueCurrentSwarm,
    NarrowValidationScope,
    BackoffHeavyCargo,
    RepairCoordinationTooling,
    GenerateNewBeads,
    HandoffForHumanTriage,
}

/// Observation window used for the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressSloTimeWindow {
    pub start_utc: String,
    pub end_utc: String,
    pub duration_seconds: u64,
    pub comparison_baseline: String,
}

impl ProgressSloTimeWindow {
    #[must_use]
    pub fn new(
        start_utc: impl Into<String>,
        end_utc: impl Into<String>,
        duration_seconds: u64,
        comparison_baseline: impl Into<String>,
    ) -> Self {
        Self {
            start_utc: start_utc.into(),
            end_utc: end_utc.into(),
            duration_seconds,
            comparison_baseline: comparison_baseline.into(),
        }
    }

    fn is_valid(&self) -> bool {
        self.duration_seconds > 0
            && !self.start_utc.trim().is_empty()
            && !self.end_utc.trim().is_empty()
            && !self.comparison_baseline.trim().is_empty()
    }
}

/// One normalized source row consumed by the evaluator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressSloSourceStatus {
    pub source_id: String,
    pub source_class: String,
    pub source_kind: String,
    pub path: Option<String>,
    pub availability: SourceAvailability,
    pub freshness_state: FreshnessState,
    pub observed_at_utc: Option<String>,
    pub source_hash: Option<String>,
    pub authoritative_for: Vec<String>,
    pub redaction_state: RedactionState,
    pub degraded_reasons: Vec<String>,
    pub suppressed_claims: Vec<String>,
}

impl ProgressSloSourceStatus {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        source_id: impl Into<String>,
        source_class: impl Into<String>,
        source_kind: impl Into<String>,
        availability: SourceAvailability,
        freshness_state: FreshnessState,
        redaction_state: RedactionState,
        authoritative_for: Vec<String>,
    ) -> Self {
        Self {
            source_id: source_id.into(),
            source_class: source_class.into(),
            source_kind: source_kind.into(),
            path: None,
            availability,
            freshness_state,
            observed_at_utc: None,
            source_hash: None,
            authoritative_for,
            redaction_state,
            degraded_reasons: Vec::new(),
            suppressed_claims: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    #[must_use]
    pub fn with_observed_at(mut self, observed_at_utc: impl Into<String>) -> Self {
        self.observed_at_utc = Some(observed_at_utc.into());
        self
    }

    #[must_use]
    pub fn with_source_hash(mut self, source_hash: impl Into<String>) -> Self {
        self.source_hash = Some(source_hash.into());
        self
    }

    #[must_use]
    pub fn with_degraded_reason(mut self, reason: impl Into<String>) -> Self {
        self.degraded_reasons.push(reason.into());
        self
    }

    #[must_use]
    pub fn with_suppressed_claim(mut self, claim: impl Into<String>) -> Self {
        self.suppressed_claims.push(claim.into());
        self
    }

    const fn is_malformed(&self) -> bool {
        self.availability.is_malformed() || self.freshness_state.is_malformed()
    }

    const fn is_degraded(&self) -> bool {
        self.availability.is_degraded() || self.freshness_state.is_degraded()
    }

    const fn is_currently_available(&self) -> bool {
        self.availability.is_available() && self.freshness_state.is_current()
    }
}

/// Aggregate progress counters over the requested time window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressSloMetrics {
    pub closed_beads: u64,
    pub open_beads: u64,
    pub in_progress_beads: u64,
    pub ready_beads: u64,
    pub dependency_blocked_beads: u64,
    pub commits: u64,
    pub pushed_commits: u64,
    pub closed_with_commit_reference_count: u64,
    pub validation_passes: u64,
    pub validation_failures: u64,
    pub agent_mail_health: AgentMailHealth,
    pub rch_posture: RchPosture,
    pub rch_queue_depth: u64,
    pub rch_queue_saturation_threshold: u64,
    pub validation_broker_posture: ValidationBrokerPosture,
    pub stale_in_progress_candidates: u64,
    pub malformed_source_records: u64,
    pub contradictory_source_records: u64,
}

impl Default for ProgressSloMetrics {
    fn default() -> Self {
        Self {
            closed_beads: 0,
            open_beads: 0,
            in_progress_beads: 0,
            ready_beads: 0,
            dependency_blocked_beads: 0,
            commits: 0,
            pushed_commits: 0,
            closed_with_commit_reference_count: 0,
            validation_passes: 0,
            validation_failures: 0,
            agent_mail_health: AgentMailHealth::Unknown,
            rch_posture: RchPosture::Unknown,
            rch_queue_depth: 0,
            rch_queue_saturation_threshold: 1,
            validation_broker_posture: ValidationBrokerPosture::Unknown,
            stale_in_progress_candidates: 0,
            malformed_source_records: 0,
            contradictory_source_records: 0,
        }
    }
}

/// Saturation dimensions in the emitted report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressSloSaturationSummary {
    pub coordination_saturation: DimensionStatus,
    pub build_saturation: DimensionStatus,
    pub validation_saturation: DimensionStatus,
    pub queue_convergence: DimensionStatus,
    pub recommended_operator_posture: RecommendedOperatorPosture,
}

/// Redaction accounting in the emitted report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressSloRedactionSummary {
    pub redacted_count: u64,
    pub omitted_count: u64,
    pub unsafe_to_emit_count: u64,
    pub suppressed_claims: Vec<String>,
}

/// Pure evaluator input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressSloEvaluationInput {
    pub generated_at: String,
    pub time_window: ProgressSloTimeWindow,
    pub source_statuses: Vec<ProgressSloSourceStatus>,
    pub progress_metrics: ProgressSloMetrics,
}

impl ProgressSloEvaluationInput {
    #[must_use]
    pub fn new(
        generated_at: impl Into<String>,
        time_window: ProgressSloTimeWindow,
        source_statuses: Vec<ProgressSloSourceStatus>,
        progress_metrics: ProgressSloMetrics,
    ) -> Self {
        Self {
            generated_at: generated_at.into(),
            time_window,
            source_statuses,
            progress_metrics,
        }
    }
}

/// Emitted progress SLO report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgressSloReport {
    pub schema: String,
    pub generated_at: String,
    pub contract_version: String,
    pub time_window: ProgressSloTimeWindow,
    pub status: ProgressSloStatus,
    pub confidence: f64,
    pub reason_ids: Vec<String>,
    pub source_statuses: Vec<ProgressSloSourceStatus>,
    pub progress_metrics: ProgressSloMetrics,
    pub saturation_summary: ProgressSloSaturationSummary,
    pub redaction_summary: ProgressSloRedactionSummary,
    pub suppressed_claims: Vec<String>,
    pub next_actions: Vec<String>,
}

/// Deterministic synthetic profile class used for large-host stress budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressSloStressScenario {
    Nominal,
    Saturated,
    HugeHistory,
    MissingData,
}

impl ProgressSloStressScenario {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Nominal => "nominal",
            Self::Saturated => "saturated",
            Self::HugeHistory => "huge_history",
            Self::MissingData => "missing_data",
        }
    }

    const fn profile_id(self) -> &'static str {
        match self {
            Self::Nominal => "large_host_nominal",
            Self::Saturated => "large_host_saturated",
            Self::HugeHistory => "large_host_huge_history",
            Self::MissingData => "large_host_missing_data",
        }
    }
}

/// Synthetic progress-SLO stress profile for large swarm hosts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressSloStressProfile {
    pub profile_id: String,
    pub scenario: ProgressSloStressScenario,
    pub host_cpu_cores: u16,
    pub host_memory_gib: u16,
    pub source_record_count: u64,
    pub validation_slot_count: u64,
    pub expected_status: ProgressSloStatus,
    pub progress_input: ProgressSloEvaluationInput,
}

/// Budget envelope for evaluating progress-SLO stress profiles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressSloStressBudget {
    pub min_host_cpu_cores: u16,
    pub min_host_memory_gib: u16,
    pub max_source_records: u64,
    pub max_open_beads: u64,
    pub max_in_progress_beads: u64,
    pub max_ready_beads: u64,
    pub max_rch_queue_depth: u64,
    pub max_validation_slots: u64,
    pub max_evaluation_budget_units: u64,
    pub required_caveat: String,
}

/// Provenance attached to a synthetic stress-budget measurement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressSloStressProvenance {
    pub profile_id: String,
    pub scenario: ProgressSloStressScenario,
    pub generated_at: String,
    pub generated_by: String,
    pub source_profile_fingerprint: String,
    pub synthetic: bool,
    pub host_cpu_cores: u16,
    pub host_memory_gib: u16,
    pub caveats: Vec<String>,
}

/// Deterministic measurement proxy for a progress-SLO stress profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressSloStressMeasurement {
    pub evaluated_source_records: Option<u64>,
    pub evaluated_budget_units: Option<u64>,
    pub cache_key: Option<String>,
    pub cache_hit: Option<bool>,
    pub provenance: Option<ProgressSloStressProvenance>,
}

/// Fail-closed verdict for one synthetic stress-budget profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressSloStressBudgetVerdict {
    pub schema: String,
    pub profile_id: String,
    pub scenario: ProgressSloStressScenario,
    pub passed: bool,
    pub reason_ids: Vec<String>,
    pub caveats: Vec<String>,
    pub report_status: ProgressSloStatus,
    pub report_reason_ids: Vec<String>,
    pub expected_report_status: ProgressSloStatus,
    pub cache_key: Option<String>,
    pub cache_hit: Option<bool>,
    pub provenance: Option<ProgressSloStressProvenance>,
    pub observed_source_records: Option<u64>,
    pub max_source_records: u64,
    pub observed_evaluation_budget_units: Option<u64>,
    pub max_evaluation_budget_units: u64,
}

struct SyntheticStressShape {
    source_record_count: u64,
    validation_slot_count: u64,
    expected_status: ProgressSloStatus,
    progress_metrics: ProgressSloMetrics,
}

struct ProgressSloClassification {
    status: ProgressSloStatus,
    reason_ids: BTreeSet<&'static str>,
    suppressed_claims: Vec<String>,
    missing_required_source_count: usize,
    malformed_source_count: u64,
    has_unsafe_redaction: bool,
}

/// Evaluate a normalized progress snapshot without touching live systems.
#[must_use]
pub fn evaluate_progress_slo(input: ProgressSloEvaluationInput) -> ProgressSloReport {
    let mut suppressed_claims = collect_suppressed_claims(&input.source_statuses);
    let redaction_summary = build_redaction_summary(&input.source_statuses, &suppressed_claims);
    let classification = classify_progress_slo(&input, &redaction_summary);

    suppressed_claims.extend(classification.suppressed_claims.iter().cloned());
    suppressed_claims.sort();
    suppressed_claims.dedup();

    let saturation_summary =
        build_saturation_summary(&input.progress_metrics, classification.status);
    let confidence = confidence_for(
        &input,
        classification.status,
        classification.missing_required_source_count,
        classification.malformed_source_count,
        classification.has_unsafe_redaction,
    );
    let next_actions = next_actions_for(classification.status, &saturation_summary);

    ProgressSloReport {
        schema: SWARM_PROGRESS_SLO_SCHEMA.to_string(),
        generated_at: input.generated_at,
        contract_version: SWARM_PROGRESS_SLO_CONTRACT_VERSION.to_string(),
        time_window: input.time_window,
        status: classification.status,
        confidence,
        reason_ids: classification
            .reason_ids
            .into_iter()
            .map(str::to_string)
            .collect(),
        source_statuses: input.source_statuses,
        progress_metrics: input.progress_metrics,
        saturation_summary,
        redaction_summary: ProgressSloRedactionSummary {
            suppressed_claims: suppressed_claims.clone(),
            ..redaction_summary
        },
        suppressed_claims,
        next_actions,
    }
}

/// Default synthetic stress budget for 64+ core / 256 GiB swarm hosts.
#[must_use]
pub fn default_large_host_progress_slo_stress_budget() -> ProgressSloStressBudget {
    ProgressSloStressBudget {
        min_host_cpu_cores: LARGE_HOST_STRESS_MIN_CPU_CORES,
        min_host_memory_gib: LARGE_HOST_STRESS_MIN_MEMORY_GIB,
        max_source_records: LARGE_HOST_STRESS_MAX_SOURCE_RECORDS,
        max_open_beads: LARGE_HOST_STRESS_MAX_OPEN_BEADS,
        max_in_progress_beads: LARGE_HOST_STRESS_MAX_IN_PROGRESS_BEADS,
        max_ready_beads: LARGE_HOST_STRESS_MAX_READY_BEADS,
        max_rch_queue_depth: LARGE_HOST_STRESS_MAX_RCH_QUEUE_DEPTH,
        max_validation_slots: LARGE_HOST_STRESS_MAX_VALIDATION_SLOTS,
        max_evaluation_budget_units: LARGE_HOST_STRESS_MAX_EVALUATION_BUDGET_UNITS,
        required_caveat: SWARM_PROGRESS_SLO_STRESS_BUDGET_CAVEAT.to_string(),
    }
}

/// Deterministic synthetic profiles used to keep the evaluator cheap on large hosts.
#[must_use]
pub fn large_host_progress_slo_stress_profiles() -> Vec<ProgressSloStressProfile> {
    [
        ProgressSloStressScenario::Nominal,
        ProgressSloStressScenario::Saturated,
        ProgressSloStressScenario::HugeHistory,
        ProgressSloStressScenario::MissingData,
    ]
    .into_iter()
    .map(synthetic_stress_profile)
    .collect()
}

/// Build a deterministic synthetic measurement for one stress profile.
#[must_use]
pub fn measure_progress_slo_stress_profile(
    profile: &ProgressSloStressProfile,
) -> ProgressSloStressMeasurement {
    let fingerprint = stress_profile_fingerprint(profile);
    ProgressSloStressMeasurement {
        evaluated_source_records: Some(profile.source_record_count),
        evaluated_budget_units: Some(stress_evaluation_budget_units(profile)),
        cache_key: Some(stress_cache_key(profile)),
        cache_hit: Some(false),
        provenance: Some(ProgressSloStressProvenance {
            profile_id: profile.profile_id.clone(),
            scenario: profile.scenario,
            generated_at: profile.progress_input.generated_at.clone(),
            generated_by: "pi.swarm.progress_slo.synthetic_stress_budget".to_string(),
            source_profile_fingerprint: fingerprint,
            synthetic: true,
            host_cpu_cores: profile.host_cpu_cores,
            host_memory_gib: profile.host_memory_gib,
            caveats: vec![SWARM_PROGRESS_SLO_STRESS_BUDGET_CAVEAT.to_string()],
        }),
    }
}

/// Evaluate one stress profile against a deterministic budget.
#[must_use]
pub fn evaluate_progress_slo_stress_budget(
    profile: &ProgressSloStressProfile,
    budget: &ProgressSloStressBudget,
    measurement: &ProgressSloStressMeasurement,
) -> ProgressSloStressBudgetVerdict {
    let report = evaluate_progress_slo(profile.progress_input.clone());
    let mut reason_ids = BTreeSet::new();
    let expected_cache_key = stress_cache_key(profile);

    if profile.host_cpu_cores < budget.min_host_cpu_cores
        || profile.host_memory_gib < budget.min_host_memory_gib
    {
        reason_ids.insert(REASON_STRESS_HOST_BELOW_FLOOR);
    }

    if matches!(
        report.status,
        ProgressSloStatus::MalformedSourceDegraded
            | ProgressSloStatus::InsufficientEvidenceDegraded
    ) {
        reason_ids.insert(REASON_STRESS_SOURCE_DEGRADED);
    }

    match (
        measurement.evaluated_source_records,
        measurement.evaluated_budget_units,
    ) {
        (Some(source_records), Some(budget_units)) => {
            if source_records > budget.max_source_records
                || budget_units > budget.max_evaluation_budget_units
                || profile.progress_input.progress_metrics.open_beads > budget.max_open_beads
                || profile.progress_input.progress_metrics.in_progress_beads
                    > budget.max_in_progress_beads
                || profile.progress_input.progress_metrics.ready_beads > budget.max_ready_beads
                || profile.progress_input.progress_metrics.rch_queue_depth
                    > budget.max_rch_queue_depth
                || profile.validation_slot_count > budget.max_validation_slots
            {
                reason_ids.insert(REASON_STRESS_BUDGET_EXCEEDED);
            }
        }
        _ => {
            reason_ids.insert(REASON_STRESS_MISSING_MEASUREMENT);
        }
    }

    if measurement.cache_key.as_deref() != Some(expected_cache_key.as_str())
        || measurement.cache_hit.is_none()
    {
        reason_ids.insert(REASON_STRESS_CACHE_MISSING_OR_MISMATCHED);
    }

    if !has_valid_stress_provenance(profile, measurement, &budget.required_caveat) {
        reason_ids.insert(REASON_STRESS_MISSING_PROVENANCE);
    }

    if !measurement.provenance.as_ref().is_some_and(|provenance| {
        provenance
            .caveats
            .iter()
            .any(|caveat| caveat == &budget.required_caveat)
    }) {
        reason_ids.insert(REASON_STRESS_MISSING_CAVEAT);
    }

    let reason_ids: Vec<String> = reason_ids.into_iter().map(str::to_string).collect();

    ProgressSloStressBudgetVerdict {
        schema: SWARM_PROGRESS_SLO_STRESS_BUDGET_SCHEMA.to_string(),
        profile_id: profile.profile_id.clone(),
        scenario: profile.scenario,
        passed: reason_ids.is_empty(),
        reason_ids,
        caveats: vec![SWARM_PROGRESS_SLO_STRESS_BUDGET_CAVEAT.to_string()],
        report_status: report.status,
        report_reason_ids: report.reason_ids,
        expected_report_status: profile.expected_status,
        cache_key: measurement.cache_key.clone(),
        cache_hit: measurement.cache_hit,
        provenance: measurement.provenance.clone(),
        observed_source_records: measurement.evaluated_source_records,
        max_source_records: budget.max_source_records,
        observed_evaluation_budget_units: measurement.evaluated_budget_units,
        max_evaluation_budget_units: budget.max_evaluation_budget_units,
    }
}

fn synthetic_stress_profile(scenario: ProgressSloStressScenario) -> ProgressSloStressProfile {
    let shape = synthetic_stress_shape(scenario);

    ProgressSloStressProfile {
        profile_id: scenario.profile_id().to_string(),
        scenario,
        host_cpu_cores: LARGE_HOST_STRESS_MIN_CPU_CORES,
        host_memory_gib: LARGE_HOST_STRESS_MIN_MEMORY_GIB,
        source_record_count: shape.source_record_count,
        validation_slot_count: shape.validation_slot_count,
        expected_status: shape.expected_status,
        progress_input: synthetic_progress_slo_input(scenario, shape.progress_metrics),
    }
}

const fn synthetic_stress_shape(scenario: ProgressSloStressScenario) -> SyntheticStressShape {
    match scenario {
        ProgressSloStressScenario::Nominal => SyntheticStressShape {
            source_record_count: 4_096,
            validation_slot_count: 64,
            expected_status: ProgressSloStatus::Progressing,
            progress_metrics: nominal_stress_metrics(),
        },
        ProgressSloStressScenario::Saturated => SyntheticStressShape {
            source_record_count: 12_000,
            validation_slot_count: 256,
            expected_status: ProgressSloStatus::BuildSaturated,
            progress_metrics: saturated_stress_metrics(),
        },
        ProgressSloStressScenario::HugeHistory => SyntheticStressShape {
            source_record_count: 80_000,
            validation_slot_count: 512,
            expected_status: ProgressSloStatus::Progressing,
            progress_metrics: huge_history_stress_metrics(),
        },
        ProgressSloStressScenario::MissingData => SyntheticStressShape {
            source_record_count: 0,
            validation_slot_count: 64,
            expected_status: ProgressSloStatus::InsufficientEvidenceDegraded,
            progress_metrics: missing_data_stress_metrics(),
        },
    }
}

const fn nominal_stress_metrics() -> ProgressSloMetrics {
    ProgressSloMetrics {
        closed_beads: 32,
        open_beads: 512,
        in_progress_beads: 64,
        ready_beads: 128,
        dependency_blocked_beads: 320,
        commits: 32,
        pushed_commits: 32,
        closed_with_commit_reference_count: 32,
        validation_passes: 96,
        validation_failures: 1,
        agent_mail_health: AgentMailHealth::Green,
        rch_posture: RchPosture::Green,
        rch_queue_depth: 8,
        rch_queue_saturation_threshold: LARGE_HOST_STRESS_MAX_RCH_QUEUE_DEPTH,
        validation_broker_posture: ValidationBrokerPosture::Green,
        stale_in_progress_candidates: 0,
        malformed_source_records: 0,
        contradictory_source_records: 0,
    }
}

const fn saturated_stress_metrics() -> ProgressSloMetrics {
    ProgressSloMetrics {
        closed_beads: 8,
        open_beads: 2_048,
        in_progress_beads: 384,
        ready_beads: 640,
        dependency_blocked_beads: 1_024,
        commits: 8,
        pushed_commits: 8,
        closed_with_commit_reference_count: 8,
        validation_passes: 128,
        validation_failures: 12,
        agent_mail_health: AgentMailHealth::Green,
        rch_posture: RchPosture::Queueing,
        rch_queue_depth: 512,
        rch_queue_saturation_threshold: 512,
        validation_broker_posture: ValidationBrokerPosture::Saturated,
        stale_in_progress_candidates: 0,
        malformed_source_records: 0,
        contradictory_source_records: 0,
    }
}

const fn huge_history_stress_metrics() -> ProgressSloMetrics {
    ProgressSloMetrics {
        closed_beads: 1_024,
        open_beads: 16_384,
        in_progress_beads: 512,
        ready_beads: 2_048,
        dependency_blocked_beads: 4_096,
        commits: 512,
        pushed_commits: 512,
        closed_with_commit_reference_count: 1_024,
        validation_passes: 2_048,
        validation_failures: 32,
        agent_mail_health: AgentMailHealth::Green,
        rch_posture: RchPosture::Queueing,
        rch_queue_depth: 384,
        rch_queue_saturation_threshold: LARGE_HOST_STRESS_MAX_RCH_QUEUE_DEPTH,
        validation_broker_posture: ValidationBrokerPosture::Queueing,
        stale_in_progress_candidates: 0,
        malformed_source_records: 0,
        contradictory_source_records: 0,
    }
}

const fn missing_data_stress_metrics() -> ProgressSloMetrics {
    ProgressSloMetrics {
        closed_beads: 0,
        open_beads: 512,
        in_progress_beads: 64,
        ready_beads: 128,
        dependency_blocked_beads: 320,
        commits: 0,
        pushed_commits: 0,
        closed_with_commit_reference_count: 0,
        validation_passes: 0,
        validation_failures: 0,
        agent_mail_health: AgentMailHealth::Green,
        rch_posture: RchPosture::Green,
        rch_queue_depth: 0,
        rch_queue_saturation_threshold: LARGE_HOST_STRESS_MAX_RCH_QUEUE_DEPTH,
        validation_broker_posture: ValidationBrokerPosture::Green,
        stale_in_progress_candidates: 0,
        malformed_source_records: 0,
        contradictory_source_records: 0,
    }
}

fn synthetic_progress_slo_input(
    scenario: ProgressSloStressScenario,
    progress_metrics: ProgressSloMetrics,
) -> ProgressSloEvaluationInput {
    let missing_source_id = match scenario {
        ProgressSloStressScenario::MissingData => Some("git_commit_delta"),
        ProgressSloStressScenario::Nominal
        | ProgressSloStressScenario::Saturated
        | ProgressSloStressScenario::HugeHistory => None,
    };

    ProgressSloEvaluationInput::new(
        "2026-05-15T03:00:00Z",
        ProgressSloTimeWindow::new(
            "2026-05-15T02:00:00Z",
            "2026-05-15T03:00:00Z",
            3600,
            "synthetic_large_host_progress_slo_stress_budget",
        ),
        synthetic_progress_sources(missing_source_id),
        progress_metrics,
    )
}

fn synthetic_progress_sources(missing_source_id: Option<&str>) -> Vec<ProgressSloSourceStatus> {
    REQUIRED_SOURCE_IDS
        .iter()
        .filter(|source_id| missing_source_id != Some(*source_id))
        .map(|source_id| {
            ProgressSloSourceStatus::new(
                *source_id,
                synthetic_source_class_for(source_id),
                synthetic_source_kind_for(source_id),
                SourceAvailability::Available,
                FreshnessState::Current,
                RedactionState::None,
                vec![format!("{source_id}_authority")],
            )
            .with_path(format!("synthetic/progress_slo/{source_id}.json"))
            .with_observed_at("2026-05-15T03:00:00Z")
            .with_source_hash(format!("synthetic-sha256-{source_id}"))
        })
        .collect()
}

fn synthetic_source_class_for(source_id: &str) -> &'static str {
    match source_id {
        "beads_active_delta" | "beads_closed_delta" => "beads_active_closed_delta",
        "git_commit_delta" => "git_commit_delta",
        "rch_posture" | "validation_broker_posture" => "rch_and_validation_broker_posture",
        "agent_mail_health" => "agent_mail_health",
        "operator_runpack_summary" | "swarm_autopilot_summary" | "context_intelligence_summary" => {
            "runpack_autopilot_context_summaries"
        }
        "operator_time_window" => "operator_provided_time_window",
        _ => "unknown",
    }
}

fn synthetic_source_kind_for(source_id: &str) -> &'static str {
    match source_id {
        "beads_active_delta" | "beads_closed_delta" => "beads",
        "git_commit_delta" => "git",
        "rch_posture" => "rch",
        "validation_broker_posture" => "validation_broker",
        "agent_mail_health" => "agent_mail",
        "operator_runpack_summary" => "runpack",
        "swarm_autopilot_summary" => "autopilot",
        "context_intelligence_summary" => "context_intelligence",
        "operator_time_window" => "operator",
        _ => "unknown",
    }
}

fn stress_evaluation_budget_units(profile: &ProgressSloStressProfile) -> u64 {
    let metrics = &profile.progress_input.progress_metrics;
    128_u64
        .saturating_add(profile.source_record_count.saturating_mul(8))
        .saturating_add(metrics.open_beads.saturating_mul(2))
        .saturating_add(metrics.in_progress_beads.saturating_mul(4))
        .saturating_add(metrics.ready_beads.saturating_mul(3))
        .saturating_add(metrics.rch_queue_depth.saturating_mul(4))
        .saturating_add(profile.validation_slot_count.saturating_mul(4))
        .saturating_add(u64_from_usize_saturating(
            profile.progress_input.source_statuses.len(),
        ))
}

fn stress_profile_fingerprint(profile: &ProgressSloStressProfile) -> String {
    let metrics = &profile.progress_input.progress_metrics;
    format!(
        "{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
        SWARM_PROGRESS_SLO_STRESS_BUDGET_SCHEMA,
        profile.profile_id,
        profile.scenario.as_str(),
        profile.host_cpu_cores,
        profile.host_memory_gib,
        profile.source_record_count,
        profile.validation_slot_count,
        metrics.open_beads,
        metrics.in_progress_beads,
        metrics.ready_beads
    )
}

fn stress_cache_key(profile: &ProgressSloStressProfile) -> String {
    format!("cache:{}", stress_profile_fingerprint(profile))
}

fn has_valid_stress_provenance(
    profile: &ProgressSloStressProfile,
    measurement: &ProgressSloStressMeasurement,
    required_caveat: &str,
) -> bool {
    measurement.provenance.as_ref().is_some_and(|provenance| {
        provenance.profile_id == profile.profile_id
            && provenance.scenario == profile.scenario
            && !provenance.generated_at.trim().is_empty()
            && !provenance.generated_by.trim().is_empty()
            && provenance.source_profile_fingerprint == stress_profile_fingerprint(profile)
            && provenance.synthetic
            && provenance.host_cpu_cores == profile.host_cpu_cores
            && provenance.host_memory_gib == profile.host_memory_gib
            && provenance
                .caveats
                .iter()
                .any(|caveat| caveat == required_caveat)
    })
}

fn classify_progress_slo(
    input: &ProgressSloEvaluationInput,
    redaction_summary: &ProgressSloRedactionSummary,
) -> ProgressSloClassification {
    let mut reason_ids = BTreeSet::new();
    let mut suppressed_claims = Vec::new();
    let source_ids = source_id_set(&input.source_statuses);
    let missing_required_sources = missing_sources(REQUIRED_SOURCE_IDS, &source_ids);
    let missing_progress_authority = missing_sources(PROGRESSING_AUTHORITY_SOURCE_IDS, &source_ids);
    let malformed_source_count = input.progress_metrics.malformed_source_records
        + u64_from_usize_saturating(
            input
                .source_statuses
                .iter()
                .filter(|source| source.is_malformed())
                .count(),
        );
    let has_unsafe_redaction = redaction_summary.unsafe_to_emit_count > 0;
    let has_malformed_required_source = input
        .source_statuses
        .iter()
        .any(|source| is_required_source(&source.source_id) && source.is_malformed());
    let has_degraded_progress_authority = input.source_statuses.iter().any(|source| {
        is_progress_authority_source(&source.source_id) && !source.is_currently_available()
    });
    let rch_saturated = input.progress_metrics.rch_posture.is_saturated()
        || input.progress_metrics.rch_queue_depth
            >= input.progress_metrics.rch_queue_saturation_threshold.max(1);
    let validation_saturated = input
        .progress_metrics
        .validation_broker_posture
        .is_saturated();
    let agent_mail_degraded = input.progress_metrics.agent_mail_health.is_degraded()
        || source_is_degraded(&input.source_statuses, "agent_mail_health");

    let status = if !input.time_window.is_valid()
        || input.source_statuses.is_empty()
        || !missing_required_sources.is_empty()
        || !missing_progress_authority.is_empty()
    {
        reason_ids.insert(REASON_MISSING_AUTHORITY);
        suppressed_claims.push("progressing".to_string());
        ProgressSloStatus::InsufficientEvidenceDegraded
    } else if has_malformed_required_source
        || malformed_source_count > 0
        || input.progress_metrics.contradictory_source_records > 0
    {
        reason_ids.insert(REASON_MALFORMED_SOURCE);
        suppressed_claims.push("progressing".to_string());
        ProgressSloStatus::MalformedSourceDegraded
    } else if has_degraded_progress_authority || has_unsafe_redaction {
        reason_ids.insert(REASON_MISSING_AUTHORITY);
        suppressed_claims.push("progressing".to_string());
        ProgressSloStatus::InsufficientEvidenceDegraded
    } else if agent_mail_degraded {
        reason_ids.insert(REASON_AGENT_MAIL_DEGRADED);
        suppressed_claims.push("coordination_green".to_string());
        ProgressSloStatus::CoordinationDegraded
    } else if rch_saturated || validation_saturated {
        if rch_saturated {
            reason_ids.insert(REASON_RCH_SATURATED);
        }
        if validation_saturated {
            reason_ids.insert(REASON_VALIDATION_BROKER_SATURATED);
        }
        suppressed_claims.push("build_capacity_green".to_string());
        ProgressSloStatus::BuildSaturated
    } else if input.progress_metrics.open_beads == 0
        && input.progress_metrics.in_progress_beads == 0
        && input.progress_metrics.ready_beads == 0
    {
        reason_ids.insert(REASON_CONVERGED_NO_OPEN_WORK);
        ProgressSloStatus::ConvergedNoOpenWork
    } else if input.progress_metrics.stale_in_progress_candidates > 0 {
        reason_ids.insert(REASON_STALE_IN_PROGRESS);
        suppressed_claims.push("progressing".to_string());
        ProgressSloStatus::Stalled
    } else if has_useful_progress(&input.progress_metrics) {
        if input.progress_metrics.closed_beads > 0 {
            reason_ids.insert(REASON_BEAD_CLOSEOUT);
        }
        if input.progress_metrics.commits > 0 || input.progress_metrics.pushed_commits > 0 {
            reason_ids.insert(REASON_GIT_COMMIT_DELTA);
        }
        ProgressSloStatus::Progressing
    } else if input.progress_metrics.ready_beads == 0 {
        reason_ids.insert(REASON_NO_READY_WORK);
        ProgressSloStatus::QuietBlocked
    } else {
        reason_ids.insert(REASON_STALE_IN_PROGRESS);
        suppressed_claims.push("progressing".to_string());
        ProgressSloStatus::Stalled
    };

    suppressed_claims.sort();
    suppressed_claims.dedup();

    ProgressSloClassification {
        status,
        reason_ids,
        suppressed_claims,
        missing_required_source_count: missing_required_sources.len(),
        malformed_source_count,
        has_unsafe_redaction,
    }
}

fn source_id_set(source_statuses: &[ProgressSloSourceStatus]) -> BTreeSet<&str> {
    source_statuses
        .iter()
        .map(|source| source.source_id.as_str())
        .collect()
}

fn missing_sources(required: &[&str], available: &BTreeSet<&str>) -> Vec<String> {
    required
        .iter()
        .filter(|source_id| !available.contains(**source_id))
        .map(|source_id| (*source_id).to_string())
        .collect()
}

fn is_required_source(source_id: &str) -> bool {
    REQUIRED_SOURCE_IDS.contains(&source_id)
}

fn is_progress_authority_source(source_id: &str) -> bool {
    PROGRESSING_AUTHORITY_SOURCE_IDS.contains(&source_id)
}

fn source_is_degraded(source_statuses: &[ProgressSloSourceStatus], source_id: &str) -> bool {
    source_statuses
        .iter()
        .find(|source| source.source_id == source_id)
        .is_some_and(ProgressSloSourceStatus::is_degraded)
}

const fn has_useful_progress(metrics: &ProgressSloMetrics) -> bool {
    metrics.closed_with_commit_reference_count > 0
        || (metrics.closed_beads > 0 && (metrics.commits > 0 || metrics.validation_passes > 0))
        || (metrics.commits > 0 && metrics.pushed_commits > 0)
}

fn collect_suppressed_claims(source_statuses: &[ProgressSloSourceStatus]) -> Vec<String> {
    let mut claims: Vec<String> = source_statuses
        .iter()
        .flat_map(|source| source.suppressed_claims.iter().cloned())
        .collect();
    claims.sort();
    claims.dedup();
    claims
}

fn build_redaction_summary(
    source_statuses: &[ProgressSloSourceStatus],
    suppressed_claims: &[String],
) -> ProgressSloRedactionSummary {
    let mut summary = ProgressSloRedactionSummary {
        redacted_count: 0,
        omitted_count: 0,
        unsafe_to_emit_count: 0,
        suppressed_claims: suppressed_claims.to_vec(),
    };

    for source in source_statuses {
        match source.redaction_state {
            RedactionState::None => {}
            RedactionState::Redacted => summary.redacted_count += 1,
            RedactionState::SensitiveOmitted => summary.omitted_count += 1,
            RedactionState::UnsafeToEmit => summary.unsafe_to_emit_count += 1,
        }
    }

    summary.suppressed_claims.sort();
    summary.suppressed_claims.dedup();
    summary
}

fn build_saturation_summary(
    metrics: &ProgressSloMetrics,
    status: ProgressSloStatus,
) -> ProgressSloSaturationSummary {
    let coordination_saturation = match metrics.agent_mail_health {
        AgentMailHealth::Green => DimensionStatus::Green,
        AgentMailHealth::Yellow => DimensionStatus::Yellow,
        AgentMailHealth::Unknown => DimensionStatus::Unknown,
        AgentMailHealth::Red | AgentMailHealth::Corrupt | AgentMailHealth::Unavailable => {
            DimensionStatus::Red
        }
    };

    let build_saturation = match metrics.rch_posture {
        RchPosture::Green => DimensionStatus::Green,
        RchPosture::Queueing => DimensionStatus::Yellow,
        RchPosture::Unknown => DimensionStatus::Unknown,
        RchPosture::Saturated | RchPosture::FailOpenLocalRisk | RchPosture::Unavailable => {
            DimensionStatus::Red
        }
    };

    let validation_saturation = match metrics.validation_broker_posture {
        ValidationBrokerPosture::Green => DimensionStatus::Green,
        ValidationBrokerPosture::Queueing => DimensionStatus::Yellow,
        ValidationBrokerPosture::Unknown => DimensionStatus::Unknown,
        ValidationBrokerPosture::Saturated
        | ValidationBrokerPosture::StaleSlots
        | ValidationBrokerPosture::Unavailable => DimensionStatus::Red,
    };

    let queue_convergence =
        if metrics.open_beads == 0 && metrics.in_progress_beads == 0 && metrics.ready_beads == 0 {
            DimensionStatus::Green
        } else if metrics.ready_beads == 0 || metrics.stale_in_progress_candidates > 0 {
            DimensionStatus::Yellow
        } else {
            DimensionStatus::Green
        };

    let recommended_operator_posture = match status {
        ProgressSloStatus::Progressing => RecommendedOperatorPosture::ContinueCurrentSwarm,
        ProgressSloStatus::QuietBlocked | ProgressSloStatus::ConvergedNoOpenWork => {
            RecommendedOperatorPosture::GenerateNewBeads
        }
        ProgressSloStatus::CoordinationDegraded => {
            RecommendedOperatorPosture::RepairCoordinationTooling
        }
        ProgressSloStatus::BuildSaturated => {
            if validation_saturation == DimensionStatus::Red {
                RecommendedOperatorPosture::NarrowValidationScope
            } else {
                RecommendedOperatorPosture::BackoffHeavyCargo
            }
        }
        ProgressSloStatus::Stalled
        | ProgressSloStatus::MalformedSourceDegraded
        | ProgressSloStatus::InsufficientEvidenceDegraded => {
            RecommendedOperatorPosture::HandoffForHumanTriage
        }
    };

    ProgressSloSaturationSummary {
        coordination_saturation,
        build_saturation,
        validation_saturation,
        queue_convergence,
        recommended_operator_posture,
    }
}

fn confidence_for(
    input: &ProgressSloEvaluationInput,
    status: ProgressSloStatus,
    missing_required_sources: usize,
    malformed_source_count: u64,
    has_unsafe_redaction: bool,
) -> f64 {
    if input.source_statuses.is_empty() {
        return 0.0;
    }

    let available_current = input
        .source_statuses
        .iter()
        .filter(|source| source.is_currently_available())
        .count();
    let coverage = f64_from_usize_saturating(available_current)
        / f64_from_usize_saturating(input.source_statuses.len());
    let base = match status {
        ProgressSloStatus::Progressing | ProgressSloStatus::ConvergedNoOpenWork => 0.92,
        ProgressSloStatus::QuietBlocked | ProgressSloStatus::BuildSaturated => 0.82,
        ProgressSloStatus::CoordinationDegraded | ProgressSloStatus::Stalled => 0.76,
        ProgressSloStatus::MalformedSourceDegraded
        | ProgressSloStatus::InsufficientEvidenceDegraded => 0.54,
    };
    let mut confidence = base * coverage;
    confidence = f64_from_usize_saturating(missing_required_sources).mul_add(-0.08, confidence);
    confidence = f64_from_u64_saturating(malformed_source_count).mul_add(-0.04, confidence);
    if has_unsafe_redaction {
        confidence -= 0.2;
    }
    confidence.clamp(0.0, 0.99)
}

fn u64_from_usize_saturating(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn f64_from_usize_saturating(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

fn f64_from_u64_saturating(value: u64) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

fn next_actions_for(
    status: ProgressSloStatus,
    saturation_summary: &ProgressSloSaturationSummary,
) -> Vec<String> {
    let mut actions = Vec::new();
    match status {
        ProgressSloStatus::Progressing => {
            actions.push("continue_current_swarm".to_string());
        }
        ProgressSloStatus::QuietBlocked => {
            actions.push("inspect_dependency_blockers".to_string());
            actions.push("generate_new_beads_if_backlog_is_empty".to_string());
        }
        ProgressSloStatus::CoordinationDegraded => {
            actions.push("repair_coordination_tooling".to_string());
            actions.push("use_beads_soft_locks_until_agent_mail_recovers".to_string());
        }
        ProgressSloStatus::BuildSaturated => {
            if saturation_summary.validation_saturation == DimensionStatus::Red {
                actions.push("narrow_validation_scope".to_string());
            }
            actions.push("backoff_heavy_cargo".to_string());
        }
        ProgressSloStatus::Stalled => {
            actions.push("reclaim_or_reopen_stale_in_progress_beads".to_string());
            actions.push("reduce_scope_to_finishable_slice".to_string());
        }
        ProgressSloStatus::ConvergedNoOpenWork => {
            actions.push("generate_new_beads".to_string());
            actions.push("run_closeout_gate".to_string());
        }
        ProgressSloStatus::MalformedSourceDegraded => {
            actions.push("repair_progress_slo_source_artifact".to_string());
        }
        ProgressSloStatus::InsufficientEvidenceDegraded => {
            actions.push("provide_required_progress_sources".to_string());
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::{
        AgentMailHealth, DimensionStatus, FreshnessState, ProgressSloEvaluationInput,
        ProgressSloMetrics, ProgressSloSourceStatus, ProgressSloStatus, ProgressSloStressProfile,
        ProgressSloStressScenario, ProgressSloTimeWindow, REASON_AGENT_MAIL_DEGRADED,
        REASON_BEAD_CLOSEOUT, REASON_CONVERGED_NO_OPEN_WORK, REASON_MALFORMED_SOURCE,
        REASON_MISSING_AUTHORITY, REASON_NO_READY_WORK, REASON_RCH_SATURATED,
        REASON_STALE_IN_PROGRESS, REASON_STRESS_BUDGET_EXCEEDED,
        REASON_STRESS_CACHE_MISSING_OR_MISMATCHED, REASON_STRESS_MISSING_CAVEAT,
        REASON_STRESS_MISSING_MEASUREMENT, REASON_STRESS_MISSING_PROVENANCE,
        REASON_STRESS_SOURCE_DEGRADED, REASON_VALIDATION_BROKER_SATURATED, RchPosture,
        RecommendedOperatorPosture, RedactionState, SWARM_PROGRESS_SLO_SCHEMA,
        SWARM_PROGRESS_SLO_STRESS_BUDGET_CAVEAT, SWARM_PROGRESS_SLO_STRESS_BUDGET_SCHEMA,
        SourceAvailability, ValidationBrokerPosture, default_large_host_progress_slo_stress_budget,
        evaluate_progress_slo, evaluate_progress_slo_stress_budget,
        large_host_progress_slo_stress_profiles, measure_progress_slo_stress_profile,
        synthetic_stress_profile,
    };

    fn window() -> ProgressSloTimeWindow {
        ProgressSloTimeWindow::new(
            "2026-05-15T02:00:00Z",
            "2026-05-15T03:00:00Z",
            3600,
            "operator_requested_window",
        )
    }

    fn source(id: &str) -> ProgressSloSourceStatus {
        ProgressSloSourceStatus::new(
            id,
            source_class_for(id),
            source_kind_for(id),
            SourceAvailability::Available,
            FreshnessState::Current,
            RedactionState::None,
            vec![format!("{id}_authority")],
        )
        .with_path(format!("evidence/{id}.json"))
        .with_observed_at("2026-05-15T03:00:00Z")
        .with_source_hash(format!("sha256-{id}"))
    }

    fn source_class_for(id: &str) -> &'static str {
        match id {
            "beads_active_delta" | "beads_closed_delta" => "beads_active_closed_delta",
            "git_commit_delta" => "git_commit_delta",
            "rch_posture" | "validation_broker_posture" => "rch_and_validation_broker_posture",
            "agent_mail_health" => "agent_mail_health",
            "operator_runpack_summary"
            | "swarm_autopilot_summary"
            | "context_intelligence_summary" => "runpack_autopilot_context_summaries",
            "operator_time_window" => "operator_provided_time_window",
            _ => "unknown",
        }
    }

    fn source_kind_for(id: &str) -> &'static str {
        match id {
            "beads_active_delta" | "beads_closed_delta" => "beads",
            "git_commit_delta" => "git",
            "rch_posture" => "rch",
            "validation_broker_posture" => "validation_broker",
            "agent_mail_health" => "agent_mail",
            "operator_runpack_summary" => "runpack",
            "swarm_autopilot_summary" => "autopilot",
            "context_intelligence_summary" => "context_intelligence",
            "operator_time_window" => "operator",
            _ => "unknown",
        }
    }

    fn all_sources() -> Vec<ProgressSloSourceStatus> {
        [
            "beads_active_delta",
            "beads_closed_delta",
            "git_commit_delta",
            "rch_posture",
            "validation_broker_posture",
            "agent_mail_health",
            "operator_runpack_summary",
            "swarm_autopilot_summary",
            "context_intelligence_summary",
            "operator_time_window",
        ]
        .into_iter()
        .map(source)
        .collect()
    }

    fn healthy_metrics() -> ProgressSloMetrics {
        ProgressSloMetrics {
            closed_beads: 2,
            open_beads: 8,
            in_progress_beads: 1,
            ready_beads: 3,
            dependency_blocked_beads: 4,
            commits: 2,
            pushed_commits: 2,
            closed_with_commit_reference_count: 2,
            validation_passes: 3,
            validation_failures: 0,
            agent_mail_health: AgentMailHealth::Green,
            rch_posture: RchPosture::Green,
            rch_queue_depth: 0,
            rch_queue_saturation_threshold: 10,
            validation_broker_posture: ValidationBrokerPosture::Green,
            stale_in_progress_candidates: 0,
            malformed_source_records: 0,
            contradictory_source_records: 0,
        }
    }

    fn evaluate(metrics: ProgressSloMetrics) -> super::ProgressSloReport {
        evaluate_progress_slo(ProgressSloEvaluationInput::new(
            "2026-05-15T03:00:00Z",
            window(),
            all_sources(),
            metrics,
        ))
    }

    fn stress_profile(scenario: ProgressSloStressScenario) -> ProgressSloStressProfile {
        large_host_progress_slo_stress_profiles()
            .into_iter()
            .find(|profile| profile.scenario == scenario)
            .unwrap_or_else(|| synthetic_stress_profile(scenario))
    }

    #[test]
    fn healthy_closeout_and_commit_delta_reports_progressing() {
        let report = evaluate(healthy_metrics());

        assert_eq!(report.schema, SWARM_PROGRESS_SLO_SCHEMA);
        assert_eq!(report.status, ProgressSloStatus::Progressing);
        assert!(report.confidence > 0.9);
        assert!(
            report
                .reason_ids
                .iter()
                .any(|reason| reason == REASON_BEAD_CLOSEOUT)
        );
        assert!(report.progress_metrics.closed_with_commit_reference_count > 0);
        assert_eq!(
            report.saturation_summary.recommended_operator_posture,
            RecommendedOperatorPosture::ContinueCurrentSwarm
        );
        assert!(report.suppressed_claims.is_empty());
    }

    #[test]
    fn no_open_or_in_progress_work_reports_converged() {
        let report = evaluate(ProgressSloMetrics {
            closed_beads: 0,
            open_beads: 0,
            in_progress_beads: 0,
            ready_beads: 0,
            commits: 0,
            pushed_commits: 0,
            closed_with_commit_reference_count: 0,
            ..healthy_metrics()
        });

        assert_eq!(report.status, ProgressSloStatus::ConvergedNoOpenWork);
        assert!(
            report
                .reason_ids
                .iter()
                .any(|reason| { reason == REASON_CONVERGED_NO_OPEN_WORK })
        );
        assert_eq!(
            report.saturation_summary.queue_convergence,
            DimensionStatus::Green
        );
        assert!(
            report
                .next_actions
                .iter()
                .any(|action| action == "generate_new_beads")
        );
    }

    #[test]
    fn no_ready_work_with_open_backlog_reports_quiet_blocked() {
        let report = evaluate(ProgressSloMetrics {
            closed_beads: 0,
            open_beads: 5,
            in_progress_beads: 0,
            ready_beads: 0,
            commits: 0,
            pushed_commits: 0,
            closed_with_commit_reference_count: 0,
            ..healthy_metrics()
        });

        assert_eq!(report.status, ProgressSloStatus::QuietBlocked);
        assert!(
            report
                .reason_ids
                .iter()
                .any(|reason| reason == REASON_NO_READY_WORK)
        );
        assert!(
            report
                .next_actions
                .iter()
                .any(|action| { action == "inspect_dependency_blockers" })
        );
    }

    #[test]
    fn stale_in_progress_without_progress_reports_stalled() {
        let report = evaluate(ProgressSloMetrics {
            closed_beads: 0,
            open_beads: 6,
            in_progress_beads: 3,
            ready_beads: 2,
            commits: 0,
            pushed_commits: 0,
            closed_with_commit_reference_count: 0,
            stale_in_progress_candidates: 2,
            ..healthy_metrics()
        });

        assert_eq!(report.status, ProgressSloStatus::Stalled);
        assert!(
            report
                .reason_ids
                .iter()
                .any(|reason| { reason == REASON_STALE_IN_PROGRESS })
        );
        assert!(
            report
                .suppressed_claims
                .iter()
                .any(|claim| claim == "progressing")
        );
    }

    #[test]
    fn corrupt_agent_mail_reports_coordination_degraded() {
        let report = evaluate(ProgressSloMetrics {
            agent_mail_health: AgentMailHealth::Corrupt,
            ..healthy_metrics()
        });

        assert_eq!(report.status, ProgressSloStatus::CoordinationDegraded);
        assert!(
            report
                .reason_ids
                .iter()
                .any(|reason| { reason == REASON_AGENT_MAIL_DEGRADED })
        );
        assert_eq!(
            report.saturation_summary.coordination_saturation,
            DimensionStatus::Red
        );
        assert_eq!(
            report.saturation_summary.recommended_operator_posture,
            RecommendedOperatorPosture::RepairCoordinationTooling
        );
    }

    #[test]
    fn rch_queue_or_validation_saturation_reports_build_saturated() {
        let rch_report = evaluate(ProgressSloMetrics {
            rch_posture: RchPosture::Queueing,
            rch_queue_depth: 12,
            rch_queue_saturation_threshold: 10,
            ..healthy_metrics()
        });

        assert_eq!(rch_report.status, ProgressSloStatus::BuildSaturated);
        assert!(
            rch_report
                .reason_ids
                .iter()
                .any(|reason| { reason == REASON_RCH_SATURATED })
        );
        assert_eq!(
            rch_report.saturation_summary.recommended_operator_posture,
            RecommendedOperatorPosture::BackoffHeavyCargo
        );

        let validation_report = evaluate(ProgressSloMetrics {
            validation_broker_posture: ValidationBrokerPosture::Saturated,
            ..healthy_metrics()
        });

        assert_eq!(validation_report.status, ProgressSloStatus::BuildSaturated);
        assert!(
            validation_report
                .reason_ids
                .iter()
                .any(|reason| { reason == REASON_VALIDATION_BROKER_SATURATED })
        );
        assert_eq!(
            validation_report
                .saturation_summary
                .recommended_operator_posture,
            RecommendedOperatorPosture::NarrowValidationScope
        );
    }

    #[test]
    fn malformed_or_contradictory_sources_fail_closed() {
        let mut sources = all_sources();
        if let Some(source) = sources
            .iter_mut()
            .find(|source| source.source_id == "beads_active_delta")
        {
            source.availability = SourceAvailability::Malformed;
            source
                .degraded_reasons
                .push("invalid_beads_jsonl".to_string());
        }

        let malformed_report = evaluate_progress_slo(ProgressSloEvaluationInput::new(
            "2026-05-15T03:00:00Z",
            window(),
            sources,
            healthy_metrics(),
        ));

        assert_eq!(
            malformed_report.status,
            ProgressSloStatus::MalformedSourceDegraded
        );
        assert!(
            malformed_report
                .reason_ids
                .iter()
                .any(|reason| { reason == REASON_MALFORMED_SOURCE })
        );
        assert!(
            malformed_report
                .suppressed_claims
                .iter()
                .any(|claim| { claim == "progressing" })
        );

        let contradictory_report = evaluate(ProgressSloMetrics {
            contradictory_source_records: 1,
            ..healthy_metrics()
        });

        assert_eq!(
            contradictory_report.status,
            ProgressSloStatus::MalformedSourceDegraded
        );
    }

    #[test]
    fn missing_stale_or_unsafe_authority_degrades_without_progressing() {
        let mut missing_sources = all_sources();
        missing_sources.retain(|source| source.source_id != "git_commit_delta");
        let missing_report = evaluate_progress_slo(ProgressSloEvaluationInput::new(
            "2026-05-15T03:00:00Z",
            window(),
            missing_sources,
            healthy_metrics(),
        ));

        assert_eq!(
            missing_report.status,
            ProgressSloStatus::InsufficientEvidenceDegraded
        );
        assert!(
            missing_report
                .reason_ids
                .iter()
                .any(|reason| { reason == REASON_MISSING_AUTHORITY })
        );
        assert!(
            missing_report
                .suppressed_claims
                .iter()
                .any(|claim| { claim == "progressing" })
        );

        let mut stale_sources = all_sources();
        if let Some(source) = stale_sources
            .iter_mut()
            .find(|source| source.source_id == "git_commit_delta")
        {
            source.freshness_state = FreshnessState::Stale;
            source
                .degraded_reasons
                .push("git_delta_outside_window".to_string());
        }
        let stale_report = evaluate_progress_slo(ProgressSloEvaluationInput::new(
            "2026-05-15T03:00:00Z",
            window(),
            stale_sources,
            healthy_metrics(),
        ));

        assert_eq!(
            stale_report.status,
            ProgressSloStatus::InsufficientEvidenceDegraded
        );
        assert!(
            stale_report
                .reason_ids
                .iter()
                .any(|reason| { reason == REASON_MISSING_AUTHORITY })
        );
        assert!(
            stale_report
                .suppressed_claims
                .iter()
                .any(|claim| { claim == "progressing" })
        );

        let mut unsafe_sources = all_sources();
        if let Some(source) = unsafe_sources
            .iter_mut()
            .find(|source| source.source_id == "git_commit_delta")
        {
            source.redaction_state = RedactionState::UnsafeToEmit;
        }
        let unsafe_report = evaluate_progress_slo(ProgressSloEvaluationInput::new(
            "2026-05-15T03:00:00Z",
            window(),
            unsafe_sources,
            healthy_metrics(),
        ));

        assert_eq!(
            unsafe_report.status,
            ProgressSloStatus::InsufficientEvidenceDegraded
        );
        assert_eq!(unsafe_report.redaction_summary.unsafe_to_emit_count, 1);
        assert!(unsafe_report.confidence < missing_report.confidence);
    }

    #[test]
    fn large_host_nominal_and_huge_history_profiles_pass_stress_budget() {
        let budget = default_large_host_progress_slo_stress_budget();

        for scenario in [
            ProgressSloStressScenario::Nominal,
            ProgressSloStressScenario::HugeHistory,
        ] {
            let profile = stress_profile(scenario);
            let measurement = measure_progress_slo_stress_profile(&profile);
            let verdict = evaluate_progress_slo_stress_budget(&profile, &budget, &measurement);

            assert_eq!(verdict.schema, SWARM_PROGRESS_SLO_STRESS_BUDGET_SCHEMA);
            assert!(verdict.passed, "{:?}", verdict.reason_ids);
            assert_eq!(verdict.report_status, ProgressSloStatus::Progressing);
            assert_eq!(verdict.report_status, verdict.expected_report_status);
            assert!(profile.host_cpu_cores >= 64);
            assert!(profile.host_memory_gib >= 256);
            assert!(
                verdict
                    .caveats
                    .iter()
                    .any(|caveat| caveat == SWARM_PROGRESS_SLO_STRESS_BUDGET_CAVEAT)
            );
            assert!(
                verdict
                    .provenance
                    .as_ref()
                    .is_some_and(|provenance| provenance.synthetic)
            );
        }
    }

    #[test]
    fn saturated_profile_can_pass_cost_budget_while_reporting_build_saturation() {
        let budget = default_large_host_progress_slo_stress_budget();
        let profile = stress_profile(ProgressSloStressScenario::Saturated);
        let measurement = measure_progress_slo_stress_profile(&profile);
        let verdict = evaluate_progress_slo_stress_budget(&profile, &budget, &measurement);

        assert!(verdict.passed, "{:?}", verdict.reason_ids);
        assert_eq!(verdict.report_status, ProgressSloStatus::BuildSaturated);
        assert_eq!(verdict.report_status, verdict.expected_report_status);
        assert!(
            verdict
                .report_reason_ids
                .iter()
                .any(|reason| reason == REASON_RCH_SATURATED)
        );
        assert!(
            verdict
                .observed_source_records
                .is_some_and(|records| records > 0)
        );
    }

    #[test]
    fn missing_data_profile_fails_stress_budget_closed() {
        let budget = default_large_host_progress_slo_stress_budget();
        let profile = stress_profile(ProgressSloStressScenario::MissingData);
        let measurement = measure_progress_slo_stress_profile(&profile);
        let verdict = evaluate_progress_slo_stress_budget(&profile, &budget, &measurement);

        assert!(!verdict.passed);
        assert_eq!(
            verdict.report_status,
            ProgressSloStatus::InsufficientEvidenceDegraded
        );
        assert!(
            verdict
                .reason_ids
                .iter()
                .any(|reason| reason == REASON_STRESS_SOURCE_DEGRADED)
        );
        assert!(
            verdict
                .caveats
                .iter()
                .any(|caveat| caveat == SWARM_PROGRESS_SLO_STRESS_BUDGET_CAVEAT)
        );
    }

    #[test]
    fn missing_measurement_cache_and_provenance_fail_closed() {
        let budget = default_large_host_progress_slo_stress_budget();
        let profile = stress_profile(ProgressSloStressScenario::Nominal);
        let mut measurement = measure_progress_slo_stress_profile(&profile);
        measurement.evaluated_budget_units = None;
        measurement.cache_key = None;
        measurement.cache_hit = None;
        measurement.provenance = None;

        let verdict = evaluate_progress_slo_stress_budget(&profile, &budget, &measurement);

        assert!(!verdict.passed);
        assert!(
            verdict
                .reason_ids
                .iter()
                .any(|reason| reason == REASON_STRESS_MISSING_MEASUREMENT)
        );
        assert!(
            verdict
                .reason_ids
                .iter()
                .any(|reason| reason == REASON_STRESS_CACHE_MISSING_OR_MISMATCHED)
        );
        assert!(
            verdict
                .reason_ids
                .iter()
                .any(|reason| reason == REASON_STRESS_MISSING_PROVENANCE)
        );
        assert!(
            verdict
                .reason_ids
                .iter()
                .any(|reason| reason == REASON_STRESS_MISSING_CAVEAT)
        );
    }

    #[test]
    fn exceeded_stress_budget_fails_closed() {
        let mut budget = default_large_host_progress_slo_stress_budget();
        budget.max_source_records = 1;
        budget.max_evaluation_budget_units = 1;
        let profile = stress_profile(ProgressSloStressScenario::HugeHistory);
        let measurement = measure_progress_slo_stress_profile(&profile);
        let verdict = evaluate_progress_slo_stress_budget(&profile, &budget, &measurement);

        assert!(!verdict.passed);
        assert!(
            verdict
                .reason_ids
                .iter()
                .any(|reason| reason == REASON_STRESS_BUDGET_EXCEEDED)
        );
        assert!(verdict.observed_source_records > Some(verdict.max_source_records));
        assert!(
            verdict.observed_evaluation_budget_units > Some(verdict.max_evaluation_budget_units)
        );
    }
}
