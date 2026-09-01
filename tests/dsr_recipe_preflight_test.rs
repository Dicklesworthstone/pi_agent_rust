//! DSR perf recipe preflight test (bd-ri-phase1-recipe-audit).
//!
//! Verifies that the preflight script in
//! `scripts/perf/preflight_dsr_recipe.sh`:
//! - exists and is executable
//! - produces a well-formed runpack JSON with the expected schema
//! - reports a verdict field
//! - lists per-contract findings
//! - has a non-empty findings array
//! - has the schema identifier `pi.evidence.ri_phase1_recipe_audit_runpack.v1`
//!
//! Run:
//! ```bash
//! cargo test --test dss_recipe_preflight -- --nocapture
//! ```
//! (note: name is `dsr_recipe_preflight` matching the file)
//!
//! The test does NOT execute the preflight (which depends on the live
//! DSR binary and rch state); it validates the script's structure and
//! the schema of the runpack artifact that a real run produces.

#![allow(clippy::doc_markdown, clippy::expect_used)]

use std::path::PathBuf;

fn project_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is set by cargo at test time
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn preflight_script_exists_and_is_executable() {
    let script = project_root().join("scripts/perf/preflight_dsr_recipe.sh");
    assert!(
        script.exists(),
        "preflight script missing: {}",
        script.display()
    );
    let metadata = std::fs::metadata(&script).expect("preflight metadata");
    let permissions = metadata.permissions();
    // Unix-only check; on Windows we just check existence
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = permissions.mode();
        assert!(
            mode & 0o111 != 0,
            "preflight script not executable: mode={mode:o}"
        );
    }
    let _ = permissions; // suppress unused warning on non-unix
}

#[test]
fn preflight_script_documents_required_findings() {
    let script =
        std::fs::read_to_string(project_root().join("scripts/perf/preflight_dsr_recipe.sh"))
            .expect("preflight script read");

    // The script MUST check for each of the load-bearing contracts
    // documented in docs/perf-budgets-recipe.md.
    let required_contracts = [
        "DSR_BINARY_PRESENT",
        "WORKDIR_IS_PI_AGENT_RUST",
        "AGENTS_DSR_GOVERNANCE_PRESENT",
        "DSR_REPOS_YAML_HAS_PI_AGENT_RUST",
        "DSR_DRY_RUN_PLANS_6_CHECKS",
        "RCH_ON_PATH",
        "RCHIGNORE_PRESENT",
        "PREFLIGHT_BUDGET_INPUTS_PRESENT",
        "CHECK_MODULE_REACHABILITY_PRESENT",
        "INSTALLER_REGRESSION_PRESENT",
        "EVIDENCE_SCHEMAS_PRESENT",
    ];
    for contract in required_contracts {
        assert!(
            script.contains(contract),
            "preflight script missing contract check: {contract}"
        );
    }
}

#[test]
fn preflight_runpack_schema_constant_matches_bead() {
    let script =
        std::fs::read_to_string(project_root().join("scripts/perf/preflight_dsr_recipe.sh"))
            .expect("preflight script read");
    assert!(
        script.contains("pi.evidence.ri_phase1_recipe_audit_runpack.v1"),
        "preflight script must declare the runpack schema constant"
    );
}

#[test]
fn preflight_recipe_doc_exists() {
    let doc = project_root().join("docs/perf-budgets-recipe.md");
    assert!(doc.exists(), "recipe doc missing: {}", doc.display());
    let body = std::fs::read_to_string(&doc).expect("recipe doc read");
    // Must document every hidden contract the preflight checks
    for required in [
        "DSR",
        "rch",
        "CARGO_TARGET_DIR",
        "evidence cache",
        "env_fingerprint",
        "closeout-evidence-registry",
        "phase1_matrix_validation",
    ] {
        assert!(
            body.to_lowercase().contains(&required.to_lowercase()),
            "recipe doc missing required content: {required}"
        );
    }
}

#[test]
fn preflight_runpack_artifact_when_present_validates() {
    // If a real run has been done, the runpack must parse and contain
    // the expected top-level fields. If not present, the test is a no-op.
    let runpack = project_root().join("docs/evidence/ri-phase1-recipe-audit-runpack.json");
    if !runpack.exists() {
        eprintln!("runpack not present yet; skipping JSON parse check");
        return;
    }
    let body = std::fs::read_to_string(&runpack).expect("runpack read");
    let json: serde_json::Value = serde_json::from_str(&body).expect("runpack parseable JSON");
    assert_eq!(
        json["schema"].as_str(),
        Some("pi.evidence.ri_phase1_recipe_audit_runpack.v1"),
        "runpack schema mismatch"
    );
    for field in [
        "verdict",
        "ok_count",
        "fail_count",
        "warn_count",
        "findings",
    ] {
        assert!(
            json.get(field).is_some(),
            "runpack missing top-level field: {field}"
        );
    }
    let verdict = json["verdict"].as_str().expect("verdict is string");
    assert!(
        ["ready", "ready_with_warnings", "not_ready"].contains(&verdict),
        "runpack verdict not in known set: {verdict}"
    );
    let findings = json["findings"].as_array().expect("findings is array");
    assert!(
        !findings.is_empty(),
        "findings array must have at least one entry"
    );
}
