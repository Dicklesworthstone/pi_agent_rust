use pi::extensions::CompatibilityScanner;
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Deserialize)]
struct MasterCatalog {
    extensions: Vec<MasterCatalogExtension>,
}

#[derive(Debug, Deserialize)]
struct MasterCatalogExtension {
    id: String,
    directory: String,
    checksum: String,
}

#[derive(Debug, Deserialize)]
struct ArtifactProvenanceManifest {
    items: Vec<ArtifactProvenanceItem>,
}

#[derive(Debug, Deserialize)]
struct ArtifactProvenanceItem {
    id: String,
    directory: String,
    checksum: ArtifactChecksum,
}

#[derive(Debug, Deserialize)]
struct ArtifactChecksum {
    sha256: String,
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
fn test_ext_conformance_artifact_provenance_matches_master_catalog_checksums() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let artifacts_root = repo_root.join("tests/ext_conformance/artifacts");

    let master_path = repo_root.join("docs/extension-master-catalog.json");
    let master_bytes = fs::read(&master_path).expect("read docs/extension-master-catalog.json");
    let master: MasterCatalog =
        serde_json::from_slice(&master_bytes).expect("parse docs/extension-master-catalog.json");

    let provenance_path = repo_root.join("docs/extension-artifact-provenance.json");
    let provenance_bytes =
        fs::read(&provenance_path).expect("read docs/extension-artifact-provenance.json");
    let provenance: ArtifactProvenanceManifest = serde_json::from_slice(&provenance_bytes)
        .expect("parse docs/extension-artifact-provenance.json");

    let master_map = master
        .extensions
        .into_iter()
        .map(|ext| (ext.id.clone(), ext))
        .collect::<BTreeMap<_, _>>();
    let provenance_map = provenance
        .items
        .into_iter()
        .map(|item| (item.id.clone(), item))
        .collect::<BTreeMap<_, _>>();

    assert_eq!(
        master_map.len(),
        provenance_map.len(),
        "master/provenance extension counts differ"
    );

    for (id, master_ext) in master_map {
        let Some(provenance_item) = provenance_map.get(&id) else {
            panic!("Missing provenance entry for {id}");
        };

        assert_eq!(
            provenance_item.directory, master_ext.directory,
            "directory mismatch for {id}"
        );
        assert_eq!(
            provenance_item.checksum.sha256, master_ext.checksum,
            "checksum mismatch between provenance and master catalog for {id}"
        );

        let artifact_dir = artifacts_root.join(&master_ext.directory);
        assert!(
            artifact_dir.is_dir(),
            "missing artifact directory for {id}: {}",
            artifact_dir.display()
        );

        let actual = digest_artifact_dir(&artifact_dir)
            .unwrap_or_else(|err| panic!("digest {id} ({}): {err}", artifact_dir.display()));
        assert_eq!(
            actual, master_ext.checksum,
            "artifact checksum mismatch for {id}"
        );
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

// ---------------------------------------------------------------------------
// Validated extension manifest (bd-3ay7)
// ---------------------------------------------------------------------------

const EXCLUDED_DIRS: &[&str] = &[
    "plugins-official",
    "plugins-community",
    "plugins-ariff",
    "agents-wshobson",
    "templates-davila7",
];

#[derive(Debug, Clone, Serialize)]
struct ValidatedManifest {
    schema: &'static str,
    generated_at: String,
    extensions: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, Serialize)]
struct ManifestEntry {
    id: String,
    entry_path: String,
    source_tier: String,
    capabilities: ManifestCapabilities,
    conformance_tier: u8,
    mock_requirements: Vec<String>,
    registrations: ManifestRegistrations,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize)]
struct ManifestCapabilities {
    registers_tools: bool,
    registers_commands: bool,
    registers_flags: bool,
    registers_providers: bool,
    subscribes_events: Vec<String>,
    uses_exec: bool,
    uses_http: bool,
    uses_ui: bool,
    uses_session: bool,
    is_multi_file: bool,
    has_npm_deps: bool,
}

#[derive(Debug, Clone, Serialize)]
struct ManifestRegistrations {
    tools: Vec<String>,
    commands: Vec<String>,
    flags: Vec<String>,
    event_handlers: Vec<String>,
}

fn determine_source_tier(rel_path: &str) -> &'static str {
    if rel_path.starts_with("community/") {
        "community"
    } else if rel_path.starts_with("npm/") {
        "npm-registry"
    } else if rel_path.starts_with("third-party/") {
        "third-party-github"
    } else if rel_path.starts_with("agents-mikeastock/") {
        "agents-mikeastock"
    } else {
        "official-pi-mono"
    }
}

fn is_excluded_dir(name: &str) -> bool {
    EXCLUDED_DIRS.contains(&name)
}

/// Return a substring window starting at `start` with up to `max_len` bytes,
/// clamped to the nearest char boundary.
fn safe_window(s: &str, start: usize, max_len: usize) -> &str {
    let end = s.len().min(start + max_len);
    // Walk back to a valid char boundary
    let end = (start..=end)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(start);
    &s[start..end]
}

/// Extract registration names from source content using content-level scanning.
/// Handles multi-line patterns like `registerTool({ name: "foo" })`.
fn extract_registrations(content: &str) -> ManifestRegistrations {
    let mut tools = Vec::new();
    let mut commands = Vec::new();
    let mut flags = Vec::new();
    let mut event_handlers = Vec::new();

    for (idx, _) in content.match_indices("registerTool(") {
        let window = safe_window(content, idx, 500);
        if let Some(name) = extract_quoted_after(window, "name:") {
            if !tools.contains(&name) {
                tools.push(name);
            }
        }
    }

    for (idx, _) in content.match_indices("registerCommand(") {
        let window = safe_window(content, idx, 200);
        if let Some(name) = extract_first_string_arg(window, "registerCommand(") {
            if !commands.contains(&name) {
                commands.push(name);
            }
        }
    }

    for (idx, _) in content.match_indices("registerFlag(") {
        let window = safe_window(content, idx, 500);
        if let Some(name) = extract_quoted_after(window, "name:") {
            if !flags.contains(&name) {
                flags.push(name);
            }
        }
    }

    for (idx, _) in content.match_indices(".on(") {
        let window = safe_window(content, idx, 100);
        if let Some(name) = extract_first_string_arg(window, ".on(") {
            if !event_handlers.contains(&name) {
                event_handlers.push(name);
            }
        }
    }

    tools.sort();
    commands.sort();
    flags.sort();
    event_handlers.sort();

    ManifestRegistrations {
        tools,
        commands,
        flags,
        event_handlers,
    }
}

fn extract_quoted_after(text: &str, key: &str) -> Option<String> {
    let idx = text.find(key)?;
    let after = &text[idx + key.len()..];
    let after = after.trim_start();
    let quote = after.chars().next()?;
    if quote != '"' && quote != '\'' && quote != '`' {
        return None;
    }
    let rest = &after[1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

fn extract_first_string_arg(text: &str, prefix: &str) -> Option<String> {
    let idx = text.find(prefix)?;
    let after = &text[idx + prefix.len()..];
    let after = after.trim_start();
    let quote = after.chars().next()?;
    if quote != '"' && quote != '\'' && quote != '`' {
        return None;
    }
    let rest = &after[1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

fn has_npm_dependencies(dir: &Path) -> bool {
    let pkg_path = dir.join("package.json");
    if !pkg_path.is_file() {
        return false;
    }
    let Ok(bytes) = fs::read(&pkg_path) else {
        return false;
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    json.get("dependencies")
        .and_then(|d| d.as_object())
        .is_some_and(|d| !d.is_empty())
}

fn classify_tier(caps: &ManifestCapabilities, has_forbidden: bool) -> u8 {
    if has_forbidden {
        return 5;
    }
    if caps.uses_ui && (caps.registers_tools || caps.subscribes_events.len() > 2) {
        return 4;
    }
    if caps.is_multi_file || caps.has_npm_deps || caps.registers_providers {
        return 3;
    }
    let active = [
        caps.registers_tools,
        caps.registers_commands,
        caps.registers_flags,
        caps.uses_exec,
        caps.uses_http,
        caps.uses_ui,
        caps.uses_session,
    ]
    .iter()
    .filter(|&&v| v)
    .count();
    if active >= 2 || !caps.subscribes_events.is_empty() {
        return 2;
    }
    1
}

fn determine_mock_requirements(caps: &ManifestCapabilities) -> Vec<String> {
    let mut mocks = Vec::new();
    if caps.uses_exec {
        mocks.push("exec".to_string());
    }
    if caps.uses_http {
        mocks.push("http".to_string());
    }
    if caps.uses_ui {
        mocks.push("ui".to_string());
    }
    if caps.uses_session {
        mocks.push("session".to_string());
    }
    mocks
}

/// Discover extension directories under `artifacts_dir`, excluding non-`ExtensionAPI` dirs.
/// Returns `(extension_id, extension_dir)` pairs sorted by ID.
fn discover_extension_dirs(artifacts_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut result = Vec::new();

    let Ok(entries) = fs::read_dir(artifacts_dir) else {
        return result;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }
        if is_excluded_dir(&name) {
            continue;
        }

        match name.as_str() {
            "community" | "npm" | "third-party" | "agents-mikeastock" => {
                if let Ok(sub_entries) = fs::read_dir(entry.path()) {
                    for sub in sub_entries.flatten() {
                        if sub.file_type().is_ok_and(|ft| ft.is_dir()) {
                            let sub_name = sub.file_name().to_string_lossy().to_string();
                            let id = format!("{name}/{sub_name}");
                            result.push((id, sub.path()));
                        }
                    }
                }
            }
            _ => {
                result.push((name, entry.path()));
            }
        }
    }

    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

fn find_entry_point(ext_dir: &Path, artifacts_dir: &Path) -> Option<String> {
    // Check package.json for explicit declaration.
    let pkg_path = ext_dir.join("package.json");
    if pkg_path.is_file() {
        if let Ok(bytes) = fs::read(&pkg_path) {
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let Some(extensions) = json.pointer("/pi/extensions").and_then(|v| v.as_array())
                {
                    if let Some(first) = extensions.first().and_then(|v| v.as_str()) {
                        let cleaned = first.strip_prefix("./").unwrap_or(first);
                        let candidate = ext_dir.join(cleaned);
                        if candidate.is_file() {
                            return Some(relative_posix(artifacts_dir, &candidate));
                        }
                    }
                }
            }
        }
    }

    // Scan TS files and pick the best entry point candidate.
    let ts_files = collect_ts_files(ext_dir);
    let mut best: Option<(String, u8)> = None;

    for file in &ts_files {
        let rel = relative_posix(artifacts_dir, file);
        let Ok(content) = fs::read_to_string(file) else {
            continue;
        };
        let scan = classify_ts_file(&content, &rel);
        if scan.classification == "entry_point" {
            let is_index = file
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("index"));
            let rank = match (scan.confidence.as_str(), is_index) {
                ("high", true) => 4,
                ("high", false) => 3,
                ("medium", true) => 2,
                _ => 1,
            };
            let current_rank = best.as_ref().map_or(0, |b| b.1);
            if rank > current_rank {
                best = Some((rel, rank));
            }
        }
    }

    best.map(|(path, _)| path)
}

#[allow(clippy::too_many_lines)]
#[test]
fn test_generate_validated_manifest() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let artifacts_dir = repo_root.join("tests/ext_conformance/artifacts");

    let ext_dirs = discover_extension_dirs(&artifacts_dir);
    assert!(
        ext_dirs.len() >= 100,
        "expected >= 100 extension dirs, got {}",
        ext_dirs.len()
    );

    let mut entries = Vec::new();
    let mut missing_entry_points = Vec::new();

    for (id, ext_dir) in &ext_dirs {
        let Some(entry_path) = find_entry_point(ext_dir, &artifacts_dir) else {
            missing_entry_points.push(id.clone());
            continue;
        };

        let source_tier = determine_source_tier(&entry_path);

        let scanner = CompatibilityScanner::new(ext_dir.clone());
        let ledger = scanner
            .scan_root()
            .unwrap_or_else(|err| panic!("scan {id}: {err}"));

        let cap_names: Vec<&str> = ledger
            .capabilities
            .iter()
            .map(|c| c.capability.as_str())
            .collect();

        let has_forbidden = !ledger.forbidden.is_empty();

        let ts_files = collect_ts_files(ext_dir);
        let mut all_content = String::new();
        for file in &ts_files {
            if let Ok(content) = fs::read_to_string(file) {
                all_content.push_str(&content);
                all_content.push('\n');
            }
        }

        let registrations = extract_registrations(&all_content);

        let caps = ManifestCapabilities {
            registers_tools: cap_names.contains(&"tool")
                || !registrations.tools.is_empty()
                || all_content.contains("registerTool("),
            registers_commands: !registrations.commands.is_empty()
                || all_content.contains("registerCommand("),
            registers_flags: !registrations.flags.is_empty()
                || all_content.contains("registerFlag("),
            registers_providers: all_content.contains("registerProvider("),
            subscribes_events: registrations.event_handlers.clone(),
            uses_exec: cap_names.contains(&"exec"),
            uses_http: cap_names.contains(&"http"),
            uses_ui: cap_names.contains(&"ui"),
            uses_session: cap_names.contains(&"session"),
            is_multi_file: ts_files.len() > 1,
            has_npm_deps: has_npm_dependencies(ext_dir),
        };

        let conformance_tier = classify_tier(&caps, has_forbidden);
        let mock_requirements = determine_mock_requirements(&caps);

        entries.push(ManifestEntry {
            id: id.clone(),
            entry_path,
            source_tier: source_tier.to_string(),
            capabilities: caps,
            conformance_tier,
            mock_requirements,
            registrations,
        });
    }

    let manifest = ValidatedManifest {
        schema: "pi.ext.validated-manifest.v1",
        generated_at: "2026-02-05T00:00:00Z".to_string(),
        extensions: entries,
    };

    let manifest_path = repo_root.join("tests/ext_conformance/VALIDATED_MANIFEST.json");
    let json = serde_json::to_string_pretty(&manifest).expect("serialize manifest");
    fs::write(&manifest_path, &json).expect("write VALIDATED_MANIFEST.json");

    eprintln!("=== Validated Manifest Summary ===");
    eprintln!("Extensions:          {}", manifest.extensions.len());
    eprintln!("Missing entry point: {}", missing_entry_points.len());
    if !missing_entry_points.is_empty() {
        eprintln!("  Missing: {}", missing_entry_points.join(", "));
    }

    let mut tier_counts = [0u32; 6];
    for ext in &manifest.extensions {
        if (ext.conformance_tier as usize) < tier_counts.len() {
            tier_counts[ext.conformance_tier as usize] += 1;
        }
    }
    for (i, count) in tier_counts.iter().enumerate().skip(1) {
        eprintln!("  Tier {i}: {count}");
    }

    let mut source_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for ext in &manifest.extensions {
        *source_counts.entry(&ext.source_tier).or_default() += 1;
    }
    for (tier, count) in &source_counts {
        eprintln!("  Source {tier}: {count}");
    }

    eprintln!("Manifest: {}", manifest_path.display());

    assert!(
        manifest.extensions.len() >= 150,
        "expected >= 150 extensions in manifest, got {}",
        manifest.extensions.len()
    );
    assert!(
        missing_entry_points.len() < 20,
        "too many missing entry points: {}",
        missing_entry_points.len()
    );
    assert!(
        tier_counts[1] > 10,
        "too few tier 1 extensions: {}",
        tier_counts[1]
    );
}

#[test]
fn test_manifest_spot_check_known_extensions() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let artifacts_dir = repo_root.join("tests/ext_conformance/artifacts");

    let hello_dir = artifacts_dir.join("hello");
    let entry = find_entry_point(&hello_dir, &artifacts_dir);
    assert_eq!(entry.as_deref(), Some("hello/hello.ts"));

    let content = fs::read_to_string(hello_dir.join("hello.ts")).expect("read hello.ts");
    let regs = extract_registrations(&content);
    assert!(
        regs.tools.contains(&"hello".to_string()),
        "hello.ts should register 'hello' tool, got: {:?}",
        regs.tools
    );

    let plan_dir = artifacts_dir.join("plan-mode");
    let plan_entry = find_entry_point(&plan_dir, &artifacts_dir);
    assert!(
        plan_entry
            .as_deref()
            .is_some_and(|e| e.contains("index.ts")),
        "plan-mode entry should be index.ts, got: {plan_entry:?}"
    );

    let provider_dir = artifacts_dir.join("custom-provider-anthropic");
    assert!(
        has_npm_dependencies(&provider_dir),
        "custom-provider-anthropic should have npm deps"
    );
}

#[test]
fn test_source_tier_mapping() {
    assert_eq!(determine_source_tier("hello/hello.ts"), "official-pi-mono");
    assert_eq!(
        determine_source_tier("community/mitsuhiko-answer/answer.ts"),
        "community"
    );
    assert_eq!(
        determine_source_tier("npm/pi-annotate/index.ts"),
        "npm-registry"
    );
    assert_eq!(
        determine_source_tier("third-party/aliou-pi-extensions/defaults/index.ts"),
        "third-party-github"
    );
    assert_eq!(
        determine_source_tier("agents-mikeastock/extensions/pi/AskUserQuestion/index.ts"),
        "agents-mikeastock"
    );
}

#[test]
fn test_extract_registrations_synthetic() {
    let content = r#"
pi.registerTool({
    name: "my_tool",
    description: "does stuff",
});
pi.registerCommand("/test-cmd", { handler: () => {} });
pi.on("tool_call", async (ev) => {});
pi.on("agent_end", () => {});
"#;
    let regs = extract_registrations(content);
    assert_eq!(regs.tools, vec!["my_tool"]);
    assert_eq!(regs.commands, vec!["/test-cmd"]);
    assert_eq!(regs.event_handlers, vec!["agent_end", "tool_call"]);
}

#[test]
fn test_tier_classification_logic() {
    let simple = ManifestCapabilities {
        registers_tools: true,
        registers_commands: false,
        registers_flags: false,
        registers_providers: false,
        subscribes_events: vec![],
        uses_exec: false,
        uses_http: false,
        uses_ui: false,
        uses_session: false,
        is_multi_file: false,
        has_npm_deps: false,
    };
    assert_eq!(classify_tier(&simple, false), 1);

    let medium = ManifestCapabilities {
        registers_commands: true,
        ..simple.clone()
    };
    assert_eq!(classify_tier(&medium, false), 2);

    let complex_multi = ManifestCapabilities {
        is_multi_file: true,
        ..simple.clone()
    };
    assert_eq!(classify_tier(&complex_multi, false), 3);

    let complex_npm = ManifestCapabilities {
        has_npm_deps: true,
        ..simple.clone()
    };
    assert_eq!(classify_tier(&complex_npm, false), 3);

    let ui_heavy = ManifestCapabilities {
        uses_ui: true,
        subscribes_events: vec!["a".into(), "b".into(), "c".into()],
        ..simple
    };
    assert_eq!(classify_tier(&ui_heavy, false), 4);

    assert_eq!(classify_tier(&simple, true), 5);
}

// ---------------------------------------------------------------------------
// Snapshot protocol validation (bd-1pqf)
// ---------------------------------------------------------------------------

/// Validate that ALL provenance entries conform to the snapshot protocol:
/// - Extension IDs are valid (lowercase, no special chars)
/// - Directories match their source tier prefix
/// - Checksums match actual artifacts on disk
#[test]
fn test_snapshot_protocol_provenance_entries_valid() {
    use pi::conformance::snapshot::{
        SourceTier, digest_artifact_dir as lib_digest, validate_directory, validate_id,
    };

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let artifacts_root = repo_root.join("tests/ext_conformance/artifacts");

    let provenance_path = repo_root.join("docs/extension-artifact-provenance.json");
    let provenance_bytes =
        fs::read(&provenance_path).expect("read docs/extension-artifact-provenance.json");
    let provenance: ArtifactProvenanceManifest = serde_json::from_slice(&provenance_bytes)
        .expect("parse docs/extension-artifact-provenance.json");

    let mut failures: Vec<String> = Vec::new();

    for item in &provenance.items {
        // 1. Validate ID naming
        if let Err(e) = validate_id(&item.id) {
            failures.push(format!("{}: id validation: {e}", item.id));
        }

        // 2. Validate directory matches tier
        let tier = SourceTier::from_directory(&item.directory);
        if let Err(e) = validate_directory(&item.directory, tier) {
            failures.push(format!("{}: directory validation: {e}", item.id));
        }

        // 3. Validate artifact directory exists
        let artifact_dir = artifacts_root.join(&item.directory);
        if !artifact_dir.is_dir() {
            failures.push(format!(
                "{}: missing artifact directory: {}",
                item.id,
                artifact_dir.display()
            ));
            continue;
        }

        // 4. Validate checksum via library function matches provenance
        let actual = lib_digest(&artifact_dir)
            .unwrap_or_else(|err| panic!("digest {} ({}): {err}", item.id, artifact_dir.display()));
        if actual != item.checksum.sha256 {
            failures.push(format!(
                "{}: checksum mismatch: provenance={}, actual={}",
                item.id, item.checksum.sha256, actual
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "Snapshot protocol violations ({} failures):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Verify that the library's `digest_artifact_dir` produces identical results
/// to the test-local implementation, ensuring protocol consistency.
#[test]
fn test_snapshot_protocol_digest_matches_local_implementation() {
    use pi::conformance::snapshot::digest_artifact_dir as lib_digest;

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let artifacts_root = repo_root.join("tests/ext_conformance/artifacts");

    // Pick a few well-known extensions to cross-check
    let known = ["hello", "bash-spawn-hook", "community/mitsuhiko-answer"];

    for id in &known {
        let dir = artifacts_root.join(id);
        if !dir.is_dir() {
            continue;
        }
        let local =
            digest_artifact_dir(&dir).unwrap_or_else(|err| panic!("local digest {id}: {err}"));
        let lib = lib_digest(&dir).unwrap_or_else(|err| panic!("lib digest {id}: {err}"));
        assert_eq!(
            local, lib,
            "digest mismatch for {id}: local implementation and library must agree"
        );
    }
}
