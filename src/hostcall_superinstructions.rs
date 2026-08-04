//! Superinstruction compiler for hot typed-hostcall opcode traces.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Versioned schema for serialized superinstruction plans.
pub const HOSTCALL_SUPERINSTRUCTION_SCHEMA_VERSION: &str = "pi.ext.hostcall_superinstruction.v1";
/// Plan payload version.
pub const HOSTCALL_SUPERINSTRUCTION_PLAN_VERSION: u16 = 1;

const DEFAULT_MIN_SUPPORT: u32 = 3;
const DEFAULT_MAX_WINDOW: usize = 4;
const BASE_OPCODE_COST_UNITS: i64 = 10;
const FUSED_OPCODE_FIXED_COST_UNITS: i64 = 6;
const FUSED_OPCODE_STEP_COST_UNITS: i64 = 2;

/// A fused superinstruction plan derived from repeated hostcall opcode windows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostcallSuperinstructionPlan {
    pub schema: String,
    pub version: u16,
    pub plan_id: String,
    pub trace_signature: String,
    pub opcode_window: Vec<String>,
    pub support_count: u32,
    pub estimated_cost_baseline: i64,
    pub estimated_cost_fused: i64,
    pub expected_cost_delta: i64,
}

impl HostcallSuperinstructionPlan {
    #[must_use]
    pub const fn width(&self) -> usize {
        self.opcode_window.len()
    }

    #[must_use]
    pub fn matches_trace_prefix(&self, trace: &[String]) -> bool {
        trace.len() >= self.opcode_window.len()
            && trace
                .iter()
                .zip(self.opcode_window.iter())
                .all(|(left, right)| left == right)
    }
}

/// Deterministic compiler for recurring opcode motifs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostcallSuperinstructionCompiler {
    enabled: bool,
    min_support: u32,
    max_window: usize,
}

impl Default for HostcallSuperinstructionCompiler {
    fn default() -> Self {
        Self::from_env()
    }
}

impl HostcallSuperinstructionCompiler {
    #[must_use]
    pub const fn new(enabled: bool, min_support: u32, max_window: usize) -> Self {
        Self {
            enabled,
            min_support,
            max_window,
        }
    }

    #[must_use]
    pub fn from_env() -> Self {
        let enabled = bool_from_env("PI_HOSTCALL_SUPERINSTRUCTIONS", true);
        let min_support = std::env::var("PI_HOSTCALL_SUPERINSTRUCTION_MIN_SUPPORT")
            .ok()
            .and_then(|raw| raw.trim().parse::<u32>().ok())
            .map_or(DEFAULT_MIN_SUPPORT, |value| value.max(2));
        let max_window = std::env::var("PI_HOSTCALL_SUPERINSTRUCTION_MAX_WINDOW")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .map_or(DEFAULT_MAX_WINDOW, |value| value.max(2));
        Self::new(enabled, min_support, max_window)
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn min_support(&self) -> u32 {
        self.min_support
    }

    #[must_use]
    pub const fn max_window(&self) -> usize {
        self.max_window
    }

    /// Compile frequent opcode windows into deterministic fused plans.
    #[must_use]
    pub fn compile_plans(&self, traces: &[Vec<String>]) -> Vec<HostcallSuperinstructionPlan> {
        if !self.enabled {
            return Vec::new();
        }
        let mut windows: BTreeMap<Vec<String>, u32> = BTreeMap::new();
        for trace in traces {
            let trace_len = trace.len();
            if trace_len < 2 {
                continue;
            }
            let max_width = self.max_window.min(trace_len);
            for width in 2..=max_width {
                let mut next_valid: HashMap<&[String], usize> = HashMap::new();
                for start in 0..=trace_len - width {
                    let window_slice = &trace[start..start + width];
                    if window_slice.iter().any(|opcode| opcode.trim().is_empty()) {
                        continue;
                    }

                    let min_start = *next_valid.get(window_slice).unwrap_or(&0);
                    if start >= min_start {
                        let entry = windows.entry(window_slice.to_vec()).or_insert(0);
                        *entry = entry.saturating_add(1);
                        next_valid.insert(window_slice, start + width);
                    }
                }
            }
        }

        let mut plans = windows
            .into_iter()
            .filter_map(|(opcode_window, support_count)| {
                if support_count < self.min_support {
                    return None;
                }
                let estimated_cost_baseline = estimated_baseline_cost(opcode_window.len());
                let estimated_cost_fused = estimated_fused_cost(opcode_window.len());
                let expected_cost_delta =
                    estimated_cost_baseline.saturating_sub(estimated_cost_fused);
                if expected_cost_delta <= 0 {
                    return None;
                }

                let trace_signature = opcode_window_signature(&opcode_window);
                let plan_id = format!("fuse_{trace_signature}");
                Some(HostcallSuperinstructionPlan {
                    schema: HOSTCALL_SUPERINSTRUCTION_SCHEMA_VERSION.to_string(),
                    version: HOSTCALL_SUPERINSTRUCTION_PLAN_VERSION,
                    plan_id,
                    trace_signature,
                    opcode_window,
                    support_count,
                    estimated_cost_baseline,
                    estimated_cost_fused,
                    expected_cost_delta,
                })
            })
            .collect::<Vec<_>>();

        plans.sort_by(|left, right| {
            right
                .expected_cost_delta
                .cmp(&left.expected_cost_delta)
                .then_with(|| right.support_count.cmp(&left.support_count))
                .then_with(|| right.width().cmp(&left.width()))
                .then_with(|| left.opcode_window.cmp(&right.opcode_window))
                .then_with(|| left.plan_id.cmp(&right.plan_id))
        });
        plans
    }
}

/// Plan lookup/deoptimization result for a concrete trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostcallSuperinstructionSelection {
    pub trace_signature: String,
    pub selected_plan_id: Option<String>,
    pub selected_window: Option<Vec<String>>,
    pub expected_cost_delta: i64,
    pub deopt_reason: Option<&'static str>,
}

impl HostcallSuperinstructionSelection {
    #[must_use]
    pub const fn hit(&self) -> bool {
        self.selected_plan_id.is_some()
    }
}

/// Canonical + fused execution representation with safe fallback details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostcallSuperinstructionExecution {
    pub canonical_trace: Vec<String>,
    pub fused_trace: Vec<String>,
    pub selection: HostcallSuperinstructionSelection,
}

/// Select the best matching superinstruction plan for a trace prefix.
#[must_use]
pub fn select_plan_for_trace(
    trace: &[String],
    plans: &[HostcallSuperinstructionPlan],
) -> HostcallSuperinstructionSelection {
    let trace_signature = opcode_window_signature(trace);
    if trace.is_empty() {
        return HostcallSuperinstructionSelection {
            trace_signature,
            selected_plan_id: None,
            selected_window: None,
            expected_cost_delta: 0,
            deopt_reason: Some("empty_trace"),
        };
    }

    let mut matching = plans
        .iter()
        .filter(|plan| plan.matches_trace_prefix(trace))
        .collect::<Vec<_>>();
    if matching.is_empty() {
        return HostcallSuperinstructionSelection {
            trace_signature,
            selected_plan_id: None,
            selected_window: None,
            expected_cost_delta: 0,
            deopt_reason: Some("no_matching_plan"),
        };
    }

    matching.sort_by(|left, right| {
        right
            .expected_cost_delta
            .cmp(&left.expected_cost_delta)
            .then_with(|| right.support_count.cmp(&left.support_count))
            .then_with(|| right.width().cmp(&left.width()))
            .then_with(|| left.plan_id.cmp(&right.plan_id))
    });

    let best = matching[0];
    if matching.iter().skip(1).any(|candidate| {
        candidate.expected_cost_delta == best.expected_cost_delta
            && candidate.support_count == best.support_count
            && candidate.width() == best.width()
    }) {
        return HostcallSuperinstructionSelection {
            trace_signature,
            selected_plan_id: None,
            selected_window: None,
            expected_cost_delta: 0,
            deopt_reason: Some("ambiguous_top_plan"),
        };
    }

    HostcallSuperinstructionSelection {
        trace_signature,
        selected_plan_id: Some(best.plan_id.clone()),
        selected_window: Some(best.opcode_window.clone()),
        expected_cost_delta: best.expected_cost_delta,
        deopt_reason: None,
    }
}

/// Execute a trace under superinstruction selection with immediate safe fallback.
///
/// Semantic output always remains canonical opcode ordering.
#[must_use]
pub fn execute_with_superinstruction(
    trace: &[String],
    plans: &[HostcallSuperinstructionPlan],
) -> HostcallSuperinstructionExecution {
    let canonical_trace = trace.to_vec();
    let selection = select_plan_for_trace(trace, plans);
    if !selection.hit() {
        return HostcallSuperinstructionExecution {
            canonical_trace: canonical_trace.clone(),
            fused_trace: canonical_trace,
            selection,
        };
    }

    let mut fused_trace = Vec::new();
    if let Some(plan_id) = selection.selected_plan_id.as_ref() {
        fused_trace.push(format!("@{plan_id}"));
    }
    let consumed = selection
        .selected_window
        .as_ref()
        .map_or(0, std::vec::Vec::len)
        .min(trace.len());
    fused_trace.extend_from_slice(&trace[consumed..]);

    HostcallSuperinstructionExecution {
        canonical_trace,
        fused_trace,
        selection,
    }
}

fn estimated_baseline_cost(width: usize) -> i64 {
    let width_units = i64::try_from(width).unwrap_or(i64::MAX);
    width_units.saturating_mul(BASE_OPCODE_COST_UNITS)
}

fn estimated_fused_cost(width: usize) -> i64 {
    let width_units = i64::try_from(width).unwrap_or(i64::MAX);
    FUSED_OPCODE_FIXED_COST_UNITS
        .saturating_add(width_units.saturating_mul(FUSED_OPCODE_STEP_COST_UNITS))
}

fn opcode_window_signature(window: &[String]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for opcode in window {
        for byte in opcode.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3_u64);
        }
        hash ^= u64::from(b'|');
        hash = hash.wrapping_mul(0x0100_0000_01b3_u64);
    }
    format!("{hash:016x}")
}

fn bool_from_env(var: &str, default: bool) -> bool {
    std::env::var(var).ok().as_deref().map_or(default, |value| {
        match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "on" | "enabled" | "yes" => true,
            "0" | "false" | "off" | "disabled" | "no" => false,
            _ => default,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opcode_trace(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    fn plan(
        plan_id: &str,
        window: &[&str],
        support_count: u32,
        expected_cost_delta: i64,
    ) -> HostcallSuperinstructionPlan {
        HostcallSuperinstructionPlan {
            schema: HOSTCALL_SUPERINSTRUCTION_SCHEMA_VERSION.to_string(),
            version: HOSTCALL_SUPERINSTRUCTION_PLAN_VERSION,
            plan_id: plan_id.to_string(),
            trace_signature: opcode_window_signature(&opcode_trace(window)),
            opcode_window: opcode_trace(window),
            support_count,
            estimated_cost_baseline: 0,
            estimated_cost_fused: 0,
            expected_cost_delta,
        }
    }

    #[test]
    fn compiler_extracts_hot_windows_deterministically() {
        let compiler = HostcallSuperinstructionCompiler::new(true, 2, 4);
        let traces = vec![
            opcode_trace(&[
                "session.get_state",
                "session.get_messages",
                "session.get_entries",
                "events.list_flags",
            ]),
            opcode_trace(&[
                "session.get_state",
                "session.get_messages",
                "session.get_entries",
                "events.emit",
            ]),
            opcode_trace(&[
                "session.get_state",
                "session.get_messages",
                "session.get_entries",
                "events.get_model",
            ]),
        ];

        let plans = compiler.compile_plans(&traces);
        assert!(!plans.is_empty());
        assert!(plans.iter().any(|entry| {
            entry.opcode_window
                == opcode_trace(&[
                    "session.get_state",
                    "session.get_messages",
                    "session.get_entries",
                ])
        }));

        let reversed = traces.iter().rev().cloned().collect::<Vec<_>>();
        let plans_reversed = compiler.compile_plans(&reversed);
        assert_eq!(plans, plans_reversed);
    }

    #[test]
    fn selection_prefers_higher_delta_then_support_then_width() {
        let trace = opcode_trace(&["tool.read", "events.list", "events.get_model"]);
        let plans = vec![
            plan("p_low_delta", &["tool.read", "events.list"], 8, 10),
            plan(
                "p_best",
                &["tool.read", "events.list", "events.get_model"],
                7,
                14,
            ),
            plan(
                "p_low_support",
                &["tool.read", "events.list", "events.get_model"],
                4,
                14,
            ),
        ];

        let selected = select_plan_for_trace(&trace, &plans);
        assert_eq!(selected.selected_plan_id.as_deref(), Some("p_best"));
        assert!(selected.deopt_reason.is_none());
        assert!(selected.hit());
    }

    #[test]
    fn selection_deopts_on_ambiguous_top_plan() {
        let trace = opcode_trace(&["session.get_state", "session.get_entries"]);
        let plans = vec![
            plan("p_a", &["session.get_state", "session.get_entries"], 5, 11),
            plan("p_b", &["session.get_state", "session.get_entries"], 5, 11),
        ];

        let selected = select_plan_for_trace(&trace, &plans);
        assert!(!selected.hit());
        assert_eq!(selected.deopt_reason, Some("ambiguous_top_plan"));
    }

    #[test]
    fn execution_preserves_canonical_semantics_with_fused_projection() {
        let trace = opcode_trace(&[
            "session.get_state",
            "session.get_messages",
            "session.get_entries",
            "events.list",
        ]);
        let plans = vec![plan(
            "p_fuse",
            &[
                "session.get_state",
                "session.get_messages",
                "session.get_entries",
            ],
            6,
            18,
        )];

        let execution = execute_with_superinstruction(&trace, &plans);
        assert_eq!(execution.canonical_trace, trace);
        assert_eq!(execution.fused_trace.len(), 2);
        assert_eq!(execution.fused_trace[0], "@p_fuse");
        assert_eq!(execution.fused_trace[1], "events.list");
        assert!(execution.selection.hit());
    }

    #[test]
    fn execution_deopts_immediately_on_guard_mismatch() {
        let trace = opcode_trace(&["events.get_model", "events.set_model"]);
        let plans = vec![plan("p_tool", &["tool.read", "tool.write"], 9, 12)];

        let execution = execute_with_superinstruction(&trace, &plans);
        assert_eq!(execution.canonical_trace, trace);
        assert_eq!(execution.fused_trace, execution.canonical_trace);
        assert!(!execution.selection.hit());
        assert_eq!(execution.selection.deopt_reason, Some("no_matching_plan"));
    }

    // ── Cost estimation ──

    #[test]
    fn estimated_baseline_cost_is_linear_in_width() {
        assert_eq!(estimated_baseline_cost(1), BASE_OPCODE_COST_UNITS);
        assert_eq!(estimated_baseline_cost(2), 2 * BASE_OPCODE_COST_UNITS);
        assert_eq!(estimated_baseline_cost(4), 4 * BASE_OPCODE_COST_UNITS);
        assert_eq!(estimated_baseline_cost(0), 0);
    }

    #[test]
    fn estimated_fused_cost_is_fixed_plus_step() {
        assert_eq!(
            estimated_fused_cost(2),
            FUSED_OPCODE_FIXED_COST_UNITS + 2 * FUSED_OPCODE_STEP_COST_UNITS
        );
        assert_eq!(
            estimated_fused_cost(4),
            FUSED_OPCODE_FIXED_COST_UNITS + 4 * FUSED_OPCODE_STEP_COST_UNITS
        );
        assert_eq!(estimated_fused_cost(0), FUSED_OPCODE_FIXED_COST_UNITS);
    }

    #[test]
    fn fused_cost_always_less_than_baseline_for_width_ge_2() {
        for width in 2..=32 {
            let baseline = estimated_baseline_cost(width);
            let fused = estimated_fused_cost(width);
            assert!(
                fused < baseline,
                "fused ({fused}) should be less than baseline ({baseline}) at width {width}"
            );
        }
    }

    // ── Compiler disabled ──

    #[test]
    fn compiler_disabled_returns_empty_plans() {
        let compiler = HostcallSuperinstructionCompiler::new(false, 2, 4);
        let traces = vec![
            opcode_trace(&["a", "b", "c"]),
            opcode_trace(&["a", "b", "c"]),
            opcode_trace(&["a", "b", "c"]),
        ];
        let plans = compiler.compile_plans(&traces);
        assert!(plans.is_empty());
    }

    // ── Compiler edge cases ──

    #[test]
    fn compiler_ignores_single_opcode_traces() {
        let compiler = HostcallSuperinstructionCompiler::new(true, 1, 4);
        let traces = vec![
            opcode_trace(&["single"]),
            opcode_trace(&["single"]),
            opcode_trace(&["single"]),
        ];
        let plans = compiler.compile_plans(&traces);
        assert!(plans.is_empty());
    }

    #[test]
    fn compiler_ignores_empty_traces() {
        let compiler = HostcallSuperinstructionCompiler::new(true, 1, 4);
        let plans = compiler.compile_plans(&[Vec::new(), Vec::new()]);
        assert!(plans.is_empty());
    }

    #[test]
    fn compiler_skips_windows_with_empty_opcodes() {
        let compiler = HostcallSuperinstructionCompiler::new(true, 1, 4);
        let traces = vec![opcode_trace(&["a", "", "c"]), opcode_trace(&["a", "", "c"])];
        let plans = compiler.compile_plans(&traces);
        // Windows containing "" are skipped
        assert!(
            plans
                .iter()
                .all(|p| p.opcode_window.iter().all(|op| !op.trim().is_empty())),
            "no plan should contain empty opcodes"
        );
    }

    #[test]
    fn compiler_respects_min_support_threshold() {
        let compiler = HostcallSuperinstructionCompiler::new(true, 5, 4);
        // Only 3 traces — below min_support of 5
        let traces = vec![
            opcode_trace(&["a", "b"]),
            opcode_trace(&["a", "b"]),
            opcode_trace(&["a", "b"]),
        ];
        let plans = compiler.compile_plans(&traces);
        assert!(plans.is_empty(), "support 3 < min_support 5");
    }

    #[test]
    fn compiler_respects_max_window_size() {
        let compiler = HostcallSuperinstructionCompiler::new(true, 2, 2);
        let traces = vec![
            opcode_trace(&["a", "b", "c", "d"]),
            opcode_trace(&["a", "b", "c", "d"]),
            opcode_trace(&["a", "b", "c", "d"]),
        ];
        let plans = compiler.compile_plans(&traces);
        assert!(
            plans.iter().all(|p| p.width() <= 2),
            "max_window=2 should cap window width"
        );
    }

    #[test]
    fn compiler_plans_sorted_by_cost_delta_descending() {
        let compiler = HostcallSuperinstructionCompiler::new(true, 2, 4);
        let traces = vec![
            opcode_trace(&["a", "b", "c", "d"]),
            opcode_trace(&["a", "b", "c", "d"]),
            opcode_trace(&["a", "b", "c", "d"]),
        ];
        let plans = compiler.compile_plans(&traces);
        for pair in plans.windows(2) {
            assert!(
                pair[0].expected_cost_delta >= pair[1].expected_cost_delta,
                "plans should be sorted by cost delta descending"
            );
        }
    }

    // ── Plan schema + serde ──

    #[test]
    fn plan_serde_roundtrip() {
        let compiler = HostcallSuperinstructionCompiler::new(true, 2, 4);
        let traces = vec![
            opcode_trace(&["x", "y", "z"]),
            opcode_trace(&["x", "y", "z"]),
            opcode_trace(&["x", "y", "z"]),
        ];
        let plans = compiler.compile_plans(&traces);
        assert!(!plans.is_empty());
        for p in &plans {
            let json = serde_json::to_string(p).expect("serialize");
            let roundtrip: HostcallSuperinstructionPlan =
                serde_json::from_str(&json).expect("deserialize");
            assert_eq!(*p, roundtrip);
            assert_eq!(p.schema, HOSTCALL_SUPERINSTRUCTION_SCHEMA_VERSION);
            assert_eq!(p.version, HOSTCALL_SUPERINSTRUCTION_PLAN_VERSION);
        }
    }

    // ── Plan.matches_trace_prefix ──

    #[test]
    fn matches_trace_prefix_exact() {
        let p = plan("test", &["a", "b"], 3, 10);
        assert!(p.matches_trace_prefix(&opcode_trace(&["a", "b"])));
        assert!(p.matches_trace_prefix(&opcode_trace(&["a", "b", "c"])));
        assert!(!p.matches_trace_prefix(&opcode_trace(&["a"])));
        assert!(!p.matches_trace_prefix(&opcode_trace(&["b", "a"])));
        assert!(!p.matches_trace_prefix(&[]));
    }

    // ── Selection edge cases ──

    #[test]
    fn selection_on_empty_trace_returns_empty_trace_deopt() {
        let plans = vec![plan("p", &["a", "b"], 3, 10)];
        let selected = select_plan_for_trace(&[], &plans);
        assert!(!selected.hit());
        assert_eq!(selected.deopt_reason, Some("empty_trace"));
    }

    #[test]
    fn selection_on_empty_plans_returns_no_matching_plan() {
        let trace = opcode_trace(&["a", "b"]);
        let selected = select_plan_for_trace(&trace, &[]);
        assert!(!selected.hit());
        assert_eq!(selected.deopt_reason, Some("no_matching_plan"));
    }

    // ── Opcode window signature ──

    #[test]
    fn opcode_window_signature_is_deterministic() {
        let window = opcode_trace(&["session.get_state", "session.get_messages"]);
        let sig1 = opcode_window_signature(&window);
        let sig2 = opcode_window_signature(&window);
        assert_eq!(sig1, sig2);
        assert_eq!(sig1.len(), 16, "signature should be 16 hex chars");
    }

    #[test]
    fn opcode_window_signature_differs_for_different_windows() {
        let sig_ab = opcode_window_signature(&opcode_trace(&["a", "b"]));
        let sig_ba = opcode_window_signature(&opcode_trace(&["b", "a"]));
        let sig_abc = opcode_window_signature(&opcode_trace(&["a", "b", "c"]));
        assert_ne!(sig_ab, sig_ba, "order matters");
        assert_ne!(sig_ab, sig_abc, "different length windows differ");
    }

    // ── Execution with entire trace consumed ──

    #[test]
    fn execution_fuses_entire_trace() {
        let trace = opcode_trace(&["a", "b"]);
        let plans = vec![plan("p_full", &["a", "b"], 5, 14)];
        let execution = execute_with_superinstruction(&trace, &plans);
        assert!(execution.selection.hit());
        assert_eq!(execution.fused_trace, vec!["@p_full"]);
        assert!(execution.fused_trace.len() < execution.canonical_trace.len());
    }

    // ── Compiler constructor + accessors ──

    #[test]
    fn compiler_accessors_match_constructor() {
        let compiler = HostcallSuperinstructionCompiler::new(true, 7, 5);
        assert!(compiler.enabled());
        assert_eq!(compiler.min_support(), 7);
        assert_eq!(compiler.max_window(), 5);

        let disabled = HostcallSuperinstructionCompiler::new(false, 2, 3);
        assert!(!disabled.enabled());
    }

    // ── Property tests ──

    mod proptest_superinstructions {
        use super::*;
        use proptest::prelude::*;

        fn arb_opcode() -> impl Strategy<Value = String> {
            prop::sample::select(vec![
                "session.get_state".to_string(),
                "session.get_messages".to_string(),
                "events.list".to_string(),
                "events.emit".to_string(),
                "tool.read".to_string(),
                "tool.write".to_string(),
                "events.get_model".to_string(),
                "session.set_label".to_string(),
            ])
        }

        fn arb_trace() -> impl Strategy<Value = Vec<String>> {
            prop::collection::vec(arb_opcode(), 0..6)
        }

        fn arb_compiler() -> impl Strategy<Value = HostcallSuperinstructionCompiler> {
            (2..8u32, 2..6usize).prop_map(|(min_support, max_window)| {
                HostcallSuperinstructionCompiler::new(true, min_support, max_window)
            })
        }

        proptest! {
            #[test]
            fn compile_plans_is_deterministic(
                compiler in arb_compiler(),
                traces in prop::collection::vec(arb_trace(), 0..8),
            ) {
                let plans1 = compiler.compile_plans(&traces);
                let plans2 = compiler.compile_plans(&traces);
                assert!(plans1 == plans2, "compile_plans must be deterministic");
            }

            #[test]
            fn all_plans_have_positive_cost_delta(
                compiler in arb_compiler(),
                traces in prop::collection::vec(arb_trace(), 1..8),
            ) {
                let plans = compiler.compile_plans(&traces);
                for plan in &plans {
                    assert!(
                        plan.expected_cost_delta > 0,
                        "plan {} has non-positive delta {}",
                        plan.plan_id,
                        plan.expected_cost_delta,
                    );
                }
            }

            #[test]
            fn plans_sorted_by_cost_delta_descending(
                compiler in arb_compiler(),
                traces in prop::collection::vec(arb_trace(), 1..8),
            ) {
                let plans = compiler.compile_plans(&traces);
                for pair in plans.windows(2) {
                    assert!(
                        pair[0].expected_cost_delta >= pair[1].expected_cost_delta,
                        "plans must be sorted by cost delta descending: {} vs {}",
                        pair[0].expected_cost_delta,
                        pair[1].expected_cost_delta,
                    );
                }
            }

            #[test]
            fn cost_delta_equals_baseline_minus_fused(
                width in 2..64usize,
            ) {
                let baseline = estimated_baseline_cost(width);
                let fused = estimated_fused_cost(width);
                let delta = baseline.saturating_sub(fused);
                assert!(
                    delta > 0,
                    "fused cost must be less than baseline for width {width}"
                );
                assert!(
                    delta == baseline - fused,
                    "delta must equal baseline - fused"
                );
            }

            #[test]
            fn fused_cost_strictly_less_than_baseline_for_width_ge_2(
                width in 2..1000usize,
            ) {
                let baseline = estimated_baseline_cost(width);
                let fused = estimated_fused_cost(width);
                assert!(
                    fused < baseline,
                    "fused ({fused}) must be < baseline ({baseline}) at width {width}"
                );
            }

            #[test]
            fn opcode_window_signature_is_deterministic(
                window in prop::collection::vec(arb_opcode(), 1..6),
            ) {
                let sig1 = opcode_window_signature(&window);
                let sig2 = opcode_window_signature(&window);
                assert!(sig1 == sig2, "signature must be deterministic");
                assert!(sig1.len() == 16, "signature must be 16 hex chars");
            }

            #[test]
            fn disabled_compiler_always_returns_empty(
                min_support in 1..10u32,
                max_window in 2..8usize,
                traces in prop::collection::vec(arb_trace(), 0..8),
            ) {
                let compiler = HostcallSuperinstructionCompiler::new(false, min_support, max_window);
                let plans = compiler.compile_plans(&traces);
                assert!(plans.is_empty(), "disabled compiler must return no plans");
            }

            #[test]
            fn plan_width_never_exceeds_max_window(
                compiler in arb_compiler(),
                traces in prop::collection::vec(arb_trace(), 1..8),
            ) {
                let plans = compiler.compile_plans(&traces);
                for plan in &plans {
                    assert!(
                        plan.width() <= compiler.max_window(),
                        "plan width {} exceeds max_window {}",
                        plan.width(),
                        compiler.max_window(),
                    );
                }
            }

            #[test]
            fn plan_support_count_meets_min_support(
                compiler in arb_compiler(),
                traces in prop::collection::vec(arb_trace(), 1..10),
            ) {
                let plans = compiler.compile_plans(&traces);
                for plan in &plans {
                    assert!(
                        plan.support_count >= compiler.min_support(),
                        "plan {} has support {} < min_support {}",
                        plan.plan_id,
                        plan.support_count,
                        compiler.min_support(),
                    );
                }
            }
        }
    }
}
