#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use pi::validation_broker::{
    VALIDATION_BROKER_CLI_PLAN_SCHEMA, VALIDATION_BROKER_CLI_STATUS_SCHEMA,
    VALIDATION_BROKER_DECISION_SCHEMA, ValidationAdmissionPolicy,
    ValidationAdmissionRequestContext, ValidationBrokerInputParts, ValidationBrokerInputSnapshot,
    ValidationSlotArtifact, ValidationSlotLease, ValidationSlotRequest, ValidationSlotStore,
    ValidationSourceProvenance, normalize_available_source, normalize_beads_json,
    normalize_doctor_json, normalize_git_status_text, normalize_headroom_json,
    normalize_rch_queue_text, normalize_unavailable_source,
};
use serde::Serialize;
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const START: &str = "2026-05-14T07:00:00Z";
const HEARTBEAT: &str = "2026-05-14T07:05:00Z";
const EXPIRES: &str = "2026-05-14T07:30:00Z";
const PLAN_AT: &str = "2026-05-14T08:30:00Z";
const RENEWED_EXPIRES: &str = "2026-05-14T09:00:00Z";
const E2E_FIXTURE_DIR: &str = "tests/golden_corpus/validation_broker/e2e";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_pi"))
}

fn test_temp_dir() -> Result<TempDir, std::io::Error> {
    let root = repo_root().join("target").join("validation-broker-cli-tmp");
    fs::create_dir_all(&root)?;
    tempfile::Builder::new()
        .prefix("validation-broker-cli-")
        .tempdir_in(root)
}

fn test_error(message: impl Into<String>) -> Box<dyn std::error::Error> {
    Box::new(io::Error::other(message.into()))
}

fn path_str(path: &Path) -> TestResult<&str> {
    path.to_str()
        .ok_or_else(|| test_error(format!("path is not UTF-8: {}", path.display())))
}

fn e2e_fixture_path(name: &str) -> PathBuf {
    repo_root().join(E2E_FIXTURE_DIR).join(name)
}

fn read_fixture_text(name: &str) -> TestResult<String> {
    fs::read_to_string(e2e_fixture_path(name)).map_err(Into::into)
}

fn read_fixture_json(name: &str) -> TestResult<Value> {
    Ok(serde_json::from_str(&read_fixture_text(name)?)?)
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

fn run_pi(args: &[&str]) -> Result<Output, std::io::Error> {
    Command::new(binary_path()) // ubs:ignore Cargo provides this test binary path.
        .current_dir(repo_root())
        .args(args)
        .output()
}

fn run_validation_broker_plan(
    request_path: &Path,
    inputs_path: &Path,
    store_path: &Path,
    policy_path: Option<&Path>,
    out_json: &Path,
) -> Result<Output, std::io::Error> {
    let mut command = Command::new(binary_path()); // ubs:ignore Cargo provides this test binary path.
    command
        .current_dir(repo_root())
        .args(["validation-broker", "plan", "--request"])
        .arg(request_path)
        .arg("--inputs")
        .arg(inputs_path)
        .arg("--store")
        .arg(store_path);
    if let Some(path) = policy_path {
        command.arg("--policy").arg(path);
    }
    command
        .args(["--format", "json", "--out-json"])
        .arg(out_json)
        .args(["--generated-at", PLAN_AT])
        .output()
}

fn run_validation_broker_status(
    store_path: &Path,
    out_json: &Path,
) -> Result<Output, std::io::Error> {
    Command::new(binary_path()) // ubs:ignore Cargo provides this test binary path.
        .current_dir(repo_root())
        .args(["validation-broker", "status", "--store"])
        .arg(store_path)
        .args(["--format", "json", "--out-json"])
        .arg(out_json)
        .args(["--generated-at", PLAN_AT])
        .output()
}

fn run_runpack_builder(
    validation_broker_json: &Path,
    out_json: &Path,
    out_md: &Path,
) -> Result<Output, std::io::Error> {
    Command::new("python3")
        .current_dir(repo_root())
        .arg(repo_root().join("scripts/build_swarm_operator_runpack.py"))
        .arg("--doctor-json")
        .arg(e2e_fixture_path("doctor_swarm.json"))
        .arg("--claim-readiness-json")
        .arg(e2e_fixture_path("claim_readiness.json"))
        .arg("--smoke-summary-json")
        .arg(e2e_fixture_path("smoke_summary.json"))
        .arg("--activity-digest-json")
        .arg(e2e_fixture_path("activity_digest.json"))
        .arg("--cargo-admission-json")
        .arg(e2e_fixture_path("cargo_admission.json"))
        .arg("--beads-json")
        .arg(e2e_fixture_path("beads_export.json"))
        .arg("--git-status-file")
        .arg(e2e_fixture_path("git_context.json"))
        .arg("--validation-broker-json")
        .arg(validation_broker_json)
        .arg("--out-json")
        .arg(out_json)
        .arg("--out-md")
        .arg(out_md)
        .arg("--generated-at")
        .arg(PLAN_AT)
        .arg("--max-items")
        .arg("4")
        .output()
}

fn write_json(path: &Path, value: &impl Serialize) -> TestResult {
    fs::write(path, serde_json::to_string_pretty(value)?)?;
    Ok(())
}

fn write_jsonl(path: &Path, events: &[Value]) -> TestResult {
    let mut output = String::new();
    for event in events {
        output.push_str(&serde_json::to_string(event)?);
        output.push('\n');
    }
    fs::write(path, output)?;
    Ok(())
}

fn base_request(slot_id: &str) -> ValidationSlotRequest {
    static WORKSPACE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let workspace = WORKSPACE.get_or_init(|| {
        let root = tempfile::tempdir().expect("fixture workspace").keep();
        for name in ["rch", "cargo", "ubs", "rch.exe", "cargo.exe", "ubs.exe"] {
            std::fs::write(root.join(name), b"validation fixture executable") // ubs:ignore[rust.security.path-traversal] Only the six literal fixture filenames above enter this join.
                .expect("fixture tool");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(0o755)) // ubs:ignore[rust.security.path-traversal] The same six literal fixture filenames are chmodded inside their test directory.
                    .expect("executable fixture tool");
            }
        }
        root
    });
    let mut environment = BTreeMap::new();
    environment.insert("PATH".into(), workspace.to_string_lossy().into_owned());
    environment.insert(
        "CARGO_TARGET_DIR".to_string(),
        "/data/tmp/pi_agent_rust_cargo/codex/target".to_string(),
    );
    environment.insert(
        "TMPDIR".to_string(),
        "/data/tmp/pi_agent_rust_cargo/codex/tmp".to_string(),
    );

    ValidationSlotRequest {
        slot_id: slot_id.to_string(),
        owner_agent: "Codex".to_string(),
        bead_id: "bd-gusp4.5".to_string(),
        command: vec![
            "rch".to_string(),
            "exec".to_string(),
            "--".to_string(),
            "cargo".to_string(),
            "check".to_string(),
            "--all-targets".to_string(),
        ],
        command_class: "cargo_check".to_string(),
        cwd: workspace.to_string_lossy().into_owned(),
        git_head: "3048e53f3".to_string(),
        feature_flags: vec!["default".to_string()],
        target_dir: "/data/tmp/pi_agent_rust_cargo/codex/target".to_string(),
        tmpdir: "/data/tmp/pi_agent_rust_cargo/codex/tmp".to_string(),
        runner: "rch_required".to_string(),
        rust_toolchain: Some("nightly".to_string()),
        rch_job_id: None,
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

fn admission_context(slot_id: &str) -> ValidationAdmissionRequestContext {
    ValidationAdmissionRequestContext {
        request_id: "request-cli-plan".to_string(),
        request: base_request(slot_id),
        requested_at_utc: START.to_string(),
        bead_priority: 4,
    }
}

fn provenance(source: &str) -> Result<ValidationSourceProvenance, pi::error::Error> {
    ValidationSourceProvenance::new(
        source,
        vec![source.to_string(), "--json".to_string()],
        "/data/projects/pi_agent_rust",
        START,
        Some(format!("artifacts/{source}.json")),
    )
}

fn healthy_inputs() -> Result<ValidationBrokerInputSnapshot, pi::error::Error> {
    let rch = normalize_rch_queue_text(
        provenance("rch")?,
        "Build Queue\n  - 1 Active Build(s)\n  - 0 Queued Build(s)\nWorker Availability\n  -> 4 / 18 slots free\n",
    )?;
    let cargo_headroom = normalize_headroom_json(
        provenance("cargo_headroom")?,
        &json!({"available_bytes": 50_000_u64, "required_bytes": 10_000_u64}),
    )?;
    let doctor = normalize_doctor_json(
        provenance("doctor")?,
        &json!({"checks": [{"name": "scratch", "status": "ok"}]}),
    )?;
    let git =
        normalize_git_status_text(provenance("git")?, "3048e53f3", "## main...origin/main\n")?;
    let beads = normalize_beads_json(provenance("beads")?, &json!({"issues": []}), PLAN_AT, 3600)?;
    let scratch_headroom = normalize_headroom_json(
        provenance("scratch_headroom")?,
        &json!({"available_bytes": 50_000_u64, "required_bytes": 10_000_u64}),
    )?;
    let agent_mail = normalize_available_source(provenance("agent_mail")?)?;

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
}

#[derive(Clone, Copy)]
enum E2eSlotSeed {
    Active,
    Reusable,
    Stale,
}

fn e2e_inputs(
    rch_fixture: &str,
    cargo_headroom_fixture: &str,
    scratch_headroom_fixture: &str,
    agent_mail_available: bool,
) -> TestResult<ValidationBrokerInputSnapshot> {
    let rch = normalize_rch_queue_text(provenance("rch")?, &read_fixture_text(rch_fixture)?)?;
    let cargo_headroom = normalize_headroom_json(
        provenance("cargo_headroom")?,
        &read_fixture_json(cargo_headroom_fixture)?,
    )?;
    let doctor = normalize_doctor_json(
        provenance("doctor")?,
        &read_fixture_json("doctor_swarm.json")?,
    )?;
    let git = normalize_git_status_text(
        provenance("git")?,
        "abc123fixture",
        &read_fixture_text("git_status.txt")?,
    )?;
    let beads = normalize_beads_json(
        provenance("beads")?,
        &read_fixture_json("beads_ready.json")?,
        PLAN_AT,
        3600,
    )?;
    let scratch_headroom = normalize_headroom_json(
        provenance("scratch_headroom")?,
        &read_fixture_json(scratch_headroom_fixture)?,
    )?;
    let agent_mail = if agent_mail_available {
        normalize_available_source(provenance("agent_mail")?)?
    } else {
        let status = read_fixture_json("agent_mail_status.json")?;
        let issue = status
            .get("issue")
            .and_then(Value::as_str)
            .unwrap_or("agent_mail_unavailable_fixture");
        let semantic = status
            .pointer("/semantic_readiness/detail")
            .and_then(Value::as_str);
        let recovery = status
            .pointer("/recovery/next_action")
            .and_then(Value::as_str);
        let mut reasons = vec![issue.to_string()];
        if let Some(detail) = semantic {
            reasons.push(format!("semantic_readiness: {detail}"));
        }
        if let Some(action) = recovery {
            reasons.push(format!("recovery.next_action: {action}"));
        }
        let reason = reasons.join("; ");
        normalize_unavailable_source(provenance("agent_mail")?, reason)?
    };

    Ok(ValidationBrokerInputSnapshot::from_parts(
        ValidationBrokerInputParts {
            captured_at_utc: PLAN_AT.to_string(),
            rch,
            cargo_headroom,
            doctor,
            git,
            beads,
            scratch_headroom,
            agent_mail,
        },
    )?)
}

fn e2e_context(slot_id: &str, scenario: &str) -> ValidationAdmissionRequestContext {
    let mut context = admission_context(slot_id);
    context.request_id = format!("request-{scenario}");
    context.request.bead_id = "bd-gusp4.8".to_string();
    context
}

fn seed_equivalent_slot(store_path: &Path, seed: E2eSlotSeed, slot_id: &str) -> TestResult {
    let store = ValidationSlotStore::new(store_path);
    let expires_at = match seed {
        E2eSlotSeed::Active | E2eSlotSeed::Reusable => RENEWED_EXPIRES,
        E2eSlotSeed::Stale => EXPIRES,
    };
    let mut request = base_request(slot_id);
    request.bead_id = "bd-gusp4.8".to_string();
    let mut lease = ValidationSlotLease::acquire(request, START, expires_at)?;
    store.append_lease("fixture_acquire", START, &lease)?;

    match seed {
        E2eSlotSeed::Active => {}
        E2eSlotSeed::Reusable => {
            let artifacts = vec![ValidationSlotArtifact {
                path: "target/debug/deps/pi.fixture.d".to_string(),
                sha256: Some("a".repeat(64)),
                schema: Some("cargo_check_result.v1".to_string()),
            }];
            #[cfg(unix)]
            lease.mark_reusable("Codex", HEARTBEAT, artifacts)?;
            #[cfg(not(unix))]
            {
                if lease.input_identity.is_some() {
                    return Err(test_error("unsupported native capture must be unavailable"));
                }
                if lease
                    .mark_reusable("Codex", HEARTBEAT, artifacts.clone())
                    .is_ok()
                {
                    return Err(test_error(
                        "unsupported native capture must refuse result reuse",
                    ));
                }
                // Preserve the historical-state/status fixture, while planning
                // must refuse to reuse its unavailable native input identity.
                lease.state = pi::validation_broker::ValidationSlotState::Reusable;
                lease.heartbeat_at_utc = HEARTBEAT.into();
                lease.artifacts = artifacts;
            }
            store.append_lease("fixture_reusable", HEARTBEAT, &lease)?;
        }
        E2eSlotSeed::Stale => {
            lease.mark_stale(PLAN_AT, "fixture_expired")?;
            store.append_lease("fixture_stale", PLAN_AT, &lease)?;
        }
    }

    Ok(())
}

fn run_plan_scenario(
    temp: &TempDir,
    scenario: &str,
    inputs: &ValidationBrokerInputSnapshot,
    seed: Option<E2eSlotSeed>,
    policy: Option<&ValidationAdmissionPolicy>,
) -> TestResult<(Value, PathBuf)> {
    let request_path = temp.path().join(format!("{scenario}-request.json"));
    let inputs_path = temp.path().join(format!("{scenario}-inputs.json"));
    let store_path = temp.path().join(format!("{scenario}-slots.jsonl"));
    let out_json = temp.path().join(format!("{scenario}-plan.json"));
    let policy_path = if let Some(policy) = policy {
        let path = temp.path().join(format!("{scenario}-policy.json"));
        write_json(&path, policy)?;
        Some(path)
    } else {
        None
    };
    if let Some(seed) = seed {
        seed_equivalent_slot(&store_path, seed, &format!("slot-{scenario}-existing"))?;
    }

    write_json(
        &request_path,
        &e2e_context(&format!("slot-{scenario}-request"), scenario),
    )?;
    write_json(&inputs_path, inputs)?;

    let output = run_validation_broker_plan(
        &request_path,
        &inputs_path,
        &store_path,
        policy_path.as_deref(),
        &out_json,
    )?;
    if !output.status.success() {
        return Err(test_error(format!(
            "validation-broker plan failed for {scenario}\n{}",
            output_debug(&output)
        )));
    }

    let plan = serde_json::from_str(&fs::read_to_string(&out_json)?)?;
    Ok((plan, out_json))
}

fn record_plan_decision(
    events: &mut Vec<Value>,
    decisions: &mut BTreeSet<String>,
    scenario: &str,
    plan: &Value,
    artifact_path: &Path,
    expected: &str,
    expected_next_action: &str,
) -> TestResult {
    assert_eq!(
        plan.pointer("/schema").and_then(Value::as_str),
        Some(VALIDATION_BROKER_CLI_PLAN_SCHEMA)
    );
    assert_eq!(
        plan.pointer("/decision/schema").and_then(Value::as_str),
        Some(VALIDATION_BROKER_DECISION_SCHEMA)
    );
    assert_eq!(
        plan.pointer("/guards/live_mutations")
            .and_then(Value::as_u64),
        Some(0)
    );
    let decision = plan
        .pointer("/decision/decision")
        .and_then(Value::as_str)
        .ok_or_else(|| test_error(format!("{scenario} plan is missing a decision")))?;
    assert_eq!(decision, expected, "unexpected decision for {scenario}");
    assert_eq!(
        plan.pointer("/next_action").and_then(Value::as_str),
        Some(expected_next_action),
        "unexpected next_action for {scenario}"
    );
    decisions.insert(decision.to_string());
    events.push(json!({
        "schema": "pi.validation_broker.e2e_event.v1",
        "generated_at_utc": PLAN_AT,
        "event": "plan_decision",
        "scenario": scenario,
        "decision": decision,
        "artifact_path": artifact_path.display().to_string(),
    }));
    Ok(())
}

fn value_from_stdout(output: &Output) -> Result<Value, serde_json::Error> {
    serde_json::from_slice(&output.stdout)
}

#[test]
fn validation_broker_status_json_is_schema_stable_for_missing_store() -> TestResult {
    let temp = test_temp_dir()?;
    let store = temp.path().join("missing-slots.jsonl");
    let output = run_pi(&[
        "validation-broker",
        "status",
        "--store",
        store.to_str().ok_or("store path is not UTF-8")?,
        "--format",
        "json",
        "--generated-at",
        PLAN_AT,
    ])?;

    assert!(
        output.status.success(),
        "status command failed\nstdout:\n{}\nstderr:\n{}",
        output_text(&output.stdout),
        output_text(&output.stderr)
    );
    let status = value_from_stdout(&output)?;
    assert_eq!(
        status.pointer("/schema").and_then(Value::as_str),
        Some(VALIDATION_BROKER_CLI_STATUS_SCHEMA)
    );
    assert_eq!(
        status.pointer("/store/status").and_then(Value::as_str),
        Some("available")
    );
    assert_eq!(
        status.pointer("/store/total_slots").and_then(Value::as_u64),
        Some(0)
    );
    Ok(())
}

#[test]
fn validation_broker_plan_is_read_only_and_explains_run_now() -> TestResult {
    let temp = test_temp_dir()?;
    let request_path = temp.path().join("request.json");
    let inputs_path = temp.path().join("inputs.json");
    let store = temp.path().join("slots.jsonl");
    write_json(&request_path, &admission_context("slot-plan"))?;
    write_json(&inputs_path, &healthy_inputs()?)?;

    let output = run_pi(&[
        "validation-broker",
        "plan",
        "--request",
        request_path.to_str().ok_or("request path is not UTF-8")?,
        "--inputs",
        inputs_path.to_str().ok_or("inputs path is not UTF-8")?,
        "--store",
        store.to_str().ok_or("store path is not UTF-8")?,
        "--format",
        "json",
        "--generated-at",
        PLAN_AT,
    ])?;

    assert!(
        output.status.success(),
        "plan command failed\nstdout:\n{}\nstderr:\n{}",
        output_text(&output.stdout),
        output_text(&output.stderr)
    );
    assert!(
        !store.exists(),
        "plan mode should not create or append the slot store"
    );
    let plan = value_from_stdout(&output)?;
    assert_eq!(
        plan.pointer("/schema").and_then(Value::as_str),
        Some(VALIDATION_BROKER_CLI_PLAN_SCHEMA)
    );
    assert_eq!(
        plan.pointer("/read_only").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        plan.pointer("/decision/decision").and_then(Value::as_str),
        Some("allow")
    );
    assert_eq!(
        plan.pointer("/next_action").and_then(Value::as_str),
        Some("run_now")
    );
    assert_eq!(
        plan.pointer("/guards/live_mutations")
            .and_then(Value::as_u64),
        Some(0)
    );
    Ok(())
}

#[test]
fn validation_broker_acquire_renew_release_append_records() -> TestResult {
    let temp = test_temp_dir()?;
    let request_path = temp.path().join("request.json");
    let store = temp.path().join("slots.jsonl");
    write_json(&request_path, &base_request("slot-cli-mutate"))?;

    let acquire = run_pi(&[
        "validation-broker",
        "acquire",
        "--request",
        request_path.to_str().ok_or("request path is not UTF-8")?,
        "--store",
        store.to_str().ok_or("store path is not UTF-8")?,
        "--started-at",
        START,
        "--expires-at",
        EXPIRES,
        "--format",
        "json",
    ])?;
    assert!(
        acquire.status.success(),
        "acquire failed\nstdout:\n{}\nstderr:\n{}",
        output_text(&acquire.stdout),
        output_text(&acquire.stderr)
    );

    let renew = run_pi(&[
        "validation-broker",
        "renew",
        "--store",
        store.to_str().ok_or("store path is not UTF-8")?,
        "--slot-id",
        "slot-cli-mutate",
        "--owner",
        "Codex",
        "--heartbeat-at",
        HEARTBEAT,
        "--expires-at",
        RENEWED_EXPIRES,
        "--format",
        "json",
    ])?;
    assert!(
        renew.status.success(),
        "renew failed\nstdout:\n{}\nstderr:\n{}",
        output_text(&renew.stdout),
        output_text(&renew.stderr)
    );

    let release = run_pi(&[
        "validation-broker",
        "release",
        "--store",
        store.to_str().ok_or("store path is not UTF-8")?,
        "--slot-id",
        "slot-cli-mutate",
        "--owner",
        "Codex",
        "--at",
        PLAN_AT,
        "--reason",
        "focused gate finished",
        "--format",
        "json",
    ])?;
    assert!(
        release.status.success(),
        "release failed\nstdout:\n{}\nstderr:\n{}",
        output_text(&release.stdout),
        output_text(&release.stderr)
    );

    let status = run_pi(&[
        "validation-broker",
        "status",
        "--store",
        store.to_str().ok_or("store path is not UTF-8")?,
        "--format",
        "json",
        "--generated-at",
        PLAN_AT,
    ])?;
    assert!(status.status.success(), "status after release failed");
    let status_json = value_from_stdout(&status)?;
    assert_eq!(
        status_json
            .pointer("/store/state_counts/released")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        status_json
            .pointer("/store/total_records")
            .and_then(Value::as_u64),
        Some(3)
    );
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn validation_broker_no_mock_e2e_harness_emits_decisions_and_runpack_projection() -> TestResult {
    let temp = test_temp_dir()?;
    let fixture_names = [
        "rch_queue_healthy.txt",
        "rch_queue_saturated.txt",
        "cargo_headroom_ok.json",
        "cargo_headroom_low.json",
        "doctor_swarm.json",
        "beads_ready.json",
        "beads_export.json",
        "git_status.txt",
        "git_context.json",
        "agent_mail_status.json",
        "claim_readiness.json",
        "smoke_summary.json",
        "activity_digest.json",
        "cargo_admission.json",
    ];
    let mut artifact_entries = Vec::new();
    for fixture in &fixture_names {
        let path = e2e_fixture_path(fixture);
        assert!(
            path.exists(),
            "missing validation broker e2e fixture {fixture}"
        );
        artifact_entries.push(json!({
            "id": format!("fixture_{}", fixture.replace(['/', '.'], "_")),
            "path": path.display().to_string(),
            "artifact_schema": "fixture_source",
            "evidence_kind": "checked_in_fixture",
        }));
    }

    let mut events = vec![json!({
        "schema": "pi.validation_broker.e2e_event.v1",
        "generated_at_utc": PLAN_AT,
        "event": "fixtures_loaded",
        "fixture_dir": E2E_FIXTURE_DIR,
        "source_count": fixture_names.len(),
    })];
    let mut decisions = BTreeSet::new();

    let allow_inputs = e2e_inputs(
        "rch_queue_healthy.txt",
        "cargo_headroom_ok.json",
        "cargo_headroom_ok.json",
        false,
    )?;
    let (allow_plan, allow_plan_path) = run_plan_scenario(
        &temp,
        "allow_agent_mail_unavailable",
        &allow_inputs,
        None,
        None,
    )?;
    record_plan_decision(
        &mut events,
        &mut decisions,
        "allow_agent_mail_unavailable",
        &allow_plan,
        &allow_plan_path,
        "allow",
        "run_now",
    )?;
    assert_eq!(
        allow_plan
            .pointer("/decision/source_statuses")
            .and_then(Value::as_array)
            .and_then(|statuses| {
                statuses.iter().find(|status| {
                    status.get("source_id").and_then(Value::as_str) == Some("agent_mail")
                })
            })
            .and_then(|status| status.get("state"))
            .and_then(Value::as_str),
        Some("unavailable")
    );
    let agent_mail_status = allow_plan
        .pointer("/decision/source_statuses")
        .and_then(Value::as_array)
        .and_then(|statuses| {
            statuses.iter().find(|status| {
                status.get("source_id").and_then(Value::as_str) == Some("agent_mail")
            })
        })
        .ok_or_else(|| test_error("missing agent_mail source status"))?;
    let agent_mail_reasons = agent_mail_status
        .get("degraded_reasons")
        .and_then(Value::as_array)
        .ok_or_else(|| test_error("missing agent_mail degraded reasons"))?;
    assert!(
        agent_mail_reasons.iter().any(|reason| {
            reason
                .as_str()
                .is_some_and(|value| value.contains("semantic_readiness"))
        }),
        "agent_mail status should expose corrupt semantic readiness"
    );
    assert!(
        agent_mail_reasons.iter().any(|reason| {
            reason
                .as_str()
                .is_some_and(|value| value.contains("am doctor repair --yes"))
        }),
        "agent_mail status should expose recovery action"
    );
    artifact_entries.push(json!({
        "id": "plan_allow_agent_mail_unavailable",
        "path": allow_plan_path.display().to_string(),
        "artifact_schema": VALIDATION_BROKER_CLI_PLAN_SCHEMA,
        "evidence_kind": "broker_decision",
    }));

    let wait_policy = ValidationAdmissionPolicy {
        allow_narrow_scope: false,
        ..ValidationAdmissionPolicy::default()
    };
    let wait_inputs = e2e_inputs(
        "rch_queue_saturated.txt",
        "cargo_headroom_ok.json",
        "cargo_headroom_ok.json",
        true,
    )?;
    let (wait_plan, wait_plan_path) = run_plan_scenario(
        &temp,
        "wait_backoff_saturated_rch",
        &wait_inputs,
        None,
        Some(&wait_policy),
    )?;
    record_plan_decision(
        &mut events,
        &mut decisions,
        "wait_backoff_saturated_rch",
        &wait_plan,
        &wait_plan_path,
        "wait",
        "wait",
    )?;
    assert_eq!(
        wait_plan
            .pointer("/decision/reasons/0")
            .and_then(Value::as_str),
        Some("hard_rch_backpressure")
    );
    artifact_entries.push(json!({
        "id": "plan_wait_backoff_saturated_rch",
        "path": wait_plan_path.display().to_string(),
        "artifact_schema": VALIDATION_BROKER_CLI_PLAN_SCHEMA,
        "evidence_kind": "broker_decision",
    }));

    let coalesce_inputs = e2e_inputs(
        "rch_queue_healthy.txt",
        "cargo_headroom_ok.json",
        "cargo_headroom_ok.json",
        true,
    )?;
    let (coalesce_plan, coalesce_plan_path) = run_plan_scenario(
        &temp,
        "coalesce_reusable",
        &coalesce_inputs,
        Some(E2eSlotSeed::Reusable),
        None,
    )?;
    record_plan_decision(
        &mut events,
        &mut decisions,
        "coalesce_reusable",
        &coalesce_plan,
        &coalesce_plan_path,
        if cfg!(unix) { "coalesce" } else { "allow" },
        if cfg!(unix) {
            "coalesce_with_reusable_slot"
        } else {
            "run_now"
        },
    )?;
    assert_eq!(
        coalesce_plan
            .pointer("/decision/coalesced_artifacts")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(usize::from(cfg!(unix)))
    );
    #[cfg(not(unix))]
    assert!(
        coalesce_plan
            .pointer("/decision/reusable_slot")
            .is_some_and(Value::is_null)
    );
    artifact_entries.push(json!({
        "id": "plan_coalesce_reusable",
        "path": coalesce_plan_path.display().to_string(),
        "artifact_schema": VALIDATION_BROKER_CLI_PLAN_SCHEMA,
        "evidence_kind": "broker_decision",
    }));

    let narrow_inputs = e2e_inputs(
        "rch_queue_healthy.txt",
        "cargo_headroom_low.json",
        "cargo_headroom_ok.json",
        true,
    )?;
    let (narrow_plan, narrow_plan_path) =
        run_plan_scenario(&temp, "narrow_low_headroom", &narrow_inputs, None, None)?;
    record_plan_decision(
        &mut events,
        &mut decisions,
        "narrow_low_headroom",
        &narrow_plan,
        &narrow_plan_path,
        "narrow",
        "narrow_scope",
    )?;
    assert_eq!(
        narrow_plan
            .pointer("/decision/policy/low_cargo_headroom")
            .and_then(Value::as_bool),
        Some(true)
    );
    artifact_entries.push(json!({
        "id": "plan_narrow_low_headroom",
        "path": narrow_plan_path.display().to_string(),
        "artifact_schema": VALIDATION_BROKER_CLI_PLAN_SCHEMA,
        "evidence_kind": "broker_decision",
    }));

    let stale_inputs = e2e_inputs(
        "rch_queue_healthy.txt",
        "cargo_headroom_ok.json",
        "cargo_headroom_ok.json",
        true,
    )?;
    let (stale_plan, stale_plan_path) = run_plan_scenario(
        &temp,
        "stale_recover_expired_slot",
        &stale_inputs,
        Some(E2eSlotSeed::Stale),
        None,
    )?;
    record_plan_decision(
        &mut events,
        &mut decisions,
        "stale_recover_expired_slot",
        &stale_plan,
        &stale_plan_path,
        "stale_recover",
        "recover_stale_slot_or_bead",
    )?;
    assert_eq!(
        stale_plan
            .pointer("/decision/policy/stale_equivalent_slots")
            .and_then(Value::as_u64),
        Some(1)
    );
    artifact_entries.push(json!({
        "id": "plan_stale_recover_expired_slot",
        "path": stale_plan_path.display().to_string(),
        "artifact_schema": VALIDATION_BROKER_CLI_PLAN_SCHEMA,
        "evidence_kind": "broker_decision",
    }));

    for required in ["allow", "wait", "narrow", "stale_recover"] {
        assert!(
            decisions.contains(required),
            "missing required e2e decision {required}; saw {decisions:?}"
        );
    }
    assert_eq!(
        decisions.contains("coalesce"),
        cfg!(unix),
        "coalescing requires supported native capture"
    );

    let status_store_path = temp.path().join("combined-status-slots.jsonl");
    seed_equivalent_slot(
        &status_store_path,
        E2eSlotSeed::Active,
        "slot-status-active",
    )?;
    seed_equivalent_slot(
        &status_store_path,
        E2eSlotSeed::Reusable,
        "slot-status-reusable",
    )?;
    seed_equivalent_slot(&status_store_path, E2eSlotSeed::Stale, "slot-status-stale")?;
    let status_path = temp.path().join("validation-broker-status.json");
    let status_output = run_validation_broker_status(&status_store_path, &status_path)?;
    if !status_output.status.success() {
        return Err(test_error(format!(
            "validation-broker status failed\n{}",
            output_debug(&status_output)
        )));
    }
    let status_json: Value = serde_json::from_str(&fs::read_to_string(&status_path)?)?;
    assert_eq!(
        status_json.pointer("/schema").and_then(Value::as_str),
        Some(VALIDATION_BROKER_CLI_STATUS_SCHEMA)
    );
    assert_eq!(
        status_json
            .pointer("/store/state_counts/active")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        status_json
            .pointer("/store/state_counts/reusable")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        status_json
            .pointer("/store/state_counts/stale")
            .and_then(Value::as_u64),
        Some(1)
    );
    events.push(json!({
        "schema": "pi.validation_broker.e2e_event.v1",
        "generated_at_utc": PLAN_AT,
        "event": "status_projection",
        "artifact_path": status_path.display().to_string(),
        "total_slots": status_json.pointer("/store/total_slots").and_then(Value::as_u64),
    }));
    artifact_entries.push(json!({
        "id": "validation_broker_status_projection",
        "path": status_path.display().to_string(),
        "artifact_schema": VALIDATION_BROKER_CLI_STATUS_SCHEMA,
        "evidence_kind": "broker_status_projection",
    }));
    artifact_entries.push(json!({
        "id": "validation_slot_records",
        "path": status_store_path.display().to_string(),
        "artifact_schema": "pi.validation_broker.slot_store.record.v1",
        "evidence_kind": "validation_slot_records",
    }));

    let runpack_path = temp.path().join("operator-runpack.json");
    let runpack_md_path = temp.path().join("operator-runpack.md");
    let runpack_output = run_runpack_builder(&status_path, &runpack_path, &runpack_md_path)?;
    if !runpack_output.status.success() {
        return Err(test_error(format!(
            "runpack builder failed\n{}",
            output_debug(&runpack_output)
        )));
    }
    let runpack: Value = serde_json::from_str(&fs::read_to_string(&runpack_path)?)?;
    assert_eq!(
        runpack
            .pointer("/validation_broker/schema")
            .and_then(Value::as_str),
        Some(VALIDATION_BROKER_CLI_STATUS_SCHEMA)
    );
    assert_eq!(
        runpack
            .pointer("/validation_broker/current_slots/reusable")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert_eq!(
        runpack
            .pointer("/validation_broker/guards/provider_calls")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        runpack
            .pointer("/doctor_swarm/validation_broker/schema")
            .and_then(Value::as_str),
        Some("pi.doctor.validation_broker_posture.v1")
    );
    assert_eq!(
        runpack
            .pointer("/doctor_swarm/validation_broker/stale_build_warnings/count")
            .and_then(Value::as_u64),
        Some(1)
    );
    assert!(
        fs::read_to_string(&runpack_md_path)?.contains("Validation broker"),
        "runpack Markdown should include validation broker projection"
    );
    events.push(json!({
        "schema": "pi.validation_broker.e2e_event.v1",
        "generated_at_utc": PLAN_AT,
        "event": "runpack_projection",
        "json_artifact_path": runpack_path.display().to_string(),
        "markdown_artifact_path": runpack_md_path.display().to_string(),
        "validation_broker_status": runpack.pointer("/validation_broker/status"),
    }));
    artifact_entries.push(json!({
        "id": "operator_runpack_json",
        "path": runpack_path.display().to_string(),
        "artifact_schema": "pi.swarm.operator_runpack.v1",
        "evidence_kind": "runpack_projection",
    }));
    artifact_entries.push(json!({
        "id": "operator_runpack_markdown",
        "path": runpack_md_path.display().to_string(),
        "artifact_schema": "markdown",
        "evidence_kind": "runpack_projection",
    }));

    let negative_request_path = temp.path().join("negative-request.json");
    let negative_store_path = temp.path().join("negative-slots.jsonl");
    write_json(
        &negative_request_path,
        &e2e_context("slot-negative-request", "negative_source"),
    )?;
    let missing_source_output = run_validation_broker_plan(
        &negative_request_path,
        &temp.path().join("missing-source-inputs.json"),
        &negative_store_path,
        None,
        &temp.path().join("missing-source-plan.json"),
    )?;
    assert!(
        !missing_source_output.status.success(),
        "missing source plan unexpectedly succeeded"
    );

    let malformed_inputs_path = temp.path().join("malformed-inputs.json");
    fs::write(&malformed_inputs_path, "{ malformed validation inputs")?;
    let malformed_inputs_output = run_validation_broker_plan(
        &negative_request_path,
        &malformed_inputs_path,
        &negative_store_path,
        None,
        &temp.path().join("malformed-inputs-plan.json"),
    )?;
    assert!(
        !malformed_inputs_output.status.success(),
        "malformed validation inputs unexpectedly succeeded"
    );

    let malformed_broker_path = temp.path().join("malformed-validation-broker.json");
    fs::write(&malformed_broker_path, "{ malformed broker artifact")?;
    let malformed_runpack_output = run_runpack_builder(
        &malformed_broker_path,
        &temp.path().join("malformed-runpack.json"),
        &temp.path().join("malformed-runpack.md"),
    )?;
    assert!(
        !malformed_runpack_output.status.success(),
        "malformed runpack broker artifact unexpectedly succeeded"
    );
    assert!(
        output_text(&malformed_runpack_output.stderr)
            .contains("validation_broker source is malformed JSON"),
        "malformed runpack artifact did not explain the broker JSON error:\n{}",
        output_debug(&malformed_runpack_output)
    );
    events.push(json!({
        "schema": "pi.validation_broker.e2e_event.v1",
        "generated_at_utc": PLAN_AT,
        "event": "negative_cases",
        "missing_source_failed": true,
        "malformed_inputs_failed": true,
        "malformed_runpack_artifact_failed": true,
    }));

    let event_log_path = temp.path().join("validation-broker-e2e-events.jsonl");
    write_jsonl(&event_log_path, &events)?;
    artifact_entries.push(json!({
        "id": "e2e_event_log",
        "path": event_log_path.display().to_string(),
        "artifact_schema": "pi.validation_broker.e2e_event.v1",
        "evidence_kind": "jsonl_log",
    }));

    for entry in &artifact_entries {
        let path = entry
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| test_error("artifact entry is missing a path"))?;
        assert!(
            Path::new(path).exists(),
            "artifact manifest path does not exist: {path}"
        );
    }

    let manifest_path = temp.path().join("artifact-manifest.json");
    let manifest_path_value = path_str(&manifest_path)?.to_string();
    write_json(
        &manifest_path,
        &json!({
            "schema": "pi.validation_broker.e2e_artifact_manifest.v1",
            "bead_id": "bd-gusp4.8",
            "generated_at_utc": PLAN_AT,
            "fixture_dir": E2E_FIXTURE_DIR,
            "artifact_manifest_path": manifest_path_value,
            "decisions_observed": decisions.iter().cloned().collect::<Vec<_>>(),
            "guards": {
                "no_network_required": true,
                "live_mutations": 0,
                "provider_calls": 0,
                "destructive_actions": 0
            },
            "entries": artifact_entries,
        }),
    )?;
    let manifest: Value = serde_json::from_str(&fs::read_to_string(&manifest_path)?)?;
    assert_eq!(
        manifest.pointer("/schema").and_then(Value::as_str),
        Some("pi.validation_broker.e2e_artifact_manifest.v1")
    );
    assert_eq!(
        manifest
            .pointer("/guards/no_network_required")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert!(
        manifest
            .pointer("/entries")
            .and_then(Value::as_array)
            .is_some_and(|entries| entries.len() >= fixture_names.len() + decisions.len() + 4),
        "artifact manifest should include fixtures plus emitted broker/runpack evidence"
    );

    Ok(())
}

#[test]
fn validation_broker_plan_rejects_missing_and_malformed_inputs() -> TestResult {
    let temp = test_temp_dir()?;
    let malformed_request = temp.path().join("malformed-request.json");
    let missing_inputs = temp.path().join("missing-inputs.json");
    let store = temp.path().join("slots.jsonl");
    fs::write(&malformed_request, "{}")?;

    let output = run_pi(&[
        "validation-broker",
        "plan",
        "--request",
        malformed_request
            .to_str()
            .ok_or("malformed request path is not UTF-8")?,
        "--inputs",
        missing_inputs
            .to_str()
            .ok_or("missing inputs path is not UTF-8")?,
        "--store",
        store.to_str().ok_or("store path is not UTF-8")?,
        "--format",
        "json",
        "--generated-at",
        PLAN_AT,
    ])?;

    assert!(
        !output.status.success(),
        "malformed/missing input command unexpectedly succeeded"
    );
    Ok(())
}

#[test]
fn validation_broker_outputs_refuse_overwrite() -> TestResult {
    let temp = test_temp_dir()?;
    let store = temp.path().join("slots.jsonl");
    let out_json = temp.path().join("status.json");
    fs::write(&out_json, "{}")?;

    let output = run_pi(&[
        "validation-broker",
        "status",
        "--store",
        store.to_str().ok_or("store path is not UTF-8")?,
        "--out-json",
        out_json.to_str().ok_or("output path is not UTF-8")?,
        "--generated-at",
        PLAN_AT,
    ])?;

    assert!(
        !output.status.success(),
        "overwrite command unexpectedly succeeded"
    );
    assert!(
        output_text(&output.stderr)
            .contains("refusing to overwrite existing validation-broker JSON output"),
        "stderr did not explain overwrite refusal:\n{}",
        output_text(&output.stderr)
    );
    Ok(())
}
