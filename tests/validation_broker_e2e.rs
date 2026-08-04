#![allow(clippy::too_many_lines)]
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use pi::validation_broker::{
    VALIDATION_BROKER_CLI_PLAN_SCHEMA, VALIDATION_BROKER_CLI_STATUS_SCHEMA,
    ValidationAdmissionPolicy, ValidationAdmissionRequestContext, ValidationBrokerInputParts,
    ValidationBrokerInputSnapshot, ValidationSlotArtifact, ValidationSlotLease,
    ValidationSlotRequest, ValidationSlotStore, ValidationSourceProvenance,
    normalize_available_source, normalize_beads_json, normalize_doctor_json,
    normalize_git_status_text, normalize_headroom_json, normalize_rch_queue_text,
    normalize_unavailable_source,
};
use serde::Deserialize;
use serde::Serialize;
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const START: &str = "2026-05-14T07:00:00Z";
const HEARTBEAT: &str = "2026-05-14T07:05:00Z";
const PLAN_AT: &str = "2026-05-14T08:30:00Z";
const RUNPACK_SCHEMA: &str = "pi.swarm.operator_runpack.v1";
const DOCTOR_VALIDATION_BROKER_SCHEMA: &str = "pi.doctor.validation_broker_posture.v1";
const E2E_EVENT_SCHEMA: &str = "pi.validation_broker.e2e.event.v1";
const E2E_MANIFEST_SCHEMA: &str = "pi.validation_broker.e2e.artifact_manifest.v1";

#[derive(Debug, Deserialize)]
struct FaultCorpus {
    schema: String,
    event_log_path: String,
    scenarios: Vec<FaultScenario>,
}

#[derive(Debug, Deserialize)]
struct FaultScenario {
    scenario_id: String,
    faults: Vec<String>,
    request: FaultRequest,
    inputs: FaultInputs,
    #[serde(default)]
    slot_store: Vec<FaultSlot>,
    #[serde(default)]
    policy: FaultPolicy,
    artifact_manifest: Vec<FaultArtifactManifestEntry>,
    expected: FaultExpected,
}

#[derive(Debug, Deserialize)]
struct FaultRequest {
    slot_id: String,
    command_class: String,
    #[serde(default)]
    runner: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FaultInputs {
    rch: String,
    cargo_headroom: String,
    scratch_headroom: String,
    doctor: String,
    git: String,
    beads: String,
    agent_mail: String,
}

#[derive(Debug, Default, Deserialize)]
struct FaultPolicy {
    allow_narrow_scope: Option<bool>,
    reuse_required: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct FaultSlot {
    slot_id: String,
    state: String,
    equivalence: String,
    expires_at_utc: String,
}

#[derive(Debug, Deserialize)]
struct FaultArtifactManifestEntry {
    path: String,
    artifact_schema: String,
    evidence_kind: String,
}

#[derive(Debug, Deserialize)]
struct FaultExpected {
    decision: String,
    confidence: String,
    #[serde(default)]
    reasons: Vec<String>,
    #[serde(default)]
    required_actions: Vec<String>,
    #[serde(default)]
    reusable_slot: Option<String>,
    #[serde(default)]
    coalesced_artifacts: Option<usize>,
    #[serde(default)]
    source_statuses: BTreeMap<String, String>,
    #[serde(default)]
    policy: BTreeMap<String, Value>,
    #[serde(default)]
    rejected_reusable_slots: Vec<FaultRejectedReusableSlot>,
}

#[derive(Debug, Deserialize)]
struct FaultRejectedReusableSlot {
    slot_id: String,
    reasons: Vec<String>,
}

#[derive(Debug)]
struct ScenarioArtifacts {
    scenario_dir: PathBuf,
    request_path: PathBuf,
    inputs_path: PathBuf,
    policy_path: PathBuf,
    store_path: PathBuf,
    plan_path: PathBuf,
    source_paths: Vec<(String, PathBuf)>,
}

fn test_error(message: impl Into<String>) -> Box<dyn Error> {
    io::Error::other(message.into()).into()
}

fn require(condition: bool, message: impl Into<String>) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(test_error(message))
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_pi"))
}

fn test_temp_dir() -> Result<TempDir, io::Error> {
    let root = repo_root().join("target").join("validation-broker-e2e-tmp");
    fs::create_dir_all(&root)?;
    tempfile::Builder::new()
        .prefix("validation-broker-e2e-")
        .tempdir_in(root)
}

fn fixture_path(path: &str) -> PathBuf {
    repo_root().join(path)
}

fn path_str(path: &Path) -> TestResult<&str> {
    path.to_str()
        .ok_or_else(|| test_error(format!("path is not UTF-8: {}", path.display())))
}

fn output_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn read_json(path: &Path) -> TestResult<Value> {
    let raw = fs::read_to_string(path)?;
    serde_json::from_str(&raw).map_err(Into::into)
}

fn write_json(path: &Path, value: &impl Serialize) -> TestResult {
    fs::write(path, serde_json::to_string_pretty(value)?)?;
    Ok(())
}

fn write_text(path: &Path, text: &str) -> TestResult {
    fs::write(path, text)?;
    Ok(())
}

fn run_output(mut command: Command, label: &str) -> TestResult<Output> {
    let output = command.output()?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(test_error(format!(
            "{label} failed\nstdout:\n{}\nstderr:\n{}",
            output_text(&output.stdout),
            output_text(&output.stderr)
        )))
    }
}

#[test]
fn swarm_runpack_freshness_script_self_test_passes() -> TestResult {
    let output = run_output(
        {
            let mut command = Command::new("python3");
            command
                .current_dir(repo_root())
                .args(["scripts/check_swarm_runpack_freshness.py", "--self-test"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            command
        },
        "check_swarm_runpack_freshness_self_test",
    )?;
    require(
        output_text(&output.stdout).contains("SELF-TEST PASS"),
        "freshness script self-test should report PASS",
    )?;
    Ok(())
}

#[test]
fn swarm_runpack_freshness_runpack_smoke_passes() -> TestResult {
    let output = run_output(
        {
            let mut command = Command::new("python3");
            command
                .current_dir(repo_root())
                .args([
                    "scripts/check_swarm_runpack_freshness.py",
                    "--run-runpack-smoke",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            command
        },
        "check_swarm_runpack_freshness_runpack_smoke",
    )?;
    require(
        output_text(&output.stdout).contains("RUNPACK-SMOKE PASS"),
        "freshness runpack smoke should rebuild and verify the runpack",
    )?;
    Ok(())
}

#[test]
fn extension_conformance_triage_script_self_test_passes() -> TestResult {
    let output = run_output(
        {
            let mut command = Command::new("python3");
            command
                .current_dir(repo_root())
                .args([
                    "scripts/summarize_ext_conformance_failures.py",
                    "--self-test",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            command
        },
        "summarize_ext_conformance_failures_self_test",
    )?;
    require(
        output_text(&output.stdout).contains("SELF-TEST PASS"),
        "extension conformance triage script self-test should report PASS",
    )?;
    Ok(())
}

fn run_pi(args: &[String], label: &str) -> TestResult<Output> {
    let mut command = Command::new(binary_path()); // ubs:ignore Cargo provides this test binary path.
    command
        .current_dir(repo_root())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_output(command, label)
}

fn run_pi_allow_failure(args: &[String]) -> TestResult<Output> {
    let output = Command::new(binary_path()) // ubs:ignore Cargo provides this test binary path.
        .current_dir(repo_root())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    Ok(output)
}

fn run_doctor_json(store_path: &Path, out_json: &Path) -> TestResult<Value> {
    let target_dir = out_json
        .parent()
        .ok_or_else(|| test_error("doctor output has no parent"))?
        .join("target");
    let tmpdir = out_json
        .parent()
        .ok_or_else(|| test_error("doctor output has no parent"))?
        .join("tmp");
    fs::create_dir_all(&target_dir)?;
    fs::create_dir_all(&tmpdir)?;

    let mut command = Command::new(binary_path()); // ubs:ignore Cargo provides this test binary path.
    command
        .current_dir(repo_root())
        .args(["doctor", "--only", "swarm", "--format", "json"])
        .env("PI_VALIDATION_BROKER_STORE", store_path)
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("TMPDIR", &tmpdir)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("GROQ_API_KEY")
        .env_remove("KIMI_API_KEY")
        .env_remove("AZURE_OPENAI_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = command.output()?;
    let code = output.status.code();
    require(
        matches!(code, Some(0 | 1)),
        format!(
            "doctor exited with {code:?}\nstdout:\n{}\nstderr:\n{}",
            output_text(&output.stdout),
            output_text(&output.stderr)
        ),
    )?;
    fs::write(out_json, &output.stdout)?;
    serde_json::from_slice(&output.stdout).map_err(Into::into)
}

fn load_fault_corpus() -> TestResult<FaultCorpus> {
    let raw = fs::read_to_string(fixture_path(
        "tests/golden_corpus/validation_broker/fault_corpus.json",
    ))?;
    serde_json::from_str(&raw).map_err(Into::into)
}

fn fault_event_scenario_ids(event_log_path: &str) -> TestResult<BTreeSet<String>> {
    let raw = fs::read_to_string(fixture_path(event_log_path))?;
    let mut ids = BTreeSet::new();
    for (index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(line).map_err(|err| {
            test_error(format!(
                "fault event line {} is malformed JSON: {err}",
                index + 1
            ))
        })?;
        require(
            event.get("schema").and_then(Value::as_str)
                == Some("pi.validation_broker.fault_event.v1"),
            format!("fault event line {} has wrong schema", index + 1),
        )?;
        let scenario_id = event
            .get("scenario_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                test_error(format!(
                    "fault event line {} missing scenario_id",
                    index + 1
                ))
            })?;
        ids.insert(scenario_id.to_string());
    }
    Ok(ids)
}

fn base_request(slot_id: &str) -> ValidationSlotRequest {
    let mut environment = BTreeMap::new();
    environment.insert(
        "CARGO_TARGET_DIR".to_string(),
        "/data/tmp/pi_agent_rust_cargo/e2e/target".to_string(),
    );
    environment.insert(
        "TMPDIR".to_string(),
        "/data/tmp/pi_agent_rust_cargo/e2e/tmp".to_string(),
    );

    ValidationSlotRequest {
        slot_id: slot_id.to_string(),
        owner_agent: "Codex".to_string(),
        bead_id: "bd-gusp4.8".to_string(),
        command: vec![
            "rch".to_string(),
            "exec".to_string(),
            "--".to_string(),
            "cargo".to_string(),
            "check".to_string(),
            "--all-targets".to_string(),
        ],
        command_class: "cargo_check".to_string(),
        cwd: "/data/projects/pi_agent_rust".to_string(),
        git_head: "validation-broker-e2e-head".to_string(),
        feature_flags: vec!["default".to_string()],
        target_dir: "/data/tmp/pi_agent_rust_cargo/e2e/target".to_string(),
        tmpdir: "/data/tmp/pi_agent_rust_cargo/e2e/tmp".to_string(),
        runner: "rch_required".to_string(),
        rust_toolchain: Some("nightly".to_string()),
        rch_job_id: Some("rch-job-validation-broker-e2e".to_string()),
        environment,
        expected_artifacts: vec![ValidationSlotArtifact {
            path: "target/debug/deps/pi.d".to_string(),
            sha256: None,
            schema: Some("cargo_metadata".to_string()),
        }],
        artifact_schema: Some("cargo_check_result.v1".to_string()),
        artifact_hash: Some("artifact-hash-1".to_string()),
    }
}

fn fault_request(request: &FaultRequest) -> ValidationSlotRequest {
    let mut slot_request = base_request(&request.slot_id);
    slot_request
        .command_class
        .clone_from(&request.command_class);
    slot_request.command = fault_command(&request.command_class);
    if let Some(runner) = &request.runner {
        slot_request.runner.clone_from(runner);
    }
    slot_request
}

fn fault_command(command_class: &str) -> Vec<String> {
    match command_class {
        "cargo_clippy" => vec![
            "rch".to_string(),
            "exec".to_string(),
            "--".to_string(),
            "cargo".to_string(),
            "clippy".to_string(),
            "--all-targets".to_string(),
            "--".to_string(),
            "-D".to_string(),
            "warnings".to_string(),
        ],
        "ubs_staged" => vec![
            "ubs".to_string(),
            "--staged".to_string(),
            "--only=rust".to_string(),
            ".".to_string(),
        ],
        _ => vec![
            "rch".to_string(),
            "exec".to_string(),
            "--".to_string(),
            "cargo".to_string(),
            "check".to_string(),
            "--all-targets".to_string(),
        ],
    }
}

fn policy_for(scenario: &FaultScenario) -> ValidationAdmissionPolicy {
    let mut policy = ValidationAdmissionPolicy::default();
    if let Some(allow_narrow_scope) = scenario.policy.allow_narrow_scope {
        policy.allow_narrow_scope = allow_narrow_scope;
    }
    if let Some(reuse_required) = scenario.policy.reuse_required {
        policy.reuse_required = reuse_required;
    }
    policy
}

fn materialize_scenario(root: &Path, scenario: &FaultScenario) -> TestResult<ScenarioArtifacts> {
    let scenario_dir = root.join(&scenario.scenario_id);
    fs::create_dir_all(&scenario_dir)?;

    let request = fault_request(&scenario.request);
    let context = ValidationAdmissionRequestContext {
        request_id: format!("request-{}", scenario.scenario_id),
        request: request.clone(),
        requested_at_utc: START.to_string(),
        bead_priority: 4,
    };
    let request_path = scenario_dir.join("request.json");
    write_json(&request_path, &context)?;

    let source_paths = write_source_artifacts(&scenario_dir, &scenario.inputs)?;
    let inputs = build_input_snapshot(&scenario_dir, &source_paths, &scenario.inputs)?;
    let inputs_path = scenario_dir.join("inputs.json");
    write_json(&inputs_path, &inputs)?;

    let policy_path = scenario_dir.join("policy.json");
    write_json(&policy_path, &policy_for(scenario))?;

    let store_path = scenario_dir.join("validation-slots.jsonl");
    materialize_slot_store(&store_path, &request, &scenario.slot_store)?;

    Ok(ScenarioArtifacts {
        scenario_dir: scenario_dir.clone(),
        request_path,
        inputs_path,
        policy_path,
        store_path,
        plan_path: scenario_dir.join("broker-plan.json"),
        source_paths,
    })
}

fn write_source_artifacts(
    scenario_dir: &Path,
    inputs: &FaultInputs,
) -> TestResult<Vec<(String, PathBuf)>> {
    let rch_path = scenario_dir.join("rch-status.txt");
    write_text(&rch_path, fault_rch_text(&inputs.rch))?;
    let cargo_headroom_path = scenario_dir.join("cargo-headroom.json");
    write_json(
        &cargo_headroom_path,
        &headroom_value(&inputs.cargo_headroom),
    )?;
    let scratch_headroom_path = scenario_dir.join("scratch-headroom.json");
    write_json(
        &scratch_headroom_path,
        &headroom_value(&inputs.scratch_headroom),
    )?;
    let doctor_path = scenario_dir.join("doctor-swarm.json");
    write_json(&doctor_path, &doctor_value(&inputs.doctor))?;
    let git_path = scenario_dir.join("git-status.txt");
    write_text(&git_path, fault_git_status(&inputs.git))?;
    let beads_path = scenario_dir.join("beads.json");
    write_json(&beads_path, &beads_value(&inputs.beads))?;
    let agent_mail_path = scenario_dir.join("agent-mail-status.json");
    write_json(&agent_mail_path, &agent_mail_value(&inputs.agent_mail))?;

    Ok(vec![
        ("rch".to_string(), rch_path),
        ("cargo_headroom".to_string(), cargo_headroom_path),
        ("scratch_headroom".to_string(), scratch_headroom_path),
        ("doctor".to_string(), doctor_path),
        ("git".to_string(), git_path),
        ("beads".to_string(), beads_path),
        ("agent_mail".to_string(), agent_mail_path),
    ])
}

fn build_input_snapshot(
    scenario_dir: &Path,
    source_paths: &[(String, PathBuf)],
    inputs: &FaultInputs,
) -> TestResult<ValidationBrokerInputSnapshot> {
    let source_path = |source: &str| -> TestResult<&Path> {
        source_paths
            .iter()
            .find_map(|(id, path)| (id == source).then_some(path.as_path()))
            .ok_or_else(|| test_error(format!("missing source artifact {source}")))
    };

    let rch = normalize_rch_queue_text(
        provenance("rch", source_path("rch")?)?,
        &fs::read_to_string(source_path("rch")?)?,
    )?;
    let cargo_headroom = normalize_headroom_json(
        provenance("cargo_headroom", source_path("cargo_headroom")?)?,
        &read_json(source_path("cargo_headroom")?)?,
    )?;
    let scratch_headroom = normalize_headroom_json(
        provenance("scratch_headroom", source_path("scratch_headroom")?)?,
        &read_json(source_path("scratch_headroom")?)?,
    )?;
    let doctor = normalize_doctor_json(
        provenance("doctor", source_path("doctor")?)?,
        &read_json(source_path("doctor")?)?,
    )?;
    let git = normalize_git_status_text(
        provenance("git", source_path("git")?)?,
        "validation-broker-e2e-head",
        &fs::read_to_string(source_path("git")?)?,
    )?;
    let beads = normalize_beads_json(
        provenance("beads", source_path("beads")?)?,
        &read_json(source_path("beads")?)?,
        PLAN_AT,
        3600,
    )?;
    let agent_mail = match inputs.agent_mail.as_str() {
        "available" => {
            normalize_available_source(provenance("agent_mail", source_path("agent_mail")?)?)?
        }
        "unavailable" => normalize_unavailable_source(
            provenance("agent_mail", source_path("agent_mail")?)?,
            "agent_mail_schema_missing",
        )?,
        other => {
            return Err(test_error(format!(
                "unknown agent_mail fixture state: {other}"
            )));
        }
    };

    ValidationBrokerInputSnapshot::from_parts(ValidationBrokerInputParts {
        captured_at_utc: PLAN_AT.to_string(),
        rch,
        cargo_headroom,
        doctor,
        git,
        beads,
        scratch_headroom,
        agent_mail,
    })
    .map_err(|err: pi::error::Error| {
        test_error(format!(
            "input snapshot failed for {}: {err}",
            scenario_dir.display()
        ))
    })
}

fn provenance(source: &str, path: &Path) -> TestResult<ValidationSourceProvenance> {
    ValidationSourceProvenance::new(
        source,
        vec![source.to_string(), "--json".to_string()],
        "/data/projects/pi_agent_rust",
        PLAN_AT,
        Some(path.display().to_string()),
    )
    .map_err(Into::into)
}

fn fault_rch_text(state: &str) -> &'static str {
    match state {
        "healthy" => {
            "Build Queue\n  - 1 Active Build(s)\n  - 0 Queued Build(s)\nWorker Availability\n  -> 4 / 18 slots free\n"
        }
        "saturated" => {
            "Build Queue\n  - 5 Active Build(s)\n  - 2 Queued Build(s)\nWorker Availability\n  -> 0 / 18 slots free\n"
        }
        "local_fallback" => {
            "Build Queue\n  - 1 Active Build(s)\n  - 0 Queued Build(s)\nWorker Availability\n  -> 4 / 18 slots free\nRCH fails open; command may run with local fallback\n"
        }
        _ => "",
    }
}

fn headroom_value(state: &str) -> Value {
    match state {
        "low" => json!({"available_bytes": 5_000_u64, "required_bytes": 10_000_u64}),
        _ => json!({"available_bytes": 50_000_u64, "required_bytes": 10_000_u64}),
    }
}

fn doctor_value(state: &str) -> Value {
    match state {
        "failed" => json!({"checks": [{"name": "scratch", "status": "fail"}]}),
        _ => json!({"checks": [{"name": "scratch", "status": "ok"}]}),
    }
}

fn fault_git_status(state: &str) -> &'static str {
    match state {
        "dirty" => "## main...origin/main\n M src/validation_broker.rs\n",
        _ => "## main...origin/main\n",
    }
}

fn beads_value(state: &str) -> Value {
    match state {
        "stale_in_progress" => json!({"issues": [
            {"id": "bd-stale", "status": "in_progress", "assignee": "AbsentAgent", "updated_at": START}
        ]}),
        _ => json!({"issues": []}),
    }
}

fn agent_mail_value(state: &str) -> Value {
    match state {
        "unavailable" => json!({
            "schema": "pi.agent_mail.robot_status.v1",
            "generated_at": PLAN_AT,
            "status": "error",
            "health_level": "red",
            "issue": "database schema missing required tables"
        }),
        _ => json!({
            "schema": "pi.agent_mail.robot_status.v1",
            "generated_at": PLAN_AT,
            "status": "ok",
            "health_level": "green"
        }),
    }
}

fn materialize_slot_store(
    store_path: &Path,
    request: &ValidationSlotRequest,
    slots: &[FaultSlot],
) -> TestResult {
    let store = ValidationSlotStore::new(store_path);
    for slot in slots {
        let slot_request = fault_slot_request(slot, request)?;
        let mut lease = ValidationSlotLease::acquire(slot_request, START, &slot.expires_at_utc)?;
        store.append_lease("acquire", START, &lease)?;
        match slot.state.as_str() {
            "active" => {}
            "reusable" => {
                lease.mark_reusable(&request.owner_agent, HEARTBEAT, reusable_artifacts())?;
                store.append_lease("mark_reusable", HEARTBEAT, &lease)?;
            }
            other => return Err(test_error(format!("unknown fault slot state: {other}"))),
        }
    }
    Ok(())
}

fn fault_slot_request(
    slot: &FaultSlot,
    request: &ValidationSlotRequest,
) -> TestResult<ValidationSlotRequest> {
    let mut slot_request = ValidationSlotRequest {
        slot_id: slot.slot_id.clone(),
        owner_agent: request.owner_agent.clone(),
        bead_id: request.bead_id.clone(),
        command: request.command.clone(),
        command_class: request.command_class.clone(),
        cwd: request.cwd.clone(),
        git_head: request.git_head.clone(),
        feature_flags: request.feature_flags.clone(),
        target_dir: request.target_dir.clone(),
        tmpdir: request.tmpdir.clone(),
        runner: request.runner.clone(),
        rust_toolchain: request.rust_toolchain.clone(),
        rch_job_id: request.rch_job_id.clone(),
        environment: request.environment.clone(),
        expected_artifacts: request.expected_artifacts.clone(),
        artifact_schema: request.artifact_schema.clone(),
        artifact_hash: request.artifact_hash.clone(),
    };
    match slot.equivalence.as_str() {
        "matching" => {}
        "target_dir_mismatch" => {
            slot_request.target_dir = "/data/tmp/pi_agent_rust_cargo/other/target".to_string();
        }
        other => {
            return Err(test_error(format!(
                "unknown slot equivalence fixture: {other}"
            )));
        }
    }
    Ok(slot_request)
}

fn reusable_artifacts() -> Vec<ValidationSlotArtifact> {
    vec![ValidationSlotArtifact {
        path: "target/debug/deps/pi.d".to_string(),
        sha256: Some("artifact-hash-1".to_string()),
        schema: Some("cargo_check_result.v1".to_string()),
    }]
}

fn run_plan(artifacts: &ScenarioArtifacts) -> TestResult<Value> {
    let args = vec![
        "validation-broker".to_string(),
        "plan".to_string(),
        "--request".to_string(),
        path_str(&artifacts.request_path)?.to_string(),
        "--inputs".to_string(),
        path_str(&artifacts.inputs_path)?.to_string(),
        "--store".to_string(),
        path_str(&artifacts.store_path)?.to_string(),
        "--policy".to_string(),
        path_str(&artifacts.policy_path)?.to_string(),
        "--format".to_string(),
        "json".to_string(),
        "--out-json".to_string(),
        path_str(&artifacts.plan_path)?.to_string(),
        "--generated-at".to_string(),
        PLAN_AT.to_string(),
    ];
    run_pi(&args, "validation-broker plan")?;
    read_json(&artifacts.plan_path)
}

fn run_status(store_path: &Path, out_json: &Path) -> TestResult<Value> {
    let args = vec![
        "validation-broker".to_string(),
        "status".to_string(),
        "--store".to_string(),
        path_str(store_path)?.to_string(),
        "--out-json".to_string(),
        path_str(out_json)?.to_string(),
        "--generated-at".to_string(),
        PLAN_AT.to_string(),
    ];
    run_pi(&args, "validation-broker status")?;
    read_json(out_json)
}

fn assert_plan_matches_fault_expectation(plan: &Value, scenario: &FaultScenario) -> TestResult {
    require(
        plan.pointer("/schema").and_then(Value::as_str) == Some(VALIDATION_BROKER_CLI_PLAN_SCHEMA),
        format!("{} plan schema mismatch: {plan}", scenario.scenario_id),
    )?;
    require(
        plan.pointer("/read_only").and_then(Value::as_bool) == Some(true),
        format!("{} plan should be read-only", scenario.scenario_id),
    )?;
    require(
        plan.pointer("/guards/live_mutations")
            .and_then(Value::as_u64)
            == Some(0),
        format!("{} plan should not mutate live state", scenario.scenario_id),
    )?;
    require_eq_str(
        plan.pointer("/decision/decision").and_then(Value::as_str),
        &scenario.expected.decision,
        &scenario.scenario_id,
        "decision",
    )?;
    require_eq_str(
        plan.pointer("/decision/confidence").and_then(Value::as_str),
        &scenario.expected.confidence,
        &scenario.scenario_id,
        "confidence",
    )?;
    assert_expected_strings(
        plan.pointer("/decision/reasons").and_then(Value::as_array),
        &scenario.expected.reasons,
        &scenario.scenario_id,
        "reason",
    )?;
    assert_expected_strings(
        plan.pointer("/decision/required_actions")
            .and_then(Value::as_array),
        &scenario.expected.required_actions,
        &scenario.scenario_id,
        "required action",
    )?;
    if let Some(expected_slot) = &scenario.expected.reusable_slot {
        require_eq_str(
            plan.pointer("/decision/reusable_slot")
                .and_then(Value::as_str),
            expected_slot,
            &scenario.scenario_id,
            "reusable slot",
        )?;
    }
    if let Some(expected_count) = scenario.expected.coalesced_artifacts {
        require(
            plan.pointer("/decision/coalesced_artifacts")
                .and_then(Value::as_array)
                .is_some_and(|artifacts| artifacts.len() == expected_count),
            format!(
                "{} expected {expected_count} coalesced artifacts",
                scenario.scenario_id
            ),
        )?;
    }
    assert_expected_source_statuses(plan, scenario)?;
    assert_expected_policy_fields(plan, scenario)?;
    assert_expected_rejections(plan, scenario)
}

fn require_eq_str(
    actual: Option<&str>,
    expected: &str,
    scenario_id: &str,
    label: &str,
) -> TestResult {
    require(
        actual == Some(expected),
        format!("{scenario_id} {label} mismatch: expected {expected:?}, got {actual:?}"),
    )
}

fn assert_expected_strings(
    actual: Option<&Vec<Value>>,
    expected: &[String],
    scenario_id: &str,
    label: &str,
) -> TestResult {
    let actual = actual.ok_or_else(|| test_error(format!("{scenario_id} missing {label}s")))?;
    for expected_value in expected {
        require(
            actual
                .iter()
                .any(|value| value.as_str() == Some(expected_value.as_str())),
            format!("{scenario_id} missing {label} {expected_value}"),
        )?;
    }
    Ok(())
}

fn assert_expected_source_statuses(plan: &Value, scenario: &FaultScenario) -> TestResult {
    let Some(statuses) = plan
        .pointer("/decision/source_statuses")
        .and_then(Value::as_array)
    else {
        return Err(test_error(format!(
            "{} missing source statuses",
            scenario.scenario_id
        )));
    };
    for (source_id, expected_state) in &scenario.expected.source_statuses {
        let Some(status) = statuses.iter().find(|value| {
            value.get("source_id").and_then(Value::as_str) == Some(source_id.as_str())
        }) else {
            return Err(test_error(format!(
                "{} missing source status {source_id}",
                scenario.scenario_id
            )));
        };
        require(
            status.get("state").and_then(Value::as_str) == Some(expected_state.as_str()),
            format!("{} source {source_id} state mismatch", scenario.scenario_id),
        )?;
    }
    Ok(())
}

fn assert_expected_policy_fields(plan: &Value, scenario: &FaultScenario) -> TestResult {
    for (field, expected_value) in &scenario.expected.policy {
        let pointer = format!("/decision/policy/{field}");
        let actual = plan.pointer(&pointer).ok_or_else(|| {
            test_error(format!(
                "{} missing policy field {field}",
                scenario.scenario_id
            ))
        })?;
        require(
            actual == expected_value,
            format!(
                "{} policy {field} mismatch: expected {expected_value}, got {actual}",
                scenario.scenario_id
            ),
        )?;
    }
    Ok(())
}

fn assert_expected_rejections(plan: &Value, scenario: &FaultScenario) -> TestResult {
    let rejections = plan
        .pointer("/decision/rejected_reusable_slots")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            test_error(format!(
                "{} missing rejected reusable slots",
                scenario.scenario_id
            ))
        })?;
    require(
        rejections.len() == scenario.expected.rejected_reusable_slots.len(),
        format!("{} rejected slot count mismatch", scenario.scenario_id),
    )?;
    for expected in &scenario.expected.rejected_reusable_slots {
        let Some(actual) = rejections.iter().find(|value| {
            value.get("slot_id").and_then(Value::as_str) == Some(expected.slot_id.as_str())
        }) else {
            return Err(test_error(format!(
                "{} missing rejected slot {}",
                scenario.scenario_id, expected.slot_id
            )));
        };
        let reasons = actual
            .get("reasons")
            .and_then(Value::as_array)
            .ok_or_else(|| test_error("rejected slot reasons missing"))?;
        for reason in &expected.reasons {
            require(
                reasons
                    .iter()
                    .any(|value| value.as_str() == Some(reason.as_str())),
                format!(
                    "{} rejected slot {} missing reason {reason}",
                    scenario.scenario_id, expected.slot_id
                ),
            )?;
        }
    }
    Ok(())
}

fn validate_fault_manifest(scenario: &FaultScenario) -> TestResult {
    require(
        !scenario.faults.is_empty(),
        format!("{} should list faults", scenario.scenario_id),
    )?;
    require(
        !scenario.artifact_manifest.is_empty(),
        format!("{} should list artifact evidence", scenario.scenario_id),
    )?;
    for artifact in &scenario.artifact_manifest {
        require(
            fixture_path(&artifact.path).exists(),
            format!(
                "{} missing artifact {}",
                scenario.scenario_id, artifact.path
            ),
        )?;
        require(
            !artifact.artifact_schema.trim().is_empty(),
            format!("{} artifact schema missing", scenario.scenario_id),
        )?;
        require(
            !artifact.evidence_kind.trim().is_empty(),
            format!("{} artifact evidence kind missing", scenario.scenario_id),
        )?;
    }
    Ok(())
}

fn decision_key(plan: &Value) -> TestResult<String> {
    plan.pointer("/decision/decision")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| test_error("plan missing decision"))
}

fn write_event_log(events_path: &Path, events: &[Value]) -> TestResult {
    let mut content = String::new();
    for event in events {
        content.push_str(&serde_json::to_string(event)?);
        content.push('\n');
    }
    fs::write(events_path, content)?;
    Ok(())
}

fn validation_broker_finding(report: &Value) -> TestResult<&Value> {
    let findings = report
        .get("findings")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("doctor report missing findings"))?;
    findings
        .iter()
        .find(|finding| {
            finding
                .get("data")
                .and_then(|data| data.get("schema"))
                .and_then(Value::as_str)
                == Some(DOCTOR_VALIDATION_BROKER_SCHEMA)
        })
        .ok_or_else(|| test_error("doctor report missing validation broker finding"))
}

fn write_runpack_sources(root: &Path, doctor_path: &Path, broker_status_path: &Path) -> TestResult {
    write_json(
        &root.join("claim-readiness.json"),
        &json!({
            "schema": "pi.swarm.claim_readiness_report.v1",
            "overall_status": "ready",
            "max_age_days": 14,
            "artifact_statuses": [{
                "id": "validation_broker_e2e",
                "category": "validation",
                "status": "ready",
                "release_blocking": false,
                "issue_kinds": []
            }],
            "stale_claims": {"summary": {"stale_count": 0}}
        }),
    )?;
    write_json(
        &root.join("smoke-summary.json"),
        &json!({
            "schema": "pi.swarm.smoke_harness.v1",
            "status": "pass",
            "correlation_id": "validation-broker-e2e",
            "reservation_ids": [],
            "failed_scenarios": [],
            "scenarios": {"validation_broker_e2e": {"status": "pass"}},
            "artifacts": {"summary_json": root.join("smoke-summary.json").display().to_string()},
            "artifact_manifest": [{
                "id": "validation_broker_e2e_events",
                "path": root.join("validation-broker-e2e-events.jsonl").display().to_string(),
                "size_bytes": 1,
                "sha256": "a".repeat(64)
            }]
        }),
    )?;
    write_json(
        &root.join("activity-digest.json"),
        &json!({
            "schema": "pi.swarm.activity_digest.v1",
            "saturation": {
                "saturated": false,
                "signals": [],
                "reasons": [],
                "evidence_pointers": []
            },
            "recommendations": [{"mode": "validation-broker-e2e"}]
        }),
    )?;
    write_json(
        &root.join("cargo-admission.json"),
        &json!({
            "schema": "pi.cargo_headroom.admission.v1",
            "decision": "admit",
            "reason": "validation_broker_e2e_fixture",
            "requested_runner": "rch",
            "resolved_runner": "rch",
            "command_class": "heavy",
            "allow_local_fallback": false,
            "cargo_target_dir": "/data/tmp/pi_agent_rust_cargo/e2e/target",
            "tmpdir": "/data/tmp/pi_agent_rust_cargo/e2e/tmp",
            "rch_queue_forecast": {
                "schema": "pi.cargo_headroom.rch_queue_forecast.v1",
                "status": "ok",
                "recommended_action": "proceed",
                "reason": "validation_broker_e2e",
                "slot_pressure": "available",
                "queue_depth": 0,
                "active_builds": 0,
                "queued_builds": 0,
                "slots_available": 8,
                "slots_total": 8,
                "workers_healthy": 8,
                "workers_total": 8,
                "estimated_wait_seconds": 0
            }
        }),
    )?;
    write_json(
        &root.join("beads.json"),
        &json!({"issues": [{
            "id": "bd-gusp4.8",
            "title": "Add no-mock validation broker E2E harness",
            "status": "in_progress",
            "priority": 4,
            "updated_at": PLAN_AT
        }]}),
    )?;
    write_json(
        &root.join("git-status.json"),
        &json!({
            "schema": "pi.swarm.git_context.v1",
            "generated_at": PLAN_AT,
            "branch": "main",
            "head": "validation-broker-e2e-head",
            "upstream": {"name": "origin/main", "ahead": 0, "behind": 0, "status": "ok"},
            "porcelain_lines": [],
            "recent_commits": ["validation broker e2e fixture"],
            "recent_remote_commits": ["origin/main validation broker e2e fixture"]
        }),
    )?;
    require(doctor_path.exists(), "doctor JSON source exists")?;
    require(
        broker_status_path.exists(),
        "validation broker status JSON source exists",
    )
}

fn run_runpack(root: &Path, doctor_path: &Path, broker_status_path: &Path) -> TestResult<Value> {
    write_runpack_sources(root, doctor_path, broker_status_path)?;
    let runpack_path = root.join("operator-runpack.json");
    let mut command = Command::new("python3");
    command
        .current_dir(repo_root())
        .args([
            "scripts/build_swarm_operator_runpack.py",
            "--doctor-json",
            path_str(doctor_path)?,
            "--claim-readiness-json",
            path_str(&root.join("claim-readiness.json"))?,
            "--smoke-summary-json",
            path_str(&root.join("smoke-summary.json"))?,
            "--activity-digest-json",
            path_str(&root.join("activity-digest.json"))?,
            "--cargo-admission-json",
            path_str(&root.join("cargo-admission.json"))?,
            "--beads-json",
            path_str(&root.join("beads.json"))?,
            "--git-status-file",
            path_str(&root.join("git-status.json"))?,
            "--validation-broker-json",
            path_str(broker_status_path)?,
            "--out-json",
            path_str(&runpack_path)?,
            "--generated-at",
            PLAN_AT,
            "--max-items",
            "6",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_output(command, "build_swarm_operator_runpack")?;
    read_json(&runpack_path)
}

fn write_artifact_manifest(
    path: &Path,
    events_path: &Path,
    runpack_path: &Path,
    doctor_path: &Path,
    broker_status_path: &Path,
    scenario_artifacts: &[ScenarioArtifacts],
) -> TestResult {
    let decision_paths = scenario_artifacts
        .iter()
        .map(|artifacts| artifacts.plan_path.display().to_string())
        .collect::<Vec<_>>();
    let source_paths = scenario_artifacts
        .iter()
        .flat_map(|artifacts| {
            artifacts.source_paths.iter().map(|(source, path)| {
                json!({
                    "source": source,
                    "scenario_dir": artifacts.scenario_dir.display().to_string(),
                    "path": path.display().to_string()
                })
            })
        })
        .collect::<Vec<_>>();
    write_json(
        path,
        &json!({
            "schema": E2E_MANIFEST_SCHEMA,
            "generated_at_utc": PLAN_AT,
            "source_corpus": "tests/golden_corpus/validation_broker/fault_corpus.json",
            "source_events": "tests/golden_corpus/validation_broker/fault_events.jsonl",
            "artifacts": [
                {"id": "events_jsonl", "path": events_path.display().to_string(), "schema": E2E_EVENT_SCHEMA},
                {"id": "doctor_projection", "path": doctor_path.display().to_string(), "schema": "pi.doctor.report.v1"},
                {"id": "operator_runpack", "path": runpack_path.display().to_string(), "schema": RUNPACK_SCHEMA},
                {"id": "validation_broker_status", "path": broker_status_path.display().to_string(), "schema": VALIDATION_BROKER_CLI_STATUS_SCHEMA}
            ],
            "broker_decision_paths": decision_paths,
            "source_artifact_paths": source_paths
        }),
    )
}

#[test]
fn validation_broker_fault_corpus_runs_through_cli_doctor_and_runpack() -> TestResult {
    let temp = test_temp_dir()?;
    let corpus = load_fault_corpus()?;
    require(
        corpus.schema == "pi.validation_broker.fault_corpus.v1",
        "fault corpus schema",
    )?;
    let event_scenario_ids = fault_event_scenario_ids(&corpus.event_log_path)?;

    let mut seen_decisions = BTreeSet::new();
    let mut e2e_events = Vec::new();
    let mut scenario_artifacts = Vec::new();
    let mut stale_store_for_projection = None;

    for scenario in &corpus.scenarios {
        require(
            event_scenario_ids.contains(&scenario.scenario_id),
            format!("{} has JSONL fault evidence", scenario.scenario_id),
        )?;
        validate_fault_manifest(scenario)?;

        let artifacts = materialize_scenario(temp.path(), scenario)?;
        let plan = run_plan(&artifacts)?;
        assert_plan_matches_fault_expectation(&plan, scenario)?;
        let decision = decision_key(&plan)?;
        seen_decisions.insert(decision.clone());
        if scenario.scenario_id == "stale_pre_commit_ubs" {
            stale_store_for_projection = Some(artifacts.store_path.clone());
        }
        e2e_events.push(json!({
            "schema": E2E_EVENT_SCHEMA,
            "scenario_id": scenario.scenario_id,
            "observed_at_utc": PLAN_AT,
            "decision": decision,
            "source": "validation_broker_cli",
            "plan_path": artifacts.plan_path.display().to_string(),
            "store_path": artifacts.store_path.display().to_string(),
            "source_artifact_count": artifacts.source_paths.len(),
            "read_only": plan.pointer("/read_only").and_then(Value::as_bool).unwrap_or(false),
            "live_mutations": plan.pointer("/guards/live_mutations").and_then(Value::as_u64).unwrap_or(1)
        }));
        scenario_artifacts.push(artifacts);
    }

    for required_decision in ["allow", "wait", "coalesce", "narrow", "stale_recover"] {
        require(
            seen_decisions.contains(required_decision),
            format!("E2E corpus missing decision {required_decision}"),
        )?;
    }

    let events_path = temp.path().join("validation-broker-e2e-events.jsonl");
    write_event_log(&events_path, &e2e_events)?;
    require(events_path.exists(), "E2E event log exists")?;

    let projection_store = stale_store_for_projection
        .ok_or_else(|| test_error("stale scenario store was not materialized"))?;
    let broker_status_path = temp.path().join("validation-broker-status.json");
    let broker_status = run_status(&projection_store, &broker_status_path)?;
    require(
        broker_status.pointer("/schema").and_then(Value::as_str)
            == Some(VALIDATION_BROKER_CLI_STATUS_SCHEMA),
        "validation broker status schema",
    )?;
    require(
        broker_status
            .pointer("/guards/live_mutations")
            .and_then(Value::as_u64)
            == Some(0),
        "validation broker status remains read-only",
    )?;

    let doctor_path = temp.path().join("doctor-validation-broker.json");
    let doctor = run_doctor_json(&projection_store, &doctor_path)?;
    let finding = validation_broker_finding(&doctor)?;
    let data = finding
        .get("data")
        .ok_or_else(|| test_error("validation broker doctor finding missing data"))?;
    require(
        data.pointer("/guards/no_live_mutation")
            .and_then(Value::as_bool)
            == Some(true),
        "Doctor validation broker projection has no-live-mutation guard",
    )?;
    require(
        data.pointer("/stale_build_warnings/count")
            .and_then(Value::as_u64)
            .is_some_and(|count| count >= 1),
        "Doctor projection reports stale validation slot warnings",
    )?;

    let runpack = run_runpack(temp.path(), &doctor_path, &broker_status_path)?;
    require(
        runpack.pointer("/schema").and_then(Value::as_str) == Some(RUNPACK_SCHEMA),
        "operator runpack schema",
    )?;
    require(
        runpack
            .pointer("/validation_broker/schema")
            .and_then(Value::as_str)
            == Some(VALIDATION_BROKER_CLI_STATUS_SCHEMA),
        "operator runpack carries validation broker status",
    )?;
    require(
        runpack
            .pointer("/doctor_swarm/validation_broker/stale_build_warnings/count")
            .and_then(Value::as_u64)
            .is_some_and(|count| count >= 1),
        "operator runpack carries Doctor validation broker projection",
    )?;

    let manifest_path = temp.path().join("validation-broker-e2e-manifest.json");
    write_artifact_manifest(
        &manifest_path,
        &events_path,
        &temp.path().join("operator-runpack.json"),
        &doctor_path,
        &broker_status_path,
        &scenario_artifacts,
    )?;
    let manifest = read_json(&manifest_path)?;
    require(
        manifest.pointer("/schema").and_then(Value::as_str) == Some(E2E_MANIFEST_SCHEMA),
        "E2E manifest schema",
    )?;
    require(
        manifest
            .pointer("/broker_decision_paths")
            .and_then(Value::as_array)
            .is_some_and(|paths| paths.len() == corpus.scenarios.len()),
        "E2E manifest records every broker decision path",
    )?;
    Ok(())
}

#[test]
fn validation_broker_e2e_fails_closed_on_missing_and_malformed_artifacts() -> TestResult {
    let temp = test_temp_dir()?;
    let corpus = load_fault_corpus()?;
    let scenario = corpus
        .scenarios
        .iter()
        .find(|scenario| scenario.scenario_id == "agent_mail_unavailable")
        .ok_or_else(|| test_error("missing agent_mail_unavailable scenario"))?;
    let artifacts = materialize_scenario(temp.path(), scenario)?;

    let missing_inputs = artifacts.scenario_dir.join("missing-inputs.json");
    let missing_output = run_pi_allow_failure(&[
        "validation-broker".to_string(),
        "plan".to_string(),
        "--request".to_string(),
        path_str(&artifacts.request_path)?.to_string(),
        "--inputs".to_string(),
        path_str(&missing_inputs)?.to_string(),
        "--store".to_string(),
        path_str(&artifacts.store_path)?.to_string(),
        "--policy".to_string(),
        path_str(&artifacts.policy_path)?.to_string(),
        "--format".to_string(),
        "json".to_string(),
        "--generated-at".to_string(),
        PLAN_AT.to_string(),
    ])?;
    require(
        !missing_output.status.success(),
        "plan should fail when a source file is missing",
    )?;

    let malformed_inputs = artifacts.scenario_dir.join("malformed-inputs.json");
    write_text(&malformed_inputs, "{not-json")?;
    let malformed_output = run_pi_allow_failure(&[
        "validation-broker".to_string(),
        "plan".to_string(),
        "--request".to_string(),
        path_str(&artifacts.request_path)?.to_string(),
        "--inputs".to_string(),
        path_str(&malformed_inputs)?.to_string(),
        "--store".to_string(),
        path_str(&artifacts.store_path)?.to_string(),
        "--policy".to_string(),
        path_str(&artifacts.policy_path)?.to_string(),
        "--format".to_string(),
        "json".to_string(),
        "--generated-at".to_string(),
        PLAN_AT.to_string(),
    ])?;
    require(
        !malformed_output.status.success(),
        "plan should fail when an input artifact is malformed",
    )?;

    let bad_broker_path = artifacts.scenario_dir.join("bad-validation-broker.json");
    write_json(
        &bad_broker_path,
        &json!({"schema": "pi.validation_broker.unknown.v1"}),
    )?;
    let doctor_path = artifacts.scenario_dir.join("doctor.json");
    write_json(&doctor_path, &json!({"overall": "pass", "findings": []}))?;
    write_runpack_sources(&artifacts.scenario_dir, &doctor_path, &bad_broker_path)?;
    let mut command = Command::new("python3");
    command
        .current_dir(repo_root())
        .args([
            "scripts/build_swarm_operator_runpack.py",
            "--doctor-json",
            path_str(&doctor_path)?,
            "--claim-readiness-json",
            path_str(&artifacts.scenario_dir.join("claim-readiness.json"))?,
            "--smoke-summary-json",
            path_str(&artifacts.scenario_dir.join("smoke-summary.json"))?,
            "--activity-digest-json",
            path_str(&artifacts.scenario_dir.join("activity-digest.json"))?,
            "--cargo-admission-json",
            path_str(&artifacts.scenario_dir.join("cargo-admission.json"))?,
            "--beads-json",
            path_str(&artifacts.scenario_dir.join("beads.json"))?,
            "--git-status-file",
            path_str(&artifacts.scenario_dir.join("git-status.json"))?,
            "--validation-broker-json",
            path_str(&bad_broker_path)?,
            "--out-json",
            path_str(&artifacts.scenario_dir.join("bad-runpack.json"))?,
            "--generated-at",
            PLAN_AT,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command.output()?;
    require(
        !output.status.success(),
        "runpack should fail closed for a malformed validation-broker artifact",
    )?;
    require(
        output_text(&output.stderr).contains("validation_broker source schema mismatch")
            || output_text(&output.stdout).contains("validation_broker source schema mismatch"),
        "runpack should explain the validation-broker schema mismatch",
    )
}
