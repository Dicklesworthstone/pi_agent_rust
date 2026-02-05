//! E2E: Extension Registration lifecycle with detailed JSONL logging (bd-229v).
//!
//! This test loads a real `.mjs` extension through the full JS → Rust pipeline,
//! exercising `pi.registerCommand`, `pi.registerShortcut`, `pi.registerFlag`,
//! and `pi.registerProvider` from JavaScript, then verifies all registrations
//! surface correctly through the Rust `ExtensionManager` APIs.

mod common;

use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
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
}

// ─── Command execution E2E ──────────────────────────────────────────────────

#[test]
fn e2e_command_execute_returns_display() {
    let harness = common::TestHarness::new("e2e_command_execute_returns_display");
    let manager = load_extension(&harness, FULL_REGISTRATION_EXTENSION);

    let result =
        common::run_async(async move { manager.execute_command("ext-hello", "", 5000).await });

    assert!(
        result.is_ok(),
        "ext-hello execution should succeed: {result:?}"
    );
    let value = result.unwrap();
    assert_eq!(
        value.get("display").and_then(|v| v.as_str()),
        Some("Hello from ext-hello!")
    );
}

#[test]
fn e2e_command_execute_with_args() {
    let harness = common::TestHarness::new("e2e_command_execute_with_args");
    let manager = load_extension(&harness, FULL_REGISTRATION_EXTENSION);

    let result =
        common::run_async(async move { manager.execute_command("ext-echo", "world", 5000).await });

    assert!(
        result.is_ok(),
        "ext-echo execution should succeed: {result:?}"
    );
    let value = result.unwrap();
    assert_eq!(
        value.get("display").and_then(|v| v.as_str()),
        Some("Echo: world")
    );
}

// ─── Shortcut execution E2E ─────────────────────────────────────────────────

#[test]
fn e2e_shortcut_execute() {
    let harness = common::TestHarness::new("e2e_shortcut_execute");
    let manager = load_extension(&harness, FULL_REGISTRATION_EXTENSION);

    let result = common::run_async(async move {
        manager
            .execute_shortcut("ctrl+e", serde_json::json!({}), 5000)
            .await
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

    // Set a flag value through the runtime
    let result = common::run_async(async move {
        manager
            .set_flag_value("ext", "ext-verbose", serde_json::json!(true))
            .await
    });

    assert!(result.is_ok(), "set_flag_value should succeed: {result:?}");
}

// ─── Minimal extension tests ────────────────────────────────────────────────

#[test]
fn e2e_empty_extension_loads_cleanly() {
    let harness = common::TestHarness::new("e2e_empty_extension_loads_cleanly");
    let manager = load_extension(
        &harness,
        r"export default function init(pi) { /* no registrations */ }",
    );

    assert!(manager.list_commands().is_empty());
    assert!(manager.list_shortcuts().is_empty());
    assert!(manager.list_flags().is_empty());
    assert!(manager.extension_providers().is_empty());
}

#[test]
fn e2e_command_only_extension() {
    let harness = common::TestHarness::new("e2e_command_only_extension");
    let manager = load_extension(
        &harness,
        r#"
export default function init(pi) {
  pi.registerCommand("greet", {
    description: "Greet the user",
    handler: async () => ({ display: "Hi!" })
  });
}
"#,
    );

    assert_eq!(manager.list_commands().len(), 1);
    assert!(manager.has_command("greet"));
    assert!(manager.list_shortcuts().is_empty());
    assert!(manager.list_flags().is_empty());
    assert!(manager.extension_providers().is_empty());
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

    assert!(manager.has_command("from-a"));
    assert!(manager.has_command("from-b"));
    assert_eq!(manager.list_commands().len(), 2);
    assert_eq!(manager.list_flags().len(), 1);
}
