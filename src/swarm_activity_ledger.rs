//! Redacted multi-agent activity ledger for swarm runs.
//!
//! The ledger is intentionally small and append-oriented: callers provide
//! operational events, the ledger assigns monotonic sequence numbers, redacts
//! sensitive fields by default, and exports stable JSONL for incident review.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// Schema emitted by every swarm activity ledger entry.
pub const SWARM_ACTIVITY_LEDGER_SCHEMA: &str = "pi.swarm.activity_ledger.v1";

/// Schema emitted by bounded swarm activity summaries.
pub const SWARM_ACTIVITY_SUMMARY_SCHEMA: &str = "pi.swarm.activity_summary.v1";

/// Schema emitted by bounded swarm transcript digests.
pub const SWARM_ACTIVITY_DIGEST_SCHEMA: &str = "pi.swarm.activity_digest.v1";

/// Default number of hot spots retained per summary dimension.
pub const DEFAULT_SWARM_ACTIVITY_HOTSPOT_CAPACITY: usize = 64;

/// Default number of latency samples retained by the bounded sketch.
pub const DEFAULT_SWARM_ACTIVITY_LATENCY_SAMPLE_CAPACITY: usize = 256;

/// Default number of items retained per digest section.
pub const DEFAULT_SWARM_ACTIVITY_DIGEST_ITEM_CAPACITY: usize = 16;

/// Default age after which an Agent Mail thread is reported as stale.
pub const DEFAULT_SWARM_ACTIVITY_STALE_THREAD_AFTER_MS: u64 = 30 * 60 * 1000;

/// Default effort window for saturation detection.
pub const DEFAULT_SWARM_ACTIVITY_SATURATION_WINDOW_MS: u64 = 60 * 60 * 1000;

/// Default repeated closed-surface edit count that flags saturation.
pub const DEFAULT_SWARM_ACTIVITY_CLOSED_SURFACE_EDIT_THRESHOLD: u64 = 2;

/// Default stale introduction count that flags saturation.
pub const DEFAULT_SWARM_ACTIVITY_STALE_INTRODUCTION_THRESHOLD: u64 = 2;

/// Default Agent Mail chatter count that can flag low-throughput saturation.
pub const DEFAULT_SWARM_ACTIVITY_COORDINATION_CHATTER_THRESHOLD: u64 = 5;

/// Default maximum throughput events allowed during high-chatter saturation.
pub const DEFAULT_SWARM_ACTIVITY_LOW_THROUGHPUT_THRESHOLD: u64 = 1;

const REDACTED: &str = "[REDACTED]";
const HOTSPOT_KEY_MAX_CHARS: usize = 240;
const BLOCKER_FINGERPRINT_PREFIX: &str = "blocker:";
const BLOCKER_FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const BLOCKER_FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const DETAIL_HOTSPOT_KEYS: &[&str] = &[
    "command",
    "decision",
    "exit_code",
    "model",
    "provider",
    "status",
    "tool",
    "tool_name",
    "verification_id",
];
const LATENCY_DETAIL_KEYS: &[&str] = &["duration_ms", "elapsed_ms", "latency_ms"];
const DIGEST_DETAIL_KEYS: &[&str] = &[
    "action",
    "command",
    "decision",
    "exit_code",
    "file",
    "issue_type",
    "model",
    "path",
    "provider",
    "status",
    "tool",
    "tool_name",
    "verification_id",
];
const BLOCKER_FINGERPRINT_DETAIL_KEYS: &[&str] = &[
    "artifact",
    "command",
    "error",
    "exit_code",
    "file",
    "message",
    "path",
    "reason",
    "status",
    "stderr",
    "stdout",
    "tool",
    "tool_name",
];
const SENSITIVE_KEY_FRAGMENTS: &[&str] = &[
    "authorization",
    "bearer",
    "body",
    "cookie",
    "key",
    "password",
    "prompt",
    "secret",
    "token",
    "transcript",
];

/// Capacity controls for bounded swarm activity sketches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmActivitySummaryConfig {
    /// Maximum retained items for each hot spot list.
    pub max_hotspots: usize,
    /// Maximum retained latency samples for approximate quantiles.
    pub max_latency_samples: usize,
}

impl Default for SwarmActivitySummaryConfig {
    fn default() -> Self {
        Self {
            max_hotspots: DEFAULT_SWARM_ACTIVITY_HOTSPOT_CAPACITY,
            max_latency_samples: DEFAULT_SWARM_ACTIVITY_LATENCY_SAMPLE_CAPACITY,
        }
    }
}

impl SwarmActivitySummaryConfig {
    /// Create capacity controls for a bounded summary sketch.
    #[must_use]
    pub const fn new(max_hotspots: usize, max_latency_samples: usize) -> Self {
        Self {
            max_hotspots,
            max_latency_samples,
        }
    }
}

/// Capacity and threshold controls for swarm transcript digests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmActivityDigestConfig {
    /// Maximum retained rows in each bounded digest section.
    pub max_items: usize,
    /// Thread inactivity age, measured from the newest ledger row.
    pub stale_thread_after_ms: u64,
    /// Recent effort window used when counting newly filed bugs.
    pub saturation_window_ms: u64,
    /// Minimum newly filed bug count expected in the effort window.
    pub min_new_bugs_per_window: u64,
    /// Count at which duplicate-work events become a saturation signal.
    pub duplicate_work_threshold: u64,
    /// Count at which a blocker key is considered repeated.
    pub repeated_blocker_threshold: u64,
    /// Count at which edits to already-closed bead surfaces become a signal.
    #[serde(default = "default_closed_surface_edit_threshold")]
    pub closed_surface_edit_threshold: u64,
    /// Count at which introductions without later claims become a signal.
    #[serde(default = "default_stale_introduction_threshold")]
    pub stale_introduction_threshold: u64,
    /// Agent Mail events in the effort window that trigger throughput review.
    #[serde(default = "default_coordination_chatter_threshold")]
    pub coordination_chatter_threshold: u64,
    /// Maximum throughput events allowed when chatter is above threshold.
    #[serde(default = "default_low_throughput_threshold")]
    pub low_throughput_event_threshold: u64,
}

impl Default for SwarmActivityDigestConfig {
    fn default() -> Self {
        Self {
            max_items: DEFAULT_SWARM_ACTIVITY_DIGEST_ITEM_CAPACITY,
            stale_thread_after_ms: DEFAULT_SWARM_ACTIVITY_STALE_THREAD_AFTER_MS,
            saturation_window_ms: DEFAULT_SWARM_ACTIVITY_SATURATION_WINDOW_MS,
            min_new_bugs_per_window: 1,
            duplicate_work_threshold: 2,
            repeated_blocker_threshold: 2,
            closed_surface_edit_threshold: DEFAULT_SWARM_ACTIVITY_CLOSED_SURFACE_EDIT_THRESHOLD,
            stale_introduction_threshold: DEFAULT_SWARM_ACTIVITY_STALE_INTRODUCTION_THRESHOLD,
            coordination_chatter_threshold: DEFAULT_SWARM_ACTIVITY_COORDINATION_CHATTER_THRESHOLD,
            low_throughput_event_threshold: DEFAULT_SWARM_ACTIVITY_LOW_THROUGHPUT_THRESHOLD,
        }
    }
}

impl SwarmActivityDigestConfig {
    /// Create digest controls for deterministic handoff summaries.
    #[must_use]
    pub const fn new(
        max_items: usize,
        stale_thread_after_ms: u64,
        saturation_window_ms: u64,
        min_new_bugs_per_window: u64,
        duplicate_work_threshold: u64,
        repeated_blocker_threshold: u64,
    ) -> Self {
        Self {
            max_items,
            stale_thread_after_ms,
            saturation_window_ms,
            min_new_bugs_per_window,
            duplicate_work_threshold,
            repeated_blocker_threshold,
            closed_surface_edit_threshold: DEFAULT_SWARM_ACTIVITY_CLOSED_SURFACE_EDIT_THRESHOLD,
            stale_introduction_threshold: DEFAULT_SWARM_ACTIVITY_STALE_INTRODUCTION_THRESHOLD,
            coordination_chatter_threshold: DEFAULT_SWARM_ACTIVITY_COORDINATION_CHATTER_THRESHOLD,
            low_throughput_event_threshold: DEFAULT_SWARM_ACTIVITY_LOW_THROUGHPUT_THRESHOLD,
        }
    }
}

const fn default_closed_surface_edit_threshold() -> u64 {
    DEFAULT_SWARM_ACTIVITY_CLOSED_SURFACE_EDIT_THRESHOLD
}

const fn default_stale_introduction_threshold() -> u64 {
    DEFAULT_SWARM_ACTIVITY_STALE_INTRODUCTION_THRESHOLD
}

const fn default_coordination_chatter_threshold() -> u64 {
    DEFAULT_SWARM_ACTIVITY_COORDINATION_CHATTER_THRESHOLD
}

const fn default_low_throughput_threshold() -> u64 {
    DEFAULT_SWARM_ACTIVITY_LOW_THROUGHPUT_THRESHOLD
}

/// Count for one retained hot spot key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmActivityHotspot {
    /// Retained key, truncated to a bounded length.
    pub key: String,
    /// Number of events observed for this key.
    pub count: u64,
    /// Stable normalized fingerprint used for grouping, when distinct from the display key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Representative already-redacted evidence excerpt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<String>,
}

/// Approximate latency quantiles retained by a bounded sketch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmActivityLatencySummary {
    /// Total latency observations recorded before downsampling.
    pub sample_count: u64,
    /// Number of retained samples used for the reported quantiles.
    pub retained_samples: usize,
    /// Smallest retained latency sample in milliseconds.
    pub min_ms: u64,
    /// Approximate p50 latency in milliseconds.
    pub p50_ms: u64,
    /// Approximate p95 latency in milliseconds.
    pub p95_ms: u64,
    /// Approximate p99 latency in milliseconds.
    pub p99_ms: u64,
    /// Largest retained latency sample in milliseconds.
    pub max_ms: u64,
    /// Conservative rank-error bound from bounded retention.
    pub rank_error_bound: u64,
}

/// Derived bounded view of a raw swarm activity ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmActivitySummary {
    /// Stable schema identifier.
    pub schema: String,
    /// Total events represented by this summary.
    pub event_count: u64,
    /// Events that had at least one redacted field.
    pub redacted_entry_count: u64,
    /// Total redacted fields represented by this summary.
    pub redacted_field_count: u64,
    /// Exact counts by activity kind.
    pub kind_counts: BTreeMap<SwarmActivityKind, u64>,
    /// Most frequent agent identifiers.
    pub agent_hotspots: Vec<SwarmActivityHotspot>,
    /// Most frequent Beads issue identifiers.
    pub bead_hotspots: Vec<SwarmActivityHotspot>,
    /// Most frequent verification identifiers.
    pub verification_hotspots: Vec<SwarmActivityHotspot>,
    /// Most frequent tool names from redacted detail fields.
    pub tool_hotspots: Vec<SwarmActivityHotspot>,
    /// Most frequent provider/model names from redacted detail fields.
    pub provider_hotspots: Vec<SwarmActivityHotspot>,
    /// Most frequent selected detail key/value pairs.
    pub detail_hotspots: Vec<SwarmActivityHotspot>,
    /// Approximate latency quantiles when latency detail fields were present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<SwarmActivityLatencySummary>,
}

/// One representative redacted event retained in a swarm digest section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmActivityDigestItem {
    /// Event timestamp in Unix milliseconds.
    pub timestamp_ms: u64,
    /// Activity category.
    pub kind: SwarmActivityKind,
    /// Stable event correlation ID.
    pub correlation_id: String,
    /// Redacted human summary.
    pub summary: String,
    /// Beads issue ID, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bead_id: Option<String>,
    /// Agent name, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// One selected redacted detail field for quick scanning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Agent Mail thread with no recent ledger activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmActivityStaleThread {
    /// Agent Mail thread ID.
    pub mail_thread_id: String,
    /// Last observed activity timestamp in Unix milliseconds.
    pub last_timestamp_ms: u64,
    /// Number of ledger rows observed for this thread.
    pub event_count: u64,
    /// Last redacted summary observed for this thread.
    pub last_summary: String,
}

/// Saturation signals derived from a bounded swarm transcript digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmActivitySaturationSignal {
    /// The effort window contains fewer newly filed bugs than expected.
    FewNewBugs,
    /// Blocker events repeat across the represented transcript.
    RepeatedBlockers,
    /// Duplicate-work events cross the configured threshold.
    DuplicateWork,
    /// Agents edited or reserved surfaces for already-closed beads.
    RepeatedClosedSurfaceEdits,
    /// Agent introductions did not lead to later claim or reservation evidence.
    StaleIntroductionsWithoutClaims,
    /// Coordination chatter is high while closeout throughput is low.
    HighChatterLowThroughput,
    /// Agent Mail threads are stale relative to the newest event.
    StaleThreads,
}

/// Saturation metrics derived from a bounded swarm transcript digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmActivitySaturationSignals {
    /// Effort window used for new-bug counting.
    pub window_ms: u64,
    /// Start of the effort window, when the digest is non-empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_start_ms: Option<u64>,
    /// Newly filed bug count in the effort window.
    pub new_bug_count: u64,
    /// True when the recent window found too few new bugs.
    pub few_new_bugs: bool,
    /// Repeated blocker events represented by digest hot spots.
    pub repeated_blocker_count: u64,
    /// Duplicate-work events in the represented transcript.
    pub duplicate_work_count: u64,
    /// Events that indicate edits or reservations against already-closed bead surfaces.
    #[serde(default)]
    pub closed_surface_edit_count: u64,
    /// Introductory Agent Mail events that never turn into a claim/reservation.
    #[serde(default)]
    pub stale_introduction_count: u64,
    /// Agent Mail events in the current effort window.
    #[serde(default)]
    pub coordination_chatter_count: u64,
    /// Commit, validation, or closeout events in the current effort window.
    #[serde(default)]
    pub throughput_event_count: u64,
    /// Stale Agent Mail thread count.
    pub stale_thread_count: u64,
    /// Typed active saturation signals.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub signals: BTreeSet<SwarmActivitySaturationSignal>,
    /// True when any saturation signal is active.
    pub saturated: bool,
    /// Stable textual reasons for active signals.
    pub reasons: Vec<String>,
    /// Stable redacted pointers to representative evidence behind the signals.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_pointers: Vec<String>,
}

impl SwarmActivitySaturationSignals {
    /// Return whether the digest carries a typed saturation signal.
    #[must_use]
    pub fn has_signal(&self, signal: SwarmActivitySaturationSignal) -> bool {
        self.signals.contains(&signal)
    }
}

/// Advisory work mode operators can switch to when swarm activity saturates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmActivitySkillSwitchMode {
    /// Assign one narrow implementation bead with explicit ownership.
    NarrowImplementationBead,
    /// Freeze disputed behavior with golden artifacts.
    TestingGoldenArtifacts,
    /// Build or tighten conformance harnesses for repeated blockers.
    TestingConformanceHarnesses,
    /// Search for concrete placeholder or mock code before another broad review.
    MockCodeFinder,
    /// Inspect stale coordination for blocked waits or dependency deadlocks.
    DeadlockFinder,
    /// Profile the saturated work path before increasing fanout.
    Profiling,
}

impl SwarmActivitySkillSwitchMode {
    const fn label(self) -> &'static str {
        match self {
            Self::NarrowImplementationBead => "narrow implementation bead",
            Self::TestingGoldenArtifacts => "testing-golden-artifacts",
            Self::TestingConformanceHarnesses => "testing-conformance-harnesses",
            Self::MockCodeFinder => "mock-code-finder",
            Self::DeadlockFinder => "deadlock-finder",
            Self::Profiling => "profiling",
        }
    }
}

/// Confidence assigned to an advisory skill-switch recommendation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmActivityRecommendationConfidence {
    /// Weak signal; inspect before acting.
    Low,
    /// Clear signal, but not enough to make it the only reasonable path.
    Medium,
    /// Direct saturation signal with bounded evidence.
    High,
}

impl SwarmActivityRecommendationConfidence {
    const fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Deterministic advisory recommendation derived from redacted saturation fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmActivitySkillSwitchRecommendation {
    /// Recommended next work mode.
    pub mode: SwarmActivitySkillSwitchMode,
    /// Confidence in the recommendation.
    pub confidence: SwarmActivityRecommendationConfidence,
    /// Why this mode fits the active saturation evidence.
    pub reason: String,
    /// Why the confidence level was assigned.
    pub confidence_reason: String,
    /// Redacted saturation evidence pointers that support this recommendation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_pointers: Vec<String>,
}

/// Deterministic redacted digest for swarm handoff and saturation review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmActivityDigest {
    /// Stable schema identifier.
    pub schema: String,
    /// Total events represented by this digest.
    pub event_count: u64,
    /// Earliest represented event timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_timestamp_ms: Option<u64>,
    /// Latest represented event timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_timestamp_ms: Option<u64>,
    /// Most active agents by redacted event count.
    pub active_agents: Vec<SwarmActivityHotspot>,
    /// Recent Beads changes.
    pub bead_changes: Vec<SwarmActivityDigestItem>,
    /// Recent Agent Mail activity.
    pub agent_mail_activity: Vec<SwarmActivityDigestItem>,
    /// Recent file reservation activity.
    pub file_reservations: Vec<SwarmActivityDigestItem>,
    /// Recent verification, RCH, and git evidence.
    pub verification_evidence: Vec<SwarmActivityDigestItem>,
    /// Repeated blocker hot spots.
    pub repeated_blockers: Vec<SwarmActivityHotspot>,
    /// Inactive Agent Mail threads.
    pub stale_threads: Vec<SwarmActivityStaleThread>,
    /// Saturation and duplicate-work signals.
    pub saturation: SwarmActivitySaturationSignals,
    /// Advisory skill-switch recommendations derived from saturation evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommendations: Vec<SwarmActivitySkillSwitchRecommendation>,
}

impl SwarmActivityDigest {
    /// Render a deterministic redacted text digest for operators.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str("Swarm activity digest\n");
        if self.event_count == 0 {
            out.push_str("Events: 0\n");
            out.push_str("No swarm activity events.\n");
            return out;
        }

        let _ = writeln!(out, "Events: {}", self.event_count);
        if let (Some(first), Some(last)) = (self.first_timestamp_ms, self.last_timestamp_ms) {
            let _ = writeln!(out, "Window: {first}..{last}");
        }
        write_hotspot_section(&mut out, "Active agents", &self.active_agents);
        write_item_section(&mut out, "Bead changes", &self.bead_changes);
        write_item_section(&mut out, "Agent Mail", &self.agent_mail_activity);
        write_item_section(&mut out, "File reservations", &self.file_reservations);
        write_item_section(
            &mut out,
            "Verification evidence",
            &self.verification_evidence,
        );
        write_hotspot_section(&mut out, "Repeated blockers", &self.repeated_blockers);
        write_stale_thread_section(&mut out, &self.stale_threads);
        let _ = writeln!(
            out,
            "Saturation: {}",
            if self.saturation.saturated {
                "yes"
            } else {
                "no"
            }
        );
        if self.saturation.reasons.is_empty() {
            out.push_str("- none\n");
        } else {
            for reason in &self.saturation.reasons {
                let _ = writeln!(out, "- {reason}");
            }
        }
        if !self.saturation.evidence_pointers.is_empty() {
            out.push_str("Saturation evidence:\n");
            for pointer in &self.saturation.evidence_pointers {
                let _ = writeln!(out, "- {pointer}");
            }
        }
        write_recommendation_section(&mut out, &self.recommendations);
        out
    }
}

/// Mergeable bounded sketch for swarm activity events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmActivitySketch {
    schema: String,
    config: SwarmActivitySummaryConfig,
    event_count: u64,
    redacted_entry_count: u64,
    redacted_field_count: u64,
    kind_counts: BTreeMap<SwarmActivityKind, u64>,
    agent_counts: BTreeMap<String, u64>,
    bead_counts: BTreeMap<String, u64>,
    verification_counts: BTreeMap<String, u64>,
    tool_counts: BTreeMap<String, u64>,
    provider_counts: BTreeMap<String, u64>,
    detail_counts: BTreeMap<String, u64>,
    latency_ms: BoundedLatencySamples,
}

impl Default for SwarmActivitySketch {
    fn default() -> Self {
        Self::new(SwarmActivitySummaryConfig::default())
    }
}

impl SwarmActivitySketch {
    /// Create an empty bounded sketch with the supplied capacity controls.
    #[must_use]
    pub fn new(config: SwarmActivitySummaryConfig) -> Self {
        Self {
            schema: SWARM_ACTIVITY_SUMMARY_SCHEMA.to_string(),
            config,
            event_count: 0,
            redacted_entry_count: 0,
            redacted_field_count: 0,
            kind_counts: BTreeMap::new(),
            agent_counts: BTreeMap::new(),
            bead_counts: BTreeMap::new(),
            verification_counts: BTreeMap::new(),
            tool_counts: BTreeMap::new(),
            provider_counts: BTreeMap::new(),
            detail_counts: BTreeMap::new(),
            latency_ms: BoundedLatencySamples::new(config.max_latency_samples),
        }
    }

    /// Record all entries from an existing ledger slice.
    pub fn record_entries<'entry>(
        &mut self,
        entries: impl IntoIterator<Item = &'entry SwarmActivityLedgerEntry>,
    ) {
        for entry in entries {
            self.record_entry(entry);
        }
    }

    /// Record one raw ledger entry into the bounded sketch.
    pub fn record_entry(&mut self, entry: &SwarmActivityLedgerEntry) {
        self.event_count = self.event_count.saturating_add(1);
        if entry.redaction.redacted_count > 0 {
            self.redacted_entry_count = self.redacted_entry_count.saturating_add(1);
        }
        self.redacted_field_count = self
            .redacted_field_count
            .saturating_add(usize_to_u64(entry.redaction.redacted_count));
        increment_kind_count(&mut self.kind_counts, entry.kind);
        record_optional_hotspot(
            &mut self.agent_counts,
            entry.ids.agent_name.as_deref(),
            self.config.max_hotspots,
        );
        record_optional_hotspot(
            &mut self.bead_counts,
            entry.ids.bead_id.as_deref(),
            self.config.max_hotspots,
        );
        record_optional_hotspot(
            &mut self.verification_counts,
            entry.ids.verification_id.as_deref(),
            self.config.max_hotspots,
        );
        for (key, value) in entry.details() {
            self.record_detail(key, value);
        }
    }

    /// Merge another sketch into this sketch, retaining this sketch's capacities.
    pub fn merge(&mut self, other: &Self) {
        self.event_count = self.event_count.saturating_add(other.event_count);
        self.redacted_entry_count = self
            .redacted_entry_count
            .saturating_add(other.redacted_entry_count);
        self.redacted_field_count = self
            .redacted_field_count
            .saturating_add(other.redacted_field_count);
        merge_kind_counts(&mut self.kind_counts, &other.kind_counts);
        merge_count_map(
            &mut self.agent_counts,
            &other.agent_counts,
            self.config.max_hotspots,
        );
        merge_count_map(
            &mut self.bead_counts,
            &other.bead_counts,
            self.config.max_hotspots,
        );
        merge_count_map(
            &mut self.verification_counts,
            &other.verification_counts,
            self.config.max_hotspots,
        );
        merge_count_map(
            &mut self.tool_counts,
            &other.tool_counts,
            self.config.max_hotspots,
        );
        merge_count_map(
            &mut self.provider_counts,
            &other.provider_counts,
            self.config.max_hotspots,
        );
        merge_count_map(
            &mut self.detail_counts,
            &other.detail_counts,
            self.config.max_hotspots,
        );
        self.latency_ms.merge(&other.latency_ms);
    }

    /// Return a serializable bounded summary from this sketch.
    #[must_use]
    pub fn snapshot(&self) -> SwarmActivitySummary {
        SwarmActivitySummary {
            schema: self.schema.clone(),
            event_count: self.event_count,
            redacted_entry_count: self.redacted_entry_count,
            redacted_field_count: self.redacted_field_count,
            kind_counts: self.kind_counts.clone(),
            agent_hotspots: top_hotspots(&self.agent_counts, self.config.max_hotspots),
            bead_hotspots: top_hotspots(&self.bead_counts, self.config.max_hotspots),
            verification_hotspots: top_hotspots(
                &self.verification_counts,
                self.config.max_hotspots,
            ),
            tool_hotspots: top_hotspots(&self.tool_counts, self.config.max_hotspots),
            provider_hotspots: top_hotspots(&self.provider_counts, self.config.max_hotspots),
            detail_hotspots: top_hotspots(&self.detail_counts, self.config.max_hotspots),
            latency_ms: self.latency_ms.summary(),
        }
    }

    fn record_detail(&mut self, key: &str, value: &str) {
        let normalized_key = key.to_ascii_lowercase();
        match normalized_key.as_str() {
            "tool" | "tool_name" => {
                record_hotspot(&mut self.tool_counts, value, self.config.max_hotspots);
            }
            "model" | "provider" => {
                record_hotspot(&mut self.provider_counts, value, self.config.max_hotspots);
            }
            _ => {}
        }
        if DETAIL_HOTSPOT_KEYS.contains(&normalized_key.as_str()) {
            let detail_key = format!("{normalized_key}={value}");
            record_hotspot(
                &mut self.detail_counts,
                &detail_key,
                self.config.max_hotspots,
            );
        }
        if LATENCY_DETAIL_KEYS.contains(&normalized_key.as_str())
            && let Some(sample_ms) = parse_latency_ms(value)
        {
            self.latency_ms.record(sample_ms);
        }
    }
}

/// Category of activity captured by the swarm ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmActivityKind {
    /// Beads status or ownership changed.
    BeadStatus,
    /// Agent Mail message/thread activity.
    AgentMail,
    /// Agent Mail file reservation activity.
    FileReservation,
    /// RCH verification job state.
    RchJob,
    /// Local or remote verification command result.
    Verification,
    /// Git commit or push event.
    GitCommit,
    /// Explicit recovery or operator intervention.
    Recovery,
    /// General redacted note.
    Note,
}

/// Correlation identifiers attached to a swarm activity event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmActivityIds {
    /// Stable event correlation ID for joining entries across systems.
    pub correlation_id: String,
    /// Beads issue ID, when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bead_id: Option<String>,
    /// Agent Mail thread ID, when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mail_thread_id: Option<String>,
    /// Agent Mail message ID, when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mail_message_id: Option<u64>,
    /// Agent name that produced or owns the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// File reservation ID, when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_reservation_id: Option<u64>,
    /// RCH job/build ID, when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rch_job_id: Option<String>,
    /// Verification command/run ID, when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_id: Option<String>,
    /// Git commit SHA, when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,
}

impl SwarmActivityIds {
    /// Create ID metadata with the required correlation ID.
    #[must_use]
    pub fn new(correlation_id: impl Into<String>) -> Self {
        Self {
            correlation_id: correlation_id.into(),
            ..Self::default()
        }
    }

    /// Attach a bead ID.
    #[must_use]
    pub fn with_bead_id(mut self, bead_id: impl Into<String>) -> Self {
        self.bead_id = Some(bead_id.into());
        self
    }

    /// Attach an Agent Mail thread ID.
    #[must_use]
    pub fn with_mail_thread_id(mut self, mail_thread_id: impl Into<String>) -> Self {
        self.mail_thread_id = Some(mail_thread_id.into());
        self
    }

    /// Attach an agent name.
    #[must_use]
    pub fn with_agent_name(mut self, agent_name: impl Into<String>) -> Self {
        self.agent_name = Some(agent_name.into());
        self
    }

    /// Attach an RCH job ID.
    #[must_use]
    pub fn with_rch_job_id(mut self, rch_job_id: impl Into<String>) -> Self {
        self.rch_job_id = Some(rch_job_id.into());
        self
    }

    /// Attach a git commit SHA.
    #[must_use]
    pub fn with_git_sha(mut self, git_sha: impl Into<String>) -> Self {
        self.git_sha = Some(git_sha.into());
        self
    }
}

/// Summary of field-level redaction applied before serialization.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmActivityRedaction {
    /// Number of fields redacted in this entry.
    pub redacted_count: usize,
    /// Field names that were redacted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redacted_fields: Vec<String>,
}

impl SwarmActivityRedaction {
    fn record(&mut self, field: impl Into<String>) {
        self.redacted_count = self.redacted_count.saturating_add(1);
        self.redacted_fields.push(field.into());
    }
}

/// One redacted JSONL entry in the swarm activity ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmActivityLedgerEntry {
    /// Stable schema identifier.
    pub schema: String,
    /// Monotonic sequence number assigned by the producing ledger.
    pub sequence: u64,
    /// Event timestamp in Unix milliseconds.
    pub timestamp_ms: u64,
    /// Activity category.
    pub kind: SwarmActivityKind,
    /// Redacted human summary.
    pub summary: String,
    /// Correlation IDs for joining with Beads, Agent Mail, RCH, and Git.
    #[serde(default)]
    pub ids: SwarmActivityIds,
    /// Additional redacted structured fields.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    details: BTreeMap<String, String>,
    /// Redaction metadata.
    #[serde(default)]
    pub redaction: SwarmActivityRedaction,
}

impl SwarmActivityLedgerEntry {
    /// Return structured redacted detail fields.
    #[must_use]
    pub const fn details(&self) -> &BTreeMap<String, String> {
        &self.details
    }

    /// True when the entry uses the current schema.
    #[must_use]
    pub fn has_current_schema(&self) -> bool {
        self.schema == SWARM_ACTIVITY_LEDGER_SCHEMA
    }
}

/// Append-only in-memory activity ledger.
#[derive(Debug, Clone, Default)]
pub struct SwarmActivityLedger {
    entries: Vec<SwarmActivityLedgerEntry>,
    next_sequence: u64,
}

impl SwarmActivityLedger {
    /// Create an empty ledger.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_sequence: 0,
        }
    }

    /// Append one activity event and return its assigned sequence.
    pub fn append(
        &mut self,
        timestamp_ms: u64,
        kind: SwarmActivityKind,
        ids: SwarmActivityIds,
        summary: impl Into<String>,
        details: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);

        let (summary, details, redaction) = redact_entry(summary.into(), details);
        self.entries.push(SwarmActivityLedgerEntry {
            schema: SWARM_ACTIVITY_LEDGER_SCHEMA.to_string(),
            sequence,
            timestamp_ms,
            kind,
            summary,
            ids,
            details,
            redaction,
        });
        sequence
    }

    /// All entries in append order.
    #[must_use]
    pub fn entries(&self) -> &[SwarmActivityLedgerEntry] {
        &self.entries
    }

    /// Number of entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no entries have been appended.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Serialize entries as JSONL.
    ///
    /// # Errors
    ///
    /// Returns a serde error if an entry cannot be serialized.
    pub fn to_jsonl(&self) -> Result<String, serde_json::Error> {
        entries_to_jsonl(&self.entries)
    }

    /// Build a bounded summary from all retained raw entries.
    #[must_use]
    pub fn summarize(&self) -> SwarmActivitySummary {
        self.summarize_with_config(SwarmActivitySummaryConfig::default())
    }

    /// Build a bounded summary from all retained raw entries with custom capacities.
    #[must_use]
    pub fn summarize_with_config(
        &self,
        config: SwarmActivitySummaryConfig,
    ) -> SwarmActivitySummary {
        let mut sketch = SwarmActivitySketch::new(config);
        sketch.record_entries(&self.entries);
        sketch.snapshot()
    }

    /// Build a deterministic redacted digest from all retained raw entries.
    #[must_use]
    pub fn digest(&self) -> SwarmActivityDigest {
        self.digest_with_config(SwarmActivityDigestConfig::default())
    }

    /// Build a deterministic redacted digest with custom capacities.
    #[must_use]
    pub fn digest_with_config(&self, config: SwarmActivityDigestConfig) -> SwarmActivityDigest {
        digest_entries_with_config(&self.entries, config)
    }
}

/// Timeline event used by replay/incident review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwarmActivityTimelineEvent {
    /// Original ledger sequence.
    pub sequence: u64,
    /// Event timestamp in Unix milliseconds.
    pub timestamp_ms: u64,
    /// Activity category.
    pub kind: SwarmActivityKind,
    /// Stable event correlation ID.
    pub correlation_id: String,
    /// Redacted summary.
    pub summary: String,
}

impl From<&SwarmActivityLedgerEntry> for SwarmActivityTimelineEvent {
    fn from(entry: &SwarmActivityLedgerEntry) -> Self {
        Self {
            sequence: entry.sequence,
            timestamp_ms: entry.timestamp_ms,
            kind: entry.kind,
            correlation_id: entry.ids.correlation_id.clone(),
            summary: entry.summary.clone(),
        }
    }
}

/// Errors when parsing or validating activity ledger JSONL.
#[derive(Debug, thiserror::Error)]
pub enum SwarmActivityLedgerError {
    /// One JSONL row was not valid JSON.
    #[error("failed to parse swarm activity ledger line {line}: {source}")]
    Parse {
        /// 1-based line number.
        line: usize,
        /// serde parse error.
        source: serde_json::Error,
    },
    /// One JSONL row used an unsupported schema.
    #[error("unsupported swarm activity ledger schema on line {line}: {schema}")]
    UnsupportedSchema {
        /// 1-based line number.
        line: usize,
        /// Unsupported schema value.
        schema: String,
    },
    /// One JSONL row omitted a required correlation ID.
    #[error("missing correlation_id on swarm activity ledger line {line}")]
    MissingCorrelationId {
        /// 1-based line number.
        line: usize,
    },
}

/// Serialize entries as JSONL.
///
/// # Errors
///
/// Returns a serde error if an entry cannot be serialized.
pub fn entries_to_jsonl(entries: &[SwarmActivityLedgerEntry]) -> Result<String, serde_json::Error> {
    let mut out = String::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&serde_json::to_string(entry)?);
    }
    Ok(out)
}

/// Parse and validate activity ledger JSONL entries.
///
/// # Errors
///
/// Returns a validation error if any row is invalid, uses an unsupported schema,
/// or omits the required correlation ID.
pub fn entries_from_jsonl(
    input: &str,
) -> Result<Vec<SwarmActivityLedgerEntry>, SwarmActivityLedgerError> {
    let mut entries = Vec::new();
    for (index, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let line_number = index + 1;
        let entry: SwarmActivityLedgerEntry =
            serde_json::from_str(line).map_err(|source| SwarmActivityLedgerError::Parse {
                line: line_number,
                source,
            })?;
        if !entry.has_current_schema() {
            return Err(SwarmActivityLedgerError::UnsupportedSchema {
                line: line_number,
                schema: entry.schema,
            });
        }
        if entry.ids.correlation_id.trim().is_empty() {
            return Err(SwarmActivityLedgerError::MissingCorrelationId { line: line_number });
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// Build a deterministic timeline from JSONL, regardless of input row order.
///
/// # Errors
///
/// Returns a validation error if any JSONL row is invalid.
pub fn timeline_from_jsonl(
    input: &str,
) -> Result<Vec<SwarmActivityTimelineEvent>, SwarmActivityLedgerError> {
    let mut entries = entries_from_jsonl(input)?;
    entries.sort_by(|left, right| {
        left.timestamp_ms
            .cmp(&right.timestamp_ms)
            .then_with(|| left.sequence.cmp(&right.sequence))
            .then_with(|| left.ids.correlation_id.cmp(&right.ids.correlation_id))
    });
    Ok(entries
        .iter()
        .map(SwarmActivityTimelineEvent::from)
        .collect())
}

/// Build a deterministic redacted digest from JSONL ledger rows.
///
/// # Errors
///
/// Returns a validation error if any JSONL row is invalid.
pub fn digest_from_jsonl(
    input: &str,
    config: SwarmActivityDigestConfig,
) -> Result<SwarmActivityDigest, SwarmActivityLedgerError> {
    let entries = entries_from_jsonl(input)?;
    Ok(digest_entries_with_config(&entries, config))
}

/// Build a deterministic redacted digest from validated ledger entries.
#[must_use]
pub fn digest_entries_with_config(
    entries: &[SwarmActivityLedgerEntry],
    config: SwarmActivityDigestConfig,
) -> SwarmActivityDigest {
    let event_count = usize_to_u64(entries.len());
    let first_timestamp_ms = entries.iter().map(|entry| entry.timestamp_ms).min();
    let last_timestamp_ms = entries.iter().map(|entry| entry.timestamp_ms).max();
    let mut agent_counts = BTreeMap::new();
    for entry in entries {
        record_optional_hotspot(
            &mut agent_counts,
            entry.ids.agent_name.as_deref(),
            config.max_items,
        );
    }

    let repeated_blockers = repeated_blockers(entries, config);
    let stale_threads = stale_threads(entries, config);
    let saturation = saturation_signals(entries, config, &repeated_blockers, &stale_threads);
    let recommendations = skill_switch_recommendations(&saturation, config.max_items);

    SwarmActivityDigest {
        schema: SWARM_ACTIVITY_DIGEST_SCHEMA.to_string(),
        event_count,
        first_timestamp_ms,
        last_timestamp_ms,
        active_agents: top_hotspots(&agent_counts, config.max_items),
        bead_changes: recent_digest_items(entries, config.max_items, |entry| {
            matches!(entry.kind, SwarmActivityKind::BeadStatus)
        }),
        agent_mail_activity: recent_digest_items(entries, config.max_items, |entry| {
            matches!(entry.kind, SwarmActivityKind::AgentMail)
        }),
        file_reservations: recent_digest_items(entries, config.max_items, |entry| {
            matches!(entry.kind, SwarmActivityKind::FileReservation)
        }),
        verification_evidence: recent_digest_items(entries, config.max_items, |entry| {
            matches!(
                entry.kind,
                SwarmActivityKind::Verification
                    | SwarmActivityKind::RchJob
                    | SwarmActivityKind::GitCommit
            )
        }),
        repeated_blockers,
        stale_threads,
        saturation,
        recommendations,
    }
}

fn recent_digest_items(
    entries: &[SwarmActivityLedgerEntry],
    max_items: usize,
    mut include: impl FnMut(&SwarmActivityLedgerEntry) -> bool,
) -> Vec<SwarmActivityDigestItem> {
    if max_items == 0 {
        return Vec::new();
    }
    let mut retained = entries
        .iter()
        .filter(|entry| include(entry))
        .collect::<Vec<_>>();
    retained.sort_by(|left, right| {
        right
            .timestamp_ms
            .cmp(&left.timestamp_ms)
            .then_with(|| right.sequence.cmp(&left.sequence))
            .then_with(|| left.ids.correlation_id.cmp(&right.ids.correlation_id))
    });
    retained.truncate(max_items);
    retained.into_iter().map(digest_item_from_entry).collect()
}

fn digest_item_from_entry(entry: &SwarmActivityLedgerEntry) -> SwarmActivityDigestItem {
    SwarmActivityDigestItem {
        timestamp_ms: entry.timestamp_ms,
        kind: entry.kind,
        correlation_id: entry.ids.correlation_id.clone(),
        summary: entry.summary.clone(),
        bead_id: entry.ids.bead_id.clone(),
        agent_name: entry.ids.agent_name.clone(),
        detail: selected_digest_detail(entry),
    }
}

fn selected_digest_detail(entry: &SwarmActivityLedgerEntry) -> Option<String> {
    for key in DIGEST_DETAIL_KEYS {
        if let Some(value) = entry.details().get(*key) {
            return Some(format!("{key}={value}"));
        }
    }
    None
}

fn repeated_blockers(
    entries: &[SwarmActivityLedgerEntry],
    config: SwarmActivityDigestConfig,
) -> Vec<SwarmActivityHotspot> {
    if config.max_items == 0 {
        return Vec::new();
    }
    let mut counts = BTreeMap::<String, BlockerHotspotAccumulator>::new();
    for entry in entries {
        if is_blocker_entry(entry) {
            let fingerprint = blocker_fingerprint(entry);
            let accumulator = counts.entry(fingerprint.fingerprint).or_insert_with(|| {
                BlockerHotspotAccumulator {
                    key: fingerprint.display_key,
                    sample: fingerprint.sample,
                    count: 0,
                }
            });
            accumulator.count = accumulator.count.saturating_add(1);
            prune_blocker_accumulators(&mut counts, config.max_items);
        }
    }
    top_blocker_hotspots(&counts, config.max_items)
        .into_iter()
        .filter(|hotspot| hotspot.count >= config.repeated_blocker_threshold)
        .collect()
}

fn stale_threads(
    entries: &[SwarmActivityLedgerEntry],
    config: SwarmActivityDigestConfig,
) -> Vec<SwarmActivityStaleThread> {
    if config.max_items == 0 {
        return Vec::new();
    }
    let Some(last_timestamp_ms) = entries.iter().map(|entry| entry.timestamp_ms).max() else {
        return Vec::new();
    };
    let mut thread_stats = BTreeMap::<String, ThreadDigestAccumulator>::new();
    for entry in entries {
        if !matches!(entry.kind, SwarmActivityKind::AgentMail) {
            continue;
        }
        let Some(thread_id) = entry.ids.mail_thread_id.as_deref() else {
            continue;
        };
        let stats = thread_stats.entry(thread_id.to_string()).or_default();
        stats.event_count = stats.event_count.saturating_add(1);
        if entry.timestamp_ms >= stats.last_timestamp_ms {
            stats.last_timestamp_ms = entry.timestamp_ms;
            stats.last_summary.clone_from(&entry.summary);
        }
    }

    let mut stale = thread_stats
        .into_iter()
        .filter_map(|(mail_thread_id, stats)| {
            let age_ms = last_timestamp_ms.saturating_sub(stats.last_timestamp_ms);
            (age_ms >= config.stale_thread_after_ms).then_some(SwarmActivityStaleThread {
                mail_thread_id,
                last_timestamp_ms: stats.last_timestamp_ms,
                event_count: stats.event_count,
                last_summary: stats.last_summary,
            })
        })
        .collect::<Vec<_>>();
    stale.sort_by(|left, right| {
        left.last_timestamp_ms
            .cmp(&right.last_timestamp_ms)
            .then_with(|| left.mail_thread_id.cmp(&right.mail_thread_id))
    });
    stale.truncate(config.max_items);
    stale
}

fn saturation_signals(
    entries: &[SwarmActivityLedgerEntry],
    config: SwarmActivityDigestConfig,
    repeated_blockers: &[SwarmActivityHotspot],
    stale_threads: &[SwarmActivityStaleThread],
) -> SwarmActivitySaturationSignals {
    let last_timestamp_ms = entries.iter().map(|entry| entry.timestamp_ms).max();
    let window_start_ms = last_timestamp_ms
        .map(|timestamp_ms| timestamp_ms.saturating_sub(config.saturation_window_ms));
    let counts =
        saturation_signal_counts(entries, repeated_blockers, stale_threads, window_start_ms);
    let mut evidence = saturation_signal_evidence(
        entries,
        config,
        repeated_blockers,
        stale_threads,
        &counts,
        window_start_ms,
    );
    evidence.evidence_pointers.truncate(config.max_items);

    let few_new_bugs = evidence
        .signals
        .contains(&SwarmActivitySaturationSignal::FewNewBugs);
    let saturated = !evidence.signals.is_empty();

    SwarmActivitySaturationSignals {
        window_ms: config.saturation_window_ms,
        window_start_ms,
        new_bug_count: counts.new_bugs,
        few_new_bugs,
        repeated_blocker_count: counts.repeated_blockers,
        duplicate_work_count: counts.duplicate_work,
        closed_surface_edit_count: counts.closed_surface_edits,
        stale_introduction_count: counts.stale_introductions,
        coordination_chatter_count: counts.coordination_chatter,
        throughput_event_count: counts.throughput_events,
        stale_thread_count: counts.stale_threads,
        signals: evidence.signals,
        saturated,
        reasons: evidence.reasons,
        evidence_pointers: evidence.evidence_pointers,
    }
}

#[derive(Debug)]
struct SkillSwitchRecommendationDraft {
    confidence: SwarmActivityRecommendationConfidence,
    reasons: Vec<String>,
    confidence_reasons: Vec<String>,
    evidence_pointers: BTreeSet<String>,
}

type SkillSwitchRecommendationDrafts =
    BTreeMap<SwarmActivitySkillSwitchMode, SkillSwitchRecommendationDraft>;

impl SkillSwitchRecommendationDraft {
    const fn new(confidence: SwarmActivityRecommendationConfidence) -> Self {
        Self {
            confidence,
            reasons: Vec::new(),
            confidence_reasons: Vec::new(),
            evidence_pointers: BTreeSet::new(),
        }
    }
}

struct SkillSwitchRecommendationInput<'a> {
    mode: SwarmActivitySkillSwitchMode,
    confidence: SwarmActivityRecommendationConfidence,
    reason: String,
    confidence_reason: &'a str,
    evidence_prefixes: &'a [&'a str],
}

fn skill_switch_recommendations(
    saturation: &SwarmActivitySaturationSignals,
    max_items: usize,
) -> Vec<SwarmActivitySkillSwitchRecommendation> {
    if max_items == 0 || !saturation.saturated {
        return Vec::new();
    }

    let mut drafts = BTreeMap::new();
    push_few_new_bug_recommendation(&mut drafts, saturation, max_items);
    push_repeated_blocker_recommendation(&mut drafts, saturation, max_items);
    push_duplicate_work_recommendation(&mut drafts, saturation, max_items);
    push_closed_surface_recommendation(&mut drafts, saturation, max_items);
    push_stale_introduction_recommendation(&mut drafts, saturation, max_items);
    push_chatter_throughput_recommendation(&mut drafts, saturation, max_items);
    push_stale_thread_recommendation(&mut drafts, saturation, max_items);
    materialize_skill_switch_recommendations(drafts, max_items)
}

fn push_few_new_bug_recommendation(
    drafts: &mut SkillSwitchRecommendationDrafts,
    saturation: &SwarmActivitySaturationSignals,
    max_items: usize,
) {
    if saturation.has_signal(SwarmActivitySaturationSignal::FewNewBugs) {
        push_skill_switch_recommendation(
            drafts,
            saturation,
            max_items,
            SkillSwitchRecommendationInput {
                mode: SwarmActivitySkillSwitchMode::MockCodeFinder,
                confidence: SwarmActivityRecommendationConfidence::Medium,
                reason: format!(
                    "{} newly filed bugs in {} ms suggests broad review is not producing concrete findings.",
                    saturation.new_bug_count, saturation.window_ms
                ),
                confidence_reason: "medium: few-new-bugs signal should redirect review into concrete placeholder/mock discovery.",
                evidence_prefixes: &["new_bug_window:"],
            },
        );
    }
}

fn push_repeated_blocker_recommendation(
    drafts: &mut SkillSwitchRecommendationDrafts,
    saturation: &SwarmActivitySaturationSignals,
    max_items: usize,
) {
    if saturation.has_signal(SwarmActivitySaturationSignal::RepeatedBlockers) {
        push_skill_switch_recommendation(
            drafts,
            saturation,
            max_items,
            SkillSwitchRecommendationInput {
                mode: SwarmActivitySkillSwitchMode::TestingConformanceHarnesses,
                confidence: SwarmActivityRecommendationConfidence::High,
                reason: format!(
                    "{} repeated blocker observations need one reproducible contract fixture.",
                    saturation.repeated_blocker_count
                ),
                confidence_reason: "high: repeated blocker fingerprints are grouped from bounded redacted evidence.",
                evidence_prefixes: &["repeated_blocker:"],
            },
        );
    }
}

fn push_duplicate_work_recommendation(
    drafts: &mut SkillSwitchRecommendationDrafts,
    saturation: &SwarmActivitySaturationSignals,
    max_items: usize,
) {
    if saturation.has_signal(SwarmActivitySaturationSignal::DuplicateWork) {
        push_skill_switch_recommendation(
            drafts,
            saturation,
            max_items,
            SkillSwitchRecommendationInput {
                mode: SwarmActivitySkillSwitchMode::TestingGoldenArtifacts,
                confidence: SwarmActivityRecommendationConfidence::High,
                reason: format!(
                    "{} duplicate-work events need a frozen target that makes ownership and completion visible.",
                    saturation.duplicate_work_count
                ),
                confidence_reason: "high: duplicate-work count crossed the configured saturation threshold.",
                evidence_prefixes: &["duplicate_work:"],
            },
        );
    }
}

fn push_closed_surface_recommendation(
    drafts: &mut SkillSwitchRecommendationDrafts,
    saturation: &SwarmActivitySaturationSignals,
    max_items: usize,
) {
    if saturation.has_signal(SwarmActivitySaturationSignal::RepeatedClosedSurfaceEdits) {
        push_skill_switch_recommendation(
            drafts,
            saturation,
            max_items,
            SkillSwitchRecommendationInput {
                mode: SwarmActivitySkillSwitchMode::NarrowImplementationBead,
                confidence: SwarmActivityRecommendationConfidence::High,
                reason: format!(
                    "{} closed-surface edit events mean new effort should move to one unclosed bead.",
                    saturation.closed_surface_edit_count
                ),
                confidence_reason: "high: closed-surface churn is direct evidence of agents working past completed surfaces.",
                evidence_prefixes: &["closed_surface_edit:"],
            },
        );
    }
}

fn push_stale_introduction_recommendation(
    drafts: &mut SkillSwitchRecommendationDrafts,
    saturation: &SwarmActivitySaturationSignals,
    max_items: usize,
) {
    if saturation.has_signal(SwarmActivitySaturationSignal::StaleIntroductionsWithoutClaims) {
        push_skill_switch_recommendation(
            drafts,
            saturation,
            max_items,
            SkillSwitchRecommendationInput {
                mode: SwarmActivitySkillSwitchMode::NarrowImplementationBead,
                confidence: SwarmActivityRecommendationConfidence::Medium,
                reason: format!(
                    "{} introductions did not turn into claims or reservations.",
                    saturation.stale_introduction_count
                ),
                confidence_reason: "medium: stale introductions are coordination evidence, so use them to narrow assignment rather than prove a code defect.",
                evidence_prefixes: &["stale_introduction:"],
            },
        );
    }
}

fn push_chatter_throughput_recommendation(
    drafts: &mut SkillSwitchRecommendationDrafts,
    saturation: &SwarmActivitySaturationSignals,
    max_items: usize,
) {
    if saturation.has_signal(SwarmActivitySaturationSignal::HighChatterLowThroughput) {
        push_skill_switch_recommendation(
            drafts,
            saturation,
            max_items,
            SkillSwitchRecommendationInput {
                mode: SwarmActivitySkillSwitchMode::Profiling,
                confidence: SwarmActivityRecommendationConfidence::High,
                reason: format!(
                    "{} Agent Mail events with {} throughput events means the run is coordination-heavy.",
                    saturation.coordination_chatter_count, saturation.throughput_event_count
                ),
                confidence_reason: "high: high-chatter/low-throughput is a direct saturation signal before raising fanout.",
                evidence_prefixes: &["coordination_window:"],
            },
        );
    }
}

fn push_stale_thread_recommendation(
    drafts: &mut SkillSwitchRecommendationDrafts,
    saturation: &SwarmActivitySaturationSignals,
    max_items: usize,
) {
    if saturation.has_signal(SwarmActivitySaturationSignal::StaleThreads) {
        push_skill_switch_recommendation(
            drafts,
            saturation,
            max_items,
            SkillSwitchRecommendationInput {
                mode: SwarmActivitySkillSwitchMode::DeadlockFinder,
                confidence: SwarmActivityRecommendationConfidence::Medium,
                reason: format!(
                    "{} stale Agent Mail threads need blocked-wait or dependency inspection.",
                    saturation.stale_thread_count
                ),
                confidence_reason: "medium: stale threads may be benign, but they are the right evidence to inspect for coordination deadlocks.",
                evidence_prefixes: &["stale_thread:"],
            },
        );
    }
}

fn materialize_skill_switch_recommendations(
    drafts: SkillSwitchRecommendationDrafts,
    max_items: usize,
) -> Vec<SwarmActivitySkillSwitchRecommendation> {
    let mut recommendations = drafts
        .into_iter()
        .map(|(mode, draft)| SwarmActivitySkillSwitchRecommendation {
            mode,
            confidence: draft.confidence,
            reason: draft.reasons.join(" "),
            confidence_reason: draft.confidence_reasons.join(" "),
            evidence_pointers: draft
                .evidence_pointers
                .into_iter()
                .take(max_items)
                .collect(),
        })
        .collect::<Vec<_>>();
    recommendations.sort_by(|left, right| {
        right
            .confidence
            .cmp(&left.confidence)
            .then_with(|| left.mode.cmp(&right.mode))
    });
    recommendations.truncate(max_items);
    recommendations
}

fn push_skill_switch_recommendation(
    drafts: &mut SkillSwitchRecommendationDrafts,
    saturation: &SwarmActivitySaturationSignals,
    max_items: usize,
    input: SkillSwitchRecommendationInput<'_>,
) {
    let draft = drafts
        .entry(input.mode)
        .or_insert_with(|| SkillSwitchRecommendationDraft::new(input.confidence));
    if input.confidence > draft.confidence {
        draft.confidence = input.confidence;
    }
    push_unique_string(&mut draft.reasons, input.reason);
    push_unique_string(
        &mut draft.confidence_reasons,
        input.confidence_reason.to_string(),
    );
    for pointer in matching_evidence_pointers(saturation, input.evidence_prefixes, max_items) {
        draft.evidence_pointers.insert(pointer);
    }
}

fn push_unique_string(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn matching_evidence_pointers(
    saturation: &SwarmActivitySaturationSignals,
    prefixes: &[&str],
    max_items: usize,
) -> Vec<String> {
    saturation
        .evidence_pointers
        .iter()
        .filter(|pointer| prefixes.iter().any(|prefix| pointer.starts_with(prefix)))
        .take(max_items)
        .cloned()
        .collect()
}

#[derive(Default)]
struct SaturationSignalCounts {
    new_bugs: u64,
    duplicate_work: u64,
    closed_surface_edits: u64,
    stale_introductions: u64,
    coordination_chatter: u64,
    throughput_events: u64,
    repeated_blockers: u64,
    stale_threads: u64,
}

#[derive(Default)]
struct SaturationEvidence {
    signals: BTreeSet<SwarmActivitySaturationSignal>,
    reasons: Vec<String>,
    evidence_pointers: Vec<String>,
}

fn saturation_signal_counts(
    entries: &[SwarmActivityLedgerEntry],
    repeated_blockers: &[SwarmActivityHotspot],
    stale_threads: &[SwarmActivityStaleThread],
    window_start_ms: Option<u64>,
) -> SaturationSignalCounts {
    let in_window = |entry: &SwarmActivityLedgerEntry| {
        window_start_ms.is_none_or(|start| entry.timestamp_ms >= start)
    };

    SaturationSignalCounts {
        new_bugs: window_start_ms.map_or(0, |start_ms| {
            count_entries(entries, |entry| {
                entry.timestamp_ms >= start_ms && is_new_bug_entry(entry)
            })
        }),
        duplicate_work: count_entries(entries, is_duplicate_work_entry),
        closed_surface_edits: count_entries(entries, is_closed_surface_edit_entry),
        stale_introductions: usize_to_u64(stale_introduction_pointers(entries, usize::MAX).len()),
        coordination_chatter: count_entries(entries, |entry| {
            in_window(entry) && is_coordination_chatter_entry(entry)
        }),
        throughput_events: count_entries(entries, |entry| {
            in_window(entry) && is_throughput_entry(entry)
        }),
        repeated_blockers: repeated_blockers.iter().map(|hotspot| hotspot.count).sum(),
        stale_threads: usize_to_u64(stale_threads.len()),
    }
}

fn saturation_signal_evidence(
    entries: &[SwarmActivityLedgerEntry],
    config: SwarmActivityDigestConfig,
    repeated_blockers: &[SwarmActivityHotspot],
    stale_threads: &[SwarmActivityStaleThread],
    counts: &SaturationSignalCounts,
    window_start_ms: Option<u64>,
) -> SaturationEvidence {
    let mut evidence = SaturationEvidence::default();
    push_new_bug_signal(entries, config, counts, window_start_ms, &mut evidence);
    push_repeated_blocker_signal(counts, repeated_blockers, &mut evidence);
    push_duplicate_work_signal(config, counts, &mut evidence);
    push_closed_surface_signal(entries, config, counts, &mut evidence);
    push_stale_introduction_signal(entries, config, counts, &mut evidence);
    push_chatter_throughput_signal(config, counts, &mut evidence);
    push_stale_thread_signal(counts, stale_threads, &mut evidence);
    evidence
}

fn push_saturation_signal<I>(
    evidence: &mut SaturationEvidence,
    signal: SwarmActivitySaturationSignal,
    reason: String,
    pointers: I,
) where
    I: IntoIterator<Item = String>,
{
    evidence.signals.insert(signal);
    evidence.reasons.push(reason);
    evidence.evidence_pointers.extend(pointers);
}

fn push_new_bug_signal(
    entries: &[SwarmActivityLedgerEntry],
    config: SwarmActivityDigestConfig,
    counts: &SaturationSignalCounts,
    window_start_ms: Option<u64>,
    evidence: &mut SaturationEvidence,
) {
    if entries.is_empty() || counts.new_bugs >= config.min_new_bugs_per_window {
        return;
    }
    push_saturation_signal(
        evidence,
        SwarmActivitySaturationSignal::FewNewBugs,
        format!(
            "few_new_bugs: {} in {} ms",
            counts.new_bugs, config.saturation_window_ms
        ),
        [format!(
            "new_bug_window:start={}",
            window_start_ms.unwrap_or(0)
        )],
    );
}

fn push_repeated_blocker_signal(
    counts: &SaturationSignalCounts,
    repeated_blockers: &[SwarmActivityHotspot],
    evidence: &mut SaturationEvidence,
) {
    if counts.repeated_blockers == 0 {
        return;
    }
    push_saturation_signal(
        evidence,
        SwarmActivitySaturationSignal::RepeatedBlockers,
        format!("repeated_blockers: {}", counts.repeated_blockers),
        repeated_blockers.iter().map(|hotspot| {
            let key = hotspot.fingerprint.as_deref().unwrap_or(&hotspot.key);
            format!("repeated_blocker:{key}={}", hotspot.count)
        }),
    );
}

fn push_duplicate_work_signal(
    config: SwarmActivityDigestConfig,
    counts: &SaturationSignalCounts,
    evidence: &mut SaturationEvidence,
) {
    if counts.duplicate_work < config.duplicate_work_threshold {
        return;
    }
    push_saturation_signal(
        evidence,
        SwarmActivitySaturationSignal::DuplicateWork,
        format!("duplicate_work: {}", counts.duplicate_work),
        [format!("duplicate_work:count={}", counts.duplicate_work)],
    );
}

fn push_closed_surface_signal(
    entries: &[SwarmActivityLedgerEntry],
    config: SwarmActivityDigestConfig,
    counts: &SaturationSignalCounts,
    evidence: &mut SaturationEvidence,
) {
    if counts.closed_surface_edits < config.closed_surface_edit_threshold {
        return;
    }
    push_saturation_signal(
        evidence,
        SwarmActivitySaturationSignal::RepeatedClosedSurfaceEdits,
        format!("closed_surface_edits: {}", counts.closed_surface_edits),
        closed_surface_edit_pointers(entries, config.max_items),
    );
}

fn push_stale_introduction_signal(
    entries: &[SwarmActivityLedgerEntry],
    config: SwarmActivityDigestConfig,
    counts: &SaturationSignalCounts,
    evidence: &mut SaturationEvidence,
) {
    if counts.stale_introductions < config.stale_introduction_threshold {
        return;
    }
    push_saturation_signal(
        evidence,
        SwarmActivitySaturationSignal::StaleIntroductionsWithoutClaims,
        format!("stale_introductions: {}", counts.stale_introductions),
        stale_introduction_pointers(entries, config.max_items),
    );
}

fn push_chatter_throughput_signal(
    config: SwarmActivityDigestConfig,
    counts: &SaturationSignalCounts,
    evidence: &mut SaturationEvidence,
) {
    if counts.coordination_chatter < config.coordination_chatter_threshold
        || counts.throughput_events > config.low_throughput_event_threshold
    {
        return;
    }
    push_saturation_signal(
        evidence,
        SwarmActivitySaturationSignal::HighChatterLowThroughput,
        format!(
            "high_chatter_low_throughput: chatter={} throughput={}",
            counts.coordination_chatter, counts.throughput_events
        ),
        [format!(
            "coordination_window:chatter={},throughput={}",
            counts.coordination_chatter, counts.throughput_events
        )],
    );
}

fn push_stale_thread_signal(
    counts: &SaturationSignalCounts,
    stale_threads: &[SwarmActivityStaleThread],
    evidence: &mut SaturationEvidence,
) {
    if counts.stale_threads == 0 {
        return;
    }
    push_saturation_signal(
        evidence,
        SwarmActivitySaturationSignal::StaleThreads,
        format!("stale_threads: {}", counts.stale_threads),
        stale_threads
            .iter()
            .map(|thread| format!("stale_thread:{}", thread.mail_thread_id)),
    );
}

fn count_entries<F>(entries: &[SwarmActivityLedgerEntry], predicate: F) -> u64
where
    F: Fn(&SwarmActivityLedgerEntry) -> bool,
{
    entries
        .iter()
        .filter(|entry| predicate(entry))
        .count()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Default)]
struct ThreadDigestAccumulator {
    last_timestamp_ms: u64,
    event_count: u64,
    last_summary: String,
}

struct BlockerFingerprint {
    fingerprint: String,
    display_key: String,
    sample: String,
}

#[derive(Default)]
struct BlockerHotspotAccumulator {
    key: String,
    sample: String,
    count: u64,
}

fn blocker_fingerprint(entry: &SwarmActivityLedgerEntry) -> BlockerFingerprint {
    let normalized = normalized_blocker_evidence(entry);
    let digest = stable_blocker_hash(&normalized);
    BlockerFingerprint {
        fingerprint: format!("{BLOCKER_FINGERPRINT_PREFIX}{digest:016x}"),
        display_key: bounded_hotspot_key(&normalized),
        sample: blocker_sample(entry),
    }
}

fn normalized_blocker_evidence(entry: &SwarmActivityLedgerEntry) -> String {
    if let Some(context) = blocker_context_evidence(entry) {
        return context;
    }
    let mut parts = vec![entry.summary.as_str()];
    for key in BLOCKER_FINGERPRINT_DETAIL_KEYS {
        if let Some(value) = entry.details().get(*key)
            && !is_sensitive_field(key)
            && value != REDACTED
        {
            parts.push(*key);
            parts.push(value.as_str());
        }
    }
    let normalized = normalize_blocker_text(&parts.join(" "));
    if normalized.is_empty() {
        "empty".to_string()
    } else {
        normalized
    }
}

fn blocker_context_evidence(entry: &SwarmActivityLedgerEntry) -> Option<String> {
    if has_blocker_diagnostic_details(entry) {
        return None;
    }
    let id = entry
        .ids
        .bead_id
        .as_deref()
        .or(entry.ids.mail_thread_id.as_deref())?;
    let status = entry
        .details()
        .get("status")
        .map_or("unknown", String::as_str);
    Some(normalize_blocker_text(&format!("id={id} status={status}")))
}

fn has_blocker_diagnostic_details(entry: &SwarmActivityLedgerEntry) -> bool {
    entry.details().keys().any(|key| {
        matches!(
            key.as_str(),
            "artifact"
                | "command"
                | "error"
                | "exit_code"
                | "file"
                | "message"
                | "path"
                | "reason"
                | "stderr"
                | "stdout"
        )
    })
}

fn blocker_sample(entry: &SwarmActivityLedgerEntry) -> String {
    let mut sample = entry.summary.clone();
    if let Some(detail) = selected_digest_detail(entry) {
        sample.push_str("; ");
        sample.push_str(&detail);
    }
    bounded_hotspot_key(&sample)
}

fn normalize_blocker_text(value: &str) -> String {
    value
        .split_whitespace()
        .filter_map(normalize_blocker_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_blocker_token(token: &str) -> Option<String> {
    let trimmed = token
        .trim_matches(|character: char| {
            !character.is_ascii_alphanumeric()
                && !matches!(
                    character,
                    '/' | '\\' | '_' | '-' | '.' | ':' | '=' | '[' | ']'
                )
        })
        .to_ascii_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    if matches!(trimmed.as_str(), "[redacted]" | "redacted") {
        return Some("<redacted>".to_string());
    }
    if let Some((key, value)) = trimmed.split_once('=') {
        let normalized = normalize_blocker_value(value);
        return Some(format!("{key}={normalized}"));
    }
    Some(normalize_blocker_value(&trimmed))
}

fn normalize_blocker_value(value: &str) -> String {
    if looks_like_path(value) {
        return "<path>".to_string();
    }
    if looks_like_network_endpoint(value) {
        return "<addr>".to_string();
    }
    if looks_like_uuid(value) {
        return "<uuid>".to_string();
    }
    if looks_like_hex_id(value) {
        return "<hex>".to_string();
    }
    if looks_like_duration(value) {
        return "<duration>".to_string();
    }
    if looks_like_large_number(value) {
        return "<num>".to_string();
    }
    match value {
        "blocked" | "blocker" | "blocking" => "block".to_string(),
        "failed" | "failure" | "failing" => "fail".to_string(),
        "timed-out" | "timedout" => "timeout".to_string(),
        _ => value.to_string(),
    }
}

fn looks_like_path(value: &str) -> bool {
    value.contains('/')
        || value.contains('\\')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with("~/")
}

fn looks_like_network_endpoint(value: &str) -> bool {
    let Some((host, port)) = value.rsplit_once(':') else {
        return false;
    };
    !host.is_empty()
        && !port.is_empty()
        && port.chars().all(|character| character.is_ascii_digit())
        && (host.contains('.') || host == "localhost" || host.starts_with('['))
}

fn looks_like_uuid(value: &str) -> bool {
    let parts = value.split('-').collect::<Vec<_>>();
    parts.len() == 5
        && [8, 4, 4, 4, 12].iter().zip(parts).all(|(len, part)| {
            part.len() == *len && part.chars().all(|character| character.is_ascii_hexdigit())
        })
}

fn looks_like_hex_id(value: &str) -> bool {
    value.len() >= 8 && value.chars().all(|character| character.is_ascii_hexdigit())
}

fn looks_like_duration(value: &str) -> bool {
    for suffix in [
        "milliseconds",
        "millisecond",
        "msecs",
        "msec",
        "ms",
        "secs",
        "sec",
        "s",
    ] {
        if let Some(number) = value.strip_suffix(suffix) {
            return !number.is_empty()
                && number
                    .chars()
                    .all(|character| character.is_ascii_digit() || character == '.');
        }
    }
    false
}

fn looks_like_large_number(value: &str) -> bool {
    value.len() >= 3 && value.chars().all(|character| character.is_ascii_digit())
}

fn stable_blocker_hash(value: &str) -> u64 {
    let mut hash = BLOCKER_FNV_OFFSET;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(BLOCKER_FNV_PRIME);
    }
    hash
}

fn prune_blocker_accumulators(
    counts: &mut BTreeMap<String, BlockerHotspotAccumulator>,
    capacity: usize,
) {
    if counts.len() <= capacity {
        return;
    }
    let keep_keys = top_blocker_hotspots(counts, capacity)
        .into_iter()
        .filter_map(|hotspot| hotspot.fingerprint)
        .collect::<BTreeSet<_>>();
    counts.retain(|fingerprint, _| keep_keys.contains(fingerprint));
}

fn top_blocker_hotspots(
    counts: &BTreeMap<String, BlockerHotspotAccumulator>,
    capacity: usize,
) -> Vec<SwarmActivityHotspot> {
    if capacity == 0 {
        return Vec::new();
    }
    let mut hotspots = counts
        .iter()
        .map(|(fingerprint, accumulator)| SwarmActivityHotspot {
            key: accumulator.key.clone(),
            count: accumulator.count,
            fingerprint: Some(fingerprint.clone()),
            sample: Some(accumulator.sample.clone()),
        })
        .collect::<Vec<_>>();
    hotspots.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| {
                left.fingerprint
                    .as_deref()
                    .unwrap_or(&left.key)
                    .cmp(right.fingerprint.as_deref().unwrap_or(&right.key))
            })
            .then_with(|| left.key.cmp(&right.key))
    });
    hotspots.truncate(capacity);
    hotspots
}

fn is_blocker_entry(entry: &SwarmActivityLedgerEntry) -> bool {
    entry_contains_any(
        entry,
        &[
            "blocked", "blocker", "failed", "failure", "stalled", "timeout",
        ],
    )
}

fn is_duplicate_work_entry(entry: &SwarmActivityLedgerEntry) -> bool {
    entry_contains_any(
        entry,
        &[
            "already claimed",
            "duplicate",
            "duplicate work",
            "same bead",
        ],
    )
}

fn is_closed_surface_edit_entry(entry: &SwarmActivityLedgerEntry) -> bool {
    let closed_signal = detail_equals(entry, "status", "closed")
        || entry_contains_any(
            entry,
            &[
                "already-closed",
                "already closed",
                "closed bead",
                "closed issue",
                "closed surface",
            ],
        );
    let surface_signal = entry_contains_any(
        entry,
        &[
            "edit",
            "edited",
            "file",
            "modified",
            "path",
            "reservation",
            "reserved",
            "surface",
            "touch",
        ],
    );
    closed_signal && surface_signal
}

fn closed_surface_edit_pointers(
    entries: &[SwarmActivityLedgerEntry],
    max_items: usize,
) -> Vec<String> {
    entries
        .iter()
        .filter(|entry| is_closed_surface_edit_entry(entry))
        .take(max_items)
        .map(|entry| {
            format!(
                "closed_surface_edit:{}",
                saturation_actor_key(entry).unwrap_or_else(|| entry.ids.correlation_id.clone())
            )
        })
        .collect()
}

fn stale_introduction_pointers(
    entries: &[SwarmActivityLedgerEntry],
    max_items: usize,
) -> Vec<String> {
    let mut introduction_keys = BTreeSet::new();
    let mut claim_keys = BTreeSet::new();
    for entry in entries {
        if is_introduction_entry(entry)
            && let Some(key) = saturation_actor_key(entry)
        {
            introduction_keys.insert(key);
        }
        if is_claim_or_reservation_entry(entry) {
            for key in saturation_claim_keys(entry) {
                claim_keys.insert(key);
            }
        }
    }

    introduction_keys
        .difference(&claim_keys)
        .take(max_items)
        .map(|key| format!("stale_introduction:{key}"))
        .collect()
}

fn is_introduction_entry(entry: &SwarmActivityLedgerEntry) -> bool {
    matches!(entry.kind, SwarmActivityKind::AgentMail)
        && entry_contains_any(entry, &["available", "hello", "intro", "introduc", "start"])
        && !entry_contains_any(
            entry,
            &[
                "claim",
                "claimed",
                "closed",
                "completed",
                "done",
                "in_progress",
                "reserved",
            ],
        )
}

fn is_claim_or_reservation_entry(entry: &SwarmActivityLedgerEntry) -> bool {
    if detail_equals(entry, "status", "closed")
        || entry_contains_any(
            entry,
            &[
                "already-closed",
                "already closed",
                "closed bead",
                "closed issue",
                "closed surface",
            ],
        )
    {
        return false;
    }

    matches!(
        entry.kind,
        SwarmActivityKind::BeadStatus | SwarmActivityKind::FileReservation
    ) && (detail_equals(entry, "status", "in_progress")
        || detail_equals(entry, "status", "claimed")
        || detail_equals(entry, "action", "claim")
        || detail_equals(entry, "action", "reserved")
        || entry_contains_any(entry, &["claim", "claimed", "in_progress", "reserved"]))
}

const fn is_coordination_chatter_entry(entry: &SwarmActivityLedgerEntry) -> bool {
    matches!(entry.kind, SwarmActivityKind::AgentMail)
}

fn is_throughput_entry(entry: &SwarmActivityLedgerEntry) -> bool {
    match entry.kind {
        SwarmActivityKind::GitCommit => true,
        SwarmActivityKind::Verification | SwarmActivityKind::RchJob => {
            detail_equals(entry, "status", "ok")
                || detail_equals(entry, "status", "pass")
                || detail_equals(entry, "status", "passed")
                || detail_equals(entry, "status", "success")
                || entry_contains_any(entry, &["passed", "success", "succeeded"])
        }
        SwarmActivityKind::BeadStatus => {
            detail_equals(entry, "status", "closed")
                || detail_equals(entry, "status", "completed")
                || entry_contains_any(entry, &["closed", "completed"])
        }
        _ => false,
    }
}

fn saturation_actor_key(entry: &SwarmActivityLedgerEntry) -> Option<String> {
    entry
        .ids
        .agent_name
        .as_deref()
        .or(entry.ids.bead_id.as_deref())
        .or(entry.ids.mail_thread_id.as_deref())
        .map(bounded_hotspot_key)
}

fn saturation_claim_keys(entry: &SwarmActivityLedgerEntry) -> Vec<String> {
    [
        entry.ids.agent_name.as_deref(),
        entry.ids.bead_id.as_deref(),
        entry.ids.mail_thread_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(bounded_hotspot_key)
    .collect()
}

fn is_new_bug_entry(entry: &SwarmActivityLedgerEntry) -> bool {
    if !matches!(entry.kind, SwarmActivityKind::BeadStatus) {
        return false;
    }
    let has_bug_signal =
        detail_equals(entry, "issue_type", "bug") || entry_contains_any(entry, &["bug"]);
    let has_open_signal = detail_equals(entry, "status", "open")
        || detail_equals(entry, "status", "created")
        || detail_equals(entry, "action", "created")
        || entry_contains_any(entry, &["filed", "created"]);
    has_bug_signal && has_open_signal
}

fn detail_equals(entry: &SwarmActivityLedgerEntry, key: &str, expected: &str) -> bool {
    entry
        .details()
        .get(key)
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

fn entry_contains_any(entry: &SwarmActivityLedgerEntry, needles: &[&str]) -> bool {
    let summary = entry.summary.to_ascii_lowercase();
    if needles.iter().any(|needle| summary.contains(needle)) {
        return true;
    }
    entry.details().iter().any(|(key, value)| {
        let key = key.to_ascii_lowercase();
        let value = value.to_ascii_lowercase();
        needles
            .iter()
            .any(|needle| key.contains(needle) || value.contains(needle))
    })
}

fn write_hotspot_section(out: &mut String, title: &str, hotspots: &[SwarmActivityHotspot]) {
    let _ = writeln!(out, "{title}:");
    if hotspots.is_empty() {
        out.push_str("- none\n");
        return;
    }
    for hotspot in hotspots {
        let _ = write!(out, "- {} ({})", hotspot.key, hotspot.count);
        if let Some(fingerprint) = &hotspot.fingerprint {
            let _ = write!(out, " fingerprint={fingerprint}");
        }
        if let Some(sample) = &hotspot.sample {
            let _ = write!(out, " sample={sample}");
        }
        out.push('\n');
    }
}

fn write_item_section(out: &mut String, title: &str, items: &[SwarmActivityDigestItem]) {
    let _ = writeln!(out, "{title}:");
    if items.is_empty() {
        out.push_str("- none\n");
        return;
    }
    for item in items {
        let _ = write!(
            out,
            "- {} {:?} {}",
            item.timestamp_ms, item.kind, item.summary
        );
        if let Some(bead_id) = &item.bead_id {
            let _ = write!(out, " bead={bead_id}");
        }
        if let Some(agent_name) = &item.agent_name {
            let _ = write!(out, " agent={agent_name}");
        }
        if let Some(detail) = &item.detail {
            let _ = write!(out, " {detail}");
        }
        out.push('\n');
    }
}

fn write_stale_thread_section(out: &mut String, threads: &[SwarmActivityStaleThread]) {
    out.push_str("Stale threads:\n");
    if threads.is_empty() {
        out.push_str("- none\n");
        return;
    }
    for thread in threads {
        let _ = writeln!(
            out,
            "- {} last={} events={} {}",
            thread.mail_thread_id,
            thread.last_timestamp_ms,
            thread.event_count,
            thread.last_summary
        );
    }
}

fn write_recommendation_section(
    out: &mut String,
    recommendations: &[SwarmActivitySkillSwitchRecommendation],
) {
    if recommendations.is_empty() {
        return;
    }
    out.push_str("Skill-switch recommendations (advisory):\n");
    for recommendation in recommendations {
        let _ = writeln!(
            out,
            "- {} ({} confidence): {}",
            recommendation.mode.label(),
            recommendation.confidence.label(),
            recommendation.reason
        );
        let _ = writeln!(out, "  confidence: {}", recommendation.confidence_reason);
        if recommendation.evidence_pointers.is_empty() {
            continue;
        }
        out.push_str("  evidence:");
        for pointer in &recommendation.evidence_pointers {
            let _ = write!(out, " {pointer}");
        }
        out.push('\n');
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BoundedLatencySamples {
    capacity: usize,
    sample_count: u64,
    buckets: BTreeMap<u64, u64>,
    min_ms: Option<u64>,
    max_ms: Option<u64>,
}

impl BoundedLatencySamples {
    const fn new(capacity: usize) -> Self {
        Self {
            capacity,
            sample_count: 0,
            buckets: BTreeMap::new(),
            min_ms: None,
            max_ms: None,
        }
    }

    fn record(&mut self, sample_ms: u64) {
        self.sample_count = self.sample_count.saturating_add(1);
        self.min_ms = Some(
            self.min_ms
                .map_or(sample_ms, |min_ms| min_ms.min(sample_ms)),
        );
        self.max_ms = Some(
            self.max_ms
                .map_or(sample_ms, |max_ms| max_ms.max(sample_ms)),
        );
        if self.capacity == 0 {
            return;
        }
        let count = self.buckets.entry(sample_ms).or_insert(0);
        *count = count.saturating_add(1);
        self.compact_to_capacity();
    }

    fn merge(&mut self, other: &Self) {
        self.sample_count = self.sample_count.saturating_add(other.sample_count);
        self.min_ms = match (self.min_ms, other.min_ms) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(left), None) => Some(left),
            (None, Some(right)) => Some(right),
            (None, None) => None,
        };
        self.max_ms = match (self.max_ms, other.max_ms) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (Some(left), None) => Some(left),
            (None, Some(right)) => Some(right),
            (None, None) => None,
        };
        if self.capacity == 0 {
            self.buckets.clear();
            return;
        }
        for (sample_ms, count) in &other.buckets {
            let target_count = self.buckets.entry(*sample_ms).or_insert(0);
            *target_count = target_count.saturating_add(*count);
        }
        self.compact_to_capacity();
    }

    fn summary(&self) -> Option<SwarmActivityLatencySummary> {
        if self.buckets.is_empty() {
            return None;
        }
        let min_ms = self.min_ms?;
        let max_ms = self.max_ms?;
        let retained_samples = self.buckets.len();
        Some(SwarmActivityLatencySummary {
            sample_count: self.sample_count,
            retained_samples,
            min_ms,
            p50_ms: percentile_bucket(&self.buckets, self.sample_count, 50),
            p95_ms: percentile_bucket(&self.buckets, self.sample_count, 95),
            p99_ms: percentile_bucket(&self.buckets, self.sample_count, 99),
            max_ms,
            rank_error_bound: self.rank_error_bound(),
        })
    }

    fn rank_error_bound(&self) -> u64 {
        let retained_samples = usize_to_u64(self.buckets.len()).max(1);
        self.sample_count.max(1).div_ceil(retained_samples)
    }

    fn compact_to_capacity(&mut self) {
        while self.buckets.len() > self.capacity {
            self.merge_closest_buckets();
        }
    }

    fn merge_closest_buckets(&mut self) {
        let mut previous_bucket = None;
        let mut closest_pair = None;
        for (sample_ms, count) in &self.buckets {
            if let Some((previous_sample_ms, previous_count)) = previous_bucket {
                let gap = sample_ms.saturating_sub(previous_sample_ms);
                let should_replace =
                    closest_pair.is_none_or(|(_, _, closest_gap)| gap < closest_gap);
                if should_replace {
                    closest_pair = Some((
                        (previous_sample_ms, previous_count),
                        (*sample_ms, *count),
                        gap,
                    ));
                }
            }
            previous_bucket = Some((*sample_ms, *count));
        }

        if let Some(((left_sample_ms, left_count), (right_sample_ms, right_count), _gap)) =
            closest_pair
        {
            self.buckets.remove(&left_sample_ms);
            self.buckets.remove(&right_sample_ms);
            let merged_count = left_count.saturating_add(right_count);
            let merged_sample_ms =
                weighted_average_ms(left_sample_ms, left_count, right_sample_ms, right_count);
            let target_count = self.buckets.entry(merged_sample_ms).or_insert(0);
            *target_count = target_count.saturating_add(merged_count);
        }
    }
}

fn increment_kind_count(counts: &mut BTreeMap<SwarmActivityKind, u64>, kind: SwarmActivityKind) {
    let count = counts.entry(kind).or_insert(0);
    *count = count.saturating_add(1);
}

fn merge_kind_counts(
    target: &mut BTreeMap<SwarmActivityKind, u64>,
    source: &BTreeMap<SwarmActivityKind, u64>,
) {
    for (kind, count) in source {
        let target_count = target.entry(*kind).or_insert(0);
        *target_count = target_count.saturating_add(*count);
    }
}

fn record_optional_hotspot(
    counts: &mut BTreeMap<String, u64>,
    value: Option<&str>,
    capacity: usize,
) {
    if let Some(value) = value {
        record_hotspot(counts, value, capacity);
    }
}

fn record_hotspot(counts: &mut BTreeMap<String, u64>, value: &str, capacity: usize) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    if capacity == 0 {
        counts.clear();
        return;
    }
    let key = bounded_hotspot_key(value);
    let count = counts.entry(key).or_insert(0);
    *count = count.saturating_add(1);
    prune_count_map(counts, capacity);
}

fn merge_count_map(
    target: &mut BTreeMap<String, u64>,
    source: &BTreeMap<String, u64>,
    capacity: usize,
) {
    if capacity == 0 {
        target.clear();
        return;
    }
    for (key, count) in source {
        let target_count = target.entry(key.clone()).or_insert(0);
        *target_count = target_count.saturating_add(*count);
    }
    prune_count_map(target, capacity);
}

fn prune_count_map(counts: &mut BTreeMap<String, u64>, capacity: usize) {
    if capacity == 0 {
        counts.clear();
        return;
    }
    if counts.len() <= capacity {
        return;
    }
    let keep_keys = top_hotspots(counts, capacity)
        .into_iter()
        .map(|hotspot| hotspot.key)
        .collect::<BTreeSet<_>>();
    counts.retain(|key, _| keep_keys.contains(key));
}

fn top_hotspots(counts: &BTreeMap<String, u64>, capacity: usize) -> Vec<SwarmActivityHotspot> {
    if capacity == 0 {
        return Vec::new();
    }
    let mut hotspots = counts
        .iter()
        .map(|(key, count)| SwarmActivityHotspot {
            key: key.clone(),
            count: *count,
            fingerprint: None,
            sample: None,
        })
        .collect::<Vec<_>>();
    hotspots.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.key.cmp(&right.key))
    });
    hotspots.truncate(capacity);
    hotspots
}

fn percentile_bucket(buckets: &BTreeMap<u64, u64>, sample_count: u64, percentile: u8) -> u64 {
    let target_rank = sample_count
        .saturating_mul(u64::from(percentile))
        .div_ceil(100)
        .max(1);
    let mut observed_rank = 0_u64;
    for (sample_ms, bucket_count) in buckets {
        observed_rank = observed_rank.saturating_add(*bucket_count);
        if observed_rank >= target_rank {
            return *sample_ms;
        }
    }
    buckets.keys().next_back().copied().unwrap_or(0)
}

fn weighted_average_ms(
    left_sample_ms: u64,
    left_count: u64,
    right_sample_ms: u64,
    right_count: u64,
) -> u64 {
    let total_count = u128::from(left_count).saturating_add(u128::from(right_count));
    if total_count == 0 {
        return left_sample_ms;
    }
    let weighted_total = u128::from(left_sample_ms)
        .saturating_mul(u128::from(left_count))
        .saturating_add(u128::from(right_sample_ms).saturating_mul(u128::from(right_count)));
    u64::try_from(weighted_total / total_count).unwrap_or(u64::MAX)
}

fn parse_latency_ms(value: &str) -> Option<u64> {
    let trimmed = value.trim().trim_end_matches("ms").trim();
    let whole_milliseconds = trimmed
        .split_once('.')
        .map_or(trimmed, |(whole, _fractional)| whole);
    if whole_milliseconds.is_empty() {
        return None;
    }
    whole_milliseconds.parse::<u64>().ok()
}

fn bounded_hotspot_key(value: &str) -> String {
    let mut bounded = String::new();
    for (index, character) in value.chars().enumerate() {
        if index == HOTSPOT_KEY_MAX_CHARS {
            bounded.push_str("...");
            return bounded;
        }
        bounded.push(character);
    }
    bounded
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn redact_entry(
    summary: String,
    details: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
) -> (String, BTreeMap<String, String>, SwarmActivityRedaction) {
    let mut redaction = SwarmActivityRedaction::default();
    let summary = redact_value("summary", summary, &mut redaction);
    let mut redacted_details = BTreeMap::new();
    for (key, value) in details {
        let key = key.into();
        let value = redact_value(&key, value.into(), &mut redaction);
        redacted_details.insert(key, value);
    }
    (summary, redacted_details, redaction)
}

fn redact_value(field: &str, value: String, redaction: &mut SwarmActivityRedaction) -> String {
    if is_sensitive_field(field) || looks_sensitive(&value) {
        redaction.record(field);
        REDACTED.to_string()
    } else {
        value
    }
}

fn is_sensitive_field(field: &str) -> bool {
    let normalized = field.to_ascii_lowercase();
    SENSITIVE_KEY_FRAGMENTS
        .iter()
        .any(|fragment| normalized.contains(fragment))
}

fn looks_sensitive(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase();
    normalized.contains("bearer ")
        || normalized.contains("sk-")
        || normalized.contains("api_key")
        || normalized.contains("password=")
        || normalized.contains("token=")
}

#[cfg(test)]
mod tests {
    use super::{
        BLOCKER_FINGERPRINT_PREFIX, SWARM_ACTIVITY_DIGEST_SCHEMA, SWARM_ACTIVITY_LEDGER_SCHEMA,
        SWARM_ACTIVITY_SUMMARY_SCHEMA, SwarmActivityDigestConfig, SwarmActivityIds,
        SwarmActivityKind, SwarmActivityLedger, SwarmActivityLedgerError,
        SwarmActivitySaturationSignal, SwarmActivitySketch, SwarmActivitySkillSwitchMode,
        SwarmActivitySummaryConfig, digest_from_jsonl, entries_from_jsonl, timeline_from_jsonl,
    };

    #[test]
    fn exports_versioned_jsonl_with_correlation_ids() {
        let mut ledger = SwarmActivityLedger::new();
        let sequence = ledger.append(
            1_000,
            SwarmActivityKind::BeadStatus,
            SwarmActivityIds::new("corr-1")
                .with_bead_id("bd-123")
                .with_agent_name("CopperOx"),
            "claimed bd-123",
            [("status", "in_progress")],
        );

        assert_eq!(sequence, 0);
        let jsonl = ledger.to_jsonl().expect("ledger should serialize");
        let entries = entries_from_jsonl(&jsonl).expect("ledger should parse");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].schema, SWARM_ACTIVITY_LEDGER_SCHEMA);
        assert_eq!(entries[0].ids.correlation_id, "corr-1");
        assert_eq!(
            entries[0].details().get("status").map(String::as_str),
            Some("in_progress")
        );
    }

    #[test]
    fn timeline_reorders_out_of_order_jsonl_deterministically() {
        let mut ledger = SwarmActivityLedger::new();
        ledger.append(
            2_000,
            SwarmActivityKind::Verification,
            SwarmActivityIds::new("corr-late").with_rch_job_id("298"),
            "verification finished",
            [("command", "cargo check --all-targets")],
        );
        ledger.append(
            1_000,
            SwarmActivityKind::AgentMail,
            SwarmActivityIds::new("corr-early").with_mail_thread_id("bd-123"),
            "start message sent",
            [("subject", "[bd-123] start")],
        );
        let lines = ledger
            .to_jsonl()
            .expect("ledger should serialize")
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let reversed = format!("{}\n{}", lines[1], lines[0]);

        let timeline = timeline_from_jsonl(&reversed).expect("timeline should parse");

        assert_eq!(timeline[0].correlation_id, "corr-early");
        assert_eq!(timeline[1].correlation_id, "corr-late");
    }

    #[test]
    fn missing_optional_fields_still_parse() {
        let raw = format!(
            "{{\"schema\":\"{SWARM_ACTIVITY_LEDGER_SCHEMA}\",\"sequence\":7,\"timestamp_ms\":42,\"kind\":\"note\",\"summary\":\"ok\",\"ids\":{{\"correlation_id\":\"corr-min\"}}}}"
        );

        let entries = entries_from_jsonl(&raw).expect("minimal entry should parse");

        assert_eq!(entries[0].ids.correlation_id, "corr-min");
        assert!(entries[0].ids.bead_id.is_none());
        assert!(entries[0].details().is_empty());
    }

    #[test]
    fn redacts_prompt_bodies_and_secret_values_by_default() {
        let mut ledger = SwarmActivityLedger::new();
        ledger.append(
            1_000,
            SwarmActivityKind::Recovery,
            SwarmActivityIds::new("corr-redact").with_agent_name("CopperOx"),
            "operator used bearer token",
            [
                ("prompt_body", "please inspect this private prompt"),
                ("api_key", "sk-test-secret"),
                ("safe_status", "recovered"),
            ],
        );

        let entry = ledger
            .entries()
            .first()
            .expect("redaction fixture should append one entry");

        assert_eq!(entry.summary, "[REDACTED]");
        assert_eq!(
            entry.details().get("prompt_body").map(String::as_str),
            Some("[REDACTED]")
        );
        assert_eq!(
            entry.details().get("api_key").map(String::as_str),
            Some("[REDACTED]")
        );
        assert_eq!(
            entry.details().get("safe_status").map(String::as_str),
            Some("recovered")
        );
        assert_eq!(entry.redaction.redacted_count, 3);
        assert!(
            entry
                .redaction
                .redacted_fields
                .contains(&"summary".to_string())
        );
        assert!(
            entry
                .redaction
                .redacted_fields
                .contains(&"prompt_body".to_string())
        );
        assert!(
            entry
                .redaction
                .redacted_fields
                .contains(&"api_key".to_string())
        );
    }

    #[test]
    fn summary_tracks_hotspots_with_fixed_capacity_without_losing_raw_entries() {
        let mut ledger = SwarmActivityLedger::new();
        for index in 0_u64..20 {
            let agent_name = if index < 8 {
                "agent-hot".to_string()
            } else {
                format!("agent-{index}")
            };
            ledger.append(
                10_000 + index,
                SwarmActivityKind::Verification,
                SwarmActivityIds::new(format!("corr-{index}"))
                    .with_agent_name(agent_name)
                    .with_bead_id(format!("bd-{index:02}")),
                format!("verification event {index}"),
                [
                    ("tool".to_string(), format!("tool-{}", index % 5)),
                    (
                        "provider".to_string(),
                        if index % 2 == 0 {
                            "openai".to_string()
                        } else {
                            "anthropic".to_string()
                        },
                    ),
                    ("latency_ms".to_string(), (index + 1).to_string()),
                ],
            );
        }

        let summary = ledger.summarize_with_config(SwarmActivitySummaryConfig::new(3, 5));

        assert_eq!(ledger.len(), 20);
        assert_eq!(summary.schema, SWARM_ACTIVITY_SUMMARY_SCHEMA);
        assert_eq!(summary.event_count, 20);
        assert_eq!(summary.agent_hotspots.len(), 3);
        assert_eq!(summary.bead_hotspots.len(), 3);
        assert_eq!(summary.tool_hotspots.len(), 3);
        assert_eq!(summary.detail_hotspots.len(), 3);
        assert_eq!(summary.agent_hotspots[0].key, "agent-hot");
        assert_eq!(summary.agent_hotspots[0].count, 8);
        assert_eq!(summary.provider_hotspots.len(), 2);
        assert!(
            summary
                .provider_hotspots
                .iter()
                .all(|hotspot| hotspot.count == 10)
        );
        let latency = summary
            .latency_ms
            .expect("latency sketch should be present");
        assert_eq!(latency.sample_count, 20);
        assert_eq!(latency.retained_samples, 5);
        assert_eq!(latency.rank_error_bound, 4);
    }

    #[test]
    fn sketches_merge_counts_and_latency_samples_across_runs() {
        let mut left_ledger = SwarmActivityLedger::new();
        left_ledger.append(
            1_000,
            SwarmActivityKind::Verification,
            SwarmActivityIds::new("left-1")
                .with_agent_name("alpha")
                .with_bead_id("bd-left"),
            "left verification 1",
            [
                ("tool".to_string(), "read".to_string()),
                ("provider".to_string(), "openai".to_string()),
                ("latency_ms".to_string(), "10".to_string()),
            ],
        );
        left_ledger.append(
            1_001,
            SwarmActivityKind::Verification,
            SwarmActivityIds::new("left-2")
                .with_agent_name("alpha")
                .with_bead_id("bd-left"),
            "left verification 2",
            [
                ("tool".to_string(), "read".to_string()),
                ("provider".to_string(), "openai".to_string()),
                ("latency_ms".to_string(), "20".to_string()),
            ],
        );

        let mut right_ledger = SwarmActivityLedger::new();
        right_ledger.append(
            2_000,
            SwarmActivityKind::AgentMail,
            SwarmActivityIds::new("right-1")
                .with_agent_name("alpha")
                .with_bead_id("bd-right"),
            "mail sent",
            [
                ("tool".to_string(), "send_message".to_string()),
                ("provider".to_string(), "agent-mail".to_string()),
                ("latency_ms".to_string(), "30".to_string()),
            ],
        );
        right_ledger.append(
            2_001,
            SwarmActivityKind::Verification,
            SwarmActivityIds::new("right-2")
                .with_agent_name("beta")
                .with_bead_id("bd-right"),
            "right verification",
            [
                ("tool".to_string(), "read".to_string()),
                ("provider".to_string(), "openai".to_string()),
                ("latency_ms".to_string(), "40".to_string()),
            ],
        );

        let config = SwarmActivitySummaryConfig::new(2, 3);
        let mut left = SwarmActivitySketch::new(config);
        left.record_entries(left_ledger.entries());
        let mut right = SwarmActivitySketch::new(config);
        right.record_entries(right_ledger.entries());

        left.merge(&right);
        let summary = left.snapshot();

        assert_eq!(summary.event_count, 4);
        assert_eq!(
            summary.kind_counts.get(&SwarmActivityKind::Verification),
            Some(&3)
        );
        assert_eq!(
            summary.kind_counts.get(&SwarmActivityKind::AgentMail),
            Some(&1)
        );
        assert_eq!(summary.agent_hotspots[0].key, "alpha");
        assert_eq!(summary.agent_hotspots[0].count, 3);
        assert_eq!(summary.tool_hotspots[0].key, "read");
        assert_eq!(summary.tool_hotspots[0].count, 3);
        let latency = summary.latency_ms.expect("merged latency should summarize");
        assert_eq!(latency.sample_count, 4);
        assert_eq!(latency.retained_samples, 3);
        assert_eq!(latency.rank_error_bound, 2);
    }

    #[test]
    fn latency_quantiles_report_rank_error_bound_after_downsampling() {
        let mut ledger = SwarmActivityLedger::new();
        for latency_ms in 1_u64..=100 {
            ledger.append(
                latency_ms,
                SwarmActivityKind::Verification,
                SwarmActivityIds::new(format!("latency-{latency_ms}")),
                "latency sample",
                [("latency_ms".to_string(), latency_ms.to_string())],
            );
        }

        let summary = ledger.summarize_with_config(SwarmActivitySummaryConfig::new(4, 10));
        let latency = summary.latency_ms.expect("latency summary should exist");

        assert_eq!(latency.sample_count, 100);
        assert_eq!(latency.retained_samples, 10);
        assert_eq!(latency.rank_error_bound, 10);
        assert_rank_within_bound(latency.p50_ms, 50, latency.rank_error_bound);
        assert_rank_within_bound(latency.p95_ms, 95, latency.rank_error_bound);
        assert_rank_within_bound(latency.p99_ms, 99, latency.rank_error_bound);
    }

    #[test]
    fn digest_handles_empty_ledgers_deterministically() {
        let ledger = SwarmActivityLedger::new();
        let digest = ledger.digest();

        assert_eq!(digest.schema, SWARM_ACTIVITY_DIGEST_SCHEMA);
        assert_eq!(digest.event_count, 0);
        assert!(digest.active_agents.is_empty());
        assert!(!digest.saturation.saturated);
        assert!(digest.to_text().contains("No swarm activity events."));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn digest_summarizes_handoff_inputs_without_prompt_content() {
        let mut ledger = SwarmActivityLedger::new();
        ledger.append(
            10_000,
            SwarmActivityKind::AgentMail,
            SwarmActivityIds::new("mail-old")
                .with_agent_name("CopperOx")
                .with_mail_thread_id("bd-old"),
            "start message sent",
            [("subject", "[bd-old] start")],
        );
        ledger.append(
            4_000_000,
            SwarmActivityKind::FileReservation,
            SwarmActivityIds::new("lease-1")
                .with_agent_name("SunnyBeacon")
                .with_bead_id("bd-2zcs5.20"),
            "reserved digest files",
            [("path", "src/swarm_activity_ledger.rs")],
        );
        ledger.append(
            4_000_100,
            SwarmActivityKind::Verification,
            SwarmActivityIds::new("verify-1")
                .with_agent_name("SunnyBeacon")
                .with_bead_id("bd-2zcs5.20"),
            "cargo check passed",
            [
                ("command", "cargo check --all-targets"),
                ("status", "passed"),
            ],
        );
        ledger.append(
            4_000_200,
            SwarmActivityKind::BeadStatus,
            SwarmActivityIds::new("bug-1")
                .with_agent_name("SunnyBeacon")
                .with_bead_id("bd-bug"),
            "filed bug for failing digest edge case",
            [
                ("issue_type", "bug"),
                ("status", "open"),
                ("prompt_body", "secret prompt text"),
            ],
        );
        ledger.append(
            4_000_300,
            SwarmActivityKind::AgentMail,
            SwarmActivityIds::new("mail-dup-1")
                .with_agent_name("OtherAgent")
                .with_mail_thread_id("bd-2zcs5.20"),
            "duplicate work noticed",
            [("status", "duplicate")],
        );
        ledger.append(
            4_000_400,
            SwarmActivityKind::AgentMail,
            SwarmActivityIds::new("mail-dup-2")
                .with_agent_name("OtherAgent")
                .with_mail_thread_id("bd-2zcs5.20"),
            "same bead duplicate work noticed",
            [("status", "duplicate")],
        );
        ledger.append(
            4_000_500,
            SwarmActivityKind::BeadStatus,
            SwarmActivityIds::new("blocked-1")
                .with_agent_name("SunnyBeacon")
                .with_bead_id("bd-blocked"),
            "blocked by UBS historical findings",
            [("status", "blocked")],
        );
        ledger.append(
            4_000_600,
            SwarmActivityKind::BeadStatus,
            SwarmActivityIds::new("blocked-2")
                .with_agent_name("SunnyBeacon")
                .with_bead_id("bd-blocked"),
            "blocker repeated in UBS staged scan",
            [("status", "blocked")],
        );

        let digest = ledger.digest_with_config(SwarmActivityDigestConfig::new(
            4, 30_000, 1_000_000, 1, 2, 2,
        ));
        let text = digest.to_text();

        assert_eq!(digest.event_count, 8);
        assert_eq!(digest.active_agents[0].key, "SunnyBeacon");
        assert_eq!(digest.file_reservations.len(), 1);
        assert_eq!(digest.verification_evidence.len(), 1);
        assert_eq!(digest.saturation.new_bug_count, 1);
        assert!(!digest.saturation.few_new_bugs);
        assert_eq!(digest.saturation.duplicate_work_count, 2);
        assert_eq!(
            digest.repeated_blockers[0].key,
            "id=bd-blocked status=block"
        );
        assert_eq!(digest.repeated_blockers[0].count, 2);
        assert!(
            digest.repeated_blockers[0]
                .fingerprint
                .as_deref()
                .is_some_and(|fingerprint| fingerprint.starts_with(BLOCKER_FINGERPRINT_PREFIX))
        );
        assert_eq!(
            digest.repeated_blockers[0].sample.as_deref(),
            Some("blocked by UBS historical findings; status=blocked")
        );
        assert_eq!(digest.stale_threads[0].mail_thread_id, "bd-old");
        assert!(digest.saturation.saturated);
        assert!(text.contains("duplicate_work: 2"));
        assert!(text.contains("fingerprint=blocker:"));
        assert!(!text.contains("secret prompt text"));
    }

    #[test]
    fn digest_groups_dynamic_validation_blockers_by_normalized_fingerprint() {
        let mut ledger = SwarmActivityLedger::new();
        ledger.append(
            1_000,
            SwarmActivityKind::Verification,
            SwarmActivityIds::new("verify-a").with_agent_name("MagentaOak"),
            "cargo check failed for pid 12345 in /data/tmp/pi_agent_rust_cargo/agent_a/target after 1200ms",
            [
                (
                    "command",
                    "cargo check --all-targets --target-dir /data/tmp/pi_agent_rust_cargo/agent_a/target",
                ),
                ("stderr", "error[E0308]: mismatched types at /data/projects/pi_agent_rust/src/lib.rs:123:45"),
                ("status", "failed"),
            ],
        );
        ledger.append(
            2_000,
            SwarmActivityKind::Verification,
            SwarmActivityIds::new("verify-b").with_agent_name("CopperOx"),
            "cargo check failed for pid 98765 in /data/tmp/pi_agent_rust_cargo/agent_b/target after 980ms",
            [
                (
                    "command",
                    "cargo check --all-targets --target-dir /data/tmp/pi_agent_rust_cargo/agent_b/target",
                ),
                ("stderr", "error[E0308]: mismatched types at /data/projects/pi_agent_rust/src/lib.rs:777:8"),
                ("status", "failure"),
            ],
        );
        ledger.append(
            3_000,
            SwarmActivityKind::Verification,
            SwarmActivityIds::new("verify-c").with_agent_name("SunnyBeacon"),
            "cargo clippy failed for pid 33333 after 750ms",
            [
                ("command", "cargo clippy --all-targets"),
                ("stderr", "error[E0599]: no method named frobnicate found"),
                ("status", "failed"),
            ],
        );

        let digest =
            ledger.digest_with_config(SwarmActivityDigestConfig::new(8, 30_000, 30_000, 1, 2, 2));

        assert_eq!(digest.repeated_blockers.len(), 1);
        let blocker = digest
            .repeated_blockers
            .first()
            .expect("one grouped blocker expected");
        assert_eq!(blocker.count, 2);
        assert!(
            blocker
                .fingerprint
                .as_deref()
                .is_some_and(|fingerprint| fingerprint.starts_with(BLOCKER_FINGERPRINT_PREFIX))
        );
        assert!(blocker.key.contains("cargo check fail"));
        assert!(blocker.key.contains("<path>"));
        assert!(blocker.key.contains("<duration>"));
        assert!(!blocker.key.contains("/data/tmp"));
        assert!(!blocker.key.contains("12345"));
        assert!(
            blocker
                .sample
                .as_deref()
                .is_some_and(|sample| sample.contains("/data/tmp/pi_agent_rust_cargo/agent_a"))
        );
        assert!(
            digest
                .saturation
                .evidence_pointers
                .iter()
                .any(|pointer| pointer.starts_with("repeated_blocker:blocker:"))
        );
    }

    #[test]
    fn digest_from_jsonl_flags_few_new_bugs_in_effort_window() {
        let mut ledger = SwarmActivityLedger::new();
        ledger.append(
            1_000,
            SwarmActivityKind::Verification,
            SwarmActivityIds::new("verify-only").with_agent_name("SunnyBeacon"),
            "cargo check passed",
            [("status", "passed")],
        );

        let jsonl = ledger.to_jsonl().expect("ledger should serialize");
        let digest = digest_from_jsonl(
            &jsonl,
            SwarmActivityDigestConfig::new(8, 60_000, 60_000, 1, 2, 2),
        )
        .expect("digest should parse");

        assert_eq!(digest.saturation.new_bug_count, 0);
        assert!(digest.saturation.few_new_bugs);
        assert!(digest.saturation.saturated);
        assert_eq!(digest.saturation.reasons[0], "few_new_bugs: 0 in 60000 ms");
    }

    #[test]
    fn digest_flags_saturation_from_stale_intros_closed_surfaces_and_chatter() {
        let mut ledger = SwarmActivityLedger::new();
        append_saturation_signal_fixture(&mut ledger);

        let digest = ledger.digest_with_config(saturation_signal_test_config());
        let text = digest.to_text();

        assert!(digest.saturation.saturated);
        assert!(
            digest
                .saturation
                .has_signal(SwarmActivitySaturationSignal::RepeatedClosedSurfaceEdits)
        );
        assert!(
            digest
                .saturation
                .has_signal(SwarmActivitySaturationSignal::StaleIntroductionsWithoutClaims)
        );
        assert!(
            digest
                .saturation
                .has_signal(SwarmActivitySaturationSignal::HighChatterLowThroughput)
        );
        assert_eq!(digest.saturation.closed_surface_edit_count, 2);
        assert_eq!(digest.saturation.stale_introduction_count, 2);
        assert_eq!(digest.saturation.coordination_chatter_count, 7);
        assert_eq!(digest.saturation.throughput_event_count, 1);
        assert!(
            digest
                .saturation
                .evidence_pointers
                .iter()
                .any(|pointer| pointer.starts_with("closed_surface_edit:"))
        );
        assert!(
            digest
                .saturation
                .evidence_pointers
                .iter()
                .any(|pointer| pointer == "stale_introduction:IdleOne")
        );
        assert!(text.contains("high_chatter_low_throughput: chatter=7 throughput=1"));
        assert!(text.contains("Saturation evidence:"));
    }

    #[test]
    fn digest_recommends_skill_switches_from_saturation_evidence() {
        let mut ledger = SwarmActivityLedger::new();
        append_saturation_signal_fixture(&mut ledger);
        append_duplicate_blocker_churn_fixture(&mut ledger);

        let digest = ledger.digest_with_config(skill_switch_test_config());
        let text = digest.to_text();

        assert!(digest.saturation.saturated);
        assert_recommendation(
            &digest.recommendations,
            SwarmActivitySkillSwitchMode::NarrowImplementationBead,
            "closed_surface_edit:",
        );
        assert_recommendation(
            &digest.recommendations,
            SwarmActivitySkillSwitchMode::TestingGoldenArtifacts,
            "duplicate_work:",
        );
        assert_recommendation(
            &digest.recommendations,
            SwarmActivitySkillSwitchMode::TestingConformanceHarnesses,
            "repeated_blocker:",
        );
        assert_recommendation(
            &digest.recommendations,
            SwarmActivitySkillSwitchMode::Profiling,
            "coordination_window:",
        );
        assert!(text.contains("Skill-switch recommendations (advisory):"));
        assert!(text.contains("testing-golden-artifacts"));
        assert!(text.contains("confidence:"));
        assert!(!text.contains("secret duplicate prompt"));
    }

    #[test]
    fn healthy_digest_has_no_skill_switch_recommendations() {
        let mut ledger = SwarmActivityLedger::new();
        ledger.append(
            1_000,
            SwarmActivityKind::BeadStatus,
            SwarmActivityIds::new("bug-healthy").with_agent_name("ActiveAgent"),
            "filed bug for concrete issue",
            [("issue_type", "bug"), ("status", "open")],
        );
        ledger.append(
            1_100,
            SwarmActivityKind::Verification,
            SwarmActivityIds::new("verify-healthy").with_agent_name("ActiveAgent"),
            "cargo test passed",
            [("status", "passed")],
        );

        let digest =
            ledger.digest_with_config(SwarmActivityDigestConfig::new(8, 60_000, 60_000, 1, 2, 2));
        let text = digest.to_text();

        assert!(!digest.saturation.saturated);
        assert!(digest.recommendations.is_empty());
        assert!(text.contains("Saturation: no"));
        assert!(!text.contains("Skill-switch recommendations"));
    }

    fn assert_recommendation(
        recommendations: &[super::SwarmActivitySkillSwitchRecommendation],
        mode: SwarmActivitySkillSwitchMode,
        evidence_prefix: &str,
    ) {
        let recommendation = recommendations
            .iter()
            .find(|recommendation| recommendation.mode == mode)
            .expect("expected skill-switch recommendation");

        assert!(!recommendation.reason.is_empty());
        assert!(!recommendation.confidence_reason.is_empty());
        assert!(
            recommendation
                .evidence_pointers
                .iter()
                .any(|pointer| pointer.starts_with(evidence_prefix))
        );
    }

    fn append_duplicate_blocker_churn_fixture(ledger: &mut SwarmActivityLedger) {
        ledger.append(
            1_800,
            SwarmActivityKind::AgentMail,
            SwarmActivityIds::new("duplicate-1")
                .with_agent_name("DuplicateOne")
                .with_mail_thread_id("bd-dup"),
            "duplicate work noticed on same bead",
            [
                ("status", "duplicate"),
                ("prompt_body", "secret duplicate prompt"),
            ],
        );
        ledger.append(
            1_900,
            SwarmActivityKind::AgentMail,
            SwarmActivityIds::new("duplicate-2")
                .with_agent_name("DuplicateTwo")
                .with_mail_thread_id("bd-dup"),
            "same bead duplicate work noticed again",
            [("status", "duplicate")],
        );
        ledger.append(
            2_000,
            SwarmActivityKind::Verification,
            SwarmActivityIds::new("blocker-1").with_agent_name("DuplicateOne"),
            "cargo check failed with provider contract mismatch",
            [
                ("command", "cargo check --all-targets"),
                ("stderr", "error[E0308]: provider contract mismatch"),
                ("status", "failed"),
            ],
        );
        ledger.append(
            2_100,
            SwarmActivityKind::Verification,
            SwarmActivityIds::new("blocker-2").with_agent_name("DuplicateTwo"),
            "cargo check failed with provider contract mismatch",
            [
                ("command", "cargo check --all-targets"),
                ("stderr", "error[E0308]: provider contract mismatch"),
                ("status", "failed"),
            ],
        );
    }

    fn append_saturation_signal_fixture(ledger: &mut SwarmActivityLedger) {
        ledger.append(
            1_000,
            SwarmActivityKind::AgentMail,
            SwarmActivityIds::new("intro-idle-1").with_agent_name("IdleOne"),
            "hello, available for work",
            [("subject", "intro")],
        );
        ledger.append(
            1_100,
            SwarmActivityKind::AgentMail,
            SwarmActivityIds::new("intro-idle-2").with_agent_name("IdleTwo"),
            "introduction only, waiting for work",
            [("subject", "intro")],
        );
        ledger.append(
            1_200,
            SwarmActivityKind::AgentMail,
            SwarmActivityIds::new("intro-active").with_agent_name("ActiveAgent"),
            "starting current bead",
            [("subject", "start")],
        );
        ledger.append(
            1_250,
            SwarmActivityKind::BeadStatus,
            SwarmActivityIds::new("claim-active").with_agent_name("ActiveAgent"),
            "claimed bd-live",
            [("status", "in_progress"), ("action", "claim")],
        );
        for index in 0_u64..4 {
            ledger.append(
                1_300 + index,
                SwarmActivityKind::AgentMail,
                SwarmActivityIds::new(format!("chatter-{index}")).with_agent_name("IdleOne"),
                "coordination chatter without closeout",
                [("subject", "status note")],
            );
        }
        ledger.append(
            1_500,
            SwarmActivityKind::FileReservation,
            SwarmActivityIds::new("closed-surface-1")
                .with_agent_name("IdleOne")
                .with_bead_id("bd-closed"),
            "reserved already-closed bead surface",
            [("path", "src/old.rs"), ("status", "closed")],
        );
        ledger.append(
            1_600,
            SwarmActivityKind::FileReservation,
            SwarmActivityIds::new("closed-surface-2")
                .with_agent_name("IdleTwo")
                .with_bead_id("bd-closed"),
            "edited closed bead surface again",
            [("path", "docs/old.md"), ("status", "closed")],
        );
        ledger.append(
            1_700,
            SwarmActivityKind::GitCommit,
            SwarmActivityIds::new("commit-1")
                .with_agent_name("ActiveAgent")
                .with_git_sha("abc123"),
            "one commit pushed",
            [("status", "success")],
        );
    }

    fn saturation_signal_test_config() -> SwarmActivityDigestConfig {
        SwarmActivityDigestConfig {
            max_items: 8,
            stale_thread_after_ms: 60_000,
            saturation_window_ms: 10_000,
            min_new_bugs_per_window: 0,
            duplicate_work_threshold: 10,
            repeated_blocker_threshold: 10,
            closed_surface_edit_threshold: 2,
            stale_introduction_threshold: 2,
            coordination_chatter_threshold: 5,
            low_throughput_event_threshold: 1,
        }
    }

    fn skill_switch_test_config() -> SwarmActivityDigestConfig {
        SwarmActivityDigestConfig {
            duplicate_work_threshold: 2,
            repeated_blocker_threshold: 2,
            ..saturation_signal_test_config()
        }
    }

    #[test]
    fn rejects_missing_correlation_id() {
        let raw = format!(
            "{{\"schema\":\"{SWARM_ACTIVITY_LEDGER_SCHEMA}\",\"sequence\":0,\"timestamp_ms\":1,\"kind\":\"note\",\"summary\":\"ok\",\"ids\":{{\"correlation_id\":\"\"}}}}"
        );

        let error = entries_from_jsonl(&raw).expect_err("empty correlation ID should fail");

        assert!(matches!(
            error,
            SwarmActivityLedgerError::MissingCorrelationId { line: 1 }
        ));
    }

    fn assert_rank_within_bound(sample: u64, expected_rank: u64, rank_error_bound: u64) {
        let lower_bound = expected_rank.saturating_sub(rank_error_bound);
        let upper_bound = expected_rank.saturating_add(rank_error_bound);
        assert!(
            (lower_bound..=upper_bound).contains(&sample),
            "sample {sample} should be within {rank_error_bound} ranks of {expected_rank}"
        );
    }
}
