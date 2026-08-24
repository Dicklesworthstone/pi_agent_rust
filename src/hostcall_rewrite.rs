//! Constrained hostcall rewrite planner for hot-path marshalling.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostcallRewritePlanKind {
    /// The canonical marshalling path: always correct, never the fastest.
    BaselineCanonical,
    /// Hash-dispatch fusion on a typed opcode, authorized by params-hash and
    /// args-shape-hash equivalence with the baseline.
    FastOpcodeFusion,
    /// A plan the equality-saturation search found by fusing adjacent stages
    /// into one intrinsic (`crate::hostcall_egraph`).
    ///
    /// Distinct from [`Self::FastOpcodeFusion`] because the two are authorized
    /// by different evidence: that one by hash equivalence against the
    /// canonical path, this one by a rewrite chain whose every step preserves
    /// opcode identity and policy-stage count. Keeping them apart means
    /// telemetry can say which argument justified a given fast path.
    FusedIntrinsic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostcallRewritePlan {
    pub kind: HostcallRewritePlanKind,
    pub estimated_cost: u32,
    pub rule_id: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostcallRewriteDecision {
    pub selected: HostcallRewritePlan,
    pub expected_cost_delta: i64,
    pub fallback_reason: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostcallRewriteEngine {
    enabled: bool,
}

impl HostcallRewriteEngine {
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    #[must_use]
    pub fn from_env() -> Self {
        Self::from_opt(std::env::var("PI_HOSTCALL_EGRAPH_REWRITE").ok().as_deref())
    }

    #[must_use]
    pub fn from_opt(value: Option<&str>) -> Self {
        let enabled = value.is_none_or(|v| {
            !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "disabled"
            )
        });
        Self::new(enabled)
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub fn select_plan(
        &self,
        baseline: HostcallRewritePlan,
        candidates: &[HostcallRewritePlan],
    ) -> HostcallRewriteDecision {
        if !self.enabled {
            return HostcallRewriteDecision {
                selected: baseline,
                expected_cost_delta: 0,
                fallback_reason: Some("rewrite_disabled"),
            };
        }

        let mut best: Option<HostcallRewritePlan> = None;
        let mut ambiguous = false;
        for candidate in candidates {
            if candidate.estimated_cost >= baseline.estimated_cost {
                continue;
            }
            match best {
                None => best = Some(*candidate),
                Some(current) => {
                    if candidate.estimated_cost < current.estimated_cost {
                        best = Some(*candidate);
                        ambiguous = false;
                    } else if candidate.estimated_cost == current.estimated_cost
                        && (candidate.kind != current.kind || candidate.rule_id != current.rule_id)
                    {
                        ambiguous = true;
                    }
                }
            }
        }

        let Some(selected) = best else {
            return HostcallRewriteDecision {
                selected: baseline,
                expected_cost_delta: 0,
                fallback_reason: Some("no_better_candidate"),
            };
        };

        if ambiguous {
            return HostcallRewriteDecision {
                selected: baseline,
                expected_cost_delta: 0,
                fallback_reason: Some("ambiguous_min_cost"),
            };
        }

        HostcallRewriteDecision {
            selected,
            expected_cost_delta: i64::from(baseline.estimated_cost)
                - i64::from(selected.estimated_cost),
            fallback_reason: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASELINE: HostcallRewritePlan = HostcallRewritePlan {
        kind: HostcallRewritePlanKind::BaselineCanonical,
        estimated_cost: 100,
        rule_id: "baseline",
    };

    const FAST_FUSION: HostcallRewritePlan = HostcallRewritePlan {
        kind: HostcallRewritePlanKind::FastOpcodeFusion,
        estimated_cost: 35,
        rule_id: "fuse_hash_dispatch_fast_opcode",
    };

    #[test]
    fn rewrite_engine_selects_unique_lower_cost_plan() {
        let engine = HostcallRewriteEngine::new(true);
        let decision = engine.select_plan(BASELINE, &[FAST_FUSION]);
        assert_eq!(decision.selected, FAST_FUSION);
        assert_eq!(decision.expected_cost_delta, 65);
        assert!(decision.fallback_reason.is_none());
    }

    #[test]
    fn rewrite_engine_rejects_when_disabled() {
        let engine = HostcallRewriteEngine::new(false);
        let decision = engine.select_plan(BASELINE, &[FAST_FUSION]);
        assert_eq!(decision.selected, BASELINE);
        assert_eq!(decision.expected_cost_delta, 0);
        assert_eq!(decision.fallback_reason, Some("rewrite_disabled"));
    }

    #[test]
    fn rewrite_engine_rejects_ambiguous_min_cost_candidates() {
        let engine = HostcallRewriteEngine::new(true);
        let alt = HostcallRewritePlan {
            kind: HostcallRewritePlanKind::FastOpcodeFusion,
            estimated_cost: 35,
            rule_id: "fuse_validate_dispatch_fast_opcode",
        };
        let decision = engine.select_plan(BASELINE, &[FAST_FUSION, alt]);
        assert_eq!(decision.selected, BASELINE);
        assert_eq!(decision.fallback_reason, Some("ambiguous_min_cost"));
    }

    // ── Additional coverage ──

    #[test]
    fn rewrite_engine_rejects_no_better_candidate() {
        let engine = HostcallRewriteEngine::new(true);
        let worse = HostcallRewritePlan {
            kind: HostcallRewritePlanKind::FastOpcodeFusion,
            estimated_cost: 120,
            rule_id: "slow_rule",
        };
        let equal = HostcallRewritePlan {
            kind: HostcallRewritePlanKind::FastOpcodeFusion,
            estimated_cost: 100,
            rule_id: "equal_rule",
        };
        let decision = engine.select_plan(BASELINE, &[worse, equal]);
        assert_eq!(decision.selected, BASELINE);
        assert_eq!(decision.expected_cost_delta, 0);
        assert_eq!(decision.fallback_reason, Some("no_better_candidate"));
    }

    #[test]
    fn rewrite_engine_selects_cheapest_among_multiple_candidates() {
        let engine = HostcallRewriteEngine::new(true);
        let mid = HostcallRewritePlan {
            kind: HostcallRewritePlanKind::FastOpcodeFusion,
            estimated_cost: 50,
            rule_id: "mid_rule",
        };
        let cheapest = HostcallRewritePlan {
            kind: HostcallRewritePlanKind::FastOpcodeFusion,
            estimated_cost: 20,
            rule_id: "cheapest_rule",
        };
        let decision = engine.select_plan(BASELINE, &[mid, FAST_FUSION, cheapest]);
        assert_eq!(decision.selected, cheapest);
        assert_eq!(decision.expected_cost_delta, 80);
        assert!(decision.fallback_reason.is_none());
    }

    #[test]
    fn rewrite_engine_empty_candidates_returns_no_better() {
        let engine = HostcallRewriteEngine::new(true);
        let decision = engine.select_plan(BASELINE, &[]);
        assert_eq!(decision.selected, BASELINE);
        assert_eq!(decision.fallback_reason, Some("no_better_candidate"));
    }

    #[test]
    fn rewrite_engine_ambiguity_resolved_by_same_kind_and_rule() {
        let engine = HostcallRewriteEngine::new(true);
        // Same kind AND same rule_id = NOT ambiguous (they're the same plan)
        let dup = HostcallRewritePlan {
            kind: HostcallRewritePlanKind::FastOpcodeFusion,
            estimated_cost: 35,
            rule_id: "fuse_hash_dispatch_fast_opcode",
        };
        let decision = engine.select_plan(BASELINE, &[FAST_FUSION, dup]);
        assert_eq!(decision.selected, FAST_FUSION);
        assert!(decision.fallback_reason.is_none());
    }

    #[test]
    fn rewrite_engine_accessors() {
        let enabled = HostcallRewriteEngine::new(true);
        assert!(enabled.enabled());
        let disabled = HostcallRewriteEngine::new(false);
        assert!(!disabled.enabled());
    }

    #[test]
    fn plan_kind_variants_distinct() {
        assert_ne!(
            HostcallRewritePlanKind::BaselineCanonical,
            HostcallRewritePlanKind::FastOpcodeFusion
        );
    }

    #[test]
    fn from_env_returns_valid_engine() {
        // Smoke test: from_env() should not panic regardless of env state
        let engine = HostcallRewriteEngine::from_env();
        let _ = engine.enabled();
    }

    #[test]
    fn from_opt_disabled_by_known_off_values() {
        for value in ["0", "false", "off", "disabled", "FALSE", "OFF", "Disabled"] {
            let engine = HostcallRewriteEngine::from_opt(Some(value));
            assert!(!engine.enabled(), "should be disabled for '{value}'");
        }
    }

    #[test]
    fn from_opt_enabled_for_other_values_and_none() {
        // None (env var unset) → enabled
        assert!(HostcallRewriteEngine::from_opt(None).enabled());

        // Any non-disabled value → enabled
        for value in ["1", "true", "on", "yes", "anything_else"] {
            let engine = HostcallRewriteEngine::from_opt(Some(value));
            assert!(engine.enabled(), "should be enabled for '{value}'");
        }
    }

    #[test]
    fn rewrite_decision_cost_delta_correct_sign() {
        let engine = HostcallRewriteEngine::new(true);
        let decision = engine.select_plan(BASELINE, &[FAST_FUSION]);
        assert!(
            decision.expected_cost_delta > 0,
            "positive delta means improvement"
        );
        assert_eq!(
            decision.expected_cost_delta,
            i64::from(BASELINE.estimated_cost) - i64::from(FAST_FUSION.estimated_cost)
        );
    }

    // ── Property tests ──

    mod proptest_rewrite {
        use super::*;
        use proptest::prelude::*;

        fn arb_kind() -> impl Strategy<Value = HostcallRewritePlanKind> {
            prop::sample::select(vec![
                HostcallRewritePlanKind::BaselineCanonical,
                HostcallRewritePlanKind::FastOpcodeFusion,
            ])
        }

        fn arb_plan() -> impl Strategy<Value = HostcallRewritePlan> {
            (arb_kind(), 0..1000u32).prop_map(|(kind, cost)| HostcallRewritePlan {
                kind,
                estimated_cost: cost,
                rule_id: "arb_rule",
            })
        }

        proptest! {
            #[test]
            fn selected_cost_never_exceeds_baseline(
                baseline in arb_plan(),
                candidates in prop::collection::vec(arb_plan(), 0..10),
            ) {
                let engine = HostcallRewriteEngine::new(true);
                let decision = engine.select_plan(baseline, &candidates);
                assert!(
                    decision.selected.estimated_cost <= baseline.estimated_cost,
                    "selected cost {} must not exceed baseline {}",
                    decision.selected.estimated_cost,
                    baseline.estimated_cost,
                );
            }

            #[test]
            fn cost_delta_is_nonnegative(
                baseline in arb_plan(),
                candidates in prop::collection::vec(arb_plan(), 0..10),
            ) {
                let engine = HostcallRewriteEngine::new(true);
                let decision = engine.select_plan(baseline, &candidates);
                assert!(
                    decision.expected_cost_delta >= 0,
                    "cost delta must be non-negative, got {}",
                    decision.expected_cost_delta,
                );
            }

            #[test]
            fn cost_delta_equals_baseline_minus_selected(
                baseline in arb_plan(),
                candidates in prop::collection::vec(arb_plan(), 0..10),
            ) {
                let engine = HostcallRewriteEngine::new(true);
                let decision = engine.select_plan(baseline, &candidates);
                let expected_delta = i64::from(baseline.estimated_cost)
                    - i64::from(decision.selected.estimated_cost);
                assert_eq!(
                    decision.expected_cost_delta, expected_delta,
                    "delta must equal baseline - selected"
                );
            }

            #[test]
            fn disabled_engine_always_returns_baseline(
                baseline in arb_plan(),
                candidates in prop::collection::vec(arb_plan(), 0..10),
            ) {
                let engine = HostcallRewriteEngine::new(false);
                let decision = engine.select_plan(baseline, &candidates);
                assert_eq!(decision.selected, baseline);
                assert_eq!(decision.expected_cost_delta, 0);
                assert_eq!(decision.fallback_reason, Some("rewrite_disabled"));
            }

            #[test]
            fn select_plan_is_deterministic(
                baseline in arb_plan(),
                candidates in prop::collection::vec(arb_plan(), 0..10),
                enabled in any::<bool>(),
            ) {
                let engine = HostcallRewriteEngine::new(enabled);
                let d1 = engine.select_plan(baseline, &candidates);
                let d2 = engine.select_plan(baseline, &candidates);
                assert_eq!(d1.selected, d2.selected);
                assert_eq!(d1.expected_cost_delta, d2.expected_cost_delta);
                assert_eq!(d1.fallback_reason, d2.fallback_reason);
            }
        }
    }
}
