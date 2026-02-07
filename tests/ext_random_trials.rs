#![cfg(feature = "ext-conformance")]
//! Random extension trials harness (bd-2fps.1.2).
//!
//! Selects a deterministic random subset from the Rust-N/A pool and runs
//! them through the conformance harness. Results are written as JSONL logs
//! and a summary manifest.
//!
//! # Environment Variables
//! - `PI_EXT_RANDOM_SEED`   — u64 seed for deterministic selection (default: 42)
//! - `PI_EXT_RANDOM_N`      — sample size (default: 20)
//! - `PI_EXT_RANDOM_FILTER` — optional `tier:1-3` or `source:community` filter
//! - `PI_EXT_RANDOM_IDS`    — optional comma-separated explicit ID list (bypasses selector)
//!
//! # Run
//! ```sh
//! PI_EXT_RANDOM_SEED=42 PI_EXT_RANDOM_N=20 \
//!   cargo test --features ext-conformance --test ext_random_trials -- --include-ignored --nocapture
//! ```
#![allow(clippy::too_many_lines, clippy::needless_raw_string_hashes)]

mod common;

use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

// ===========================================================================
// SplitMix64 PRNG (same as ext_conformance_selector.rs)
// ===========================================================================

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn next_bounded(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        if n.is_power_of_two() {
            return self.next_u64() & (n - 1);
        }
        let threshold = n.wrapping_neg() % n;
        loop {
            let r = self.next_u64();
            if r >= threshold {
                return r % n;
            }
        }
    }
}

// ===========================================================================
// Manifest types
// ===========================================================================

#[derive(Debug, Clone)]
struct ManifestEntry {
    id: String,
    entry_path: String,
    source_tier: String,
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

struct Manifest {
    extensions: Vec<ManifestEntry>,
}

impl Manifest {
    fn find(&self, ext_id: &str) -> Option<&ManifestEntry> {
        self.extensions.iter().find(|e| e.id == ext_id)
    }
}

// ===========================================================================
// Paths
// ===========================================================================

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

fn events_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("ext_conformance")
        .join("reports")
        .join("conformance_events.jsonl")
}

fn output_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("ext_conformance")
        .join("logs")
        .join("random_trials")
}

// ===========================================================================
// Manifest loading
// ===========================================================================

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
                    source_tier: e["source_tier"].as_str().unwrap_or("").to_string(),
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

// ===========================================================================
// N/A pool identification
// ===========================================================================

fn load_na_pool() -> Vec<String> {
    let ep = events_path();
    if !ep.exists() {
        // No conformance events yet → entire manifest is N/A
        let manifest = load_manifest();
        return manifest.extensions.iter().map(|e| e.id.clone()).collect();
    }

    let data = std::fs::read_to_string(&ep).expect("read conformance_events.jsonl");
    let mut passed: HashSet<String> = HashSet::new();
    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(evt) = serde_json::from_str::<Value>(line) {
            if evt["overall_status"].as_str() != Some("N/A") {
                if let Some(id) = evt["extension_id"].as_str() {
                    passed.insert(id.to_string());
                }
            }
        }
    }

    let manifest = load_manifest();
    let mut na: Vec<String> = manifest
        .extensions
        .iter()
        .filter(|e| !passed.contains(&e.id))
        .map(|e| e.id.clone())
        .collect();
    na.sort();
    na
}

// ===========================================================================
// Selector
// ===========================================================================

#[derive(Default)]
struct SelectionFilter {
    tier_range: Option<(u32, u32)>,
    source_category: Option<String>,
}

fn select_extensions(
    pool: &[String],
    seed: u64,
    sample_size: usize,
    filter: &SelectionFilter,
) -> Vec<String> {
    let manifest = load_manifest();

    // Apply filter
    let filtered: Vec<&String> = pool
        .iter()
        .filter(|id| {
            if let Some(entry) = manifest.find(id) {
                if let Some((min, max)) = filter.tier_range {
                    if entry.conformance_tier < min || entry.conformance_tier > max {
                        return false;
                    }
                }
                if let Some(ref cat) = filter.source_category {
                    if &entry.source_tier != cat {
                        return false;
                    }
                }
                true
            } else {
                false
            }
        })
        .collect();

    if filtered.is_empty() {
        return vec![];
    }

    let n = filtered.len();
    let actual_size = sample_size.min(n);

    // Fisher-Yates partial shuffle
    let mut indices: Vec<usize> = (0..n).collect();
    let mut rng = SplitMix64::new(seed);
    for i in 0..actual_size {
        let j = i + rng.next_bounded((n - i) as u64) as usize;
        indices.swap(i, j);
    }

    indices[..actual_size]
        .iter()
        .map(|&idx| filtered[idx].clone())
        .collect()
}

// ===========================================================================
// Failure classification
// ===========================================================================

fn classify_failure(reason: &str) -> &'static str {
    if reason.contains("Unsupported module specifier") || reason.contains("Error resolving module")
    {
        "resolver"
    } else if reason.contains("ENOENT") || reason.contains("readFileSync") {
        "shim/fs"
    } else if reason.contains("not available in PiJS") {
        "shim/missing"
    } else if reason.contains("Missing command") || reason.contains("Missing flag") {
        "registration"
    } else if reason.contains("expects tools") || reason.contains("expects providers") {
        "registration"
    } else if reason.contains("Runtime start error") || reason.contains("Load spec error") {
        "runtime"
    } else if reason.contains("Load error") {
        "load"
    } else {
        "unknown"
    }
}

// ===========================================================================
// Conformance runner (simplified from ext_conformance_generated.rs)
// ===========================================================================

#[derive(serde::Serialize)]
struct TrialResult {
    id: String,
    source_tier: String,
    conformance_tier: u32,
    entry_path: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_class: Option<String>,
    commands_registered: usize,
    flags_registered: usize,
    tools_registered: usize,
    providers_registered: usize,
    duration_ms: u64,
}

#[allow(clippy::cast_possible_truncation)]
fn run_trial(ext_id: &str) -> TrialResult {
    let manifest = load_manifest();
    let Some(entry) = manifest.find(ext_id) else {
        return TrialResult {
            id: ext_id.to_string(),
            source_tier: String::new(),
            conformance_tier: 0,
            entry_path: String::new(),
            status: "skip".to_string(),
            failure_reason: Some("Not found in VALIDATED_MANIFEST.json".to_string()),
            failure_class: None,
            commands_registered: 0,
            flags_registered: 0,
            tools_registered: 0,
            providers_registered: 0,
            duration_ms: 0,
        };
    };

    let start = std::time::Instant::now();
    let cwd = std::env::temp_dir().join(format!("pi-random-trial-{}", ext_id.replace('/', "_")));
    let _ = std::fs::create_dir_all(&cwd);

    let entry_file = artifacts_dir().join(&entry.entry_path);
    if !entry_file.exists() {
        return TrialResult {
            id: ext_id.to_string(),
            source_tier: entry.source_tier.clone(),
            conformance_tier: entry.conformance_tier,
            entry_path: entry.entry_path.clone(),
            status: "skip".to_string(),
            failure_reason: Some(format!("Artifact not found: {}", entry_file.display())),
            failure_class: None,
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
            let reason = format!("Load spec error: {e}");
            return TrialResult {
                id: ext_id.to_string(),
                source_tier: entry.source_tier.clone(),
                conformance_tier: entry.conformance_tier,
                entry_path: entry.entry_path.clone(),
                status: "fail".to_string(),
                failure_class: Some(classify_failure(&reason).to_string()),
                failure_reason: Some(reason),
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
            let reason = format!("Runtime start error: {e}");
            return TrialResult {
                id: ext_id.to_string(),
                source_tier: entry.source_tier.clone(),
                conformance_tier: entry.conformance_tier,
                entry_path: entry.entry_path.clone(),
                status: "fail".to_string(),
                failure_class: Some(classify_failure(&reason).to_string()),
                failure_reason: Some(reason),
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
        let reason = format!("Load error: {e}");
        return TrialResult {
            id: ext_id.to_string(),
            source_tier: entry.source_tier.clone(),
            conformance_tier: entry.conformance_tier,
            entry_path: entry.entry_path.clone(),
            status: "fail".to_string(),
            failure_class: Some(classify_failure(&reason).to_string()),
            failure_reason: Some(reason),
            commands_registered: 0,
            flags_registered: 0,
            tools_registered: 0,
            providers_registered: 0,
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    // Validate registrations
    let actual_commands = manager.list_commands();
    let actual_cmd_names: Vec<&str> = actual_commands
        .iter()
        .filter_map(|v| v.get("name").and_then(Value::as_str))
        .collect();
    for expected_cmd in &entry.registrations.commands {
        if !actual_cmd_names.contains(&expected_cmd.as_str()) {
            let reason = format!("Missing command '{expected_cmd}'. Actual: {actual_cmd_names:?}");
            return TrialResult {
                id: ext_id.to_string(),
                source_tier: entry.source_tier.clone(),
                conformance_tier: entry.conformance_tier,
                entry_path: entry.entry_path.clone(),
                status: "fail".to_string(),
                failure_class: Some(classify_failure(&reason).to_string()),
                failure_reason: Some(reason),
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
            let reason = format!("Missing flag '{expected_flag}'. Actual: {actual_flag_names:?}");
            return TrialResult {
                id: ext_id.to_string(),
                source_tier: entry.source_tier.clone(),
                conformance_tier: entry.conformance_tier,
                entry_path: entry.entry_path.clone(),
                status: "fail".to_string(),
                failure_class: Some(classify_failure(&reason).to_string()),
                failure_reason: Some(reason),
                commands_registered: actual_commands.len(),
                flags_registered: actual_flags.len(),
                tools_registered: manager.extension_tool_defs().len(),
                providers_registered: manager.extension_providers().len(),
                duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    }

    if entry.capabilities.registers_tools && manager.extension_tool_defs().is_empty() {
        let reason = "Manifest expects tools but none registered".to_string();
        return TrialResult {
            id: ext_id.to_string(),
            source_tier: entry.source_tier.clone(),
            conformance_tier: entry.conformance_tier,
            entry_path: entry.entry_path.clone(),
            status: "fail".to_string(),
            failure_class: Some(classify_failure(&reason).to_string()),
            failure_reason: Some(reason),
            commands_registered: actual_commands.len(),
            flags_registered: actual_flags.len(),
            tools_registered: 0,
            providers_registered: manager.extension_providers().len(),
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    if entry.capabilities.registers_providers && manager.extension_providers().is_empty() {
        let reason = "Manifest expects providers but none registered".to_string();
        return TrialResult {
            id: ext_id.to_string(),
            source_tier: entry.source_tier.clone(),
            conformance_tier: entry.conformance_tier,
            entry_path: entry.entry_path.clone(),
            status: "fail".to_string(),
            failure_class: Some(classify_failure(&reason).to_string()),
            failure_reason: Some(reason),
            commands_registered: actual_commands.len(),
            flags_registered: actual_flags.len(),
            tools_registered: manager.extension_tool_defs().len(),
            providers_registered: 0,
            duration_ms: start.elapsed().as_millis() as u64,
        };
    }

    TrialResult {
        id: ext_id.to_string(),
        source_tier: entry.source_tier.clone(),
        conformance_tier: entry.conformance_tier,
        entry_path: entry.entry_path.clone(),
        status: "pass".to_string(),
        failure_reason: None,
        failure_class: None,
        commands_registered: actual_commands.len(),
        flags_registered: actual_flags.len(),
        tools_registered: manager.extension_tool_defs().len(),
        providers_registered: manager.extension_providers().len(),
        duration_ms: start.elapsed().as_millis() as u64,
    }
}

// ===========================================================================
// Parse env-var filter
// ===========================================================================

fn parse_filter(filter_str: &str) -> SelectionFilter {
    let mut filter = SelectionFilter::default();
    for part in filter_str.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(rest) = part.strip_prefix("tier:") {
            if let Some((min_s, max_s)) = rest.split_once('-') {
                if let (Ok(min), Ok(max)) = (min_s.parse::<u32>(), max_s.parse::<u32>()) {
                    filter.tier_range = Some((min, max));
                }
            } else if let Ok(t) = rest.parse::<u32>() {
                filter.tier_range = Some((t, t));
            }
        } else if let Some(rest) = part.strip_prefix("source:") {
            filter.source_category = Some(rest.to_string());
        }
    }
    filter
}

// ===========================================================================
// Main trial runner
// ===========================================================================

#[derive(serde::Serialize)]
struct TrialRun {
    schema: String,
    seed: u64,
    sample_size: usize,
    filter_str: String,
    selected_ids: Vec<String>,
    results: Vec<TrialResult>,
    summary: TrialSummary,
    #[serde(with = "chrono_rfc3339")]
    timestamp: std::time::SystemTime,
}

mod chrono_rfc3339 {
    use serde::Serializer;
    use std::time::SystemTime;

    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn serialize<S: Serializer>(time: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let duration = time
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = duration.as_secs();
        s.serialize_str(&format!("{secs}"))
    }
}

#[derive(serde::Serialize)]
struct TrialSummary {
    total: usize,
    pass: usize,
    fail: usize,
    skip: usize,
    pass_rate_pct: f64,
    failure_classes: std::collections::HashMap<String, usize>,
}

fn run_random_trials() -> TrialRun {
    let seed: u64 = std::env::var("PI_EXT_RANDOM_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(42);
    let sample_size: usize = std::env::var("PI_EXT_RANDOM_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let filter_str = std::env::var("PI_EXT_RANDOM_FILTER").unwrap_or_default();
    let explicit_ids = std::env::var("PI_EXT_RANDOM_IDS").ok();

    let filter = parse_filter(&filter_str);

    // Determine selected extensions
    let selected_ids = if let Some(ids_str) = explicit_ids {
        ids_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        let pool = load_na_pool();
        select_extensions(&pool, seed, sample_size, &filter)
    };

    eprintln!(
        "[random_trials] seed={seed} n={sample_size} filter={filter_str:?} selected={}",
        selected_ids.len()
    );

    // Run each extension
    let mut results = Vec::with_capacity(selected_ids.len());
    for (i, id) in selected_ids.iter().enumerate() {
        eprintln!(
            "[random_trials] [{}/{}] {id} ...",
            i + 1,
            selected_ids.len()
        );
        let result = run_trial(id);
        eprintln!(
            "[random_trials] [{}/{}] {id} → {} ({}ms){}",
            i + 1,
            selected_ids.len(),
            result.status,
            result.duration_ms,
            result
                .failure_reason
                .as_deref()
                .map(|r| format!(" — {}", &r[..r.len().min(100)]))
                .unwrap_or_default()
        );
        results.push(result);
    }

    // Compute summary
    let total = results.len();
    let pass = results.iter().filter(|r| r.status == "pass").count();
    let fail = results.iter().filter(|r| r.status == "fail").count();
    let skip = results.iter().filter(|r| r.status == "skip").count();
    let pass_rate_pct = if total > 0 {
        (pass as f64 / total as f64) * 100.0
    } else {
        0.0
    };

    let mut failure_classes: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for r in &results {
        if let Some(ref cls) = r.failure_class {
            *failure_classes.entry(cls.clone()).or_insert(0) += 1;
        }
    }

    let summary = TrialSummary {
        total,
        pass,
        fail,
        skip,
        pass_rate_pct,
        failure_classes,
    };

    TrialRun {
        schema: "pi.ext.random_trials.v1".to_string(),
        seed,
        sample_size,
        filter_str,
        selected_ids,
        results,
        summary,
        timestamp: std::time::SystemTime::now(),
    }
}

fn write_trial_output(run: &TrialRun) {
    let out_dir = output_dir();
    let _ = std::fs::create_dir_all(&out_dir);

    // Write JSONL events (one per result)
    let jsonl_path = out_dir.join(format!("trial_seed_{}.jsonl", run.seed));
    let mut jsonl_lines = Vec::new();

    // First line: selection metadata
    let selection_event = serde_json::json!({
        "event": "selection",
        "seed": run.seed,
        "sample_size": run.sample_size,
        "filter": run.filter_str,
        "selected_ids": run.selected_ids,
    });
    jsonl_lines.push(serde_json::to_string(&selection_event).unwrap());

    // Per-extension result events
    for result in &run.results {
        jsonl_lines.push(serde_json::to_string(result).unwrap());
    }

    // Summary event
    let summary_event = serde_json::json!({
        "event": "summary",
        "total": run.summary.total,
        "pass": run.summary.pass,
        "fail": run.summary.fail,
        "skip": run.summary.skip,
        "pass_rate_pct": run.summary.pass_rate_pct,
        "failure_classes": run.summary.failure_classes,
    });
    jsonl_lines.push(serde_json::to_string(&summary_event).unwrap());

    std::fs::write(&jsonl_path, jsonl_lines.join("\n") + "\n").expect("write JSONL");
    eprintln!("[random_trials] JSONL written to {}", jsonl_path.display());

    // Write full JSON manifest
    let manifest_path = out_dir.join(format!("trial_seed_{}_manifest.json", run.seed));
    let json = serde_json::to_string_pretty(run).expect("serialize manifest");
    std::fs::write(&manifest_path, json).expect("write manifest");
    eprintln!(
        "[random_trials] Manifest written to {}",
        manifest_path.display()
    );
}

// ===========================================================================
// Test entry point
// ===========================================================================

/// Run a batch of random extension trials.
///
/// This test is `#[ignore]` by default — run with `--include-ignored`.
#[test]
#[ignore]
fn random_trials_batch() {
    let run = run_random_trials();
    write_trial_output(&run);

    eprintln!(
        "\n[random_trials] === SUMMARY ===\n  Total: {}\n  Pass:  {} ({:.1}%)\n  Fail:  {}\n  Skip:  {}",
        run.summary.total,
        run.summary.pass,
        run.summary.pass_rate_pct,
        run.summary.fail,
        run.summary.skip,
    );
    if !run.summary.failure_classes.is_empty() {
        eprintln!("  Failure classes:");
        for (cls, count) in &run.summary.failure_classes {
            eprintln!("    {cls}: {count}");
        }
    }
}

// ===========================================================================
// Unit tests for the harness itself
// ===========================================================================

#[test]
fn parse_filter_tier_range() {
    let f = parse_filter("tier:1-3");
    assert_eq!(f.tier_range, Some((1, 3)));
    assert!(f.source_category.is_none());
}

#[test]
fn parse_filter_single_tier() {
    let f = parse_filter("tier:2");
    assert_eq!(f.tier_range, Some((2, 2)));
}

#[test]
fn parse_filter_source() {
    let f = parse_filter("source:community");
    assert!(f.tier_range.is_none());
    assert_eq!(f.source_category.as_deref(), Some("community"));
}

#[test]
fn parse_filter_combined() {
    let f = parse_filter("tier:1-3,source:npm-registry");
    assert_eq!(f.tier_range, Some((1, 3)));
    assert_eq!(f.source_category.as_deref(), Some("npm-registry"));
}

#[test]
fn parse_filter_empty() {
    let f = parse_filter("");
    assert!(f.tier_range.is_none());
    assert!(f.source_category.is_none());
}

#[test]
fn classify_failure_categories() {
    assert_eq!(
        classify_failure("Unsupported module specifier: node-pty"),
        "resolver"
    );
    assert_eq!(classify_failure("Error resolving module 'foo'"), "resolver");
    assert_eq!(
        classify_failure("ENOENT: no such file, open '/x'"),
        "shim/fs"
    );
    assert_eq!(classify_failure("readFileSync failed"), "shim/fs");
    assert_eq!(
        classify_failure("node:http.request is not available in PiJS"),
        "shim/missing"
    );
    assert_eq!(
        classify_failure("Missing command 'foo'. Actual: []"),
        "registration"
    );
    assert_eq!(
        classify_failure("Missing flag 'bar'. Actual: []"),
        "registration"
    );
    assert_eq!(
        classify_failure("Manifest expects tools but none registered"),
        "registration"
    );
    assert_eq!(classify_failure("Runtime start error: oops"), "runtime");
    assert_eq!(
        classify_failure("Load error: Extension error: kaboom"),
        "load"
    );
    assert_eq!(classify_failure("something else entirely"), "unknown");
}

#[test]
fn selector_integration_with_manifest() {
    // Verify that the N/A pool can be loaded and selected from
    if !manifest_path().exists() || !events_path().exists() {
        eprintln!("Skipping: conformance files not found");
        return;
    }

    let pool = load_na_pool();
    assert!(!pool.is_empty(), "N/A pool should not be empty");

    // Default selection
    let selected = select_extensions(&pool, 42, 5, &SelectionFilter::default());
    assert_eq!(selected.len(), 5);

    // Same seed → same result
    let selected2 = select_extensions(&pool, 42, 5, &SelectionFilter::default());
    assert_eq!(selected, selected2);

    // Community filter
    let community = select_extensions(
        &pool,
        42,
        5,
        &SelectionFilter {
            source_category: Some("community".to_string()),
            ..Default::default()
        },
    );
    let manifest = load_manifest();
    for id in &community {
        let entry = manifest.find(id).expect("ID in manifest");
        assert_eq!(entry.source_tier, "community");
    }
}

/// Quick smoke test: run a single known extension (if available).
#[test]
fn trial_smoke_single_extension() {
    if !manifest_path().exists() {
        eprintln!("Skipping: manifest not found");
        return;
    }

    let pool = load_na_pool();
    if pool.is_empty() {
        eprintln!("Skipping: empty N/A pool");
        return;
    }

    // Pick the first tier-1 community extension for a quick smoke test
    let manifest = load_manifest();
    let target = pool.iter().find(|id| {
        manifest
            .find(id)
            .map(|e| e.conformance_tier == 1 && e.source_tier == "community")
            .unwrap_or(false)
    });

    if let Some(id) = target {
        let result = run_trial(id);
        eprintln!(
            "Smoke test: {id} → {} ({}ms)",
            result.status, result.duration_ms
        );
        // We don't assert pass here — the extension might legitimately fail.
        // We just verify the harness doesn't panic.
        assert!(
            result.status == "pass" || result.status == "fail",
            "status should be pass or fail, got: {}",
            result.status
        );
    } else {
        eprintln!("No tier-1 community extension in N/A pool");
    }
}
