//! `FrankenNode` Semantic Compatibility Harness (bd-3ar8v.7.3)
//!
//! Executes JS fixture scripts against Node.js and Bun to capture baseline
//! compatibility data, then verifies a machine-readable compatibility matrix.
//! If `FRANKEN_NODE_RUNTIME` points at a runtime executable, the same fixtures
//! are also executed against that runtime and reported as a separate leg.
//! Set `PI_GENERATE_FRANKEN_NODE_COMPATIBILITY_MATRIX=1` to regenerate the
//! committed matrix explicitly.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

const FRANKEN_NODE_RUNTIME_ENV: &str = "FRANKEN_NODE_RUNTIME";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture_dir() -> PathBuf {
    repo_root().join("tests/franken_node_compat/fixtures")
}

fn reports_dir() -> PathBuf {
    repo_root().join("tests/franken_node_compat/reports")
}

fn is_real_node(path: &str) -> bool {
    // Guard against Bun's `node` shim being mistaken for real Node in CI/worker images.
    let version_ok = Command::new(path).arg("--version").output().is_ok_and(|o| {
        if !o.status.success() {
            return false;
        }
        let version = String::from_utf8_lossy(&o.stdout);
        let mut chars = version.trim().chars();
        matches!(chars.next(), Some('v')) && chars.next().is_some_and(|c| c.is_ascii_digit())
    });
    if !version_ok {
        return false;
    }

    Command::new(path)
        .args([
            "-p",
            "(process.release && process.release.name) + ':' + !!(process.versions && process.versions.node) + ':' + !!(process.versions && process.versions.bun)",
        ])
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "node:true:false")
}

fn find_node() -> Option<String> {
    let candidates = [
        "/usr/bin/node",
        "/usr/local/bin/node",
        "/home/ubuntu/.nvm/versions/node/current/bin/node",
    ];
    if let Some(path) = candidates
        .into_iter()
        .find(|candidate| Path::new(candidate).exists() && is_real_node(candidate))
    {
        return Some(path.to_owned());
    }

    // Fallback: try `which node` and verify it's real
    if let Ok(out) = Command::new("which").arg("node").output() {
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !path.is_empty() && is_real_node(&path) {
            return Some(path);
        }
    }
    None
}

fn find_bun() -> Option<String> {
    let candidates = [
        "/home/ubuntu/.bun/bin/bun",
        "/usr/local/bin/bun",
        "/usr/bin/bun",
    ];
    candidates
        .into_iter()
        .find(|candidate| Path::new(candidate).exists())
        .map(str::to_owned)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FixtureCheck {
    name: String,
    pass: bool,
    detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FixtureResult {
    fixture_id: String,
    scenario_id: String,
    #[serde(default)]
    surface: Option<String>,
    checks: Vec<FixtureCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RuntimeResult {
    runtime: String,
    version: String,
    fixture_id: String,
    scenario_id: String,
    exit_code: i32,
    all_pass: bool,
    check_count: usize,
    pass_count: usize,
    fail_count: usize,
    checks: Vec<FixtureCheck>,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FrankenNodeStatus {
    NotConfigured,
    RuntimeMissing,
    Configured,
    RuntimeFailed,
    Executed,
}

impl std::fmt::Display for FrankenNodeStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotConfigured => "not_configured",
            Self::RuntimeMissing => "runtime_missing",
            Self::Configured => "configured",
            Self::RuntimeFailed => "runtime_failed",
            Self::Executed => "executed",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrankenNodeRuntimeConfig {
    env_var: String,
    status: FrankenNodeStatus,
    path: Option<String>,
    version: Option<String>,
    error: Option<String>,
}

impl FrankenNodeRuntimeConfig {
    fn not_configured() -> Self {
        Self {
            env_var: FRANKEN_NODE_RUNTIME_ENV.to_string(),
            status: FrankenNodeStatus::NotConfigured,
            path: None,
            version: None,
            error: Some(format!("{FRANKEN_NODE_RUNTIME_ENV} is not set")),
        }
    }

    fn missing(path: String) -> Self {
        let error = format!("configured runtime path does not exist: {path}");
        Self {
            env_var: FRANKEN_NODE_RUNTIME_ENV.to_string(),
            status: FrankenNodeStatus::RuntimeMissing,
            path: Some(path),
            version: None,
            error: Some(error),
        }
    }

    fn configured(path: String) -> Self {
        Self {
            env_var: FRANKEN_NODE_RUNTIME_ENV.to_string(),
            status: FrankenNodeStatus::Configured,
            version: Some(runtime_version(&path)),
            path: Some(path),
            error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrankenNodeFixtureVerdict {
    status: FrankenNodeStatus,
    all_pass: Option<bool>,
    exit_code: Option<i32>,
    pass_count: usize,
    check_count: usize,
    error: Option<String>,
    node_divergences: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScenarioVerdict {
    scenario_id: String,
    domain: String,
    criticality: String,
    node_pass_rate: f64,
    bun_pass_rate: f64,
    franken_node_pass_rate: Option<f64>,
    franken_node_status: FrankenNodeStatus,
    node_bun_parity: String,
    fixture_count: usize,
    fixtures: Vec<FixtureVerdict>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FixtureVerdict {
    fixture_id: String,
    node_all_pass: bool,
    bun_all_pass: bool,
    franken_node: FrankenNodeFixtureVerdict,
    divergences: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompatibilityMatrix {
    schema: String,
    bead_id: String,
    generated_at: String,
    node_version: String,
    bun_version: String,
    franken_node_runtime: FrankenNodeRuntimeConfig,
    scenarios: Vec<ScenarioVerdict>,
    summary: MatrixSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MatrixSummary {
    total_scenarios: usize,
    total_fixtures: usize,
    total_checks: usize,
    node_pass_rate: f64,
    bun_pass_rate: f64,
    franken_node_status: FrankenNodeStatus,
    franken_node_pass_rate: Option<f64>,
    franken_node_executed_fixture_count: usize,
    franken_node_status_counts: BTreeMap<FrankenNodeStatus, usize>,
    node_bun_divergence_count: usize,
    overall_parity: String,
}

fn runtime_version(runtime_path: &str) -> String {
    Command::new(runtime_path)
        .arg("--version")
        .output()
        .map_or_else(
            |_| "unknown".to_string(),
            |output| String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )
}

fn ratio(pass: usize, total: usize) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let pass = u32::try_from(pass).expect("pass counts should fit in u32");
    let total = u32::try_from(total).expect("total counts should fit in u32");
    f64::from(pass) / f64::from(total)
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn franken_node_runtime_config_from_env_value(value: Option<OsString>) -> FrankenNodeRuntimeConfig {
    let Some(raw_path) = value else {
        return FrankenNodeRuntimeConfig::not_configured();
    };
    let path = raw_path.to_string_lossy().trim().to_string();
    if path.is_empty() {
        return FrankenNodeRuntimeConfig::not_configured();
    }
    if !Path::new(&path).exists() {
        return FrankenNodeRuntimeConfig::missing(path);
    }
    FrankenNodeRuntimeConfig::configured(path)
}

fn find_franken_node_runtime() -> FrankenNodeRuntimeConfig {
    franken_node_runtime_config_from_env_value(std::env::var_os(FRANKEN_NODE_RUNTIME_ENV))
}

/// Run a JS fixture with the given runtime binary and return parsed result.
fn run_fixture(runtime_path: &str, fixture_path: &Path) -> RuntimeResult {
    let runtime_name = if runtime_path.contains("bun") {
        "bun"
    } else {
        "node"
    };

    run_fixture_as(runtime_name, runtime_path, fixture_path)
}

fn run_fixture_as(runtime_name: &str, runtime_path: &str, fixture_path: &Path) -> RuntimeResult {
    let version = runtime_version(runtime_path);

    let output = Command::new(runtime_path).arg(fixture_path).output();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let exit_code = out.status.code().unwrap_or(-1);

            if !out.status.success() {
                return RuntimeResult {
                    runtime: runtime_name.to_string(),
                    version,
                    fixture_id: fixture_path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    scenario_id: "unknown".to_string(),
                    exit_code,
                    all_pass: false,
                    check_count: 0,
                    pass_count: 0,
                    fail_count: 0,
                    checks: Vec::new(),
                    error: Some(format!(
                        "process exited with status {exit_code}; stderr: {}; stdout: {}",
                        truncate_chars(&stderr, 200),
                        truncate_chars(&stdout, 200)
                    )),
                };
            }

            match serde_json::from_str::<FixtureResult>(stdout.trim()) {
                Ok(result) => {
                    let pass_count = result.checks.iter().filter(|c| c.pass).count();
                    let fail_count = result.checks.len() - pass_count;
                    RuntimeResult {
                        runtime: runtime_name.to_string(),
                        version,
                        fixture_id: result.fixture_id,
                        scenario_id: result.scenario_id,
                        exit_code,
                        all_pass: fail_count == 0,
                        check_count: result.checks.len(),
                        pass_count,
                        fail_count,
                        checks: result.checks,
                        error: None,
                    }
                }
                Err(err) => RuntimeResult {
                    runtime: runtime_name.to_string(),
                    version,
                    fixture_id: fixture_path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    scenario_id: "unknown".to_string(),
                    exit_code,
                    all_pass: false,
                    check_count: 0,
                    pass_count: 0,
                    fail_count: 0,
                    checks: Vec::new(),
                    error: Some(format!(
                        "parse error: {err}; stdout: {}",
                        truncate_chars(&stdout, 200)
                    )),
                },
            }
        }
        Err(err) => RuntimeResult {
            runtime: runtime_name.to_string(),
            version,
            fixture_id: fixture_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            scenario_id: "unknown".to_string(),
            exit_code: -1,
            all_pass: false,
            check_count: 0,
            pass_count: 0,
            fail_count: 0,
            checks: Vec::new(),
            error: Some(format!("execution error: {err}")),
        },
    }
}

fn check_divergences(
    reference: &RuntimeResult,
    candidate: &RuntimeResult,
    label: &str,
) -> Vec<String> {
    let reference_checks: HashMap<&str, bool> = reference
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.pass))
        .collect();
    let mut divergences = Vec::new();
    for check in &candidate.checks {
        if let Some(&reference_pass) = reference_checks.get(check.name.as_str())
            && reference_pass != check.pass
        {
            let mut divergence = String::with_capacity(check.name.len() + label.len() + 32);
            let _ = write!(
                &mut divergence,
                "{}: node={}, {}={}",
                check.name, reference_pass, label, check.pass
            );
            divergences.push(divergence);
        }
    }
    divergences
}

fn franken_node_fixture_verdict(
    config: &FrankenNodeRuntimeConfig,
    fixture_path: &Path,
    node_result: &RuntimeResult,
) -> FrankenNodeFixtureVerdict {
    if matches!(
        config.status,
        FrankenNodeStatus::NotConfigured | FrankenNodeStatus::RuntimeMissing
    ) {
        return FrankenNodeFixtureVerdict {
            status: config.status,
            all_pass: None,
            exit_code: None,
            pass_count: 0,
            check_count: 0,
            error: config.error.clone(),
            node_divergences: Vec::new(),
        };
    }

    let Some(path) = config.path.as_deref() else {
        return FrankenNodeFixtureVerdict {
            status: FrankenNodeStatus::RuntimeMissing,
            all_pass: None,
            exit_code: None,
            pass_count: 0,
            check_count: 0,
            error: Some("configured runtime path missing from config".to_string()),
            node_divergences: Vec::new(),
        };
    };

    let result = run_fixture_as("franken_node", path, fixture_path);
    if result.error.is_some() || result.check_count == 0 {
        return FrankenNodeFixtureVerdict {
            status: FrankenNodeStatus::RuntimeFailed,
            all_pass: Some(false),
            exit_code: Some(result.exit_code),
            pass_count: result.pass_count,
            check_count: result.check_count,
            error: result.error,
            node_divergences: Vec::new(),
        };
    }

    let node_divergences = check_divergences(node_result, &result, "franken_node");
    FrankenNodeFixtureVerdict {
        status: FrankenNodeStatus::Executed,
        all_pass: Some(result.all_pass),
        exit_code: Some(result.exit_code),
        pass_count: result.pass_count,
        check_count: result.check_count,
        error: None,
        node_divergences,
    }
}

/// Scenario metadata from the contract.
struct ScenarioMeta {
    scenario_id: &'static str,
    domain: &'static str,
    criticality: &'static str,
    fixtures: &'static [&'static str],
}

const SCENARIOS: &[ScenarioMeta] = &[
    ScenarioMeta {
        scenario_id: "SCN-module-resolution-esm-cjs",
        domain: "module-resolution",
        criticality: "high",
        fixtures: &["esm_import.mjs", "cjs_require.cjs"],
    },
    ScenarioMeta {
        scenario_id: "SCN-node-builtin-apis",
        domain: "builtin-apis",
        criticality: "high",
        fixtures: &["builtin_apis.mjs"],
    },
    ScenarioMeta {
        scenario_id: "SCN-event-loop-io-ordering",
        domain: "event-loop-io",
        criticality: "high",
        fixtures: &["event_loop.mjs"],
    },
    ScenarioMeta {
        scenario_id: "SCN-error-and-diagnostics-parity",
        domain: "errors-diagnostics",
        criticality: "medium",
        fixtures: &["error_diagnostics.mjs"],
    },
];

fn compute_parity(node_rate: f64, bun_rate: f64) -> &'static str {
    if node_rate >= 1.0 && bun_rate >= 1.0 {
        "EXACT_PARITY"
    } else if node_rate >= 1.0 || bun_rate >= 1.0 {
        "ACCEPTABLE_SUPERSET"
    } else if node_rate >= 0.8 && bun_rate >= 0.8 {
        "PARTIAL_PARITY"
    } else {
        "INCOMPATIBLE"
    }
}

struct ScenarioRun {
    verdict: ScenarioVerdict,
    fixture_count: usize,
    check_count: usize,
    node_pass: usize,
    node_checks: usize,
    bun_pass: usize,
    bun_checks: usize,
    franken_node_pass: usize,
    franken_node_checks: usize,
    franken_node_executed_fixtures: usize,
    franken_node_status_counts: BTreeMap<FrankenNodeStatus, usize>,
    divergence_count: usize,
}

fn run_scenario(
    meta: &ScenarioMeta,
    node_path: &str,
    bun_path: &str,
    franken_node_runtime: &FrankenNodeRuntimeConfig,
    fixture_base: &Path,
) -> ScenarioRun {
    let mut fixture_verdicts = Vec::new();
    let mut scenario_node_pass = 0;
    let mut scenario_node_total = 0;
    let mut scenario_bun_pass = 0;
    let mut scenario_bun_total = 0;
    let mut scenario_franken_node_pass = 0;
    let mut scenario_franken_node_total = 0;
    let mut scenario_franken_node_executed_fixtures = 0;
    let mut scenario_franken_node_status_counts = BTreeMap::new();
    let mut scenario_check_count = 0;
    let mut scenario_divergences = 0;

    for fixture_file in meta.fixtures {
        let fixture_path = fixture_base.join(fixture_file);
        if !fixture_path.exists() {
            continue;
        }

        let node_result = run_fixture(node_path, &fixture_path);
        let bun_result = run_fixture(bun_path, &fixture_path);
        let franken_node =
            franken_node_fixture_verdict(franken_node_runtime, &fixture_path, &node_result);

        scenario_node_pass += node_result.pass_count;
        scenario_node_total += node_result.check_count;
        scenario_bun_pass += bun_result.pass_count;
        scenario_bun_total += bun_result.check_count;
        if franken_node.status == FrankenNodeStatus::Executed {
            scenario_franken_node_executed_fixtures += 1;
            scenario_franken_node_pass += franken_node.pass_count;
            scenario_franken_node_total += franken_node.check_count;
        }
        *scenario_franken_node_status_counts
            .entry(franken_node.status)
            .or_insert(0) += 1;
        scenario_check_count += node_result.check_count.max(bun_result.check_count);

        // Find divergences (where node and bun disagree)
        let divergences = check_divergences(&node_result, &bun_result, "bun");
        scenario_divergences += divergences.len();

        fixture_verdicts.push(FixtureVerdict {
            fixture_id: node_result.fixture_id.clone(),
            node_all_pass: node_result.all_pass,
            bun_all_pass: bun_result.all_pass,
            franken_node,
            divergences,
        });
    }

    let node_rate = ratio(scenario_node_pass, scenario_node_total);
    let bun_rate = ratio(scenario_bun_pass, scenario_bun_total);
    let franken_node_rate = if scenario_franken_node_total == 0 {
        None
    } else {
        Some(ratio(
            scenario_franken_node_pass,
            scenario_franken_node_total,
        ))
    };
    let franken_node_status = if scenario_franken_node_executed_fixtures > 0 {
        FrankenNodeStatus::Executed
    } else {
        franken_node_runtime.status
    };
    let fixture_count = fixture_verdicts.len();

    ScenarioRun {
        verdict: ScenarioVerdict {
            scenario_id: meta.scenario_id.to_string(),
            domain: meta.domain.to_string(),
            criticality: meta.criticality.to_string(),
            node_pass_rate: node_rate,
            bun_pass_rate: bun_rate,
            franken_node_pass_rate: franken_node_rate,
            franken_node_status,
            node_bun_parity: compute_parity(node_rate, bun_rate).to_string(),
            fixture_count,
            fixtures: fixture_verdicts,
        },
        fixture_count,
        check_count: scenario_check_count,
        node_pass: scenario_node_pass,
        node_checks: scenario_node_total,
        bun_pass: scenario_bun_pass,
        bun_checks: scenario_bun_total,
        franken_node_pass: scenario_franken_node_pass,
        franken_node_checks: scenario_franken_node_total,
        franken_node_executed_fixtures: scenario_franken_node_executed_fixtures,
        franken_node_status_counts: scenario_franken_node_status_counts,
        divergence_count: scenario_divergences,
    }
}

/// Run all fixtures and produce the compatibility matrix.
fn run_compatibility_matrix() -> Result<CompatibilityMatrix, String> {
    run_compatibility_matrix_with_franken_node(find_franken_node_runtime())
}

fn run_compatibility_matrix_with_franken_node(
    franken_node_runtime: FrankenNodeRuntimeConfig,
) -> Result<CompatibilityMatrix, String> {
    let node_path = find_node().ok_or_else(|| "Node.js not found".to_string())?;
    let bun_path = find_bun().ok_or_else(|| "Bun not found".to_string())?;
    let fixture_base = fixture_dir();

    let node_version = runtime_version(&node_path);
    let bun_version = runtime_version(&bun_path);

    let mut scenarios = Vec::new();
    let mut total_fixtures = 0;
    let mut total_checks = 0;
    let mut total_node_pass = 0;
    let mut total_bun_pass = 0;
    let mut total_node_checks = 0;
    let mut total_bun_checks = 0;
    let mut total_franken_node_pass = 0;
    let mut total_franken_node_checks = 0;
    let mut total_franken_node_executed_fixtures = 0;
    let mut franken_node_status_counts = BTreeMap::new();
    let mut total_divergences = 0;

    for meta in SCENARIOS {
        let run = run_scenario(
            meta,
            &node_path,
            &bun_path,
            &franken_node_runtime,
            &fixture_base,
        );
        total_fixtures += run.fixture_count;
        total_checks += run.check_count;
        total_node_pass += run.node_pass;
        total_node_checks += run.node_checks;
        total_bun_pass += run.bun_pass;
        total_bun_checks += run.bun_checks;
        total_franken_node_pass += run.franken_node_pass;
        total_franken_node_checks += run.franken_node_checks;
        total_franken_node_executed_fixtures += run.franken_node_executed_fixtures;
        for (status, count) in run.franken_node_status_counts {
            *franken_node_status_counts.entry(status).or_insert(0) += count;
        }
        total_divergences += run.divergence_count;
        scenarios.push(run.verdict);
    }

    let overall_node_rate = ratio(total_node_pass, total_node_checks);
    let overall_bun_rate = ratio(total_bun_pass, total_bun_checks);
    let overall_franken_node_rate = if total_franken_node_checks == 0 {
        None
    } else {
        Some(ratio(total_franken_node_pass, total_franken_node_checks))
    };
    let franken_node_status = if total_franken_node_executed_fixtures > 0 {
        FrankenNodeStatus::Executed
    } else {
        franken_node_runtime.status
    };

    let overall_parity = if total_divergences == 0 {
        "EXACT_PARITY"
    } else if overall_node_rate >= 0.95 && overall_bun_rate >= 0.95 {
        "ACCEPTABLE_SUPERSET"
    } else {
        "PARTIAL_PARITY"
    };

    Ok(CompatibilityMatrix {
        schema: "pi.frankennode.compatibility_matrix.v1".to_string(),
        bead_id: "bd-3ar8v.7.3".to_string(),
        generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        node_version,
        bun_version,
        franken_node_runtime,
        scenarios,
        summary: MatrixSummary {
            total_scenarios: SCENARIOS.len(),
            total_fixtures,
            total_checks,
            node_pass_rate: overall_node_rate,
            bun_pass_rate: overall_bun_rate,
            franken_node_status,
            franken_node_pass_rate: overall_franken_node_rate,
            franken_node_executed_fixture_count: total_franken_node_executed_fixtures,
            franken_node_status_counts,
            node_bun_divergence_count: total_divergences,
            overall_parity: overall_parity.to_string(),
        },
    })
}

// ─── Tests ───

/// Helper: skip test if runtime not available.
macro_rules! require_node {
    () => {
        match find_node() {
            Some(p) => p,
            None => {
                eprintln!("SKIP: Node.js not found on this machine");
                return;
            }
        }
    };
}

macro_rules! require_bun {
    () => {
        match find_bun() {
            Some(p) => p,
            None => {
                eprintln!("SKIP: Bun not found on this machine");
                return;
            }
        }
    };
}

#[test]
fn node_detection_rejects_bun_node_shim_when_present() {
    let bun_node_shim = "/home/ubuntu/.bun/bin/node";
    if !Path::new(bun_node_shim).exists() {
        eprintln!("SKIP: Bun node shim not present on this machine");
        return;
    }
    assert!(
        !is_real_node(bun_node_shim),
        "Bun's node shim must never be accepted as a real Node runtime: {bun_node_shim}"
    );
}

fn assert_fixture_all_pass(result: &RuntimeResult, label: &str) {
    assert!(
        result.error.is_none(),
        "{label} fixture error: {:?}",
        result.error
    );
    assert!(
        result.all_pass,
        "{label}: {}/{} checks passed. Failures: {:?}",
        result.pass_count,
        result.check_count,
        result.checks.iter().filter(|c| !c.pass).collect::<Vec<_>>()
    );
}

fn fixture_check(name: &str, pass: bool, detail: &str) -> FixtureCheck {
    FixtureCheck {
        name: name.to_string(),
        pass,
        detail: detail.to_string(),
    }
}

fn fake_node_result(checks: Vec<FixtureCheck>) -> RuntimeResult {
    let pass_count = checks.iter().filter(|check| check.pass).count();
    let fail_count = checks.len() - pass_count;
    RuntimeResult {
        runtime: "node".to_string(),
        version: "v-test".to_string(),
        fixture_id: "fake_fixture".to_string(),
        scenario_id: "fake_scenario".to_string(),
        exit_code: 0,
        all_pass: fail_count == 0,
        check_count: checks.len(),
        pass_count,
        fail_count,
        checks,
        error: None,
    }
}

#[cfg(unix)]
fn write_fake_franken_node_runtime(payload: &str) -> std::io::Result<(tempfile::TempDir, PathBuf)> {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("franken-node-fake");
    let script = format!(
        r#"#!/bin/sh
if [ "${{1:-}}" = "--version" ]; then
  printf '%s\n' 'franken-node-test 0.0.0'
  exit 0
fi
cat <<'JSON'
{payload}
JSON
"#
    );
    std::fs::write(&path, script)?;
    let mut permissions = std::fs::metadata(&path)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions)?;
    Ok((dir, path))
}

#[test]
fn franken_node_runtime_config_absent_is_not_configured() {
    let config = franken_node_runtime_config_from_env_value(None);

    assert_eq!(config.env_var, FRANKEN_NODE_RUNTIME_ENV);
    assert_eq!(config.status, FrankenNodeStatus::NotConfigured);
    assert!(config.path.is_none());
    assert!(config.version.is_none());
    assert!(
        config
            .error
            .as_deref()
            .is_some_and(|err| err.contains(FRANKEN_NODE_RUNTIME_ENV))
    );

    let node = fake_node_result(vec![fixture_check("basic", true, "node ok")]);
    let verdict = franken_node_fixture_verdict(&config, Path::new("fake_fixture.mjs"), &node);
    assert_eq!(verdict.status, FrankenNodeStatus::NotConfigured);
    assert_eq!(verdict.all_pass, None);
    assert_eq!(verdict.check_count, 0);
}

#[test]
fn franken_node_runtime_config_missing_path_is_runtime_missing() {
    let missing_path = format!(
        "/tmp/pi-agent-rust-missing-franken-node-{}",
        std::process::id()
    );
    let config = franken_node_runtime_config_from_env_value(Some(OsString::from(&missing_path)));

    assert_eq!(config.status, FrankenNodeStatus::RuntimeMissing);
    assert_eq!(config.path.as_deref(), Some(missing_path.as_str()));
    assert!(config.version.is_none());
    assert!(
        config
            .error
            .as_deref()
            .is_some_and(|err| err.contains(&missing_path))
    );

    let node = fake_node_result(vec![fixture_check("basic", true, "node ok")]);
    let verdict = franken_node_fixture_verdict(&config, Path::new("fake_fixture.mjs"), &node);
    assert_eq!(verdict.status, FrankenNodeStatus::RuntimeMissing);
    assert_eq!(verdict.all_pass, None);
    assert_eq!(verdict.check_count, 0);
}

#[cfg(unix)]
#[test]
fn franken_node_fake_runtime_success_executes_fixture() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, runtime) = write_fake_franken_node_runtime(
        r#"{"fixture_id":"fake_fixture","scenario_id":"fake_scenario","checks":[{"name":"basic","pass":true,"detail":"franken ok"}]}"#,
    )?;
    let config =
        franken_node_runtime_config_from_env_value(Some(OsString::from(runtime.as_os_str())));
    let node = fake_node_result(vec![fixture_check("basic", true, "node ok")]);

    let verdict = franken_node_fixture_verdict(&config, Path::new("fake_fixture.mjs"), &node);

    assert_eq!(config.status, FrankenNodeStatus::Configured);
    assert!(
        config.version.is_some(),
        "configured runtime should record best-effort version metadata"
    );
    assert_eq!(verdict.status, FrankenNodeStatus::Executed);
    assert_eq!(verdict.all_pass, Some(true));
    assert_eq!(verdict.exit_code, Some(0));
    assert_eq!(verdict.pass_count, 1);
    assert_eq!(verdict.check_count, 1);
    assert!(verdict.node_divergences.is_empty());
    Ok(())
}

#[cfg(unix)]
#[test]
fn franken_node_fake_runtime_invalid_output_is_runtime_failed()
-> Result<(), Box<dyn std::error::Error>> {
    let (_dir, runtime) = write_fake_franken_node_runtime("not json")?;
    let config =
        franken_node_runtime_config_from_env_value(Some(OsString::from(runtime.as_os_str())));
    let node = fake_node_result(vec![fixture_check("basic", true, "node ok")]);

    let verdict = franken_node_fixture_verdict(&config, Path::new("fake_fixture.mjs"), &node);

    assert_eq!(verdict.status, FrankenNodeStatus::RuntimeFailed);
    assert_eq!(verdict.all_pass, Some(false));
    assert_eq!(verdict.exit_code, Some(0));
    assert_eq!(verdict.check_count, 0);
    assert!(
        verdict
            .error
            .as_deref()
            .is_some_and(|err| err.contains("parse error"))
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn franken_node_fake_runtime_output_diff_reports_node_divergence()
-> Result<(), Box<dyn std::error::Error>> {
    let (_dir, runtime) = write_fake_franken_node_runtime(
        r#"{"fixture_id":"fake_fixture","scenario_id":"fake_scenario","checks":[{"name":"basic","pass":false,"detail":"franken diverged"}]}"#,
    )?;
    let config =
        franken_node_runtime_config_from_env_value(Some(OsString::from(runtime.as_os_str())));
    let node = fake_node_result(vec![fixture_check("basic", true, "node ok")]);

    let verdict = franken_node_fixture_verdict(&config, Path::new("fake_fixture.mjs"), &node);

    assert_eq!(verdict.status, FrankenNodeStatus::Executed);
    assert_eq!(verdict.all_pass, Some(false));
    assert_eq!(verdict.pass_count, 0);
    assert_eq!(verdict.check_count, 1);
    assert_eq!(
        verdict.node_divergences,
        vec!["basic: node=true, franken_node=false"]
    );
    Ok(())
}

#[test]
fn compat_harness_node_esm_import_all_pass() {
    let node = require_node!();
    let result = run_fixture(&node, &fixture_dir().join("esm_import.mjs"));
    assert_fixture_all_pass(&result, "Node ESM import");
}

#[test]
fn compat_harness_node_cjs_require_all_pass() {
    let node = require_node!();
    let result = run_fixture(&node, &fixture_dir().join("cjs_require.cjs"));
    assert_fixture_all_pass(&result, "Node CJS require");
}

#[test]
fn compat_harness_node_builtin_apis_all_pass() {
    let node = require_node!();
    let result = run_fixture(&node, &fixture_dir().join("builtin_apis.mjs"));
    assert_fixture_all_pass(&result, "Node builtin APIs");
}

#[test]
fn compat_harness_node_event_loop_ordering() {
    let node = require_node!();
    let result = run_fixture(&node, &fixture_dir().join("event_loop.mjs"));
    assert_fixture_all_pass(&result, "Node event loop ordering");
}

#[test]
fn compat_harness_node_error_diagnostics() {
    let node = require_node!();
    let result = run_fixture(&node, &fixture_dir().join("error_diagnostics.mjs"));
    assert_fixture_all_pass(&result, "Node error diagnostics");
}

#[test]
fn compat_harness_bun_esm_import_all_pass() {
    let bun = require_bun!();
    let result = run_fixture(&bun, &fixture_dir().join("esm_import.mjs"));
    assert_fixture_all_pass(&result, "Bun ESM import");
}

#[test]
fn compat_harness_bun_cjs_require_all_pass() {
    let bun = require_bun!();
    let result = run_fixture(&bun, &fixture_dir().join("cjs_require.cjs"));
    assert_fixture_all_pass(&result, "Bun CJS require");
}

#[test]
fn compat_harness_bun_builtin_apis_all_pass() {
    let bun = require_bun!();
    let result = run_fixture(&bun, &fixture_dir().join("builtin_apis.mjs"));
    assert_fixture_all_pass(&result, "Bun builtin APIs");
}

#[test]
fn compat_harness_bun_event_loop_ordering() {
    let bun = require_bun!();
    let result = run_fixture(&bun, &fixture_dir().join("event_loop.mjs"));
    assert_fixture_all_pass(&result, "Bun event loop ordering");
}

#[test]
fn compat_harness_captures_node_bun_divergences() {
    let node = require_node!();
    let bun = require_bun!();
    let fixture = fixture_dir().join("error_diagnostics.mjs");

    let node_result = run_fixture(&node, &fixture);
    let bun_result = run_fixture(&bun, &fixture);

    // Bun is known to diverge on stack_has_function_names
    let node_checks: HashMap<&str, bool> = node_result
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.pass))
        .collect();
    let mut divergences = Vec::new();
    for check in &bun_result.checks {
        if let Some(&node_pass) = node_checks.get(check.name.as_str())
            && node_pass != check.pass
        {
            divergences.push(check.name.clone());
        }
    }

    // We expect at least the stack_has_function_names divergence
    assert!(
        !divergences.is_empty(),
        "expected at least one Node/Bun divergence in error_diagnostics"
    );
    println!(
        "Captured {} divergence(s): {:?}",
        divergences.len(),
        divergences
    );
}

fn print_compatibility_matrix_summary(matrix: &CompatibilityMatrix, artifact_path: &Path) {
    println!("\n=== FrankenNode Compatibility Matrix ===");
    println!("  Node version: {}", matrix.node_version);
    println!("  Bun version:  {}", matrix.bun_version);
    println!(
        "  FrankenNode:  {} ({})",
        matrix.summary.franken_node_status,
        matrix
            .franken_node_runtime
            .path
            .as_deref()
            .unwrap_or(FRANKEN_NODE_RUNTIME_ENV)
    );
    println!("  Scenarios:    {}", matrix.summary.total_scenarios);
    println!("  Fixtures:     {}", matrix.summary.total_fixtures);
    println!("  Checks:       {}", matrix.summary.total_checks);
    println!(
        "  Node rate:    {:.1}%",
        matrix.summary.node_pass_rate * 100.0
    );
    println!(
        "  Bun rate:     {:.1}%",
        matrix.summary.bun_pass_rate * 100.0
    );
    println!(
        "  Divergences:  {}",
        matrix.summary.node_bun_divergence_count
    );
    println!("  Parity:       {}", matrix.summary.overall_parity);
    for scenario in &matrix.scenarios {
        println!(
            "  [{:6}] {}: node={:.0}% bun={:.0}% → {}",
            scenario.criticality,
            scenario.scenario_id,
            scenario.node_pass_rate * 100.0,
            scenario.bun_pass_rate * 100.0,
            scenario.node_bun_parity,
        );
    }
    println!("  Artifact: {}", artifact_path.display());
}

#[test]
fn generate_compatibility_matrix() {
    if find_node().is_none() || find_bun().is_none() {
        eprintln!("SKIP: generate_compatibility_matrix requires both Node.js and Bun");
        return;
    }
    let matrix = match run_compatibility_matrix() {
        Ok(matrix) => matrix,
        Err(err) => {
            eprintln!("SKIP: generate_compatibility_matrix runtime discovery failed: {err}");
            return;
        }
    };

    // Validate structure
    assert_eq!(matrix.schema, "pi.frankennode.compatibility_matrix.v1");
    assert_eq!(matrix.bead_id, "bd-3ar8v.7.3");
    assert_eq!(
        matrix.franken_node_runtime.env_var,
        FRANKEN_NODE_RUNTIME_ENV
    );
    assert_eq!(matrix.summary.total_scenarios, 4);
    assert!(
        matrix.summary.total_fixtures >= 5,
        "expected at least 5 fixtures, got {}",
        matrix.summary.total_fixtures
    );
    assert!(
        matrix.summary.total_checks >= 20,
        "expected at least 20 checks, got {}",
        matrix.summary.total_checks
    );

    // Node should pass all checks
    assert!(
        matrix.summary.node_pass_rate >= 1.0,
        "Node pass rate should be 100%, got {:.1}%",
        matrix.summary.node_pass_rate * 100.0
    );

    // Bun has known divergences
    assert!(
        matrix.summary.bun_pass_rate >= 0.9,
        "Bun pass rate should be >= 90%, got {:.1}%",
        matrix.summary.bun_pass_rate * 100.0
    );

    // Should capture divergences
    assert!(
        matrix.summary.node_bun_divergence_count >= 1,
        "should capture at least 1 Node/Bun divergence"
    );
    assert_eq!(matrix.summary.overall_parity, "ACCEPTABLE_SUPERSET");
    assert!(matches!(
        matrix.franken_node_runtime.status,
        FrankenNodeStatus::NotConfigured
            | FrankenNodeStatus::RuntimeMissing
            | FrankenNodeStatus::Configured
            | FrankenNodeStatus::Executed
    ));
    assert!(
        !matrix.summary.franken_node_status_counts.is_empty(),
        "FrankenNode status counts must distinguish absence, failure, or execution"
    );

    // High-criticality scenarios should have good rates
    for scenario in &matrix.scenarios {
        if scenario.criticality == "high" {
            assert!(
                scenario.node_pass_rate >= 1.0,
                "high-criticality scenario {} should have 100% Node pass rate",
                scenario.scenario_id
            );
        }
    }

    // Ordinary test runs verify the committed artifact without mutating the
    // source tree. Regeneration is an explicit maintainer operation.
    let reports = reports_dir();
    let artifact_path = reports.join("compatibility_matrix.json");
    let json = serde_json::to_string_pretty(&matrix).expect("serialize matrix");
    let generate = matches!(
        std::env::var("PI_GENERATE_FRANKEN_NODE_COMPATIBILITY_MATRIX").as_deref(),
        Ok("1")
    );
    if generate {
        std::fs::create_dir_all(&reports).expect("create reports dir");
        std::fs::write(&artifact_path, format!("{json}\n"))
            .expect("write compatibility matrix artifact");
    } else {
        let committed_json = std::fs::read_to_string(&artifact_path)
            .expect("read committed compatibility matrix artifact");
        let mut committed: serde_json::Value = serde_json::from_str(&committed_json)
            .expect("parse committed compatibility matrix artifact");
        let mut computed: serde_json::Value =
            serde_json::from_str(&json).expect("parse computed compatibility matrix artifact");
        let committed_generated_at = committed
            .as_object_mut()
            .and_then(|object| object.remove("generated_at"));
        let computed_generated_at = computed
            .as_object_mut()
            .and_then(|object| object.remove("generated_at"));
        assert!(
            committed_generated_at.is_some(),
            "committed compatibility matrix is missing generated_at"
        );
        assert!(
            computed_generated_at.is_some(),
            "computed compatibility matrix is missing generated_at"
        );
        assert_eq!(
            committed, computed,
            "committed FrankenNode compatibility matrix is stale; regenerate explicitly with \
             PI_GENERATE_FRANKEN_NODE_COMPATIBILITY_MATRIX=1 cargo test \
             --test franken_node_compat_harness generate_compatibility_matrix -- --exact"
        );
    }

    print_compatibility_matrix_summary(&matrix, &artifact_path);
}

#[test]
fn compatibility_matrix_tracks_not_configured_franken_node_leg() {
    if find_node().is_none() || find_bun().is_none() {
        eprintln!(
            "SKIP: compatibility_matrix_tracks_not_configured_franken_node_leg requires both Node.js and Bun"
        );
        return;
    }

    let matrix = match run_compatibility_matrix_with_franken_node(
        FrankenNodeRuntimeConfig::not_configured(),
    ) {
        Ok(matrix) => matrix,
        Err(err) => {
            eprintln!(
                "SKIP: compatibility_matrix_tracks_not_configured_franken_node_leg runtime discovery failed: {err}"
            );
            return;
        }
    };

    assert_eq!(
        matrix.franken_node_runtime.status,
        FrankenNodeStatus::NotConfigured
    );
    assert_eq!(
        matrix.summary.franken_node_status,
        FrankenNodeStatus::NotConfigured
    );
    assert_eq!(matrix.summary.franken_node_pass_rate, None);
    assert_eq!(matrix.summary.franken_node_executed_fixture_count, 0);
    assert_eq!(
        matrix
            .summary
            .franken_node_status_counts
            .get(&FrankenNodeStatus::NotConfigured),
        Some(&matrix.summary.total_fixtures)
    );
    assert!(
        matrix
            .scenarios
            .iter()
            .all(|scenario| scenario.franken_node_status == FrankenNodeStatus::NotConfigured)
    );
}
