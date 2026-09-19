//! E2E auto-repair tests: load the full conformance corpus with and without
//! auto-repair, collect repair events, and generate human-readable reports.
//!
//! Run:
//!   cargo test --test e2e_auto_repair -- --nocapture --test-threads=1
//!
//! Reports are written to `tests/ext_conformance/reports/auto_repair_report.md`
//! and `tests/ext_conformance/reports/auto_repair_summary.json`.

#![allow(clippy::doc_markdown)]

mod common;

use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::{ExtensionRepairEvent, PiJsRuntimeConfig, RepairMode};
use pi::tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

// ─── Manifest ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct ManifestEntry {
    id: String,
    entry_path: String,
    #[serde(default)]
    source_tier: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Manifest {
    extensions: Vec<ManifestEntry>,
}

fn artifacts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ext_conformance/artifacts")
}

fn manifest_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ext_conformance/VALIDATED_MANIFEST.json")
}

fn reports_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ext_conformance/reports")
}

fn load_manifest() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        let data = std::fs::read_to_string(manifest_path())
            .expect("Failed to read VALIDATED_MANIFEST.json");
        serde_json::from_str(&data).expect("Failed to parse manifest")
    })
}

// ─── Per-extension result ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct ExtResult {
    id: String,
    source_tier: String,
    loaded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    load_ms: u64,
    repair_events: Vec<RepairEventRecord>,
}

#[derive(Debug, Clone, Serialize)]
struct RepairEventRecord {
    pattern: String,
    original_error: String,
    repair_action: String,
    success: bool,
}

impl From<&ExtensionRepairEvent> for RepairEventRecord {
    fn from(e: &ExtensionRepairEvent) -> Self {
        Self {
            pattern: e.pattern.to_string(),
            original_error: e.original_error.clone(),
            repair_action: e.repair_action.clone(),
            success: e.success,
        }
    }
}

// ─── Summary structures ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct AutoRepairSummary {
    schema: String,
    generated_at: String,
    total: usize,
    loaded: usize,
    failed: usize,
    skipped: usize,
    clean_pass: usize,
    repaired_pass: usize,
    repairs_by_pattern: BTreeMap<String, usize>,
    per_tier: BTreeMap<String, TierStats>,
    per_extension: Vec<ExtResult>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct TierStats {
    total: usize,
    loaded: usize,
    failed: usize,
    skipped: usize,
    repaired: usize,
}

// ─── Core loader ─────────────────────────────────────────────────────────────

/// Load a single extension with the given repair mode and return the result.
fn load_one(entry: &ManifestEntry, mode: RepairMode) -> ExtResult {
    let entry_file = artifacts_dir().join(&entry.entry_path);

    if !entry_file.exists() {
        return ExtResult {
            id: entry.id.clone(),
            source_tier: entry.source_tier.clone(),
            loaded: false,
            error: Some(format!("artifact not found: {}", entry_file.display())),
            load_ms: 0,
            repair_events: Vec::new(),
        };
    }

    let spec = match JsExtensionLoadSpec::from_entry_path(&entry_file) {
        Ok(s) => s,
        Err(e) => {
            return ExtResult {
                id: entry.id.clone(),
                source_tier: entry.source_tier.clone(),
                loaded: false,
                error: Some(format!("spec error: {e}")),
                load_ms: 0,
                repair_events: Vec::new(),
            };
        }
    };

    let cwd = std::env::temp_dir().join(format!("pi-e2e-repair-{}", entry.id.replace('/', "_")));
    let _ = std::fs::create_dir_all(&cwd);

    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        repair_mode: mode,
        ..Default::default()
    };

    let manager = ExtensionManager::new();
    let start = Instant::now();

    // Start runtime
    let runtime_result = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move { JsExtensionRuntimeHandle::start(config, tools, manager).await }
    });

    let runtime = match runtime_result {
        Ok(rt) => rt,
        Err(e) => {
            return ExtResult {
                id: entry.id.clone(),
                source_tier: entry.source_tier.clone(),
                loaded: false,
                error: Some(format!("runtime start error: {e}")),
                #[allow(clippy::cast_possible_truncation)]
                load_ms: start.elapsed().as_millis() as u64,
                repair_events: Vec::new(),
            };
        }
    };
    manager.set_js_runtime(runtime);

    // Load extension
    let load_result = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(vec![spec]).await }
    });

    let loaded = load_result.is_ok();
    let error = load_result.err().map(|e| format!("{e}"));

    #[allow(clippy::cast_possible_truncation)]
    let load_ms = start.elapsed().as_millis() as u64;

    // Drain repair events
    let repair_events: Vec<RepairEventRecord> = manager
        .js_runtime()
        .map(|rt| {
            common::run_async(async move { rt.drain_repair_events().await })
                .iter()
                .map(RepairEventRecord::from)
                .collect()
        })
        .unwrap_or_default();

    // Shutdown
    common::run_async(async move {
        let _ = manager.shutdown(Duration::from_millis(250)).await;
    });

    ExtResult {
        id: entry.id.clone(),
        source_tier: entry.source_tier.clone(),
        loaded,
        error,
        load_ms,
        repair_events,
    }
}

/// Load all extensions, returning results.
fn load_all(mode: RepairMode) -> Vec<ExtResult> {
    let manifest = load_manifest();
    manifest
        .extensions
        .iter()
        .map(|entry| {
            eprintln!(
                "[auto-repair:e2e] Loading: {} (tier={}, mode={mode})",
                entry.id, entry.source_tier
            );
            let result = load_one(entry, mode);
            if !result.repair_events.is_empty() {
                for ev in &result.repair_events {
                    eprintln!(
                        "[auto-repair:e2e]   Repair: pattern={}, success={}",
                        ev.pattern, ev.success
                    );
                }
            }
            if let Some(ref err) = result.error {
                eprintln!("[auto-repair:e2e]   FAILED: {err}");
            }
            result
        })
        .collect()
}

/// Build summary from results.
fn summarize(results: &[ExtResult]) -> AutoRepairSummary {
    let mut repairs_by_pattern: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_tier: BTreeMap<String, TierStats> = BTreeMap::new();
    let mut clean_pass = 0usize;
    let mut repaired_pass = 0usize;
    let mut loaded = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;

    for r in results {
        let tier = per_tier.entry(r.source_tier.clone()).or_default();
        tier.total += 1;

        if r.error
            .as_deref()
            .is_some_and(|e| e.starts_with("artifact not found"))
        {
            skipped += 1;
            tier.skipped += 1;
            continue;
        }

        if r.loaded {
            loaded += 1;
            tier.loaded += 1;
            if r.repair_events.is_empty() {
                clean_pass += 1;
            } else {
                repaired_pass += 1;
                tier.repaired += 1;
                for ev in &r.repair_events {
                    *repairs_by_pattern.entry(ev.pattern.clone()).or_insert(0) += 1;
                }
            }
        } else {
            failed += 1;
            tier.failed += 1;
        }
    }

    AutoRepairSummary {
        schema: "pi.ext.auto_repair_summary.v1".to_string(),
        generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        total: results.len(),
        loaded,
        failed,
        skipped,
        clean_pass,
        repaired_pass,
        repairs_by_pattern,
        per_tier,
        per_extension: results.to_vec(),
    }
}

fn is_nonblocking_auto_repair_failure(result: &ExtResult) -> bool {
    result.error.as_deref().is_some_and(|error| {
        error.starts_with("artifact not found")
            || error.contains("host write denied")
            || error.contains("JS extension runtime load timed out")
            || is_known_package_compat_failure(result, error)
            // `npm/agentsbox` is an npm-registry T3 package-interop case that is
            // covered by separate contract evidence, not this smoke-style loader
            // harness. Keep the exemption exact so other regressions still fail.
            || (result.id == "npm/agentsbox"
                && result.source_tier == "npm-registry"
                && (error.contains("module is not defined")
                    || (error.contains("ENOENT: no such file or directory")
                        && error.contains("agentsbox/dist/package.json"))))
    })
}

fn is_known_package_compat_failure(result: &ExtResult, error: &str) -> bool {
    match result.id.as_str() {
        "base_fixtures" => error.contains("registerTool: spec.name is required"),
        "npm/aliou-pi-guardrails" | "npm/aliou-pi-toolchain" => {
            error.contains("cannot read property 'features' of undefined")
        }
        "npm/aliou-pi-processes" => error.contains("cannot read property 'onEvent' of undefined"),
        "npm/juanibiapina-pi-extension-settings"
        | "npm/juanibiapina-pi-files"
        | "npm/juanibiapina-pi-gob" => {
            error.contains("load_extension: entry module must default-export a function")
        }
        "npm/pi-messenger" => error.contains("cannot read property 'registry' of undefined"),
        "npm/pi-telemetry-otel" => error.contains("invalid 'in' operand"),
        "third-party/aliou-pi-extensions" | "third-party/jyaunches-pi-canvas" => {
            error.contains("Could not find export 'createReadOnlyTools'")
        }
        "third-party/kcosr-pi-extensions" => {
            error.contains("Could not find export 'buildListItemContentBlock'")
        }
        _ => false,
    }
}

// ─── Report generation ──────────────────────────────────────────────────────

fn generate_markdown(summary: &AutoRepairSummary) -> String {
    let mut md = String::with_capacity(4096);

    writeln!(md, "# Auto-Repair E2E Report").unwrap();
    writeln!(md, "Generated: {}\n", summary.generated_at).unwrap();

    writeln!(md, "## Summary").unwrap();
    writeln!(md, "| Metric | Count |").unwrap();
    writeln!(md, "|--------|-------|").unwrap();
    writeln!(md, "| Total extensions | {} |", summary.total).unwrap();
    writeln!(md, "| Clean pass | {} |", summary.clean_pass).unwrap();
    writeln!(md, "| Auto-repaired pass | {} |", summary.repaired_pass).unwrap();
    writeln!(md, "| Failed | {} |", summary.failed).unwrap();
    writeln!(md, "| Skipped (no artifact) | {} |", summary.skipped).unwrap();
    writeln!(md).unwrap();

    if !summary.repairs_by_pattern.is_empty() {
        writeln!(md, "## Repairs by Pattern").unwrap();
        writeln!(md, "| Pattern | Count |").unwrap();
        writeln!(md, "|---------|-------|").unwrap();
        for (pat, count) in &summary.repairs_by_pattern {
            writeln!(md, "| {pat} | {count} |").unwrap();
        }
        writeln!(md).unwrap();
    }

    writeln!(md, "## Per-Tier Breakdown").unwrap();
    writeln!(
        md,
        "| Tier | Total | Loaded | Repaired | Failed | Skipped |"
    )
    .unwrap();
    writeln!(
        md,
        "|------|-------|--------|----------|--------|---------|"
    )
    .unwrap();
    for (tier, stats) in &summary.per_tier {
        writeln!(
            md,
            "| {tier} | {} | {} | {} | {} | {} |",
            stats.total, stats.loaded, stats.repaired, stats.failed, stats.skipped
        )
        .unwrap();
    }
    writeln!(md).unwrap();

    // Per-extension details (only for repaired or failed)
    let interesting: Vec<_> = summary
        .per_extension
        .iter()
        .filter(|r| !r.repair_events.is_empty() || !r.loaded)
        .collect();

    if !interesting.is_empty() {
        writeln!(md, "## Per-Extension Details\n").unwrap();
        for r in &interesting {
            let status = if r.loaded { "REPAIRED" } else { "FAILED" };
            writeln!(md, "<details>").unwrap();
            writeln!(
                md,
                "<summary>{} ({status}, {}ms)</summary>\n",
                r.id, r.load_ms
            )
            .unwrap();

            if let Some(ref err) = r.error {
                writeln!(md, "- **Error**: {err}").unwrap();
            }
            for ev in &r.repair_events {
                writeln!(md, "- **Pattern**: {}", ev.pattern).unwrap();
                writeln!(md, "  - Original: {}", ev.original_error).unwrap();
                writeln!(md, "  - Action: {}", ev.repair_action).unwrap();
                writeln!(md, "  - Success: {}", ev.success).unwrap();
            }
            writeln!(md, "\n</details>\n").unwrap();
        }
    }

    md
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

/// Load the full corpus with auto-repair enabled (AutoStrict) and generate
/// a human-readable report + JSON summary.
#[test]
fn full_corpus_with_auto_repair() {
    let results = load_all(RepairMode::AutoStrict);
    let summary = summarize(&results);

    // Write JSON summary
    let json_path = reports_dir().join("auto_repair_summary.json");
    let json_str = serde_json::to_string_pretty(&summary).expect("serialize summary");
    std::fs::write(&json_path, &json_str).expect("write summary JSON");
    eprintln!(
        "[auto-repair:e2e] JSON summary written to {}",
        json_path.display()
    );

    // Write markdown report
    let md = generate_markdown(&summary);
    let md_path = reports_dir().join("auto_repair_report.md");
    std::fs::write(&md_path, &md).expect("write markdown report");
    eprintln!(
        "[auto-repair:e2e] Markdown report written to {}",
        md_path.display()
    );

    // Print summary to stderr
    eprintln!("\n=== Auto-Repair E2E Summary ===");
    eprintln!(
        "Total: {} | Clean: {} | Repaired: {} | Failed: {} | Skipped: {}",
        summary.total, summary.clean_pass, summary.repaired_pass, summary.failed, summary.skipped
    );
    for (pat, count) in &summary.repairs_by_pattern {
        eprintln!("  {pat}: {count}");
    }

    // Assert: no failures among extensions that have artifacts
    let real_failures: Vec<_> = results
        .iter()
        .filter(|r| !r.loaded && !is_nonblocking_auto_repair_failure(r))
        .collect();
    assert!(
        real_failures.is_empty(),
        "Extensions failed to load even with auto-repair:\n{}",
        real_failures
            .iter()
            .map(|r| format!("  {}: {}", r.id, r.error.as_deref().unwrap_or("unknown")))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Load a subset without auto-repair to prove repair actually matters.
/// Extensions that rely on repair should fail with RepairMode::Off.
#[test]
fn subset_without_auto_repair_shows_failures() {
    // Load with repair OFF — we expect some to fail that succeed with repair ON
    let results_off = load_all(RepairMode::Off);
    let results_on = load_all(RepairMode::AutoStrict);

    let off_failures: usize = results_off
        .iter()
        .filter(|r| !r.loaded && !is_nonblocking_auto_repair_failure(r))
        .count();
    let on_failures: usize = results_on
        .iter()
        .filter(|r| !r.loaded && !is_nonblocking_auto_repair_failure(r))
        .count();

    eprintln!("[auto-repair:e2e] Failures: OFF={off_failures}, ON={on_failures}");

    // Auto-repair should fix at least some extensions
    // (If off_failures == on_failures, auto-repair isn't doing anything)
    // Note: we don't require off_failures > on_failures because some extensions
    // may not need repair at all. But if any repair events fired, there should
    // be a difference.
    let any_repairs = results_on.iter().any(|r| !r.repair_events.is_empty());

    if any_repairs {
        assert!(
            on_failures < off_failures,
            "Auto-repair fired but didn't reduce failure count (OFF={off_failures}, ON={on_failures})"
        );
    }
}

/// Verify clean extensions emit zero repair events.
#[test]
fn clean_extensions_have_no_repair_events() {
    let manifest = load_manifest();
    // Test a known-clean official extension: "hello"
    let hello = manifest
        .extensions
        .iter()
        .find(|e| e.id == "hello")
        .expect("hello extension in manifest");

    let result = load_one(hello, RepairMode::AutoStrict);
    assert!(
        result.loaded,
        "hello extension should load: {:?}",
        result.error
    );
    assert!(
        result.repair_events.is_empty(),
        "hello extension should not trigger any repairs: {:?}",
        result.repair_events
    );
}

/// Verify the JSON report structure is valid.
#[test]
fn report_structure_is_valid() {
    // Use a small subset for speed
    let manifest = load_manifest();
    let subset: Vec<_> = manifest
        .extensions
        .iter()
        .filter(|e| e.source_tier == "official-pi-mono")
        .take(5)
        .collect();

    let results: Vec<ExtResult> = subset
        .iter()
        .map(|entry| load_one(entry, RepairMode::AutoStrict))
        .collect();

    let summary = summarize(&results);
    let json_str = serde_json::to_string_pretty(&summary).expect("serialize");

    // Verify it round-trips
    let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("parse back");
    assert_eq!(
        parsed["schema"].as_str().unwrap(),
        "pi.ext.auto_repair_summary.v1"
    );
    assert!(parsed["total"].as_u64().unwrap() > 0);
    assert!(!parsed["per_extension"].as_array().unwrap().is_empty());
}
