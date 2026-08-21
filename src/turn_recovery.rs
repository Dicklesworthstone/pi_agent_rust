//! Turn recovery: unexpected-stop classification + auto-continue
//! (bd-cv653.3.15).
//!
//! A model that stops mid-task — token budget exhausted mid-code-block, or a
//! "I will now edit the files" promise with no follow-through — used to end
//! the turn silently, leaving the user to nudge by hand. At each turn end the
//! agent now runs a deterministic, zero-cost heuristic classifier over the
//! final assistant message; actionable classes inject one synthetic
//! continue-nudge user message (visible in the transcript, persisted with the
//! normal message flow) and let the turn loop run again. A hard cap of
//! [`MAX_AUTO_CONTINUATIONS`] per run prevents loops; transport errors and
//! provider failover stay entirely out of scope here (RetrySettings owns
//! them).

use serde::{Deserialize, Serialize};

use crate::model::StopReason;

/// Schema/marker tag carried in nudge messages and logs.
pub const TURN_RECOVERY_SCHEMA: &str = "pi.turn_recovery.v1";

/// Maximum auto-continuations per agent run — then the user decides.
pub const MAX_AUTO_CONTINUATIONS: u8 = 2;

/// How aggressively to auto-continue unexpected stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnRecoveryMode {
    /// Never auto-continue.
    Off,
    /// Auto-continue only provable interruptions: token-budget truncation
    /// and structurally unclosed output.
    #[default]
    Conservative,
    /// Also auto-continue semantic premature stops (announced-but-unstarted
    /// work).
    Aggressive,
}

/// What the heuristic classifier concluded about a finished turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryClass {
    CleanStop,
    /// The provider hit max_tokens (`StopReason::Length`).
    BudgetTruncated,
    /// Output ends inside an unclosed code fence or on a dangling list
    /// bullet — structurally cut off even though the stop looked clean.
    UnclosedStructure,
    /// The message announces imminent work ("I will now ...") and then
    /// stops without doing it.
    SemanticPrematureStop,
}

impl RecoveryClass {
    /// Human-readable reason used in the transcript marker.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::CleanStop => "clean stop",
            Self::BudgetTruncated => "response truncated by token budget",
            Self::UnclosedStructure => "response ended inside unfinished output",
            Self::SemanticPrematureStop => "announced work was not started",
        }
    }

    const fn actionable_in(self, mode: TurnRecoveryMode) -> bool {
        match self {
            Self::CleanStop => false,
            Self::BudgetTruncated | Self::UnclosedStructure => {
                !matches!(mode, TurnRecoveryMode::Off)
            }
            Self::SemanticPrematureStop => matches!(mode, TurnRecoveryMode::Aggressive),
        }
    }
}

/// Classify a finished turn from its stop reason and final text.
///
/// Only `Stop` and `Length` stops are examined: errors, aborts, refusals,
/// tool-use and pause-turn stops all have dedicated handling elsewhere.
#[must_use]
pub fn classify(stop_reason: StopReason, text: &str) -> RecoveryClass {
    match stop_reason {
        StopReason::Length => return RecoveryClass::BudgetTruncated,
        StopReason::Stop => {}
        _ => return RecoveryClass::CleanStop,
    }

    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return RecoveryClass::CleanStop;
    }

    if has_unclosed_fence(trimmed) || ends_on_dangling_bullet(trimmed) {
        return RecoveryClass::UnclosedStructure;
    }
    if ends_on_unfulfilled_promise(trimmed) {
        return RecoveryClass::SemanticPrematureStop;
    }
    RecoveryClass::CleanStop
}

/// Odd number of code-fence delimiters means the last fence never closed.
fn has_unclosed_fence(text: &str) -> bool {
    let fences = text
        .lines()
        .filter(|line| line.trim_start().starts_with("```"))
        .count();
    fences % 2 == 1
}

/// The final line is a bare list bullet ("- ", "3.") with no content.
fn ends_on_dangling_bullet(text: &str) -> bool {
    let Some(last) = text.lines().next_back() else {
        return false;
    };
    let last = last.trim();
    if last.is_empty() {
        return false;
    }
    if matches!(last, "-" | "*" | "+") {
        return true;
    }
    last.strip_suffix('.')
        .is_some_and(|head| !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()))
}

/// The message's closing sentence announces imminent action and nothing
/// follows it.
fn ends_on_unfulfilled_promise(text: &str) -> bool {
    let tail: String = text
        .chars()
        .rev()
        .take(240)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let tail_lower = tail.to_lowercase();
    let Some(position) = PROMISE_PHRASES
        .iter()
        .filter_map(|phrase| tail_lower.rfind(phrase))
        .max()
    else {
        return false;
    };
    // Nothing but one closing sentence may follow the announcement: if
    // another sentence terminator appears well before the end, the promise
    // was presumably followed through in text.
    let after = &tail_lower[position..]; // ubs:ignore position from rfind on this same string, always a valid boundary
    let interior = after.trim_end_matches(['.', ':', '!', '…', ' ', '\n']);
    !interior.contains(". ") && !interior.contains(":\n\n")
}

const PROMISE_PHRASES: &[&str] = &[
    "i will now",
    "i'll now",
    "let me now",
    "i am going to",
    "i'm going to",
    "next, i will",
    "next, i'll",
    "now i will",
    "now i'll",
    "proceeding to",
];

/// A recovery decision: the class plus the nudge to inject.
#[derive(Debug, Clone)]
pub struct RecoveryAction {
    pub class: RecoveryClass,
    pub nudge_text: String,
}

/// Per-run auto-continuation state (mode gating + hard cap).
#[derive(Debug)]
pub struct TurnRecoveryState {
    mode: TurnRecoveryMode,
    continuations: u8,
}

impl TurnRecoveryState {
    #[must_use]
    pub const fn new(mode: TurnRecoveryMode) -> Self {
        Self {
            mode,
            continuations: 0,
        }
    }

    /// Number of auto-continuations issued so far this run.
    #[must_use]
    pub const fn continuations(&self) -> u8 {
        self.continuations
    }

    /// Classify a finished turn and decide whether to auto-continue.
    ///
    /// Consumes one continuation from the cap when it returns `Some`.
    pub fn evaluate(&mut self, stop_reason: StopReason, text: &str) -> Option<RecoveryAction> {
        if matches!(self.mode, TurnRecoveryMode::Off) {
            return None;
        }
        let class = classify(stop_reason, text);
        if !class.actionable_in(self.mode) {
            return None;
        }
        // After a continuation the model may legitimately open or close
        // structures spanning message boundaries (finishing an earlier code
        // fence looks "unclosed" in isolation), so the text-shape heuristics
        // only apply to the first stop; re-continuation needs the provider's
        // own truncation signal.
        if self.continuations > 0 && !matches!(class, RecoveryClass::BudgetTruncated) {
            return None;
        }
        if self.continuations >= MAX_AUTO_CONTINUATIONS {
            tracing::info!(
                schema = TURN_RECOVERY_SCHEMA,
                class = ?class,
                cap = MAX_AUTO_CONTINUATIONS,
                "auto-continuation cap reached; leaving the stop to the user"
            );
            return None;
        }
        self.continuations += 1;
        tracing::info!(
            schema = TURN_RECOVERY_SCHEMA,
            class = ?class,
            continuation = self.continuations,
            "auto-continuing unexpected stop"
        );
        Some(RecoveryAction {
            class,
            nudge_text: format!(
                "[auto-continue {}/{}: {}] Continue from exactly where you stopped. \
                 Do not repeat content you already produced; finish the remaining work.",
                self.continuations,
                MAX_AUTO_CONTINUATIONS,
                class.reason()
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_stop_is_budget_truncated() {
        assert_eq!(
            classify(StopReason::Length, "half a sentence"),
            RecoveryClass::BudgetTruncated
        );
    }

    #[test]
    fn clean_prose_is_clean() {
        assert_eq!(
            classify(StopReason::Stop, "All done. The tests pass."),
            RecoveryClass::CleanStop
        );
    }

    #[test]
    fn non_stop_reasons_never_classify() {
        for reason in [
            StopReason::ToolUse,
            StopReason::Error,
            StopReason::Aborted,
            StopReason::PauseTurn,
        ] {
            assert_eq!(
                classify(reason, "```\nunclosed"),
                RecoveryClass::CleanStop,
                "{reason:?}"
            );
        }
    }

    #[test]
    fn unclosed_fence_detected() {
        let text = "Here is the fix:\n```rust\nfn main() {\n    let x = 1;";
        assert_eq!(
            classify(StopReason::Stop, text),
            RecoveryClass::UnclosedStructure
        );
        let closed = "Here is the fix:\n```rust\nfn main() {}\n```\nDone.";
        assert_eq!(classify(StopReason::Stop, closed), RecoveryClass::CleanStop);
    }

    #[test]
    fn dangling_bullet_detected() {
        let text = "Plan:\n1. read the file\n2.";
        assert_eq!(
            classify(StopReason::Stop, text),
            RecoveryClass::UnclosedStructure
        );
        let fine = "Plan:\n1. read the file\n2. edit it";
        assert_eq!(classify(StopReason::Stop, fine), RecoveryClass::CleanStop);
    }

    #[test]
    fn unfulfilled_promise_detected() {
        let text = "The bug is in parse(). I will now edit the three files.";
        assert_eq!(
            classify(StopReason::Stop, text),
            RecoveryClass::SemanticPrematureStop
        );
        let fulfilled =
            "I will now edit the file. Done — the change is applied and the test passes.";
        assert_eq!(
            classify(StopReason::Stop, fulfilled),
            RecoveryClass::CleanStop
        );
    }

    #[test]
    fn mode_gating_matrix() {
        let semantic = "I'll now update the config.";
        let budget_text = "cut off";

        let mut off = TurnRecoveryState::new(TurnRecoveryMode::Off);
        assert!(off.evaluate(StopReason::Length, budget_text).is_none());

        let mut conservative = TurnRecoveryState::new(TurnRecoveryMode::Conservative);
        assert!(
            conservative
                .evaluate(StopReason::Length, budget_text)
                .is_some(),
            "conservative handles budget truncation"
        );
        assert!(
            conservative.evaluate(StopReason::Stop, semantic).is_none(),
            "conservative ignores the semantic class"
        );

        let mut aggressive = TurnRecoveryState::new(TurnRecoveryMode::Aggressive);
        assert!(
            aggressive.evaluate(StopReason::Stop, semantic).is_some(),
            "aggressive handles the semantic class"
        );
    }

    #[test]
    fn cap_stops_after_two() {
        let mut state = TurnRecoveryState::new(TurnRecoveryMode::Conservative);
        assert!(state.evaluate(StopReason::Length, "a").is_some());
        assert!(state.evaluate(StopReason::Length, "b").is_some());
        assert!(
            state.evaluate(StopReason::Length, "c").is_none(),
            "third auto-continue must be refused"
        );
        assert_eq!(state.continuations(), 2);
    }

    #[test]
    fn clean_stops_do_not_consume_the_cap() {
        let mut state = TurnRecoveryState::new(TurnRecoveryMode::Conservative);
        for _ in 0..10 {
            assert!(state.evaluate(StopReason::Stop, "all done.").is_none());
        }
        assert_eq!(state.continuations(), 0);
    }

    #[test]
    fn structure_heuristics_only_apply_to_the_first_stop() {
        let mut state = TurnRecoveryState::new(TurnRecoveryMode::Conservative);
        assert!(
            state
                .evaluate(StopReason::Length, "```rust\nfn main() {")
                .is_some()
        );
        assert!(
            state
                .evaluate(StopReason::Stop, "}\n```\nAll done.")
                .is_none(),
            "a continuation closing an earlier fence must not re-trigger"
        );
        assert!(
            state
                .evaluate(StopReason::Length, "more truncation")
                .is_some(),
            "provider-signaled truncation still continues"
        );
    }

    #[test]
    fn nudge_text_carries_reason_and_counter() {
        let mut state = TurnRecoveryState::new(TurnRecoveryMode::Conservative);
        let action = state
            .evaluate(StopReason::Length, "partial")
            .expect("actionable");
        assert!(action.nudge_text.contains("auto-continue 1/2"));
        assert!(action.nudge_text.contains("token budget"));
    }
}
