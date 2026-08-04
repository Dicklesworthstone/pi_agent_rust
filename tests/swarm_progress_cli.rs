#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use pi::swarm_progress_slo::{
    AgentMailHealth, FreshnessState, ProgressSloEvaluationInput, ProgressSloMetrics,
    ProgressSloSourceStatus, ProgressSloTimeWindow, RchPosture, RedactionState, SourceAvailability,
    ValidationBrokerPosture,
};
use serde_json::Value;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const GENERATED_AT: &str = "2026-05-15T03:00:00Z";
const SINCE: &str = "operator_requested_window";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_pi"))
}

fn test_workspace(name: &str) -> TestResult<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .to_string();
    let path = repo_root()
        .join("target")
        .join("swarm-progress-cli-tmp")
        .join(format!("{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn run_pi(args: &[&str]) -> Result<Output, std::io::Error> {
    run_pi_in(&repo_root(), args)
}

fn run_pi_in(cwd: &Path, args: &[&str]) -> Result<Output, std::io::Error> {
    Command::new(binary_path()) // ubs:ignore Cargo provides this test binary path.
        .current_dir(cwd)
        .args(args)
        .output()
}

fn run_git_in(cwd: &Path, args: &[&str]) -> TestResult<Output> {
    let output = Command::new("git").current_dir(cwd).args(args).output()?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(format!("git {} failed\n{}", args.join(" "), output_debug(&output)).into())
    }
}

fn snapshot_files(root: &Path) -> TestResult<BTreeMap<PathBuf, Vec<u8>>> {
    fn visit(
        root: &Path,
        directory: &Path,
        snapshot: &mut BTreeMap<PathBuf, Vec<u8>>,
    ) -> TestResult {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                visit(root, &path, snapshot)?;
            } else {
                snapshot.insert(path.strip_prefix(root)?.to_path_buf(), fs::read(&path)?);
            }
        }
        Ok(())
    }

    let mut snapshot = BTreeMap::new();
    visit(root, root, &mut snapshot)?;
    Ok(snapshot)
}

fn output_text(output: &[u8]) -> String {
    String::from_utf8_lossy(output).into_owned()
}

fn output_debug(output: &Output) -> String {
    format!(
        "status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        output_text(&output.stdout),
        output_text(&output.stderr)
    )
}

fn path_str(path: &Path) -> TestResult<&str> {
    path.to_str()
        .ok_or_else(|| format!("path is not UTF-8: {}", path.display()).into())
}

fn window() -> ProgressSloTimeWindow {
    ProgressSloTimeWindow::new("2026-05-15T02:00:00Z", "2026-05-15T03:00:00Z", 3600, SINCE)
}

fn source(id: &str) -> ProgressSloSourceStatus {
    ProgressSloSourceStatus::new(
        id,
        source_class_for(id),
        source_kind_for(id),
        SourceAvailability::Available,
        FreshnessState::Current,
        RedactionState::None,
        vec![format!("{id}_authority")],
    )
    .with_path(format!("evidence/{id}.json"))
    .with_observed_at(GENERATED_AT)
    .with_source_hash(format!("sha256-{id}"))
}

fn source_class_for(id: &str) -> &'static str {
    match id {
        "beads_active_delta" | "beads_closed_delta" => "beads_active_closed_delta",
        "git_commit_delta" => "git_commit_delta",
        "rch_posture" | "validation_broker_posture" => "rch_and_validation_broker_posture",
        "agent_mail_health" => "agent_mail_health",
        "operator_runpack_summary" | "swarm_autopilot_summary" | "context_intelligence_summary" => {
            "runpack_autopilot_context_summaries"
        }
        "operator_time_window" => "operator_provided_time_window",
        _ => "unknown",
    }
}

fn source_kind_for(id: &str) -> &'static str {
    match id {
        "beads_active_delta" | "beads_closed_delta" => "beads",
        "git_commit_delta" => "git",
        "rch_posture" => "rch",
        "validation_broker_posture" => "validation_broker",
        "agent_mail_health" => "agent_mail",
        "operator_runpack_summary" => "runpack",
        "swarm_autopilot_summary" => "autopilot",
        "context_intelligence_summary" => "context_intelligence",
        "operator_time_window" => "operator",
        _ => "unknown",
    }
}

fn all_sources() -> Vec<ProgressSloSourceStatus> {
    [
        "beads_active_delta",
        "beads_closed_delta",
        "git_commit_delta",
        "rch_posture",
        "validation_broker_posture",
        "agent_mail_health",
        "operator_runpack_summary",
        "swarm_autopilot_summary",
        "context_intelligence_summary",
        "operator_time_window",
    ]
    .into_iter()
    .map(source)
    .collect()
}

const fn healthy_metrics() -> ProgressSloMetrics {
    ProgressSloMetrics {
        closed_beads: 2,
        open_beads: 8,
        in_progress_beads: 1,
        ready_beads: 3,
        dependency_blocked_beads: 4,
        commits: 2,
        pushed_commits: 2,
        closed_with_commit_reference_count: 2,
        validation_passes: 3,
        validation_failures: 0,
        agent_mail_health: AgentMailHealth::Green,
        rch_posture: RchPosture::Green,
        rch_queue_depth: 0,
        rch_queue_saturation_threshold: 10,
        validation_broker_posture: ValidationBrokerPosture::Green,
        stale_in_progress_candidates: 0,
        malformed_source_records: 0,
        contradictory_source_records: 0,
    }
}

fn write_input(path: &Path, metrics: ProgressSloMetrics) -> TestResult {
    let input = ProgressSloEvaluationInput::new(GENERATED_AT, window(), all_sources(), metrics);
    fs::write(path, serde_json::to_string_pretty(&input)?)?;
    Ok(())
}

#[test]
fn swarm_progress_json_stdout_reports_degraded_agent_mail() -> TestResult {
    let temp = test_workspace("json-stdout")?;
    let input_path = temp.join("progress-input.json");
    write_input(
        &input_path,
        ProgressSloMetrics {
            agent_mail_health: AgentMailHealth::Corrupt,
            ..healthy_metrics()
        },
    )?;

    let output = run_pi(&[
        "swarm-progress",
        "--input",
        path_str(&input_path)?,
        "--since",
        SINCE,
        "--format",
        "json",
    ])?;

    assert!(output.status.success(), "{}", output_debug(&output));
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        report.pointer("/schema").and_then(Value::as_str),
        Some("pi.swarm.progress_slo.v1")
    );
    assert_eq!(
        report.pointer("/status").and_then(Value::as_str),
        Some("coordination_degraded")
    );
    assert!(
        report
            .pointer("/reason_ids")
            .and_then(Value::as_array)
            .is_some_and(|reasons| reasons
                .iter()
                .any(|reason| { reason.as_str() == Some("PROGRESS-SLO-AGENT-MAIL-DEGRADED") })),
        "report did not include Agent Mail degradation reason: {report:#}"
    );
    Ok(())
}

#[test]
fn swarm_progress_text_states_advisory_boundaries() -> TestResult {
    let temp = test_workspace("text-stdout")?;
    let input_path = temp.join("progress-input.json");
    write_input(&input_path, healthy_metrics())?;

    let output = run_pi(&[
        "swarm-progress",
        "--input",
        path_str(&input_path)?,
        "--format",
        "text",
    ])?;

    assert!(output.status.success(), "{}", output_debug(&output));
    let stdout = output_text(&output.stdout);
    assert!(stdout.contains("Swarm Progress SLO"));
    assert!(stdout.contains("status: progressing"));
    assert!(stdout.contains("advisory_only: true"));
    assert!(stdout.contains("read_only: true"));
    assert!(stdout.contains("live_mutations: 0"));
    assert!(stdout.contains("no live Beads/git/Agent Mail/RCH mutations"));
    Ok(())
}

#[test]
fn swarm_progress_output_paths_refuse_overwrite() -> TestResult {
    let temp = test_workspace("overwrite")?;
    let input_path = temp.join("progress-input.json");
    let output_path = temp.join("progress-output.json");
    write_input(&input_path, healthy_metrics())?;
    fs::write(&output_path, "{}")?;

    let output = run_pi(&[
        "swarm-progress",
        "--input",
        path_str(&input_path)?,
        "--out-json",
        path_str(&output_path)?,
    ])?;

    assert!(
        !output.status.success(),
        "overwrite command unexpectedly succeeded"
    );
    assert!(
        output_text(&output.stderr).contains("refusing to overwrite existing swarm-progress"),
        "stderr did not explain overwrite refusal:\n{}",
        output_text(&output.stderr)
    );
    Ok(())
}

#[test]
fn swarm_progress_malformed_input_fails_nonzero() -> TestResult {
    let temp = test_workspace("malformed")?;
    let input_path = temp.join("bad-progress-input.json");
    fs::write(&input_path, "{ not json")?;

    let output = run_pi(&[
        "swarm-progress",
        "--input",
        path_str(&input_path)?,
        "--format",
        "json",
    ])?;

    assert!(
        !output.status.success(),
        "malformed input command unexpectedly succeeded"
    );
    assert!(
        output_text(&output.stderr)
            .contains("swarm-progress requires normalized progress SLO input JSON"),
        "stderr did not explain malformed input:\n{}",
        output_text(&output.stderr)
    );
    Ok(())
}

#[test]
fn swarm_progress_stdout_does_not_mutate_git_or_beads_files() -> TestResult {
    let temp = test_workspace("read-only")?;
    let input_path = temp.join("progress-input.json");
    write_input(&input_path, healthy_metrics())?;
    run_git_in(&temp, &["init", "-b", "main"])?;
    run_git_in(&temp, &["config", "user.email", "pi-test@example.invalid"])?;
    run_git_in(&temp, &["config", "user.name", "Pi Test"])?;
    let marker_path = temp.join("tracked.txt");
    fs::write(&marker_path, "tracked fixture\n")?;
    run_git_in(&temp, &["add", path_str(&marker_path)?])?;
    run_git_in(&temp, &["commit", "-m", "initial fixture"])?;

    let git_dir = temp.join(".git");
    let beads_dir = temp.join(".beads");
    let beads_path = temp.join(".beads").join("issues.jsonl");
    fs::create_dir_all(&beads_dir)?;
    fs::write(&beads_path, "{\"id\":\"pi-test\",\"status\":\"open\"}\n")?;
    let git_before = snapshot_files(&git_dir)?;
    let beads_before = snapshot_files(&beads_dir)?;

    let output = run_pi_in(
        &temp,
        &[
            "swarm-progress",
            "--input",
            path_str(&input_path)?,
            "--format",
            "json",
        ],
    )?;

    assert!(output.status.success(), "{}", output_debug(&output));
    assert_eq!(snapshot_files(&git_dir)?, git_before);
    assert_eq!(snapshot_files(&beads_dir)?, beads_before);
    Ok(())
}
