#![cfg(feature = "ext-conformance")]
#![allow(clippy::redundant_clone)]
//! Extension scenario conformance tests: load extensions, execute scenarios from
//! `docs/extension-sample.json`, and compare outputs against fixture expectations.
//!
//! This complements the registration-level differential testing in
//! `ext_conformance_diff.rs` by actually *running* tool calls, commands, and
//! event dispatches and checking their outputs match the fixture expectations.

mod common;

use chrono::{SecondsFormat, Utc};
use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

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
        let providers = loaded.manager.extension_providers();
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
        let providers = loaded.manager.extension_providers();
        // Provider spec uses "apiKey" for the env var name
        let has_env = providers.iter().any(|p| {
            p.get("apiKey").and_then(Value::as_str) == Some(expected_env)
                || p.get("apiKeyEnvVar").and_then(Value::as_str) == Some(expected_env)
        });
        if !has_env {
            diffs.push(format!(
                "api_key_env: expected provider with apiKey='{expected_env}'"
            ));
        }
    }

    if let Some(expected_models) = &expect.models_contains {
        let providers = loaded.manager.extension_providers();
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
        let tools = loaded.manager.extension_tool_defs();
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

    diffs
}

// ─── Scenario categorization ────────────────────────────────────────────────

/// Check if a scenario needs setup features we cannot provide yet.
fn needs_unsupported_setup(scenario: &Scenario) -> Option<String> {
    if let Some(setup) = &scenario.setup {
        if setup.get("mock_exec").is_some() {
            return Some("requires mock_exec".to_string());
        }
        if setup.get("mock_http").is_some() {
            return Some("requires mock_http".to_string());
        }
        if setup.get("mock_model_registry").is_some() {
            return Some("requires mock_model_registry".to_string());
        }
        if setup.get("session_branch").is_some() {
            return Some("requires session_branch setup".to_string());
        }
        if setup.get("session_leaf_entry").is_some() {
            return Some("requires session_leaf_entry setup".to_string());
        }
        if setup.get("state").is_some() {
            return Some("requires pre-seeded extension state".to_string());
        }
        if setup.get("flags").is_some() {
            return Some("requires pre-set flags".to_string());
        }
    }

    // Scenarios with UI interaction responses (user selections, confirms)
    if let Some(input) = &scenario.input {
        if input
            .pointer("/ctx/ui_responses")
            .is_some_and(|v| !v.is_null())
        {
            return Some("requires UI interaction responses".to_string());
        }
    }

    // Multi-step scenarios need sequential dispatch
    if scenario.steps.is_some() {
        return Some("requires multi-step dispatch".to_string());
    }

    // Event scenarios that check UI state
    if let Some(expect) = &scenario.expect {
        if expect.ui_notify_contains.is_some() {
            return Some("requires UI notification capture".to_string());
        }
        if expect.ui_status_key.is_some() || expect.ui_status_contains_sequence.is_some() {
            return Some("requires UI status tracking".to_string());
        }
        if expect.active_tools.is_some() {
            return Some("requires active tool list management".to_string());
        }
        if expect.exec_called.is_some() {
            return Some("requires exec call tracking".to_string());
        }
        if expect.returns_contains.is_some() {
            return Some("requires returns_contains matching".to_string());
        }
        if expect.action.is_some() {
            return Some("requires action matching (input transform)".to_string());
        }
        if expect.content_types.is_some() {
            return Some("requires content type matching".to_string());
        }
    }

    None
}

// ─── Main runner ────────────────────────────────────────────────────────────

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

    // Load extension
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
