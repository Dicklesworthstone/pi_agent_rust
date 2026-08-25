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
    run_postcondition_baseline_many(repo, &[generated_path], before_manifest)
}

fn run_postcondition_baseline_many(
    repo: &Path,
    generated_paths: &[&str],
    before_manifest: &Path,
) -> Result<Output, Box<dyn Error>> {
    let mut command = Command::new("python3");
    command
        .arg(script_path())
        .arg("--repo-root")
        .arg(repo)
        .arg("--mode")
        .arg("postcondition");
    for generated_path in generated_paths {
        command.arg("--generated-artifact").arg(generated_path);
    }
    Ok(command
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
    run_postcondition_many(repo, &[generated_path], before_manifest)
}

fn run_postcondition_many(
    repo: &Path,
    generated_paths: &[&str],
    before_manifest: &Path,
) -> Result<Output, Box<dyn Error>> {
    let mut command = Command::new("python3");
    command
        .arg(script_path())
        .arg("--repo-root")
        .arg(repo)
        .arg("--mode")
        .arg("postcondition");
    for generated_path in generated_paths {
        command.arg("--generated-artifact").arg(generated_path);
    }
    Ok(command
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
                Ok(1),
                Ok("artifacts/"),
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
fn project_rch_config_exclude_blocks_required_artifact() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    write_required_artifact(repo)?;
    fs::write(repo.join(".rchignore"), "# no project-local ignore rules\n")?;
    fs::create_dir_all(repo.join(".rch"))?;
    fs::write(
        repo.join(".rch/config.toml"),
        "[transfer]\nexclude_patterns = [\"tests/ext_conformance/artifacts/\"]\n",
    )?;

    let output = run_preflight(repo, REQUIRED_ARTIFACT)?;
    if output.status.success() {
        return Err(test_error(format!(
            "project transfer excludes are part of RCH's effective source filter\n{}",
            output_debug(&output)
        )));
    }

    let report = parse_json(&output)?;
    let excluded_by_config = array_field(&report, "violations")?.iter().any(|violation| {
        matches!(
            (
                string_field(violation, "source"),
                string_field(violation, "pattern"),
                string_field(violation, "reason"),
            ),
            (
                Ok(".rch/config.toml"),
                Ok("tests/ext_conformance/artifacts/"),
                Ok("required_path_excluded"),
            )
        )
    });
    if !excluded_by_config {
        return Err(test_error(format!(
            "config exclusion failure must identify .rch/config.toml\n{}",
            output_debug(&output)
        )));
    }
    Ok(())
}

#[test]
fn project_rch_config_uses_installed_rch_exclude_normalization() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    let required_path = ".git/refs/heads/main";
    let artifact = repo.join(required_path);
    fs::create_dir_all(
        artifact
            .parent()
            .ok_or_else(|| test_error("normalized config artifact should have a parent"))?,
    )?;
    fs::write(&artifact, "0123456789abcdef\n")?;
    fs::write(repo.join(".rchignore"), "# no project-local ignore rules\n")?;
    fs::create_dir_all(repo.join(".rch"))?;
    fs::write(
        repo.join(".rch/config.toml"),
        "[transfer]\nexclude_patterns = [\".git/objects/\"]\n",
    )?;

    let output = run_preflight(repo, required_path)?;
    if output.status.success() {
        return Err(test_error(format!(
            "RCH rewrites .git/objects/ to .git/, so sibling Git paths must be excluded\n{}",
            output_debug(&output)
        )));
    }
    let report = parse_json(&output)?;
    let normalized = array_field(&report, "violations")?.iter().any(|violation| {
        matches!(
            (
                string_field(violation, "source"),
                string_field(violation, "pattern"),
                string_field(violation, "reason"),
            ),
            (
                Ok(".rch/config.toml"),
                Ok(".git/"),
                Ok("required_path_excluded"),
            )
        )
    });
    if !normalized {
        return Err(test_error(format!(
            "config diagnostics must report RCH's normalized .git/ exclusion\n{}",
            output_debug(&output)
        )));
    }
    Ok(())
}

#[test]
fn mandatory_rch_runtime_exclude_blocks_required_artifact() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    let required_path = ".rch-tmp/current-evidence.json";
    let artifact = repo.join(required_path);
    fs::create_dir_all(
        artifact
            .parent()
            .ok_or_else(|| test_error("mandatory-exclude artifact should have a parent"))?,
    )?;
    fs::write(&artifact, "{\"current\":true}\n")?;
    fs::write(repo.join(".rchignore"), "# no project-local ignore rules\n")?;

    let output = run_preflight(repo, required_path)?;
    if output.status.success() {
        return Err(test_error(format!(
            "RCH's mandatory runtime exclusions must be part of preflight\n{}",
            output_debug(&output)
        )));
    }
    let report = parse_json(&output)?;
    let mandatory = array_field(&report, "violations")?.iter().any(|violation| {
        matches!(
            (
                string_field(violation, "source"),
                string_field(violation, "pattern"),
                string_field(violation, "reason"),
            ),
            (
                Ok("RCH mandatory exclusions"),
                Ok(".rch-tmp/"),
                Ok("required_path_excluded"),
            )
        )
    });
    if !mandatory {
        return Err(test_error(format!(
            "mandatory exclusion failure must identify RCH's .rch-tmp/ rule\n{}",
            output_debug(&output)
        )));
    }
    Ok(())
}

#[test]
fn globbed_directory_exclude_matches_descendant_artifact() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    let required_path = "target-worker/perf/current.json";
    let artifact = repo.join(required_path);
    fs::create_dir_all(
        artifact
            .parent()
            .ok_or_else(|| test_error("globbed-directory artifact should have a parent"))?,
    )?;
    fs::write(&artifact, "{\"current\":true}\n")?;
    fs::write(repo.join(".rchignore"), "# no project-local ignore rules\n")?;
    fs::create_dir_all(repo.join(".rch"))?;
    fs::write(
        repo.join(".rch/config.toml"),
        "[transfer]\nexclude_patterns = [\"target-*/\"]\n",
    )?;

    let output = run_preflight(repo, required_path)?;
    if output.status.success() {
        return Err(test_error(format!(
            "wildcards in directory excludes must match the directory component\n{}",
            output_debug(&output)
        )));
    }
    let report = parse_json(&output)?;
    let matched = array_field(&report, "violations")?.iter().any(|violation| {
        matches!(
            (
                string_field(violation, "source"),
                string_field(violation, "pattern"),
                string_field(violation, "reason"),
            ),
            (
                Ok(".rch/config.toml"),
                Ok("target-*/"),
                Ok("required_path_excluded"),
            )
        )
    });
    if !matched {
        return Err(test_error(format!(
            "globbed directory exclusion must identify the matching config rule\n{}",
            output_debug(&output)
        )));
    }
    Ok(())
}

#[test]
fn single_star_does_not_cross_path_separators() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    let required_path = "tests/one/two/artifact.json";
    let artifact = repo.join(required_path);
    fs::create_dir_all(
        artifact
            .parent()
            .ok_or_else(|| test_error("slash-sensitive artifact should have a parent"))?,
    )?;
    fs::write(&artifact, "{\"current\":true}\n")?;
    fs::write(repo.join(".rchignore"), "tests/*/artifact.json\n")?;

    let output = run_preflight(repo, required_path)?;
    if !output.status.success() {
        return Err(test_error(format!(
            "an rsync single-star component must not match across a slash\n{}",
            output_debug(&output)
        )));
    }
    let report = parse_json(&output)?;
    require_string_field(&report, "status", "pass")?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn required_artifact_symlink_fails_preflight() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    let target = repo.join("outside-artifact.json");
    fs::write(&target, "{\"schema\":\"fixture\"}\n")?;
    let artifact = repo.join(REQUIRED_ARTIFACT);
    fs::create_dir_all(
        artifact
            .parent()
            .ok_or_else(|| test_error("required artifact path should have a parent"))?,
    )?;
    symlink(&target, &artifact)?;
    fs::write(repo.join(".rchignore"), "# no excludes\n")?;

    let output = run_preflight(repo, REQUIRED_ARTIFACT)?;
    if output.status.success() {
        return Err(test_error(format!(
            "a symlink must not satisfy required source-artifact presence\n{}",
            output_debug(&output)
        )));
    }
    let report = parse_json(&output)?;
    let non_regular = array_field(&report, "violations")?.iter().any(|violation| {
        string_field(violation, "reason").is_ok_and(|reason| reason == "required_path_not_regular")
    });
    if !non_regular {
        return Err(test_error(format!(
            "symlink rejection must report required_path_not_regular\n{}",
            output_debug(&output)
        )));
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn required_artifact_with_symlinked_ancestor_fails_preflight() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    let external = temp.path().join("external");
    fs::create_dir_all(repo.join("tests"))?;
    fs::create_dir_all(&external)?;
    fs::write(external.join("artifact.json"), "{\"external\":true}\n")?;
    symlink(&external, repo.join("tests/linked"))?;
    fs::write(repo.join(".rchignore"), "# no excludes\n")?;

    let output = run_preflight(&repo, "tests/linked/artifact.json")?;
    if output.status.success() {
        return Err(test_error(format!(
            "a symlinked ancestor must not satisfy required source-artifact presence\n{}",
            output_debug(&output)
        )));
    }
    let report = parse_json(&output)?;
    let non_regular = array_field(&report, "violations")?.iter().any(|violation| {
        string_field(violation, "reason").is_ok_and(|reason| reason == "required_path_not_regular")
    });
    if !non_regular {
        return Err(test_error(format!(
            "ancestor symlink rejection must report required_path_not_regular\n{}",
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
fn leading_bang_is_literal_and_does_not_reinclude_required_artifact() -> Result<(), Box<dyn Error>>
{
    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    write_required_artifact(repo)?;
    fs::write(
        repo.join(".rchignore"),
        "tests/**\n!tests/ext_conformance/artifacts/**\n",
    )?;

    let output = run_preflight(repo, REQUIRED_ARTIFACT)?;
    if output.status.success() {
        return Err(test_error(format!(
            "RCH treats a leading bang literally, so it must not reinclude an excluded path\n{}",
            output_debug(&output)
        )));
    }

    let report = parse_json(&output)?;
    let violations = array_field(&report, "violations")?;
    let excluded = violations.iter().any(|violation| {
        string_field(violation, "reason").is_ok_and(|reason| reason == "required_path_excluded")
    });
    if !excluded {
        return Err(test_error(format!(
            "literal bang rule must not mask the original exclusion\n{}",
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
fn postcondition_requires_exact_baseline_artifact_set() -> Result<(), Box<dyn Error>> {
    const SECOND_GENERATED_ARTIFACT: &str =
        "tests/full_suite_gate/secondary_full_suite_verdict.json";

    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    fs::write(repo.join(".rchignore"), "/artifacts/\n")?;
    write_generated_artifact(repo, "{\"generated_at\":\"old\"}\n")?;
    let second = repo.join(SECOND_GENERATED_ARTIFACT);
    fs::create_dir_all(
        second
            .parent()
            .ok_or_else(|| test_error("secondary generated artifact should have a parent"))?,
    )?;
    fs::write(&second, "{\"generated_at\":\"old\"}\n")?;

    let before_manifest = repo.join("before-rch-artifacts.json");
    let baseline_output = run_postcondition_baseline_many(
        repo,
        &[GENERATED_ARTIFACT, SECOND_GENERATED_ARTIFACT],
        &before_manifest,
    )?;
    if !baseline_output.status.success() {
        return Err(test_error(format!(
            "two-artifact baseline capture should pass\n{}",
            output_debug(&baseline_output)
        )));
    }

    write_generated_artifact(repo, "{\"generated_at\":\"new\"}\n")?;
    let output = run_postcondition(repo, GENERATED_ARTIFACT, &before_manifest)?;
    if output.status.success() {
        return Err(test_error(format!(
            "a subset postcondition request must not silently omit a baseline artifact\n{}",
            output_debug(&output)
        )));
    }
    let report = parse_json(&output)?;
    let mismatch = array_field(&report, "violations")?.iter().any(|violation| {
        string_field(violation, "reason")
            .is_ok_and(|reason| reason == "before_manifest_artifact_set_mismatch")
    });
    if !mismatch {
        return Err(test_error(format!(
            "subset rejection must report before_manifest_artifact_set_mismatch\n{}",
            output_debug(&output)
        )));
    }
    Ok(())
}

#[test]
fn postcondition_rejects_duplicate_baseline_artifact_identity() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    fs::write(repo.join(".rchignore"), "/artifacts/\n")?;
    write_generated_artifact(repo, "{\"generated_at\":\"old\"}\n")?;

    let before_manifest = repo.join("before-rch-artifacts.json");
    let baseline_output = run_postcondition_baseline(repo, GENERATED_ARTIFACT, &before_manifest)?;
    if !baseline_output.status.success() {
        return Err(test_error(format!(
            "duplicate negative baseline capture should pass\n{}",
            output_debug(&baseline_output)
        )));
    }
    let mut baseline: Value = serde_json::from_slice(&fs::read(&before_manifest)?)?;
    let duplicate = baseline["generated_artifacts"][0].clone();
    baseline["generated_artifacts"]
        .as_array_mut()
        .ok_or_else(|| test_error("baseline generated_artifacts must be an array"))?
        .push(duplicate);
    fs::write(&before_manifest, serde_json::to_vec_pretty(&baseline)?)?;
    write_generated_artifact(repo, "{\"generated_at\":\"new\"}\n")?;

    let output = run_postcondition(repo, GENERATED_ARTIFACT, &before_manifest)?;
    if output.status.success() {
        return Err(test_error(format!(
            "duplicate baseline artifact identities must fail closed\n{}",
            output_debug(&output)
        )));
    }
    let report = parse_json(&output)?;
    let duplicate_rejected = array_field(&report, "violations")?.iter().any(|violation| {
        string_field(violation, "reason")
            .is_ok_and(|reason| reason == "before_manifest_duplicate_artifact")
    });
    if !duplicate_rejected {
        return Err(test_error(format!(
            "duplicate baseline must report before_manifest_duplicate_artifact\n{}",
            output_debug(&output)
        )));
    }
    Ok(())
}

#[test]
fn postcondition_rejects_metadata_only_changes_with_identical_bytes() -> Result<(), Box<dyn Error>>
{
    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    fs::write(repo.join(".rchignore"), "/artifacts/\n")?;
    write_generated_artifact(repo, "{\"generated_at\":\"old\"}\n")?;

    let before_manifest = repo.join("before-rch-artifacts.json");
    let baseline_output = run_postcondition_baseline(repo, GENERATED_ARTIFACT, &before_manifest)?;
    if !baseline_output.status.success() {
        return Err(test_error(format!(
            "metadata negative baseline capture should pass\n{}",
            output_debug(&baseline_output)
        )));
    }

    let mut baseline: Value = serde_json::from_slice(&fs::read(&before_manifest)?)?;
    baseline["generated_artifacts"][0]["snapshot"]["mtime_ns"] = Value::from(0);
    fs::write(&before_manifest, serde_json::to_vec_pretty(&baseline)?)?;

    let output = run_postcondition(repo, GENERATED_ARTIFACT, &before_manifest)?;
    if output.status.success() {
        return Err(test_error(format!(
            "metadata-only change with identical bytes must remain stale\n{}",
            output_debug(&output)
        )));
    }
    let report = parse_json(&output)?;
    let stale = array_field(&report, "violations")?.iter().any(|violation| {
        string_field(violation, "reason")
            .is_ok_and(|reason| reason == "generated_artifact_not_updated")
    });
    if !stale {
        return Err(test_error(format!(
            "metadata-only change must report generated_artifact_not_updated\n{}",
            output_debug(&output)
        )));
    }
    Ok(())
}

#[test]
fn postcondition_rejects_before_manifest_from_another_repo() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    let foreign_repo = temp.path().join("foreign-repo");
    fs::create_dir_all(&repo)?;
    fs::create_dir_all(&foreign_repo)?;
    fs::write(repo.join(".rchignore"), "/artifacts/\n")?;
    write_generated_artifact(&repo, "{\"generated_at\":\"old\"}\n")?;

    let before_manifest = repo.join("before-rch-artifacts.json");
    let baseline_output = run_postcondition_baseline(&repo, GENERATED_ARTIFACT, &before_manifest)?;
    if !baseline_output.status.success() {
        return Err(test_error(format!(
            "foreign-root negative baseline capture should pass\n{}",
            output_debug(&baseline_output)
        )));
    }

    let mut baseline: Value = serde_json::from_slice(&fs::read(&before_manifest)?)?;
    baseline["repo_root"] = Value::from(foreign_repo.display().to_string());
    fs::write(&before_manifest, serde_json::to_vec_pretty(&baseline)?)?;
    write_generated_artifact(&repo, "{\"generated_at\":\"new\"}\n")?;

    let output = run_postcondition(&repo, GENERATED_ARTIFACT, &before_manifest)?;
    if output.status.success() {
        return Err(test_error(format!(
            "before manifest from another repo must fail closed\n{}",
            output_debug(&output)
        )));
    }
    let report = parse_json(&output)?;
    let wrong_root = array_field(&report, "violations")?.iter().any(|violation| {
        string_field(violation, "reason")
            .is_ok_and(|reason| reason == "before_manifest_repo_root_mismatch")
    });
    if !wrong_root {
        return Err(test_error(format!(
            "foreign before manifest must report repo-root mismatch\n{}",
            output_debug(&output)
        )));
    }
    Ok(())
}

#[test]
fn postcondition_reports_malformed_before_manifest_as_json() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    fs::write(repo.join(".rchignore"), "/artifacts/\n")?;
    write_generated_artifact(repo, "{\"generated_at\":\"new\"}\n")?;
    let before_manifest = repo.join("before-rch-artifacts.json");
    fs::write(&before_manifest, "{not-json\n")?;

    let output = run_postcondition(repo, GENERATED_ARTIFACT, &before_manifest)?;
    if output.status.success() {
        return Err(test_error(
            "a malformed before manifest must fail the postcondition",
        ));
    }
    let report = parse_json(&output)?;
    require_string_field(&report, "status", "fail")?;
    let read_error = array_field(&report, "violations")?.iter().any(|violation| {
        string_field(violation, "reason")
            .is_ok_and(|reason| reason == "before_manifest_read_error")
    });
    if !read_error {
        return Err(test_error(format!(
            "malformed baseline must produce a structured read diagnostic\n{}",
            output_debug(&output)
        )));
    }
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
    let baseline_output = run_postcondition_baseline(&repo, generated_path, &before_manifest)?;
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

#[cfg(unix)]
#[test]
fn postcondition_baseline_rejects_preexisting_symlink() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir()?;
    let repo = temp.path();
    let target = repo.join("actual-artifact.json");
    fs::write(&target, "{\"stale\":true}\n")?;
    let link = repo.join(GENERATED_ARTIFACT);
    fs::create_dir_all(
        link.parent()
            .ok_or_else(|| test_error("generated artifact should have a parent"))?,
    )?;
    symlink(&target, &link)?;

    let before_manifest = repo.join("before-rch-artifacts.json");
    let output = run_postcondition_baseline(repo, GENERATED_ARTIFACT, &before_manifest)?;
    if output.status.success() {
        return Err(test_error(format!(
            "baseline capture must reject a preexisting symlinked output\n{}",
            output_debug(&output)
        )));
    }
    let report = parse_json(&output)?;
    let rejected = array_field(&report, "violations")?.iter().any(|violation| {
        string_field(violation, "reason")
            .is_ok_and(|reason| reason == "before_snapshot_not_regular_file")
    });
    if !rejected {
        return Err(test_error(format!(
            "preexisting symlink must fail with before_snapshot_not_regular_file\n{}",
            output_debug(&output)
        )));
    }
    Ok(())
}
