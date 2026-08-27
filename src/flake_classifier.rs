// Conformance flake classifier (bd-k5q5.5.4)
//
// Classifies test failures as deterministic or transient based on
// known flake patterns.  Used by CI retry logic and triage tooling.

use serde::{Deserialize, Serialize};

/// Category of a recognized transient failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlakeCategory {
    /// TS oracle process timed out.
    OracleTimeout,
    /// OS-level resource exhaustion (OOM, file descriptors).
    ResourceExhaustion,
    /// Filesystem lock or busy error.
    FsContention,
    /// TCP port already in use.
    PortConflict,
    /// Temp directory disappeared mid-test.
    TmpdirRace,
    /// QuickJS runtime ran out of memory.
    JsGcPressure,
}

impl FlakeCategory {
    /// All known flake categories.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::OracleTimeout,
            Self::ResourceExhaustion,
            Self::FsContention,
            Self::PortConflict,
            Self::TmpdirRace,
            Self::JsGcPressure,
        ]
    }

    /// Human-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::OracleTimeout => "TS oracle timeout",
            Self::ResourceExhaustion => "resource exhaustion",
            Self::FsContention => "filesystem contention",
            Self::PortConflict => "port conflict",
            Self::TmpdirRace => "temp directory race",
            Self::JsGcPressure => "QuickJS GC pressure",
        }
    }
}

impl std::fmt::Display for FlakeCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Result of classifying a test failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlakeClassification {
    /// Matches a known transient pattern — eligible for retry.
    Transient {
        category: FlakeCategory,
        matched_line: String,
    },
    /// No known flake pattern matched — treat as deterministic.
    Deterministic,
}

impl FlakeClassification {
    /// Whether this classification allows automatic retry.
    #[must_use]
    pub const fn is_retriable(&self) -> bool {
        matches!(self, Self::Transient { .. })
    }
}

/// A logged flake event for JSONL tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlakeEvent {
    pub target: String,
    pub classification: FlakeClassification,
    pub attempt: u32,
    pub timestamp: String,
}

/// Maximum input size for failure classification to prevent DoS.
const MAX_CLASSIFY_INPUT_SIZE: usize = 1024 * 1024; // 1MB

/// Classify a test failure based on its output text.
///
/// Scans the output for known transient failure patterns and returns
/// the first match, or `Deterministic` if no patterns match.
///
/// For security, input is bounded to prevent DoS through excessive
/// memory allocation or processing time on malicious test outputs.
#[must_use]
pub fn classify_failure(output: &str) -> FlakeClassification {
    // Bound input size to prevent DoS through excessive memory/CPU usage.
    // Legitimate test failure output should be well under 1MB.
    let bounded_output = if output.len() > MAX_CLASSIFY_INPUT_SIZE {
        &output[..MAX_CLASSIFY_INPUT_SIZE]
    } else {
        output
    };

    // Check each line for known patterns.  We use simple substring
    // matching to avoid regex dependency for this module.
    let lower = bounded_output.to_lowercase();

    for line in lower.lines() {
        let trimmed = line.trim();

        // Oracle timeout
        if (trimmed.contains("oracle") || trimmed.contains("bun"))
            && (trimmed.contains("timed out") || trimmed.contains("timeout"))
        {
            return FlakeClassification::Transient {
                category: FlakeCategory::OracleTimeout,
                matched_line: trimmed.to_string(),
            };
        }

        // Resource exhaustion
        if trimmed.contains("out of memory")
            || trimmed.contains("enomem")
            || trimmed.contains("cannot allocate")
        {
            // Distinguish JS GC pressure from OS-level OOM
            let category = if trimmed.contains("quickjs") || trimmed.contains("allocation failed") {
                FlakeCategory::JsGcPressure
            } else {
                FlakeCategory::ResourceExhaustion
            };
            return FlakeClassification::Transient {
                category,
                matched_line: trimmed.to_string(),
            };
        }

        // Filesystem contention
        if trimmed.contains("ebusy")
            || trimmed.contains("etxtbsy")
            || trimmed.contains("resource busy")
        {
            return FlakeClassification::Transient {
                category: FlakeCategory::FsContention,
                matched_line: trimmed.to_string(),
            };
        }

        // Port conflict
        if trimmed.contains("eaddrinuse") || trimmed.contains("address already in use") {
            return FlakeClassification::Transient {
                category: FlakeCategory::PortConflict,
                matched_line: trimmed.to_string(),
            };
        }

        // Temp directory race
        if (trimmed.contains("no such file or directory") || trimmed.contains("enoent"))
            && (trimmed.contains("/tmp") || trimmed.contains("\\tmp") || trimmed.contains("tmpdir"))
        {
            return FlakeClassification::Transient {
                category: FlakeCategory::TmpdirRace,
                matched_line: trimmed.to_string(),
            };
        }

        // QuickJS GC pressure (standalone)
        if trimmed.contains("quickjs") && trimmed.contains("allocation failed") {
            return FlakeClassification::Transient {
                category: FlakeCategory::JsGcPressure,
                matched_line: trimmed.to_string(),
            };
        }
    }

    FlakeClassification::Deterministic
}

/// Retry policy configuration.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum automatic retries per target per run.
    pub max_retries: u32,
    /// Delay between retry attempts in seconds.
    pub retry_delay_secs: u32,
    /// Per-target 30-day flake budget.
    pub flake_budget: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::from_env(|key| std::env::var(key))
    }
}

impl RetryPolicy {
    fn from_env<F>(get_env: F) -> Self
    where
        F: Fn(&str) -> std::result::Result<String, std::env::VarError>,
    {
        Self {
            max_retries: get_env("PI_CONFORMANCE_MAX_RETRIES")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .map_or(1, |v| v.min(100)), // Cap at 100 retries to prevent DoS
            retry_delay_secs: get_env("PI_CONFORMANCE_RETRY_DELAY")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .map_or(5, |v| v.min(3600)), // Cap at 1 hour to prevent DoS
            flake_budget: get_env("PI_CONFORMANCE_FLAKE_BUDGET")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .map_or(3, |v| v.min(1000)), // Cap at 1000 to prevent DoS
        }
    }

    /// Whether we should retry after this classification.
    #[must_use]
    pub const fn should_retry(&self, classification: &FlakeClassification, attempt: u32) -> bool {
        classification.is_retriable() && attempt < self.max_retries
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_oracle_timeout() {
        let output = "error: TS oracle process timed out after 30s";
        let result = classify_failure(output);
        assert!(matches!(
            result,
            FlakeClassification::Transient {
                category: FlakeCategory::OracleTimeout,
                ..
            }
        ));
    }

    #[test]
    fn classify_bun_timeout() {
        let output = "bun process timed out waiting for response";
        let result = classify_failure(output);
        assert!(matches!(
            result,
            FlakeClassification::Transient {
                category: FlakeCategory::OracleTimeout,
                ..
            }
        ));
    }

    #[test]
    fn classify_oom() {
        let output = "fatal: out of memory (allocator returned null)";
        let result = classify_failure(output);
        assert!(matches!(
            result,
            FlakeClassification::Transient {
                category: FlakeCategory::ResourceExhaustion,
                ..
            }
        ));
    }

    #[test]
    fn classify_enomem() {
        let output = "error: ENOMEM: not enough memory";
        let result = classify_failure(output);
        assert!(matches!(
            result,
            FlakeClassification::Transient {
                category: FlakeCategory::ResourceExhaustion,
                ..
            }
        ));
    }

    #[test]
    fn classify_quickjs_gc() {
        let output = "quickjs runtime: allocation failed, out of memory";
        let result = classify_failure(output);
        assert!(matches!(
            result,
            FlakeClassification::Transient {
                category: FlakeCategory::JsGcPressure,
                ..
            }
        ));
    }

    #[test]
    fn classify_ebusy() {
        let output = "error: EBUSY: resource busy or locked";
        let result = classify_failure(output);
        assert!(matches!(
            result,
            FlakeClassification::Transient {
                category: FlakeCategory::FsContention,
                ..
            }
        ));
    }

    #[test]
    fn classify_port_conflict() {
        let output = "listen EADDRINUSE: address already in use :::8080";
        let result = classify_failure(output);
        assert!(matches!(
            result,
            FlakeClassification::Transient {
                category: FlakeCategory::PortConflict,
                ..
            }
        ));
    }

    #[test]
    fn classify_tmpdir_race() {
        let output = "error: No such file or directory (os error 2), path: /tmp/pi-test-abc123";
        let result = classify_failure(output);
        assert!(matches!(
            result,
            FlakeClassification::Transient {
                category: FlakeCategory::TmpdirRace,
                ..
            }
        ));
    }

    #[test]
    fn classify_deterministic() {
        let output = "assertion failed: expected PASS but got FAIL\nnote: left == right";
        let result = classify_failure(output);
        assert_eq!(result, FlakeClassification::Deterministic);
    }

    #[test]
    fn classify_empty_output() {
        assert_eq!(classify_failure(""), FlakeClassification::Deterministic);
    }

    #[test]
    fn classification_is_retriable() {
        let transient = FlakeClassification::Transient {
            category: FlakeCategory::OracleTimeout,
            matched_line: "timeout".into(),
        };
        assert!(transient.is_retriable());
        assert!(!FlakeClassification::Deterministic.is_retriable());
    }

    #[test]
    fn retry_policy_default() {
        let policy = RetryPolicy {
            max_retries: 1,
            retry_delay_secs: 5,
            flake_budget: 3,
        };
        let transient = FlakeClassification::Transient {
            category: FlakeCategory::OracleTimeout,
            matched_line: "x".into(),
        };
        assert!(policy.should_retry(&transient, 0));
        assert!(!policy.should_retry(&transient, 1));
        assert!(!policy.should_retry(&FlakeClassification::Deterministic, 0));
    }

    #[test]
    fn flake_event_serde_roundtrip() {
        let event = FlakeEvent {
            target: "ext_conformance".into(),
            classification: FlakeClassification::Transient {
                category: FlakeCategory::OracleTimeout,
                matched_line: "oracle timed out".into(),
            },
            attempt: 1,
            timestamp: "2026-02-08T03:00:00Z".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: FlakeEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.target, "ext_conformance");
        assert!(back.classification.is_retriable());
    }

    #[test]
    fn retry_policy_bounds_environment_variables() {
        let policy = RetryPolicy::from_env(|key| match key {
            "PI_CONFORMANCE_MAX_RETRIES"
            | "PI_CONFORMANCE_RETRY_DELAY"
            | "PI_CONFORMANCE_FLAKE_BUDGET" => Ok("999999999".to_string()),
            _ => Err(std::env::VarError::NotPresent),
        });

        // Values should be capped at reasonable limits.
        assert_eq!(policy.max_retries, 100);
        assert_eq!(policy.retry_delay_secs, 3600);
        assert_eq!(policy.flake_budget, 1000);
    }

    #[test]
    fn flake_category_all_covered() {
        assert_eq!(FlakeCategory::all().len(), 6);
        for cat in FlakeCategory::all() {
            assert!(!cat.label().is_empty());
            assert!(!cat.to_string().is_empty());
        }
    }

    #[test]
    fn multiline_output_matches_first_pattern() {
        let output = "starting test...\ncompiling extensions...\nerror: bun process timed out\nassert failed";
        let result = classify_failure(output);
        assert!(matches!(
            result,
            FlakeClassification::Transient {
                category: FlakeCategory::OracleTimeout,
                ..
            }
        ));
    }

    #[test]
    fn case_insensitive_matching() {
        let output = "ERROR: OUT OF MEMORY";
        let result = classify_failure(output);
        assert!(result.is_retriable());
    }

    #[test]
    fn bounded_input_prevents_dos() {
        // Test that extremely large input is bounded to prevent DoS.
        // Create input larger than MAX_CLASSIFY_INPUT_SIZE with a flake pattern at the end.
        let large_prefix = "x".repeat(MAX_CLASSIFY_INPUT_SIZE + 1000);
        let pattern = "oracle timed out";
        let large_input = format!("{large_prefix}\n{pattern}");

        // Should still classify correctly but not process the oversized portion.
        // This would timeout or OOM without bounds checking.
        let result = classify_failure(&large_input);

        // The pattern is beyond the bound, so it won't be found.
        assert_eq!(result, FlakeClassification::Deterministic);

        // Test with pattern within bounds.
        let bounded_input = format!("{pattern}\n{}", "y".repeat(MAX_CLASSIFY_INPUT_SIZE));
        let result = classify_failure(&bounded_input);
        assert!(matches!(
            result,
            FlakeClassification::Transient {
                category: FlakeCategory::OracleTimeout,
                ..
            }
        ));
    }

    mod proptest_flake_classifier {
        use super::*;
        use proptest::prelude::*;

        /// Generates a string guaranteed to trigger a specific `FlakeCategory`.
        fn arb_transient_line() -> impl Strategy<Value = (String, FlakeCategory)> {
            prop_oneof![
                Just((
                    "oracle process timed out".to_string(),
                    FlakeCategory::OracleTimeout
                )),
                Just((
                    "bun timed out waiting".to_string(),
                    FlakeCategory::OracleTimeout
                )),
                Just((
                    "fatal: out of memory".to_string(),
                    FlakeCategory::ResourceExhaustion
                )),
                Just((
                    "error: ENOMEM".to_string(),
                    FlakeCategory::ResourceExhaustion
                )),
                Just((
                    "cannot allocate 4 GB".to_string(),
                    FlakeCategory::ResourceExhaustion
                )),
                Just((
                    "quickjs runtime: allocation failed, out of memory".to_string(),
                    FlakeCategory::JsGcPressure
                )),
                Just((
                    "EBUSY: resource busy".to_string(),
                    FlakeCategory::FsContention
                )),
                Just(("ETXTBSY".to_string(), FlakeCategory::FsContention)),
                Just((
                    "resource busy or locked".to_string(),
                    FlakeCategory::FsContention
                )),
                Just((
                    "EADDRINUSE on port 8080".to_string(),
                    FlakeCategory::PortConflict
                )),
                Just((
                    "address already in use".to_string(),
                    FlakeCategory::PortConflict
                )),
                Just((
                    "ENOENT: no such file or directory /tmp/pi-test".to_string(),
                    FlakeCategory::TmpdirRace
                )),
            ]
        }

        proptest! {
            #[test]
            fn classify_failure_never_panics(s in ".*") {
                let _ = classify_failure(&s);
            }

            #[test]
            fn deterministic_is_not_retriable(s in "[a-zA-Z0-9 ]{0,200}") {
                let result = classify_failure(&s);
                if result == FlakeClassification::Deterministic {
                    assert!(!result.is_retriable());
                }
            }

            #[test]
            fn transient_is_always_retriable(s in ".*") {
                let result = classify_failure(&s);
                if let FlakeClassification::Transient { .. } = &result {
                    assert!(result.is_retriable());
                }
            }

            #[test]
            fn known_transient_lines_classify_correctly(
                (line, expected_cat) in arb_transient_line()
            ) {
                let result = classify_failure(&line);
                match result {
                    FlakeClassification::Transient { category, .. } => {
                        assert_eq!(
                            category, expected_cat,
                            "line {line:?} got {category:?} expected {expected_cat:?}"
                        );
                    }
                    FlakeClassification::Deterministic => {
                        assert!(false, "expected Transient for {line:?}, got Deterministic");
                    }
                }
            }

            #[test]
            fn classify_is_case_insensitive(
                (line, expected_cat) in arb_transient_line()
            ) {
                let upper = classify_failure(&line.to_uppercase());
                let lower = classify_failure(&line.to_lowercase());
                match (&upper, &lower) {
                    (
                        FlakeClassification::Transient { category: cu, .. },
                        FlakeClassification::Transient { category: cl, .. },
                    ) => {
                        assert_eq!(*cu, expected_cat);
                        assert_eq!(*cl, expected_cat);
                    }
                    _ => assert!(false, "expected both Transient for line {line:?}"),
                }
            }

            #[test]
            fn noise_prefix_preserves_classification(
                noise in "[a-zA-Z0-9 ]{0,50}",
                (line, expected_cat) in arb_transient_line(),
            ) {
                let input = format!("{noise}\n{line}");
                let result = classify_failure(&input);
                match result {
                    FlakeClassification::Transient { category, .. } => {
                        assert_eq!(category, expected_cat);
                    }
                    FlakeClassification::Deterministic => {
                        assert!(false, "expected Transient for input with line {line:?}");
                    }
                }
            }

            #[test]
            fn whitespace_only_is_deterministic(s in "[ \\t\\n]{0,100}") {
                assert_eq!(classify_failure(&s), FlakeClassification::Deterministic);
            }

            #[test]
            fn serde_roundtrip_transient((line, _cat) in arb_transient_line()) {
                let result = classify_failure(&line);
                let json = serde_json::to_string(&result).unwrap();
                let back: FlakeClassification = serde_json::from_str(&json).unwrap();
                assert_eq!(result, back);
            }

            #[test]
            fn serde_roundtrip_category(idx in 0..6usize) {
                let cat = FlakeCategory::all()[idx];
                let json = serde_json::to_string(&cat).unwrap();
                let back: FlakeCategory = serde_json::from_str(&json).unwrap();
                assert_eq!(cat, back);
            }

            #[test]
            fn all_categories_have_nonempty_labels(idx in 0..6usize) {
                let cat = FlakeCategory::all()[idx];
                assert!(!cat.label().is_empty());
                assert!(!cat.to_string().is_empty());
                assert_eq!(cat.label(), cat.to_string());
            }

            #[test]
            fn retry_policy_respects_attempt_bound(
                max_retries in 0..10u32,
                attempt in 0..20u32,
            ) {
                let policy = RetryPolicy {
                    max_retries,
                    retry_delay_secs: 1,
                    flake_budget: 3,
                };
                let transient = FlakeClassification::Transient {
                    category: FlakeCategory::OracleTimeout,
                    matched_line: "x".into(),
                };
                let should = policy.should_retry(&transient, attempt);
                assert_eq!(should, attempt < max_retries);
            }

            #[test]
            fn retry_policy_never_retries_deterministic(
                max_retries in 0..10u32,
                attempt in 0..20u32,
            ) {
                let policy = RetryPolicy {
                    max_retries,
                    retry_delay_secs: 1,
                    flake_budget: 3,
                };
                assert!(!policy.should_retry(&FlakeClassification::Deterministic, attempt));
            }

            #[test]
            fn flake_event_serde_roundtrip_prop(
                target in "[a-z_]{1,20}",
                attempt in 0..100u32,
                idx in 0..6usize,
            ) {
                let cat = FlakeCategory::all()[idx];
                let event = FlakeEvent {
                    target: target.clone(),
                    classification: FlakeClassification::Transient {
                        category: cat,
                        matched_line: "matched".into(),
                    },
                    attempt,
                    timestamp: "2026-01-01T00:00:00Z".into(),
                };
                let json = serde_json::to_string(&event).unwrap();
                let back: FlakeEvent = serde_json::from_str(&json).unwrap();
                assert_eq!(back.target, target);
                assert_eq!(back.attempt, attempt);
                assert!(back.classification.is_retriable());
            }
        }
    }
}
