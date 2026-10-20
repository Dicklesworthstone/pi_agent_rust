//! Integration tests for prioritized code review with ship verdict (`/review` and `pi review`) (bd-cv653.3.11).

use std::fs;
use std::process::Command;
use tempfile::tempdir;

use pi::commit_split::DiffParser;
use pi::review::{
    CodeReviewer, REVIEW_SCHEMA, ReviewDeduplicator, ReviewFinding, ReviewOptions, ReviewReport,
    ReviewRuleEngine, ReviewSeverity, ReviewVerdict,
};

fn git(repo: &std::path::Path, args: &[&str]) {
    let output = Command::new("git").args(args).current_dir(repo).output().expect("git command"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Native Git fixture setup must execute successfully.
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr)); // ubs:ignore[rust.panic.assert-macros] -- A failed fixture command invalidates every target assertion.
}

fn review_repository() -> tempfile::TempDir {
    let temp = tempdir().expect("review repository"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Each integration test requires an isolated repository.
    git(temp.path(), &["init"]);
    git(temp.path(), &["config", "user.name", "Review Test"]);
    git(temp.path(), &["config", "user.email", "review@example.invalid"]);
    git(temp.path(), &["config", "commit.gpgsign", "false"]);
    temp
}

fn files_in(hunks: &[pi::commit_split::DiffHunk]) -> std::collections::BTreeSet<&str> {
    hunks.iter().map(|hunk| hunk.file_path.as_str()).collect()
}

#[test]
fn review_git_targets_distinguish_index_commit_branch_and_untracked_changes() {
    let repo = review_repository();
    fs::write(repo.path().join("first.rs"), "base first\n").expect("first file"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Establish the first tracked target fixture.
    fs::write(repo.path().join("second.rs"), "base second\n").expect("second file"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Establish a distinct second tracked target fixture.
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "base"]);
    git(repo.path(), &["branch", "baseline"]);
    fs::write(repo.path().join("first.rs"), "committed first\n").expect("commit change"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The branch comparison needs this first committed change.
    git(repo.path(), &["add", "first.rs"]);
    git(repo.path(), &["commit", "-m", "first change"]);
    fs::write(repo.path().join("second.rs"), "committed second\n").expect("second change"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The selected last commit must change only the second file.
    git(repo.path(), &["add", "second.rs"]);
    git(repo.path(), &["commit", "-m", "second change"]);

    fs::write(repo.path().join("first.rs"), "staged first\n").expect("index change"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Create an index-only change distinct from committed history.
    git(repo.path(), &["add", "first.rs"]);
    fs::write(repo.path().join("second.rs"), "unstaged second\n").expect("working change"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Create the working-tree-only fixture change.
    fs::create_dir(repo.path().join("nested")).expect("subdirectory"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Root resolution must be exercised from an actual subdirectory.
    fs::write(repo.path().join("nested/space name.rs"), "untracked text\n").expect("new file"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- A real spaced untracked path exercises Git's header delimiter.

    let (_, all) = CodeReviewer::acquire_diff(&repo.path().join("nested"), None).expect("whole repository from subdirectory"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Valid uncommitted target acquisition must succeed from the nested directory.
    assert_eq!(files_in(&all), ["first.rs", "nested/space name.rs", "second.rs"].into_iter().collect()); // ubs:ignore[rust.panic.assert-macros] -- Uncommitted review must include index, working-tree, and untracked files.
    let (_, staged) = CodeReviewer::acquire_diff(repo.path(), Some("staged")).expect("staged"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The real index snapshot is the staged-target oracle.
    assert_eq!(files_in(&staged), ["first.rs"].into_iter().collect()); // ubs:ignore[rust.panic.assert-macros] -- Staged review must exclude working-tree-only and untracked changes.
    for target in ["commit:HEAD", "HEAD~1..HEAD"] {
        let (_, hunks) = CodeReviewer::acquire_diff(repo.path(), Some(target)).expect("commit selection"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Both explicit commit forms must resolve the valid fixture history.
        assert_eq!(files_in(&hunks), ["second.rs"].into_iter().collect()); // ubs:ignore[rust.panic.assert-macros] -- Commit selection must retain only the last commit's changed file.
        assert!(hunks.iter().all(|hunk| !hunk.content.contains("unstaged"))); // ubs:ignore[rust.panic.assert-macros] -- Commit snapshots must not incorporate later working-tree bytes.
    }
    let (_, branch) = CodeReviewer::acquire_diff(repo.path(), Some("branch:baseline")).expect("merge base selection"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The baseline fixture branch must have a resolvable merge base.
    assert_eq!(files_in(&branch), ["first.rs", "second.rs"].into_iter().collect()); // ubs:ignore[rust.panic.assert-macros] -- Branch review covers both commits since the merge base.
    assert!(branch.iter().all(|hunk| !hunk.content.contains("staged"))); // ubs:ignore[rust.panic.assert-macros] -- Branch review must use committed bytes only.
    eprintln!("{}", serde_json::json!({"schema":"pi.review.test.v1","case":"git_targets","outcome":"index, commit, merge base, subdirectory and spaced untracked path verified"}));
}

#[test]
fn review_reports_partial_coverage_and_refuses_binary_only_ship() {
    let repo = review_repository();
    fs::write(repo.path().join("blob.bin"), b"\0old binary\n").expect("binary base"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The fixture needs a genuine binary tracked file.
    fs::write(repo.path().join("source.rs"), "unchanged\n").expect("text base"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Seed text for the later mixed-coverage snapshot.
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "base"]);
    fs::write(repo.path().join("blob.bin"), b"\0new binary\n").expect("binary change"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- This snapshot must contain only an actual binary change.
    let error = CodeReviewer::review(repo.path(), &ReviewOptions::default()).expect_err("binary changes are not a completed text review"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Binary-only refusal is the required regression outcome.
    assert!(error.to_string().contains("PI_REVIEW_NO_TEXT")); // ubs:ignore[rust.panic.assert-macros] -- An uncovered change must not silently become a clean verdict.

    fs::write(repo.path().join("source.rs"), "changed text\n").expect("text change"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Add one reviewable file to the existing binary fixture.
    let report = CodeReviewer::review(repo.path(), &ReviewOptions::default()).expect("mixed review"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Mixed text and binary changes must produce an explicitly scoped report.
    assert_eq!(report.stats.files_analyzed, 1); // ubs:ignore[rust.panic.assert-macros] -- Only the text file is included in analyzed coverage.
    assert_eq!(report.skipped_changes.len(), 1); // ubs:ignore[rust.panic.assert-macros] -- The binary change must appear exactly once outside coverage.
    assert!(report.skipped_changes[0].contains("blob.bin")); // ubs:ignore[rust.panic.assert-macros,rust.panic.direct-indexing] -- The preceding length assertion guarantees the one skipped binary entry.
    assert!(report.summary.contains("outside review coverage")); // ubs:ignore[rust.panic.assert-macros] -- A partial review must explicitly qualify its verdict.
    assert!(report.format_markdown().contains("blob.bin")); // ubs:ignore[rust.panic.assert-macros] -- The rendered report must identify the skipped file.
    eprintln!("{}", serde_json::json!({"schema":"pi.review.test.v1","case":"review_coverage","outcome":"binary-only refused; mixed review labels skipped file"}));
}

#[test]
fn review_submodule_pointer_changes_are_explicitly_outside_coverage() {
    let repo = review_repository();
    fs::write(repo.path().join("source.rs"), "base text\n").expect("source"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- A real initial commit supplies valid gitlink object IDs.
    git(repo.path(), &["add", "source.rs"]);
    git(repo.path(), &["commit", "-m", "base"]);
    let revision = |name: &str| {
        let output = Command::new("git").args(["rev-parse", name]).current_dir(repo.path()).output().expect("revision"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Resolve only fixture revisions created by the test.
        assert!(output.status.success()); // ubs:ignore[rust.panic.assert-macros] -- A missing fixture commit invalidates the gitlink regression.
        String::from_utf8(output.stdout).expect("revision text").trim().to_string() // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect,rust.parsing.strict-utf8] -- Git object IDs are required ASCII fixture data.
    };
    // A real gitlink index entry requires a commit ID but no checked-out
    // submodule or network access. Reuse local commits for this fixture.
    let base = revision("HEAD");
    git(repo.path(), &["update-index", "--add", "--cacheinfo", &format!("160000,{base},deps/library")]);
    git(repo.path(), &["commit", "-m", "add gitlink"]);
    let next = revision("HEAD");
    git(repo.path(), &["update-index", "--cacheinfo", &format!("160000,{next},deps/library")]);
    git(repo.path(), &["config", "diff.submodule", "log"]);

    let options = ReviewOptions { target: Some("staged".to_string()), ..Default::default() };
    let error = CodeReviewer::review(repo.path(), &options).expect_err("a SHA-only change is not a completed code review"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Gitlink-only refusal is the required coverage outcome.
    assert!(error.to_string().contains("PI_REVIEW_NO_TEXT")); // ubs:ignore[rust.panic.assert-macros] -- Opaque commit IDs cannot produce a completed text verdict.

    fs::write(repo.path().join("source.rs"), "changed text\n").expect("text change"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Add reviewable text alongside the staged gitlink change.
    git(repo.path(), &["add", "source.rs"]);
    let report = CodeReviewer::review(repo.path(), &options).expect("mixed snapshot"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Mixed gitlink and text coverage must remain reviewable.
    assert_eq!(report.stats.files_analyzed, 1); // ubs:ignore[rust.panic.assert-macros] -- A gitlink must not count as an analyzed source file.
    assert_eq!(report.skipped_changes, vec!["deps/library"]); // ubs:ignore[rust.panic.assert-macros] -- The exact nested repository path must remain visible outside coverage.
    assert!(report.summary.contains("outside review coverage")); // ubs:ignore[rust.panic.assert-macros] -- The mixed verdict must declare its limited coverage.
    assert!(report.format_markdown().contains("deps/library")); // ubs:ignore[rust.panic.assert-macros] -- The displayed report must identify the skipped gitlink.
    eprintln!("{}", serde_json::json!({"schema":"pi.review.test.v1","case":"submodule_coverage","outcome":"gitlink-only refused; mixed review explicitly excludes gitlink"}));
}

#[test]
fn review_commit_target_includes_nonconflicting_merge_changes() {
    let repo = review_repository();
    fs::write(repo.path().join("base.rs"), "base\n").expect("base file"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Establish the shared ancestor for the merge fixture.
    git(repo.path(), &["add", "base.rs"]);
    git(repo.path(), &["commit", "-m", "base"]);
    git(repo.path(), &["branch", "review-mainline"]);
    git(repo.path(), &["checkout", "-b", "review-feature"]);
    fs::write(repo.path().join("feature.rs"), "introduced by feature\n").expect("feature file"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The feature branch must introduce a distinct real source file.
    git(repo.path(), &["add", "feature.rs"]);
    git(repo.path(), &["commit", "-m", "feature"]);
    git(repo.path(), &["checkout", "review-mainline"]);
    fs::write(repo.path().join("mainline.rs"), "already in first parent\n").expect("mainline file"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Divergence forces a nonconflicting merge commit.
    git(repo.path(), &["add", "mainline.rs"]);
    git(repo.path(), &["commit", "-m", "mainline"]);
    git(repo.path(), &["merge", "--no-ff", "review-feature", "-m", "merge feature"]);

    let (_, hunks) = CodeReviewer::acquire_diff(repo.path(), Some("commit:HEAD")).expect("merge commit review"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- A valid merge commit must expose its first-parent diff.
    assert_eq!(files_in(&hunks), ["feature.rs"].into_iter().collect()); // ubs:ignore[rust.panic.assert-macros] -- Merge review must retain the feature change without repeating first-parent changes.
    assert!(hunks.iter().any(|hunk| hunk.content.contains("introduced by feature"))); // ubs:ignore[rust.panic.assert-macros] -- The reviewed source bytes must include the introduced feature.
    eprintln!("{}", serde_json::json!({"schema":"pi.review.test.v1","case":"merge_commit_coverage","outcome":"nonconflicting merge changes retained relative to first parent"}));
}

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
        reviewer: "model:fixture/reviewer".to_string(),
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
        omitted_findings: 0,
        failing_findings: 1,
        skipped_changes: Vec::new(),
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
