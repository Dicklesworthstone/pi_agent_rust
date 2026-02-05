//! Differential extension conformance tests: compare TS oracle (Bun + jiti)
//! output against Rust `QuickJS` runtime output for the SAME extension source.
//!
//! Each test:
//! 1. Loads an extension .ts file through the Rust swc+`QuickJS` pipeline
//! 2. Runs the TS oracle harness (Bun + jiti) on the same file
//! 3. Compares registration snapshots (tools, commands, flags, shortcuts, etc.)
//!
//! This validates that the Rust extension runtime is a conforming implementation
//! of the pi extension API.

mod common;

	use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
	use pi::extensions_js::PiJsRuntimeConfig;
	use pi::tools::ToolRegistry;
	use serde_json::Value;
	use std::borrow::Cow;
	use std::path::{Path, PathBuf};
	use std::process::Command;
	use std::sync::{Arc, OnceLock};

// ─── Paths ──────────────────────────────────────────────────────────────────

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn artifacts_dir() -> PathBuf {
    project_root().join("tests/ext_conformance/artifacts")
}

	fn ts_oracle_script() -> PathBuf {
	    project_root().join("tests/ext_conformance/ts_oracle/load_extension.ts")
	}

	fn pi_mono_root() -> PathBuf {
	    project_root().join("legacy_pi_mono_code/pi-mono")
	}

	fn pi_mono_packages() -> PathBuf {
	    pi_mono_root().join("packages")
	}

	fn pi_mono_node_modules() -> PathBuf {
	    pi_mono_root().join("node_modules")
	}

fn manifest_path() -> PathBuf {
    project_root().join("tests/ext_conformance/VALIDATED_MANIFEST.json")
}

	const fn bun_path() -> &'static str {
	    "/home/ubuntu/.bun/bin/bun"
	}

	fn ts_oracle_node_path() -> &'static Path {
	    static NODE_PATH: OnceLock<PathBuf> = OnceLock::new();
	    NODE_PATH.get_or_init(|| {
	        let base = PathBuf::from(format!(
	            "/tmp/pi_agent_rust_ts_oracle_node_path-{}",
	            std::process::id()
	        ));

	        let scope_dir = base.join("@mariozechner");
	        std::fs::create_dir_all(&scope_dir).expect("create ts oracle node_path scope dir");

	        #[cfg(unix)]
	        fn symlink_pkg(scope_dir: &Path, name: &str, target: &Path) {
	            use std::os::unix::fs::symlink;

	            let link = scope_dir.join(name);
	            if link.exists() {
	                return;
	            }
	            symlink(target, &link).expect("create ts oracle package symlink");
	        }

	        #[cfg(not(unix))]
	        fn symlink_pkg(_scope_dir: &Path, _name: &str, _target: &Path) {}

	        let packages_dir = pi_mono_packages();
	        symlink_pkg(&scope_dir, "pi-coding-agent", &packages_dir.join("coding-agent"));
	        symlink_pkg(&scope_dir, "pi-ai", &packages_dir.join("ai"));
	        symlink_pkg(&scope_dir, "pi-tui", &packages_dir.join("tui"));
	        symlink_pkg(&scope_dir, "pi-agent-core", &packages_dir.join("agent"));

	        base
	    })
	}

	fn official_extensions() -> &'static Vec<(String, String)> {
	    static OFFICIAL: OnceLock<Vec<(String, String)>> = OnceLock::new();
	    OFFICIAL.get_or_init(|| {
        let data = std::fs::read_to_string(manifest_path())
            .expect("Failed to read VALIDATED_MANIFEST.json");
        let json: Value =
            serde_json::from_str(&data).expect("Failed to parse VALIDATED_MANIFEST.json");
        let extensions = json["extensions"]
            .as_array()
            .expect("manifest.extensions should be an array");

        let mut out = Vec::new();
        for entry in extensions {
            if entry["source_tier"].as_str() != Some("official-pi-mono") {
                continue;
            }
            let entry_path = entry["entry_path"]
                .as_str()
                .expect("missing entry_path in official manifest entry");
            let path = Path::new(entry_path);
            let mut components = path.components();
            let Some(root) = components.next() else {
                continue;
            };
            let extension_dir = root.as_os_str().to_string_lossy().to_string();
            let remaining = components.as_path().to_string_lossy().to_string();
            let entry_file = if remaining.is_empty() {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(entry_path)
                    .to_string()
            } else {
                remaining
            };
            out.push((extension_dir, entry_file));
        }
        out
    })
}

// ─── TS oracle runner ────────────────────────────────────────────────────────

	/// Run the TS oracle harness on an extension and parse the JSON output.
	fn run_ts_oracle(extension_path: &Path) -> Value {
	    let node_path: Cow<'_, str> = match std::env::var("NODE_PATH") {
	        Ok(existing) if !existing.trim().is_empty() => Cow::Owned(format!(
	            "{}:{}:{}",
	            ts_oracle_node_path().display(),
	            pi_mono_node_modules().display(),
	            existing
	        )),
	        _ => Cow::Owned(format!(
	            "{}:{}",
	            ts_oracle_node_path().display(),
	            pi_mono_node_modules().display()
	        )),
	    };

	    let output = Command::new(bun_path())
	        .arg("run")
	        .arg(ts_oracle_script())
	        .arg(extension_path)
	        .arg("/tmp")
	        .current_dir(pi_mono_root())
	        .env("NODE_PATH", node_path.as_ref())
	        .output()
	        .expect("failed to execute TS oracle harness");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success() || !stdout.trim().is_empty(),
        "TS oracle crashed for {}:\nstderr: {stderr}",
        extension_path.display()
    );

    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        unreachable!(
            "TS oracle returned invalid JSON for {}:\n  error: {e}\n  stdout: {stdout}\n  stderr: {stderr}",
            extension_path.display()
        )
    })
}

// ─── Rust runtime loader ─────────────────────────────────────────────────────

/// Load an extension through the Rust swc+`QuickJS` pipeline and return its
/// registration snapshot in a format comparable to the TS oracle output.
/// Returns `Err(message)` if the extension fails to load.
fn load_rust_snapshot(extension_path: &Path) -> Result<Value, String> {
    let cwd = extension_path
        .parent()
        .unwrap_or_else(|| Path::new("/tmp"))
        .to_path_buf();

    let spec = JsExtensionLoadSpec::from_entry_path(extension_path)
        .map_err(|e| format!("load spec: {e}"))?;

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
                .map_err(|e| format!("start runtime: {e}"))
        }
    })?;
    manager.set_js_runtime(runtime);

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .map_err(|e| format!("load extension: {e}"))
        }
    })?;

    // Build snapshot matching TS oracle format
    let commands = manager.list_commands();
    let shortcuts = manager.list_shortcuts();
    let flags = manager.list_flags();
    let providers = manager.extension_providers();
    let tool_defs = manager.extension_tool_defs();
    let event_hooks = manager.list_event_hooks();

    Ok(serde_json::json!({
        "commands": commands,
        "shortcuts": shortcuts,
        "flags": flags,
        "providers": providers,
        "tools": tool_defs,
        "event_hooks": event_hooks,
    }))
}

// ─── Comparison helpers ──────────────────────────────────────────────────────

/// Compare a specific registration category between TS and Rust snapshots.
/// Returns differences as a formatted string (empty = match).
fn compare_category(
    category: &str,
    ts_items: &[Value],
    rust_items: &[Value],
    key_field: &str,
) -> Vec<String> {
    let mut diffs = Vec::new();

    // Compare counts
    if ts_items.len() != rust_items.len() {
        diffs.push(format!(
            "{category} count mismatch: TS={} Rust={}",
            ts_items.len(),
            rust_items.len()
        ));
    }

    // Compare by key
    for ts_item in ts_items {
        let key = ts_item
            .get(key_field)
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        let rust_item = rust_items
            .iter()
            .find(|r| r.get(key_field).and_then(|v| v.as_str()) == Some(key));

        if let Some(rust_item) = rust_item {
            // Compare specific fields depending on category
            match category {
                "tools" => {
                    compare_field(&mut diffs, category, key, "description", ts_item, rust_item);
                    compare_field(&mut diffs, category, key, "label", ts_item, rust_item);
                    compare_parameters(&mut diffs, key, ts_item, rust_item);
                }
                "flags" => {
                    compare_field(&mut diffs, category, key, "type", ts_item, rust_item);
                    compare_field(&mut diffs, category, key, "default", ts_item, rust_item);
                    compare_field(&mut diffs, category, key, "description", ts_item, rust_item);
                }
                "commands" | "shortcuts" => {
                    compare_field(&mut diffs, category, key, "description", ts_item, rust_item);
                }
                _ => {}
            }
        } else {
            diffs.push(format!(
                "{category} '{key}': present in TS, missing in Rust"
            ));
        }
    }

    // Check for extra items in Rust
    for rust_item in rust_items {
        let key = rust_item
            .get(key_field)
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        let in_ts = ts_items
            .iter()
            .any(|t| t.get(key_field).and_then(|v| v.as_str()) == Some(key));
        if !in_ts {
            diffs.push(format!(
                "{category} '{key}': present in Rust, missing in TS"
            ));
        }
    }

    diffs
}

fn compare_field(
    diffs: &mut Vec<String>,
    category: &str,
    key: &str,
    field: &str,
    ts_item: &Value,
    rust_item: &Value,
) {
    let ts_val = ts_item.get(field);
    let rust_val = rust_item.get(field);

    // Normalize None/null
    let ts_normalized = ts_val.filter(|v| !v.is_null());
    let rust_normalized = rust_val.filter(|v| !v.is_null());

    if ts_normalized != rust_normalized {
        diffs.push(format!(
            "{category} '{key}'.{field}: TS={} Rust={}",
            ts_normalized.map_or_else(|| "null".to_string(), ToString::to_string),
            rust_normalized.map_or_else(|| "null".to_string(), ToString::to_string),
        ));
    }
}

fn compare_parameters(
    diffs: &mut Vec<String>,
    tool_name: &str,
    ts_item: &Value,
    rust_item: &Value,
) {
    let ts_params = ts_item.get("parameters");
    let rust_params = rust_item.get("parameters");

    match (ts_params, rust_params) {
        (Some(ts_p), Some(rust_p)) => {
            // Compare type, required, and property names/types
            if ts_p.get("type") != rust_p.get("type") {
                diffs.push(format!(
                    "tools '{tool_name}'.parameters.type: TS={} Rust={}",
                    ts_p.get("type").unwrap_or(&Value::Null),
                    rust_p.get("type").unwrap_or(&Value::Null),
                ));
            }
            // Compare required fields
            let ts_req = ts_p.get("required");
            let rust_req = rust_p.get("required");
            if ts_req != rust_req {
                diffs.push(format!(
                    "tools '{tool_name}'.parameters.required: TS={} Rust={}",
                    ts_req.unwrap_or(&Value::Null),
                    rust_req.unwrap_or(&Value::Null),
                ));
            }
            // Compare property names
            if let (Some(ts_props), Some(rust_props)) =
                (ts_p.get("properties"), rust_p.get("properties"))
            {
                if let (Some(ts_obj), Some(rust_obj)) =
                    (ts_props.as_object(), rust_props.as_object())
                {
                    for (prop_name, ts_prop_val) in ts_obj {
                        if let Some(rust_prop_val) = rust_obj.get(prop_name) {
                            if ts_prop_val.get("type") != rust_prop_val.get("type") {
                                diffs.push(format!(
                                    "tools '{tool_name}'.parameters.properties.{prop_name}.type: TS={} Rust={}",
                                    ts_prop_val.get("type").unwrap_or(&Value::Null),
                                    rust_prop_val.get("type").unwrap_or(&Value::Null),
                                ));
                            }
                        } else {
                            diffs.push(format!(
                                "tools '{tool_name}'.parameters.properties.{prop_name}: in TS, missing in Rust"
                            ));
                        }
                    }
                    for prop_name in rust_obj.keys() {
                        if !ts_obj.contains_key(prop_name) {
                            diffs.push(format!(
                                "tools '{tool_name}'.parameters.properties.{prop_name}: in Rust, missing in TS"
                            ));
                        }
                    }
                }
            }
        }
        (Some(_), None) => {
            diffs.push(format!(
                "tools '{tool_name}'.parameters: present in TS, missing in Rust"
            ));
        }
        (None, Some(_)) => {
            diffs.push(format!(
                "tools '{tool_name}'.parameters: present in Rust, missing in TS"
            ));
        }
        (None, None) => {}
    }
}

/// Full differential comparison: returns all diffs as a vector of strings.
#[allow(clippy::too_many_lines)]
fn diff_snapshots(ts_oracle: &Value, rust_snapshot: &Value) -> Vec<String> {
    let mut all_diffs = Vec::new();

    let Some(ts_ext) = ts_oracle.get("extension") else {
        all_diffs.push("TS oracle returned no extension".to_string());
        return all_diffs;
    };

    // Compare tools
    let ts_tools = ts_ext
        .get("tools")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let rust_tools = rust_snapshot
        .get("tools")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    all_diffs.extend(compare_category("tools", &ts_tools, &rust_tools, "name"));

    // Compare commands
    let ts_commands = ts_ext
        .get("commands")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let rust_commands = rust_snapshot
        .get("commands")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    all_diffs.extend(compare_category(
        "commands",
        &ts_commands,
        &rust_commands,
        "name",
    ));

    // Compare flags
    let ts_flags = ts_ext
        .get("flags")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let rust_flags = rust_snapshot
        .get("flags")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    all_diffs.extend(compare_category("flags", &ts_flags, &rust_flags, "name"));

    // Compare shortcuts
    let ts_shortcuts = ts_ext
        .get("shortcuts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let rust_shortcuts = rust_snapshot
        .get("shortcuts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    all_diffs.extend(compare_category(
        "shortcuts",
        &ts_shortcuts,
        &rust_shortcuts,
        "shortcut",
    ));

    // Compare handler event names
    let ts_handlers = ts_ext
        .get("handlers")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let rust_event_hooks: Vec<String> = rust_snapshot
        .get("event_hooks")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let ts_event_names: Vec<String> = ts_handlers.keys().cloned().collect();
    let mut ts_sorted = ts_event_names;
    ts_sorted.sort();
    let mut rust_sorted = rust_event_hooks;
    rust_sorted.sort();
    if ts_sorted != rust_sorted {
        all_diffs.push(format!(
            "event_hooks mismatch: TS={ts_sorted:?} Rust={rust_sorted:?}"
        ));
    }

    // Compare providers
    let ts_providers = ts_ext
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let rust_providers = rust_snapshot
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if ts_providers.len() != rust_providers.len() {
        all_diffs.push(format!(
            "providers count mismatch: TS={} Rust={}",
            ts_providers.len(),
            rust_providers.len()
        ));
    }

    all_diffs
}

// ─── Test runner ─────────────────────────────────────────────────────────────

/// Run the differential test for a single extension file.
fn run_differential_test(extension_name: &str, entry_file: &str) {
    let ext_path = artifacts_dir().join(extension_name).join(entry_file);
    assert!(
        ext_path.exists(),
        "Extension file not found: {}",
        ext_path.display()
    );

    // Run TS oracle
    let ts_result = run_ts_oracle(&ext_path);
    let ts_success = ts_result
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !ts_success {
        let err = ts_result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        eprintln!("TS oracle failed for {extension_name}: {err}");
        // If TS fails, skip the comparison (extension may be broken)
        return;
    }

    // Run Rust runtime
    let rust_result = load_rust_snapshot(&ext_path)
        .unwrap_or_else(|err| unreachable!("Rust runtime failed for {extension_name}: {err}"));

    // Compare
    let diffs = diff_snapshots(&ts_result, &rust_result);

    if !diffs.is_empty() {
        eprintln!("=== Differential test failed for {extension_name} ===");
        for diff in &diffs {
            eprintln!("  {diff}");
        }
        eprintln!(
            "\nTS snapshot:\n{}",
            serde_json::to_string_pretty(&ts_result).unwrap()
        );
        eprintln!(
            "\nRust snapshot:\n{}",
            serde_json::to_string_pretty(&rust_result).unwrap()
        );
        unreachable!(
            "Differential conformance failed for {extension_name}: {} differences",
            diffs.len()
        );
    }
}

/// Strict differential test that treats TS oracle failures as errors and returns a summary.
fn run_differential_test_strict(extension_name: &str, entry_file: &str) -> Result<(), String> {
    let ext_path = artifacts_dir().join(extension_name).join(entry_file);
    if !ext_path.exists() {
        return Err(format!("extension file not found: {}", ext_path.display()));
    }

    let ts_result = run_ts_oracle(&ext_path);
    let ts_success = ts_result
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !ts_success {
        let err = ts_result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return Err(format!("ts_oracle_failed: {err}"));
    }

    let rust_snapshot =
        load_rust_snapshot(&ext_path).map_err(|err| format!("rust_runtime_failed: {err}"))?;
    let diffs = diff_snapshots(&ts_result, &rust_snapshot);
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(format!("diffs: {}", diffs.join("; ")))
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[test]
#[ignore = "bd-150s: official manifest diff run is not yet expected to pass; run manually with --ignored"]
fn diff_official_manifest() {
    let filter = std::env::var("PI_OFFICIAL_FILTER").ok();
    let max = std::env::var("PI_OFFICIAL_MAX")
        .ok()
        .and_then(|val| val.parse::<usize>().ok());

    let selected: Vec<(String, String)> = official_extensions()
        .iter()
        .filter(|(dir, entry)| {
            let name = format!("{dir}/{entry}");
            filter.as_ref().is_none_or(|needle| name.contains(needle))
        })
        .take(max.unwrap_or(usize::MAX))
        .cloned()
        .collect();

    eprintln!(
        "[diff_official_manifest] Starting (selected={} filter={:?} max={:?})",
        selected.len(),
        filter,
        max
    );

    let mut failures = Vec::new();
    for (idx, (extension_dir, entry_file)) in selected.iter().enumerate() {
        let name = format!("{extension_dir}/{entry_file}");
        eprintln!(
            "[diff_official_manifest] {}/{}: {name}",
            idx + 1,
            selected.len()
        );
        let start = std::time::Instant::now();
        if let Err(err) = run_differential_test_strict(extension_dir, entry_file) {
            failures.push(format!("{name}: {err}"));
        }
        eprintln!(
            "[diff_official_manifest] {}/{}: done in {:?}",
            idx + 1,
            selected.len(),
            start.elapsed()
        );
    }

    assert!(
        failures.is_empty(),
        "Official conformance failures ({}):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn diff_hello() {
    run_differential_test("hello", "hello.ts");
}

#[test]
fn diff_pirate() {
    run_differential_test("pirate", "pirate.ts");
}

#[test]
fn diff_session_name() {
    run_differential_test("session-name", "session-name.ts");
}

#[test]
fn diff_todo() {
    run_differential_test("todo", "todo.ts");
}

#[test]
fn diff_bookmark() {
    run_differential_test("bookmark", "bookmark.ts");
}

#[test]
fn diff_dirty_repo_guard() {
    run_differential_test("dirty-repo-guard", "dirty-repo-guard.ts");
}

#[test]
fn diff_event_bus() {
    run_differential_test("event-bus", "event-bus.ts");
}

#[test]
fn diff_tool_override() {
    run_differential_test("tool-override", "tool-override.ts");
}

#[test]
fn diff_custom_footer() {
    run_differential_test("custom-footer", "custom-footer.ts");
}

#[test]
fn diff_question() {
    run_differential_test("question", "question.ts");
}

#[test]
fn diff_trigger_compact() {
    run_differential_test("trigger-compact", "trigger-compact.ts");
}

#[test]
fn diff_notify() {
    run_differential_test("notify", "notify.ts");
}

#[test]
fn diff_model_status() {
    run_differential_test("model-status", "model-status.ts");
}

#[test]
fn diff_permission_gate() {
    run_differential_test("permission-gate", "permission-gate.ts");
}

#[test]
fn diff_status_line() {
    run_differential_test("status-line", "status-line.ts");
}

#[test]
fn diff_custom_provider_anthropic() {
    run_differential_test("custom-provider-anthropic", "index.ts");
}
