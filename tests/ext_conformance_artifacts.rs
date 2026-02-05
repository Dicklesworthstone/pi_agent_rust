use pi::extensions::CompatibilityScanner;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn hex_lower(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn collect_files_recursive(dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_files_recursive(&path, files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn relative_posix(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn digest_artifact_dir(dir: &Path) -> io::Result<String> {
    let mut files = Vec::new();
    collect_files_recursive(dir, &mut files)?;
    files.sort_by_key(|left| relative_posix(dir, left));

    let mut hasher = Sha256::new();
    for path in files {
        let rel = relative_posix(dir, &path);
        hasher.update(b"file\0");
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        hasher.update(&fs::read(&path)?);
        hasher.update(b"\0");
    }

    Ok(hex_lower(&hasher.finalize()))
}

#[test]
fn test_compat_scanner_unit_fixture_ordering() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    fs::write(
        root.join("b.ts"),
        "import fs from 'fs';\npi.tool('read', {});\nnew Function('return 1');\n",
    )
    .expect("write b.ts");

    fs::create_dir_all(root.join("sub")).expect("mkdir sub");
    fs::write(
        root.join("sub/a.ts"),
        "import { spawn } from 'child_process';\nprocess.env.PATH;\n",
    )
    .expect("write sub/a.ts");

    let scanner = CompatibilityScanner::new(root.to_path_buf());
    let ledger = scanner.scan_root().expect("scan root");
    let text = ledger.to_json_pretty().expect("ledger json");
    insta::assert_snapshot!("compat_scanner_unit_fixture_ordering", text);
}

#[test]
fn test_ext_conformance_artifacts_match_manifest_checksums() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));

    let manifest_path = repo_root.join("docs/extension-sample.json");
    let manifest_bytes = fs::read(&manifest_path).expect("read docs/extension-sample.json");
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).expect("parse docs/extension-sample.json");

    let items = manifest
        .get("items")
        .and_then(serde_json::Value::as_array)
        .expect("docs/extension-sample.json: items[]");

    for item in items {
        let id = item
            .get("id")
            .and_then(serde_json::Value::as_str)
            .expect("docs/extension-sample.json: items[].id");

        let expected = item
            .pointer("/checksum/sha256")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        assert!(
            !expected.is_empty(),
            "docs/extension-sample.json: missing checksum.sha256 for {id}"
        );

        let artifact_dir = repo_root.join("tests/ext_conformance/artifacts").join(id);
        assert!(
            artifact_dir.is_dir(),
            "missing artifact directory for {id}: {}",
            artifact_dir.display()
        );

        let actual =
            digest_artifact_dir(&artifact_dir).unwrap_or_else(|err| panic!("digest {id}: {err}"));
        assert_eq!(actual, expected, "artifact checksum mismatch for {id}");
    }
}

#[test]
fn test_ext_conformance_pinned_sample_compat_ledger_snapshot() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest_path = repo_root.join("docs/extension-sample.json");
    let manifest_bytes = fs::read(&manifest_path).expect("read docs/extension-sample.json");
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).expect("parse docs/extension-sample.json");

    let items = manifest
        .get("items")
        .and_then(serde_json::Value::as_array)
        .expect("docs/extension-sample.json: items[]");

    let mut ids = items
        .iter()
        .map(|item| {
            item.get("id")
                .and_then(serde_json::Value::as_str)
                .expect("docs/extension-sample.json: items[].id")
                .to_string()
        })
        .collect::<Vec<_>>();
    ids.sort();

    let mut ledgers: BTreeMap<String, pi::extensions::CompatLedger> = BTreeMap::new();
    for id in ids {
        let artifact_dir = repo_root.join("tests/ext_conformance/artifacts").join(&id);
        assert!(
            artifact_dir.is_dir(),
            "missing artifact directory for {id}: {}",
            artifact_dir.display()
        );

        let scanner = CompatibilityScanner::new(artifact_dir);
        let ledger = scanner
            .scan_root()
            .unwrap_or_else(|err| panic!("scan {id}: {err}"));
        ledgers.insert(id, ledger);
    }

    let text = serde_json::to_string_pretty(&ledgers).expect("serialize ledgers");
    insta::assert_snapshot!("compat_scanner_pinned_sample_ledger", text);
}

// ---------------------------------------------------------------------------
// Entry-point scanner (bd-2u2s)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct EntryPointScan {
    path: String,
    classification: String,
    confidence: String,
    patterns_found: Vec<String>,
}

fn is_ts_file(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    matches!(ext, "ts" | "tsx" | "mts" | "cts")
}

fn collect_ts_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files_recursive(dir, &mut files).expect("collect ts files");
    files.retain(|p| is_ts_file(p));
    files.sort_by_key(|p| relative_posix(dir, p));
    files
}

/// Scan a single TypeScript file and classify it as an extension entry point,
/// sub-module, non-extension, or unknown.
#[allow(clippy::too_many_lines)]
fn classify_ts_file(content: &str, rel_path: &str) -> EntryPointScan {
    let filename = rel_path.rsplit('/').next().unwrap_or(rel_path);

    // Test files are never entry points.
    if filename.ends_with(".test.ts")
        || filename.ends_with(".spec.ts")
        || filename.ends_with(".bench.ts")
    {
        return EntryPointScan {
            path: rel_path.to_string(),
            classification: "non_extension".to_string(),
            confidence: "high".to_string(),
            patterns_found: vec!["test_file".to_string()],
        };
    }

    let mut patterns: Vec<String> = Vec::new();
    let mut has_export_default_fn = false;
    let mut has_export_default_async_fn = false;
    let mut has_export_default_reexport = false;
    let mut has_export_default_identifier = false;
    let mut has_extension_api = false;
    let mut has_named_export = false;
    let mut has_any_export = false;
    let mut has_pi_register = false;
    let mut has_pi_on = false;
    let mut has_pi_events_or_session = false;
    let mut has_pi_ui = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // `export default function` or `export default async function`
        if !has_export_default_fn
            && (trimmed.starts_with("export default function")
                || trimmed.starts_with("export default function("))
        {
            has_export_default_fn = true;
            patterns.push("export_default_function".to_string());
        }

        if !has_export_default_async_fn
            && (trimmed.starts_with("export default async function")
                || trimmed.starts_with("export default async function("))
        {
            has_export_default_async_fn = true;
            patterns.push("export_default_async_function".to_string());
        }

        // Re-export: `export { default } from "..."`
        if !has_export_default_reexport
            && (trimmed.contains("export { default }")
                || trimmed.contains("export {default}")
                || trimmed.contains("export { default,"))
        {
            has_export_default_reexport = true;
            patterns.push("export_default_reexport".to_string());
        }

        // `export default <identifier>;` (variable reference default export)
        // Matches: `export default extension;`, `export default factory;`, etc.
        // but NOT `export default function` or `export default {`.
        if !has_export_default_identifier
            && trimmed.starts_with("export default ")
            && !trimmed.starts_with("export default function")
            && !trimmed.starts_with("export default async")
            && !trimmed.starts_with("export default class")
            && !trimmed.starts_with("export default {")
            && !trimmed.starts_with("export default (")
            && trimmed.ends_with(';')
        {
            has_export_default_identifier = true;
            patterns.push("export_default_identifier".to_string());
        }

        // `ExtensionAPI` or `ExtensionFactory` type reference
        if !has_extension_api
            && (trimmed.contains("ExtensionAPI") || trimmed.contains("ExtensionFactory"))
        {
            has_extension_api = true;
            patterns.push("extension_api_ref".to_string());
        }

        // pi.registerTool / pi.registerCommand / pi.registerProvider / pi.registerFlag
        if !has_pi_register
            && (trimmed.contains(".registerTool(")
                || trimmed.contains(".registerCommand(")
                || trimmed.contains(".registerProvider(")
                || trimmed.contains(".registerFlag("))
        {
            has_pi_register = true;
            patterns.push("pi_register_call".to_string());
        }

        // pi.on(...)
        if !has_pi_on && trimmed.contains(".on(") && trimmed.contains("pi") {
            has_pi_on = true;
            patterns.push("pi_on_event".to_string());
        }

        // pi.events / pi.session
        if !has_pi_events_or_session
            && (trimmed.contains("pi.events") || trimmed.contains("pi.session"))
        {
            has_pi_events_or_session = true;
            patterns.push("pi_events_or_session".to_string());
        }

        // pi.ui.*
        if !has_pi_ui
            && (trimmed.contains("pi.ui.")
                || trimmed.contains(".setHeader(")
                || trimmed.contains(".setFooter("))
        {
            has_pi_ui = true;
            patterns.push("pi_ui_call".to_string());
        }

        // Track any export statement
        if trimmed.starts_with("export ") || trimmed.starts_with("export{") {
            has_any_export = true;
            // Named export (not default)
            if !trimmed.contains("default") {
                has_named_export = true;
            }
        }
    }

    let has_default_export = has_export_default_fn
        || has_export_default_async_fn
        || has_export_default_reexport
        || has_export_default_identifier;
    let has_pi_api = has_pi_register || has_pi_on || has_pi_events_or_session || has_pi_ui;

    // Classification logic:
    // 1. default export + ExtensionAPI → entry_point (high)
    // 2. default re-export → entry_point (high)
    // 3. default export + pi API calls → entry_point (high)
    // 4. default export alone (no ExtensionAPI, no pi calls) → entry_point (medium)
    // 5. ExtensionAPI ref + pi API calls but no default export → sub_module (high)
    // 6. named exports only → sub_module (high)
    // 7. no exports at all → non_extension (medium)
    // 8. otherwise → unknown (low)

    if (has_default_export && (has_extension_api || has_pi_api)) || has_export_default_reexport {
        EntryPointScan {
            path: rel_path.to_string(),
            classification: "entry_point".to_string(),
            confidence: "high".to_string(),
            patterns_found: patterns,
        }
    } else if has_default_export {
        EntryPointScan {
            path: rel_path.to_string(),
            classification: "entry_point".to_string(),
            confidence: "medium".to_string(),
            patterns_found: patterns,
        }
    } else if has_named_export || (has_extension_api && has_pi_api) {
        if !has_named_export {
            patterns.push("named_export_absent".to_string());
        }
        EntryPointScan {
            path: rel_path.to_string(),
            classification: "sub_module".to_string(),
            confidence: "high".to_string(),
            patterns_found: patterns,
        }
    } else if !has_any_export {
        EntryPointScan {
            path: rel_path.to_string(),
            classification: "non_extension".to_string(),
            confidence: "medium".to_string(),
            patterns_found: patterns,
        }
    } else {
        EntryPointScan {
            path: rel_path.to_string(),
            classification: "unknown".to_string(),
            confidence: "low".to_string(),
            patterns_found: patterns,
        }
    }
}

/// Check `package.json` files for `pi.extensions` field and return the declared
/// entry points (relative to the package directory).
fn collect_package_json_entry_points(artifacts_dir: &Path) -> BTreeMap<String, Vec<String>> {
    let mut result = BTreeMap::new();
    let mut pkg_files = Vec::new();
    collect_files_recursive(artifacts_dir, &mut pkg_files).expect("collect package.json files");
    pkg_files.retain(|p| p.file_name().is_some_and(|n| n == "package.json"));

    for pkg_path in pkg_files {
        let Ok(bytes) = fs::read(&pkg_path) else {
            continue;
        };
        let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };

        let Some(extensions) = json.pointer("/pi/extensions").and_then(|v| v.as_array()) else {
            continue;
        };

        let pkg_dir = pkg_path.parent().expect("package.json parent");
        let pkg_rel = relative_posix(artifacts_dir, pkg_dir);

        let entries: Vec<String> = extensions
            .iter()
            .filter_map(|v| v.as_str())
            .map(|entry| {
                let cleaned = entry.strip_prefix("./").unwrap_or(entry);
                if pkg_rel.is_empty() {
                    cleaned.to_string()
                } else {
                    format!("{pkg_rel}/{cleaned}")
                }
            })
            .collect();

        if !entries.is_empty() {
            result.insert(relative_posix(artifacts_dir, &pkg_path), entries);
        }
    }
    result
}

#[test]
fn test_scan_all_ts_entry_points() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let artifacts_dir = repo_root.join("tests/ext_conformance/artifacts");
    assert!(
        artifacts_dir.is_dir(),
        "artifacts dir missing: {}",
        artifacts_dir.display()
    );

    let ts_files = collect_ts_files(&artifacts_dir);
    assert!(
        ts_files.len() > 100,
        "expected >100 TS files, got {}",
        ts_files.len()
    );

    let pkg_entry_points = collect_package_json_entry_points(&artifacts_dir);

    let mut results: Vec<EntryPointScan> = Vec::with_capacity(ts_files.len());
    for path in &ts_files {
        let rel = relative_posix(&artifacts_dir, path);
        let content = fs::read_to_string(path).unwrap_or_else(|err| panic!("read {rel}: {err}"));
        let mut scan = classify_ts_file(&content, &rel);

        // Boost confidence if file is declared in a package.json pi.extensions field.
        for entries in pkg_entry_points.values() {
            if entries.iter().any(|e| e == &rel || rel.ends_with(e)) {
                if !scan
                    .patterns_found
                    .contains(&"package_json_declared".to_string())
                {
                    scan.patterns_found
                        .push("package_json_declared".to_string());
                }
                if scan.classification == "entry_point" {
                    scan.confidence = "high".to_string();
                }
            }
        }

        results.push(scan);
    }

    // Write the full JSON manifest.
    let manifest_path = artifacts_dir.join("entry-point-scan.json");
    let json = serde_json::to_string_pretty(&results).expect("serialize scan results");
    fs::write(&manifest_path, &json).expect("write entry-point-scan.json");

    // Verify classification distribution is reasonable.
    let entry_count = results
        .iter()
        .filter(|r| r.classification == "entry_point")
        .count();
    let entry_high = results
        .iter()
        .filter(|r| r.classification == "entry_point" && r.confidence == "high")
        .count();
    let entry_medium = results
        .iter()
        .filter(|r| r.classification == "entry_point" && r.confidence == "medium")
        .count();
    let sub_count = results
        .iter()
        .filter(|r| r.classification == "sub_module")
        .count();
    let non_ext_count = results
        .iter()
        .filter(|r| r.classification == "non_extension")
        .count();
    let unknown_count = results
        .iter()
        .filter(|r| r.classification == "unknown")
        .count();

    eprintln!("=== Entry Point Scan Summary ===");
    eprintln!("Total TS files:  {}", results.len());
    eprintln!("Entry points:    {entry_count} ({entry_high} high, {entry_medium} medium)",);
    eprintln!("Sub-modules:     {sub_count}");
    eprintln!("Non-extensions:  {non_ext_count}");
    eprintln!("Unknown:         {unknown_count}");
    eprintln!("Manifest:        {}", manifest_path.display());

    // Sanity: we should have a reasonable number of entry points.
    // The catalog has ~205 extensions, so we expect at least ~100 entry points
    // (some extensions are multi-file with nested entry points).
    assert!(
        entry_count >= 80,
        "too few entry points classified: {entry_count} (expected >= 80)",
    );

    // Unknown should be a small fraction (<10%).
    #[allow(clippy::cast_precision_loss)]
    let unknown_pct = unknown_count as f64 / results.len() as f64 * 100.0;
    assert!(
        unknown_pct < 10.0,
        "too many unknowns: {unknown_count} ({unknown_pct:.1}% of total)",
    );

    // Every file should be scanned (no gaps).
    assert_eq!(
        results.len(),
        ts_files.len(),
        "scan results count != ts files count"
    );
}

#[test]
fn test_known_entry_points_classified_correctly() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let artifacts_dir = repo_root.join("tests/ext_conformance/artifacts");

    // Known entry points that MUST be classified as entry_point with high confidence.
    let known_high = &[
        "hello/hello.ts",
        "custom-provider-anthropic/index.ts",
        "sandbox/index.ts",
        "plan-mode/index.ts",
        "handoff/handoff.ts",
        "ssh/ssh.ts",
    ];

    for rel_path in known_high {
        let path = artifacts_dir.join(rel_path);
        if !path.exists() {
            continue;
        }
        let content =
            fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {rel_path}: {err}"));
        let scan = classify_ts_file(&content, rel_path);
        assert_eq!(
            scan.classification, "entry_point",
            "{rel_path}: expected entry_point, got {}",
            scan.classification
        );
        assert_eq!(
            scan.confidence, "high",
            "{rel_path}: expected high confidence, got {}",
            scan.confidence
        );
    }
}

#[test]
fn test_known_sub_modules_classified_correctly() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let artifacts_dir = repo_root.join("tests/ext_conformance/artifacts");

    // Known sub-module files (have named exports but no default export).
    let known_sub = &["plan-mode/utils.ts"];

    for rel_path in known_sub {
        let path = artifacts_dir.join(rel_path);
        if !path.exists() {
            continue;
        }
        let content =
            fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {rel_path}: {err}"));
        let scan = classify_ts_file(&content, rel_path);
        assert_eq!(
            scan.classification, "sub_module",
            "{rel_path}: expected sub_module, got {} (patterns: {:?})",
            scan.classification, scan.patterns_found
        );
    }
}

#[test]
fn test_package_json_entry_point_detection() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let artifacts_dir = repo_root.join("tests/ext_conformance/artifacts");

    let pkg_entries = collect_package_json_entry_points(&artifacts_dir);

    // We know several package.json files have pi.extensions field.
    assert!(
        !pkg_entries.is_empty(),
        "expected at least one package.json with pi.extensions"
    );

    // custom-provider-anthropic/package.json should declare ./index.ts
    let anthropic_key = pkg_entries
        .keys()
        .find(|k| k.contains("custom-provider-anthropic"))
        .expect("custom-provider-anthropic package.json");

    let entries = &pkg_entries[anthropic_key];
    assert!(
        entries.iter().any(|e| e.ends_with("index.ts")),
        "custom-provider-anthropic should declare index.ts, got: {entries:?}"
    );
}

#[test]
fn test_classify_synthetic_files() {
    // Test the classifier with synthetic content.
    let entry_high = classify_ts_file(
        r#"import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
export default function(pi: ExtensionAPI) {
    pi.registerTool({ name: "test" });
}"#,
        "test/index.ts",
    );
    assert_eq!(entry_high.classification, "entry_point");
    assert_eq!(entry_high.confidence, "high");
    assert!(
        entry_high
            .patterns_found
            .contains(&"export_default_function".to_string())
    );
    assert!(
        entry_high
            .patterns_found
            .contains(&"extension_api_ref".to_string())
    );

    // Re-export proxy
    let reexport = classify_ts_file(r#"export { default } from "./extension";"#, "test/index.ts");
    assert_eq!(reexport.classification, "entry_point");
    assert_eq!(reexport.confidence, "high");
    assert!(
        reexport
            .patterns_found
            .contains(&"export_default_reexport".to_string())
    );

    // Sub-module: named exports only
    let sub = classify_ts_file(
        r"export interface Config { name: string; }
	export function helper(): void {}",
        "test/utils.ts",
    );
    assert_eq!(sub.classification, "sub_module");

    // Non-extension: no exports
    let non_ext = classify_ts_file("const x = 42;\nconsole.log(x);\n", "test/script.ts");
    assert_eq!(non_ext.classification, "non_extension");

    // Test file
    let test_file = classify_ts_file(
        r#"import { describe, it } from "vitest";
describe("test", () => { it("works", () => {}); });"#,
        "test/foo.test.ts",
    );
    assert_eq!(test_file.classification, "non_extension");
    assert!(test_file.patterns_found.contains(&"test_file".to_string()));
}
