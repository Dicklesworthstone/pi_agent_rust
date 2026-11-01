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
use pi::package_manager::{PackageManager, ResolveExtensionSourcesOptions};
use pi::session::{Session, SessionHandle};
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

fn create_doom_like_package(
    harness: &common::TestHarness,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let package_json = br#"{
  "name": "doom-like-ext",
  "version": "0.1.0",
  "private": true,
  "pi": {
    "extensions": ["./index.ts"]
  }
}"#;
    let package_root = harness.temp_dir().join("doom-like-ext");
    let package_json_path = harness.create_file("doom-like-ext/package.json", package_json);
    let entry_path = harness.create_file(
        "doom-like-ext/index.ts",
        br#"
import { bundled } from "./wad-finder.js";

export default function init(pi: any): void {
  pi.registerCommand("doom-inline-check", {
    description: "Verify doom-style helper modules load from package manifests",
    handler: async (): Promise<{ display: string }> => ({ display: bundled })
  });
}
"#,
    );
    let helper_path = harness.create_file(
        "doom-like-ext/wad-finder.ts",
        br#"import { dirname, join } from "node:path"; import { fileURLToPath } from "node:url"; const __dirname = dirname(fileURLToPath(import.meta.url)); export const bundled = join(__dirname, "doom1.wad");
"#,
    );
    harness.record_artifact("doom-like-ext/package.json", &package_json_path);
    harness.record_artifact("doom-like-ext/index.ts", &entry_path);
    harness.record_artifact("doom-like-ext/wad-finder.ts", &helper_path);
    (package_root, entry_path)
}

#[test]
fn ts_package_manifest_loads_doom_style_helper_out_of_box() {
    let harness =
        common::TestHarness::new("ts_package_manifest_loads_doom_style_helper_out_of_box");
    let cwd = harness.temp_dir().to_path_buf();
    let (package_root, entry_path) = create_doom_like_package(&harness);

    let resolved = common::run_async({
        let package_manager = PackageManager::new(cwd.clone());
        let sources = vec![package_root.display().to_string()];
        async move {
            package_manager
                .resolve_extension_sources(
                    &sources,
                    ResolveExtensionSourcesOptions {
                        local: false,
                        temporary: true,
                    },
                )
                .await
        }
    })
    .expect("resolve doom-like package");

    assert_eq!(
        resolved.extensions.len(),
        1,
        "expected one resolved extension"
    );
    assert_eq!(
        resolved.extensions[0].path, entry_path,
        "package manifest should resolve index.ts as the entrypoint"
    );

    let spec = JsExtensionLoadSpec::from_entry_path(&resolved.extensions[0].path)
        .expect("load spec from resolved package entry");
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
                .expect("load doom-like package extension");
        }
    });

    assert!(
        manager.has_command("doom-inline-check"),
        "doom-like package command should register from the manifest entry"
    );

    let result =
        common::run_async(
            async move { manager.execute_command("doom-inline-check", "", 5000).await },
        )
        .expect("execute doom-like package command");
    let bundled = result
        .get("display")
        .and_then(|value| value.as_str())
        .expect("display text");
    let normalized = bundled.replace('\\', "/");
    assert!(
        normalized.ends_with("/doom1.wad"),
        "expected doom-style helper to resolve bundled path, got {bundled}"
    );

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

/// Reproduces the package, storage and SQLite initialization sequence used by
/// context-mode 1.0.169 (build/adapters/base.js, build/db-base.js and
/// build/session/db.js). This is a real database across two cold PiJS realms;
/// no hostcall interceptor or database stub participates in this test.
#[test]
fn npm_context_mode_storage_and_sqlite_initialize_then_survive_reload() {
    let temp = tempfile::tempdir().expect("tempdir");
    let base = pi::extensions::safe_canonicalize(temp.path());
    let workspace = base.join("workspace");
    let home = base.join("home");
    let package = base.join("npm/node_modules/context-mode");
    let entry = package.join("build/adapters/pi/extension.js");
    std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        package.join("package.json"),
        r#"{
        "name": "context-mode", "version": "1.0.169", "type": "module",
        "pi": { "extensions": ["./build/adapters/pi/extension.js"] }
    }"#,
    )
    .unwrap();
    std::fs::write(package.join("build/db.js"), r#"
        import { createRequire } from 'node:module';
        import { Database as ESMDatabase } from 'bun:sqlite';
        const require = createRequire(import.meta.url);
        const { Database } = require('bun:sqlite');
        if (Database !== ESMDatabase) throw new Error('SQLite module identities diverged');

        export function openSessionDB(path) {
            const db = new Database(path);
            db.prepare('PRAGMA journal_mode = WAL').all();
            db.prepare('PRAGMA busy_timeout = 5000').all();
            db.exec(`
                CREATE TABLE IF NOT EXISTS session_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    type TEXT NOT NULL,
                    data TEXT NOT NULL,
                    data_hash TEXT NOT NULL,
                    UNIQUE(session_id, type, data_hash)
                );
                CREATE INDEX IF NOT EXISTS idx_session_events_session ON session_events(session_id);
            `);
            const insert = db.prepare('INSERT OR IGNORE INTO session_events (session_id, type, data, data_hash) VALUES (?, ?, ?, ?)');
            const write = db.transaction((sessionId, type, data) => {
                insert.run(sessionId, type, data, data);
                insert.run(sessionId, type, data, data);
            });
            return { db, write };
        }
    "#).unwrap();
    std::fs::write(&entry, r#"
        import { mkdirSync } from 'node:fs';
        import { createHash } from 'node:crypto';
        import { homedir } from 'node:os';
        import { join } from 'node:path';
        import { openSessionDB } from '../../db.js';
        export default function piExtension(pi) {
            const directory = join(homedir(), '.pi', 'context-mode', 'sessions');
            mkdirSync(directory, { recursive: true });
            const { db, write } = openSessionDB(join(directory, 'project-fixture.db'));
            let sessionId = '';
            let sessionFile = null;
            pi.on('session_start', (_event, ctx) => {
                // context-mode 1.0.169 derives its database key from this path.
                // A constant fixture ID masks loss of history on cold resume.
                sessionFile = ctx.sessionManager?.getSessionFile?.();
                sessionId = sessionFile && typeof sessionFile === 'string'
                    ? createHash('sha256').update(sessionFile).digest('hex').slice(0, 16)
                    : `pi-${Date.now()}`;
                write(sessionId, 'session_start', 'started');
            });
            pi.on('tool_result', (event) => {
                // The unmodified adapter consumes result/output, including
                // error text and facts used to build its resume snapshot.
                const result = event.result ?? event.output;
                write(sessionId, 'tool_result', typeof result === 'string' ? result : JSON.stringify(result));
            });
            pi.registerCommand('ctx-storage-stats', {
                description: 'Only registers after persistent SQLite initialization',
                handler: async () => ({ display: JSON.stringify({
                    sessionFile, sessionId,
                    count: db.prepare('SELECT count(*) AS count FROM session_events WHERE session_id = ?').get(sessionId).count,
                    rows: db.prepare('SELECT type, data FROM session_events WHERE session_id = ? ORDER BY id').all(sessionId),
                    sessionCount: db.prepare('SELECT count(DISTINCT session_id) AS count FROM session_events').get().count,
                }) }),
            });
            pi.registerCommand('ctx-storage-close', { handler: async () => { db.close(); return { display: 'closed' }; } });
        }
    "#).unwrap();

    let load = |session: Arc<asupersync::sync::Mutex<Session>>| {
        let manager = ExtensionManager::new();
        manager.set_session(Arc::new(SessionHandle(session)));
        let tools = Arc::new(ToolRegistry::new(&[], &workspace, None));
        let mut policy = pi::extensions::ExtensionPolicy::default();
        policy.deny_caps.retain(|cap| cap != "env");
        policy.default_caps.push("env".to_string());
        let config = PiJsRuntimeConfig {
            cwd: workspace.display().to_string(),
            env: std::collections::HashMap::from([(
                "HOME".to_string(),
                home.display().to_string(),
            )]),
            disk_cache_dir: None,
            ..Default::default()
        };
        let runtime = common::run_async(JsExtensionRuntimeHandle::start_with_policy(
            config,
            tools,
            manager.clone(),
            policy,
        ))
        .expect("start runtime");
        manager.set_js_runtime(runtime);
        let spec = JsExtensionLoadSpec::from_entry_path(&entry).unwrap();
        let loading_manager = manager.clone();
        common::run_async(async move { loading_manager.load_js_extensions(vec![spec]).await })
            .expect("load npm-style extension through production loader");
        assert!(manager.has_command("ctx-storage-stats"));
        manager
    };
    let stats = |manager: &ExtensionManager| {
        let manager = manager.clone();
        let result = common::run_async(async move {
            manager.execute_command("ctx-storage-stats", "", 5000).await
        })
        .unwrap();
        serde_json::from_str::<Value>(result["display"].as_str().unwrap()).unwrap()
    };
    let close = |manager: ExtensionManager| {
        common::run_async(async move {
            manager
                .execute_command("ctx-storage-close", "", 5000)
                .await
                .unwrap();
            assert!(manager.shutdown(std::time::Duration::from_secs(5)).await);
        });
    };
    let dispatch = |manager: &ExtensionManager, event, data| {
        let manager = manager.clone();
        common::run_async(async move { manager.dispatch_event(event, data).await }).unwrap();
    };

    let transcripts = base.join("transcripts");
    let mut fresh_session = Session::create_with_dir(Some(transcripts.clone()));
    fresh_session.header.cwd = workspace.display().to_string();
    fresh_session
        .reserve_persistence_path()
        .expect("reserve session identity before loading");
    let session_file = fresh_session
        .session_file()
        .expect("future transcript")
        .to_path_buf();
    assert!(fresh_session.path.is_none());
    assert!(
        !transcripts.exists(),
        "reserving an identity must not create a transcript"
    );
    let shared = Arc::new(asupersync::sync::Mutex::new(fresh_session));
    let manager = load(Arc::clone(&shared));
    assert_eq!(stats(&manager)["count"], 0);
    dispatch(
        &manager,
        pi::extensions::ExtensionEventName::SessionStart,
        None,
    );
    let initial = stats(&manager);
    assert_eq!(initial["count"], 1);
    assert_eq!(initial["sessionFile"], session_file.display().to_string());
    assert!(!initial["sessionId"].as_str().unwrap().starts_with("pi-"));
    common::run_async(async move {
        let cx = pi::agent_cx::AgentCx::for_request();
        let mut session = shared.lock(cx.cx()).await.expect("session lock");
        session.set_name("Durable context-mode session");
        session.save().await.expect("first native session save");
    });
    assert!(session_file.is_file());
    close(manager);
    assert!(
        home.join(".pi/context-mode/sessions/project-fixture.db")
            .is_file()
    );

    let reopen_path = session_file.to_str().unwrap().to_string();
    let reopened = common::run_async(async move { Session::open(&reopen_path).await })
        .expect("cold native transcript reopen");
    let manager = load(Arc::new(asupersync::sync::Mutex::new(reopened)));
    dispatch(
        &manager,
        pi::extensions::ExtensionEventName::SessionStart,
        None,
    );
    let resumed = stats(&manager);
    assert_eq!(resumed["sessionId"], initial["sessionId"]);
    assert_eq!(resumed["sessionFile"], initial["sessionFile"]);
    assert_eq!(
        resumed["sessionCount"], 1,
        "resume must not orphan a second database identity"
    );
    assert_eq!(
        resumed["rows"],
        serde_json::json!([{"type":"session_start","data":"started"}])
    );
    assert_eq!(
        stats(&manager)["count"],
        1,
        "unique event dedup survives a cold reload"
    );
    let result_manager = manager.clone();
    common::run_async(async move {
        result_manager
            .dispatch_tool_result(
                &pi::model::ToolCall {
                    id: "after-resume".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::json!({"command":"cargo test"}),
                    thought_signature: None,
                },
                &pi::tools::ToolOutput {
                    content: vec![pi::model::ContentBlock::Text(pi::model::TextContent::new(
                        "Error: permission denied: /project/.cache",
                    ))],
                    details: Some(serde_json::json!({"exitCode":1})),
                    is_error: true,
                },
                true,
                5_000,
            )
            .await
            .expect("capture real tool output after resume");
    });
    let expected_result = serde_json::json!({
        "content":[{"type":"text","text":"Error: permission denied: /project/.cache"}],
        "details":{"exitCode":1},
        "isError":true,
    })
    .to_string();
    assert_eq!(
        stats(&manager)["rows"],
        serde_json::json!([
            {"type":"session_start","data":"started"},
            {"type":"tool_result","data":expected_result},
        ])
    );
    close(manager);
}

#[test]
fn npm_invalid_state_namespace_does_not_block_unrelated_extension_loading() {
    let temp = tempfile::tempdir().expect("tempdir");
    let base = pi::extensions::safe_canonicalize(temp.path());
    let home = base.join("home");
    std::fs::create_dir_all(home.join(".pi")).unwrap();
    let invalid = home.join(".pi/broken-state");
    #[cfg(unix)]
    {
        let outside = base.join("private-data");
        std::fs::write(&outside, "must remain inaccessible").unwrap();
        std::os::unix::fs::symlink(outside, &invalid).unwrap();
    }
    #[cfg(not(unix))]
    std::fs::write(&invalid, "must remain inaccessible").unwrap();

    let mut specs = Vec::new();
    for (package_name, entry_name, source) in [
        (
            "broken-state",
            "broken.js",
            r#"
            import { readFileSync } from 'node:fs';
            import { homedir } from 'node:os';
            import { join } from 'node:path';
            export default function(pi) {
                let denied = false;
                try { readFileSync(join(homedir(), '.pi', 'broken-state'), 'utf8'); }
                catch (_) { denied = true; }
                pi.registerCommand('broken-state-check', { handler: async () => ({ display: denied ? 'denied' : 'escaped' }) });
            }
        "#,
        ),
        (
            "healthy-state",
            "healthy.js",
            r#"
            import { mkdirSync, writeFileSync, readFileSync } from 'node:fs';
            import { homedir } from 'node:os';
            import { join } from 'node:path';
            export default function(pi) {
                const root = join(homedir(), '.pi', 'healthy-state');
                mkdirSync(root, { recursive: true });
                writeFileSync(join(root, 'state.txt'), 'persistent');
                let peerDenied = false;
                try { readFileSync(join(homedir(), '.pi', 'broken-state'), 'utf8'); }
                catch (_) { peerDenied = true; }
                pi.registerCommand('healthy-state-check', { handler: async () => ({ display: peerDenied ? readFileSync(join(root, 'state.txt'), 'utf8') : 'escaped' }) });
            }
        "#,
        ),
    ] {
        let package = base.join("npm/node_modules").join(package_name);
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("package.json"), serde_json::json!({
            "name": package_name, "type": "module", "pi": {"extensions": [format!("./{entry_name}")]}
        }).to_string()).unwrap();
        let entry = package.join(entry_name);
        std::fs::write(&entry, source).unwrap();
        specs.push(JsExtensionLoadSpec::from_entry_path(entry).unwrap());
    }

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &home, None));
    let mut policy = pi::extensions::ExtensionPolicy::default();
    policy.deny_caps.retain(|cap| cap != "env");
    policy.default_caps.push("env".to_string());
    let config = PiJsRuntimeConfig {
        // The protected namespaces are deliberately below cwd: ordinary
        // workspace access must not override a denied storage capability.
        cwd: home.display().to_string(),
        env: std::collections::HashMap::from([("HOME".to_string(), home.display().to_string())]),
        disk_cache_dir: None,
        ..Default::default()
    };
    let runtime = common::run_async(JsExtensionRuntimeHandle::start_with_policy(
        config,
        tools,
        manager.clone(),
        policy,
    ))
    .expect("start runtime");
    manager.set_js_runtime(runtime);
    let loading_manager = manager.clone();
    common::run_async(async move { loading_manager.load_js_extensions(specs).await })
        .expect("invalid storage namespace must not block either factory");
    common::run_async(async move {
        assert_eq!(
            manager
                .execute_command("broken-state-check", "", 5000)
                .await
                .unwrap()["display"],
            "denied"
        );
        assert_eq!(
            manager
                .execute_command("healthy-state-check", "", 5000)
                .await
                .unwrap()["display"],
            "persistent"
        );
        assert!(manager.shutdown(std::time::Duration::from_secs(5)).await);
    });
    assert_eq!(
        std::fs::read_to_string(home.join(".pi/healthy-state/state.txt")).unwrap(),
        "persistent"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn npm_peer_without_env_cannot_access_owner_state_under_configured_workspace_home() {
    let temp = tempfile::tempdir().expect("tempdir");
    let base = pi::extensions::safe_canonicalize(temp.path());
    let workspace = base.join("workspace");
    let home = workspace.join("configured-home");
    let state_root = home.join(".pi/owned-state");
    let state_path = state_root.join("counter.txt");
    std::fs::create_dir_all(&state_root).unwrap();
    // Existing state makes a peer loaded before the owner's factory a real
    // disclosure test, rather than an ENOENT failure on an absent directory.
    std::fs::write(&state_path, "0").unwrap();
    let ordinary_path = workspace.join("ordinary.txt");
    std::fs::write(&ordinary_path, "ordinary workspace content").unwrap();

    let owner_package = base.join("npm/node_modules/owned-state");
    let peer_package = base.join("npm/node_modules/state-peer");
    for (package, name, entry) in [
        (&owner_package, "owned-state", "owner.js"),
        (&peer_package, "state-peer", "peer.js"),
    ] {
        std::fs::create_dir_all(package).unwrap();
        std::fs::write(
            package.join("package.json"),
            serde_json::json!({
                "name": name, "type": "module", "pi": {"extensions": [format!("./{entry}")]}
            })
            .to_string(),
        )
        .unwrap();
    }
    let owner_entry = owner_package.join("owner.js");
    std::fs::write(
        &owner_entry,
        r#"
        import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
        import { homedir } from 'node:os';
        import { join } from 'node:path';
        export default function(pi) {
            const root = join(homedir(), '.pi', 'owned-state');
            mkdirSync(root, { recursive: true });
            const path = join(root, 'counter.txt');
            const previous = Number(readFileSync(path, 'utf8'));
            writeFileSync(path, String(previous + 1));
            pi.registerCommand('owned-state-count', {
                handler: async () => ({ display: readFileSync(path, 'utf8') }),
            });
        }
        "#,
    )
    .unwrap();
    let state_json = serde_json::to_string(&state_path).unwrap();
    let ordinary_json = serde_json::to_string(&ordinary_path).unwrap();
    let peer_entry = peer_package.join("peer.js");
    std::fs::write(
        &peer_entry,
        format!(
            r#"
        import {{ readFileSync, writeFileSync }} from 'node:fs';
        export default function(pi) {{
            const state = {state_json};
            const ordinary = {ordinary_json};
            const probes = {{}};
            for (const name of ['__pi_host_check_read_access', '__pi_host_check_write_access',
                '__pi_host_read_file_sync', '__pi_host_persistent_storage_mutate']) {{
                if (typeof globalThis[name] !== 'function') throw new Error('missing native bridge ' + name);
            }}
            for (const [name, operation] of [
                ['read', () => readFileSync(state, 'utf8')],
                ['write', () => writeFileSync(state, 'peer corruption')],
                ['nativeReadCheck', () => globalThis.__pi_host_check_read_access(state)],
                ['nativeWriteCheck', () => globalThis.__pi_host_check_write_access(state)],
                ['nativeRead', () => globalThis.__pi_host_read_file_sync(state)],
                ['nativeWrite', () => globalThis.__pi_host_persistent_storage_mutate('write', state,
                    JSON.stringify({{data: Buffer.from('peer corruption').toString('base64')}}))],
            ]) {{
                try {{ operation(); probes[name] = 'allowed'; }}
                catch (error) {{ probes[name] = String(error && error.message || error); }}
            }}
            const workspaceRead = readFileSync(ordinary, 'utf8');
            const workspaceNativeRead = Buffer.from(globalThis.__pi_host_read_file_sync(ordinary), 'base64').toString('utf8');
            writeFileSync(ordinary + '.peer', 'permitted workspace write');
            const workspaceWrite = readFileSync(ordinary + '.peer', 'utf8');
            pi.registerCommand('state-peer-check', {{ handler: async () => ({{ display: JSON.stringify({{
                probes, workspaceRead, workspaceNativeRead, workspaceWrite,
                homeHidden: process.env.HOME === undefined && pi.env.get('HOME') === undefined,
            }}) }}) }});
        }}
        "#
        ),
    )
    .unwrap();
    let owner_spec = JsExtensionLoadSpec::from_entry_path(owner_entry).unwrap();
    let peer_spec = JsExtensionLoadSpec::from_entry_path(peer_entry).unwrap();
    assert_ne!(owner_spec.extension_id, peer_spec.extension_id);

    let load = || {
        let manager = ExtensionManager::new();
        let tools = Arc::new(ToolRegistry::new(&[], &workspace, None));
        let mut policy = pi::extensions::ExtensionPolicy {
            mode: pi::extensions::ExtensionPolicyMode::Strict,
            deny_caps: Vec::new(),
            default_caps: vec!["read".to_string(), "write".to_string()],
            ..Default::default()
        };
        policy.per_extension.insert(
            owner_spec.extension_id.clone(),
            pi::extensions::ExtensionOverride {
                allow: vec!["env".to_string()],
                ..Default::default()
            },
        );
        policy.per_extension.insert(
            peer_spec.extension_id.clone(),
            pi::extensions::ExtensionOverride {
                deny: vec!["env".to_string()],
                ..Default::default()
            },
        );
        let config = PiJsRuntimeConfig {
            cwd: workspace.display().to_string(),
            env: std::collections::HashMap::from([(
                "HOME".to_string(),
                home.display().to_string(),
            )]),
            disk_cache_dir: None,
            ..Default::default()
        };
        let runtime = common::run_async(JsExtensionRuntimeHandle::start_with_policy(
            config,
            tools,
            manager.clone(),
            policy,
        ))
        .expect("start mixed-policy runtime");
        manager.set_js_runtime(runtime);
        let loading_manager = manager.clone();
        let specs = vec![peer_spec.clone(), owner_spec.clone()];
        common::run_async(async move { loading_manager.load_js_extensions(specs).await })
            .expect("load peer before storage owner with complete boundaries");
        manager
    };

    for expected_count in [1, 2] {
        let manager = load();
        common::run_async(async move {
            let response = manager
                .execute_command("state-peer-check", "", 5000)
                .await
                .unwrap();
            let checks: Value =
                serde_json::from_str(response["display"].as_str().unwrap()).unwrap();
            assert_eq!(checks["homeHidden"], true, "{checks}");
            assert_eq!(checks["workspaceRead"], "ordinary workspace content");
            assert_eq!(checks["workspaceNativeRead"], "ordinary workspace content");
            assert_eq!(checks["workspaceWrite"], "permitted workspace write");
            for (operation, error) in checks["probes"].as_object().unwrap() {
                let error = error.as_str().unwrap();
                assert!(
                    error.contains("denied") || error.contains("not granted"),
                    "{operation} must fail because of its storage boundary: {checks}"
                );
            }
            assert_eq!(
                manager
                    .execute_command("owned-state-count", "", 5000)
                    .await
                    .unwrap()["display"],
                expected_count.to_string(),
                "only the owner can increment its persistent counter"
            );
            assert!(manager.shutdown(std::time::Duration::from_secs(5)).await);
        });
        assert_eq!(
            std::fs::read_to_string(&state_path).unwrap(),
            expected_count.to_string(),
            "owner state survives cold realm reloads without peer corruption"
        );
    }
}

/// Follow context-mode's driver selection and JSON imports through the real
/// compiler/resolver, with repairs disabled. Optional native drivers must not
/// become eager imports when the Bun SQLite branch supplies the real database.
#[test]
#[allow(clippy::too_many_lines)]
fn context_mode_driver_selection_and_encoded_module_paths_load_without_stubs() {
    let harness = common::TestHarness::new("context_mode_module_graph");
    let cwd = harness.temp_dir().to_path_buf();
    let prefix = "extensions/context # % λ";
    harness.create_file(
        &format!("{prefix}/package.json"),
        br#"{"name":"context-mode-loader-fixture","type":"module","pi":{"extensions":["./entry.mjs"]}}"#,
    );
    harness.create_file(
        &format!("{prefix}/driver.mjs"),
        br"
import { createRequire } from 'node:module';
export function openDatabase() {
    const require = createRequire(import.meta.url);
    if (globalThis.Bun) {
        const Database = require(['bun', 'sqlite'].join(':')).Database;
        return new Database(':memory:');
    }
    return new (require('better-sqlite3'))(':memory:');
}
",
    );
    let factory = harness.create_file(
        &format!("{prefix}/factory.cjs"),
        br"
const { basename } = require('node:path');
module.exports = value => ({ value, filename: __filename, directory: __dirname, basename: basename(__filename) });
",
    );
    harness.create_file(&format!("{prefix}/value.cjs"), b"module.exports = 41;\n");
    harness.create_file(
        &format!("{prefix}/side-effect.cjs"),
        b"const path = require ('node:path'); globalThis.loaderSideEffect = path.basename('/fixture/loaded');\n",
    );
    harness.create_file(
        &format!("{prefix}/consumer.cjs"),
        br"
require('./side-effect.cjs');
const createValue = require('./factory.cjs');
const value = require('./value.cjs');
module.exports = createValue(value);
",
    );
    harness.create_file(&format!("{prefix}/prices.json"), br#"{"price":3}"#);
    harness.create_file(
        &format!("{prefix}/helper #%.mjs"),
        b"export const count = 7;\n",
    );
    let entry = harness.create_file(
        &format!("{prefix}/entry.mjs"),
        br"
import { openDatabase } from './driver.mjs';
import importedValue from './consumer.cjs';
import './side-effect.cjs';
import catalog from './prices.json' with { type: 'json' };
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
export default async function (pi) {
    const filename = fileURLToPath(import.meta.url);
    const helperUrl = pathToFileURL(join(dirname(filename), 'helper #%.mjs')).href;
    const { count } = await import(helperUrl);
    const database = openDatabase();
    database.exec('CREATE TABLE usage (count INTEGER NOT NULL)');
    database.query('INSERT INTO usage (count) VALUES (?)').run(count);
    const row = database.query('SELECT count * ? AS cost FROM usage').get(catalog.price);
    database.close();
    const result = { row, importedValue, filename, helperUrl, sideEffect: globalThis.loaderSideEffect };
    pi.registerCommand('loaded-module-result', {
        handler: async () => ({ display: JSON.stringify(result) }),
    });
}
",
    );
    let spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("module fixture load spec");
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        repair_mode: pi::extensions_js::RepairMode::Off,
        disk_cache_dir: None,
        env: std::collections::HashMap::from([("PI_EXT_COMPAT_SCAN".to_string(), "0".to_string())]),
        ..Default::default()
    };
    let runtime = common::run_async(JsExtensionRuntimeHandle::start(
        config,
        tools,
        manager.clone(),
    ))
    .expect("start strict module loader runtime");
    manager.set_js_runtime(runtime);
    let response = common::run_async(async move {
        manager
            .load_js_extensions(vec![spec])
            .await
            .expect("load real module graph without optional native driver");
        let response = manager
            .execute_command("loaded-module-result", "", 5000)
            .await
            .expect("execute command registered after SQLite initialization");
        assert!(manager.shutdown(std::time::Duration::from_secs(5)).await);
        response
    });
    let result: Value = serde_json::from_str(response["display"].as_str().unwrap()).unwrap();
    assert_eq!(result["row"]["cost"], 21, "{result}");
    assert_eq!(result["sideEffect"], "loaded", "{result}");
    assert_eq!(result["importedValue"]["value"], 41, "{result}");
    assert_eq!(result["importedValue"]["basename"], "factory.cjs");
    assert_eq!(
        result["filename"],
        pi::extensions::safe_canonicalize(&entry)
            .display()
            .to_string()
    );
    assert_eq!(
        result["importedValue"]["filename"],
        pi::extensions::safe_canonicalize(&factory)
            .display()
            .to_string()
    );
    assert_eq!(
        result["importedValue"]["directory"],
        pi::extensions::safe_canonicalize(factory.parent().unwrap())
            .display()
            .to_string()
    );
    let helper_url = result["helperUrl"].as_str().unwrap();
    assert!(
        helper_url.contains("%23") && helper_url.contains("%25") && helper_url.contains("%CE%BB"),
        "{helper_url}"
    );
}
