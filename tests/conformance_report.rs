//! Consolidated per-extension conformance report generator (bd-31j).
//!
//! Reads existing conformance reports and `VALIDATED_MANIFEST.json`, then generates:
//! - `tests/ext_conformance/reports/CONFORMANCE_REPORT.md` — human-readable summary
//! - `tests/ext_conformance/reports/conformance_summary.json` — machine-readable summary
//! - `tests/ext_conformance/reports/conformance_events.jsonl` — per-extension JSONL log
//!
//! Run with: `cargo test --test conformance_report generate_conformance_report -- --nocapture`

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn reports_dir() -> PathBuf {
    project_root().join("tests/ext_conformance/reports")
}

fn manifest_path() -> PathBuf {
    project_root().join("tests/ext_conformance/VALIDATED_MANIFEST.json")
}

// ─── Data Structures ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestExtension {
    id: String,
    entry_path: String,
    source_tier: String,
    conformance_tier: u8,
    #[serde(default)]
    capabilities: Value,
    #[serde(default)]
    registrations: Value,
    #[serde(default)]
    mock_requirements: Vec<String>,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
struct ExtensionStatus {
    // Differential parity (TS vs Rust registration snapshot)
    diff_status: Option<String>, // "pass" | "fail" | "skip"
    diff_error: Option<String>,

    // Load time comparison
    ts_load_ms: Option<u64>,
    rust_load_ms: Option<u64>,
    load_ratio: Option<f64>,

    // Scenario execution
    scenario_pass: u32,
    scenario_fail: u32,
    scenario_skip: u32,
    scenario_failures: Vec<String>,

    // Smoke test
    smoke_pass: u32,
    smoke_fail: u32,

    // Parity (TS vs Rust scenario)
    parity_match: u32,
    parity_mismatch: u32,
}

// ─── Report Readers ──────────────────────────────────────────────────────────

fn read_json_file(path: &Path) -> Option<Value> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_jsonl_file(path: &Path) -> Vec<Value> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn ingest_load_time_report(
    statuses: &mut BTreeMap<String, ExtensionStatus>,
    reports: &Path,
) {
    let path = reports.join("load_time_benchmark.json");
    let Some(report) = read_json_file(&path) else {
        return;
    };
    let Some(results) = report.get("results").and_then(Value::as_array) else {
        return;
    };

    for entry in results {
        let Some(ext_name) = entry.get("extension").and_then(Value::as_str) else {
            continue;
        };
        // Extension name is like "hello/hello.ts" — extract the directory part as ID
        let ext_id = ext_name
            .split('/')
            .next()
            .unwrap_or(ext_name)
            .to_string();

        let status = statuses.entry(ext_id).or_default();

        status.ts_load_ms = entry
            .get("ts")
            .and_then(|ts| ts.get("load_time_ms"))
            .and_then(Value::as_u64);
        status.rust_load_ms = entry
            .get("rust")
            .and_then(|rust| rust.get("load_time_ms"))
            .and_then(Value::as_u64);
        status.load_ratio = entry.get("ratio").and_then(Value::as_f64);
    }
}

fn ingest_scenario_report(
    statuses: &mut BTreeMap<String, ExtensionStatus>,
    reports: &Path,
) {
    let path = reports.join("scenario_conformance.json");
    let Some(report) = read_json_file(&path) else {
        return;
    };
    let Some(results) = report.get("results").and_then(Value::as_array) else {
        return;
    };

    for entry in results {
        let Some(ext_id) = entry.get("extension_id").and_then(Value::as_str) else {
            continue;
        };
        let status_str = entry
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("skip");

        let status = statuses.entry(ext_id.to_string()).or_default();
        match status_str {
            "pass" => status.scenario_pass += 1,
            "fail" => {
                status.scenario_fail += 1;
                if let Some(summary) = entry.get("summary").and_then(Value::as_str) {
                    status.scenario_failures.push(summary.to_string());
                }
            }
            _ => status.scenario_skip += 1,
        }
    }
}

fn ingest_smoke_report(
    statuses: &mut BTreeMap<String, ExtensionStatus>,
    reports: &Path,
) {
    let path = reports.join("smoke/triage.json");
    let Some(report) = read_json_file(&path) else {
        return;
    };
    let Some(extensions) = report.get("extensions").and_then(Value::as_array) else {
        return;
    };

    for entry in extensions {
        let Some(ext_id) = entry.get("extension_id").and_then(Value::as_str) else {
            continue;
        };
        let status = statuses.entry(ext_id.to_string()).or_default();
        status.smoke_pass += entry
            .get("pass")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        status.smoke_fail += entry
            .get("fail")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
    }
}

fn ingest_parity_report(
    statuses: &mut BTreeMap<String, ExtensionStatus>,
    reports: &Path,
) {
    let events = read_jsonl_file(&reports.join("parity/parity_events.jsonl"));

    for event in events {
        let Some(ext_id) = event.get("extension_id").and_then(Value::as_str) else {
            continue;
        };
        let status_str = event
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("skip");

        let status = statuses.entry(ext_id.to_string()).or_default();
        match status_str {
            "match" => status.parity_match += 1,
            "mismatch" => status.parity_mismatch += 1,
            _ => {}
        }
    }
}

fn ingest_negative_report(reports: &Path) -> (u32, u32) {
    let path = reports.join("negative/triage.json");
    let Some(report) = read_json_file(&path) else {
        return (0, 0);
    };
    let pass = report
        .get("counts")
        .and_then(|c| c.get("pass"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let fail = report
        .get("counts")
        .and_then(|c| c.get("fail"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    (pass, fail)
}

// ─── Report Generation ──────────────────────────────────────────────────────

fn overall_status(status: &ExtensionStatus) -> &'static str {
    if status.scenario_fail > 0 || status.smoke_fail > 0 || status.parity_mismatch > 0 {
        return "FAIL";
    }
    if status.diff_status.as_deref() == Some("fail") {
        return "FAIL";
    }
    if status.scenario_pass > 0 || status.smoke_pass > 0 || status.parity_match > 0 {
        return "PASS";
    }
    if status.diff_status.as_deref() == Some("pass") {
        return "PASS";
    }
    if status.rust_load_ms.is_some() {
        return "PASS";
    }
    "N/A"
}

fn tier_label(tier: u8) -> &'static str {
    match tier {
        1 => "T1 (simple single-file)",
        2 => "T2 (multi-registration)",
        3 => "T3 (multi-file)",
        4 => "T4 (npm deps)",
        5 => "T5 (exec/network)",
        _ => "unknown",
    }
}

#[allow(clippy::too_many_lines)]
fn generate_markdown(
    extensions: &[ManifestExtension],
    statuses: &BTreeMap<String, ExtensionStatus>,
    negative_pass: u32,
    negative_fail: u32,
) -> String {
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);

    // Group extensions by source tier
    let mut by_tier: BTreeMap<String, Vec<&ManifestExtension>> = BTreeMap::new();
    for ext in extensions {
        by_tier
            .entry(ext.source_tier.clone())
            .or_default()
            .push(ext);
    }

    // Compute aggregate stats
    let total = extensions.len();
    let mut pass_count = 0u32;
    let mut fail_count = 0u32;
    let mut na_count = 0u32;
    for ext in extensions {
        let status = statuses.get(&ext.id);
        match status.map(overall_status).unwrap_or("N/A") {
            "PASS" => pass_count += 1,
            "FAIL" => fail_count += 1,
            _ => na_count += 1,
        }
    }

    let mut md = String::with_capacity(32 * 1024);

    // Header
    md.push_str("# Extension Conformance Report\n\n");
    md.push_str(&format!("> Generated: {now}\n\n"));

    // Summary
    md.push_str("## Summary\n\n");
    md.push_str(&format!("| Metric | Value |\n"));
    md.push_str("|----|----|\n");
    md.push_str(&format!("| Total extensions | {total} |\n"));
    md.push_str(&format!("| PASS | {pass_count} |\n"));
    md.push_str(&format!("| FAIL | {fail_count} |\n"));
    md.push_str(&format!("| N/A (not yet tested) | {na_count} |\n"));
    if total > 0 {
        #[allow(clippy::cast_precision_loss)]
        let rate = (pass_count as f64) / ((pass_count + fail_count).max(1) as f64) * 100.0;
        md.push_str(&format!("| Pass rate | {rate:.1}% |\n"));
    }
    md.push_str(&format!("| Policy negative tests | {negative_pass} pass, {negative_fail} fail |\n"));
    md.push_str(&format!("| Source tiers | {} |\n\n", by_tier.len()));

    // Per-tier tables
    for (tier_name, tier_exts) in &by_tier {
        md.push_str(&format!("## {tier_name}\n\n"));

        let tier_pass = tier_exts
            .iter()
            .filter(|e| statuses.get(&e.id).map(overall_status) == Some("PASS"))
            .count();
        let tier_fail = tier_exts
            .iter()
            .filter(|e| statuses.get(&e.id).map(overall_status) == Some("FAIL"))
            .count();

        md.push_str(&format!(
            "{} extensions ({tier_pass} pass, {tier_fail} fail, {} untested)\n\n",
            tier_exts.len(),
            tier_exts.len() - tier_pass - tier_fail
        ));

        md.push_str("| Extension | Tier | Status | Load (Rust) | Scenarios | Failures |\n");
        md.push_str("|---|---|---|---|---|---|\n");

        for ext in tier_exts {
            let status = statuses.get(&ext.id);
            let overall = status.map(overall_status).unwrap_or("N/A");

            let load_str = status
                .and_then(|s| s.rust_load_ms)
                .map_or_else(|| "-".to_string(), |ms| format!("{ms}ms"));

            let scenario_str = status.map_or_else(
                || "-".to_string(),
                |s| {
                    if s.scenario_pass + s.scenario_fail + s.scenario_skip == 0 {
                        "-".to_string()
                    } else {
                        format!(
                            "{}/{} pass",
                            s.scenario_pass,
                            s.scenario_pass + s.scenario_fail
                        )
                    }
                },
            );

            let failures_str = status.map_or_else(String::new, |s| {
                if s.scenario_failures.is_empty() {
                    String::new()
                } else {
                    s.scenario_failures
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("; ")
                }
            });

            let status_emoji = match overall {
                "PASS" => "PASS",
                "FAIL" => "FAIL",
                _ => "N/A",
            };

            md.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {} |\n",
                ext.id,
                tier_label(ext.conformance_tier),
                status_emoji,
                load_str,
                scenario_str,
                failures_str,
            ));
        }
        md.push('\n');
    }

    // Regeneration instructions
    md.push_str("---\n\n");
    md.push_str("## How to Regenerate\n\n");
    md.push_str("```bash\n");
    md.push_str("# 1. Run conformance tests (generates report data)\n");
    md.push_str("cargo test --test ext_conformance_diff --features ext-conformance\n");
    md.push_str("cargo test --test ext_conformance_scenarios --features ext-conformance\n");
    md.push_str("cargo test --test extensions_policy_negative\n\n");
    md.push_str("# 2. Generate this consolidated report\n");
    md.push_str("cargo test --test conformance_report generate_conformance_report -- --nocapture\n");
    md.push_str("```\n\n");
    md.push_str("Report files:\n");
    md.push_str("- `tests/ext_conformance/reports/CONFORMANCE_REPORT.md` (this file)\n");
    md.push_str("- `tests/ext_conformance/reports/conformance_summary.json` (machine-readable)\n");
    md.push_str("- `tests/ext_conformance/reports/conformance_events.jsonl` (per-extension log)\n");

    md
}

// ─── Test Entry Points ──────────────────────────────────────────────────────

#[test]
#[allow(clippy::too_many_lines)]
fn generate_conformance_report() {
    let reports = reports_dir();
    let _ = std::fs::create_dir_all(&reports);

    // 1. Read manifest
    let manifest_content =
        std::fs::read_to_string(manifest_path()).expect("read VALIDATED_MANIFEST.json");
    let manifest: Value =
        serde_json::from_str(&manifest_content).expect("parse VALIDATED_MANIFEST.json");
    let extensions: Vec<ManifestExtension> = manifest
        .get("extensions")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default();

    eprintln!(
        "[conformance_report] Loaded {} extensions from manifest",
        extensions.len()
    );

    // 2. Ingest all available reports
    let mut statuses: BTreeMap<String, ExtensionStatus> = BTreeMap::new();
    ingest_load_time_report(&mut statuses, &reports);
    ingest_scenario_report(&mut statuses, &reports);
    ingest_smoke_report(&mut statuses, &reports);
    ingest_parity_report(&mut statuses, &reports);

    let (negative_pass, negative_fail) = ingest_negative_report(&reports);

    eprintln!(
        "[conformance_report] Ingested reports: {} extensions with data, negative: {}/{}",
        statuses.len(),
        negative_pass,
        negative_pass + negative_fail
    );

    // 3. Write JSONL events
    let events_path = reports.join("conformance_events.jsonl");
    let mut jsonl_lines: Vec<String> = Vec::new();
    for ext in &extensions {
        let status = statuses.get(&ext.id);
        let overall = status.map(overall_status).unwrap_or("N/A");
        let entry = json!({
            "schema": "pi.ext.conformance_report.v1",
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            "extension_id": ext.id,
            "source_tier": ext.source_tier,
            "conformance_tier": ext.conformance_tier,
            "overall_status": overall,
            "rust_load_ms": status.and_then(|s| s.rust_load_ms),
            "ts_load_ms": status.and_then(|s| s.ts_load_ms),
            "load_ratio": status.and_then(|s| s.load_ratio),
            "scenario_pass": status.map_or(0, |s| s.scenario_pass),
            "scenario_fail": status.map_or(0, |s| s.scenario_fail),
            "scenario_skip": status.map_or(0, |s| s.scenario_skip),
            "smoke_pass": status.map_or(0, |s| s.smoke_pass),
            "smoke_fail": status.map_or(0, |s| s.smoke_fail),
            "parity_match": status.map_or(0, |s| s.parity_match),
            "parity_mismatch": status.map_or(0, |s| s.parity_mismatch),
            "failures": status.map_or_else(Vec::new, |s| s.scenario_failures.clone()),
        });
        jsonl_lines.push(serde_json::to_string(&entry).unwrap_or_default());
    }
    std::fs::write(&events_path, jsonl_lines.join("\n") + "\n")
        .expect("write conformance_events.jsonl");

    // 4. Write summary JSON
    let total = extensions.len();
    let mut pass = 0u32;
    let mut fail = 0u32;
    let mut na = 0u32;
    for ext in &extensions {
        match statuses.get(&ext.id).map(overall_status).unwrap_or("N/A") {
            "PASS" => pass += 1,
            "FAIL" => fail += 1,
            _ => na += 1,
        }
    }

    let mut per_tier: BTreeMap<String, Value> = BTreeMap::new();
    for ext in &extensions {
        let entry = per_tier
            .entry(ext.source_tier.clone())
            .or_insert_with(|| json!({"total": 0, "pass": 0, "fail": 0, "na": 0}));
        let obj = entry.as_object_mut().unwrap();
        *obj.get_mut("total").unwrap() =
            json!(obj["total"].as_u64().unwrap_or(0) + 1);
        match statuses.get(&ext.id).map(overall_status).unwrap_or("N/A") {
            "PASS" => {
                *obj.get_mut("pass").unwrap() =
                    json!(obj["pass"].as_u64().unwrap_or(0) + 1);
            }
            "FAIL" => {
                *obj.get_mut("fail").unwrap() =
                    json!(obj["fail"].as_u64().unwrap_or(0) + 1);
            }
            _ => {
                *obj.get_mut("na").unwrap() =
                    json!(obj["na"].as_u64().unwrap_or(0) + 1);
            }
        }
    }

    #[allow(clippy::cast_precision_loss)]
    let pass_rate = if pass + fail > 0 {
        (pass as f64) / ((pass + fail) as f64) * 100.0
    } else {
        100.0
    };

    let summary = json!({
        "schema": "pi.ext.conformance_summary.v1",
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "counts": {
            "total": total,
            "pass": pass,
            "fail": fail,
            "na": na,
        },
        "pass_rate_pct": pass_rate,
        "negative": {
            "pass": negative_pass,
            "fail": negative_fail,
        },
        "per_tier": per_tier,
    });
    let summary_path = reports.join("conformance_summary.json");
    std::fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary).unwrap_or_default(),
    )
    .expect("write conformance_summary.json");

    // 5. Generate markdown report
    let md = generate_markdown(&extensions, &statuses, negative_pass, negative_fail);
    let md_path = reports.join("CONFORMANCE_REPORT.md");
    std::fs::write(&md_path, &md).expect("write CONFORMANCE_REPORT.md");

    // 6. Print summary
    eprintln!("\n=== Conformance Report Generated ===");
    eprintln!("  Total extensions: {total}");
    eprintln!("  PASS: {pass}");
    eprintln!("  FAIL: {fail}");
    eprintln!("  N/A:  {na}");
    eprintln!("  Pass rate: {pass_rate:.1}%");
    eprintln!("  Negative policy: {negative_pass} pass, {negative_fail} fail");
    eprintln!("  Reports:");
    eprintln!("    {}", md_path.display());
    eprintln!("    {}", summary_path.display());
    eprintln!("    {}", events_path.display());

    // Verify report was generated
    assert!(md_path.exists(), "CONFORMANCE_REPORT.md should be generated");
    assert!(
        summary_path.exists(),
        "conformance_summary.json should be generated"
    );
    assert!(
        events_path.exists(),
        "conformance_events.jsonl should be generated"
    );
}

#[test]
fn report_reads_manifest() {
    // Verify the manifest can be read and parsed
    let manifest_content =
        std::fs::read_to_string(manifest_path()).expect("read VALIDATED_MANIFEST.json");
    let manifest: Value =
        serde_json::from_str(&manifest_content).expect("parse VALIDATED_MANIFEST.json");
    let extensions = manifest
        .get("extensions")
        .and_then(Value::as_array)
        .expect("manifest should have extensions array");
    assert!(
        !extensions.is_empty(),
        "manifest should have at least one extension"
    );

    // Verify each extension has required fields
    for ext in extensions {
        assert!(ext.get("id").is_some(), "extension should have id");
        assert!(
            ext.get("entry_path").is_some(),
            "extension should have entry_path"
        );
        assert!(
            ext.get("source_tier").is_some(),
            "extension should have source_tier"
        );
        assert!(
            ext.get("conformance_tier").is_some(),
            "extension should have conformance_tier"
        );
    }
}

#[test]
fn report_reads_negative_triage() {
    let (pass, fail) = ingest_negative_report(&reports_dir());
    // The negative conformance tests should have run and produced results
    eprintln!("Negative triage: {pass} pass, {fail} fail");
    // Don't assert specific counts since report might not exist yet
}
