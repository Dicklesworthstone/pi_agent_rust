//! E2E: structural codemod of a fixture crate via `ast_edit`, proven by
//! `cargo check` on the rewritten crate (bd-cv653.1.3).
//!
//! Flow: build a real (dependency-free) fixture crate in a temp dir, rename a
//! call pattern with a staged `ast_edit` proposal, apply it with a one-line
//! reason, verify the rewrite with `ast_grep`, then run `cargo check` on the
//! rewritten crate to prove it still compiles.
//!
//! No mocks, no network: the fixture crate has zero dependencies and cargo is
//! invoked with `--offline`. Structured JSONL logs are emitted per the repo
//! harness pattern (tests/common/logging.rs) with per-case events, tool
//! inputs, decisions, and artifact paths.

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::model::ContentBlock;
use pi::tools::{ToolOutput, ToolRegistry};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

const FIXTURE_MANIFEST: &str = r#"
[package]
name = "ast_codemod_fixture"
version = "0.1.0"
edition = "2021"
"#;

const FIXTURE_LIB: &str = r#"
#![allow(dead_code)]

fn old_parse(input: &str) -> i64 {
    input.parse().unwrap()
}

fn new_parse(input: &str) -> i64 {
    input.parse().unwrap()
}

pub fn compute() -> i64 {
    old_parse("1") + old_parse("2") + new_parse("3")
}
"#;

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

fn output_json(output: &ToolOutput) -> Value {
    serde_json::from_str(first_text(output)).expect("tool output must be a JSON payload")
}

fn cargo_available() -> bool {
    Command::new("cargo")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn write_fixture_crate(root: &Path) {
    std::fs::create_dir_all(root.join("src")).expect("create fixture src dir");
    std::fs::write(root.join("Cargo.toml"), FIXTURE_MANIFEST).expect("write fixture manifest");
    std::fs::write(root.join("src/lib.rs"), FIXTURE_LIB).expect("write fixture lib");
}

#[test]
fn e2e_ast_edit_codemod_fixture_crate_still_compiles() {
    asupersync::test_utils::run_test(|| async {
        let harness = TestHarness::new("e2e_ast_edit_codemod_fixture_crate_still_compiles");
        let log = harness.log();

        if !cargo_available() {
            log.warn("skip", "cargo not available on this host; skipping e2e codemod proof");
            let path = harness.temp_path("e2e_ast_tools.jsonl");
            harness.write_jsonl_logs(&path).expect("write JSONL logs");
            harness.record_artifact("e2e_ast_tools.jsonl", &path);
            return;
        }

        let crate_root = harness.temp_path("fixture_crate");
        write_fixture_crate(&crate_root);
        log.info_ctx("setup", "fixture crate created", |ctx| {
            ctx.push(("path".into(), crate_root.display().to_string()));
        });

        let registry = ToolRegistry::new(&["ast_grep", "ast_edit"], &crate_root, None);
        let edit = registry.get("ast_edit").expect("ast_edit registered");
        let grep = registry.get("ast_grep").expect("ast_grep registered");

        // Stage the codemod: rename old_parse(...) call sites to new_parse(...).
        let stage_input = json!({
            "ops": [{"pat": "old_parse($$$ARGS)", "out": "new_parse($$$ARGS)"}],
            "path": "src",
        });
        log.info_ctx("action", "ast_edit stage codemod", |ctx| {
            ctx.push(("input".into(), stage_input.to_string()));
        });
        let staged = edit
            .execute("e2e-stage", stage_input, None)
            .await
            .expect("stage must succeed");
        let staged_json = output_json(&staged);
        assert_eq!(staged_json["staged"], json!(true));
        assert_eq!(
            staged_json["replacements"],
            json!(2),
            "two call sites must be staged (the fn item must not match):\n{staged_json}"
        );
        let proposal_id = staged_json["proposalId"].as_str().unwrap().to_string();
        // Staging writes nothing.
        assert!(std::fs::read_to_string(crate_root.join("src/lib.rs"))
            .expect("read lib")
            .contains("old_parse(\"1\")"));

        // Resolve with a one-line reason.
        let resolve_input = json!({
            "action": "resolve",
            "proposalId": proposal_id,
            "reason": "rename old_parse call sites to new_parse",
        });
        log.info_ctx("action", "ast_edit resolve codemod", |ctx| {
            ctx.push(("proposalId".into(), proposal_id.clone()));
        });
        let resolved = edit
            .execute("e2e-resolve", resolve_input, None)
            .await
            .expect("resolve must succeed");
        let resolved_json = output_json(&resolved);
        assert_eq!(resolved_json["applied"], json!(true));
        assert_eq!(resolved_json["filesWritten"], json!(1));

        // Structural verification with ast_grep: 3 new_parse calls, 0 old_parse calls.
        let verify = grep
            .execute(
                "e2e-grep-new",
                json!({"pattern": "new_parse($$$ARGS)", "path": "src"}),
                None,
            )
            .await
            .expect("ast_grep must succeed");
        assert_eq!(
            output_json(&verify)["matchCount"],
            json!(3),
            "2 rewritten + 1 pre-existing new_parse call"
        );
        let old = grep
            .execute(
                "e2e-grep-old",
                json!({"pattern": "old_parse($$$ARGS)", "path": "src"}),
                None,
            )
            .await
            .expect("ast_grep must succeed");
        assert_eq!(output_json(&old)["matchCount"], json!(0));
        log.info("verify", "codemod applied structurally (3 new_parse, 0 old_parse calls)");

        // Proof: the rewritten crate still compiles.
        let target_dir = harness.temp_path("cargo_target");
        let check = Command::new("cargo")
            .args(["check", "--offline", "--manifest-path"])
            .arg(crate_root.join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", &target_dir)
            .output()
            .expect("run cargo check");
        log.info_ctx("verify", "cargo check on rewritten crate", |ctx| {
            ctx.push(("exitCode".into(), format!("{:?}", check.status.code())));
        });
        let cargo_log = harness.temp_path("cargo_check.log");
        std::fs::write(
            &cargo_log,
            format!(
                "status: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                check.status.code(),
                String::from_utf8_lossy(&check.stdout),
                String::from_utf8_lossy(&check.stderr)
            ),
        )
        .expect("write cargo log");
        harness.record_artifact("cargo_check.log", &cargo_log);
        assert!(
            check.status.success(),
            "rewritten fixture crate must compile; see cargo_check.log artifact:\n{}",
            String::from_utf8_lossy(&check.stderr)
        );

        // Close out: JSONL logs validated and recorded.
        let log_path = harness.temp_path("e2e_ast_tools.jsonl");
        harness.write_jsonl_logs(&log_path).expect("write JSONL logs");
        let payload = std::fs::read_to_string(&log_path).expect("read JSONL logs");
        let errors = validate_jsonl_v2_only(&payload);
        assert!(errors.is_empty(), "JSONL schema violations: {errors:?}");
        harness.record_artifact("e2e_ast_tools.jsonl", &log_path);
    });
}
