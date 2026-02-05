//! E2E: Extension Registration lifecycle with detailed JSONL logging (bd-229v, bd-nh33).
//!
//! This test loads a real `.mjs` extension through the full JS → Rust pipeline,
//! exercising `pi.registerCommand`, `pi.registerShortcut`, `pi.registerFlag`,
//! and `pi.registerProvider` from JavaScript, then verifies all registrations
//! surface correctly through the Rust `ExtensionManager` APIs.
//!
//! All tests emit JSONL logs + artifact index for CI diffing and diagnosis (bd-nh33).

mod common;

use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::Value;
use std::fs;
use std::sync::Arc;

/// Full JS extension source that exercises all four registration APIs.
const FULL_REGISTRATION_EXTENSION: &str = r#"
export default function init(pi) {
  // --- Commands ---
  pi.registerCommand("ext-hello", {
    description: "Say hello from extension",
    handler: async (args, ctx) => {
      return { display: "Hello from ext-hello!" };
    }
  });

  pi.registerCommand("ext-echo", {
    description: "Echo arguments back",
    handler: async (args, ctx) => {
      return { display: "Echo: " + (args || "") };
    }
  });

  // --- Shortcuts ---
  pi.registerShortcut("ctrl+e", {
    description: "Quick edit shortcut",
    handler: async (ctx) => {
      return { display: "Ctrl+E triggered" };
    }
  });

  // --- Flags ---
  pi.registerFlag("ext-verbose", {
    type: "boolean",
    description: "Enable verbose extension output",
    default: false
  });

  pi.registerFlag("ext-format", {
    type: "string",
    description: "Output format for extension",
    default: "json"
  });

  // --- Provider ---
  pi.registerProvider("mock-provider", {
    baseUrl: "https://api.mock-provider.test/v1",
    apiKey: "MOCK_API_KEY",
    api: "openai-completions",
    models: [
      {
        id: "mock-fast",
        name: "Mock Fast Model",
        contextWindow: 32000,
        maxTokens: 4096,
        input: ["text"],
      },
      {
        id: "mock-large",
        name: "Mock Large Model",
        contextWindow: 128000,
        maxTokens: 16384,
        input: ["text", "image"],
        reasoning: true,
      }
    ]
  });
}
"#;

/// Helper: create manager + JS runtime, load an extension, return the manager.
fn load_extension(harness: &common::TestHarness, source: &str) -> ExtensionManager {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_entry_path = harness.create_file("extensions/ext.mjs", source.as_bytes());
    harness.record_artifact("extensions/ext.mjs", &ext_entry_path);
    let spec = JsExtensionLoadSpec::from_entry_path(&ext_entry_path).expect("load spec");

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
                .expect("start js runtime")
        }
    });
    manager.set_js_runtime(runtime);

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load extension");
        }
    });

    manager
}

fn capture_registration_artifacts(harness: &common::TestHarness, manager: &ExtensionManager) {
    let mut commands = manager.list_commands();
    sort_json_values_by_key(&mut commands, &["name"]);

    let mut shortcuts = manager.list_shortcuts();
    sort_json_values_by_key(&mut shortcuts, &["key_id", "keyId"]);

    let mut flags = manager.list_flags();
    sort_json_values_by_key(&mut flags, &["name"]);

    let mut providers = manager.extension_providers();
    sort_json_values_by_key(&mut providers, &["id"]);
    let mut redactions = 0usize;
    for provider in &mut providers {
        redactions = redactions.saturating_add(redact_json_secrets(provider));
        sort_nested_models(provider);
        *provider = canonicalize_json(provider.clone());
    }

    let mut tool_defs = manager.extension_tool_defs();
    sort_json_values_by_key(&mut tool_defs, &["name", "id"]);

    let mut models: Vec<Value> = manager
        .extension_model_entries()
        .into_iter()
        .map(|entry| serde_json::to_value(entry.model).expect("model to json"))
        .collect();
    sort_json_values_by_key(&mut models, &["id"]);

    let commands_value = Value::Array(commands.into_iter().map(canonicalize_json).collect());
    let shortcuts_value = Value::Array(shortcuts.into_iter().map(canonicalize_json).collect());
    let flags_value = Value::Array(flags.into_iter().map(canonicalize_json).collect());
    let providers_value = Value::Array(providers);
    let tool_defs_value = Value::Array(tool_defs.into_iter().map(canonicalize_json).collect());
    let models_value = Value::Array(models.into_iter().map(canonicalize_json).collect());

    let commands_path = harness.temp_path("registration_commands.json");
    write_pretty_json(&commands_path, &commands_value);
    harness.record_artifact("registration_commands.json", &commands_path);

    let shortcuts_path = harness.temp_path("registration_shortcuts.json");
    write_pretty_json(&shortcuts_path, &shortcuts_value);
    harness.record_artifact("registration_shortcuts.json", &shortcuts_path);

    let flags_path = harness.temp_path("registration_flags.json");
    write_pretty_json(&flags_path, &flags_value);
    harness.record_artifact("registration_flags.json", &flags_path);

    let providers_path = harness.temp_path("registration_providers.json");
    write_pretty_json(&providers_path, &providers_value);
    harness.record_artifact("registration_providers.json", &providers_path);

    let tool_defs_path = harness.temp_path("registration_tool_defs.json");
    write_pretty_json(&tool_defs_path, &tool_defs_value);
    harness.record_artifact("registration_tool_defs.json", &tool_defs_path);

    let models_path = harness.temp_path("registration_models.json");
    write_pretty_json(&models_path, &models_value);
    harness.record_artifact("registration_models.json", &models_path);

    harness.log().info_ctx("snapshot", "Registration snapshot artifacts", |ctx| {
        ctx.push(("commands".into(), commands_value.as_array().unwrap().len().to_string()));
        ctx.push((
            "shortcuts".into(),
            shortcuts_value.as_array().unwrap().len().to_string(),
        ));
        ctx.push(("flags".into(), flags_value.as_array().unwrap().len().to_string()));
        ctx.push((
            "providers".into(),
            providers_value.as_array().unwrap().len().to_string(),
        ));
        ctx.push((
            "tool_defs".into(),
            tool_defs_value.as_array().unwrap().len().to_string(),
        ));
        ctx.push(("models".into(), models_value.as_array().unwrap().len().to_string()));
        ctx.push(("redactions".into(), redactions.to_string()));
    });
}

fn write_pretty_json(path: &std::path::Path, value: &Value) {
    let json = serde_json::to_string_pretty(value).expect("serialize json");
    fs::write(path, format!("{json}\n")).expect("write snapshot json");
}

fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            let mut out = serde_json::Map::with_capacity(entries.len());
            for (key, value) in entries {
                out.insert(key, canonicalize_json(value));
            }
            Value::Object(out)
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize_json).collect()),
        other => other,
    }
}

fn sort_json_values_by_key(values: &mut [Value], keys: &[&str]) {
    values.sort_by(|a, b| {
        let a_key = extract_sort_key(a, keys);
        let b_key = extract_sort_key(b, keys);
        a_key.cmp(&b_key)
    });
}

fn extract_sort_key(value: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(v) = value.get(*key).and_then(Value::as_str) {
            return v.to_string();
        }
    }
    String::new()
}

fn sort_nested_models(provider: &mut Value) {
    let Some(models) = provider.get_mut("models").and_then(Value::as_array_mut) else {
        return;
    };
    sort_json_values_by_key(models, &["id", "name"]);
    for model in models.iter_mut() {
        *model = canonicalize_json(model.clone());
    }
}

fn redact_json_secrets(value: &mut Value) -> usize {
    match value {
        Value::Object(map) => {
            let mut redactions = 0usize;
            for (key, value) in map.iter_mut() {
                if is_sensitive_json_key(key) {
                    *value = Value::String("[REDACTED]".to_string());
                    redactions = redactions.saturating_add(1);
                } else {
                    redactions = redactions.saturating_add(redact_json_secrets(value));
                }
            }
            redactions
        }
        Value::Array(values) => values.iter_mut().map(redact_json_secrets).sum(),
        _ => 0,
    }
}

fn is_sensitive_json_key(key: &str) -> bool {
    let key = key.trim().to_ascii_lowercase();
    [
        "api_key",
        "api-key",
        "apikey",
        "authorization",
        "bearer",
        "cookie",
        "credential",
        "password",
        "private_key",
        "secret",
        "token",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

/// Write JSONL logs and artifact index if output directory is set.
fn write_jsonl_artifacts(harness: &common::TestHarness) {
    let logs_path = harness.temp_path("test_logs.jsonl");
    if let Err(e) = harness.write_jsonl_logs_normalized(&logs_path) {
        harness
            .log()
            .warn("jsonl", format!("Failed to write JSONL logs: {e}"));
    } else {
        harness.record_artifact("jsonl_logs", &logs_path);
    }

    let index_path = harness.temp_path("artifact_index.jsonl");
    if let Err(e) = harness.write_artifact_index_jsonl_normalized(&index_path) {
        harness
            .log()
            .warn("jsonl", format!("Failed to write artifact index: {e}"));
    } else {
        harness.record_artifact("artifact_index", &index_path);
    }
}

// ─── Full lifecycle test ────────────────────────────────────────────────────

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_full_registration_lifecycle() {
    let harness = common::TestHarness::new("e2e_full_registration_lifecycle");
    harness
        .log()
        .info("registration", "Starting full registration lifecycle test");

    let manager = load_extension(&harness, FULL_REGISTRATION_EXTENSION);

    // ── Commands ──
    harness
        .log()
        .info("registration", "Verifying command registrations");
    assert!(
        manager.has_command("ext-hello"),
        "ext-hello command should be registered"
    );
    assert!(
        manager.has_command("ext-echo"),
        "ext-echo command should be registered"
    );
    assert!(
        !manager.has_command("nonexistent"),
        "nonexistent command should not be found"
    );

    let commands = manager.list_commands();
    assert_eq!(
        commands.len(),
        2,
        "expected 2 commands, got {}",
        commands.len()
    );

    let hello_cmd = commands
        .iter()
        .find(|c| c.get("name").and_then(|v| v.as_str()) == Some("ext-hello"))
        .expect("ext-hello should be in list_commands");
    assert_eq!(
        hello_cmd.get("description").and_then(|v| v.as_str()),
        Some("Say hello from extension")
    );

    harness
        .log()
        .info_ctx("registration", "Commands verified", |ctx| {
            ctx.push(("count".into(), commands.len().to_string()));
            for cmd in &commands {
                if let Some(name) = cmd.get("name").and_then(|v| v.as_str()) {
                    ctx.push(("command".into(), name.to_string()));
                }
            }
        });

    // ── Shortcuts ──
    harness
        .log()
        .info("registration", "Verifying shortcut registrations");
    assert!(
        manager.has_shortcut("ctrl+e"),
        "ctrl+e shortcut should be registered"
    );
    assert!(
        !manager.has_shortcut("ctrl+q"),
        "ctrl+q shortcut should not be found"
    );

    let shortcuts = manager.list_shortcuts();
    assert_eq!(
        shortcuts.len(),
        1,
        "expected 1 shortcut, got {}",
        shortcuts.len()
    );
    assert_eq!(
        shortcuts[0].get("key_id").and_then(|v| v.as_str()),
        Some("ctrl+e")
    );

    harness
        .log()
        .info_ctx("registration", "Shortcuts verified", |ctx| {
            ctx.push(("count".into(), shortcuts.len().to_string()));
        });

    // ── Flags ──
    harness
        .log()
        .info("registration", "Verifying flag registrations");

    let flags = manager.list_flags();
    assert_eq!(flags.len(), 2, "expected 2 flags, got {}", flags.len());

    let verbose_flag = flags
        .iter()
        .find(|f| f.get("name").and_then(|v| v.as_str()) == Some("ext-verbose"))
        .expect("ext-verbose flag should be in list_flags");
    assert_eq!(
        verbose_flag.get("type").and_then(|v| v.as_str()),
        Some("boolean"),
    );

    let format_flag = flags
        .iter()
        .find(|f| f.get("name").and_then(|v| v.as_str()) == Some("ext-format"))
        .expect("ext-format flag should be in list_flags");
    assert_eq!(
        format_flag.get("type").and_then(|v| v.as_str()),
        Some("string"),
    );

    harness
        .log()
        .info_ctx("registration", "Flags verified", |ctx| {
            ctx.push(("count".into(), flags.len().to_string()));
            for flag in &flags {
                if let Some(name) = flag.get("name").and_then(|v| v.as_str()) {
                    ctx.push(("flag".into(), name.to_string()));
                }
            }
        });

    // ── Providers ──
    harness
        .log()
        .info("registration", "Verifying provider registrations");

    let providers = manager.extension_providers();
    assert_eq!(
        providers.len(),
        1,
        "expected 1 provider, got {}",
        providers.len()
    );
    assert_eq!(
        providers[0].get("id").and_then(|v| v.as_str()),
        Some("mock-provider")
    );

    let model_entries = manager.extension_model_entries();
    assert_eq!(
        model_entries.len(),
        2,
        "expected 2 model entries, got {}",
        model_entries.len()
    );

    let fast_model = model_entries
        .iter()
        .find(|e| e.model.id == "mock-fast")
        .expect("mock-fast model should exist");
    assert_eq!(fast_model.model.provider, "mock-provider");
    assert_eq!(fast_model.model.context_window, 32000);
    assert_eq!(fast_model.model.max_tokens, 4096);
    assert!(!fast_model.model.reasoning);

    let large_model = model_entries
        .iter()
        .find(|e| e.model.id == "mock-large")
        .expect("mock-large model should exist");
    assert_eq!(large_model.model.provider, "mock-provider");
    assert_eq!(large_model.model.context_window, 128_000);
    assert_eq!(large_model.model.max_tokens, 16_384);
    assert!(large_model.model.reasoning);

    harness
        .log()
        .info_ctx("registration", "Providers verified", |ctx| {
            ctx.push(("provider_count".into(), providers.len().to_string()));
            ctx.push(("model_count".into(), model_entries.len().to_string()));
        });

    harness
        .log()
        .info("registration", "Full registration lifecycle test complete");

    capture_registration_artifacts(&harness, &manager);
    write_jsonl_artifacts(&harness);
}

// ─── Command execution E2E ──────────────────────────────────────────────────

#[test]
fn e2e_command_execute_returns_display() {
    let harness = common::TestHarness::new("e2e_command_execute_returns_display");
    let manager = load_extension(&harness, FULL_REGISTRATION_EXTENSION);

    harness
        .log()
        .info_ctx("command", "Executing ext-hello", |ctx| {
            ctx.push(("command".into(), "ext-hello".to_string()));
            ctx.push(("args".into(), String::new()));
        });

    let result =
        common::run_async(async move { manager.execute_command("ext-hello", "", 5000).await });

    harness
        .log()
        .info_ctx("command", "Execution result", |ctx| {
            ctx.push(("success".into(), result.is_ok().to_string()));
            if let Ok(ref v) = result {
                ctx.push(("output".into(), v.to_string()));
            }
        });

    assert!(
        result.is_ok(),
        "ext-hello execution should succeed: {result:?}"
    );
    let value = result.unwrap();
    assert_eq!(
        value.get("display").and_then(|v| v.as_str()),
        Some("Hello from ext-hello!")
    );

    capture_registration_artifacts(&harness, &manager);
    write_jsonl_artifacts(&harness);
}

#[test]
fn e2e_command_execute_with_args() {
    let harness = common::TestHarness::new("e2e_command_execute_with_args");
    let manager = load_extension(&harness, FULL_REGISTRATION_EXTENSION);

    harness
        .log()
        .info_ctx("command", "Executing ext-echo", |ctx| {
            ctx.push(("command".into(), "ext-echo".to_string()));
            ctx.push(("args".into(), "world".to_string()));
        });

    let result =
        common::run_async(async move { manager.execute_command("ext-echo", "world", 5000).await });

    harness
        .log()
        .info_ctx("command", "Execution result", |ctx| {
            ctx.push(("success".into(), result.is_ok().to_string()));
            if let Ok(ref v) = result {
                ctx.push(("output".into(), v.to_string()));
            }
        });

    assert!(
        result.is_ok(),
        "ext-echo execution should succeed: {result:?}"
    );
    let value = result.unwrap();
    assert_eq!(
        value.get("display").and_then(|v| v.as_str()),
        Some("Echo: world")
    );

    capture_registration_artifacts(&harness, &manager);
    write_jsonl_artifacts(&harness);
}

// ─── Shortcut execution E2E ─────────────────────────────────────────────────

#[test]
fn e2e_shortcut_execute() {
    let harness = common::TestHarness::new("e2e_shortcut_execute");
    let manager = load_extension(&harness, FULL_REGISTRATION_EXTENSION);

    harness
        .log()
        .info_ctx("shortcut", "Executing ctrl+e", |ctx| {
            ctx.push(("key_id".into(), "ctrl+e".to_string()));
        });

    let result = common::run_async(async move {
        manager
            .execute_shortcut("ctrl+e", serde_json::json!({}), 5000)
            .await
    });

    harness
        .log()
        .info_ctx("shortcut", "Execution result", |ctx| {
            ctx.push(("success".into(), result.is_ok().to_string()));
            if let Ok(ref v) = result {
                ctx.push(("output".into(), v.to_string()));
            }
        });

    assert!(
        result.is_ok(),
        "ctrl+e shortcut execution should succeed: {result:?}"
    );
    let value = result.unwrap();
    assert_eq!(
        value.get("display").and_then(|v| v.as_str()),
        Some("Ctrl+E triggered")
    );

    capture_registration_artifacts(&harness, &manager);
    write_jsonl_artifacts(&harness);
}

// ─── Flag value round-trip E2E ──────────────────────────────────────────────

#[test]
fn e2e_flag_set_value() {
    let harness = common::TestHarness::new("e2e_flag_set_value");
    let manager = load_extension(&harness, FULL_REGISTRATION_EXTENSION);

    // Verify the flag exists
    let flags = manager.list_flags();
    let verbose = flags
        .iter()
        .find(|f| f.get("name").and_then(|v| v.as_str()) == Some("ext-verbose"))
        .expect("ext-verbose flag should exist");
    assert_eq!(
        verbose.get("type").and_then(|v| v.as_str()),
        Some("boolean")
    );

    harness
        .log()
        .info_ctx("flag", "Setting ext-verbose to true", |ctx| {
            ctx.push(("flag_name".into(), "ext-verbose".to_string()));
            ctx.push(("value".into(), "true".to_string()));
        });

    // Set a flag value through the runtime
    let result = common::run_async(async move {
        manager
            .set_flag_value("ext", "ext-verbose", serde_json::json!(true))
            .await
    });

    harness.log().info_ctx("flag", "Set result", |ctx| {
        ctx.push(("success".into(), result.is_ok().to_string()));
    });

    assert!(result.is_ok(), "set_flag_value should succeed: {result:?}");

    capture_registration_artifacts(&harness, &manager);
    write_jsonl_artifacts(&harness);
}

// ─── Minimal extension tests ────────────────────────────────────────────────

#[test]
fn e2e_empty_extension_loads_cleanly() {
    let harness = common::TestHarness::new("e2e_empty_extension_loads_cleanly");
    let source = r"export default function init(pi) { /* no registrations */ }";
    let manager = load_extension(&harness, source);

    assert!(manager.list_commands().is_empty());
    assert!(manager.list_shortcuts().is_empty());
    assert!(manager.list_flags().is_empty());
    assert!(manager.extension_providers().is_empty());

    capture_registration_artifacts(&harness, &manager);
    write_jsonl_artifacts(&harness);
}

#[test]
fn e2e_command_only_extension() {
    let harness = common::TestHarness::new("e2e_command_only_extension");
    let source = r#"
export default function init(pi) {
  pi.registerCommand("greet", {
    description: "Greet the user",
    handler: async () => ({ display: "Hi!" })
  });
}
"#;
    let manager = load_extension(&harness, source);

    assert_eq!(manager.list_commands().len(), 1);
    assert!(manager.has_command("greet"));
    assert!(manager.list_shortcuts().is_empty());
    assert!(manager.list_flags().is_empty());
    assert!(manager.extension_providers().is_empty());

    capture_registration_artifacts(&harness, &manager);
    write_jsonl_artifacts(&harness);
}

#[test]
fn e2e_multiple_extensions_loaded() {
    let harness = common::TestHarness::new("e2e_multiple_extensions_loaded");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_a_path = harness.create_file(
        "extensions/ext_a.mjs",
        br#"
export default function init(pi) {
  pi.registerCommand("from-a", {
    description: "Command from extension A",
    handler: async () => ({})
  });
}
"#,
    );
    let ext_b_path = harness.create_file(
        "extensions/ext_b.mjs",
        br#"
export default function init(pi) {
  pi.registerCommand("from-b", {
    description: "Command from extension B",
    handler: async () => ({})
  });
  pi.registerFlag("b-flag", {
    type: "string",
    description: "Flag from B",
    default: "hello"
  });
}
"#,
    );
    harness.record_artifact("extensions/ext_a.mjs", &ext_a_path);
    harness.record_artifact("extensions/ext_b.mjs", &ext_b_path);

    let spec_a = JsExtensionLoadSpec::from_entry_path(&ext_a_path).expect("spec a");
    let spec_b = JsExtensionLoadSpec::from_entry_path(&ext_b_path).expect("spec b");

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
                .expect("start js runtime")
        }
    });
    manager.set_js_runtime(runtime);

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec_a, spec_b])
                .await
                .expect("load extensions");
        }
    });

    harness
        .log()
        .info_ctx("multi", "Multiple extensions loaded", |ctx| {
            ctx.push(("ext_a".into(), ext_a_path.display().to_string()));
            ctx.push(("ext_b".into(), ext_b_path.display().to_string()));
        });

    assert!(manager.has_command("from-a"));
    assert!(manager.has_command("from-b"));
    assert_eq!(manager.list_commands().len(), 2);
    assert_eq!(manager.list_flags().len(), 1);

    capture_registration_artifacts(&harness, &manager);
    write_jsonl_artifacts(&harness);
}
