//! Tier-2 trace-JIT compiler for stabilized superinstruction plans.
//!
//! Compiles high-confidence hostcall superinstruction traces into guarded
//! native dispatch stubs, removing residual interpreter overhead while
//! preserving deterministic fallback to the sequential dispatch path.
//!
//! # Architecture
//!
//! The JIT tier sits above the superinstruction compiler:
//!
//! ```text
//! Tier 0: Interpreter (sequential dispatch via match)
//! Tier 1: Superinstruction fusion (plan-based dispatch, see hostcall_superinstructions.rs)
//! Tier 2: Trace-JIT (pre-compiled dispatch tables with guard stubs) ← this module
//! ```
//!
//! A plan is promoted to JIT when it reaches a configurable hotness
//! threshold (`min_jit_executions`). The compiled trace holds a guard
//! sequence that is checked at entry; on guard failure the trace
//! deoptimizes and falls back to tier-0 sequential dispatch.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::hostcall_superinstructions::HostcallSuperinstructionPlan;

// ── Configuration constants ──────────────────────────────────────────

/// Minimum executions of a superinstruction plan before JIT promotion.
const DEFAULT_MIN_JIT_EXECUTIONS: u64 = 8;
/// Maximum compiled traces held in cache before LRU eviction.
const DEFAULT_MAX_COMPILED_TRACES: usize = 64;
/// Maximum consecutive guard failures before a trace is invalidated.
const DEFAULT_MAX_GUARD_FAILURES: u64 = 4;
/// Cost units for a JIT-compiled dispatch (lower than fused tier-1).
const JIT_DISPATCH_COST_UNITS: i64 = 3;
/// Per-opcode marginal cost in JIT dispatch.
const JIT_DISPATCH_STEP_COST_UNITS: i64 = 1;

// ── Configuration ────────────────────────────────────────────────────

/// Configuration for the trace-JIT compiler tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceJitConfig {
    /// Whether the JIT tier is enabled.
    pub enabled: bool,
    /// Minimum plan executions before promoting to JIT.
    pub min_jit_executions: u64,
    /// Maximum compiled traces in cache.
    pub max_compiled_traces: usize,
    /// Maximum consecutive guard failures before invalidation.
    pub max_guard_failures: u64,
}

impl Default for TraceJitConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

impl TraceJitConfig {
    /// Create a new config with explicit values.
    #[must_use]
    pub const fn new(
        enabled: bool,
        min_jit_executions: u64,
        max_compiled_traces: usize,
        max_guard_failures: u64,
    ) -> Self {
        Self {
            enabled,
            min_jit_executions,
            max_compiled_traces,
            max_guard_failures,
        }
    }

    /// Create from environment variables.
    #[must_use]
    pub fn from_env() -> Self {
        let enabled = bool_from_env("PI_HOSTCALL_TRACE_JIT", true);
        let min_jit_executions = u64_from_env(
            "PI_HOSTCALL_TRACE_JIT_MIN_EXECUTIONS",
            DEFAULT_MIN_JIT_EXECUTIONS,
        );
        let max_compiled_traces = usize_from_env(
            "PI_HOSTCALL_TRACE_JIT_MAX_TRACES",
            DEFAULT_MAX_COMPILED_TRACES,
        );
        let max_guard_failures = u64_from_env(
            "PI_HOSTCALL_TRACE_JIT_MAX_GUARD_FAILURES",
            DEFAULT_MAX_GUARD_FAILURES,
        );
        Self::new(
            enabled,
            min_jit_executions,
            max_compiled_traces,
            max_guard_failures,
        )
    }
}

// ── Guard condition ──────────────────────────────────────────────────

/// A guard condition that must hold for a compiled trace to execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraceGuard {
    /// Trace prefix must match the given opcode window exactly.
    OpcodePrefix(Vec<String>),
    /// Safety envelope must not be in a vetoing state.
    SafetyEnvelopeNotVetoing,
    /// Minimum support count threshold must still be met.
    MinSupportCount(u32),
}

impl TraceGuard {
    /// Check this guard against the given trace and context.
    #[must_use]
    pub fn check(&self, trace: &[String], ctx: &GuardContext) -> bool {
        match self {
            Self::OpcodePrefix(window) => {
                trace.len() >= window.len()
                    && trace
                        .iter()
                        .zip(window.iter())
                        .all(|(actual, expected)| actual == expected)
            }
            Self::SafetyEnvelopeNotVetoing => !ctx.safety_envelope_vetoing,
            Self::MinSupportCount(min) => ctx.current_support_count >= *min,
        }
    }

    /// Zero-allocation `&[&str]` variant of [`Self::check`].
    #[must_use]
    pub fn check_str(&self, trace: &[&str], ctx: &GuardContext) -> bool {
        match self {
            Self::OpcodePrefix(window) => {
                trace.len() >= window.len()
                    && trace
                        .iter()
                        .zip(window.iter())
                        .all(|(actual, expected)| *actual == expected.as_str())
            }
            Self::SafetyEnvelopeNotVetoing => !ctx.safety_envelope_vetoing,
            Self::MinSupportCount(min) => ctx.current_support_count >= *min,
        }
    }
}

/// Context supplied to guard checks.
#[derive(Debug, Clone, Default)]
pub struct GuardContext {
    /// Whether any safety envelope is currently vetoing.
    pub safety_envelope_vetoing: bool,
    /// Current support count for the plan being checked.
    pub current_support_count: u32,
}

// ── Compiled trace ───────────────────────────────────────────────────

/// Compilation tier for a trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompilationTier {
    /// Tier 1: superinstruction fusion (plan-based).
    Superinstruction,
    /// Tier 2: JIT-compiled dispatch table.
    TraceJit,
}

/// A compiled trace stub with guard conditions and dispatch metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledTrace {
    /// Plan ID this trace was compiled from.
    pub plan_id: String,
    /// Trace signature (hash of opcode window).
    pub trace_signature: String,
    /// Guard conditions that must all hold for this trace to execute.
    pub guards: Vec<TraceGuard>,
    /// The opcode window this trace covers.
    pub opcode_window: Vec<String>,
    /// Width of the compiled trace (number of opcodes fused).
    pub width: usize,
    /// Estimated cost of JIT dispatch (lower than tier-1 fused cost).
    pub estimated_cost_jit: i64,
    /// Original tier-1 fused cost for comparison.
    pub estimated_cost_fused: i64,
    /// Cost improvement over tier-1: `fused - jit`.
    pub tier_improvement_delta: i64,
    /// Compilation tier.
    pub tier: CompilationTier,
}

impl CompiledTrace {
    /// Create a compiled trace from a superinstruction plan.
    #[must_use]
    pub fn from_plan(plan: &HostcallSuperinstructionPlan) -> Self {
        let width = plan.width();
        let estimated_cost_jit = estimated_jit_cost(width);
        let tier_improvement_delta = plan.estimated_cost_fused.saturating_sub(estimated_cost_jit);

        let guards = vec![
            TraceGuard::OpcodePrefix(plan.opcode_window.clone()),
            TraceGuard::SafetyEnvelopeNotVetoing,
            TraceGuard::MinSupportCount(plan.support_count / 2),
        ];

        Self {
            plan_id: plan.plan_id.clone(),
            trace_signature: plan.trace_signature.clone(),
            guards,
            opcode_window: plan.opcode_window.clone(),
            width,
            estimated_cost_jit,
            estimated_cost_fused: plan.estimated_cost_fused,
            tier_improvement_delta,
            tier: CompilationTier::TraceJit,
        }
    }

    /// Check all guards against the given trace and context.
    #[must_use]
    pub fn guards_pass(&self, trace: &[String], ctx: &GuardContext) -> bool {
        self.guards.iter().all(|guard| guard.check(trace, ctx))
    }
}

// ── Deoptimization ───────────────────────────────────────────────────

/// Reason why a JIT-compiled trace deoptimized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeoptReason {
    /// Guard check failed.
    GuardFailure {
        /// Which guard failed (index into guards vec).
        guard_index: usize,
        /// Description of the failure.
        description: String,
    },
    /// Trace was invalidated after too many guard failures.
    TraceInvalidated {
        /// Total guard failures before invalidation.
        total_failures: u64,
    },
    /// JIT tier is disabled.
    JitDisabled,
    /// No compiled trace exists for this plan.
    NotCompiled,
    /// Safety envelope vetoed execution.
    SafetyVeto,
}

/// Result of attempting to execute via JIT.
#[derive(Debug, Clone)]
pub struct JitExecutionResult {
    /// Whether JIT dispatch was used.
    pub jit_hit: bool,
    /// Plan ID if a compiled trace was found.
    pub plan_id: Option<String>,
    /// Deoptimization reason if JIT was not used.
    pub deopt_reason: Option<DeoptReason>,
    /// Estimated cost savings from JIT dispatch.
    pub cost_delta: i64,
}

// ── Execution tracking ───────────────────────────────────────────────

/// Per-plan execution profile for JIT promotion decisions.
#[derive(Debug, Clone, Default)]
struct PlanProfile {
    /// Number of times this plan has been executed at tier-1.
    execution_count: u64,
    /// Consecutive guard failures (reset on success).
    consecutive_guard_failures: u64,
    /// Whether this plan has been invalidated.
    invalidated: bool,
    /// LRU generation counter for eviction.
    last_access_generation: u64,
}

// ── Telemetry ────────────────────────────────────────────────────────

/// Snapshot of JIT compiler telemetry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceJitTelemetry {
    /// Total plans evaluated for promotion.
    pub plans_evaluated: u64,
    /// Total traces compiled to JIT.
    pub traces_compiled: u64,
    /// Total JIT dispatch hits (guard pass + JIT execution).
    pub jit_hits: u64,
    /// Total JIT dispatch misses (guard failure → fallback).
    pub jit_misses: u64,
    /// Total deoptimizations (guard failures).
    pub deopts: u64,
    /// Total trace invalidations (too many guard failures).
    pub invalidations: u64,
    /// Total LRU evictions from trace cache.
    pub evictions: u64,
    /// Current number of compiled traces in cache.
    pub cache_size: u64,
}

// ── Trace-JIT compiler ──────────────────────────────────────────────

/// Tier-2 trace-JIT compiler and dispatch cache.
///
/// Monitors superinstruction plan executions and promotes hot plans
/// to JIT-compiled dispatch stubs when they reach the hotness threshold.
#[derive(Debug, Clone)]
pub struct TraceJitCompiler {
    config: TraceJitConfig,
    /// Compiled trace cache keyed by plan_id.
    cache: BTreeMap<String, CompiledTrace>,
    /// Per-plan execution profiles.
    profiles: BTreeMap<String, PlanProfile>,
    /// Global generation counter for LRU.
    generation: u64,
    /// Telemetry counters.
    telemetry: TraceJitTelemetry,
}

impl Default for TraceJitCompiler {
    fn default() -> Self {
        Self::new(TraceJitConfig::default())
    }
}

impl TraceJitCompiler {
    /// Create a new JIT compiler with the given config.
    #[must_use]
    pub fn new(config: TraceJitConfig) -> Self {
        Self {
            config,
            cache: BTreeMap::new(),
            profiles: BTreeMap::new(),
            generation: 0,
            telemetry: TraceJitTelemetry::default(),
        }
    }

    /// Whether the JIT tier is enabled.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Access the current config.
    #[must_use]
    pub const fn config(&self) -> &TraceJitConfig {
        &self.config
    }

    /// Get a telemetry snapshot.
    #[must_use]
    pub const fn telemetry(&self) -> &TraceJitTelemetry {
        &self.telemetry
    }

    /// Number of compiled traces in cache.
    #[must_use]
    pub fn cache_size(&self) -> usize {
        self.cache.len()
    }

    /// Record that a superinstruction plan was executed at tier-1.
    ///
    /// If the plan reaches the hotness threshold, it is promoted to JIT.
    /// Returns `true` if the plan was promoted.
    pub fn record_plan_execution(&mut self, plan: &HostcallSuperinstructionPlan) -> bool {
        if !self.config.enabled {
            return false;
        }

        self.telemetry.plans_evaluated += 1;
        self.generation += 1;

        let (execution_count, invalidated) =
            if let Some(profile) = self.profiles.get_mut(&plan.plan_id) {
                profile.execution_count += 1;
                profile.last_access_generation = self.generation;
                (profile.execution_count, profile.invalidated)
            } else {
                let profile = self.profiles.entry(plan.plan_id.clone()).or_default();
                profile.execution_count += 1;
                profile.last_access_generation = self.generation;
                (profile.execution_count, profile.invalidated)
            };

        if invalidated {
            return false;
        }

        // Check if plan has reached the hotness threshold.
        if execution_count >= self.config.min_jit_executions
            && !self.cache.contains_key(&plan.plan_id)
        {
            self.compile_trace(plan);
            return true;
        }

        false
    }

    /// Compile a superinstruction plan into a JIT dispatch stub.
    fn compile_trace(&mut self, plan: &HostcallSuperinstructionPlan) {
        // Evict LRU if at capacity.
        if self.cache.len() >= self.config.max_compiled_traces {
            self.evict_lru();
        }

        let compiled = CompiledTrace::from_plan(plan);
        self.cache.insert(plan.plan_id.clone(), compiled);
        self.telemetry.traces_compiled += 1;
        self.telemetry.cache_size = u64::try_from(self.cache.len()).unwrap_or(u64::MAX);
    }

    /// Evict the least-recently-used trace from cache.
    fn evict_lru(&mut self) {
        let lru_plan_id = self
            .cache
            .keys()
            .min_by_key(|plan_id| {
                self.profiles
                    .get(*plan_id)
                    .map_or(0, |profile| profile.last_access_generation)
            })
            .cloned();

        if let Some(plan_id) = lru_plan_id {
            self.cache.remove(&plan_id);
            self.telemetry.evictions += 1;
            self.telemetry.cache_size = u64::try_from(self.cache.len()).unwrap_or(u64::MAX);
        }
    }

    /// Attempt JIT dispatch for a given trace and plan ID.
    ///
    /// Returns a `JitExecutionResult` indicating whether JIT was used
    /// and any deoptimization reason.
    pub fn try_jit_dispatch(
        &mut self,
        plan_id: &str,
        trace: &[String],
        ctx: &GuardContext,
    ) -> JitExecutionResult {
        if !self.config.enabled {
            return JitExecutionResult {
                jit_hit: false,
                plan_id: Some(plan_id.to_string()),
                deopt_reason: Some(DeoptReason::JitDisabled),
                cost_delta: 0,
            };
        }

        self.generation += 1;

        // Check if trace is compiled, evaluating guards without holding mutable borrow or cloning
        let (tier_improvement_delta, failed_guard) = {
            let Some(compiled) = self.cache.get(plan_id) else {
                return JitExecutionResult {
                    jit_hit: false,
                    plan_id: Some(plan_id.to_string()),
                    deopt_reason: Some(DeoptReason::NotCompiled),
                    cost_delta: 0,
                };
            };

            let mut failed_guard = None;
            for (idx, guard) in compiled.guards.iter().enumerate() {
                if !guard.check(trace, ctx) {
                    let description = match guard {
                        TraceGuard::OpcodePrefix(_) => "opcode_prefix_mismatch",
                        TraceGuard::SafetyEnvelopeNotVetoing => "safety_envelope_vetoing",
                        TraceGuard::MinSupportCount(_) => "support_count_below_threshold",
                    };
                    failed_guard = Some((idx, description));
                    break;
                }
            }

            (compiled.tier_improvement_delta, failed_guard)
        };

        // Update LRU.
        if let Some(profile) = self.profiles.get_mut(plan_id) {
            profile.last_access_generation = self.generation;
        }

        // Handle guard failure if one occurred.
        if let Some((idx, description)) = failed_guard {
            let invalidated_after_failures = self.record_guard_failure(plan_id);
            let deopt_reason = invalidated_after_failures.map_or_else(
                || DeoptReason::GuardFailure {
                    guard_index: idx,
                    description: description.to_string(),
                },
                |total_failures| DeoptReason::TraceInvalidated { total_failures },
            );
            return JitExecutionResult {
                jit_hit: false,
                plan_id: Some(plan_id.to_string()),
                deopt_reason: Some(deopt_reason),
                cost_delta: 0,
            };
        }

        // All guards passed — JIT dispatch.
        self.telemetry.jit_hits += 1;
        if let Some(profile) = self.profiles.get_mut(plan_id) {
            profile.consecutive_guard_failures = 0;
        }

        JitExecutionResult {
            jit_hit: true,
            plan_id: Some(plan_id.to_string()),
            deopt_reason: None,
            cost_delta: tier_improvement_delta,
        }
    }

    /// Zero-allocation `&[&str]` hot-path variant of [`Self::try_jit_dispatch`]
    /// returning `(jit_hit, cost_delta)` while maintaining identical telemetry,
    /// LRU updates, guard checks, and trace invalidation semantics.
    pub(crate) fn try_jit_dispatch_str(
        &mut self,
        plan_id: &str,
        trace: &[&str],
        ctx: &GuardContext,
    ) -> (bool, i64) {
        if !self.config.enabled {
            return (false, 0);
        }

        self.generation += 1;

        let (tier_improvement_delta, guard_failed) = {
            let Some(compiled) = self.cache.get(plan_id) else {
                return (false, 0);
            };

            let guard_failed = compiled
                .guards
                .iter()
                .any(|guard| !guard.check_str(trace, ctx));
            (compiled.tier_improvement_delta, guard_failed)
        };

        if let Some(profile) = self.profiles.get_mut(plan_id) {
            profile.last_access_generation = self.generation;
        }

        if guard_failed {
            let _ = self.record_guard_failure(plan_id);
            return (false, 0);
        }

        self.telemetry.jit_hits += 1;
        if let Some(profile) = self.profiles.get_mut(plan_id) {
            profile.consecutive_guard_failures = 0;
        }

        (true, tier_improvement_delta)
    }

    /// Record a guard failure for a plan, possibly invalidating the trace.
    fn record_guard_failure(&mut self, plan_id: &str) -> Option<u64> {
        self.telemetry.deopts += 1;
        self.telemetry.jit_misses += 1;

        if let Some(profile) = self.profiles.get_mut(plan_id) {
            profile.consecutive_guard_failures += 1;
            if !profile.invalidated
                && profile.consecutive_guard_failures >= self.config.max_guard_failures
            {
                profile.invalidated = true;
                self.cache.remove(plan_id);
                self.telemetry.invalidations += 1;
                self.telemetry.cache_size = u64::try_from(self.cache.len()).unwrap_or(u64::MAX);
                return Some(profile.consecutive_guard_failures);
            }
        }
        None
    }

    /// Look up a compiled trace by plan ID (read-only).
    #[must_use]
    pub fn get_compiled_trace(&self, plan_id: &str) -> Option<&CompiledTrace> {
        self.cache.get(plan_id)
    }

    /// Check if a plan ID has been invalidated.
    #[must_use]
    pub fn is_invalidated(&self, plan_id: &str) -> bool {
        self.profiles
            .get(plan_id)
            .is_some_and(|profile| profile.invalidated)
    }

    /// Reset all state (for testing or recalibration).
    pub fn reset(&mut self) {
        self.cache.clear();
        self.profiles.clear();
        self.generation = 0;
        self.telemetry = TraceJitTelemetry::default();
    }
}

// ── Cost estimation ──────────────────────────────────────────────────

/// Estimated JIT dispatch cost for a given opcode window width.
#[must_use]
pub fn estimated_jit_cost(width: usize) -> i64 {
    let width_units = i64::try_from(width).unwrap_or(i64::MAX);
    JIT_DISPATCH_COST_UNITS.saturating_add(width_units.saturating_mul(JIT_DISPATCH_STEP_COST_UNITS))
}

// ── Environment helpers ──────────────────────────────────────────────

fn bool_from_env(var: &str, default: bool) -> bool {
    std::env::var(var).ok().as_deref().map_or(default, |value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "disabled"
        )
    })
}

fn u64_from_env(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

fn usize_from_env(var: &str, default: usize) -> usize {
    std::env::var(var)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(default)
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hostcall_superinstructions::{
        HOSTCALL_SUPERINSTRUCTION_PLAN_VERSION, HOSTCALL_SUPERINSTRUCTION_SCHEMA_VERSION,
        HostcallSuperinstructionPlan,
    };

    fn make_plan(
        plan_id: &str,
        window: &[&str],
        support_count: u32,
    ) -> HostcallSuperinstructionPlan {
        let opcode_window: Vec<String> = window.iter().map(ToString::to_string).collect();
        let width = opcode_window.len();
        HostcallSuperinstructionPlan {
            schema: HOSTCALL_SUPERINSTRUCTION_SCHEMA_VERSION.to_string(),
            version: HOSTCALL_SUPERINSTRUCTION_PLAN_VERSION,
            plan_id: plan_id.to_string(),
            trace_signature: format!("sig_{plan_id}"),
            opcode_window,
            support_count,
            estimated_cost_baseline: i64::try_from(width).unwrap_or(0) * 10,
            estimated_cost_fused: 6 + i64::try_from(width).unwrap_or(0) * 2,
            expected_cost_delta: i64::try_from(width).unwrap_or(0) * 8 - 6,
        }
    }

    fn trace(opcodes: &[&str]) -> Vec<String> {
        opcodes.iter().map(ToString::to_string).collect()
    }

    fn default_ctx() -> GuardContext {
        GuardContext {
            safety_envelope_vetoing: false,
            current_support_count: 100,
        }
    }

    // ── Config tests ─────────────────────────────────────────────

    #[test]
    fn config_default_values() {
        let config = TraceJitConfig::new(true, 8, 64, 4);
        assert!(config.enabled);
        assert_eq!(config.min_jit_executions, 8);
        assert_eq!(config.max_compiled_traces, 64);
        assert_eq!(config.max_guard_failures, 4);
    }

    #[test]
    fn config_disabled_prevents_compilation() {
        let config = TraceJitConfig::new(false, 1, 64, 4);
        let mut jit = TraceJitCompiler::new(config);
        let plan = make_plan("p1", &["a", "b"], 10);

        let promoted = jit.record_plan_execution(&plan);
        assert!(!promoted);
        assert_eq!(jit.cache_size(), 0);
    }

    // ── Promotion tests ──────────────────────────────────────────

    #[test]
    fn plan_promoted_after_reaching_threshold() {
        let config = TraceJitConfig::new(true, 3, 64, 4);
        let mut jit = TraceJitCompiler::new(config);
        let plan = make_plan("p1", &["session.get_state", "session.get_messages"], 10);

        assert!(!jit.record_plan_execution(&plan));
        assert!(!jit.record_plan_execution(&plan));
        assert!(jit.record_plan_execution(&plan)); // 3rd = threshold
        assert_eq!(jit.cache_size(), 1);

        // Further executions don't re-compile.
        assert!(!jit.record_plan_execution(&plan));
        assert_eq!(jit.telemetry().traces_compiled, 1);
    }

    #[test]
    fn plan_not_promoted_before_threshold() {
        let config = TraceJitConfig::new(true, 10, 64, 4);
        let mut jit = TraceJitCompiler::new(config);
        let plan = make_plan("p1", &["a", "b"], 5);

        for _ in 0..9 {
            assert!(!jit.record_plan_execution(&plan));
        }
        assert_eq!(jit.cache_size(), 0);
        assert!(jit.record_plan_execution(&plan)); // 10th = threshold
        assert_eq!(jit.cache_size(), 1);
    }

    // ── Guard tests ──────────────────────────────────────────────

    #[test]
    fn guard_opcode_prefix_passes_on_match() {
        let guard = TraceGuard::OpcodePrefix(trace(&["a", "b"]));
        let ctx = default_ctx();
        assert!(guard.check(&trace(&["a", "b", "c"]), &ctx));
        assert!(guard.check(&trace(&["a", "b"]), &ctx));
    }

    #[test]
    fn guard_opcode_prefix_fails_on_mismatch() {
        let guard = TraceGuard::OpcodePrefix(trace(&["a", "b"]));
        let ctx = default_ctx();
        assert!(!guard.check(&trace(&["a", "c"]), &ctx));
        assert!(!guard.check(&trace(&["a"]), &ctx));
        assert!(!guard.check(&trace(&[]), &ctx));
    }

    #[test]
    fn guard_safety_envelope_passes_when_not_vetoing() {
        let guard = TraceGuard::SafetyEnvelopeNotVetoing;
        let ctx = GuardContext {
            safety_envelope_vetoing: false,
            ..default_ctx()
        };
        assert!(guard.check(&[], &ctx));
    }

    #[test]
    fn guard_safety_envelope_fails_when_vetoing() {
        let guard = TraceGuard::SafetyEnvelopeNotVetoing;
        let ctx = GuardContext {
            safety_envelope_vetoing: true,
            ..default_ctx()
        };
        assert!(!guard.check(&[], &ctx));
    }

    #[test]
    fn guard_min_support_count_passes() {
        let guard = TraceGuard::MinSupportCount(5);
        let ctx = GuardContext {
            current_support_count: 10,
            ..default_ctx()
        };
        assert!(guard.check(&[], &ctx));
    }

    #[test]
    fn guard_min_support_count_fails() {
        let guard = TraceGuard::MinSupportCount(5);
        let ctx = GuardContext {
            current_support_count: 3,
            ..default_ctx()
        };
        assert!(!guard.check(&[], &ctx));
    }

    // ── Compiled trace tests ─────────────────────────────────────

    #[test]
    fn compiled_trace_from_plan_sets_tier() {
        let plan = make_plan("p1", &["a", "b", "c"], 10);
        let compiled = CompiledTrace::from_plan(&plan);

        assert_eq!(compiled.plan_id, "p1");
        assert_eq!(compiled.tier, CompilationTier::TraceJit);
        assert_eq!(compiled.width, 3);
        assert_eq!(compiled.guards.len(), 3);
    }

    #[test]
    fn compiled_trace_cost_lower_than_fused() {
        let plan = make_plan("p1", &["a", "b", "c"], 10);
        let compiled = CompiledTrace::from_plan(&plan);

        assert!(
            compiled.estimated_cost_jit < compiled.estimated_cost_fused,
            "JIT cost ({}) should be less than fused cost ({})",
            compiled.estimated_cost_jit,
            compiled.estimated_cost_fused
        );
        assert!(compiled.tier_improvement_delta > 0);
    }

    #[test]
    fn compiled_trace_guards_pass_on_matching_trace() {
        let plan = make_plan("p1", &["a", "b"], 10);
        let compiled = CompiledTrace::from_plan(&plan);
        let ctx = default_ctx();

        assert!(compiled.guards_pass(&trace(&["a", "b", "c"]), &ctx));
    }

    #[test]
    fn compiled_trace_guards_fail_on_wrong_prefix() {
        let plan = make_plan("p1", &["a", "b"], 10);
        let compiled = CompiledTrace::from_plan(&plan);
        let ctx = default_ctx();

        assert!(!compiled.guards_pass(&trace(&["x", "y"]), &ctx));
    }

    #[test]
    fn compiled_trace_guards_fail_on_safety_veto() {
        let plan = make_plan("p1", &["a", "b"], 10);
        let compiled = CompiledTrace::from_plan(&plan);
        let ctx = GuardContext {
            safety_envelope_vetoing: true,
            ..default_ctx()
        };

        assert!(!compiled.guards_pass(&trace(&["a", "b"]), &ctx));
    }

    // ── JIT dispatch tests ───────────────────────────────────────

    #[test]
    fn jit_dispatch_hits_after_promotion() {
        let config = TraceJitConfig::new(true, 2, 64, 4);
        let mut jit = TraceJitCompiler::new(config);
        let plan = make_plan("p1", &["a", "b"], 10);

        // Warm up to promotion.
        jit.record_plan_execution(&plan);
        jit.record_plan_execution(&plan);
        assert_eq!(jit.cache_size(), 1);

        // JIT dispatch should hit.
        let result = jit.try_jit_dispatch("p1", &trace(&["a", "b", "c"]), &default_ctx());
        assert!(result.jit_hit);
        assert!(result.deopt_reason.is_none());
        assert!(result.cost_delta > 0);
        assert_eq!(jit.telemetry().jit_hits, 1);
    }

    #[test]
    fn jit_dispatch_returns_not_compiled_before_promotion() {
        let config = TraceJitConfig::new(true, 10, 64, 4);
        let mut jit = TraceJitCompiler::new(config);

        let result = jit.try_jit_dispatch("p1", &trace(&["a", "b"]), &default_ctx());
        assert!(!result.jit_hit);
        assert_eq!(result.deopt_reason, Some(DeoptReason::NotCompiled));
    }

    #[test]
    fn jit_dispatch_deopt_on_guard_failure() {
        let config = TraceJitConfig::new(true, 1, 64, 4);
        let mut jit = TraceJitCompiler::new(config);
        let plan = make_plan("p1", &["a", "b"], 10);
        jit.record_plan_execution(&plan);

        // Wrong trace prefix → guard failure.
        let result = jit.try_jit_dispatch("p1", &trace(&["x", "y"]), &default_ctx());
        assert!(!result.jit_hit);
        assert!(matches!(
            result.deopt_reason,
            Some(DeoptReason::GuardFailure { guard_index: 0, .. })
        ));
        assert_eq!(jit.telemetry().deopts, 1);
    }

    #[test]
    fn jit_dispatch_deopt_on_safety_veto() {
        let config = TraceJitConfig::new(true, 1, 64, 4);
        let mut jit = TraceJitCompiler::new(config);
        let plan = make_plan("p1", &["a", "b"], 10);
        jit.record_plan_execution(&plan);

        let ctx = GuardContext {
            safety_envelope_vetoing: true,
            ..default_ctx()
        };
        let result = jit.try_jit_dispatch("p1", &trace(&["a", "b"]), &ctx);
        assert!(!result.jit_hit);
        assert!(matches!(
            result.deopt_reason,
            Some(DeoptReason::GuardFailure { guard_index: 1, .. })
        ));
    }

    #[test]
    fn jit_dispatch_deopt_on_support_count_guard() {
        let config = TraceJitConfig::new(true, 1, 64, 4);
        let mut jit = TraceJitCompiler::new(config);
        let plan = make_plan("p1", &["a", "b"], 20);
        jit.record_plan_execution(&plan);

        let ctx = GuardContext {
            safety_envelope_vetoing: false,
            current_support_count: 9, // plan guard requires at least support_count / 2 = 10
        };
        let result = jit.try_jit_dispatch("p1", &trace(&["a", "b"]), &ctx);
        assert!(!result.jit_hit);
        assert_eq!(
            result.deopt_reason,
            Some(DeoptReason::GuardFailure {
                guard_index: 2,
                description: "support_count_below_threshold".to_string(),
            })
        );
    }

    #[test]
    fn jit_dispatch_disabled_returns_jit_disabled() {
        let config = TraceJitConfig::new(false, 1, 64, 4);
        let mut jit = TraceJitCompiler::new(config);

        let result = jit.try_jit_dispatch("p1", &trace(&["a"]), &default_ctx());
        assert!(!result.jit_hit);
        assert_eq!(result.deopt_reason, Some(DeoptReason::JitDisabled));
    }

    // ── Invalidation tests ───────────────────────────────────────

    #[test]
    fn trace_invalidated_after_max_guard_failures() {
        let config = TraceJitConfig::new(true, 1, 64, 3);
        let mut jit = TraceJitCompiler::new(config);
        let plan = make_plan("p1", &["a", "b"], 10);
        jit.record_plan_execution(&plan);
        assert_eq!(jit.cache_size(), 1);

        // 3 consecutive guard failures → invalidation.
        for _ in 0..3 {
            jit.try_jit_dispatch("p1", &trace(&["x"]), &default_ctx());
        }

        assert!(jit.is_invalidated("p1"));
        assert_eq!(jit.cache_size(), 0);
        assert_eq!(jit.telemetry().invalidations, 1);

        // Further executions don't re-promote.
        assert!(!jit.record_plan_execution(&plan));
    }

    #[test]
    fn threshold_crossing_failure_reports_trace_invalidated() {
        let config = TraceJitConfig::new(true, 1, 64, 2);
        let mut jit = TraceJitCompiler::new(config);
        let plan = make_plan("p1", &["a", "b"], 10);
        jit.record_plan_execution(&plan);

        let first = jit.try_jit_dispatch("p1", &trace(&["x"]), &default_ctx());
        assert_eq!(
            first.deopt_reason,
            Some(DeoptReason::GuardFailure {
                guard_index: 0,
                description: "opcode_prefix_mismatch".to_string(),
            })
        );

        let second = jit.try_jit_dispatch("p1", &trace(&["x"]), &default_ctx());
        assert_eq!(
            second.deopt_reason,
            Some(DeoptReason::TraceInvalidated { total_failures: 2 })
        );

        assert!(jit.is_invalidated("p1"));
        assert_eq!(jit.cache_size(), 0);

        let after_invalidation = jit.try_jit_dispatch("p1", &trace(&["x"]), &default_ctx());
        assert_eq!(
            after_invalidation.deopt_reason,
            Some(DeoptReason::NotCompiled)
        );

        let telemetry = jit.telemetry();
        assert_eq!(telemetry.deopts, 2);
        assert_eq!(telemetry.jit_misses, 2);
        assert_eq!(telemetry.invalidations, 1);
    }

    #[test]
    fn guard_failure_counter_resets_on_success() {
        let config = TraceJitConfig::new(true, 1, 64, 3);
        let mut jit = TraceJitCompiler::new(config);
        let plan = make_plan("p1", &["a", "b"], 10);
        jit.record_plan_execution(&plan);

        // 2 failures, then a success, then 2 more failures.
        jit.try_jit_dispatch("p1", &trace(&["x"]), &default_ctx());
        jit.try_jit_dispatch("p1", &trace(&["x"]), &default_ctx());
        let result = jit.try_jit_dispatch("p1", &trace(&["a", "b"]), &default_ctx());
        assert!(result.jit_hit); // Success resets counter.
        jit.try_jit_dispatch("p1", &trace(&["x"]), &default_ctx());
        jit.try_jit_dispatch("p1", &trace(&["x"]), &default_ctx());

        // Still in cache (only 2 consecutive, not 3).
        assert!(!jit.is_invalidated("p1"));
        assert_eq!(jit.cache_size(), 1);
    }

    // ── LRU eviction tests ───────────────────────────────────────

    #[test]
    fn lru_eviction_when_cache_full() {
        let config = TraceJitConfig::new(true, 1, 2, 4);
        let mut jit = TraceJitCompiler::new(config);

        let p1 = make_plan("p1", &["a", "b"], 10);
        let p2 = make_plan("p2", &["c", "d"], 10);
        let p3 = make_plan("p3", &["e", "f"], 10);

        jit.record_plan_execution(&p1); // p1 compiled
        jit.record_plan_execution(&p2); // p2 compiled (cache full)
        assert_eq!(jit.cache_size(), 2);

        // Access p2 to make it more recent.
        jit.try_jit_dispatch("p2", &trace(&["c", "d"]), &default_ctx());

        // p3 should evict p1 (LRU).
        jit.record_plan_execution(&p3);
        assert_eq!(jit.cache_size(), 2);
        assert!(jit.get_compiled_trace("p1").is_none());
        assert!(jit.get_compiled_trace("p2").is_some());
        assert!(jit.get_compiled_trace("p3").is_some());
        assert_eq!(jit.telemetry().evictions, 1);
    }

    // ── Telemetry tests ──────────────────────────────────────────

    #[test]
    fn telemetry_tracks_all_counters() {
        let config = TraceJitConfig::new(true, 2, 64, 4);
        let mut jit = TraceJitCompiler::new(config);
        let plan = make_plan("p1", &["a", "b"], 10);

        // 2 evaluations to promote.
        jit.record_plan_execution(&plan);
        jit.record_plan_execution(&plan);

        // 1 hit.
        jit.try_jit_dispatch("p1", &trace(&["a", "b"]), &default_ctx());
        // 1 miss.
        jit.try_jit_dispatch("p1", &trace(&["x"]), &default_ctx());

        let t = jit.telemetry();
        assert_eq!(t.plans_evaluated, 2);
        assert_eq!(t.traces_compiled, 1);
        assert_eq!(t.jit_hits, 1);
        assert_eq!(t.jit_misses, 1);
        assert_eq!(t.deopts, 1);
        assert_eq!(t.cache_size, 1);
    }

    #[test]
    fn telemetry_serializes_round_trip() {
        let telemetry = TraceJitTelemetry {
            plans_evaluated: 100,
            traces_compiled: 10,
            jit_hits: 50,
            jit_misses: 5,
            deopts: 5,
            invalidations: 1,
            evictions: 2,
            cache_size: 8,
        };

        let json = serde_json::to_string(&telemetry).expect("serialize");
        let parsed: TraceJitTelemetry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(telemetry, parsed);
    }

    // ── Reset tests ──────────────────────────────────────────────

    #[test]
    fn reset_clears_all_state() {
        let config = TraceJitConfig::new(true, 1, 64, 4);
        let mut jit = TraceJitCompiler::new(config);
        let plan = make_plan("p1", &["a", "b"], 10);

        jit.record_plan_execution(&plan);
        jit.try_jit_dispatch("p1", &trace(&["a", "b"]), &default_ctx());
        assert!(jit.cache_size() > 0);
        assert!(jit.telemetry().jit_hits > 0);

        jit.reset();
        assert_eq!(jit.cache_size(), 0);
        assert_eq!(jit.telemetry().jit_hits, 0);
        assert_eq!(jit.telemetry().traces_compiled, 0);
    }

    // ── Cost estimation tests ────────────────────────────────────

    #[test]
    fn jit_cost_less_than_fused_cost() {
        for width in 2..=8 {
            let jit_cost = estimated_jit_cost(width);
            let fused_cost = 6 + i64::try_from(width).unwrap() * 2;
            assert!(
                jit_cost < fused_cost,
                "JIT cost ({jit_cost}) should be less than fused cost ({fused_cost}) for width {width}"
            );
        }
    }

    #[test]
    fn jit_cost_scales_linearly() {
        let cost_2 = estimated_jit_cost(2);
        let cost_4 = estimated_jit_cost(4);
        let delta = cost_4 - cost_2;
        // 2 extra opcodes × 1 unit each = 2 units.
        assert_eq!(delta, 2);
    }

    // ── Compiled trace serialization test ─────────────────────────

    #[test]
    fn compiled_trace_serializes_round_trip() {
        let plan = make_plan("p_rt", &["a", "b", "c"], 10);
        let compiled = CompiledTrace::from_plan(&plan);

        let json = serde_json::to_string(&compiled).expect("serialize");
        let parsed: CompiledTrace = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(compiled, parsed);
    }

    // ── DeoptReason serialization test ────────────────────────────

    #[test]
    fn deopt_reason_serializes_round_trip() {
        let reasons = vec![
            DeoptReason::GuardFailure {
                guard_index: 1,
                description: "test".to_string(),
            },
            DeoptReason::TraceInvalidated { total_failures: 5 },
            DeoptReason::JitDisabled,
            DeoptReason::NotCompiled,
            DeoptReason::SafetyVeto,
        ];

        for reason in &reasons {
            let value = serde_json::to_value(reason).expect("serialize to value");
            let parsed: DeoptReason =
                serde_json::from_value(value).expect("deserialize from value");
            assert_eq!(*reason, parsed);
        }
    }

    // ── Multi-plan tests ─────────────────────────────────────────

    #[test]
    fn multiple_plans_compile_independently() {
        let config = TraceJitConfig::new(true, 2, 64, 4);
        let mut jit = TraceJitCompiler::new(config);

        let p1 = make_plan("p1", &["a", "b"], 10);
        let p2 = make_plan("p2", &["c", "d"], 10);

        // Promote both.
        jit.record_plan_execution(&p1);
        jit.record_plan_execution(&p1);
        jit.record_plan_execution(&p2);
        jit.record_plan_execution(&p2);
        assert_eq!(jit.cache_size(), 2);

        // Both should dispatch independently.
        let r1 = jit.try_jit_dispatch("p1", &trace(&["a", "b"]), &default_ctx());
        let r2 = jit.try_jit_dispatch("p2", &trace(&["c", "d"]), &default_ctx());
        assert!(r1.jit_hit);
        assert!(r2.jit_hit);
        assert_eq!(jit.telemetry().jit_hits, 2);
    }

    // ── Property tests ──

    mod proptest_trace_jit {
        use super::*;

        use proptest::prelude::*;

        fn arb_opcode() -> impl Strategy<Value = String> {
            prop::sample::select(vec![
                "session.get_state".to_string(),
                "session.get_messages".to_string(),
                "events.list".to_string(),
                "tool.read".to_string(),
                "tool.write".to_string(),
                "events.emit".to_string(),
            ])
        }

        fn arb_window() -> impl Strategy<Value = Vec<String>> {
            prop::collection::vec(arb_opcode(), 2..6)
        }

        fn arb_plan() -> impl Strategy<Value = HostcallSuperinstructionPlan> {
            (arb_window(), 2..100u32).prop_map(|(window, support)| {
                let width = window.len();
                let baseline = i64::try_from(width).unwrap_or(0) * 10;
                let fused = 6 + i64::try_from(width).unwrap_or(0) * 2;
                HostcallSuperinstructionPlan {
                    schema: HOSTCALL_SUPERINSTRUCTION_SCHEMA_VERSION.to_string(),
                    version: HOSTCALL_SUPERINSTRUCTION_PLAN_VERSION,
                    plan_id: format!("arb_{width}_{support}"),
                    trace_signature: format!("sig_arb_{width}_{support}"),
                    opcode_window: window,
                    support_count: support,
                    estimated_cost_baseline: baseline,
                    estimated_cost_fused: fused,
                    expected_cost_delta: baseline - fused,
                }
            })
        }

        fn arb_guard_context() -> impl Strategy<Value = GuardContext> {
            (any::<bool>(), 0..200u32).prop_map(|(vetoing, support)| GuardContext {
                safety_envelope_vetoing: vetoing,
                current_support_count: support,
            })
        }

        fn arb_config() -> impl Strategy<Value = TraceJitConfig> {
            (1..16u64, 2..32usize, 1..8u64).prop_map(|(min_exec, max_traces, max_failures)| {
                TraceJitConfig::new(true, min_exec, max_traces, max_failures)
            })
        }

        proptest! {
            #[test]
            fn jit_cost_less_than_fused_for_width_ge_2(width in 2..1000usize) {
                let jit_cost = estimated_jit_cost(width);
                let fused_cost = 6 + i64::try_from(width).unwrap() * 2;
                assert!(
                    jit_cost < fused_cost,
                    "JIT cost ({jit_cost}) must be < fused cost ({fused_cost}) at width {width}"
                );
            }

            #[test]
            fn compiled_trace_tier_improvement_nonnegative(plan in arb_plan()) {
                let compiled = CompiledTrace::from_plan(&plan);
                assert!(
                    compiled.tier_improvement_delta >= 0,
                    "tier_improvement_delta must be non-negative, got {}",
                    compiled.tier_improvement_delta,
                );
            }

            #[test]
            fn compiled_trace_always_has_three_guards(plan in arb_plan()) {
                let compiled = CompiledTrace::from_plan(&plan);
                assert!(
                    compiled.guards.len() == 3,
                    "compiled trace must have 3 guards (OpcodePrefix, SafetyEnvelope, MinSupport)"
                );
            }

            #[test]
            fn compiled_trace_width_matches_plan(plan in arb_plan()) {
                let compiled = CompiledTrace::from_plan(&plan);
                assert!(
                    compiled.width == plan.width(),
                    "compiled width {} != plan width {}",
                    compiled.width,
                    plan.width(),
                );
            }

            #[test]
            fn disabled_jit_never_promotes(
                plan in arb_plan(),
                executions in 1..50usize,
            ) {
                let config = TraceJitConfig::new(false, 1, 64, 4);
                let mut jit = TraceJitCompiler::new(config);
                for _ in 0..executions {
                    let promoted = jit.record_plan_execution(&plan);
                    assert!(!promoted, "disabled JIT must never promote");
                }
                assert!(
                    jit.cache_size() == 0,
                    "disabled JIT must have empty cache"
                );
            }

            #[test]
            fn cache_size_never_exceeds_max(
                config in arb_config(),
                plans in prop::collection::vec(arb_plan(), 1..20),
            ) {
                let max = config.max_compiled_traces;
                let min_exec = config.min_jit_executions;
                let mut jit = TraceJitCompiler::new(config);
                for plan in &plans {
                    for _ in 0..min_exec {
                        jit.record_plan_execution(plan);
                    }
                }
                assert!(
                    jit.cache_size() <= max,
                    "cache size {} exceeds max {}",
                    jit.cache_size(),
                    max,
                );
            }

            #[test]
            fn telemetry_traces_compiled_matches_cache_plus_evictions(
                config in arb_config(),
                plans in prop::collection::vec(arb_plan(), 1..10),
            ) {
                let min_exec = config.min_jit_executions;
                let mut jit = TraceJitCompiler::new(config);
                for plan in &plans {
                    for _ in 0..min_exec {
                        jit.record_plan_execution(plan);
                    }
                }
                let t = jit.telemetry();
                // compiled = currently cached + evicted + invalidated
                assert!(
                    t.traces_compiled >= t.cache_size,
                    "traces_compiled ({}) must be >= cache_size ({})",
                    t.traces_compiled,
                    t.cache_size,
                );
            }

            #[test]
            fn guard_check_is_deterministic(
                plan in arb_plan(),
                trace_opcodes in arb_window(),
                ctx in arb_guard_context(),
            ) {
                let compiled = CompiledTrace::from_plan(&plan);
                let r1 = compiled.guards_pass(&trace_opcodes, &ctx);
                let r2 = compiled.guards_pass(&trace_opcodes, &ctx);
                assert!(r1 == r2, "guard check must be deterministic");
            }

            #[test]
            fn jit_hit_implies_zero_deopt_reason(
                config in arb_config(),
                plan in arb_plan(),
            ) {
                let min_exec = config.min_jit_executions;
                let mut jit = TraceJitCompiler::new(config);
                // Promote the plan
                for _ in 0..min_exec {
                    jit.record_plan_execution(&plan);
                }
                // Dispatch with matching trace and benign context
                let ctx = GuardContext {
                    safety_envelope_vetoing: false,
                    current_support_count: plan.support_count,
                };
                let result = jit.try_jit_dispatch(&plan.plan_id, &plan.opcode_window, &ctx);
                if result.jit_hit {
                    assert!(
                        result.deopt_reason.is_none(),
                        "JIT hit must have no deopt reason"
                    );
                    assert!(
                        result.cost_delta >= 0,
                        "JIT hit must have non-negative cost delta"
                    );
                }
            }

            #[test]
            fn deopts_stop_growing_after_invalidation(
                max_guard_failures in 1..8u64,
                attempts in 1..40u64,
            ) {
                let config = TraceJitConfig::new(true, 1, 8, max_guard_failures);
                let mut jit = TraceJitCompiler::new(config);
                let plan = make_plan("prop_invalidation", &["a", "b"], 10);
                jit.record_plan_execution(&plan);

                for _ in 0..attempts {
                    let _ = jit.try_jit_dispatch("prop_invalidation", &trace(&["x"]), &default_ctx());
                }

                let telemetry = jit.telemetry();
                let expected_deopts = attempts.min(max_guard_failures);
                prop_assert_eq!(telemetry.deopts, expected_deopts);
                prop_assert_eq!(telemetry.jit_misses, expected_deopts);
                prop_assert!(telemetry.invalidations <= 1);

                if attempts >= max_guard_failures {
                    prop_assert!(jit.is_invalidated("prop_invalidation"));
                    prop_assert_eq!(telemetry.invalidations, 1);
                } else {
                    prop_assert!(!jit.is_invalidated("prop_invalidation"));
                    prop_assert_eq!(telemetry.invalidations, 0);
                }
            }
        }
    }

    #[test]
    fn try_jit_dispatch_str_matches_owned_dispatch_and_handles_invalidation() {
        let config = TraceJitConfig::new(true, 2, 8, 2);
        let mut owned_jit = TraceJitCompiler::new(config.clone());
        let mut str_jit = TraceJitCompiler::new(config);
        let plan = make_plan("str_parity", &["tool.read", "tool.write"], 6);

        // Before promotion threshold -> not compiled.
        assert!(!owned_jit.record_plan_execution(&plan));
        assert!(!str_jit.record_plan_execution(&plan));
        let good_ctx = GuardContext {
            safety_envelope_vetoing: false,
            current_support_count: plan.support_count,
        };
        let owned_pre = owned_jit.try_jit_dispatch(&plan.plan_id, &plan.opcode_window, &good_ctx);
        let str_pre =
            str_jit.try_jit_dispatch_str(&plan.plan_id, &["tool.read", "tool.write"], &good_ctx);
        assert_eq!((owned_pre.jit_hit, owned_pre.cost_delta), str_pre);
        assert!(!str_pre.0);

        // Second execution promotes to JIT -> dispatch succeeds with positive delta.
        assert!(owned_jit.record_plan_execution(&plan));
        assert!(str_jit.record_plan_execution(&plan));
        let owned_hit = owned_jit.try_jit_dispatch(&plan.plan_id, &plan.opcode_window, &good_ctx);
        let str_hit =
            str_jit.try_jit_dispatch_str(&plan.plan_id, &["tool.read", "tool.write"], &good_ctx);
        assert_eq!((owned_hit.jit_hit, owned_hit.cost_delta), str_hit);
        assert!(str_hit.0);
        assert!(str_hit.1 > 0);

        // Guard failure via low support count (the default-ctx bug class) deoptimizes & invalidates identically.
        let zero_ctx = GuardContext::default();
        for _ in 0..2 {
            let owned_miss =
                owned_jit.try_jit_dispatch(&plan.plan_id, &plan.opcode_window, &zero_ctx);
            let str_miss = str_jit.try_jit_dispatch_str(
                &plan.plan_id,
                &["tool.read", "tool.write"],
                &zero_ctx,
            );
            assert_eq!((owned_miss.jit_hit, owned_miss.cost_delta), str_miss);
            assert!(!str_miss.0);
        }
        assert!(str_jit.is_invalidated(&plan.plan_id));
        assert_eq!(str_jit.telemetry(), owned_jit.telemetry());
    }
}
