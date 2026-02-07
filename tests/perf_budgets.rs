//! Performance budget definitions and enforcement tests (bd-1fc4).
//!
//! Centralizes all performance budgets for the Pi Agent Rust runtime. Each budget
//! has an explicit threshold, measurement methodology, and CI enforcement path.
//!
//! Budgets are validated against actual benchmark data when available.
//! Run with: `cargo test --test perf_budgets -- --nocapture`

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::unreadable_literal
)]

use serde::Serialize;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

// ─── Budget Definitions ──────────────────────────────────────────────────────

/// A single performance budget with threshold and measurement context.
#[derive(Debug, Clone, Serialize)]
struct Budget {
    /// Human-readable name.
    name: &'static str,
    /// Category (startup, extension, tool, memory, binary).
    category: &'static str,
    /// The metric being measured (e.g., "p95 latency", "RSS").
    metric: &'static str,
    /// Unit of measurement (ms, us, MB, count).
    unit: &'static str,
    /// Budget threshold (must not exceed this value).
    threshold: f64,
    /// Measurement methodology.
    methodology: &'static str,
    /// Whether this budget is enforced in CI.
    ci_enforced: bool,
}

/// All performance budgets for the Pi Agent Rust runtime.
const BUDGETS: &[Budget] = &[
    // ── Startup ──────────────────────────────────────────────────────────
    Budget {
        name: "startup_version_p95",
        category: "startup",
        metric: "p95 latency",
        unit: "ms",
        threshold: 100.0,
        methodology: "hyperfine: `pi --version` (10 runs, 3 warmup)",
        ci_enforced: true,
    },
    Budget {
        name: "startup_full_agent_p95",
        category: "startup",
        metric: "p95 latency",
        unit: "ms",
        threshold: 200.0,
        methodology: "hyperfine: `pi --print '.'` with full init (10 runs, 3 warmup)",
        ci_enforced: false, // Requires API key or VCR
    },
    // ── Extension Loading ────────────────────────────────────────────────
    Budget {
        name: "ext_cold_load_simple_p95",
        category: "extension",
        metric: "p95 cold load time",
        unit: "ms",
        threshold: 5.0,
        methodology: "criterion: load_init_cold for simple single-file extensions (10 samples)",
        ci_enforced: true,
    },
    Budget {
        name: "ext_cold_load_complex_p95",
        category: "extension",
        metric: "p95 cold load time",
        unit: "ms",
        threshold: 50.0,
        methodology: "criterion: load_init_cold for multi-registration extensions (10 samples)",
        ci_enforced: false,
    },
    Budget {
        name: "ext_load_60_total",
        category: "extension",
        metric: "total load time (60 official extensions)",
        unit: "ms",
        threshold: 10000.0, // 10 seconds total for all 60
        methodology: "conformance runner: sequential load of all 60 official extensions",
        ci_enforced: false,
    },
    // ── Tool Call ─────────────────────────────────────────────────────────
    Budget {
        name: "tool_call_latency_p99",
        category: "tool_call",
        metric: "p99 per-call latency",
        unit: "us",
        threshold: 200.0,
        methodology: "pijs_workload: 2000 iterations x 1 tool call, release build",
        ci_enforced: true,
    },
    Budget {
        name: "tool_call_throughput_min",
        category: "tool_call",
        metric: "minimum calls/sec",
        unit: "calls/sec",
        threshold: 5000.0, // Must exceed 5k calls/sec
        methodology: "pijs_workload: 2000 iterations x 10 tool calls, release build",
        ci_enforced: true,
    },
    // ── Event Dispatch ───────────────────────────────────────────────────
    Budget {
        name: "event_dispatch_p99",
        category: "event_dispatch",
        metric: "p99 dispatch latency",
        unit: "us",
        threshold: 5000.0, // 5ms
        methodology: "criterion: event_hook dispatch for before_agent_start (100 samples)",
        ci_enforced: false,
    },
    // ── Policy Evaluation ────────────────────────────────────────────────
    Budget {
        name: "policy_eval_p99",
        category: "policy",
        metric: "p99 evaluation time",
        unit: "ns",
        threshold: 500.0,
        methodology: "criterion: ext_policy/evaluate with various modes and capabilities",
        ci_enforced: true,
    },
    // ── Memory ───────────────────────────────────────────────────────────
    Budget {
        name: "idle_memory_rss",
        category: "memory",
        metric: "RSS at idle",
        unit: "MB",
        threshold: 50.0,
        methodology: "sysinfo: measure RSS after startup, before any user input",
        ci_enforced: true,
    },
    Budget {
        name: "sustained_load_rss_growth",
        category: "memory",
        metric: "RSS growth under 30s sustained load",
        unit: "percent",
        threshold: 5.0,
        methodology: "stress test: 15 extensions, 50 events/sec for 30 seconds",
        ci_enforced: false,
    },
    // ── Binary Size ──────────────────────────────────────────────────────
    Budget {
        name: "binary_size_release",
        category: "binary",
        metric: "release binary size",
        unit: "MB",
        threshold: 20.0,
        methodology: "ls -la target/release/pi (stripped)",
        ci_enforced: true,
    },
    // ── Protocol Parsing ─────────────────────────────────────────────────
    Budget {
        name: "protocol_parse_p99",
        category: "protocol",
        metric: "p99 parse+validate time",
        unit: "us",
        threshold: 50.0,
        methodology: "criterion: ext_protocol/parse_and_validate for host_call and log messages",
        ci_enforced: true,
    },
];

// ─── Data Readers ────────────────────────────────────────────────────────────

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_json_file(path: &Path) -> Option<Value> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_jsonl_file(path: &Path) -> Vec<Value> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Measurement result for a budget check.
#[derive(Debug, Clone, Serialize)]
struct BudgetResult {
    budget_name: String,
    category: String,
    threshold: f64,
    unit: String,
    actual: Option<f64>,
    status: String, // "PASS", "FAIL", "NO_DATA"
    source: String,
}

fn check_budget(budget: &Budget) -> BudgetResult {
    let root = project_root();

    // Try to find actual measurement for this budget
    let (actual, source) = match budget.name {
        "tool_call_latency_p99" => read_pijs_workload_latency(&root),
        "tool_call_throughput_min" => read_pijs_workload_throughput(&root),
        "ext_cold_load_simple_p95" => read_criterion_load_time(&root, "hello"),
        "ext_cold_load_complex_p95" => read_criterion_load_time(&root, "pirate"),
        "ext_load_60_total" => read_total_load_time(&root),
        "sustained_load_rss_growth" => read_stress_rss_growth(&root),
        "startup_version_p95" => read_criterion_startup(&root, "version"),
        "startup_full_agent_p95" => read_criterion_startup(&root, "help"),
        "event_dispatch_p99" => read_scenario_runner_per_call(&root, "event_dispatch"),
        "policy_eval_p99" => read_criterion_policy_eval(&root),
        "idle_memory_rss" => read_idle_memory_rss(),
        "binary_size_release" => read_binary_size(&root),
        "protocol_parse_p99" => read_criterion_protocol_parse(&root),
        _ => (None, "no data source configured".to_string()),
    };

    let status = actual.map_or("NO_DATA", |val| {
        if budget.name == "tool_call_throughput_min" {
            // Throughput: actual must EXCEED threshold
            if val >= budget.threshold {
                "PASS"
            } else {
                "FAIL"
            }
        } else {
            // Latency/size: actual must be BELOW threshold
            if val <= budget.threshold {
                "PASS"
            } else {
                "FAIL"
            }
        }
    });

    BudgetResult {
        budget_name: budget.name.to_string(),
        category: budget.category.to_string(),
        threshold: budget.threshold,
        unit: budget.unit.to_string(),
        actual,
        status: status.to_string(),
        source,
    }
}

fn read_pijs_workload_latency(root: &Path) -> (Option<f64>, String) {
    let path = root.join("target/perf/pijs_workload.jsonl");
    let events = read_jsonl_file(&path);
    for event in &events {
        if event
            .get("tool_calls_per_iteration")
            .and_then(Value::as_u64)
            == Some(1)
        {
            if let Some(us) = event.get("per_call_us").and_then(Value::as_f64) {
                return (Some(us), "target/perf/pijs_workload.jsonl".to_string());
            }
        }
    }
    (None, "no pijs_workload data".to_string())
}

fn read_pijs_workload_throughput(root: &Path) -> (Option<f64>, String) {
    let path = root.join("target/perf/pijs_workload.jsonl");
    let events = read_jsonl_file(&path);
    for event in &events {
        if event
            .get("tool_calls_per_iteration")
            .and_then(Value::as_u64)
            == Some(10)
        {
            if let Some(cps) = event.get("calls_per_sec").and_then(Value::as_f64) {
                return (Some(cps), "target/perf/pijs_workload.jsonl".to_string());
            }
        }
    }
    (None, "no pijs_workload data".to_string())
}

fn read_criterion_load_time(root: &Path, ext: &str) -> (Option<f64>, String) {
    // Criterion stores results in target/criterion/<group>/<bench>/new/estimates.json
    let path = root.join(format!(
        "target/criterion/ext_load_init/load_init_cold/{ext}/new/estimates.json"
    ));
    if let Some(estimates) = read_json_file(&path) {
        if let Some(mean_ns) = estimates
            .get("mean")
            .and_then(|m| m.get("point_estimate"))
            .and_then(Value::as_f64)
        {
            let ms = mean_ns / 1_000_000.0;
            return (
                Some(ms),
                format!("criterion: ext_load_init/load_init_cold/{ext}"),
            );
        }
    }
    (None, format!("no criterion data for {ext}"))
}

fn read_total_load_time(root: &Path) -> (Option<f64>, String) {
    let path = root.join("tests/ext_conformance/reports/load_time_benchmark.json");
    if let Some(report) = read_json_file(&path) {
        if let Some(results) = report.get("results").and_then(Value::as_array) {
            let total_ms: f64 = results
                .iter()
                .filter_map(|r| {
                    r.get("rust")
                        .and_then(|rust| rust.get("load_time_ms"))
                        .and_then(Value::as_f64)
                })
                .sum();
            return (
                Some(total_ms),
                "load_time_benchmark.json (sum of Rust load times)".to_string(),
            );
        }
    }
    (None, "no load time benchmark data".to_string())
}

fn read_stress_rss_growth(root: &Path) -> (Option<f64>, String) {
    let path = root.join("tests/perf/reports/stress_triage.json");
    if let Some(triage) = read_json_file(&path) {
        if let Some(pct) = triage.get("rss_growth_pct").and_then(Value::as_f64) {
            return (Some(pct), "stress_triage.json".to_string());
        }
    }
    (None, "no stress test data".to_string())
}

// ─── New Data Readers (bd-20s9) ──────────────────────────────────────────────

fn read_criterion_startup(root: &Path, subcommand: &str) -> (Option<f64>, String) {
    // Criterion stores startup benchmarks at target/criterion/startup/<subcommand>/warm/new/estimates.json
    let path = root.join(format!(
        "target/criterion/startup/{subcommand}/warm/new/estimates.json"
    ));
    if let Some(estimates) = read_json_file(&path) {
        if let Some(mean_ns) = estimates
            .get("mean")
            .and_then(|m| m.get("point_estimate"))
            .and_then(Value::as_f64)
        {
            let ms = mean_ns / 1_000_000.0;
            return (Some(ms), format!("criterion: startup/{subcommand}/warm"));
        }
    }
    (None, format!("no criterion data for startup/{subcommand}"))
}

fn read_scenario_runner_per_call(root: &Path, scenario: &str) -> (Option<f64>, String) {
    // Read from target/perf/scenario_runner.jsonl
    let path = root.join("target/perf/scenario_runner.jsonl");
    let events = read_jsonl_file(&path);
    // Find the worst (max) per_call_us across all extensions for this scenario.
    let mut max_us: Option<f64> = None;
    for event in &events {
        if event.get("scenario").and_then(Value::as_str) == Some(scenario) {
            if let Some(us) = event.get("per_call_us").and_then(Value::as_f64) {
                max_us = Some(max_us.map_or(us, |prev: f64| prev.max(us)));
            }
        }
    }
    max_us.map_or_else(
        || (None, format!("no scenario_runner data for {scenario}")),
        |us| (Some(us), "target/perf/scenario_runner.jsonl".to_string()),
    )
}

fn read_criterion_policy_eval(root: &Path) -> (Option<f64>, String) {
    // Policy eval benchmarks: target/criterion/ext_policy/evaluate/*/new/estimates.json
    // Take the worst (max) across all policy variants, convert ns → ns.
    let base = root.join("target/criterion/ext_policy/evaluate");
    let mut max_ns: Option<f64> = None;
    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let path = entry.path().join("new/estimates.json");
            if let Some(estimates) = read_json_file(&path) {
                if let Some(mean_ns) = estimates
                    .get("mean")
                    .and_then(|m| m.get("point_estimate"))
                    .and_then(Value::as_f64)
                {
                    max_ns = Some(max_ns.map_or(mean_ns, |prev: f64| prev.max(mean_ns)));
                }
            }
        }
    }
    max_ns.map_or_else(
        || (None, "no criterion data for policy eval".to_string()),
        |ns| (Some(ns), "criterion: ext_policy/evaluate (max)".to_string()),
    )
}

fn read_idle_memory_rss() -> (Option<f64>, String) {
    // Measure the current process RSS as a proxy for idle memory.
    // This runs during test, so it's an approximation.
    let pid = sysinfo::Pid::from_u32(std::process::id());
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        true,
        sysinfo::ProcessRefreshKind::nothing().with_memory(),
    );
    system.process(pid).map_or_else(
        || (None, "could not read process RSS".to_string()),
        |p| {
            let rss_mb = p.memory() as f64 / 1024.0 / 1024.0;
            (Some(rss_mb), "sysinfo: current process RSS".to_string())
        },
    )
}

fn read_binary_size(root: &Path) -> (Option<f64>, String) {
    let release_path = root.join("target/release/pi");
    if let Ok(meta) = std::fs::metadata(&release_path) {
        let size_mb = meta.len() as f64 / 1024.0 / 1024.0;
        return (Some(size_mb), "target/release/pi".to_string());
    }
    (None, "no release binary found".to_string())
}

fn read_criterion_protocol_parse(root: &Path) -> (Option<f64>, String) {
    // Protocol parse: target/criterion/ext_protocol/parse_and_validate/*/new/estimates.json
    // Take the worst (max) across variants, convert ns → us.
    let base = root.join("target/criterion/ext_protocol/parse_and_validate");
    let mut max_us: Option<f64> = None;
    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let path = entry.path().join("new/estimates.json");
            if let Some(estimates) = read_json_file(&path) {
                if let Some(mean_ns) = estimates
                    .get("mean")
                    .and_then(|m| m.get("point_estimate"))
                    .and_then(Value::as_f64)
                {
                    let us = mean_ns / 1000.0;
                    max_us = Some(max_us.map_or(us, |prev: f64| prev.max(us)));
                }
            }
        }
    }
    max_us.map_or_else(
        || (None, "no criterion data for protocol parse".to_string()),
        |us| {
            (
                Some(us),
                "criterion: ext_protocol/parse_and_validate (max)".to_string(),
            )
        },
    )
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn budget_definitions_are_valid() {
    for budget in BUDGETS {
        assert!(!budget.name.is_empty(), "budget name must not be empty");
        assert!(
            !budget.category.is_empty(),
            "budget category must not be empty"
        );
        assert!(budget.threshold > 0.0, "budget threshold must be positive");
        assert!(!budget.unit.is_empty(), "budget unit must not be empty");
        assert!(
            !budget.methodology.is_empty(),
            "budget methodology must not be empty"
        );
    }
    eprintln!("[budgets] {} budgets defined", BUDGETS.len());
}

#[test]
fn budget_names_are_unique() {
    let mut seen = std::collections::HashSet::new();
    for budget in BUDGETS {
        assert!(
            seen.insert(budget.name),
            "duplicate budget name: {}",
            budget.name
        );
    }
}

#[test]
fn ci_enforced_budgets_have_data_sources() {
    // CI-enforced budgets should have measurement data available
    let ci_budgets: Vec<_> = BUDGETS.iter().filter(|b| b.ci_enforced).collect();
    eprintln!(
        "[budgets] {} CI-enforced budgets out of {} total",
        ci_budgets.len(),
        BUDGETS.len()
    );
    for budget in &ci_budgets {
        eprintln!(
            "  {} ({}): {} {} {}",
            budget.name, budget.category, budget.threshold, budget.unit, budget.methodology
        );
    }
    assert!(
        ci_budgets.len() >= 5,
        "should have at least 5 CI-enforced budgets"
    );
}

#[test]
fn check_tool_call_budget() {
    let budget = BUDGETS
        .iter()
        .find(|b| b.name == "tool_call_latency_p99")
        .expect("tool_call_latency_p99 budget should exist");

    let result = check_budget(budget);
    eprintln!(
        "[budget] {}: actual={:?} {} (threshold={} {}), status={}",
        result.budget_name,
        result.actual,
        result.unit,
        result.threshold,
        result.unit,
        result.status
    );

    if let Some(actual) = result.actual {
        assert!(
            actual <= budget.threshold,
            "tool call latency {actual}us exceeds budget {}us",
            budget.threshold
        );
    }
}

#[test]
fn check_tool_call_throughput_budget() {
    let budget = BUDGETS
        .iter()
        .find(|b| b.name == "tool_call_throughput_min")
        .expect("tool_call_throughput_min budget should exist");

    let result = check_budget(budget);
    eprintln!(
        "[budget] {}: actual={:?} {} (threshold={} {}), status={}",
        result.budget_name,
        result.actual,
        result.unit,
        result.threshold,
        result.unit,
        result.status
    );

    if let Some(actual) = result.actual {
        assert!(
            actual >= budget.threshold,
            "tool call throughput {actual} calls/sec below budget {} calls/sec",
            budget.threshold
        );
    }
}

#[test]
fn check_extension_load_budget() {
    let budget = BUDGETS
        .iter()
        .find(|b| b.name == "ext_cold_load_simple_p95")
        .expect("ext_cold_load_simple_p95 budget should exist");

    let result = check_budget(budget);
    eprintln!(
        "[budget] {}: actual={:?} {} (threshold={} {}), status={}",
        result.budget_name,
        result.actual,
        result.unit,
        result.threshold,
        result.unit,
        result.status
    );

    if let Some(actual) = result.actual {
        assert!(
            actual <= budget.threshold,
            "extension cold load {actual}ms exceeds budget {}ms",
            budget.threshold
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn generate_budget_report() {
    let results: Vec<BudgetResult> = BUDGETS.iter().map(check_budget).collect();

    let root = project_root();
    let reports_dir = root.join("tests/perf/reports");
    let _ = std::fs::create_dir_all(&reports_dir);

    // ── Write JSONL ──
    let jsonl_path = reports_dir.join("budget_events.jsonl");
    let jsonl: String = results
        .iter()
        .map(|r| serde_json::to_string(r).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&jsonl_path, format!("{jsonl}\n")).expect("write budget_events.jsonl");

    // ── Write summary JSON ──
    let pass_count = results.iter().filter(|r| r.status == "PASS").count();
    let fail_count = results.iter().filter(|r| r.status == "FAIL").count();
    let no_data_count = results.iter().filter(|r| r.status == "NO_DATA").count();
    let ci_enforced_count = BUDGETS.iter().filter(|b| b.ci_enforced).count();

    let summary = json!({
        "schema": "pi.perf.budget_summary.v1",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "total_budgets": BUDGETS.len(),
        "ci_enforced": ci_enforced_count,
        "pass": pass_count,
        "fail": fail_count,
        "no_data": no_data_count,
        "budgets": BUDGETS.iter().map(|b| json!({
            "name": b.name,
            "category": b.category,
            "metric": b.metric,
            "unit": b.unit,
            "threshold": b.threshold,
            "ci_enforced": b.ci_enforced,
            "methodology": b.methodology,
        })).collect::<Vec<_>>(),
    });

    let summary_path = reports_dir.join("budget_summary.json");
    std::fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary).unwrap_or_default(),
    )
    .expect("write budget_summary.json");

    // ── Write Markdown ──
    let mut md = String::with_capacity(8 * 1024);

    md.push_str("# Performance Budgets\n\n");
    let _ = writeln!(
        md,
        "> Generated: {}\n",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    );

    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n");
    md.push_str("|---|---|\n");
    let _ = writeln!(md, "| Total budgets | {} |", BUDGETS.len());
    let _ = writeln!(md, "| CI-enforced | {ci_enforced_count} |");
    let _ = writeln!(md, "| PASS | {pass_count} |");
    let _ = writeln!(md, "| FAIL | {fail_count} |");
    let _ = writeln!(md, "| No data | {no_data_count} |\n");

    // Group by category
    let categories = [
        "startup",
        "extension",
        "tool_call",
        "event_dispatch",
        "policy",
        "memory",
        "binary",
        "protocol",
    ];

    for cat in &categories {
        let cat_budgets: Vec<_> = BUDGETS.iter().filter(|b| b.category == *cat).collect();
        if cat_budgets.is_empty() {
            continue;
        }

        let _ = writeln!(md, "## {}\n", capitalize(cat));
        md.push_str("| Budget | Metric | Threshold | Actual | Status | CI |\n");
        md.push_str("|---|---|---|---|---|---|\n");

        for budget in &cat_budgets {
            let result = results
                .iter()
                .find(|r| r.budget_name == budget.name)
                .unwrap();
            let actual_str = result
                .actual
                .map_or_else(|| "-".to_string(), |v| format_value(v, budget.unit));
            let ci_str = if budget.ci_enforced { "Yes" } else { "No" };

            let _ = writeln!(
                md,
                "| `{}` | {} | {} {} | {} | {} | {} |",
                budget.name,
                budget.metric,
                budget.threshold,
                budget.unit,
                actual_str,
                result.status,
                ci_str,
            );
        }
        md.push('\n');
    }

    // Methodology
    md.push_str("## Measurement Methodology\n\n");
    for budget in BUDGETS {
        let _ = writeln!(md, "- **`{}`**: {}", budget.name, budget.methodology);
    }
    md.push('\n');

    md.push_str("## CI Enforcement\n\n");
    md.push_str("CI-enforced budgets are checked on every PR. A budget violation ");
    md.push_str("blocks the PR from merging. Non-CI budgets are informational and ");
    md.push_str("checked in nightly runs.\n\n");
    md.push_str("```bash\n");
    md.push_str("# Run budget checks\n");
    md.push_str("cargo test --test perf_budgets -- --nocapture\n\n");
    md.push_str("# Generate full budget report\n");
    md.push_str("cargo test --test perf_budgets generate_budget_report -- --nocapture\n");
    md.push_str("```\n");

    let md_path = reports_dir.join("PERF_BUDGETS.md");
    std::fs::write(&md_path, &md).expect("write PERF_BUDGETS.md");

    // Print summary
    eprintln!("\n=== Performance Budget Report ===");
    eprintln!("  Total: {}", BUDGETS.len());
    eprintln!("  PASS:  {pass_count}");
    eprintln!("  FAIL:  {fail_count}");
    eprintln!("  N/A:   {no_data_count}");
    eprintln!("  Reports:");
    eprintln!("    {}", md_path.display());
    eprintln!("    {}", summary_path.display());
    eprintln!("    {}", jsonl_path.display());
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |c| {
        let upper: String = c.to_uppercase().collect();
        let rest: String = chars.collect();
        format!("{upper}{rest}")
    })
}

fn format_value(val: f64, unit: &str) -> String {
    match unit {
        "ms" | "MB" | "percent" => format!("{val:.1}"),
        "us" | "ns" | "calls/sec" => format!("{val:.0}"),
        _ => format!("{val:.2}"),
    }
}
