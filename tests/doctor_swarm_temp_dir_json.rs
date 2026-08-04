use pi::validation_broker::{
    ValidationSlotArtifact, ValidationSlotLease, ValidationSlotRequest, ValidationSlotStore,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const SWARM_TEMP_DIR_SCHEMA: &str = "pi.doctor.swarm_temp_dir.v1";
const SWARM_RESOURCE_PREFLIGHT_SCHEMA: &str = "pi.doctor.swarm_resource_preflight.v1";
const SWARM_LANE_PLACEMENT_SCHEMA: &str = "pi.doctor.swarm_lane_placement.v1";
const SWARM_MAIL_DEGRADED_SCHEMA: &str = "pi.doctor.agent_mail_degraded_mode.v1";
const SWARM_CONTEXT_INTELLIGENCE_SCHEMA: &str = "pi.doctor.context_intelligence_posture.v1";
const SWARM_VALIDATION_BROKER_SCHEMA: &str = "pi.doctor.validation_broker_posture.v1";
const SWARM_INCIDENT_DIAGNOSTICS_SCHEMA: &str = "pi.doctor.swarm_incident_diagnostics.v1";
const SWARM_TEMP_EXPECTED_ROOT: &str = "/data/tmp/pi_agent_rust_cargo";
const SWARM_TEMP_WARN_AVAILABLE_KB: u64 = 10 * 1024 * 1024;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[derive(Debug)]
struct TestError(String);

impl fmt::Display for TestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for TestError {}

fn fail<T>(message: impl Into<String>) -> TestResult<T> {
    Err(Box::new(TestError(message.into())))
}

fn require(condition: bool, message: impl Into<String>) -> TestResult {
    if condition { Ok(()) } else { fail(message) }
}

fn require_eq<T, U>(actual: &T, expected: &U, context: &str) -> TestResult
where
    T: fmt::Debug + PartialEq<U> + ?Sized,
    U: fmt::Debug + ?Sized,
{
    if actual == expected {
        Ok(())
    } else {
        fail(format!("{context}: expected {expected:?}, got {actual:?}"))
    }
}

fn field<'a>(value: &'a Value, key: &str) -> TestResult<&'a Value> {
    value
        .get(key)
        .ok_or_else(|| TestError(format!("missing JSON field `{key}` in {value}")))
        .map_err(Into::into)
}

fn field_str<'a>(value: &'a Value, key: &str) -> TestResult<&'a str> {
    field(value, key)?
        .as_str()
        .ok_or_else(|| TestError(format!("JSON field `{key}` is not a string in {value}")))
        .map_err(Into::into)
}

fn field_bool(value: &Value, key: &str) -> TestResult<bool> {
    field(value, key)?
        .as_bool()
        .ok_or_else(|| TestError(format!("JSON field `{key}` is not a bool in {value}")))
        .map_err(Into::into)
}

fn field_u64(value: &Value, key: &str) -> TestResult<u64> {
    field(value, key)?
        .as_u64()
        .ok_or_else(|| TestError(format!("JSON field `{key}` is not a u64 in {value}")))
        .map_err(Into::into)
}

fn run_doctor_json(env_overrides: &[(&str, Option<&str>)]) -> TestResult<Value> {
    let cwd = create_swarm_temp_test_dir(Path::new("/tmp"), "cwd")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi")); // ubs:ignore false positive: Cargo provides the compiled test binary path.
    command
        .args(["doctor", "--only", "swarm", "--format", "json"])
        .current_dir(cwd)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("GROQ_API_KEY")
        .env_remove("KIMI_API_KEY")
        .env_remove("AZURE_OPENAI_API_KEY")
        .env_remove("PI_VALIDATION_BROKER_STORE")
        .env_remove("PI_DOCTOR_LOGICAL_CPU_CORES")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env_overrides {
        match value {
            Some(value) => {
                command.env(*key, *value);
            }
            None => {
                command.env_remove(*key);
            }
        }
    }

    let output = command.output()?;
    let exit_code = output.status.code();
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !matches!(exit_code, Some(0 | 1)) {
        return fail(format!(
            "doctor should exit cleanly with 0 or 1, got {exit_code:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        ));
    }

    serde_json::from_str(&stdout).map_err(|err| {
        TestError(format!(
            "doctor stdout should be JSON: {err}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        ))
        .into()
    })
}

fn run_doctor_text(env_overrides: &[(&str, Option<&str>)]) -> TestResult<String> {
    let cwd = create_swarm_temp_test_dir(Path::new("/tmp"), "text-cwd")?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi")); // ubs:ignore false positive: Cargo provides the compiled test binary path.
    command
        .args(["doctor", "--only", "swarm", "--format", "text"])
        .current_dir(cwd)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("GROQ_API_KEY")
        .env_remove("KIMI_API_KEY")
        .env_remove("AZURE_OPENAI_API_KEY")
        .env_remove("PI_VALIDATION_BROKER_STORE")
        .env_remove("PI_DOCTOR_LOGICAL_CPU_CORES")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env_overrides {
        match value {
            Some(value) => {
                command.env(*key, *value);
            }
            None => {
                command.env_remove(*key);
            }
        }
    }

    let output = command.output()?;
    let exit_code = output.status.code();
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !matches!(exit_code, Some(0 | 1)) {
        return fail(format!(
            "doctor text should exit cleanly with 0 or 1, got {exit_code:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        ));
    }
    Ok(stdout)
}

fn temp_dir_finding<'a>(report: &'a Value, env_name: &str) -> TestResult<&'a Value> {
    let findings = field(report, "findings")?
        .as_array()
        .ok_or_else(|| TestError(format!("doctor findings is not an array in {report}")))?;
    for finding in findings {
        let Some(data) = finding.get("data") else {
            continue;
        };
        let schema = data.get("schema").and_then(Value::as_str);
        let finding_env_name = data.get("env_name").and_then(Value::as_str);
        if schema == Some(SWARM_TEMP_DIR_SCHEMA) && finding_env_name == Some(env_name) {
            return Ok(finding);
        }
    }

    fail(format!(
        "missing swarm temp-dir finding for {env_name}: {report}"
    ))
}

fn finding_by_schema<'a>(report: &'a Value, schema: &str) -> TestResult<&'a Value> {
    let findings = field(report, "findings")?
        .as_array()
        .ok_or_else(|| TestError(format!("doctor findings is not an array in {report}")))?;
    for finding in findings {
        if finding
            .get("data")
            .and_then(|data| data.get("schema"))
            .and_then(Value::as_str)
            == Some(schema)
        {
            return Ok(finding);
        }
    }

    fail(format!(
        "missing doctor finding for schema {schema}: {report}"
    ))
}

fn require_temp_dir_data_shape(data: &Value, env_name: &str) -> TestResult {
    require_eq(field_str(data, "schema")?, SWARM_TEMP_DIR_SCHEMA, "schema")?;
    require_eq(field_str(data, "env_name")?, env_name, "env_name")?;
    require_eq(
        field_str(data, "expected_root")?,
        SWARM_TEMP_EXPECTED_ROOT,
        "expected_root",
    )?;
    require_eq(
        &field_u64(data, "warn_available_kb")?,
        &SWARM_TEMP_WARN_AVAILABLE_KB,
        "warn_available_kb",
    )?;
    let _ = field(data, "path")?;
    let _ = field(data, "exists")?;
    let _ = field(data, "under_expected_root")?;
    let _ = field(data, "available_kb")?;
    require(
        field_str(data, "recommended_pattern")?.contains(SWARM_TEMP_EXPECTED_ROOT),
        format!("recommended_pattern should mention expected root: {data}"),
    )
}

fn require_available_kb_shape(data: &Value) -> TestResult {
    let value = field(data, "available_kb")?;
    require(
        value.is_null() || value.as_u64().is_some(),
        format!("available_kb should be null or an integer: {data}"),
    )
}

fn require_lane_placement_shape(data: &Value) -> TestResult<&Value> {
    let lane_placement = field(data, "lane_placement")?;
    require_eq(
        field_str(lane_placement, "schema")?,
        SWARM_LANE_PLACEMENT_SCHEMA,
        "lane placement schema",
    )?;
    let status = field_str(lane_placement, "status")?;
    require(
        matches!(status, "ready" | "degraded" | "blocked"),
        format!("unexpected lane placement status: {lane_placement}"),
    )?;
    let groups = field(lane_placement, "lane_groups")?
        .as_array()
        .ok_or_else(|| TestError(format!("lane_groups is not an array: {lane_placement}")))?;
    require(
        !groups.is_empty(),
        format!("lane placement should include at least one group: {lane_placement}"),
    )?;
    for group in groups {
        let _ = field_str(group, "lane_id")?;
        let _ = field(group, "numa_node")?;
        let _ = field_str(group, "cpu_affinity_hint")?;
        let _ = field(group, "cpus")?;
        require(
            field_str(group, "target_dir")?.contains(SWARM_TEMP_EXPECTED_ROOT),
            "lane target_dir should use expected root",
        )?;
        require(
            field_str(group, "tmpdir")?.contains(SWARM_TEMP_EXPECTED_ROOT),
            "lane tmpdir should use expected root",
        )?;
        let _ = field_u64(group, "max_agents")?;
        let _ = field_u64(group, "max_tool_batches")?;
        let _ = field_u64(group, "max_extension_hostcall_lanes")?;
        let _ = field_u64(group, "max_rch_verification_fanout")?;
    }
    let _ = field(lane_placement, "caveats")?;
    let _ = field(lane_placement, "recommendations")?;
    Ok(lane_placement)
}

fn create_swarm_temp_test_dir(root: &Path, name: &str) -> TestResult<PathBuf> {
    let dir = root
        .join("pi-doctor-json-e2e")
        .join(format!("{}-{name}", std::process::id()));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn create_expected_root_test_dir(name: &str) -> TestResult<(PathBuf, bool)> {
    let dir = Path::new(SWARM_TEMP_EXPECTED_ROOT)
        .join("pi-doctor-json-e2e")
        .join(format!("{}-{name}", std::process::id()));
    match fs::create_dir_all(&dir) {
        Ok(()) => Ok((dir, true)),
        Err(err) if err.kind() == ErrorKind::PermissionDenied => Ok((dir, false)),
        Err(err) => Err(err.into()),
    }
}

fn validation_broker_request(slot_id: &str) -> ValidationSlotRequest {
    let mut environment = BTreeMap::new();
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
        bead_id: "bd-gusp4.7".to_string(),
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
        git_head: "cf653c29b5836afabf979bb44325d4712de7088d".to_string(),
        feature_flags: vec!["default".to_string()],
        target_dir: "/data/tmp/pi_agent_rust_cargo/codex/target".to_string(),
        tmpdir: "/data/tmp/pi_agent_rust_cargo/codex/tmp".to_string(),
        runner: "rch_required".to_string(),
        rust_toolchain: Some("nightly".to_string()),
        rch_job_id: Some("rch-job-doctor".to_string()),
        environment,
        expected_artifacts: vec![ValidationSlotArtifact {
            path: "target/debug/deps/pi.d".to_string(),
            sha256: None,
            schema: Some("cargo_metadata".to_string()),
        }],
        artifact_schema: Some("cargo_check_result.v1".to_string()),
        artifact_hash: Some("artifact-hash-doctor".to_string()),
    }
}

fn create_stale_validation_broker_store() -> TestResult<PathBuf> {
    let root = create_swarm_temp_test_dir(Path::new("/tmp"), "validation-broker-store")?;
    let store_path = root.join("slots.jsonl");
    let store = ValidationSlotStore::new(&store_path);
    let mut lease = ValidationSlotLease::acquire(
        validation_broker_request("slot-stale"),
        "2024-05-14T07:00:00Z",
        "2024-05-14T07:30:00Z",
    )?;
    store.append_lease("acquire", "2024-05-14T07:00:00Z", &lease)?;
    lease.mark_stale("2024-05-14T08:00:00Z", "owner missed validation handoff")?;
    store.append_lease("mark_stale", "2024-05-14T08:00:00Z", &lease)?;
    Ok(store_path)
}

#[cfg(unix)]
fn create_fake_agent_mail_cli(name: &str) -> TestResult<PathBuf> {
    let root = create_swarm_temp_test_dir(Path::new("/tmp"), name)?;
    let bin_dir = root.join("bin");
    fs::create_dir_all(&bin_dir)?;
    let am_path = bin_dir.join("am");
    fs::write(
        &am_path,
        r#"#!/bin/sh
if [ "$1" = "robot" ] && [ "$2" = "health" ]; then
  cat <<'JSON'
{
  "_alerts": [
    {"severity": "error", "summary": "Core SQLite schema tables are missing"}
  ],
  "_actions": [
    "Run `am doctor check` and reconstruct from the Git archive before trusting mailbox reads"
  ],
  "status": "error",
  "health_level": "red",
  "semantic_readiness": {
    "status": "fail",
    "detail": "sqlite schema missing required health_check tables: projects, agents, messages, message_recipients"
  },
  "recovery": {
    "mode": "corrupt",
    "next_action": "am doctor repair --yes or restore archive backup"
  }
}
JSON
  exit 0
fi
if [ "$1" = "robot" ] && [ "$2" = "agents" ]; then
  cat <<'JSON'
{
  "agents": [
    {
      "name": "GoldenGlacier",
      "last_active_ts": "2026-05-11T03:00:00Z",
      "task_description": "current bead"
    },
    {
      "name": "DarkGoose",
      "last_active_ts": "2026-05-09T03:00:00Z",
      "task_description": "stale cargo-test bead"
    }
  ]
}
JSON
  exit 0
fi
if [ "$1" = "robot" ] && [ "$2" = "status" ]; then
  echo "send/ack path unavailable: missing table messages" >&2
  exit 2
fi
if [ "$1" = "robot" ] && [ "$2" = "inbox" ]; then
  echo '{"messages":[],"urgent":0,"ack_required":0,"unread":0}'
  exit 0
fi
if [ "$1" = "robot" ] && [ "$2" = "contacts" ]; then
  echo '{"contacts":[]}'
  exit 0
fi
if [ "$1" = "robot" ] && [ "$2" = "reservations" ]; then
  echo '{"reservations":[]}'
  exit 0
fi
if [ "$1" = "amctl" ]; then
  echo "mail archive unavailable: missing table projects" >&2
  exit 2
fi
echo '{}'
"#,
    )?;
    let mut perms = fs::metadata(&am_path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&am_path, perms)?;
    Ok(bin_dir)
}

#[cfg(unix)]
fn path_with_fake_bin(fake_bin: &Path) -> TestResult<String> {
    let mut paths = vec![fake_bin.to_path_buf()];
    if let Some(existing) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing));
    }
    env::join_paths(paths)?
        .into_string()
        .map_err(|_| TestError("PATH contains non-UTF-8 data".to_string()).into())
}

#[derive(Clone, Copy)]
enum ResourceFixtureFile {
    CpuMax,
    CpusetCpusEffective,
    NumaOnline,
    MemoryMax,
    Meminfo,
}

fn write_resource_fixture_file(
    root: &Path,
    file: ResourceFixtureFile,
    content: &str,
) -> TestResult<PathBuf> {
    let path = match file {
        ResourceFixtureFile::CpuMax => root.join("cpu.max"),
        ResourceFixtureFile::CpusetCpusEffective => root.join("cpuset.cpus.effective"),
        ResourceFixtureFile::NumaOnline => root.join("numa-online"),
        ResourceFixtureFile::MemoryMax => root.join("memory.max"),
        ResourceFixtureFile::Meminfo => root.join("meminfo"),
    };
    fs::write(&path, content)?;
    Ok(path)
}

fn require_missing_env(report: &Value, env_name: &str) -> TestResult {
    let finding = temp_dir_finding(report, env_name)?;
    let data = field(finding, "data")?;
    require_eq(field_str(finding, "severity")?, "warn", "severity")?;
    require_eq(
        field_str(finding, "title")?,
        format!("{env_name} is not set").as_str(),
        "title",
    )?;
    require_temp_dir_data_shape(data, env_name)?;
    require(field(data, "path")?.is_null(), "missing env path is null")?;
    require_eq(&field_bool(data, "exists")?, &false, "exists")?;
    require(
        field(data, "under_expected_root")?.is_null(),
        "missing env root posture is null",
    )?;
    require(
        field(data, "available_kb")?.is_null(),
        "missing env available_kb is null",
    )
}

#[test]
fn doctor_swarm_temp_dir_json_reports_missing_env() -> TestResult {
    let report = run_doctor_json(&[("CARGO_TARGET_DIR", None), ("TMPDIR", None)])?;

    require_missing_env(&report, "CARGO_TARGET_DIR")?;
    require_missing_env(&report, "TMPDIR")
}

#[test]
fn doctor_swarm_context_intelligence_json_reports_posture() -> TestResult {
    let report = run_doctor_json(&[("CARGO_TARGET_DIR", None), ("TMPDIR", None)])?;
    let finding = finding_by_schema(&report, SWARM_CONTEXT_INTELLIGENCE_SCHEMA)?;
    require(
        ["pass", "warn"].contains(&field_str(finding, "severity")?),
        format!("context finding should be pass or warn: {finding}"),
    )?;
    require(
        field_str(finding, "title")?.contains("Context intelligence posture"),
        format!("title should name context intelligence: {finding}"),
    )?;

    let data = field(finding, "data")?;
    require_eq(
        field_str(data, "mode")?,
        "audit_only",
        "context intelligence mode",
    )?;
    require_eq(
        &field_bool(data, "mutation_performed")?,
        &false,
        "context intelligence mutation flag",
    )?;
    let graph = field(data, "graph")?;
    let bundle = field(data, "bundle")?;
    let cache = field(data, "cache")?;
    let _ = field_u64(graph, "node_count")?;
    let _ = field_u64(graph, "missing_input_count")?;
    let _ = field_u64(bundle, "selected_count")?;
    let _ = field_u64(bundle, "missing_test_link_count")?;
    let _ = field_u64(bundle, "stale_evidence_suppression_count")?;
    let _ = field_u64(cache, "pressure_count")?;
    let _ = field_bool(cache, "pressure")?;
    require(
        field(data, "redaction_summary")?.is_object(),
        format!("redaction_summary should be present: {data}"),
    )
}

#[test]
fn doctor_swarm_context_intelligence_text_reports_posture() -> TestResult {
    let stdout = run_doctor_text(&[("CARGO_TARGET_DIR", None), ("TMPDIR", None)])?;
    require(
        stdout.contains("Context intelligence posture"),
        format!("text output should include context intelligence title:\n{stdout}"),
    )?;
    require(
        stdout.contains("bundle"),
        format!("text output should include bundle detail:\n{stdout}"),
    )
}

#[test]
fn doctor_swarm_validation_broker_json_reports_advisory_posture() -> TestResult {
    let report = run_doctor_json(&[("CARGO_TARGET_DIR", None), ("TMPDIR", None)])?;
    let finding = finding_by_schema(&report, SWARM_VALIDATION_BROKER_SCHEMA)?;
    require_eq(field_str(finding, "severity")?, "info", "severity")?;
    require(
        field_str(finding, "title")?.contains("Validation broker posture"),
        format!("title should name validation broker posture: {finding}"),
    )?;

    let data = field(finding, "data")?;
    require_eq(
        field_str(data, "mode")?,
        "advisory_projection",
        "validation broker mode",
    )?;
    require_eq(
        &field_bool(data, "mutation_performed")?,
        &false,
        "validation broker mutation flag",
    )?;
    require_eq(
        field_str(field(data, "source_status")?, "validation_slot_store")?,
        "not_configured",
        "slot-store source status",
    )?;
    require_eq(
        &field_bool(field(data, "guards")?, "advisory_only")?,
        &true,
        "advisory guard",
    )?;
    require_eq(
        &field_bool(field(data, "guards")?, "not_ci_success")?,
        &true,
        "not CI success guard",
    )?;
    require(
        field(data, "current_slots")?.is_object(),
        format!("current_slots should be present: {data}"),
    )?;
    require(
        field(data, "recommended_next_actions")?.is_array(),
        format!("recommended_next_actions should be present: {data}"),
    )
}

#[test]
fn doctor_swarm_validation_broker_json_reports_missing_configured_store() -> TestResult {
    let missing_path =
        create_swarm_temp_test_dir(Path::new("/tmp"), "missing-validation-broker-store")?
            .join("missing-slots.jsonl");
    let missing_store = missing_path.display().to_string();
    let report = run_doctor_json(&[
        ("CARGO_TARGET_DIR", None),
        ("TMPDIR", None),
        ("PI_VALIDATION_BROKER_STORE", Some(missing_store.as_str())),
    ])?;
    let finding = finding_by_schema(&report, SWARM_VALIDATION_BROKER_SCHEMA)?;
    require_eq(field_str(finding, "severity")?, "warn", "severity")?;
    require(
        field_str(finding, "title")?.contains("unavailable"),
        format!("title should report unavailable store: {finding}"),
    )?;

    let data = field(finding, "data")?;
    require_eq(
        field_str(field(data, "source_status")?, "validation_slot_store")?,
        "unavailable",
        "slot-store source status",
    )?;
    let store = field(data, "store")?;
    require_eq(&field_bool(store, "configured")?, &true, "store configured")?;
    require_eq(&field_bool(store, "exists")?, &false, "store exists")?;
    require_eq(field_str(store, "status")?, "unavailable", "store status")?;
    let degraded_reasons = field(data, "degraded_reasons")?
        .as_array()
        .ok_or_else(|| TestError(format!("degraded_reasons is not an array: {data}")))?;
    require(
        degraded_reasons
            .iter()
            .any(|reason| reason.as_str() == Some("validation_broker_store_missing")),
        format!("missing store should be listed as a degraded reason: {data}"),
    )?;
    let actions = field(data, "recommended_next_actions")?
        .as_array()
        .ok_or_else(|| TestError(format!("recommended_next_actions is not an array: {data}")))?;
    require(
        actions.iter().any(|action| {
            action
                .as_str()
                .is_some_and(|raw| raw.contains("Create the validation-broker store"))
        }),
        format!("missing store should recommend creating the broker store: {data}"),
    )
}

#[test]
fn doctor_swarm_validation_broker_json_reports_stale_slot_posture() -> TestResult {
    let store_path = create_stale_validation_broker_store()?;
    let store_path = store_path.display().to_string();
    let report = run_doctor_json(&[
        ("CARGO_TARGET_DIR", None),
        ("TMPDIR", None),
        ("PI_VALIDATION_BROKER_STORE", Some(store_path.as_str())),
    ])?;
    let finding = finding_by_schema(&report, SWARM_VALIDATION_BROKER_SCHEMA)?;
    require_eq(field_str(finding, "severity")?, "warn", "severity")?;
    require(
        field_str(finding, "title")?.contains("stale slot warnings"),
        format!("title should report stale slots: {finding}"),
    )?;

    let data = field(finding, "data")?;
    require_eq(
        field_str(field(data, "source_status")?, "validation_slot_store")?,
        "available",
        "slot-store source status",
    )?;
    let store = field(data, "store")?;
    require_eq(&field_bool(store, "configured")?, &true, "store configured")?;
    require_eq(&field_bool(store, "exists")?, &true, "store exists")?;
    require_eq(field_str(store, "status")?, "available", "store status")?;
    require_eq(&field_u64(store, "total_records")?, &2, "total records")?;
    require_eq(&field_u64(store, "total_slots")?, &1, "total slots")?;
    let current_slots = field(data, "current_slots")?;
    require_eq(&field_u64(current_slots, "stale")?, &1, "stale slots")?;
    require_eq(
        &field_u64(current_slots, "expired_at_report_time")?,
        &1,
        "expired slots",
    )?;
    require_eq(
        &field_u64(field(data, "stale_build_warnings")?, "count")?,
        &1,
        "stale warning count should not double-count stale expired slots",
    )?;
    let actions = field(data, "recommended_next_actions")?
        .as_array()
        .ok_or_else(|| TestError(format!("recommended_next_actions is not an array: {data}")))?;
    require(
        actions.iter().any(|action| {
            action
                .as_str()
                .is_some_and(|raw| raw.contains("Recover stale validation slots"))
        }),
        format!("stale slots should recommend owner-visible recovery: {data}"),
    )
}

#[cfg(unix)]
fn require_agent_mail_degraded_health_payload(data: &Value) -> TestResult {
    require_eq(field_str(data, "mode")?, "beads_soft_lock_fallback", "mode")?;
    let mail_health = field(data, "mail_health")?;
    require_eq(field_str(mail_health, "status")?, "error", "mail status")?;
    require_eq(
        field_str(field(mail_health, "semantic_readiness")?, "status")?,
        "fail",
        "semantic readiness status",
    )?;
    require_eq(
        field_str(field(mail_health, "recovery")?, "mode")?,
        "corrupt",
        "recovery mode",
    )?;
    require_eq(
        field_str(field(data, "fallback")?, "soft_lock")?,
        "beads",
        "soft_lock",
    )?;
    require_eq(
        &field_bool(field(data, "fallback")?, "non_blocking")?,
        &true,
        "fallback non_blocking",
    )?;
    let missing_tables = field(mail_health, "missing_schema_tables")?
        .as_array()
        .ok_or_else(|| TestError(format!("missing_schema_tables is not an array: {data}")))?;
    let has_all_missing_tables = ["projects", "agents", "messages", "message_recipients"]
        .into_iter()
        .all(|table| {
            missing_tables
                .iter()
                .any(|value| value.as_str() == Some(table))
        });
    require(
        has_all_missing_tables,
        format!("missing_schema_tables should include core tables: {data}"),
    )
}

#[cfg(unix)]
fn require_agent_mail_write_paths_blocked(data: &Value) -> TestResult {
    let write_paths = field(data, "write_paths")?;
    require_eq(
        &field_bool(write_paths, "expected_failed")?,
        &true,
        "write paths expected_failed",
    )?;
    let blocked = field(write_paths, "blocked_operations")?
        .as_array()
        .ok_or_else(|| TestError(format!("blocked_operations is not an array: {data}")))?;
    let has_blocked_writes = [
        "fetch_inbox",
        "send_message",
        "acknowledge_message",
        "file_reservation_paths",
    ]
    .into_iter()
    .all(|operation| {
        blocked
            .iter()
            .any(|value| value.as_str() == Some(operation))
    });
    require(
        has_blocked_writes,
        format!("blocked_operations should include message/reservation paths: {data}"),
    )
}

#[cfg(unix)]
#[test]
fn doctor_swarm_agent_mail_degraded_mode_json_reports_beads_fallback() -> TestResult {
    let fake_bin = create_fake_agent_mail_cli("fake-agent-mail")?;
    let path = path_with_fake_bin(&fake_bin)?;
    let report = run_doctor_json(&[
        ("PATH", Some(path.as_str())),
        ("AGENT_NAME", Some("GoldenGlacier")),
        ("AGENT_MAIL_AGENT", Some("GoldenGlacier")),
        ("CARGO_TARGET_DIR", None),
        ("TMPDIR", None),
    ])?;
    let finding = finding_by_schema(&report, SWARM_MAIL_DEGRADED_SCHEMA)?;
    require_eq(field_str(finding, "severity")?, "warn", "severity")?;
    require_eq(
        field_str(finding, "title")?,
        "Agent Mail degraded; Beads fallback required",
        "title",
    )?;
    require(
        field_str(finding, "detail")?.contains("fallback=beads_soft_lock"),
        format!("detail should mention Beads fallback: {finding}"),
    )?;

    let data = field(finding, "data")?;
    require_agent_mail_degraded_health_payload(data)?;

    let active_agents = field(data, "active_agents")?;
    require_eq(
        &field_u64(active_agents, "total_seen")?,
        &2,
        "active agent total_seen",
    )?;
    let rows = field(active_agents, "rows")?
        .as_array()
        .ok_or_else(|| TestError(format!("active_agents.rows is not an array: {data}")))?;
    require(
        rows.iter().any(|row| {
            row.get("name").and_then(Value::as_str) == Some("GoldenGlacier")
                && row
                    .get("task_description")
                    .and_then(Value::as_str)
                    .is_some_and(|description| description.contains("current bead"))
        }),
        format!("active agent rows should include GoldenGlacier activity: {data}"),
    )?;

    require_agent_mail_write_paths_blocked(data)?;
    Ok(())
}

#[test]
fn doctor_swarm_incident_diagnostics_json_reports_stable_components() -> TestResult {
    let report = run_doctor_json(&[("CARGO_TARGET_DIR", None), ("TMPDIR", None)])?;
    let finding = finding_by_schema(&report, SWARM_INCIDENT_DIAGNOSTICS_SCHEMA)?;
    require(
        ["pass", "warn", "fail"].contains(&field_str(finding, "severity")?),
        format!("incident diagnostics should use a stable severity: {finding}"),
    )?;

    let data = field(finding, "data")?;
    require_eq(
        field_str(data, "schema")?,
        SWARM_INCIDENT_DIAGNOSTICS_SCHEMA,
        "schema",
    )?;
    require_eq(field_str(data, "mode")?, "audit_only", "mode")?;
    require_eq(
        &field_bool(data, "mutation_performed")?,
        &false,
        "mutation flag",
    )?;
    require(
        field(data, "primary_failure_domain")?.is_string(),
        format!("primary_failure_domain should be a string: {data}"),
    )?;
    let components = field(data, "components")?;
    for domain in [
        "agent_mail",
        "beads",
        "rch",
        "temp_dirs",
        "resource_governor",
        "session_queue",
    ] {
        let component = field(components, domain)?;
        let _ = field_str(component, "status")?;
        let _ = field_str(component, "classification")?;
        require(
            field(component, "evidence")?.is_object(),
            "component evidence should be an object",
        )?;
    }
    require_eq(
        field_str(field(components, "temp_dirs")?, "classification")?,
        "temp_env_missing",
        "temp classification",
    )?;
    require(
        field(data, "failure_domains")?.is_array(),
        format!("failure_domains should be an array: {data}"),
    )?;
    require_eq(
        &field_bool(field(data, "redaction")?, "sensitive_line_redaction")?,
        &true,
        "redaction flag",
    )?;
    require_eq(
        &field_bool(field(data, "guards")?, "read_only")?,
        &true,
        "read-only guard",
    )
}

#[test]
fn doctor_swarm_temp_dir_json_freezes_root_posture() -> TestResult {
    let (expected_target, expected_target_exists) =
        create_expected_root_test_dir("expected-target")?;
    let outside_tmp = create_swarm_temp_test_dir(Path::new("/tmp"), "outside-tmp")?;
    let expected_target = expected_target.display().to_string();
    let outside_tmp = outside_tmp.display().to_string();
    let report = run_doctor_json(&[
        ("CARGO_TARGET_DIR", Some(expected_target.as_str())),
        ("TMPDIR", Some(outside_tmp.as_str())),
    ])?;

    let target_finding = temp_dir_finding(&report, "CARGO_TARGET_DIR")?;
    if expected_target_exists {
        require_eq(
            field_str(target_finding, "severity")?,
            "pass",
            "target severity",
        )?;
    } else {
        require_eq(
            field_str(target_finding, "severity")?,
            "warn",
            "target severity",
        )?;
        require(
            field_str(target_finding, "title")?.contains("does not point to a directory"),
            format!(
                "CARGO_TARGET_DIR should warn when expected root is unwritable: {target_finding}"
            ),
        )?;
    }
    let target_data = field(target_finding, "data")?;
    require_temp_dir_data_shape(target_data, "CARGO_TARGET_DIR")?;
    require_eq(
        field_str(target_data, "path")?,
        expected_target.as_str(),
        "target path",
    )?;
    require_eq(
        &field_bool(target_data, "exists")?,
        &expected_target_exists,
        "target exists",
    )?;
    require_eq(
        &field_bool(target_data, "under_expected_root")?,
        &true,
        "target under_expected_root",
    )?;
    if expected_target_exists {
        require_available_kb_shape(target_data)?;
    } else {
        require(
            field(target_data, "available_kb")?.is_null(),
            "uncreated target available_kb is null",
        )?;
    }

    let tmp_finding = temp_dir_finding(&report, "TMPDIR")?;
    require_eq(field_str(tmp_finding, "severity")?, "warn", "tmp severity")?;
    let tmp_data = field(tmp_finding, "data")?;
    require_temp_dir_data_shape(tmp_data, "TMPDIR")?;
    require_eq(
        field_str(tmp_data, "path")?,
        outside_tmp.as_str(),
        "tmp path",
    )?;
    require_eq(&field_bool(tmp_data, "exists")?, &true, "tmp exists")?;
    require_eq(
        &field_bool(tmp_data, "under_expected_root")?,
        &false,
        "tmp under_expected_root",
    )?;
    require_available_kb_shape(tmp_data)
}

#[test]
fn doctor_swarm_resource_preflight_json_reports_constrained_profile() -> TestResult {
    let root = create_swarm_temp_test_dir(Path::new("/tmp"), "constrained-resource-fixture")?;
    let (target_dir, target_exists) = create_expected_root_test_dir("constrained-target")?;
    let (tmp_dir, tmp_exists) = create_expected_root_test_dir("constrained-tmp")?;
    let cpu_max =
        write_resource_fixture_file(&root, ResourceFixtureFile::CpuMax, "200000 100000\n")?;
    let cpuset =
        write_resource_fixture_file(&root, ResourceFixtureFile::CpusetCpusEffective, "0-3\n")?;
    let numa = write_resource_fixture_file(&root, ResourceFixtureFile::NumaOnline, "0\n")?;
    let memory =
        write_resource_fixture_file(&root, ResourceFixtureFile::MemoryMax, "2147483648\n")?;
    let meminfo = write_resource_fixture_file(
        &root,
        ResourceFixtureFile::Meminfo,
        "MemTotal: 33554432 kB\n",
    )?;
    let target_dir = target_dir.display().to_string();
    let tmp_dir = tmp_dir.display().to_string();
    let cpu_max = cpu_max.display().to_string();
    let cpuset = cpuset.display().to_string();
    let numa = numa.display().to_string();
    let memory = memory.display().to_string();
    let meminfo = meminfo.display().to_string();

    let report = run_doctor_json(&[
        ("CARGO_TARGET_DIR", Some(target_dir.as_str())),
        ("TMPDIR", Some(tmp_dir.as_str())),
        ("PI_DOCTOR_CGROUP_CPU_MAX_PATH", Some(cpu_max.as_str())),
        ("PI_DOCTOR_CPUSET_CPUS_PATH", Some(cpuset.as_str())),
        ("PI_DOCTOR_NUMA_ONLINE_PATH", Some(numa.as_str())),
        ("PI_DOCTOR_CGROUP_MEMORY_MAX_PATH", Some(memory.as_str())),
        ("PI_DOCTOR_MEMINFO_PATH", Some(meminfo.as_str())),
        ("PI_DOCTOR_LOGICAL_CPU_CORES", Some("4")),
        ("PI_DOCTOR_LOCAL_BUILD_PROCESS_COUNT", Some("0")),
    ])?;
    let finding = finding_by_schema(&report, SWARM_RESOURCE_PREFLIGHT_SCHEMA)?;
    let data = field(finding, "data")?;

    if target_exists && tmp_exists {
        require_eq(field_str(finding, "severity")?, "pass", "severity")?;
        require(
            field(data, "critical_failures")?
                .as_array()
                .is_some_and(Vec::is_empty),
            format!("resource preflight should have no critical failures: {data}"),
        )?;
    }
    require_eq(
        field_str(data, "schema")?,
        SWARM_RESOURCE_PREFLIGHT_SCHEMA,
        "schema",
    )?;
    require_eq(
        &field_u64(field(data, "cpu")?, "effective_cores")?,
        &2,
        "effective cores",
    )?;
    require_eq(
        &field_u64(field(field(data, "cpu")?, "cpuset")?, "cpu_count")?,
        &4,
        "cpuset cpu count",
    )?;
    require_eq(
        &field_u64(field(data, "numa")?, "node_count")?,
        &1,
        "numa node count",
    )?;
    require_eq(
        &field_u64(field(data, "memory")?, "effective_limit_bytes")?,
        &2_147_483_648,
        "effective memory limit",
    )?;
    require(
        field(field(data, "recommended_budgets")?, "agent_concurrency")?
            .as_u64()
            .is_some(),
        format!("recommended budgets should include agent concurrency: {data}"),
    )?;
    require(
        field(data, "tmpfs_headroom")?
            .get("paths")
            .and_then(Value::as_array)
            .is_some_and(|paths| paths.len() == 2),
        format!("tmpfs_headroom should report target and tmp paths: {data}"),
    )?;
    let lane_placement = require_lane_placement_shape(data)?;
    if target_exists && tmp_exists {
        require_eq(
            field_str(lane_placement, "status")?,
            "degraded",
            "constrained lane status",
        )?;
    }
    require(
        field(lane_placement, "caveats")?
            .as_array()
            .is_some_and(|caveats| {
                caveats
                    .iter()
                    .any(|caveat| caveat == "tight_memory_limit:2048MiB")
            }),
        format!("constrained lane placement should report tight memory: {lane_placement}"),
    )
}

#[test]
fn doctor_swarm_resource_preflight_json_reports_high_capacity_profile() -> TestResult {
    let root = create_swarm_temp_test_dir(Path::new("/tmp"), "high-capacity-resource-fixture")?;
    let (target_dir, target_exists) = create_expected_root_test_dir("high-capacity-target")?;
    let (tmp_dir, tmp_exists) = create_expected_root_test_dir("high-capacity-tmp")?;
    let cpu_max = write_resource_fixture_file(&root, ResourceFixtureFile::CpuMax, "max 100000\n")?;
    let cpuset =
        write_resource_fixture_file(&root, ResourceFixtureFile::CpusetCpusEffective, "0-63\n")?;
    let numa = write_resource_fixture_file(&root, ResourceFixtureFile::NumaOnline, "0-3\n")?;
    let memory = write_resource_fixture_file(&root, ResourceFixtureFile::MemoryMax, "max\n")?;
    let meminfo = write_resource_fixture_file(
        &root,
        ResourceFixtureFile::Meminfo,
        "MemTotal: 268435456 kB\n",
    )?;
    let target_dir = target_dir.display().to_string();
    let tmp_dir = tmp_dir.display().to_string();
    let cpu_max = cpu_max.display().to_string();
    let cpuset = cpuset.display().to_string();
    let numa = numa.display().to_string();
    let memory = memory.display().to_string();
    let meminfo = meminfo.display().to_string();

    let report = run_doctor_json(&[
        ("CARGO_TARGET_DIR", Some(target_dir.as_str())),
        ("TMPDIR", Some(tmp_dir.as_str())),
        ("PI_DOCTOR_CGROUP_CPU_MAX_PATH", Some(cpu_max.as_str())),
        ("PI_DOCTOR_CPUSET_CPUS_PATH", Some(cpuset.as_str())),
        ("PI_DOCTOR_NUMA_ONLINE_PATH", Some(numa.as_str())),
        ("PI_DOCTOR_CGROUP_MEMORY_MAX_PATH", Some(memory.as_str())),
        ("PI_DOCTOR_MEMINFO_PATH", Some(meminfo.as_str())),
        ("PI_DOCTOR_LOGICAL_CPU_CORES", Some("64")),
        ("PI_DOCTOR_LOCAL_BUILD_PROCESS_COUNT", Some("0")),
    ])?;
    let finding = finding_by_schema(&report, SWARM_RESOURCE_PREFLIGHT_SCHEMA)?;
    let data = field(finding, "data")?;

    if target_exists && tmp_exists {
        require_eq(field_str(finding, "severity")?, "pass", "severity")?;
        require(
            field(data, "critical_failures")?
                .as_array()
                .is_some_and(Vec::is_empty),
            format!("resource preflight should have no critical failures: {data}"),
        )?;
    }
    require_eq(
        &field_u64(field(field(data, "cpu")?, "cpuset")?, "cpu_count")?,
        &64,
        "cpuset cpu count",
    )?;
    require(
        field(field(field(data, "cpu")?, "cgroup_quota")?, "unlimited")?
            .as_bool()
            .unwrap_or(false),
        format!("cgroup CPU quota should be unlimited: {data}"),
    )?;
    require_eq(
        &field_u64(field(data, "numa")?, "node_count")?,
        &4,
        "numa node count",
    )?;
    require_eq(
        &field_u64(field(data, "memory")?, "effective_limit_bytes")?,
        &(256_u64 * 1024 * 1024 * 1024),
        "effective memory limit",
    )?;
    let lane_placement = require_lane_placement_shape(data)?;
    if target_exists && tmp_exists {
        require_eq(
            field_str(lane_placement, "status")?,
            "ready",
            "high capacity lane status",
        )?;
    }
    require_eq(
        &field(lane_placement, "lane_groups")?
            .as_array()
            .ok_or_else(|| TestError(format!("lane_groups should be array: {lane_placement}")))?
            .len(),
        &4,
        "high capacity lane group count",
    )
}

#[test]
fn doctor_swarm_resource_preflight_json_reports_unknown_topology_lane_plan() -> TestResult {
    let root = create_swarm_temp_test_dir(Path::new("/tmp"), "unknown-topology-resource-fixture")?;
    let (target_dir, target_exists) = create_expected_root_test_dir("unknown-topology-target")?;
    let (tmp_dir, tmp_exists) = create_expected_root_test_dir("unknown-topology-tmp")?;
    let cpu_max = write_resource_fixture_file(&root, ResourceFixtureFile::CpuMax, "max 100000\n")?;
    let memory = write_resource_fixture_file(&root, ResourceFixtureFile::MemoryMax, "max\n")?;
    let meminfo = write_resource_fixture_file(
        &root,
        ResourceFixtureFile::Meminfo,
        "MemTotal: 16777216 kB\n",
    )?;
    let missing_cpuset = root.join("missing-cpuset").display().to_string();
    let missing_numa = root.join("missing-numa").display().to_string();
    let target_dir = target_dir.display().to_string();
    let tmp_dir = tmp_dir.display().to_string();
    let cpu_max = cpu_max.display().to_string();
    let memory = memory.display().to_string();
    let meminfo = meminfo.display().to_string();

    let report = run_doctor_json(&[
        ("CARGO_TARGET_DIR", Some(target_dir.as_str())),
        ("TMPDIR", Some(tmp_dir.as_str())),
        ("PI_DOCTOR_CGROUP_CPU_MAX_PATH", Some(cpu_max.as_str())),
        ("PI_DOCTOR_CPUSET_CPUS_PATH", Some(missing_cpuset.as_str())),
        ("PI_DOCTOR_NUMA_ONLINE_PATH", Some(missing_numa.as_str())),
        ("PI_DOCTOR_CGROUP_MEMORY_MAX_PATH", Some(memory.as_str())),
        ("PI_DOCTOR_MEMINFO_PATH", Some(meminfo.as_str())),
        ("PI_DOCTOR_LOCAL_BUILD_PROCESS_COUNT", Some("0")),
    ])?;
    let finding = finding_by_schema(&report, SWARM_RESOURCE_PREFLIGHT_SCHEMA)?;
    let data = field(finding, "data")?;
    let lane_placement = require_lane_placement_shape(data)?;

    if target_exists && tmp_exists {
        require_eq(
            field_str(lane_placement, "status")?,
            "degraded",
            "unknown topology lane status",
        )?;
    }
    let groups = field(lane_placement, "lane_groups")?
        .as_array()
        .ok_or_else(|| TestError(format!("lane_groups should be array: {lane_placement}")))?;
    require_eq(&groups.len(), &1, "unknown topology lane group count")?;
    let group = groups
        .first()
        .ok_or_else(|| TestError(format!("missing first lane group: {lane_placement}")))?;
    require_eq(field_str(group, "lane_id")?, "shared", "lane id")?;
    require_eq(
        field_str(group, "cpu_affinity_hint")?,
        "cpuset-unavailable",
        "cpu affinity hint",
    )?;
    require(
        field(lane_placement, "caveats")?
            .as_array()
            .is_some_and(|caveats| {
                caveats
                    .iter()
                    .any(|caveat| caveat == "numa_topology_unavailable")
            }),
        format!("unknown topology should report missing NUMA: {lane_placement}"),
    )?;
    require(
        field(lane_placement, "caveats")?
            .as_array()
            .is_some_and(|caveats| caveats.iter().any(|caveat| caveat == "cpuset_unavailable")),
        format!("unknown topology should report missing cpuset: {lane_placement}"),
    )
}
