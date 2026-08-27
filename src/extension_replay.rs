//! Deterministic replay trace bundle core for extension runtime forensics.
//!
//! This module provides a standalone schema + codec surface that records
//! extension runtime events in a stable order so race/tail anomalies can be
//! replayed and compared deterministically.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

/// Canonical schema identifier for replay trace bundles.
pub const REPLAY_TRACE_SCHEMA_V1: &str = "pi.ext.replay.trace.v1";

/// Kind of extension runtime event captured for deterministic replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayEventKind {
    Scheduled,
    QueueAccepted,
    PolicyDecision,
    Cancelled,
    Retried,
    Completed,
    Failed,
}

impl ReplayEventKind {
    const fn canonical_rank(self) -> u8 {
        match self {
            Self::Scheduled => 0,
            Self::QueueAccepted => 1,
            Self::PolicyDecision => 2,
            Self::Cancelled => 3,
            Self::Retried => 4,
            Self::Completed => 5,
            Self::Failed => 6,
        }
    }
}

/// Single deterministic replay trace event.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayTraceEvent {
    pub seq: u64,
    pub logical_clock: u64,
    pub extension_id: String,
    pub request_id: String,
    pub kind: ReplayEventKind,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, String>,
}

/// Builder input event before canonical ordering/sequence assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayEventDraft {
    pub logical_clock: u64,
    pub extension_id: String,
    pub request_id: String,
    pub kind: ReplayEventKind,
    pub attributes: BTreeMap<String, String>,
}

impl ReplayEventDraft {
    #[must_use]
    pub fn new(
        logical_clock: u64,
        extension_id: impl Into<String>,
        request_id: impl Into<String>,
        kind: ReplayEventKind,
    ) -> Self {
        Self {
            logical_clock,
            extension_id: extension_id.into(),
            request_id: request_id.into(),
            kind,
            attributes: BTreeMap::new(),
        }
    }
}

/// Deterministic replay trace bundle.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayTraceBundle {
    pub schema: String,
    pub trace_id: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    pub events: Vec<ReplayTraceEvent>,
}

/// First deterministic mismatch between two replay bundles.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayDivergence {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub reason: ReplayDivergenceReason,
}

/// Machine-readable mismatch reason for replay comparisons.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayDivergenceReason {
    SchemaMismatch {
        expected: String,
        observed: String,
    },
    TraceIdMismatch {
        expected: String,
        observed: String,
    },
    EventCountMismatch {
        expected: u64,
        observed: u64,
    },
    EventFieldMismatch {
        field: String,
        expected: String,
        observed: String,
    },
}

/// Configuration budget for deterministic replay trace capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayCaptureBudget {
    /// Global kill switch for replay capture in production.
    pub capture_enabled: bool,
    /// Maximum allowed overhead in per-mille (1/1000) units.
    pub max_overhead_per_mille: u32,
    /// Maximum allowed serialized trace bytes for a capture window.
    pub max_trace_bytes: u64,
}

/// Runtime observation used to evaluate replay capture budget gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayCaptureObservation {
    /// Baseline runtime cost without replay capture.
    pub baseline_micros: u64,
    /// Measured runtime cost with replay capture active.
    pub captured_micros: u64,
    /// Size of the collected replay trace payload in bytes.
    pub trace_bytes: u64,
}

/// Deterministic gate reason emitted by replay capture budget evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayCaptureGateReason {
    Enabled,
    DisabledByConfig,
    DisabledByOverheadBudget,
    DisabledByTraceBudget,
    DisabledByInvalidBaseline,
}

/// Machine-readable report for replay capture gating decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayCaptureGateReport {
    pub capture_allowed: bool,
    pub reason: ReplayCaptureGateReason,
    pub observed_overhead_per_mille: u32,
    pub max_overhead_per_mille: u32,
    pub observed_trace_bytes: u64,
    pub max_trace_bytes: u64,
}

/// Deterministic hint categories for automated replay triage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayRootCauseHint {
    TraceSchemaMismatch,
    TraceIdMismatch,
    EventCountDrift,
    EventPayloadDrift,
    LogicalClockDrift,
    PolicyGateDisabled,
    OverheadBudgetExceeded,
    TraceBudgetExceeded,
    InvalidBaselineTelemetry,
}

/// Structured replay diagnostic snapshot for log/event sinks.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayDiagnosticSnapshot {
    pub trace_id: String,
    pub schema: String,
    pub event_count: u64,
    pub capture_gate: ReplayCaptureGateReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub divergence: Option<ReplayDivergence>,
    pub root_cause_hints: Vec<ReplayRootCauseHint>,
}

/// Evaluate replay capture against overhead and size budgets.
#[must_use]
pub fn evaluate_replay_capture_gate(
    budget: ReplayCaptureBudget,
    observation: ReplayCaptureObservation,
) -> ReplayCaptureGateReport {
    if !budget.capture_enabled {
        return ReplayCaptureGateReport {
            capture_allowed: false,
            reason: ReplayCaptureGateReason::DisabledByConfig,
            observed_overhead_per_mille: 0,
            max_overhead_per_mille: budget.max_overhead_per_mille,
            observed_trace_bytes: observation.trace_bytes,
            max_trace_bytes: budget.max_trace_bytes,
        };
    }

    let observed_overhead_per_mille =
        compute_overhead_per_mille(observation.baseline_micros, observation.captured_micros);

    if observed_overhead_per_mille == u32::MAX {
        return ReplayCaptureGateReport {
            capture_allowed: false,
            reason: ReplayCaptureGateReason::DisabledByInvalidBaseline,
            observed_overhead_per_mille,
            max_overhead_per_mille: budget.max_overhead_per_mille,
            observed_trace_bytes: observation.trace_bytes,
            max_trace_bytes: budget.max_trace_bytes,
        };
    }

    if observed_overhead_per_mille > budget.max_overhead_per_mille {
        return ReplayCaptureGateReport {
            capture_allowed: false,
            reason: ReplayCaptureGateReason::DisabledByOverheadBudget,
            observed_overhead_per_mille,
            max_overhead_per_mille: budget.max_overhead_per_mille,
            observed_trace_bytes: observation.trace_bytes,
            max_trace_bytes: budget.max_trace_bytes,
        };
    }

    if observation.trace_bytes > budget.max_trace_bytes {
        return ReplayCaptureGateReport {
            capture_allowed: false,
            reason: ReplayCaptureGateReason::DisabledByTraceBudget,
            observed_overhead_per_mille,
            max_overhead_per_mille: budget.max_overhead_per_mille,
            observed_trace_bytes: observation.trace_bytes,
            max_trace_bytes: budget.max_trace_bytes,
        };
    }

    ReplayCaptureGateReport {
        capture_allowed: true,
        reason: ReplayCaptureGateReason::Enabled,
        observed_overhead_per_mille,
        max_overhead_per_mille: budget.max_overhead_per_mille,
        observed_trace_bytes: observation.trace_bytes,
        max_trace_bytes: budget.max_trace_bytes,
    }
}

/// Build a machine-readable replay diagnostics snapshot.
///
/// # Errors
///
/// Returns an error when the replay bundle fails deterministic validation.
/// Schema mismatches relax only the schema-version check so diagnostics can
/// still be emitted for otherwise well-formed foreign bundles.
pub fn build_replay_diagnostic_snapshot(
    bundle: &ReplayTraceBundle,
    capture_gate: ReplayCaptureGateReport,
    divergence: Option<&ReplayDivergence>,
) -> Result<ReplayDiagnosticSnapshot, ReplayTraceValidationError> {
    if matches!(
        divergence,
        Some(ReplayDivergence {
            reason: ReplayDivergenceReason::SchemaMismatch { .. },
            ..
        })
    ) {
        bundle.validate_structure()?;
    } else {
        bundle.validate()?;
    }

    let event_count = u64::try_from(bundle.events.len())
        .map_err(|_| ReplayTraceValidationError::TooManyEvents)?;
    let root_cause_hints = derive_root_cause_hints(capture_gate.reason, divergence);

    Ok(ReplayDiagnosticSnapshot {
        trace_id: bundle.trace_id.clone(),
        schema: bundle.schema.clone(),
        event_count,
        capture_gate,
        divergence: divergence.cloned(),
        root_cause_hints,
    })
}

fn compute_overhead_per_mille(baseline_micros: u64, captured_micros: u64) -> u32 {
    if baseline_micros == 0 {
        return u32::MAX;
    }
    if captured_micros <= baseline_micros {
        return 0;
    }

    let overhead = u128::from(captured_micros - baseline_micros);
    let baseline = u128::from(baseline_micros);
    let scaled = overhead.saturating_mul(1_000);
    let rounded_up = scaled
        .saturating_add(baseline - 1)
        .checked_div(baseline)
        .unwrap_or(u128::MAX);
    u32::try_from(rounded_up).unwrap_or(u32::MAX)
}

fn derive_root_cause_hints(
    gate_reason: ReplayCaptureGateReason,
    divergence: Option<&ReplayDivergence>,
) -> Vec<ReplayRootCauseHint> {
    let mut hints = BTreeSet::new();

    match gate_reason {
        ReplayCaptureGateReason::Enabled => {}
        ReplayCaptureGateReason::DisabledByConfig => {
            hints.insert(ReplayRootCauseHint::PolicyGateDisabled);
        }
        ReplayCaptureGateReason::DisabledByOverheadBudget => {
            hints.insert(ReplayRootCauseHint::OverheadBudgetExceeded);
        }
        ReplayCaptureGateReason::DisabledByTraceBudget => {
            hints.insert(ReplayRootCauseHint::TraceBudgetExceeded);
        }
        ReplayCaptureGateReason::DisabledByInvalidBaseline => {
            hints.insert(ReplayRootCauseHint::InvalidBaselineTelemetry);
        }
    }

    if let Some(divergence) = divergence {
        match &divergence.reason {
            ReplayDivergenceReason::SchemaMismatch { .. } => {
                hints.insert(ReplayRootCauseHint::TraceSchemaMismatch);
            }
            ReplayDivergenceReason::TraceIdMismatch { .. } => {
                hints.insert(ReplayRootCauseHint::TraceIdMismatch);
            }
            ReplayDivergenceReason::EventCountMismatch { .. } => {
                hints.insert(ReplayRootCauseHint::EventCountDrift);
            }
            ReplayDivergenceReason::EventFieldMismatch { field, .. } => {
                if field == "logical_clock" {
                    hints.insert(ReplayRootCauseHint::LogicalClockDrift);
                } else {
                    hints.insert(ReplayRootCauseHint::EventPayloadDrift);
                }
            }
        }
    }

    hints.into_iter().collect()
}

fn observation_with_measured_trace_bytes(
    mut observation: ReplayCaptureObservation,
    bundle: &ReplayTraceBundle,
) -> ReplayCaptureObservation {
    if observation.trace_bytes == 0 {
        observation.trace_bytes = encoded_trace_len(bundle);
    }
    observation
}

fn encoded_trace_len(bundle: &ReplayTraceBundle) -> u64 {
    bundle
        .encode_json()
        .ok()
        .and_then(|json| u64::try_from(json.len()).ok())
        .unwrap_or(u64::MAX)
}

impl ReplayTraceBundle {
    /// Encode bundle as compact JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn encode_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Decode bundle from JSON and validate deterministic invariants.
    ///
    /// # Errors
    ///
    /// Returns an error if parsing fails or bundle invariants are invalid.
    pub fn decode_json(input: &str) -> Result<Self, ReplayTraceCodecError> {
        let bundle: Self = serde_json::from_str(input)?;
        bundle.validate()?;
        Ok(bundle)
    }

    /// Validate schema, sequence continuity, and cancellation/retry ordering.
    ///
    /// # Errors
    ///
    /// Returns an error when the bundle is malformed or violates replay
    /// ordering invariants.
    pub fn validate(&self) -> Result<(), ReplayTraceValidationError> {
        self.validate_schema()?;
        self.validate_structure()
    }

    fn validate_schema(&self) -> Result<(), ReplayTraceValidationError> {
        if self.schema != REPLAY_TRACE_SCHEMA_V1 {
            return Err(ReplayTraceValidationError::UnknownSchema(
                self.schema.clone(),
            ));
        }
        Ok(())
    }

    fn validate_structure(&self) -> Result<(), ReplayTraceValidationError> {
        if self.trace_id.trim().is_empty() {
            return Err(ReplayTraceValidationError::EmptyTraceId);
        }

        for (idx, event) in self.events.iter().enumerate() {
            let seq_index = idx
                .checked_add(1)
                .ok_or(ReplayTraceValidationError::TooManyEvents)?;
            let expected_seq =
                u64::try_from(seq_index).map_err(|_| ReplayTraceValidationError::TooManyEvents)?;
            if event.seq != expected_seq {
                return Err(ReplayTraceValidationError::NonContiguousSequence {
                    expected: expected_seq,
                    observed: event.seq,
                });
            }

            if event.extension_id.trim().is_empty() {
                return Err(ReplayTraceValidationError::MissingExtensionId { seq: event.seq });
            }
            if event.request_id.trim().is_empty() {
                return Err(ReplayTraceValidationError::MissingRequestId { seq: event.seq });
            }
        }

        self.validate_retry_ordering()
    }

    fn validate_retry_ordering(&self) -> Result<(), ReplayTraceValidationError> {
        let mut pending_cancel: BTreeSet<(String, String)> = BTreeSet::new();
        for event in &self.events {
            let key = (event.extension_id.clone(), event.request_id.clone());
            match event.kind {
                ReplayEventKind::Cancelled => {
                    if !pending_cancel.insert(key) {
                        return Err(ReplayTraceValidationError::DuplicateCancelWithoutRetry {
                            seq: event.seq,
                            extension_id: event.extension_id.clone(),
                            request_id: event.request_id.clone(),
                        });
                    }
                }
                ReplayEventKind::Retried => {
                    if !pending_cancel.remove(&key) {
                        return Err(ReplayTraceValidationError::RetryWithoutCancel {
                            seq: event.seq,
                            extension_id: event.extension_id.clone(),
                            request_id: event.request_id.clone(),
                        });
                    }
                }
                ReplayEventKind::Completed | ReplayEventKind::Failed => {
                    pending_cancel.remove(&key);
                }
                ReplayEventKind::Scheduled
                | ReplayEventKind::QueueAccepted
                | ReplayEventKind::PolicyDecision => {}
            }
        }
        Ok(())
    }
}

/// Compare two bundles and return the first deterministic divergence.
///
/// Both bundles must be structurally valid before comparison. If the schemas
/// differ, the comparison reports a schema mismatch without requiring support
/// for the foreign schema version.
///
/// # Errors
///
/// Returns an error if either bundle fails validation.
pub fn first_divergence(
    expected: &ReplayTraceBundle,
    observed: &ReplayTraceBundle,
) -> Result<Option<ReplayDivergence>, ReplayTraceValidationError> {
    expected.validate_structure()?;
    observed.validate_structure()?;

    if expected.schema != observed.schema {
        return Ok(Some(ReplayDivergence {
            seq: None,
            reason: ReplayDivergenceReason::SchemaMismatch {
                expected: expected.schema.clone(),
                observed: observed.schema.clone(),
            },
        }));
    }

    expected.validate_schema()?;
    observed.validate_schema()?;

    if expected.trace_id != observed.trace_id {
        return Ok(Some(ReplayDivergence {
            seq: None,
            reason: ReplayDivergenceReason::TraceIdMismatch {
                expected: expected.trace_id.clone(),
                observed: observed.trace_id.clone(),
            },
        }));
    }

    let max_shared = expected.events.len().min(observed.events.len());
    for idx in 0..max_shared {
        let left = &expected.events[idx];
        let right = &observed.events[idx];
        if left.logical_clock != right.logical_clock {
            return Ok(Some(field_mismatch(
                left.seq,
                "logical_clock",
                left.logical_clock.to_string(),
                right.logical_clock.to_string(),
            )));
        }
        if left.extension_id != right.extension_id {
            return Ok(Some(field_mismatch(
                left.seq,
                "extension_id",
                left.extension_id.clone(),
                right.extension_id.clone(),
            )));
        }
        if left.request_id != right.request_id {
            return Ok(Some(field_mismatch(
                left.seq,
                "request_id",
                left.request_id.clone(),
                right.request_id.clone(),
            )));
        }
        if left.kind != right.kind {
            return Ok(Some(field_mismatch(
                left.seq,
                "kind",
                format!("{:?}", left.kind),
                format!("{:?}", right.kind),
            )));
        }
        if left.attributes != right.attributes {
            return Ok(Some(field_mismatch(
                left.seq,
                "attributes",
                format!("{:?}", left.attributes),
                format!("{:?}", right.attributes),
            )));
        }
    }

    if expected.events.len() != observed.events.len() {
        let next_seq = max_shared
            .checked_add(1)
            .ok_or(ReplayTraceValidationError::TooManyEvents)?;
        let seq = u64::try_from(next_seq).map_err(|_| ReplayTraceValidationError::TooManyEvents)?;
        return Ok(Some(ReplayDivergence {
            seq: Some(seq),
            reason: ReplayDivergenceReason::EventCountMismatch {
                expected: u64::try_from(expected.events.len())
                    .map_err(|_| ReplayTraceValidationError::TooManyEvents)?,
                observed: u64::try_from(observed.events.len())
                    .map_err(|_| ReplayTraceValidationError::TooManyEvents)?,
            },
        }));
    }

    Ok(None)
}

fn field_mismatch(seq: u64, field: &str, expected: String, observed: String) -> ReplayDivergence {
    ReplayDivergence {
        seq: Some(seq),
        reason: ReplayDivergenceReason::EventFieldMismatch {
            field: field.to_string(),
            expected,
            observed,
        },
    }
}

/// Builder that canonicalizes event ordering and sequence assignment.
#[derive(Debug, Clone, Default)]
pub struct ReplayTraceBuilder {
    trace_id: String,
    metadata: BTreeMap<String, String>,
    drafts: Vec<ReplayEventDraft>,
}

impl ReplayTraceBuilder {
    #[must_use]
    pub fn new(trace_id: impl Into<String>) -> Self {
        Self {
            trace_id: trace_id.into(),
            metadata: BTreeMap::new(),
            drafts: Vec::new(),
        }
    }

    pub fn insert_metadata(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.metadata.insert(key.into(), value.into());
    }

    pub fn push(&mut self, draft: ReplayEventDraft) {
        self.drafts.push(draft);
    }

    /// Build a validated, deterministic trace bundle.
    ///
    /// # Errors
    ///
    /// Returns an error if sequence assignment overflows or validation fails.
    pub fn build(self) -> Result<ReplayTraceBundle, ReplayTraceValidationError> {
        let mut indexed = self
            .drafts
            .into_iter()
            .enumerate()
            .map(|(insertion_index, draft)| IndexedDraft {
                insertion_index,
                draft,
            })
            .collect::<Vec<_>>();
        indexed.sort_by(compare_indexed_drafts);

        let events = indexed
            .into_iter()
            .enumerate()
            .map(|(idx, entry)| {
                let seq_index = idx
                    .checked_add(1)
                    .ok_or(ReplayTraceValidationError::TooManyEvents)?;
                let seq = u64::try_from(seq_index)
                    .map_err(|_| ReplayTraceValidationError::TooManyEvents)?;
                Ok(ReplayTraceEvent {
                    seq,
                    logical_clock: entry.draft.logical_clock,
                    extension_id: entry.draft.extension_id,
                    request_id: entry.draft.request_id,
                    kind: entry.draft.kind,
                    attributes: entry.draft.attributes,
                })
            })
            .collect::<Result<Vec<_>, ReplayTraceValidationError>>()?;

        let bundle = ReplayTraceBundle {
            schema: REPLAY_TRACE_SCHEMA_V1.to_string(),
            trace_id: self.trace_id,
            metadata: self.metadata,
            events,
        };
        bundle.validate()?;
        Ok(bundle)
    }
}

#[derive(Debug, Clone)]
struct IndexedDraft {
    insertion_index: usize,
    draft: ReplayEventDraft,
}

fn compare_indexed_drafts(left: &IndexedDraft, right: &IndexedDraft) -> Ordering {
    left.draft
        .logical_clock
        .cmp(&right.draft.logical_clock)
        .then_with(|| left.draft.extension_id.cmp(&right.draft.extension_id))
        .then_with(|| left.draft.request_id.cmp(&right.draft.request_id))
        .then_with(|| {
            left.draft
                .kind
                .canonical_rank()
                .cmp(&right.draft.kind.canonical_rank())
        })
        .then_with(|| left.insertion_index.cmp(&right.insertion_index))
}

/// Validation failures for replay bundle semantics.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ReplayTraceValidationError {
    #[error("unknown replay trace schema: {0}")]
    UnknownSchema(String),
    #[error("trace id must not be empty")]
    EmptyTraceId,
    #[error("replay bundle contains too many events to index")]
    TooManyEvents,
    #[error("non-contiguous sequence: expected {expected}, observed {observed}")]
    NonContiguousSequence { expected: u64, observed: u64 },
    #[error("event seq {seq} missing extension id")]
    MissingExtensionId { seq: u64 },
    #[error("event seq {seq} missing request id")]
    MissingRequestId { seq: u64 },
    #[error("retry without prior cancel at seq {seq} for {extension_id}/{request_id}")]
    RetryWithoutCancel {
        seq: u64,
        extension_id: String,
        request_id: String,
    },
    #[error("duplicate cancel without retry at seq {seq} for {extension_id}/{request_id}")]
    DuplicateCancelWithoutRetry {
        seq: u64,
        extension_id: String,
        request_id: String,
    },
}

/// Codec-level decode failures.
#[derive(Debug, Error)]
pub enum ReplayTraceCodecError {
    #[error("failed to parse replay trace JSON: {0}")]
    Deserialize(#[from] serde_json::Error),
    #[error("invalid replay trace bundle: {0}")]
    Validation(#[from] ReplayTraceValidationError),
}

/// Configuration for a replay recording lane.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayLaneConfig {
    /// Budget constraints for this lane.
    pub budget: ReplayCaptureBudget,
    /// Static metadata attached to every trace produced by this lane.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lane_metadata: BTreeMap<String, String>,
}

impl ReplayLaneConfig {
    #[must_use]
    pub const fn new(budget: ReplayCaptureBudget) -> Self {
        Self {
            budget,
            lane_metadata: BTreeMap::new(),
        }
    }

    /// Insert a metadata key-value pair into the lane config.
    pub fn insert_metadata(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.lane_metadata.insert(key.into(), value.into());
    }
}

/// Outcome of a completed replay recording session.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayLaneResult {
    /// The recorded trace bundle (present even when gated, for forensic access).
    pub bundle: ReplayTraceBundle,
    /// Budget gate report for this recording.
    pub gate_report: ReplayCaptureGateReport,
    /// Diagnostic snapshot with root-cause hints.
    pub diagnostic: ReplayDiagnosticSnapshot,
}

/// Outcome of comparing a recorded trace against a reference.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayComparisonResult {
    /// The reference bundle used for comparison.
    pub reference_trace_id: String,
    /// The observed bundle being compared.
    pub observed_trace_id: String,
    /// First divergence found, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub divergence: Option<ReplayDivergence>,
    /// Root-cause hints derived from the comparison.
    pub root_cause_hints: Vec<ReplayRootCauseHint>,
}

/// Stateful recorder that accumulates extension runtime events during a
/// dispatch cycle and produces a deterministic [`ReplayTraceBundle`].
///
/// Events are pushed in arrival order; the recorder's [`finish`](Self::finish)
/// method canonicalizes ordering, applies the budget gate, and builds the
/// diagnostic snapshot.
#[derive(Debug)]
pub struct ReplayRecorder {
    config: ReplayLaneConfig,
    builder: ReplayTraceBuilder,
    logical_clock: u64,
    event_count: u64,
}

impl ReplayRecorder {
    /// Create a new recorder for a single dispatch cycle.
    #[must_use]
    pub fn new(trace_id: impl Into<String>, config: ReplayLaneConfig) -> Self {
        let mut builder = ReplayTraceBuilder::new(trace_id);
        for (key, value) in &config.lane_metadata {
            builder.insert_metadata(key.clone(), value.clone());
        }
        Self {
            config,
            builder,
            logical_clock: 0,
            event_count: 0,
        }
    }

    /// Current logical clock value.
    #[must_use]
    pub const fn logical_clock(&self) -> u64 {
        self.logical_clock
    }

    /// Number of events recorded so far.
    #[must_use]
    pub const fn event_count(&self) -> u64 {
        self.event_count
    }

    /// Advance the logical clock and return the new value.
    pub const fn tick(&mut self) -> u64 {
        self.logical_clock = self.logical_clock.saturating_add(1);
        self.logical_clock
    }

    /// Record an event at the current logical clock.
    pub fn record(
        &mut self,
        extension_id: impl Into<String>,
        request_id: impl Into<String>,
        kind: ReplayEventKind,
        attributes: BTreeMap<String, String>,
    ) {
        let mut draft = ReplayEventDraft::new(self.logical_clock, extension_id, request_id, kind);
        draft.attributes = attributes;
        self.builder.push(draft);
        self.event_count = self.event_count.saturating_add(1);
    }

    /// Record a `Scheduled` event.
    pub fn record_scheduled(
        &mut self,
        extension_id: impl Into<String>,
        request_id: impl Into<String>,
        attributes: BTreeMap<String, String>,
    ) {
        self.record(
            extension_id,
            request_id,
            ReplayEventKind::Scheduled,
            attributes,
        );
    }

    /// Record a `QueueAccepted` event.
    pub fn record_queue_accepted(
        &mut self,
        extension_id: impl Into<String>,
        request_id: impl Into<String>,
        attributes: BTreeMap<String, String>,
    ) {
        self.record(
            extension_id,
            request_id,
            ReplayEventKind::QueueAccepted,
            attributes,
        );
    }

    /// Record a `PolicyDecision` event.
    pub fn record_policy_decision(
        &mut self,
        extension_id: impl Into<String>,
        request_id: impl Into<String>,
        attributes: BTreeMap<String, String>,
    ) {
        self.record(
            extension_id,
            request_id,
            ReplayEventKind::PolicyDecision,
            attributes,
        );
    }

    /// Record a `Cancelled` event.
    pub fn record_cancelled(
        &mut self,
        extension_id: impl Into<String>,
        request_id: impl Into<String>,
        attributes: BTreeMap<String, String>,
    ) {
        self.record(
            extension_id,
            request_id,
            ReplayEventKind::Cancelled,
            attributes,
        );
    }

    /// Record a `Retried` event.
    pub fn record_retried(
        &mut self,
        extension_id: impl Into<String>,
        request_id: impl Into<String>,
        attributes: BTreeMap<String, String>,
    ) {
        self.record(
            extension_id,
            request_id,
            ReplayEventKind::Retried,
            attributes,
        );
    }

    /// Record a `Completed` event.
    pub fn record_completed(
        &mut self,
        extension_id: impl Into<String>,
        request_id: impl Into<String>,
        attributes: BTreeMap<String, String>,
    ) {
        self.record(
            extension_id,
            request_id,
            ReplayEventKind::Completed,
            attributes,
        );
    }

    /// Record a `Failed` event.
    pub fn record_failed(
        &mut self,
        extension_id: impl Into<String>,
        request_id: impl Into<String>,
        attributes: BTreeMap<String, String>,
    ) {
        self.record(
            extension_id,
            request_id,
            ReplayEventKind::Failed,
            attributes,
        );
    }

    /// Finalize the recording: canonicalize event ordering, apply budget gate,
    /// and build the diagnostic snapshot.
    ///
    /// The `observation` provides runtime telemetry needed for budget gating.
    ///
    /// # Errors
    ///
    /// Returns an error if the trace bundle fails validation.
    pub fn finish(
        self,
        observation: ReplayCaptureObservation,
    ) -> Result<ReplayLaneResult, ReplayTraceValidationError> {
        let bundle = self.builder.build()?;
        let observation = observation_with_measured_trace_bytes(observation, &bundle);
        let gate_report = evaluate_replay_capture_gate(self.config.budget, observation);
        let diagnostic = build_replay_diagnostic_snapshot(&bundle, gate_report, None)?;

        Ok(ReplayLaneResult {
            bundle,
            gate_report,
            diagnostic,
        })
    }

    /// Finalize and compare against a reference bundle.
    ///
    /// Returns the lane result together with a comparison result that
    /// includes the first divergence (if any) and derived root-cause hints.
    ///
    /// # Errors
    ///
    /// Returns an error if either bundle fails validation.
    pub fn finish_and_compare(
        self,
        observation: ReplayCaptureObservation,
        reference: &ReplayTraceBundle,
    ) -> Result<(ReplayLaneResult, ReplayComparisonResult), ReplayTraceValidationError> {
        let bundle = self.builder.build()?;
        let observation = observation_with_measured_trace_bytes(observation, &bundle);
        let gate_report = evaluate_replay_capture_gate(self.config.budget, observation);
        let divergence_opt = first_divergence(reference, &bundle)?;
        let diagnostic =
            build_replay_diagnostic_snapshot(&bundle, gate_report, divergence_opt.as_ref())?;

        let comparison = ReplayComparisonResult {
            reference_trace_id: reference.trace_id.clone(),
            observed_trace_id: bundle.trace_id.clone(),
            divergence: divergence_opt,
            root_cause_hints: diagnostic.root_cause_hints.clone(),
        };

        let result = ReplayLaneResult {
            bundle,
            gate_report,
            diagnostic,
        };

        Ok((result, comparison))
    }
}

/// Compare two previously-recorded bundles without an active recorder.
///
/// # Errors
///
/// Returns an error if either bundle fails structural validation, or if a
/// same-schema comparison uses an unsupported schema version.
pub fn compare_replay_bundles(
    reference: &ReplayTraceBundle,
    observed: &ReplayTraceBundle,
    gate_report: ReplayCaptureGateReport,
) -> Result<(ReplayDiagnosticSnapshot, ReplayComparisonResult), ReplayTraceValidationError> {
    let divergence_opt = first_divergence(reference, observed)?;
    let diagnostic =
        build_replay_diagnostic_snapshot(observed, gate_report, divergence_opt.as_ref())?;

    let comparison = ReplayComparisonResult {
        reference_trace_id: reference.trace_id.clone(),
        observed_trace_id: observed.trace_id.clone(),
        divergence: divergence_opt,
        root_cause_hints: diagnostic.root_cause_hints.clone(),
    };

    Ok((diagnostic, comparison))
}

#[cfg(test)]
mod tests {
    use super::{
        REPLAY_TRACE_SCHEMA_V1, ReplayCaptureBudget, ReplayCaptureGateReason,
        ReplayCaptureObservation, ReplayDivergenceReason, ReplayEventDraft, ReplayEventKind,
        ReplayRootCauseHint, ReplayTraceBuilder, ReplayTraceBundle, ReplayTraceCodecError,
        ReplayTraceValidationError, build_replay_diagnostic_snapshot, evaluate_replay_capture_gate,
        first_divergence,
    };
    use std::collections::BTreeMap;

    fn draft(
        logical_clock: u64,
        extension_id: &str,
        request_id: &str,
        kind: ReplayEventKind,
    ) -> ReplayEventDraft {
        ReplayEventDraft::new(
            logical_clock,
            extension_id.to_string(),
            request_id.to_string(),
            kind,
        )
    }

    const fn standard_capture_budget() -> ReplayCaptureBudget {
        ReplayCaptureBudget {
            capture_enabled: true,
            max_overhead_per_mille: 120,
            max_trace_bytes: 8_192,
        }
    }

    fn standard_bundle() -> ReplayTraceBundle {
        let mut builder = ReplayTraceBuilder::new("trace-diagnostic");
        builder.push(draft(1, "ext.a", "req-1", ReplayEventKind::Scheduled));
        builder.push(draft(2, "ext.a", "req-1", ReplayEventKind::PolicyDecision));
        builder.push(draft(3, "ext.a", "req-1", ReplayEventKind::Completed));
        builder.build().expect("bundle should build")
    }

    #[test]
    fn deterministic_build_is_order_stable_across_input_permutations() {
        let mut left = ReplayTraceBuilder::new("trace-a");
        left.push(draft(10, "ext.alpha", "req-1", ReplayEventKind::Retried));
        left.push(draft(10, "ext.alpha", "req-1", ReplayEventKind::Cancelled));
        left.push(draft(11, "ext.alpha", "req-1", ReplayEventKind::Scheduled));
        left.push(draft(11, "ext.beta", "req-2", ReplayEventKind::Scheduled));

        let mut right = ReplayTraceBuilder::new("trace-a");
        right.push(draft(11, "ext.beta", "req-2", ReplayEventKind::Scheduled));
        right.push(draft(10, "ext.alpha", "req-1", ReplayEventKind::Cancelled));
        right.push(draft(11, "ext.alpha", "req-1", ReplayEventKind::Scheduled));
        right.push(draft(10, "ext.alpha", "req-1", ReplayEventKind::Retried));

        let left_bundle = left.build().expect("left bundle should build");
        let right_bundle = right.build().expect("right bundle should build");

        assert_eq!(left_bundle, right_bundle);
        assert_eq!(left_bundle.events[0].kind, ReplayEventKind::Cancelled);
        assert_eq!(left_bundle.events[1].kind, ReplayEventKind::Retried);
    }

    #[test]
    fn json_roundtrip_preserves_replay_bundle() {
        let mut builder = ReplayTraceBuilder::new("trace-roundtrip");
        builder.insert_metadata("lane", "shadow");
        let mut event = draft(20, "ext.gamma", "req-7", ReplayEventKind::PolicyDecision);
        event
            .attributes
            .insert("decision".to_string(), "fast_lane".to_string());
        builder.push(draft(19, "ext.gamma", "req-7", ReplayEventKind::Scheduled));
        builder.push(event);
        builder.push(draft(21, "ext.gamma", "req-7", ReplayEventKind::Completed));

        let bundle = builder.build().expect("bundle should build");
        let encoded = bundle.encode_json().expect("bundle should encode");
        let decoded = ReplayTraceBundle::decode_json(&encoded).expect("bundle should decode");
        assert_eq!(decoded, bundle);
    }

    #[test]
    fn decode_rejects_retry_without_prior_cancel() {
        let bundle = ReplayTraceBundle {
            schema: REPLAY_TRACE_SCHEMA_V1.to_string(),
            trace_id: "trace-invalid".to_string(),
            metadata: BTreeMap::new(),
            events: vec![super::ReplayTraceEvent {
                seq: 1,
                logical_clock: 1,
                extension_id: "ext.a".to_string(),
                request_id: "req".to_string(),
                kind: ReplayEventKind::Retried,
                attributes: BTreeMap::new(),
            }],
        };
        let encoded = bundle
            .encode_json()
            .expect("invalid bundle should serialize");

        let error = ReplayTraceBundle::decode_json(&encoded).expect_err("retry without cancel");
        match error {
            ReplayTraceCodecError::Validation(ReplayTraceValidationError::RetryWithoutCancel {
                ..
            }) => {}
            other => unreachable!("expected RetryWithoutCancel, got: {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_non_contiguous_sequence() {
        let bundle = ReplayTraceBundle {
            schema: REPLAY_TRACE_SCHEMA_V1.to_string(),
            trace_id: "trace-seq".to_string(),
            metadata: BTreeMap::new(),
            events: vec![
                super::ReplayTraceEvent {
                    seq: 1,
                    logical_clock: 1,
                    extension_id: "ext.a".to_string(),
                    request_id: "req-1".to_string(),
                    kind: ReplayEventKind::Scheduled,
                    attributes: BTreeMap::new(),
                },
                super::ReplayTraceEvent {
                    seq: 3,
                    logical_clock: 2,
                    extension_id: "ext.a".to_string(),
                    request_id: "req-1".to_string(),
                    kind: ReplayEventKind::Completed,
                    attributes: BTreeMap::new(),
                },
            ],
        };
        let encoded = bundle
            .encode_json()
            .expect("invalid bundle should serialize");

        let error = ReplayTraceBundle::decode_json(&encoded).expect_err("non-contiguous seq");
        match error {
            ReplayTraceCodecError::Validation(
                ReplayTraceValidationError::NonContiguousSequence { expected, observed },
            ) => {
                assert_eq!(expected, 2);
                assert_eq!(observed, 3);
            }
            other => unreachable!("expected NonContiguousSequence, got: {other:?}"),
        }
    }

    #[test]
    fn divergence_reports_kind_mismatch_with_seq() {
        let mut expected_builder = ReplayTraceBuilder::new("trace-divergence");
        expected_builder.push(draft(1, "ext.a", "req-1", ReplayEventKind::Scheduled));
        expected_builder.push(draft(2, "ext.a", "req-1", ReplayEventKind::Completed));
        let expected = expected_builder.build().expect("expected bundle");

        let mut observed_builder = ReplayTraceBuilder::new("trace-divergence");
        observed_builder.push(draft(1, "ext.a", "req-1", ReplayEventKind::Scheduled));
        observed_builder.push(draft(2, "ext.a", "req-1", ReplayEventKind::Failed));
        let observed = observed_builder.build().expect("observed bundle");

        let divergence = first_divergence(&expected, &observed)
            .expect("comparison should succeed")
            .expect("divergence expected");
        assert_eq!(divergence.seq, Some(2));
        match divergence.reason {
            ReplayDivergenceReason::EventFieldMismatch { field, .. } => {
                assert_eq!(field, "kind");
            }
            other => unreachable!("expected EventFieldMismatch, got: {other:?}"),
        }
    }

    #[test]
    fn divergence_reports_event_count_mismatch() {
        let mut expected_builder = ReplayTraceBuilder::new("trace-length");
        expected_builder.push(draft(1, "ext.a", "req-1", ReplayEventKind::Scheduled));
        expected_builder.push(draft(2, "ext.a", "req-1", ReplayEventKind::Completed));
        let expected = expected_builder.build().expect("expected bundle");

        let mut observed_builder = ReplayTraceBuilder::new("trace-length");
        observed_builder.push(draft(1, "ext.a", "req-1", ReplayEventKind::Scheduled));
        let observed = observed_builder.build().expect("observed bundle");

        let divergence = first_divergence(&expected, &observed)
            .expect("comparison should succeed")
            .expect("divergence expected");
        assert_eq!(divergence.seq, Some(2));
        match divergence.reason {
            ReplayDivergenceReason::EventCountMismatch { expected, observed } => {
                assert_eq!(expected, 2);
                assert_eq!(observed, 1);
            }
            other => unreachable!("expected EventCountMismatch, got: {other:?}"),
        }
    }

    #[test]
    fn divergence_returns_none_for_identical_bundles() {
        let mut builder = ReplayTraceBuilder::new("trace-identical");
        builder.push(draft(1, "ext.a", "req-1", ReplayEventKind::Scheduled));
        builder.push(draft(2, "ext.a", "req-1", ReplayEventKind::Completed));
        let bundle = builder.build().expect("bundle");

        let divergence =
            first_divergence(&bundle, &bundle).expect("comparison should validate identical");
        assert!(divergence.is_none());
    }

    #[test]
    fn capture_gate_disables_when_config_switch_is_off() {
        let mut budget = standard_capture_budget();
        budget.capture_enabled = false;
        let observation = ReplayCaptureObservation {
            baseline_micros: 1_000,
            captured_micros: 1_010,
            trace_bytes: 128,
        };

        let report = evaluate_replay_capture_gate(budget, observation);
        assert!(!report.capture_allowed);
        assert_eq!(report.reason, ReplayCaptureGateReason::DisabledByConfig);
    }

    #[test]
    fn capture_gate_disables_when_overhead_exceeds_budget() {
        let budget = standard_capture_budget();
        let observation = ReplayCaptureObservation {
            baseline_micros: 1_000,
            captured_micros: 1_140,
            trace_bytes: 512,
        };

        let report = evaluate_replay_capture_gate(budget, observation);
        assert!(!report.capture_allowed);
        assert_eq!(
            report.reason,
            ReplayCaptureGateReason::DisabledByOverheadBudget
        );
        assert_eq!(report.observed_overhead_per_mille, 140);
    }

    #[test]
    fn capture_gate_disables_when_trace_budget_exceeded() {
        let budget = standard_capture_budget();
        let observation = ReplayCaptureObservation {
            baseline_micros: 1_000,
            captured_micros: 1_050,
            trace_bytes: 9_000,
        };

        let report = evaluate_replay_capture_gate(budget, observation);
        assert!(!report.capture_allowed);
        assert_eq!(
            report.reason,
            ReplayCaptureGateReason::DisabledByTraceBudget
        );
        assert_eq!(report.observed_overhead_per_mille, 50);
    }

    #[test]
    fn capture_gate_fails_closed_on_invalid_baseline() {
        let budget = standard_capture_budget();
        let observation = ReplayCaptureObservation {
            baseline_micros: 0,
            captured_micros: 1,
            trace_bytes: 64,
        };

        let report = evaluate_replay_capture_gate(budget, observation);
        assert!(!report.capture_allowed);
        assert_eq!(
            report.reason,
            ReplayCaptureGateReason::DisabledByInvalidBaseline
        );
        assert_eq!(report.observed_overhead_per_mille, u32::MAX);
    }

    #[test]
    fn capture_gate_fails_closed_when_zero_baseline_reports_zero_capture_cost() {
        let budget = standard_capture_budget();
        let observation = ReplayCaptureObservation {
            baseline_micros: 0,
            captured_micros: 0,
            trace_bytes: 64,
        };

        let report = evaluate_replay_capture_gate(budget, observation);
        assert!(!report.capture_allowed);
        assert_eq!(
            report.reason,
            ReplayCaptureGateReason::DisabledByInvalidBaseline
        );
        assert_eq!(report.observed_overhead_per_mille, u32::MAX);
    }

    #[test]
    fn capture_gate_reports_deterministic_within_budget_enablement() {
        let budget = standard_capture_budget();
        let observation = ReplayCaptureObservation {
            baseline_micros: 1_000,
            captured_micros: 1_080,
            trace_bytes: 4_096,
        };

        let first = evaluate_replay_capture_gate(budget, observation);
        let second = evaluate_replay_capture_gate(budget, observation);

        assert_eq!(first, second);
        assert!(first.capture_allowed);
        assert_eq!(first.reason, ReplayCaptureGateReason::Enabled);
        assert_eq!(first.observed_overhead_per_mille, 80);
    }

    #[test]
    fn diagnostic_snapshot_emits_hint_codes_for_gate_and_payload_drift() {
        let expected = standard_bundle();

        let mut observed_builder = ReplayTraceBuilder::new("trace-diagnostic");
        observed_builder.push(draft(1, "ext.a", "req-1", ReplayEventKind::Scheduled));
        observed_builder.push(draft(2, "ext.a", "req-1", ReplayEventKind::PolicyDecision));
        observed_builder.push(draft(3, "ext.a", "req-1", ReplayEventKind::Failed));
        let observed = observed_builder.build().expect("observed bundle");

        let divergence = first_divergence(&expected, &observed)
            .expect("comparison should succeed")
            .expect("divergence expected");
        let capture_gate = evaluate_replay_capture_gate(
            standard_capture_budget(),
            ReplayCaptureObservation {
                baseline_micros: 1_000,
                captured_micros: 1_150,
                trace_bytes: 64,
            },
        );

        let snapshot = build_replay_diagnostic_snapshot(&expected, capture_gate, Some(&divergence))
            .expect("snapshot should build");
        assert_eq!(snapshot.event_count, 3);
        assert_eq!(
            snapshot.root_cause_hints,
            vec![
                ReplayRootCauseHint::EventPayloadDrift,
                ReplayRootCauseHint::OverheadBudgetExceeded,
            ]
        );
    }

    #[test]
    fn diagnostic_snapshot_maps_logical_clock_drift_hint() {
        let expected = standard_bundle();
        let mut observed = expected.clone();
        observed.events[1].logical_clock = 77;

        let divergence = first_divergence(&expected, &observed)
            .expect("comparison should succeed")
            .expect("divergence expected");
        let capture_gate = evaluate_replay_capture_gate(
            standard_capture_budget(),
            ReplayCaptureObservation {
                baseline_micros: 1_000,
                captured_micros: 1_010,
                trace_bytes: 64,
            },
        );

        let snapshot = build_replay_diagnostic_snapshot(&expected, capture_gate, Some(&divergence))
            .expect("snapshot should build");
        assert_eq!(
            snapshot.root_cause_hints,
            vec![ReplayRootCauseHint::LogicalClockDrift]
        );
    }

    #[test]
    fn diagnostic_snapshot_is_deterministic_for_same_inputs() {
        let bundle = standard_bundle();
        let capture_gate = evaluate_replay_capture_gate(
            standard_capture_budget(),
            ReplayCaptureObservation {
                baseline_micros: 1_000,
                captured_micros: 1_020,
                trace_bytes: 256,
            },
        );

        let first =
            build_replay_diagnostic_snapshot(&bundle, capture_gate, None).expect("first snapshot");
        let second =
            build_replay_diagnostic_snapshot(&bundle, capture_gate, None).expect("second snapshot");
        assert_eq!(first, second);
    }

    #[test]
    fn diagnostic_snapshot_rejects_invalid_bundle() {
        let invalid = ReplayTraceBundle {
            schema: "invalid.schema".to_string(),
            trace_id: "trace-bad".to_string(),
            metadata: BTreeMap::new(),
            events: Vec::new(),
        };
        let capture_gate = evaluate_replay_capture_gate(
            standard_capture_budget(),
            ReplayCaptureObservation {
                baseline_micros: 1_000,
                captured_micros: 1_000,
                trace_bytes: 0,
            },
        );

        let error = build_replay_diagnostic_snapshot(&invalid, capture_gate, None)
            .expect_err("invalid schema should fail");
        assert!(matches!(
            error,
            ReplayTraceValidationError::UnknownSchema(_)
        ));
    }

    // ── Builder edge cases ──

    #[test]
    fn builder_empty_events_produces_valid_bundle() {
        let builder = ReplayTraceBuilder::new("trace-empty");
        let bundle = builder.build().expect("empty bundle should be valid");
        assert!(bundle.events.is_empty());
        assert_eq!(bundle.schema, REPLAY_TRACE_SCHEMA_V1);
        assert_eq!(bundle.trace_id, "trace-empty");
    }

    #[test]
    fn builder_metadata_preserved_in_output() {
        let mut builder = ReplayTraceBuilder::new("trace-meta");
        builder.insert_metadata("env", "production");
        builder.insert_metadata("version", "1.2.3");
        builder.push(draft(1, "ext.a", "req-1", ReplayEventKind::Scheduled));
        let bundle = builder.build().expect("bundle with metadata");
        assert_eq!(
            bundle.metadata.get("env").map(String::as_str),
            Some("production")
        );
        assert_eq!(
            bundle.metadata.get("version").map(String::as_str),
            Some("1.2.3")
        );
    }

    #[test]
    fn builder_metadata_overwrite_works() {
        let mut builder = ReplayTraceBuilder::new("trace-meta-ow");
        builder.insert_metadata("key", "old");
        builder.insert_metadata("key", "new");
        builder.push(draft(1, "ext.a", "req-1", ReplayEventKind::Scheduled));
        let bundle = builder.build().expect("metadata overwrite");
        assert_eq!(bundle.metadata.get("key").map(String::as_str), Some("new"));
    }

    #[test]
    fn draft_attributes_carried_through_build() {
        let mut d = draft(1, "ext.a", "req-1", ReplayEventKind::PolicyDecision);
        d.attributes
            .insert("policy".to_string(), "fast_lane".to_string());
        d.attributes
            .insert("latency_ms".to_string(), "12".to_string());
        let mut builder = ReplayTraceBuilder::new("trace-attrs");
        builder.push(d);
        let bundle = builder.build().expect("bundle with attrs");
        assert_eq!(bundle.events[0].attributes.len(), 2);
        assert_eq!(
            bundle.events[0]
                .attributes
                .get("policy")
                .map(String::as_str),
            Some("fast_lane")
        );
    }

    // ── Validation error paths ──

    #[test]
    fn validate_rejects_empty_trace_id() {
        let mut builder = ReplayTraceBuilder::new("");
        builder.push(draft(1, "ext.a", "req-1", ReplayEventKind::Scheduled));
        let err = builder.build().expect_err("empty trace_id should fail");
        assert!(matches!(err, ReplayTraceValidationError::EmptyTraceId));
    }

    #[test]
    fn validate_rejects_whitespace_only_trace_id() {
        let mut builder = ReplayTraceBuilder::new("   ");
        builder.push(draft(1, "ext.a", "req-1", ReplayEventKind::Scheduled));
        let err = builder
            .build()
            .expect_err("whitespace trace_id should fail");
        assert!(matches!(err, ReplayTraceValidationError::EmptyTraceId));
    }

    #[test]
    fn validate_rejects_empty_extension_id() {
        let mut builder = ReplayTraceBuilder::new("trace-val");
        builder.push(draft(1, "", "req-1", ReplayEventKind::Scheduled));
        let err = builder.build().expect_err("empty extension_id should fail");
        assert!(matches!(
            err,
            ReplayTraceValidationError::MissingExtensionId { .. }
        ));
    }

    #[test]
    fn validate_rejects_empty_request_id() {
        let mut builder = ReplayTraceBuilder::new("trace-val");
        builder.push(draft(1, "ext.a", "", ReplayEventKind::Scheduled));
        let err = builder.build().expect_err("empty request_id should fail");
        assert!(matches!(
            err,
            ReplayTraceValidationError::MissingRequestId { .. }
        ));
    }

    #[test]
    fn validate_rejects_duplicate_cancel_without_retry() {
        let mut builder = ReplayTraceBuilder::new("trace-dup-cancel");
        builder.push(draft(1, "ext.a", "req-1", ReplayEventKind::Cancelled));
        builder.push(draft(2, "ext.a", "req-1", ReplayEventKind::Cancelled));
        let err = builder.build().expect_err("duplicate cancel should fail");
        assert!(matches!(
            err,
            ReplayTraceValidationError::DuplicateCancelWithoutRetry { .. }
        ));
    }

    #[test]
    fn cancel_then_retry_then_cancel_is_valid() {
        let mut builder = ReplayTraceBuilder::new("trace-cancel-retry-cancel");
        builder.push(draft(1, "ext.a", "req-1", ReplayEventKind::Cancelled));
        builder.push(draft(2, "ext.a", "req-1", ReplayEventKind::Retried));
        builder.push(draft(3, "ext.a", "req-1", ReplayEventKind::Cancelled));
        let bundle = builder
            .build()
            .expect("cancel-retry-cancel should be valid");
        assert_eq!(bundle.events.len(), 3);
    }

    #[test]
    fn completed_clears_pending_cancel() {
        let mut builder = ReplayTraceBuilder::new("trace-complete-clear");
        builder.push(draft(1, "ext.a", "req-1", ReplayEventKind::Cancelled));
        builder.push(draft(2, "ext.a", "req-1", ReplayEventKind::Completed));
        builder.push(draft(3, "ext.a", "req-1", ReplayEventKind::Cancelled));
        let bundle = builder
            .build()
            .expect("completed should clear cancel state");
        assert_eq!(bundle.events.len(), 3);
    }

    // ── ReplayEventKind ordering ──

    #[test]
    fn event_kind_canonical_rank_is_monotonic() {
        let kinds = [
            ReplayEventKind::Scheduled,
            ReplayEventKind::QueueAccepted,
            ReplayEventKind::PolicyDecision,
            ReplayEventKind::Cancelled,
            ReplayEventKind::Retried,
            ReplayEventKind::Completed,
            ReplayEventKind::Failed,
        ];
        for pair in kinds.windows(2) {
            assert!(
                pair[0].canonical_rank() < pair[1].canonical_rank(),
                "{:?} should have lower rank than {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn event_kind_serde_roundtrip() {
        let kinds = [
            ReplayEventKind::Scheduled,
            ReplayEventKind::QueueAccepted,
            ReplayEventKind::PolicyDecision,
            ReplayEventKind::Cancelled,
            ReplayEventKind::Retried,
            ReplayEventKind::Completed,
            ReplayEventKind::Failed,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).expect("serialize kind");
            let roundtrip: ReplayEventKind = serde_json::from_str(&json).expect("deserialize kind");
            assert_eq!(kind, roundtrip);
        }
    }

    // ── Divergence edge cases ──

    #[test]
    fn divergence_detects_schema_mismatch() {
        let mut observed = standard_bundle();
        observed.schema = "pi.ext.replay.trace.v2".to_string();

        let divergence = first_divergence(&standard_bundle(), &observed)
            .expect("comparison should succeed")
            .expect("schema mismatch expected");

        assert_eq!(divergence.seq, None);
        match divergence.reason {
            ReplayDivergenceReason::SchemaMismatch { expected, observed } => {
                assert_eq!(expected, REPLAY_TRACE_SCHEMA_V1);
                assert_eq!(observed, "pi.ext.replay.trace.v2");
            }
            other => unreachable!("expected SchemaMismatch, got: {other:?}"),
        }
    }

    #[test]
    fn divergence_rejects_same_unknown_schema() {
        let mut expected = standard_bundle();
        expected.schema = "pi.ext.replay.trace.v2".to_string();
        let observed = expected.clone();

        let error = first_divergence(&expected, &observed).expect_err("unsupported schema");
        assert!(matches!(
            error,
            ReplayTraceValidationError::UnknownSchema(schema) if schema == "pi.ext.replay.trace.v2"
        ));
    }

    #[test]
    fn divergence_detects_attribute_mismatch() {
        let mut builder_a = ReplayTraceBuilder::new("trace-attrs-cmp");
        let mut d1 = draft(1, "ext.a", "req-1", ReplayEventKind::PolicyDecision);
        d1.attributes
            .insert("decision".to_string(), "fast".to_string());
        builder_a.push(d1);
        let expected = builder_a.build().expect("bundle a");

        let mut builder_b = ReplayTraceBuilder::new("trace-attrs-cmp");
        let mut d2 = draft(1, "ext.a", "req-1", ReplayEventKind::PolicyDecision);
        d2.attributes
            .insert("decision".to_string(), "slow".to_string());
        builder_b.push(d2);
        let observed = builder_b.build().expect("bundle b");

        let divergence = first_divergence(&expected, &observed)
            .expect("comparison should succeed")
            .expect("attribute mismatch expected");
        assert_eq!(divergence.seq, Some(1));
        match divergence.reason {
            ReplayDivergenceReason::EventFieldMismatch { field, .. } => {
                assert_eq!(field, "attributes");
            }
            other => unreachable!("expected EventFieldMismatch for attributes, got: {other:?}"),
        }
    }

    // ── Capture gate boundary cases ──

    #[test]
    fn capture_gate_zero_overhead_when_captured_equals_baseline() {
        let budget = standard_capture_budget();
        let observation = ReplayCaptureObservation {
            baseline_micros: 1_000,
            captured_micros: 1_000,
            trace_bytes: 100,
        };
        let report = evaluate_replay_capture_gate(budget, observation);
        assert!(report.capture_allowed);
        assert_eq!(report.observed_overhead_per_mille, 0);
    }

    #[test]
    fn capture_gate_zero_overhead_when_captured_less_than_baseline() {
        let budget = standard_capture_budget();
        let observation = ReplayCaptureObservation {
            baseline_micros: 1_000,
            captured_micros: 900,
            trace_bytes: 100,
        };
        let report = evaluate_replay_capture_gate(budget, observation);
        assert!(report.capture_allowed);
        assert_eq!(report.observed_overhead_per_mille, 0);
    }

    #[test]
    fn capture_gate_exact_boundary_at_max_overhead() {
        let budget = ReplayCaptureBudget {
            capture_enabled: true,
            max_overhead_per_mille: 100,
            max_trace_bytes: 10_000,
        };
        // 100/1000 = 100 per mille — exactly at budget
        let observation = ReplayCaptureObservation {
            baseline_micros: 1_000,
            captured_micros: 1_100,
            trace_bytes: 100,
        };
        let report = evaluate_replay_capture_gate(budget, observation);
        assert!(report.capture_allowed);
        assert_eq!(report.observed_overhead_per_mille, 100);
    }

    #[test]
    fn capture_gate_exact_boundary_at_max_trace_bytes() {
        let budget = ReplayCaptureBudget {
            capture_enabled: true,
            max_overhead_per_mille: 1_000,
            max_trace_bytes: 500,
        };
        // Exactly at budget
        let at_limit = ReplayCaptureObservation {
            baseline_micros: 1_000,
            captured_micros: 1_010,
            trace_bytes: 500,
        };
        let report = evaluate_replay_capture_gate(budget, at_limit);
        assert!(report.capture_allowed);

        // One over budget
        let over_limit = ReplayCaptureObservation {
            baseline_micros: 1_000,
            captured_micros: 1_010,
            trace_bytes: 501,
        };
        let report = evaluate_replay_capture_gate(budget, over_limit);
        assert!(!report.capture_allowed);
        assert_eq!(
            report.reason,
            ReplayCaptureGateReason::DisabledByTraceBudget
        );
    }

    // ── Diagnostic snapshot root cause hints ──

    #[test]
    fn diagnostic_snapshot_maps_config_disabled_hint() {
        let bundle = standard_bundle();
        let budget = ReplayCaptureBudget {
            capture_enabled: false,
            max_overhead_per_mille: 100,
            max_trace_bytes: 1_000,
        };
        let gate = evaluate_replay_capture_gate(
            budget,
            ReplayCaptureObservation {
                baseline_micros: 100,
                captured_micros: 100,
                trace_bytes: 0,
            },
        );
        let snapshot = build_replay_diagnostic_snapshot(&bundle, gate, None).expect("snapshot");
        assert_eq!(
            snapshot.root_cause_hints,
            vec![ReplayRootCauseHint::PolicyGateDisabled]
        );
    }

    #[test]
    fn diagnostic_snapshot_maps_trace_budget_hint() {
        let bundle = standard_bundle();
        let budget = ReplayCaptureBudget {
            capture_enabled: true,
            max_overhead_per_mille: 1_000,
            max_trace_bytes: 100,
        };
        let gate = evaluate_replay_capture_gate(
            budget,
            ReplayCaptureObservation {
                baseline_micros: 1_000,
                captured_micros: 1_010,
                trace_bytes: 200,
            },
        );
        let snapshot = build_replay_diagnostic_snapshot(&bundle, gate, None).expect("snapshot");
        assert_eq!(
            snapshot.root_cause_hints,
            vec![ReplayRootCauseHint::TraceBudgetExceeded]
        );
    }

    #[test]
    fn diagnostic_snapshot_serde_roundtrip() {
        let bundle = standard_bundle();
        let gate = evaluate_replay_capture_gate(
            standard_capture_budget(),
            ReplayCaptureObservation {
                baseline_micros: 1_000,
                captured_micros: 1_010,
                trace_bytes: 64,
            },
        );
        let snapshot = build_replay_diagnostic_snapshot(&bundle, gate, None).expect("snapshot");
        let json = serde_json::to_string(&snapshot).expect("serialize");
        let roundtrip: super::ReplayDiagnosticSnapshot =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(snapshot, roundtrip);
    }

    // ── compute_overhead_per_mille edge cases ──

    #[test]
    fn overhead_per_mille_exact_computation() {
        // 50 overhead on 1000 baseline = 50 per mille
        assert_eq!(super::compute_overhead_per_mille(1_000, 1_050), 50);
        // 200 overhead on 1000 baseline = 200 per mille
        assert_eq!(super::compute_overhead_per_mille(1_000, 1_200), 200);
        // 0 overhead
        assert_eq!(super::compute_overhead_per_mille(1_000, 1_000), 0);
        // Captured < baseline
        assert_eq!(super::compute_overhead_per_mille(1_000, 500), 0);
    }

    #[test]
    fn overhead_per_mille_rounding_up() {
        // 1 overhead on 3 baseline = 333.3... per mille → rounds up to 334
        assert_eq!(super::compute_overhead_per_mille(3, 4), 334);
    }

    #[test]
    fn overhead_per_mille_zero_baseline_returns_max() {
        assert_eq!(super::compute_overhead_per_mille(0, 1), u32::MAX);
        assert_eq!(super::compute_overhead_per_mille(0, 0), u32::MAX);
    }

    // ── ReplayRecorder tests ──

    fn within_budget_observation() -> ReplayCaptureObservation {
        ReplayCaptureObservation {
            baseline_micros: 1_000,
            captured_micros: 1_050,
            trace_bytes: 256,
        }
    }

    fn standard_lane_config() -> super::ReplayLaneConfig {
        super::ReplayLaneConfig::new(standard_capture_budget())
    }

    #[test]
    fn recorder_empty_produces_valid_bundle() {
        let recorder = super::ReplayRecorder::new("trace-empty-rec", standard_lane_config());
        assert_eq!(recorder.event_count(), 0);
        assert_eq!(recorder.logical_clock(), 0);

        let result = recorder
            .finish(within_budget_observation())
            .expect("finish");
        assert!(result.bundle.events.is_empty());
        assert!(result.gate_report.capture_allowed);
        assert_eq!(result.diagnostic.event_count, 0);
    }

    #[test]
    fn recorder_captures_events_in_sequence() {
        let mut recorder = super::ReplayRecorder::new("trace-seq-rec", standard_lane_config());
        recorder.tick();
        recorder.record_scheduled("ext.a", "req-1", BTreeMap::new());
        recorder.tick();
        recorder.record_queue_accepted("ext.a", "req-1", BTreeMap::new());
        recorder.tick();
        recorder.record_policy_decision("ext.a", "req-1", BTreeMap::new());
        recorder.tick();
        recorder.record_completed("ext.a", "req-1", BTreeMap::new());

        assert_eq!(recorder.event_count(), 4);
        assert_eq!(recorder.logical_clock(), 4);

        let result = recorder
            .finish(within_budget_observation())
            .expect("finish");
        assert_eq!(result.bundle.events.len(), 4);
        assert_eq!(result.bundle.events[0].kind, ReplayEventKind::Scheduled);
        assert_eq!(result.bundle.events[1].kind, ReplayEventKind::QueueAccepted);
        assert_eq!(
            result.bundle.events[2].kind,
            ReplayEventKind::PolicyDecision
        );
        assert_eq!(result.bundle.events[3].kind, ReplayEventKind::Completed);

        // Sequences are 1-based contiguous
        for (i, event) in result.bundle.events.iter().enumerate() {
            assert_eq!(event.seq, (i + 1) as u64);
        }
    }

    #[test]
    fn recorder_attributes_flow_through() {
        let mut recorder = super::ReplayRecorder::new("trace-attrs-rec", standard_lane_config());
        recorder.tick();
        let mut attrs = BTreeMap::new();
        attrs.insert("lane".to_string(), "fast".to_string());
        attrs.insert("capability".to_string(), "tool".to_string());
        recorder.record_policy_decision("ext.a", "req-1", attrs);

        let result = recorder
            .finish(within_budget_observation())
            .expect("finish");
        let event = &result.bundle.events[0];
        assert_eq!(
            event.attributes.get("lane").map(String::as_str),
            Some("fast")
        );
        assert_eq!(
            event.attributes.get("capability").map(String::as_str),
            Some("tool")
        );
    }

    #[test]
    fn recorder_lane_metadata_propagated() {
        let mut config = standard_lane_config();
        config.insert_metadata("env", "staging");
        config.insert_metadata("worker", "w-3");
        let mut recorder = super::ReplayRecorder::new("trace-meta-rec", config);
        recorder.tick();
        recorder.record_scheduled("ext.a", "req-1", BTreeMap::new());

        let result = recorder
            .finish(within_budget_observation())
            .expect("finish");
        assert_eq!(
            result.bundle.metadata.get("env").map(String::as_str),
            Some("staging")
        );
        assert_eq!(
            result.bundle.metadata.get("worker").map(String::as_str),
            Some("w-3")
        );
    }

    #[test]
    fn recorder_cancel_retry_lifecycle() {
        let mut recorder = super::ReplayRecorder::new("trace-cancel-retry", standard_lane_config());
        recorder.tick();
        recorder.record_scheduled("ext.a", "req-1", BTreeMap::new());
        recorder.tick();
        recorder.record_cancelled("ext.a", "req-1", BTreeMap::new());
        recorder.tick();
        recorder.record_retried("ext.a", "req-1", BTreeMap::new());
        recorder.tick();
        recorder.record_completed("ext.a", "req-1", BTreeMap::new());

        let result = recorder
            .finish(within_budget_observation())
            .expect("finish");
        assert_eq!(result.bundle.events.len(), 4);
        assert_eq!(result.bundle.events[1].kind, ReplayEventKind::Cancelled);
        assert_eq!(result.bundle.events[2].kind, ReplayEventKind::Retried);
    }

    #[test]
    fn recorder_failed_event() {
        let mut recorder = super::ReplayRecorder::new("trace-fail", standard_lane_config());
        recorder.tick();
        recorder.record_scheduled("ext.a", "req-1", BTreeMap::new());
        recorder.tick();
        let mut attrs = BTreeMap::new();
        attrs.insert("error".to_string(), "timeout".to_string());
        recorder.record_failed("ext.a", "req-1", attrs);

        let result = recorder
            .finish(within_budget_observation())
            .expect("finish");
        assert_eq!(result.bundle.events[1].kind, ReplayEventKind::Failed);
        assert_eq!(
            result.bundle.events[1]
                .attributes
                .get("error")
                .map(String::as_str),
            Some("timeout")
        );
    }

    #[test]
    fn recorder_gate_report_reflects_budget() {
        let mut config = super::ReplayLaneConfig::new(ReplayCaptureBudget {
            capture_enabled: true,
            max_overhead_per_mille: 50,
            max_trace_bytes: 10_000,
        });
        config.insert_metadata("lane", "shadow");
        let mut recorder = super::ReplayRecorder::new("trace-gated", config);
        recorder.tick();
        recorder.record_scheduled("ext.a", "req-1", BTreeMap::new());

        // Overhead 100 per mille > budget 50 per mille
        let result = recorder
            .finish(ReplayCaptureObservation {
                baseline_micros: 1_000,
                captured_micros: 1_100,
                trace_bytes: 64,
            })
            .expect("finish");

        assert!(!result.gate_report.capture_allowed);
        assert_eq!(
            result.gate_report.reason,
            ReplayCaptureGateReason::DisabledByOverheadBudget
        );
        // Bundle is still present even when gated
        assert_eq!(result.bundle.events.len(), 1);
    }

    #[test]
    fn recorder_measures_trace_bytes_when_observation_omits_size() {
        let mut config = super::ReplayLaneConfig::new(ReplayCaptureBudget {
            capture_enabled: true,
            max_overhead_per_mille: 1_000,
            max_trace_bytes: 1,
        });
        config.insert_metadata("lane", "shadow");
        let mut recorder = super::ReplayRecorder::new("trace-sized", config);
        recorder.tick();
        recorder.record_scheduled("ext.a", "req-1", BTreeMap::new());

        let result = recorder
            .finish(ReplayCaptureObservation {
                baseline_micros: 1_000,
                captured_micros: 1_000,
                trace_bytes: 0,
            })
            .expect("finish");

        assert!(!result.gate_report.capture_allowed);
        assert_eq!(
            result.gate_report.reason,
            ReplayCaptureGateReason::DisabledByTraceBudget
        );
        assert!(result.gate_report.observed_trace_bytes > 1);
    }

    #[test]
    fn recorder_diagnostic_snapshot_populated() {
        let mut recorder = super::ReplayRecorder::new("trace-diag", standard_lane_config());
        recorder.tick();
        recorder.record_scheduled("ext.a", "req-1", BTreeMap::new());
        recorder.tick();
        recorder.record_completed("ext.a", "req-1", BTreeMap::new());

        let result = recorder
            .finish(within_budget_observation())
            .expect("finish");
        assert_eq!(result.diagnostic.trace_id, "trace-diag");
        assert_eq!(result.diagnostic.schema, REPLAY_TRACE_SCHEMA_V1);
        assert_eq!(result.diagnostic.event_count, 2);
        assert!(result.diagnostic.divergence.is_none());
        assert!(result.diagnostic.root_cause_hints.is_empty());
    }

    #[test]
    fn recorder_finish_and_compare_identical() {
        let mut rec1 = super::ReplayRecorder::new("trace-cmp", standard_lane_config());
        rec1.tick();
        rec1.record_scheduled("ext.a", "req-1", BTreeMap::new());
        rec1.tick();
        rec1.record_completed("ext.a", "req-1", BTreeMap::new());
        let reference = rec1
            .finish(within_budget_observation())
            .expect("ref")
            .bundle;

        let mut rec2 = super::ReplayRecorder::new("trace-cmp", standard_lane_config());
        rec2.tick();
        rec2.record_scheduled("ext.a", "req-1", BTreeMap::new());
        rec2.tick();
        rec2.record_completed("ext.a", "req-1", BTreeMap::new());

        let (result, comparison) = rec2
            .finish_and_compare(within_budget_observation(), &reference)
            .expect("compare");
        assert!(comparison.divergence.is_none());
        assert!(comparison.root_cause_hints.is_empty());
        assert_eq!(comparison.reference_trace_id, "trace-cmp");
        assert_eq!(comparison.observed_trace_id, "trace-cmp");
        assert!(result.diagnostic.divergence.is_none());
    }

    #[test]
    fn recorder_finish_and_compare_detects_divergence() {
        let mut rec1 = super::ReplayRecorder::new("trace-div", standard_lane_config());
        rec1.tick();
        rec1.record_scheduled("ext.a", "req-1", BTreeMap::new());
        rec1.tick();
        rec1.record_completed("ext.a", "req-1", BTreeMap::new());
        let reference = rec1
            .finish(within_budget_observation())
            .expect("ref")
            .bundle;

        let mut rec2 = super::ReplayRecorder::new("trace-div", standard_lane_config());
        rec2.tick();
        rec2.record_scheduled("ext.a", "req-1", BTreeMap::new());
        rec2.tick();
        rec2.record_failed("ext.a", "req-1", BTreeMap::new());

        let (result, comparison) = rec2
            .finish_and_compare(within_budget_observation(), &reference)
            .expect("compare");
        assert!(comparison.divergence.is_some());
        let div = comparison.divergence.as_ref().unwrap();
        assert_eq!(div.seq, Some(2));
        assert!(matches!(
            div.reason,
            ReplayDivergenceReason::EventFieldMismatch { ref field, .. } if field == "kind"
        ));
        assert!(result.diagnostic.divergence.is_some());
        assert!(!result.diagnostic.root_cause_hints.is_empty());
    }

    #[test]
    fn recorder_multi_extension_interleaving() {
        let mut recorder = super::ReplayRecorder::new("trace-multi", standard_lane_config());
        recorder.tick();
        recorder.record_scheduled("ext.a", "req-1", BTreeMap::new());
        recorder.record_scheduled("ext.b", "req-2", BTreeMap::new());
        recorder.tick();
        recorder.record_policy_decision("ext.a", "req-1", BTreeMap::new());
        recorder.record_policy_decision("ext.b", "req-2", BTreeMap::new());
        recorder.tick();
        recorder.record_completed("ext.a", "req-1", BTreeMap::new());
        recorder.record_completed("ext.b", "req-2", BTreeMap::new());

        let result = recorder
            .finish(within_budget_observation())
            .expect("finish");
        assert_eq!(result.bundle.events.len(), 6);

        // Canonical ordering: at same clock, ext.a < ext.b
        let clock_1_events: Vec<_> = result
            .bundle
            .events
            .iter()
            .filter(|e| e.logical_clock == 1)
            .collect();
        assert_eq!(clock_1_events.len(), 2);
        assert_eq!(clock_1_events[0].extension_id, "ext.a");
        assert_eq!(clock_1_events[1].extension_id, "ext.b");
    }

    // ── compare_replay_bundles standalone tests ──

    #[test]
    fn compare_replay_bundles_no_divergence() {
        let bundle = standard_bundle();
        let gate =
            evaluate_replay_capture_gate(standard_capture_budget(), within_budget_observation());

        let (diagnostic, comparison) =
            super::compare_replay_bundles(&bundle, &bundle, gate).expect("compare");
        assert!(comparison.divergence.is_none());
        assert!(comparison.root_cause_hints.is_empty());
        assert!(diagnostic.divergence.is_none());
    }

    #[test]
    fn compare_replay_bundles_with_divergence() {
        let reference = standard_bundle();
        let mut observed_builder = ReplayTraceBuilder::new("trace-diagnostic");
        observed_builder.push(draft(1, "ext.a", "req-1", ReplayEventKind::Scheduled));
        observed_builder.push(draft(2, "ext.a", "req-1", ReplayEventKind::PolicyDecision));
        observed_builder.push(draft(3, "ext.a", "req-1", ReplayEventKind::Failed));
        let observed = observed_builder.build().expect("observed bundle");

        let gate =
            evaluate_replay_capture_gate(standard_capture_budget(), within_budget_observation());

        let (diagnostic, comparison) =
            super::compare_replay_bundles(&reference, &observed, gate).expect("compare");
        assert!(comparison.divergence.is_some());
        assert!(!comparison.root_cause_hints.is_empty());
        assert!(diagnostic.divergence.is_some());
    }

    #[test]
    fn compare_replay_bundles_reports_schema_mismatch_with_hint() {
        let reference = standard_bundle();
        let mut observed = standard_bundle();
        observed.schema = "pi.ext.replay.trace.v2".to_string();
        let gate =
            evaluate_replay_capture_gate(standard_capture_budget(), within_budget_observation());

        let (diagnostic, comparison) =
            super::compare_replay_bundles(&reference, &observed, gate).expect("compare");

        assert_eq!(
            comparison.root_cause_hints,
            vec![ReplayRootCauseHint::TraceSchemaMismatch]
        );
        assert_eq!(
            diagnostic.root_cause_hints,
            vec![ReplayRootCauseHint::TraceSchemaMismatch]
        );
        match comparison
            .divergence
            .expect("schema mismatch divergence")
            .reason
        {
            ReplayDivergenceReason::SchemaMismatch { expected, observed } => {
                assert_eq!(expected, REPLAY_TRACE_SCHEMA_V1);
                assert_eq!(observed, "pi.ext.replay.trace.v2");
            }
            other => unreachable!("expected SchemaMismatch, got: {other:?}"),
        }
    }

    // ── ReplayLaneConfig tests ──

    #[test]
    fn lane_config_serde_roundtrip() {
        let mut config = super::ReplayLaneConfig::new(standard_capture_budget());
        config.insert_metadata("env", "prod");

        let json = serde_json::to_string(&config).expect("serialize");
        let roundtrip: super::ReplayLaneConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(config, roundtrip);
    }

    #[test]
    fn lane_config_empty_metadata_omitted_in_json() {
        let config = super::ReplayLaneConfig::new(standard_capture_budget());
        let json = serde_json::to_string(&config).expect("serialize");
        assert!(!json.contains("laneMetadata"));
    }

    #[test]
    fn lane_result_serde_roundtrip() {
        let mut recorder = super::ReplayRecorder::new("trace-serde", standard_lane_config());
        recorder.tick();
        recorder.record_scheduled("ext.a", "req-1", BTreeMap::new());
        recorder.tick();
        recorder.record_completed("ext.a", "req-1", BTreeMap::new());

        let result = recorder
            .finish(within_budget_observation())
            .expect("finish");
        let json = serde_json::to_string(&result).expect("serialize");
        let roundtrip: super::ReplayLaneResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(result, roundtrip);
    }

    #[test]
    fn comparison_result_serde_roundtrip() {
        let comparison = super::ReplayComparisonResult {
            reference_trace_id: "ref-1".to_string(),
            observed_trace_id: "obs-1".to_string(),
            divergence: None,
            root_cause_hints: vec![],
        };
        let json = serde_json::to_string(&comparison).expect("serialize");
        let roundtrip: super::ReplayComparisonResult =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(comparison, roundtrip);
    }

    #[test]
    fn recorder_tick_is_monotonic() {
        let mut recorder = super::ReplayRecorder::new("trace-tick", standard_lane_config());
        let t1 = recorder.tick();
        let t2 = recorder.tick();
        let t3 = recorder.tick();
        assert_eq!(t1, 1);
        assert_eq!(t2, 2);
        assert_eq!(t3, 3);
    }

    // ── Property tests ──────────────────────────────────────────────────

    mod proptest_extension_replay {
        use super::*;
        use proptest::prelude::*;

        fn arb_event_kind() -> impl Strategy<Value = ReplayEventKind> {
            prop::sample::select(vec![
                ReplayEventKind::Scheduled,
                ReplayEventKind::QueueAccepted,
                ReplayEventKind::PolicyDecision,
                ReplayEventKind::Completed,
                ReplayEventKind::Failed,
            ])
        }

        fn arb_ext_id() -> impl Strategy<Value = String> {
            "ext\\.[a-z]{1,5}"
        }

        fn arb_req_id() -> impl Strategy<Value = String> {
            "req-[0-9]{1,4}"
        }

        fn arb_simple_draft() -> impl Strategy<Value = ReplayEventDraft> {
            (1..100u64, arb_ext_id(), arb_req_id(), arb_event_kind())
                .prop_map(|(clock, ext, req, kind)| ReplayEventDraft::new(clock, ext, req, kind))
        }

        proptest! {
            #[test]
            fn compute_overhead_per_mille_zero_when_captured_leq_baseline(
                baseline in 1..10_000u64,
                captured in 0..10_000u64,
            ) {
                if captured <= baseline {
                    let result = super::super::compute_overhead_per_mille(baseline, captured);
                    assert_eq!(
                        result, 0,
                        "captured <= baseline should yield 0 overhead"
                    );
                }
            }

            #[test]
            fn compute_overhead_per_mille_zero_baseline_returns_max(
                captured in 0..10_000u64,
            ) {
                let result = super::super::compute_overhead_per_mille(0, captured);
                assert_eq!(
                    result, u32::MAX,
                    "zero baseline should always be treated as invalid telemetry"
                );
            }

            #[test]
            fn compute_overhead_per_mille_is_non_negative(
                baseline in 0..10_000u64,
                captured in 0..10_000u64,
            ) {
                let result = super::super::compute_overhead_per_mille(baseline, captured);
                // u32 is always non-negative, but verify we never panic
                let _ = result;
            }

            #[test]
            fn builder_produces_contiguous_sequences(
                drafts in prop::collection::vec(arb_simple_draft(), 0..10),
            ) {
                let mut builder = ReplayTraceBuilder::new("trace-prop");
                for d in drafts {
                    builder.push(d);
                }
                let bundle = builder.build().expect("build should succeed");
                for (idx, event) in bundle.events.iter().enumerate() {
                    assert_eq!(
                        event.seq,
                        (idx + 1) as u64,
                        "sequence should be 1-based contiguous"
                    );
                }
            }

            #[test]
            fn builder_is_deterministic_regardless_of_push_order(
                drafts in prop::collection::vec(arb_simple_draft(), 0..8),
            ) {
                let mut builder1 = ReplayTraceBuilder::new("trace-det");
                for d in &drafts {
                    builder1.push(d.clone());
                }
                let bundle1 = builder1.build().expect("build1");

                let mut reversed = drafts;
                reversed.reverse();
                let mut builder2 = ReplayTraceBuilder::new("trace-det");
                for d in &reversed {
                    builder2.push(d.clone());
                }
                let bundle2 = builder2.build().expect("build2");

                assert_eq!(
                    bundle1, bundle2,
                    "canonical ordering should be same regardless of push order"
                );
            }

            #[test]
            fn identical_bundles_have_no_divergence(
                drafts in prop::collection::vec(arb_simple_draft(), 0..8),
            ) {
                let mut builder = ReplayTraceBuilder::new("trace-id");
                for d in &drafts {
                    builder.push(d.clone());
                }
                let bundle = builder.build().expect("build");
                let divergence = first_divergence(&bundle, &bundle)
                    .expect("comparison should succeed");
                assert!(
                    divergence.is_none(),
                    "identical bundles should have no divergence"
                );
            }

            #[test]
            fn json_roundtrip_preserves_bundle(
                drafts in prop::collection::vec(arb_simple_draft(), 0..6),
            ) {
                let mut builder = ReplayTraceBuilder::new("trace-rt");
                for d in drafts {
                    builder.push(d);
                }
                let bundle = builder.build().expect("build");
                let json = bundle.encode_json().expect("encode");
                let decoded = ReplayTraceBundle::decode_json(&json).expect("decode");
                assert_eq!(bundle, decoded, "JSON roundtrip should preserve bundle");
            }

            #[test]
            fn capture_gate_disabled_config_always_rejects(
                baseline in 1..10_000u64,
                captured in 1..10_000u64,
                trace_bytes in 0..10_000u64,
                max_overhead in 0..1_000u32,
                max_bytes in 0..10_000u64,
            ) {
                let budget = ReplayCaptureBudget {
                    capture_enabled: false,
                    max_overhead_per_mille: max_overhead,
                    max_trace_bytes: max_bytes,
                };
                let observation = ReplayCaptureObservation {
                    baseline_micros: baseline,
                    captured_micros: captured,
                    trace_bytes,
                };
                let report = evaluate_replay_capture_gate(budget, observation);
                assert!(
                    !report.capture_allowed,
                    "disabled config should always reject"
                );
                assert_eq!(report.reason, ReplayCaptureGateReason::DisabledByConfig);
            }

            #[test]
            fn capture_gate_is_deterministic(
                baseline in 0..5_000u64,
                captured in 0..5_000u64,
                trace_bytes in 0..5_000u64,
                enabled in any::<bool>(),
                max_overhead in 0..500u32,
                max_bytes in 0..5_000u64,
            ) {
                let budget = ReplayCaptureBudget {
                    capture_enabled: enabled,
                    max_overhead_per_mille: max_overhead,
                    max_trace_bytes: max_bytes,
                };
                let observation = ReplayCaptureObservation {
                    baseline_micros: baseline,
                    captured_micros: captured,
                    trace_bytes,
                };
                let r1 = evaluate_replay_capture_gate(budget, observation);
                let r2 = evaluate_replay_capture_gate(budget, observation);
                assert_eq!(r1, r2, "capture gate must be deterministic");
            }

            #[test]
            fn event_kind_canonical_rank_all_distinct(
                a_idx in 0..7usize,
                b_idx in 0..7usize,
            ) {
                let kinds = [
                    ReplayEventKind::Scheduled,
                    ReplayEventKind::QueueAccepted,
                    ReplayEventKind::PolicyDecision,
                    ReplayEventKind::Cancelled,
                    ReplayEventKind::Retried,
                    ReplayEventKind::Completed,
                    ReplayEventKind::Failed,
                ];
                if a_idx != b_idx {
                    assert_ne!(
                        kinds[a_idx].canonical_rank(),
                        kinds[b_idx].canonical_rank(),
                        "distinct kinds should have distinct ranks"
                    );
                }
            }

            #[test]
            fn builder_events_sorted_by_logical_clock(
                clocks in prop::collection::vec(0..50u64, 1..10),
            ) {
                let mut builder = ReplayTraceBuilder::new("trace-clock");
                for (i, clock) in clocks.iter().enumerate() {
                    builder.push(ReplayEventDraft::new(
                        *clock,
                        format!("ext.{i}"),
                        format!("req-{i}"),
                        ReplayEventKind::Scheduled,
                    ));
                }
                let bundle = builder.build().expect("build");
                for pair in bundle.events.windows(2) {
                    assert!(
                        pair[0].logical_clock <= pair[1].logical_clock,
                        "events should be sorted by logical clock: {} > {}",
                        pair[0].logical_clock,
                        pair[1].logical_clock,
                    );
                }
            }
        }
    }
}
