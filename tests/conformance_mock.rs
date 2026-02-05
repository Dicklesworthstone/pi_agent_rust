//! Conformance mock layer for Rust `QuickJS` extension runtime (bd-1bje).
//!
//! Provides:
//! - `ConformanceMockSpec`: JSON-loadable specification for mock session responses
//! - `ConformanceMockSession`: Implements `ExtensionSession` using a mock spec
//! - `HostcallCaptureLog`: Records all hostcall invocations during extension loading
//! - Integration tests: load `.ts` extensions with mock session, capture output as JSON

mod common;

use async_trait::async_trait;
use pi::PiResult;
use pi::extensions::{
    ExtensionManager, ExtensionSession, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::session::SessionMessage;
use pi::tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};

// ─── Mock Spec ───────────────────────────────────────────────────────────────

/// JSON-loadable specification for conformance mock responses.
///
/// Configures what the mock session returns for each operation.
/// Fields are optional — unset fields fall back to sensible defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConformanceMockSpec {
    /// Session state returned by `get_state`.
    #[serde(default)]
    pub session_state: Value,

    /// Messages returned by `get_messages`.
    #[serde(default)]
    pub messages: Vec<Value>,

    /// Entries returned by `get_entries`.
    #[serde(default)]
    pub entries: Vec<Value>,

    /// Branch returned by `get_branch`.
    #[serde(default)]
    pub branch: Vec<Value>,

    /// Initial model (provider, `model_id`).
    #[serde(default)]
    pub model: Option<(String, String)>,

    /// Initial thinking level.
    #[serde(default)]
    pub thinking_level: Option<String>,
}

// ─── Hostcall Capture Log ────────────────────────────────────────────────────

/// A single captured hostcall invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedHostcall {
    /// Operation name (e.g., `get_state`, `set_name`).
    pub op: String,
    /// Payload passed to the operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    /// Result returned (if captured).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

/// Thread-safe log of all hostcall invocations.
#[derive(Debug, Clone, Default)]
pub struct HostcallCaptureLog {
    entries: Arc<Mutex<Vec<CapturedHostcall>>>,
}

#[allow(clippy::must_use_candidate)]
impl HostcallCaptureLog {
    pub fn new() -> Self {
        Self::default()
    }

    fn record(&self, op: &str, payload: Option<Value>, result: Option<Value>) {
        self.entries.lock().unwrap().push(CapturedHostcall {
            op: op.to_string(),
            payload,
            result,
        });
    }

    /// Drain all captured entries and return them.
    pub fn drain(&self) -> Vec<CapturedHostcall> {
        std::mem::take(&mut *self.entries.lock().unwrap())
    }

    /// Return a snapshot of captured entries.
    pub fn snapshot(&self) -> Vec<CapturedHostcall> {
        self.entries.lock().unwrap().clone()
    }

    /// Serialize the capture log to JSON.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self.snapshot()).expect("serialize capture log")
    }
}

// ─── Conformance Mock Session ────────────────────────────────────────────────

/// Mock session that returns configured responses from a `ConformanceMockSpec`
/// and records all invocations to a `HostcallCaptureLog`.
pub struct ConformanceMockSession {
    /// Mutable state — starts from spec, mutated by set_* calls.
    state: Mutex<Value>,
    messages: Mutex<Vec<Value>>,
    entries: Mutex<Vec<Value>>,
    branch: Mutex<Vec<Value>>,
    model: Mutex<(Option<String>, Option<String>)>,
    thinking_level: Mutex<Option<String>>,
    /// Mutations captured for inspection.
    name_history: Mutex<Vec<String>>,
    label_history: Mutex<Vec<(String, Option<String>)>>,
    appended_messages: Mutex<Vec<SessionMessage>>,
    custom_entries: Mutex<Vec<(String, Option<Value>)>>,
    /// Capture log for all operations.
    capture: HostcallCaptureLog,
}

#[allow(clippy::must_use_candidate)]
impl ConformanceMockSession {
    /// Create a new mock session from a specification.
    pub fn new(spec: ConformanceMockSpec, capture: HostcallCaptureLog) -> Self {
        let (provider, model_id) = spec.model.map_or((None, None), |(p, m)| (Some(p), Some(m)));

        Self {
            state: Mutex::new(spec.session_state),
            messages: Mutex::new(spec.messages),
            entries: Mutex::new(spec.entries),
            branch: Mutex::new(spec.branch),
            model: Mutex::new((provider, model_id)),
            thinking_level: Mutex::new(spec.thinking_level),
            name_history: Mutex::new(Vec::new()),
            label_history: Mutex::new(Vec::new()),
            appended_messages: Mutex::new(Vec::new()),
            custom_entries: Mutex::new(Vec::new()),
            capture,
        }
    }

    /// Get the history of names set via `set_name`.
    pub fn name_history(&self) -> Vec<String> {
        self.name_history.lock().unwrap().clone()
    }

    /// Get the history of labels set via `set_label`.
    pub fn label_history(&self) -> Vec<(String, Option<String>)> {
        self.label_history.lock().unwrap().clone()
    }

    /// Get all messages appended via `append_message`.
    pub fn appended_messages(&self) -> Vec<SessionMessage> {
        self.appended_messages.lock().unwrap().clone()
    }

    /// Get all custom entries appended via `append_custom_entry`.
    pub fn custom_entries(&self) -> Vec<(String, Option<Value>)> {
        self.custom_entries.lock().unwrap().clone()
    }
}

#[async_trait]
impl ExtensionSession for ConformanceMockSession {
    async fn get_state(&self) -> Value {
        let result = self.state.lock().unwrap().clone();
        self.capture.record("get_state", None, Some(result.clone()));
        result
    }

    async fn get_messages(&self) -> Vec<SessionMessage> {
        let raw = self.messages.lock().unwrap().clone();
        let parsed: Vec<SessionMessage> = raw
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        self.capture.record(
            "get_messages",
            None,
            Some(serde_json::to_value(&parsed).unwrap_or(Value::Null)),
        );
        parsed
    }

    async fn get_entries(&self) -> Vec<Value> {
        let result = self.entries.lock().unwrap().clone();
        self.capture
            .record("get_entries", None, Some(Value::Array(result.clone())));
        result
    }

    async fn get_branch(&self) -> Vec<Value> {
        let result = self.branch.lock().unwrap().clone();
        self.capture
            .record("get_branch", None, Some(Value::Array(result.clone())));
        result
    }

    async fn set_name(&self, name: String) -> PiResult<()> {
        self.capture
            .record("set_name", Some(serde_json::json!({ "name": name })), None);
        self.name_history.lock().unwrap().push(name.clone());
        // Also update state to reflect the name change
        if let Value::Object(ref mut map) = *self.state.lock().unwrap() {
            map.insert("sessionName".to_string(), Value::String(name));
        }
        Ok(())
    }

    async fn append_message(&self, message: SessionMessage) -> PiResult<()> {
        self.capture
            .record("append_message", serde_json::to_value(&message).ok(), None);
        self.appended_messages.lock().unwrap().push(message);
        Ok(())
    }

    async fn append_custom_entry(&self, custom_type: String, data: Option<Value>) -> PiResult<()> {
        self.capture.record(
            "append_custom_entry",
            Some(serde_json::json!({
                "customType": custom_type,
                "data": data,
            })),
            None,
        );
        self.custom_entries
            .lock()
            .unwrap()
            .push((custom_type, data));
        Ok(())
    }

    async fn set_model(&self, provider: String, model_id: String) -> PiResult<()> {
        self.capture.record(
            "set_model",
            Some(serde_json::json!({
                "provider": provider,
                "modelId": model_id,
            })),
            None,
        );
        *self.model.lock().unwrap() = (Some(provider), Some(model_id));
        Ok(())
    }

    async fn get_model(&self) -> (Option<String>, Option<String>) {
        let result = self.model.lock().unwrap().clone();
        self.capture.record(
            "get_model",
            None,
            Some(serde_json::json!({
                "provider": result.0,
                "modelId": result.1,
            })),
        );
        result
    }

    async fn set_thinking_level(&self, level: String) -> PiResult<()> {
        self.capture.record(
            "set_thinking_level",
            Some(serde_json::json!({ "level": level })),
            None,
        );
        *self.thinking_level.lock().unwrap() = Some(level);
        Ok(())
    }

    async fn get_thinking_level(&self) -> Option<String> {
        let result = self.thinking_level.lock().unwrap().clone();
        self.capture
            .record("get_thinking_level", None, Some(serde_json::json!(result)));
        result
    }

    async fn set_label(&self, target_id: String, label: Option<String>) -> PiResult<()> {
        self.capture.record(
            "set_label",
            Some(serde_json::json!({
                "targetId": target_id,
                "label": label,
            })),
            None,
        );
        self.label_history.lock().unwrap().push((target_id, label));
        Ok(())
    }
}

// ─── Conformance Output Format ───────────────────────────────────────────────

/// Output format for conformance test results.
///
/// This format is designed to match the TS harness output so both runtimes
/// produce identical JSON for diff-based conformance testing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConformanceOutput {
    /// Extension identifier.
    pub extension_id: String,
    /// Extension name (from package.json or derived).
    pub name: String,
    /// Extension version.
    pub version: String,
    /// All captured registrations.
    pub registrations: ConformanceRegistrations,
    /// Hostcall invocations captured during loading.
    pub hostcall_log: Vec<CapturedHostcall>,
}

/// Registrations captured from an extension load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConformanceRegistrations {
    pub commands: Vec<Value>,
    pub shortcuts: Vec<Value>,
    pub flags: Vec<Value>,
    pub providers: Vec<Value>,
    pub tool_defs: Vec<Value>,
    pub models: Vec<Value>,
    pub event_hooks: Vec<String>,
}

/// Build conformance output from a loaded `ExtensionManager` and capture log.
fn build_conformance_output(
    spec: &JsExtensionLoadSpec,
    manager: &ExtensionManager,
    capture: &HostcallCaptureLog,
) -> ConformanceOutput {
    let models: Vec<Value> = manager
        .extension_model_entries()
        .into_iter()
        .map(|entry| serde_json::to_value(entry.model).expect("model to json"))
        .collect();

    // Event hooks are per-extension (no aggregate list API on ExtensionManager),
    // so we leave this empty for now — the differential runner will populate it
    // from the snapshot when needed.
    let event_hooks: Vec<String> = Vec::new();

    ConformanceOutput {
        extension_id: spec.extension_id.clone(),
        name: spec.name.clone(),
        version: spec.version.clone(),
        registrations: ConformanceRegistrations {
            commands: manager.list_commands(),
            shortcuts: manager.list_shortcuts(),
            flags: manager.list_flags(),
            providers: manager.extension_providers(),
            tool_defs: manager.extension_tool_defs(),
            models,
            event_hooks,
        },
        hostcall_log: capture.snapshot(),
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Load a TypeScript extension with a conformance mock session.
///
/// Returns the manager, the load spec, and the capture log.
fn load_with_mock(
    harness: &common::TestHarness,
    source: &str,
    filename: &str,
    spec: ConformanceMockSpec,
) -> (ExtensionManager, JsExtensionLoadSpec, HostcallCaptureLog) {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_path = harness.create_file(format!("extensions/{filename}"), source.as_bytes());
    harness.record_artifact(format!("extensions/{filename}"), &ext_path);

    let load_spec = JsExtensionLoadSpec::from_entry_path(&ext_path).expect("load spec");

    let capture = HostcallCaptureLog::new();
    let session = Arc::new(ConformanceMockSession::new(spec, capture.clone()));

    let manager = ExtensionManager::new();
    manager.set_session(session);

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
        let spec = load_spec.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load extension with mock");
        }
    });

    (manager, load_spec, capture)
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

// ─── Extension Sources ───────────────────────────────────────────────────────

/// Minimal TypeScript extension that registers a command.
const HELLO_TS: &str = r#"
export default function init(pi: any): void {
  pi.registerCommand("hello", {
    description: "A simple greeting command",
    handler: async (args: string): Promise<{ display: string }> => {
      return { display: "Hello from conformance!" };
    }
  });
}
"#;

/// Extension that registers a tool (the standard hello.ts pattern).
const HELLO_TOOL_TS: &str = r#"
export default function init(pi: any): void {
  pi.registerTool({
    name: "hello",
    label: "Hello",
    description: "A simple greeting tool",
    parameters: {
      type: "object",
      properties: {
        name: { type: "string", description: "Name to greet" }
      },
      required: ["name"]
    },
    execute: async (_toolCallId: string, params: any) => {
      return {
        content: [{ type: "text", text: "Hello, " + params.name + "!" }],
        details: { greeted: params.name }
      };
    }
  });
}
"#;

/// Extension that exercises session hostcalls during init.
const SESSION_CALLING_TS: &str = r#"
export default function init(pi: any): void {
  // Register a command
  pi.registerCommand("session-test", {
    description: "Tests session access",
    handler: async (args: string, ctx: any) => {
      // These session calls happen at command execution time, not load time
      const state = await pi.session("get_state", {});
      return { display: "Session state: " + JSON.stringify(state) };
    }
  });

  // Register a flag
  pi.registerFlag("session-flag", {
    type: "boolean",
    description: "Session test flag",
    default: false
  });
}
"#;

/// Extension with multiple registration types.
const MULTI_REG_TS: &str = r#"
interface CommandResult {
  display: string;
}

export default function init(pi: any): void {
  pi.registerCommand("multi-cmd", {
    description: "Multi-registration command",
    handler: async (): Promise<CommandResult> => ({ display: "multi" })
  });

  pi.registerShortcut("ctrl+m", {
    description: "Multi shortcut",
    handler: async (): Promise<CommandResult> => ({ display: "shortcut" })
  });

  pi.registerFlag("multi-flag", {
    type: "string",
    description: "Multi flag",
    default: "default-val"
  });

  pi.registerProvider("multi-provider", {
    baseUrl: "https://api.multi.test/v1",
    apiKey: "MULTI_KEY",
    api: "openai-completions",
    models: [{
      id: "multi-model",
      name: "Multi Model",
      contextWindow: 8000,
      maxTokens: 1024,
      input: ["text"]
    }]
  });
}
"#;

// ─── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn mock_session_compiles_and_default_spec() {
    let _harness = common::TestHarness::new("mock_session_compiles_and_default_spec");
    let capture = HostcallCaptureLog::new();
    let spec = ConformanceMockSpec::default();
    let session = ConformanceMockSession::new(spec, capture.clone());

    // Verify default state
    let state = common::run_async({
        let session = Arc::new(session);
        async move { session.get_state().await }
    });
    assert_eq!(state, Value::Null, "default state should be Null");

    let log = capture.snapshot();
    assert_eq!(log.len(), 1, "one hostcall should be captured");
    assert_eq!(log[0].op, "get_state");
}

#[test]
fn mock_session_returns_configured_state() {
    let _harness = common::TestHarness::new("mock_session_returns_configured_state");
    let capture = HostcallCaptureLog::new();
    let spec = ConformanceMockSpec {
        session_state: serde_json::json!({
            "sessionName": "test-session",
            "sessionFile": "/tmp/test.jsonl",
        }),
        model: Some(("anthropic".to_string(), "claude-sonnet-4-5".to_string())),
        thinking_level: Some("medium".to_string()),
        ..Default::default()
    };
    let session = Arc::new(ConformanceMockSession::new(spec, capture.clone()));

    let state = common::run_async({
        let s = Arc::clone(&session);
        async move { s.get_state().await }
    });
    assert_eq!(
        state.get("sessionName").and_then(|v| v.as_str()),
        Some("test-session")
    );

    let model = common::run_async({
        let s = Arc::clone(&session);
        async move { s.get_model().await }
    });
    assert_eq!(model.0.as_deref(), Some("anthropic"));
    assert_eq!(model.1.as_deref(), Some("claude-sonnet-4-5"));

    let level = common::run_async({
        let s = Arc::clone(&session);
        async move { s.get_thinking_level().await }
    });
    assert_eq!(level.as_deref(), Some("medium"));

    let log = capture.snapshot();
    assert_eq!(log.len(), 3, "three hostcalls captured");
}

#[test]
fn mock_session_captures_mutations() {
    let _harness = common::TestHarness::new("mock_session_captures_mutations");
    let capture = HostcallCaptureLog::new();
    let spec = ConformanceMockSpec {
        session_state: serde_json::json!({}),
        ..Default::default()
    };
    let session = Arc::new(ConformanceMockSession::new(spec, capture.clone()));

    common::run_async({
        let s = Arc::clone(&session);
        async move {
            s.set_name("new-name".to_string()).await.unwrap();
            s.set_model("openai".to_string(), "gpt-4".to_string())
                .await
                .unwrap();
            s.set_thinking_level("high".to_string()).await.unwrap();
            s.set_label("msg-123".to_string(), Some("important".to_string()))
                .await
                .unwrap();
        }
    });

    assert_eq!(session.name_history(), vec!["new-name"]);
    assert_eq!(
        session.label_history(),
        vec![("msg-123".to_string(), Some("important".to_string()))]
    );

    let log = capture.snapshot();
    assert_eq!(log.len(), 4, "four mutations captured");
    assert_eq!(log[0].op, "set_name");
    assert_eq!(log[1].op, "set_model");
    assert_eq!(log[2].op, "set_thinking_level");
    assert_eq!(log[3].op, "set_label");
}

#[test]
fn mock_spec_deserializes_from_json() {
    let _harness = common::TestHarness::new("mock_spec_deserializes_from_json");
    let json = serde_json::json!({
        "session_state": { "sessionName": "from-json" },
        "model": ["anthropic", "claude-opus-4-5"],
        "thinking_level": "high",
        "messages": [],
        "entries": [{ "type": "custom", "data": "test" }]
    });

    let spec: ConformanceMockSpec = serde_json::from_value(json).expect("deserialize mock spec");
    assert_eq!(
        spec.session_state
            .get("sessionName")
            .and_then(|v| v.as_str()),
        Some("from-json")
    );
    assert_eq!(
        spec.model,
        Some(("anthropic".to_string(), "claude-opus-4-5".to_string()))
    );
    assert_eq!(spec.thinking_level.as_deref(), Some("high"));
    assert_eq!(spec.entries.len(), 1);
}

#[test]
fn capture_log_serializes_to_json() {
    let _harness = common::TestHarness::new("capture_log_serializes_to_json");
    let capture = HostcallCaptureLog::new();

    capture.record("get_state", None, Some(serde_json::json!({})));
    capture.record(
        "set_name",
        Some(serde_json::json!({ "name": "test" })),
        None,
    );

    let json = capture.to_json();
    assert!(json.is_array());
    assert_eq!(json.as_array().unwrap().len(), 2);
    assert_eq!(json[0]["op"].as_str(), Some("get_state"));
    assert_eq!(json[1]["op"].as_str(), Some("set_name"));
}

#[test]
fn hello_ts_loads_with_mock_session() {
    let harness = common::TestHarness::new("hello_ts_loads_with_mock_session");
    harness
        .log()
        .info("mock", "Loading hello.ts with mock session");

    let spec = ConformanceMockSpec {
        session_state: serde_json::json!({
            "sessionName": "conformance-test",
        }),
        ..Default::default()
    };

    let (manager, load_spec, capture) = load_with_mock(&harness, HELLO_TS, "hello.ts", spec);

    // Verify registrations captured
    assert!(
        manager.has_command("hello"),
        "hello command should be registered"
    );
    let commands = manager.list_commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].get("description").and_then(|v| v.as_str()),
        Some("A simple greeting command")
    );

    // Build conformance output
    let output = build_conformance_output(&load_spec, &manager, &capture);

    harness
        .log()
        .info_ctx("mock", "Conformance output built", |ctx| {
            ctx.push(("extension_id".into(), output.extension_id.clone()));
            ctx.push((
                "commands".into(),
                output.registrations.commands.len().to_string(),
            ));
        });

    assert_eq!(output.extension_id, "hello");
    assert_eq!(output.registrations.commands.len(), 1);

    // Serialize output to JSON
    let json_str = serde_json::to_string_pretty(&output).expect("serialize conformance output");
    let output_path = harness.temp_path("conformance_output.json");
    std::fs::write(&output_path, format!("{json_str}\n")).expect("write output");
    harness.record_artifact("conformance_output.json", &output_path);

    harness
        .log()
        .info("mock", "hello.ts loaded with mock session successfully");
    write_jsonl_artifacts(&harness);
}

#[test]
fn hello_tool_ts_captures_tool_defs() {
    let harness = common::TestHarness::new("hello_tool_ts_captures_tool_defs");
    let spec = ConformanceMockSpec::default();

    let (manager, load_spec, capture) =
        load_with_mock(&harness, HELLO_TOOL_TS, "hello_tool.ts", spec);

    let output = build_conformance_output(&load_spec, &manager, &capture);

    assert_eq!(output.extension_id, "hello_tool");
    assert_eq!(
        output.registrations.tool_defs.len(),
        1,
        "expected 1 tool_def"
    );
    let tool_def = &output.registrations.tool_defs[0];
    assert_eq!(tool_def.get("name").and_then(|v| v.as_str()), Some("hello"));
    assert_eq!(
        tool_def.get("description").and_then(|v| v.as_str()),
        Some("A simple greeting tool")
    );

    // Verify JSON round-trip
    let json_str = serde_json::to_string_pretty(&output).expect("serialize");
    let parsed: ConformanceOutput = serde_json::from_str(&json_str).expect("deserialize");
    assert_eq!(parsed.extension_id, "hello_tool");
    assert_eq!(parsed.registrations.tool_defs.len(), 1);

    write_jsonl_artifacts(&harness);
}

#[test]
fn session_calling_extension_loads_cleanly() {
    let harness = common::TestHarness::new("session_calling_extension_loads_cleanly");
    let spec = ConformanceMockSpec {
        session_state: serde_json::json!({
            "sessionName": "mock-session",
            "cwd": "/tmp/test",
        }),
        ..Default::default()
    };

    let (manager, _load_spec, _capture) =
        load_with_mock(&harness, SESSION_CALLING_TS, "session_test.ts", spec);

    // The extension registers a command and flag at load time (no session calls during init)
    assert!(manager.has_command("session-test"));
    let flags = manager.list_flags();
    assert_eq!(flags.len(), 1);
    assert_eq!(
        flags[0].get("name").and_then(|v| v.as_str()),
        Some("session-flag")
    );

    write_jsonl_artifacts(&harness);
}

#[test]
#[allow(clippy::too_many_lines)]
fn multi_registration_captures_all_types() {
    let harness = common::TestHarness::new("multi_registration_captures_all_types");
    let spec = ConformanceMockSpec::default();

    let (manager, load_spec, capture) = load_with_mock(&harness, MULTI_REG_TS, "multi.ts", spec);

    let output = build_conformance_output(&load_spec, &manager, &capture);

    // Verify all registration types captured
    assert_eq!(output.registrations.commands.len(), 1, "expected 1 command");
    assert_eq!(
        output.registrations.shortcuts.len(),
        1,
        "expected 1 shortcut"
    );
    assert_eq!(output.registrations.flags.len(), 1, "expected 1 flag");
    assert_eq!(
        output.registrations.providers.len(),
        1,
        "expected 1 provider"
    );
    assert_eq!(output.registrations.models.len(), 1, "expected 1 model");

    // Verify command
    assert_eq!(
        output.registrations.commands[0]
            .get("name")
            .and_then(|v| v.as_str()),
        Some("multi-cmd")
    );

    // Verify shortcut
    assert_eq!(
        output.registrations.shortcuts[0]
            .get("key_id")
            .and_then(|v| v.as_str()),
        Some("ctrl+m")
    );

    // Verify flag
    assert_eq!(
        output.registrations.flags[0]
            .get("name")
            .and_then(|v| v.as_str()),
        Some("multi-flag")
    );
    assert_eq!(
        output.registrations.flags[0]
            .get("default")
            .and_then(|v| v.as_str()),
        Some("default-val")
    );

    // Verify provider
    assert_eq!(
        output.registrations.providers[0]
            .get("id")
            .and_then(|v| v.as_str()),
        Some("multi-provider")
    );

    // Verify model
    assert_eq!(
        output.registrations.models[0]
            .get("id")
            .and_then(|v| v.as_str()),
        Some("multi-model")
    );

    // Serialize full output
    let json_str = serde_json::to_string_pretty(&output).expect("serialize");
    let output_path = harness.temp_path("multi_conformance_output.json");
    std::fs::write(&output_path, format!("{json_str}\n")).expect("write");
    harness.record_artifact("multi_conformance_output.json", &output_path);

    harness
        .log()
        .info_ctx("mock", "Multi-registration conformance output", |ctx| {
            ctx.push(("commands".into(), "1".to_string()));
            ctx.push(("shortcuts".into(), "1".to_string()));
            ctx.push(("flags".into(), "1".to_string()));
            ctx.push(("providers".into(), "1".to_string()));
            ctx.push(("models".into(), "1".to_string()));
            ctx.push(("json_bytes".into(), json_str.len().to_string()));
        });

    write_jsonl_artifacts(&harness);
}

#[test]
fn conformance_output_json_round_trips() {
    let harness = common::TestHarness::new("conformance_output_json_round_trips");
    let spec = ConformanceMockSpec::default();

    let (manager, load_spec, capture) =
        load_with_mock(&harness, MULTI_REG_TS, "roundtrip.ts", spec);

    let output = build_conformance_output(&load_spec, &manager, &capture);
    let json_str = serde_json::to_string_pretty(&output).expect("serialize");
    let parsed: ConformanceOutput =
        serde_json::from_str(&json_str).expect("deserialize round-trip");

    assert_eq!(parsed.extension_id, output.extension_id);
    assert_eq!(parsed.name, output.name);
    assert_eq!(parsed.version, output.version);
    assert_eq!(
        parsed.registrations.commands.len(),
        output.registrations.commands.len()
    );
    assert_eq!(
        parsed.registrations.shortcuts.len(),
        output.registrations.shortcuts.len()
    );
    assert_eq!(
        parsed.registrations.flags.len(),
        output.registrations.flags.len()
    );
    assert_eq!(
        parsed.registrations.providers.len(),
        output.registrations.providers.len()
    );
    assert_eq!(
        parsed.registrations.models.len(),
        output.registrations.models.len()
    );

    write_jsonl_artifacts(&harness);
}

// ─── Scenario Execution: Event Dispatch ──────────────────────────────────────

/// Extension that registers event hooks.
const EVENT_HOOK_TS: &str = r#"
export default function init(pi: any): void {
  pi.events.on("tool_call", (payload: any, _ctx: any) => {
    return { block: false };
  });

  pi.events.on("turn_start", (payload: any, _ctx: any) => {
    // Track turn start events
  });
}
"#;

#[test]
fn event_hook_extension_registers_hooks() {
    let harness = common::TestHarness::new("event_hook_extension_registers_hooks");
    let spec = ConformanceMockSpec::default();

    let (manager, load_spec, capture) =
        load_with_mock(&harness, EVENT_HOOK_TS, "event_hooks.ts", spec);

    let output = build_conformance_output(&load_spec, &manager, &capture);

    // Verify the extension loaded
    assert_eq!(output.extension_id, "event_hooks");

    // Serialize for artifact logging
    let json_str = serde_json::to_string_pretty(&output).expect("serialize");
    let output_path = harness.temp_path("event_hook_output.json");
    std::fs::write(&output_path, format!("{json_str}\n")).expect("write");
    harness.record_artifact("event_hook_output.json", &output_path);

    write_jsonl_artifacts(&harness);
}

#[test]
fn event_dispatch_through_manager() {
    use pi::extensions::ExtensionEventName;

    let harness = common::TestHarness::new("event_dispatch_through_manager");
    let spec = ConformanceMockSpec {
        session_state: serde_json::json!({
            "sessionName": "event-test",
        }),
        ..Default::default()
    };

    let (manager, _load_spec, capture) =
        load_with_mock(&harness, EVENT_HOOK_TS, "event_dispatch.ts", spec);

    // Dispatch a turn_start event through the manager
    let result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event(
                    ExtensionEventName::TurnStart,
                    Some(serde_json::json!({ "turn": 1 })),
                )
                .await
        }
    });

    // Event dispatch should succeed (handler registered for turn_start)
    assert!(result.is_ok(), "dispatch_event should succeed: {result:?}");

    // Capture log records hostcalls made during event dispatch
    let log = capture.snapshot();
    harness
        .log()
        .info_ctx("event", "Event dispatch completed", |ctx| {
            ctx.push(("hostcalls".into(), log.len().to_string()));
        });

    write_jsonl_artifacts(&harness);
}

// ─── Scenario Execution: Session Mutations ───────────────────────────────────

/// Extension that mutates session state during init.
const SESSION_MUTATING_TS: &str = r#"
export default async function init(pi: any): Promise<void> {
  const state = await pi.session("get_state", {});
  await pi.session("set_name", { name: "mutated-by-extension" });
  await pi.session("set_label", { targetId: "init-label", label: "loaded" });

  pi.registerCommand("mutator", {
    description: "Extension that mutates session",
    handler: async () => ({ display: "mutated" })
  });
}
"#;

#[test]
fn session_mutation_scenario() {
    let harness = common::TestHarness::new("session_mutation_scenario");
    let spec = ConformanceMockSpec {
        session_state: serde_json::json!({
            "sessionName": "original",
            "cwd": "/test",
        }),
        ..Default::default()
    };

    let capture = HostcallCaptureLog::new();
    let session = Arc::new(ConformanceMockSession::new(spec, capture.clone()));

    let cwd = harness.temp_dir().to_path_buf();
    let ext_path = harness.create_file(
        "extensions/session_mutating.ts",
        SESSION_MUTATING_TS.as_bytes(),
    );

    let load_spec = JsExtensionLoadSpec::from_entry_path(&ext_path).expect("load spec");

    let manager = ExtensionManager::new();
    manager.set_session(Arc::clone(&session) as Arc<dyn ExtensionSession>);

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
        let spec = load_spec.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load extension");
        }
    });

    // Verify session mutations happened
    let name_history = session.name_history();
    assert!(
        name_history.contains(&"mutated-by-extension".to_string()),
        "set_name should have been called: {name_history:?}"
    );

    let label_history = session.label_history();
    assert!(
        label_history.iter().any(|(id, _)| id == "init-label"),
        "set_label should have been called: {label_history:?}"
    );

    // Verify hostcall log captured the sequence
    let log = capture.snapshot();
    let ops: Vec<&str> = log.iter().map(|c| c.op.as_str()).collect();
    assert!(ops.contains(&"get_state"), "get_state should be in hostcall log: {ops:?}");
    assert!(ops.contains(&"set_name"), "set_name should be in hostcall log: {ops:?}");
    assert!(ops.contains(&"set_label"), "set_label should be in hostcall log: {ops:?}");

    // Verify the command was registered
    assert!(manager.has_command("mutator"), "mutator command should be registered");

    let output = build_conformance_output(&load_spec, &manager, &capture);
    let json_str = serde_json::to_string_pretty(&output).expect("serialize");
    let output_path = harness.temp_path("session_mutation_output.json");
    std::fs::write(&output_path, format!("{json_str}\n")).expect("write");
    harness.record_artifact("session_mutation_output.json", &output_path);

    write_jsonl_artifacts(&harness);
}

// ─── Scenario Execution: Custom Entry ────────────────────────────────────────

/// Extension that exercises custom entry append during init.
const CUSTOM_ENTRY_TS: &str = r#"
export default async function init(pi: any): Promise<void> {
  await pi.session("append_entry", {
    customType: "audit_log",
    data: { action: "extension_loaded", extension: "custom_entry" }
  });

  pi.registerCommand("audit", {
    description: "Extension with custom entries",
    handler: async () => ({ display: "audited" })
  });
}
"#;

#[test]
fn custom_entry_scenario() {
    let harness = common::TestHarness::new("custom_entry_scenario");
    let spec = ConformanceMockSpec::default();

    let capture = HostcallCaptureLog::new();
    let session = Arc::new(ConformanceMockSession::new(spec, capture.clone()));

    let cwd = harness.temp_dir().to_path_buf();
    let ext_path =
        harness.create_file("extensions/custom_entry.ts", CUSTOM_ENTRY_TS.as_bytes());

    let load_spec = JsExtensionLoadSpec::from_entry_path(&ext_path).expect("load spec");

    let manager = ExtensionManager::new();
    manager.set_session(Arc::clone(&session) as Arc<dyn ExtensionSession>);

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
        let spec = load_spec.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load extension");
        }
    });

    // Verify custom entry was appended
    let entries = session.custom_entries();
    assert_eq!(entries.len(), 1, "expected 1 custom entry: {entries:?}");
    assert_eq!(entries[0].0, "audit_log");
    let data = entries[0].1.as_ref().expect("entry should have data");
    assert_eq!(data["action"], "extension_loaded");

    // Verify hostcall log
    let log = capture.snapshot();
    let ops: Vec<&str> = log.iter().map(|c| c.op.as_str()).collect();
    assert!(
        ops.contains(&"append_custom_entry"),
        "append_custom_entry should be in log: {ops:?}"
    );

    assert!(manager.has_command("audit"));

    write_jsonl_artifacts(&harness);
}

// ─── Scenario Execution: Model Control ──────────────────────────────────────

/// Extension that reads and modifies model settings during init.
const MODEL_CONTROL_TS: &str = r#"
export default async function init(pi: any): Promise<void> {
  const model = await pi.session("get_model", {});
  await pi.session("set_model", { provider: "openai", modelId: "gpt-4o" });
  const level = await pi.session("get_thinking_level", {});
  await pi.session("set_thinking_level", { level: "high" });

  pi.registerCommand("model-control", {
    description: "Extension that controls model settings",
    handler: async () => ({ display: "controlled" })
  });
}
"#;

#[test]
fn model_control_scenario() {
    let harness = common::TestHarness::new("model_control_scenario");
    let spec = ConformanceMockSpec {
        model: Some(("anthropic".to_string(), "claude-sonnet-4-5".to_string())),
        thinking_level: Some("medium".to_string()),
        ..Default::default()
    };

    let capture = HostcallCaptureLog::new();
    let session = Arc::new(ConformanceMockSession::new(spec, capture.clone()));

    let cwd = harness.temp_dir().to_path_buf();
    let ext_path =
        harness.create_file("extensions/model_control.ts", MODEL_CONTROL_TS.as_bytes());

    let load_spec = JsExtensionLoadSpec::from_entry_path(&ext_path).expect("load spec");

    let manager = ExtensionManager::new();
    manager.set_session(Arc::clone(&session) as Arc<dyn ExtensionSession>);

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
        let spec = load_spec.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load extension");
        }
    });

    // Verify hostcall sequence
    let log = capture.snapshot();
    let ops: Vec<&str> = log.iter().map(|c| c.op.as_str()).collect();

    assert!(ops.contains(&"get_model"), "get_model in log: {ops:?}");
    assert!(ops.contains(&"set_model"), "set_model in log: {ops:?}");
    assert!(ops.contains(&"get_thinking_level"), "get_thinking_level in log: {ops:?}");
    assert!(ops.contains(&"set_thinking_level"), "set_thinking_level in log: {ops:?}");

    // Verify model was changed
    let model = common::run_async({
        let s = Arc::clone(&session);
        async move { s.get_model().await }
    });
    assert_eq!(model.0.as_deref(), Some("openai"));
    assert_eq!(model.1.as_deref(), Some("gpt-4o"));

    // Verify thinking level was changed
    let level = common::run_async({
        let s = Arc::clone(&session);
        async move { s.get_thinking_level().await }
    });
    assert_eq!(level.as_deref(), Some("high"));

    assert!(manager.has_command("model-control"));

    let output = build_conformance_output(&load_spec, &manager, &capture);
    let json_str = serde_json::to_string_pretty(&output).expect("serialize");
    let output_path = harness.temp_path("model_control_output.json");
    std::fs::write(&output_path, format!("{json_str}\n")).expect("write");
    harness.record_artifact("model_control_output.json", &output_path);

    write_jsonl_artifacts(&harness);
}
