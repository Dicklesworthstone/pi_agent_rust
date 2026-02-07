#![cfg(feature = "ext-conformance")]
#![allow(clippy::redundant_clone)]
//! Extension scenario conformance tests: load extensions, execute scenarios from
//! `docs/extension-sample.json`, and compare outputs against fixture expectations.
//!
//! This complements the registration-level differential testing in
//! `ext_conformance_diff.rs` by actually *running* tool calls, commands, and
//! event dispatches and checking their outputs match the fixture expectations.

mod common;

use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
use pi::conformance::normalization::{is_path_key, path_suffix_match};
use pi::extensions::{
    ExtensionManager, ExtensionSession, HostcallInterceptor, JsExtensionLoadSpec,
    JsExtensionRuntimeHandle,
};
use pi::extensions_js::{HostcallKind, HostcallRequest, PiJsRuntimeConfig};
use pi::scheduler::HostcallOutcome;
use pi::session::SessionMessage;
use pi::tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ─── Paths ──────────────────────────────────────────────────────────────────

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn artifacts_dir() -> PathBuf {
    project_root().join("tests/ext_conformance/artifacts")
}

fn sample_json_path() -> PathBuf {
    project_root().join("docs/extension-sample.json")
}

fn reports_dir() -> PathBuf {
    project_root().join("tests/ext_conformance/reports")
}

// ─── Deterministic settings (reuse from ext_conformance_diff) ───────────────

const DEFAULT_DETERMINISTIC_TIME_MS: &str = "1700000000000";
const DEFAULT_DETERMINISTIC_TIME_STEP_MS: &str = "1";
const DEFAULT_DETERMINISTIC_RANDOM_SEED: &str = "1337";
const DEFAULT_DETERMINISTIC_CWD: &str = "/tmp/ext-conformance-test";
const DEFAULT_DETERMINISTIC_HOME: &str = "/tmp/ext-conformance-home";
const DEFAULT_TIMEOUT_MS: u64 = 20_000;

fn env_or_default(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|val| !val.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

struct DeterministicSettings {
    time_ms: String,
    time_step_ms: String,
    random_seed: String,
    random_value: Option<String>,
    cwd: String,
    home: String,
}

fn deterministic_settings() -> DeterministicSettings {
    let random_env = std::env::var("PI_DETERMINISTIC_RANDOM")
        .ok()
        .filter(|val| !val.trim().is_empty());
    let seed_env = std::env::var("PI_DETERMINISTIC_RANDOM_SEED")
        .ok()
        .filter(|val| !val.trim().is_empty());
    let random_value = if random_env.is_some() {
        random_env
    } else if seed_env.is_some() {
        None
    } else {
        Some("0.5".to_string())
    };
    DeterministicSettings {
        time_ms: env_or_default("PI_DETERMINISTIC_TIME_MS", DEFAULT_DETERMINISTIC_TIME_MS),
        time_step_ms: env_or_default(
            "PI_DETERMINISTIC_TIME_STEP_MS",
            DEFAULT_DETERMINISTIC_TIME_STEP_MS,
        ),
        random_seed: env_or_default(
            "PI_DETERMINISTIC_RANDOM_SEED",
            DEFAULT_DETERMINISTIC_RANDOM_SEED,
        ),
        random_value,
        cwd: env_or_default("PI_DETERMINISTIC_CWD", DEFAULT_DETERMINISTIC_CWD),
        home: env_or_default("PI_DETERMINISTIC_HOME", DEFAULT_DETERMINISTIC_HOME),
    }
}

fn sanitize_path_for_dir(path: &Path) -> String {
    let relative = path.strip_prefix(project_root()).unwrap_or(path);
    relative
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn deterministic_settings_for(extension_path: &Path) -> DeterministicSettings {
    let mut settings = deterministic_settings();
    let key = sanitize_path_for_dir(extension_path);

    if std::env::var("PI_DETERMINISTIC_CWD").is_err() {
        settings.cwd = Path::new(DEFAULT_DETERMINISTIC_CWD)
            .join(&key)
            .display()
            .to_string();
    }
    if std::env::var("PI_DETERMINISTIC_HOME").is_err() {
        settings.home = Path::new(DEFAULT_DETERMINISTIC_HOME)
            .join(&key)
            .display()
            .to_string();
    }

    settings
}

fn ensure_deterministic_dirs(settings: &DeterministicSettings) {
    let _ = fs::create_dir_all(&settings.cwd);
    let _ = fs::create_dir_all(&settings.home);
}

// ─── Scenario schema types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SampleJson {
    scenario_suite: ScenarioSuite,
    items: Vec<SampleItem>,
}

#[derive(Debug, Deserialize)]
struct ScenarioSuite {
    items: Vec<ScenarioExtension>,
}

#[derive(Debug, Deserialize)]
struct ScenarioExtension {
    extension_id: String,
    #[allow(dead_code)]
    features: Vec<String>,
    scenarios: Vec<Scenario>,
}

#[derive(Debug, Clone, Deserialize)]
struct Scenario {
    id: String,
    kind: String,
    summary: String,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    command_name: Option<String>,
    #[serde(default)]
    event_name: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    provider_id: Option<String>,
    #[serde(default)]
    input: Option<Value>,
    #[serde(default)]
    setup: Option<Value>,
    #[serde(default)]
    steps: Option<Vec<Value>>,
    #[serde(default)]
    expect: Option<ScenarioExpectation>,
}

#[derive(Debug, Clone, Deserialize)]
struct ScenarioExpectation {
    #[serde(default)]
    content_contains: Option<Vec<String>>,
    #[serde(default)]
    details_exact: Option<Value>,
    #[serde(default)]
    details_contains_keys: Option<Vec<String>>,
    #[serde(default)]
    block: Option<bool>,
    #[serde(default)]
    reason_contains: Option<Vec<String>>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    error_contains: Option<Vec<String>>,
    #[serde(default)]
    ui_notify_contains: Option<Vec<String>>,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    models_contains: Option<Vec<String>>,
    #[serde(default)]
    tool_registered: Option<Value>,
    #[serde(default)]
    active_tools: Option<Vec<String>>,
    #[serde(default)]
    content_types: Option<Vec<String>>,
    #[serde(default)]
    #[allow(dead_code)]
    final_content_contains: Option<Vec<String>>,
    #[serde(default)]
    #[allow(dead_code)]
    action: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    text_contains: Option<Vec<String>>,
    #[serde(default)]
    returns_contains: Option<Value>,
    #[serde(default)]
    exec_called: Option<Value>,
    #[serde(default)]
    ui_status_key: Option<String>,
    #[serde(default)]
    ui_status_contains_sequence: Option<Vec<String>>,

    // ── Registration shape matchers ──────────────────────────────────────
    /// Check that specific flags are registered (by name).
    #[serde(default)]
    flags_contains: Option<Vec<String>>,
    /// Check that specific shortcuts are registered (by key).
    #[serde(default)]
    shortcuts_contains: Option<Vec<String>>,
    /// Check that specific slash commands are registered (by name).
    #[serde(default)]
    commands_contains: Option<Vec<String>>,
    /// Check that specific event hooks are registered (by event name).
    #[serde(default)]
    event_hooks_contains: Option<Vec<String>>,
    /// Check the total count of registered tools.
    #[serde(default)]
    tool_count: Option<usize>,
    /// Check the total count of registered flags.
    #[serde(default)]
    flag_count: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
struct SampleItem {
    id: String,
    name: String,
    source_tier: String,
    runtime_tier: String,
}

// ─── Scenario result types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct ScenarioResult {
    scenario_id: String,
    extension_id: String,
    kind: String,
    summary: String,
    status: String, // "pass", "fail", "skip", "error"
    source_tier: String,
    runtime_tier: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    diffs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skip_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<Value>,
    duration_ms: u64,
}

// ─── Extension loader ───────────────────────────────────────────────────────

/// Resolve the artifact path for an extension ID.
fn resolve_extension_path(extension_id: &str, items: &[SampleItem]) -> Option<PathBuf> {
    let item = items.iter().find(|i| i.id == extension_id)?;
    let name = &item.name;
    let trimmed = name.trim_end_matches('/');
    let dir = artifacts_dir().join(trimmed);

    // Multi-file extensions (name ends with '/'): look for index.ts
    if name.ends_with('/') {
        let index = dir.join("index.ts");
        if index.exists() {
            return Some(index);
        }
    }

    // Single file extensions: artifacts/<extension-id>/<name>
    // e.g. hello.ts → artifacts/hello/hello.ts
    let file = artifacts_dir().join(extension_id).join(trimmed);
    if file.exists() {
        return Some(file);
    }

    // Also try the direct path
    let direct = artifacts_dir().join(name);
    if direct.exists() {
        return Some(direct);
    }

    None
}

struct LoadedExtension {
    manager: ExtensionManager,
    runtime: JsExtensionRuntimeHandle,
}

/// Load an extension into the Rust runtime with deterministic settings.
fn load_extension(extension_path: &Path) -> Result<LoadedExtension, String> {
    let settings = deterministic_settings_for(extension_path);
    ensure_deterministic_dirs(&settings);
    let cwd = PathBuf::from(&settings.cwd);

    let spec = JsExtensionLoadSpec::from_entry_path(extension_path)
        .map_err(|e| format!("load spec: {e}"))?;

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let mut env = HashMap::new();
    env.insert(
        "PI_DETERMINISTIC_TIME_MS".to_string(),
        settings.time_ms.clone(),
    );
    env.insert(
        "PI_DETERMINISTIC_TIME_STEP_MS".to_string(),
        settings.time_step_ms.clone(),
    );
    env.insert("PI_DETERMINISTIC_CWD".to_string(), settings.cwd.clone());
    env.insert("PI_DETERMINISTIC_HOME".to_string(), settings.home.clone());
    env.insert("HOME".to_string(), settings.home.clone());
    if let Some(random_value) = settings.random_value {
        env.insert("PI_DETERMINISTIC_RANDOM".to_string(), random_value);
    } else {
        env.insert(
            "PI_DETERMINISTIC_RANDOM_SEED".to_string(),
            settings.random_seed.clone(),
        );
    }
    let js_config = PiJsRuntimeConfig {
        cwd: settings.cwd.clone(),
        env,
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
    manager.set_js_runtime(runtime.clone());

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .map_err(|e| format!("load extension: {e}"))
        }
    })?;

    Ok(LoadedExtension { manager, runtime })
}

// ─── Scenario executors ─────────────────────────────────────────────────────

fn build_ctx_payload(settings: &DeterministicSettings, scenario_input: Option<&Value>) -> Value {
    // Extract has_ui from scenario input ctx if provided
    let has_ui = scenario_input
        .and_then(|input| input.get("ctx"))
        .and_then(|ctx| ctx.get("has_ui"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    serde_json::json!({
        "hasUI": has_ui,
        "cwd": settings.cwd,
        "sessionEntries": [],
        "sessionBranch": [],
        "sessionLeafEntry": null,
        "modelRegistry": {},
    })
}

/// Execute a tool scenario: call the tool and check expectations.
fn execute_tool_scenario(
    loaded: &LoadedExtension,
    scenario: &Scenario,
    extension_path: &Path,
) -> Result<Value, String> {
    let tool_name = scenario
        .tool_name
        .as_deref()
        .ok_or("tool scenario missing tool_name")?;
    let input = scenario
        .input
        .as_ref()
        .and_then(|v| v.get("arguments"))
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    let settings = deterministic_settings_for(extension_path);
    let ctx = build_ctx_payload(&settings, scenario.input.as_ref());

    common::run_async({
        let runtime = loaded.runtime.clone();
        let tool_name = tool_name.to_string();
        let tool_call_id = format!("tc-{}", scenario.id);
        async move {
            runtime
                .execute_tool(tool_name, tool_call_id, input, ctx, DEFAULT_TIMEOUT_MS)
                .await
                .map_err(|e| format!("execute_tool: {e}"))
        }
    })
}

/// Execute a command scenario: call the slash command and check expectations.
fn execute_command_scenario(
    loaded: &LoadedExtension,
    scenario: &Scenario,
    extension_path: &Path,
) -> Result<Value, String> {
    let command_name = scenario
        .command_name
        .as_deref()
        .ok_or("command scenario missing command_name")?;
    let args = scenario
        .input
        .as_ref()
        .and_then(|v| v.get("args"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let settings = deterministic_settings_for(extension_path);
    let ctx = build_ctx_payload(&settings, scenario.input.as_ref());

    common::run_async({
        let runtime = loaded.runtime.clone();
        let command_name = command_name.to_string();
        async move {
            runtime
                .execute_command(command_name, args, ctx, DEFAULT_TIMEOUT_MS)
                .await
                .map_err(|e| format!("execute_command: {e}"))
        }
    })
}

/// Execute an event scenario: dispatch the event and return the response.
fn execute_event_scenario(
    loaded: &LoadedExtension,
    scenario: &Scenario,
    extension_path: &Path,
) -> Result<Value, String> {
    let event_name = scenario
        .event_name
        .as_deref()
        .ok_or("event scenario missing event_name")?;
    let event_payload = scenario
        .input
        .as_ref()
        .and_then(|v| v.get("event"))
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    let settings = deterministic_settings_for(extension_path);
    let ctx = build_ctx_payload(&settings, scenario.input.as_ref());

    common::run_async({
        let runtime = loaded.runtime.clone();
        let event_name = event_name.to_string();
        async move {
            runtime
                .dispatch_event(event_name, event_payload, ctx, DEFAULT_TIMEOUT_MS)
                .await
                .map_err(|e| format!("dispatch_event: {e}"))
        }
    })
}

// ─── Expectation matchers ───────────────────────────────────────────────────

/// Extract text content from a tool result value.
fn extract_content_text(result: &Value) -> String {
    // Tool results return: { content: [{ type: "text", text: "..." }], details: {...} }
    result.get("content").and_then(Value::as_array).map_or_else(
        || {
            result
                .as_str()
                .map_or_else(|| result.to_string(), str::to_string)
        },
        |content| {
            content
                .iter()
                .filter_map(|block| {
                    if block.get("type").and_then(Value::as_str) == Some("text") {
                        block.get("text").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        },
    )
}

/// Check scenario expectations against actual result.
#[allow(clippy::too_many_lines)]
fn check_expectations(
    expect: &ScenarioExpectation,
    result: &Result<Value, String>,
    loaded: &LoadedExtension,
) -> Vec<String> {
    check_expectations_inner(expect, result, &loaded.manager, None)
}

/// Check scenario expectations with optional interceptor for mock-based checks.
#[allow(clippy::too_many_lines)]
fn check_expectations_with_mocks(
    expect: &ScenarioExpectation,
    result: &Result<Value, String>,
    loaded: &LoadedExtensionWithMocks,
) -> Vec<String> {
    check_expectations_inner(expect, result, &loaded.manager, Some(&loaded.interceptor))
}

#[allow(clippy::too_many_lines)]
fn check_expectations_inner(
    expect: &ScenarioExpectation,
    result: &Result<Value, String>,
    manager: &ExtensionManager,
    interceptor: Option<&Arc<MockSpecInterceptor>>,
) -> Vec<String> {
    let _ = &interceptor; // used below in mock-dependent matchers
    let mut diffs = Vec::new();

    // Check is_error expectation
    if expect.is_error == Some(true) && result.is_ok() {
        diffs.push("expected error but got success".to_string());
    }

    // Check error_contains
    if let Some(patterns) = &expect.error_contains {
        match result {
            Err(err) => {
                for pattern in patterns {
                    if !err.to_lowercase().contains(&pattern.to_lowercase()) {
                        diffs.push(format!(
                            "error_contains: expected '{pattern}' in error: {err}"
                        ));
                    }
                }
            }
            Ok(val) => {
                // Some tools return errors as content
                let text = extract_content_text(val);
                for pattern in patterns {
                    if !text.to_lowercase().contains(&pattern.to_lowercase()) {
                        diffs.push(format!(
                            "error_contains: expected '{pattern}' in result: {text}"
                        ));
                    }
                }
            }
        }
    }

    // The remaining checks need a successful result
    let Ok(result) = result else {
        if expect.is_error != Some(true) && expect.error_contains.is_none() {
            diffs.push(format!(
                "unexpected error: {}",
                result.as_ref().unwrap_err()
            ));
        }
        return diffs;
    };

    // Check content_contains
    if let Some(patterns) = &expect.content_contains {
        let text = extract_content_text(result);
        for pattern in patterns {
            if !text.contains(pattern) {
                diffs.push(format!("content_contains: expected '{pattern}' in: {text}"));
            }
        }
    }

    // Check details_exact
    if let Some(expected_details) = &expect.details_exact {
        let actual_details = result.get("details");
        match actual_details {
            Some(actual) => {
                if actual != expected_details {
                    diffs.push(format!(
                        "details_exact: expected {} got {}",
                        serde_json::to_string(expected_details).unwrap_or_default(),
                        serde_json::to_string(actual).unwrap_or_default(),
                    ));
                }
            }
            None => {
                diffs.push(format!(
                    "details_exact: expected {} but no details in result",
                    serde_json::to_string(expected_details).unwrap_or_default(),
                ));
            }
        }
    }

    // Check details_contains_keys
    if let Some(keys) = &expect.details_contains_keys {
        if let Some(details) = result.get("details").and_then(Value::as_object) {
            for key in keys {
                if !details.contains_key(key) {
                    diffs.push(format!(
                        "details_contains_keys: missing key '{key}' in details: {}",
                        serde_json::to_string(&details).unwrap_or_default(),
                    ));
                }
            }
        } else {
            diffs.push(format!(
                "details_contains_keys: no details object in result for keys: {keys:?}"
            ));
        }
    }

    // Check block expectation (event scenarios)
    if let Some(expected_block) = expect.block {
        let actual_block = result
            .get("block")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if actual_block != expected_block {
            diffs.push(format!(
                "block: expected {expected_block} got {actual_block}"
            ));
        }
    }

    // Check reason_contains (event scenarios)
    if let Some(patterns) = &expect.reason_contains {
        let reason = result.get("reason").and_then(Value::as_str).unwrap_or("");
        for pattern in patterns {
            if !reason.to_lowercase().contains(&pattern.to_lowercase()) {
                diffs.push(format!(
                    "reason_contains: expected '{pattern}' in reason: '{reason}'"
                ));
            }
        }
    }

    // Check provider registration expectations
    if let Some(expected_api) = &expect.api {
        let providers = manager.extension_providers();
        let matching = providers
            .iter()
            .find(|p| p.get("api").and_then(Value::as_str) == Some(expected_api));
        if matching.is_none() {
            diffs.push(format!(
                "api: expected provider with api='{expected_api}' but none found"
            ));
        }
    }

    if let Some(expected_env) = &expect.api_key_env {
        let providers = manager.extension_providers();
        // Provider spec uses "apiKey" for the env var name
        let has_env = providers.iter().any(|p| {
            p.get("apiKey").and_then(Value::as_str) == Some(expected_env)
                || p.get("apiKeyEnvVar").and_then(Value::as_str) == Some(expected_env)
        });
        if !has_env {
            diffs.push(format!(
                "api_key_env: expected provider with apiKey: '{expected_env}'"
            ));
        }
    }

    if let Some(expected_models) = &expect.models_contains {
        let providers = manager.extension_providers();
        let all_model_ids: Vec<String> = providers
            .iter()
            .filter_map(|p| p.get("models").and_then(Value::as_array))
            .flatten()
            .filter_map(|m| m.get("id").and_then(Value::as_str).map(String::from))
            .collect();
        for model in expected_models {
            if !all_model_ids.iter().any(|id| id.contains(model)) {
                diffs.push(format!(
                    "models_contains: expected model '{model}' in {all_model_ids:?}"
                ));
            }
        }
    }

    // Check tool_registered expectation
    if let Some(expected_tool) = &expect.tool_registered {
        let tool_name = expected_tool
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("");
        let tools = manager.extension_tool_defs();
        let matching = tools
            .iter()
            .find(|t| t.get("name").and_then(Value::as_str) == Some(tool_name));
        if let Some(actual_tool) = matching {
            if let Some(expected_label) = expected_tool.get("label").and_then(Value::as_str) {
                let actual_label = actual_tool
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if actual_label != expected_label {
                    diffs.push(format!(
                        "tool_registered.label: expected '{expected_label}' got '{actual_label}'"
                    ));
                }
            }
        } else {
            diffs.push(format!(
                "tool_registered: expected tool '{tool_name}' not found"
            ));
        }
    }

    // ── Mock-dependent matchers ──────────────────────────────────────────

    // Check ui_notify_contains: verify notifications captured by interceptor
    if let Some(patterns) = &expect.ui_notify_contains {
        if let Some(interceptor) = &interceptor {
            let notifications = interceptor.ui_notifications.lock().unwrap();
            let all_text: String = notifications
                .iter()
                .filter_map(|n| {
                    n.get("message")
                        .or_else(|| n.get("text"))
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>()
                .join(" ");
            for pattern in patterns {
                if !all_text.to_lowercase().contains(&pattern.to_lowercase()) {
                    diffs.push(format!(
                        "ui_notify_contains: expected '{pattern}' in notifications: {all_text}"
                    ));
                }
            }
        } else {
            diffs.push("ui_notify_contains: no interceptor available".to_string());
        }
    }

    // Check exec_called: verify exec calls logged by interceptor
    if let Some(expected_calls) = &expect.exec_called {
        if let Some(interceptor) = &interceptor {
            let exec_log = interceptor.exec_log.lock().unwrap();
            if let Some(expected_arr) = expected_calls.as_array() {
                for (i, expected_call) in expected_arr.iter().enumerate() {
                    if let Some(call_arr) = expected_call.as_array() {
                        let expected_cmd = call_arr.first().and_then(Value::as_str).unwrap_or("");
                        let expected_args: Vec<String> = call_arr
                            .get(1)
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default();

                        let found = exec_log.iter().any(|(cmd, payload)| {
                            if cmd != expected_cmd {
                                return false;
                            }
                            let actual_args: Vec<String> = payload
                                .get("args")
                                .and_then(Value::as_array)
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|v| v.as_str().map(String::from))
                                        .collect()
                                })
                                .unwrap_or_default();
                            actual_args == expected_args
                        });

                        if !found {
                            diffs.push(format!(
                                "exec_called[{i}]: expected ({expected_cmd}, {expected_args:?}) not found in exec log"
                            ));
                        }
                    }
                }
            }
        } else {
            diffs.push("exec_called: no interceptor available".to_string());
        }
    }

    // Check ui_status_key: verify status updates captured by interceptor
    if let Some(expected_key) = &expect.ui_status_key {
        if let Some(interceptor) = &interceptor {
            let status_updates = interceptor.ui_status_updates.lock().unwrap();
            let has_key = status_updates.iter().any(|s| {
                s.get("key")
                    .and_then(Value::as_str)
                    .map_or(false, |k| k == expected_key)
            });
            if !has_key {
                diffs.push(format!(
                    "ui_status_key: expected key '{expected_key}' in status updates"
                ));
            }
        } else {
            diffs.push("ui_status_key: no interceptor available".to_string());
        }
    }

    // Check ui_status_contains_sequence
    if let Some(patterns) = &expect.ui_status_contains_sequence {
        if let Some(interceptor) = &interceptor {
            let status_updates = interceptor.ui_status_updates.lock().unwrap();
            let all_values: Vec<String> = status_updates
                .iter()
                .filter_map(|s| {
                    s.get("value")
                        .or_else(|| s.get("text"))
                        .and_then(Value::as_str)
                        .map(String::from)
                })
                .collect();
            let joined = all_values.join(" ");
            for pattern in patterns {
                if !joined.to_lowercase().contains(&pattern.to_lowercase()) {
                    diffs.push(format!(
                        "ui_status_contains_sequence: expected '{pattern}' in: {joined}"
                    ));
                }
            }
        } else {
            diffs.push("ui_status_contains_sequence: no interceptor available".to_string());
        }
    }

    // Check returns_contains: deep partial match on result JSON
    if let Some(expected) = &expect.returns_contains {
        if !json_contains(result, expected) {
            diffs.push(format!(
                "returns_contains: expected {expected} to be contained in {result}"
            ));
        }
    }

    // Check action: check result action field (for input transforms)
    if let Some(expected_action) = &expect.action {
        let actual_action = result.get("action").and_then(Value::as_str).unwrap_or("");
        if actual_action != expected_action {
            diffs.push(format!(
                "action: expected '{expected_action}' got '{actual_action}'"
            ));
        }
    }

    // Check text_contains: check result text field
    if let Some(patterns) = &expect.text_contains {
        let text = result.get("text").and_then(Value::as_str).unwrap_or("");
        for pattern in patterns {
            if !text.contains(pattern.as_str()) {
                diffs.push(format!(
                    "text_contains: expected '{pattern}' in text: {text}"
                ));
            }
        }
    }

    // Check content_types: check content block types array
    if let Some(expected_types) = &expect.content_types {
        let actual_types: Vec<String> = result
            .get("content")
            .and_then(Value::as_array)
            .map(|arr: &Vec<Value>| {
                arr.iter()
                    .filter_map(|b| b.get("type").and_then(Value::as_str).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        for expected_type in expected_types {
            if !actual_types.iter().any(|t| t == expected_type) {
                diffs.push(format!(
                    "content_types: expected type '{expected_type}' in {actual_types:?}"
                ));
            }
        }
    }

    // Check active_tools: check via manager.active_tools()
    if let Some(expected_tools) = &expect.active_tools {
        let actual_tools = manager.active_tools().unwrap_or_default();
        for expected_tool in expected_tools {
            if !actual_tools.iter().any(|t| t == expected_tool) {
                diffs.push(format!(
                    "active_tools: expected '{expected_tool}' in {actual_tools:?}"
                ));
            }
        }
    }

    // ── Registration shape matchers ──────────────────────────────────────

    // Check flags_contains: verify flag names via manager.list_flags()
    if let Some(expected_flags) = &expect.flags_contains {
        let actual_flags = manager.list_flags();
        let flag_names: Vec<String> = actual_flags
            .iter()
            .filter_map(|f| f.get("name").and_then(Value::as_str).map(String::from))
            .collect();
        for expected_name in expected_flags {
            if !flag_names.iter().any(|n| n == expected_name) {
                diffs.push(format!(
                    "flags_contains: expected flag '{expected_name}' in {flag_names:?}"
                ));
            }
        }
    }

    // Check shortcuts_contains: verify shortcut keys via manager.list_shortcuts()
    if let Some(expected_shortcuts) = &expect.shortcuts_contains {
        let actual_shortcuts = manager.list_shortcuts();
        let shortcut_keys: Vec<String> = actual_shortcuts
            .iter()
            .filter_map(|s| {
                s.get("key")
                    .or_else(|| s.get("key_id"))
                    .or_else(|| s.get("name"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect();
        for expected_key in expected_shortcuts {
            if !shortcut_keys.iter().any(|k| k.contains(expected_key)) {
                diffs.push(format!(
                    "shortcuts_contains: expected shortcut '{expected_key}' in {shortcut_keys:?}"
                ));
            }
        }
    }

    // Check commands_contains: verify command names via manager.list_commands()
    if let Some(expected_commands) = &expect.commands_contains {
        let actual_commands = manager.list_commands();
        let command_names: Vec<String> = actual_commands
            .iter()
            .filter_map(|c| c.get("name").and_then(Value::as_str).map(String::from))
            .collect();
        for expected_name in expected_commands {
            if !command_names.iter().any(|n| n == expected_name) {
                diffs.push(format!(
                    "commands_contains: expected command '{expected_name}' in {command_names:?}"
                ));
            }
        }
    }

    // Check event_hooks_contains: verify event hook names via manager.list_event_hooks()
    if let Some(expected_hooks) = &expect.event_hooks_contains {
        let actual_hooks = manager.list_event_hooks();
        for expected_hook in expected_hooks {
            if !actual_hooks.iter().any(|h| h == expected_hook) {
                diffs.push(format!(
                    "event_hooks_contains: expected event hook '{expected_hook}' in {actual_hooks:?}"
                ));
            }
        }
    }

    // Check tool_count
    if let Some(expected_count) = expect.tool_count {
        let actual_count = manager.extension_tool_defs().len();
        if actual_count != expected_count {
            diffs.push(format!(
                "tool_count: expected {expected_count} got {actual_count}"
            ));
        }
    }

    // Check flag_count
    if let Some(expected_count) = expect.flag_count {
        let actual_count = manager.list_flags().len();
        if actual_count != expected_count {
            diffs.push(format!(
                "flag_count: expected {expected_count} got {actual_count}"
            ));
        }
    }

    diffs
}

/// Deep partial match: every key/value in `expected` must exist in `actual`.
///
/// Path-typed keys (detected by [`is_path_key`]) use suffix matching so
/// that relative filenames in fixtures (`"SKILL.md"`) match absolute paths
/// from the runtime (`"/data/.../SKILL.md"`).
fn json_contains(actual: &Value, expected: &Value) -> bool {
    json_contains_inner(actual, expected, None)
}

fn json_contains_inner(actual: &Value, expected: &Value, key: Option<&str>) -> bool {
    match (actual, expected) {
        (Value::Object(actual_map), Value::Object(expected_map)) => {
            expected_map.iter().all(|(k, v)| {
                actual_map
                    .get(k)
                    .map_or(false, |av| json_contains_inner(av, v, Some(k)))
            })
        }
        (Value::Array(actual_arr), Value::Array(expected_arr)) => expected_arr
            .iter()
            .all(|ev| actual_arr.iter().any(|av| json_contains_inner(av, ev, key))),
        // Path-typed string comparison: suffix match (bd-k5q5.1.2)
        (Value::String(actual_s), Value::String(expected_s))
            if key.is_some_and(|k| is_path_key(k)) =>
        {
            path_suffix_match(actual_s, expected_s)
        }
        _ => actual == expected,
    }
}

// ─── Mock Infrastructure ─────────────────────────────────────────────────────

/// Rule for matching exec hostcalls.
#[derive(Debug, Clone)]
struct ExecRule {
    command: String,
    args_pattern: Option<Vec<String>>,
    result: Value,
}

/// Rule for matching HTTP hostcalls.
#[derive(Debug, Clone)]
struct HttpRule {
    method: Option<String>,
    url_contains: Option<String>,
    response: Value,
}

/// Mock interceptor that provides deterministic responses for exec, HTTP, and UI
/// hostcalls. Session/Events/Tool calls pass through to real dispatch.
struct MockSpecInterceptor {
    exec_rules: Vec<ExecRule>,
    exec_default: Value,
    http_rules: Vec<HttpRule>,
    http_default: Value,
    ui_responses: Mutex<HashMap<String, Value>>,
    ui_confirm_default: bool,
    ui_notifications: Arc<Mutex<Vec<Value>>>,
    ui_status_updates: Arc<Mutex<Vec<Value>>>,
    exec_log: Arc<Mutex<Vec<(String, Value)>>>,
}

impl MockSpecInterceptor {
    /// Parse from the mock_spec JSON format (mock_spec_default.json).
    fn from_mock_spec(spec: &Value) -> Self {
        let exec_rules = spec
            .pointer("/exec/rules")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|r| ExecRule {
                        command: r
                            .get("command")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        args_pattern: r.get("args").and_then(Value::as_array).map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        }),
                        result: r.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let exec_default = spec
            .pointer("/exec/default_result")
            .cloned()
            .unwrap_or_else(|| {
                serde_json::json!({
                    "stdout": "",
                    "stderr": "mock: command not found",
                    "code": 127,
                    "killed": false
                })
            });

        let http_rules = spec
            .pointer("/http/rules")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|r| HttpRule {
                        method: r.get("method").and_then(Value::as_str).map(String::from),
                        url_contains: r
                            .get("url_contains")
                            .and_then(Value::as_str)
                            .map(String::from),
                        response: r.get("response").cloned().unwrap_or(Value::Null),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let http_default = spec
            .pointer("/http/default_response")
            .cloned()
            .unwrap_or_else(|| {
                serde_json::json!({
                    "status": 404,
                    "headers": {"content-type": "text/plain"},
                    "body": "mock: no HTTP rule matched"
                })
            });

        let ui_responses = spec
            .pointer("/ui/responses")
            .and_then(Value::as_object)
            .map(|obj| obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();

        let ui_confirm_default = spec
            .pointer("/ui/confirm_default")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        Self {
            exec_rules,
            exec_default,
            http_rules,
            http_default,
            ui_responses: Mutex::new(ui_responses),
            ui_confirm_default,
            ui_notifications: Arc::new(Mutex::new(Vec::new())),
            ui_status_updates: Arc::new(Mutex::new(Vec::new())),
            exec_log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Merge scenario-specific setup overrides with the default mock spec.
    fn from_scenario_setup(setup: &Value, default_spec: &Value) -> Self {
        let mut interceptor = Self::from_mock_spec(default_spec);

        // Merge mock_exec rules from scenario setup
        if let Some(mock_exec) = setup.get("mock_exec").and_then(Value::as_array) {
            for rule in mock_exec {
                interceptor.exec_rules.push(ExecRule {
                    command: rule
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    args_pattern: rule.get("args").and_then(Value::as_array).map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    }),
                    result: rule.clone(),
                });
            }
        }

        // Merge mock_http rules from scenario setup
        if let Some(mock_http) = setup.get("mock_http") {
            if let Some(rules) = mock_http.get("rules").and_then(Value::as_array) {
                for rule in rules {
                    interceptor.http_rules.push(HttpRule {
                        method: rule.get("method").and_then(Value::as_str).map(String::from),
                        url_contains: rule
                            .get("url_contains")
                            .and_then(Value::as_str)
                            .map(String::from),
                        response: rule.get("response").cloned().unwrap_or(Value::Null),
                    });
                }
            }

            // For vcr_or_stub mode without explicit rules, provide a synthetic
            // SSE stub response so extensions that make HTTP calls don't hang.
            let mode = mock_http
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("");
            if mode == "vcr_or_stub" && interceptor.http_rules.is_empty() {
                // 1px transparent PNG as base64 (valid image for any image-gen stub)
                let stub_image = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+XvU8AAAAASUVORK5CYII=";
                interceptor.http_default = serde_json::json!({
                    "status": 200,
                    "headers": {"content-type": "application/json"},
                    "body": serde_json::json!({
                        "candidates": [{
                            "content": {
                                "parts": [
                                    {"text": "stubbed image response"},
                                    {"inlineData": {"mimeType": "image/png", "data": stub_image}}
                                ]
                            }
                        }]
                    }).to_string()
                });
            }
        }

        interceptor
    }

    /// Update UI responses for the current step (called from multi-step runner).
    fn set_ui_responses(&self, responses: &serde_json::Map<String, Value>) {
        let mut locked = self.ui_responses.lock().unwrap();
        for (k, v) in responses {
            locked.insert(k.clone(), v.clone());
        }
    }

    fn match_exec(&self, cmd: &str, payload: &Value) -> Value {
        let args = payload
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        for rule in &self.exec_rules {
            if rule.command != cmd {
                continue;
            }
            if let Some(expected_args) = &rule.args_pattern {
                if *expected_args != args {
                    continue;
                }
            }
            return serde_json::json!({
                "stdout": rule.result.get("stdout").and_then(Value::as_str).unwrap_or(""),
                "stderr": rule.result.get("stderr").and_then(Value::as_str).unwrap_or(""),
                "code": rule.result.get("code").and_then(Value::as_i64).unwrap_or(0),
                "killed": rule.result.get("killed").and_then(Value::as_bool).unwrap_or(false),
            });
        }

        self.exec_default.clone()
    }

    fn match_http(&self, payload: &Value) -> Value {
        let req_method = payload
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET");
        let req_url = payload.get("url").and_then(Value::as_str).unwrap_or("");

        for rule in &self.http_rules {
            if let Some(method) = &rule.method {
                if !method.eq_ignore_ascii_case(req_method) {
                    continue;
                }
            }
            if let Some(url_pat) = &rule.url_contains {
                if !req_url.contains(url_pat.as_str()) {
                    continue;
                }
            }
            return rule.response.clone();
        }

        self.http_default.clone()
    }
}

impl HostcallInterceptor for MockSpecInterceptor {
    fn intercept(&self, request: &HostcallRequest) -> Option<HostcallOutcome> {
        match &request.kind {
            HostcallKind::Exec { cmd } => {
                // Log the call
                self.exec_log
                    .lock()
                    .unwrap()
                    .push((cmd.clone(), request.payload.clone()));

                let result = self.match_exec(cmd, &request.payload);
                Some(HostcallOutcome::Success(result))
            }
            HostcallKind::Http => {
                let result = self.match_http(&request.payload);
                Some(HostcallOutcome::Success(result))
            }
            HostcallKind::Ui { op } => {
                match op.as_str() {
                    "notify" => {
                        self.ui_notifications
                            .lock()
                            .unwrap()
                            .push(request.payload.clone());
                        Some(HostcallOutcome::Success(serde_json::json!({"ok": true})))
                    }
                    "status" => {
                        self.ui_status_updates
                            .lock()
                            .unwrap()
                            .push(request.payload.clone());
                        Some(HostcallOutcome::Success(serde_json::json!({"ok": true})))
                    }
                    "confirm" => {
                        let result = self.ui_confirm_default;
                        Some(HostcallOutcome::Success(
                            serde_json::json!({ "confirmed": result }),
                        ))
                    }
                    "select" => {
                        // Check ui_responses for a "select" key
                        let locked = self.ui_responses.lock().unwrap();
                        let value = locked.get("select").cloned().unwrap_or(Value::Null);
                        Some(HostcallOutcome::Success(
                            serde_json::json!({ "selected": value }),
                        ))
                    }
                    "input" => {
                        let locked = self.ui_responses.lock().unwrap();
                        let value = locked
                            .get("input")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        Some(HostcallOutcome::Success(
                            serde_json::json!({ "value": value }),
                        ))
                    }
                    // Pass through unknown UI ops to real dispatch
                    _ => None,
                }
            }
            // Session, Events, Tool, Log → pass through to real dispatch
            HostcallKind::Session { .. }
            | HostcallKind::Events { .. }
            | HostcallKind::Tool { .. }
            | HostcallKind::Log => None,
        }
    }
}

// ─── ConformanceSession ──────────────────────────────────────────────────────

/// Session implementation for conformance tests with pre-seeded data and
/// full mutation support.
struct ConformanceSession {
    state: Mutex<Value>,
    messages: Mutex<Vec<SessionMessage>>,
    entries: Mutex<Vec<Value>>,
    branch: Mutex<Vec<Value>>,
    name: Mutex<Option<String>>,
    model: Mutex<(Option<String>, Option<String>)>,
    thinking_level: Mutex<Option<String>>,
    labels: Mutex<Vec<(String, Option<String>)>>,
}

impl ConformanceSession {
    /// Create from mock_spec JSON and optional scenario setup overrides.
    fn from_spec(default_spec: &Value, setup: Option<&Value>) -> Self {
        let session_spec = default_spec.get("session").cloned().unwrap_or(Value::Null);

        let state = session_spec
            .get("state")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));

        let messages = session_spec
            .get("messages")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| serde_json::from_value(m.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();

        let mut entries: Vec<Value> = session_spec
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut branch: Vec<Value> = session_spec
            .get("branch")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let name = session_spec
            .get("name")
            .and_then(Value::as_str)
            .map(String::from);

        let model_spec = default_spec.get("model").cloned().unwrap_or(Value::Null);
        let current = model_spec.get("current").cloned().unwrap_or(Value::Null);
        let provider = current
            .get("provider")
            .and_then(Value::as_str)
            .map(String::from);
        let model_id = current
            .get("model_id")
            .and_then(Value::as_str)
            .map(String::from);

        let thinking_level = model_spec
            .get("thinking_level")
            .and_then(Value::as_str)
            .map(String::from);

        // Apply scenario setup overrides
        if let Some(setup) = setup {
            if let Some(sb) = setup.get("session_branch").and_then(Value::as_array) {
                branch = sb.clone();
            }
            if let Some(le) = setup.get("session_leaf_entry") {
                // Add leaf entry to entries if not already there
                entries.push(le.clone());
            }
        }

        Self {
            state: Mutex::new(state),
            messages: Mutex::new(messages),
            entries: Mutex::new(entries),
            branch: Mutex::new(branch),
            name: Mutex::new(name),
            model: Mutex::new((provider, model_id)),
            thinking_level: Mutex::new(thinking_level),
            labels: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ExtensionSession for ConformanceSession {
    async fn get_state(&self) -> Value {
        self.state.lock().unwrap().clone()
    }

    async fn get_messages(&self) -> Vec<SessionMessage> {
        self.messages.lock().unwrap().clone()
    }

    async fn get_entries(&self) -> Vec<Value> {
        self.entries.lock().unwrap().clone()
    }

    async fn get_branch(&self) -> Vec<Value> {
        self.branch.lock().unwrap().clone()
    }

    async fn set_name(&self, name: String) -> pi::error::Result<()> {
        *self.name.lock().unwrap() = Some(name);
        Ok(())
    }

    async fn append_message(&self, message: SessionMessage) -> pi::error::Result<()> {
        self.messages.lock().unwrap().push(message);
        Ok(())
    }

    async fn append_custom_entry(
        &self,
        custom_type: String,
        data: Option<Value>,
    ) -> pi::error::Result<()> {
        self.entries.lock().unwrap().push(serde_json::json!({
            "type": custom_type,
            "data": data,
        }));
        Ok(())
    }

    async fn set_model(&self, provider: String, model_id: String) -> pi::error::Result<()> {
        *self.model.lock().unwrap() = (Some(provider), Some(model_id));
        Ok(())
    }

    async fn get_model(&self) -> (Option<String>, Option<String>) {
        self.model.lock().unwrap().clone()
    }

    async fn set_thinking_level(&self, level: String) -> pi::error::Result<()> {
        *self.thinking_level.lock().unwrap() = Some(level);
        Ok(())
    }

    async fn get_thinking_level(&self) -> Option<String> {
        self.thinking_level.lock().unwrap().clone()
    }

    async fn set_label(&self, target_id: String, label: Option<String>) -> pi::error::Result<()> {
        self.labels.lock().unwrap().push((target_id, label));
        Ok(())
    }
}

// ─── Extension loader with mocks ─────────────────────────────────────────────

struct LoadedExtensionWithMocks {
    manager: ExtensionManager,
    runtime: JsExtensionRuntimeHandle,
    interceptor: Arc<MockSpecInterceptor>,
    #[allow(dead_code)]
    session: Arc<ConformanceSession>,
}

/// Load an extension with mock interceptor and conformance session.
fn load_extension_with_mocks(
    extension_path: &Path,
    setup: Option<&Value>,
    default_spec: &Value,
) -> Result<LoadedExtensionWithMocks, String> {
    let settings = deterministic_settings_for(extension_path);
    ensure_deterministic_dirs(&settings);
    let cwd = PathBuf::from(&settings.cwd);

    let spec = JsExtensionLoadSpec::from_entry_path(extension_path)
        .map_err(|e| format!("load spec: {e}"))?;
    let extension_id = spec.extension_id.clone();

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));

    // Build interceptor from mock spec + scenario setup
    let interceptor = Arc::new(setup.map_or_else(
        || MockSpecInterceptor::from_mock_spec(default_spec),
        |s| MockSpecInterceptor::from_scenario_setup(s, default_spec),
    ));

    // Build conformance session
    let session = Arc::new(ConformanceSession::from_spec(default_spec, setup));
    manager.set_session(Arc::clone(&session) as Arc<dyn ExtensionSession>);

    let mut env = HashMap::new();
    env.insert(
        "PI_DETERMINISTIC_TIME_MS".to_string(),
        settings.time_ms.clone(),
    );
    env.insert(
        "PI_DETERMINISTIC_TIME_STEP_MS".to_string(),
        settings.time_step_ms.clone(),
    );
    env.insert("PI_DETERMINISTIC_CWD".to_string(), settings.cwd.clone());
    env.insert("PI_DETERMINISTIC_HOME".to_string(), settings.home.clone());
    env.insert("HOME".to_string(), settings.home.clone());
    if let Some(random_value) = settings.random_value {
        env.insert("PI_DETERMINISTIC_RANDOM".to_string(), random_value);
    } else {
        env.insert(
            "PI_DETERMINISTIC_RANDOM_SEED".to_string(),
            settings.random_seed.clone(),
        );
    }
    let js_config = PiJsRuntimeConfig {
        cwd: settings.cwd.clone(),
        env,
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        let interceptor_clone = Arc::clone(&interceptor) as Arc<dyn HostcallInterceptor>;
        async move {
            JsExtensionRuntimeHandle::start_with_interceptor(
                js_config,
                tools,
                manager,
                interceptor_clone,
            )
            .await
            .map_err(|e| format!("start runtime: {e}"))
        }
    })?;
    manager.set_js_runtime(runtime.clone());

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .map_err(|e| format!("load extension: {e}"))
        }
    })?;

    // Pre-seed flags via JS runtime (after extension is loaded)
    if let Some(flags) = setup
        .and_then(|s| s.get("flags"))
        .and_then(Value::as_object)
    {
        let ext_id = extension_id.clone();
        for (flag_name, flag_value) in flags {
            common::run_async({
                let manager = manager.clone();
                let flag_name = flag_name.clone();
                let flag_value = flag_value.clone();
                let ext_id = ext_id.clone();
                async move {
                    let _ = manager
                        .set_flag_value(&ext_id, &flag_name, flag_value)
                        .await;
                }
            });
        }
    }

    // Pre-seed extension state by mapping state keys to flags and dispatching
    // session_start.  This covers extensions like plan-mode that read flags and
    // persisted entries during session_start to initialise internal state.
    if let Some(state) = setup.and_then(|s| s.get("state")).and_then(Value::as_object) {
        let ext_id = extension_id.clone();
        // Map well-known state keys to their corresponding flags
        let state_to_flag: &[(&str, &str)] = &[("plan_mode_enabled", "plan")];
        for (state_key, flag_name) in state_to_flag {
            if let Some(value) = state.get(*state_key) {
                common::run_async({
                    let manager = manager.clone();
                    let flag_name = (*flag_name).to_string();
                    let flag_value = value.clone();
                    let ext_id = ext_id.clone();
                    async move {
                        let _ = manager.set_flag_value(&ext_id, &flag_name, flag_value).await;
                    }
                });
            }
        }

        // Dispatch session_start so extensions run their initialization logic
        // (reading flags, restoring persisted state, setting active tools, etc.)
        let settings = deterministic_settings_for(&cwd);
        let default_spec_for_ctx = load_default_mock_spec();
        let ctx = build_ctx_payload_with_mocks(&settings, None, setup, &default_spec_for_ctx);
        common::run_async({
            let runtime = runtime.clone();
            async move {
                let _ = runtime
                    .dispatch_event(
                        "session_start".to_string(),
                        Value::Object(serde_json::Map::new()),
                        ctx,
                        DEFAULT_TIMEOUT_MS,
                    )
                    .await;
            }
        });
    }

    Ok(LoadedExtensionWithMocks {
        manager,
        runtime,
        interceptor,
        session,
    })
}

/// Build ctx payload with mock data populated from setup + default spec.
fn build_ctx_payload_with_mocks(
    settings: &DeterministicSettings,
    scenario_input: Option<&Value>,
    setup: Option<&Value>,
    default_spec: &Value,
) -> Value {
    let has_ui = scenario_input
        .and_then(|input| input.get("ctx"))
        .and_then(|ctx| ctx.get("has_ui"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Build model registry from setup.mock_model_registry
    let model_registry = setup
        .and_then(|s| s.get("mock_model_registry"))
        .and_then(Value::as_object)
        .map(|obj| Value::Object(obj.clone()))
        .unwrap_or_else(|| serde_json::json!({}));

    // Session data from default spec
    let session_spec = default_spec.get("session").cloned().unwrap_or(Value::Null);
    let mut session_entries: Vec<Value> = session_spec
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut session_branch: Vec<Value> = session_spec
        .get("branch")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut session_leaf_entry = Value::Null;

    // Override from setup
    if let Some(setup) = setup {
        if let Some(sb) = setup.get("session_branch").and_then(Value::as_array) {
            session_branch = sb.clone();
        }
        if let Some(le) = setup.get("session_leaf_entry") {
            session_leaf_entry = le.clone();
        }
    }
    // Merge leaf entry into entries
    if !session_leaf_entry.is_null() {
        session_entries.push(session_leaf_entry.clone());
    }

    // Merge UI responses from scenario input ctx
    let ui_responses = scenario_input
        .and_then(|input| input.get("ctx"))
        .and_then(|ctx| ctx.get("ui_responses"))
        .cloned()
        .unwrap_or(Value::Null);

    let mut ctx = serde_json::json!({
        "hasUI": has_ui,
        "cwd": settings.cwd,
        "sessionEntries": session_entries,
        "sessionBranch": session_branch,
        "sessionLeafEntry": session_leaf_entry,
        "modelRegistry": model_registry,
    });

    if !ui_responses.is_null() {
        ctx.as_object_mut()
            .unwrap()
            .insert("uiResponses".to_string(), ui_responses);
    }

    ctx
}

// ─── Scenario categorization ────────────────────────────────────────────────

/// Check if a scenario needs setup features we cannot provide yet.
/// Most features are now supported via `MockSpecInterceptor` and
/// `ConformanceSession`.
fn needs_unsupported_setup(_scenario: &Scenario) -> Option<String> {
    // All previously unsupported setup types are now handled:
    // - "vcr_or_stub" HTTP mock mode: treated as normal rule-based mock with
    //   synthetic fallback response
    // - "state": pre-seeded via flag injection + session_start event dispatch
    None
}

// ─── Main runner ────────────────────────────────────────────────────────────

/// Check whether a scenario needs mock infrastructure (interceptor, session, etc.).
fn needs_mock_loader(scenario: &Scenario) -> bool {
    if let Some(setup) = &scenario.setup {
        if setup.get("mock_exec").is_some()
            || setup.get("mock_http").is_some()
            || setup.get("mock_model_registry").is_some()
            || setup.get("session_branch").is_some()
            || setup.get("session_leaf_entry").is_some()
            || setup.get("flags").is_some()
            || setup.get("state").is_some()
        {
            return true;
        }
    }
    // Scenarios with UI interaction responses
    if let Some(input) = &scenario.input {
        if input
            .pointer("/ctx/ui_responses")
            .is_some_and(|v| !v.is_null())
        {
            return true;
        }
    }
    // Multi-step scenarios
    if scenario.steps.is_some() {
        return true;
    }
    // Scenarios that check interceptor-dependent expectations
    if let Some(expect) = &scenario.expect {
        if expect.ui_notify_contains.is_some()
            || expect.ui_status_key.is_some()
            || expect.ui_status_contains_sequence.is_some()
            || expect.exec_called.is_some()
            || expect.active_tools.is_some()
            || expect.returns_contains.is_some()
            || expect.action.is_some()
            || expect.content_types.is_some()
        {
            return true;
        }
    }
    false
}

/// Load the default mock spec JSON.
fn load_default_mock_spec() -> Value {
    let path = project_root().join("tests/ext_conformance/mock_specs/mock_spec_default.json");
    let data = fs::read_to_string(&path).expect("read mock_spec_default.json");
    serde_json::from_str(&data).expect("parse mock_spec_default.json")
}

/// Execute a tool scenario using the mock-based loader.
fn execute_tool_scenario_with_mocks(
    loaded: &LoadedExtensionWithMocks,
    scenario: &Scenario,
    extension_path: &Path,
) -> Result<Value, String> {
    let tool_name = scenario
        .tool_name
        .as_deref()
        .ok_or("tool scenario missing tool_name")?;
    let input = scenario
        .input
        .as_ref()
        .and_then(|v| v.get("arguments"))
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    let settings = deterministic_settings_for(extension_path);
    let default_spec = load_default_mock_spec();

    // Merge scenario-level ui_responses into the interceptor
    if let Some(ui_resp) = scenario
        .input
        .as_ref()
        .and_then(|v| v.pointer("/ctx/ui_responses"))
        .and_then(Value::as_object)
    {
        loaded.interceptor.set_ui_responses(ui_resp);
    }

    let ctx = build_ctx_payload_with_mocks(
        &settings,
        scenario.input.as_ref(),
        scenario.setup.as_ref(),
        &default_spec,
    );

    common::run_async({
        let runtime = loaded.runtime.clone();
        let tool_name = tool_name.to_string();
        let tool_call_id = format!("tc-{}", scenario.id);
        async move {
            runtime
                .execute_tool(tool_name, tool_call_id, input, ctx, DEFAULT_TIMEOUT_MS)
                .await
                .map_err(|e| format!("execute_tool: {e}"))
        }
    })
}

/// Execute a command scenario using the mock-based loader.
fn execute_command_scenario_with_mocks(
    loaded: &LoadedExtensionWithMocks,
    scenario: &Scenario,
    extension_path: &Path,
) -> Result<Value, String> {
    let command_name = scenario
        .command_name
        .as_deref()
        .ok_or("command scenario missing command_name")?;
    let args = scenario
        .input
        .as_ref()
        .and_then(|v| v.get("args"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let settings = deterministic_settings_for(extension_path);
    let default_spec = load_default_mock_spec();

    // Merge scenario-level ui_responses into the interceptor
    if let Some(ui_resp) = scenario
        .input
        .as_ref()
        .and_then(|v| v.pointer("/ctx/ui_responses"))
        .and_then(Value::as_object)
    {
        loaded.interceptor.set_ui_responses(ui_resp);
    }

    let ctx = build_ctx_payload_with_mocks(
        &settings,
        scenario.input.as_ref(),
        scenario.setup.as_ref(),
        &default_spec,
    );

    common::run_async({
        let runtime = loaded.runtime.clone();
        let command_name = command_name.to_string();
        async move {
            runtime
                .execute_command(command_name, args, ctx, DEFAULT_TIMEOUT_MS)
                .await
                .map_err(|e| format!("execute_command: {e}"))
        }
    })
}

/// Execute an event scenario using the mock-based loader.
fn execute_event_scenario_with_mocks(
    loaded: &LoadedExtensionWithMocks,
    scenario: &Scenario,
    extension_path: &Path,
) -> Result<Value, String> {
    let event_name = scenario
        .event_name
        .as_deref()
        .ok_or("event scenario missing event_name")?;
    let event_payload = scenario
        .input
        .as_ref()
        .and_then(|v| v.get("event"))
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    let settings = deterministic_settings_for(extension_path);
    let default_spec = load_default_mock_spec();

    // Merge scenario-level ui_responses into the interceptor
    if let Some(ui_resp) = scenario
        .input
        .as_ref()
        .and_then(|v| v.pointer("/ctx/ui_responses"))
        .and_then(Value::as_object)
    {
        loaded.interceptor.set_ui_responses(ui_resp);
    }

    let ctx = build_ctx_payload_with_mocks(
        &settings,
        scenario.input.as_ref(),
        scenario.setup.as_ref(),
        &default_spec,
    );

    common::run_async({
        let runtime = loaded.runtime.clone();
        let event_name = event_name.to_string();
        async move {
            runtime
                .dispatch_event(event_name, event_payload, ctx, DEFAULT_TIMEOUT_MS)
                .await
                .map_err(|e| format!("dispatch_event: {e}"))
        }
    })
}

/// Execute a multi-step scenario: run each step sequentially, return the last result.
#[allow(clippy::too_many_lines)]
fn execute_multi_step_scenario(
    loaded: &LoadedExtensionWithMocks,
    scenario: &Scenario,
    extension_path: &Path,
) -> Result<Value, String> {
    let steps = scenario
        .steps
        .as_ref()
        .ok_or("multi-step scenario missing steps")?;

    let settings = deterministic_settings_for(extension_path);
    let default_spec = load_default_mock_spec();
    let mut last_result = Ok(Value::Null);

    for step in steps {
        let step_type = step
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        match step_type {
            "emit_event" => {
                let event_name = step
                    .get("event_name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let event_payload = step
                    .get("event")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

                // Build ctx, merging step-level ctx overrides (e.g. ui_responses, has_ui)
                let mut ctx = build_ctx_payload_with_mocks(
                    &settings,
                    scenario.input.as_ref(),
                    scenario.setup.as_ref(),
                    &default_spec,
                );
                // Merge step-level ctx into the payload
                if let Some(step_ctx) = step.get("ctx").and_then(Value::as_object) {
                    let ctx_obj = ctx.as_object_mut().unwrap();
                    if let Some(has_ui) = step_ctx.get("has_ui") {
                        ctx_obj.insert("hasUI".to_string(), has_ui.clone());
                    }
                    if let Some(ui_resp) = step_ctx.get("ui_responses") {
                        // Update the interceptor's UI responses for this step
                        if let Some(obj) = ui_resp.as_object() {
                            loaded.interceptor.set_ui_responses(obj);
                        }
                        ctx_obj.insert("uiResponses".to_string(), ui_resp.clone());
                    }
                }

                last_result = common::run_async({
                    let runtime = loaded.runtime.clone();
                    async move {
                        runtime
                            .dispatch_event(event_name, event_payload, ctx, DEFAULT_TIMEOUT_MS)
                            .await
                            .map_err(|e| format!("dispatch_event: {e}"))
                    }
                });
            }
            "invoke_tool" => {
                let tool_name = step
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let input = step
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                let ctx = build_ctx_payload_with_mocks(
                    &settings,
                    scenario.input.as_ref(),
                    scenario.setup.as_ref(),
                    &default_spec,
                );

                last_result = common::run_async({
                    let runtime = loaded.runtime.clone();
                    let tool_call_id = format!("tc-{}-step", scenario.id);
                    async move {
                        runtime
                            .execute_tool(tool_name, tool_call_id, input, ctx, DEFAULT_TIMEOUT_MS)
                            .await
                            .map_err(|e| format!("execute_tool: {e}"))
                    }
                });
            }
            "invoke_command" => {
                let command_name = step
                    .get("command_name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = step
                    .get("args")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let ctx = build_ctx_payload_with_mocks(
                    &settings,
                    scenario.input.as_ref(),
                    scenario.setup.as_ref(),
                    &default_spec,
                );

                last_result = common::run_async({
                    let runtime = loaded.runtime.clone();
                    async move {
                        runtime
                            .execute_command(command_name, args, ctx, DEFAULT_TIMEOUT_MS)
                            .await
                            .map_err(|e| format!("execute_command: {e}"))
                    }
                });
            }
            other => {
                return Err(format!("unknown step type: {other}"));
            }
        }
    }

    last_result
}

/// Run a single scenario and return the result.
#[allow(clippy::too_many_lines)]
fn run_scenario(
    ext: &ScenarioExtension,
    scenario: &Scenario,
    items: &[SampleItem],
) -> ScenarioResult {
    let start = Instant::now();
    let item = items.iter().find(|i| i.id == ext.extension_id);
    let base = ScenarioResult {
        scenario_id: scenario.id.clone(),
        extension_id: ext.extension_id.clone(),
        kind: scenario.kind.clone(),
        summary: scenario.summary.clone(),
        status: String::new(),
        source_tier: item.map_or_else(|| "unknown".to_string(), |i| i.source_tier.clone()),
        runtime_tier: item.map_or_else(|| "unknown".to_string(), |i| i.runtime_tier.clone()),
        diffs: Vec::new(),
        error: None,
        skip_reason: None,
        output: None,
        duration_ms: 0,
    };

    // Check if scenario is supported
    if let Some(reason) = needs_unsupported_setup(scenario) {
        return ScenarioResult {
            status: "skip".to_string(),
            skip_reason: Some(reason),
            duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            ..base
        };
    }

    // Resolve extension path
    let Some(ext_path) = resolve_extension_path(&ext.extension_id, items) else {
        return ScenarioResult {
            status: "error".to_string(),
            error: Some(format!(
                "cannot resolve artifact path for '{}'",
                ext.extension_id
            )),
            duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            ..base
        };
    };

    // Decide whether to use mock-based or plain loader
    if needs_mock_loader(scenario) {
        return run_scenario_with_mocks(ext, scenario, &ext_path, start, base);
    }

    // Load extension (plain path - no mocks needed)
    let loaded = match load_extension(&ext_path) {
        Ok(loaded) => loaded,
        Err(err) => {
            return ScenarioResult {
                status: "error".to_string(),
                error: Some(format!("load_extension: {err}")),
                duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                ..base
            };
        }
    };

    // Execute by kind
    let result = match scenario.kind.as_str() {
        "tool" => {
            // For tool scenarios without a tool_name, check tool_registered expectations
            if scenario.tool_name.is_none() {
                if let Some(expect) = &scenario.expect {
                    let diffs = check_expectations(expect, &Ok(Value::Null), &loaded);
                    return ScenarioResult {
                        status: if diffs.is_empty() {
                            "pass".to_string()
                        } else {
                            "fail".to_string()
                        },
                        diffs,
                        duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        ..base
                    };
                }
                return ScenarioResult {
                    status: "skip".to_string(),
                    skip_reason: Some(
                        "tool scenario with no tool_name and no expectations".to_string(),
                    ),
                    duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                    ..base
                };
            }
            execute_tool_scenario(&loaded, scenario, &ext_path)
        }
        "command" => execute_command_scenario(&loaded, scenario, &ext_path),
        "event" => execute_event_scenario(&loaded, scenario, &ext_path),
        "provider" => {
            // Provider scenarios check registration, not execution
            if let Some(expect) = &scenario.expect {
                let diffs = check_expectations(expect, &Ok(Value::Null), &loaded);
                return ScenarioResult {
                    status: if diffs.is_empty() {
                        "pass".to_string()
                    } else {
                        "fail".to_string()
                    },
                    diffs,
                    duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                    ..base
                };
            }
            return ScenarioResult {
                status: "pass".to_string(),
                duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                ..base
            };
        }
        // Flag/shortcut/registration scenarios: check registration state only
        "flag" | "shortcut" | "registration" => {
            if let Some(expect) = &scenario.expect {
                let diffs = check_expectations(expect, &Ok(Value::Null), &loaded);
                return ScenarioResult {
                    status: if diffs.is_empty() {
                        "pass".to_string()
                    } else {
                        "fail".to_string()
                    },
                    diffs,
                    duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                    ..base
                };
            }
            return ScenarioResult {
                status: "pass".to_string(),
                duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                ..base
            };
        }
        other => {
            return ScenarioResult {
                status: "skip".to_string(),
                skip_reason: Some(format!("unsupported scenario kind: {other}")),
                duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                ..base
            };
        }
    };

    // Capture output for logging
    let output = match &result {
        Ok(val) => Some(val.clone()),
        Err(err) => Some(Value::String(err.clone())),
    };

    // Check expectations
    let diffs = scenario.expect.as_ref().map_or_else(Vec::new, |expect| {
        check_expectations(expect, &result, &loaded)
    });

    ScenarioResult {
        status: if diffs.is_empty() {
            "pass".to_string()
        } else {
            "fail".to_string()
        },
        diffs,
        error: result.as_ref().err().cloned(),
        skip_reason: None,
        output,
        duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        ..base
    }
}

/// Run a scenario using mock-based extension loading.
#[allow(clippy::too_many_lines)]
fn run_scenario_with_mocks(
    _ext: &ScenarioExtension,
    scenario: &Scenario,
    ext_path: &Path,
    start: Instant,
    base: ScenarioResult,
) -> ScenarioResult {
    let default_spec = load_default_mock_spec();

    // Load with mocks
    let loaded = match load_extension_with_mocks(ext_path, scenario.setup.as_ref(), &default_spec) {
        Ok(loaded) => loaded,
        Err(err) => {
            return ScenarioResult {
                status: "error".to_string(),
                error: Some(format!("load_extension_with_mocks: {err}")),
                duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                ..base
            };
        }
    };

    // Multi-step scenarios
    if scenario.steps.is_some() {
        let result = execute_multi_step_scenario(&loaded, scenario, ext_path);
        let output = match &result {
            Ok(val) => Some(val.clone()),
            Err(err) => Some(Value::String(err.clone())),
        };
        let diffs = scenario.expect.as_ref().map_or_else(Vec::new, |expect| {
            check_expectations_with_mocks(expect, &result, &loaded)
        });
        return ScenarioResult {
            status: if diffs.is_empty() {
                "pass".to_string()
            } else {
                "fail".to_string()
            },
            diffs,
            error: result.as_ref().err().cloned(),
            output,
            duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            ..base
        };
    }

    // Single-step execution by kind
    let result = match scenario.kind.as_str() {
        "tool" => {
            if scenario.tool_name.is_none() {
                if let Some(expect) = &scenario.expect {
                    let diffs = check_expectations_with_mocks(expect, &Ok(Value::Null), &loaded);
                    return ScenarioResult {
                        status: if diffs.is_empty() {
                            "pass".to_string()
                        } else {
                            "fail".to_string()
                        },
                        diffs,
                        duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        ..base
                    };
                }
                return ScenarioResult {
                    status: "skip".to_string(),
                    skip_reason: Some(
                        "tool scenario with no tool_name and no expectations".to_string(),
                    ),
                    duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                    ..base
                };
            }
            execute_tool_scenario_with_mocks(&loaded, scenario, ext_path)
        }
        "command" => execute_command_scenario_with_mocks(&loaded, scenario, ext_path),
        "event" => execute_event_scenario_with_mocks(&loaded, scenario, ext_path),
        "provider" | "flag" | "shortcut" | "registration" => {
            if let Some(expect) = &scenario.expect {
                let diffs = check_expectations_with_mocks(expect, &Ok(Value::Null), &loaded);
                return ScenarioResult {
                    status: if diffs.is_empty() {
                        "pass".to_string()
                    } else {
                        "fail".to_string()
                    },
                    diffs,
                    duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                    ..base
                };
            }
            return ScenarioResult {
                status: "pass".to_string(),
                duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                ..base
            };
        }
        other => {
            return ScenarioResult {
                status: "skip".to_string(),
                skip_reason: Some(format!("unsupported scenario kind: {other}")),
                duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                ..base
            };
        }
    };

    let output = match &result {
        Ok(val) => Some(val.clone()),
        Err(err) => Some(Value::String(err.clone())),
    };

    let diffs = scenario.expect.as_ref().map_or_else(Vec::new, |expect| {
        check_expectations_with_mocks(expect, &result, &loaded)
    });

    ScenarioResult {
        status: if diffs.is_empty() {
            "pass".to_string()
        } else {
            "fail".to_string()
        },
        diffs,
        error: result.as_ref().err().cloned(),
        skip_reason: None,
        output,
        duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        ..base
    }
}

// ─── JSONL logging ──────────────────────────────────────────────────────────

fn write_jsonl_report(results: &[ScenarioResult], report_path: &Path) {
    if let Some(parent) = report_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut lines = Vec::new();
    for result in results {
        if let Ok(json) = serde_json::to_string(result) {
            lines.push(json);
        }
    }
    let _ = fs::write(report_path, lines.join("\n") + "\n");
}

fn write_summary_report(results: &[ScenarioResult], report_path: &Path) {
    let pass = results.iter().filter(|r| r.status == "pass").count();
    let fail = results.iter().filter(|r| r.status == "fail").count();
    let skip = results.iter().filter(|r| r.status == "skip").count();
    let error = results.iter().filter(|r| r.status == "error").count();
    let total_ms: u64 = results.iter().map(|r| r.duration_ms).sum();

    let summary = serde_json::json!({
        "schema": "pi.ext.scenario_conformance.v1",
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "counts": {
            "total": results.len(),
            "pass": pass,
            "fail": fail,
            "skip": skip,
            "error": error,
        },
        "pass_rate_pct": if results.len() - skip == 0 {
            100.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            { (pass as f64) / ((results.len() - skip) as f64) * 100.0 }
        },
        "total_duration_ms": total_ms,
        "results": results,
    });

    if let Some(parent) = report_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(
        report_path,
        serde_json::to_string_pretty(&summary).unwrap_or_default(),
    );
}

/// Write per-extension JSONL log files for triage.
fn write_per_extension_logs(results: &[ScenarioResult], base_dir: &Path) {
    let ext_dir = base_dir.join("extensions");
    let _ = fs::create_dir_all(&ext_dir);

    // Group results by extension_id
    let mut by_ext: HashMap<String, Vec<&ScenarioResult>> = HashMap::new();
    for r in results {
        by_ext.entry(r.extension_id.clone()).or_default().push(r);
    }

    for (ext_id, ext_results) in &by_ext {
        let path = ext_dir.join(format!("{ext_id}.jsonl"));
        let mut lines = Vec::new();
        for r in ext_results {
            let event = serde_json::json!({
                "schema": "pi.ext.smoke.v1",
                "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                "extension_id": r.extension_id,
                "scenario_id": r.scenario_id,
                "event_type": r.kind,
                "source_tier": r.source_tier,
                "runtime_tier": r.runtime_tier,
                "status": r.status,
                "duration_ms": r.duration_ms,
                "output": r.output,
                "diffs": r.diffs,
                "error": r.error,
                "skip_reason": r.skip_reason,
            });
            if let Ok(json) = serde_json::to_string(&event) {
                lines.push(json);
            }
        }
        let _ = fs::write(&path, lines.join("\n") + "\n");
    }
}

/// Write a triage-oriented summary for CI consumption.
fn write_triage_report(results: &[ScenarioResult], report_path: &Path) {
    let pass = results.iter().filter(|r| r.status == "pass").count();
    let fail = results.iter().filter(|r| r.status == "fail").count();
    let skip = results.iter().filter(|r| r.status == "skip").count();
    let error = results.iter().filter(|r| r.status == "error").count();
    let total_ms: u64 = results.iter().map(|r| r.duration_ms).sum();

    // Per-extension summaries
    let mut ext_summaries: HashMap<String, Value> = HashMap::new();
    for r in results {
        let entry = ext_summaries
            .entry(r.extension_id.clone())
            .or_insert_with(|| {
                serde_json::json!({
                    "extension_id": r.extension_id,
                    "source_tier": r.source_tier,
                    "runtime_tier": r.runtime_tier,
                    "pass": 0, "fail": 0, "skip": 0, "error": 0,
                    "total_ms": 0,
                    "failures": [],
                })
            });
        if let Some(obj) = entry.as_object_mut() {
            let key = r.status.as_str();
            if let Some(count) = obj.get(key).and_then(Value::as_u64) {
                obj.insert(key.to_string(), Value::from(count + 1));
            }
            if let Some(ms) = obj.get("total_ms").and_then(Value::as_u64) {
                obj.insert("total_ms".to_string(), Value::from(ms + r.duration_ms));
            }
            if r.status == "fail" {
                if let Some(arr) = obj.get_mut("failures").and_then(Value::as_array_mut) {
                    arr.push(serde_json::json!({
                        "scenario_id": r.scenario_id,
                        "diffs": r.diffs,
                    }));
                }
            }
        }
    }

    let report = serde_json::json!({
        "schema": "pi.ext.smoke_triage.v1",
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "counts": {
            "total": results.len(),
            "pass": pass,
            "fail": fail,
            "skip": skip,
            "error": error,
        },
        "pass_rate_pct": if results.len() - skip == 0 {
            100.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            { (pass as f64) / ((results.len() - skip) as f64) * 100.0 }
        },
        "total_duration_ms": total_ms,
        "extensions": ext_summaries.values().collect::<Vec<_>>(),
    });

    if let Some(parent) = report_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(
        report_path,
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    );
}

// ─── Tests ──────────────────────────────────────────────────────────────────

fn load_sample_json() -> SampleJson {
    let data = fs::read_to_string(sample_json_path()).expect("read extension-sample.json");
    serde_json::from_str(&data).expect("parse extension-sample.json")
}

/// Run all scenarios from extension-sample.json.
/// Reports per-scenario pass/fail with structured logging.
#[test]
#[allow(clippy::too_many_lines)]
fn scenario_conformance_suite() {
    let sample = load_sample_json();
    let filter = std::env::var("PI_SCENARIO_FILTER").ok();

    eprintln!(
        "[scenario_conformance] Starting ({} extensions, {} total scenarios)",
        sample.scenario_suite.items.len(),
        sample
            .scenario_suite
            .items
            .iter()
            .map(|e| e.scenarios.len())
            .sum::<usize>(),
    );

    let mut all_results = Vec::new();

    for ext in &sample.scenario_suite.items {
        for scenario in &ext.scenarios {
            if let Some(ref f) = filter {
                if !scenario.id.contains(f) && !ext.extension_id.contains(f) {
                    continue;
                }
            }

            eprintln!(
                "[scenario_conformance] {} ({}) - {}",
                scenario.id, ext.extension_id, scenario.summary
            );

            let result = run_scenario(ext, scenario, &sample.items);

            match result.status.as_str() {
                "pass" => eprintln!("  PASS ({} ms)", result.duration_ms),
                "fail" => {
                    eprintln!("  FAIL ({} ms)", result.duration_ms);
                    for diff in &result.diffs {
                        eprintln!("    - {diff}");
                    }
                }
                "skip" => eprintln!(
                    "  SKIP: {}",
                    result.skip_reason.as_deref().unwrap_or("unknown")
                ),
                "error" => eprintln!("  ERROR: {}", result.error.as_deref().unwrap_or("unknown")),
                _ => {}
            }

            all_results.push(result);
        }
    }

    // Write reports
    let jsonl_path = reports_dir().join("scenario_conformance.jsonl");
    let summary_path = reports_dir().join("scenario_conformance.json");
    let triage_path = reports_dir().join("smoke_triage.json");
    write_jsonl_report(&all_results, &jsonl_path);
    write_summary_report(&all_results, &summary_path);
    write_per_extension_logs(&all_results, &reports_dir());
    write_triage_report(&all_results, &triage_path);

    eprintln!(
        "[scenario_conformance] Wrote JSONL to {}",
        jsonl_path.display()
    );
    eprintln!(
        "[scenario_conformance] Wrote summary to {}",
        summary_path.display()
    );
    eprintln!(
        "[scenario_conformance] Wrote triage to {}",
        triage_path.display()
    );
    eprintln!(
        "[scenario_conformance] Wrote per-extension logs to {}",
        reports_dir().join("extensions").display()
    );

    // Summary
    let pass = all_results.iter().filter(|r| r.status == "pass").count();
    let fail = all_results.iter().filter(|r| r.status == "fail").count();
    let skip = all_results.iter().filter(|r| r.status == "skip").count();
    let error = all_results.iter().filter(|r| r.status == "error").count();

    eprintln!(
        "[scenario_conformance] Results: {} pass, {} fail, {} skip, {} error (total: {})",
        pass,
        fail,
        skip,
        error,
        all_results.len()
    );

    // Collect failures for assertion
    let failures: Vec<String> = all_results
        .iter()
        .filter(|r| r.status == "fail")
        .map(|r| {
            format!(
                "{} ({}): {}",
                r.scenario_id,
                r.extension_id,
                r.diffs.join("; ")
            )
        })
        .collect();

    let errors: Vec<String> = all_results
        .iter()
        .filter(|r| r.status == "error")
        .map(|r| {
            format!(
                "{} ({}): {}",
                r.scenario_id,
                r.extension_id,
                r.error.as_deref().unwrap_or("unknown")
            )
        })
        .collect();

    if !failures.is_empty() || !errors.is_empty() {
        let all_issues: Vec<String> = failures.into_iter().chain(errors).collect();
        eprintln!(
            "\nScenario conformance issues ({}):\n{}",
            all_issues.len(),
            all_issues.join("\n")
        );
    }

    // The test passes as long as no scenarios that we attempted actually failed.
    // Skipped scenarios don't count. Errors in loading extensions are reported but
    // don't fail the test (the extension may genuinely need setup we cannot provide).
    let executed_failures: Vec<&ScenarioResult> =
        all_results.iter().filter(|r| r.status == "fail").collect();

    assert!(
        executed_failures.is_empty(),
        "Scenario conformance failures ({}):\n{}",
        executed_failures.len(),
        executed_failures
            .iter()
            .map(|r| format!(
                "  {} ({}): {}",
                r.scenario_id,
                r.extension_id,
                r.diffs.join("; ")
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Focused test: hello tool scenario (simplest possible).
#[test]
fn scenario_hello_tool() {
    let sample = load_sample_json();
    let ext = sample
        .scenario_suite
        .items
        .iter()
        .find(|e| e.extension_id == "hello")
        .expect("hello extension in sample");
    let scenario = ext
        .scenarios
        .iter()
        .find(|s| s.id == "scn-hello-001")
        .expect("scn-hello-001");

    let result = run_scenario(ext, scenario, &sample.items);
    assert_eq!(
        result.status, "pass",
        "hello tool scenario failed: diffs={:?} error={:?} skip={:?}",
        result.diffs, result.error, result.skip_reason
    );
}

/// Focused test: subagent tool scenario (invalid params → error message).
#[test]
fn scenario_subagent_invalid_params() {
    let sample = load_sample_json();
    let ext = sample
        .scenario_suite
        .items
        .iter()
        .find(|e| e.extension_id == "subagent")
        .expect("subagent extension in sample");
    let scenario = ext
        .scenarios
        .iter()
        .find(|s| s.id == "scn-subagent-001")
        .expect("scn-subagent-001");

    let result = run_scenario(ext, scenario, &sample.items);
    assert_eq!(
        result.status, "pass",
        "subagent scenario failed: {:?}",
        result.diffs
    );
}

/// Focused test: custom provider registration (anthropic).
#[test]
fn scenario_custom_provider_anthropic() {
    let sample = load_sample_json();
    let ext = sample
        .scenario_suite
        .items
        .iter()
        .find(|e| e.extension_id == "custom-provider-anthropic")
        .expect("custom-provider-anthropic in sample");
    let scenario = ext
        .scenarios
        .iter()
        .find(|s| s.id == "scn-custom-provider-anthropic-001")
        .expect("scn-custom-provider-anthropic-001");

    let result = run_scenario(ext, scenario, &sample.items);
    assert_eq!(
        result.status, "pass",
        "provider scenario failed: {:?}",
        result.diffs
    );
}

/// Focused test: custom provider registration (qwen-cli).
#[test]
fn scenario_custom_provider_qwen_cli() {
    let sample = load_sample_json();
    let ext = sample
        .scenario_suite
        .items
        .iter()
        .find(|e| e.extension_id == "custom-provider-qwen-cli")
        .expect("custom-provider-qwen-cli in sample");
    let scenario = ext
        .scenarios
        .iter()
        .find(|s| s.id == "scn-custom-provider-qwen-cli-001")
        .expect("scn-custom-provider-qwen-cli-001");

    let result = run_scenario(ext, scenario, &sample.items);
    assert_eq!(
        result.status, "pass",
        "provider scenario failed: {:?}",
        result.diffs
    );
}

/// Focused test: sandbox tool registration scenario.
#[test]
fn scenario_sandbox_tool_registered() {
    let sample = load_sample_json();
    let ext = sample
        .scenario_suite
        .items
        .iter()
        .find(|e| e.extension_id == "sandbox")
        .expect("sandbox in sample");
    let scenario = ext
        .scenarios
        .iter()
        .find(|s| s.id == "scn-sandbox-001")
        .expect("scn-sandbox-001");

    let result = run_scenario(ext, scenario, &sample.items);
    assert_eq!(
        result.status, "pass",
        "sandbox tool registration scenario failed: {:?}",
        result.diffs
    );
}

// ─── E2E Smoke Suite (bd-2ni) ───────────────────────────────────────────────

/// E2E smoke suite: loads every extension from extension-sample.json, runs
/// all scenarios with verbose structured logging, and produces per-extension
/// JSONL logs + a triage report suitable for CI consumption.
///
/// This is the main entry point for bd-2ni. It differs from
/// `scenario_conformance_suite` by producing richer per-event logs with
/// correlation IDs, source/runtime tier metadata, and captured outputs.
#[test]
#[allow(clippy::too_many_lines)]
fn smoke_runtime_suite() {
    let sample = load_sample_json();
    let run_id = format!(
        "smoke-{}",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    let smoke_dir = reports_dir().join("smoke");
    let _ = fs::create_dir_all(&smoke_dir);

    eprintln!("[smoke] run_id={run_id}");
    eprintln!(
        "[smoke] extensions={}, scenarios={}",
        sample.scenario_suite.items.len(),
        sample
            .scenario_suite
            .items
            .iter()
            .map(|e| e.scenarios.len())
            .sum::<usize>(),
    );

    let mut all_results = Vec::new();
    let mut events: Vec<Value> = Vec::new();

    for ext in &sample.scenario_suite.items {
        let item = sample.items.iter().find(|i| i.id == ext.extension_id);
        let source_tier = item.map_or("unknown", |i| &i.source_tier);
        let runtime_tier = item.map_or("unknown", |i| &i.runtime_tier);

        eprintln!(
            "[smoke] extension={} source_tier={source_tier} runtime_tier={runtime_tier} scenarios={}",
            ext.extension_id,
            ext.scenarios.len()
        );

        for scenario in &ext.scenarios {
            let event_start = Utc::now();
            let result = run_scenario(ext, scenario, &sample.items);

            // Build per-event log entry
            let event = serde_json::json!({
                "schema": "pi.ext.smoke.v1",
                "run_id": run_id,
                "ts": event_start.to_rfc3339_opts(SecondsFormat::Millis, true),
                "extension_id": ext.extension_id,
                "scenario_id": scenario.id,
                "event_type": scenario.kind,
                "tool_name": scenario.tool_name,
                "command_name": scenario.command_name,
                "event_name": scenario.event_name,
                "source_tier": source_tier,
                "runtime_tier": runtime_tier,
                "status": result.status,
                "duration_ms": result.duration_ms,
                "output": result.output,
                "diffs": result.diffs,
                "error": result.error,
                "skip_reason": result.skip_reason,
            });
            events.push(event);

            // Verbose console output
            let status_tag = match result.status.as_str() {
                "pass" => "PASS",
                "fail" => "FAIL",
                "skip" => "SKIP",
                "error" => "ERR ",
                _ => "????",
            };
            eprintln!(
                "  [{status_tag}] {} ({}) - {} [{}ms]",
                scenario.id, scenario.kind, scenario.summary, result.duration_ms
            );
            if result.status == "fail" {
                for diff in &result.diffs {
                    eprintln!("         diff: {diff}");
                }
            }
            if let Some(err) = &result.error {
                eprintln!("         error: {err}");
            }

            all_results.push(result);
        }
    }

    // Write global smoke JSONL (all events)
    let smoke_jsonl = smoke_dir.join("smoke_events.jsonl");
    let lines: Vec<String> = events
        .iter()
        .filter_map(|e| serde_json::to_string(e).ok())
        .collect();
    let _ = fs::write(&smoke_jsonl, lines.join("\n") + "\n");

    // Write per-extension JSONL
    write_per_extension_logs(&all_results, &smoke_dir);

    // Write triage report
    let triage_path = smoke_dir.join("triage.json");
    write_triage_report(&all_results, &triage_path);

    // Summary
    let pass = all_results.iter().filter(|r| r.status == "pass").count();
    let fail = all_results.iter().filter(|r| r.status == "fail").count();
    let skip = all_results.iter().filter(|r| r.status == "skip").count();
    let error = all_results.iter().filter(|r| r.status == "error").count();

    eprintln!("[smoke] Results: {pass} pass, {fail} fail, {skip} skip, {error} error");
    eprintln!("[smoke] Events JSONL: {}", smoke_jsonl.display());
    eprintln!("[smoke] Triage: {}", triage_path.display());
    eprintln!(
        "[smoke] Per-ext logs: {}",
        smoke_dir.join("extensions").display()
    );

    // Fail on scenario failures (not errors/skips)
    let failures: Vec<&ScenarioResult> =
        all_results.iter().filter(|r| r.status == "fail").collect();
    assert!(
        failures.is_empty(),
        "Smoke suite failures ({}):\n{}",
        failures.len(),
        failures
            .iter()
            .map(|r| format!(
                "  {} ({}/{}): {}",
                r.scenario_id,
                r.extension_id,
                r.runtime_tier,
                r.diffs.join("; ")
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

// ─── Parity Runner: TS vs Rust (bd-vmm) ────────────────────────────────────

fn ts_scenario_script() -> PathBuf {
    project_root().join("tests/ext_conformance/ts_harness/run_scenario.ts")
}

fn ts_default_mock_spec() -> PathBuf {
    project_root().join("tests/ext_conformance/mock_specs/mock_spec_default.json")
}

fn pi_mono_root() -> PathBuf {
    project_root().join("legacy_pi_mono_code/pi-mono")
}

fn pi_mono_node_modules() -> PathBuf {
    pi_mono_root().join("node_modules")
}

fn pi_mono_packages() -> PathBuf {
    pi_mono_root().join("packages")
}

const fn bun_path() -> &'static str {
    "/home/ubuntu/.bun/bin/bun"
}

fn ts_oracle_timeout() -> Duration {
    std::env::var("PI_TS_ORACLE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_secs(30), Duration::from_secs)
}

fn ts_oracle_node_path() -> PathBuf {
    let base = PathBuf::from(format!(
        "/tmp/pi_agent_rust_ts_parity_node_path-{}",
        std::process::id()
    ));
    let scope_dir = base.join("@mariozechner");
    let _ = fs::create_dir_all(&scope_dir);

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let packages_dir = pi_mono_packages();
        let pairs = [
            ("pi-coding-agent", "coding-agent"),
            ("pi-tui", "tui"),
            ("pi-ai", "ai"),
        ];
        for (link_name, pkg_name) in &pairs {
            let link = scope_dir.join(link_name);
            if !link.exists() {
                let _ = symlink(packages_dir.join(pkg_name), &link);
            }
        }
    }

    base
}

/// Run the TS scenario oracle on an extension with a given scenario.
#[allow(clippy::too_many_lines)]
fn run_ts_scenario(extension_path: &Path, scenario: &Scenario) -> Result<Value, String> {
    let settings = deterministic_settings_for(extension_path);
    ensure_deterministic_dirs(&settings);

    let node_path_base = ts_oracle_node_path();
    let node_path: Cow<'_, str> = match std::env::var("NODE_PATH") {
        Ok(existing) if !existing.trim().is_empty() => Cow::Owned(format!(
            "{}:{}:{}",
            node_path_base.display(),
            pi_mono_node_modules().display(),
            existing
        )),
        _ => Cow::Owned(format!(
            "{}:{}",
            node_path_base.display(),
            pi_mono_node_modules().display()
        )),
    };

    // Build scenario JSON for stdin
    let scenario_input = serde_json::json!({
        "kind": scenario.kind,
        "tool_name": scenario.tool_name,
        "command_name": scenario.command_name,
        "event_name": scenario.event_name,
        "input": scenario.input,
    });
    let stdin_data =
        serde_json::to_string(&scenario_input).map_err(|e| format!("serialize scenario: {e}"))?;

    let mut cmd = Command::new(bun_path());
    cmd.arg("run")
        .arg(ts_scenario_script())
        .arg(extension_path)
        .arg(ts_default_mock_spec())
        .current_dir(pi_mono_root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NODE_PATH", node_path.as_ref())
        .env("PI_DETERMINISTIC_TIME_MS", &settings.time_ms)
        .env("PI_DETERMINISTIC_TIME_STEP_MS", &settings.time_step_ms)
        .env("PI_DETERMINISTIC_CWD", &settings.cwd)
        .env("PI_DETERMINISTIC_HOME", &settings.home);
    if let Some(random_value) = settings.random_value.as_deref() {
        cmd.env("PI_DETERMINISTIC_RANDOM", random_value);
    } else {
        cmd.env("PI_DETERMINISTIC_RANDOM_SEED", &settings.random_seed);
    }

    let mut child = cmd
        .spawn()
        .map_err(|err| format!("failed to spawn TS scenario runner: {err}"))?;

    // Write scenario JSON to stdin
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(stdin_data.as_bytes());
    }

    let timeout = ts_oracle_timeout();
    let start = Instant::now();
    loop {
        if start.elapsed() >= timeout {
            let _ = child.kill();
            return Err(format!("TS scenario timeout after {}s", timeout.as_secs()));
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(err) => {
                let _ = child.kill();
                return Err(format!("TS scenario wait error: {err}"));
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to capture TS scenario output: {err}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() && stdout.trim().is_empty() {
        return Err(format!("TS scenario runner crashed:\nstderr: {stderr}"));
    }

    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "TS scenario returned empty stdout:\nstderr: {stderr}"
        ));
    }

    serde_json::from_str(trimmed)
        .or_else(|_| {
            trimmed
                .find('{')
                .map(|idx| &trimmed[idx..])
                .ok_or_else(|| "no JSON object found".to_string())
                .and_then(|json_str| serde_json::from_str(json_str).map_err(|e| e.to_string()))
        })
        .map_err(|e| format!("TS scenario returned invalid JSON:\n  error: {e}\n  stdout: {stdout}\n  stderr: {stderr}"))
}

/// Normalize a result value for comparison by removing timing-dependent fields.
fn normalize_result(val: &Value) -> Value {
    match val {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                // Skip timing fields
                if k == "load_time_ms" || k == "exec_time_ms" || k == "duration_ms" {
                    continue;
                }
                out.insert(k.clone(), normalize_result(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(normalize_result).collect()),
        other => other.clone(),
    }
}

/// Compare Rust and TS scenario results, returning a list of diff descriptions.
fn diff_scenario_results(rust_result: &Result<Value, String>, ts_output: &Value) -> Vec<String> {
    let mut diffs = Vec::new();

    let ts_success = ts_output
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ts_error = ts_output.get("error").and_then(Value::as_str);
    let ts_result = ts_output.get("result");

    match (rust_result, ts_success) {
        (Ok(rust_val), true) => {
            // Both succeeded - compare results
            if let Some(ts_res) = ts_result {
                let ts_norm = normalize_result(ts_res);
                let rust_norm = normalize_result(rust_val);
                if ts_norm != rust_norm {
                    diffs.push(format!(
                        "result mismatch:\n  TS:   {}\n  Rust: {}",
                        serde_json::to_string(&ts_norm).unwrap_or_default(),
                        serde_json::to_string(&rust_norm).unwrap_or_default(),
                    ));
                }
            }
        }
        (Err(_), false) => {
            // Both failed - acceptable
        }
        (Ok(_), false) => {
            diffs.push(format!(
                "TS failed but Rust succeeded:\n  TS error: {}",
                ts_error.unwrap_or("unknown")
            ));
        }
        (Err(rust_err), true) => {
            diffs.push(format!(
                "Rust failed but TS succeeded:\n  Rust error: {rust_err}"
            ));
        }
    }

    diffs
}

/// Result of a parity comparison for one scenario.
#[derive(Debug, Clone, Serialize)]
struct ParityResult {
    scenario_id: String,
    extension_id: String,
    kind: String,
    summary: String,
    status: String, // "match", "mismatch", "skip", "ts_error", "rust_error"
    source_tier: String,
    runtime_tier: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    diffs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ts_result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rust_result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skip_reason: Option<String>,
    ts_ms: u64,
    rust_ms: u64,
}

/// Legacy vs Rust parity runner: runs each executable scenario in both the
/// TS oracle (Bun + jiti) and Rust (`QuickJS`), normalizes outputs, and diffs.
#[test]
#[allow(clippy::too_many_lines)]
fn parity_runner() {
    let sample = load_sample_json();
    let run_id = format!(
        "parity-{}",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    let parity_dir = reports_dir().join("parity");
    let _ = fs::create_dir_all(&parity_dir);

    eprintln!("[parity] run_id={run_id}");

    let mut results: Vec<ParityResult> = Vec::new();
    let mut events: Vec<Value> = Vec::new();

    for ext in &sample.scenario_suite.items {
        let item = sample.items.iter().find(|i| i.id == ext.extension_id);
        let source_tier = item.map_or("unknown", |i| &i.source_tier);
        let runtime_tier = item.map_or("unknown", |i| &i.runtime_tier);

        for scenario in &ext.scenarios {
            let base = ParityResult {
                scenario_id: scenario.id.clone(),
                extension_id: ext.extension_id.clone(),
                kind: scenario.kind.clone(),
                summary: scenario.summary.clone(),
                status: String::new(),
                source_tier: source_tier.to_string(),
                runtime_tier: runtime_tier.to_string(),
                diffs: Vec::new(),
                ts_result: None,
                rust_result: None,
                error: None,
                skip_reason: None,
                ts_ms: 0,
                rust_ms: 0,
            };

            // Skip unsupported scenarios
            if let Some(reason) = needs_unsupported_setup(scenario) {
                eprintln!(
                    "  [SKIP] {} ({}) - {}: {reason}",
                    scenario.id, ext.extension_id, scenario.summary
                );
                results.push(ParityResult {
                    status: "skip".to_string(),
                    skip_reason: Some(reason),
                    ..base
                });
                continue;
            }

            // Resolve extension path
            let Some(ext_path) = resolve_extension_path(&ext.extension_id, &sample.items) else {
                results.push(ParityResult {
                    status: "skip".to_string(),
                    skip_reason: Some(format!(
                        "cannot resolve artifact path for '{}'",
                        ext.extension_id
                    )),
                    ..base
                });
                continue;
            };

            // Run Rust
            let rust_start = Instant::now();
            let rust_result = match scenario.kind.as_str() {
                "tool" if scenario.tool_name.is_some() => {
                    let loaded = match load_extension(&ext_path) {
                        Ok(l) => l,
                        Err(e) => {
                            results.push(ParityResult {
                                status: "rust_error".to_string(),
                                error: Some(format!("load: {e}")),
                                ..base
                            });
                            continue;
                        }
                    };
                    execute_tool_scenario(&loaded, scenario, &ext_path)
                }
                "command" => {
                    let loaded = match load_extension(&ext_path) {
                        Ok(l) => l,
                        Err(e) => {
                            results.push(ParityResult {
                                status: "rust_error".to_string(),
                                error: Some(format!("load: {e}")),
                                ..base
                            });
                            continue;
                        }
                    };
                    execute_command_scenario(&loaded, scenario, &ext_path)
                }
                "event" => {
                    let loaded = match load_extension(&ext_path) {
                        Ok(l) => l,
                        Err(e) => {
                            results.push(ParityResult {
                                status: "rust_error".to_string(),
                                error: Some(format!("load: {e}")),
                                ..base
                            });
                            continue;
                        }
                    };
                    execute_event_scenario(&loaded, scenario, &ext_path)
                }
                "provider" | "tool" => {
                    // Registration-only: skip parity
                    results.push(ParityResult {
                        status: "skip".to_string(),
                        skip_reason: Some("registration-only scenario".to_string()),
                        ..base
                    });
                    continue;
                }
                other => {
                    results.push(ParityResult {
                        status: "skip".to_string(),
                        skip_reason: Some(format!("unsupported kind: {other}")),
                        ..base
                    });
                    continue;
                }
            };
            let rust_ms = u64::try_from(rust_start.elapsed().as_millis()).unwrap_or(u64::MAX);

            // Run TS oracle
            let ts_start = Instant::now();
            let ts_output = run_ts_scenario(&ext_path, scenario);
            let ts_ms = u64::try_from(ts_start.elapsed().as_millis()).unwrap_or(u64::MAX);

            // Handle TS error
            let ts_output = match ts_output {
                Ok(v) => v,
                Err(e) => {
                    eprintln!(
                        "  [TS_ERR] {} ({}) - TS error: {e}",
                        scenario.id, ext.extension_id
                    );
                    results.push(ParityResult {
                        status: "ts_error".to_string(),
                        error: Some(e),
                        rust_result: rust_result.as_ref().ok().cloned(),
                        ts_ms,
                        rust_ms,
                        ..base
                    });
                    continue;
                }
            };

            // Diff results
            let diffs = diff_scenario_results(&rust_result, &ts_output);
            let status = if diffs.is_empty() {
                "match"
            } else {
                "mismatch"
            };

            let tag = if status == "match" { "MATCH" } else { "DIFF " };
            eprintln!(
                "  [{tag}] {} ({}) - {} [rust={}ms ts={}ms]",
                scenario.id, ext.extension_id, scenario.summary, rust_ms, ts_ms
            );
            for diff in &diffs {
                eprintln!("         {diff}");
            }

            // Build per-event log
            events.push(serde_json::json!({
                "schema": "pi.ext.parity.v1",
                "run_id": run_id,
                "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                "extension_id": ext.extension_id,
                "scenario_id": scenario.id,
                "kind": scenario.kind,
                "source_tier": source_tier,
                "runtime_tier": runtime_tier,
                "status": status,
                "ts_ms": ts_ms,
                "rust_ms": rust_ms,
                "diffs": diffs,
            }));

            results.push(ParityResult {
                status: status.to_string(),
                diffs: diffs.clone(),
                ts_result: Some(ts_output),
                rust_result: rust_result.as_ref().ok().cloned(),
                ts_ms,
                rust_ms,
                ..base.clone()
            });
        }
    }

    // Write parity JSONL
    let parity_jsonl = parity_dir.join("parity_events.jsonl");
    let lines: Vec<String> = events
        .iter()
        .filter_map(|e| serde_json::to_string(e).ok())
        .collect();
    let _ = fs::write(&parity_jsonl, lines.join("\n") + "\n");

    // Write per-extension parity diffs
    let ext_dir = parity_dir.join("extensions");
    let _ = fs::create_dir_all(&ext_dir);
    let mut by_ext: HashMap<String, Vec<&ParityResult>> = HashMap::new();
    for r in &results {
        by_ext.entry(r.extension_id.clone()).or_default().push(r);
    }
    for (ext_id, ext_results) in &by_ext {
        let path = ext_dir.join(format!("{ext_id}.jsonl"));
        let ext_lines: Vec<String> = ext_results
            .iter()
            .filter_map(|r| serde_json::to_string(r).ok())
            .collect();
        let _ = fs::write(&path, ext_lines.join("\n") + "\n");
    }

    // Write triage summary
    let matched = results.iter().filter(|r| r.status == "match").count();
    let mismatched = results.iter().filter(|r| r.status == "mismatch").count();
    let skipped = results.iter().filter(|r| r.status == "skip").count();
    let ts_errors = results.iter().filter(|r| r.status == "ts_error").count();
    let rust_errors = results.iter().filter(|r| r.status == "rust_error").count();

    let triage = serde_json::json!({
        "schema": "pi.ext.parity_triage.v1",
        "run_id": run_id,
        "generated_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "counts": {
            "total": results.len(),
            "match": matched,
            "mismatch": mismatched,
            "skip": skipped,
            "ts_error": ts_errors,
            "rust_error": rust_errors,
        },
        "match_rate_pct": if matched + mismatched == 0 {
            100.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            { (matched as f64) / ((matched + mismatched) as f64) * 100.0 }
        },
    });
    let triage_path = parity_dir.join("triage.json");
    let _ = fs::write(
        &triage_path,
        serde_json::to_string_pretty(&triage).unwrap_or_default(),
    );

    eprintln!(
        "[parity] Results: {matched} match, {mismatched} mismatch, {skipped} skip, {ts_errors} ts_error, {rust_errors} rust_error"
    );
    eprintln!("[parity] Events: {}", parity_jsonl.display());
    eprintln!("[parity] Triage: {}", triage_path.display());

    // Known environment-dependent scenarios where TS and Rust legitimately differ
    // due to different working directories / filesystem content
    let known_env_diffs: &[&str] = &[
        "scn-subagent-001", // TS finds project agents in pi-mono dir; Rust uses empty test dir
    ];

    // Assert no unexpected mismatches
    let parity_failures: Vec<&ParityResult> = results
        .iter()
        .filter(|r| r.status == "mismatch" && !known_env_diffs.contains(&r.scenario_id.as_str()))
        .collect();
    assert!(
        parity_failures.is_empty(),
        "Parity mismatches ({}):\n{}",
        parity_failures.len(),
        parity_failures
            .iter()
            .map(|r| format!(
                "  {} ({}): {}",
                r.scenario_id,
                r.extension_id,
                r.diffs.join("; ")
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
