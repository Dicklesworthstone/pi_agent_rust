//! Host-scale resource admission control for swarm workloads.
//!
//! The governor is intentionally conservative and dependency-light: Linux hosts
//! get live `/proc` sampling, while other platforms keep deterministic fallback
//! budgets and only enforce request-local limits such as tool-output caps.

use std::{collections::BTreeSet, fmt};

use crate::swarm_activity_ledger::{
    SwarmActivityDigest, SwarmActivityKind, SwarmActivityLedgerEntry, SwarmActivityLedgerError,
    entries_from_jsonl,
};

use serde::Serialize;
use serde_json::{Map, Value, json};

const PROC_PAGE_SIZE_BYTES: u64 = 4096;
const DEFAULT_MEMORY_BYTES: u64 = 1_073_741_824;
const DEFAULT_FD_LIMIT: u64 = 1024;
const DEFAULT_TOOL_OUTPUT_BYTES: u64 = 128 * 1024 * 1024;
const DEFAULT_MAX_QUEUE_DEPTH: usize = 256;
const DEFAULT_MIN_QUEUE_DEPTH_BUDGET: usize = 128;
const DEFAULT_MAX_QUEUE_DEPTH_BUDGET: usize = 4096;
const DEFAULT_QUEUE_DEPTH_PER_CORE: u64 = 64;
const DEFAULT_TAIL_LATENCY_ENTER_SAMPLES: usize = 3;
const DEFAULT_TAIL_LATENCY_EXIT_SAMPLES: usize = 3;
const DEFAULT_TAIL_LATENCY_RECOVERY_RATIO: f64 = 0.80;
const DEFAULT_TAIL_LATENCY_RESOURCE_PRESSURE_RATIO: f64 = 0.85;
const DEFAULT_CAPACITY_AGENT_CPU_HEADROOM_RATIO: f64 = 0.50;
const DEFAULT_CAPACITY_MEMORY_PRESSURE_RATIO: f64 = 0.70;
const DEFAULT_CAPACITY_TOOL_CONCURRENCY_PER_AGENT: u64 = 2;
const DEFAULT_CAPACITY_EXTENSION_LANE_CPU_DIVISOR: u64 = 4;
const DEFAULT_CAPACITY_RCH_FANOUT_CPU_DIVISOR: u64 = 8;
const MAX_RECOMMENDED_EXTENSION_HOSTCALL_LANES: u64 = 32;
const MAX_RECOMMENDED_RCH_FANOUT: u64 = 8;
const MIN_AGENT_MEMORY_BYTES: u64 = 256 * 1024 * 1024;
const MIN_TOOL_OUTPUT_BYTES: u64 = 1024 * 1024;
const MIN_PROCESS_BUDGET: u64 = 64;
const MIN_FD_BUDGET: u64 = 128;
const MIN_LOAD_BUDGET: f64 = 2.0;

/// Host resource budgets used by admission checks.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HostResourceBudgets {
    /// Logical CPU cores available to this process.
    pub cpu_cores: u64,
    /// Maximum acceptable one-minute load average before denial.
    pub max_load_avg_1m: f64,
    /// Maximum RSS for this process.
    pub max_rss_bytes: u64,
    /// Maximum observed process count on the host.
    pub max_processes: u64,
    /// Maximum file descriptors open by this process.
    pub max_fds: u64,
    /// Maximum tool-output bytes admitted for one hostcall.
    pub max_tool_output_bytes: u64,
    /// Maximum queued hostcalls before queue-depth pressure is unsafe.
    pub max_queue_depth: usize,
    /// Ratio at which the governor starts delaying work.
    pub backpressure_ratio: f64,
    /// Ratio at which the governor rejects work fail-closed.
    pub deny_ratio: f64,
}

impl HostResourceBudgets {
    /// Derive conservative budgets from the current host.
    #[must_use]
    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    pub fn from_host() -> Self {
        let cpu_cores = std::thread::available_parallelism()
            .ok()
            .and_then(|value| u64::try_from(value.get()).ok())
            .unwrap_or(1);
        let mem_total = read_mem_total_bytes().unwrap_or(DEFAULT_MEMORY_BYTES);
        let max_rss_bytes = (mem_total / 2).clamp(512 * 1024 * 1024, 8 * 1024 * 1024 * 1024);
        let fd_soft_limit = read_open_files_soft_limit().unwrap_or(DEFAULT_FD_LIMIT);
        let max_queue_depth = queue_depth_budget(cpu_cores);

        Self {
            cpu_cores,
            max_load_avg_1m: ((cpu_cores as f64) * 4.0).max(MIN_LOAD_BUDGET),
            max_rss_bytes,
            max_processes: cpu_cores.saturating_mul(128).max(MIN_PROCESS_BUDGET),
            max_fds: ((fd_soft_limit.saturating_mul(4)) / 5).max(MIN_FD_BUDGET),
            max_tool_output_bytes: DEFAULT_TOOL_OUTPUT_BYTES,
            max_queue_depth,
            backpressure_ratio: 0.85,
            deny_ratio: 1.10,
        }
    }

    /// Test helper for fixed budgets.
    #[must_use]
    pub const fn fixed(
        max_load_avg_1m: f64,
        max_rss_bytes: u64,
        max_processes: u64,
        max_fds: u64,
        max_tool_output_bytes: u64,
    ) -> Self {
        Self {
            cpu_cores: 1,
            max_load_avg_1m,
            max_rss_bytes,
            max_processes,
            max_fds,
            max_tool_output_bytes,
            max_queue_depth: DEFAULT_MAX_QUEUE_DEPTH,
            backpressure_ratio: 0.85,
            deny_ratio: 1.10,
        }
    }

    /// Test helper for fixed budgets with an explicit queue-depth budget.
    #[must_use]
    pub const fn fixed_with_queue_depth(
        max_load_avg_1m: f64,
        max_rss_bytes: u64,
        max_processes: u64,
        max_fds: u64,
        max_tool_output_bytes: u64,
        max_queue_depth: usize,
    ) -> Self {
        Self {
            cpu_cores: 1,
            max_load_avg_1m,
            max_rss_bytes,
            max_processes,
            max_fds,
            max_tool_output_bytes,
            max_queue_depth,
            backpressure_ratio: 0.85,
            deny_ratio: 1.10,
        }
    }

    /// Return stricter budgets for conservative fallback mode.
    #[must_use]
    pub fn conservative_fallback(&self) -> Self {
        let backpressure_ratio = (self.backpressure_ratio * 0.75).max(0.10);
        let deny_ratio = (self.deny_ratio * 0.85)
            .max(backpressure_ratio + 0.05)
            .min(self.deny_ratio);
        Self {
            cpu_cores: self.cpu_cores,
            max_load_avg_1m: conservative_f64(self.max_load_avg_1m, 0.75),
            max_rss_bytes: conservative_u64(self.max_rss_bytes, 3, 4),
            max_processes: conservative_u64(self.max_processes, 3, 4),
            max_fds: conservative_u64(self.max_fds, 3, 4),
            max_tool_output_bytes: conservative_u64(self.max_tool_output_bytes, 1, 2),
            max_queue_depth: conservative_usize(self.max_queue_depth, 1, 2),
            backpressure_ratio,
            deny_ratio,
        }
    }
}

impl Default for HostResourceBudgets {
    fn default() -> Self {
        Self::from_host()
    }
}

/// Current host sample used for one admission decision.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct HostResourceSample {
    /// One-minute load average, when available.
    pub load_avg_1m: Option<f64>,
    /// Current process RSS in bytes, when available.
    pub rss_bytes: Option<u64>,
    /// Current host process count, when available.
    pub process_count: Option<u64>,
    /// Current open file descriptor count for this process, when available.
    pub fd_count: Option<u64>,
}

impl HostResourceSample {
    /// Sample the current process/host state.
    #[must_use]
    pub fn current() -> Self {
        Self {
            load_avg_1m: read_load_avg_1m(),
            rss_bytes: read_self_rss_bytes(),
            process_count: count_proc_processes(),
            fd_count: count_self_fds(),
        }
    }
}

/// Hostcall class submitted to the resource governor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceOperationKind {
    /// Built-in or extension-provided tool call.
    Tool,
    /// Shell command hostcall.
    Exec,
    /// HTTP hostcall.
    Http,
    /// Session metadata or persistence hostcall.
    Session,
    /// Extension UI hostcall.
    Ui,
    /// Extension event hostcall.
    Events,
    /// Extension log/telemetry hostcall.
    Log,
    /// Unknown or future hostcall kind.
    Unknown,
}

/// One unit of work being considered for admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceRequest {
    /// Operation class.
    pub operation: ResourceOperationKind,
    /// Required capability label.
    pub capability: String,
    /// Estimated maximum output bytes retained by the operation.
    pub estimated_tool_output_bytes: u64,
    /// Current scheduler queue depth.
    pub queue_depth: usize,
}

impl ResourceRequest {
    /// Create a request for the given operation/capability.
    #[must_use]
    pub fn new(operation: ResourceOperationKind, capability: impl Into<String>) -> Self {
        Self {
            operation,
            capability: capability.into(),
            estimated_tool_output_bytes: 0,
            queue_depth: 1,
        }
    }

    /// Attach estimated output bytes.
    #[must_use]
    pub const fn with_estimated_tool_output_bytes(mut self, bytes: u64) -> Self {
        self.estimated_tool_output_bytes = bytes;
        self
    }

    /// Attach queue depth.
    #[must_use]
    pub const fn with_queue_depth(mut self, queue_depth: usize) -> Self {
        self.queue_depth = if queue_depth == 0 { 1 } else { queue_depth };
        self
    }
}

/// Admission action selected by the governor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionAction {
    /// Dispatch immediately.
    Admit,
    /// Delay briefly, then dispatch.
    Backpressure,
    /// Reject before dispatch.
    Deny,
}

/// Resource dimension that dominated a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceDimension {
    /// CPU load average.
    CpuLoad,
    /// Process resident memory.
    Rss,
    /// Host process count.
    Processes,
    /// Open file descriptors.
    FileDescriptors,
    /// Estimated tool output.
    ToolOutput,
    /// Scheduler queue depth.
    QueueDepth,
    /// No dimension is currently pressurized.
    None,
}

/// Stable schema for tail-latency regime telemetry.
pub const TAIL_LATENCY_REGIME_SCHEMA: &str = "pi.resource_governor.tail_latency_regime.v1";

/// Stable schema for swarm capacity recommendations.
pub const SWARM_CAPACITY_PLAN_SCHEMA: &str = "pi.resource_governor.capacity_plan.v1";

/// Stable schema for generated operator budget profile bundles.
pub const SWARM_OPERATOR_BUDGET_PROFILES_SCHEMA: &str =
    "pi.resource_governor.operator_budget_profiles.v1";

/// Stable schema for live swarm admission-controller decisions.
pub const SWARM_ADMISSION_CONTROLLER_SCHEMA: &str =
    "pi.resource_governor.swarm_admission_controller.v1";

/// Stable schema for deterministic admission replay reports.
pub const SWARM_ADMISSION_REPLAY_SCHEMA: &str = "pi.resource_governor.swarm_admission_replay.v1";

/// Stable schema for digest-to-admission replay alignment assertions.
pub const SWARM_ADMISSION_REPLAY_DIGEST_ALIGNMENT_SCHEMA: &str =
    "pi.resource_governor.swarm_admission_replay_digest_alignment.v1";

/// Stable schema for deterministic memory-pressure replay reports.
pub const SWARM_MEMORY_PRESSURE_REPLAY_SCHEMA: &str = "pi.swarm.memory_pressure_replay.v1";

/// Current tail-latency regime selected by the guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TailLatencyRegime {
    /// Observed telemetry is still within the calibrated regime.
    Calibrated,
    /// Observed telemetry has left the calibrated regime; use safer budgets.
    ConservativeFallback,
}

/// Reason conservative fallback is active or pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TailLatencyFallbackReason {
    /// p99 latency exceeded the calibrated threshold.
    P99Latency,
    /// p999 latency exceeded the calibrated threshold.
    P999Latency,
    /// Scheduler queue depth exceeded the calibrated threshold.
    QueueDepth,
    /// Host resource pressure exceeded the calibrated threshold.
    ResourcePressure,
    /// Samples are healthy, but hysteresis has not yet allowed recovery.
    HysteresisHold,
}

/// Live telemetry consumed by the tail-latency regime guard.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct TailLatencyRegimeSample {
    /// Live p99 latency in milliseconds.
    pub p99_ms: u64,
    /// Live p999 latency in milliseconds.
    pub p999_ms: u64,
    /// Current scheduler queue depth.
    pub queue_depth: usize,
    /// Highest current host resource pressure ratio.
    pub resource_pressure_ratio: f64,
}

impl TailLatencyRegimeSample {
    /// Create one live guard sample.
    #[must_use]
    pub const fn new(
        p99_ms: u64,
        p999_ms: u64,
        queue_depth: usize,
        resource_pressure_ratio: f64,
    ) -> Self {
        Self {
            p99_ms,
            p999_ms,
            queue_depth,
            resource_pressure_ratio,
        }
    }

    /// Build a guard sample from an admission decision and explicit tail latency.
    #[must_use]
    pub const fn from_admission_decision(
        p99_ms: u64,
        p999_ms: u64,
        queue_depth: usize,
        decision: &AdmissionDecision,
    ) -> Self {
        Self {
            p99_ms,
            p999_ms,
            queue_depth,
            resource_pressure_ratio: decision.dominant_ratio,
        }
    }
}

/// Calibrated thresholds and hysteresis controls for regime detection.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct TailLatencyRegimeConfig {
    /// Calibrated p99 latency threshold in milliseconds.
    pub calibrated_p99_ms: u64,
    /// Calibrated p999 latency threshold in milliseconds.
    pub calibrated_p999_ms: u64,
    /// Calibrated queue-depth threshold.
    pub max_queue_depth: usize,
    /// Calibrated host resource pressure threshold.
    pub max_resource_pressure_ratio: f64,
    /// Fraction of calibrated thresholds required before exiting fallback.
    pub recovery_ratio: f64,
    /// Consecutive violating samples required to enter fallback.
    pub enter_consecutive_samples: usize,
    /// Consecutive recovered samples required to exit fallback.
    pub exit_consecutive_samples: usize,
}

impl Default for TailLatencyRegimeConfig {
    fn default() -> Self {
        Self {
            calibrated_p99_ms: 1_000,
            calibrated_p999_ms: 5_000,
            max_queue_depth: DEFAULT_MAX_QUEUE_DEPTH,
            max_resource_pressure_ratio: DEFAULT_TAIL_LATENCY_RESOURCE_PRESSURE_RATIO,
            recovery_ratio: DEFAULT_TAIL_LATENCY_RECOVERY_RATIO,
            enter_consecutive_samples: DEFAULT_TAIL_LATENCY_ENTER_SAMPLES,
            exit_consecutive_samples: DEFAULT_TAIL_LATENCY_EXIT_SAMPLES,
        }
    }
}

impl TailLatencyRegimeConfig {
    /// Create explicit guard thresholds.
    #[must_use]
    pub const fn new(
        calibrated_p99_ms: u64,
        calibrated_p999_ms: u64,
        max_queue_depth: usize,
        max_resource_pressure_ratio: f64,
        recovery_ratio: f64,
        enter_consecutive_samples: usize,
        exit_consecutive_samples: usize,
    ) -> Self {
        Self {
            calibrated_p99_ms,
            calibrated_p999_ms,
            max_queue_depth,
            max_resource_pressure_ratio,
            recovery_ratio,
            enter_consecutive_samples,
            exit_consecutive_samples,
        }
    }

    fn normalized(self) -> Self {
        let max_resource_pressure_ratio = if self.max_resource_pressure_ratio.is_finite()
            && self.max_resource_pressure_ratio > 0.0
        {
            self.max_resource_pressure_ratio
        } else {
            DEFAULT_TAIL_LATENCY_RESOURCE_PRESSURE_RATIO
        };
        let recovery_ratio = if self.recovery_ratio.is_finite() {
            self.recovery_ratio.clamp(0.10, 1.0)
        } else {
            DEFAULT_TAIL_LATENCY_RECOVERY_RATIO
        };
        Self {
            calibrated_p99_ms: self.calibrated_p99_ms,
            calibrated_p999_ms: self.calibrated_p999_ms.max(self.calibrated_p99_ms),
            max_queue_depth: self.max_queue_depth.max(1),
            max_resource_pressure_ratio,
            recovery_ratio,
            enter_consecutive_samples: self.enter_consecutive_samples.max(1),
            exit_consecutive_samples: self.exit_consecutive_samples.max(1),
        }
    }

    fn entry_reasons(&self, sample: TailLatencyRegimeSample) -> Vec<TailLatencyFallbackReason> {
        let mut reasons = Vec::new();
        if sample.p99_ms > self.calibrated_p99_ms {
            reasons.push(TailLatencyFallbackReason::P99Latency);
        }
        if sample.p999_ms > self.calibrated_p999_ms {
            reasons.push(TailLatencyFallbackReason::P999Latency);
        }
        if sample.queue_depth > self.max_queue_depth {
            reasons.push(TailLatencyFallbackReason::QueueDepth);
        }
        if sample.resource_pressure_ratio.is_finite()
            && sample.resource_pressure_ratio > self.max_resource_pressure_ratio
        {
            reasons.push(TailLatencyFallbackReason::ResourcePressure);
        }
        reasons
    }

    fn recovery_blockers(&self, sample: TailLatencyRegimeSample) -> Vec<TailLatencyFallbackReason> {
        let mut reasons = Vec::new();
        if sample.p99_ms > scale_u64_by_ratio(self.calibrated_p99_ms, self.recovery_ratio) {
            reasons.push(TailLatencyFallbackReason::P99Latency);
        }
        if sample.p999_ms > scale_u64_by_ratio(self.calibrated_p999_ms, self.recovery_ratio) {
            reasons.push(TailLatencyFallbackReason::P999Latency);
        }
        if sample.queue_depth > scale_usize_by_ratio(self.max_queue_depth, self.recovery_ratio) {
            reasons.push(TailLatencyFallbackReason::QueueDepth);
        }
        if sample.resource_pressure_ratio.is_finite()
            && sample.resource_pressure_ratio
                > self.max_resource_pressure_ratio * self.recovery_ratio
        {
            reasons.push(TailLatencyFallbackReason::ResourcePressure);
        }
        reasons
    }
}

/// CPU and memory inventory for the host that will run the swarm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SwarmHostInventory {
    /// Intended host class for the evidence run.
    pub target_cpu_cores: u64,
    /// CPU cores actually observed on the host.
    pub observed_cpu_cores: u64,
    /// Total host memory in MiB.
    pub mem_total_mb: u64,
}

impl SwarmHostInventory {
    /// Create a host inventory snapshot.
    #[must_use]
    pub const fn new(target_cpu_cores: u64, observed_cpu_cores: u64, mem_total_mb: u64) -> Self {
        Self {
            target_cpu_cores,
            observed_cpu_cores,
            mem_total_mb,
        }
    }

    fn validate(self) -> Result<Self, SwarmCapacityPlanError> {
        if self.target_cpu_cores == 0 {
            return Err(SwarmCapacityPlanError::InvalidHostInventory(
                "target_cpu_cores",
            ));
        }
        if self.observed_cpu_cores == 0 {
            return Err(SwarmCapacityPlanError::InvalidHostInventory(
                "observed_cpu_cores",
            ));
        }
        if self.mem_total_mb == 0 {
            return Err(SwarmCapacityPlanError::InvalidHostInventory("mem_total_mb"));
        }
        self.memory_bytes()?;
        Ok(self)
    }

    fn effective_cpu_cores(self) -> u64 {
        self.target_cpu_cores.min(self.observed_cpu_cores).max(1)
    }

    fn memory_bytes(self) -> Result<u64, SwarmCapacityPlanError> {
        self.mem_total_mb
            .checked_mul(1024 * 1024)
            .ok_or(SwarmCapacityPlanError::InvalidHostInventory("mem_total_mb"))
    }
}

/// Conservative knobs used when deriving a swarm capacity plan from evidence.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SwarmCapacityPlannerConfig {
    /// Fraction of effective CPU cores initially assigned to agent loops.
    pub agent_cpu_headroom_ratio: f64,
    /// Fraction of host memory allowed before memory pressure is considered unsafe.
    pub memory_pressure_threshold_ratio: f64,
    /// Tool hostcall lanes per recommended agent.
    pub tool_concurrency_per_agent: u64,
    /// CPU divisor used to size extension hostcall lanes.
    pub extension_lane_cpu_divisor: u64,
    /// CPU divisor used to size RCH verification fanout.
    pub rch_fanout_cpu_divisor: u64,
}

impl Default for SwarmCapacityPlannerConfig {
    fn default() -> Self {
        Self {
            agent_cpu_headroom_ratio: DEFAULT_CAPACITY_AGENT_CPU_HEADROOM_RATIO,
            memory_pressure_threshold_ratio: DEFAULT_CAPACITY_MEMORY_PRESSURE_RATIO,
            tool_concurrency_per_agent: DEFAULT_CAPACITY_TOOL_CONCURRENCY_PER_AGENT,
            extension_lane_cpu_divisor: DEFAULT_CAPACITY_EXTENSION_LANE_CPU_DIVISOR,
            rch_fanout_cpu_divisor: DEFAULT_CAPACITY_RCH_FANOUT_CPU_DIVISOR,
        }
    }
}

impl SwarmCapacityPlannerConfig {
    /// Build a plan from JSONL rows emitted by the swarm performance harness.
    pub fn plan_from_jsonl(
        self,
        jsonl: &str,
        inventory: SwarmHostInventory,
    ) -> Result<SwarmCapacityPlan, SwarmCapacityPlanError> {
        let inventory = inventory.validate()?;
        let evidence = parse_capacity_evidence_jsonl(jsonl, inventory)?;
        self.plan_from_summary(evidence, inventory)
    }

    /// Build a plan from an already-validated evidence summary.
    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    pub fn plan_from_summary(
        self,
        evidence: SwarmCapacityEvidenceSummary,
        inventory: SwarmHostInventory,
    ) -> Result<SwarmCapacityPlan, SwarmCapacityPlanError> {
        let config = self.normalized();
        let inventory = inventory.validate()?;
        if evidence.complete_records == 0 {
            return Err(SwarmCapacityPlanError::MissingEvidence("swarm_metrics"));
        }
        if evidence.max_p99_ms == 0 {
            return Err(SwarmCapacityPlanError::InvalidEvidence {
                line: 0,
                field: "swarm_metrics.latency_quantiles_ms.p99",
            });
        }
        if evidence.max_p999_ms == 0 {
            return Err(SwarmCapacityPlanError::InvalidEvidence {
                line: 0,
                field: "swarm_metrics.latency_quantiles_ms.p999",
            });
        }
        if evidence.max_rss_mb == 0 {
            return Err(SwarmCapacityPlanError::InvalidEvidence {
                line: 0,
                field: "swarm_metrics.resource_usage.rss_mb",
            });
        }

        let effective_cpu_cores = inventory.effective_cpu_cores();
        let memory_bytes = inventory.memory_bytes()?;
        let observed_rss_bytes =
            mb_to_bytes(evidence.max_rss_mb, "swarm_metrics.resource_usage.rss_mb")?;
        let usable_memory_bytes =
            scale_u64_by_ratio(memory_bytes, config.memory_pressure_threshold_ratio).max(1);
        let per_agent_memory_bytes = observed_rss_bytes
            .saturating_mul(2)
            .max(MIN_AGENT_MEMORY_BYTES);
        let memory_limited_agents = usable_memory_bytes
            .checked_div(per_agent_memory_bytes)
            .unwrap_or(0)
            .max(1);
        let cpu_limited_agents =
            scale_u64_by_ratio(effective_cpu_cores, config.agent_cpu_headroom_ratio).max(1);
        let recommended_agent_concurrency = cpu_limited_agents
            .min(memory_limited_agents)
            .min(effective_cpu_cores)
            .max(1);
        let max_queue_depth =
            planned_queue_depth_budget(effective_cpu_cores, evidence.max_queue_depth);
        let queue_limited_tools = u64::try_from(max_queue_depth)
            .unwrap_or(u64::MAX)
            .saturating_div(2)
            .max(1);
        let recommended_tool_concurrency = recommended_agent_concurrency
            .saturating_mul(config.tool_concurrency_per_agent)
            .min(queue_limited_tools)
            .max(1);
        let recommended_extension_hostcall_lanes = effective_cpu_cores
            .div_ceil(config.extension_lane_cpu_divisor)
            .clamp(1, MAX_RECOMMENDED_EXTENSION_HOSTCALL_LANES)
            .min(recommended_tool_concurrency)
            .max(1);
        let recommended_rch_verification_fanout = effective_cpu_cores
            .div_ceil(config.rch_fanout_cpu_divisor)
            .clamp(1, MAX_RECOMMENDED_RCH_FANOUT)
            .min(recommended_agent_concurrency)
            .max(1);
        let max_rss_floor = observed_rss_bytes
            .saturating_mul(2)
            .max(MIN_AGENT_MEMORY_BYTES);
        let max_rss_bytes = max_rss_floor.clamp(1, usable_memory_bytes);
        let max_tool_output_bytes =
            (max_rss_bytes / 8).clamp(MIN_TOOL_OUTPUT_BYTES, DEFAULT_TOOL_OUTPUT_BYTES);
        let max_processes = recommended_agent_concurrency
            .saturating_mul(64)
            .saturating_add(recommended_rch_verification_fanout.saturating_mul(8))
            .max(MIN_PROCESS_BUDGET);
        let max_fds = recommended_tool_concurrency
            .saturating_mul(128)
            .clamp(MIN_FD_BUDGET, DEFAULT_FD_LIMIT.saturating_mul(4));
        let resource_budgets = HostResourceBudgets {
            cpu_cores: effective_cpu_cores,
            max_load_avg_1m: ((effective_cpu_cores as f64) * 2.0).max(MIN_LOAD_BUDGET),
            max_rss_bytes,
            max_processes,
            max_fds,
            max_tool_output_bytes,
            max_queue_depth,
            backpressure_ratio: config.memory_pressure_threshold_ratio.clamp(0.50, 0.85),
            deny_ratio: 1.0,
        };
        let calibrated_p99_ms = scale_u64_by_ratio(evidence.max_p99_ms, 1.50).max(100);
        let calibrated_p999_ms =
            scale_u64_by_ratio(evidence.max_p999_ms, 1.50).max(calibrated_p99_ms.saturating_mul(2));
        let tail_latency_config = TailLatencyRegimeConfig::new(
            calibrated_p99_ms,
            calibrated_p999_ms,
            max_queue_depth,
            DEFAULT_TAIL_LATENCY_RESOURCE_PRESSURE_RATIO,
            DEFAULT_TAIL_LATENCY_RECOVERY_RATIO,
            DEFAULT_TAIL_LATENCY_ENTER_SAMPLES,
            DEFAULT_TAIL_LATENCY_EXIT_SAMPLES,
        )
        .normalized();
        let uncertainties = capacity_uncertainties(
            &evidence,
            max_rss_floor > usable_memory_bytes,
            max_queue_depth == DEFAULT_MIN_QUEUE_DEPTH_BUDGET,
        );
        let confidence = capacity_confidence(&evidence, &uncertainties);

        Ok(SwarmCapacityPlan {
            schema: SWARM_CAPACITY_PLAN_SCHEMA,
            host_inventory: inventory,
            recommended_agent_concurrency,
            recommended_tool_concurrency,
            recommended_extension_hostcall_lanes,
            recommended_rch_verification_fanout,
            memory_pressure_threshold_ratio: config.memory_pressure_threshold_ratio,
            backoff_initial_ms: evidence.max_p99_ms.clamp(50, 500),
            backoff_max_ms: evidence
                .max_p999_ms
                .max(evidence.max_p99_ms.saturating_mul(2))
                .clamp(500, 5_000),
            resource_budgets,
            tail_latency_config,
            confidence,
            uncertainties,
            evidence,
        })
    }

    fn normalized(self) -> Self {
        Self {
            agent_cpu_headroom_ratio: normalized_ratio(
                self.agent_cpu_headroom_ratio,
                DEFAULT_CAPACITY_AGENT_CPU_HEADROOM_RATIO,
                0.10,
                1.0,
            ),
            memory_pressure_threshold_ratio: normalized_ratio(
                self.memory_pressure_threshold_ratio,
                DEFAULT_CAPACITY_MEMORY_PRESSURE_RATIO,
                0.10,
                0.90,
            ),
            tool_concurrency_per_agent: self.tool_concurrency_per_agent.max(1),
            extension_lane_cpu_divisor: self.extension_lane_cpu_divisor.max(1),
            rch_fanout_cpu_divisor: self.rch_fanout_cpu_divisor.max(1),
        }
    }
}

/// Build a capacity plan from JSONL rows with default conservative knobs.
pub fn plan_swarm_capacity_from_jsonl(
    jsonl: &str,
    inventory: SwarmHostInventory,
) -> Result<SwarmCapacityPlan, SwarmCapacityPlanError> {
    SwarmCapacityPlannerConfig::default().plan_from_jsonl(jsonl, inventory)
}

/// Representative host classes used to generate operator starting profiles.
pub const DEFAULT_OPERATOR_HOST_CLASSES: [SwarmOperatorHostClass; 3] = [
    SwarmOperatorHostClass {
        id: "cpu16_mem64gib",
        description: "16 logical CPUs with 64 GiB RAM",
        inventory: SwarmHostInventory::new(16, 16, 65_536),
    },
    SwarmOperatorHostClass {
        id: "cpu32_mem128gib",
        description: "32 logical CPUs with 128 GiB RAM",
        inventory: SwarmHostInventory::new(32, 32, 131_072),
    },
    SwarmOperatorHostClass {
        id: "cpu64_mem256gib",
        description: "64 logical CPUs with 256 GiB RAM",
        inventory: SwarmHostInventory::new(64, 64, 262_144),
    },
];

/// One host class that should receive an operator budget profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SwarmOperatorHostClass {
    /// Stable profile identifier.
    pub id: &'static str,
    /// Human-readable profile description.
    pub description: &'static str,
    /// Host inventory used for the derived plan.
    pub inventory: SwarmHostInventory,
}

/// Generate the default operator budget profiles from swarm performance JSONL.
pub fn generate_operator_budget_profiles_from_jsonl(
    jsonl: &str,
    source_inventory: SwarmHostInventory,
) -> Result<SwarmOperatorBudgetProfiles, SwarmCapacityPlanError> {
    generate_operator_budget_profiles_from_jsonl_with_host_classes(
        jsonl,
        source_inventory,
        &DEFAULT_OPERATOR_HOST_CLASSES,
    )
}

/// Generate operator budget profiles for explicit host classes.
pub fn generate_operator_budget_profiles_from_jsonl_with_host_classes(
    jsonl: &str,
    source_inventory: SwarmHostInventory,
    host_classes: &[SwarmOperatorHostClass],
) -> Result<SwarmOperatorBudgetProfiles, SwarmCapacityPlanError> {
    if host_classes.is_empty() {
        return Err(SwarmCapacityPlanError::MissingEvidence(
            "operator_host_classes",
        ));
    }

    let source_plan = plan_swarm_capacity_from_jsonl(jsonl, source_inventory)?;
    let mut profiles = Vec::with_capacity(host_classes.len());
    for host_class in host_classes {
        profiles.push(SwarmOperatorBudgetProfile::from_plan(
            host_class,
            source_inventory,
            &source_plan.what_if(host_class.inventory.validate()?)?,
        ));
    }

    Ok(SwarmOperatorBudgetProfiles {
        schema: SWARM_OPERATOR_BUDGET_PROFILES_SCHEMA,
        source_inventory,
        source_plan_confidence: source_plan.confidence,
        evidence: source_plan.evidence,
        profiles,
    })
}

/// Confidence assigned to a generated swarm capacity plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmCapacityConfidence {
    /// Multiple complete rows match the requested host inventory.
    High,
    /// Evidence is usable but has limited row count or minor caveats.
    Medium,
    /// Evidence is complete enough to plan from, but material caveats remain.
    Low,
}

/// Bounded summary of the swarm performance evidence consumed by the planner.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SwarmCapacityEvidenceSummary {
    /// Complete `swarm_metrics` records consumed.
    pub complete_records: usize,
    /// Rows with a complete `host_capacity` section.
    pub host_capacity_rows: usize,
    /// Host-capacity rows that did not match the requested inventory.
    pub host_capacity_mismatch_rows: usize,
    /// Maximum p99 latency observed in milliseconds.
    pub max_p99_ms: u64,
    /// Maximum p999 latency observed in milliseconds.
    pub max_p999_ms: u64,
    /// Maximum queue depth observed.
    pub max_queue_depth: usize,
    /// Maximum RSS observed in MiB.
    pub max_rss_mb: u64,
    /// Maximum CPU utilization percentage observed.
    pub max_cpu_pct: f64,
}

/// Recommended starting budgets for a host-scale agent swarm.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SwarmCapacityPlan {
    /// Stable schema identifier for serialized plans.
    pub schema: &'static str,
    /// Host inventory the plan targets.
    pub host_inventory: SwarmHostInventory,
    /// Recommended number of concurrently active agents.
    pub recommended_agent_concurrency: u64,
    /// Recommended tool hostcall concurrency.
    pub recommended_tool_concurrency: u64,
    /// Recommended extension hostcall lanes.
    pub recommended_extension_hostcall_lanes: u64,
    /// Recommended concurrent RCH verification jobs.
    pub recommended_rch_verification_fanout: u64,
    /// Memory pressure ratio used for backpressure budgets.
    pub memory_pressure_threshold_ratio: f64,
    /// Initial backoff delay in milliseconds when pressure starts.
    pub backoff_initial_ms: u64,
    /// Maximum backoff delay in milliseconds under sustained pressure.
    pub backoff_max_ms: u64,
    /// Budgets that can be passed directly into [`ResourceGovernor`].
    pub resource_budgets: HostResourceBudgets,
    /// Tail-latency thresholds that can be passed into [`TailLatencyRegimeGuard`].
    pub tail_latency_config: TailLatencyRegimeConfig,
    /// Planner confidence after evidence validation.
    pub confidence: SwarmCapacityConfidence,
    /// Deterministic caveats attached to the recommendation.
    pub uncertainties: Vec<String>,
    /// Evidence summary used to derive this plan.
    pub evidence: SwarmCapacityEvidenceSummary,
}

impl SwarmCapacityPlan {
    /// Render stable JSON telemetry for the capacity plan.
    #[must_use]
    pub fn telemetry(&self) -> Value {
        json!(self)
    }

    /// Re-plan the same evidence summary against a different CPU/RAM inventory.
    pub fn what_if(&self, inventory: SwarmHostInventory) -> Result<Self, SwarmCapacityPlanError> {
        SwarmCapacityPlannerConfig::default().plan_from_summary(self.evidence.clone(), inventory)
    }
}

/// Bundle of deterministic operator budget profiles for common host classes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SwarmOperatorBudgetProfiles {
    /// Stable schema identifier.
    pub schema: &'static str,
    /// Inventory used to read and validate the source evidence.
    pub source_inventory: SwarmHostInventory,
    /// Confidence assigned to the source capacity plan before what-if replay.
    pub source_plan_confidence: SwarmCapacityConfidence,
    /// Evidence summary shared by all generated profiles.
    pub evidence: SwarmCapacityEvidenceSummary,
    /// Derived operator starting profiles.
    pub profiles: Vec<SwarmOperatorBudgetProfile>,
}

impl SwarmOperatorBudgetProfiles {
    /// Render stable JSON telemetry for generated operator budget profiles.
    #[must_use]
    pub fn telemetry(&self) -> Value {
        json!(self)
    }

    /// Return the profile with the requested stable identifier.
    #[must_use]
    pub fn profile(&self, profile_id: &str) -> Option<&SwarmOperatorBudgetProfile> {
        self.profiles
            .iter()
            .find(|profile| profile.profile_id == profile_id)
    }
}

/// Operator starting profile derived from a validated capacity plan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SwarmOperatorBudgetProfile {
    /// Stable profile identifier.
    pub profile_id: &'static str,
    /// Human-readable profile description.
    pub description: &'static str,
    /// Host inventory targeted by the profile.
    pub host_inventory: SwarmHostInventory,
    /// Recommended number of concurrently active agents.
    pub recommended_agent_concurrency: u64,
    /// Recommended tool hostcall concurrency.
    pub recommended_tool_concurrency: u64,
    /// Recommended extension hostcall lanes.
    pub recommended_extension_hostcall_lanes: u64,
    /// Recommended concurrent RCH verification jobs.
    pub recommended_rch_verification_fanout: u64,
    /// Memory pressure threshold used for backpressure.
    pub memory_pressure_threshold_ratio: f64,
    /// Initial backoff delay in milliseconds.
    pub backoff_initial_ms: u64,
    /// Maximum backoff delay in milliseconds.
    pub backoff_max_ms: u64,
    /// Host-resource budgets that can seed [`ResourceGovernor`].
    pub resource_budgets: HostResourceBudgets,
    /// Tail-latency guard settings for this host class.
    pub tail_latency_config: TailLatencyRegimeConfig,
    /// Confidence for this operator profile.
    pub confidence: SwarmCapacityConfidence,
    /// Caveats that must be displayed with the profile.
    pub caveats: Vec<String>,
}

impl SwarmOperatorBudgetProfile {
    fn from_plan(
        host_class: &SwarmOperatorHostClass,
        source_inventory: SwarmHostInventory,
        plan: &SwarmCapacityPlan,
    ) -> Self {
        let derived_from_source = plan.host_inventory != source_inventory;
        let confidence = operator_profile_confidence(plan.confidence, derived_from_source);
        let caveats = operator_profile_caveats(plan, source_inventory, derived_from_source);
        Self {
            profile_id: host_class.id,
            description: host_class.description,
            host_inventory: plan.host_inventory,
            recommended_agent_concurrency: plan.recommended_agent_concurrency,
            recommended_tool_concurrency: plan.recommended_tool_concurrency,
            recommended_extension_hostcall_lanes: plan.recommended_extension_hostcall_lanes,
            recommended_rch_verification_fanout: plan.recommended_rch_verification_fanout,
            memory_pressure_threshold_ratio: plan.memory_pressure_threshold_ratio,
            backoff_initial_ms: plan.backoff_initial_ms,
            backoff_max_ms: plan.backoff_max_ms,
            resource_budgets: plan.resource_budgets.clone(),
            tail_latency_config: plan.tail_latency_config,
            confidence,
            caveats,
        }
    }
}

/// Live swarm load counts compared against a capacity plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SwarmLiveLoad {
    /// Currently active agent loops.
    pub active_agents: u64,
    /// Currently active tool/hostcall work items.
    pub active_tool_calls: u64,
    /// Currently configured extension hostcall lanes.
    pub extension_hostcall_lanes: u64,
    /// Currently active RCH verification jobs.
    pub active_rch_jobs: u64,
}

impl SwarmLiveLoad {
    /// Create an empty live-load snapshot.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            active_agents: 0,
            active_tool_calls: 0,
            extension_hostcall_lanes: 0,
            active_rch_jobs: 0,
        }
    }

    /// Attach the current active agent count.
    #[must_use]
    pub const fn with_active_agents(mut self, active_agents: u64) -> Self {
        self.active_agents = active_agents;
        self
    }

    /// Attach the current active tool/hostcall count.
    #[must_use]
    pub const fn with_active_tool_calls(mut self, active_tool_calls: u64) -> Self {
        self.active_tool_calls = active_tool_calls;
        self
    }

    /// Attach the current extension hostcall lane count.
    #[must_use]
    pub const fn with_extension_hostcall_lanes(mut self, extension_hostcall_lanes: u64) -> Self {
        self.extension_hostcall_lanes = extension_hostcall_lanes;
        self
    }

    /// Attach the current active RCH verification job count.
    #[must_use]
    pub const fn with_active_rch_jobs(mut self, active_rch_jobs: u64) -> Self {
        self.active_rch_jobs = active_rch_jobs;
        self
    }
}

impl Default for SwarmLiveLoad {
    fn default() -> Self {
        Self::empty()
    }
}

/// Capacity-plan dimension that dominated a live swarm decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmCapacityDimension {
    /// Active agent loops.
    ActiveAgents,
    /// Active tool/hostcall work.
    ActiveToolCalls,
    /// Extension hostcall lanes.
    ExtensionHostcallLanes,
    /// Active RCH verification jobs.
    RchVerificationFanout,
    /// No capacity dimension is currently pressurized.
    None,
}

/// Capacity pressure selected from live swarm load counts.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SwarmCapacityPressure {
    /// Capacity-plan dimension with the highest observed ratio.
    pub dimension: SwarmCapacityDimension,
    /// Observed live count for the dimension.
    pub observed: u64,
    /// Planned budget for the dimension.
    pub budget: u64,
    /// Observed divided by planned budget.
    pub ratio: f64,
}

impl SwarmCapacityPressure {
    const fn none() -> Self {
        Self {
            dimension: SwarmCapacityDimension::None,
            observed: 0,
            budget: 0,
            ratio: 0.0,
        }
    }
}

/// Stateful live admission controller built from a swarm capacity plan.
#[derive(Debug, Clone)]
pub struct SwarmAdmissionController {
    plan: SwarmCapacityPlan,
    governor: ResourceGovernor,
    tail_latency_guard: TailLatencyRegimeGuard,
}

impl SwarmAdmissionController {
    /// Build a live controller from a pre-validated capacity plan.
    #[must_use]
    pub fn from_plan(plan: SwarmCapacityPlan) -> Self {
        Self {
            governor: ResourceGovernor::with_budgets(plan.resource_budgets.clone()),
            tail_latency_guard: TailLatencyRegimeGuard::new(plan.tail_latency_config),
            plan,
        }
    }

    /// Return the capacity plan that owns this controller's budgets.
    #[must_use]
    pub const fn plan(&self) -> &SwarmCapacityPlan {
        &self.plan
    }

    /// Return the controller's current tail-latency regime.
    #[must_use]
    pub const fn tail_latency_regime(&self) -> TailLatencyRegime {
        self.tail_latency_guard.regime()
    }

    /// Evaluate one request against live resource, latency, and capacity state.
    pub fn decide(
        &mut self,
        request: &ResourceRequest,
        sample: HostResourceSample,
        tail_latency_sample: TailLatencyRegimeSample,
        live_load: SwarmLiveLoad,
    ) -> SwarmAdmissionControllerDecision {
        let (resource_decision, tail_latency_decision) =
            self.governor.admit_sample_with_tail_latency_guard(
                request,
                sample,
                &mut self.tail_latency_guard,
                tail_latency_sample,
            );
        let capacity_pressure = live_capacity_pressure(&live_load, &self.plan);
        let capacity_action = capacity_action(
            capacity_pressure.ratio,
            self.plan.resource_budgets.backpressure_ratio,
            self.plan.resource_budgets.deny_ratio,
        );
        let action = most_restrictive_action(resource_decision.action, capacity_action);
        let retry_after_ms = controller_retry_after_ms(
            action,
            resource_decision.retry_after_ms,
            capacity_action,
            capacity_pressure.ratio,
            &self.plan,
        );
        let reason = controller_reason(
            action,
            capacity_action,
            capacity_pressure,
            &resource_decision,
        );

        SwarmAdmissionControllerDecision {
            schema: SWARM_ADMISSION_CONTROLLER_SCHEMA,
            action,
            reason,
            retry_after_ms,
            capacity_pressure,
            live_load,
            resource_decision,
            tail_latency_decision,
            plan_confidence: self.plan.confidence,
            recommended_agent_concurrency: self.plan.recommended_agent_concurrency,
            recommended_tool_concurrency: self.plan.recommended_tool_concurrency,
            recommended_extension_hostcall_lanes: self.plan.recommended_extension_hostcall_lanes,
            recommended_rch_verification_fanout: self.plan.recommended_rch_verification_fanout,
        }
    }
}

/// Result of one live swarm admission-controller evaluation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SwarmAdmissionControllerDecision {
    /// Stable schema identifier.
    pub schema: &'static str,
    /// Final selected action after resource and capacity checks.
    pub action: AdmissionAction,
    /// Human-readable reason for the final action.
    pub reason: String,
    /// Delay to apply for [`AdmissionAction::Backpressure`].
    pub retry_after_ms: u64,
    /// Capacity dimension with the highest live pressure.
    pub capacity_pressure: SwarmCapacityPressure,
    /// Live load input used by the controller.
    pub live_load: SwarmLiveLoad,
    /// Underlying host-resource admission decision.
    pub resource_decision: AdmissionDecision,
    /// Tail-latency regime decision used for fallback budgets.
    pub tail_latency_decision: TailLatencyRegimeDecision,
    /// Planner confidence attached to the active budget.
    pub plan_confidence: SwarmCapacityConfidence,
    /// Active-agent budget copied from the plan for telemetry consumers.
    pub recommended_agent_concurrency: u64,
    /// Tool-concurrency budget copied from the plan for telemetry consumers.
    pub recommended_tool_concurrency: u64,
    /// Extension-hostcall lane budget copied from the plan for telemetry consumers.
    pub recommended_extension_hostcall_lanes: u64,
    /// RCH fanout budget copied from the plan for telemetry consumers.
    pub recommended_rch_verification_fanout: u64,
}

impl SwarmAdmissionControllerDecision {
    /// Render stable JSON telemetry for the live controller decision.
    #[must_use]
    pub fn telemetry(&self) -> Value {
        json!(self)
    }
}

/// Action selected by deterministic memory-pressure replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmMemoryPressureReplayAction {
    /// No degradation was needed.
    Continue,
    /// Compact session messages before admitting more work.
    CompactMessages,
    /// Trim or reject retained tool output before it grows further.
    TrimToolOutput,
    /// Throttle extension hostcall lanes or queued extension work.
    ThrottleExtensionHostcalls,
    /// Delay new work until pressure falls.
    Backpressure,
    /// Reject new work before OOM or unbounded buffering risk.
    Deny,
}

/// Verdict assigned to a memory-pressure replay profile or report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmMemoryPressureReplayVerdict {
    /// Replay matched the expected profile behavior.
    Pass,
    /// Replay evidence did not match the expected fail-closed policy.
    FailClosed,
}

/// One deterministic memory-pressure replay profile.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SwarmMemoryPressureReplayProfile {
    /// Stable profile identifier.
    pub profile_id: &'static str,
    /// Human-readable profile description.
    pub description: &'static str,
    /// Host inventory used to derive profile budgets.
    pub host_inventory: SwarmHostInventory,
    /// Captured host-resource sample.
    pub host_resource_sample: HostResourceSample,
    /// Captured tail-latency and queue-depth sample.
    pub tail_latency_sample: TailLatencyRegimeSample,
    /// Captured live swarm load counters.
    pub live_load: SwarmLiveLoad,
    /// Retained transcript/message volume in tokens.
    pub message_volume_tokens: u64,
    /// Retained tool-output volume in bytes.
    pub retained_tool_output_bytes: u64,
    /// Buffered extension workload volume in bytes.
    pub extension_workload_bytes: u64,
    /// Expected admission action for this profile.
    pub expected_admission_action: AdmissionAction,
}

/// Budgets used while replaying one memory-pressure profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SwarmMemoryPressureReplayBudgets {
    /// Maximum RSS budget from the derived capacity plan.
    pub max_rss_bytes: u64,
    /// Maximum retained message tokens before compaction pressure.
    pub max_message_tokens: u64,
    /// Maximum retained tool-output bytes.
    pub max_tool_output_bytes: u64,
    /// Maximum buffered extension workload bytes.
    pub max_extension_workload_bytes: u64,
    /// Recommended active agent count from the derived plan.
    pub recommended_agent_concurrency: u64,
    /// Recommended tool hostcall concurrency from the derived plan.
    pub recommended_tool_concurrency: u64,
    /// Recommended extension hostcall lanes from the derived plan.
    pub recommended_extension_hostcall_lanes: u64,
    /// Recommended RCH verification fanout from the derived plan.
    pub recommended_rch_verification_fanout: u64,
}

/// One memory-pressure replay decision.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SwarmMemoryPressureReplayDecision {
    /// Stable profile identifier.
    pub profile_id: &'static str,
    /// Human-readable profile description.
    pub description: &'static str,
    /// Host inventory used for this replay.
    pub host_inventory: SwarmHostInventory,
    /// Derived budgets used by the replay.
    pub budgets: SwarmMemoryPressureReplayBudgets,
    /// Message-volume pressure ratio.
    pub message_pressure_ratio: f64,
    /// Tool-output pressure ratio.
    pub tool_output_pressure_ratio: f64,
    /// Extension-workload pressure ratio.
    pub extension_workload_pressure_ratio: f64,
    /// Expected admission action for this profile.
    pub expected_admission_action: AdmissionAction,
    /// Actual admission action selected by the replay.
    pub admission_action: AdmissionAction,
    /// True when the replay rejected work before OOM or unbounded buffering risk.
    pub fail_closed: bool,
    /// Profile verdict after comparing expected and actual action.
    pub verdict: SwarmMemoryPressureReplayVerdict,
    /// Ordered degradation and admission actions.
    pub actions: Vec<SwarmMemoryPressureReplayAction>,
    /// Human-readable replay reasons.
    pub reasons: Vec<String>,
    /// Full underlying admission-controller decision.
    pub admission_decision: SwarmAdmissionControllerDecision,
}

/// Deterministic memory-pressure replay report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SwarmMemoryPressureReplayReport {
    /// Stable schema identifier.
    pub schema: &'static str,
    /// Overall report verdict.
    pub verdict: SwarmMemoryPressureReplayVerdict,
    /// Inventory used to parse source capacity evidence.
    pub source_inventory: SwarmHostInventory,
    /// Confidence assigned to the source capacity plan.
    pub source_plan_confidence: SwarmCapacityConfidence,
    /// Number of profiles replayed.
    pub profile_count: usize,
    /// Ordered decisions for each memory-pressure profile.
    pub decisions: Vec<SwarmMemoryPressureReplayDecision>,
}

impl SwarmMemoryPressureReplayReport {
    /// Render stable JSON telemetry for the memory-pressure replay report.
    #[must_use]
    pub fn telemetry(&self) -> Value {
        json!(self)
    }
}

/// Replay memory-pressure profiles against capacity-derived admission budgets.
pub fn replay_swarm_memory_pressure_profiles(
    jsonl: &str,
    source_inventory: SwarmHostInventory,
    profiles: &[SwarmMemoryPressureReplayProfile],
) -> Result<SwarmMemoryPressureReplayReport, SwarmCapacityPlanError> {
    if profiles.is_empty() {
        return Err(SwarmCapacityPlanError::MissingEvidence(
            "memory_pressure_profiles",
        ));
    }

    let source_plan = plan_swarm_capacity_from_jsonl(jsonl, source_inventory)?;
    let mut decisions = Vec::with_capacity(profiles.len());
    for profile in profiles {
        validate_memory_pressure_profile(profile)?;
        let plan = source_plan.what_if(profile.host_inventory)?;
        let budgets = memory_pressure_budgets(&plan);
        let request = memory_pressure_profile_request(profile);
        let mut controller = SwarmAdmissionController::from_plan(plan);
        let admission_decision = controller.decide(
            &request,
            profile.host_resource_sample,
            profile.tail_latency_sample,
            profile.live_load,
        );
        let message_pressure_ratio =
            pressure_ratio_u64(profile.message_volume_tokens, budgets.max_message_tokens);
        let tool_output_pressure_ratio = pressure_ratio_u64(
            profile.retained_tool_output_bytes,
            budgets.max_tool_output_bytes,
        );
        let extension_workload_pressure_ratio = pressure_ratio_u64(
            profile.extension_workload_bytes,
            budgets.max_extension_workload_bytes,
        );
        let actions = memory_pressure_actions(profile, &budgets, admission_decision.action);
        let verdict = memory_pressure_profile_verdict(
            profile.expected_admission_action,
            admission_decision.action,
        );
        let reasons =
            memory_pressure_reasons(profile, &actions, admission_decision.action, verdict);

        decisions.push(SwarmMemoryPressureReplayDecision {
            profile_id: profile.profile_id,
            description: profile.description,
            host_inventory: profile.host_inventory,
            budgets,
            message_pressure_ratio,
            tool_output_pressure_ratio,
            extension_workload_pressure_ratio,
            expected_admission_action: profile.expected_admission_action,
            admission_action: admission_decision.action,
            fail_closed: matches!(admission_decision.action, AdmissionAction::Deny),
            verdict,
            actions,
            reasons,
            admission_decision,
        });
    }

    let verdict = if decisions.iter().any(|decision| {
        matches!(
            decision.verdict,
            SwarmMemoryPressureReplayVerdict::FailClosed
        )
    }) {
        SwarmMemoryPressureReplayVerdict::FailClosed
    } else {
        SwarmMemoryPressureReplayVerdict::Pass
    };

    Ok(SwarmMemoryPressureReplayReport {
        schema: SWARM_MEMORY_PRESSURE_REPLAY_SCHEMA,
        verdict,
        source_inventory,
        source_plan_confidence: source_plan.confidence,
        profile_count: decisions.len(),
        decisions,
    })
}

/// Status for a deterministic swarm admission replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmAdmissionReplayStatus {
    /// Every replayable ledger event had fresh resource evidence.
    Pass,
    /// One or more replayable events lacked trustworthy evidence.
    FailClosed,
}

/// Divergence marker emitted while replaying an admission ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmAdmissionReplayDivergenceKind {
    /// A correlation ID appeared more than once in replayable work.
    DuplicateCorrelationId,
    /// No resource sample was available at or before the event timestamp.
    MissingResourceSample,
    /// The newest available resource sample was older than the replay policy.
    StaleResourceSample,
    /// An optional expected action field was present but not understood.
    InvalidExpectedAction,
    /// A captured expected action differs from the replayed decision.
    ExpectedActionMismatch,
}

/// One deterministic replay divergence marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SwarmAdmissionReplayDivergence {
    /// Divergence category.
    pub kind: SwarmAdmissionReplayDivergenceKind,
    /// Event timestamp in Unix milliseconds.
    pub timestamp_ms: u64,
    /// Stable correlation ID for the affected ledger event.
    pub correlation_id: String,
    /// Human-readable detail for operator output.
    pub detail: String,
}

/// Captured evidence sample used by admission replay.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SwarmAdmissionReplaySample {
    /// Sample timestamp in Unix milliseconds.
    pub timestamp_ms: u64,
    /// Captured host-resource sample.
    pub host_resource_sample: HostResourceSample,
    /// Captured tail-latency and queue-depth sample.
    pub tail_latency_sample: TailLatencyRegimeSample,
    /// Captured live swarm load counters.
    pub live_load: SwarmLiveLoad,
}

impl SwarmAdmissionReplaySample {
    /// Create one replay sample from captured host, latency, and live-load state.
    #[must_use]
    pub const fn new(
        timestamp_ms: u64,
        host_resource_sample: HostResourceSample,
        tail_latency_sample: TailLatencyRegimeSample,
        live_load: SwarmLiveLoad,
    ) -> Self {
        Self {
            timestamp_ms,
            host_resource_sample,
            tail_latency_sample,
            live_load,
        }
    }
}

/// Configuration for deterministic admission replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SwarmAdmissionReplayConfig {
    /// Maximum age allowed for the selected resource sample.
    pub max_sample_age_ms: u64,
}

impl SwarmAdmissionReplayConfig {
    /// Build a replay config with an explicit sample freshness bound.
    #[must_use]
    pub const fn new(max_sample_age_ms: u64) -> Self {
        Self { max_sample_age_ms }
    }
}

impl Default for SwarmAdmissionReplayConfig {
    fn default() -> Self {
        Self {
            max_sample_age_ms: 60_000,
        }
    }
}

/// One replayed admission decision attached to an activity-ledger event.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SwarmAdmissionReplayDecision {
    /// Original ledger sequence.
    pub sequence: u64,
    /// Event timestamp in Unix milliseconds.
    pub timestamp_ms: u64,
    /// Replayed activity kind.
    pub kind: SwarmActivityKind,
    /// Stable event correlation ID.
    pub correlation_id: String,
    /// Resource-sample timestamp used by the replay.
    pub sample_timestamp_ms: u64,
    /// Age of the selected sample at replay time.
    pub sample_age_ms: u64,
    /// Optional expected action captured by the ledger fixture.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_action: Option<AdmissionAction>,
    /// Final replay decision.
    pub admission_decision: SwarmAdmissionControllerDecision,
    /// Capacity pressure selected by the replay decision.
    pub dominant_pressure: SwarmCapacityPressure,
    /// Divergence markers attached to this event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub divergence_markers: Vec<SwarmAdmissionReplayDivergence>,
}

/// Deterministic replay report for one activity-ledger incident.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SwarmAdmissionReplayReport {
    /// Stable schema identifier.
    pub schema: &'static str,
    /// Overall replay status.
    pub status: SwarmAdmissionReplayStatus,
    /// Number of ledger events considered replayable work.
    pub replayed_event_count: usize,
    /// Number of decisions emitted with fresh captured samples.
    pub decision_count: usize,
    /// Decision timeline sorted by ledger timestamp, sequence, and correlation ID.
    pub decision_timeline: Vec<SwarmAdmissionReplayDecision>,
    /// Divergence markers gathered across the replay.
    pub divergence_markers: Vec<SwarmAdmissionReplayDivergence>,
}

impl SwarmAdmissionReplayReport {
    /// Render stable JSON telemetry for the replay report.
    #[must_use]
    pub fn telemetry(&self) -> Value {
        json!(self)
    }
}

/// Severity used when comparing a transcript digest to admission replay output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmAdmissionReplayDigestSeverity {
    /// Digest and replay evidence show no expansion pressure.
    Safe,
    /// Digest saturation or replay backpressure/denial shows degraded capacity.
    Degraded,
    /// Replay evidence is missing, stale, or otherwise cannot be trusted.
    FailClosed,
}

/// Alignment mismatch between digest saturation and replayed admission severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmAdmissionReplayDigestAssertionKind {
    /// The digest reports saturation while replay stays optimistic.
    SaturatedDigestOptimisticReplay,
    /// Replay reports degraded/fail-closed admission while the digest has no saturation signal.
    UnsaturatedDigestConservativeReplay,
}

/// One actionable assertion emitted by digest/admission alignment checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SwarmAdmissionReplayDigestAssertion {
    /// Mismatch category.
    pub kind: SwarmAdmissionReplayDigestAssertionKind,
    /// Human-readable assertion detail.
    pub detail: String,
    /// Exact operator action recommended before increasing swarm fanout.
    pub recommended_operator_action: String,
}

/// Deterministic bridge between transcript saturation and admission replay evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SwarmAdmissionReplayDigestAlignment {
    /// Stable schema identifier.
    pub schema: &'static str,
    /// Pass only when digest severity and replay severity agree.
    pub status: SwarmAdmissionReplayStatus,
    /// Severity derived from the transcript digest.
    pub digest_severity: SwarmAdmissionReplayDigestSeverity,
    /// Severity derived from the admission replay report.
    pub replay_severity: SwarmAdmissionReplayDigestSeverity,
    /// Digest reasons carried into the assertion report.
    pub digest_saturation_reasons: Vec<String>,
    /// Redacted digest evidence pointers carried into the assertion report.
    pub digest_evidence_pointers: Vec<String>,
    /// Replay divergence marker count retained for operator triage.
    pub replay_divergence_count: usize,
    /// Actionable mismatches. Empty when status is pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actionable_assertions: Vec<SwarmAdmissionReplayDigestAssertion>,
}

impl SwarmAdmissionReplayDigestAlignment {
    /// Render stable JSON telemetry for the alignment report.
    #[must_use]
    pub fn telemetry(&self) -> Value {
        json!(self)
    }
}

/// Error returned when deterministic admission replay cannot start safely.
#[derive(Debug)]
pub enum SwarmAdmissionReplayError {
    /// Activity ledger JSONL could not be parsed or validated.
    InvalidLedger(String),
    /// Required replay evidence was absent.
    MissingEvidence(&'static str),
    /// Captured replay evidence contained unsafe or non-finite values.
    InvalidEvidence(&'static str),
}

impl From<SwarmActivityLedgerError> for SwarmAdmissionReplayError {
    fn from(source: SwarmActivityLedgerError) -> Self {
        Self::InvalidLedger(source.to_string())
    }
}

impl fmt::Display for SwarmAdmissionReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLedger(message) => write!(formatter, "invalid activity ledger: {message}"),
            Self::MissingEvidence(field) => write!(formatter, "missing replay evidence: {field}"),
            Self::InvalidEvidence(field) => write!(formatter, "invalid replay evidence: {field}"),
        }
    }
}

impl std::error::Error for SwarmAdmissionReplayError {}

/// Error returned when capacity evidence cannot safely produce recommendations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwarmCapacityPlanError {
    /// Required host inventory is absent or invalid.
    InvalidHostInventory(&'static str),
    /// A JSONL row is not valid JSON.
    InvalidJson {
        /// One-indexed JSONL line number.
        line: usize,
        /// Parser error.
        message: String,
    },
    /// A row contains `swarm_metrics`, but a required field is absent or invalid.
    InvalidEvidence {
        /// One-indexed JSONL line number, or zero for summarized evidence.
        line: usize,
        /// Required evidence field.
        field: &'static str,
    },
    /// No complete required evidence was found.
    MissingEvidence(&'static str),
}

impl fmt::Display for SwarmCapacityPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHostInventory(field) => {
                write!(formatter, "invalid host inventory field: {field}")
            }
            Self::InvalidJson { line, message } => {
                write!(formatter, "invalid JSONL at line {line}: {message}")
            }
            Self::InvalidEvidence { line, field } if *line == 0 => {
                write!(formatter, "invalid summarized evidence field: {field}")
            }
            Self::InvalidEvidence { line, field } => {
                write!(formatter, "invalid swarm evidence at line {line}: {field}")
            }
            Self::MissingEvidence(field) => {
                write!(formatter, "missing required swarm evidence: {field}")
            }
        }
    }
}

impl std::error::Error for SwarmCapacityPlanError {}

/// Result of observing one tail-latency sample.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TailLatencyRegimeDecision {
    /// Active regime after processing the sample.
    pub regime: TailLatencyRegime,
    /// True when conservative fallback should be applied.
    pub fallback_active: bool,
    /// True when this sample changed the active regime.
    pub changed: bool,
    /// Reasons fallback is active or pending.
    pub reasons: Vec<TailLatencyFallbackReason>,
    /// Consecutive violating samples seen while calibrated.
    pub bad_sample_streak: usize,
    /// Consecutive recovered samples seen while in fallback.
    pub recovery_sample_streak: usize,
    /// Sample used for this decision.
    pub sample: TailLatencyRegimeSample,
}

impl TailLatencyRegimeDecision {
    /// Render stable JSON telemetry for the regime decision.
    #[must_use]
    pub fn telemetry(&self) -> Value {
        json!({
            "schema": TAIL_LATENCY_REGIME_SCHEMA,
            "decision": self,
        })
    }

    /// Apply conservative fallback budgets only when fallback is active.
    #[must_use]
    pub fn conservative_budgets(&self, budgets: &HostResourceBudgets) -> HostResourceBudgets {
        if self.fallback_active {
            budgets.conservative_fallback()
        } else {
            budgets.clone()
        }
    }
}

/// Stateful guard that detects tail-latency regime shifts with hysteresis.
#[derive(Debug, Clone)]
pub struct TailLatencyRegimeGuard {
    config: TailLatencyRegimeConfig,
    regime: TailLatencyRegime,
    bad_sample_streak: usize,
    recovery_sample_streak: usize,
    last_reasons: Vec<TailLatencyFallbackReason>,
}

impl TailLatencyRegimeGuard {
    /// Build a guard from calibrated thresholds.
    #[must_use]
    pub fn new(config: TailLatencyRegimeConfig) -> Self {
        Self {
            config: config.normalized(),
            regime: TailLatencyRegime::Calibrated,
            bad_sample_streak: 0,
            recovery_sample_streak: 0,
            last_reasons: Vec::new(),
        }
    }

    /// Return the normalized guard config.
    #[must_use]
    pub const fn config(&self) -> TailLatencyRegimeConfig {
        self.config
    }

    /// Return the active regime.
    #[must_use]
    pub const fn regime(&self) -> TailLatencyRegime {
        self.regime
    }

    /// Observe one live telemetry sample and update the regime.
    pub fn observe(&mut self, sample: TailLatencyRegimeSample) -> TailLatencyRegimeDecision {
        let entry_reasons = self.config.entry_reasons(sample);
        let mut changed = false;

        match self.regime {
            TailLatencyRegime::Calibrated => {
                self.recovery_sample_streak = 0;
                if entry_reasons.is_empty() {
                    self.bad_sample_streak = 0;
                    self.last_reasons.clear();
                } else {
                    self.bad_sample_streak = self.bad_sample_streak.saturating_add(1);
                    self.last_reasons.clone_from(&entry_reasons);
                    if self.bad_sample_streak >= self.config.enter_consecutive_samples {
                        self.regime = TailLatencyRegime::ConservativeFallback;
                        self.recovery_sample_streak = 0;
                        changed = true;
                    }
                }
            }
            TailLatencyRegime::ConservativeFallback => {
                self.bad_sample_streak = 0;
                let blockers = self.config.recovery_blockers(sample);
                if blockers.is_empty() {
                    self.recovery_sample_streak = self.recovery_sample_streak.saturating_add(1);
                    if self.recovery_sample_streak >= self.config.exit_consecutive_samples {
                        self.regime = TailLatencyRegime::Calibrated;
                        self.bad_sample_streak = 0;
                        self.recovery_sample_streak = 0;
                        self.last_reasons.clear();
                        changed = true;
                    } else {
                        self.last_reasons = vec![TailLatencyFallbackReason::HysteresisHold];
                    }
                } else {
                    self.recovery_sample_streak = 0;
                    self.last_reasons = blockers;
                }
            }
        }

        let fallback_active = matches!(self.regime, TailLatencyRegime::ConservativeFallback);
        let reasons = if fallback_active {
            self.last_reasons.clone()
        } else {
            entry_reasons
        };
        TailLatencyRegimeDecision {
            regime: self.regime,
            fallback_active,
            changed,
            reasons,
            bad_sample_streak: self.bad_sample_streak,
            recovery_sample_streak: self.recovery_sample_streak,
            sample,
        }
    }
}

/// Result of one admission check.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AdmissionDecision {
    /// Selected action.
    pub action: AdmissionAction,
    /// Resource dimension with the highest budget ratio.
    pub dominant_dimension: ResourceDimension,
    /// Highest observed budget ratio.
    pub dominant_ratio: f64,
    /// Human-readable reason for telemetry and errors.
    pub reason: String,
    /// Delay to apply for [`AdmissionAction::Backpressure`].
    pub retry_after_ms: u64,
    /// Sample used by this decision.
    pub sample: HostResourceSample,
    /// Budgets used by this decision.
    pub budgets: HostResourceBudgets,
    /// True when the decision used conservative fallback budgets.
    #[serde(skip_serializing_if = "is_false")]
    pub conservative_fallback_active: bool,
    /// Tail-latency fallback reasons applied to this decision.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fallback_reasons: Vec<TailLatencyFallbackReason>,
}

impl AdmissionDecision {
    /// Render a stable JSON telemetry payload.
    #[must_use]
    pub fn telemetry(&self, request: &ResourceRequest) -> Value {
        json!({
            "schema": "pi.resource_governor.admission.v1",
            "request": request,
            "decision": self,
        })
    }
}

/// Stateless resource governor.
#[derive(Debug, Clone)]
pub struct ResourceGovernor {
    budgets: HostResourceBudgets,
}

impl ResourceGovernor {
    /// Build a governor from host-derived budgets.
    #[must_use]
    pub fn from_host() -> Self {
        Self {
            budgets: HostResourceBudgets::from_host(),
        }
    }

    /// Build a governor with explicit budgets.
    #[must_use]
    pub const fn with_budgets(budgets: HostResourceBudgets) -> Self {
        Self { budgets }
    }

    /// Return the active budgets.
    #[must_use]
    pub const fn budgets(&self) -> &HostResourceBudgets {
        &self.budgets
    }

    /// Evaluate a request against the live host sample.
    #[must_use]
    pub fn admit(&self, request: &ResourceRequest) -> AdmissionDecision {
        self.admit_sample(request, HostResourceSample::current())
    }

    /// Evaluate a request against an injected sample.
    #[must_use]
    pub fn admit_sample(
        &self,
        request: &ResourceRequest,
        sample: HostResourceSample,
    ) -> AdmissionDecision {
        Self::admit_sample_with_budgets(request, sample, self.budgets.clone(), false, Vec::new())
    }

    /// Evaluate a request using conservative budgets when a regime guard is in fallback.
    #[must_use]
    pub fn admit_sample_with_tail_latency_decision(
        &self,
        request: &ResourceRequest,
        sample: HostResourceSample,
        regime_decision: &TailLatencyRegimeDecision,
    ) -> AdmissionDecision {
        let budgets = regime_decision.conservative_budgets(&self.budgets);
        let fallback_reasons = if regime_decision.fallback_active {
            regime_decision.reasons.clone()
        } else {
            Vec::new()
        };
        Self::admit_sample_with_budgets(
            request,
            sample,
            budgets,
            regime_decision.fallback_active,
            fallback_reasons,
        )
    }

    /// Observe live tail-latency telemetry, then evaluate admission with the selected regime.
    pub fn admit_sample_with_tail_latency_guard(
        &self,
        request: &ResourceRequest,
        sample: HostResourceSample,
        guard: &mut TailLatencyRegimeGuard,
        latency_sample: TailLatencyRegimeSample,
    ) -> (AdmissionDecision, TailLatencyRegimeDecision) {
        let regime_decision = guard.observe(latency_sample);
        let admission =
            self.admit_sample_with_tail_latency_decision(request, sample, &regime_decision);
        (admission, regime_decision)
    }

    fn admit_sample_with_budgets(
        request: &ResourceRequest,
        sample: HostResourceSample,
        budgets: HostResourceBudgets,
        conservative_fallback_active: bool,
        fallback_reasons: Vec<TailLatencyFallbackReason>,
    ) -> AdmissionDecision {
        let (dominant_dimension, dominant_ratio) = dominant_pressure(&budgets, &sample, request);
        let action = if dominant_ratio >= budgets.deny_ratio {
            AdmissionAction::Deny
        } else if dominant_ratio >= budgets.backpressure_ratio {
            AdmissionAction::Backpressure
        } else {
            AdmissionAction::Admit
        };
        let retry_after_ms = match action {
            AdmissionAction::Backpressure => retry_after_ms(dominant_ratio),
            AdmissionAction::Admit | AdmissionAction::Deny => 0,
        };
        AdmissionDecision {
            action,
            dominant_dimension,
            dominant_ratio,
            reason: decision_reason(action, dominant_dimension, dominant_ratio),
            retry_after_ms,
            sample,
            budgets,
            conservative_fallback_active,
            fallback_reasons,
        }
    }
}

impl Default for ResourceGovernor {
    fn default() -> Self {
        Self::from_host()
    }
}

fn validate_memory_pressure_profile(
    profile: &SwarmMemoryPressureReplayProfile,
) -> Result<(), SwarmCapacityPlanError> {
    if profile.profile_id.trim().is_empty() {
        return Err(SwarmCapacityPlanError::InvalidEvidence {
            line: 0,
            field: "memory_pressure_profile.profile_id",
        });
    }
    profile.host_inventory.validate()?;
    validate_capacity_optional_f64(
        profile.host_resource_sample.load_avg_1m,
        "memory_pressure_profile.host_resource_sample.load_avg_1m",
    )?;
    if profile.host_resource_sample.rss_bytes.is_none() {
        return Err(SwarmCapacityPlanError::InvalidEvidence {
            line: 0,
            field: "memory_pressure_profile.host_resource_sample.rss_bytes",
        });
    }
    if !profile
        .tail_latency_sample
        .resource_pressure_ratio
        .is_finite()
        || profile.tail_latency_sample.resource_pressure_ratio < 0.0
    {
        return Err(SwarmCapacityPlanError::InvalidEvidence {
            line: 0,
            field: "memory_pressure_profile.tail_latency_sample.resource_pressure_ratio",
        });
    }
    Ok(())
}

fn validate_capacity_optional_f64(
    value: Option<f64>,
    field: &'static str,
) -> Result<(), SwarmCapacityPlanError> {
    if value.is_some_and(|number| !number.is_finite() || number < 0.0) {
        return Err(SwarmCapacityPlanError::InvalidEvidence { line: 0, field });
    }
    Ok(())
}

fn memory_pressure_budgets(plan: &SwarmCapacityPlan) -> SwarmMemoryPressureReplayBudgets {
    SwarmMemoryPressureReplayBudgets {
        max_rss_bytes: plan.resource_budgets.max_rss_bytes,
        max_message_tokens: message_token_budget(plan.recommended_agent_concurrency),
        max_tool_output_bytes: plan.resource_budgets.max_tool_output_bytes,
        max_extension_workload_bytes: extension_workload_budget(plan),
        recommended_agent_concurrency: plan.recommended_agent_concurrency,
        recommended_tool_concurrency: plan.recommended_tool_concurrency,
        recommended_extension_hostcall_lanes: plan.recommended_extension_hostcall_lanes,
        recommended_rch_verification_fanout: plan.recommended_rch_verification_fanout,
    }
}

fn memory_pressure_profile_request(profile: &SwarmMemoryPressureReplayProfile) -> ResourceRequest {
    ResourceRequest::new(ResourceOperationKind::Tool, "memory_pressure_replay")
        .with_estimated_tool_output_bytes(profile.retained_tool_output_bytes)
        .with_queue_depth(profile.tail_latency_sample.queue_depth)
}

fn memory_pressure_actions(
    profile: &SwarmMemoryPressureReplayProfile,
    budgets: &SwarmMemoryPressureReplayBudgets,
    admission_action: AdmissionAction,
) -> Vec<SwarmMemoryPressureReplayAction> {
    let mut actions = Vec::new();
    push_pressure_action(
        &mut actions,
        profile.message_volume_tokens,
        budgets.max_message_tokens,
        SwarmMemoryPressureReplayAction::CompactMessages,
    );
    push_pressure_action(
        &mut actions,
        profile.retained_tool_output_bytes,
        budgets.max_tool_output_bytes,
        SwarmMemoryPressureReplayAction::TrimToolOutput,
    );
    push_pressure_action(
        &mut actions,
        profile.extension_workload_bytes,
        budgets.max_extension_workload_bytes,
        SwarmMemoryPressureReplayAction::ThrottleExtensionHostcalls,
    );
    match admission_action {
        AdmissionAction::Admit => {}
        AdmissionAction::Backpressure => {
            actions.push(SwarmMemoryPressureReplayAction::Backpressure);
        }
        AdmissionAction::Deny => actions.push(SwarmMemoryPressureReplayAction::Deny),
    }
    if actions.is_empty() {
        actions.push(SwarmMemoryPressureReplayAction::Continue);
    }
    actions
}

fn push_pressure_action(
    actions: &mut Vec<SwarmMemoryPressureReplayAction>,
    observed: u64,
    budget: u64,
    action: SwarmMemoryPressureReplayAction,
) {
    if observed >= scale_u64_by_ratio(budget.max(1), DEFAULT_CAPACITY_MEMORY_PRESSURE_RATIO) {
        actions.push(action);
    }
}

#[allow(clippy::cast_precision_loss)]
fn pressure_ratio_u64(observed: u64, budget: u64) -> f64 {
    if budget == 0 {
        return f64::INFINITY;
    }
    (observed as f64) / (budget as f64)
}

fn message_token_budget(recommended_agent_concurrency: u64) -> u64 {
    recommended_agent_concurrency
        .max(1)
        .saturating_mul(16_384)
        .clamp(16_384, 1_048_576)
}

fn extension_workload_budget(plan: &SwarmCapacityPlan) -> u64 {
    plan.resource_budgets
        .max_tool_output_bytes
        .saturating_mul(plan.recommended_extension_hostcall_lanes.max(1))
        .max(MIN_TOOL_OUTPUT_BYTES)
}

const fn memory_pressure_profile_verdict(
    expected: AdmissionAction,
    actual: AdmissionAction,
) -> SwarmMemoryPressureReplayVerdict {
    if admission_actions_match(expected, actual) {
        SwarmMemoryPressureReplayVerdict::Pass
    } else {
        SwarmMemoryPressureReplayVerdict::FailClosed
    }
}

fn memory_pressure_reasons(
    profile: &SwarmMemoryPressureReplayProfile,
    actions: &[SwarmMemoryPressureReplayAction],
    admission_action: AdmissionAction,
    verdict: SwarmMemoryPressureReplayVerdict,
) -> Vec<String> {
    let mut reasons = Vec::new();
    for action in actions {
        match action {
            SwarmMemoryPressureReplayAction::Continue => {
                reasons.push("profile_within_memory_pressure_budgets".to_string());
            }
            SwarmMemoryPressureReplayAction::CompactMessages => {
                reasons.push(format!(
                    "message_volume_tokens={} crossed compaction threshold",
                    profile.message_volume_tokens
                ));
            }
            SwarmMemoryPressureReplayAction::TrimToolOutput => {
                reasons.push(format!(
                    "retained_tool_output_bytes={} crossed tool-output threshold",
                    profile.retained_tool_output_bytes
                ));
            }
            SwarmMemoryPressureReplayAction::ThrottleExtensionHostcalls => {
                reasons.push(format!(
                    "extension_workload_bytes={} crossed extension workload threshold",
                    profile.extension_workload_bytes
                ));
            }
            SwarmMemoryPressureReplayAction::Backpressure => {
                reasons.push("admission controller selected backpressure".to_string());
            }
            SwarmMemoryPressureReplayAction::Deny => {
                reasons.push("admission controller rejected work fail-closed".to_string());
            }
        }
    }
    if matches!(verdict, SwarmMemoryPressureReplayVerdict::FailClosed) {
        reasons.push(format!(
            "expected {:?}, replayed {:?}",
            profile.expected_admission_action, admission_action
        ));
    }
    reasons
}

/// Replay admission decisions from redacted activity-ledger JSONL and captured samples.
///
/// # Errors
///
/// Returns an error when the ledger cannot be parsed, when no captured samples
/// are available, or when a captured sample contains non-finite evidence.
pub fn replay_swarm_admission_from_jsonl(
    ledger_jsonl: &str,
    plan: SwarmCapacityPlan,
    samples: &[SwarmAdmissionReplaySample],
    config: SwarmAdmissionReplayConfig,
) -> Result<SwarmAdmissionReplayReport, SwarmAdmissionReplayError> {
    validate_replay_samples(samples)?;

    let mut entries = entries_from_jsonl(ledger_jsonl)?;
    entries.sort_by(|left, right| {
        left.timestamp_ms
            .cmp(&right.timestamp_ms)
            .then_with(|| left.sequence.cmp(&right.sequence))
            .then_with(|| left.ids.correlation_id.cmp(&right.ids.correlation_id))
    });

    let mut controller = SwarmAdmissionController::from_plan(plan);
    let mut seen_correlation_ids = BTreeSet::new();
    let mut decision_timeline = Vec::new();
    let mut divergence_markers = Vec::new();
    let mut replayed_event_count = 0usize;

    for entry in entries
        .iter()
        .filter(|entry| is_replayable_activity(entry.kind))
    {
        replayed_event_count = replayed_event_count.saturating_add(1);
        let mut event_markers = Vec::new();
        record_duplicate_correlation_id(entry, &mut seen_correlation_ids, &mut event_markers);

        let Some(sample) = select_replay_sample(samples, entry.timestamp_ms) else {
            event_markers.push(replay_marker(
                SwarmAdmissionReplayDivergenceKind::MissingResourceSample,
                entry,
                "no captured resource sample at or before event timestamp",
            ));
            divergence_markers.extend(event_markers);
            continue;
        };

        let sample_age_ms = entry.timestamp_ms.saturating_sub(sample.timestamp_ms);
        if sample_age_ms > config.max_sample_age_ms {
            event_markers.push(replay_marker(
                SwarmAdmissionReplayDivergenceKind::StaleResourceSample,
                entry,
                format!(
                    "resource sample age {sample_age_ms}ms exceeds {}ms",
                    config.max_sample_age_ms
                ),
            ));
            divergence_markers.extend(event_markers);
            continue;
        }

        let (expected_action, invalid_expected_action) = expected_action_from_entry(entry);
        if let Some(value) = invalid_expected_action {
            event_markers.push(replay_marker(
                SwarmAdmissionReplayDivergenceKind::InvalidExpectedAction,
                entry,
                format!("unsupported expected action: {value}"),
            ));
        }

        let request = replay_request_from_entry(entry, sample.tail_latency_sample.queue_depth);
        let decision = controller.decide(
            &request,
            sample.host_resource_sample,
            sample.tail_latency_sample,
            sample.live_load,
        );

        if let Some(expected) = expected_action
            && !admission_actions_match(expected, decision.action)
        {
            event_markers.push(replay_marker(
                SwarmAdmissionReplayDivergenceKind::ExpectedActionMismatch,
                entry,
                format!("expected {:?}, replayed {:?}", expected, decision.action),
            ));
        }

        divergence_markers.extend(event_markers.clone());
        decision_timeline.push(SwarmAdmissionReplayDecision {
            sequence: entry.sequence,
            timestamp_ms: entry.timestamp_ms,
            kind: entry.kind,
            correlation_id: entry.ids.correlation_id.clone(),
            sample_timestamp_ms: sample.timestamp_ms,
            sample_age_ms,
            expected_action,
            dominant_pressure: decision.capacity_pressure,
            admission_decision: decision,
            divergence_markers: event_markers,
        });
    }

    let status = if divergence_markers
        .iter()
        .any(|marker| replay_marker_is_fail_closed(marker.kind))
    {
        SwarmAdmissionReplayStatus::FailClosed
    } else {
        SwarmAdmissionReplayStatus::Pass
    };

    Ok(SwarmAdmissionReplayReport {
        schema: SWARM_ADMISSION_REPLAY_SCHEMA,
        status,
        replayed_event_count,
        decision_count: decision_timeline.len(),
        decision_timeline,
        divergence_markers,
    })
}

/// Compare transcript-derived saturation with deterministic admission replay severity.
///
/// The returned report is a separate assertion artifact; it does not mutate or
/// extend the replay report schema.
#[must_use]
pub fn assert_swarm_digest_admission_replay_alignment(
    digest: &SwarmActivityDigest,
    replay: &SwarmAdmissionReplayReport,
) -> SwarmAdmissionReplayDigestAlignment {
    let digest_severity = digest_replay_severity_from_digest(digest);
    let replay_severity = digest_replay_severity_from_replay(replay);
    let actionable_assertions =
        digest_replay_alignment_assertions(digest, replay, digest_severity, replay_severity);
    let status = if actionable_assertions.is_empty() {
        SwarmAdmissionReplayStatus::Pass
    } else {
        SwarmAdmissionReplayStatus::FailClosed
    };

    SwarmAdmissionReplayDigestAlignment {
        schema: SWARM_ADMISSION_REPLAY_DIGEST_ALIGNMENT_SCHEMA,
        status,
        digest_severity,
        replay_severity,
        digest_saturation_reasons: digest.saturation.reasons.clone(),
        digest_evidence_pointers: digest.saturation.evidence_pointers.clone(),
        replay_divergence_count: replay.divergence_markers.len(),
        actionable_assertions,
    }
}

const fn digest_replay_severity_from_digest(
    digest: &SwarmActivityDigest,
) -> SwarmAdmissionReplayDigestSeverity {
    if digest.saturation.saturated {
        SwarmAdmissionReplayDigestSeverity::Degraded
    } else {
        SwarmAdmissionReplayDigestSeverity::Safe
    }
}

fn digest_replay_severity_from_replay(
    replay: &SwarmAdmissionReplayReport,
) -> SwarmAdmissionReplayDigestSeverity {
    if replay.status == SwarmAdmissionReplayStatus::FailClosed {
        return SwarmAdmissionReplayDigestSeverity::FailClosed;
    }
    if replay
        .decision_timeline
        .iter()
        .any(|decision| !matches!(decision.admission_decision.action, AdmissionAction::Admit))
    {
        return SwarmAdmissionReplayDigestSeverity::Degraded;
    }
    SwarmAdmissionReplayDigestSeverity::Safe
}

fn digest_replay_alignment_assertions(
    digest: &SwarmActivityDigest,
    replay: &SwarmAdmissionReplayReport,
    digest_severity: SwarmAdmissionReplayDigestSeverity,
    replay_severity: SwarmAdmissionReplayDigestSeverity,
) -> Vec<SwarmAdmissionReplayDigestAssertion> {
    use SwarmAdmissionReplayDigestSeverity::{Degraded, FailClosed, Safe};

    match (digest_severity, replay_severity) {
        (Degraded, Safe) => {
            vec![SwarmAdmissionReplayDigestAssertion {
                kind: SwarmAdmissionReplayDigestAssertionKind::SaturatedDigestOptimisticReplay,
                detail: format!(
                    "digest saturated with {} reason(s) and {} evidence pointer(s), but replay stayed safe across {} decision(s)",
                    digest.saturation.reasons.len(),
                    digest.saturation.evidence_pointers.len(),
                    replay.decision_count
                ),
                recommended_operator_action:
                    "pause new agent launches; inspect digest evidence and replay with captured resource pressure before raising fanout".to_string(),
            }]
        }
        (Safe, Degraded | FailClosed) => {
            vec![SwarmAdmissionReplayDigestAssertion {
                kind: SwarmAdmissionReplayDigestAssertionKind::UnsaturatedDigestConservativeReplay,
                detail: format!(
                    "digest has no saturation signal, but replay severity was {:?} with {} divergence marker(s)",
                    replay_severity,
                    replay.divergence_markers.len()
                ),
                recommended_operator_action:
                    "keep admission conservative; refresh captured resource samples or explain replay divergences before changing budgets".to_string(),
            }]
        }
        _ => Vec::new(),
    }
}

fn validate_replay_samples(
    samples: &[SwarmAdmissionReplaySample],
) -> Result<(), SwarmAdmissionReplayError> {
    if samples.is_empty() {
        return Err(SwarmAdmissionReplayError::MissingEvidence(
            "resource_samples",
        ));
    }
    for sample in samples {
        validate_optional_f64(sample.host_resource_sample.load_avg_1m, "load_avg_1m")?;
        if !sample
            .tail_latency_sample
            .resource_pressure_ratio
            .is_finite()
            || sample.tail_latency_sample.resource_pressure_ratio < 0.0
        {
            return Err(SwarmAdmissionReplayError::InvalidEvidence(
                "resource_pressure_ratio",
            ));
        }
    }
    Ok(())
}

fn validate_optional_f64(
    value: Option<f64>,
    field: &'static str,
) -> Result<(), SwarmAdmissionReplayError> {
    if value.is_some_and(|number| !number.is_finite() || number < 0.0) {
        return Err(SwarmAdmissionReplayError::InvalidEvidence(field));
    }
    Ok(())
}

const fn is_replayable_activity(kind: SwarmActivityKind) -> bool {
    matches!(
        kind,
        SwarmActivityKind::BeadStatus
            | SwarmActivityKind::AgentMail
            | SwarmActivityKind::FileReservation
            | SwarmActivityKind::RchJob
            | SwarmActivityKind::Verification
    )
}

fn record_duplicate_correlation_id(
    entry: &SwarmActivityLedgerEntry,
    seen_correlation_ids: &mut BTreeSet<String>,
    markers: &mut Vec<SwarmAdmissionReplayDivergence>,
) {
    if !seen_correlation_ids.insert(entry.ids.correlation_id.clone()) {
        markers.push(replay_marker(
            SwarmAdmissionReplayDivergenceKind::DuplicateCorrelationId,
            entry,
            "correlation ID was already replayed",
        ));
    }
}

fn select_replay_sample(
    samples: &[SwarmAdmissionReplaySample],
    timestamp_ms: u64,
) -> Option<&SwarmAdmissionReplaySample> {
    samples
        .iter()
        .filter(|sample| sample.timestamp_ms <= timestamp_ms)
        .max_by_key(|sample| sample.timestamp_ms)
}

fn expected_action_from_entry(
    entry: &SwarmActivityLedgerEntry,
) -> (Option<AdmissionAction>, Option<String>) {
    let Some(raw) = entry
        .details()
        .get("expected_action")
        .or_else(|| entry.details().get("expected_admission_action"))
        .or_else(|| entry.details().get("admission_action"))
    else {
        return (None, None);
    };
    parse_admission_action(raw)
        .map_or_else(|| (None, Some(raw.clone())), |action| (Some(action), None))
}

const fn admission_actions_match(left: AdmissionAction, right: AdmissionAction) -> bool {
    matches!(
        (left, right),
        (AdmissionAction::Admit, AdmissionAction::Admit)
            | (AdmissionAction::Backpressure, AdmissionAction::Backpressure)
            | (AdmissionAction::Deny, AdmissionAction::Deny)
    )
}

fn replay_request_from_entry(
    entry: &SwarmActivityLedgerEntry,
    fallback_queue_depth: usize,
) -> ResourceRequest {
    let operation = entry
        .details()
        .get("request_operation")
        .or_else(|| entry.details().get("operation"))
        .and_then(|value| parse_operation_kind(value))
        .unwrap_or_else(|| default_operation_for_activity(entry.kind));
    let capability = entry
        .details()
        .get("request_capability")
        .or_else(|| entry.details().get("capability"))
        .cloned()
        .unwrap_or_else(|| default_capability_for_activity(entry.kind).to_string());
    let output_bytes =
        detail_u64(entry, &["estimated_tool_output_bytes", "tool_output_bytes"]).unwrap_or(0);
    let queue_depth = detail_usize(entry, &["queue_depth"]).unwrap_or(fallback_queue_depth);
    ResourceRequest::new(operation, capability)
        .with_estimated_tool_output_bytes(output_bytes)
        .with_queue_depth(queue_depth)
}

fn detail_u64(entry: &SwarmActivityLedgerEntry, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| entry.details().get(*key))
        .and_then(|value| value.parse::<u64>().ok())
}

fn detail_usize(entry: &SwarmActivityLedgerEntry, keys: &[&str]) -> Option<usize> {
    detail_u64(entry, keys).and_then(|value| usize::try_from(value).ok())
}

fn parse_admission_action(value: &str) -> Option<AdmissionAction> {
    match value.trim().to_ascii_lowercase().as_str() {
        "admit" | "allowed" | "allow" => Some(AdmissionAction::Admit),
        "backpressure" | "degraded" | "delay" => Some(AdmissionAction::Backpressure),
        "deny" | "denied" | "reject" => Some(AdmissionAction::Deny),
        _ => None,
    }
}

fn parse_operation_kind(value: &str) -> Option<ResourceOperationKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "tool" => Some(ResourceOperationKind::Tool),
        "exec" | "bash" | "shell" => Some(ResourceOperationKind::Exec),
        "http" => Some(ResourceOperationKind::Http),
        "session" => Some(ResourceOperationKind::Session),
        "ui" => Some(ResourceOperationKind::Ui),
        "events" | "event" => Some(ResourceOperationKind::Events),
        "log" => Some(ResourceOperationKind::Log),
        "unknown" => Some(ResourceOperationKind::Unknown),
        _ => None,
    }
}

const fn default_operation_for_activity(kind: SwarmActivityKind) -> ResourceOperationKind {
    match kind {
        SwarmActivityKind::RchJob | SwarmActivityKind::Verification => ResourceOperationKind::Exec,
        SwarmActivityKind::AgentMail | SwarmActivityKind::FileReservation => {
            ResourceOperationKind::Session
        }
        SwarmActivityKind::BeadStatus => ResourceOperationKind::Session,
        SwarmActivityKind::GitCommit | SwarmActivityKind::Recovery | SwarmActivityKind::Note => {
            ResourceOperationKind::Unknown
        }
    }
}

const fn default_capability_for_activity(kind: SwarmActivityKind) -> &'static str {
    match kind {
        SwarmActivityKind::BeadStatus => "beads.status",
        SwarmActivityKind::AgentMail => "agent_mail.message",
        SwarmActivityKind::FileReservation => "agent_mail.file_reservation",
        SwarmActivityKind::RchJob => "rch.job",
        SwarmActivityKind::Verification => "verification.command",
        SwarmActivityKind::GitCommit => "git.commit",
        SwarmActivityKind::Recovery => "recovery.action",
        SwarmActivityKind::Note => "note",
    }
}

fn replay_marker(
    kind: SwarmAdmissionReplayDivergenceKind,
    entry: &SwarmActivityLedgerEntry,
    detail: impl Into<String>,
) -> SwarmAdmissionReplayDivergence {
    SwarmAdmissionReplayDivergence {
        kind,
        timestamp_ms: entry.timestamp_ms,
        correlation_id: entry.ids.correlation_id.clone(),
        detail: detail.into(),
    }
}

const fn replay_marker_is_fail_closed(kind: SwarmAdmissionReplayDivergenceKind) -> bool {
    matches!(
        kind,
        SwarmAdmissionReplayDivergenceKind::MissingResourceSample
            | SwarmAdmissionReplayDivergenceKind::StaleResourceSample
            | SwarmAdmissionReplayDivergenceKind::InvalidExpectedAction
    )
}

fn live_capacity_pressure(
    live_load: &SwarmLiveLoad,
    plan: &SwarmCapacityPlan,
) -> SwarmCapacityPressure {
    let mut pressure = SwarmCapacityPressure::none();
    consider_capacity_pressure(
        &mut pressure,
        SwarmCapacityDimension::ActiveAgents,
        live_load.active_agents,
        plan.recommended_agent_concurrency,
    );
    consider_capacity_pressure(
        &mut pressure,
        SwarmCapacityDimension::ActiveToolCalls,
        live_load.active_tool_calls,
        plan.recommended_tool_concurrency,
    );
    consider_capacity_pressure(
        &mut pressure,
        SwarmCapacityDimension::ExtensionHostcallLanes,
        live_load.extension_hostcall_lanes,
        plan.recommended_extension_hostcall_lanes,
    );
    consider_capacity_pressure(
        &mut pressure,
        SwarmCapacityDimension::RchVerificationFanout,
        live_load.active_rch_jobs,
        plan.recommended_rch_verification_fanout,
    );
    pressure
}

#[allow(clippy::cast_precision_loss)]
fn consider_capacity_pressure(
    pressure: &mut SwarmCapacityPressure,
    dimension: SwarmCapacityDimension,
    observed: u64,
    budget: u64,
) {
    if budget == 0 {
        return;
    }
    let ratio = (observed as f64) / (budget as f64);
    if ratio > pressure.ratio {
        *pressure = SwarmCapacityPressure {
            dimension,
            observed,
            budget,
            ratio,
        };
    }
}

fn capacity_action(ratio: f64, backpressure_ratio: f64, deny_ratio: f64) -> AdmissionAction {
    if !ratio.is_finite() {
        return AdmissionAction::Deny;
    }
    let backpressure_ratio = normalized_ratio(backpressure_ratio, 0.85, 0.01, 1.0);
    let deny_ratio = normalized_ratio(deny_ratio, 1.0, backpressure_ratio, 2.0);
    if ratio >= deny_ratio {
        AdmissionAction::Deny
    } else if ratio >= backpressure_ratio {
        AdmissionAction::Backpressure
    } else {
        AdmissionAction::Admit
    }
}

const fn action_rank(action: AdmissionAction) -> u8 {
    match action {
        AdmissionAction::Admit => 0,
        AdmissionAction::Backpressure => 1,
        AdmissionAction::Deny => 2,
    }
}

const fn most_restrictive_action(left: AdmissionAction, right: AdmissionAction) -> AdmissionAction {
    if action_rank(left) >= action_rank(right) {
        left
    } else {
        right
    }
}

fn controller_retry_after_ms(
    action: AdmissionAction,
    resource_retry_after_ms: u64,
    capacity_action: AdmissionAction,
    capacity_ratio: f64,
    plan: &SwarmCapacityPlan,
) -> u64 {
    if action != AdmissionAction::Backpressure {
        return 0;
    }
    let planned_retry = if capacity_action == AdmissionAction::Backpressure {
        planned_capacity_backoff_ms(capacity_ratio, plan)
    } else {
        plan.backoff_initial_ms
    };
    let lower = plan.backoff_initial_ms.min(plan.backoff_max_ms);
    let upper = plan.backoff_initial_ms.max(plan.backoff_max_ms).max(lower);
    resource_retry_after_ms
        .max(planned_retry)
        .clamp(lower, upper)
}

#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
fn planned_capacity_backoff_ms(ratio: f64, plan: &SwarmCapacityPlan) -> u64 {
    let lower = plan.backoff_initial_ms.min(plan.backoff_max_ms);
    let upper = plan.backoff_initial_ms.max(plan.backoff_max_ms);
    if upper <= lower {
        return lower;
    }
    let backpressure_ratio = plan.resource_budgets.backpressure_ratio;
    let deny_ratio = plan
        .resource_budgets
        .deny_ratio
        .max(backpressure_ratio + f64::EPSILON);
    let progress =
        ((ratio - backpressure_ratio) / (deny_ratio - backpressure_ratio)).clamp(0.0, 1.0);
    let span = upper.saturating_sub(lower);
    lower
        .saturating_add(((span as f64) * progress).ceil() as u64)
        .min(upper)
}

fn controller_reason(
    action: AdmissionAction,
    capacity_action: AdmissionAction,
    capacity_pressure: SwarmCapacityPressure,
    resource_decision: &AdmissionDecision,
) -> String {
    if action_rank(capacity_action) > action_rank(resource_decision.action) {
        return format!(
            "swarm capacity pressure on {:?}: {} active vs {} planned ({:.2}x)",
            capacity_pressure.dimension,
            capacity_pressure.observed,
            capacity_pressure.budget,
            capacity_pressure.ratio
        );
    }
    if action == AdmissionAction::Admit {
        return "host resources and swarm capacity within budgets".to_string();
    }
    resource_decision.reason.clone()
}

fn dominant_pressure(
    budgets: &HostResourceBudgets,
    sample: &HostResourceSample,
    request: &ResourceRequest,
) -> (ResourceDimension, f64) {
    let mut dominant = (ResourceDimension::None, 0.0);
    consider_ratio(
        &mut dominant,
        ResourceDimension::CpuLoad,
        sample.load_avg_1m,
        budgets.max_load_avg_1m,
    );
    consider_ratio_u64(
        &mut dominant,
        ResourceDimension::Rss,
        sample.rss_bytes,
        budgets.max_rss_bytes,
    );
    consider_ratio_u64(
        &mut dominant,
        ResourceDimension::Processes,
        sample.process_count,
        budgets.max_processes,
    );
    consider_ratio_u64(
        &mut dominant,
        ResourceDimension::FileDescriptors,
        sample.fd_count,
        budgets.max_fds,
    );
    consider_ratio_u64(
        &mut dominant,
        ResourceDimension::ToolOutput,
        Some(request.estimated_tool_output_bytes),
        budgets.max_tool_output_bytes,
    );
    consider_ratio_usize(
        &mut dominant,
        ResourceDimension::QueueDepth,
        Some(request.queue_depth),
        budgets.max_queue_depth,
    );
    dominant
}

fn consider_ratio(
    dominant: &mut (ResourceDimension, f64),
    dimension: ResourceDimension,
    observed: Option<f64>,
    budget: f64,
) {
    let Some(observed) = observed else {
        return;
    };
    if budget <= 0.0 {
        return;
    }
    let ratio = observed.max(0.0) / budget;
    if ratio > dominant.1 {
        *dominant = (dimension, ratio);
    }
}

#[allow(clippy::cast_precision_loss)]
fn consider_ratio_u64(
    dominant: &mut (ResourceDimension, f64),
    dimension: ResourceDimension,
    observed: Option<u64>,
    budget: u64,
) {
    if budget == 0 {
        return;
    }
    consider_ratio(
        dominant,
        dimension,
        observed.map(|value| value as f64),
        budget as f64,
    );
}

#[allow(clippy::cast_precision_loss)]
fn consider_ratio_usize(
    dominant: &mut (ResourceDimension, f64),
    dimension: ResourceDimension,
    observed: Option<usize>,
    budget: usize,
) {
    if budget == 0 {
        return;
    }
    consider_ratio(
        dominant,
        dimension,
        observed.map(|value| value as f64),
        budget as f64,
    );
}

fn queue_depth_budget(cpu_cores: u64) -> usize {
    let queue_depth = cpu_cores.saturating_mul(DEFAULT_QUEUE_DEPTH_PER_CORE);
    usize::try_from(queue_depth)
        .unwrap_or(DEFAULT_MAX_QUEUE_DEPTH_BUDGET)
        .clamp(
            DEFAULT_MIN_QUEUE_DEPTH_BUDGET,
            DEFAULT_MAX_QUEUE_DEPTH_BUDGET,
        )
}

fn conservative_u64(value: u64, numerator: u64, denominator: u64) -> u64 {
    if value == 0 || denominator == 0 {
        return value;
    }
    value.saturating_mul(numerator).div_ceil(denominator).max(1)
}

fn conservative_usize(value: usize, numerator: usize, denominator: usize) -> usize {
    if value == 0 || denominator == 0 {
        return value;
    }
    value.saturating_mul(numerator).div_ceil(denominator).max(1)
}

fn conservative_f64(value: f64, ratio: f64) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        return value;
    }
    (value * ratio).max(f64::MIN_POSITIVE)
}

#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
fn scale_u64_by_ratio(value: u64, ratio: f64) -> u64 {
    if value == 0 {
        return 0;
    }
    ((value as f64) * ratio).ceil().max(1.0) as u64
}

#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
fn scale_usize_by_ratio(value: usize, ratio: f64) -> usize {
    if value == 0 {
        return 0;
    }
    ((value as f64) * ratio).ceil().max(1.0) as usize
}

fn parse_capacity_evidence_jsonl(
    jsonl: &str,
    requested_inventory: SwarmHostInventory,
) -> Result<SwarmCapacityEvidenceSummary, SwarmCapacityPlanError> {
    let mut accumulator = CapacityEvidenceAccumulator::default();
    for (line_index, row) in jsonl.lines().enumerate() {
        let line = line_index.saturating_add(1);
        let row = row.trim();
        if row.is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_str(row).map_err(|err| SwarmCapacityPlanError::InvalidJson {
                line,
                message: err.to_string(),
            })?;
        let Some(swarm_metrics) = value.get("swarm_metrics") else {
            continue;
        };
        let swarm_metrics = value_as_object(swarm_metrics, "swarm_metrics", line)?;
        let record = parse_swarm_metrics_record(swarm_metrics, line)?;
        accumulator.observe(record, requested_inventory);
    }
    accumulator.finish()
}

#[derive(Debug, Clone, Copy)]
struct SwarmMetricsRecord {
    host_inventory: SwarmHostInventory,
    p99_ms: u64,
    p999_ms: u64,
    max_queue_depth: usize,
    rss_mb: u64,
    cpu_pct: f64,
}

#[derive(Default)]
struct CapacityEvidenceAccumulator {
    complete_records: usize,
    host_capacity_rows: usize,
    host_capacity_mismatch_rows: usize,
    max_p99_ms: u64,
    max_p999_ms: u64,
    max_queue_depth: usize,
    max_rss_mb: u64,
    max_cpu_pct: f64,
}

impl CapacityEvidenceAccumulator {
    fn observe(&mut self, record: SwarmMetricsRecord, requested_inventory: SwarmHostInventory) {
        self.complete_records = self.complete_records.saturating_add(1);
        self.host_capacity_rows = self.host_capacity_rows.saturating_add(1);
        if record.host_inventory != requested_inventory {
            self.host_capacity_mismatch_rows = self.host_capacity_mismatch_rows.saturating_add(1);
        }
        self.max_p99_ms = self.max_p99_ms.max(record.p99_ms);
        self.max_p999_ms = self.max_p999_ms.max(record.p999_ms);
        self.max_queue_depth = self.max_queue_depth.max(record.max_queue_depth);
        self.max_rss_mb = self.max_rss_mb.max(record.rss_mb);
        self.max_cpu_pct = self.max_cpu_pct.max(record.cpu_pct);
    }

    const fn finish(self) -> Result<SwarmCapacityEvidenceSummary, SwarmCapacityPlanError> {
        if self.complete_records == 0 {
            return Err(SwarmCapacityPlanError::MissingEvidence("swarm_metrics"));
        }
        Ok(SwarmCapacityEvidenceSummary {
            complete_records: self.complete_records,
            host_capacity_rows: self.host_capacity_rows,
            host_capacity_mismatch_rows: self.host_capacity_mismatch_rows,
            max_p99_ms: self.max_p99_ms,
            max_p999_ms: self.max_p999_ms,
            max_queue_depth: self.max_queue_depth,
            max_rss_mb: self.max_rss_mb,
            max_cpu_pct: self.max_cpu_pct,
        })
    }
}

fn parse_swarm_metrics_record(
    metrics: &Map<String, Value>,
    line: usize,
) -> Result<SwarmMetricsRecord, SwarmCapacityPlanError> {
    let latency = required_object(
        metrics,
        "latency_quantiles_ms",
        "swarm_metrics.latency_quantiles_ms",
        line,
    )?;
    let queue_depth = required_object(metrics, "queue_depth", "swarm_metrics.queue_depth", line)?;
    let resource_usage = required_object(
        metrics,
        "resource_usage",
        "swarm_metrics.resource_usage",
        line,
    )?;
    let host_capacity = required_object(
        metrics,
        "host_capacity",
        "swarm_metrics.host_capacity",
        line,
    )?;
    let p99_ms = required_positive_u64_ceil(
        latency,
        "p99",
        "swarm_metrics.latency_quantiles_ms.p99",
        line,
    )?;
    let p999_ms = required_positive_u64_ceil(
        latency,
        "p999",
        "swarm_metrics.latency_quantiles_ms.p999",
        line,
    )?;
    if p999_ms < p99_ms {
        return Err(SwarmCapacityPlanError::InvalidEvidence {
            line,
            field: "swarm_metrics.latency_quantiles_ms.p999",
        });
    }
    let max_queue_depth =
        required_positive_usize_ceil(queue_depth, "max", "swarm_metrics.queue_depth.max", line)?;
    let rss_mb = required_positive_u64_ceil(
        resource_usage,
        "rss_mb",
        "swarm_metrics.resource_usage.rss_mb",
        line,
    )?;
    let cpu_pct = required_non_negative_f64(
        resource_usage,
        "cpu_pct",
        "swarm_metrics.resource_usage.cpu_pct",
        line,
    )?;
    let target_cpu_cores = required_positive_u64_ceil(
        host_capacity,
        "target_cpu_cores",
        "swarm_metrics.host_capacity.target_cpu_cores",
        line,
    )?;
    let observed_cpu_cores = required_positive_u64_ceil(
        host_capacity,
        "observed_cpu_cores",
        "swarm_metrics.host_capacity.observed_cpu_cores",
        line,
    )?;
    let mem_total_mb = required_positive_u64_ceil(
        host_capacity,
        "mem_total_mb",
        "swarm_metrics.host_capacity.mem_total_mb",
        line,
    )?;

    Ok(SwarmMetricsRecord {
        host_inventory: SwarmHostInventory::new(target_cpu_cores, observed_cpu_cores, mem_total_mb),
        p99_ms,
        p999_ms,
        max_queue_depth,
        rss_mb,
        cpu_pct,
    })
}

fn value_as_object<'a>(
    value: &'a Value,
    field: &'static str,
    line: usize,
) -> Result<&'a Map<String, Value>, SwarmCapacityPlanError> {
    value
        .as_object()
        .ok_or(SwarmCapacityPlanError::InvalidEvidence { line, field })
}

fn required_object<'a>(
    map: &'a Map<String, Value>,
    key: &'static str,
    field: &'static str,
    line: usize,
) -> Result<&'a Map<String, Value>, SwarmCapacityPlanError> {
    let value = map
        .get(key)
        .ok_or(SwarmCapacityPlanError::InvalidEvidence { line, field })?;
    value_as_object(value, field, line)
}

fn required_non_negative_f64(
    map: &Map<String, Value>,
    key: &'static str,
    field: &'static str,
    line: usize,
) -> Result<f64, SwarmCapacityPlanError> {
    let value = map
        .get(key)
        .ok_or(SwarmCapacityPlanError::InvalidEvidence { line, field })?;
    let number = value
        .as_f64()
        .ok_or(SwarmCapacityPlanError::InvalidEvidence { line, field })?;
    if number.is_finite() && number >= 0.0 {
        Ok(number)
    } else {
        Err(SwarmCapacityPlanError::InvalidEvidence { line, field })
    }
}

fn required_positive_u64_ceil(
    map: &Map<String, Value>,
    key: &'static str,
    field: &'static str,
    line: usize,
) -> Result<u64, SwarmCapacityPlanError> {
    let number = required_non_negative_f64(map, key, field, line)?;
    let value =
        ceil_f64_to_u64(number).ok_or(SwarmCapacityPlanError::InvalidEvidence { line, field })?;
    if value == 0 {
        Err(SwarmCapacityPlanError::InvalidEvidence { line, field })
    } else {
        Ok(value)
    }
}

fn required_positive_usize_ceil(
    map: &Map<String, Value>,
    key: &'static str,
    field: &'static str,
    line: usize,
) -> Result<usize, SwarmCapacityPlanError> {
    let value = required_positive_u64_ceil(map, key, field, line)?;
    usize::try_from(value).map_err(|_| SwarmCapacityPlanError::InvalidEvidence { line, field })
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn ceil_f64_to_u64(value: f64) -> Option<u64> {
    if !value.is_finite() || value < 0.0 || value > (u64::MAX as f64) {
        return None;
    }
    Some(value.ceil() as u64)
}

fn mb_to_bytes(mb: u64, field: &'static str) -> Result<u64, SwarmCapacityPlanError> {
    mb.checked_mul(1024 * 1024)
        .ok_or(SwarmCapacityPlanError::InvalidEvidence { line: 0, field })
}

fn planned_queue_depth_budget(cpu_cores: u64, observed_max_queue_depth: usize) -> usize {
    let host_budget = queue_depth_budget(cpu_cores);
    observed_max_queue_depth
        .saturating_mul(2)
        .max(DEFAULT_MIN_QUEUE_DEPTH_BUDGET)
        .min(host_budget)
        .max(1)
}

fn normalized_ratio(value: f64, fallback: f64, minimum: f64, maximum: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value.clamp(minimum, maximum)
    } else {
        fallback
    }
}

fn capacity_uncertainties(
    evidence: &SwarmCapacityEvidenceSummary,
    rss_exceeds_memory_headroom: bool,
    queue_depth_floor_applied: bool,
) -> Vec<String> {
    let mut uncertainties = Vec::new();
    if evidence.complete_records < 3 {
        uncertainties.push("fewer_than_three_complete_swarm_records".to_owned());
    }
    if evidence.host_capacity_mismatch_rows > 0 {
        uncertainties.push("host_capacity_mismatch".to_owned());
    }
    if evidence.max_cpu_pct <= f64::EPSILON {
        uncertainties.push("cpu_usage_reported_zero".to_owned());
    }
    if rss_exceeds_memory_headroom {
        uncertainties.push("rss_exceeds_memory_headroom".to_owned());
    }
    if queue_depth_floor_applied {
        uncertainties.push("queue_depth_floor_applied".to_owned());
    }
    uncertainties
}

fn capacity_confidence(
    evidence: &SwarmCapacityEvidenceSummary,
    uncertainties: &[String],
) -> SwarmCapacityConfidence {
    let material_caveat = evidence.host_capacity_mismatch_rows > 0
        || evidence.max_cpu_pct <= f64::EPSILON
        || uncertainties
            .iter()
            .any(|uncertainty| uncertainty == "rss_exceeds_memory_headroom");
    if material_caveat {
        SwarmCapacityConfidence::Low
    } else if uncertainties.is_empty() {
        SwarmCapacityConfidence::High
    } else {
        SwarmCapacityConfidence::Medium
    }
}

const fn operator_profile_confidence(
    plan_confidence: SwarmCapacityConfidence,
    derived_from_source: bool,
) -> SwarmCapacityConfidence {
    match (plan_confidence, derived_from_source) {
        (SwarmCapacityConfidence::High, true) => SwarmCapacityConfidence::Medium,
        (confidence, _) => confidence,
    }
}

fn operator_profile_caveats(
    plan: &SwarmCapacityPlan,
    source_inventory: SwarmHostInventory,
    derived_from_source: bool,
) -> Vec<String> {
    let mut caveats = plan.uncertainties.clone();
    if derived_from_source {
        caveats.push(format!(
            "derived_from_{}cpu_{}mib_source_evidence",
            source_inventory.effective_cpu_cores(),
            source_inventory.mem_total_mb
        ));
    }
    caveats.push("starting_point_not_release_performance_claim".to_owned());
    caveats
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn retry_after_ms(ratio: f64) -> u64 {
    let excess = (ratio - 0.85).max(0.0);
    (50.0 + (excess * 1_000.0)).clamp(50.0, 500.0) as u64
}

fn decision_reason(
    action: AdmissionAction,
    dominant_dimension: ResourceDimension,
    dominant_ratio: f64,
) -> String {
    match action {
        AdmissionAction::Admit => "host resources within budgets".to_string(),
        AdmissionAction::Backpressure => format!(
            "host resource pressure on {dominant_dimension:?} at {dominant_ratio:.2}x budget"
        ),
        AdmissionAction::Deny => format!(
            "host resource limit exceeded on {dominant_dimension:?} at {dominant_ratio:.2}x budget"
        ),
    }
}

#[cfg(target_os = "linux")]
fn read_load_avg_1m() -> Option<f64> {
    std::fs::read_to_string("/proc/loadavg")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(not(target_os = "linux"))]
const fn read_load_avg_1m() -> Option<f64> {
    None
}

#[cfg(target_os = "linux")]
fn read_self_rss_bytes() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = content.split_whitespace().nth(1)?.parse().ok()?;
    resident_pages.checked_mul(PROC_PAGE_SIZE_BYTES)
}

#[cfg(not(target_os = "linux"))]
const fn read_self_rss_bytes() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn read_mem_total_bytes() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in content.lines() {
        let Some(rest) = line.strip_prefix("MemTotal:") else {
            continue;
        };
        let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
        return kb.checked_mul(1024);
    }
    None
}

#[cfg(not(target_os = "linux"))]
const fn read_mem_total_bytes() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn count_proc_processes() -> Option<u64> {
    let mut count = 0_u64;
    for entry in std::fs::read_dir("/proc").ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()) {
            count = count.saturating_add(1);
        }
    }
    Some(count)
}

#[cfg(not(target_os = "linux"))]
const fn count_proc_processes() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn count_self_fds() -> Option<u64> {
    let count = std::fs::read_dir("/proc/self/fd").ok()?.count();
    u64::try_from(count).ok()
}

#[cfg(not(target_os = "linux"))]
const fn count_self_fds() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn read_open_files_soft_limit() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/self/limits").ok()?;
    for line in content.lines() {
        if !line.starts_with("Max open files") {
            continue;
        }
        let token = line.split_whitespace().nth(3)?;
        if matches!(token, "unlimited") {
            return None;
        }
        return token.parse().ok();
    }
    None
}

#[cfg(not(target_os = "linux"))]
const fn read_open_files_soft_limit() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::{
        AdmissionAction, AdmissionDecision, HostResourceBudgets, HostResourceSample,
        ResourceDimension, ResourceGovernor, ResourceOperationKind, ResourceRequest,
        SWARM_ADMISSION_CONTROLLER_SCHEMA, SWARM_ADMISSION_REPLAY_DIGEST_ALIGNMENT_SCHEMA,
        SWARM_ADMISSION_REPLAY_SCHEMA, SWARM_CAPACITY_PLAN_SCHEMA,
        SWARM_MEMORY_PRESSURE_REPLAY_SCHEMA, SWARM_OPERATOR_BUDGET_PROFILES_SCHEMA,
        SwarmAdmissionController, SwarmAdmissionReplayConfig,
        SwarmAdmissionReplayDigestAssertionKind, SwarmAdmissionReplayDigestSeverity,
        SwarmAdmissionReplayDivergenceKind, SwarmAdmissionReplayError, SwarmAdmissionReplaySample,
        SwarmAdmissionReplayStatus, SwarmCapacityConfidence, SwarmCapacityDimension,
        SwarmCapacityPlanError, SwarmHostInventory, SwarmLiveLoad, SwarmMemoryPressureReplayAction,
        SwarmMemoryPressureReplayProfile, SwarmMemoryPressureReplayVerdict, SwarmOperatorHostClass,
        TAIL_LATENCY_REGIME_SCHEMA, TailLatencyFallbackReason, TailLatencyRegime,
        TailLatencyRegimeConfig, TailLatencyRegimeGuard, TailLatencyRegimeSample,
        assert_swarm_digest_admission_replay_alignment,
        generate_operator_budget_profiles_from_jsonl,
        generate_operator_budget_profiles_from_jsonl_with_host_classes,
        plan_swarm_capacity_from_jsonl, replay_swarm_admission_from_jsonl,
        replay_swarm_memory_pressure_profiles,
    };
    use crate::swarm_activity_ledger::{
        SwarmActivityDigestConfig, SwarmActivityIds, SwarmActivityKind, SwarmActivityLedger,
        digest_from_jsonl,
    };
    use std::io::Write as _;

    use serde_json::{Value, json};

    const RESOURCE_GOVERNOR_SURFACE_CONTRACT_SCHEMA: &str =
        "pi.resource_governor.surface_contract.v1";

    fn budgets() -> HostResourceBudgets {
        HostResourceBudgets::fixed(10.0, 1_000, 100, 100, 1_000)
    }

    fn sample() -> HostResourceSample {
        HostResourceSample {
            load_avg_1m: Some(2.0),
            rss_bytes: Some(200),
            process_count: Some(20),
            fd_count: Some(20),
        }
    }

    fn tail_config() -> TailLatencyRegimeConfig {
        TailLatencyRegimeConfig::new(100, 500, 4, 0.80, 0.50, 2, 2)
    }

    fn capacity_inventory() -> SwarmHostInventory {
        SwarmHostInventory::new(64, 64, 262_144)
    }

    fn capacity_fixture_jsonl() -> &'static str {
        r#"{"schema":"pi.perf.session_workload_matrix_cell.v1","swarm_metrics":{"latency_quantiles_ms":{"p50":80.0,"p95":105.0,"p99":120.0,"p999":600.0},"queue_depth":{"p50":32,"p95":96,"p99":160,"p999":224,"max":256},"resource_usage":{"rss_mb":384,"cpu_pct":41.0},"component_breakdown_ms":{"tool":4.0,"provider":12.0,"extension":8.0,"session":56.0},"stage_breakdown_ms":{"open":8.0,"append":16.0,"save":20.0,"index":12.0},"host_capacity":{"target_cpu_cores":64,"observed_cpu_cores":64,"mem_total_mb":262144}}}
	{"schema":"pi.perf.session_workload_matrix_cell.v1","swarm_metrics":{"latency_quantiles_ms":{"p50":95.0,"p95":130.0,"p99":180.0,"p999":800.0},"queue_depth":{"p50":48,"p95":112,"p99":192,"p999":256,"max":256},"resource_usage":{"rss_mb":512,"cpu_pct":55.0},"component_breakdown_ms":{"tool":6.0,"provider":14.0,"extension":10.0,"session":65.0},"stage_breakdown_ms":{"open":10.0,"append":18.0,"save":24.0,"index":13.0},"host_capacity":{"target_cpu_cores":64,"observed_cpu_cores":64,"mem_total_mb":262144}}}
	{"schema":"pi.perf.session_workload_matrix_cell.v1","swarm_metrics":{"latency_quantiles_ms":{"p50":90.0,"p95":125.0,"p99":150.0,"p999":700.0},"queue_depth":{"p50":40,"p95":100,"p99":180,"p999":240,"max":256},"resource_usage":{"rss_mb":448,"cpu_pct":49.0},"component_breakdown_ms":{"tool":5.0,"provider":13.0,"extension":9.0,"session":63.0},"stage_breakdown_ms":{"open":9.0,"append":17.0,"save":23.0,"index":14.0},"host_capacity":{"target_cpu_cores":64,"observed_cpu_cores":64,"mem_total_mb":262144}}}"#
    }

    fn live_controller_sample() -> HostResourceSample {
        HostResourceSample {
            load_avg_1m: Some(20.0),
            rss_bytes: Some(512 * 1024 * 1024),
            process_count: Some(128),
            fd_count: Some(128),
        }
    }

    fn healthy_tail_sample() -> TailLatencyRegimeSample {
        TailLatencyRegimeSample::new(100, 400, 64, 0.20)
    }

    fn live_controller_request() -> ResourceRequest {
        ResourceRequest::new(ResourceOperationKind::Tool, "read")
            .with_estimated_tool_output_bytes(16 * 1024 * 1024)
            .with_queue_depth(128)
    }

    fn contract_budgets() -> HostResourceBudgets {
        HostResourceBudgets::fixed_with_queue_depth(10.0, 1_000, 100, 100, 1_000, 8)
    }

    fn action_label(action: AdmissionAction) -> &'static str {
        match action {
            AdmissionAction::Admit => "admit",
            AdmissionAction::Backpressure => "backpressure",
            AdmissionAction::Deny => "deny",
        }
    }

    fn dimension_label(dimension: ResourceDimension) -> &'static str {
        match dimension {
            ResourceDimension::CpuLoad => "cpu_load",
            ResourceDimension::Rss => "rss",
            ResourceDimension::Processes => "processes",
            ResourceDimension::FileDescriptors => "file_descriptors",
            ResourceDimension::ToolOutput => "tool_output",
            ResourceDimension::QueueDepth => "queue_depth",
            ResourceDimension::None => "none",
        }
    }

    fn reason_code(decision: &AdmissionDecision) -> String {
        if matches!(decision.action, AdmissionAction::Admit) {
            return "admit_within_budget".to_string();
        }
        format!(
            "{}_{}",
            action_label(decision.action),
            dimension_label(decision.dominant_dimension)
        )
    }

    fn contract_budget(budgets: &HostResourceBudgets) -> Value {
        json!({
            "max_tool_output_bytes": budgets.max_tool_output_bytes,
            "max_queue_depth": budgets.max_queue_depth,
            "backpressure_ratio": budgets.backpressure_ratio,
            "deny_ratio": budgets.deny_ratio,
        })
    }

    fn contract_request(request: &ResourceRequest) -> Value {
        json!({
            "operation": dimensionless_operation_label(request.operation),
            "capability": request.capability.as_str(),
            "estimated_tool_output_bytes": request.estimated_tool_output_bytes,
            "queue_depth": request.queue_depth,
        })
    }

    fn contract_decision(decision: &AdmissionDecision) -> Value {
        json!({
            "action": action_label(decision.action),
            "dominant_dimension": dimension_label(decision.dominant_dimension),
            "dominant_ratio": decision.dominant_ratio,
            "reason_code": reason_code(decision),
            "reason": decision.reason.as_str(),
            "retry_after_ms": decision.retry_after_ms,
        })
    }

    fn dimensionless_operation_label(operation: ResourceOperationKind) -> &'static str {
        match operation {
            ResourceOperationKind::Tool => "tool",
            ResourceOperationKind::Exec => "exec",
            ResourceOperationKind::Http => "http",
            ResourceOperationKind::Session => "session",
            ResourceOperationKind::Ui => "ui",
            ResourceOperationKind::Events => "events",
            ResourceOperationKind::Log => "log",
            ResourceOperationKind::Unknown => "unknown",
        }
    }

    fn write_resource_contract_evidence(entry: &Value) {
        let path = std::env::var_os("PI_RESOURCE_GOVERNOR_CONTRACT_EVIDENCE")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("PI_EVIDENCE_DIR").map(|base| {
                    std::path::PathBuf::from(base)
                        .join("perf")
                        .join("resource_governor_surface_contract.jsonl")
                })
            });
        let Some(path) = path else {
            return;
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("create resource governor contract evidence dir");
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open resource governor contract evidence file");
        writeln!(
            file,
            "{}",
            serde_json::to_string(entry).expect("serialize resource governor contract evidence")
        )
        .expect("write resource governor contract evidence");
        file.sync_all()
            .expect("sync resource governor contract evidence");
    }

    fn replay_sample(
        timestamp_ms: u64,
        active_agents: u64,
        active_tool_calls: u64,
    ) -> SwarmAdmissionReplaySample {
        SwarmAdmissionReplaySample::new(
            timestamp_ms,
            live_controller_sample(),
            healthy_tail_sample(),
            SwarmLiveLoad::empty()
                .with_active_agents(active_agents)
                .with_active_tool_calls(active_tool_calls)
                .with_extension_hostcall_lanes(4)
                .with_active_rch_jobs(1),
        )
    }

    fn high_capacity_memory_pressure_profile() -> SwarmMemoryPressureReplayProfile {
        SwarmMemoryPressureReplayProfile {
            profile_id: "cpu64_mem256gib_nominal",
            description: "64 CPU / 256 GiB host with heavy but healthy retained state",
            host_inventory: capacity_inventory(),
            host_resource_sample: HostResourceSample {
                load_avg_1m: Some(20.0),
                rss_bytes: Some(512 * 1024 * 1024),
                process_count: Some(128),
                fd_count: Some(128),
            },
            tail_latency_sample: TailLatencyRegimeSample::new(120, 600, 128, 0.30),
            live_load: SwarmLiveLoad::empty()
                .with_active_agents(16)
                .with_active_tool_calls(32)
                .with_extension_hostcall_lanes(8)
                .with_active_rch_jobs(4),
            message_volume_tokens: 128_000,
            retained_tool_output_bytes: 32 * 1024 * 1024,
            extension_workload_bytes: 256 * 1024 * 1024,
            expected_admission_action: AdmissionAction::Admit,
        }
    }

    fn constrained_memory_pressure_profile() -> SwarmMemoryPressureReplayProfile {
        SwarmMemoryPressureReplayProfile {
            profile_id: "cgroup_mem1gib_degraded",
            description: "1 GiB cgroup limit with retained transcript and tool-output pressure",
            host_inventory: SwarmHostInventory::new(4, 4, 1_024),
            host_resource_sample: HostResourceSample {
                load_avg_1m: Some(2.0),
                rss_bytes: Some(690 * 1024 * 1024),
                process_count: Some(80),
                fd_count: Some(96),
            },
            tail_latency_sample: TailLatencyRegimeSample::new(180, 800, 64, 0.88),
            live_load: SwarmLiveLoad::empty()
                .with_active_agents(1)
                .with_active_tool_calls(2)
                .with_extension_hostcall_lanes(1)
                .with_active_rch_jobs(1),
            message_volume_tokens: 48_000,
            retained_tool_output_bytes: 128 * 1024 * 1024,
            extension_workload_bytes: 180 * 1024 * 1024,
            expected_admission_action: AdmissionAction::Deny,
        }
    }

    fn saturated_admission_fixture_jsonl() -> String {
        let mut ledger = SwarmActivityLedger::new();
        ledger.append(
            100,
            SwarmActivityKind::AgentMail,
            SwarmActivityIds::new("saturation-chatter")
                .with_agent_name("MagentaOak")
                .with_mail_thread_id("bd-saturation"),
            "coordination note while saturated",
            [("subject", "status")],
        );
        ledger.append(
            200,
            SwarmActivityKind::Verification,
            SwarmActivityIds::new("saturation-replay")
                .with_agent_name("MagentaOak")
                .with_bead_id("bd-saturation"),
            "verification should wait for capacity",
            [
                ("expected_action", "backpressure"),
                ("request_operation", "exec"),
                ("queue_depth", "64"),
            ],
        );
        ledger.to_jsonl().expect("fixture should serialize")
    }

    fn saturated_digest_config() -> SwarmActivityDigestConfig {
        SwarmActivityDigestConfig {
            max_items: 8,
            stale_thread_after_ms: 60_000,
            saturation_window_ms: 1_000,
            min_new_bugs_per_window: 1,
            duplicate_work_threshold: 10,
            repeated_blocker_threshold: 10,
            closed_surface_edit_threshold: 10,
            stale_introduction_threshold: 10,
            coordination_chatter_threshold: 10,
            low_throughput_event_threshold: 0,
        }
    }

    #[test]
    fn admits_when_all_dimensions_are_below_backpressure_ratio() {
        let governor = ResourceGovernor::with_budgets(budgets());
        let request = ResourceRequest::new(ResourceOperationKind::Tool, "read")
            .with_estimated_tool_output_bytes(200);

        let decision = governor.admit_sample(&request, sample());

        assert_eq!(decision.action, AdmissionAction::Admit);
        assert_eq!(decision.dominant_dimension, ResourceDimension::CpuLoad);
    }

    #[test]
    fn backpressures_before_hard_overload() {
        let governor = ResourceGovernor::with_budgets(budgets());
        let request = ResourceRequest::new(ResourceOperationKind::Tool, "read")
            .with_estimated_tool_output_bytes(900);

        let decision = governor.admit_sample(&request, sample());

        assert_eq!(decision.action, AdmissionAction::Backpressure);
        assert_eq!(decision.dominant_dimension, ResourceDimension::ToolOutput);
        assert!(decision.retry_after_ms >= 50);
    }

    #[test]
    fn denies_when_a_dimension_exceeds_the_deny_ratio() {
        let governor = ResourceGovernor::with_budgets(budgets());
        let request = ResourceRequest::new(ResourceOperationKind::Exec, "exec")
            .with_estimated_tool_output_bytes(1_200);

        let decision = governor.admit_sample(&request, sample());

        assert_eq!(decision.action, AdmissionAction::Deny);
        assert_eq!(decision.dominant_dimension, ResourceDimension::ToolOutput);
    }

    #[test]
    fn queue_depth_participates_in_admission_pressure() {
        let governor = ResourceGovernor::with_budgets(HostResourceBudgets::fixed_with_queue_depth(
            10.0, 1_000, 100, 100, 1_000, 4,
        ));
        let request =
            ResourceRequest::new(ResourceOperationKind::Events, "events").with_queue_depth(5);

        let decision = governor.admit_sample(&request, sample());

        assert_eq!(decision.action, AdmissionAction::Deny);
        assert_eq!(decision.dominant_dimension, ResourceDimension::QueueDepth);
    }

    #[test]
    fn ignores_unavailable_host_metrics_but_still_checks_request_size() {
        let governor = ResourceGovernor::with_budgets(budgets());
        let request = ResourceRequest::new(ResourceOperationKind::Exec, "exec")
            .with_estimated_tool_output_bytes(1_200);
        let sample = HostResourceSample {
            load_avg_1m: None,
            rss_bytes: None,
            process_count: None,
            fd_count: None,
        };

        let decision = governor.admit_sample(&request, sample);

        assert_eq!(decision.action, AdmissionAction::Deny);
        assert_eq!(decision.dominant_dimension, ResourceDimension::ToolOutput);
    }

    #[test]
    fn telemetry_contains_stable_schema() {
        let governor = ResourceGovernor::with_budgets(budgets());
        let request = ResourceRequest::new(ResourceOperationKind::Session, "session");
        let decision = governor.admit_sample(&request, sample());

        let telemetry = decision.telemetry(&request);

        assert_eq!(
            telemetry.get("schema").and_then(serde_json::Value::as_str),
            Some("pi.resource_governor.admission.v1")
        );
        assert_eq!(
            telemetry
                .get("decision")
                .and_then(|value| value.get("action"))
                .and_then(serde_json::Value::as_str),
            Some("admit")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn surface_contract_enumerates_governed_pressure_outcomes() {
        let budgets = contract_budgets();
        let governor = ResourceGovernor::with_budgets(budgets.clone());
        let tool_over_budget = ResourceRequest::new(ResourceOperationKind::Tool, "tool.read")
            .with_estimated_tool_output_bytes(1_200);
        let extension_queue_pressure =
            ResourceRequest::new(ResourceOperationKind::Exec, "extension.exec").with_queue_depth(7);
        let extension_semantic_event =
            ResourceRequest::new(ResourceOperationKind::Events, "extension.events.emit")
                .with_queue_depth(1);
        let session_overload =
            ResourceRequest::new(ResourceOperationKind::Session, "session.append_jsonl")
                .with_queue_depth(9);

        let tool_decision = governor.admit_sample(&tool_over_budget, sample());
        let extension_queue_decision = governor.admit_sample(&extension_queue_pressure, sample());
        let extension_event_decision = governor.admit_sample(&extension_semantic_event, sample());
        let session_decision = governor.admit_sample(&session_overload, sample());

        assert_eq!(tool_decision.action, AdmissionAction::Deny);
        assert_eq!(
            tool_decision.dominant_dimension,
            ResourceDimension::ToolOutput
        );
        assert_eq!(
            extension_queue_decision.action,
            AdmissionAction::Backpressure
        );
        assert_eq!(
            extension_queue_decision.dominant_dimension,
            ResourceDimension::QueueDepth
        );
        assert_eq!(extension_event_decision.action, AdmissionAction::Admit);
        assert_eq!(session_decision.action, AdmissionAction::Deny);
        assert_eq!(
            session_decision.dominant_dimension,
            ResourceDimension::QueueDepth
        );

        let entries = vec![
            json!({
                "surface": "rpc_input_pressure",
                "enforcement_path": "RpcSharedState::push_steering / push_follow_up",
                "resource_governor_direct": false,
                "budget": {
                    "max_pending_messages": 128,
                    "source": "MAX_RPC_PENDING_MESSAGES",
                },
                "decision": {
                    "action": "deny",
                    "reason_code": "rpc_input_queue_full",
                    "user_visible_outcome": "reject enqueue with session error",
                },
                "evidence_pointer": "src/rpc.rs::shared_state_blocks_follow_up_when_steering_queue_reaches_total_cap",
            }),
            json!({
                "surface": "rpc_output_pressure",
                "enforcement_path": "RpcOutputPressureState::send_agent_event",
                "resource_governor_direct": false,
                "budget": {
                    "output_channel_capacity": 1,
                    "coalescible_classes": ["message_delta", "tool_update"],
                },
                "decision": {
                    "action": "coalesce_and_flush_before_semantic",
                    "reason_code": "rpc_output_semantic_preserved",
                    "user_visible_outcome": "latest low-value update survives before final semantic event",
                },
                "semantic_preservation": {
                    "preserved_event": "agent_end",
                    "proof": "semantic event flushes pending message and tool updates before sending",
                },
                "evidence_pointer": "src/rpc.rs::rpc_output_pressure_conformance_matrix_flushes_each_coalesced_class_before_semantic",
            }),
            json!({
                "surface": "tool_execution_hostcall",
                "enforcement_path": "ExtensionHostcallDispatcher::apply_resource_governor",
                "resource_governor_direct": true,
                "budget": contract_budget(&budgets),
                "request": contract_request(&tool_over_budget),
                "decision": contract_decision(&tool_decision),
                "semantic_preservation": null,
            }),
            json!({
                "surface": "extension_hostcall_queue",
                "enforcement_path": "ExtensionHostcallDispatcher::apply_resource_governor",
                "resource_governor_direct": true,
                "budget": contract_budget(&budgets),
                "request": contract_request(&extension_queue_pressure),
                "decision": contract_decision(&extension_queue_decision),
                "semantic_preservation": null,
            }),
            json!({
                "surface": "extension_event_semantic",
                "enforcement_path": "ExtensionHostcallDispatcher::apply_resource_governor",
                "resource_governor_direct": true,
                "budget": contract_budget(&budgets),
                "request": contract_request(&extension_semantic_event),
                "decision": contract_decision(&extension_event_decision),
                "semantic_preservation": {
                    "preserved_event": "extension.events.emit",
                    "proof": "healthy event hostcall admits immediately instead of coalescing or dropping semantic events",
                },
            }),
            json!({
                "surface": "session_persistence_under_load",
                "enforcement_path": "ExtensionHostcallDispatcher::apply_resource_governor",
                "resource_governor_direct": true,
                "budget": contract_budget(&budgets),
                "request": contract_request(&session_overload),
                "decision": contract_decision(&session_decision),
                "semantic_preservation": null,
            }),
        ];
        let evidence = json!({
            "schema": RESOURCE_GOVERNOR_SURFACE_CONTRACT_SCHEMA,
            "budget_profile": "unit_contract_fixed_queue_depth",
            "surface_count": entries.len(),
            "contract": entries,
            "required_surfaces": [
                "rpc_input_pressure",
                "rpc_output_pressure",
                "tool_execution_hostcall",
                "extension_hostcall_queue",
                "extension_event_semantic",
                "session_persistence_under_load",
            ],
            "verdict": "pass",
        });

        let contract = evidence
            .get("contract")
            .and_then(Value::as_array)
            .expect("contract should contain entries");
        assert_eq!(contract.len(), 6);
        assert!(contract.iter().any(|entry| {
            entry
                .get("decision")
                .and_then(|decision| decision.get("action"))
                .and_then(Value::as_str)
                == Some("deny")
        }));
        assert!(contract.iter().any(|entry| {
            entry.get("semantic_preservation").is_some_and(|value| {
                value
                    .get("preserved_event")
                    .and_then(Value::as_str)
                    .is_some()
            })
        }));
        for entry in contract {
            assert!(
                entry.get("budget").is_some_and(Value::is_object),
                "contract entry must expose machine-readable budget: {entry:?}"
            );
            assert!(
                entry
                    .get("decision")
                    .and_then(|decision| decision.get("reason_code"))
                    .and_then(Value::as_str)
                    .is_some_and(|code| !code.is_empty()),
                "contract entry must expose a machine-readable reason code: {entry:?}"
            );
        }

        write_resource_contract_evidence(&evidence);
    }

    #[test]
    fn capacity_plan_derives_stable_resource_governor_budgets() {
        let plan =
            plan_swarm_capacity_from_jsonl(capacity_fixture_jsonl(), capacity_inventory()).unwrap();

        assert_eq!(plan.schema, SWARM_CAPACITY_PLAN_SCHEMA);
        assert_eq!(plan.confidence, SwarmCapacityConfidence::High);
        assert!(plan.uncertainties.is_empty());
        assert_eq!(plan.evidence.complete_records, 3);
        assert_eq!(plan.recommended_agent_concurrency, 32);
        assert_eq!(plan.recommended_tool_concurrency, 64);
        assert_eq!(plan.recommended_extension_hostcall_lanes, 16);
        assert_eq!(plan.recommended_rch_verification_fanout, 8);
        assert_eq!(plan.resource_budgets.cpu_cores, 64);
        assert_eq!(plan.resource_budgets.max_queue_depth, 512);
        assert_eq!(plan.tail_latency_config.calibrated_p99_ms, 270);
        assert_eq!(plan.tail_latency_config.calibrated_p999_ms, 1_200);

        let governor = ResourceGovernor::with_budgets(plan.resource_budgets);
        let request =
            ResourceRequest::new(ResourceOperationKind::Tool, "read").with_queue_depth(128);
        let decision = governor.admit_sample(
            &request,
            HostResourceSample {
                load_avg_1m: Some(20.0),
                rss_bytes: Some(512 * 1024 * 1024),
                process_count: Some(128),
                fd_count: Some(128),
            },
        );
        assert_eq!(decision.action, AdmissionAction::Admit);
    }

    #[test]
    fn capacity_plan_telemetry_contains_stable_schema() {
        let plan =
            plan_swarm_capacity_from_jsonl(capacity_fixture_jsonl(), capacity_inventory()).unwrap();

        let telemetry = plan.telemetry();

        assert_eq!(
            telemetry.get("schema").and_then(serde_json::Value::as_str),
            Some(SWARM_CAPACITY_PLAN_SCHEMA)
        );
        assert_eq!(
            telemetry
                .get("recommended_agent_concurrency")
                .and_then(serde_json::Value::as_u64),
            Some(32)
        );
    }

    #[test]
    fn capacity_plan_fails_closed_without_swarm_metrics() {
        let err = plan_swarm_capacity_from_jsonl(
            "{\"schema\":\"pi.perf.session_workload_matrix_cell.v1\"}\n",
            capacity_inventory(),
        )
        .unwrap_err();

        assert!(matches!(
            err,
            SwarmCapacityPlanError::MissingEvidence("swarm_metrics")
        ));
    }

    #[test]
    fn capacity_plan_what_if_reduces_budgets_for_smaller_hosts() {
        let plan =
            plan_swarm_capacity_from_jsonl(capacity_fixture_jsonl(), capacity_inventory()).unwrap();

        let constrained = plan
            .what_if(SwarmHostInventory::new(16, 16, 1_024))
            .unwrap();

        assert!(constrained.recommended_agent_concurrency < plan.recommended_agent_concurrency);
        assert!(
            constrained.recommended_rch_verification_fanout
                < plan.recommended_rch_verification_fanout
        );
        assert!(constrained.resource_budgets.max_rss_bytes < plan.resource_budgets.max_rss_bytes);
        assert_eq!(constrained.host_inventory.observed_cpu_cores, 16);
        assert!(
            constrained
                .uncertainties
                .iter()
                .any(|uncertainty| uncertainty == "rss_exceeds_memory_headroom")
        );
    }

    #[test]
    fn operator_budget_profiles_cover_common_large_hosts() {
        let profiles = generate_operator_budget_profiles_from_jsonl(
            capacity_fixture_jsonl(),
            capacity_inventory(),
        )
        .unwrap();

        assert_eq!(profiles.schema, SWARM_OPERATOR_BUDGET_PROFILES_SCHEMA);
        assert_eq!(
            profiles.source_plan_confidence,
            SwarmCapacityConfidence::High
        );
        assert_eq!(profiles.evidence.complete_records, 3);
        assert_eq!(profiles.profiles.len(), 3);

        let small = profiles.profile("cpu16_mem64gib").unwrap();
        let medium = profiles.profile("cpu32_mem128gib").unwrap();
        let large = profiles.profile("cpu64_mem256gib").unwrap();

        assert_eq!(
            small.host_inventory,
            SwarmHostInventory::new(16, 16, 65_536)
        );
        assert_eq!(
            medium.host_inventory,
            SwarmHostInventory::new(32, 32, 131_072)
        );
        assert_eq!(large.host_inventory, capacity_inventory());
        assert!(small.recommended_agent_concurrency < medium.recommended_agent_concurrency);
        assert!(medium.recommended_agent_concurrency < large.recommended_agent_concurrency);
        assert!(
            small.recommended_rch_verification_fanout < large.recommended_rch_verification_fanout
        );
        assert_eq!(small.confidence, SwarmCapacityConfidence::Medium);
        assert_eq!(large.confidence, SwarmCapacityConfidence::High);
        assert!(
            small
                .caveats
                .iter()
                .any(|caveat| caveat == "starting_point_not_release_performance_claim")
        );
        assert!(
            small
                .caveats
                .iter()
                .any(|caveat| caveat.starts_with("derived_from_64cpu_262144mib"))
        );
    }

    #[test]
    fn operator_budget_profiles_telemetry_contains_stable_schema() {
        let profiles = generate_operator_budget_profiles_from_jsonl(
            capacity_fixture_jsonl(),
            capacity_inventory(),
        )
        .unwrap();

        let telemetry = profiles.telemetry();

        assert_eq!(
            telemetry.get("schema").and_then(serde_json::Value::as_str),
            Some(SWARM_OPERATOR_BUDGET_PROFILES_SCHEMA)
        );
        assert_eq!(
            telemetry
                .get("profiles")
                .and_then(serde_json::Value::as_array)
                .map(std::vec::Vec::len),
            Some(3)
        );
    }

    #[test]
    fn operator_budget_profiles_fail_closed_for_bad_inventory() {
        let err = generate_operator_budget_profiles_from_jsonl(
            capacity_fixture_jsonl(),
            SwarmHostInventory::new(0, 64, 262_144),
        )
        .unwrap_err();

        assert!(matches!(
            err,
            SwarmCapacityPlanError::InvalidHostInventory("target_cpu_cores")
        ));
    }

    #[test]
    fn operator_budget_profiles_fail_closed_for_empty_host_classes() {
        let err = generate_operator_budget_profiles_from_jsonl_with_host_classes(
            capacity_fixture_jsonl(),
            capacity_inventory(),
            &[],
        )
        .unwrap_err();

        assert!(matches!(
            err,
            SwarmCapacityPlanError::MissingEvidence("operator_host_classes")
        ));
    }

    #[test]
    fn operator_budget_profiles_fail_closed_for_malformed_host_class() {
        let malformed = [SwarmOperatorHostClass {
            id: "bad",
            description: "missing observed CPU",
            inventory: SwarmHostInventory::new(16, 0, 65_536),
        }];

        let err = generate_operator_budget_profiles_from_jsonl_with_host_classes(
            capacity_fixture_jsonl(),
            capacity_inventory(),
            &malformed,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            SwarmCapacityPlanError::InvalidHostInventory("observed_cpu_cores")
        ));
    }

    #[test]
    fn memory_pressure_replay_exploits_large_host_without_premature_degradation()
    -> Result<(), SwarmCapacityPlanError> {
        let profile = high_capacity_memory_pressure_profile();
        let report = replay_swarm_memory_pressure_profiles(
            capacity_fixture_jsonl(),
            capacity_inventory(),
            &[profile],
        )?;

        assert_eq!(report.schema, SWARM_MEMORY_PRESSURE_REPLAY_SCHEMA);
        assert_eq!(report.verdict, SwarmMemoryPressureReplayVerdict::Pass);
        assert_eq!(report.profile_count, 1);
        assert_eq!(report.source_plan_confidence, SwarmCapacityConfidence::High);

        let [decision] = report.decisions.as_slice() else {
            assert_eq!(report.decisions.len(), 1);
            return Ok(());
        };
        assert_eq!(decision.profile_id, "cpu64_mem256gib_nominal");
        assert_eq!(decision.admission_action, AdmissionAction::Admit);
        assert!(!decision.fail_closed);
        assert_eq!(
            decision.actions,
            [SwarmMemoryPressureReplayAction::Continue]
        );
        assert_eq!(decision.budgets.recommended_agent_concurrency, 32);
        assert_eq!(decision.budgets.recommended_tool_concurrency, 64);
        assert!(decision.budgets.max_message_tokens >= 524_288);
        assert!(decision.message_pressure_ratio < 0.70);
        assert!(decision.tool_output_pressure_ratio < 0.70);
        assert!(decision.extension_workload_pressure_ratio < 0.70);
        assert_eq!(
            report.telemetry().get("schema").and_then(Value::as_str),
            Some(SWARM_MEMORY_PRESSURE_REPLAY_SCHEMA)
        );
        Ok(())
    }

    #[test]
    fn memory_pressure_replay_degrades_constrained_container_before_oom()
    -> Result<(), SwarmCapacityPlanError> {
        let profile = constrained_memory_pressure_profile();
        let report = replay_swarm_memory_pressure_profiles(
            capacity_fixture_jsonl(),
            capacity_inventory(),
            &[profile],
        )?;

        assert_eq!(report.verdict, SwarmMemoryPressureReplayVerdict::Pass);
        let [decision] = report.decisions.as_slice() else {
            assert_eq!(report.decisions.len(), 1);
            return Ok(());
        };
        assert_eq!(decision.profile_id, "cgroup_mem1gib_degraded");
        assert_eq!(decision.verdict, SwarmMemoryPressureReplayVerdict::Pass);
        assert_eq!(decision.admission_action, AdmissionAction::Deny);
        assert!(decision.fail_closed);
        assert!(
            decision
                .actions
                .contains(&SwarmMemoryPressureReplayAction::CompactMessages)
        );
        assert!(
            decision
                .actions
                .contains(&SwarmMemoryPressureReplayAction::TrimToolOutput)
        );
        assert!(
            decision
                .actions
                .contains(&SwarmMemoryPressureReplayAction::ThrottleExtensionHostcalls)
        );
        assert!(
            decision
                .actions
                .contains(&SwarmMemoryPressureReplayAction::Deny)
        );
        assert_eq!(
            decision
                .admission_decision
                .resource_decision
                .dominant_dimension,
            ResourceDimension::ToolOutput
        );
        assert!(decision.budgets.max_rss_bytes < 1024 * 1024 * 1024);
        assert!(matches!(
            decision
                .admission_decision
                .resource_decision
                .sample
                .rss_bytes,
            Some(bytes) if bytes < 1024 * 1024 * 1024
        ));
        assert!(decision.message_pressure_ratio >= 1.0);
        assert!(decision.tool_output_pressure_ratio >= 1.0);
        assert!(decision.extension_workload_pressure_ratio >= 1.0);
        Ok(())
    }

    #[test]
    fn memory_pressure_replay_fails_closed_on_expected_action_mismatch()
    -> Result<(), SwarmCapacityPlanError> {
        let mut profile = high_capacity_memory_pressure_profile();
        profile.expected_admission_action = AdmissionAction::Deny;
        let report = replay_swarm_memory_pressure_profiles(
            capacity_fixture_jsonl(),
            capacity_inventory(),
            &[profile],
        )?;

        assert_eq!(report.verdict, SwarmMemoryPressureReplayVerdict::FailClosed);
        let [decision] = report.decisions.as_slice() else {
            assert_eq!(report.decisions.len(), 1);
            return Ok(());
        };
        assert_eq!(
            decision.verdict,
            SwarmMemoryPressureReplayVerdict::FailClosed
        );
        assert_eq!(decision.admission_action, AdmissionAction::Admit);
        assert!(
            decision
                .reasons
                .iter()
                .any(|reason| reason.contains("expected Deny, replayed Admit"))
        );
        Ok(())
    }

    #[test]
    fn live_swarm_admission_controller_admits_under_capacity_plan() {
        let plan =
            plan_swarm_capacity_from_jsonl(capacity_fixture_jsonl(), capacity_inventory()).unwrap();
        let mut controller = SwarmAdmissionController::from_plan(plan.clone());
        let load = SwarmLiveLoad::empty()
            .with_active_agents(8)
            .with_active_tool_calls(16)
            .with_extension_hostcall_lanes(4)
            .with_active_rch_jobs(1);

        let decision = controller.decide(
            &live_controller_request(),
            live_controller_sample(),
            healthy_tail_sample(),
            load,
        );

        assert_eq!(decision.schema, SWARM_ADMISSION_CONTROLLER_SCHEMA);
        assert_eq!(decision.action, AdmissionAction::Admit);
        assert_eq!(
            decision.capacity_pressure.dimension,
            SwarmCapacityDimension::ActiveAgents
        );
        assert_eq!(
            decision.recommended_agent_concurrency,
            plan.recommended_agent_concurrency
        );
        assert_eq!(
            controller.tail_latency_regime(),
            TailLatencyRegime::Calibrated
        );
    }

    #[test]
    fn live_swarm_admission_controller_backpressures_near_capacity_budget() {
        let plan =
            plan_swarm_capacity_from_jsonl(capacity_fixture_jsonl(), capacity_inventory()).unwrap();
        let mut controller = SwarmAdmissionController::from_plan(plan.clone());
        let load = SwarmLiveLoad::empty()
            .with_active_agents(24)
            .with_active_tool_calls(16)
            .with_extension_hostcall_lanes(4)
            .with_active_rch_jobs(1);

        let decision = controller.decide(
            &live_controller_request(),
            live_controller_sample(),
            healthy_tail_sample(),
            load,
        );

        assert_eq!(decision.action, AdmissionAction::Backpressure);
        assert_eq!(
            decision.capacity_pressure.dimension,
            SwarmCapacityDimension::ActiveAgents
        );
        assert!(decision.retry_after_ms >= plan.backoff_initial_ms);
        assert!(decision.reason.contains("swarm capacity pressure"));
    }

    #[test]
    fn live_swarm_admission_controller_denies_when_capacity_budget_is_exhausted() {
        let plan =
            plan_swarm_capacity_from_jsonl(capacity_fixture_jsonl(), capacity_inventory()).unwrap();
        let mut controller = SwarmAdmissionController::from_plan(plan);
        let load = SwarmLiveLoad::empty()
            .with_active_agents(8)
            .with_active_tool_calls(64)
            .with_extension_hostcall_lanes(4)
            .with_active_rch_jobs(1);

        let decision = controller.decide(
            &live_controller_request(),
            live_controller_sample(),
            healthy_tail_sample(),
            load,
        );

        assert_eq!(decision.action, AdmissionAction::Deny);
        assert_eq!(
            decision.capacity_pressure.dimension,
            SwarmCapacityDimension::ActiveToolCalls
        );
        assert_eq!(decision.retry_after_ms, 0);
        assert!(decision.reason.contains("swarm capacity pressure"));
    }

    #[test]
    fn live_swarm_admission_controller_telemetry_contains_stable_schema() {
        let plan =
            plan_swarm_capacity_from_jsonl(capacity_fixture_jsonl(), capacity_inventory()).unwrap();
        let mut controller = SwarmAdmissionController::from_plan(plan);
        let decision = controller.decide(
            &live_controller_request(),
            live_controller_sample(),
            healthy_tail_sample(),
            SwarmLiveLoad::empty().with_active_agents(8),
        );

        let telemetry = decision.telemetry();

        assert_eq!(
            telemetry.get("schema").and_then(serde_json::Value::as_str),
            Some(SWARM_ADMISSION_CONTROLLER_SCHEMA)
        );
        assert_eq!(
            telemetry
                .get("resource_decision")
                .and_then(|value| value.get("action"))
                .and_then(serde_json::Value::as_str),
            Some("admit")
        );
    }

    #[test]
    fn admission_replay_sorts_out_of_order_events_and_allows_missing_optional_fields() {
        let mut ledger = SwarmActivityLedger::new();
        ledger.append(
            300,
            SwarmActivityKind::Verification,
            SwarmActivityIds::new("later"),
            "later verification",
            [("expected_action", "admit")],
        );
        ledger.append(
            100,
            SwarmActivityKind::BeadStatus,
            SwarmActivityIds::new("earlier"),
            "earlier claim with no optional request fields",
            std::iter::empty::<(&str, &str)>(),
        );
        let plan =
            plan_swarm_capacity_from_jsonl(capacity_fixture_jsonl(), capacity_inventory()).unwrap();
        let report = replay_swarm_admission_from_jsonl(
            &ledger.to_jsonl().unwrap(),
            plan,
            &[replay_sample(0, 8, 16)],
            SwarmAdmissionReplayConfig::new(1_000),
        )
        .unwrap();

        assert_eq!(report.schema, SWARM_ADMISSION_REPLAY_SCHEMA);
        assert_eq!(report.status, SwarmAdmissionReplayStatus::Pass);
        assert_eq!(report.replayed_event_count, 2);
        assert_eq!(report.decision_count, 2);
        assert_eq!(report.decision_timeline[0].correlation_id, "earlier");
        assert!(report.decision_timeline[0].expected_action.is_none());
        assert_eq!(
            report.decision_timeline[1].admission_decision.action,
            AdmissionAction::Admit
        );
        assert_eq!(
            report.decision_timeline[1].dominant_pressure.dimension,
            SwarmCapacityDimension::ActiveAgents
        );
    }

    #[test]
    fn admission_replay_marks_duplicate_correlation_ids_and_expected_action_mismatch() {
        let mut ledger = SwarmActivityLedger::new();
        let ids = SwarmActivityIds::new("dup").with_bead_id("bd-replay");
        ledger.append(
            100,
            SwarmActivityKind::BeadStatus,
            ids.clone(),
            "first claim",
            [("expected_action", "deny")],
        );
        ledger.append(
            101,
            SwarmActivityKind::AgentMail,
            ids,
            "duplicate thread note",
            [("expected_action", "admit")],
        );
        let plan =
            plan_swarm_capacity_from_jsonl(capacity_fixture_jsonl(), capacity_inventory()).unwrap();
        let report = replay_swarm_admission_from_jsonl(
            &ledger.to_jsonl().unwrap(),
            plan,
            &[replay_sample(0, 8, 16)],
            SwarmAdmissionReplayConfig::new(1_000),
        )
        .unwrap();

        assert_eq!(report.status, SwarmAdmissionReplayStatus::Pass);
        assert!(report.divergence_markers.iter().any(
            |marker| marker.kind == SwarmAdmissionReplayDivergenceKind::DuplicateCorrelationId
        ));
        assert!(report.divergence_markers.iter().any(
            |marker| marker.kind == SwarmAdmissionReplayDivergenceKind::ExpectedActionMismatch
        ));
        assert_eq!(report.decision_count, 2);
    }

    #[test]
    fn admission_replay_fails_closed_for_stale_samples() {
        let mut ledger = SwarmActivityLedger::new();
        ledger.append(
            10_000,
            SwarmActivityKind::RchJob,
            SwarmActivityIds::new("stale"),
            "rch queued",
            [("expected_action", "admit")],
        );
        let plan =
            plan_swarm_capacity_from_jsonl(capacity_fixture_jsonl(), capacity_inventory()).unwrap();
        let report = replay_swarm_admission_from_jsonl(
            &ledger.to_jsonl().unwrap(),
            plan,
            &[replay_sample(0, 8, 16)],
            SwarmAdmissionReplayConfig::new(10),
        )
        .unwrap();

        assert_eq!(report.status, SwarmAdmissionReplayStatus::FailClosed);
        assert_eq!(report.decision_count, 0);
        assert_eq!(
            report.divergence_markers[0].kind,
            SwarmAdmissionReplayDivergenceKind::StaleResourceSample
        );
    }

    #[test]
    fn admission_replay_requires_captured_resource_samples() {
        let mut ledger = SwarmActivityLedger::new();
        ledger.append(
            100,
            SwarmActivityKind::FileReservation,
            SwarmActivityIds::new("reservation"),
            "reservation requested",
            std::iter::empty::<(&str, &str)>(),
        );
        let plan =
            plan_swarm_capacity_from_jsonl(capacity_fixture_jsonl(), capacity_inventory()).unwrap();
        let err = replay_swarm_admission_from_jsonl(
            &ledger.to_jsonl().unwrap(),
            plan,
            &[],
            SwarmAdmissionReplayConfig::default(),
        )
        .unwrap_err();

        assert!(matches!(
            err,
            SwarmAdmissionReplayError::MissingEvidence("resource_samples")
        ));
    }

    #[test]
    fn admission_replay_alignment_accepts_matching_saturated_backpressure_evidence() {
        let jsonl = saturated_admission_fixture_jsonl();
        let digest = digest_from_jsonl(&jsonl, saturated_digest_config()).unwrap();
        let plan =
            plan_swarm_capacity_from_jsonl(capacity_fixture_jsonl(), capacity_inventory()).unwrap();
        let report = replay_swarm_admission_from_jsonl(
            &jsonl,
            plan,
            &[replay_sample(0, 24, 16)],
            SwarmAdmissionReplayConfig::new(1_000),
        )
        .unwrap();

        assert!(digest.saturation.saturated);
        assert_eq!(report.status, SwarmAdmissionReplayStatus::Pass);
        assert!(report.decision_timeline.iter().any(|decision| matches!(
            decision.admission_decision.action,
            AdmissionAction::Backpressure
        )));

        let alignment = assert_swarm_digest_admission_replay_alignment(&digest, &report);

        assert_eq!(
            alignment.schema,
            SWARM_ADMISSION_REPLAY_DIGEST_ALIGNMENT_SCHEMA
        );
        assert_eq!(alignment.status, SwarmAdmissionReplayStatus::Pass);
        assert_eq!(
            alignment.digest_severity,
            SwarmAdmissionReplayDigestSeverity::Degraded
        );
        assert_eq!(
            alignment.replay_severity,
            SwarmAdmissionReplayDigestSeverity::Degraded
        );
        assert!(alignment.actionable_assertions.is_empty());
        assert!(
            alignment
                .digest_evidence_pointers
                .iter()
                .any(|pointer| pointer.starts_with("new_bug_window:start="))
        );
        assert_eq!(
            report.telemetry().get("schema").and_then(Value::as_str),
            Some(SWARM_ADMISSION_REPLAY_SCHEMA)
        );
        assert!(report.telemetry().get("digest_severity").is_none());
    }

    #[test]
    fn admission_replay_alignment_fails_closed_for_saturated_digest_and_safe_replay() {
        let jsonl = saturated_admission_fixture_jsonl();
        let digest = digest_from_jsonl(&jsonl, saturated_digest_config()).unwrap();
        let plan =
            plan_swarm_capacity_from_jsonl(capacity_fixture_jsonl(), capacity_inventory()).unwrap();
        let report = replay_swarm_admission_from_jsonl(
            &jsonl,
            plan,
            &[replay_sample(0, 8, 16)],
            SwarmAdmissionReplayConfig::new(1_000),
        )
        .unwrap();

        assert!(digest.saturation.saturated);
        assert_eq!(report.status, SwarmAdmissionReplayStatus::Pass);
        assert!(
            report.decision_timeline.iter().all(|decision| matches!(
                decision.admission_decision.action,
                AdmissionAction::Admit
            ))
        );

        let alignment = assert_swarm_digest_admission_replay_alignment(&digest, &report);

        assert_eq!(alignment.status, SwarmAdmissionReplayStatus::FailClosed);
        assert_eq!(
            alignment.digest_severity,
            SwarmAdmissionReplayDigestSeverity::Degraded
        );
        assert_eq!(
            alignment.replay_severity,
            SwarmAdmissionReplayDigestSeverity::Safe
        );
        assert_eq!(alignment.actionable_assertions.len(), 1);
        assert_eq!(
            alignment.actionable_assertions[0].kind,
            SwarmAdmissionReplayDigestAssertionKind::SaturatedDigestOptimisticReplay
        );
        assert!(
            alignment.actionable_assertions[0]
                .recommended_operator_action
                .contains("pause new agent launches")
        );
    }

    #[test]
    fn admission_replay_alignment_fails_closed_for_unsaturated_digest_and_degraded_replay() {
        let jsonl = saturated_admission_fixture_jsonl();
        let digest = digest_from_jsonl(
            &jsonl,
            SwarmActivityDigestConfig {
                min_new_bugs_per_window: 0,
                ..saturated_digest_config()
            },
        )
        .unwrap();
        let plan =
            plan_swarm_capacity_from_jsonl(capacity_fixture_jsonl(), capacity_inventory()).unwrap();
        let report = replay_swarm_admission_from_jsonl(
            &jsonl,
            plan,
            &[replay_sample(0, 24, 16)],
            SwarmAdmissionReplayConfig::new(1_000),
        )
        .unwrap();

        assert!(!digest.saturation.saturated);

        let alignment = assert_swarm_digest_admission_replay_alignment(&digest, &report);

        assert_eq!(alignment.status, SwarmAdmissionReplayStatus::FailClosed);
        assert_eq!(
            alignment.digest_severity,
            SwarmAdmissionReplayDigestSeverity::Safe
        );
        assert_eq!(
            alignment.replay_severity,
            SwarmAdmissionReplayDigestSeverity::Degraded
        );
        assert_eq!(
            alignment.actionable_assertions[0].kind,
            SwarmAdmissionReplayDigestAssertionKind::UnsaturatedDigestConservativeReplay
        );
    }

    #[test]
    fn tail_latency_regime_telemetry_contains_stable_schema() {
        let mut guard = TailLatencyRegimeGuard::new(TailLatencyRegimeConfig::new(
            100, 500, 4, 0.80, 0.50, 1, 2,
        ));
        let decision = guard.observe(TailLatencyRegimeSample::new(150, 700, 8, 0.90));

        let telemetry = decision.telemetry();

        assert_eq!(
            telemetry.get("schema").and_then(serde_json::Value::as_str),
            Some(TAIL_LATENCY_REGIME_SCHEMA)
        );
        assert_eq!(
            telemetry
                .get("decision")
                .and_then(|value| value.get("regime"))
                .and_then(serde_json::Value::as_str),
            Some("conservative_fallback")
        );
        assert_eq!(
            telemetry
                .get("decision")
                .and_then(|value| value.get("sample"))
                .and_then(|value| value.get("queue_depth"))
                .and_then(serde_json::Value::as_u64),
            Some(8)
        );
    }

    #[test]
    fn tail_latency_guard_requires_consecutive_spikes_before_fallback() {
        let mut guard = TailLatencyRegimeGuard::new(tail_config());
        let spike = TailLatencyRegimeSample::new(150, 700, 8, 0.90);

        let first = guard.observe(spike);
        assert_eq!(first.regime, TailLatencyRegime::Calibrated);
        assert!(!first.fallback_active);
        assert_eq!(first.bad_sample_streak, 1);
        assert!(
            first
                .reasons
                .contains(&TailLatencyFallbackReason::P99Latency)
        );
        assert!(
            first
                .reasons
                .contains(&TailLatencyFallbackReason::P999Latency)
        );
        assert!(
            first
                .reasons
                .contains(&TailLatencyFallbackReason::QueueDepth)
        );
        assert!(
            first
                .reasons
                .contains(&TailLatencyFallbackReason::ResourcePressure)
        );

        let second = guard.observe(spike);
        assert_eq!(second.regime, TailLatencyRegime::ConservativeFallback);
        assert!(second.fallback_active);
        assert!(second.changed);
        assert_eq!(second.bad_sample_streak, 2);
    }

    #[test]
    fn tail_latency_guard_hysteresis_requires_recovery_sequence() {
        let mut guard = TailLatencyRegimeGuard::new(tail_config());
        let spike = TailLatencyRegimeSample::new(150, 700, 8, 0.90);
        guard.observe(spike);
        guard.observe(spike);

        let recovered = TailLatencyRegimeSample::new(40, 200, 2, 0.30);
        let first_recovery = guard.observe(recovered);
        assert_eq!(
            first_recovery.regime,
            TailLatencyRegime::ConservativeFallback
        );
        assert!(first_recovery.fallback_active);
        assert!(
            first_recovery
                .reasons
                .contains(&TailLatencyFallbackReason::HysteresisHold)
        );
        assert_eq!(first_recovery.recovery_sample_streak, 1);

        let second_recovery = guard.observe(recovered);
        assert_eq!(second_recovery.regime, TailLatencyRegime::Calibrated);
        assert!(!second_recovery.fallback_active);
        assert!(second_recovery.changed);
        assert!(second_recovery.reasons.is_empty());
    }

    #[test]
    fn tail_latency_fallback_applies_conservative_budgets_to_admission() {
        let governor = ResourceGovernor::with_budgets(HostResourceBudgets::fixed_with_queue_depth(
            10.0, 1_000, 100, 100, 1_000, 16,
        ));
        let request = ResourceRequest::new(ResourceOperationKind::Tool, "read")
            .with_estimated_tool_output_bytes(600)
            .with_queue_depth(4);
        let normal = governor.admit_sample(&request, sample());
        assert_eq!(normal.action, AdmissionAction::Admit);

        let mut guard = TailLatencyRegimeGuard::new(TailLatencyRegimeConfig::new(
            100, 500, 4, 0.80, 0.50, 1, 2,
        ));
        let (fallback, regime) = governor.admit_sample_with_tail_latency_guard(
            &request,
            sample(),
            &mut guard,
            TailLatencyRegimeSample::new(150, 700, 8, 0.90),
        );

        assert!(regime.fallback_active);
        assert_eq!(fallback.action, AdmissionAction::Deny);
        assert!(fallback.conservative_fallback_active);
        assert!(
            fallback
                .fallback_reasons
                .contains(&TailLatencyFallbackReason::P99Latency)
        );
        assert_eq!(fallback.budgets.max_tool_output_bytes, 500);
    }
}
