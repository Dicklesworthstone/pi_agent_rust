//! Integration tests for prioritized code review with ship verdict (`/review` and `pi review`) (bd-cv653.3.11).

use std::fs;
use std::process::Command;
use tempfile::tempdir;

use pi::commit_split::DiffParser;
use pi::review::{
    CodeReviewer, REVIEW_SCHEMA, ReviewDeduplicator, ReviewFinding, ReviewOptions, ReviewReport,
    ReviewRuleEngine, ReviewSeverity, ReviewVerdict,
};

#[test]
fn test_severity_levels_and_verdict_logic() {
    // P0 > P1 > P2 > P3
    assert!(ReviewSeverity::P0 < ReviewSeverity::P1);
    assert!(ReviewSeverity::P1 < ReviewSeverity::P2);
    assert!(ReviewSeverity::P2 < ReviewSeverity::P3);

    assert_eq!(ReviewSeverity::parse("p0"), Some(ReviewSeverity::P0));
    assert_eq!(ReviewSeverity::parse("P1"), Some(ReviewSeverity::P1));
    assert_eq!(ReviewSeverity::parse("p2"), Some(ReviewSeverity::P2));
    assert_eq!(ReviewSeverity::parse("p3"), Some(ReviewSeverity::P3));
    assert_eq!(ReviewSeverity::parse("invalid"), None);

    assert_eq!(ReviewVerdict::Ship.as_str(), "SHIP");
    assert_eq!(ReviewVerdict::ShipWithNits.as_str(), "SHIP-WITH-NITS");
    assert_eq!(ReviewVerdict::Block.as_str(), "BLOCK");

    assert!(ReviewVerdict::Ship.is_passing());
    assert!(ReviewVerdict::ShipWithNits.is_passing());
    assert!(!ReviewVerdict::Block.is_passing());
}

#[test]
fn test_rule_engine_findings_and_deduplication() {
    let diff_text = r#"
--- a/src/auth.rs
+++ b/src/auth.rs
@@ -10,4 +10,6 @@
+let api_key = "ghp_123456789012345678901234567890123456";
+let query = format!("SELECT * FROM secrets WHERE token = {}", api_key);
+let child = Command::new("sh").arg("-c").arg(user_input);
+let val = res.unwrap();
+let todo_item = 1; // TODO: implement later
"#;

    let hunks = DiffParser::parse_unified_diff(diff_text).unwrap_or_default();
    assert!(!hunks.is_empty());

    let findings = ReviewRuleEngine::analyze_hunks(&hunks);
    assert!(findings.len() >= 4);

    // Verify critical findings detected
    assert!(
        findings
            .iter()
            .any(|f| f.severity == ReviewSeverity::P0 && f.category == "security")
    );
    assert!(
        findings
            .iter()
            .any(|f| f.severity == ReviewSeverity::P1 && f.category == "correctness")
    );
    assert!(
        findings
            .iter()
            .any(|f| f.severity == ReviewSeverity::P2 && f.category == "style")
    );

    // Verify deduplication and ranking puts P0 first
    let ranked = ReviewDeduplicator::dedupe_and_rank(findings, 20);
    assert!(!ranked.is_empty());
    assert_eq!(ranked[0].severity, ReviewSeverity::P0);
}

#[test]
fn test_report_formatting_schema_and_json() {
    let report = ReviewReport {
        schema: REVIEW_SCHEMA.to_string(),
        target: "uncommitted".to_string(),
        verdict: ReviewVerdict::Block,
        summary: "Found 1 blocker and 1 nit".to_string(),
        findings: vec![
            ReviewFinding {
                id: "sec-1".to_string(),
                severity: ReviewSeverity::P0,
                confidence: 0.95,
                file: "src/db.rs".to_string(),
                line_start: Some(15),
                line_end: Some(15),
                category: "security".to_string(),
                title: "SQL injection".to_string(),
                rationale: "Unescaped format string in query".to_string(),
                suggestion: Some("Use prepared statement".to_string()),
            },
            ReviewFinding {
                id: "qual-1".to_string(),
                severity: ReviewSeverity::P2,
                confidence: 0.85,
                file: "src/db.rs".to_string(),
                line_start: Some(25),
                line_end: Some(25),
                category: "style".to_string(),
                title: "TODO comment".to_string(),
                rationale: "Unresolved item".to_string(),
                suggestion: None,
            },
        ],
        stats: pi::review::ReviewStats {
            files_analyzed: 1,
            hunks_analyzed: 1,
            findings_count: 2,
            duration_ms: 12,
            p0_count: 1,
            p1_count: 0,
            p2_count: 1,
            p3_count: 0,
        },
        timestamp_ms: 1_700_000_000_000,
    };

    let md = report.format_markdown();
    assert!(md.contains("Code Review Report: 🔴 BLOCK"));
    assert!(md.contains("SQL injection"));
    assert!(md.contains("Confidence: 95%"));

    let txt = report.format_text();
    assert!(txt.contains("=== PI CODE REVIEW ==="));
    assert!(txt.contains("🔴 BLOCK"));

    let json_str = report.format_json().expect("serialize json");
    assert!(json_str.contains("\"schema\": \"pi.review.v1\""));
    assert!(json_str.contains("\"verdict\": \"BLOCK\""));
}

#[test]
fn test_review_integration_on_scratch_git_repo() {
    let Ok(tmp) = tempdir() else {
        return;
    };
    let repo_dir = tmp.path();

    // Initialize test git repo
    let _ = Command::new("git")
        .arg("init")
        .current_dir(repo_dir)
        .output();
    let _ = Command::new("git")
        .args(["config", "user.name", "Tester"])
        .current_dir(repo_dir)
        .output();
    let _ = Command::new("git")
        .args(["config", "user.email", "tester@example.com"])
        .current_dir(repo_dir)
        .output();

    // Initial commit
    let src_file = repo_dir.join("src.rs");
    let _ = fs::write(&src_file, "fn main() { println!(\"hello\"); }\n");
    let _ = Command::new("git")
        .args(["add", "src.rs"])
        .current_dir(repo_dir)
        .output();
    let _ = Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(repo_dir)
        .output();

    // Clean review on unmodified repo
    let options = ReviewOptions::default();
    let report_clean = CodeReviewer::review(repo_dir, &options).expect("review clean");
    assert_eq!(report_clean.verdict, ReviewVerdict::Ship);
    assert_eq!(report_clean.findings.len(), 0);

    // Introduce a modification with a P0 security flaw
    let bad_code = r#"
fn query_user(id: &str) {
    let q = format!("SELECT * FROM users WHERE id = {}", id);
    println!("{}", q);
}
"#;
    let _ = fs::write(&src_file, bad_code);

    let report_bad = CodeReviewer::review(repo_dir, &options).expect("review bad");
    assert_eq!(report_bad.verdict, ReviewVerdict::Block);
    assert!(!report_bad.findings.is_empty());
    assert_eq!(report_bad.findings[0].severity, ReviewSeverity::P0);
}
