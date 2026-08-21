//! Integration tests for dependency-ordered atomic commit splitting (`pi commit`) (bd-cv653.3.14).

use pi::commit_split::{
    CommitOptions, CommitPlanner, CommitUnit, ConflictScanner, DiffParser, FileCategory,
};

#[test]
fn test_commit_split_category_priority() {
    let files = vec![
        "Cargo.toml".to_string(),
        "docs/ARCHITECTURE.md".to_string(),
        "tests/integration_test.rs".to_string(),
        "src/lib.rs".to_string(),
    ];

    let options = CommitOptions {
        dry_run: true,
        include_lockfiles: false,
        all_untracked: false,
        bead_reference: Some("bd-cv653.3.14".to_string()),
        custom_prefix: None,
    };

    let plan = CommitPlanner::plan(&[], &files, &options);
    let Ok(plan) = plan else {
        assert!(false, "plan should succeed");
        return;
    };

    assert_eq!(plan.units.len(), 4);
    assert_eq!(plan.cycles_detected, 0);

    // Headline commit should be the source file
    let Some(headline_id) = plan.headline_unit_id else {
        assert!(false, "headline unit must be present");
        return;
    };
    assert!(headline_id.contains("lib"));
}

#[test]
fn test_commit_split_lockfile_exclusion() {
    let files = vec![
        "src/main.rs".to_string(),
        "Cargo.lock".to_string(),
        "package-lock.json".to_string(),
    ];

    // Case 1: excluded by default
    let opts_exclude = CommitOptions {
        dry_run: true,
        include_lockfiles: false,
        all_untracked: false,
        bead_reference: None,
        custom_prefix: None,
    };
    let plan1 = CommitPlanner::plan(&[], &files, &opts_exclude);
    let Ok(plan1) = plan1 else {
        return;
    };
    assert_eq!(plan1.total_files, 1);
    assert_eq!(plan1.units.len(), 1);
    assert_eq!(plan1.units[0].files[0], "src/main.rs");

    // Case 2: included when explicitly requested
    let opts_include = CommitOptions {
        dry_run: true,
        include_lockfiles: true,
        all_untracked: false,
        bead_reference: None,
        custom_prefix: None,
    };
    let plan2 = CommitPlanner::plan(&[], &files, &opts_include);
    let Ok(plan2) = plan2 else {
        return;
    };
    assert_eq!(plan2.total_files, 3);
}

#[test]
fn test_conflict_marker_rejection() {
    let clean_code = "fn valid() {}\n";
    assert!(ConflictScanner::check_content(clean_code, "test.rs").is_ok());

    let conflicted = "fn broken() {\n<<<<<<< HEAD\n    1\n=======\n    2\n>>>>>>> branch\n}\n";
    let res = ConflictScanner::check_content(conflicted, "test.rs");
    assert!(res.is_err());
    let Err(e) = res else {
        return;
    };
    assert!(e.to_string().contains("Unresolved merge conflict marker"));
}

#[test]
fn test_diff_parser_hunks_extraction() {
    let diff = r#"diff --git a/src/commit_split.rs b/src/commit_split.rs
index abc..def 100644
--- a/src/commit_split.rs
+++ b/src/commit_split.rs
@@ -10,3 +10,5 @@ pub struct Test {
+    pub name: String,
+    pub value: usize,
 }
"#;

    let hunks = DiffParser::parse_unified_diff(diff);
    let Ok(hunks) = hunks else {
        return;
    };
    assert_eq!(hunks.len(), 1);
    let Some(hunk) = hunks.first() else {
        return;
    };
    assert_eq!(hunk.file_path, "src/commit_split.rs");
    assert_eq!(hunk.old_start, 10);
    assert_eq!(hunk.new_lines, 5);
}

#[test]
fn test_conventional_message_formatting() {
    let unit = CommitUnit {
        id: "unit-core".to_string(),
        commit_type: "feat".to_string(),
        scope: "commit-split".to_string(),
        summary: "implement dependency-ordered commit planner".to_string(),
        files: vec!["src/commit_split.rs".to_string()],
        category: FileCategory::Source,
        score: 400,
        dependencies: Vec::new(),
        rationale: None,
    };

    let msg_with_bead = unit.formatted_message(Some("bd-cv653.3.14"));
    assert_eq!(
        msg_with_bead,
        "feat(commit-split): implement dependency-ordered commit planner (bd-cv653.3.14)"
    );

    let msg_without_bead = unit.formatted_message(None);
    assert_eq!(
        msg_without_bead,
        "feat(commit-split): implement dependency-ordered commit planner"
    );
}
