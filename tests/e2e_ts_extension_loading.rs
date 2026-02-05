//! E2E: TypeScript extension loading through swc transpilation + `QuickJS` (bd-1e1b).
//!
//! Validates that the Rust `QuickJS` runtime can:
//! 1. Load `.ts` TypeScript extensions (swc transpiles TS → JS → `QuickJS` executes)
//! 2. Capture registrations into `JsExtensionSnapshot` via `ExtensionManager`
//! 3. Serialize snapshots to JSON for comparison
//!
//! This exercises a different code path than `.mjs` loading — the swc transpiler
//! must strip type annotations, interfaces, enums, and generics before `QuickJS`
//! can execute the resulting JavaScript.

mod common;

use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::Value;
use std::sync::Arc;

// ─── TypeScript extension sources ────────────────────────────────────────────

/// Minimal TS extension with type annotations on parameters and return type.
const MINIMAL_TS_EXTENSION: &str = r#"
interface PiApi {
  registerCommand(name: string, opts: any): void;
}

export default function init(pi: PiApi): void {
  pi.registerCommand("ts-hello", {
    description: "Hello from TypeScript",
    handler: async (args: string, ctx: any): Promise<any> => {
      return { display: "Hello from TS!" };
    }
  });
}
"#;

/// TS extension using interfaces, type aliases, optional fields, and generics.
const RICH_TS_EXTENSION: &str = r#"
interface CommandOptions {
  description: string;
  handler: (args: string, ctx: any) => Promise<CommandResult>;
}

interface CommandResult {
  display: string;
}

type FlagType = "boolean" | "string" | "number";

interface FlagOptions<T extends FlagType = FlagType> {
  type: T;
  description: string;
  default?: any;
}

export default function init(pi: any): void {
  // Commands with typed options
  pi.registerCommand("ts-greet", {
    description: "Greet with TypeScript types",
    handler: async (args: string, ctx: any): Promise<CommandResult> => {
      return { display: `Greetings: ${args}` };
    }
  } as CommandOptions);

  pi.registerCommand("ts-upper", {
    description: "Uppercase input",
    handler: async (args: string): Promise<CommandResult> => {
      const result: string = (args || "").toUpperCase();
      return { display: result };
    }
  } as CommandOptions);

  // Shortcuts
  pi.registerShortcut("ctrl+t", {
    description: "TypeScript shortcut",
    handler: async (ctx: any): Promise<CommandResult> => {
      return { display: "Ctrl+T from TS" };
    }
  });

  // Flags with type annotations
  const verboseFlag: FlagOptions<"boolean"> = {
    type: "boolean",
    description: "Enable verbose TS output",
    default: false
  };
  pi.registerFlag("ts-verbose", verboseFlag);

  pi.registerFlag("ts-output", {
    type: "string",
    description: "Output format",
    default: "text"
  } as FlagOptions<"string">);

  // Provider
  pi.registerProvider("ts-mock-provider", {
    baseUrl: "https://api.ts-mock.test/v1",
    apiKey: "TS_MOCK_KEY",
    api: "openai-completions",
    models: [
      {
        id: "ts-fast",
        name: "TS Fast Model",
        contextWindow: 16000,
        maxTokens: 2048,
        input: ["text"],
      }
    ]
  });
}
"#;

/// Extension with TypeScript enum (const enum should be inlined by swc).
const ENUM_TS_EXTENSION: &str = r#"
const enum LogLevel {
  Debug = "debug",
  Info = "info",
  Warn = "warn",
}

export default function init(pi: any): void {
  pi.registerCommand("ts-log", {
    description: "Log with level " + "info",
    handler: async (_args: string): Promise<{ display: string }> => {
      return { display: "Logged at info" };
    }
  });

  pi.registerFlag("ts-log-level", {
    type: "string",
    description: "Logging level",
    default: "info"
  });
}
"#;

/// Extension that uses only type imports (should be fully erased).
const TYPE_ONLY_TS_EXTENSION: &str = r#"
type Handler = (args: string) => Promise<{ display: string }>;

export default function init(pi: any): void {
  const handler: Handler = async (args: string) => {
    return { display: "Type-only extension: " + (args || "none") };
  };

  pi.registerCommand("ts-typed", {
    description: "Uses type alias for handler",
    handler: handler
  });
}
"#;

/// Empty TypeScript extension — no registrations, just typed signature.
const EMPTY_TS_EXTENSION: &str = r"
export default function init(pi: any): void {
  // No registrations — validates that empty TS extensions load cleanly.
}
";

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Load a TypeScript extension through the full pipeline: write .ts file →
/// `JsExtensionLoadSpec::from_entry_path` → swc transpile → `QuickJS` execute →
/// `ExtensionManager` captures registrations.
fn load_ts_extension(harness: &common::TestHarness, source: &str) -> ExtensionManager {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_entry_path = harness.create_file("extensions/ext.ts", source.as_bytes());
    harness.record_artifact("extensions/ext.ts", &ext_entry_path);

    let spec = JsExtensionLoadSpec::from_entry_path(&ext_entry_path).expect("load spec from .ts");

    harness
        .log()
        .info_ctx("ts_load", "Created JsExtensionLoadSpec", |ctx| {
            ctx.push(("extension_id".into(), spec.extension_id.clone()));
            ctx.push(("entry_path".into(), ext_entry_path.display().to_string()));
            ctx.push(("name".into(), spec.name.clone()));
            ctx.push(("version".into(), spec.version.clone()));
            ctx.push(("api_version".into(), spec.api_version.clone()));
        });

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
                .expect("load .ts extension");
        }
    });

    manager
}

/// Capture registration state as a JSON `Value` for snapshot comparison.
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

/// Write JSONL logs and artifact index.
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

// ─── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn ts_minimal_extension_loads_and_captures_command() {
    let harness = common::TestHarness::new("ts_minimal_extension_loads_and_captures_command");
    harness
        .log()
        .info("ts_load", "Loading minimal TypeScript extension");

    let manager = load_ts_extension(&harness, MINIMAL_TS_EXTENSION);

    assert!(
        manager.has_command("ts-hello"),
        "ts-hello command should be registered from .ts extension"
    );
    let commands = manager.list_commands();
    assert_eq!(
        commands.len(),
        1,
        "expected 1 command, got {}",
        commands.len()
    );

    let cmd = &commands[0];
    assert_eq!(
        cmd.get("description").and_then(|v| v.as_str()),
        Some("Hello from TypeScript")
    );

    harness
        .log()
        .info("ts_load", "Minimal TS extension loaded successfully");
    write_jsonl_artifacts(&harness);
}

#[test]
fn ts_minimal_command_executes() {
    let harness = common::TestHarness::new("ts_minimal_command_executes");
    let manager = load_ts_extension(&harness, MINIMAL_TS_EXTENSION);

    let result =
        common::run_async(async move { manager.execute_command("ts-hello", "", 5000).await });

    assert!(
        result.is_ok(),
        "ts-hello execution should succeed: {result:?}"
    );
    let value = result.unwrap();
    assert_eq!(
        value.get("display").and_then(|v| v.as_str()),
        Some("Hello from TS!")
    );

    write_jsonl_artifacts(&harness);
}

#[test]
#[allow(clippy::too_many_lines)]
fn ts_rich_extension_captures_all_registrations() {
    let harness = common::TestHarness::new("ts_rich_extension_captures_all_registrations");
    harness.log().info(
        "ts_load",
        "Loading rich TypeScript extension with interfaces, generics, and type aliases",
    );

    let manager = load_ts_extension(&harness, RICH_TS_EXTENSION);

    // Commands
    assert!(
        manager.has_command("ts-greet"),
        "ts-greet should be registered"
    );
    assert!(
        manager.has_command("ts-upper"),
        "ts-upper should be registered"
    );
    let commands = manager.list_commands();
    assert_eq!(
        commands.len(),
        2,
        "expected 2 commands, got {}",
        commands.len()
    );

    let greet_cmd = commands
        .iter()
        .find(|c| c.get("name").and_then(|v| v.as_str()) == Some("ts-greet"))
        .expect("ts-greet should be in list_commands");
    assert_eq!(
        greet_cmd.get("description").and_then(|v| v.as_str()),
        Some("Greet with TypeScript types")
    );

    // Shortcuts
    assert!(
        manager.has_shortcut("ctrl+t"),
        "ctrl+t shortcut should be registered"
    );
    let shortcuts = manager.list_shortcuts();
    assert_eq!(
        shortcuts.len(),
        1,
        "expected 1 shortcut, got {}",
        shortcuts.len()
    );

    // Flags
    let flags = manager.list_flags();
    assert_eq!(flags.len(), 2, "expected 2 flags, got {}", flags.len());

    let verbose_flag = flags
        .iter()
        .find(|f| f.get("name").and_then(|v| v.as_str()) == Some("ts-verbose"))
        .expect("ts-verbose flag should exist");
    assert_eq!(
        verbose_flag.get("type").and_then(|v| v.as_str()),
        Some("boolean")
    );

    let output_flag = flags
        .iter()
        .find(|f| f.get("name").and_then(|v| v.as_str()) == Some("ts-output"))
        .expect("ts-output flag should exist");
    assert_eq!(
        output_flag.get("type").and_then(|v| v.as_str()),
        Some("string")
    );

    // Providers
    let providers = manager.extension_providers();
    assert_eq!(
        providers.len(),
        1,
        "expected 1 provider, got {}",
        providers.len()
    );
    assert_eq!(
        providers[0].get("id").and_then(|v| v.as_str()),
        Some("ts-mock-provider")
    );

    // Models
    let model_entries = manager.extension_model_entries();
    assert_eq!(
        model_entries.len(),
        1,
        "expected 1 model entry, got {}",
        model_entries.len()
    );
    let model = &model_entries[0].model;
    assert_eq!(model.id, "ts-fast");
    assert_eq!(model.provider, "ts-mock-provider");
    assert_eq!(model.context_window, 16000);
    assert_eq!(model.max_tokens, 2048);

    harness.log().info_ctx(
        "ts_load",
        "Rich TS extension registrations verified",
        |ctx| {
            ctx.push(("commands".into(), commands.len().to_string()));
            ctx.push(("shortcuts".into(), shortcuts.len().to_string()));
            ctx.push(("flags".into(), flags.len().to_string()));
            ctx.push(("providers".into(), providers.len().to_string()));
            ctx.push(("models".into(), model_entries.len().to_string()));
        },
    );
    write_jsonl_artifacts(&harness);
}

#[test]
fn ts_rich_command_executes_with_args() {
    let harness = common::TestHarness::new("ts_rich_command_executes_with_args");
    let manager = load_ts_extension(&harness, RICH_TS_EXTENSION);

    // Execute ts-greet
    let result = common::run_async({
        let manager = manager.clone();
        async move { manager.execute_command("ts-greet", "world", 5000).await }
    });
    assert!(
        result.is_ok(),
        "ts-greet execution should succeed: {result:?}"
    );
    assert_eq!(
        result.unwrap().get("display").and_then(|v| v.as_str()),
        Some("Greetings: world")
    );

    // Execute ts-upper
    let result =
        common::run_async(async move { manager.execute_command("ts-upper", "hello", 5000).await });
    assert!(
        result.is_ok(),
        "ts-upper execution should succeed: {result:?}"
    );
    assert_eq!(
        result.unwrap().get("display").and_then(|v| v.as_str()),
        Some("HELLO")
    );

    write_jsonl_artifacts(&harness);
}

#[test]
fn ts_rich_shortcut_executes() {
    let harness = common::TestHarness::new("ts_rich_shortcut_executes");
    let manager = load_ts_extension(&harness, RICH_TS_EXTENSION);

    let result = common::run_async(async move {
        manager
            .execute_shortcut("ctrl+t", serde_json::json!({}), 5000)
            .await
    });

    assert!(result.is_ok(), "ctrl+t shortcut should succeed: {result:?}");
    assert_eq!(
        result.unwrap().get("display").and_then(|v| v.as_str()),
        Some("Ctrl+T from TS")
    );

    write_jsonl_artifacts(&harness);
}

#[test]
fn ts_enum_extension_loads() {
    let harness = common::TestHarness::new("ts_enum_extension_loads");
    harness
        .log()
        .info("ts_load", "Loading TS extension with const enum");

    let manager = load_ts_extension(&harness, ENUM_TS_EXTENSION);

    assert!(
        manager.has_command("ts-log"),
        "ts-log command should be registered"
    );
    let commands = manager.list_commands();
    assert_eq!(commands.len(), 1);

    let flags = manager.list_flags();
    assert_eq!(flags.len(), 1);
    assert_eq!(
        flags[0].get("name").and_then(|v| v.as_str()),
        Some("ts-log-level")
    );
    assert_eq!(
        flags[0].get("default").and_then(|v| v.as_str()),
        Some("info")
    );

    harness
        .log()
        .info("ts_load", "Enum TS extension loaded successfully");
    write_jsonl_artifacts(&harness);
}

#[test]
fn ts_type_only_extension_loads() {
    let harness = common::TestHarness::new("ts_type_only_extension_loads");
    harness
        .log()
        .info("ts_load", "Loading TS extension with type-only constructs");

    let manager = load_ts_extension(&harness, TYPE_ONLY_TS_EXTENSION);

    assert!(
        manager.has_command("ts-typed"),
        "ts-typed command should be registered"
    );

    let result =
        common::run_async(async move { manager.execute_command("ts-typed", "test", 5000).await });
    assert!(
        result.is_ok(),
        "ts-typed execution should succeed: {result:?}"
    );
    assert_eq!(
        result.unwrap().get("display").and_then(|v| v.as_str()),
        Some("Type-only extension: test")
    );

    write_jsonl_artifacts(&harness);
}

#[test]
fn ts_empty_extension_loads_cleanly() {
    let harness = common::TestHarness::new("ts_empty_extension_loads_cleanly");
    harness.log().info("ts_load", "Loading empty TS extension");

    let manager = load_ts_extension(&harness, EMPTY_TS_EXTENSION);

    assert!(manager.list_commands().is_empty(), "no commands expected");
    assert!(manager.list_shortcuts().is_empty(), "no shortcuts expected");
    assert!(manager.list_flags().is_empty(), "no flags expected");
    assert!(
        manager.extension_providers().is_empty(),
        "no providers expected"
    );

    harness
        .log()
        .info("ts_load", "Empty TS extension loaded cleanly");
    write_jsonl_artifacts(&harness);
}

#[test]
fn ts_snapshot_serializes_to_json() {
    let harness = common::TestHarness::new("ts_snapshot_serializes_to_json");
    harness
        .log()
        .info("ts_load", "Loading rich TS extension for JSON snapshot");

    let manager = load_ts_extension(&harness, RICH_TS_EXTENSION);

    let snapshot = snapshot_registrations(&manager);

    // Verify the snapshot is valid JSON
    let json_str =
        serde_json::to_string_pretty(&snapshot).expect("snapshot should serialize to JSON");
    harness
        .log()
        .info_ctx("snapshot", "Registration snapshot serialized", |ctx| {
            ctx.push(("json_bytes".into(), json_str.len().to_string()));
        });

    // Write the snapshot as an artifact
    let snapshot_path = harness.temp_path("ts_registration_snapshot.json");
    std::fs::write(&snapshot_path, format!("{json_str}\n")).expect("write snapshot json");
    harness.record_artifact("ts_registration_snapshot.json", &snapshot_path);

    // Verify structure
    let parsed: Value = serde_json::from_str(&json_str).expect("snapshot JSON should round-trip");
    assert!(parsed.get("commands").unwrap().is_array());
    assert!(parsed.get("shortcuts").unwrap().is_array());
    assert!(parsed.get("flags").unwrap().is_array());
    assert!(parsed.get("providers").unwrap().is_array());
    assert!(parsed.get("models").unwrap().is_array());

    // Verify counts from the snapshot
    assert_eq!(parsed["commands"].as_array().unwrap().len(), 2);
    assert_eq!(parsed["shortcuts"].as_array().unwrap().len(), 1);
    assert_eq!(parsed["flags"].as_array().unwrap().len(), 2);
    assert_eq!(parsed["providers"].as_array().unwrap().len(), 1);
    assert_eq!(parsed["models"].as_array().unwrap().len(), 1);

    harness
        .log()
        .info("snapshot", "JSON snapshot validated successfully");
    write_jsonl_artifacts(&harness);
}

#[test]
fn ts_load_spec_metadata_correct() {
    let harness = common::TestHarness::new("ts_load_spec_metadata_correct");
    let ext_path = harness.create_file("extensions/my_ext.ts", MINIMAL_TS_EXTENSION.as_bytes());

    let spec = JsExtensionLoadSpec::from_entry_path(&ext_path).expect("load spec");

    harness
        .log()
        .info_ctx("spec", "JsExtensionLoadSpec created", |ctx| {
            ctx.push(("extension_id".into(), spec.extension_id.clone()));
            ctx.push(("name".into(), spec.name.clone()));
            ctx.push(("version".into(), spec.version.clone()));
            ctx.push(("api_version".into(), spec.api_version.clone()));
        });

    // extension_id should be derived from filename without .ts extension
    assert_eq!(
        spec.extension_id, "my_ext",
        "extension_id should be 'my_ext', got '{}'",
        spec.extension_id
    );
    // Without package.json, version defaults to "0.0.0"
    assert_eq!(spec.version, "0.0.0");
    // api_version should be PROTOCOL_VERSION
    assert_eq!(spec.api_version, "1.0");

    write_jsonl_artifacts(&harness);
}

#[test]
fn ts_load_spec_with_package_json() {
    let harness = common::TestHarness::new("ts_load_spec_with_package_json");

    // Create package.json alongside the extension
    harness.create_file(
        "extensions/package.json",
        br#"{ "name": "my-ts-ext", "version": "2.1.0" }"#,
    );
    let ext_path = harness.create_file("extensions/index.ts", MINIMAL_TS_EXTENSION.as_bytes());

    let spec = JsExtensionLoadSpec::from_entry_path(&ext_path).expect("load spec");

    harness
        .log()
        .info_ctx("spec", "LoadSpec with package.json", |ctx| {
            ctx.push(("extension_id".into(), spec.extension_id.clone()));
            ctx.push(("name".into(), spec.name.clone()));
            ctx.push(("version".into(), spec.version.clone()));
        });

    // When filename is "index", extension_id comes from parent directory name
    assert_eq!(spec.extension_id, "extensions");
    // name and version from package.json
    assert_eq!(spec.name, "my-ts-ext");
    assert_eq!(spec.version, "2.1.0");

    write_jsonl_artifacts(&harness);
}

#[test]
fn ts_multiple_extensions_loaded() {
    let harness = common::TestHarness::new("ts_multiple_extensions_loaded");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_a_path = harness.create_file(
        "extensions/ext_a.ts",
        br#"
export default function init(pi: any): void {
  pi.registerCommand("from-ts-a", {
    description: "Command from TypeScript extension A",
    handler: async (): Promise<any> => ({})
  });
}
"#,
    );
    let ext_b_path = harness.create_file(
        "extensions/ext_b.ts",
        br#"
export default function init(pi: any): void {
  pi.registerCommand("from-ts-b", {
    description: "Command from TypeScript extension B",
    handler: async (): Promise<any> => ({})
  });
  pi.registerFlag("ts-b-flag", {
    type: "string",
    description: "Flag from TS extension B",
    default: "hello"
  });
}
"#,
    );
    harness.record_artifact("extensions/ext_a.ts", &ext_a_path);
    harness.record_artifact("extensions/ext_b.ts", &ext_b_path);

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
                .expect("load multiple .ts extensions");
        }
    });

    harness
        .log()
        .info_ctx("multi", "Multiple TS extensions loaded", |ctx| {
            ctx.push(("ext_a".into(), ext_a_path.display().to_string()));
            ctx.push(("ext_b".into(), ext_b_path.display().to_string()));
        });

    assert!(manager.has_command("from-ts-a"), "from-ts-a should exist");
    assert!(manager.has_command("from-ts-b"), "from-ts-b should exist");
    assert_eq!(manager.list_commands().len(), 2);
    assert_eq!(manager.list_flags().len(), 1);

    write_jsonl_artifacts(&harness);
}
