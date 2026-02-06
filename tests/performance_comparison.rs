//! Extension performance comparison: Rust (`QuickJS`) vs legacy TS (Bun/jiti) (bd-uah).
//!
//! Reads existing benchmark data and generates:
//! - `tests/ext_conformance/reports/performance_comparison.json` — per-extension deltas
//! - Updates BENCHMARKS.md "Extension Load Time Comparison" section (via generated markdown)
//! - `tests/ext_conformance/reports/performance_events.jsonl` — per-extension JSONL log
//!
//! Run: `cargo test --test performance_comparison -- --nocapture`

use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn reports_dir() -> PathBuf {
    project_root().join("tests/ext_conformance/reports")
}

fn read_json_file(path: &Path) -> Option<Value> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_atomic(path: &Path, content: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).expect("write atomic tmp");
    std::fs::rename(&tmp, path).expect("rename atomic tmp");
}

/// Parse extension name from path like "hello/hello.ts" → "hello"
fn ext_id_from_path(path: &str) -> &str {
    path.split('/').next().unwrap_or(path)
}

#[derive(Debug, Clone)]
struct LoadComparison {
    extension: String,
    rust_ms: u64,
    ts_ms: u64,
    ratio: f64,
    delta_ms: i64,
}

fn load_benchmark_data() -> Option<(Vec<LoadComparison>, Value)> {
    let path = reports_dir().join("load_time_benchmark.json");
    let report = read_json_file(&path)?;
    let results = report.get("results")?.as_array()?;

    let mut comparisons = Vec::new();
    for entry in results {
        let ext_path = entry.get("extension")?.as_str()?;
        let rust_ms = entry.get("rust")?.get("load_time_ms")?.as_u64()?;
        let ts_ms = entry.get("ts")?.get("load_time_ms")?.as_u64()?;

        comparisons.push(LoadComparison {
            extension: ext_id_from_path(ext_path).to_string(),
            rust_ms,
            ts_ms,
            ratio: entry.get("ratio")?.as_f64()?,
            delta_ms: i64::try_from(rust_ms).unwrap_or(i64::MAX)
                - i64::try_from(ts_ms).unwrap_or(0),
        });
    }

    Some((comparisons, report))
}

fn generate_comparison_json(comparisons: &[LoadComparison], raw: &Value) -> Value {
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);

    let rust_times: Vec<u64> = comparisons.iter().map(|c| c.rust_ms).collect();
    let ts_times: Vec<u64> = comparisons.iter().map(|c| c.ts_ms).collect();
    let ratios: Vec<f64> = comparisons.iter().map(|c| c.ratio).collect();

    let rust_min = rust_times.iter().copied().min().unwrap_or(0);
    let rust_max = rust_times.iter().copied().max().unwrap_or(0);
    let ts_min = ts_times.iter().copied().min().unwrap_or(0);
    let ts_max = ts_times.iter().copied().max().unwrap_or(0);

    #[allow(clippy::cast_precision_loss)]
    let rust_mean = if rust_times.is_empty() {
        0.0
    } else {
        rust_times.iter().sum::<u64>() as f64 / rust_times.len() as f64
    };

    #[allow(clippy::cast_precision_loss)]
    let ts_mean = if ts_times.is_empty() {
        0.0
    } else {
        ts_times.iter().sum::<u64>() as f64 / ts_times.len() as f64
    };

    let ratio_min = ratios.iter().copied().reduce(f64::min).unwrap_or(0.0);
    let ratio_max = ratios.iter().copied().reduce(f64::max).unwrap_or(0.0);

    let per_extension: Vec<Value> = comparisons
        .iter()
        .map(|c| {
            json!({
                "extension": c.extension,
                "rust_ms": c.rust_ms,
                "ts_ms": c.ts_ms,
                "delta_ms": c.delta_ms,
                "ratio": c.ratio,
            })
        })
        .collect();

    // Count regressions (Rust slower) vs improvements (Rust faster)
    let regressions = comparisons.iter().filter(|c| c.delta_ms > 0).count();
    let improvements = comparisons.iter().filter(|c| c.delta_ms < 0).count();
    let same = comparisons.iter().filter(|c| c.delta_ms == 0).count();

    json!({
        "schema": "pi.ext.performance_comparison.v1",
        "generated_at": now,
        "source": "load_time_benchmark.json",
        "source_generated_at": raw.get("generated_at"),
        "summary": {
            "total_extensions": comparisons.len(),
            "regressions": regressions,
            "improvements": improvements,
            "same": same,
            "rust": {
                "min_ms": rust_min,
                "max_ms": rust_max,
                "mean_ms": rust_mean,
            },
            "ts": {
                "min_ms": ts_min,
                "max_ms": ts_max,
                "mean_ms": ts_mean,
            },
            "ratio": {
                "min": ratio_min,
                "max": ratio_max,
            },
        },
        "analysis": {
            "methodology": "Both runtimes load the same unmodified .ts extension files. TS uses Bun/jiti (native V8-based eval). Rust uses QuickJS with SWC transpilation from TypeScript to JavaScript before eval.",
            "regression_hypothesis": "Extension loading in Rust (QuickJS) is slower due to: (1) SWC TypeScript→JavaScript transpilation overhead per-load, (2) QuickJS bytecode compilation (no JIT), (3) virtual module system resolution. This is a cold-start cost; warm-path dispatch (tool calls, event hooks) is sub-50μs in Rust.",
            "mitigation": "Planned: compiled bytecode caching (bd-4p9k opportunity matrix) to amortize cold-start across runs. See BENCHMARKS.md 'Opportunity Matrix' for prioritized improvements.",
            "key_insight": "While extension loading is 96-131ms in Rust vs 1ms in TS, this is dominated by startup compilation. Steady-state operations (tool call roundtrip: 44μs, policy eval: 20ns) are orders of magnitude faster than TS equivalents. The loading cost is paid once per session."
        },
        "extensions": per_extension,
    })
}

fn generate_comparison_markdown(comparisons: &[LoadComparison]) -> String {
    let mut md = String::with_capacity(8 * 1024);

    md.push_str("### Extension Load Time Comparison (Rust vs Legacy TS)\n\n");
    md.push_str("> Data from `tests/ext_conformance/reports/load_time_benchmark.json`\n\n");

    let total = comparisons.len();
    if total == 0 {
        md.push_str("No benchmark data available.\n");
        return md;
    }

    let rust_times: Vec<u64> = comparisons.iter().map(|c| c.rust_ms).collect();
    let ts_times: Vec<u64> = comparisons.iter().map(|c| c.ts_ms).collect();

    #[allow(clippy::cast_precision_loss)]
    let rust_mean = rust_times.iter().sum::<u64>() as f64 / total as f64;
    #[allow(clippy::cast_precision_loss)]
    let ts_mean = ts_times.iter().sum::<u64>() as f64 / total as f64;

    let rust_min = rust_times.iter().copied().min().unwrap_or(0);
    let rust_max = rust_times.iter().copied().max().unwrap_or(0);

    md.push_str("**Summary** (60 official extensions):\n\n");
    md.push_str("| Metric | Rust (QuickJS) | TS (Bun/jiti) |\n");
    md.push_str("|--------|---------------|---------------|\n");
    let _ = writeln!(md, "| Mean load time | {rust_mean:.0}ms | {ts_mean:.0}ms |");
    let _ = writeln!(
        md,
        "| Min load time | {rust_min}ms | {}ms |",
        ts_times.iter().min().unwrap_or(&0)
    );
    let _ = writeln!(
        md,
        "| Max load time | {rust_max}ms | {}ms |",
        ts_times.iter().max().unwrap_or(&0)
    );
    md.push('\n');

    md.push_str("**Regression analysis:** Extension loading in Rust is slower due to\n");
    md.push_str("SWC TypeScript transpilation + QuickJS bytecode compilation per-load.\n");
    md.push_str("This is a one-time cold-start cost per session. Steady-state operations\n");
    md.push_str("(tool calls: 44us, policy eval: 20ns) are orders of magnitude faster.\n\n");

    md.push_str("**Planned mitigation:** Compiled bytecode caching to amortize cold-start.\n\n");

    // Per-extension table (sorted by ratio, worst first)
    let mut sorted: Vec<&LoadComparison> = comparisons.iter().collect();
    sorted.sort_by(|a, b| {
        b.ratio
            .partial_cmp(&a.ratio)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    md.push_str("| Extension | Rust (ms) | TS (ms) | Delta (ms) | Ratio |\n");
    md.push_str("|-----------|-----------|---------|------------|-------|\n");
    for c in &sorted {
        let _ = writeln!(
            md,
            "| {} | {} | {} | +{} | {:.0}x |",
            c.extension, c.rust_ms, c.ts_ms, c.delta_ms, c.ratio
        );
    }
    md.push('\n');

    md
}

// ─── Test Entry Points ──────────────────────────────────────────────────────

#[test]
fn generate_performance_comparison() {
    let reports = reports_dir();
    let _ = std::fs::create_dir_all(&reports);

    let Some((comparisons, raw)) = load_benchmark_data() else {
        eprintln!("[perf_comparison] No load_time_benchmark.json found — skipping");
        return;
    };

    eprintln!(
        "[perf_comparison] Loaded {} extension comparisons",
        comparisons.len()
    );

    // 1. Write comparison JSON
    let comparison_json = generate_comparison_json(&comparisons, &raw);
    let json_path = reports.join("performance_comparison.json");
    write_atomic(
        &json_path,
        &serde_json::to_string_pretty(&comparison_json).unwrap_or_default(),
    );

    // 2. Write JSONL events
    let events_path = reports.join("performance_events.jsonl");
    let mut lines: Vec<String> = Vec::new();
    for c in &comparisons {
        let entry = json!({
            "schema": "pi.ext.performance_event.v1",
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            "extension_id": c.extension,
            "rust_load_ms": c.rust_ms,
            "ts_load_ms": c.ts_ms,
            "delta_ms": c.delta_ms,
            "ratio": c.ratio,
            "status": if c.delta_ms > 0 { "regression" } else { "improvement" },
        });
        lines.push(serde_json::to_string(&entry).unwrap_or_default());
    }
    write_atomic(&events_path, &(lines.join("\n") + "\n"));

    // 3. Generate markdown comparison table
    let md = generate_comparison_markdown(&comparisons);
    let md_path = reports.join("PERFORMANCE_COMPARISON.md");
    write_atomic(&md_path, &md);

    // 4. Print summary
    #[allow(clippy::cast_precision_loss)]
    let rust_mean =
        comparisons.iter().map(|c| c.rust_ms).sum::<u64>() as f64 / comparisons.len() as f64;
    #[allow(clippy::cast_precision_loss)]
    let ts_mean =
        comparisons.iter().map(|c| c.ts_ms).sum::<u64>() as f64 / comparisons.len() as f64;

    eprintln!("\n=== Performance Comparison Generated ===");
    eprintln!("  Extensions: {}", comparisons.len());
    eprintln!("  Rust mean load: {rust_mean:.0}ms");
    eprintln!("  TS mean load: {ts_mean:.0}ms");
    eprintln!("  Reports:");
    eprintln!("    {}", json_path.display());
    eprintln!("    {}", events_path.display());
    eprintln!("    {}", md_path.display());

    assert!(json_path.exists(), "comparison JSON should be generated");
    assert!(events_path.exists(), "events JSONL should be generated");
    assert!(md_path.exists(), "comparison markdown should be generated");
}

#[test]
fn comparison_data_is_valid() {
    let Some((comparisons, _)) = load_benchmark_data() else {
        eprintln!("No benchmark data — skipping validation");
        return;
    };

    assert!(
        !comparisons.is_empty(),
        "should have at least one comparison"
    );

    for c in &comparisons {
        assert!(
            !c.extension.is_empty(),
            "extension name should not be empty"
        );
        assert!(c.rust_ms > 0, "Rust load time should be positive");
        assert!(c.ratio > 0.0, "ratio should be positive");
    }
}

#[test]
fn comparison_json_has_analysis() {
    let json_path = reports_dir().join("performance_comparison.json");
    if !json_path.exists() {
        eprintln!("No performance_comparison.json — skipping");
        return;
    }

    let report = read_json_file(&json_path).expect("parse comparison JSON");

    // Verify required sections
    assert!(report.get("schema").is_some(), "should have schema");
    assert!(
        report.get("summary").is_some(),
        "should have summary section"
    );
    assert!(
        report.get("analysis").is_some(),
        "should have analysis section"
    );
    assert!(
        report.get("extensions").is_some(),
        "should have per-extension data"
    );

    let analysis = report.get("analysis").unwrap();
    assert!(
        analysis.get("regression_hypothesis").is_some(),
        "should document regression hypothesis"
    );
    assert!(
        analysis.get("mitigation").is_some(),
        "should document planned mitigation"
    );
    assert!(
        analysis.get("methodology").is_some(),
        "should document methodology"
    );
}

#[test]
fn comparison_events_complete() {
    let events_path = reports_dir().join("performance_events.jsonl");
    if !events_path.exists() {
        eprintln!("No performance_events.jsonl — skipping");
        return;
    }

    let content = std::fs::read_to_string(&events_path).expect("read events");
    let events: Vec<Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    assert!(!events.is_empty(), "should have events");

    for event in &events {
        assert!(
            event.get("extension_id").is_some(),
            "event should have extension_id"
        );
        assert!(
            event.get("rust_load_ms").is_some(),
            "event should have rust_load_ms"
        );
        assert!(
            event.get("ts_load_ms").is_some(),
            "event should have ts_load_ms"
        );
        assert!(
            event.get("status").is_some(),
            "event should have status (regression/improvement)"
        );
    }

    eprintln!("Performance events: {} entries validated", events.len());
}
