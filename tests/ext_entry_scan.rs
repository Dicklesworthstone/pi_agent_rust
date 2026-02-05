//! Static scan: identify valid extension entry points in the conformance corpus.
//!
//! Scans all `.ts` files in `tests/ext_conformance/artifacts/` and classifies each as:
//! - `entry_point`: Has `export default function` + `ExtensionAPI` import/pi.* calls
//! - `sub_module`: Exports functions/types but is not an entry point
//! - `non_extension`: Configuration, test, or other non-extension file
//! - `unknown`: Cannot determine classification
//!
//! Output is saved to `docs/extension-entry-scan.json`.
//!
//! Bead: bd-2u2s

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Classification for a single file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileClassification {
    /// Relative path from artifacts root.
    path: String,
    /// Classification: `entry_point` | `sub_module` | `non_extension` | `unknown`.
    classification: String,
    /// Confidence: high | medium | low.
    confidence: String,
    /// Patterns found in the file.
    patterns_found: Vec<String>,
}

/// Summary statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScanSummary {
    total_files: usize,
    entry_points: usize,
    sub_modules: usize,
    non_extensions: usize,
    unknown: usize,
    by_confidence: BTreeMap<String, usize>,
    by_tier: BTreeMap<String, TierStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TierStats {
    total_files: usize,
    entry_points: usize,
    sub_modules: usize,
}

/// Full scan output.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScanOutput {
    #[serde(rename = "$schema")]
    schema: String,
    generated: String,
    description: String,
    summary: ScanSummary,
    files: Vec<FileClassification>,
}

fn artifacts_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/ext_conformance/artifacts")
}

fn collect_ts_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_ts_files_recursive(root, &mut files);
    files.sort();
    files
}

fn collect_ts_files_recursive(dir: &Path, files: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_ts_files_recursive(&path, files);
            } else if path.extension().is_some_and(|e| e == "ts" || e == "tsx") {
                files.push(path);
            }
        }
    }
}

/// Detect patterns in a TypeScript source file.
fn detect_patterns(content: &str) -> Vec<String> {
    let mut patterns = Vec::new();

    // --- Export patterns ---
    if has_export_default_function(content) {
        patterns.push("export_default_function".to_string());
    }
    if content.contains("export default {") || content.contains("export default class") {
        patterns.push("export_default_other".to_string());
    }

    // --- Import patterns ---
    if content.contains("@mariozechner/pi-coding-agent")
        || content.contains("@anthropic-ai/claude-code")
    {
        patterns.push("ExtensionAPI_import".to_string());
    }
    if content.contains("ExtensionAPI") {
        patterns.push("ExtensionAPI_type".to_string());
    }

    // --- Registration patterns ---
    if content.contains(".registerTool(") || content.contains(".registerTool (") {
        patterns.push("registerTool".to_string());
    }
    if content.contains(".registerCommand(") || content.contains(".registerCommand (") {
        patterns.push("registerCommand".to_string());
    }
    if content.contains(".registerProvider(") || content.contains(".registerProvider (") {
        patterns.push("registerProvider".to_string());
    }
    if content.contains(".registerShortcut(") || content.contains(".registerShortcut (") {
        patterns.push("registerShortcut".to_string());
    }
    if content.contains(".registerFlag(") || content.contains(".registerFlag (") {
        patterns.push("registerFlag".to_string());
    }

    // --- Event patterns ---
    // pi.on("event", handler) style
    if has_pi_on_call(content) {
        patterns.push("event_hook".to_string());
    }
    if content.contains(".events.on(") || content.contains(".events.emit(") {
        patterns.push("events_api".to_string());
    }

    // --- API patterns ---
    if content.contains(".exec(") && !content.contains("document.exec") {
        patterns.push("exec_api".to_string());
    }
    if content.contains(".session.") {
        patterns.push("session_api".to_string());
    }
    if content.contains(".ui.") || content.contains("ctx.ui") {
        patterns.push("ui_api".to_string());
    }

    // --- Sub-module indicators ---
    if content.contains("export function ")
        || content.contains("export class ")
        || content.contains("export const ")
        || content.contains("export type ")
        || content.contains("export interface ")
        || content.contains("export enum ")
        || content.contains("export async function ")
    {
        patterns.push("named_exports".to_string());
    }

    // --- Non-extension indicators ---
    if content.contains("package.json") && content.contains("\"main\"") {
        patterns.push("package_json_ref".to_string());
    }

    patterns
}

/// Check for `export default function` patterns.
fn has_export_default_function(content: &str) -> bool {
    // Pattern: export default function ...
    // Pattern: export default async function ...
    // Pattern: export default (pi: ExtensionAPI) => ...
    // Pattern: export default identifier (where identifier is defined above)
    // Pattern: const ext: ExtensionFactory = (pi) => { ... }; export default ext;
    content.contains("export default function")
        || content.contains("export default async function")
        || (content.contains("export default (") && content.contains("=>"))
        || (content.contains("export default ") && content.contains("ExtensionAPI"))
        || (content.contains("ExtensionFactory") && content.contains("export default"))
        || has_export_default_identifier(content)
}

/// Check for `export default identifier` where identifier was assigned a function.
fn has_export_default_identifier(content: &str) -> bool {
    // Looks for patterns like:
    //   const extension = (pi) => { ... };
    //   export default extension;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("export default ")
            && !trimmed.starts_with("export default function")
            && !trimmed.starts_with("export default async")
            && !trimmed.starts_with("export default class")
            && !trimmed.starts_with("export default {")
            && !trimmed.starts_with("export default (")
        {
            // It's `export default someIdentifier;`
            let rest = trimmed.strip_prefix("export default ").unwrap_or("");
            let ident = rest.trim_end_matches(';').trim();
            // Check if this identifier was assigned a function/arrow expression earlier
            if !ident.is_empty()
                && ident.chars().all(|c| c.is_alphanumeric() || c == '_')
                && (content.contains(&format!("const {ident}"))
                    || content.contains(&format!("let {ident}"))
                    || content.contains(&format!("function {ident}")))
            {
                return true;
            }
        }
    }
    false
}

/// Check for `pi.on(` event registration (but not `pi.once(` which is different).
fn has_pi_on_call(content: &str) -> bool {
    // Look for .on("event_name" pattern
    for line in content.lines() {
        let trimmed = line.trim();
        if (trimmed.contains(".on(\"") || trimmed.contains(".on('") || trimmed.contains(".on(`"))
            && !trimmed.starts_with("//")
            && !trimmed.starts_with('*')
        {
            return true;
        }
    }
    false
}

/// Check whether a path looks like a test file (never an entry point).
fn is_test_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.contains(".test.") || lower.contains(".spec.") || lower.contains("__tests__")
}

/// Classify a file based on detected patterns.
fn classify(patterns: &[String], path: &str) -> (String, String) {
    // Early exit: test files are never entry points
    if is_test_file(path) {
        return ("non_extension".to_string(), "high".to_string());
    }
    // Early exit: type declaration files (.d.ts) are never entry points
    if path.ends_with(".d.ts") {
        return ("sub_module".to_string(), "high".to_string());
    }

    let has_export_default = patterns
        .iter()
        .any(|p| p == "export_default_function" || p == "export_default_other");
    let has_extension_api = patterns
        .iter()
        .any(|p| p == "ExtensionAPI_import" || p == "ExtensionAPI_type");
    let has_registration = patterns.iter().any(|p| {
        p == "registerTool"
            || p == "registerCommand"
            || p == "registerProvider"
            || p == "registerShortcut"
            || p == "registerFlag"
    });
    let has_event_hook = patterns.iter().any(|p| p == "event_hook");
    let has_events_api = patterns.iter().any(|p| p == "events_api");
    let has_named_exports = patterns.iter().any(|p| p == "named_exports");
    let has_any_pi_api = has_registration || has_event_hook || has_events_api;

    // File is named index.ts or matches extension dir name
    let basename = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let is_index = basename == "index";

    // HIGH confidence entry point: export default + ExtensionAPI import + pi API calls
    if has_export_default && has_extension_api && has_any_pi_api {
        return ("entry_point".to_string(), "high".to_string());
    }

    // HIGH confidence entry point: export default + ExtensionAPI import (no explicit pi calls needed)
    if has_export_default && has_extension_api {
        return ("entry_point".to_string(), "high".to_string());
    }

    // MEDIUM confidence entry point: export default function + pi API calls (no ExtensionAPI type)
    if has_export_default && has_any_pi_api {
        return ("entry_point".to_string(), "medium".to_string());
    }

    // MEDIUM confidence entry point: export default function + is index.ts
    if has_export_default && is_index {
        return ("entry_point".to_string(), "medium".to_string());
    }

    // LOW confidence entry point: has export default but no extension patterns
    if has_export_default {
        return ("entry_point".to_string(), "low".to_string());
    }

    // Sub-module: has named exports (functions, classes, types) but no default export
    if has_named_exports {
        // If it also has pi API calls, it might be a helper that registers things
        if has_any_pi_api {
            return ("sub_module".to_string(), "medium".to_string());
        }
        return ("sub_module".to_string(), "high".to_string());
    }

    // Has pi API calls but no exports at all (unusual)
    if has_any_pi_api {
        return ("unknown".to_string(), "low".to_string());
    }

    // No extension-related patterns found
    if patterns.is_empty() {
        return ("non_extension".to_string(), "high".to_string());
    }

    // Has some patterns but not enough to classify
    ("unknown".to_string(), "low".to_string())
}

/// Determine source tier from relative path.
fn source_tier(rel_path: &str) -> String {
    if rel_path.starts_with("community/") {
        "community".to_string()
    } else if rel_path.starts_with("npm/") || rel_path.starts_with("npm-registry/") {
        "npm-registry".to_string()
    } else if rel_path.starts_with("third-party/") || rel_path.starts_with("third-party-github/") {
        "third-party-github".to_string()
    } else if rel_path.starts_with("agents-") {
        if rel_path.contains("mikeastock") {
            "agents-mikeastock".to_string()
        } else {
            "agents-other".to_string()
        }
    } else if rel_path.starts_with("plugins-") || rel_path.starts_with("templates-") {
        "non-conformance".to_string()
    } else {
        "official-pi-mono".to_string()
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn scan_extension_entry_points() {
    let root = artifacts_root();
    assert!(root.exists(), "artifacts directory must exist: {root:?}");

    let ts_files = collect_ts_files(&root);
    assert!(
        ts_files.len() > 1000,
        "expected >1000 TS files, found {}",
        ts_files.len()
    );

    let mut classifications = Vec::new();
    let mut summary = ScanSummary {
        total_files: ts_files.len(),
        entry_points: 0,
        sub_modules: 0,
        non_extensions: 0,
        unknown: 0,
        by_confidence: BTreeMap::new(),
        by_tier: BTreeMap::new(),
    };

    for file in &ts_files {
        let rel_path = file
            .strip_prefix(&root)
            .unwrap_or(file)
            .to_string_lossy()
            .to_string();

        let Ok(content) = std::fs::read_to_string(file) else {
            classifications.push(FileClassification {
                path: rel_path,
                classification: "non_extension".to_string(),
                confidence: "high".to_string(),
                patterns_found: vec!["read_error".to_string()],
            });
            summary.non_extensions += 1;
            continue;
        };

        let patterns = detect_patterns(&content);
        let (classification, confidence) = classify(&patterns, &rel_path);

        // Update summary
        match classification.as_str() {
            "entry_point" => summary.entry_points += 1,
            "sub_module" => summary.sub_modules += 1,
            "non_extension" => summary.non_extensions += 1,
            _ => summary.unknown += 1,
        }
        *summary.by_confidence.entry(confidence.clone()).or_insert(0) += 1;

        let tier = source_tier(&rel_path);
        let tier_stats = summary.by_tier.entry(tier).or_insert(TierStats {
            total_files: 0,
            entry_points: 0,
            sub_modules: 0,
        });
        tier_stats.total_files += 1;
        if classification == "entry_point" {
            tier_stats.entry_points += 1;
        } else if classification == "sub_module" {
            tier_stats.sub_modules += 1;
        }

        classifications.push(FileClassification {
            path: rel_path,
            classification,
            confidence,
            patterns_found: patterns,
        });
    }

    let output = ScanOutput {
        schema: "pi.ext.entry-scan.v1".to_string(),
        generated: "2026-02-05".to_string(),
        description: "Static scan of extension corpus entry points (bd-2u2s)".to_string(),
        summary,
        files: classifications,
    };

    // Write output to docs/
    let output_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/extension-entry-scan.json");
    let json = serde_json::to_string_pretty(&output).expect("serialize scan output");
    std::fs::write(&output_path, &json).expect("write scan output");

    // Print summary
    println!("\n=== Extension Entry Point Scan ===");
    println!("Total files: {}", output.summary.total_files);
    println!("Entry points: {}", output.summary.entry_points);
    println!("Sub-modules: {}", output.summary.sub_modules);
    println!("Non-extensions: {}", output.summary.non_extensions);
    println!("Unknown: {}", output.summary.unknown);
    println!("\nBy confidence:");
    for (conf, count) in &output.summary.by_confidence {
        println!("  {conf}: {count}");
    }
    println!("\nBy tier:");
    for (tier, stats) in &output.summary.by_tier {
        println!(
            "  {tier}: {} files, {} entry points, {} sub-modules",
            stats.total_files, stats.entry_points, stats.sub_modules
        );
    }
    println!("\nOutput written to: {}", output_path.display());

    // Assertions
    assert!(
        output.summary.entry_points > 150,
        "expected >150 entry points (catalog has 205 extensions), found {}",
        output.summary.entry_points
    );
    assert!(
        output.summary.unknown < output.summary.total_files / 10,
        "too many unknown files: {} / {}",
        output.summary.unknown,
        output.summary.total_files
    );

    // Verify known entry points are classified correctly
    let hello = output
        .files
        .iter()
        .find(|f| f.path.contains("hello/hello.ts"));
    assert!(
        hello.is_some(),
        "hello/hello.ts should be in the scan output"
    );
    let hello = hello.unwrap();
    assert_eq!(
        hello.classification, "entry_point",
        "hello/hello.ts should be entry_point, got: {}",
        hello.classification
    );

    // Spot-check: doom-overlay sub-modules should NOT be entry points
    for f in &output.files {
        if f.path.contains("doom-overlay/") && !f.path.contains("index.ts") {
            assert_ne!(
                f.classification, "entry_point",
                "doom-overlay sub-module {} should not be entry_point",
                f.path
            );
        }
    }
}

/// Focused test: verify classification accuracy on known extensions.
#[test]
fn known_entry_points_classified_correctly() {
    let root = artifacts_root();
    let known_entry_points = [
        "hello/hello.ts",
        "event-bus/event-bus.ts",
        "git-checkpoint/git-checkpoint.ts",
        "inline-bash/inline-bash.ts",
    ];

    for entry in &known_entry_points {
        let path = root.join(entry);
        if !path.exists() {
            continue;
        }
        let content = std::fs::read_to_string(&path).unwrap();
        let patterns = detect_patterns(&content);
        let (classification, confidence) = classify(&patterns, entry);
        assert_eq!(
            classification, "entry_point",
            "{entry} should be entry_point (confidence={confidence}), patterns: {patterns:?}"
        );
        assert!(
            confidence == "high" || confidence == "medium",
            "{entry} should have high/medium confidence, got: {confidence}"
        );
    }
}

/// Focused test: sub-modules in multi-file extensions are not entry points.
#[test]
fn sub_modules_not_classified_as_entry_points() {
    let root = artifacts_root();

    // doom-overlay has index.ts (entry) + several sub-modules
    let doom_dir = root.join("doom-overlay");
    if !doom_dir.exists() {
        return;
    }

    let ts_files = collect_ts_files(&doom_dir);
    for file in &ts_files {
        let rel_path = file
            .strip_prefix(&root)
            .unwrap_or(file)
            .to_string_lossy()
            .to_string();
        let content = std::fs::read_to_string(file).unwrap();
        let patterns = detect_patterns(&content);
        let (classification, _confidence) = classify(&patterns, &rel_path);

        if rel_path.contains("index.ts") {
            assert_eq!(
                classification, "entry_point",
                "doom-overlay/index.ts should be entry_point, got: {classification}"
            );
        } else {
            assert_ne!(
                classification, "entry_point",
                "doom-overlay sub-module {rel_path} should not be entry_point"
            );
        }
    }
}
