//! Keeps `scripts/check_readme_evidence_freshness.py` honest inside the DSR
//! quality gate.
//!
//! The checker is the only thing that binds README performance/evidence
//! claims to artifacts (docs/releasing.md pre-release checklist). Its
//! fixture-based `--self-test` exercises the citation parser, the artifact
//! binding rules, and the v2 performance-budget contract, but until
//! 2026-09-02 nothing ran it: it had been failing since 2026-08-24 without
//! anyone noticing, and a dead early return in the checker itself had let a
//! path-only citation of the release-facing budget summary skip the v2
//! contract entirely. This test makes both regressions visible in
//! `cargo test --all-targets`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn run_checker(args: &[&str]) -> Result<(i32, String, String), String> {
    let output = Command::new("python3")
        .current_dir(repo_root())
        .arg("scripts/check_readme_evidence_freshness.py")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| format!("failed to spawn python3 for the README evidence checker: {err}"))?;
    let code = output.status.code().unwrap_or(-1); // ubs:ignore test assertion
    Ok((
        code,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

#[test]
fn readme_evidence_checker_self_test_passes() -> Result<(), String> {
    let (code, stdout, stderr) = run_checker(&["--self-test"])?;
    if code != 0 || !stdout.contains("SELF-TEST PASS") {
        return Err(format!(
            "README evidence checker self-test failed (exit {code})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        ));
    }
    Ok(())
}

#[test]
fn readme_evidence_checker_reports_a_verdict_for_the_live_readme() -> Result<(), String> {
    // The live verdict is allowed to be red (the perf evidence is currently
    // blocked, see bd-sog97.20); what must not happen is a crash or a silent
    // exit. Exit 0 means every cited artifact passed, exit 1 means the checker
    // found and printed violations; anything else is a checker defect.
    let (code, stdout, stderr) = run_checker(&[])?;
    if !stdout.contains("SUMMARY:") || !(code == 0 || code == 1) {
        return Err(format!(
            "README evidence checker did not produce a verdict (exit {code})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        ));
    }
    Ok(())
}
