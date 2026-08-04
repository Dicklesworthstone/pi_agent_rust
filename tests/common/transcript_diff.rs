//! Transcript diff tooling for E2E test failure triage.
//!
//! Compares two JSONL transcripts (expected vs actual) and produces
//! structured diff reports for deterministic failure analysis.
//!
//! # Versioned Logging Contract
//!
//! All transcript events conform to schema `pi.test.transcript.v1`:
//! - Each line is a JSON object with a `type` field
//! - Known types: `scenario_header`, `step_result`, `event_boundary`, `artifact`
//! - All timestamps are in milliseconds since scenario start
//! - Correlation IDs follow `{run_id}/{step_index}` format
//! - Fields are ordered alphabetically for stable diffing
//!
//! # Example
//!
//! ```ignore
//! let diff = TranscriptDiff::compare(&expected_lines, &actual_lines);
//! if diff.has_differences() {
//!     eprintln!("{}", diff.human_summary());
//!     diff.write_jsonl(path)?;
//! }
//! ```

use serde::Serialize;
use std::fmt;

/// Schema version for the transcript logging contract.
pub const TRANSCRIPT_SCHEMA: &str = "pi.test.transcript.v1";

/// Known event types in a transcript.
pub const EVENT_TYPE_HEADER: &str = "scenario_header";
pub const EVENT_TYPE_STEP: &str = "step_result";
pub const EVENT_TYPE_BOUNDARY: &str = "event_boundary";
pub const EVENT_TYPE_ARTIFACT: &str = "artifact";

// ---------------------------------------------------------------------------
// Parsed transcript
// ---------------------------------------------------------------------------

/// A single parsed line from a JSONL transcript.
#[derive(Clone, Debug)]
pub struct TranscriptLine {
    /// Line number (0-indexed).
    pub index: usize,
    /// Parsed JSON value.
    pub value: serde_json::Value,
    /// The `type` field, if present.
    pub event_type: Option<String>,
}

impl TranscriptLine {
    fn parse(index: usize, raw: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(raw).ok()?;
        let event_type = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(String::from);
        Some(Self {
            index,
            value,
            event_type,
        })
    }
}

/// Parse a JSONL transcript into structured lines.
pub fn parse_transcript(content: &str) -> Vec<TranscriptLine> {
    content
        .lines()
        .enumerate()
        .filter_map(|(i, line)| TranscriptLine::parse(i, line))
        .collect()
}

// ---------------------------------------------------------------------------
// Diff types
// ---------------------------------------------------------------------------

/// A single difference between expected and actual transcripts.
#[derive(Clone, Debug, Serialize)]
pub struct StepDiff {
    /// Which step (by index or label).
    pub step_ref: String,
    /// What differs.
    pub field: String,
    /// Expected value (stringified).
    pub expected: String,
    /// Actual value (stringified).
    pub actual: String,
    /// Severity: `mismatch`, `missing`, `extra`.
    pub severity: DiffSeverity,
}

/// Severity level for a diff entry.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub enum DiffSeverity {
    /// Values don't match.
    Mismatch,
    /// Expected step missing from actual.
    Missing,
    /// Unexpected extra step in actual.
    Extra,
}

impl fmt::Display for DiffSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mismatch => write!(f, "MISMATCH"),
            Self::Missing => write!(f, "MISSING"),
            Self::Extra => write!(f, "EXTRA"),
        }
    }
}

/// Complete diff result between two transcripts.
#[derive(Clone, Debug)]
pub struct TranscriptDiff {
    /// Individual differences found.
    pub diffs: Vec<StepDiff>,
    /// Number of steps in expected transcript.
    pub expected_step_count: usize,
    /// Number of steps in actual transcript.
    pub actual_step_count: usize,
}

impl TranscriptDiff {
    /// Compare expected and actual transcript lines.
    ///
    /// Compares `step_result` entries by index, checking `label`, `action`,
    /// `success`, and `expected` fields.
    pub fn compare(expected: &[TranscriptLine], actual: &[TranscriptLine]) -> Self {
        let expected_steps: Vec<&TranscriptLine> = expected
            .iter()
            .filter(|l| l.event_type.as_deref() == Some(EVENT_TYPE_STEP))
            .collect();
        let actual_steps: Vec<&TranscriptLine> = actual
            .iter()
            .filter(|l| l.event_type.as_deref() == Some(EVENT_TYPE_STEP))
            .collect();

        let mut diffs = Vec::new();

        let max_len = expected_steps.len().max(actual_steps.len());
        for i in 0..max_len {
            match (expected_steps.get(i), actual_steps.get(i)) {
                (Some(exp), Some(act)) => {
                    compare_step_fields(i, &exp.value, &act.value, &mut diffs);
                }
                (Some(exp), None) => {
                    let label = exp.value["label"].as_str().unwrap_or("?").to_string();
                    diffs.push(StepDiff {
                        step_ref: format!("step[{i}]/{label}"),
                        field: "(entire step)".to_string(),
                        expected: "(present)".to_string(),
                        actual: "(absent)".to_string(),
                        severity: DiffSeverity::Missing,
                    });
                }
                (None, Some(act)) => {
                    let label = act.value["label"].as_str().unwrap_or("?").to_string();
                    diffs.push(StepDiff {
                        step_ref: format!("step[{i}]/{label}"),
                        field: "(entire step)".to_string(),
                        expected: "(absent)".to_string(),
                        actual: "(present)".to_string(),
                        severity: DiffSeverity::Extra,
                    });
                }
                (None, None) => panic!(),
            }
        }

        Self {
            diffs,
            expected_step_count: expected_steps.len(),
            actual_step_count: actual_steps.len(),
        }
    }

    /// Whether any differences were found.
    pub const fn has_differences(&self) -> bool {
        !self.diffs.is_empty()
    }

    /// Produce a compact human-readable diff summary.
    pub fn human_summary(&self) -> String {
        use std::fmt::Write as _;

        if self.diffs.is_empty() {
            return "Transcripts match (no differences found).".to_string();
        }

        let mut out = String::new();
        let _ = writeln!(
            out,
            "Transcript diff: {} difference(s) (expected {} steps, actual {} steps)",
            self.diffs.len(),
            self.expected_step_count,
            self.actual_step_count,
        );
        out.push_str("─────────────────────────────────────────\n");

        for diff in &self.diffs {
            let _ = write!(
                out,
                "  [{severity}] {step} :: {field}\n    expected: {exp}\n    actual:   {act}\n",
                severity = diff.severity,
                step = diff.step_ref,
                field = diff.field,
                exp = diff.expected,
                act = diff.actual,
            );
        }

        out
    }

    /// Write the diff as JSONL.
    pub fn write_jsonl(&self, path: &std::path::Path) -> std::io::Result<()> {
        use std::fmt::Write as _;
        let mut buf = String::new();

        let header = serde_json::json!({
            "type": "transcript_diff_header",
            "schema": TRANSCRIPT_SCHEMA,
            "diff_count": self.diffs.len(),
            "expected_step_count": self.expected_step_count,
            "actual_step_count": self.actual_step_count,
        });
        let _ = writeln!(
            buf,
            "{}",
            serde_json::to_string(&header).unwrap_or_default()
        );

        for diff in &self.diffs {
            let line = serde_json::json!({
                "type": "step_diff",
                "step_ref": diff.step_ref,
                "field": diff.field,
                "expected": diff.expected,
                "actual": diff.actual,
                "severity": diff.severity.to_string(),
            });
            let _ = writeln!(buf, "{}", serde_json::to_string(&line).unwrap_or_default());
        }

        std::fs::write(path, buf)
    }
}

/// Generate a compact failure summary for a failed scenario transcript.
pub fn failure_summary(scenario_name: &str, steps: &[TranscriptLine], exit_status: &str) -> String {
    use std::fmt::Write as _;

    let step_lines: Vec<&TranscriptLine> = steps
        .iter()
        .filter(|l| l.event_type.as_deref() == Some(EVENT_TYPE_STEP))
        .collect();

    let failed: Vec<&&TranscriptLine> = step_lines
        .iter()
        .filter(|s| s.value["success"] == false)
        .collect();

    let mut out = String::new();
    let _ = writeln!(out, "FAILURE: {scenario_name}");
    let _ = writeln!(
        out,
        "  Steps: {} total, {} failed",
        step_lines.len(),
        failed.len()
    );
    let _ = writeln!(out, "  Exit: {exit_status}");

    for f in &failed {
        let label = f.value["label"].as_str().unwrap_or("?");
        let action = f.value["action"].as_str().unwrap_or("?");
        let expected = f.value["expected"].as_str().unwrap_or("?");
        let elapsed = f.value["elapsed_ms"].as_u64().unwrap_or(0);
        let _ = writeln!(
            out,
            "  FAILED step '{label}': {action} (expected: '{expected}', took {elapsed}ms)"
        );
    }

    out
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Compare key fields of two `step_result` JSON objects.
fn compare_step_fields(
    index: usize,
    expected: &serde_json::Value,
    actual: &serde_json::Value,
    diffs: &mut Vec<StepDiff>,
) {
    let label = expected["label"].as_str().unwrap_or("?").to_string();
    let step_ref = format!("step[{index}]/{label}");

    // Compare key fields that should match
    for field in &["label", "action", "expected", "success"] {
        let exp_val = &expected[*field];
        let act_val = &actual[*field];
        if exp_val != act_val {
            diffs.push(StepDiff {
                step_ref: step_ref.clone(),
                field: (*field).to_string(),
                expected: format_value(exp_val),
                actual: format_value(act_val),
                severity: DiffSeverity::Mismatch,
            });
        }
    }
}

fn format_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "(null)".to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_step(label: &str, success: bool) -> String {
        serde_json::json!({
            "type": "step_result",
            "correlation_id": format!("run-1/{}", label),
            "label": label,
            "action": format!("send_text: {label}"),
            "expected": "output",
            "success": success,
            "elapsed_ms": 100,
            "pane_snapshot_lines": 24,
        })
        .to_string()
    }

    fn make_header() -> String {
        serde_json::json!({
            "type": "scenario_header",
            "scenario_name": "test",
            "run_id": "run-1",
            "step_count": 2,
        })
        .to_string()
    }

    #[test]
    fn identical_transcripts_produce_no_diffs() {
        let content = format!(
            "{}\n{}\n{}",
            make_header(),
            make_step("a", true),
            make_step("b", true)
        );
        let expected = parse_transcript(&content);
        let actual = parse_transcript(&content);
        let diff = TranscriptDiff::compare(&expected, &actual);
        assert!(!diff.has_differences());
        assert!(diff.human_summary().contains("no differences"));
    }

    #[test]
    fn success_mismatch_detected() {
        let expected_content = format!("{}\n{}", make_header(), make_step("a", true));
        let actual_content = format!("{}\n{}", make_header(), make_step("a", false));
        let expected = parse_transcript(&expected_content);
        let actual = parse_transcript(&actual_content);
        let diff = TranscriptDiff::compare(&expected, &actual);
        assert!(diff.has_differences());
        assert_eq!(diff.diffs.len(), 1);
        assert_eq!(diff.diffs[0].field, "success");
        assert_eq!(diff.diffs[0].severity, DiffSeverity::Mismatch);
    }

    #[test]
    fn missing_step_detected() {
        let expected_content = format!(
            "{}\n{}\n{}",
            make_header(),
            make_step("a", true),
            make_step("b", true)
        );
        let actual_content = format!("{}\n{}", make_header(), make_step("a", true));
        let expected = parse_transcript(&expected_content);
        let actual = parse_transcript(&actual_content);
        let diff = TranscriptDiff::compare(&expected, &actual);
        assert!(diff.has_differences());
        assert_eq!(diff.diffs[0].severity, DiffSeverity::Missing);
        assert!(diff.diffs[0].step_ref.contains('b'));
    }

    #[test]
    fn extra_step_detected() {
        let expected_content = format!("{}\n{}", make_header(), make_step("a", true));
        let actual_content = format!(
            "{}\n{}\n{}",
            make_header(),
            make_step("a", true),
            make_step("extra", true)
        );
        let expected = parse_transcript(&expected_content);
        let actual = parse_transcript(&actual_content);
        let diff = TranscriptDiff::compare(&expected, &actual);
        assert!(diff.has_differences());
        assert_eq!(diff.diffs[0].severity, DiffSeverity::Extra);
    }

    #[test]
    fn label_mismatch_detected() {
        let exp_step = serde_json::json!({
            "type": "step_result",
            "label": "original",
            "action": "wait",
            "expected": "out",
            "success": true,
        })
        .to_string();
        let act_step = serde_json::json!({
            "type": "step_result",
            "label": "changed",
            "action": "wait",
            "expected": "out",
            "success": true,
        })
        .to_string();

        let expected = parse_transcript(&exp_step);
        let actual = parse_transcript(&act_step);
        let diff = TranscriptDiff::compare(&expected, &actual);
        assert!(diff.has_differences());
        assert_eq!(diff.diffs[0].field, "label");
    }

    #[test]
    fn diff_jsonl_roundtrip() {
        let expected_content = format!("{}\n{}", make_header(), make_step("a", true));
        let actual_content = format!("{}\n{}", make_header(), make_step("a", false));
        let expected = parse_transcript(&expected_content);
        let actual = parse_transcript(&actual_content);
        let diff = TranscriptDiff::compare(&expected, &actual);

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("diff.jsonl");
        diff.write_jsonl(&path).expect("write");

        let content = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2); // header + 1 diff

        let header: serde_json::Value = serde_json::from_str(lines[0]).expect("parse");
        assert_eq!(header["type"], "transcript_diff_header");
        assert_eq!(header["schema"], TRANSCRIPT_SCHEMA);
        assert_eq!(header["diff_count"], 1);
    }

    #[test]
    fn human_summary_includes_all_diffs() {
        let expected_content = format!(
            "{}\n{}\n{}",
            make_header(),
            make_step("a", true),
            make_step("b", true)
        );
        let actual_content = format!(
            "{}\n{}\n{}",
            make_header(),
            make_step("a", false),
            make_step("b", false)
        );
        let expected = parse_transcript(&expected_content);
        let actual = parse_transcript(&actual_content);
        let diff = TranscriptDiff::compare(&expected, &actual);

        let summary = diff.human_summary();
        assert!(summary.contains("2 difference(s)"));
        assert!(summary.contains("step[0]/a"));
        assert!(summary.contains("step[1]/b"));
        assert!(summary.contains("MISMATCH"));
    }

    #[test]
    fn failure_summary_format() {
        let content = format!(
            "{}\n{}\n{}",
            make_header(),
            make_step("setup", true),
            make_step("chat", false)
        );
        let lines = parse_transcript(&content);
        let summary = failure_summary("test_scenario", &lines, "Timeout");

        assert!(summary.contains("FAILURE: test_scenario"));
        assert!(summary.contains("2 total"));
        assert!(summary.contains("1 failed"));
        assert!(summary.contains("Exit: Timeout"));
        assert!(summary.contains("FAILED step 'chat'"));
    }

    #[test]
    fn parse_transcript_skips_invalid_json() {
        let content = "not json\n{\"type\":\"step_result\"}\nalso not json";
        let lines = parse_transcript(content);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].event_type.as_deref(), Some("step_result"));
    }

    #[test]
    fn schema_version_is_stable() {
        assert_eq!(TRANSCRIPT_SCHEMA, "pi.test.transcript.v1");
        assert_eq!(EVENT_TYPE_HEADER, "scenario_header");
        assert_eq!(EVENT_TYPE_STEP, "step_result");
        assert_eq!(EVENT_TYPE_BOUNDARY, "event_boundary");
        assert_eq!(EVENT_TYPE_ARTIFACT, "artifact");
    }
}
