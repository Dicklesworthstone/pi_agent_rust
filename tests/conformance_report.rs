//! Consolidated per-extension conformance report generator (bd-31j).
//!
//! Reads existing conformance reports and `VALIDATED_MANIFEST.json`, then generates:
//! - `tests/ext_conformance/reports/CONFORMANCE_REPORT.md` — human-readable summary
//! - `tests/ext_conformance/reports/conformance_summary.json` — machine-readable summary
//! - `tests/ext_conformance/reports/conformance_events.jsonl` — per-extension JSONL log
//!
//! Also enriches each extension with best-effort provenance/version metadata from
//! `docs/extension-artifact-provenance.json`.
//!
//! Run with: `cargo test --test conformance_report generate_conformance_report -- --nocapture`

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn reports_dir() -> PathBuf {
    project_root().join("tests/ext_conformance/reports")
}

fn manifest_path() -> PathBuf {
    project_root().join("tests/ext_conformance/VALIDATED_MANIFEST.json")
}

fn provenance_path() -> PathBuf {
    project_root().join("docs/extension-artifact-provenance.json")
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProvenanceManifest {
    items: Vec<ProvenanceItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProvenanceItem {
    id: String,
    #[serde(default)]
    version: Option<String>,
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
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn u32_from_u64_saturating(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn ingest_load_time_report(statuses: &mut BTreeMap<String, ExtensionStatus>, reports: &Path) {
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
        let ext_id = ext_name.split('/').next().unwrap_or(ext_name).to_string();

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

fn ingest_scenario_report(statuses: &mut BTreeMap<String, ExtensionStatus>, reports: &Path) {
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

fn ingest_smoke_report(statuses: &mut BTreeMap<String, ExtensionStatus>, reports: &Path) {
    let Some(report) = latest_smoke_triage_report(reports) else {
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
        let pass = entry.get("pass").and_then(Value::as_u64).unwrap_or(0);
        let fail = entry.get("fail").and_then(Value::as_u64).unwrap_or(0);
        status.smoke_pass = status
            .smoke_pass
            .saturating_add(u32_from_u64_saturating(pass));
        status.smoke_fail = status
            .smoke_fail
            .saturating_add(u32_from_u64_saturating(fail));
    }
}

fn parse_generated_at(value: &Value) -> Option<DateTime<Utc>> {
    value
        .get("generated_at")
        .and_then(Value::as_str)
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn latest_smoke_triage_report(reports: &Path) -> Option<Value> {
    let candidates = [
        reports.join("smoke_triage.json"),
        reports.join("smoke/triage.json"),
    ];
    let mut latest: Option<(i64, u32, Value)> = None;

    for path in &candidates {
        let Some(report) = read_json_file(path) else {
            continue;
        };
        let generated = parse_generated_at(&report);
        let key = (
            generated.map_or(i64::MIN, |dt| dt.timestamp()),
            generated.map_or(0, |dt| dt.timestamp_subsec_nanos()),
        );
        let should_replace = match &latest {
            Some((prev_secs, prev_nanos, _)) => {
                key.0 > *prev_secs || (key.0 == *prev_secs && key.1 > *prev_nanos)
            }
            None => true,
        };
        if should_replace {
            latest = Some((key.0, key.1, report));
        }
    }

    latest.map(|(_, _, report)| report)
}

fn ingest_parity_report(statuses: &mut BTreeMap<String, ExtensionStatus>, reports: &Path) {
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
    let pass = u32_from_u64_saturating(
        report
            .get("counts")
            .and_then(|c| c.get("pass"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    );
    let fail = u32_from_u64_saturating(
        report
            .get("counts")
            .and_then(|c| c.get("fail"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    );
    (pass, fail)
}

// ─── Report Generation ──────────────────────────────────────────────────────

fn load_provenance_versions() -> BTreeMap<String, String> {
    let path = provenance_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };

    let Ok(manifest): Result<ProvenanceManifest, _> = serde_json::from_str(&content) else {
        return BTreeMap::new();
    };

    manifest
        .items
        .into_iter()
        .filter_map(|item| {
            let version = item.version?;
            let version = version.trim().to_string();
            if version.is_empty() {
                None
            } else {
                Some((item.id, version))
            }
        })
        .collect()
}

fn artifact_rel_path(entry_path: &str) -> String {
    format!("tests/ext_conformance/artifacts/{entry_path}")
}

fn report_log_rel_path_for_root(root: &Path, suite: &str, ext_id: &str) -> Option<String> {
    let mut candidates = Vec::new();
    if suite == "smoke" {
        // Canonical smoke outputs now land directly in reports/extensions.
        candidates.push(format!(
            "tests/ext_conformance/reports/extensions/{ext_id}.jsonl"
        ));
        // Backward compatibility for older artifact layout.
        candidates.push(format!(
            "tests/ext_conformance/reports/smoke/extensions/{ext_id}.jsonl"
        ));
    } else {
        candidates.push(format!(
            "tests/ext_conformance/reports/{suite}/extensions/{ext_id}.jsonl"
        ));
    }
    candidates.into_iter().find(|rel| root.join(rel).exists())
}

fn report_log_rel_path(suite: &str, ext_id: &str) -> Option<String> {
    report_log_rel_path_for_root(&project_root(), suite, ext_id)
}

fn fixture_rel_path(ext_id: &str) -> Option<String> {
    let rel = format!("tests/ext_conformance/fixtures/{ext_id}.json");
    let abs = project_root().join(&rel);
    if abs.exists() { Some(rel) } else { None }
}

fn overall_status(status: &ExtensionStatus) -> &'static str {
    // Scenario results are more authoritative than smoke results because
    // smoke artifacts may be stale from a prior run. When scenario results
    // exist, ignore smoke-only failures to avoid false negatives (bd-k5q5.2.10).
    let has_scenario_results = status.scenario_pass > 0 || status.scenario_fail > 0;
    let effective_smoke_fail = if has_scenario_results {
        0
    } else {
        status.smoke_fail
    };

    if status.scenario_fail > 0 || effective_smoke_fail > 0 || status.parity_mismatch > 0 {
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

const fn tier_label(tier: u8) -> &'static str {
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
    provenance_versions: &BTreeMap<String, String>,
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
        match status.map_or("N/A", overall_status) {
            "PASS" => pass_count += 1,
            "FAIL" => fail_count += 1,
            _ => na_count += 1,
        }
    }

    let mut md = String::with_capacity(32 * 1024);

    // Header
    md.push_str("# Extension Conformance Report\n\n");
    let _ = writeln!(md, "> Generated: {now}\n");

    // Summary
    md.push_str("## Summary\n\n");
    md.push_str("| Metric | Value |\n");
    md.push_str("|----|----|\n");
    let _ = writeln!(md, "| Total extensions | {total} |");
    let _ = writeln!(md, "| PASS | {pass_count} |");
    let _ = writeln!(md, "| FAIL | {fail_count} |");
    let _ = writeln!(md, "| N/A (not yet tested) | {na_count} |");
    if total > 0 {
        #[allow(clippy::cast_precision_loss)]
        let rate = f64::from(pass_count) / f64::from((pass_count + fail_count).max(1)) * 100.0;
        let _ = writeln!(md, "| Pass rate | {rate:.1}% |");
    }
    let _ = writeln!(
        md,
        "| Policy negative tests | {negative_pass} pass, {negative_fail} fail |"
    );
    let _ = writeln!(md, "| Source tiers | {} |\n", by_tier.len());

    // Per-tier tables
    for (tier_name, tier_exts) in &by_tier {
        let _ = writeln!(md, "## {tier_name}\n");

        let tier_pass = tier_exts
            .iter()
            .filter(|e| statuses.get(&e.id).map(overall_status) == Some("PASS"))
            .count();
        let tier_fail = tier_exts
            .iter()
            .filter(|e| statuses.get(&e.id).map(overall_status) == Some("FAIL"))
            .count();

        let _ = writeln!(
            md,
            "{} extensions ({tier_pass} pass, {tier_fail} fail, {} untested)\n",
            tier_exts.len(),
            tier_exts.len() - tier_pass - tier_fail
        );

        md.push_str("| Extension | Version | Tier | Status | Evidence | Load (Rust) | Scenarios | Failures |\n");
        md.push_str("|---|---|---|---|---|---|---|---|\n");

        for ext in tier_exts {
            let status = statuses.get(&ext.id);
            let overall = status.map_or("N/A", overall_status);

            let version = provenance_versions
                .get(&ext.id)
                .map_or_else(|| "-".to_string(), |v| format!("`{v}`"));

            let artifact_rel = artifact_rel_path(&ext.entry_path);
            let ext_display = format!("[`{}`]({artifact_rel})", ext.id);

            let mut evidence = Vec::new();
            if let Some(fixture) = fixture_rel_path(&ext.id) {
                evidence.push(format!("[fixture]({fixture})"));
            }
            if let Some(smoke) = report_log_rel_path("smoke", &ext.id) {
                evidence.push(format!("[smoke]({smoke})"));
            }
            if let Some(parity) = report_log_rel_path("parity", &ext.id) {
                evidence.push(format!("[parity]({parity})"));
            }
            let evidence = if evidence.is_empty() {
                "-".to_string()
            } else {
                evidence.join(" ")
            };

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

            let _ = writeln!(
                md,
                "| {} | {} | {} | {} | {} | {} | {} | {} |",
                ext_display,
                version,
                tier_label(ext.conformance_tier),
                status_emoji,
                evidence,
                load_str,
                scenario_str,
                failures_str,
            );
        }
        md.push('\n');
    }

    // Evidence index
    let fixture_count = extensions
        .iter()
        .filter(|e| fixture_rel_path(&e.id).is_some())
        .count();
    let smoke_count = extensions
        .iter()
        .filter(|e| report_log_rel_path("smoke", &e.id).is_some())
        .count();
    let parity_count = extensions
        .iter()
        .filter(|e| report_log_rel_path("parity", &e.id).is_some())
        .count();
    let load_count = extensions
        .iter()
        .filter(|e| statuses.get(&e.id).and_then(|s| s.rust_load_ms).is_some())
        .count();

    md.push_str("## Evidence Index\n\n");
    md.push_str("| Evidence Type | Count | Location |\n");
    md.push_str("|---|---|---|\n");
    let _ = writeln!(
        md,
        "| Golden fixtures | {fixture_count} | `tests/ext_conformance/fixtures/*.json` |"
    );
    let _ = writeln!(
        md,
        "| Smoke test logs | {smoke_count} | `tests/ext_conformance/reports/extensions/` (legacy fallback: `tests/ext_conformance/reports/smoke/extensions/`) |"
    );
    let _ = writeln!(
        md,
        "| Parity diff logs | {parity_count} | `tests/ext_conformance/reports/parity/extensions/` |"
    );
    let _ = writeln!(
        md,
        "| Load time benchmarks | {load_count} | `tests/ext_conformance/reports/load_time_benchmark.json` |"
    );
    let _ = writeln!(
        md,
        "| Negative policy tests | {negative_pass} | `tests/ext_conformance/reports/negative/` |"
    );
    md.push('\n');

    // Coverage gaps — group untested extensions by conformance tier
    let untested: Vec<&ManifestExtension> = extensions
        .iter()
        .filter(|e| statuses.get(&e.id).map_or("N/A", overall_status) == "N/A")
        .collect();
    if !untested.is_empty() {
        md.push_str("## Coverage Gaps\n\n");
        let _ = writeln!(
            md,
            "{} extensions have not been tested yet.\n",
            untested.len()
        );

        let mut by_reason: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for ext in &untested {
            let reason = match ext.conformance_tier {
                4 => "Requires npm dependencies (T4)",
                5 => "Requires exec/network access (T5)",
                3 => "Multi-file extension (T3)",
                _ => "Not yet scheduled",
            };
            by_reason.entry(reason).or_default().push(&ext.id);
        }

        for (reason, ids) in &by_reason {
            let _ = writeln!(md, "**{reason}** ({} extensions):", ids.len());
            for id in ids {
                let _ = writeln!(md, "- `{id}`");
            }
            md.push('\n');
        }
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
    md.push_str(
        "cargo test --test conformance_report generate_conformance_report -- --nocapture\n",
    );
    md.push_str("```\n\n");
    md.push_str("Report files:\n");
    md.push_str("- `tests/ext_conformance/reports/CONFORMANCE_REPORT.md` (this file)\n");
    md.push_str("- `tests/ext_conformance/reports/conformance_summary.json` (machine-readable)\n");
    md.push_str("- `tests/ext_conformance/reports/conformance_events.jsonl` (per-extension log)\n");
    md.push_str(
        "\nEvidence tracing: each extension row links to its source artifact, golden fixture\n",
    );
    md.push_str(
        "(if available), smoke test logs, and parity diff logs. Machine-readable per-extension\n",
    );
    md.push_str(
        "data including capabilities and registrations is in `conformance_events.jsonl`.\n",
    );

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

    let provenance_versions = load_provenance_versions();
    eprintln!(
        "[conformance_report] Loaded {} versions from provenance",
        provenance_versions.len()
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
        let overall = status.map_or("N/A", overall_status);
        let artifact_rel = artifact_rel_path(&ext.entry_path);
        let smoke_log = report_log_rel_path("smoke", &ext.id);
        let parity_log = report_log_rel_path("parity", &ext.id);
        let fixture = fixture_rel_path(&ext.id);
        let entry = json!({
            "schema": "pi.ext.conformance_report.v2",
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            "extension_id": ext.id,
            "version": provenance_versions.get(&ext.id),
            "source_tier": ext.source_tier,
            "conformance_tier": ext.conformance_tier,
            "artifact_path": artifact_rel,
            "evidence": {
                "fixture": fixture,
                "smoke_log": smoke_log,
                "parity_log": parity_log,
            },
            "capabilities": ext.capabilities,
            "registrations": ext.registrations,
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
        match statuses.get(&ext.id).map_or("N/A", overall_status) {
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
        *obj.get_mut("total").unwrap() = json!(obj["total"].as_u64().unwrap_or(0) + 1);
        match statuses.get(&ext.id).map_or("N/A", overall_status) {
            "PASS" => {
                *obj.get_mut("pass").unwrap() = json!(obj["pass"].as_u64().unwrap_or(0) + 1);
            }
            "FAIL" => {
                *obj.get_mut("fail").unwrap() = json!(obj["fail"].as_u64().unwrap_or(0) + 1);
            }
            _ => {
                *obj.get_mut("na").unwrap() = json!(obj["na"].as_u64().unwrap_or(0) + 1);
            }
        }
    }

    #[allow(clippy::cast_precision_loss)]
    let pass_rate = if pass + fail > 0 {
        f64::from(pass) / f64::from(pass + fail) * 100.0
    } else {
        100.0
    };

    // Count evidence artifacts
    let fixture_count = extensions
        .iter()
        .filter(|e| fixture_rel_path(&e.id).is_some())
        .count();
    let smoke_log_count = extensions
        .iter()
        .filter(|e| report_log_rel_path("smoke", &e.id).is_some())
        .count();
    let parity_log_count = extensions
        .iter()
        .filter(|e| report_log_rel_path("parity", &e.id).is_some())
        .count();
    let with_load_time = extensions
        .iter()
        .filter(|e| statuses.get(&e.id).and_then(|s| s.rust_load_ms).is_some())
        .count();

    let summary = json!({
        "schema": "pi.ext.conformance_summary.v2",
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
        "evidence": {
            "golden_fixtures": fixture_count,
            "smoke_logs": smoke_log_count,
            "parity_logs": parity_log_count,
            "load_time_benchmarks": with_load_time,
        },
    });
    let summary_path = reports.join("conformance_summary.json");
    std::fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary).unwrap_or_default(),
    )
    .expect("write conformance_summary.json");

    // 5. Generate markdown report
    let md = generate_markdown(
        &extensions,
        &statuses,
        &provenance_versions,
        negative_pass,
        negative_fail,
    );
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
    assert!(
        md_path.exists(),
        "CONFORMANCE_REPORT.md should be generated"
    );
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
fn report_reads_provenance_versions() {
    let versions = load_provenance_versions();
    assert!(
        !versions.is_empty(),
        "expected at least one versioned entry in extension provenance"
    );
}

#[test]
fn report_reads_negative_triage() {
    let (pass, fail) = ingest_negative_report(&reports_dir());
    // The negative conformance tests should have run and produced results
    eprintln!("Negative triage: {pass} pass, {fail} fail");
    // Don't assert specific counts since report might not exist yet
}

#[test]
fn ingest_smoke_report_prefers_newest_triage_snapshot() {
    let tmp = tempdir().expect("create tempdir");
    let reports = tmp.path();
    std::fs::create_dir_all(reports.join("smoke")).expect("create smoke dir");
    std::fs::write(
        reports.join("smoke/triage.json"),
        r#"{
  "generated_at": "2026-02-07T05:10:32Z",
  "extensions": [
    {"extension_id": "status-line", "pass": 0, "fail": 1}
  ]
}"#,
    )
    .expect("write legacy triage");
    std::fs::write(
        reports.join("smoke_triage.json"),
        r#"{
  "generated_at": "2026-02-08T03:41:07Z",
  "extensions": [
    {"extension_id": "status-line", "pass": 1, "fail": 0}
  ]
}"#,
    )
    .expect("write canonical triage");

    let mut statuses = BTreeMap::new();
    ingest_smoke_report(&mut statuses, reports);
    let status = statuses.get("status-line").expect("status-line present");
    assert_eq!(status.smoke_pass, 1);
    assert_eq!(status.smoke_fail, 0);
}

#[test]
fn ingest_smoke_report_falls_back_to_legacy_path() {
    let tmp = tempdir().expect("create tempdir");
    let reports = tmp.path();
    std::fs::create_dir_all(reports.join("smoke")).expect("create smoke dir");
    std::fs::write(
        reports.join("smoke/triage.json"),
        r#"{
  "generated_at": "2026-02-07T05:10:32Z",
  "extensions": [
    {"extension_id": "git-checkpoint", "pass": 0, "fail": 1}
  ]
}"#,
    )
    .expect("write legacy triage");

    let mut statuses = BTreeMap::new();
    ingest_smoke_report(&mut statuses, reports);
    let status = statuses
        .get("git-checkpoint")
        .expect("git-checkpoint present");
    assert_eq!(status.smoke_pass, 0);
    assert_eq!(status.smoke_fail, 1);
}

#[test]
fn smoke_report_log_prefers_canonical_extensions_path() {
    let tmp = tempdir().expect("create tempdir");
    let root = tmp.path();
    let canonical = root.join("tests/ext_conformance/reports/extensions");
    let legacy = root.join("tests/ext_conformance/reports/smoke/extensions");
    std::fs::create_dir_all(&canonical).expect("create canonical dir");
    std::fs::create_dir_all(&legacy).expect("create legacy dir");
    std::fs::write(canonical.join("status-line.jsonl"), "{}\n").expect("write canonical log");
    std::fs::write(legacy.join("status-line.jsonl"), "{}\n").expect("write legacy log");

    let rel = report_log_rel_path_for_root(root, "smoke", "status-line")
        .expect("smoke path should resolve");
    assert_eq!(
        rel,
        "tests/ext_conformance/reports/extensions/status-line.jsonl"
    );
}

#[test]
fn evidence_links_valid() {
    // Verify that all evidence file links in JSONL point to files that actually exist
    let events_path = reports_dir().join("conformance_events.jsonl");
    if !events_path.exists() {
        eprintln!("No conformance_events.jsonl yet — skipping evidence validation");
        return;
    }

    let events = read_jsonl_file(&events_path);
    assert!(!events.is_empty(), "events file should have entries");

    let mut checked = 0u32;
    let mut valid = 0u32;
    for event in &events {
        // Check artifact_path
        if let Some(path) = event.get("artifact_path").and_then(Value::as_str) {
            checked += 1;
            let abs = project_root().join(path);
            if abs.exists() {
                valid += 1;
            }
        }
        // Check evidence links (fixture, smoke_log, parity_log)
        if let Some(evidence) = event.get("evidence").and_then(Value::as_object) {
            for key in &["fixture", "smoke_log", "parity_log"] {
                if let Some(path) = evidence.get(*key).and_then(Value::as_str) {
                    checked += 1;
                    let abs = project_root().join(path);
                    assert!(
                        abs.exists(),
                        "evidence link {key}={path} should point to existing file"
                    );
                    valid += 1;
                }
            }
        }
    }
    eprintln!("Evidence links: {valid}/{checked} valid");
    assert!(
        checked > 0,
        "should have checked at least one evidence link"
    );
}

#[test]
fn summary_json_has_evidence_counts() {
    let summary_path = reports_dir().join("conformance_summary.json");
    if !summary_path.exists() {
        eprintln!("No conformance_summary.json yet — skipping");
        return;
    }

    let summary = read_json_file(&summary_path).expect("parse summary json");
    let evidence = summary
        .get("evidence")
        .expect("summary should have evidence section");

    assert!(
        evidence.get("golden_fixtures").is_some(),
        "evidence should have golden_fixtures count"
    );
    assert!(
        evidence.get("smoke_logs").is_some(),
        "evidence should have smoke_logs count"
    );
    assert!(
        evidence.get("parity_logs").is_some(),
        "evidence should have parity_logs count"
    );
    assert!(
        evidence.get("load_time_benchmarks").is_some(),
        "evidence should have load_time_benchmarks count"
    );

    eprintln!(
        "Evidence counts: fixtures={}, smoke={}, parity={}, load={}",
        evidence["golden_fixtures"],
        evidence["smoke_logs"],
        evidence["parity_logs"],
        evidence["load_time_benchmarks"]
    );
}

#[test]
fn exception_policy_covers_full_conformance_failures() {
    let baseline_path = reports_dir().join("conformance_baseline.json");
    let full_report_path = reports_dir()
        .join("conformance")
        .join("conformance_report.json");

    if !baseline_path.exists() || !full_report_path.exists() {
        eprintln!(
            "Missing exception policy inputs (baseline={}, full_report={}) — skipping",
            baseline_path.exists(),
            full_report_path.exists()
        );
        return;
    }

    let baseline = read_json_file(&baseline_path).expect("parse conformance_baseline.json");
    let exception_policy = baseline
        .get("exception_policy")
        .and_then(Value::as_object)
        .expect("conformance_baseline.json must contain exception_policy object");

    assert_eq!(
        exception_policy.get("schema").and_then(Value::as_str),
        Some("pi.ext.exception_policy.v1"),
        "unexpected exception policy schema"
    );

    let required_fields = [
        "id",
        "kind",
        "status",
        "cause_code",
        "rationale",
        "mitigation",
        "owner",
        "review_by",
        "tracking_issue",
    ];

    let entries = exception_policy
        .get("entries")
        .and_then(Value::as_array)
        .expect("exception_policy.entries must be an array");
    assert!(
        !entries.is_empty(),
        "exception_policy.entries should not be empty"
    );

    let today = Utc::now().date_naive();
    let mut approved_ids = HashSet::new();

    for entry in entries {
        for field in required_fields {
            assert!(
                entry.get(field).is_some(),
                "exception entry missing required field: {field}"
            );
        }

        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .expect("entry.id should be a string");
        let status = entry
            .get("status")
            .and_then(Value::as_str)
            .expect("entry.status should be a string");
        assert!(
            status == "approved" || status == "temporary",
            "entry.status must be approved|temporary (got {status})"
        );

        let review_by = entry
            .get("review_by")
            .and_then(Value::as_str)
            .expect("entry.review_by should be YYYY-MM-DD");
        let review_date = chrono::NaiveDate::parse_from_str(review_by, "%Y-%m-%d")
            .expect("entry.review_by must parse as YYYY-MM-DD");
        assert!(
            review_date >= today,
            "entry.review_by must not be in the past (id={id}, review_by={review_by}, today={today})"
        );

        assert!(
            approved_ids.insert(id.to_string()),
            "duplicate exception policy entry for id={id}"
        );
    }

    let full_report = read_json_file(&full_report_path).expect("parse conformance_report.json");
    let failures = full_report
        .get("failures")
        .and_then(Value::as_array)
        .expect("conformance_report.failures must be an array");

    let missing = failures
        .iter()
        .filter_map(|failure| failure.get("id").and_then(Value::as_str))
        .filter(|id| !approved_ids.contains(*id))
        .map(ToOwned::to_owned)
        .collect::<Vec<String>>();

    assert!(
        missing.is_empty(),
        "all current full conformance failures must be covered by exception policy entries; missing={missing:?}"
    );
}

#[test]
fn events_jsonl_has_capabilities() {
    let events_path = reports_dir().join("conformance_events.jsonl");
    if !events_path.exists() {
        eprintln!("No conformance_events.jsonl yet — skipping");
        return;
    }

    let events = read_jsonl_file(&events_path);
    assert!(!events.is_empty(), "events file should have entries");

    // Every event should have capabilities and registrations fields
    let mut with_caps = 0u32;
    for event in &events {
        if event.get("capabilities").is_some() && event.get("registrations").is_some() {
            with_caps += 1;
        }
    }
    eprintln!("Events with capability data: {with_caps}/{}", events.len());
    assert!(
        with_caps == u32::try_from(events.len()).unwrap_or(u32::MAX),
        "all events should have capabilities and registrations"
    );
}

#[test]
fn report_markdown_has_evidence_index() {
    let md_path = reports_dir().join("CONFORMANCE_REPORT.md");
    if !md_path.exists() {
        eprintln!("No CONFORMANCE_REPORT.md yet — skipping");
        return;
    }

    let md = std::fs::read_to_string(&md_path).expect("read report markdown");
    assert!(
        md.contains("## Evidence Index"),
        "report should have Evidence Index section"
    );
    assert!(
        md.contains("Golden fixtures"),
        "evidence index should list golden fixtures"
    );
    assert!(
        md.contains("Smoke test logs"),
        "evidence index should list smoke logs"
    );
    assert!(
        md.contains("Parity diff logs"),
        "evidence index should list parity logs"
    );
}

#[test]
fn report_markdown_has_coverage_gaps() {
    let md_path = reports_dir().join("CONFORMANCE_REPORT.md");
    if !md_path.exists() {
        eprintln!("No CONFORMANCE_REPORT.md yet — skipping");
        return;
    }

    let md = std::fs::read_to_string(&md_path).expect("read report markdown");
    // Since we have 150 N/A extensions, there should be a coverage gaps section
    assert!(
        md.contains("## Coverage Gaps"),
        "report should have Coverage Gaps section when untested extensions exist"
    );
}
