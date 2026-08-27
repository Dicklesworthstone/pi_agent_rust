//! Mock specification format validation tests.
//!
//! Validates:
//! - JSON schema is well-formed
//! - Example fixtures parse correctly
//! - Roundtrip serialization is lossless
//! - Edge cases (empty specs, minimal specs) are handled
//!
//! Bead: bd-1yfi

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Mock Spec Types (Rust representation of the JSON schema)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MockSpec {
    schema: String,
    extension_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<SessionMock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    http: Option<HttpMock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exec: Option<ExecMock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<ToolsMock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ui: Option<UiMock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    events: Option<EventsMock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<ModelMock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMock {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    messages: Option<Vec<SessionMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entries: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<Vec<serde_json::Value>>,
    #[serde(default = "default_true")]
    accept_mutations: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HttpMock {
    #[serde(default)]
    rules: Vec<HttpRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_response: Option<HttpResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HttpRule {
    #[serde(rename = "match")]
    match_rule: HttpMatch,
    response: HttpResponse,
    #[serde(default)]
    times: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HttpMatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url_exact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url_pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    headers: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body_contains: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HttpResponse {
    status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    headers: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body_json: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExecMock {
    #[serde(default)]
    rules: Vec<ExecRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_result: Option<ExecResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExecRule {
    #[serde(rename = "match")]
    match_rule: ExecMatch,
    result: ExecResult,
    #[serde(default)]
    times: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExecMatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    cmd_exact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cmd_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cmd_pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    args_contain: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExecResult {
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    #[serde(default)]
    code: i32,
    #[serde(default)]
    killed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolsMock {
    #[serde(skip_serializing_if = "Option::is_none")]
    active_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    all_tools: Option<Vec<ToolDefinition>>,
    #[serde(default)]
    invocations: Vec<ToolInvocationRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolDefinition {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolInvocationRule {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_match: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default)]
    times: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UiMock {
    #[serde(default = "default_true")]
    capture: bool,
    #[serde(default)]
    responses: BTreeMap<String, serde_json::Value>,
    #[serde(default = "default_true")]
    confirm_default: bool,
    #[serde(default)]
    dialog_default: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventsMock {
    #[serde(default)]
    fire_sequence: Vec<EventFire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventFire {
    event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_response: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelMock {
    #[serde(skip_serializing_if = "Option::is_none")]
    current: Option<ModelSelection>,
    #[serde(default = "default_off")]
    thinking_level: String,
    #[serde(default)]
    available_models: Vec<serde_json::Value>,
    #[serde(default = "default_true")]
    accept_mutations: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelSelection {
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

const fn default_true() -> bool {
    true
}

fn default_off() -> String {
    "off".to_string()
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/ext_conformance/fixtures")
}

fn schema_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/schema")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn json_schema_is_valid_json() {
    let path = schema_dir().join("mock_spec.json");
    let content = std::fs::read_to_string(&path).expect("read schema file");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("parse schema JSON");
    assert_eq!(
        parsed["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(parsed["title"], "Pi Extension Mock Specification");
    assert!(
        parsed["$defs"].is_object(),
        "schema should have $defs section"
    );
}

#[test]
fn schema_covers_all_hostcall_categories() {
    let path = schema_dir().join("mock_spec.json");
    let content = std::fs::read_to_string(&path).expect("read schema");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("parse schema");
    let props = parsed["properties"].as_object().expect("properties");

    // All hostcall categories must be present
    let required_sections = ["session", "http", "exec", "tools", "ui", "events", "model"];
    for section in &required_sections {
        assert!(
            props.contains_key(*section),
            "schema must define {section} section"
        );
    }
}

#[test]
fn schema_defines_all_event_types() {
    let path = schema_dir().join("mock_spec.json");
    let content = std::fs::read_to_string(&path).expect("read schema");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("parse schema");

    let event_enum = &parsed["$defs"]["event_fire"]["properties"]["event"]["enum"];
    let events = event_enum.as_array().expect("event enum");
    let event_names: Vec<&str> = events.iter().map(|v| v.as_str().expect("string")).collect();

    let expected = [
        "tool_call",
        "tool_result",
        "turn_start",
        "turn_end",
        "before_agent_start",
        "input",
        "context",
        "resources_discover",
        "user_bash",
        "session_before_compact",
        "session_before_tree",
    ];
    for ev in &expected {
        assert!(
            event_names.contains(ev),
            "event type {ev} missing from schema"
        );
    }
}

#[test]
fn default_fixture_parses_to_mock_spec() {
    let path = fixtures_dir().join("mock_spec_default.json");
    let content = std::fs::read_to_string(&path).expect("read default fixture");
    let spec: MockSpec = serde_json::from_str(&content).expect("parse default fixture");

    assert_eq!(spec.schema, "pi.ext.mock_spec.v1");
    assert_eq!(spec.extension_id, "_default");

    // Session
    let session = spec.session.as_ref().expect("session");
    assert_eq!(session.name.as_deref(), Some("test-session"));
    assert!(session.accept_mutations);
    let messages = session.messages.as_ref().expect("messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].role, "assistant");

    // HTTP
    let http = spec.http.as_ref().expect("http");
    assert!(http.rules.is_empty());
    let default_resp = http.default_response.as_ref().expect("default_response");
    assert_eq!(default_resp.status, 404);

    // Exec
    let exec = spec.exec.as_ref().expect("exec");
    assert!(exec.rules.is_empty());
    let default_result = exec.default_result.as_ref().expect("default_result");
    assert_eq!(default_result.code, 127);

    // Tools
    let tools = spec.tools.as_ref().expect("tools");
    let active = tools.active_tools.as_ref().expect("active_tools");
    assert!(active.contains(&"read".to_string()));
    let all = tools.all_tools.as_ref().expect("all_tools");
    assert!(all.len() >= 5);

    // UI
    let ui = spec.ui.as_ref().expect("ui");
    assert!(ui.capture);
    assert!(ui.confirm_default);

    // Model
    let model = spec.model.as_ref().expect("model");
    let current = model.current.as_ref().expect("current model");
    assert_eq!(current.provider.as_deref(), Some("anthropic"));
    assert_eq!(model.thinking_level, "off");
    assert_ne!(model.available_models, [] as [serde_json::Value; 0]);
}

#[test]
fn http_fixture_parses_with_rules() {
    let path = fixtures_dir().join("mock_spec_with_http.json");
    let content = std::fs::read_to_string(&path).expect("read http fixture");
    let spec: MockSpec = serde_json::from_str(&content).expect("parse http fixture");

    assert_eq!(spec.extension_id, "git-checkpoint");

    // HTTP rules
    let http = spec.http.as_ref().expect("http");
    assert_eq!(http.rules.len(), 1);
    let rule = &http.rules[0];
    assert_eq!(rule.match_rule.method.as_deref(), Some("GET"));
    assert!(rule.match_rule.url_prefix.is_some());
    assert_eq!(rule.response.status, 200);

    // Exec rules
    let exec = spec.exec.as_ref().expect("exec");
    assert!(
        exec.rules.len() >= 4,
        "expected 4+ exec rules, got {}",
        exec.rules.len()
    );

    // Check git status rule
    let status_rule = exec
        .rules
        .iter()
        .find(|r| {
            r.match_rule
                .args_contain
                .as_ref()
                .is_some_and(|a| a.contains(&"status".to_string()))
        })
        .expect("git status rule");
    assert_eq!(status_rule.result.code, 0);
    assert!(status_rule.result.stdout.contains("main.rs"));

    // Check stash rule has times=1
    let stash_rule = exec
        .rules
        .iter()
        .find(|r| {
            r.match_rule
                .args_contain
                .as_ref()
                .is_some_and(|a| a.contains(&"stash".to_string()))
        })
        .expect("git stash rule");
    assert_eq!(stash_rule.times, 1, "stash should only match once");

    // Events
    let events = spec.events.as_ref().expect("events");
    assert_eq!(events.fire_sequence.len(), 1);
    assert_eq!(events.fire_sequence[0].event, "turn_end");
}

#[test]
fn roundtrip_serialization_is_lossless() {
    let path = fixtures_dir().join("mock_spec_default.json");
    let content = std::fs::read_to_string(&path).expect("read fixture");
    let spec: MockSpec = serde_json::from_str(&content).expect("parse");
    let reserialized = serde_json::to_string_pretty(&spec).expect("serialize");
    let spec2: MockSpec = serde_json::from_str(&reserialized).expect("reparse");
    let reserialized2 = serde_json::to_string_pretty(&spec2).expect("serialize2");
    assert_eq!(reserialized, reserialized2, "roundtrip should be stable");
}

#[test]
fn minimal_mock_spec_parses() {
    let json = r#"{"schema": "pi.ext.mock_spec.v1", "extension_id": "test"}"#;
    let spec: MockSpec = serde_json::from_str(json).expect("parse minimal spec");
    assert_eq!(spec.extension_id, "test");
    assert!(spec.session.is_none());
    assert!(spec.http.is_none());
    assert!(spec.exec.is_none());
    assert!(spec.tools.is_none());
    assert!(spec.ui.is_none());
    assert!(spec.events.is_none());
    assert!(spec.model.is_none());
}

#[test]
fn exec_match_variants_parse() {
    let json = r#"{
        "schema": "pi.ext.mock_spec.v1",
        "extension_id": "exec-test",
        "exec": {
            "rules": [
                {
                    "match": { "cmd_exact": "git" },
                    "result": { "stdout": "ok", "code": 0 }
                },
                {
                    "match": { "cmd_prefix": "npm " },
                    "result": { "stdout": "installed", "code": 0 }
                },
                {
                    "match": { "cmd_pattern": "^curl\\s" },
                    "result": { "stdout": "{}", "code": 0 }
                },
                {
                    "match": { "cmd_exact": "ls", "args_contain": ["-la"] },
                    "result": { "stdout": "total 0", "code": 0 }
                }
            ]
        }
    }"#;
    let spec: MockSpec = serde_json::from_str(json).expect("parse exec variants");
    let exec = spec.exec.expect("exec section");
    assert_eq!(exec.rules.len(), 4);
    assert!(exec.rules[0].match_rule.cmd_exact.is_some());
    assert!(exec.rules[1].match_rule.cmd_prefix.is_some());
    assert!(exec.rules[2].match_rule.cmd_pattern.is_some());
    assert!(exec.rules[3].match_rule.args_contain.is_some());
}

#[test]
fn http_match_variants_parse() {
    let json = r#"{
        "schema": "pi.ext.mock_spec.v1",
        "extension_id": "http-test",
        "http": {
            "rules": [
                {
                    "match": { "url_exact": "https://api.example.com/v1/data" },
                    "response": { "status": 200, "body": "ok" }
                },
                {
                    "match": { "url_prefix": "https://api.example.com/", "method": "POST" },
                    "response": { "status": 201, "body_json": {"id": 42} }
                },
                {
                    "match": { "url_pattern": "https://.*\\.example\\.com/.*", "headers": {"Authorization": "Bearer test"} },
                    "response": { "status": 200, "body": "authed" }
                },
                {
                    "match": { "body_contains": "search_term" },
                    "response": { "status": 200, "body": "found" }
                }
            ]
        }
    }"#;
    let spec: MockSpec = serde_json::from_str(json).expect("parse http variants");
    let http = spec.http.expect("http section");
    assert_eq!(http.rules.len(), 4);
    assert!(http.rules[0].match_rule.url_exact.is_some());
    assert!(http.rules[1].match_rule.url_prefix.is_some());
    assert_eq!(http.rules[1].match_rule.method.as_deref(), Some("POST"));
    assert!(http.rules[2].match_rule.url_pattern.is_some());
    assert!(http.rules[2].match_rule.headers.is_some());
    assert!(http.rules[3].match_rule.body_contains.is_some());
}

#[test]
fn tool_invocation_with_error_parses() {
    let json = r#"{
        "schema": "pi.ext.mock_spec.v1",
        "extension_id": "tool-error-test",
        "tools": {
            "invocations": [
                {
                    "name": "read",
                    "input_match": {"path": "/etc/shadow"},
                    "error": "Permission denied",
                    "times": 1
                },
                {
                    "name": "bash",
                    "result": {"output": "hello world"},
                    "times": 0
                }
            ]
        }
    }"#;
    let spec: MockSpec = serde_json::from_str(json).expect("parse tool invocations");
    let tools = spec.tools.expect("tools section");
    assert_eq!(tools.invocations.len(), 2);
    assert!(tools.invocations[0].error.is_some());
    assert!(tools.invocations[0].result.is_none());
    assert!(tools.invocations[1].result.is_some());
    assert!(tools.invocations[1].error.is_none());
}

#[test]
fn event_fire_sequence_parses() {
    let json = r#"{
        "schema": "pi.ext.mock_spec.v1",
        "extension_id": "events-test",
        "events": {
            "fire_sequence": [
                {
                    "event": "turn_start",
                    "payload": {}
                },
                {
                    "event": "tool_call",
                    "payload": {"tool": "bash", "input": {"command": "echo hi"}},
                    "expected_response": {"allow": true}
                },
                {
                    "event": "tool_result",
                    "payload": {"tool": "bash", "result": {"output": "hi"}}
                },
                {
                    "event": "turn_end",
                    "payload": {}
                }
            ]
        }
    }"#;
    let spec: MockSpec = serde_json::from_str(json).expect("parse event sequence");
    let events = spec.events.expect("events section");
    assert_eq!(events.fire_sequence.len(), 4);
    assert_eq!(events.fire_sequence[0].event, "turn_start");
    assert_eq!(events.fire_sequence[1].event, "tool_call");
    assert!(events.fire_sequence[1].expected_response.is_some());
    assert_eq!(events.fire_sequence[3].event, "turn_end");
}

#[test]
fn session_with_rich_messages_parses() {
    let json = r#"{
        "schema": "pi.ext.mock_spec.v1",
        "extension_id": "session-test",
        "session": {
            "name": "rich-session",
            "messages": [
                {
                    "id": "msg-1",
                    "role": "user",
                    "content": "Simple string content"
                },
                {
                    "id": "msg-2",
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "Here is the result:"},
                        {"type": "tool_use", "id": "tu-1", "name": "bash", "input": {"command": "ls"}}
                    ]
                },
                {
                    "id": "msg-3",
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "tu-1", "content": "file1.txt\nfile2.txt"}
                    ]
                }
            ],
            "entries": [
                {"type": "custom", "data": {"key": "value"}}
            ]
        }
    }"#;
    let spec: MockSpec = serde_json::from_str(json).expect("parse session with messages");
    let session = spec.session.expect("session");
    let messages = session.messages.expect("messages");
    assert_eq!(messages.len(), 3);
    // String content
    assert!(messages[0].content.as_ref().unwrap().is_string());
    // Array content
    assert!(messages[1].content.as_ref().unwrap().is_array());
    assert_eq!(
        messages[1]
            .content
            .as_ref()
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // Entries
    let entries = session.entries.expect("entries");
    assert_eq!(entries.len(), 1);
}

#[test]
fn model_mock_with_available_models_parses() {
    let json = r#"{
        "schema": "pi.ext.mock_spec.v1",
        "extension_id": "model-test",
        "model": {
            "current": {
                "provider": "openai",
                "model_id": "gpt-4o",
                "name": "GPT-4o"
            },
            "thinking_level": "medium",
            "available_models": [
                {"provider": "openai", "id": "gpt-4o", "name": "GPT-4o"},
                {"provider": "anthropic", "id": "claude-sonnet-4-5", "name": "Claude Sonnet 4.5", "reasoning": true}
            ],
            "accept_mutations": false
        }
    }"#;
    let spec: MockSpec = serde_json::from_str(json).expect("parse model mock");
    let model = spec.model.expect("model");
    assert_eq!(model.thinking_level, "medium");
    assert!(!model.accept_mutations);
    assert_eq!(model.available_models.len(), 2);
    let current = model.current.expect("current");
    assert_eq!(current.provider.as_deref(), Some("openai"));
}
