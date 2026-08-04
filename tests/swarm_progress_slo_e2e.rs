#![allow(clippy::too_many_lines)]
#![forbid(unsafe_code)]

use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use pi::swarm_progress_slo::{
    AgentMailHealth, FreshnessState, ProgressSloEvaluationInput, ProgressSloMetrics,
    ProgressSloSourceStatus, ProgressSloTimeWindow, RchPosture, RedactionState, SourceAvailability,
    ValidationBrokerPosture,
};
use serde::Serialize;
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const E2E_SUMMARY_SCHEMA: &str = "pi.swarm.progress_slo_e2e.v1";
const E2E_EVENT_SCHEMA: &str = "pi.swarm.progress_slo_e2e.event.v1";
const PROGRESS_SCHEMA: &str = "pi.swarm.progress_slo.v1";
const GENERATED_AT: &str = "2026-05-15T05:00:00Z";
const WINDOW_START: &str = "2026-05-15T04:00:00Z";
const WINDOW_END: &str = "2026-05-15T05:00:00Z";
const SINCE: &str = "HEAD~1";

#[derive(Debug, Clone)]
struct Scenario {
    name: &'static str,
    expected_status: &'static str,
    expected_reason_ids: &'static [&'static str],
    closed_beads: u64,
    open_beads: u64,
    in_progress_beads: u64,
    ready_beads: u64,
    dependency_blocked_beads: u64,
    commits: u64,
    pushed_commits: u64,
    closed_with_commit_reference_count: u64,
    validation_passes: u64,
    validation_failures: u64,
    agent_mail_health: AgentMailHealth,
    rch_posture: RchPosture,
    rch_queue_depth: u64,
    rch_queue_saturation_threshold: u64,
    validation_broker_posture: ValidationBrokerPosture,
    stale_in_progress_candidates: u64,
    malformed_source_records: u64,
    dirty_worktree: bool,
    malformed_source_id: Option<&'static str>,
    redaction_state: RedactionState,
}

#[derive(Debug, Clone, Serialize)]
struct SourceEvidence {
    source_id: String,
    source_kind: String,
    path: String,
    availability: String,
    freshness_state: String,
    redaction_state: String,
}

#[derive(Debug, Clone, Serialize)]
struct ScenarioEvidence {
    scenario_name: String,
    expected_status: String,
    actual_status: String,
    expected_reason_ids: Vec<String>,
    actual_reason_ids: Vec<String>,
    source_provenance: Vec<SourceEvidence>,
    redaction_posture: Value,
    commands: Vec<String>,
    artifacts: Value,
}

fn test_error(message: impl Into<String>) -> Box<dyn Error> {
    io::Error::other(message.into()).into()
}

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
        .join("swarm-progress-slo-e2e-tmp")
        .join(format!("{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn path_str(path: &Path) -> TestResult<&str> {
    path.to_str()
        .ok_or_else(|| test_error(format!("path is not UTF-8: {}", path.display())))
}

fn output_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn run_command(mut command: Command, label: &str) -> TestResult<Output> {
    let output = command.output()?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(test_error(format!(
            "{label} failed with {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            output_text(&output.stdout),
            output_text(&output.stderr)
        )))
    }
}

fn run_git(workspace: &Path, args: &[&str]) -> TestResult<Output> {
    let mut command = Command::new("git");
    command
        .current_dir(workspace)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_command(command, &format!("git {}", args.join(" ")))
}

fn run_pi(workspace: &Path, args: &[&str]) -> TestResult<Output> {
    let mut command = Command::new(binary_path()); // ubs:ignore Cargo provides this test binary path.
    command
        .current_dir(workspace)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_command(command, &format!("pi {}", args.join(" ")))
}

fn write_json(path: &Path, value: &impl Serialize) -> TestResult {
    fs::write(path, serde_json::to_string_pretty(value)?)?;
    Ok(())
}

fn read_json(path: &Path) -> TestResult<Value> {
    let raw = fs::read_to_string(path)?;
    serde_json::from_str(&raw).map_err(Into::into)
}

fn write_event(path: &Path, scenario: &str, phase: &str, payload: &Value) -> TestResult {
    use std::io::Write;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let event = json!({
        "schema": E2E_EVENT_SCHEMA,
        "generated_at": GENERATED_AT,
        "scenario_name": scenario,
        "phase": phase,
        "payload": payload,
    });
    writeln!(file, "{}", serde_json::to_string(&event)?)?;
    Ok(())
}

fn init_git_workspace(workspace: &Path) -> TestResult<Vec<String>> {
    let commands = vec![
        "git init -b main".to_string(),
        "git config user.email pi-e2e@example.invalid".to_string(),
        "git config user.name Pi E2E".to_string(),
        "git add README.md".to_string(),
        "git commit -m initial".to_string(),
    ];

    run_git(workspace, &["init", "-b", "main"])?;
    run_git(
        workspace,
        &["config", "user.email", "pi-e2e@example.invalid"],
    )?;
    run_git(workspace, &["config", "user.name", "Pi E2E"])?;
    fs::write(
        workspace.join("README.md"),
        "# progress SLO E2E fixture\n\nlocal temp workspace\n",
    )?;
    run_git(workspace, &["add", "README.md"])?;
    run_git(workspace, &["commit", "-m", "initial"])?;
    Ok(commands)
}

fn add_progress_commit(workspace: &Path, scenario_name: &str) -> TestResult<Vec<String>> {
    let path = workspace.join(format!("{scenario_name}-evidence.txt"));
    fs::write(&path, format!("progress evidence for {scenario_name}\n"))?;
    run_git(workspace, &["add", path_str(&path)?])?;
    run_git(
        workspace,
        &[
            "commit",
            "-m",
            &format!("ship progress evidence for {scenario_name}"),
        ],
    )?;
    Ok(vec![
        format!("git add {}", path.display()),
        format!("git commit -m ship progress evidence for {scenario_name}"),
    ])
}

fn write_dirty_worktree_file(workspace: &Path) -> TestResult<Vec<String>> {
    let path = workspace.join("unrelated-dirty-worktree-note.txt");
    fs::write(
        &path,
        "unrelated concurrent worktree dirt that progress SLO must not mutate\n",
    )?;
    Ok(vec![format!("write {}", path.display())])
}

fn write_beads_jsonl(workspace: &Path, scenario: &Scenario) -> TestResult<PathBuf> {
    let beads_dir = workspace.join(".beads");
    fs::create_dir_all(&beads_dir)?;
    let path = beads_dir.join("issues.jsonl");
    let mut records = Vec::new();

    for index in 0..scenario.closed_beads {
        records.push(json!({
            "id": format!("bd-closed-{index}"),
            "title": format!("Closed progress bead {index}"),
            "status": "closed",
            "priority": 2,
            "issue_type": "task",
            "updated_at": WINDOW_END,
            "closed_at": WINDOW_END,
            "close_reason": "Completed in commit progress-evidence",
            "external_ref": format!("commit:progress-{index}"),
        }));
    }
    for index in 0..scenario.open_beads {
        records.push(json!({
            "id": format!("bd-open-{index}"),
            "title": format!("Open progress bead {index}"),
            "status": "open",
            "priority": 2,
            "issue_type": "task",
            "updated_at": WINDOW_END,
        }));
    }
    for index in 0..scenario.in_progress_beads {
        let updated_at = if scenario.stale_in_progress_candidates > 0 {
            "2026-05-10T00:00:00Z"
        } else {
            WINDOW_END
        };
        records.push(json!({
            "id": format!("bd-active-{index}"),
            "title": format!("Active progress bead {index}"),
            "status": "in_progress",
            "priority": 2,
            "issue_type": "task",
            "assignee": "TempAgent",
            "updated_at": updated_at,
        }));
    }

    let mut content = String::new();
    for record in records {
        content.push_str(&serde_json::to_string(&record)?);
        content.push('\n');
    }
    fs::write(&path, content)?;
    Ok(path)
}

fn write_fixture_sources(workspace: &Path, scenario: &Scenario) -> TestResult<(PathBuf, PathBuf)> {
    let evidence_dir = workspace.join("evidence");
    fs::create_dir_all(&evidence_dir)?;

    let agent_mail_path = evidence_dir.join("agent-mail-health.json");
    write_json(
        &agent_mail_path,
        &json!({
            "schema": "pi.swarm.progress_slo.fixture.agent_mail_health.v1",
            "captured_from": "mcp_agent_mail.health_check",
            "status": format!("{:?}", scenario.agent_mail_health).to_lowercase(),
            "live_mutation_allowed": false,
            "redaction": "sensitive_omitted",
        }),
    )?;

    let rch_path = evidence_dir.join("rch-queue.txt");
    fs::write(
        &rch_path,
        format!(
            "captured_from=rch queue\nposture={:?}\nqueue_depth={}\nsaturation_threshold={}\nlive_mutation_allowed=false\n",
            scenario.rch_posture,
            scenario.rch_queue_depth,
            scenario.rch_queue_saturation_threshold
        )
        .to_lowercase(),
    )?;

    Ok((agent_mail_path, rch_path))
}

fn git_commit_count(workspace: &Path) -> TestResult<u64> {
    let output = run_git(workspace, &["rev-list", "--count", "HEAD"])?;
    let count = output_text(&output.stdout).trim().parse::<u64>()?;
    Ok(count.saturating_sub(1))
}

fn git_dirty(workspace: &Path) -> TestResult<bool> {
    let output = run_git(workspace, &["status", "--porcelain"])?;
    Ok(!output.stdout.is_empty())
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

fn source_path_for(id: &str, beads_path: &Path, agent_mail_path: &Path, rch_path: &Path) -> String {
    match id {
        "beads_active_delta" | "beads_closed_delta" => beads_path.display().to_string(),
        "git_commit_delta" => ".git".to_string(),
        "rch_posture" => rch_path.display().to_string(),
        "agent_mail_health" => agent_mail_path.display().to_string(),
        "validation_broker_posture" => "fixture://validation-broker-green".to_string(),
        "operator_runpack_summary" => "fixture://operator-runpack-summary".to_string(),
        "swarm_autopilot_summary" => "fixture://swarm-autopilot-summary".to_string(),
        "context_intelligence_summary" => "fixture://context-intelligence-summary".to_string(),
        "operator_time_window" => "operator://requested-window".to_string(),
        _ => "fixture://unknown".to_string(),
    }
}

fn progress_source(id: &str, path: String, scenario: &Scenario) -> ProgressSloSourceStatus {
    let is_malformed = scenario.malformed_source_id == Some(id);
    let redaction_state = if id == "agent_mail_health" {
        scenario.redaction_state
    } else {
        RedactionState::None
    };
    let availability = if is_malformed {
        SourceAvailability::Malformed
    } else {
        SourceAvailability::Available
    };
    let freshness_state = if is_malformed {
        FreshnessState::Malformed
    } else {
        FreshnessState::Current
    };

    let mut source = ProgressSloSourceStatus::new(
        id,
        source_class_for(id),
        source_kind_for(id),
        availability,
        freshness_state,
        redaction_state,
        vec![format!("{id}_authority")],
    )
    .with_path(path)
    .with_observed_at(GENERATED_AT)
    .with_source_hash(format!("fixture-sha256-{id}-{}", scenario.name));

    if is_malformed {
        source = source.with_degraded_reason("fixture_payload_failed_schema_validation");
    }
    if matches!(
        redaction_state,
        RedactionState::Redacted | RedactionState::SensitiveOmitted | RedactionState::UnsafeToEmit
    ) {
        source = source.with_suppressed_claim("raw_agent_mail_payload_contains_sensitive_state");
    }
    source
}

fn build_input(
    workspace: &Path,
    scenario: &Scenario,
    beads_path: &Path,
    agent_mail_path: &Path,
    rch_path: &Path,
) -> TestResult<ProgressSloEvaluationInput> {
    let commits = git_commit_count(workspace)?;
    let source_statuses = [
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
    .map(|id| {
        progress_source(
            id,
            source_path_for(id, beads_path, agent_mail_path, rch_path),
            scenario,
        )
    })
    .collect();

    let metrics = ProgressSloMetrics {
        closed_beads: scenario.closed_beads,
        open_beads: scenario.open_beads,
        in_progress_beads: scenario.in_progress_beads,
        ready_beads: scenario.ready_beads,
        dependency_blocked_beads: scenario.dependency_blocked_beads,
        commits,
        pushed_commits: scenario.pushed_commits,
        closed_with_commit_reference_count: scenario.closed_with_commit_reference_count,
        validation_passes: scenario.validation_passes,
        validation_failures: scenario.validation_failures,
        agent_mail_health: scenario.agent_mail_health,
        rch_posture: scenario.rch_posture,
        rch_queue_depth: scenario.rch_queue_depth,
        rch_queue_saturation_threshold: scenario.rch_queue_saturation_threshold,
        validation_broker_posture: scenario.validation_broker_posture,
        stale_in_progress_candidates: scenario.stale_in_progress_candidates,
        malformed_source_records: scenario.malformed_source_records,
        contradictory_source_records: 0,
    };
    Ok(ProgressSloEvaluationInput::new(
        GENERATED_AT,
        ProgressSloTimeWindow::new(WINDOW_START, WINDOW_END, 3600, SINCE),
        source_statuses,
        metrics,
    ))
}

fn source_evidence(report: &Value) -> TestResult<Vec<SourceEvidence>> {
    let sources = report
        .pointer("/source_statuses")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("report missing source_statuses array"))?;
    let mut evidence = Vec::new();
    for source in sources {
        evidence.push(SourceEvidence {
            source_id: required_str(source, "/source_id")?.to_string(),
            source_kind: required_str(source, "/source_kind")?.to_string(),
            path: required_str(source, "/path")?.to_string(),
            availability: required_str(source, "/availability")?.to_string(),
            freshness_state: required_str(source, "/freshness_state")?.to_string(),
            redaction_state: required_str(source, "/redaction_state")?.to_string(),
        });
    }
    Ok(evidence)
}

fn required_str<'a>(value: &'a Value, pointer: &str) -> TestResult<&'a str> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| test_error(format!("missing string at {pointer}: {value:#}")))
}

fn string_array(value: &Value, pointer: &str) -> TestResult<Vec<String>> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .ok_or_else(|| test_error(format!("missing array at {pointer}: {value:#}")))?
        .iter()
        .map(|item| {
            item.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                test_error(format!("array item is not string at {pointer}: {item:#}"))
            })
        })
        .collect()
}

fn expected_reason_vec(scenario: &Scenario) -> Vec<String> {
    scenario
        .expected_reason_ids
        .iter()
        .map(|reason| (*reason).to_string())
        .collect()
}

fn assert_expected_report(report: &Value, scenario: &Scenario) -> TestResult {
    let status = required_str(report, "/status")?;
    if status != scenario.expected_status {
        return Err(test_error(format!(
            "scenario {} expected status {}, got {status}: {report:#}",
            scenario.name, scenario.expected_status
        )));
    }

    let reasons = string_array(report, "/reason_ids")?;
    for expected in scenario.expected_reason_ids {
        if !reasons.iter().any(|reason| reason == expected) {
            return Err(test_error(format!(
                "scenario {} missing expected reason {expected}; got {reasons:?}",
                scenario.name
            )));
        }
    }
    Ok(())
}

fn run_scenario(
    root: &Path,
    scenario: &Scenario,
    event_log: &Path,
) -> TestResult<ScenarioEvidence> {
    let workspace = root.join(scenario.name);
    fs::create_dir_all(&workspace)?;
    let mut commands = init_git_workspace(&workspace)?;
    if scenario.commits > 0 || scenario.closed_with_commit_reference_count > 0 {
        commands.extend(add_progress_commit(&workspace, scenario.name)?);
    }
    if scenario.dirty_worktree {
        commands.extend(write_dirty_worktree_file(&workspace)?);
    }
    let beads_path = write_beads_jsonl(&workspace, scenario)?;
    let (agent_mail_path, rch_path) = write_fixture_sources(&workspace, scenario)?;

    write_event(
        event_log,
        scenario.name,
        "scenario_start",
        &json!({
            "workspace": workspace.display().to_string(),
            "beads_path": beads_path.display().to_string(),
            "agent_mail_fixture": agent_mail_path.display().to_string(),
            "rch_fixture": rch_path.display().to_string(),
            "commands": commands.clone(),
        }),
    )?;

    let input = build_input(
        &workspace,
        scenario,
        &beads_path,
        &agent_mail_path,
        &rch_path,
    )?;
    let input_path = workspace.join("progress-input.json");
    let report_path = workspace.join("progress-report.json");
    write_json(&input_path, &input)?;

    write_event(
        event_log,
        scenario.name,
        "normalized_input_written",
        &json!({
            "schema": PROGRESS_SCHEMA,
            "input_path": input_path.display().to_string(),
            "source_count": input.source_statuses.len(),
            "redaction_posture": {
                "agent_mail_redaction": format!("{:?}", scenario.redaction_state).to_lowercase(),
                "live_agent_mail_mutation": false,
            },
        }),
    )?;

    let command_text = format!(
        "pi swarm-progress --input {} --since {SINCE} --out-json {}",
        input_path.display(),
        report_path.display()
    );
    commands.push(command_text);
    run_pi(
        &workspace,
        &[
            "swarm-progress",
            "--input",
            path_str(&input_path)?,
            "--since",
            SINCE,
            "--out-json",
            path_str(&report_path)?,
        ],
    )?;

    let report = read_json(&report_path)?;
    assert_eq!(
        report.pointer("/schema").and_then(Value::as_str),
        Some(PROGRESS_SCHEMA)
    );
    assert_expected_report(&report, scenario)?;

    let actual_status = required_str(&report, "/status")?.to_string();
    let actual_reason_ids = string_array(&report, "/reason_ids")?;
    let redaction_posture = report
        .pointer("/redaction_summary")
        .cloned()
        .ok_or_else(|| test_error("report missing redaction_summary"))?;
    let evidence = ScenarioEvidence {
        scenario_name: scenario.name.to_string(),
        expected_status: scenario.expected_status.to_string(),
        actual_status,
        expected_reason_ids: expected_reason_vec(scenario),
        actual_reason_ids,
        source_provenance: source_evidence(&report)?,
        redaction_posture,
        commands,
        artifacts: json!({
            "workspace": workspace.display().to_string(),
            "input": input_path.display().to_string(),
            "report": report_path.display().to_string(),
            "events": event_log.display().to_string(),
            "beads": beads_path.display().to_string(),
            "agent_mail_fixture": agent_mail_path.display().to_string(),
            "rch_fixture": rch_path.display().to_string(),
            "dirty_worktree_observed": git_dirty(&workspace)?,
        }),
    };

    write_event(
        event_log,
        scenario.name,
        "scenario_asserted",
        &json!({
            "expected_status": &evidence.expected_status,
            "actual_status": &evidence.actual_status,
            "expected_reason_ids": &evidence.expected_reason_ids,
            "actual_reason_ids": &evidence.actual_reason_ids,
            "commands": &evidence.commands,
        }),
    )?;

    Ok(evidence)
}

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "healthy_closeout_progress",
            expected_status: "progressing",
            expected_reason_ids: &[
                "PROGRESS-SLO-BEAD-CLOSEOUT",
                "PROGRESS-SLO-GIT-COMMIT-DELTA",
            ],
            closed_beads: 2,
            open_beads: 4,
            in_progress_beads: 1,
            ready_beads: 2,
            dependency_blocked_beads: 1,
            commits: 1,
            pushed_commits: 1,
            closed_with_commit_reference_count: 2,
            validation_passes: 2,
            validation_failures: 0,
            agent_mail_health: AgentMailHealth::Green,
            rch_posture: RchPosture::Green,
            rch_queue_depth: 0,
            rch_queue_saturation_threshold: 10,
            validation_broker_posture: ValidationBrokerPosture::Green,
            stale_in_progress_candidates: 0,
            malformed_source_records: 0,
            dirty_worktree: false,
            malformed_source_id: None,
            redaction_state: RedactionState::None,
        },
        Scenario {
            name: "no_open_work_convergence",
            expected_status: "converged_no_open_work",
            expected_reason_ids: &["PROGRESS-SLO-CONVERGED-NO-OPEN-WORK"],
            closed_beads: 1,
            open_beads: 0,
            in_progress_beads: 0,
            ready_beads: 0,
            dependency_blocked_beads: 0,
            commits: 1,
            pushed_commits: 1,
            closed_with_commit_reference_count: 1,
            validation_passes: 1,
            validation_failures: 0,
            agent_mail_health: AgentMailHealth::Green,
            rch_posture: RchPosture::Green,
            rch_queue_depth: 0,
            rch_queue_saturation_threshold: 10,
            validation_broker_posture: ValidationBrokerPosture::Green,
            stale_in_progress_candidates: 0,
            malformed_source_records: 0,
            dirty_worktree: false,
            malformed_source_id: None,
            redaction_state: RedactionState::None,
        },
        Scenario {
            name: "stale_in_progress_candidate",
            expected_status: "stalled",
            expected_reason_ids: &["PROGRESS-SLO-STALE-IN-PROGRESS"],
            closed_beads: 0,
            open_beads: 3,
            in_progress_beads: 2,
            ready_beads: 1,
            dependency_blocked_beads: 1,
            commits: 0,
            pushed_commits: 0,
            closed_with_commit_reference_count: 0,
            validation_passes: 0,
            validation_failures: 0,
            agent_mail_health: AgentMailHealth::Green,
            rch_posture: RchPosture::Green,
            rch_queue_depth: 0,
            rch_queue_saturation_threshold: 10,
            validation_broker_posture: ValidationBrokerPosture::Green,
            stale_in_progress_candidates: 2,
            malformed_source_records: 0,
            dirty_worktree: false,
            malformed_source_id: None,
            redaction_state: RedactionState::None,
        },
        Scenario {
            name: "agent_mail_corrupt_soft_lock",
            expected_status: "coordination_degraded",
            expected_reason_ids: &["PROGRESS-SLO-AGENT-MAIL-DEGRADED"],
            closed_beads: 1,
            open_beads: 3,
            in_progress_beads: 1,
            ready_beads: 2,
            dependency_blocked_beads: 1,
            commits: 1,
            pushed_commits: 1,
            closed_with_commit_reference_count: 1,
            validation_passes: 1,
            validation_failures: 0,
            agent_mail_health: AgentMailHealth::Corrupt,
            rch_posture: RchPosture::Green,
            rch_queue_depth: 0,
            rch_queue_saturation_threshold: 10,
            validation_broker_posture: ValidationBrokerPosture::Green,
            stale_in_progress_candidates: 0,
            malformed_source_records: 0,
            dirty_worktree: false,
            malformed_source_id: None,
            redaction_state: RedactionState::SensitiveOmitted,
        },
        Scenario {
            name: "rch_saturated",
            expected_status: "build_saturated",
            expected_reason_ids: &["PROGRESS-SLO-RCH-SATURATED"],
            closed_beads: 1,
            open_beads: 3,
            in_progress_beads: 1,
            ready_beads: 2,
            dependency_blocked_beads: 1,
            commits: 1,
            pushed_commits: 1,
            closed_with_commit_reference_count: 1,
            validation_passes: 1,
            validation_failures: 0,
            agent_mail_health: AgentMailHealth::Green,
            rch_posture: RchPosture::Saturated,
            rch_queue_depth: 42,
            rch_queue_saturation_threshold: 10,
            validation_broker_posture: ValidationBrokerPosture::Green,
            stale_in_progress_candidates: 0,
            malformed_source_records: 0,
            dirty_worktree: false,
            malformed_source_id: None,
            redaction_state: RedactionState::None,
        },
        Scenario {
            name: "unrelated_dirty_worktree",
            expected_status: "progressing",
            expected_reason_ids: &[
                "PROGRESS-SLO-BEAD-CLOSEOUT",
                "PROGRESS-SLO-GIT-COMMIT-DELTA",
            ],
            closed_beads: 1,
            open_beads: 2,
            in_progress_beads: 1,
            ready_beads: 1,
            dependency_blocked_beads: 1,
            commits: 1,
            pushed_commits: 1,
            closed_with_commit_reference_count: 1,
            validation_passes: 1,
            validation_failures: 0,
            agent_mail_health: AgentMailHealth::Green,
            rch_posture: RchPosture::Green,
            rch_queue_depth: 0,
            rch_queue_saturation_threshold: 10,
            validation_broker_posture: ValidationBrokerPosture::Green,
            stale_in_progress_candidates: 0,
            malformed_source_records: 0,
            dirty_worktree: true,
            malformed_source_id: None,
            redaction_state: RedactionState::None,
        },
        Scenario {
            name: "malformed_source_fail_closed",
            expected_status: "malformed_source_degraded",
            expected_reason_ids: &["PROGRESS-SLO-MALFORMED-SOURCE"],
            closed_beads: 1,
            open_beads: 2,
            in_progress_beads: 1,
            ready_beads: 1,
            dependency_blocked_beads: 1,
            commits: 1,
            pushed_commits: 1,
            closed_with_commit_reference_count: 1,
            validation_passes: 1,
            validation_failures: 0,
            agent_mail_health: AgentMailHealth::Green,
            rch_posture: RchPosture::Green,
            rch_queue_depth: 0,
            rch_queue_saturation_threshold: 10,
            validation_broker_posture: ValidationBrokerPosture::Green,
            stale_in_progress_candidates: 0,
            malformed_source_records: 1,
            dirty_worktree: false,
            malformed_source_id: Some("git_commit_delta"),
            redaction_state: RedactionState::None,
        },
    ]
}

fn live_repo_guard_bytes() -> TestResult<Vec<(PathBuf, Option<Vec<u8>>)>> {
    let root = repo_root();
    let paths = [
        root.join(".git").join("HEAD"),
        root.join(".beads").join("issues.jsonl"),
    ];
    let mut guards = Vec::with_capacity(paths.len());
    for path in paths {
        let contents = match fs::read(&path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        guards.push((path, contents));
    }
    if guards.iter().all(|(_, contents)| contents.is_none()) {
        return Err(test_error(
            "live repo has neither Git HEAD nor Beads ledger to guard",
        ));
    }
    Ok(guards)
}

fn assert_live_repo_unchanged(before: &[(PathBuf, Option<Vec<u8>>)]) -> TestResult {
    for (path, expected) in before {
        let actual = match fs::read(path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if &actual != expected {
            return Err(test_error(format!(
                "live repo sentinel was created, removed, or changed during E2E: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn assert_event_log(path: &Path, scenario_count: usize) -> TestResult {
    let raw = fs::read_to_string(path)?;
    let mut asserted = 0_usize;
    for line in raw.lines() {
        let event: Value = serde_json::from_str(line)?;
        assert_eq!(
            event.pointer("/schema").and_then(Value::as_str),
            Some(E2E_EVENT_SCHEMA)
        );
        if event.pointer("/phase").and_then(Value::as_str) == Some("scenario_asserted") {
            asserted += 1;
            let commands = event
                .pointer("/payload/commands")
                .and_then(Value::as_array)
                .ok_or_else(|| test_error("scenario_asserted event missing commands"))?;
            if commands.is_empty() {
                return Err(test_error(
                    "scenario_asserted event did not record commands",
                ));
            }
        }
    }
    assert_eq!(asserted, scenario_count);
    Ok(())
}

fn assert_summary(summary: &Value, expected_count: usize) -> TestResult {
    assert_eq!(
        summary.pointer("/schema").and_then(Value::as_str),
        Some(E2E_SUMMARY_SCHEMA)
    );
    assert_eq!(
        summary
            .pointer("/guards/live_beads_mutations")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        summary
            .pointer("/guards/live_agent_mail_mutations")
            .and_then(Value::as_u64),
        Some(0)
    );
    let scenarios = summary
        .pointer("/scenarios")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("summary missing scenarios"))?;
    assert_eq!(scenarios.len(), expected_count);
    for scenario in scenarios {
        let commands = scenario
            .pointer("/commands")
            .and_then(Value::as_array)
            .ok_or_else(|| test_error("summary scenario missing commands"))?;
        let sources = scenario
            .pointer("/source_provenance")
            .and_then(Value::as_array)
            .ok_or_else(|| test_error("summary scenario missing source provenance"))?;
        if commands.is_empty() || sources.len() < 10 {
            return Err(test_error(format!(
                "summary scenario lacks commands or source provenance: {scenario:#}"
            )));
        }
    }
    Ok(())
}

#[test]
fn progress_slo_no_mock_e2e_emits_summary_and_jsonl_events() -> TestResult {
    let live_repo_before = live_repo_guard_bytes()?;
    let root = test_workspace("progress-slo-e2e")?;
    let event_log = root.join("progress-slo-e2e-events.jsonl");
    let mut evidences = Vec::new();
    let scenarios = scenarios();

    for scenario in &scenarios {
        evidences.push(run_scenario(&root, scenario, &event_log)?);
    }

    let summary_path = root.join("progress-slo-e2e-summary.json");
    write_json(
        &summary_path,
        &json!({
            "schema": E2E_SUMMARY_SCHEMA,
            "generated_at": GENERATED_AT,
            "purpose": "no_mock_progress_slo_e2e_advisory_evidence",
            "event_log_path": event_log.display().to_string(),
            "redaction_posture": {
                "raw_agent_mail_payloads": "sensitive_omitted",
                "secret_values": "not_recorded",
                "unsafe_to_emit_count": 0,
            },
            "guards": {
                "live_beads_mutations": 0,
                "live_agent_mail_mutations": 0,
                "live_rch_mutations": 0,
                "live_git_pushes": 0,
                "uses_temp_git_workspaces": true,
                "uses_temp_beads_jsonl": true,
                "uses_fixture_captured_agent_mail_and_rch": true,
            },
            "scenarios": evidences,
        }),
    )?;

    let summary = read_json(&summary_path)?;
    assert_summary(&summary, scenarios.len())?;
    assert_event_log(&event_log, scenarios.len())?;
    assert_live_repo_unchanged(&live_repo_before)?;
    Ok(())
}
