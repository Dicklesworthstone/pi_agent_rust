#![cfg(feature = "ext-conformance")]
//! Auto-generated conformance tests for all extensions in the validated manifest (bd-15jg).
//!
//! Each extension in `tests/ext_conformance/VALIDATED_MANIFEST.json` gets its own `#[test]`
//! function via the `conformance_test!` macro. This provides:
//! - Parallelism (cargo test runs tests in parallel)
//! - Isolation (one failure does not block others)
//! - Clear reporting (each extension shows as pass/fail in test output)
//!
//! Tiers 1–2 run by default; tiers 3–5 are `#[ignore]` (multi-file, npm deps, UI, platform).
//!
//! Run all (including ignored):
//!   cargo test --test `ext_conformance_generated` -- --include-ignored
//!
//! Run only tier 1–2 (default):
//!   cargo test --test `ext_conformance_generated`

mod common;

use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

// ─── Manifest types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ManifestEntry {
    id: String,
    entry_path: String,
    conformance_tier: u32,
    capabilities: Capabilities,
    registrations: Registrations,
}

#[derive(Debug, Clone)]
#[allow(dead_code, clippy::struct_excessive_bools)]
struct Capabilities {
    registers_tools: bool,
    registers_commands: bool,
    registers_flags: bool,
    registers_providers: bool,
    subscribes_events: Vec<String>,
    is_multi_file: bool,
    has_npm_deps: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Registrations {
    tools: Vec<String>,
    commands: Vec<String>,
    flags: Vec<String>,
    event_handlers: Vec<String>,
}

#[derive(Debug)]
struct Manifest {
    extensions: Vec<ManifestEntry>,
}

impl Manifest {
    fn find(&self, ext_id: &str) -> Option<&ManifestEntry> {
        self.extensions.iter().find(|e| e.id == ext_id)
    }
}

// ─── Manifest loading ───────────────────────────────────────────────────────

fn artifacts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("ext_conformance")
        .join("artifacts")
}

fn manifest_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("ext_conformance")
        .join("VALIDATED_MANIFEST.json")
}

fn load_manifest() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        let data = std::fs::read_to_string(manifest_path())
            .expect("Failed to read VALIDATED_MANIFEST.json");
        let json: Value =
            serde_json::from_str(&data).expect("Failed to parse VALIDATED_MANIFEST.json");

        let extensions = json["extensions"]
            .as_array()
            .expect("manifest.extensions should be an array")
            .iter()
            .map(|e| {
                let caps = &e["capabilities"];
                let regs = &e["registrations"];
                ManifestEntry {
                    id: e["id"].as_str().unwrap_or("").to_string(),
                    entry_path: e["entry_path"].as_str().unwrap_or("").to_string(),
                    conformance_tier: u32::try_from(e["conformance_tier"].as_u64().unwrap_or(0))
                        .unwrap_or(0),
                    capabilities: Capabilities {
                        registers_tools: caps["registers_tools"].as_bool().unwrap_or(false),
                        registers_commands: caps["registers_commands"].as_bool().unwrap_or(false),
                        registers_flags: caps["registers_flags"].as_bool().unwrap_or(false),
                        registers_providers: caps["registers_providers"].as_bool().unwrap_or(false),
                        subscribes_events: caps["subscribes_events"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        is_multi_file: caps["is_multi_file"].as_bool().unwrap_or(false),
                        has_npm_deps: caps["has_npm_deps"].as_bool().unwrap_or(false),
                    },
                    registrations: Registrations {
                        tools: regs["tools"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        commands: regs["commands"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        flags: regs["flags"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        event_handlers: regs["event_handlers"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    },
                }
            })
            .collect();

        Manifest { extensions }
    })
}

// ─── Core conformance test runner ───────────────────────────────────────────

/// Load an extension from the artifacts directory, validate registrations match
/// the manifest, and capture a registration snapshot as a JSON artifact.
#[allow(clippy::too_many_lines)]
fn run_conformance_test(ext_id: &str) {
    let manifest = load_manifest();
    let Some(entry) = manifest.find(ext_id) else {
        unreachable!("Extension '{ext_id}' not found in VALIDATED_MANIFEST.json");
    };

    let harness = common::TestHarness::new(format!("conformance_{}", ext_id.replace('/', "_")));
    let cwd = harness.temp_dir().to_path_buf();

    // Resolve the extension entry file.
    let entry_file = artifacts_dir().join(&entry.entry_path);
    assert!(
        entry_file.exists(),
        "Extension artifact not found: {}",
        entry_file.display()
    );

    harness
        .log()
        .info_ctx("conformance", "Loading extension", |ctx| {
            ctx.push(("ext_id".into(), ext_id.to_string()));
            ctx.push(("entry_path".into(), entry.entry_path.clone()));
            ctx.push(("tier".into(), entry.conformance_tier.to_string()));
        });

    // Create the load spec.
    let spec = JsExtensionLoadSpec::from_entry_path(&entry_file).unwrap_or_else(|e| {
        unreachable!(
            "Failed to create JsExtensionLoadSpec for '{ext_id}' at {}: {e}",
            entry_file.display()
        )
    });

    // Start JS runtime and load the extension.
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start JS runtime")
        }
    });
    manager.set_js_runtime(runtime);

    let ext_id_owned = ext_id.to_string();
    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .unwrap_or_else(|e| unreachable!("Failed to load extension '{ext_id_owned}': {e}"));
        }
    });

    harness
        .log()
        .info("conformance", "Extension loaded successfully");

    // Capture registration snapshot.
    let snapshot = snapshot_registrations(&manager);

    // Write snapshot artifact.
    let snapshot_path = harness.temp_path("registration_snapshot.json");
    let snapshot_json =
        serde_json::to_string_pretty(&snapshot).expect("serialize registration snapshot");
    std::fs::write(&snapshot_path, &snapshot_json).expect("write snapshot");
    harness.record_artifact("registration_snapshot", &snapshot_path);

    // ── Validate registrations against manifest ──

    // Commands: verify every manifest-listed command was registered.
    let actual_commands = manager.list_commands();
    let actual_cmd_names: Vec<&str> = actual_commands
        .iter()
        .filter_map(|v| v.get("name").and_then(Value::as_str))
        .collect();

    for expected_cmd in &entry.registrations.commands {
        assert!(
            actual_cmd_names.contains(&expected_cmd.as_str()),
            "Extension '{ext_id}': expected command '{expected_cmd}' not found in actual commands: {actual_cmd_names:?}"
        );
    }

    // If the manifest says the extension registers commands, verify at least one was captured.
    if entry.capabilities.registers_commands && !entry.registrations.commands.is_empty() {
        assert!(
            !actual_commands.is_empty(),
            "Extension '{ext_id}': manifest says it registers commands, but none were captured"
        );
    }

    // Flags: verify expected flags were registered.
    let actual_flags = manager.list_flags();
    let actual_flag_names: Vec<&str> = actual_flags
        .iter()
        .filter_map(|v| v.get("name").and_then(Value::as_str))
        .collect();

    for expected_flag in &entry.registrations.flags {
        assert!(
            actual_flag_names.contains(&expected_flag.as_str()),
            "Extension '{ext_id}': expected flag '{expected_flag}' not found in actual flags: {actual_flag_names:?}"
        );
    }

    // Tools: if manifest says the extension registers tools, verify tool defs exist.
    if entry.capabilities.registers_tools {
        let tool_defs = manager.extension_tool_defs();
        assert!(
            !tool_defs.is_empty(),
            "Extension '{ext_id}': manifest says it registers tools, but no tool defs were captured"
        );
    }

    // Providers: if manifest says it registers providers, verify they exist.
    if entry.capabilities.registers_providers {
        let providers = manager.extension_providers();
        assert!(
            !providers.is_empty(),
            "Extension '{ext_id}': manifest says it registers providers, but none were captured"
        );
    }

    harness
        .log()
        .info_ctx("conformance", "Validation passed", |ctx| {
            ctx.push(("commands".into(), actual_commands.len().to_string()));
            ctx.push(("flags".into(), actual_flags.len().to_string()));
            ctx.push((
                "tool_defs".into(),
                manager.extension_tool_defs().len().to_string(),
            ));
            ctx.push((
                "providers".into(),
                manager.extension_providers().len().to_string(),
            ));
        });

    // Write JSONL logs.
    let logs_path = harness.temp_path("test_logs.jsonl");
    if let Err(e) = harness.write_jsonl_logs_normalized(&logs_path) {
        harness
            .log()
            .warn("jsonl", format!("Failed to write JSONL logs: {e}"));
    } else {
        harness.record_artifact("jsonl_logs", &logs_path);
    }
}

/// Capture the current registration state as a JSON snapshot.
fn snapshot_registrations(manager: &ExtensionManager) -> Value {
    let commands = manager.list_commands();
    let shortcuts = manager.list_shortcuts();
    let flags = manager.list_flags();
    let providers = manager.extension_providers();
    let tool_defs = manager.extension_tool_defs();
    let models: Vec<Value> = manager
        .extension_model_entries()
        .into_iter()
        .map(|entry| serde_json::to_value(entry.model).expect("model to json"))
        .collect();

    serde_json::json!({
        "commands": commands,
        "shortcuts": shortcuts,
        "flags": flags,
        "providers": providers,
        "tool_defs": tool_defs,
        "models": models,
    })
}

// ─── Report generator (bd-31j) ──────────────────────────────────────────────

/// Result of a single extension conformance check.
#[derive(Debug, serde::Serialize)]
struct ExtensionConformanceResult {
    id: String,
    tier: u32,
    status: String, // "pass", "fail", "skip"
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact_path: Option<String>,
    commands_registered: usize,
    flags_registered: usize,
    tools_registered: usize,
    providers_registered: usize,
    duration_ms: u64,
}

/// Run conformance check for a single extension, returning a result without
/// panicking. This is used by the report generator to collect all results
/// even when some extensions fail.
#[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
fn try_conformance(ext_id: &str) -> ExtensionConformanceResult {
    let manifest = load_manifest();
    let Some(entry) = manifest.find(ext_id) else {
        return ExtensionConformanceResult {
            id: ext_id.to_string(),
            tier: 0,
            status: "skip".to_string(),
            failure_reason: Some("Not found in VALIDATED_MANIFEST.json".to_string()),
            artifact_path: None,
            commands_registered: 0,
            flags_registered: 0,
            tools_registered: 0,
            providers_registered: 0,
            duration_ms: 0,
        };
    };

    let start = std::time::Instant::now();
    let cwd = std::env::temp_dir().join(format!(
        "pi-conformance-report-{}",
        ext_id.replace('/', "_")
    ));
    let _ = std::fs::create_dir_all(&cwd);

    let entry_file = artifacts_dir().join(&entry.entry_path);
    if !entry_file.exists() {
        return ExtensionConformanceResult {
            id: ext_id.to_string(),
            tier: entry.conformance_tier,
            status: "skip".to_string(),
            failure_reason: Some(format!("Artifact not found: {}", entry_file.display())),
            artifact_path: None,
            commands_registered: 0,
            flags_registered: 0,
            tools_registered: 0,
            providers_registered: 0,
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    let spec = match JsExtensionLoadSpec::from_entry_path(&entry_file) {
        Ok(s) => s,
        Err(e) => {
            return ExtensionConformanceResult {
                id: ext_id.to_string(),
                tier: entry.conformance_tier,
                status: "fail".to_string(),
                failure_reason: Some(format!("Load spec error: {e}")),
                artifact_path: None,
                commands_registered: 0,
                flags_registered: 0,
                tools_registered: 0,
                providers_registered: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let runtime_result = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move { JsExtensionRuntimeHandle::start(js_config, tools, manager).await }
    });
    let runtime = match runtime_result {
        Ok(rt) => rt,
        Err(e) => {
            return ExtensionConformanceResult {
                id: ext_id.to_string(),
                tier: entry.conformance_tier,
                status: "fail".to_string(),
                failure_reason: Some(format!("Runtime start error: {e}")),
                artifact_path: None,
                commands_registered: 0,
                flags_registered: 0,
                tools_registered: 0,
                providers_registered: 0,
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };
    manager.set_js_runtime(runtime);

    let load_err = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(vec![spec]).await }
    });
    if let Err(e) = load_err {
        return ExtensionConformanceResult {
            id: ext_id.to_string(),
            tier: entry.conformance_tier,
            status: "fail".to_string(),
            failure_reason: Some(format!("Load error: {e}")),
            artifact_path: None,
            commands_registered: 0,
            flags_registered: 0,
            tools_registered: 0,
            providers_registered: 0,
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    // Validate registrations against manifest.
    let actual_commands = manager.list_commands();
    let actual_cmd_names: Vec<&str> = actual_commands
        .iter()
        .filter_map(|v| v.get("name").and_then(Value::as_str))
        .collect();

    for expected_cmd in &entry.registrations.commands {
        if !actual_cmd_names.contains(&expected_cmd.as_str()) {
            return ExtensionConformanceResult {
                id: ext_id.to_string(),
                tier: entry.conformance_tier,
                status: "fail".to_string(),
                failure_reason: Some(format!(
                    "Missing command '{expected_cmd}'. Actual: {actual_cmd_names:?}"
                )),
                artifact_path: None,
                commands_registered: actual_commands.len(),
                flags_registered: manager.list_flags().len(),
                tools_registered: manager.extension_tool_defs().len(),
                providers_registered: manager.extension_providers().len(),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    }

    let actual_flags = manager.list_flags();
    let actual_flag_names: Vec<&str> = actual_flags
        .iter()
        .filter_map(|v| v.get("name").and_then(Value::as_str))
        .collect();

    for expected_flag in &entry.registrations.flags {
        if !actual_flag_names.contains(&expected_flag.as_str()) {
            return ExtensionConformanceResult {
                id: ext_id.to_string(),
                tier: entry.conformance_tier,
                status: "fail".to_string(),
                failure_reason: Some(format!(
                    "Missing flag '{expected_flag}'. Actual: {actual_flag_names:?}"
                )),
                artifact_path: None,
                commands_registered: actual_commands.len(),
                flags_registered: actual_flags.len(),
                tools_registered: manager.extension_tool_defs().len(),
                providers_registered: manager.extension_providers().len(),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    }

    if entry.capabilities.registers_tools && manager.extension_tool_defs().is_empty() {
        return ExtensionConformanceResult {
            id: ext_id.to_string(),
            tier: entry.conformance_tier,
            status: "fail".to_string(),
            failure_reason: Some("Manifest expects tools but none registered".to_string()),
            artifact_path: None,
            commands_registered: actual_commands.len(),
            flags_registered: actual_flags.len(),
            tools_registered: 0,
            providers_registered: manager.extension_providers().len(),
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    if entry.capabilities.registers_providers && manager.extension_providers().is_empty() {
        return ExtensionConformanceResult {
            id: ext_id.to_string(),
            tier: entry.conformance_tier,
            status: "fail".to_string(),
            failure_reason: Some("Manifest expects providers but none registered".to_string()),
            artifact_path: None,
            commands_registered: actual_commands.len(),
            flags_registered: actual_flags.len(),
            tools_registered: manager.extension_tool_defs().len(),
            providers_registered: 0,
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    let snapshot = snapshot_registrations(&manager);
    let snapshot_path = cwd.join("registration_snapshot.json");
    let _ = std::fs::write(
        &snapshot_path,
        serde_json::to_string_pretty(&snapshot).unwrap_or_default(),
    );

    ExtensionConformanceResult {
        id: ext_id.to_string(),
        tier: entry.conformance_tier,
        status: "pass".to_string(),
        failure_reason: None,
        artifact_path: Some(snapshot_path.display().to_string()),
        commands_registered: actual_commands.len(),
        flags_registered: actual_flags.len(),
        tools_registered: manager.extension_tool_defs().len(),
        providers_registered: manager.extension_providers().len(),
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

/// Generate a comprehensive conformance report for all extensions in the manifest.
///
/// Run: `cargo test --test ext_conformance_generated --features ext-conformance -- conformance_full_report --nocapture`
#[test]
#[allow(clippy::too_many_lines)]
fn conformance_full_report() {
    use chrono::{SecondsFormat, Utc};
    use std::fmt::Write as _;

    let manifest = load_manifest();
    let report_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("ext_conformance")
        .join("reports")
        .join("conformance");
    let _ = std::fs::create_dir_all(&report_dir);

    let mut results: Vec<ExtensionConformanceResult> = Vec::new();
    let total = manifest.extensions.len();

    eprintln!("\n=== Conformance Report Generator (bd-31j) ===");
    eprintln!("  Extensions in manifest: {total}");
    eprintln!("  Checking extensions with available artifacts...\n");

    for (idx, entry) in manifest.extensions.iter().enumerate() {
        let entry_file = artifacts_dir().join(&entry.entry_path);
        if !entry_file.exists() {
            results.push(ExtensionConformanceResult {
                id: entry.id.clone(),
                tier: entry.conformance_tier,
                status: "skip".to_string(),
                failure_reason: Some("Artifact not available".to_string()),
                artifact_path: None,
                commands_registered: 0,
                flags_registered: 0,
                tools_registered: 0,
                providers_registered: 0,
                duration_ms: 0,
            });
            continue;
        }

        eprint!("  [{:>3}/{total}] {:<50} ", idx + 1, &entry.id);
        let result = try_conformance(&entry.id);
        eprintln!(
            "{:<6} ({}ms)",
            result.status.to_uppercase(),
            result.duration_ms
        );
        results.push(result);
    }

    // ── Compute statistics ──
    let pass_count = results.iter().filter(|r| r.status == "pass").count();
    let fail_count = results.iter().filter(|r| r.status == "fail").count();
    let skip_count = results.iter().filter(|r| r.status == "skip").count();
    let tested = pass_count + fail_count;
    let pass_rate = if tested > 0 {
        #[allow(clippy::cast_precision_loss)]
        {
            (pass_count as f64) / (tested as f64) * 100.0
        }
    } else {
        0.0
    };

    let by_tier: std::collections::BTreeMap<u32, (usize, usize, usize)> = {
        let mut m = std::collections::BTreeMap::new();
        for r in &results {
            let entry = m.entry(r.tier).or_insert((0, 0, 0));
            match r.status.as_str() {
                "pass" => entry.0 += 1,
                "fail" => entry.1 += 1,
                _ => entry.2 += 1,
            }
        }
        m
    };

    // ── Write JSONL events ──
    let events_path = report_dir.join("conformance_events.jsonl");
    let mut lines: Vec<String> = Vec::new();
    for r in &results {
        let entry = serde_json::json!({
            "schema": "pi.ext.conformance_result.v1",
            "id": r.id,
            "tier": r.tier,
            "status": r.status,
            "failure_reason": r.failure_reason,
            "commands_registered": r.commands_registered,
            "flags_registered": r.flags_registered,
            "tools_registered": r.tools_registered,
            "providers_registered": r.providers_registered,
            "duration_ms": r.duration_ms,
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        });
        lines.push(serde_json::to_string(&entry).unwrap_or_default());
    }
    let _ = std::fs::write(&events_path, lines.join("\n") + "\n");

    // ── Write JSON summary ──
    let summary = serde_json::json!({
        "schema": "pi.ext.conformance_report.v1",
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "manifest_count": total,
        "tested": tested,
        "passed": pass_count,
        "failed": fail_count,
        "skipped": skip_count,
        "pass_rate_pct": pass_rate,
        "by_tier": by_tier.iter().map(|(tier, (p, f, s))| {
            serde_json::json!({
                "tier": tier,
                "pass": p,
                "fail": f,
                "skip": s,
            })
        }).collect::<Vec<_>>(),
        "failures": results.iter()
            .filter(|r| r.status == "fail")
            .map(|r| serde_json::json!({
                "id": r.id,
                "tier": r.tier,
                "reason": r.failure_reason,
            }))
            .collect::<Vec<_>>(),
    });
    let summary_path = report_dir.join("conformance_report.json");
    let _ = std::fs::write(
        &summary_path,
        serde_json::to_string_pretty(&summary).unwrap_or_default(),
    );

    // ── Write Markdown report ──
    let mut md = String::new();
    md.push_str("# Extension Conformance Report\n\n");
    let _ = writeln!(
        md,
        "> Generated: {}",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    md.push_str("\n## Summary\n\n");
    md.push_str("| Metric | Value |\n|--------|-------|\n");
    let _ = writeln!(md, "| Total in manifest | {total} |");
    let _ = writeln!(md, "| Tested | {tested} |");
    let _ = writeln!(md, "| Passed | {pass_count} |");
    let _ = writeln!(md, "| Failed | {fail_count} |");
    let _ = writeln!(md, "| Skipped | {skip_count} |");
    let _ = writeln!(md, "| Pass rate | {pass_rate:.1}% |");
    md.push('\n');

    md.push_str("## By Tier\n\n");
    md.push_str("| Tier | Pass | Fail | Skip |\n|------|------|------|------|\n");
    for (tier, (p, f, s)) in &by_tier {
        let _ = writeln!(md, "| {tier} | {p} | {f} | {s} |");
    }
    md.push('\n');

    if fail_count > 0 {
        md.push_str("## Failures\n\n");
        md.push_str("| Extension | Tier | Reason |\n|-----------|------|--------|\n");
        for r in results.iter().filter(|r| r.status == "fail") {
            let _ = writeln!(
                md,
                "| {} | {} | {} |",
                r.id,
                r.tier,
                r.failure_reason.as_deref().unwrap_or("unknown")
            );
        }
        md.push('\n');
    }

    md.push_str("## All Results\n\n");
    md.push_str("| Extension | Tier | Status | Cmds | Flags | Tools | Providers | Time (ms) |\n");
    md.push_str("|-----------|------|--------|------|-------|-------|-----------|-----------|\n");
    for r in &results {
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} | {} | {} | {} |",
            r.id,
            r.tier,
            r.status,
            r.commands_registered,
            r.flags_registered,
            r.tools_registered,
            r.providers_registered,
            r.duration_ms
        );
    }

    let md_path = report_dir.join("conformance_report.md");
    let _ = std::fs::write(&md_path, &md);

    // ── Print summary ──
    eprintln!("\n=== Conformance Report Summary ===");
    eprintln!("  Manifest: {total} extensions");
    eprintln!("  Tested:   {tested}");
    eprintln!("  Passed:   {pass_count}");
    eprintln!("  Failed:   {fail_count}");
    eprintln!("  Skipped:  {skip_count}");
    eprintln!("  Rate:     {pass_rate:.1}%");
    eprintln!("\n  Reports:");
    eprintln!("    JSON:   {}", summary_path.display());
    eprintln!("    JSONL:  {}", events_path.display());
    eprintln!("    MD:     {}\n", md_path.display());

    // Report is informational — per-extension tests enforce pass/fail individually.
    // Log a warning if failures exist so CI logs are visible but don't block the report.
    if fail_count > 0 {
        eprintln!(
            "  WARNING: {fail_count} extension(s) failed conformance. See {}",
            summary_path.display()
        );
    }
}

// ─── Macro ──────────────────────────────────────────────────────────────────

/// Generate a conformance test for a single extension.
macro_rules! conformance_test {
    ($name:ident, $ext_id:literal $(,)?) => {
        #[test]
        fn $name() {
            run_conformance_test($ext_id);
        }
    };
    ($name:ident, $ext_id:literal, ignore $(,)?) => {
        #[test]
        #[ignore = "TODO: enable once full extension conformance runner is supported"]
        fn $name() {
            run_conformance_test($ext_id);
        }
    };
}

// ─── Tier 1 — Single-file, no events, no deps (37 extensions) ──────────────

conformance_test!(ext_antigravity_image_gen, "antigravity-image-gen");
conformance_test!(ext_bash_spawn_hook, "bash-spawn-hook");
conformance_test!(ext_bookmark, "bookmark");
conformance_test!(
    ext_community_hjanuschka_clipboard,
    "community/hjanuschka-clipboard"
);
conformance_test!(
    ext_community_hjanuschka_cost_tracker,
    "community/hjanuschka-cost-tracker"
);
conformance_test!(
    ext_community_hjanuschka_flicker_corp,
    "community/hjanuschka-flicker-corp"
);
conformance_test!(
    ext_community_hjanuschka_handoff,
    "community/hjanuschka-handoff"
);
conformance_test!(
    ext_community_hjanuschka_memory_mode,
    "community/hjanuschka-memory-mode"
);
conformance_test!(
    ext_community_hjanuschka_oracle,
    "community/hjanuschka-oracle"
);
conformance_test!(ext_community_mitsuhiko_answer, "community/mitsuhiko-answer");
conformance_test!(
    ext_community_tmustier_arcade_mario_not,
    "community/tmustier-arcade-mario-not"
);
conformance_test!(
    ext_community_tmustier_arcade_picman,
    "community/tmustier-arcade-picman"
);
conformance_test!(
    ext_community_tmustier_arcade_ping,
    "community/tmustier-arcade-ping"
);
conformance_test!(
    ext_community_tmustier_arcade_spice_invaders,
    "community/tmustier-arcade-spice-invaders"
);
conformance_test!(
    ext_community_tmustier_arcade_tetris,
    "community/tmustier-arcade-tetris"
);
conformance_test!(
    ext_community_tmustier_tab_status,
    "community/tmustier-tab-status"
);
conformance_test!(
    ext_community_tmustier_usage_extension,
    "community/tmustier-usage-extension"
);
conformance_test!(ext_custom_footer, "custom-footer");
conformance_test!(ext_handoff, "handoff");
conformance_test!(ext_hello, "hello");
conformance_test!(ext_message_renderer, "message-renderer");
conformance_test!(
    ext_npm_benvargas_pi_ancestor_discovery,
    "npm/benvargas-pi-ancestor-discovery"
);
conformance_test!(
    ext_npm_benvargas_pi_antigravity_image_gen,
    "npm/benvargas-pi-antigravity-image-gen"
);
conformance_test!(ext_npm_pi_command_center, "npm/pi-command-center");
conformance_test!(ext_npm_pi_model_switch, "npm/pi-model-switch");
conformance_test!(ext_npm_pi_threads, "npm/pi-threads");
conformance_test!(ext_overlay_test, "overlay-test");
conformance_test!(ext_qna, "qna");
conformance_test!(ext_question, "question");
conformance_test!(ext_questionnaire, "questionnaire");
conformance_test!(ext_send_user_message, "send-user-message");
conformance_test!(ext_session_name, "session-name");
conformance_test!(ext_snake, "snake");
conformance_test!(ext_space_invaders, "space-invaders");
conformance_test!(ext_summarize, "summarize");
conformance_test!(
    ext_third_party_rytswd_questionnaire,
    "third-party/rytswd-questionnaire"
);
conformance_test!(ext_timed_confirm, "timed-confirm");

// ─── Tier 2 — Single-file with events (83 extensions) ──────────────────────

conformance_test!(ext_auto_commit_on_exit, "auto-commit-on-exit");
conformance_test!(ext_claude_rules, "claude-rules");
conformance_test!(
    ext_community_ferologics_notify,
    "community/ferologics-notify"
);
conformance_test!(
    ext_community_hjanuschka_funny_working_message,
    "community/hjanuschka-funny-working-message"
);
conformance_test!(ext_community_hjanuschka_loop, "community/hjanuschka-loop");
conformance_test!(
    ext_community_hjanuschka_plan_mode,
    "community/hjanuschka-plan-mode"
);
conformance_test!(
    ext_community_hjanuschka_resistance,
    "community/hjanuschka-resistance"
);
conformance_test!(
    ext_community_hjanuschka_speedreading,
    "community/hjanuschka-speedreading"
);
conformance_test!(
    ext_community_hjanuschka_status_widget,
    "community/hjanuschka-status-widget"
);
conformance_test!(
    ext_community_hjanuschka_ultrathink,
    "community/hjanuschka-ultrathink"
);
conformance_test!(
    ext_community_hjanuschka_usage_bar,
    "community/hjanuschka-usage-bar"
);
conformance_test!(
    ext_community_mitsuhiko_cwd_history,
    "community/mitsuhiko-cwd-history"
);
conformance_test!(ext_community_mitsuhiko_files, "community/mitsuhiko-files");
conformance_test!(ext_community_mitsuhiko_loop, "community/mitsuhiko-loop");
conformance_test!(ext_community_mitsuhiko_notify, "community/mitsuhiko-notify");
conformance_test!(ext_community_mitsuhiko_review, "community/mitsuhiko-review");
conformance_test!(ext_community_mitsuhiko_todos, "community/mitsuhiko-todos");
conformance_test!(ext_community_mitsuhiko_uv, "community/mitsuhiko-uv");
conformance_test!(
    ext_community_mitsuhiko_whimsical,
    "community/mitsuhiko-whimsical"
);
conformance_test!(
    ext_community_nicobailon_rewind_hook,
    "community/nicobailon-rewind-hook"
);
conformance_test!(
    ext_community_ogulcancelik_ghostty_theme_sync,
    "community/ogulcancelik-ghostty-theme-sync"
);
conformance_test!(
    ext_community_prateekmedia_token_rate,
    "community/prateekmedia-token-rate"
);
conformance_test!(
    ext_community_qualisero_background_notify,
    "community/qualisero-background-notify",
    ignore
);
conformance_test!(
    ext_community_qualisero_compact_config,
    "community/qualisero-compact-config"
);
conformance_test!(
    ext_community_qualisero_safe_git,
    "community/qualisero-safe-git",
    ignore
);
conformance_test!(
    ext_community_qualisero_safe_rm,
    "community/qualisero-safe-rm"
);
conformance_test!(
    ext_community_qualisero_session_color,
    "community/qualisero-session-color"
);
conformance_test!(
    ext_community_qualisero_session_emoji,
    "community/qualisero-session-emoji"
);
conformance_test!(
    ext_community_tmustier_agent_guidance,
    "community/tmustier-agent-guidance"
);
conformance_test!(
    ext_community_tmustier_ralph_wiggum,
    "community/tmustier-ralph-wiggum"
);
conformance_test!(
    ext_community_tmustier_raw_paste,
    "community/tmustier-raw-paste"
);
conformance_test!(ext_confirm_destructive, "confirm-destructive");
conformance_test!(ext_custom_compaction, "custom-compaction");
conformance_test!(ext_custom_header, "custom-header");
conformance_test!(ext_dirty_repo_guard, "dirty-repo-guard");
conformance_test!(ext_dynamic_resources, "dynamic-resources");
conformance_test!(ext_event_bus, "event-bus");
conformance_test!(ext_file_trigger, "file-trigger");
conformance_test!(ext_git_checkpoint, "git-checkpoint");
conformance_test!(ext_inline_bash, "inline-bash");
conformance_test!(ext_input_transform, "input-transform");
conformance_test!(ext_interactive_shell, "interactive-shell");
conformance_test!(ext_mac_system_theme, "mac-system-theme");
conformance_test!(ext_modal_editor, "modal-editor");
conformance_test!(ext_model_status, "model-status");
conformance_test!(ext_notify, "notify");
conformance_test!(
    ext_npm_ogulcancelik_pi_sketch,
    "npm/ogulcancelik-pi-sketch",
    ignore
);
conformance_test!(ext_npm_pi_ephemeral, "npm/pi-ephemeral");
conformance_test!(ext_npm_pi_ghostty_theme_sync, "npm/pi-ghostty-theme-sync");
conformance_test!(ext_npm_pi_md_export, "npm/pi-md-export");
conformance_test!(ext_npm_pi_notify, "npm/pi-notify");
conformance_test!(ext_npm_pi_poly_notify, "npm/pi-poly-notify");
conformance_test!(
    ext_npm_pi_prompt_template_model,
    "npm/pi-prompt-template-model"
);
conformance_test!(ext_npm_pi_session_ask, "npm/pi-session-ask");
conformance_test!(ext_npm_pi_skill_palette, "npm/pi-skill-palette");
conformance_test!(ext_npm_pi_voice_of_god, "npm/pi-voice-of-god");
conformance_test!(ext_npm_token_rate_pi, "npm/token-rate-pi");
conformance_test!(ext_npm_vpellegrino_pi_skills, "npm/vpellegrino-pi-skills");
conformance_test!(ext_overlay_qa_tests, "overlay-qa-tests");
conformance_test!(ext_permission_gate, "permission-gate");
conformance_test!(ext_pirate, "pirate");
conformance_test!(ext_preset, "preset");
conformance_test!(ext_protected_paths, "protected-paths");
conformance_test!(ext_rainbow_editor, "rainbow-editor");
conformance_test!(ext_rpc_demo, "rpc-demo");
conformance_test!(ext_shutdown_command, "shutdown-command");
conformance_test!(ext_ssh, "ssh");
conformance_test!(ext_status_line, "status-line");
conformance_test!(ext_system_prompt_header, "system-prompt-header");
conformance_test!(
    ext_third_party_graffioh_pi_screenshots_picker,
    "third-party/graffioh-pi-screenshots-picker"
);
conformance_test!(
    ext_third_party_graffioh_pi_super_curl,
    "third-party/graffioh-pi-super-curl"
);
conformance_test!(
    ext_third_party_jyaunches_pi_canvas,
    "third-party/jyaunches-pi-canvas"
);
conformance_test!(
    ext_third_party_lsj5031_pi_notification_extension,
    "third-party/lsj5031-pi-notification-extension"
);
conformance_test!(
    ext_third_party_ogulcancelik_pi_sketch,
    "third-party/ogulcancelik-pi-sketch",
    ignore
);
conformance_test!(
    ext_third_party_raunovillberg_pi_stuffed,
    "third-party/raunovillberg-pi-stuffed"
);
conformance_test!(ext_third_party_rytswd_direnv, "third-party/rytswd-direnv");
conformance_test!(ext_titlebar_spinner, "titlebar-spinner");
conformance_test!(ext_todo, "todo");
conformance_test!(ext_tool_override, "tool-override");
conformance_test!(ext_tools, "tools");
conformance_test!(ext_trigger_compact, "trigger-compact");
conformance_test!(ext_truncated_tool, "truncated-tool");
conformance_test!(ext_widget_placement, "widget-placement");

// ─── Tier 3 — Multi-file / npm deps (79 extensions) ────────────────────────

conformance_test!(ext_community_jyaunches_canvas, "community/jyaunches-canvas");
conformance_test!(
    ext_community_nicobailon_interactive_shell,
    "community/nicobailon-interactive-shell",
    ignore
);
conformance_test!(
    ext_community_nicobailon_mcp_adapter,
    "community/nicobailon-mcp-adapter"
);
conformance_test!(
    ext_community_nicobailon_powerline_footer,
    "community/nicobailon-powerline-footer"
);
conformance_test!(
    ext_community_nicobailon_subagents,
    "community/nicobailon-subagents"
);
conformance_test!(
    ext_community_prateekmedia_checkpoint,
    "community/prateekmedia-checkpoint"
);
conformance_test!(ext_community_prateekmedia_lsp, "community/prateekmedia-lsp",);
conformance_test!(
    ext_community_prateekmedia_permission,
    "community/prateekmedia-permission"
);
conformance_test!(
    ext_community_prateekmedia_ralph_loop,
    "community/prateekmedia-ralph-loop"
);
conformance_test!(
    ext_community_prateekmedia_repeat,
    "community/prateekmedia-repeat"
);
conformance_test!(
    ext_community_qualisero_pi_agent_scip,
    "community/qualisero-pi-agent-scip",
    ignore
);
conformance_test!(
    ext_community_tmustier_code_actions,
    "community/tmustier-code-actions"
);
conformance_test!(
    ext_community_tmustier_files_widget,
    "community/tmustier-files-widget"
);
conformance_test!(ext_custom_provider_anthropic, "custom-provider-anthropic");
conformance_test!(ext_custom_provider_gitlab_duo, "custom-provider-gitlab-duo");
conformance_test!(ext_custom_provider_qwen_cli, "custom-provider-qwen-cli");
conformance_test!(ext_doom_overlay, "doom-overlay");
conformance_test!(ext_npm_aliou_pi_extension_dev, "npm/aliou-pi-extension-dev");
conformance_test!(
    ext_npm_aliou_pi_guardrails,
    "npm/aliou-pi-guardrails",
    ignore
);
conformance_test!(ext_npm_aliou_pi_linkup, "npm/aliou-pi-linkup");
conformance_test!(ext_npm_aliou_pi_processes, "npm/aliou-pi-processes", ignore);
conformance_test!(ext_npm_aliou_pi_synthetic, "npm/aliou-pi-synthetic");
conformance_test!(ext_npm_aliou_pi_toolchain, "npm/aliou-pi-toolchain", ignore);
conformance_test!(
    ext_npm_benvargas_pi_synthetic_provider,
    "npm/benvargas-pi-synthetic-provider"
);
conformance_test!(ext_npm_checkpoint_pi, "npm/checkpoint-pi");
conformance_test!(
    ext_npm_imsus_pi_extension_minimax_coding_plan_mcp,
    "npm/imsus-pi-extension-minimax-coding-plan-mcp"
);
conformance_test!(
    ext_npm_juanibiapina_pi_extension_settings,
    "npm/juanibiapina-pi-extension-settings"
);
conformance_test!(
    ext_npm_juanibiapina_pi_files,
    "npm/juanibiapina-pi-files",
    ignore
);
conformance_test!(ext_npm_juanibiapina_pi_gob, "npm/juanibiapina-pi-gob");
conformance_test!(ext_npm_lsp_pi, "npm/lsp-pi");
conformance_test!(
    ext_npm_marckrenn_pi_sub_bar,
    "npm/marckrenn-pi-sub-bar",
    ignore
);
conformance_test!(
    ext_npm_marckrenn_pi_sub_core,
    "npm/marckrenn-pi-sub-core",
    ignore
);
conformance_test!(ext_npm_permission_pi, "npm/permission-pi");
conformance_test!(ext_npm_pi_agentic_compaction, "npm/pi-agentic-compaction");
conformance_test!(ext_npm_pi_amplike, "npm/pi-amplike");
conformance_test!(ext_npm_pi_bash_confirm, "npm/pi-bash-confirm");
conformance_test!(ext_npm_pi_brave_search, "npm/pi-brave-search", ignore);
conformance_test!(ext_npm_pi_mermaid, "npm/pi-mermaid", ignore);
conformance_test!(ext_npm_pi_messenger, "npm/pi-messenger");
conformance_test!(ext_npm_pi_moonshot, "npm/pi-moonshot");
conformance_test!(ext_npm_pi_multicodex, "npm/pi-multicodex");
conformance_test!(ext_npm_pi_repoprompt_mcp, "npm/pi-repoprompt-mcp");
conformance_test!(ext_npm_pi_review_loop, "npm/pi-review-loop");
conformance_test!(ext_npm_pi_screenshots_picker, "npm/pi-screenshots-picker");
conformance_test!(ext_npm_pi_search_agent, "npm/pi-search-agent", ignore);
conformance_test!(ext_npm_pi_shadow_git, "npm/pi-shadow-git");
conformance_test!(ext_npm_pi_shell_completions, "npm/pi-shell-completions");
conformance_test!(ext_npm_pi_subdir_context, "npm/pi-subdir-context");
conformance_test!(ext_npm_pi_super_curl, "npm/pi-super-curl");
conformance_test!(ext_npm_pi_telemetry_otel, "npm/pi-telemetry-otel", ignore);
conformance_test!(ext_npm_pi_wakatime, "npm/pi-wakatime", ignore);
conformance_test!(ext_npm_pi_watch, "npm/pi-watch", ignore);
conformance_test!(ext_npm_pi_web_access, "npm/pi-web-access", ignore);
conformance_test!(ext_npm_ralph_loop_pi, "npm/ralph-loop-pi");
conformance_test!(ext_npm_repeat_pi, "npm/repeat-pi");
conformance_test!(ext_npm_vaayne_agent_kit, "npm/vaayne-agent-kit");
conformance_test!(ext_npm_vaayne_pi_mcp, "npm/vaayne-pi-mcp");
conformance_test!(ext_npm_vaayne_pi_subagent, "npm/vaayne-pi-subagent");
conformance_test!(
    ext_npm_vaayne_pi_web_tools,
    "npm/vaayne-pi-web-tools",
    ignore
);
conformance_test!(ext_npm_walterra_pi_charts, "npm/walterra-pi-charts");
conformance_test!(ext_npm_walterra_pi_graphviz, "npm/walterra-pi-graphviz");
conformance_test!(ext_npm_zenobius_pi_dcp, "npm/zenobius-pi-dcp");
conformance_test!(ext_plan_mode, "plan-mode");
conformance_test!(ext_sandbox, "sandbox");
conformance_test!(ext_subagent, "subagent");
conformance_test!(
    ext_third_party_aliou_pi_extensions,
    "third-party/aliou-pi-extensions",
);
conformance_test!(
    ext_third_party_ben_vargas_pi_packages,
    "third-party/ben-vargas-pi-packages",
);
conformance_test!(
    ext_third_party_charles_cooper_pi_extensions,
    "third-party/charles-cooper-pi-extensions",
);
conformance_test!(
    ext_third_party_cv_pi_ssh_remote,
    "third-party/cv-pi-ssh-remote"
);
conformance_test!(
    ext_third_party_limouren_agent_things,
    "third-party/limouren-agent-things"
);
conformance_test!(
    ext_third_party_marckrenn_pi_sub,
    "third-party/marckrenn-pi-sub",
    ignore
);
conformance_test!(
    ext_third_party_michalvavra_agents,
    "third-party/michalvavra-agents"
);
conformance_test!(
    ext_third_party_openclaw_openclaw,
    "third-party/openclaw-openclaw",
);
conformance_test!(
    ext_third_party_pasky_pi_amplike,
    "third-party/pasky-pi-amplike",
);
conformance_test!(
    ext_third_party_qualisero_pi_agent_scip,
    "third-party/qualisero-pi-agent-scip",
    ignore
);
conformance_test!(
    ext_third_party_rytswd_slow_mode,
    "third-party/rytswd-slow-mode"
);
conformance_test!(
    ext_third_party_w_winter_dot314,
    "third-party/w-winter-dot314",
);
conformance_test!(
    ext_third_party_zenobi_us_pi_dcp,
    "third-party/zenobi-us-pi-dcp"
);
conformance_test!(ext_with_deps, "with-deps");

// ─── Tier 4 — UI-heavy extensions (2 extensions) ───────────────────────────

conformance_test!(
    ext_community_nicobailon_interview_tool,
    "community/nicobailon-interview-tool",
    ignore
);
conformance_test!(
    ext_third_party_vtemian_pi_config,
    "third-party/vtemian-pi-config"
);

// ─── Tier 5 — Platform-specific (4 extensions) ─────────────────────────────

conformance_test!(
    ext_agents_mikeastock_extensions,
    "agents-mikeastock/extensions",
);
conformance_test!(
    ext_community_mitsuhiko_control,
    "community/mitsuhiko-control"
);
conformance_test!(ext_npm_pi_annotate, "npm/pi-annotate");
conformance_test!(
    ext_third_party_kcosr_pi_extensions,
    "third-party/kcosr-pi-extensions",
    ignore
);
