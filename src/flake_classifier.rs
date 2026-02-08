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

/// Classify a test failure based on its output text.
///
/// Scans the output for known transient failure patterns and returns
/// the first match, or `Deterministic` if no patterns match.
#[must_use]
pub fn classify_failure(output: &str) -> FlakeClassification {
    // Check each line for known patterns.  We use simple substring
    // matching to avoid regex dependency for this module.
    let lower = output.to_lowercase();

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
            && trimmed.contains("tmp")
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
        Self {
            max_retries: std::env::var("PI_CONFORMANCE_MAX_RETRIES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1),
            retry_delay_secs: std::env::var("PI_CONFORMANCE_RETRY_DELAY")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5),
            flake_budget: std::env::var("PI_CONFORMANCE_FLAKE_BUDGET")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3),
        }
    }
}

impl RetryPolicy {
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
}
