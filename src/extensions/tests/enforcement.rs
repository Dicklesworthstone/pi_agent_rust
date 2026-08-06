//! Enforcement state machine tests.

use super::*;

// ====================================================================
// SEC-3.4: Enforcement state machine tests
// ====================================================================

#[test]
fn enforcement_state_ordering() {
    assert!(EnforcementState::Allow < EnforcementState::Harden);
    assert!(EnforcementState::Harden < EnforcementState::Prompt);
    assert!(EnforcementState::Prompt < EnforcementState::Deny);
    assert!(EnforcementState::Deny < EnforcementState::Terminate);
}

#[test]
fn enforcement_state_display() {
    assert_eq!(EnforcementState::Allow.as_str(), "allow");
    assert_eq!(EnforcementState::Harden.as_str(), "harden");
    assert_eq!(EnforcementState::Prompt.as_str(), "prompt");
    assert_eq!(EnforcementState::Deny.as_str(), "deny");
    assert_eq!(EnforcementState::Terminate.as_str(), "terminate");
}

#[test]
fn enforcement_state_from_risk_action() {
    assert_eq!(
        EnforcementState::from_risk_action(RuntimeRiskAction::Allow),
        EnforcementState::Allow
    );
    assert_eq!(
        EnforcementState::from_risk_action(RuntimeRiskAction::Harden),
        EnforcementState::Harden
    );
    assert_eq!(
        EnforcementState::from_risk_action(RuntimeRiskAction::Deny),
        EnforcementState::Deny
    );
    assert_eq!(
        EnforcementState::from_risk_action(RuntimeRiskAction::Terminate),
        EnforcementState::Terminate
    );
}

#[test]
fn enforcement_state_to_risk_action_maps_prompt_to_harden() {
    assert_eq!(
        EnforcementState::Prompt.to_risk_action(),
        RuntimeRiskAction::Harden
    );
}

#[test]
fn score_bands_safe_more_aggressive_than_permissive() {
    let safe = EnforcementScoreBands::safe();
    let permissive = EnforcementScoreBands::permissive();
    assert!(safe.harden < permissive.harden);
    assert!(safe.prompt < permissive.prompt);
    assert!(safe.deny < permissive.deny);
    assert!(safe.terminate < permissive.terminate);
}

#[test]
fn score_bands_balanced_between_safe_and_permissive() {
    let safe = EnforcementScoreBands::safe();
    let balanced = EnforcementScoreBands::balanced();
    let permissive = EnforcementScoreBands::permissive();
    assert!(safe.harden < balanced.harden);
    assert!(balanced.harden < permissive.harden);
}

#[test]
fn score_bands_classify_low_score_is_allow() {
    let bands = EnforcementScoreBands::balanced();
    assert_eq!(bands.classify(0.0), EnforcementState::Allow);
    assert_eq!(bands.classify(0.10), EnforcementState::Allow);
    assert_eq!(bands.classify(0.39), EnforcementState::Allow);
}

#[test]
fn score_bands_classify_at_threshold_triggers_state() {
    let bands = EnforcementScoreBands::balanced();
    assert_eq!(bands.classify(0.40), EnforcementState::Harden);
    assert_eq!(bands.classify(0.60), EnforcementState::Prompt);
    assert_eq!(bands.classify(0.75), EnforcementState::Deny);
    assert_eq!(bands.classify(0.90), EnforcementState::Terminate);
}

#[test]
fn score_bands_classify_max_score_is_terminate() {
    let bands = EnforcementScoreBands::balanced();
    assert_eq!(bands.classify(1.0), EnforcementState::Terminate);
}

#[test]
#[allow(clippy::float_cmp)]
fn score_bands_for_profile_selection() {
    let safe = EnforcementScoreBands::for_profile("safe");
    assert_eq!(safe.harden, EnforcementScoreBands::safe().harden);
    let strict = EnforcementScoreBands::for_profile("strict");
    assert_eq!(strict.harden, EnforcementScoreBands::safe().harden);
    let balanced = EnforcementScoreBands::for_profile("balanced");
    assert_eq!(balanced.harden, EnforcementScoreBands::balanced().harden);
    let permissive = EnforcementScoreBands::for_profile("permissive");
    assert_eq!(
        permissive.harden,
        EnforcementScoreBands::permissive().harden
    );
    let unknown = EnforcementScoreBands::for_profile("unknown");
    assert_eq!(unknown.harden, EnforcementScoreBands::balanced().harden);
}

#[test]
fn enforcement_machine_starts_at_allow() {
    let sm = EnforcementStateMachine::new("balanced");
    assert_eq!(sm.state(), EnforcementState::Allow);
    assert_eq!(sm.evaluation_count(), 0);
}

#[test]
fn enforcement_machine_escalation_is_immediate() {
    let mut sm = EnforcementStateMachine::new("balanced");
    // Score 0.80 → Deny (balanced deny threshold = 0.75)
    let t = sm.evaluate(0.80);
    assert_eq!(t.from, EnforcementState::Allow);
    assert_eq!(t.to, EnforcementState::Deny);
    assert!(!t.hysteresis_active);
}

#[test]
fn enforcement_machine_escalation_jumps_multiple_levels() {
    let mut sm = EnforcementStateMachine::new("balanced");
    // Score 0.95 → Terminate (balanced terminate threshold = 0.90)
    let t = sm.evaluate(0.95);
    assert_eq!(t.from, EnforcementState::Allow);
    assert_eq!(t.to, EnforcementState::Terminate);
}

#[test]
fn enforcement_machine_hysteresis_prevents_immediate_de_escalation() {
    let mut sm = EnforcementStateMachine::new("balanced");
    // Escalate to Harden
    sm.evaluate(0.50);
    assert_eq!(sm.state(), EnforcementState::Harden);

    // Score drops to Allow band but still above de-escalation floor.
    // balanced harden = 0.40, margin = 0.10, floor = 0.30
    let t = sm.evaluate(0.35);
    assert_eq!(
        t.to,
        EnforcementState::Harden,
        "hysteresis should prevent de-escalation"
    );
    assert!(t.hysteresis_active);
}

#[test]
fn enforcement_machine_de_escalation_requires_cooldown() {
    let mut sm = EnforcementStateMachine::new("balanced");
    // Escalate to Harden
    sm.evaluate(0.50);
    assert_eq!(sm.state(), EnforcementState::Harden);

    // Score drops well below floor (0.30). Need 3 consecutive calls.
    let t1 = sm.evaluate(0.10);
    assert_eq!(t1.to, EnforcementState::Harden, "cooldown 1 of 3");
    assert!(t1.hysteresis_active);
    assert_eq!(t1.cooldown_counter, 1);

    let t2 = sm.evaluate(0.10);
    assert_eq!(t2.to, EnforcementState::Harden, "cooldown 2 of 3");
    assert!(t2.hysteresis_active);
    assert_eq!(t2.cooldown_counter, 2);

    let t3 = sm.evaluate(0.10);
    assert_eq!(
        t3.to,
        EnforcementState::Allow,
        "cooldown 3 of 3 → de-escalate"
    );
    assert!(!t3.hysteresis_active);
}

#[test]
fn enforcement_machine_de_escalation_one_level_at_a_time() {
    let mut sm = EnforcementStateMachine::new("balanced");
    // Escalate to Deny
    sm.evaluate(0.80);
    assert_eq!(sm.state(), EnforcementState::Deny);

    // De-escalate: Deny → Prompt (3 cooldown calls)
    sm.evaluate(0.10);
    sm.evaluate(0.10);
    let t = sm.evaluate(0.10);
    assert_eq!(t.from, EnforcementState::Deny);
    assert_eq!(
        t.to,
        EnforcementState::Prompt,
        "de-escalate one level: Deny → Prompt"
    );

    // Prompt → Harden (3 more cooldown calls)
    sm.evaluate(0.10);
    sm.evaluate(0.10);
    let t = sm.evaluate(0.10);
    assert_eq!(
        t.to,
        EnforcementState::Harden,
        "de-escalate one level: Prompt → Harden"
    );

    // Harden → Allow (3 more cooldown calls)
    sm.evaluate(0.10);
    sm.evaluate(0.10);
    let t = sm.evaluate(0.10);
    assert_eq!(
        t.to,
        EnforcementState::Allow,
        "de-escalate one level: Harden → Allow"
    );
}

#[test]
fn enforcement_machine_terminate_is_terminal() {
    let mut sm = EnforcementStateMachine::new("balanced");
    sm.evaluate(0.95);
    assert_eq!(sm.state(), EnforcementState::Terminate);

    // Even a score of 0.0 should not de-escalate from Terminate.
    for _ in 0..10 {
        let t = sm.evaluate(0.0);
        assert_eq!(t.to, EnforcementState::Terminate);
        assert!(!t.hysteresis_active);
    }
}

#[test]
fn enforcement_machine_cooldown_resets_on_escalation() {
    let mut sm = EnforcementStateMachine::new("balanced");
    // Escalate to Harden
    sm.evaluate(0.50);

    // Start cooldown
    sm.evaluate(0.10);
    sm.evaluate(0.10);
    // 2 calls in cooldown. Now escalate again.
    sm.evaluate(0.70); // → Prompt
    assert_eq!(sm.state(), EnforcementState::Prompt);

    // Cooldown should have reset. Need 3 fresh calls to de-escalate.
    sm.evaluate(0.10);
    assert_eq!(sm.state(), EnforcementState::Prompt);
}

#[test]
fn enforcement_machine_same_band_resets_cooldown() {
    let mut sm = EnforcementStateMachine::new("balanced");
    sm.evaluate(0.50); // Harden
    assert_eq!(sm.state(), EnforcementState::Harden);

    // Start cooldown
    sm.evaluate(0.10);
    sm.evaluate(0.10);
    // 2 of 3. Now a score back in Harden band resets cooldown.
    sm.evaluate(0.45);
    assert_eq!(sm.state(), EnforcementState::Harden);

    // Need 3 fresh calls again
    sm.evaluate(0.10);
    assert_eq!(sm.state(), EnforcementState::Harden);
}

#[test]
fn enforcement_machine_evaluation_count_increments() {
    let mut sm = EnforcementStateMachine::new("balanced");
    sm.evaluate(0.10);
    sm.evaluate(0.50);
    sm.evaluate(0.10);
    assert_eq!(sm.evaluation_count(), 3);
}

#[test]
fn enforcement_machine_no_flapping_under_borderline_scores() {
    // Borderline score around harden threshold (0.40 for balanced).
    // Scores oscillating between 0.38 and 0.42 should NOT cause flapping.
    let mut sm = EnforcementStateMachine::new("balanced");

    // Initial escalation to Harden.
    sm.evaluate(0.42);
    assert_eq!(sm.state(), EnforcementState::Harden);

    // Score drops just below threshold (0.38 > floor 0.30).
    let t = sm.evaluate(0.38);
    assert_eq!(t.to, EnforcementState::Harden, "should not de-escalate");
    assert!(t.hysteresis_active);

    // Score goes back above threshold.
    let t = sm.evaluate(0.42);
    assert_eq!(t.to, EnforcementState::Harden, "still in Harden band");

    // Score below again — still no flapping.
    let t = sm.evaluate(0.38);
    assert_eq!(t.to, EnforcementState::Harden);
}

#[test]
fn enforcement_machine_jitter_10_evaluations_no_flap() {
    // Simulate 10 evaluations with jitter around deny threshold.
    let mut sm = EnforcementStateMachine::new("balanced");
    // balanced deny = 0.75
    sm.evaluate(0.80); // → Deny
    assert_eq!(sm.state(), EnforcementState::Deny);

    let scores = [0.73, 0.76, 0.72, 0.74, 0.77, 0.71, 0.73, 0.76, 0.74, 0.72];
    let mut transitions = Vec::new();
    for &s in &scores {
        let t = sm.evaluate(s);
        transitions.push(t.to);
    }
    // All should be Deny — no flapping.
    for (i, state) in transitions.iter().enumerate() {
        assert_eq!(
            *state,
            EnforcementState::Deny,
            "Evaluation {i}: expected Deny, got {state:?}"
        );
    }
}

// --- Merge with policy ---

#[test]
fn merge_policy_deny_overrides_allow_enforcement() {
    let result =
        EnforcementStateMachine::merge_with_policy(EnforcementState::Allow, PolicyDecision::Deny);
    assert_eq!(result, EnforcementState::Deny);
}

#[test]
fn merge_enforcement_terminate_overrides_policy_allow() {
    let result = EnforcementStateMachine::merge_with_policy(
        EnforcementState::Terminate,
        PolicyDecision::Allow,
    );
    assert_eq!(result, EnforcementState::Terminate);
}

#[test]
fn merge_both_allow_is_allow() {
    let result =
        EnforcementStateMachine::merge_with_policy(EnforcementState::Allow, PolicyDecision::Allow);
    assert_eq!(result, EnforcementState::Allow);
}

#[test]
fn merge_policy_prompt_with_harden_enforcement() {
    // Policy says Prompt, enforcement says Harden.
    // Prompt (2) > Harden (1), so result is Prompt.
    let result = EnforcementStateMachine::merge_with_policy(
        EnforcementState::Harden,
        PolicyDecision::Prompt,
    );
    assert_eq!(result, EnforcementState::Prompt);
}

#[test]
fn merge_enforcement_deny_with_policy_prompt() {
    // Enforcement Deny (3) > Policy Prompt (2).
    let result =
        EnforcementStateMachine::merge_with_policy(EnforcementState::Deny, PolicyDecision::Prompt);
    assert_eq!(result, EnforcementState::Deny);
}

// --- Serialization ---

#[test]
fn enforcement_state_serde_roundtrip() {
    for state in [
        EnforcementState::Allow,
        EnforcementState::Harden,
        EnforcementState::Prompt,
        EnforcementState::Deny,
        EnforcementState::Terminate,
    ] {
        let json = serde_json::to_string(&state).expect("serialize");
        let parsed: EnforcementState = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed, state);
    }
}

#[test]
fn enforcement_transition_serializes() {
    let t = EnforcementTransition {
        from: EnforcementState::Allow,
        to: EnforcementState::Harden,
        hysteresis_active: false,
        raw_band: EnforcementState::Harden,
        score: 0.45,
        cooldown_counter: 0,
    };
    let json = serde_json::to_string(&t).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
    assert_eq!(parsed["from"], "allow");
    assert_eq!(parsed["to"], "harden");
}

#[test]
fn enforcement_score_bands_serde_roundtrip() {
    let bands = EnforcementScoreBands::balanced();
    let json = serde_json::to_string(&bands).expect("serialize");
    let parsed: EnforcementScoreBands = serde_json::from_str(&json).expect("parse");
    assert!((parsed.harden - bands.harden).abs() < f64::EPSILON);
    assert!((parsed.deny - bands.deny).abs() < f64::EPSILON);
}

#[test]
fn enforcement_state_machine_serde_roundtrip() {
    let mut sm = EnforcementStateMachine::new("balanced");
    sm.evaluate(0.50);
    sm.evaluate(0.80);
    let json = serde_json::to_string(&sm).expect("serialize");
    let parsed: EnforcementStateMachine = serde_json::from_str(&json).expect("parse");
    assert_eq!(parsed.state(), sm.state());
    assert_eq!(parsed.evaluation_count(), sm.evaluation_count());
}

// --- Determinism ---

#[test]
fn enforcement_machine_deterministic_sequence() {
    let scores = [0.10, 0.45, 0.70, 0.30, 0.20, 0.15, 0.10, 0.50, 0.95];
    let mut sm1 = EnforcementStateMachine::new("balanced");
    let mut sm2 = EnforcementStateMachine::new("balanced");

    let results1: Vec<_> = scores.iter().map(|&s| sm1.evaluate(s).to).collect();
    let results2: Vec<_> = scores.iter().map(|&s| sm2.evaluate(s).to).collect();

    assert_eq!(
        results1, results2,
        "Same inputs must produce identical state sequences"
    );
}

#[test]
fn enforcement_machine_profile_comparison_safe_vs_permissive() {
    // Same score sequence produces different states for different profiles.
    let scores = [0.35, 0.55, 0.70, 0.85];
    let mut safe_sm = EnforcementStateMachine::new("safe");
    let mut perm_sm = EnforcementStateMachine::new("permissive");

    for &score in &scores {
        safe_sm.evaluate(score);
        perm_sm.evaluate(score);
    }

    // Safe profile escalates more aggressively.
    assert!(
        safe_sm.state() >= perm_sm.state(),
        "Safe profile should be at least as severe: safe={:?}, permissive={:?}",
        safe_sm.state(),
        perm_sm.state()
    );
}

#[test]
fn enforcement_machine_custom_config() {
    let bands = EnforcementScoreBands {
        allow: 0.0,
        harden: 0.20,
        prompt: 0.40,
        deny: 0.60,
        terminate: 0.80,
    };
    let hysteresis = EnforcementHysteresis {
        de_escalation_margin: 0.05,
        cooldown_calls: 2,
    };
    let mut sm = EnforcementStateMachine::with_config(bands, hysteresis);
    sm.evaluate(0.25);
    assert_eq!(sm.state(), EnforcementState::Harden);

    // De-escalation with custom margin (0.05) and cooldown (2)
    sm.evaluate(0.10);
    sm.evaluate(0.10);
    assert_eq!(sm.state(), EnforcementState::Allow);
}
