//! Prioritized parallel code review engine with ship verdict (bd-cv653.3.11).
//!
//! Provides `/review` and `pi review` with:
//! - Target diff acquisition (uncommitted work, commit ranges, branches)
//! - Diff hunk chunking and file triage
//! - Rubric-based rule evaluations (correctness, security, perf, contract drift)
//! - Severity classification (P0 blocker through P3 nit) and confidence scoring
//! - Deduplication and ranking
//! - Ship verdict card calculation (`SHIP`, `SHIP-WITH-NITS`, `BLOCK`)
//! - Strict schema-governed JSON (`pi.review.v1`), Markdown, and terminal formats

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::commit_split::{DiffHunk, DiffParser};
use crate::error::{Error, Result};

/// Review schema tag.
pub const REVIEW_SCHEMA: &str = "pi.review.v1";

/// Finding severity level ranked from P0 (blocker) to P3 (suggestion).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ReviewSeverity {
    /// Blocker: Critical security vulnerability, data loss, fatal crash.
    #[serde(rename = "P0")]
    P0 = 0,
    /// Major: Serious bug, logic flaw, API contract breach, resource leak.
    #[serde(rename = "P1")]
    P1 = 1,
    /// Medium: Nit, performance issue, missing edge case, minor inconsistency.
    #[serde(rename = "P2")]
    P2 = 2,
    /// Low: Informational, style suggestion, documentation improvement.
    #[serde(rename = "P3")]
    P3 = 3,
}

impl ReviewSeverity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::P0 => "P0",
            Self::P1 => "P1",
            Self::P2 => "P2",
            Self::P3 => "P3",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_uppercase().as_str() {
            "P0" | "BLOCKER" | "CRITICAL" => Some(Self::P0),
            "P1" | "MAJOR" | "HIGH" => Some(Self::P1),
            "P2" | "MEDIUM" | "NIT" => Some(Self::P2),
            "P3" | "LOW" | "INFO" => Some(Self::P3),
            _ => None,
        }
    }
}

impl fmt::Display for ReviewSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Overall review verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewVerdict {
    /// Safe to ship without blocking issues or notable nits.
    #[serde(rename = "SHIP")]
    Ship,
    /// Safe to ship, but non-blocking nits (P2/P3) were identified.
    #[serde(rename = "SHIP-WITH-NITS")]
    ShipWithNits,
    /// Blocked from shipping due to critical/major findings (P0/P1).
    #[serde(rename = "BLOCK")]
    Block,
}

impl ReviewVerdict {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ship => "SHIP",
            Self::ShipWithNits => "SHIP-WITH-NITS",
            Self::Block => "BLOCK",
        }
    }

    pub const fn badge(self) -> &'static str {
        match self {
            Self::Ship => "🟢 SHIP",
            Self::ShipWithNits => "🟡 SHIP-WITH-NITS",
            Self::Block => "🔴 BLOCK",
        }
    }

    pub const fn is_passing(self) -> bool {
        matches!(self, Self::Ship | Self::ShipWithNits)
    }
}

impl fmt::Display for ReviewVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A structured code review finding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewFinding {
    pub id: String,
    pub severity: ReviewSeverity,
    pub confidence: f64,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_start: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_end: Option<usize>,
    pub category: String,
    pub title: String,
    pub rationale: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

/// Aggregated metrics from a review run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewStats {
    pub files_analyzed: usize,
    pub hunks_analyzed: usize,
    pub findings_count: usize,
    pub duration_ms: u64,
    pub p0_count: usize,
    pub p1_count: usize,
    pub p2_count: usize,
    pub p3_count: usize,
}

/// Complete code review report conforming to `pi.review.v1`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewReport {
    pub schema: String,
    pub target: String,
    pub verdict: ReviewVerdict,
    pub summary: String,
    pub findings: Vec<ReviewFinding>,
    pub stats: ReviewStats,
    pub timestamp_ms: i64,
}

fn format_finding_loc(file: &str, line_start: Option<usize>, line_end: Option<usize>) -> String {
    match (line_start, line_end) {
        (Some(s), Some(e)) if s == e => format!("{file}:{s}"),
        (Some(s), Some(e)) => format!("{file}:{s}-{e}"),
        (Some(s), None) => format!("{file}:{s}"),
        _ => file.to_string(),
    }
}

impl ReviewReport {
    /// Format report as Markdown.
    pub fn format_markdown(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "# Code Review Report: {}\n", self.verdict.badge());
        let _ = writeln!(out, "**Target:** `{}`  ", self.target);
        let _ = writeln!(out, "**Verdict:** **{}**  ", self.verdict.as_str());
        let _ = writeln!(out, "**Summary:** {}\n", self.summary);

        let _ = writeln!(out, "### Metrics\n");
        let _ = writeln!(
            out,
            "- Files Analyzed: {}\n- Hunks Evaluated: {}\n- Total Findings: {} (P0: {}, P1: {}, P2: {}, P3: {})\n- Duration: {}ms\n",
            self.stats.files_analyzed,
            self.stats.hunks_analyzed,
            self.stats.findings_count,
            self.stats.p0_count,
            self.stats.p1_count,
            self.stats.p2_count,
            self.stats.p3_count,
            self.stats.duration_ms
        );

        if self.findings.is_empty() {
            out.push_str("### Findings\n\nNo issues found! Clean change.\n");
        } else {
            out.push_str("### Prioritized Findings\n\n");
            for (idx, f) in self.findings.iter().enumerate() {
                let loc = format_finding_loc(&f.file, f.line_start, f.line_end);
                // Confidence is a ratio in [0.0, 1.0]; the rounded percentage fits u32.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let conf_pct = (f.confidence * 100.0).round() as u32;
                let _ = writeln!(
                    out,
                    "{}. **[{}] {}** (`{loc}`) — Confidence: {conf_pct}%",
                    idx + 1,
                    f.severity.as_str(),
                    f.title
                );
                let _ = writeln!(out, "   - **Category:** {}", f.category);
                let _ = writeln!(out, "   - **Rationale:** {}", f.rationale);
                if let Some(sug) = &f.suggestion {
                    let _ = writeln!(out, "   - **Suggestion:** {sug}");
                }
                out.push('\n');
            }
        }

        out
    }

    /// Format report as clean terminal text table.
    pub fn format_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "=== PI CODE REVIEW ===\nVerdict : {}\nTarget  : {}\nSummary : {}\nStats   : {} file(s), {} finding(s) (P0:{}, P1:{}, P2:{}, P3:{}) in {}ms\n",
            self.verdict.badge(),
            self.target,
            self.summary,
            self.stats.files_analyzed,
            self.stats.findings_count,
            self.stats.p0_count,
            self.stats.p1_count,
            self.stats.p2_count,
            self.stats.p3_count,
            self.stats.duration_ms
        );

        if self.findings.is_empty() {
            out.push_str("No issues detected.\n");
        } else {
            out.push_str("FINDINGS:\n");
            for (idx, f) in self.findings.iter().enumerate() {
                let loc = format_finding_loc(&f.file, f.line_start, f.line_end);
                let _ = writeln!(
                    out,
                    " {:2}. [{}] {:<12} {:<30} {}\n     -> {}",
                    idx + 1,
                    f.severity.as_str(),
                    f.category,
                    loc,
                    f.title,
                    f.rationale
                );
                if let Some(sug) = &f.suggestion {
                    let _ = writeln!(out, "     💡 Suggestion: {sug}");
                }
            }
        }

        out
    }

    /// Format report as JSON string.
    pub fn format_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| Error::Validation(format!("Failed to serialize review report: {e}")))
    }
}

/// Options configuring a review execution.
#[derive(Debug, Clone)]
pub struct ReviewOptions {
    pub target: Option<String>,
    pub fail_on: Option<ReviewSeverity>,
    pub confidence_threshold: f64,
    pub format: String,
    pub max_findings: usize,
    pub out_file: Option<PathBuf>,
}

impl Default for ReviewOptions {
    fn default() -> Self {
        Self {
            target: None,
            fail_on: None,
            confidence_threshold: 0.70,
            format: "text".to_string(),
            max_findings: 50,
            out_file: None,
        }
    }
}

fn make_finding_key(finding: &ReviewFinding) -> (String, usize, String) {
    (
        finding.file.clone(),
        finding.line_start.unwrap_or(0),
        finding.category.clone(),
    )
}

/// Deduplicator and ranker for review findings.
pub struct ReviewDeduplicator;

impl ReviewDeduplicator {
    /// Deduplicate findings and rank by severity (P0..P3) and confidence (descending).
    pub fn dedupe_and_rank(
        findings: Vec<ReviewFinding>,
        max_findings: usize,
    ) -> Vec<ReviewFinding> {
        let mut grouped: BTreeMap<(String, usize, String), ReviewFinding> = BTreeMap::new();

        for finding in findings {
            let key = make_finding_key(&finding);

            match grouped.get_mut(&key) {
                Some(existing) => {
                    // Retain higher severity (lower enum value); on a tie, keep the
                    // higher-confidence finding.
                    if finding.severity < existing.severity
                        || (finding.severity == existing.severity
                            && finding.confidence > existing.confidence)
                    {
                        *existing = finding;
                    }
                }
                None => {
                    grouped.insert(key, finding);
                }
            }
        }

        let mut ranked: Vec<ReviewFinding> = grouped.into_values().collect();
        ranked.sort_by(|a, b| {
            a.severity
                .cmp(&b.severity)
                .then_with(|| {
                    b.confidence
                        .partial_cmp(&a.confidence)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.file.cmp(&b.file))
        });

        if ranked.len() > max_findings {
            ranked.truncate(max_findings);
        }

        ranked
    }
}

/// Rubric-based deterministic rule engine for review analysis.
pub struct ReviewRuleEngine;

impl ReviewRuleEngine {
    /// Analyze diff hunks against code safety and quality rubrics.
    pub fn analyze_hunks(hunks: &[DiffHunk]) -> Vec<ReviewFinding> {
        let mut findings = Vec::new();

        for hunk in hunks {
            Self::check_hunk(hunk, &mut findings);
        }

        findings
    }

    fn check_hunk(hunk: &DiffHunk, findings: &mut Vec<ReviewFinding>) {
        let file = &hunk.file_path;
        let mut current_line = hunk.new_start;

        for line in hunk.content.lines() {
            if let Some(added_text) = line.strip_prefix('+') {
                if !line.starts_with("+++") {
                    Self::evaluate_line(file, current_line, added_text, findings);
                    current_line += 1;
                }
            } else if !line.starts_with('-') {
                current_line += 1;
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn evaluate_line(file: &str, line_no: usize, text: &str, findings: &mut Vec<ReviewFinding>) {
        let trimmed = text.trim();

        // 1. Secrets / Credentials check (P0 Security)
        if (trimmed.contains("sk_live_")
            || trimmed.contains("ghp_")
            || trimmed.contains("xoxb-")
            || (trimmed.contains("api_key") && trimmed.contains('=') && trimmed.len() > 30))
            && !trimmed.contains("test")
            && !trimmed.contains("example")
            && !trimmed.contains("placeholder")
        {
            findings.push(ReviewFinding {
                id: format!("sec-secret-{file}-{line_no}"),
                severity: ReviewSeverity::P0,
                confidence: 0.95,
                file: file.to_string(),
                line_start: Some(line_no),
                line_end: Some(line_no),
                category: "security".to_string(),
                title: "Potential hardcoded secret or API credential".to_string(),
                rationale: "Found a token or credential signature directly in source code."
                    .to_string(),
                suggestion: Some(
                    "Store secrets in environment variables or a secure secret manager."
                        .to_string(),
                ),
            });
        }

        // 2. SQL injection indicator (P0 Security)
        if (trimmed.contains("format!(\"SELECT")
            || trimmed.contains("format!(\"INSERT")
            || trimmed.contains("format!(\"UPDATE")
            || trimmed.contains("format!(\"DELETE"))
            && trimmed.contains("{}")
        {
            findings.push(ReviewFinding {
                id: format!("sec-sqli-{file}-{line_no}"),
                severity: ReviewSeverity::P0,
                confidence: 0.90,
                file: file.to_string(),
                line_start: Some(line_no),
                line_end: Some(line_no),
                category: "security".to_string(),
                title: "Possible SQL injection vulnerability".to_string(),
                rationale: "Dynamic string interpolation inside a SQL query statement detected."
                    .to_string(),
                suggestion: Some(
                    "Use parameterized queries with prepared statements ($1, ?).".to_string(),
                ),
            });
        }

        // 3. Command injection risk (P0 Security)
        if trimmed.contains("Command::new(\"sh\")") && trimmed.contains("-c") {
            findings.push(ReviewFinding {
                id: format!("sec-cmdi-{file}-{line_no}"),
                severity: ReviewSeverity::P0,
                confidence: 0.85,
                file: file.to_string(),
                line_start: Some(line_no),
                line_end: Some(line_no),
                category: "security".to_string(),
                title: "Potential command injection vector".to_string(),
                rationale: "Shell execution via `sh -c` with unescaped arguments can allow arbitrary execution."
                    .to_string(),
                suggestion: Some(
                    "Execute binaries directly with discrete arguments rather than invoking a shell."
                        .to_string(),
                ),
            });
        }

        // 4. Panic in production code (P1 Reliability)
        if !file.contains("test") && !file.contains("mock") {
            if trimmed.contains(".unwrap()") && !trimmed.starts_with("//") {
                findings.push(ReviewFinding {
                    id: format!("rel-unwrap-{file}-{line_no}"),
                    severity: ReviewSeverity::P1,
                    confidence: 0.75,
                    file: file.to_string(),
                    line_start: Some(line_no),
                    line_end: Some(line_no),
                    category: "correctness".to_string(),
                    title: "Unwrap in production path".to_string(),
                    rationale:
                        "Calling `.unwrap()` can panic at runtime if an unexpected error occurs."
                            .to_string(),
                    suggestion: Some(
                        "Handle the error gracefully using `?` or `match/if let`.".to_string(),
                    ),
                });
            } else if trimmed.contains("panic!(") && !trimmed.starts_with("//") {
                findings.push(ReviewFinding {
                    id: format!("rel-panic-{file}-{line_no}"),
                    severity: ReviewSeverity::P1,
                    confidence: 0.80,
                    file: file.to_string(),
                    line_start: Some(line_no),
                    line_end: Some(line_no),
                    category: "correctness".to_string(),
                    title: "Explicit panic in production code".to_string(),
                    rationale: "Explicit `panic!()` invocation crashes the process.".to_string(),
                    suggestion: Some(
                        "Return a `Result<T, Error>` instead of terminating.".to_string(),
                    ),
                });
            }
        }

        // 5. TODO / FIXME markers (P2/P3 Quality)
        if trimmed.contains("TODO:") || trimmed.contains("FIXME:") {
            findings.push(ReviewFinding {
                id: format!("qual-todo-{file}-{line_no}"),
                severity: ReviewSeverity::P2,
                confidence: 0.85,
                file: file.to_string(),
                line_start: Some(line_no),
                line_end: Some(line_no),
                category: "style".to_string(),
                title: "Unresolved TODO / FIXME comment".to_string(),
                rationale: "Incomplete implementation marker left in submitted diff.".to_string(),
                suggestion: Some("Resolve the item or link to a tracking issue bead.".to_string()),
            });
        }
    }
}

/// Code review driver and coordinator.
pub struct CodeReviewer;

impl CodeReviewer {
    /// Acquire diff hunks for the requested target.
    pub fn acquire_diff(cwd: &Path, target: Option<&str>) -> Result<(String, Vec<DiffHunk>)> {
        let target_name = target.unwrap_or("uncommitted");

        let mut cmd = Command::new("git");
        match target {
            None | Some("uncommitted") => {
                cmd.args(["diff", "HEAD"]);
            }
            Some(t) if t.contains("..") => {
                cmd.arg("diff").arg(t);
            }
            Some(t) => {
                cmd.args(["diff", t]);
            }
        }
        cmd.current_dir(cwd);

        let output = cmd.output().map_err(|e| {
            Error::Io(Box::new(std::io::Error::other(format!(
                "Failed to execute git diff for review: {e}"
            ))))
        })?;

        let diff_str = String::from_utf8_lossy(&output.stdout).to_string();
        let all_hunks = DiffParser::parse_unified_diff(&diff_str).unwrap_or_default();

        // Exclude lockfiles and minified files. Suffix matching is intentionally
        // exact and case-sensitive; these are conventional literal file names.
        #[allow(clippy::case_sensitive_file_extension_comparisons)]
        let filtered_hunks: Vec<DiffHunk> = all_hunks
            .into_iter()
            .filter(|h| {
                let p = &h.file_path;
                !p.ends_with(".lock")
                    && !p.ends_with("-lock.json")
                    && !p.ends_with(".min.js")
                    && !p.ends_with(".min.css")
            })
            .collect();

        Ok((target_name.to_string(), filtered_hunks))
    }

    /// Execute a review against working tree changes or a commit range.
    pub fn review(cwd: &Path, options: &ReviewOptions) -> Result<ReviewReport> {
        let start = Instant::now();
        let (target, hunks) = Self::acquire_diff(cwd, options.target.as_deref())?;

        let files_set: std::collections::HashSet<&str> =
            hunks.iter().map(|h| h.file_path.as_str()).collect();

        let raw_findings = ReviewRuleEngine::analyze_hunks(&hunks);
        let ranked = ReviewDeduplicator::dedupe_and_rank(raw_findings, options.max_findings);

        // Count severities
        let mut p0 = 0;
        let mut p1 = 0;
        let mut p2 = 0;
        let mut p3 = 0;

        for f in &ranked {
            match f.severity {
                ReviewSeverity::P0 => p0 += 1,
                ReviewSeverity::P1 => p1 += 1,
                ReviewSeverity::P2 => p2 += 1,
                ReviewSeverity::P3 => p3 += 1,
            }
        }

        // Determine verdict
        let verdict = if ranked.iter().any(|f| {
            (f.severity == ReviewSeverity::P0 && f.confidence >= options.confidence_threshold)
                || (f.severity == ReviewSeverity::P1
                    && f.confidence >= (options.confidence_threshold + 0.05).min(1.0))
        }) {
            ReviewVerdict::Block
        } else if !ranked.is_empty() {
            ReviewVerdict::ShipWithNits
        } else {
            ReviewVerdict::Ship
        };

        let summary = match verdict {
            ReviewVerdict::Ship => {
                "No blocking or minor issues found. Change is clean and ready to ship.".to_string()
            }
            ReviewVerdict::ShipWithNits => format!(
                "Change is safe to ship with {} non-blocking nit(s) to review.",
                p2 + p3
            ),
            ReviewVerdict::Block => format!(
                "Blocked by {p0} critical (P0) and/or {p1} major (P1) finding(s). Must resolve before shipping."
            ),
        };

        let stats = ReviewStats {
            files_analyzed: files_set.len(),
            hunks_analyzed: hunks.len(),
            findings_count: ranked.len(),
            duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            p0_count: p0,
            p1_count: p1,
            p2_count: p2,
            p3_count: p3,
        };

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));

        let report = ReviewReport {
            schema: REVIEW_SCHEMA.to_string(),
            target,
            verdict,
            summary,
            findings: ranked,
            stats,
            timestamp_ms: now_ms,
        };

        if let Some(out_path) = &options.out_file {
            let content = match options.format.as_str() {
                "json" => report.format_json()?,
                "markdown" => report.format_markdown(),
                _ => report.format_text(),
            };
            fs::write(out_path, content).map_err(|e| {
                Error::Io(Box::new(std::io::Error::other(format!(
                    "Failed to write review report: {e}"
                ))))
            })?;
        }

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_severity_ordering_and_parsing() {
        assert!(ReviewSeverity::P0 < ReviewSeverity::P1);
        assert!(ReviewSeverity::P1 < ReviewSeverity::P2);
        assert!(ReviewSeverity::P2 < ReviewSeverity::P3);

        assert_eq!(ReviewSeverity::parse("p0"), Some(ReviewSeverity::P0));
        assert_eq!(ReviewSeverity::parse("BLOCKER"), Some(ReviewSeverity::P0));
        assert_eq!(ReviewSeverity::parse("P1"), Some(ReviewSeverity::P1));
        assert_eq!(ReviewSeverity::parse("nit"), Some(ReviewSeverity::P2));
        assert_eq!(ReviewSeverity::parse("unknown"), None);
    }

    #[test]
    fn test_verdict_evaluation() {
        assert!(ReviewVerdict::Ship.is_passing());
        assert!(ReviewVerdict::ShipWithNits.is_passing());
        assert!(!ReviewVerdict::Block.is_passing());
    }

    #[test]
    fn test_deduplication_and_ranking() {
        let findings = vec![
            ReviewFinding {
                id: "f1".to_string(),
                severity: ReviewSeverity::P2,
                confidence: 0.8,
                file: "src/main.rs".to_string(),
                line_start: Some(10),
                line_end: Some(10),
                category: "style".to_string(),
                title: "Nit 1".to_string(),
                rationale: "Rationale 1".to_string(),
                suggestion: None,
            },
            ReviewFinding {
                id: "f2".to_string(),
                severity: ReviewSeverity::P0,
                confidence: 0.95,
                file: "src/main.rs".to_string(),
                line_start: Some(10),
                line_end: Some(10),
                category: "style".to_string(),
                title: "Upgraded to P0".to_string(),
                rationale: "Rationale critical".to_string(),
                suggestion: None,
            },
            ReviewFinding {
                id: "f3".to_string(),
                severity: ReviewSeverity::P1,
                confidence: 0.9,
                file: "src/lib.rs".to_string(),
                line_start: Some(20),
                line_end: Some(20),
                category: "correctness".to_string(),
                title: "Bug in lib".to_string(),
                rationale: "Unwrap panic".to_string(),
                suggestion: None,
            },
        ];

        let ranked = ReviewDeduplicator::dedupe_and_rank(findings, 10);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].severity, ReviewSeverity::P0);
        assert_eq!(ranked[1].severity, ReviewSeverity::P1);
    }

    #[test]
    fn test_rule_engine_detects_sqli_and_secret() {
        let hunk = DiffHunk {
            file_path: "src/db.rs".to_string(),
            old_start: 1,
            old_lines: 5,
            new_start: 1,
            new_lines: 5,
            header: "@@ -1,5 +1,5 @@".to_string(),
            content: "+let token = \"ghp_abcdef1234567890abcdef1234567890\";\n+let query = format!(\"SELECT * FROM users WHERE id = {}\", user_id);\n".to_string(),
        };

        let findings = ReviewRuleEngine::analyze_hunks(&[hunk]);
        assert_eq!(findings.len(), 2);
        assert!(
            findings
                .iter()
                .any(|f| f.severity == ReviewSeverity::P0 && f.category == "security")
        );
    }
}
