//! Unit-level integration tests for the `ast_grep` and `ast_edit` structural
//! tools (bd-cv653.1.3).
//!
//! Coverage: staging lifecycle, atomic rollback with an injected write failure
//! on file 2 of 3, stale-anchor rejection, metavariable identity, comment and
//! string exclusion (planted negative), malformed-pattern named errors, empty
//! `out` deletion, and `--tools` registry gating.
//!
//! Logging: every case emits structured JSONL per the repo harness pattern
//! (tests/common/logging.rs) — per-case events with a trace/correlation id,
//! tool inputs and decisions, and artifact paths. Logs are validated against
//! the v2 schema and recorded as artifacts.

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput, ToolRegistry};
use serde_json::{Value, json};
use std::path::Path;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the first text content from a `ToolOutput`.
fn first_text(output: &ToolOutput) -> &str {
    output
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

/// Parse the tool output text as JSON (both tools emit JSON payloads).
fn output_json(output: &ToolOutput) -> Value {
    serde_json::from_str(first_text(output)).expect("tool output must be a JSON payload")
}

fn write_file(harness: &TestHarness, relative: &str, content: &str) {
    let path = harness.temp_path(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dir");
    }
    std::fs::write(&path, content).expect("write fixture file");
}

fn read_file(harness: &TestHarness, relative: &str) -> String {
    std::fs::read_to_string(harness.temp_path(relative)).expect("read fixture file")
}

/// Emit closing JSONL logs, validate them against the v2 schema, and record
/// the log artifact path so failures are diagnosable from logs alone.
fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    harness
        .write_jsonl_logs(&path)
        .expect("write JSONL test logs");
    let payload = std::fs::read_to_string(&path).expect("read JSONL test logs");
    let errors = validate_jsonl_v2_only(&payload);
    assert!(
        errors.is_empty(),
        "JSONL schema violations in {case}.jsonl: {errors:?}"
    );
    harness.record_artifact(format!("{case}.jsonl"), &path);
}

async fn stage(harness: &TestHarness, tool: &dyn Tool, input: Value) -> ToolOutput {
    harness.log().with_context(
        common::logging::LogLevel::Info,
        "action",
        "ast_edit action=stage",
        |ctx| {
            ctx.push(("tool".into(), "ast_edit".into()));
            ctx.push(("input".into(), input.to_string()));
        },
    );
    tool.execute("test-stage", input, None)
        .await
        .expect("stage must succeed")
}

async fn resolve_raw(tool: &dyn Tool, input: Value) -> pi::error::Result<ToolOutput> {
    tool.execute("test-resolve", input, None).await
}

// ---------------------------------------------------------------------------
// Staging lifecycle
// ---------------------------------------------------------------------------

#[test]
fn ast_edit_staging_lifecycle_resolve_applies_atomically() {
    asupersync::test_utils::run_test(|| async {
        let harness = TestHarness::new("ast_edit_staging_lifecycle_resolve_applies_atomically");
        write_file(
            &harness,
            "src/alpha.rs",
            "pub fn a() -> i32 {\n    compute().unwrap()\n}\n",
        );
        write_file(
            &harness,
            "src/beta.rs",
            "pub fn b() -> i32 {\n    compute().unwrap() + compute().unwrap()\n}\n",
        );
        harness
            .log()
            .info("setup", "created two rust fixture files");

        let registry = ToolRegistry::new(&["ast_grep", "ast_edit"], harness.temp_dir(), None);
        let edit = registry.get("ast_edit").expect("ast_edit registered");

        let staged = stage(
            &harness,
            edit,
            json!({
                "ops": [{"pat": "$EXPR.unwrap()", "out": "$EXPR.expect(\"boom\")"}],
                "path": "src",
            }),
        )
        .await;
        let staged_json = output_json(&staged);
        let proposal_id = staged_json["proposalId"]
            .as_str()
            .expect("stage returns a proposalId")
            .to_string();
        assert_eq!(staged_json["staged"], json!(true));
        assert_eq!(staged_json["replacements"], json!(3));
        let staged_text = first_text(&staged);
        assert!(
            staged_text.contains("--- a/src/alpha.rs"),
            "diff preview must name alpha.rs:\n{staged_text}"
        );
        assert!(
            staged_text.contains("-    compute().unwrap()"),
            "diff preview must show removed line:\n{staged_text}"
        );
        assert!(
            staged_text.contains("+    compute().expect(\"boom\")"),
            "diff preview must show added line:\n{staged_text}"
        );
        // Staging must not write.
        assert!(read_file(&harness, "src/alpha.rs").contains("compute().unwrap()"));
        harness
            .log()
            .info_ctx("verify", "stage preview verified", |ctx| {
                ctx.push(("proposalId".into(), proposal_id.clone()));
            });

        let resolved = resolve_raw(
            edit,
            json!({
                "action": "resolve",
                "proposalId": proposal_id,
                "reason": "replace unwrap with expect for diagnostics",
            }),
        )
        .await
        .expect("resolve must succeed");
        let resolved_json = output_json(&resolved);
        assert_eq!(resolved_json["applied"], json!(true));
        assert_eq!(resolved_json["filesWritten"], json!(2));
        assert_eq!(
            resolved_json["reason"],
            json!("replace unwrap with expect for diagnostics")
        );
        assert_eq!(
            read_file(&harness, "src/alpha.rs"),
            "pub fn a() -> i32 {\n    compute().expect(\"boom\")\n}\n"
        );
        assert_eq!(
            read_file(&harness, "src/beta.rs"),
            "pub fn b() -> i32 {\n    compute().expect(\"boom\") + compute().expect(\"boom\")\n}\n"
        );

        // A resolved proposal is consumed.
        let second = resolve_raw(
            edit,
            json!({
                "action": "resolve",
                "proposalId": proposal_id,
                "reason": "duplicate apply must fail",
            }),
        )
        .await;
        let err = second.expect_err("re-resolving a consumed proposal must fail");
        assert!(
            err.to_string().contains("[AST_PROPOSAL_UNKNOWN]"),
            "expected AST_PROPOSAL_UNKNOWN, got: {err}"
        );
        harness
            .log()
            .info("verify", "proposal consumption enforced");
        finish_case(
            &harness,
            "ast_edit_staging_lifecycle_resolve_applies_atomically",
        );
    });
}

#[test]
fn ast_edit_reject_discards_with_zero_writes() {
    asupersync::test_utils::run_test(|| async {
        let harness = TestHarness::new("ast_edit_reject_discards_with_zero_writes");
        let original = "pub fn a() -> i32 {\n    compute().unwrap()\n}\n";
        write_file(&harness, "src/alpha.rs", original);

        let registry = ToolRegistry::new(&["ast_edit"], harness.temp_dir(), None);
        let edit = registry.get("ast_edit").expect("ast_edit registered");

        let staged = stage(
            &harness,
            edit,
            json!({
                "ops": [{"pat": "$EXPR.unwrap()", "out": "$EXPR.expect(\"boom\")"}],
                "path": "src",
            }),
        )
        .await;
        let proposal_id = output_json(&staged)["proposalId"]
            .as_str()
            .expect("proposalId")
            .to_string();

        harness.log().info("action", "ast_edit action=reject");
        let rejected = edit
            .execute(
                "test-reject",
                json!({"action": "reject", "proposalId": proposal_id}),
                None,
            )
            .await
            .expect("reject must succeed");
        assert_eq!(output_json(&rejected)["rejected"], json!(true));
        assert_eq!(
            read_file(&harness, "src/alpha.rs"),
            original,
            "reject must perform zero writes"
        );

        let after = resolve_raw(
            edit,
            json!({
                "action": "resolve",
                "proposalId": proposal_id,
                "reason": "must fail after reject",
            }),
        )
        .await;
        let err = after.expect_err("resolve after reject must fail");
        assert!(err.to_string().contains("[AST_PROPOSAL_UNKNOWN]"));
        assert_eq!(read_file(&harness, "src/alpha.rs"), original);
        finish_case(&harness, "ast_edit_reject_discards_with_zero_writes");
    });
}

// ---------------------------------------------------------------------------
// Atomic rollback with injected write failure on file 2 of 3
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn ast_edit_atomic_rollback_on_mid_apply_failure() {
    use std::os::unix::fs::PermissionsExt;

    asupersync::test_utils::run_test(|| async {
        let harness = TestHarness::new("ast_edit_atomic_rollback_on_mid_apply_failure");
        // Sorted scan order: a_first.rs, locked/b_second.rs, z_third.rs.
        write_file(&harness, "a_first.rs", "fn a() {\n    marker(1);\n}\n");
        write_file(
            &harness,
            "locked/b_second.rs",
            "fn b() {\n    marker(2);\n}\n",
        );
        write_file(&harness, "z_third.rs", "fn c() {\n    marker(3);\n}\n");
        harness.log().info(
            "setup",
            "created three rust files; middle one in a lockable dir",
        );

        let registry = ToolRegistry::new(&["ast_edit"], harness.temp_dir(), None);
        let edit = registry.get("ast_edit").expect("ast_edit registered");

        let staged = stage(
            &harness,
            edit,
            json!({
                "ops": [{"pat": "marker($$$ARGS)", "out": "renamed($$$ARGS)"}],
                "path": ".",
            }),
        )
        .await;
        let staged_json = output_json(&staged);
        assert_eq!(staged_json["replacements"], json!(3));
        let proposal_id = staged_json["proposalId"].as_str().unwrap().to_string();

        // Inject the failure: make the middle file's directory unwritable so
        // temp-file creation fails at apply time (file 2 of 3).
        let locked_dir = harness.temp_path("locked");
        let original_perms = std::fs::metadata(&locked_dir)
            .expect("stat locked dir")
            .permissions();
        let mut read_only = original_perms.clone();
        read_only.set_mode(0o555);
        std::fs::set_permissions(&locked_dir, read_only).expect("chmod locked dir");

        // Elevated-privilege environments (root) ignore mode bits; detect and
        // skip honestly instead of faking the failure.
        let probe = locked_dir.join("probe.tmp");
        let writable_anyway = std::fs::write(&probe, "probe").is_ok();
        if writable_anyway {
            let _ = std::fs::remove_file(&probe);
        }
        harness
            .log()
            .info_ctx("setup", "write failure injected via read-only dir", |ctx| {
                ctx.push(("dir".into(), locked_dir.display().to_string()));
                ctx.push(("effective".into(), (!writable_anyway).to_string()));
            });

        let result = resolve_raw(
            edit,
            json!({
                "action": "resolve",
                "proposalId": proposal_id,
                "reason": "rename marker call sites",
            }),
        )
        .await;

        // Restore writability for TempDir cleanup regardless of outcome.
        std::fs::set_permissions(&locked_dir, original_perms).expect("restore locked dir perms");

        if writable_anyway {
            harness.log().warn(
                "skip",
                "running with elevated privileges; permission injection ineffective",
            );
            finish_case(&harness, "ast_edit_atomic_rollback_on_mid_apply_failure");
            return;
        }

        let err = result.expect_err("apply must fail on the locked middle file");
        let message = err.to_string();
        assert!(
            message.contains("[AST_APPLY_FAILED]"),
            "expected AST_APPLY_FAILED, got: {message}"
        );
        assert!(
            message.contains("locked/b_second.rs"),
            "error must name the failed file, got: {message}"
        );
        assert!(
            message.contains("rolled back 1 previously-written file(s)"),
            "error must report rollback, got: {message}"
        );
        harness
            .log()
            .info_ctx("verify", "apply failed as injected", |ctx| {
                ctx.push(("error".into(), message));
            });

        // Atomicity: file 1 rolled back, file 2 untouched, file 3 never written.
        assert_eq!(
            read_file(&harness, "a_first.rs"),
            "fn a() {\n    marker(1);\n}\n",
            "first file must be rolled back to its original content"
        );
        assert_eq!(
            read_file(&harness, "locked/b_second.rs"),
            "fn b() {\n    marker(2);\n}\n"
        );
        assert_eq!(
            read_file(&harness, "z_third.rs"),
            "fn c() {\n    marker(3);\n}\n",
            "third file must never be written"
        );
        finish_case(&harness, "ast_edit_atomic_rollback_on_mid_apply_failure");
    });
}

// ---------------------------------------------------------------------------
// Stale-anchor rejection
// ---------------------------------------------------------------------------

#[test]
fn ast_edit_stale_file_rejects_whole_proposal_naming_file() {
    asupersync::test_utils::run_test(|| async {
        let harness = TestHarness::new("ast_edit_stale_file_rejects_whole_proposal_naming_file");
        write_file(&harness, "one.rs", "fn a() {\n    marker(1);\n}\n");
        write_file(&harness, "two.rs", "fn b() {\n    marker(2);\n}\n");

        let registry = ToolRegistry::new(&["ast_edit"], harness.temp_dir(), None);
        let edit = registry.get("ast_edit").expect("ast_edit registered");

        let staged = stage(
            &harness,
            edit,
            json!({
                "ops": [{"pat": "marker($$$ARGS)", "out": "renamed($$$ARGS)"}],
                "path": ".",
            }),
        )
        .await;
        let proposal_id = output_json(&staged)["proposalId"]
            .as_str()
            .unwrap()
            .to_string();

        // Modify a staged file between stage and apply.
        write_file(
            &harness,
            "two.rs",
            "fn b() {\n    marker(2); // edited externally\n}\n",
        );
        harness
            .log()
            .info("action", "modified two.rs between stage and resolve");

        let result = resolve_raw(
            edit,
            json!({
                "action": "resolve",
                "proposalId": proposal_id,
                "reason": "rename marker call sites",
            }),
        )
        .await;
        let err = result.expect_err("stale file must reject the whole proposal");
        let message = err.to_string();
        assert!(
            message.contains("[AST_PROPOSAL_STALE]"),
            "expected AST_PROPOSAL_STALE, got: {message}"
        );
        assert!(
            message.contains("two.rs"),
            "error must name the stale file, got: {message}"
        );

        // Whole-proposal rejection: the untouched file must not be written.
        assert_eq!(
            read_file(&harness, "one.rs"),
            "fn a() {\n    marker(1);\n}\n"
        );
        finish_case(
            &harness,
            "ast_edit_stale_file_rejects_whole_proposal_naming_file",
        );
    });
}

// ---------------------------------------------------------------------------
// Metavariable identity rule
// ---------------------------------------------------------------------------

#[test]
fn ast_grep_same_metavariable_twice_requires_identical_code() {
    asupersync::test_utils::run_test(|| async {
        let harness = TestHarness::new("ast_grep_same_metavariable_twice_requires_identical_code");
        write_file(
            &harness,
            "main.rs",
            "fn main() {\n    let _ = value == value;\n    let _ = left == right;\n}\n",
        );

        let registry = ToolRegistry::new(&["ast_grep"], harness.temp_dir(), None);
        let grep = registry.get("ast_grep").expect("ast_grep registered");
        harness
            .log()
            .info("action", "ast_grep pattern '$A == $A' (identity rule)");
        let output = grep
            .execute("test-id", json!({"pattern": "$A == $A", "path": "."}), None)
            .await
            .expect("ast_grep must succeed");
        let payload = output_json(&output);
        assert_eq!(
            payload["matchCount"],
            json!(1),
            "only the identical-code comparison may match:\n{payload}"
        );
        let matched = payload["matches"][0]["matched"].as_str().unwrap();
        assert_eq!(matched, "value == value");
        assert!(
            !first_text(&output).contains("left == right"),
            "$A == $A must not match left == right"
        );
        harness.log().info("verify", "identity rule held");
        finish_case(
            &harness,
            "ast_grep_same_metavariable_twice_requires_identical_code",
        );
    });
}

// ---------------------------------------------------------------------------
// Comment / string exclusion (planted negative)
// ---------------------------------------------------------------------------

#[test]
fn ast_grep_unwrap_pattern_ignores_comments_and_strings() {
    asupersync::test_utils::run_test(|| async {
        let harness = TestHarness::new("ast_grep_unwrap_pattern_ignores_comments_and_strings");
        write_file(
            &harness,
            "main.rs",
            concat!(
                "fn f() -> i32 {\n",
                "    let r = compute().unwrap();\n",
                "    // this comment mentions data.unwrap() but must never match\n",
                "    let _s = \"also not a match: thing.unwrap()\";\n",
                "    r\n",
                "}\n",
            ),
        );
        harness.log().info(
            "setup",
            "planted negative: comment and string literal contain unwrap()",
        );

        let registry = ToolRegistry::new(&["ast_grep"], harness.temp_dir(), None);
        let grep = registry.get("ast_grep").expect("ast_grep registered");
        let output = grep
            .execute(
                "test-id",
                json!({"pattern": "$EXPR.unwrap()", "path": "."}),
                None,
            )
            .await
            .expect("ast_grep must succeed");
        let payload = output_json(&output);
        assert_eq!(
            payload["matchCount"],
            json!(1),
            "only the real unwrap() call may match:\n{payload}"
        );
        let only = &payload["matches"][0];
        assert_eq!(only["matched"], json!("compute().unwrap()"));
        assert_eq!(only["startLine"], json!(2));
        let text = first_text(&output);
        assert!(
            !text.contains("data.unwrap()"),
            "comment must never match:\n{text}"
        );
        assert!(
            !text.contains("thing.unwrap()"),
            "string literal must never match:\n{text}"
        );
        harness
            .log()
            .info("verify", "planted negative passed: comment/string excluded");
        finish_case(
            &harness,
            "ast_grep_unwrap_pattern_ignores_comments_and_strings",
        );
    });
}

#[test]
fn ast_edit_pattern_with_matches_only_in_comments_stages_nothing() {
    asupersync::test_utils::run_test(|| async {
        let harness =
            TestHarness::new("ast_edit_pattern_with_matches_only_in_comments_stages_nothing");
        write_file(
            &harness,
            "main.rs",
            "fn f() {\n    // pretend we call compute().unwrap() here\n}\n",
        );

        let registry = ToolRegistry::new(&["ast_edit"], harness.temp_dir(), None);
        let edit = registry.get("ast_edit").expect("ast_edit registered");
        let staged = stage(
            &harness,
            edit,
            json!({
                "ops": [{"pat": "$EXPR.unwrap()", "out": "$EXPR.expect(\"boom\")"}],
                "path": ".",
            }),
        )
        .await;
        let payload = output_json(&staged);
        assert_eq!(payload["staged"], json!(false));
        assert_eq!(payload["replacements"], json!(0));
        assert!(read_file(&harness, "main.rs").contains("compute().unwrap()"));
        finish_case(
            &harness,
            "ast_edit_pattern_with_matches_only_in_comments_stages_nothing",
        );
    });
}

// ---------------------------------------------------------------------------
// Malformed patterns: named parse errors, never a partial apply
// ---------------------------------------------------------------------------

#[test]
fn ast_edit_malformed_patterns_return_named_parse_error() {
    asupersync::test_utils::run_test(|| async {
        let harness = TestHarness::new("ast_edit_malformed_patterns_return_named_parse_error");
        let original = "pub fn a() -> i32 {\n    compute().unwrap()\n}\n";
        write_file(&harness, "src/alpha.rs", original);

        let registry = ToolRegistry::new(&["ast_edit"], harness.temp_dir(), None);
        let edit = registry.get("ast_edit").expect("ast_edit registered");

        // Non-single-node pattern (two items).
        let bad_pat = edit
            .execute(
                "test-bad-pat",
                json!({
                    "ops": [{"pat": "fn a() {} fn b() {}", "out": "fn c() {}"}],
                    "path": "src",
                }),
                None,
            )
            .await;
        let err = bad_pat.expect_err("multi-node pattern must fail");
        assert!(
            err.to_string().contains("[AST_PATTERN_PARSE]"),
            "expected named parse error, got: {err}"
        );
        harness
            .log()
            .info_ctx("verify", "multi-node pat rejected", |ctx| {
                ctx.push(("error".into(), err.to_string()));
            });

        // Non-single-node replacement (two statements).
        let bad_out = edit
            .execute(
                "test-bad-out",
                json!({
                    "ops": [{"pat": "$EXPR.unwrap()", "out": "foo(); bar();"}],
                    "path": "src",
                }),
                None,
            )
            .await;
        let err = bad_out.expect_err("multi-node replacement must fail");
        assert!(
            err.to_string().contains("[AST_PATTERN_PARSE]"),
            "expected named parse error, got: {err}"
        );
        assert!(
            err.to_string().contains("single"),
            "error must state the single-node rule, got: {err}"
        );

        // Never a partial apply: the file is untouched and no proposal exists.
        assert_eq!(read_file(&harness, "src/alpha.rs"), original);
        finish_case(
            &harness,
            "ast_edit_malformed_patterns_return_named_parse_error",
        );
    });
}

// ---------------------------------------------------------------------------
// Empty out deletes the matched node
// ---------------------------------------------------------------------------

#[test]
fn ast_edit_empty_out_deletes_matched_node() {
    asupersync::test_utils::run_test(|| async {
        let harness = TestHarness::new("ast_edit_empty_out_deletes_matched_node");
        write_file(
            &harness,
            "app.js",
            "function main() {\n  console.log(\"debug\", 1);\n  run();\n}\n",
        );

        let registry = ToolRegistry::new(&["ast_edit"], harness.temp_dir(), None);
        let edit = registry.get("ast_edit").expect("ast_edit registered");
        let staged = stage(
            &harness,
            edit,
            json!({
                "ops": [{"pat": "console.log($$$ARGS)", "out": ""}],
                "path": ".",
            }),
        )
        .await;
        let payload = output_json(&staged);
        assert_eq!(payload["replacements"], json!(1));
        let proposal_id = payload["proposalId"].as_str().unwrap().to_string();

        let resolved = resolve_raw(
            edit,
            json!({
                "action": "resolve",
                "proposalId": proposal_id,
                "reason": "strip debug logging",
            }),
        )
        .await
        .expect("resolve must succeed");
        assert_eq!(output_json(&resolved)["applied"], json!(true));
        let content = read_file(&harness, "app.js");
        assert!(
            !content.contains("console.log"),
            "matched statement must be deleted:\n{content}"
        );
        assert!(content.contains("run();"));
        finish_case(&harness, "ast_edit_empty_out_deletes_matched_node");
    });
}

// ---------------------------------------------------------------------------
// Multi-language scan + explicit lang override
// ---------------------------------------------------------------------------

#[test]
fn ast_grep_scans_each_file_in_its_own_language() {
    asupersync::test_utils::run_test(|| async {
        let harness = TestHarness::new("ast_grep_scans_each_file_in_its_own_language");
        write_file(&harness, "code/a.rs", "fn a() {\n    helper(1);\n}\n");
        write_file(&harness, "code/b.py", "def b():\n    helper(2)\n");
        write_file(&harness, "code/c.js", "function c() {\n  helper(3);\n}\n");
        write_file(
            &harness,
            "code/d.ts",
            "function d(): void {\n  helper(4);\n}\n",
        );
        write_file(&harness, "code/e.sh", "#!/usr/bin/env bash\nhelper 5\n");
        write_file(
            &harness,
            "code/f.go",
            "package main\n\nfunc f() {\n\thelper(6)\n}\n",
        );

        let registry = ToolRegistry::new(&["ast_grep"], harness.temp_dir(), None);
        let grep = registry.get("ast_grep").expect("ast_grep registered");
        let output = grep
            .execute(
                "test-id",
                json!({"pattern": "helper($$$ARGS)", "path": "code"}),
                None,
            )
            .await
            .expect("ast_grep must succeed");
        let payload = output_json(&output);
        let files: Vec<&str> = payload["matches"]
            .as_array()
            .expect("matches array")
            .iter()
            .map(|m| m["file"].as_str().unwrap())
            .collect();
        harness
            .log()
            .info_ctx("verify", "multi-language matches", |ctx| {
                ctx.push(("files".into(), files.join(", ")));
            });
        for expected in [
            "code/a.rs",
            "code/b.py",
            "code/c.js",
            "code/d.ts",
            "code/f.go",
        ] {
            assert!(
                files.contains(&expected),
                "missing match in {expected}: {files:?}"
            );
        }
        // bash `helper 5` is a command, not a call expression — no match.
        assert!(
            !files.contains(&"code/e.sh"),
            "bash command must not match a call-expression pattern"
        );
        finish_case(&harness, "ast_grep_scans_each_file_in_its_own_language");
    });
}

// ---------------------------------------------------------------------------
// Registry gating behind --tools
// ---------------------------------------------------------------------------

#[test]
fn ast_tools_are_gated_behind_tools_flag() {
    let harness = TestHarness::new("ast_tools_are_gated_behind_tools_flag");
    let cwd = harness.temp_dir();

    let enabled = ToolRegistry::new(&["read", "ast_grep", "ast_edit"], cwd, None);
    assert!(enabled.get("ast_grep").is_some());
    assert!(enabled.get("ast_edit").is_some());

    let not_enabled = ToolRegistry::new(&["read", "grep"], cwd, None);
    assert!(not_enabled.get("ast_grep").is_none());
    assert!(not_enabled.get("ast_edit").is_none());

    // The default built-in set does not silently enable the structural tools.
    let defaults = pi::cli::parse_with_extension_flags(vec!["pi".to_string()])
        .expect("default CLI parse")
        .cli;
    let names = defaults.enabled_tools();
    assert!(!names.contains(&"ast_grep"));
    assert!(!names.contains(&"ast_edit"));

    let cli = pi::cli::parse_with_extension_flags(vec![
        "pi".to_string(),
        "--tools".to_string(),
        "read,ast_grep,ast_edit".to_string(),
    ])
    .expect("CLI parse with --tools")
    .cli;
    assert_eq!(cli.enabled_tools(), vec!["read", "ast_grep", "ast_edit"]);

    harness.log().info("verify", "--tools gating enforced");
    finish_case(&harness, "ast_tools_are_gated_behind_tools_flag");
}
