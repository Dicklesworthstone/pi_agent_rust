#![forbid(unsafe_code)]

use serde_json::Value;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const REQUIRED_ARTIFACT: &str = "tests/ext_conformance/artifacts/PROVENANCE_VERIFICATION.json";
const GENERATED_ARTIFACT: &str = "tests/full_suite_gate/full_suite_verdict.json";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn script_path() -> PathBuf {
    repo_root().join("scripts/check_rch_artifact_sync.py")
}

fn output_debug(output: &Output) -> String {
    format!(
        "status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn test_error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::other(message.into()))
}

fn run_preflight(repo: &Path, required_path: &str) -> Result<Output, Box<dyn Error>> {
    Ok(Command::new("python3")
        .arg(script_path())
        .arg("--repo-root")
        .arg(repo)
        .arg("--ignore-file")
        .arg(repo.join(".rchignore"))
        .arg("--required-path")
        .arg(required_path)
        .arg("--json")
        .output()?)
}

fn run_postcondition_baseline(
    repo: &Path,
    generated_path: &str,
    before_manifest: &Path,
) -> Result<Output, Box<dyn Error>> {
    Ok(Command::new("python3")
        .arg(script_path())
        .arg("--repo-root")
        .arg(repo)
        .arg("--mode")
        .arg("postcondition")
        .arg("--generated-artifact")
        .arg(generated_path)
        .arg("--write-before-manifest")
        .arg(before_manifest)
        .arg("--json")
        .output()?)
}

fn run_postcondition(
    repo: &Path,
    generated_path: &str,
    before_manifest: &Path,
) -> Result<Output, Box<dyn Error>> {
    Ok(Command::new("python3")
        .arg(script_path())
        .arg("--repo-root")
        .arg(repo)
        .arg("--mode")
        .arg("postcondition")
        .arg("--generated-artifact")
        .arg(generated_path)
        .arg("--before-manifest")
        .arg(before_manifest)
        .arg("--json")
        .output()?)
}

fn parse_json(output: &Output) -> Result<Value, Box<dyn Error>> {
    serde_json::from_slice(&output.stdout).map_err(|error| {
        test_error(format!(
            "preflight output should be JSON: {error}\n{}",
            output_debug(output)
        ))
    })
}

fn object_field<'a>(value: &'a Value, key: &str) -> Result<&'a Value, Box<dyn Error>> {
    value
        .get(key)
        .ok_or_else(|| test_error(format!("missing JSON field: {key}")))
}

fn string_field<'a>(value: &'a Value, key: &str) -> Result<&'a str, Box<dyn Error>> {
    object_field(value, key)?
        .as_str()
        .ok_or_else(|| test_error(format!("JSON field is not a string: {key}")))
}

fn i64_field(value: &Value, key: &str) -> Result<i64, Box<dyn Error>> {
    object_field(value, key)?
        .as_i64()
        .ok_or_else(|| test_error(format!("JSON field is not an integer: {key}")))
}

fn u64_field(value: &Value, key: &str) -> Result<u64, Box<dyn Error>> {
    object_field(value, key)?
        .as_u64()
        .ok_or_else(|| test_error(format!("JSON field is not an unsigned integer: {key}")))
}

fn bool_field(value: &Value, key: &str) -> Result<bool, Box<dyn Error>> {
    object_field(value, key)?
        .as_bool()
        .ok_or_else(|| test_error(format!("JSON field is not a boolean: {key}")))
}

fn array_field<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, Box<dyn Error>> {
    object_field(value, key)?
        .as_array()
        .ok_or_else(|| test_error(format!("JSON field is not an array: {key}")))
}

fn require_string_field(value: &Value, key: &str, expected: &str) -> Result<(), Box<dyn Error>> {
    match string_field(value, key)? {
        actual if actual.eq(expected) => Ok(()),
        actual => Err(test_error(format!(
            "expected JSON field {key} to be {expected:?}, got {actual:?}"
        ))),
    }
}

fn require_u64_field(value: &Value, key: &str, expected: u64) -> Result<(), Box<dyn Error>> {
    match u64_field(value, key)? {
        actual if actual == expected => Ok(()),
        actual => Err(test_error(format!(
            "expected JSON field {key} to be {expected}, got {actual}"
        ))),
    }
}

fn write_required_artifact(repo: &Path) -> Result<(), Box<dyn Error>> {
    let artifact = repo.join(REQUIRED_ARTIFACT);
    let parent = artifact
        .parent()
        .ok_or_else(|| test_error("required artifact path should have a parent"))?;
    fs::create_dir_all(parent)?;
    fs::write(artifact, "{\"schema\":\"fixture\"}\n")?;
    Ok(())
}

fn write_generated_artifact(repo: &Path, body: &str) -> Result<(), Box<dyn Error>> {
    let artifact = repo.join(GENERATED_ARTIFACT);
    let parent = artifact
        .parent()
        .ok_or_else(|| test_error("generated artifact path should have a parent"))?;
    fs::create_dir_all(parent)?;
    fs::write(artifact, body)?;
    Ok(())
}

#[test]
fn unanchored_artifacts_ignore_blocks_nested_required_artifacts() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    write_required_artifact(repo)?;
    fs::write(repo.join(".rchignore"), "artifacts/\nartifacts/**\n")?;

    let output = run_preflight(repo, REQUIRED_ARTIFACT)?;
    if output.status.success() {
        return Err(test_error(format!(
            "unanchored artifact rules should fail the preflight\n{}",
            output_debug(&output)
        )));
    }

    let report = parse_json(&output)?;
    require_string_field(&report, "schema", "pi.rch.artifact_sync_preflight.v1")?;
    require_string_field(&report, "status", "fail")?;

    let violations = array_field(&report, "violations")?;
    let has_expected_diagnostic = violations.iter().any(|violation| {
        matches!(
            (
                string_field(violation, "path"),
                string_field(violation, "source"),
                i64_field(violation, "line"),
                string_field(violation, "pattern"),
                string_field(violation, "reason"),
            ),
            (
                Ok(REQUIRED_ARTIFACT),
                Ok(".rchignore"),
                Ok(2),
                Ok("artifacts/**"),
                Ok("required_path_excluded"),
            )
        )
    });
    if !has_expected_diagnostic {
        return Err(test_error(format!(
            "diagnostics should name the exact .rchignore rule at fault:\n{}",
            output_debug(&output)
        )));
    }

    Ok(())
}

#[test]
fn anchored_root_artifacts_ignore_keeps_nested_required_artifacts() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    write_required_artifact(repo)?;
    fs::write(repo.join(".rchignore"), "/artifacts/\n/artifacts/**\n")?;

    let output = run_preflight(repo, REQUIRED_ARTIFACT)?;
    if !output.status.success() {
        return Err(test_error(format!(
            "anchored root artifact rules must not hide nested test artifacts\n{}",
            output_debug(&output)
        )));
    }

    let report = parse_json(&output)?;
    require_string_field(&report, "status", "pass")?;
    let required_paths = array_field(&report, "required_paths")?;
    let first_required = required_paths
        .first()
        .ok_or_else(|| test_error("expected one required path entry"))?;
    let matched_rules = array_field(first_required, "matched_rules")?;
    if !matched_rules.is_empty() {
        return Err(test_error(format!(
            "anchored root rules should not match nested artifact path:\n{}",
            output_debug(&output)
        )));
    }

    Ok(())
}

#[test]
fn current_repo_required_artifacts_pass_sync_preflight() -> Result<(), Box<dyn Error>> {
    let output = Command::new("python3")
        .arg(script_path())
        .arg("--repo-root")
        .arg(repo_root())
        .arg("--json")
        .output()?;

    if !output.status.success() {
        return Err(test_error(format!(
            "repo .rchignore should keep required artifact paths synced\n{}",
            output_debug(&output)
        )));
    }

    let report = parse_json(&output)?;
    require_string_field(&report, "status", "pass")?;
    let summary = object_field(&report, "summary")?;
    require_u64_field(summary, "violation_count", 0)?;
    Ok(())
}

#[test]
fn postcondition_fails_when_remote_gate_does_not_update_local_artifact()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    fs::write(repo.join(".rchignore"), "/artifacts/\n")?;
    write_generated_artifact(repo, "{\"generated_at\":\"old\",\"verdict\":\"fail\"}\n")?;

    let preflight_output = run_preflight(repo, GENERATED_ARTIFACT)?;
    if !preflight_output.status.success() {
        return Err(test_error(format!(
            "mirror inclusion preflight should pass before postcondition fails\n{}",
            output_debug(&preflight_output)
        )));
    }

    let before_manifest = repo.join("before-rch-artifacts.json");
    let baseline_output = run_postcondition_baseline(repo, GENERATED_ARTIFACT, &before_manifest)?;
    if !baseline_output.status.success() {
        return Err(test_error(format!(
            "postcondition baseline capture should pass\n{}",
            output_debug(&baseline_output)
        )));
    }

    let output = run_postcondition(repo, GENERATED_ARTIFACT, &before_manifest)?;
    if output.status.success() {
        return Err(test_error(format!(
            "unchanged local artifact should fail the postcondition\n{}",
            output_debug(&output)
        )));
    }

    let report = parse_json(&output)?;
    require_string_field(&report, "mode", "postcondition")?;
    require_string_field(&report, "status", "fail")?;
    let postconditions = array_field(&report, "postconditions")?;
    let first_postcondition = postconditions
        .first()
        .ok_or_else(|| test_error("expected one postcondition entry"))?;
    require_string_field(first_postcondition, "path", GENERATED_ARTIFACT)?;
    if bool_field(first_postcondition, "updated")? {
        return Err(test_error(
            "unchanged artifact should not be marked updated",
        ));
    }

    let violations = array_field(&report, "violations")?;
    let has_expected_diagnostic = violations.iter().any(|violation| {
        matches!(
            (
                string_field(violation, "path"),
                string_field(violation, "reason"),
            ),
            (Ok(GENERATED_ARTIFACT), Ok("generated_artifact_not_updated"))
        ) && string_field(violation, "message")
            .is_ok_and(|message| message.contains(GENERATED_ARTIFACT))
            && string_field(violation, "recommended_action")
                .is_ok_and(|action| action.contains("RCH artifact retrieval/writeback"))
    });
    if !has_expected_diagnostic {
        return Err(test_error(format!(
            "postcondition should name stale local artifact and retrieval/writeback action:\n{}",
            output_debug(&output)
        )));
    }

    Ok(())
}

#[test]
fn postcondition_passes_when_local_generated_artifact_changes() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    fs::write(repo.join(".rchignore"), "/artifacts/\n")?;
    write_generated_artifact(repo, "{\"generated_at\":\"old\",\"verdict\":\"fail\"}\n")?;

    let before_manifest = repo.join("before-rch-artifacts.json");
    let baseline_output = run_postcondition_baseline(repo, GENERATED_ARTIFACT, &before_manifest)?;
    if !baseline_output.status.success() {
        return Err(test_error(format!(
            "postcondition baseline capture should pass\n{}",
            output_debug(&baseline_output)
        )));
    }

    write_generated_artifact(repo, "{\"generated_at\":\"new\",\"verdict\":\"pass\"}\n")?;
    let output = run_postcondition(repo, GENERATED_ARTIFACT, &before_manifest)?;
    if !output.status.success() {
        return Err(test_error(format!(
            "changed local artifact should pass the postcondition\n{}",
            output_debug(&output)
        )));
    }

    let report = parse_json(&output)?;
    require_string_field(&report, "status", "pass")?;
    let postconditions = array_field(&report, "postconditions")?;
    let first_postcondition = postconditions
        .first()
        .ok_or_else(|| test_error("expected one postcondition entry"))?;
    if !bool_field(first_postcondition, "updated")? {
        return Err(test_error("changed artifact should be marked updated"));
    }
    let summary = object_field(&report, "summary")?;
    require_u64_field(summary, "updated_count", 1)?;
    require_u64_field(summary, "violation_count", 0)?;
    Ok(())
}

#[test]
fn postcondition_preserves_absolute_artifact_paths_outside_repo() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    let artifact = temp.path().join("external-target/extension_bench.jsonl");
    fs::create_dir_all(&repo)?;
    fs::write(repo.join(".rchignore"), "/artifacts/\n")?;

    let generated_path = artifact
        .to_str()
        .ok_or_else(|| test_error("absolute generated artifact path must be UTF-8"))?;
    let before_manifest = repo.join("before-rch-artifacts.json");
    let baseline_output =
        run_postcondition_baseline(&repo, generated_path, &before_manifest)?;
    if !baseline_output.status.success() {
        return Err(test_error(format!(
            "absolute-path baseline capture should pass\n{}",
            output_debug(&baseline_output)
        )));
    }

    fs::create_dir_all(
        artifact
            .parent()
            .ok_or_else(|| test_error("absolute artifact should have a parent"))?,
    )?;
    fs::write(&artifact, "{\"current_run\":true}\n")?;

    let output = run_postcondition(&repo, generated_path, &before_manifest)?;
    if !output.status.success() {
        return Err(test_error(format!(
            "new absolute artifact outside repo should pass the postcondition\n{}",
            output_debug(&output)
        )));
    }

    let report = parse_json(&output)?;
    let first_postcondition = array_field(&report, "postconditions")?
        .first()
        .ok_or_else(|| test_error("expected one absolute-path postcondition"))?;
    require_string_field(first_postcondition, "path", generated_path)?;
    if !bool_field(first_postcondition, "updated")? {
        return Err(test_error("new absolute artifact should be marked updated"));
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn postcondition_rejects_symlinked_generated_artifact() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    fs::write(repo.join(".rchignore"), "/artifacts/\n")?;

    let before_manifest = repo.join("before-rch-artifacts.json");
    let baseline_output = run_postcondition_baseline(repo, GENERATED_ARTIFACT, &before_manifest)?;
    if !baseline_output.status.success() {
        return Err(test_error(format!(
            "symlink negative baseline capture should pass\n{}",
            output_debug(&baseline_output)
        )));
    }

    let target = repo.join("actual-artifact.json");
    fs::write(&target, "{\"current_run\":true}\n")?;
    let link = repo.join(GENERATED_ARTIFACT);
    fs::create_dir_all(
        link.parent()
            .ok_or_else(|| test_error("generated artifact should have a parent"))?,
    )?;
    symlink(&target, &link)?;

    let output = run_postcondition(repo, GENERATED_ARTIFACT, &before_manifest)?;
    if output.status.success() {
        return Err(test_error(format!(
            "symlinked generated artifact must fail the postcondition\n{}",
            output_debug(&output)
        )));
    }

    let report = parse_json(&output)?;
    let has_symlink_diagnostic = array_field(&report, "violations")?.iter().any(|violation| {
        string_field(violation, "reason")
            .is_ok_and(|reason| reason == "generated_artifact_not_regular_file")
    });
    if !has_symlink_diagnostic {
        return Err(test_error(format!(
            "symlink rejection must report a regular-file violation\n{}",
            output_debug(&output)
        )));
    }
    Ok(())
}
