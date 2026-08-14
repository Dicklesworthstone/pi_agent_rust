//! Read-only ingestor for offline swarm replay traces.
//!
//! The ingestor consumes already-captured repository artifacts and normalizes
//! them into `pi.swarm.replay_trace.v1`. It never claims beads, sends mail,
//! reserves files, starts builds, or performs network I/O.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result};

/// Schema emitted by normalized replay traces.
pub const SWARM_REPLAY_TRACE_SCHEMA: &str = "pi.swarm.replay_trace.v1";

/// Contract version implemented by this ingestor.
pub const SWARM_REPLAY_TRACE_CONTRACT_VERSION: &str = "1.0.0";

const SENSITIVE_REDACTION: &str = "[REDACTED]";
const SENSITIVE_KEY_FRAGMENTS: &[&str] = &[
    "authorization",
    "body",
    "cookie",
    "key",
    "password",
    "prompt",
    "registration_token",
    "secret",
    "token",
    "transcript",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceInputFormat {
    Json,
    JsonLines,
    Opaque,
}

#[derive(Debug, Clone, Copy)]
struct SourceTemplate {
    source_id: &'static str,
    source_kind: &'static str,
    default_path: &'static str,
    authoritative_for: &'static [&'static str],
    format: SourceInputFormat,
    default_redaction_state: &'static str,
}

const SOURCE_TEMPLATES: &[SourceTemplate] = &[
    SourceTemplate {
        source_id: "beads_jsonl",
        source_kind: "beads",
        default_path: ".beads/issues.jsonl",
        authoritative_for: &["bead_lifecycle"],
        format: SourceInputFormat::JsonLines,
        default_redaction_state: "none",
    },
    SourceTemplate {
        source_id: "beads_db",
        source_kind: "beads",
        default_path: ".beads/beads.db",
        authoritative_for: &["bead_lifecycle"],
        format: SourceInputFormat::Opaque,
        default_redaction_state: "none",
    },
    SourceTemplate {
        source_id: "agent_mail_archive",
        source_kind: "agent_mail",
        default_path: "/home/ubuntu/.mcp_agent_mail_git_mailbox_repo/storage.sqlite3",
        authoritative_for: &[
            "reservation_intent",
            "reservation_conflict",
            "agent_message",
            "build_slot_state",
        ],
        format: SourceInputFormat::Json,
        default_redaction_state: "sensitive_omitted",
    },
    SourceTemplate {
        source_id: "doctor_swarm_diagnostics",
        source_kind: "doctor",
        default_path: "docs/evidence/doctor-swarm.json",
        authoritative_for: &["doctor_finding", "host_resource_profile"],
        format: SourceInputFormat::Json,
        default_redaction_state: "redacted",
    },
    SourceTemplate {
        source_id: "rch_queue_status",
        source_kind: "rch",
        default_path: "docs/evidence/rch-queue-status.json",
        authoritative_for: &["rch_job_state"],
        format: SourceInputFormat::Json,
        default_redaction_state: "none",
    },
    SourceTemplate {
        source_id: "operator_runpack",
        source_kind: "runpack",
        default_path: "docs/evidence/swarm-operator-runpack.json",
        authoritative_for: &["runpack_recommendation", "operator_handoff"],
        format: SourceInputFormat::Json,
        default_redaction_state: "redacted",
    },
    SourceTemplate {
        source_id: "git_refs",
        source_kind: "git",
        default_path: ".git",
        authoritative_for: &["worktree_state"],
        format: SourceInputFormat::Json,
        default_redaction_state: "none",
    },
    SourceTemplate {
        source_id: "validation_command_records",
        source_kind: "validation",
        default_path: "tests/e2e_results",
        authoritative_for: &["cargo_gate_result", "validation_artifact"],
        format: SourceInputFormat::Json,
        default_redaction_state: "none",
    },
    SourceTemplate {
        source_id: "context_intelligence_evidence",
        source_kind: "context_intelligence",
        default_path: "docs/evidence/context-intelligence-closeout-gate.json",
        authoritative_for: &["validation_artifact"],
        format: SourceInputFormat::Json,
        default_redaction_state: "redacted",
    },
    SourceTemplate {
        source_id: "swarm_flight_recorder",
        source_kind: "flight_recorder",
        default_path: "tests/full_suite_gate/swarm_flight_recorder.jsonl",
        authoritative_for: &["validation_artifact"],
        format: SourceInputFormat::JsonLines,
        default_redaction_state: "redacted",
    },
    SourceTemplate {
        source_id: "swarm_activity_ledger",
        source_kind: "activity_ledger",
        default_path: "tests/full_suite_gate/swarm_activity_ledger.jsonl",
        authoritative_for: &["operator_handoff", "validation_artifact"],
        format: SourceInputFormat::JsonLines,
        default_redaction_state: "redacted",
    },
];

/// Request used to build a replay trace from existing artifacts.
#[derive(Debug, Clone)]
pub struct SwarmReplayIngestRequest {
    /// Stable trace identifier.
    pub trace_id: String,
    /// Fixed generation timestamp in UTC RFC3339 `Z` format.
    pub generated_at_utc: String,
    /// Workspace root used for relative source paths.
    pub workspace_root: PathBuf,
    /// Optional git commit recorded in worktree and provenance payloads.
    pub git_commit: Option<String>,
    /// Optional git branch recorded in worktree payloads.
    pub git_branch: Option<String>,
    /// Per-source path overrides. Relative paths are resolved under `workspace_root`.
    pub source_overrides: BTreeMap<String, PathBuf>,
}

impl SwarmReplayIngestRequest {
    /// Create a new replay ingest request.
    pub fn new(
        trace_id: impl Into<String>,
        generated_at_utc: impl Into<String>,
        workspace_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            trace_id: trace_id.into(),
            generated_at_utc: generated_at_utc.into(),
            workspace_root: workspace_root.into(),
            git_commit: None,
            git_branch: None,
            source_overrides: BTreeMap::new(),
        }
    }

    /// Attach immutable git identity metadata for the trace.
    #[must_use]
    pub fn with_git_identity(
        mut self,
        git_commit: impl Into<String>,
        git_branch: impl Into<String>,
    ) -> Self {
        self.git_commit = Some(git_commit.into());
        self.git_branch = Some(git_branch.into());
        self
    }

    /// Override one source path.
    #[must_use]
    pub fn with_source_override(
        mut self,
        source_id: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Self {
        self.source_overrides.insert(source_id.into(), path.into());
        self
    }
}

/// One row in the trace source inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplaySourceInventoryRow {
    pub source_id: String,
    pub source_kind: String,
    pub path: String,
    pub availability: String,
    pub freshness_state: String,
    pub source_hash: Option<String>,
    pub redaction_state: String,
    pub authoritative_for: Vec<String>,
    pub uncertainty: Vec<String>,
}

/// Uncertainty attached to one normalized event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayEventUncertainty {
    pub state: String,
    pub reasons: Vec<String>,
    pub suppressed_claims: Vec<String>,
}

/// One normalized replay event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayEvent {
    pub event_id: String,
    pub sequence: u64,
    pub occurred_at_utc: String,
    pub observed_at_utc: String,
    pub event_type: String,
    pub actor: String,
    pub source_ref: String,
    pub source_hash: Option<String>,
    pub redaction_state: String,
    pub uncertainty: SwarmReplayEventUncertainty,
    pub payload: Value,
}

/// Ordering policy recorded in every trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayOrdering {
    pub monotonic_sequence_required: bool,
    pub timestamp_normalization: String,
    pub tie_breakers: Vec<String>,
}

/// Redaction accounting for the trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayRedactionSummary {
    pub redacted_count: u64,
    pub sensitive_omitted_count: u64,
    pub raw_secret_bytes_emitted: u64,
    pub redacted_fields: Vec<String>,
}

/// Uncertainty accounting for the trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayUncertaintySummary {
    pub missing_sources: Vec<String>,
    pub malformed_sources: Vec<String>,
    pub stale_sources: Vec<String>,
    pub suppressed_claims: Vec<String>,
    pub event_count_by_uncertainty: BTreeMap<String, u64>,
}

/// Guards proving the trace is offline evidence, not live control.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayGuards {
    pub read_only: bool,
    pub no_live_mutation: bool,
    pub no_network_required: bool,
    pub fail_closed_on_missing_required_sources: bool,
    pub requires_source_inventory: bool,
    pub disallowed_live_actions: Vec<String>,
}

/// Normalized offline replay trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayTrace {
    pub schema: String,
    pub trace_id: String,
    pub generated_at: String,
    pub contract_version: String,
    pub source_inventory: Vec<SwarmReplaySourceInventoryRow>,
    pub ordering: SwarmReplayOrdering,
    pub events: Vec<SwarmReplayEvent>,
    pub redaction_summary: SwarmReplayRedactionSummary,
    pub uncertainty_summary: SwarmReplayUncertaintySummary,
    pub replay_guards: SwarmReplayGuards,
}

/// Schema emitted by the deterministic replay engine.
pub const SWARM_REPLAY_REPORT_SCHEMA: &str = "pi.swarm.replay_report.v1";

/// Deterministic report emitted after replaying a normalized trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayReport {
    pub schema: String,
    pub trace_id: String,
    pub replayed_event_count: u64,
    pub final_logical_clock: u64,
    pub snapshots: Vec<SwarmReplayStateSnapshot>,
    pub resource_pressure_timeline: Vec<SwarmReplayResourcePressureSnapshot>,
    pub final_state: SwarmReplayState,
    pub diagnostics: Vec<SwarmReplayDiagnostic>,
    pub replay_guards: SwarmReplayEngineGuards,
}

/// Replay-engine guards proving the engine stayed offline and read-only.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayEngineGuards {
    pub read_only: bool,
    pub no_live_mutation: bool,
    pub no_network_required: bool,
    pub consumed_trace_only: bool,
}

/// Full swarm state after one replayed event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayStateSnapshot {
    pub logical_clock: u64,
    pub event_id: String,
    pub occurred_at_utc: String,
    pub state: SwarmReplayState,
    pub diagnostic_count: u64,
}

/// Diagnostic emitted for invariant violations or uncertain replay evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayDiagnostic {
    pub code: String,
    pub severity: String,
    pub event_id: Option<String>,
    pub logical_clock: Option<u64>,
    pub message: String,
    pub details: Value,
}

/// Reconstructed swarm state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayState {
    pub beads: BTreeMap<String, SwarmReplayBeadState>,
    pub agents: BTreeMap<String, SwarmReplayAgentState>,
    pub reservations: BTreeMap<String, SwarmReplayReservationState>,
    pub build_slots: BTreeMap<String, SwarmReplayBuildSlotState>,
    pub rch_jobs: BTreeMap<String, SwarmReplayRchJobState>,
    pub validation_gates: BTreeMap<String, SwarmReplayValidationGateState>,
    pub runpack_recommendations: BTreeMap<String, SwarmReplayRunpackRecommendationState>,
    pub operator_handoffs: BTreeMap<String, SwarmReplayOperatorHandoffState>,
    pub worktree: Option<SwarmReplayWorktreeState>,
    pub resource_budget: Option<SwarmReplayResourceBudgetState>,
    pub coordination: SwarmReplayCoordinationState,
}

impl Default for SwarmReplayState {
    fn default() -> Self {
        Self {
            beads: BTreeMap::new(),
            agents: BTreeMap::new(),
            reservations: BTreeMap::new(),
            build_slots: BTreeMap::new(),
            rch_jobs: BTreeMap::new(),
            validation_gates: BTreeMap::new(),
            runpack_recommendations: BTreeMap::new(),
            operator_handoffs: BTreeMap::new(),
            worktree: None,
            resource_budget: None,
            coordination: SwarmReplayCoordinationState {
                agent_mail_available: true,
                missing_agent_mail_evidence: false,
                reservation_conflict_count: 0,
                last_operator_action: None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayBeadState {
    pub bead_id: String,
    pub status: String,
    pub priority: i64,
    pub assignee: String,
    pub last_event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayAgentState {
    pub agent_name: String,
    pub last_event_id: String,
    pub last_seen_at_utc: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayReservationState {
    pub reservation_id: String,
    pub holder: String,
    pub path_patterns: Vec<String>,
    pub exclusive: bool,
    pub state: String,
    pub active: bool,
    pub last_event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayBuildSlotState {
    pub slot: String,
    pub holder: String,
    pub state: String,
    pub expires_at_utc: String,
    pub last_event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayRchJobState {
    pub job_id: String,
    pub state: String,
    pub worker: String,
    pub command: String,
    pub queue_position: i64,
    pub stale_progress: bool,
    pub last_event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayValidationGateState {
    pub gate_id: String,
    pub command: String,
    pub runner: String,
    pub exit_code: i64,
    pub target_dir: String,
    pub tmpdir: String,
    pub last_event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayRunpackRecommendationState {
    pub action: String,
    pub severity: String,
    pub evidence_paths: Vec<String>,
    pub last_event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayOperatorHandoffState {
    pub handoff_id: String,
    pub summary: String,
    pub next_actions: Vec<String>,
    pub evidence_paths: Vec<String>,
    pub last_event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayWorktreeState {
    pub head: String,
    pub branch: String,
    pub dirty: bool,
    pub changed_paths: Vec<String>,
    pub last_event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayResourceBudgetState {
    pub profile_id: String,
    pub cpu_cores: Option<u64>,
    pub memory_gib: Option<u64>,
    pub numa_nodes: Option<u64>,
    pub cgroup_cpu_quota: Option<u64>,
    pub cgroup_memory_gib: Option<u64>,
    pub max_agent_concurrency: Option<u64>,
    pub max_tool_concurrency: Option<u64>,
    pub extension_hostcall_lanes: Option<u64>,
    pub rch_worker_slots: Option<u64>,
    pub target_dir: String,
    pub target_free_gib: Option<u64>,
    pub tmpdir: String,
    pub tmpdir_free_gib: Option<u64>,
    pub numa_hint: String,
    pub last_event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayResourcePressureSnapshot {
    pub logical_clock: u64,
    pub event_id: String,
    pub profile_id: Option<String>,
    pub active_agents: u64,
    pub active_build_slots: u64,
    pub active_rch_jobs: u64,
    pub rch_queue_depth: u64,
    pub estimated_rss_gib: Option<u64>,
    pub cpu_pressure: String,
    pub memory_pressure: String,
    pub tmpdir_pressure: String,
    pub target_dir_pressure: String,
    pub rch_worker_pressure: String,
    pub extension_lane_pressure: String,
    pub saturation_reasons: Vec<String>,
    pub missing_data: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayCoordinationState {
    pub agent_mail_available: bool,
    pub missing_agent_mail_evidence: bool,
    pub reservation_conflict_count: u64,
    pub last_operator_action: Option<String>,
}

/// Schema emitted by the replay policy evaluator.
pub const SWARM_REPLAY_POLICY_REPORT_SCHEMA: &str = "pi.swarm.policy_report.v1";

/// Offline report comparing replay policy decisions over one replay output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayPolicyReport {
    pub schema: String,
    pub trace_id: String,
    pub policy_ids: Vec<String>,
    pub decision_count: u64,
    pub decisions: Vec<SwarmReplayPolicyDecision>,
    pub comparison_count: u64,
    pub policy_comparisons: Vec<SwarmReplayPolicyComparison>,
    pub policy_guards: SwarmReplayPolicyGuards,
}

/// Guards proving policy evaluation stayed advisory and replay-only.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayPolicyGuards {
    pub advisory_only: bool,
    pub no_live_mutation: bool,
    pub no_network_required: bool,
    pub consumed_replay_report_only: bool,
}

/// One advisory policy decision for one replay snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayPolicyDecision {
    pub policy_id: String,
    pub logical_clock: u64,
    pub event_id: String,
    pub action: String,
    pub target_kind: String,
    pub target_id: String,
    pub reason_codes: Vec<String>,
    pub source_evidence: Vec<SwarmReplayPolicyEvidenceRef>,
    pub expected_risk: String,
    pub would_require_live_mutation: bool,
    pub advisory_only: bool,
}

/// Source evidence attached to an advisory policy decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayPolicyEvidenceRef {
    pub evidence_kind: String,
    pub evidence_id: String,
    pub event_id: String,
    pub logical_clock: u64,
    pub detail: String,
}

/// Aggregated comparison row for one policy across a replay report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayPolicyComparison {
    pub policy_id: String,
    pub rank: u64,
    pub score: i64,
    pub metrics: SwarmReplayPolicyMetrics,
    pub confidence: SwarmReplayPolicyConfidence,
    pub missing_data: Vec<SwarmReplayPolicyMissingData>,
    pub rationale: Vec<String>,
}

/// Machine-readable metrics used to compare policy behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayPolicyMetrics {
    pub completed_beads: u64,
    pub throughput_actions: u64,
    pub blocked_time_minutes: Option<u64>,
    pub average_wait_minutes: Option<u64>,
    pub p95_wait_minutes: Option<u64>,
    pub rch_slot_occupancy_events: u64,
    pub local_fallback_risk: String,
    pub reservation_conflicts_avoided: u64,
    pub stale_work_reclaimed: u64,
    pub validation_commands_deferred: u64,
    pub evidence_freshness: String,
    pub operator_handoff_quality: String,
    pub resource_budget: SwarmReplayPolicyResourceMetrics,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayPolicyResourceMetrics {
    pub profile_id: Option<String>,
    pub saturation_events: u64,
    pub first_saturation_point: Option<String>,
    pub peak_agent_concurrency: u64,
    pub peak_tool_concurrency: u64,
    pub peak_rch_fanout: u64,
    pub peak_rch_queue_depth: u64,
    pub peak_estimated_rss_gib: Option<u64>,
    pub cpu_pressure: String,
    pub memory_pressure: String,
    pub tmpdir_pressure: String,
    pub target_dir_pressure: String,
    pub rch_worker_pressure: String,
    pub extension_lane_pressure: String,
    pub numa_hint: String,
}

/// Schema emitted by replay performance-budget evidence.
pub const SWARM_REPLAY_PERFORMANCE_EVIDENCE_SCHEMA: &str =
    "pi.swarm.replay_performance_evidence.v1";

/// CI-friendly performance budget for replay and policy-report generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayPerformanceBudget {
    pub budget_id: String,
    pub min_replayed_events: u64,
    pub max_wall_time_ms: u64,
    pub max_peak_rss_gib: u64,
    pub max_report_size_bytes: u64,
    pub max_ordering_cost_units: u64,
}

/// Host and runner observations supplied by the measurement harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayPerformanceObservation {
    pub command: String,
    pub runner: String,
    pub host_profile_id: Option<String>,
    pub wall_time_ms: Option<u64>,
    pub peak_rss_gib: Option<u64>,
    pub caveats: Vec<String>,
}

/// Deterministic metrics derived from replay outputs plus harness observations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayPerformanceObservedMetrics {
    pub replayed_event_count: u64,
    pub policy_decision_count: u64,
    pub snapshot_count: u64,
    pub wall_time_ms: Option<u64>,
    pub peak_rss_gib: Option<u64>,
    pub report_size_bytes: Option<u64>,
    pub ordering_cost_units: Option<u64>,
}

/// One pass/fail/missing performance-budget check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayPerformanceCheck {
    pub metric: String,
    pub comparator: String,
    pub threshold: u64,
    pub observed: Option<u64>,
    pub status: String,
    pub reason: String,
}

/// Guards proving performance evidence remains advisory and claim-gated.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayPerformanceEvidenceGuards {
    pub read_only: bool,
    pub no_live_mutation: bool,
    pub no_network_required: bool,
    pub evidence_only: bool,
    pub release_claims_suppressed: bool,
}

/// Machine-readable performance evidence for a replay run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayPerformanceEvidence {
    pub schema: String,
    pub trace_id: String,
    pub budget: SwarmReplayPerformanceBudget,
    pub observation: SwarmReplayPerformanceObservation,
    pub observed: SwarmReplayPerformanceObservedMetrics,
    pub checks: Vec<SwarmReplayPerformanceCheck>,
    pub verdict: String,
    pub missing_data: Vec<String>,
    pub caveats: Vec<String>,
    pub guards: SwarmReplayPerformanceEvidenceGuards,
}

/// Confidence attached to policy comparison metrics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayPolicyConfidence {
    pub level: String,
    pub score: u64,
    pub reasons: Vec<String>,
}

/// Missing evidence that suppresses one or more comparison metrics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmReplayPolicyMissingData {
    pub claim: String,
    pub reasons: Vec<String>,
    pub suppressed_metrics: Vec<String>,
}

/// Context supplied to policy adapters for each replay snapshot.
pub struct SwarmReplayPolicyContext<'a> {
    pub trace_id: &'a str,
    pub final_state: &'a SwarmReplayState,
    pub diagnostics: &'a [SwarmReplayDiagnostic],
}

/// Advisory replay-time policy adapter.
pub trait SwarmReplayPolicyAdapter {
    /// Stable policy identifier used in reports.
    fn policy_id(&self) -> &'static str;

    /// Evaluate one replay snapshot without mutating live systems.
    fn evaluate_snapshot(
        &self,
        snapshot: &SwarmReplayStateSnapshot,
        context: &SwarmReplayPolicyContext<'_>,
    ) -> Vec<SwarmReplayPolicyDecision>;
}

/// Built-in baseline replay policies used for comparisons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmReplayBaselinePolicy {
    ConservativeManual,
    ExistingAutopilot,
    RchFanoutLimited,
    StaleBeadReclaiming,
    BuildSlotProtective,
}

impl SwarmReplayPolicyAdapter for SwarmReplayBaselinePolicy {
    fn policy_id(&self) -> &'static str {
        match self {
            Self::ConservativeManual => "conservative_manual",
            Self::ExistingAutopilot => "existing_autopilot",
            Self::RchFanoutLimited => "rch_fanout_limited",
            Self::StaleBeadReclaiming => "stale_bead_reclaiming",
            Self::BuildSlotProtective => "build_slot_protective",
        }
    }

    fn evaluate_snapshot(
        &self,
        snapshot: &SwarmReplayStateSnapshot,
        context: &SwarmReplayPolicyContext<'_>,
    ) -> Vec<SwarmReplayPolicyDecision> {
        match self {
            Self::ConservativeManual => conservative_manual_decisions(self.policy_id(), snapshot),
            Self::ExistingAutopilot => existing_autopilot_decisions(self.policy_id(), snapshot),
            Self::RchFanoutLimited => rch_fanout_limited_decisions(self.policy_id(), snapshot),
            Self::StaleBeadReclaiming => {
                stale_bead_reclaiming_decisions(self.policy_id(), snapshot, context)
            }
            Self::BuildSlotProtective => {
                build_slot_protective_decisions(self.policy_id(), snapshot)
            }
        }
    }
}

/// Return the full built-in replay policy set in deterministic order.
pub const fn default_swarm_replay_baseline_policies() -> [SwarmReplayBaselinePolicy; 5] {
    [
        SwarmReplayBaselinePolicy::ConservativeManual,
        SwarmReplayBaselinePolicy::ExistingAutopilot,
        SwarmReplayBaselinePolicy::RchFanoutLimited,
        SwarmReplayBaselinePolicy::StaleBeadReclaiming,
        SwarmReplayBaselinePolicy::BuildSlotProtective,
    ]
}

/// Evaluate the built-in baseline policies over a replay report.
pub fn evaluate_swarm_replay_baseline_policies(
    report: &SwarmReplayReport,
    policies: &[SwarmReplayBaselinePolicy],
) -> Result<SwarmReplayPolicyReport> {
    let adapters = policies
        .iter()
        .map(|policy| policy as &dyn SwarmReplayPolicyAdapter)
        .collect::<Vec<_>>();
    evaluate_swarm_replay_policies(report, &adapters)
}

/// Evaluate advisory replay policy adapters over a replay report.
pub fn evaluate_swarm_replay_policies(
    report: &SwarmReplayReport,
    policies: &[&dyn SwarmReplayPolicyAdapter],
) -> Result<SwarmReplayPolicyReport> {
    if report.schema != SWARM_REPLAY_REPORT_SCHEMA {
        return Err(Error::validation(format!(
            "unsupported swarm replay report schema {}",
            report.schema
        )));
    }

    let context = SwarmReplayPolicyContext {
        trace_id: report.trace_id.as_str(),
        final_state: &report.final_state,
        diagnostics: &report.diagnostics,
    };
    let mut policy_ids = BTreeSet::new();
    let mut decisions = Vec::new();
    for policy in policies {
        policy_ids.insert(policy.policy_id().to_string());
        for snapshot in &report.snapshots {
            decisions.extend(policy.evaluate_snapshot(snapshot, &context));
        }
    }
    decisions.sort_by(|left, right| {
        left.policy_id
            .cmp(&right.policy_id)
            .then_with(|| left.logical_clock.cmp(&right.logical_clock))
            .then_with(|| left.action.cmp(&right.action))
            .then_with(|| left.target_kind.cmp(&right.target_kind))
            .then_with(|| left.target_id.cmp(&right.target_id))
            .then_with(|| left.reason_codes.cmp(&right.reason_codes))
    });
    let policy_ids = policy_ids.into_iter().collect::<Vec<_>>();
    let policy_comparisons = build_swarm_replay_policy_comparisons(report, &policy_ids, &decisions);

    Ok(SwarmReplayPolicyReport {
        schema: SWARM_REPLAY_POLICY_REPORT_SCHEMA.to_string(),
        trace_id: report.trace_id.clone(),
        policy_ids,
        decision_count: u64::try_from(decisions.len()).unwrap_or(u64::MAX),
        decisions,
        comparison_count: u64::try_from(policy_comparisons.len()).unwrap_or(u64::MAX),
        policy_comparisons,
        policy_guards: SwarmReplayPolicyGuards {
            advisory_only: true,
            no_live_mutation: true,
            no_network_required: true,
            consumed_replay_report_only: true,
        },
    })
}

/// Build claim-gated performance evidence for one replay and optional policy report.
pub fn build_swarm_replay_performance_evidence(
    report: &SwarmReplayReport,
    policy_report: Option<&SwarmReplayPolicyReport>,
    budget: SwarmReplayPerformanceBudget,
    observation: SwarmReplayPerformanceObservation,
) -> Result<SwarmReplayPerformanceEvidence> {
    if report.schema != SWARM_REPLAY_REPORT_SCHEMA {
        return Err(Error::validation(format!(
            "unsupported swarm replay report schema {}",
            report.schema
        )));
    }
    if let Some(policy_report) = policy_report
        && policy_report.schema != SWARM_REPLAY_POLICY_REPORT_SCHEMA
    {
        return Err(Error::validation(format!(
            "unsupported swarm replay policy report schema {}",
            policy_report.schema
        )));
    }

    let report_size_bytes = serialized_json_len(report)?
        .saturating_add(policy_report.map_or(Ok(0), serialized_json_len)?);
    let ordering_cost_units = swarm_replay_ordering_cost_units(report.replayed_event_count);
    let policy_decision_count = policy_report.map_or(0, |report| report.decision_count);
    let snapshot_count = u64::try_from(report.snapshots.len()).unwrap_or(u64::MAX);

    let observed = SwarmReplayPerformanceObservedMetrics {
        replayed_event_count: report.replayed_event_count,
        policy_decision_count,
        snapshot_count,
        wall_time_ms: observation.wall_time_ms,
        peak_rss_gib: observation.peak_rss_gib,
        report_size_bytes: Some(report_size_bytes),
        ordering_cost_units: Some(ordering_cost_units),
    };
    let checks = vec![
        min_performance_check(
            "replayed_event_count",
            budget.min_replayed_events,
            Some(observed.replayed_event_count),
        ),
        max_performance_check(
            "wall_time_ms",
            budget.max_wall_time_ms,
            observed.wall_time_ms,
        ),
        max_performance_check(
            "peak_rss_gib",
            budget.max_peak_rss_gib,
            observed.peak_rss_gib,
        ),
        max_performance_check(
            "report_size_bytes",
            budget.max_report_size_bytes,
            observed.report_size_bytes,
        ),
        max_performance_check(
            "ordering_cost_units",
            budget.max_ordering_cost_units,
            observed.ordering_cost_units,
        ),
    ];
    let missing_data = checks
        .iter()
        .filter(|check| check.status == "missing")
        .map(|check| check.reason.clone())
        .collect::<Vec<_>>();
    let verdict = if checks.iter().all(|check| check.status == "pass") {
        "pass"
    } else {
        "fail"
    };
    let caveats = performance_evidence_caveats(report, policy_report, &observation);

    Ok(SwarmReplayPerformanceEvidence {
        schema: SWARM_REPLAY_PERFORMANCE_EVIDENCE_SCHEMA.to_string(),
        trace_id: report.trace_id.clone(),
        budget,
        observation,
        observed,
        checks,
        verdict: verdict.to_string(),
        missing_data,
        caveats,
        guards: SwarmReplayPerformanceEvidenceGuards {
            read_only: true,
            no_live_mutation: true,
            no_network_required: true,
            evidence_only: true,
            release_claims_suppressed: true,
        },
    })
}

/// Estimate deterministic ordering work as `n * ceil(log2(n))`.
#[must_use]
pub fn swarm_replay_ordering_cost_units(event_count: u64) -> u64 {
    if event_count <= 1 {
        return event_count;
    }
    let log_width = u64::from(u64::BITS - event_count.saturating_sub(1).leading_zeros());
    event_count.saturating_mul(log_width)
}

fn serialized_json_len<T: Serialize>(value: &T) -> Result<u64> {
    serde_json::to_vec(value)
        .map_err(|err| Error::Json(Box::new(err)))
        .map(|bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
}

fn min_performance_check(
    metric: &str,
    threshold: u64,
    observed: Option<u64>,
) -> SwarmReplayPerformanceCheck {
    performance_check(metric, ">=", threshold, observed, |value, limit| {
        value >= limit
    })
}

fn max_performance_check(
    metric: &str,
    threshold: u64,
    observed: Option<u64>,
) -> SwarmReplayPerformanceCheck {
    performance_check(metric, "<=", threshold, observed, |value, limit| {
        value <= limit
    })
}

fn performance_check(
    metric: &str,
    comparator: &str,
    threshold: u64,
    observed: Option<u64>,
    passes: impl FnOnce(u64, u64) -> bool,
) -> SwarmReplayPerformanceCheck {
    let (status, reason) = match observed {
        Some(value) if passes(value, threshold) => (
            "pass".to_string(),
            format!("{metric} observed {value} {comparator} threshold {threshold}"),
        ),
        Some(value) => (
            "fail".to_string(),
            format!("{metric} observed {value} exceeded budget threshold {threshold}"),
        ),
        None => (
            "missing".to_string(),
            format!("{metric} observation missing; performance claim suppressed"),
        ),
    };
    SwarmReplayPerformanceCheck {
        metric: metric.to_string(),
        comparator: comparator.to_string(),
        threshold,
        observed,
        status,
        reason,
    }
}

fn performance_evidence_caveats(
    report: &SwarmReplayReport,
    policy_report: Option<&SwarmReplayPolicyReport>,
    observation: &SwarmReplayPerformanceObservation,
) -> Vec<String> {
    let mut caveats = observation.caveats.iter().cloned().collect::<BTreeSet<_>>();
    match &report.final_state.resource_budget {
        Some(profile) => {
            if observation.host_profile_id.as_deref() != Some(profile.profile_id.as_str()) {
                caveats.insert("host_profile_observation_mismatch".to_string());
            }
            if profile.cgroup_cpu_quota.is_none() || profile.cgroup_memory_gib.is_none() {
                caveats.insert("cgroup_limit_observation_partial".to_string());
            }
        }
        None => {
            caveats.insert("host_resource_profile_missing".to_string());
        }
    }
    if policy_report.is_none() {
        caveats.insert("policy_report_not_supplied".to_string());
    }
    if !report.diagnostics.is_empty() {
        caveats.insert("replay_diagnostics_present".to_string());
    }
    caveats.into_iter().collect()
}

#[derive(Debug, Clone)]
struct PolicyLatencyMetrics {
    blocked_time_minutes: Option<u64>,
    average_wait_minutes: Option<u64>,
    p95_wait_minutes: Option<u64>,
    missing_reasons: Vec<String>,
}

fn build_swarm_replay_policy_comparisons(
    report: &SwarmReplayReport,
    policy_ids: &[String],
    decisions: &[SwarmReplayPolicyDecision],
) -> Vec<SwarmReplayPolicyComparison> {
    let mut comparisons = policy_ids
        .iter()
        .map(|policy_id| {
            let policy_decisions = decisions
                .iter()
                .filter(|decision| decision.policy_id == *policy_id)
                .collect::<Vec<_>>();
            let latency = policy_latency_metrics(report, &policy_decisions);
            let mut missing_data = comparison_missing_data(report, &latency);
            let metrics = policy_comparison_metrics(report, &policy_decisions, &latency);
            let score = policy_comparison_score(&metrics, missing_data.len());
            let confidence = policy_comparison_confidence(report, &metrics, &missing_data);
            let rationale = policy_comparison_rationale(&metrics, score, &missing_data);

            if policy_decisions.is_empty() {
                missing_data.push(SwarmReplayPolicyMissingData {
                    claim: "policy_decision_coverage".to_string(),
                    reasons: vec![
                        "policy emitted no advisory decisions for this replay".to_string(),
                    ],
                    suppressed_metrics: vec!["policy_effect_ranking".to_string()],
                });
            }

            SwarmReplayPolicyComparison {
                policy_id: policy_id.clone(),
                rank: 0,
                score,
                metrics,
                confidence,
                missing_data,
                rationale,
            }
        })
        .collect::<Vec<_>>();

    comparisons.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.policy_id.cmp(&right.policy_id))
    });
    for (index, comparison) in comparisons.iter_mut().enumerate() {
        comparison.rank = u64::try_from(index + 1).unwrap_or(u64::MAX);
    }
    comparisons
}

fn policy_comparison_metrics(
    report: &SwarmReplayReport,
    decisions: &[&SwarmReplayPolicyDecision],
    latency: &PolicyLatencyMetrics,
) -> SwarmReplayPolicyMetrics {
    let resource_budget = policy_resource_metrics(report, decisions);
    SwarmReplayPolicyMetrics {
        completed_beads: count_completed_beads(&report.final_state),
        throughput_actions: count_throughput_actions(decisions),
        blocked_time_minutes: latency.blocked_time_minutes,
        average_wait_minutes: latency.average_wait_minutes,
        p95_wait_minutes: latency.p95_wait_minutes,
        rch_slot_occupancy_events: count_rch_slot_occupancy_events(report),
        local_fallback_risk: local_fallback_risk(report, decisions),
        reservation_conflicts_avoided: count_reservation_conflicts_avoided(report, decisions),
        stale_work_reclaimed: count_decisions_with_actions(decisions, &["reclaim_stale_bead"]),
        validation_commands_deferred: count_decisions_with_actions(
            decisions,
            &["back_off_cargo", "wait_for_build_slot"],
        ),
        evidence_freshness: evidence_freshness(report, latency),
        operator_handoff_quality: operator_handoff_quality(&report.final_state),
        resource_budget,
    }
}

fn policy_latency_metrics(
    report: &SwarmReplayReport,
    decisions: &[&SwarmReplayPolicyDecision],
) -> PolicyLatencyMetrics {
    let mut parsed_times = Vec::with_capacity(report.snapshots.len());
    let mut missing_reasons = Vec::new();
    for snapshot in &report.snapshots {
        match DateTime::parse_from_rfc3339(&snapshot.occurred_at_utc) {
            Ok(timestamp) => parsed_times.push(timestamp),
            Err(_) => {
                missing_reasons.push(format!(
                    "snapshot:{}:{} invalid occurred_at_utc",
                    snapshot.logical_clock, snapshot.event_id
                ));
            }
        }
    }

    if !missing_reasons.is_empty() {
        missing_reasons.sort();
        return PolicyLatencyMetrics {
            blocked_time_minutes: None,
            average_wait_minutes: None,
            p95_wait_minutes: None,
            missing_reasons,
        };
    }

    let mut wait_minutes = Vec::new();
    for decision in decisions
        .iter()
        .filter(|decision| policy_action_waits_for_external_state(&decision.action))
    {
        let Some(index) = report
            .snapshots
            .iter()
            .position(|snapshot| snapshot.logical_clock == decision.logical_clock)
        else {
            continue;
        };
        let Some(next_timestamp) = parsed_times.get(index + 1) else {
            wait_minutes.push(0);
            continue;
        };
        let Some(current_timestamp) = parsed_times.get(index) else {
            continue;
        };
        let minutes = next_timestamp
            .signed_duration_since(*current_timestamp)
            .num_minutes()
            .max(0);
        wait_minutes.push(u64::try_from(minutes).unwrap_or(0));
    }

    if wait_minutes.is_empty() {
        return PolicyLatencyMetrics {
            blocked_time_minutes: Some(0),
            average_wait_minutes: Some(0),
            p95_wait_minutes: Some(0),
            missing_reasons,
        };
    }

    wait_minutes.sort_unstable();
    let blocked_time = wait_minutes.iter().copied().sum::<u64>();
    let average_wait = blocked_time / u64::try_from(wait_minutes.len()).unwrap_or(1);
    let p95_index = percentile_index(wait_minutes.len(), 95);

    PolicyLatencyMetrics {
        blocked_time_minutes: Some(blocked_time),
        average_wait_minutes: Some(average_wait),
        p95_wait_minutes: wait_minutes.get(p95_index).copied(),
        missing_reasons,
    }
}

const fn percentile_index(len: usize, percentile: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let numerator = len * percentile;
    let ceil_rank = numerator.div_ceil(100);
    ceil_rank.saturating_sub(1)
}

fn comparison_missing_data(
    report: &SwarmReplayReport,
    latency: &PolicyLatencyMetrics,
) -> Vec<SwarmReplayPolicyMissingData> {
    let mut missing = Vec::new();
    if !latency.missing_reasons.is_empty() {
        missing.push(SwarmReplayPolicyMissingData {
            claim: "latency_claims".to_string(),
            reasons: latency.missing_reasons.clone(),
            suppressed_metrics: vec![
                "blocked_time_minutes".to_string(),
                "average_wait_minutes".to_string(),
                "p95_wait_minutes".to_string(),
            ],
        });
    }
    if report
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "agent_mail_source_unavailable")
    {
        missing.push(SwarmReplayPolicyMissingData {
            claim: "coordination_completeness".to_string(),
            reasons: vec!["agent mail evidence unavailable in replay diagnostics".to_string()],
            suppressed_metrics: vec![
                "reservation_conflicts_avoided".to_string(),
                "operator_handoff_quality".to_string(),
            ],
        });
    }
    let resource_missing = resource_budget_missing_claims(report);
    if !resource_missing.is_empty() {
        missing.push(SwarmReplayPolicyMissingData {
            claim: "resource_budget_claims".to_string(),
            reasons: resource_missing,
            suppressed_metrics: vec![
                "resource_budget.peak_agent_concurrency".to_string(),
                "resource_budget.peak_tool_concurrency".to_string(),
                "resource_budget.peak_rch_fanout".to_string(),
                "resource_budget.peak_estimated_rss_gib".to_string(),
                "resource_budget.cpu_pressure".to_string(),
                "resource_budget.memory_pressure".to_string(),
                "resource_budget.tmpdir_pressure".to_string(),
                "resource_budget.target_dir_pressure".to_string(),
                "resource_budget.extension_lane_pressure".to_string(),
            ],
        });
    }
    missing
}

fn policy_comparison_score(metrics: &SwarmReplayPolicyMetrics, missing_data_count: usize) -> i64 {
    let risk_penalty = match metrics.local_fallback_risk.as_str() {
        "high" => 12,
        "medium" => 5,
        _ => 0,
    };
    let rch_pressure_bonus =
        if metrics.rch_slot_occupancy_events > 0 && metrics.validation_commands_deferred > 0 {
            8
        } else {
            0
        };
    let conflict_bonus = if metrics.reservation_conflicts_avoided > 0 {
        4
    } else {
        0
    };
    let handoff_bonus = match metrics.operator_handoff_quality.as_str() {
        "complete" => 3,
        "partial" => 1,
        _ => 0,
    };

    i64::try_from(metrics.throughput_actions).unwrap_or(i64::MAX / 12) * 12
        + i64::try_from(metrics.completed_beads).unwrap_or(i64::MAX / 5) * 5
        + i64::try_from(metrics.reservation_conflicts_avoided).unwrap_or(i64::MAX / 4) * 4
        + i64::try_from(metrics.stale_work_reclaimed).unwrap_or(i64::MAX / 10) * 10
        + i64::from(handoff_bonus)
        + i64::from(rch_pressure_bonus)
        + i64::from(conflict_bonus)
        - i64::try_from(metrics.validation_commands_deferred).unwrap_or(i64::MAX / 2) * 2
        - i64::from(risk_penalty)
        - i64::try_from(missing_data_count).unwrap_or(i64::MAX / 8) * 8
}

fn policy_comparison_confidence(
    report: &SwarmReplayReport,
    metrics: &SwarmReplayPolicyMetrics,
    missing_data: &[SwarmReplayPolicyMissingData],
) -> SwarmReplayPolicyConfidence {
    let diagnostic_penalty = u64::try_from(report.diagnostics.len().min(3)).unwrap_or(3) * 10;
    let missing_penalty = u64::try_from(missing_data.len()).unwrap_or(u64::MAX / 25) * 25;
    let risk_penalty = if metrics.local_fallback_risk == "high" {
        5
    } else {
        0
    };
    let score = 100_u64
        .saturating_sub(diagnostic_penalty)
        .saturating_sub(missing_penalty)
        .saturating_sub(risk_penalty);
    let level = if score >= 80 {
        "high"
    } else if score >= 50 {
        "medium"
    } else {
        "low"
    };

    let mut reasons = Vec::new();
    if missing_data.is_empty() {
        reasons.push("all_required_metrics_supported".to_string());
    } else {
        reasons.push("missing_data_suppressed_claims".to_string());
    }
    if !report.diagnostics.is_empty() {
        reasons.push("replay_diagnostics_present".to_string());
    }
    if metrics.local_fallback_risk == "high" {
        reasons.push("policy_has_high_local_fallback_risk".to_string());
    }

    SwarmReplayPolicyConfidence {
        level: level.to_string(),
        score,
        reasons,
    }
}

fn policy_comparison_rationale(
    metrics: &SwarmReplayPolicyMetrics,
    score: i64,
    missing_data: &[SwarmReplayPolicyMissingData],
) -> Vec<String> {
    let mut rationale = vec![format!(
        "Score {score} from {} throughput action(s), {} validation deferral(s), risk {}.",
        metrics.throughput_actions,
        metrics.validation_commands_deferred,
        metrics.local_fallback_risk
    )];
    if metrics.throughput_actions > 0 {
        rationale.push(format!(
            "Advances replay work through {} advisory work-start decision(s).",
            metrics.throughput_actions
        ));
    } else {
        rationale.push(
            "Uses conservative advisory posture with no live-mutation decisions.".to_string(),
        );
    }
    if metrics.validation_commands_deferred > 0 {
        rationale.push(format!(
            "Defers {} validation command(s) when build or RCH pressure is observed.",
            metrics.validation_commands_deferred
        ));
    }
    if metrics.reservation_conflicts_avoided > 0 {
        rationale.push(format!(
            "Avoids {} reservation conflict(s) through advisory backoff or review.",
            metrics.reservation_conflicts_avoided
        ));
    }
    if metrics.resource_budget.saturation_events > 0 {
        rationale.push(format!(
            "Observes {} resource saturation point(s); first saturation: {}.",
            metrics.resource_budget.saturation_events,
            metrics
                .resource_budget
                .first_saturation_point
                .as_deref()
                .unwrap_or("unknown")
        ));
    }
    if metrics.operator_handoff_quality != "not_applicable" {
        rationale.push(format!(
            "Operator handoff evidence is {}.",
            metrics.operator_handoff_quality
        ));
    }
    for missing in missing_data {
        rationale.push(format!(
            "Suppresses {} because {}.",
            missing.suppressed_metrics.join(","),
            missing.reasons.join("; ")
        ));
    }
    rationale
}

fn policy_resource_metrics(
    report: &SwarmReplayReport,
    decisions: &[&SwarmReplayPolicyDecision],
) -> SwarmReplayPolicyResourceMetrics {
    let profile_id = report
        .final_state
        .resource_budget
        .as_ref()
        .map(|profile| profile.profile_id.clone());
    let decision_rch_fanout = count_decisions_with_actions(
        decisions,
        &[
            "acquire_build_slot_for_validation",
            "split_validation",
            "claim_bead",
        ],
    );
    let peak_agent_concurrency = report
        .resource_pressure_timeline
        .iter()
        .map(|snapshot| snapshot.active_agents)
        .max()
        .unwrap_or(0);
    let peak_tool_concurrency = report
        .resource_pressure_timeline
        .iter()
        .map(|snapshot| {
            snapshot
                .active_build_slots
                .saturating_add(snapshot.active_rch_jobs)
        })
        .max()
        .unwrap_or(0)
        .saturating_add(decision_rch_fanout);
    let peak_rch_fanout = report
        .resource_pressure_timeline
        .iter()
        .map(|snapshot| snapshot.active_rch_jobs)
        .max()
        .unwrap_or(0)
        .saturating_add(decision_rch_fanout);
    let peak_rch_queue_depth = report
        .resource_pressure_timeline
        .iter()
        .map(|snapshot| snapshot.rch_queue_depth)
        .max()
        .unwrap_or(0);
    let peak_estimated_rss_gib = report
        .resource_pressure_timeline
        .iter()
        .filter_map(|snapshot| snapshot.estimated_rss_gib)
        .max();
    let saturation_events = report
        .resource_pressure_timeline
        .iter()
        .filter(|snapshot| !snapshot.saturation_reasons.is_empty())
        .count()
        .try_into()
        .unwrap_or(u64::MAX);
    let first_saturation_point = report
        .resource_pressure_timeline
        .iter()
        .find(|snapshot| !snapshot.saturation_reasons.is_empty())
        .map(|snapshot| {
            format!(
                "{}:{}",
                snapshot.logical_clock,
                snapshot.saturation_reasons.join("+")
            )
        });
    let peak = PeakResourcePressure::from_timeline(&report.resource_pressure_timeline);
    let numa_hint = report.final_state.resource_budget.as_ref().map_or_else(
        || "unknown".to_string(),
        |profile| profile.numa_hint.clone(),
    );

    SwarmReplayPolicyResourceMetrics {
        profile_id,
        saturation_events,
        first_saturation_point,
        peak_agent_concurrency,
        peak_tool_concurrency,
        peak_rch_fanout,
        peak_rch_queue_depth,
        peak_estimated_rss_gib,
        cpu_pressure: peak.cpu,
        memory_pressure: peak.memory,
        tmpdir_pressure: peak.tmpdir,
        target_dir_pressure: peak.target_dir,
        rch_worker_pressure: peak.rch_worker,
        extension_lane_pressure: peak.extension_lane,
        numa_hint,
    }
}

#[derive(Debug, Clone)]
struct PeakResourcePressure {
    cpu: String,
    memory: String,
    tmpdir: String,
    target_dir: String,
    rch_worker: String,
    extension_lane: String,
}

impl PeakResourcePressure {
    fn from_timeline(timeline: &[SwarmReplayResourcePressureSnapshot]) -> Self {
        Self {
            cpu: peak_pressure(
                timeline
                    .iter()
                    .map(|snapshot| snapshot.cpu_pressure.as_str()),
            ),
            memory: peak_pressure(
                timeline
                    .iter()
                    .map(|snapshot| snapshot.memory_pressure.as_str()),
            ),
            tmpdir: peak_pressure(
                timeline
                    .iter()
                    .map(|snapshot| snapshot.tmpdir_pressure.as_str()),
            ),
            target_dir: peak_pressure(
                timeline
                    .iter()
                    .map(|snapshot| snapshot.target_dir_pressure.as_str()),
            ),
            rch_worker: peak_pressure(
                timeline
                    .iter()
                    .map(|snapshot| snapshot.rch_worker_pressure.as_str()),
            ),
            extension_lane: peak_pressure(
                timeline
                    .iter()
                    .map(|snapshot| snapshot.extension_lane_pressure.as_str()),
            ),
        }
    }
}

fn peak_pressure<'a>(levels: impl Iterator<Item = &'a str>) -> String {
    levels
        .max_by_key(|level| pressure_rank(level))
        .unwrap_or("unknown")
        .to_string()
}

fn pressure_rank(level: &str) -> u8 {
    match level {
        "saturated" => 4,
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

fn resource_budget_missing_claims(report: &SwarmReplayReport) -> Vec<String> {
    let Some(profile) = &report.final_state.resource_budget else {
        return vec!["host resource profile missing".to_string()];
    };
    resource_budget_missing_claims_for_profile(profile)
}

fn resource_budget_missing_claims_for_profile(
    profile: &SwarmReplayResourceBudgetState,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if profile.cpu_cores.is_none() && profile.cgroup_cpu_quota.is_none() {
        reasons.push("cpu capacity missing".to_string());
    }
    if profile.memory_gib.is_none() && profile.cgroup_memory_gib.is_none() {
        reasons.push("memory capacity missing".to_string());
    }
    if profile.max_agent_concurrency.is_none() {
        reasons.push("agent concurrency budget missing".to_string());
    }
    if profile.max_tool_concurrency.is_none() {
        reasons.push("tool concurrency budget missing".to_string());
    }
    if profile.extension_hostcall_lanes.is_none() {
        reasons.push("extension hostcall lane budget missing".to_string());
    }
    if profile.rch_worker_slots.is_none() {
        reasons.push("RCH worker slot budget missing".to_string());
    }
    if profile.target_free_gib.is_none() {
        reasons.push("CARGO_TARGET_DIR free-space budget missing".to_string());
    }
    if profile.tmpdir_free_gib.is_none() {
        reasons.push("TMPDIR free-space budget missing".to_string());
    }
    reasons
}

fn count_completed_beads(state: &SwarmReplayState) -> u64 {
    state
        .beads
        .values()
        .filter(|bead| bead.status == "closed")
        .count()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn count_throughput_actions(decisions: &[&SwarmReplayPolicyDecision]) -> u64 {
    count_decisions_with_actions(
        decisions,
        &[
            "acquire_build_slot_for_validation",
            "claim_bead",
            "continue_current_work",
            "reclaim_stale_bead",
            "split_validation",
        ],
    )
}

fn count_decisions_with_actions(decisions: &[&SwarmReplayPolicyDecision], actions: &[&str]) -> u64 {
    decisions
        .iter()
        .filter(|decision| actions.iter().any(|action| *action == decision.action))
        .count()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn count_rch_slot_occupancy_events(report: &SwarmReplayReport) -> u64 {
    report
        .snapshots
        .iter()
        .filter(|snapshot| {
            active_build_slot(&snapshot.state).is_some()
                || pressured_rch_job(&snapshot.state).is_some()
        })
        .count()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn local_fallback_risk(
    report: &SwarmReplayReport,
    decisions: &[&SwarmReplayPolicyDecision],
) -> String {
    let has_live_mutation = decisions
        .iter()
        .any(|decision| decision.would_require_live_mutation);
    if !has_live_mutation {
        return "low".to_string();
    }
    if mail_is_unavailable(&report.final_state) {
        return "high".to_string();
    }
    if report
        .snapshots
        .iter()
        .any(|snapshot| pressured_rch_job(&snapshot.state).is_some())
    {
        return "medium".to_string();
    }
    "medium".to_string()
}

fn count_reservation_conflicts_avoided(
    report: &SwarmReplayReport,
    decisions: &[&SwarmReplayPolicyDecision],
) -> u64 {
    let safety_decisions = count_decisions_with_actions(
        decisions,
        &[
            "back_off_cargo",
            "handoff",
            "operator_review",
            "refresh_evidence",
            "wait",
            "wait_for_build_slot",
        ],
    );
    safety_decisions.min(report.final_state.coordination.reservation_conflict_count)
}

fn evidence_freshness(report: &SwarmReplayReport, latency: &PolicyLatencyMetrics) -> String {
    if report
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == "error")
    {
        return "degraded".to_string();
    }
    if !report.diagnostics.is_empty() || !latency.missing_reasons.is_empty() {
        return "partial".to_string();
    }
    "fresh".to_string()
}

fn operator_handoff_quality(state: &SwarmReplayState) -> String {
    if state.operator_handoffs.is_empty() {
        return "not_applicable".to_string();
    }
    let complete = state
        .operator_handoffs
        .values()
        .filter(|handoff| !handoff.next_actions.is_empty() && !handoff.evidence_paths.is_empty())
        .count();
    if complete == state.operator_handoffs.len() {
        "complete".to_string()
    } else if complete > 0 {
        "partial".to_string()
    } else {
        "missing".to_string()
    }
}

fn policy_action_waits_for_external_state(action: &str) -> bool {
    matches!(
        action,
        "back_off_cargo"
            | "handoff"
            | "operator_review"
            | "refresh_evidence"
            | "wait"
            | "wait_for_build_slot"
    )
}

fn conservative_manual_decisions(
    policy_id: &str,
    snapshot: &SwarmReplayStateSnapshot,
) -> Vec<SwarmReplayPolicyDecision> {
    let state = &snapshot.state;
    if worktree_is_dirty(state) {
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "wait",
                target_kind: "worktree",
                target_id: "current",
                reason_codes: &["dirty_worktree_requires_manual_review"],
                expected_risk: "medium",
                would_require_live_mutation: false,
                evidence_detail: "dirty worktree evidence requires operator review before new work",
            },
        )];
    }
    if mail_is_unavailable(state) {
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "handoff",
                target_kind: "coordination",
                target_id: "agent_mail",
                reason_codes: &["agent_mail_unavailable_requires_manual_coordination"],
                expected_risk: "high",
                would_require_live_mutation: false,
                evidence_detail: "Agent Mail evidence is unavailable; avoid autonomous claims",
            },
        )];
    }
    if let Some(slot) = active_build_slot(state) {
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "wait",
                target_kind: "build_slot",
                target_id: &slot.slot,
                reason_codes: &["active_build_slot_requires_operator_review"],
                expected_risk: "medium",
                would_require_live_mutation: false,
                evidence_detail: "active build slot is already held",
            },
        )];
    }
    if let Some(bead) = best_bead_with_status(state, &["open"]) {
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "operator_review",
                target_kind: "bead",
                target_id: &bead.bead_id,
                reason_codes: &["ready_bead_available_but_manual_policy_does_not_claim"],
                expected_risk: "low",
                would_require_live_mutation: false,
                evidence_detail: "ready bead exists; conservative policy leaves claim to operator",
            },
        )];
    }
    Vec::new()
}

fn existing_autopilot_decisions(
    policy_id: &str,
    snapshot: &SwarmReplayStateSnapshot,
) -> Vec<SwarmReplayPolicyDecision> {
    let state = &snapshot.state;
    if worktree_is_dirty(state) {
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "wait",
                target_kind: "worktree",
                target_id: "current",
                reason_codes: &["dirty_worktree_contention"],
                expected_risk: "medium",
                would_require_live_mutation: false,
                evidence_detail: "dirty worktree suggests another lane is active",
            },
        )];
    }
    if let Some(bead) = best_bead_with_status(state, &["open"]) {
        let reason = if mail_is_unavailable(state) {
            "agent_mail_unavailable_continue_via_beads"
        } else {
            "ready_bead_available"
        };
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "claim_bead",
                target_kind: "bead",
                target_id: &bead.bead_id,
                reason_codes: &[reason],
                expected_risk: "medium",
                would_require_live_mutation: true,
                evidence_detail: "autopilot baseline would claim the next ready bead",
            },
        )];
    }
    if let Some(bead) = best_bead_with_status(state, &["in_progress"]) {
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "continue_current_work",
                target_kind: "bead",
                target_id: &bead.bead_id,
                reason_codes: &["in_progress_work_observed"],
                expected_risk: "low",
                would_require_live_mutation: false,
                evidence_detail: "work is already in progress in the replay state",
            },
        )];
    }
    Vec::new()
}

fn rch_fanout_limited_decisions(
    policy_id: &str,
    snapshot: &SwarmReplayStateSnapshot,
) -> Vec<SwarmReplayPolicyDecision> {
    let state = &snapshot.state;
    if let Some(job) = pressured_rch_job(state) {
        let reason = if job.stale_progress {
            "rch_progress_stale"
        } else {
            "rch_queue_position_positive"
        };
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "back_off_cargo",
                target_kind: "rch_job",
                target_id: &job.job_id,
                reason_codes: &[reason],
                expected_risk: "high",
                would_require_live_mutation: false,
                evidence_detail: "RCH pressure means this policy would avoid starting more cargo work",
            },
        )];
    }
    if worktree_is_dirty(state) {
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "wait",
                target_kind: "worktree",
                target_id: "current",
                reason_codes: &["dirty_worktree_avoid_validation_fanout"],
                expected_risk: "medium",
                would_require_live_mutation: false,
                evidence_detail: "dirty worktree blocks parallel validation fanout",
            },
        )];
    }
    if let Some(bead) = best_bead_with_status(state, &["open", "in_progress"]) {
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "split_validation",
                target_kind: "bead",
                target_id: &bead.bead_id,
                reason_codes: &["rch_capacity_available_for_bounded_validation"],
                expected_risk: "medium",
                would_require_live_mutation: true,
                evidence_detail: "policy would split validation into bounded RCH-backed slices",
            },
        )];
    }
    Vec::new()
}

fn stale_bead_reclaiming_decisions(
    policy_id: &str,
    snapshot: &SwarmReplayStateSnapshot,
    context: &SwarmReplayPolicyContext<'_>,
) -> Vec<SwarmReplayPolicyDecision> {
    let state = &snapshot.state;
    if let Some(bead) = stale_in_progress_bead(state, context.final_state) {
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "reclaim_stale_bead",
                target_kind: "bead",
                target_id: &bead.bead_id,
                reason_codes: &["in_progress_assignee_absent_from_replay_agents"],
                expected_risk: "high",
                would_require_live_mutation: true,
                evidence_detail: "assigned in-progress bead has no matching active agent evidence in the trace",
            },
        )];
    }
    if mail_is_unavailable(state) {
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "refresh_evidence",
                target_kind: "coordination",
                target_id: "agent_mail",
                reason_codes: &["agent_mail_missing_prevents_confident_reclaim"],
                expected_risk: "medium",
                would_require_live_mutation: false,
                evidence_detail: "missing Agent Mail evidence prevents confident stale-work reclaim",
            },
        )];
    }
    if let Some(bead) = best_bead_with_status(state, &["open"]) {
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "claim_bead",
                target_kind: "bead",
                target_id: &bead.bead_id,
                reason_codes: &["ready_bead_available_after_stale_scan"],
                expected_risk: "medium",
                would_require_live_mutation: true,
                evidence_detail: "no stale in-progress bead was found, so policy would claim ready work",
            },
        )];
    }
    Vec::new()
}

fn build_slot_protective_decisions(
    policy_id: &str,
    snapshot: &SwarmReplayStateSnapshot,
) -> Vec<SwarmReplayPolicyDecision> {
    let state = &snapshot.state;
    if let Some(slot) = active_build_slot(state) {
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "wait_for_build_slot",
                target_kind: "build_slot",
                target_id: &slot.slot,
                reason_codes: &["active_build_slot_protected"],
                expected_risk: "medium",
                would_require_live_mutation: false,
                evidence_detail: "policy protects an already-held build slot from additional fanout",
            },
        )];
    }
    if let Some(job) = pressured_rch_job(state) {
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "back_off_cargo",
                target_kind: "rch_job",
                target_id: &job.job_id,
                reason_codes: &["rch_pressure_protects_build_capacity"],
                expected_risk: "high",
                would_require_live_mutation: false,
                evidence_detail: "RCH pressure means build capacity should be protected",
            },
        )];
    }
    if worktree_is_dirty(state) {
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "wait",
                target_kind: "worktree",
                target_id: "current",
                reason_codes: &["dirty_worktree_protect_existing_lane"],
                expected_risk: "medium",
                would_require_live_mutation: false,
                evidence_detail: "dirty worktree indicates an existing lane should finish first",
            },
        )];
    }
    if let Some(bead) = best_bead_with_status(state, &["open", "in_progress"]) {
        return vec![policy_decision(
            policy_id,
            snapshot,
            PolicyDecisionSpec {
                action: "acquire_build_slot_for_validation",
                target_kind: "bead",
                target_id: &bead.bead_id,
                reason_codes: &["build_slot_available_for_bounded_validation"],
                expected_risk: "medium",
                would_require_live_mutation: true,
                evidence_detail: "policy would acquire a build slot before validation work",
            },
        )];
    }
    Vec::new()
}

#[derive(Clone, Copy)]
struct PolicyDecisionSpec<'a> {
    action: &'a str,
    target_kind: &'a str,
    target_id: &'a str,
    reason_codes: &'a [&'a str],
    expected_risk: &'a str,
    would_require_live_mutation: bool,
    evidence_detail: &'a str,
}

fn policy_decision(
    policy_id: &str,
    snapshot: &SwarmReplayStateSnapshot,
    spec: PolicyDecisionSpec<'_>,
) -> SwarmReplayPolicyDecision {
    SwarmReplayPolicyDecision {
        policy_id: policy_id.to_string(),
        logical_clock: snapshot.logical_clock,
        event_id: snapshot.event_id.clone(),
        action: spec.action.to_string(),
        target_kind: spec.target_kind.to_string(),
        target_id: spec.target_id.to_string(),
        reason_codes: spec.reason_codes.iter().map(ToString::to_string).collect(),
        source_evidence: vec![SwarmReplayPolicyEvidenceRef {
            evidence_kind: "snapshot".to_string(),
            evidence_id: spec.target_id.to_string(),
            event_id: snapshot.event_id.clone(),
            logical_clock: snapshot.logical_clock,
            detail: spec.evidence_detail.to_string(),
        }],
        expected_risk: spec.expected_risk.to_string(),
        would_require_live_mutation: spec.would_require_live_mutation,
        advisory_only: true,
    }
}

fn worktree_is_dirty(state: &SwarmReplayState) -> bool {
    state
        .worktree
        .as_ref()
        .is_some_and(|worktree| worktree.dirty)
}

const fn mail_is_unavailable(state: &SwarmReplayState) -> bool {
    !state.coordination.agent_mail_available || state.coordination.missing_agent_mail_evidence
}

fn active_build_slot(state: &SwarmReplayState) -> Option<&SwarmReplayBuildSlotState> {
    state.build_slots.values().find(|slot| {
        matches!(
            slot.state.as_str(),
            "active" | "acquired" | "held" | "running"
        )
    })
}

fn pressured_rch_job(state: &SwarmReplayState) -> Option<&SwarmReplayRchJobState> {
    state
        .rch_jobs
        .values()
        .find(|job| job.queue_position > 0 || job.stale_progress)
}

fn best_bead_with_status<'a>(
    state: &'a SwarmReplayState,
    statuses: &[&str],
) -> Option<&'a SwarmReplayBeadState> {
    state
        .beads
        .values()
        .filter(|bead| statuses.iter().any(|status| *status == bead.status))
        .min_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.bead_id.cmp(&right.bead_id))
        })
}

fn stale_in_progress_bead<'a>(
    state: &'a SwarmReplayState,
    final_state: &SwarmReplayState,
) -> Option<&'a SwarmReplayBeadState> {
    state
        .beads
        .values()
        .filter(|bead| bead.status == "in_progress")
        .filter(|bead| {
            let assignee = bead.assignee.trim();
            !assignee.is_empty()
                && assignee != "unassigned"
                && !state.agents.contains_key(assignee)
                && !final_state.agents.contains_key(assignee)
        })
        .min_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.bead_id.cmp(&right.bead_id))
        })
}

fn build_resource_pressure_timeline(
    snapshots: &[SwarmReplayStateSnapshot],
) -> Vec<SwarmReplayResourcePressureSnapshot> {
    snapshots.iter().map(resource_pressure_snapshot).collect()
}

#[derive(Debug, Clone, Copy)]
struct ResourceDemandSnapshot {
    active_agents: u64,
    active_build_slots: u64,
    active_rch_jobs: u64,
    rch_queue_depth: u64,
}

impl ResourceDemandSnapshot {
    fn from_state(state: &SwarmReplayState) -> Self {
        let active_agents = u64::try_from(state.agents.len()).unwrap_or(u64::MAX);
        let active_build_slots = state
            .build_slots
            .values()
            .filter(|slot| {
                matches!(
                    slot.state.as_str(),
                    "active" | "acquired" | "held" | "running"
                )
            })
            .count()
            .try_into()
            .unwrap_or(u64::MAX);
        let active_rch_jobs = state
            .rch_jobs
            .values()
            .filter(|job| {
                job.queue_position > 0
                    || matches!(
                        job.state.as_str(),
                        "queued" | "running" | "syncing" | "checking" | "testing" | "building"
                    )
            })
            .count()
            .try_into()
            .unwrap_or(u64::MAX);
        let rch_queue_depth = state
            .rch_jobs
            .values()
            .filter_map(|job| u64::try_from(job.queue_position.max(0)).ok())
            .sum::<u64>();

        Self {
            active_agents,
            active_build_slots,
            active_rch_jobs,
            rch_queue_depth,
        }
    }

    const fn estimated_rss_gib(self) -> u64 {
        self.active_agents
            .saturating_mul(2)
            .saturating_add(self.active_build_slots.saturating_mul(4))
            .saturating_add(self.active_rch_jobs.saturating_mul(4))
    }

    const fn cpu_work_units(self) -> u64 {
        self.active_agents
            .saturating_add(self.active_build_slots.saturating_mul(4))
            .saturating_add(self.active_rch_jobs.saturating_mul(4))
    }
}

fn resource_pressure_snapshot(
    snapshot: &SwarmReplayStateSnapshot,
) -> SwarmReplayResourcePressureSnapshot {
    let state = &snapshot.state;
    let demand = ResourceDemandSnapshot::from_state(state);
    let estimated_rss_gib = state
        .resource_budget
        .as_ref()
        .map(|_| demand.estimated_rss_gib());

    let Some(profile) = &state.resource_budget else {
        return SwarmReplayResourcePressureSnapshot {
            logical_clock: snapshot.logical_clock,
            event_id: snapshot.event_id.clone(),
            profile_id: None,
            active_agents: demand.active_agents,
            active_build_slots: demand.active_build_slots,
            active_rch_jobs: demand.active_rch_jobs,
            rch_queue_depth: demand.rch_queue_depth,
            estimated_rss_gib,
            cpu_pressure: "unknown".to_string(),
            memory_pressure: "unknown".to_string(),
            tmpdir_pressure: "unknown".to_string(),
            target_dir_pressure: "unknown".to_string(),
            rch_worker_pressure: "unknown".to_string(),
            extension_lane_pressure: "unknown".to_string(),
            saturation_reasons: Vec::new(),
            missing_data: vec!["host resource profile missing".to_string()],
        };
    };

    let cpu_capacity = profile.cgroup_cpu_quota.or(profile.cpu_cores);
    let memory_capacity = profile.cgroup_memory_gib.or(profile.memory_gib);
    let cpu_pressure = pressure_for_capacity(demand.cpu_work_units(), cpu_capacity);
    let memory_pressure = pressure_for_capacity(estimated_rss_gib.unwrap_or(0), memory_capacity);
    let tmpdir_pressure = pressure_for_free_gib(profile.tmpdir_free_gib);
    let target_dir_pressure = pressure_for_free_gib(profile.target_free_gib);
    let rch_worker_pressure = pressure_for_capacity(
        demand
            .active_rch_jobs
            .saturating_add(demand.rch_queue_depth),
        profile.rch_worker_slots,
    );
    let extension_lane_pressure =
        pressure_for_capacity(demand.active_agents, profile.extension_hostcall_lanes);

    let mut saturation_reasons = Vec::new();
    push_if_saturated(&mut saturation_reasons, "cpu_saturated", &cpu_pressure);
    push_if_saturated(
        &mut saturation_reasons,
        "memory_saturated",
        &memory_pressure,
    );
    push_if_saturated(
        &mut saturation_reasons,
        "tmpdir_saturated",
        &tmpdir_pressure,
    );
    push_if_saturated(
        &mut saturation_reasons,
        "target_dir_saturated",
        &target_dir_pressure,
    );
    push_if_saturated(
        &mut saturation_reasons,
        "rch_workers_saturated",
        &rch_worker_pressure,
    );
    push_if_saturated(
        &mut saturation_reasons,
        "extension_lanes_saturated",
        &extension_lane_pressure,
    );

    SwarmReplayResourcePressureSnapshot {
        logical_clock: snapshot.logical_clock,
        event_id: snapshot.event_id.clone(),
        profile_id: Some(profile.profile_id.clone()),
        active_agents: demand.active_agents,
        active_build_slots: demand.active_build_slots,
        active_rch_jobs: demand.active_rch_jobs,
        rch_queue_depth: demand.rch_queue_depth,
        estimated_rss_gib,
        cpu_pressure,
        memory_pressure,
        tmpdir_pressure,
        target_dir_pressure,
        rch_worker_pressure,
        extension_lane_pressure,
        saturation_reasons,
        missing_data: resource_budget_missing_claims_for_profile(profile),
    }
}

fn pressure_for_capacity(used: u64, capacity: Option<u64>) -> String {
    let Some(capacity) = capacity else {
        return "unknown".to_string();
    };
    if capacity == 0 || used > capacity {
        return "saturated".to_string();
    }
    let usage = used.saturating_mul(100) / capacity;
    if usage >= 80 {
        "high".to_string()
    } else if usage >= 60 {
        "medium".to_string()
    } else {
        "low".to_string()
    }
}

fn pressure_for_free_gib(free_gib: Option<u64>) -> String {
    let Some(free_gib) = free_gib else {
        return "unknown".to_string();
    };
    if free_gib < 16 {
        "saturated".to_string()
    } else if free_gib < 32 {
        "high".to_string()
    } else if free_gib < 64 {
        "medium".to_string()
    } else {
        "low".to_string()
    }
}

fn push_if_saturated(reasons: &mut Vec<String>, reason: &str, pressure: &str) {
    if pressure == "saturated" {
        reasons.push(reason.to_string());
    }
}

/// Replay a normalized trace into deterministic state snapshots and diagnostics.
pub fn replay_swarm_trace(trace: &SwarmReplayTrace) -> Result<SwarmReplayReport> {
    validate_trace_for_replay(trace)?;

    let mut state = SwarmReplayState::default();
    let mut diagnostics = Vec::new();
    let mut snapshots = Vec::new();
    let mut seen_event_ids = BTreeSet::new();
    let mut last_timestamp: Option<String> = None;
    let mut logical_clock = 0_u64;

    for event in ordered_trace_events(trace) {
        if !seen_event_ids.insert(event.event_id.clone()) {
            diagnostics.push(replay_diagnostic(
                "duplicate_event_id_skipped",
                "warning",
                Some(event),
                Some(logical_clock),
                "duplicate replay event id skipped to preserve deterministic state",
                json!({ "event_id": event.event_id }),
            ));
            continue;
        }

        logical_clock = logical_clock.saturating_add(1);
        if let Some(previous) = &last_timestamp
            && timestamp_is_before(&event.occurred_at_utc, previous)
        {
            diagnostics.push(replay_diagnostic(
                "event_timestamp_regressed",
                "warning",
                Some(event),
                Some(logical_clock),
                "event timestamp is earlier than a previously replayed event; logical clock order preserved",
                json!({
                    "previous_occurred_at_utc": previous,
                    "event_occurred_at_utc": event.occurred_at_utc
                }),
            ));
        }
        if last_timestamp
            .as_ref()
            .is_none_or(|previous| timestamp_is_before(previous, &event.occurred_at_utc))
        {
            last_timestamp = Some(event.occurred_at_utc.clone());
        }

        observe_actor(event, &mut state);
        apply_replay_event(event, logical_clock, &mut state, &mut diagnostics);
        snapshots.push(SwarmReplayStateSnapshot {
            logical_clock,
            event_id: event.event_id.clone(),
            occurred_at_utc: event.occurred_at_utc.clone(),
            state: state.clone(),
            diagnostic_count: u64::try_from(diagnostics.len()).unwrap_or(u64::MAX),
        });
    }

    emit_end_of_trace_invariants(&state, &mut diagnostics);
    let resource_pressure_timeline = build_resource_pressure_timeline(&snapshots);

    Ok(SwarmReplayReport {
        schema: SWARM_REPLAY_REPORT_SCHEMA.to_string(),
        trace_id: trace.trace_id.clone(),
        replayed_event_count: logical_clock,
        final_logical_clock: logical_clock,
        snapshots,
        resource_pressure_timeline,
        final_state: state,
        diagnostics,
        replay_guards: SwarmReplayEngineGuards {
            read_only: true,
            no_live_mutation: true,
            no_network_required: true,
            consumed_trace_only: true,
        },
    })
}

fn validate_trace_for_replay(trace: &SwarmReplayTrace) -> Result<()> {
    if trace.schema != SWARM_REPLAY_TRACE_SCHEMA {
        return Err(Error::validation(format!(
            "unsupported swarm replay trace schema {}",
            trace.schema
        )));
    }
    if !trace.replay_guards.read_only || !trace.replay_guards.no_live_mutation {
        return Err(Error::validation(
            "swarm replay trace guards must prove read-only no-mutation evidence",
        ));
    }
    Ok(())
}

fn ordered_trace_events(trace: &SwarmReplayTrace) -> Vec<&SwarmReplayEvent> {
    let mut events = trace.events.iter().collect::<Vec<_>>();
    events.sort_by(|left, right| {
        left.sequence
            .cmp(&right.sequence)
            .then_with(|| left.source_ref.cmp(&right.source_ref))
            .then_with(|| left.event_id.cmp(&right.event_id))
    });
    events
}

fn apply_replay_event(
    event: &SwarmReplayEvent,
    logical_clock: u64,
    state: &mut SwarmReplayState,
    diagnostics: &mut Vec<SwarmReplayDiagnostic>,
) {
    match event.event_type.as_str() {
        "bead_lifecycle" => apply_bead_event(event, logical_clock, state, diagnostics),
        "reservation_intent" => apply_reservation_event(event, logical_clock, state, diagnostics),
        "reservation_conflict" => apply_reservation_conflict(event, state),
        "agent_message" => apply_agent_message_event(event, logical_clock, state, diagnostics),
        "build_slot_state" => apply_build_slot_event(event, state),
        "rch_job_state" => apply_rch_event(event, logical_clock, state, diagnostics),
        "cargo_gate_result" => apply_cargo_gate_event(event, logical_clock, state, diagnostics),
        "runpack_recommendation" => apply_runpack_recommendation(event, state),
        "operator_handoff" => apply_operator_handoff(event, state),
        "worktree_state" => apply_worktree_event(event, state),
        "host_resource_profile" => apply_resource_profile_event(event, state),
        "doctor_finding" | "validation_artifact" => {}
        _ => diagnostics.push(replay_diagnostic(
            "unknown_event_type_ignored",
            "info",
            Some(event),
            Some(logical_clock),
            "unknown replay event type ignored without mutation",
            json!({ "event_type": event.event_type }),
        )),
    }
}

fn observe_actor(event: &SwarmReplayEvent, state: &mut SwarmReplayState) {
    if event.actor.trim().is_empty() || event.actor == "unknown" {
        return;
    }
    state.agents.insert(
        event.actor.clone(),
        SwarmReplayAgentState {
            agent_name: event.actor.clone(),
            last_event_id: event.event_id.clone(),
            last_seen_at_utc: event.occurred_at_utc.clone(),
        },
    );
}

fn apply_bead_event(
    event: &SwarmReplayEvent,
    logical_clock: u64,
    state: &mut SwarmReplayState,
    diagnostics: &mut Vec<SwarmReplayDiagnostic>,
) {
    let bead_id = payload_string(&event.payload, &["bead_id"], "unknown");
    let to_status = payload_string(&event.payload, &["to_status", "status"], "unknown");
    if let Some(existing) = state.beads.get(&bead_id)
        && existing.status == "closed"
        && matches!(to_status.as_str(), "open" | "in_progress")
        && !event_explicitly_reopens(event)
    {
        diagnostics.push(replay_diagnostic(
            "closed_bead_reopened_without_explicit_reopen",
            "error",
            Some(event),
            Some(logical_clock),
            "closed bead transitioned back to open state without explicit reopen evidence",
            json!({
                "bead_id": bead_id,
                "previous_status": existing.status,
                "to_status": to_status
            }),
        ));
    }

    state.beads.insert(
        bead_id.clone(),
        SwarmReplayBeadState {
            bead_id,
            status: to_status,
            priority: payload_i64(&event.payload, "priority", 0),
            assignee: payload_string(&event.payload, &["assignee"], "unassigned"),
            last_event_id: event.event_id.clone(),
        },
    );
}

fn event_explicitly_reopens(event: &SwarmReplayEvent) -> bool {
    event
        .payload
        .get("reopen")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || event
            .payload
            .get("reopened")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || ["action", "reason", "close_reason"]
            .iter()
            .filter_map(|key| event.payload.get(*key).and_then(Value::as_str))
            .any(|value| value.to_ascii_lowercase().contains("reopen"))
}

fn apply_reservation_event(
    event: &SwarmReplayEvent,
    logical_clock: u64,
    state: &mut SwarmReplayState,
    diagnostics: &mut Vec<SwarmReplayDiagnostic>,
) {
    let reservation_id = payload_string(&event.payload, &["reservation_id"], "unknown");
    let reservation_state = payload_string(&event.payload, &["state"], "active");
    let release_state = matches!(
        reservation_state.as_str(),
        "released" | "expired" | "cancelled" | "canceled"
    );
    if release_state && !state.reservations.contains_key(&reservation_id) {
        diagnostics.push(replay_diagnostic(
            "impossible_reservation_release",
            "error",
            Some(event),
            Some(logical_clock),
            "reservation release observed before an active reservation intent",
            json!({
                "reservation_id": reservation_id,
                "state": reservation_state
            }),
        ));
    }

    state.reservations.insert(
        reservation_id.clone(),
        SwarmReplayReservationState {
            reservation_id,
            holder: payload_string(&event.payload, &["holder", "agent"], event.actor.as_str()),
            path_patterns: payload_string_array(&event.payload, "path_patterns"),
            exclusive: event
                .payload
                .get("exclusive")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            active: !release_state,
            state: reservation_state,
            last_event_id: event.event_id.clone(),
        },
    );
}

fn apply_reservation_conflict(event: &SwarmReplayEvent, state: &mut SwarmReplayState) {
    state.coordination.reservation_conflict_count = state
        .coordination
        .reservation_conflict_count
        .saturating_add(1);
    state.coordination.last_operator_action = Some(payload_string(
        &event.payload,
        &["conflict_reason"],
        "reservation_conflict",
    ));
}

fn apply_agent_message_event(
    event: &SwarmReplayEvent,
    logical_clock: u64,
    state: &mut SwarmReplayState,
    diagnostics: &mut Vec<SwarmReplayDiagnostic>,
) {
    let missing_mail = event.source_ref == "agent_mail_archive"
        && (event.uncertainty.state == "missing_source"
            || event
                .uncertainty
                .reasons
                .iter()
                .any(|reason| reason == "source_missing"));
    if missing_mail {
        state.coordination.agent_mail_available = false;
        state.coordination.missing_agent_mail_evidence = true;
        diagnostics.push(replay_diagnostic(
            "agent_mail_source_unavailable",
            "warning",
            Some(event),
            Some(logical_clock),
            "Agent Mail source unavailable; coordination facts remain suppressed",
            json!({ "suppressed_claims": event.uncertainty.suppressed_claims }),
        ));
    }
}

fn apply_build_slot_event(event: &SwarmReplayEvent, state: &mut SwarmReplayState) {
    let slot = payload_string(&event.payload, &["slot"], "unknown");
    state.build_slots.insert(
        slot.clone(),
        SwarmReplayBuildSlotState {
            slot,
            holder: payload_string(&event.payload, &["holder"], "unknown"),
            state: payload_string(&event.payload, &["state"], "unknown"),
            expires_at_utc: payload_string(&event.payload, &["expires_at_utc"], "unknown"),
            last_event_id: event.event_id.clone(),
        },
    );
}

fn apply_rch_event(
    event: &SwarmReplayEvent,
    logical_clock: u64,
    state: &mut SwarmReplayState,
    diagnostics: &mut Vec<SwarmReplayDiagnostic>,
) {
    let job_id = payload_string(&event.payload, &["job_id"], "unknown");
    let queue_position = payload_i64(&event.payload, "queue_position", 0);
    if queue_position < 0 {
        diagnostics.push(replay_diagnostic(
            "negative_rch_queue_position",
            "error",
            Some(event),
            Some(logical_clock),
            "RCH queue position cannot be negative",
            json!({ "job_id": job_id, "queue_position": queue_position }),
        ));
    }
    let stale_progress = event.uncertainty.state != "certain"
        || event
            .uncertainty
            .reasons
            .iter()
            .any(|reason| reason == "source_stale" || reason == "source_declared_stale");
    if stale_progress {
        diagnostics.push(replay_diagnostic(
            "rch_progress_from_uncertain_source",
            "warning",
            Some(event),
            Some(logical_clock),
            "RCH job progress came from stale or uncertain evidence",
            json!({ "job_id": job_id, "uncertainty": event.uncertainty }),
        ));
    }

    state.rch_jobs.insert(
        job_id.clone(),
        SwarmReplayRchJobState {
            job_id,
            state: payload_string(&event.payload, &["state"], "unknown"),
            worker: payload_string(&event.payload, &["worker"], "unknown"),
            command: payload_string(&event.payload, &["command"], "unknown"),
            queue_position,
            stale_progress,
            last_event_id: event.event_id.clone(),
        },
    );
}

fn apply_cargo_gate_event(
    event: &SwarmReplayEvent,
    logical_clock: u64,
    state: &mut SwarmReplayState,
    diagnostics: &mut Vec<SwarmReplayDiagnostic>,
) {
    let command = payload_string(&event.payload, &["command"], "unknown");
    let exit_code = payload_i64(&event.payload, "exit_code", 0);
    if exit_code == 0 && (command.trim().is_empty() || command == "unknown") {
        diagnostics.push(replay_diagnostic(
            "successful_cargo_gate_missing_command_evidence",
            "error",
            Some(event),
            Some(logical_clock),
            "successful cargo gate requires concrete command evidence",
            json!({ "event_id": event.event_id }),
        ));
    }
    let gate_id = stable_id(&format!("cargo-gate-{command}"));
    state.validation_gates.insert(
        gate_id.clone(),
        SwarmReplayValidationGateState {
            gate_id,
            command,
            runner: payload_string(&event.payload, &["runner"], "unknown"),
            exit_code,
            target_dir: payload_string(&event.payload, &["target_dir"], "unknown"),
            tmpdir: payload_string(&event.payload, &["tmpdir"], "unknown"),
            last_event_id: event.event_id.clone(),
        },
    );
}

fn apply_runpack_recommendation(event: &SwarmReplayEvent, state: &mut SwarmReplayState) {
    let action = payload_string(&event.payload, &["action"], "unknown");
    state.coordination.last_operator_action = Some(action.clone());
    state.runpack_recommendations.insert(
        action.clone(),
        SwarmReplayRunpackRecommendationState {
            action,
            severity: payload_string(&event.payload, &["severity"], "info"),
            evidence_paths: payload_string_array(&event.payload, "evidence_paths"),
            last_event_id: event.event_id.clone(),
        },
    );
}

fn apply_operator_handoff(event: &SwarmReplayEvent, state: &mut SwarmReplayState) {
    let handoff_id = payload_string(&event.payload, &["handoff_id"], "unknown");
    state.operator_handoffs.insert(
        handoff_id.clone(),
        SwarmReplayOperatorHandoffState {
            handoff_id,
            summary: payload_string(&event.payload, &["summary"], ""),
            next_actions: payload_string_array(&event.payload, "next_actions"),
            evidence_paths: payload_string_array(&event.payload, "evidence_paths"),
            last_event_id: event.event_id.clone(),
        },
    );
}

fn apply_worktree_event(event: &SwarmReplayEvent, state: &mut SwarmReplayState) {
    state.worktree = Some(SwarmReplayWorktreeState {
        head: payload_string(&event.payload, &["head"], "unknown"),
        branch: payload_string(&event.payload, &["branch"], "unknown"),
        dirty: event
            .payload
            .get("dirty")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        changed_paths: payload_string_array(&event.payload, "changed_paths"),
        last_event_id: event.event_id.clone(),
    });
}

fn apply_resource_profile_event(event: &SwarmReplayEvent, state: &mut SwarmReplayState) {
    state.resource_budget = Some(SwarmReplayResourceBudgetState {
        profile_id: payload_string(&event.payload, &["profile_id", "id"], "unknown"),
        cpu_cores: payload_u64_optional(&event.payload, "cpu_cores"),
        memory_gib: payload_u64_optional(&event.payload, "memory_gib"),
        numa_nodes: payload_u64_optional(&event.payload, "numa_nodes"),
        cgroup_cpu_quota: payload_u64_optional(&event.payload, "cgroup_cpu_quota"),
        cgroup_memory_gib: payload_u64_optional(&event.payload, "cgroup_memory_gib"),
        max_agent_concurrency: payload_u64_optional(&event.payload, "max_agent_concurrency"),
        max_tool_concurrency: payload_u64_optional(&event.payload, "max_tool_concurrency"),
        extension_hostcall_lanes: payload_u64_optional(&event.payload, "extension_hostcall_lanes"),
        rch_worker_slots: payload_u64_optional(&event.payload, "rch_worker_slots"),
        target_dir: payload_string(
            &event.payload,
            &["target_dir", "cargo_target_dir"],
            "unknown",
        ),
        target_free_gib: payload_u64_optional(&event.payload, "target_free_gib"),
        tmpdir: payload_string(&event.payload, &["tmpdir"], "unknown"),
        tmpdir_free_gib: payload_u64_optional(&event.payload, "tmpdir_free_gib"),
        numa_hint: payload_string(&event.payload, &["numa_hint"], "unknown"),
        last_event_id: event.event_id.clone(),
    });
}

fn emit_end_of_trace_invariants(
    state: &SwarmReplayState,
    diagnostics: &mut Vec<SwarmReplayDiagnostic>,
) {
    for reservation in state
        .reservations
        .values()
        .filter(|reservation| reservation.active)
    {
        diagnostics.push(SwarmReplayDiagnostic {
            code: "reservation_missing_release_event".to_string(),
            severity: "warning".to_string(),
            event_id: Some(reservation.last_event_id.clone()),
            logical_clock: None,
            message: "reservation remained active at end of replay without release evidence"
                .to_string(),
            details: json!({
                "reservation_id": reservation.reservation_id,
                "holder": reservation.holder,
                "path_patterns": reservation.path_patterns
            }),
        });
    }
}

fn replay_diagnostic(
    code: &str,
    severity: &str,
    event: Option<&SwarmReplayEvent>,
    logical_clock: Option<u64>,
    message: &str,
    details: Value,
) -> SwarmReplayDiagnostic {
    SwarmReplayDiagnostic {
        code: code.to_string(),
        severity: severity.to_string(),
        event_id: event.map(|item| item.event_id.clone()),
        logical_clock,
        message: message.to_string(),
        details,
    }
}

fn timestamp_is_before(left: &str, right: &str) -> bool {
    match (
        DateTime::parse_from_rfc3339(left),
        DateTime::parse_from_rfc3339(right),
    ) {
        (Ok(left), Ok(right)) => left < right,
        _ => left < right,
    }
}

fn payload_string(value: &Value, keys: &[&str], fallback: &str) -> String {
    optional_string_field(value, keys).unwrap_or_else(|| fallback.to_string())
}

fn payload_string_array(value: &Value, key: &str) -> Vec<String> {
    string_array_field(value, key)
}

fn payload_i64(value: &Value, key: &str, fallback: i64) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(fallback)
}

fn payload_u64_optional(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

#[derive(Debug, Clone)]
struct SourceAnalysis {
    row: SwarmReplaySourceInventoryRow,
    parsed: Option<ParsedSource>,
}

#[derive(Debug, Clone)]
enum ParsedSource {
    Json(Value),
    JsonLines(Vec<Value>),
}

#[derive(Debug, Clone)]
struct PendingEvent {
    candidate_id: String,
    occurred_at_utc: String,
    observed_at_utc: String,
    event_type: String,
    actor: String,
    source_ref: String,
    source_hash: Option<String>,
    redaction_state: String,
    uncertainty: SwarmReplayEventUncertainty,
    payload: Value,
}

#[derive(Debug)]
struct PendingEventSeed {
    event_type: &'static str,
    candidate_id: String,
    actor: String,
    occurred_at_utc: String,
    payload: Value,
}

#[derive(Debug, Default)]
struct RedactionAccumulator {
    redacted_count: u64,
    sensitive_omitted_count: u64,
    redacted_fields: BTreeSet<String>,
}

/// Build a normalized replay trace from source artifacts.
#[allow(clippy::too_many_lines)]
pub fn build_swarm_replay_trace(request: &SwarmReplayIngestRequest) -> Result<SwarmReplayTrace> {
    validate_request(request)?;

    let mut source_inventory = Vec::new();
    let mut events = Vec::new();
    let mut redaction = RedactionAccumulator::default();
    let mut missing_sources = BTreeSet::new();
    let mut malformed_sources = BTreeSet::new();
    let mut stale_sources = BTreeSet::new();
    let mut suppressed_claims = BTreeSet::new();

    for template in SOURCE_TEMPLATES {
        let analysis = analyze_source(request, template);
        match analysis.row.availability.as_str() {
            "unavailable" => {
                missing_sources.insert(analysis.row.source_id.clone());
                suppressed_claims.extend(
                    suppressed_claims_for_source(&analysis.row.source_id)
                        .iter()
                        .map(ToString::to_string),
                );
            }
            "malformed" => {
                malformed_sources.insert(analysis.row.source_id.clone());
                suppressed_claims.extend(
                    suppressed_claims_for_source(&analysis.row.source_id)
                        .iter()
                        .map(ToString::to_string),
                );
            }
            "stale" => {
                stale_sources.insert(analysis.row.source_id.clone());
                suppressed_claims.extend(
                    suppressed_claims_for_source(&analysis.row.source_id)
                        .iter()
                        .map(ToString::to_string),
                );
            }
            _ => {}
        }

        if let Some(parsed) = &analysis.parsed {
            events.extend(events_from_source(
                request,
                template,
                &analysis.row,
                parsed,
                &mut redaction,
            ));
        } else if analysis.row.availability == "unavailable" {
            events.extend(missing_source_events(
                template,
                &analysis.row,
                request.generated_at_utc.as_str(),
            ));
        }

        source_inventory.push(analysis.row);
    }

    let events = finalize_events(events);
    let mut event_count_by_uncertainty = BTreeMap::new();
    for event in &events {
        *event_count_by_uncertainty
            .entry(event.uncertainty.state.clone())
            .or_insert(0) += 1;
    }

    Ok(SwarmReplayTrace {
        schema: SWARM_REPLAY_TRACE_SCHEMA.to_string(),
        trace_id: request.trace_id.clone(),
        generated_at: request.generated_at_utc.clone(),
        contract_version: SWARM_REPLAY_TRACE_CONTRACT_VERSION.to_string(),
        source_inventory,
        ordering: SwarmReplayOrdering {
            monotonic_sequence_required: true,
            timestamp_normalization: "utc_rfc3339_z".to_string(),
            tie_breakers: vec![
                "sequence".to_string(),
                "source_ref".to_string(),
                "event_id".to_string(),
            ],
        },
        events,
        redaction_summary: SwarmReplayRedactionSummary {
            redacted_count: redaction.redacted_count,
            sensitive_omitted_count: redaction.sensitive_omitted_count,
            raw_secret_bytes_emitted: 0,
            redacted_fields: redaction.redacted_fields.into_iter().collect(),
        },
        uncertainty_summary: SwarmReplayUncertaintySummary {
            missing_sources: missing_sources.into_iter().collect(),
            malformed_sources: malformed_sources.into_iter().collect(),
            stale_sources: stale_sources.into_iter().collect(),
            suppressed_claims: suppressed_claims.into_iter().collect(),
            event_count_by_uncertainty,
        },
        replay_guards: SwarmReplayGuards {
            read_only: true,
            no_live_mutation: true,
            no_network_required: true,
            fail_closed_on_missing_required_sources: true,
            requires_source_inventory: true,
            disallowed_live_actions: [
                "claim_bead",
                "close_bead",
                "send_agent_mail",
                "reserve_file",
                "release_file",
                "acquire_build_slot",
                "cancel_rch_job",
                "git_commit",
                "git_push",
            ]
            .iter()
            .map(ToString::to_string)
            .collect(),
        },
    })
}

fn validate_request(request: &SwarmReplayIngestRequest) -> Result<()> {
    if request.trace_id.trim().is_empty() {
        return Err(Error::validation("swarm replay trace_id cannot be empty"));
    }
    if !is_rfc3339_z(&request.generated_at_utc) {
        return Err(Error::validation(
            "swarm replay generated_at_utc must be RFC3339 UTC ending in Z",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn analyze_source(request: &SwarmReplayIngestRequest, template: &SourceTemplate) -> SourceAnalysis {
    let path = source_path(request, template);
    let inventory_path = display_path(&request.workspace_root, &path);
    let authoritative_for = template
        .authoritative_for
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    if !path.exists() {
        return SourceAnalysis {
            row: SwarmReplaySourceInventoryRow {
                source_id: template.source_id.to_string(),
                source_kind: template.source_kind.to_string(),
                path: inventory_path,
                availability: "unavailable".to_string(),
                freshness_state: "missing".to_string(),
                source_hash: None,
                redaction_state: template.default_redaction_state.to_string(),
                authoritative_for,
                uncertainty: vec!["source_missing".to_string()],
            },
            parsed: None,
        };
    }

    if path.is_dir() || template.format == SourceInputFormat::Opaque {
        return SourceAnalysis {
            row: SwarmReplaySourceInventoryRow {
                source_id: template.source_id.to_string(),
                source_kind: template.source_kind.to_string(),
                path: inventory_path,
                availability: "available".to_string(),
                freshness_state: "freshness_unknown".to_string(),
                source_hash: None,
                redaction_state: template.default_redaction_state.to_string(),
                authoritative_for,
                uncertainty: vec!["opaque_or_directory_source_not_parsed".to_string()],
            },
            parsed: None,
        };
    }

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            return SourceAnalysis {
                row: SwarmReplaySourceInventoryRow {
                    source_id: template.source_id.to_string(),
                    source_kind: template.source_kind.to_string(),
                    path: inventory_path,
                    availability: "unavailable".to_string(),
                    freshness_state: "missing".to_string(),
                    source_hash: None,
                    redaction_state: template.default_redaction_state.to_string(),
                    authoritative_for,
                    uncertainty: vec![format!("source_read_error:{err}")],
                },
                parsed: None,
            };
        }
    };
    let source_hash = Some(sha256_prefixed(&bytes));
    let text = String::from_utf8_lossy(&bytes);
    let parsed = match template.format {
        SourceInputFormat::Json => serde_json::from_str::<Value>(&text)
            .map(ParsedSource::Json)
            .map_err(|err| format!("json_parse_error:{err}")),
        SourceInputFormat::JsonLines => parse_json_lines(&text).map(ParsedSource::JsonLines),
        SourceInputFormat::Opaque => unreachable!("opaque sources returned before parsing"),
    };

    match parsed {
        Ok(parsed_source) => {
            let stale = parsed_source_is_stale(&parsed_source);
            SourceAnalysis {
                row: SwarmReplaySourceInventoryRow {
                    source_id: template.source_id.to_string(),
                    source_kind: template.source_kind.to_string(),
                    path: inventory_path,
                    availability: if stale { "stale" } else { "available" }.to_string(),
                    freshness_state: if stale { "stale" } else { "current" }.to_string(),
                    source_hash,
                    redaction_state: template.default_redaction_state.to_string(),
                    authoritative_for,
                    uncertainty: if stale {
                        vec!["source_declared_stale".to_string()]
                    } else {
                        Vec::new()
                    },
                },
                parsed: Some(parsed_source),
            }
        }
        Err(reason) => SourceAnalysis {
            row: SwarmReplaySourceInventoryRow {
                source_id: template.source_id.to_string(),
                source_kind: template.source_kind.to_string(),
                path: inventory_path,
                availability: "malformed".to_string(),
                freshness_state: "malformed".to_string(),
                source_hash,
                redaction_state: template.default_redaction_state.to_string(),
                authoritative_for,
                uncertainty: vec![reason],
            },
            parsed: None,
        },
    }
}

fn source_path(request: &SwarmReplayIngestRequest, template: &SourceTemplate) -> PathBuf {
    let path = request
        .source_overrides
        .get(template.source_id)
        .cloned()
        .unwrap_or_else(|| PathBuf::from(template.default_path));
    if path.is_absolute() {
        path
    } else {
        request.workspace_root.join(path)
    }
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map_or(path, |relative| relative)
        .to_string_lossy()
        .replace('\\', "/")
}

fn parse_json_lines(text: &str) -> std::result::Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        rows.push(
            serde_json::from_str(trimmed)
                .map_err(|err| format!("jsonl_parse_error:line_{}:{err}", index + 1))?,
        );
    }
    Ok(rows)
}

fn parsed_source_is_stale(parsed: &ParsedSource) -> bool {
    match parsed {
        ParsedSource::Json(value) => value_declares_stale(value),
        ParsedSource::JsonLines(rows) => rows.iter().any(value_declares_stale),
    }
}

fn value_declares_stale(value: &Value) -> bool {
    value
        .get("freshness_state")
        .and_then(Value::as_str)
        .is_some_and(|state| state.eq_ignore_ascii_case("stale"))
        || value
            .get("availability")
            .and_then(Value::as_str)
            .is_some_and(|state| state.eq_ignore_ascii_case("stale"))
        || value.get("stale").and_then(Value::as_bool).unwrap_or(false)
}

fn events_from_source(
    request: &SwarmReplayIngestRequest,
    template: &SourceTemplate,
    row: &SwarmReplaySourceInventoryRow,
    parsed: &ParsedSource,
    redaction: &mut RedactionAccumulator,
) -> Vec<PendingEvent> {
    match (template.source_id, parsed) {
        ("beads_jsonl", ParsedSource::JsonLines(rows)) => rows
            .iter()
            .map(|row_value| bead_lifecycle_event(request, row, row_value, redaction))
            .collect(),
        ("agent_mail_archive", ParsedSource::Json(value)) => {
            agent_mail_events(request, row, value, redaction)
        }
        ("doctor_swarm_diagnostics", ParsedSource::Json(value)) => {
            doctor_events(request, row, value, redaction)
        }
        ("rch_queue_status", ParsedSource::Json(value)) => {
            rch_events(request, row, value, redaction)
        }
        ("operator_runpack", ParsedSource::Json(value)) => {
            runpack_events(request, row, value, redaction)
        }
        ("git_refs", ParsedSource::Json(value)) => {
            vec![git_event(request, row, Some(value), redaction)]
        }
        ("validation_command_records", ParsedSource::Json(value)) => {
            validation_events(request, row, value, redaction)
        }
        ("context_intelligence_evidence", ParsedSource::Json(value)) => {
            vec![context_intelligence_event(request, row, value, redaction)]
        }
        ("swarm_flight_recorder", ParsedSource::JsonLines(rows)) => rows
            .iter()
            .map(|value| flight_recorder_event(request, row, value, redaction))
            .collect(),
        ("swarm_activity_ledger", ParsedSource::JsonLines(rows)) => rows
            .iter()
            .map(|value| activity_ledger_event(request, row, value, redaction))
            .collect(),
        ("git_refs", _) => vec![git_event(request, row, None, redaction)],
        _ => Vec::new(),
    }
}

fn missing_source_events(
    template: &SourceTemplate,
    row: &SwarmReplaySourceInventoryRow,
    generated_at_utc: &str,
) -> Vec<PendingEvent> {
    if template.source_id != "agent_mail_archive" {
        return Vec::new();
    }

    let suppressed_claims = suppressed_claims_for_source(template.source_id)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    vec![PendingEvent {
        candidate_id: format!("{}-missing-agent-message", template.source_id),
        occurred_at_utc: generated_at_utc.to_string(),
        observed_at_utc: generated_at_utc.to_string(),
        event_type: "agent_message".to_string(),
        actor: "agent-mail".to_string(),
        source_ref: row.source_id.clone(),
        source_hash: row.source_hash.clone(),
        redaction_state: "sensitive_omitted".to_string(),
        uncertainty: SwarmReplayEventUncertainty {
            state: "missing_source".to_string(),
            reasons: row.uncertainty.clone(),
            suppressed_claims,
        },
        payload: json!({
            "thread_id": "unknown",
            "sender": "unknown",
            "recipients": [],
            "importance": "unknown",
            "ack_required": false
        }),
    }]
}

fn bead_lifecycle_event(
    request: &SwarmReplayIngestRequest,
    row: &SwarmReplaySourceInventoryRow,
    value: &Value,
    redaction: &mut RedactionAccumulator,
) -> PendingEvent {
    let bead_id = string_field(value, &["id", "bead_id"], "unknown");
    let occurred = timestamp_field(value, request.generated_at_utc.as_str());
    let payload = json!({
        "bead_id": bead_id,
        "from_status": string_field(value, &["previous_status", "from_status"], "unknown"),
        "to_status": string_field(value, &["status", "to_status"], "unknown"),
        "priority": value.get("priority").and_then(Value::as_i64).unwrap_or_default(),
        "assignee": string_field(value, &["assignee"], "unassigned")
    });
    pending_event(
        request,
        row,
        event_seed(
            "bead_lifecycle",
            format!("bead-{bead_id}"),
            string_field(value, &["assignee", "created_by"], "beads"),
            occurred,
            payload,
        ),
        redaction,
    )
}

fn agent_mail_events(
    request: &SwarmReplayIngestRequest,
    row: &SwarmReplaySourceInventoryRow,
    value: &Value,
    redaction: &mut RedactionAccumulator,
) -> Vec<PendingEvent> {
    let mut events = Vec::new();
    for message in array_field(value, "messages") {
        let thread_id = string_field(message, &["thread_id", "threadId"], "unknown");
        let sender = string_field(message, &["sender", "from"], "unknown");
        let payload = json!({
            "thread_id": thread_id,
            "sender": sender,
            "recipients": string_array_field(message, "recipients"),
            "importance": string_field(message, &["importance"], "normal"),
            "ack_required": message.get("ack_required").and_then(Value::as_bool).unwrap_or(false)
        });
        let payload = with_optional_string(
            payload,
            message,
            &["body", "body_md", "content", "text"],
            "body",
        );
        events.push(pending_event(
            request,
            row,
            event_seed(
                "agent_message",
                format!("mail-{thread_id}-{sender}"),
                sender,
                timestamp_field(message, request.generated_at_utc.as_str()),
                payload,
            ),
            redaction,
        ));
    }
    for reservation in array_field(value, "reservations") {
        let reservation_id = string_field(reservation, &["reservation_id", "id"], "unknown");
        let payload = json!({
            "reservation_id": reservation_id,
            "path_patterns": string_array_field(reservation, "path_patterns"),
            "exclusive": reservation.get("exclusive").and_then(Value::as_bool).unwrap_or(false),
            "ttl_seconds": reservation.get("ttl_seconds").and_then(Value::as_u64).unwrap_or_default(),
            "reason": string_field(reservation, &["reason"], "unknown"),
            "holder": string_field(reservation, &["holder", "agent"], "unknown"),
            "state": string_field(reservation, &["state"], "active")
        });
        events.push(pending_event(
            request,
            row,
            event_seed(
                "reservation_intent",
                format!("reservation-{reservation_id}"),
                string_field(reservation, &["holder", "agent"], "agent-mail"),
                timestamp_field(reservation, request.generated_at_utc.as_str()),
                payload,
            ),
            redaction,
        ));
    }
    for conflict in array_field(value, "reservation_conflicts") {
        let path_pattern = string_field(conflict, &["path_pattern", "path"], "unknown");
        let payload = json!({
            "path_pattern": path_pattern,
            "holder": string_field(conflict, &["holder"], "unknown"),
            "conflict_reason": string_field(conflict, &["conflict_reason", "reason"], "unknown")
        });
        events.push(pending_event(
            request,
            row,
            event_seed(
                "reservation_conflict",
                format!("reservation-conflict-{path_pattern}"),
                string_field(conflict, &["holder"], "agent-mail"),
                timestamp_field(conflict, request.generated_at_utc.as_str()),
                payload,
            ),
            redaction,
        ));
    }
    for slot in array_field(value, "build_slots") {
        let slot_name = string_field(slot, &["slot"], "unknown");
        let payload = json!({
            "slot": slot_name,
            "holder": string_field(slot, &["holder"], "unknown"),
            "state": string_field(slot, &["state"], "unknown"),
            "expires_at_utc": string_field(slot, &["expires_at_utc", "expires_at"], "unknown")
        });
        events.push(pending_event(
            request,
            row,
            event_seed(
                "build_slot_state",
                format!("build-slot-{slot_name}"),
                string_field(slot, &["holder"], "agent-mail"),
                timestamp_field(slot, request.generated_at_utc.as_str()),
                payload,
            ),
            redaction,
        ));
    }
    events
}

fn doctor_events(
    request: &SwarmReplayIngestRequest,
    row: &SwarmReplaySourceInventoryRow,
    value: &Value,
    redaction: &mut RedactionAccumulator,
) -> Vec<PendingEvent> {
    let mut events = Vec::new();
    if let Some(profile) = value
        .get("host_profile")
        .or_else(|| value.get("resource_budget"))
        .filter(|item| item.is_object())
    {
        events.push(resource_profile_event_from_value(
            request, row, profile, redaction,
        ));
    }
    for profile in array_field(value, "host_profiles") {
        events.push(resource_profile_event_from_value(
            request, row, profile, redaction,
        ));
    }
    for profile in array_field(value, "resource_profiles") {
        events.push(resource_profile_event_from_value(
            request, row, profile, redaction,
        ));
    }

    let findings = array_field(value, "findings");
    if findings.is_empty() {
        if events.is_empty() {
            events.push(doctor_event_from_value(request, row, value, redaction));
        }
        return events;
    }
    events.extend(
        findings
            .into_iter()
            .map(|finding| doctor_event_from_value(request, row, finding, redaction)),
    );
    events
}

fn doctor_event_from_value(
    request: &SwarmReplayIngestRequest,
    row: &SwarmReplaySourceInventoryRow,
    value: &Value,
    redaction: &mut RedactionAccumulator,
) -> PendingEvent {
    let finding_id = string_field(value, &["finding_id", "id", "check"], "doctor-swarm");
    let payload = json!({
        "finding_id": finding_id,
        "severity": string_field(value, &["severity", "level"], "info"),
        "surface": string_field(value, &["surface", "category"], "swarm"),
        "status": string_field(value, &["status", "verdict"], "unknown")
    });
    pending_event(
        request,
        row,
        event_seed(
            "doctor_finding",
            format!("doctor-{finding_id}"),
            "doctor",
            timestamp_field(value, request.generated_at_utc.as_str()),
            payload,
        ),
        redaction,
    )
}

fn resource_profile_event_from_value(
    request: &SwarmReplayIngestRequest,
    row: &SwarmReplaySourceInventoryRow,
    value: &Value,
    redaction: &mut RedactionAccumulator,
) -> PendingEvent {
    let profile_id = string_field(value, &["profile_id", "id", "name"], "host-profile");
    let event_fragment = format!("resource-profile-{profile_id}");
    let payload = json!({
        "profile_id": profile_id,
        "cpu_cores": value.get("cpu_cores").and_then(Value::as_u64),
        "memory_gib": value.get("memory_gib").and_then(Value::as_u64),
        "numa_nodes": value.get("numa_nodes").and_then(Value::as_u64),
        "cgroup_cpu_quota": value.get("cgroup_cpu_quota").and_then(Value::as_u64),
        "cgroup_memory_gib": value.get("cgroup_memory_gib").and_then(Value::as_u64),
        "max_agent_concurrency": value.get("max_agent_concurrency").and_then(Value::as_u64),
        "max_tool_concurrency": value.get("max_tool_concurrency").and_then(Value::as_u64),
        "extension_hostcall_lanes": value.get("extension_hostcall_lanes").and_then(Value::as_u64),
        "rch_worker_slots": value.get("rch_worker_slots").and_then(Value::as_u64),
        "target_dir": string_field(value, &["target_dir", "cargo_target_dir"], "unknown"),
        "target_free_gib": value.get("target_free_gib").and_then(Value::as_u64),
        "tmpdir": string_field(value, &["tmpdir"], "unknown"),
        "tmpdir_free_gib": value.get("tmpdir_free_gib").and_then(Value::as_u64),
        "numa_hint": string_field(value, &["numa_hint"], "unknown")
    });
    pending_event(
        request,
        row,
        event_seed(
            "host_resource_profile",
            event_fragment,
            "doctor",
            timestamp_field(value, request.generated_at_utc.as_str()),
            payload,
        ),
        redaction,
    )
}

fn rch_events(
    request: &SwarmReplayIngestRequest,
    row: &SwarmReplaySourceInventoryRow,
    value: &Value,
    redaction: &mut RedactionAccumulator,
) -> Vec<PendingEvent> {
    let jobs = array_field(value, "jobs");
    let rows = if jobs.is_empty() {
        array_field(value, "queue")
    } else {
        jobs
    };
    if rows.is_empty() {
        return vec![rch_event_from_value(request, row, value, redaction)];
    }
    rows.into_iter()
        .map(|job| rch_event_from_value(request, row, job, redaction))
        .collect()
}

fn rch_event_from_value(
    request: &SwarmReplayIngestRequest,
    row: &SwarmReplaySourceInventoryRow,
    value: &Value,
    redaction: &mut RedactionAccumulator,
) -> PendingEvent {
    let job_id = string_field(value, &["job_id", "id"], "rch-status");
    let payload = json!({
        "job_id": job_id,
        "state": string_field(value, &["state", "status"], "unknown"),
        "worker": string_field(value, &["worker"], "unknown"),
        "command": string_field(value, &["command"], "unknown"),
        "queue_position": value.get("queue_position").and_then(Value::as_u64).unwrap_or_default()
    });
    pending_event(
        request,
        row,
        event_seed(
            "rch_job_state",
            format!("rch-{job_id}"),
            "rch",
            timestamp_field(value, request.generated_at_utc.as_str()),
            payload,
        ),
        redaction,
    )
}

fn runpack_events(
    request: &SwarmReplayIngestRequest,
    row: &SwarmReplaySourceInventoryRow,
    value: &Value,
    redaction: &mut RedactionAccumulator,
) -> Vec<PendingEvent> {
    let mut events = Vec::new();
    for recommendation in array_field(value, "recommendations") {
        let action = string_field(recommendation, &["action", "selected_action"], "unknown");
        let payload = json!({
            "action": action,
            "severity": string_field(recommendation, &["severity"], "info"),
            "evidence_paths": string_array_field(recommendation, "evidence_paths"),
            "operator_notes": string_field(recommendation, &["operator_notes", "notes"], "")
        });
        events.push(pending_event(
            request,
            row,
            event_seed(
                "runpack_recommendation",
                format!("runpack-{action}"),
                "operator_runpack",
                timestamp_field(recommendation, request.generated_at_utc.as_str()),
                payload,
            ),
            redaction,
        ));
    }
    if let Some(handoff) = value.get("operator_handoff") {
        events.push(operator_handoff_event(request, row, handoff, redaction));
    }
    if events.is_empty() {
        events.push(operator_handoff_event(request, row, value, redaction));
    }
    events
}

fn operator_handoff_event(
    request: &SwarmReplayIngestRequest,
    row: &SwarmReplaySourceInventoryRow,
    value: &Value,
    redaction: &mut RedactionAccumulator,
) -> PendingEvent {
    let handoff_id = string_field(value, &["handoff_id", "id"], "operator-handoff");
    let payload = json!({
        "handoff_id": handoff_id,
        "summary": string_field(value, &["summary"], ""),
        "next_actions": string_array_field(value, "next_actions"),
        "evidence_paths": string_array_field(value, "evidence_paths")
    });
    pending_event(
        request,
        row,
        event_seed(
            "operator_handoff",
            format!("handoff-{handoff_id}"),
            "operator_runpack",
            timestamp_field(value, request.generated_at_utc.as_str()),
            payload,
        ),
        redaction,
    )
}

fn git_event(
    request: &SwarmReplayIngestRequest,
    row: &SwarmReplaySourceInventoryRow,
    value: Option<&Value>,
    redaction: &mut RedactionAccumulator,
) -> PendingEvent {
    let payload = json!({
        "head": value.map_or_else(
            || request.git_commit.clone().unwrap_or_else(|| "unknown".to_string()),
            |v| string_field(v, &["head", "commit"], request.git_commit.as_deref().unwrap_or("unknown")),
        ),
        "branch": value.map_or_else(
            || request.git_branch.clone().unwrap_or_else(|| "unknown".to_string()),
            |v| string_field(v, &["branch"], request.git_branch.as_deref().unwrap_or("unknown")),
        ),
        "dirty": value.and_then(|v| v.get("dirty")).and_then(Value::as_bool).unwrap_or(false),
        "changed_paths": value.map_or_else(Vec::new, |v| string_array_field(v, "changed_paths"))
    });
    pending_event(
        request,
        row,
        event_seed(
            "worktree_state",
            "git-worktree",
            "git",
            value.map_or_else(
                || request.generated_at_utc.clone(),
                |v| timestamp_field(v, request.generated_at_utc.as_str()),
            ),
            payload,
        ),
        redaction,
    )
}

fn validation_events(
    request: &SwarmReplayIngestRequest,
    row: &SwarmReplaySourceInventoryRow,
    value: &Value,
    redaction: &mut RedactionAccumulator,
) -> Vec<PendingEvent> {
    let mut events = Vec::new();
    for command in array_field(value, "commands") {
        let command_text = string_field(command, &["command"], "unknown");
        let payload = json!({
            "command": command_text,
            "runner": string_field(command, &["runner"], "unknown"),
            "exit_code": command.get("exit_code").and_then(Value::as_i64).unwrap_or_default(),
            "target_dir": string_field(command, &["target_dir"], "unknown"),
            "tmpdir": string_field(command, &["tmpdir"], "unknown")
        });
        events.push(pending_event(
            request,
            row,
            event_seed(
                "cargo_gate_result",
                format!("cargo-gate-{command_text}"),
                "validation",
                timestamp_field(command, request.generated_at_utc.as_str()),
                payload,
            ),
            redaction,
        ));
    }
    for artifact in array_field(value, "artifacts") {
        events.push(validation_artifact_event(request, row, artifact, redaction));
    }
    if events.is_empty() {
        events.push(validation_artifact_event(request, row, value, redaction));
    }
    events
}

fn context_intelligence_event(
    request: &SwarmReplayIngestRequest,
    row: &SwarmReplaySourceInventoryRow,
    value: &Value,
    redaction: &mut RedactionAccumulator,
) -> PendingEvent {
    let payload = json!({
        "artifact_path": row.path,
        "artifact_schema": string_field(value, &["schema"], "context_intelligence_evidence"),
        "verdict": string_field(value, &["verdict", "status", "overall_verdict"], "unknown"),
        "command": "context-intelligence-closeout-gate"
    });
    pending_event(
        request,
        row,
        event_seed(
            "validation_artifact",
            "context-intelligence-evidence",
            "context_intelligence",
            timestamp_field(value, request.generated_at_utc.as_str()),
            payload,
        ),
        redaction,
    )
}

fn flight_recorder_event(
    request: &SwarmReplayIngestRequest,
    row: &SwarmReplaySourceInventoryRow,
    value: &Value,
    redaction: &mut RedactionAccumulator,
) -> PendingEvent {
    let event_kind = string_field(value, &["event_kind", "eventKind"], "flight-recorder");
    let payload = json!({
        "artifact_path": row.path,
        "artifact_schema": string_field(value, &["schema"], "pi.swarm.flight_recorder.event.v1"),
        "verdict": "observed",
        "command": event_kind
    });
    pending_event(
        request,
        row,
        event_seed(
            "validation_artifact",
            format!("flight-{event_kind}"),
            string_field(value, &["agent_name", "agent"], "flight_recorder"),
            timestamp_field(value, request.generated_at_utc.as_str()),
            payload,
        ),
        redaction,
    )
}

fn activity_ledger_event(
    request: &SwarmReplayIngestRequest,
    row: &SwarmReplaySourceInventoryRow,
    value: &Value,
    redaction: &mut RedactionAccumulator,
) -> PendingEvent {
    let event_kind = string_field(value, &["event_kind", "kind"], "activity-ledger");
    if event_kind.contains("handoff") {
        return operator_handoff_event(request, row, value, redaction);
    }
    let payload = json!({
        "artifact_path": row.path,
        "artifact_schema": string_field(value, &["schema"], "pi.swarm.activity_ledger.v1"),
        "verdict": "observed",
        "command": event_kind
    });
    pending_event(
        request,
        row,
        event_seed(
            "validation_artifact",
            format!("activity-{event_kind}"),
            string_field(value, &["agent_name", "agent"], "activity_ledger"),
            timestamp_field(value, request.generated_at_utc.as_str()),
            payload,
        ),
        redaction,
    )
}

fn validation_artifact_event(
    request: &SwarmReplayIngestRequest,
    row: &SwarmReplaySourceInventoryRow,
    value: &Value,
    redaction: &mut RedactionAccumulator,
) -> PendingEvent {
    let artifact_path = string_field(value, &["artifact_path", "path"], row.path.as_str());
    let payload = json!({
        "artifact_path": artifact_path,
        "artifact_schema": string_field(value, &["artifact_schema", "schema"], "unknown"),
        "verdict": string_field(value, &["verdict", "status"], "unknown"),
        "command": string_field(value, &["command"], "unknown")
    });
    pending_event(
        request,
        row,
        event_seed(
            "validation_artifact",
            format!("validation-artifact-{artifact_path}"),
            "validation",
            timestamp_field(value, request.generated_at_utc.as_str()),
            payload,
        ),
        redaction,
    )
}

fn event_seed(
    event_type: &'static str,
    candidate_id: impl Into<String>,
    actor: impl Into<String>,
    occurred_at_utc: String,
    payload: Value,
) -> PendingEventSeed {
    PendingEventSeed {
        event_type,
        candidate_id: candidate_id.into(),
        actor: actor.into(),
        occurred_at_utc,
        payload,
    }
}

fn pending_event(
    request: &SwarmReplayIngestRequest,
    row: &SwarmReplaySourceInventoryRow,
    seed: PendingEventSeed,
    redaction: &mut RedactionAccumulator,
) -> PendingEvent {
    let mut redacted_payload = seed.payload;
    let redacted_fields = redact_value(&mut redacted_payload);
    let redaction_state = if redacted_fields.is_empty() {
        row.redaction_state.clone()
    } else {
        redaction.redacted_count += 1;
        redaction.sensitive_omitted_count +=
            u64::try_from(redacted_fields.len()).unwrap_or(u64::MAX);
        redaction.redacted_fields.extend(redacted_fields);
        "redacted".to_string()
    };
    let mut reasons = row.uncertainty.clone();
    let state = match row.availability.as_str() {
        "stale" => {
            reasons.push("source_stale".to_string());
            "partial"
        }
        _ if reasons.is_empty() => "certain",
        _ => "uncertain",
    };
    PendingEvent {
        candidate_id: stable_id(&seed.candidate_id),
        occurred_at_utc: seed.occurred_at_utc,
        observed_at_utc: request.generated_at_utc.clone(),
        event_type: seed.event_type.to_string(),
        actor: seed.actor,
        source_ref: row.source_id.clone(),
        source_hash: row.source_hash.clone(),
        redaction_state,
        uncertainty: SwarmReplayEventUncertainty {
            state: state.to_string(),
            reasons,
            suppressed_claims: Vec::new(),
        },
        payload: redacted_payload,
    }
}

fn finalize_events(mut pending: Vec<PendingEvent>) -> Vec<SwarmReplayEvent> {
    pending.sort_by(|left, right| {
        left.occurred_at_utc
            .cmp(&right.occurred_at_utc)
            .then_with(|| left.source_ref.cmp(&right.source_ref))
            .then_with(|| left.candidate_id.cmp(&right.candidate_id))
    });

    let mut seen = BTreeMap::<String, u64>::new();
    pending
        .into_iter()
        .enumerate()
        .map(|(index, mut event)| {
            let count = seen.entry(event.candidate_id.clone()).or_insert(0);
            *count += 1;
            let event_id = if *count == 1 {
                event.candidate_id.clone()
            } else {
                event
                    .uncertainty
                    .reasons
                    .push("duplicate_source_event_id_deduplicated".to_string());
                event.uncertainty.state = "uncertain".to_string();
                format!("{}-dup-{}", event.candidate_id, count)
            };
            SwarmReplayEvent {
                event_id,
                sequence: u64::try_from(index + 1).unwrap_or(u64::MAX),
                occurred_at_utc: event.occurred_at_utc,
                observed_at_utc: event.observed_at_utc,
                event_type: event.event_type,
                actor: event.actor,
                source_ref: event.source_ref,
                source_hash: event.source_hash,
                redaction_state: event.redaction_state,
                uncertainty: event.uncertainty,
                payload: event.payload,
            }
        })
        .collect()
}

fn suppressed_claims_for_source(source_id: &str) -> &'static [&'static str] {
    match source_id {
        "agent_mail_archive" => &[
            "ack_latency",
            "active_reservation_holder",
            "mail_thread_completeness",
            "build_slot_ownership",
        ],
        "rch_queue_status" => &[
            "queue_depth",
            "remote_admission_state",
            "rch_worker_assignment",
        ],
        "operator_runpack" => &["operator_next_action", "operator_handoff_completeness"],
        "doctor_swarm_diagnostics" => &["swarm_health_verdict"],
        "validation_command_records" => &["cargo_gate_success", "validation_artifact_verdict"],
        "context_intelligence_evidence" => &["context_intelligence_freshness"],
        "swarm_flight_recorder" => &["flight_recorder_replay_completeness"],
        "swarm_activity_ledger" => &["activity_ledger_handoff_completeness"],
        _ => &[],
    }
}

fn redact_value(value: &mut Value) -> BTreeSet<String> {
    let mut redacted = BTreeSet::new();
    redact_value_inner(value, "", &mut redacted);
    redacted
}

fn redact_value_inner(value: &mut Value, path: &str, redacted: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                let nested_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                if is_sensitive_key(key) {
                    *nested = Value::String(SENSITIVE_REDACTION.to_string());
                    redacted.insert(nested_path);
                } else {
                    redact_value_inner(nested, &nested_path, redacted);
                }
            }
        }
        Value::Array(items) => {
            for (index, nested) in items.iter_mut().enumerate() {
                redact_value_inner(nested, &format!("{path}[{index}]"), redacted);
            }
        }
        _ => {}
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SENSITIVE_KEY_FRAGMENTS
        .iter()
        .any(|fragment| lower.contains(fragment))
}

fn string_field(value: &Value, keys: &[&str], fallback: &str) -> String {
    optional_string_field(value, keys).unwrap_or_else(|| fallback.to_string())
}

fn optional_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .filter(|raw| !raw.trim().is_empty())
        .map(ToString::to_string)
}

fn with_optional_string(
    mut payload: Value,
    source: &Value,
    source_keys: &[&str],
    payload_key: &str,
) -> Value {
    if let (Some(value), Value::Object(map)) =
        (optional_string_field(source, source_keys), &mut payload)
    {
        map.insert(payload_key.to_string(), Value::String(value));
    }
    payload
}

fn string_array_field(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn array_field<'a>(value: &'a Value, key: &str) -> Vec<&'a Value> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |items| items.iter().collect())
}

fn timestamp_field(value: &Value, fallback: &str) -> String {
    let raw = [
        "occurred_at_utc",
        "occurred_at",
        "updated_at",
        "created_at",
        "generated_at",
        "timestamp",
    ]
    .iter()
    .find_map(|key| value.get(*key).and_then(Value::as_str))
    .unwrap_or(fallback);
    normalize_utc_timestamp(raw).unwrap_or_else(|| fallback.to_string())
}

fn normalize_utc_timestamp(raw: &str) -> Option<String> {
    if is_rfc3339_z(raw) {
        return Some(raw.to_string());
    }
    DateTime::parse_from_rfc3339(raw).ok().map(|datetime| {
        datetime
            .with_timezone(&Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    })
}

fn is_rfc3339_z(value: &str) -> bool {
    value.len() >= 20 && value.contains('T') && value.ends_with('Z')
}

fn stable_id(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut previous_dash = false;
    for byte in raw.bytes() {
        let ch = char::from(byte).to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            previous_dash = false;
        } else if !previous_dash {
            out.push('-');
            previous_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "event".to_string()
    } else {
        trimmed.to_string()
    }
}

fn sha256_prefixed(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", crate::package_manager::hex_encode(&digest))
}

#[allow(dead_code)]
fn object_from_pairs(pairs: &[(&str, Value)]) -> Value {
    let mut map = Map::new();
    for (key, value) in pairs {
        map.insert((*key).to_string(), value.clone());
    }
    Value::Object(map)
}
