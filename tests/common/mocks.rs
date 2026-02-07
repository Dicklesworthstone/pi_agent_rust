//! Reusable mock infrastructure for extension testing (bd-9dqa).
//!
//! Provides composable, deterministic mocks for all hostcall subsystems:
//! - **UI**: [`ScriptedUiHandler`] — queued responses per method, full recording
//! - **Session**: [`RecordingSession`] — real state tracking with assertion helpers
//! - **Exec**: [`ExecFixture`] — temp scripts returning configured stdout/stderr/exit
//! - **HTTP**: [`VcrCassetteBuilder`] — programmatic VCR cassette construction
//! - **FS**: [`FsFixture`] — temp directory with pre-populated file layout
//! - **Recording**: [`HostcallLog`] — unified interaction recording for all types

use async_trait::async_trait;
use pi::error::Result;
use pi::extension_dispatcher::ExtensionUiHandler;
use pi::extensions::{ExtensionSession, ExtensionUiRequest, ExtensionUiResponse};
use pi::session::SessionMessage;
use serde_json::Value;
use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// ─── Hostcall Log (unified recording) ──────────────────────────────────────

/// A single recorded hostcall interaction.
#[derive(Debug, Clone)]
pub struct HostcallEntry {
    pub kind: HostcallKind,
    pub request: Value,
    pub response: Value,
    pub timestamp: Instant,
}

/// Hostcall type discriminator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostcallKind {
    Ui,
    Session,
    Exec,
    Http,
    Fs,
    Tool,
}

impl fmt::Display for HostcallKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ui => write!(f, "ui"),
            Self::Session => write!(f, "session"),
            Self::Exec => write!(f, "exec"),
            Self::Http => write!(f, "http"),
            Self::Fs => write!(f, "fs"),
            Self::Tool => write!(f, "tool"),
        }
    }
}

/// Thread-safe log of all hostcall interactions.
#[derive(Debug, Clone, Default)]
pub struct HostcallLog {
    entries: Arc<Mutex<Vec<HostcallEntry>>>,
}

impl HostcallLog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a hostcall interaction.
    pub fn record(&self, kind: HostcallKind, request: Value, response: Value) {
        self.entries.lock().unwrap().push(HostcallEntry {
            kind,
            request,
            response,
            timestamp: Instant::now(),
        });
    }

    /// Get all recorded entries.
    #[must_use]
    pub fn entries(&self) -> Vec<HostcallEntry> {
        self.entries.lock().unwrap().clone()
    }

    /// Get entries filtered by kind.
    #[must_use]
    pub fn entries_of(&self, kind: HostcallKind) -> Vec<HostcallEntry> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.kind == kind)
            .cloned()
            .collect()
    }

    /// Number of recorded entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    /// Whether the log is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.lock().unwrap().is_empty()
    }

    /// Assert exactly N calls of a given kind.
    pub fn assert_count(&self, kind: HostcallKind, expected: usize) {
        let actual = self.entries_of(kind.clone()).len();
        assert_eq!(
            actual, expected,
            "Expected {expected} {kind} hostcalls, got {actual}"
        );
    }

    /// Clear all entries.
    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }
}

// ─── Scripted UI Handler ───────────────────────────────────────────────────

/// Pre-configured response for a specific UI method.
#[derive(Debug, Clone)]
pub struct UiResponse {
    /// Which method this response applies to (e.g., "select", "confirm", "input").
    /// Use `None` to match any method.
    pub method: Option<String>,
    /// The value to return.
    pub value: Option<Value>,
    /// Whether the response is cancelled.
    pub cancelled: bool,
}

impl UiResponse {
    /// Create a success response for a specific method.
    #[must_use]
    pub fn success(method: impl Into<String>, value: Value) -> Self {
        Self {
            method: Some(method.into()),
            value: Some(value),
            cancelled: false,
        }
    }

    /// Create a cancelled response for a specific method.
    #[must_use]
    pub fn cancelled(method: impl Into<String>) -> Self {
        Self {
            method: Some(method.into()),
            value: None,
            cancelled: true,
        }
    }

    /// Create a success response matching any method.
    #[must_use]
    pub fn any_success(value: Value) -> Self {
        Self {
            method: None,
            value: Some(value),
            cancelled: false,
        }
    }

    /// Create a cancelled response matching any method.
    #[must_use]
    pub fn any_cancelled() -> Self {
        Self {
            method: None,
            value: None,
            cancelled: true,
        }
    }
}

/// Recorded UI request with its response.
#[derive(Debug, Clone)]
pub struct UiCall {
    pub request: ExtensionUiRequest,
    pub response_value: Option<Value>,
    pub cancelled: bool,
}

/// A UI handler that returns pre-scripted responses from a queue.
///
/// Responses are consumed in order. Method-specific responses are matched
/// first; wildcard (`method: None`) responses serve as fallbacks.
/// If no matching response remains, returns `Ok(None)` (no response).
pub struct ScriptedUiHandler {
    responses: Arc<Mutex<VecDeque<UiResponse>>>,
    calls: Arc<Mutex<Vec<UiCall>>>,
    log: Option<HostcallLog>,
}

impl ScriptedUiHandler {
    /// Create a new handler with a queue of responses.
    #[must_use]
    pub fn new(responses: Vec<UiResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::from(responses))),
            calls: Arc::new(Mutex::new(Vec::new())),
            log: None,
        }
    }

    /// Create an empty handler (all requests get `None` response).
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Attach a hostcall log for unified recording.
    #[must_use]
    pub fn with_log(mut self, log: HostcallLog) -> Self {
        self.log = Some(log);
        self
    }

    /// Get all recorded UI calls.
    #[must_use]
    pub fn calls(&self) -> Vec<UiCall> {
        self.calls.lock().unwrap().clone()
    }

    /// Number of UI calls made.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// Assert that the Nth call had a specific method.
    pub fn assert_call_method(&self, index: usize, expected_method: &str) {
        let calls = self.calls.lock().unwrap();
        assert!(
            index < calls.len(),
            "Expected UI call at index {index}, but only {} calls recorded",
            calls.len()
        );
        assert_eq!(
            calls[index].request.method, expected_method,
            "UI call {index}: expected method '{expected_method}', got '{}'",
            calls[index].request.method
        );
    }

    /// Assert that the Nth call was cancelled.
    pub fn assert_call_cancelled(&self, index: usize) {
        let calls = self.calls.lock().unwrap();
        assert!(
            index < calls.len(),
            "Expected UI call at index {index}, but only {} calls recorded",
            calls.len()
        );
        assert!(
            calls[index].cancelled,
            "UI call {index}: expected cancelled, but was not"
        );
    }
}

#[async_trait]
impl ExtensionUiHandler for ScriptedUiHandler {
    async fn request_ui(&self, request: ExtensionUiRequest) -> Result<Option<ExtensionUiResponse>> {
        let mut queue = self.responses.lock().unwrap();

        // Find first matching response (method-specific match preferred)
        let pos = queue
            .iter()
            .position(|r| r.method.as_deref() == Some(&request.method))
            .or_else(|| queue.iter().position(|r| r.method.is_none()));

        let response = pos.map(|i| queue.remove(i).unwrap());

        let (value, cancelled) = response
            .as_ref()
            .map_or((None, false), |r| (r.value.clone(), r.cancelled));

        drop(queue);

        self.calls.lock().unwrap().push(UiCall {
            request: request.clone(),
            response_value: value.clone(),
            cancelled,
        });

        if let Some(ref log) = self.log {
            log.record(
                HostcallKind::Ui,
                serde_json::json!({
                    "method": request.method,
                    "payload": request.payload,
                }),
                serde_json::json!({
                    "value": value,
                    "cancelled": cancelled,
                }),
            );
        }

        if response.is_some() || request.expects_response() {
            Ok(Some(ExtensionUiResponse {
                id: request.id,
                value,
                cancelled,
            }))
        } else {
            Ok(None)
        }
    }
}

// ─── Recording Session ─────────────────────────────────────────────────────

/// Recorded session operation.
#[derive(Debug, Clone)]
pub struct SessionCall {
    pub op: String,
    pub args: Value,
    pub timestamp: Instant,
}

/// A session implementation that tracks state and records all operations.
///
/// Unlike the `TestSession` in `extension_dispatcher.rs` (behind `#[cfg(test)]`),
/// this one is usable across test crates and provides assertion helpers.
pub struct RecordingSession {
    state: Mutex<Value>,
    messages: Mutex<Vec<SessionMessage>>,
    entries: Mutex<Vec<Value>>,
    branch: Mutex<Vec<Value>>,
    custom_entries: Mutex<Vec<(String, Option<Value>)>>,
    labels: Mutex<Vec<(String, Option<String>)>>,
    calls: Mutex<Vec<SessionCall>>,
    log: Option<HostcallLog>,
}

impl RecordingSession {
    /// Create a new session with empty state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(serde_json::json!({})),
            messages: Mutex::new(Vec::new()),
            entries: Mutex::new(Vec::new()),
            branch: Mutex::new(Vec::new()),
            custom_entries: Mutex::new(Vec::new()),
            labels: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            log: None,
        }
    }

    /// Create with initial state.
    #[must_use]
    pub fn with_state(state: Value) -> Self {
        let s = Self::new();
        *s.state.lock().unwrap() = state;
        s
    }

    /// Attach a hostcall log for unified recording.
    #[must_use]
    pub fn with_log(mut self, log: HostcallLog) -> Self {
        self.log = Some(log);
        self
    }

    fn record_call(&self, op: &str, args: Value) {
        self.calls.lock().unwrap().push(SessionCall {
            op: op.to_string(),
            args: args.clone(),
            timestamp: Instant::now(),
        });
        if let Some(ref log) = self.log {
            log.record(
                HostcallKind::Session,
                serde_json::json!({ "op": op, "args": args }),
                Value::Null,
            );
        }
    }

    /// Get all recorded session calls.
    #[must_use]
    pub fn calls(&self) -> Vec<SessionCall> {
        self.calls.lock().unwrap().clone()
    }

    /// Get appended custom entries.
    #[must_use]
    pub fn custom_entries(&self) -> Vec<(String, Option<Value>)> {
        self.custom_entries.lock().unwrap().clone()
    }

    /// Get set labels.
    #[must_use]
    pub fn labels(&self) -> Vec<(String, Option<String>)> {
        self.labels.lock().unwrap().clone()
    }

    /// Assert that a specific custom entry was appended.
    pub fn assert_custom_entry(&self, custom_type: &str) {
        let entries = self.custom_entries.lock().unwrap();
        assert!(
            entries.iter().any(|(t, _)| t == custom_type),
            "Expected custom entry of type '{custom_type}', found: {:?}",
            entries.iter().map(|(t, _)| t).collect::<Vec<_>>()
        );
    }

    /// Assert a label was set on a target.
    pub fn assert_label_set(&self, target_id: &str, expected_label: Option<&str>) {
        let labels = self.labels.lock().unwrap();
        assert!(
            labels
                .iter()
                .any(|(id, l)| id == target_id && l.as_deref() == expected_label),
            "Expected label set on '{target_id}' with value {expected_label:?}, found: {labels:?}"
        );
    }

    /// Assert at least one message was appended.
    pub fn assert_messages_appended(&self, min_count: usize) {
        let count = self.messages.lock().unwrap().len();
        assert!(
            count >= min_count,
            "Expected at least {min_count} appended messages, got {count}"
        );
    }

    /// Assert model was set to specific values.
    pub fn assert_model_set(&self, expected_provider: &str, expected_model: &str) {
        let state = self.state.lock().unwrap();
        let provider = state.get("provider").and_then(Value::as_str).unwrap_or("");
        let model = state.get("modelId").and_then(Value::as_str).unwrap_or("");
        assert_eq!(
            provider, expected_provider,
            "Expected provider '{expected_provider}', got '{provider}'"
        );
        assert_eq!(
            model, expected_model,
            "Expected model '{expected_model}', got '{model}'"
        );
    }
}

#[async_trait]
impl ExtensionSession for RecordingSession {
    async fn get_state(&self) -> Value {
        self.record_call("get_state", Value::Null);
        self.state.lock().unwrap().clone()
    }

    async fn get_messages(&self) -> Vec<SessionMessage> {
        self.record_call("get_messages", Value::Null);
        self.messages.lock().unwrap().clone()
    }

    async fn get_entries(&self) -> Vec<Value> {
        self.record_call("get_entries", Value::Null);
        self.entries.lock().unwrap().clone()
    }

    async fn get_branch(&self) -> Vec<Value> {
        self.record_call("get_branch", Value::Null);
        self.branch.lock().unwrap().clone()
    }

    async fn set_name(&self, name: String) -> Result<()> {
        self.record_call("set_name", serde_json::json!({ "name": name }));
        let mut state = self.state.lock().unwrap();
        if let Value::Object(ref mut map) = *state {
            map.insert("sessionName".to_string(), Value::String(name));
        }
        Ok(())
    }

    async fn append_message(&self, message: SessionMessage) -> Result<()> {
        self.record_call(
            "append_message",
            serde_json::json!({ "variant": format!("{message:?}").chars().take(80).collect::<String>() }),
        );
        self.messages.lock().unwrap().push(message);
        Ok(())
    }

    async fn append_custom_entry(&self, custom_type: String, data: Option<Value>) -> Result<()> {
        self.record_call(
            "append_custom_entry",
            serde_json::json!({ "type": custom_type, "data": data }),
        );
        self.custom_entries
            .lock()
            .unwrap()
            .push((custom_type, data));
        Ok(())
    }

    async fn set_model(&self, provider: String, model_id: String) -> Result<()> {
        self.record_call(
            "set_model",
            serde_json::json!({ "provider": provider, "model_id": model_id }),
        );
        let mut state = self.state.lock().unwrap();
        if let Value::Object(ref mut map) = *state {
            map.insert("provider".to_string(), Value::String(provider));
            map.insert("modelId".to_string(), Value::String(model_id));
        }
        Ok(())
    }

    async fn get_model(&self) -> (Option<String>, Option<String>) {
        self.record_call("get_model", Value::Null);
        let state = self.state.lock().unwrap();
        let provider = state
            .get("provider")
            .and_then(Value::as_str)
            .map(String::from);
        let model_id = state
            .get("modelId")
            .and_then(Value::as_str)
            .map(String::from);
        (provider, model_id)
    }

    async fn set_thinking_level(&self, level: String) -> Result<()> {
        self.record_call("set_thinking_level", serde_json::json!({ "level": level }));
        let mut state = self.state.lock().unwrap();
        if let Value::Object(ref mut map) = *state {
            map.insert("thinkingLevel".to_string(), Value::String(level));
        }
        Ok(())
    }

    async fn get_thinking_level(&self) -> Option<String> {
        self.record_call("get_thinking_level", Value::Null);
        let state = self.state.lock().unwrap();
        state
            .get("thinkingLevel")
            .and_then(Value::as_str)
            .map(String::from)
    }

    async fn set_label(&self, target_id: String, label: Option<String>) -> Result<()> {
        self.record_call(
            "set_label",
            serde_json::json!({ "target_id": target_id, "label": label }),
        );
        self.labels.lock().unwrap().push((target_id, label));
        Ok(())
    }
}

// ─── Exec Fixture ──────────────────────────────────────────────────────────

/// A single pre-configured exec response rule.
#[derive(Debug, Clone)]
pub struct ExecRule {
    /// Command substring to match (or `None` for wildcard).
    pub match_cmd: Option<String>,
    /// Stdout content to return.
    pub stdout: String,
    /// Stderr content to return.
    pub stderr: String,
    /// Exit code to return.
    pub exit_code: i32,
}

impl ExecRule {
    /// Create a success rule (exit 0, stdout only).
    #[must_use]
    pub fn success(stdout: impl Into<String>) -> Self {
        Self {
            match_cmd: None,
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: 0,
        }
    }

    /// Create a success rule matching a specific command.
    #[must_use]
    pub fn success_for(cmd: impl Into<String>, stdout: impl Into<String>) -> Self {
        Self {
            match_cmd: Some(cmd.into()),
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: 0,
        }
    }

    /// Create a failure rule.
    #[must_use]
    pub fn failure(exit_code: i32, stderr: impl Into<String>) -> Self {
        Self {
            match_cmd: None,
            stdout: String::new(),
            stderr: stderr.into(),
            exit_code,
        }
    }

    /// Create a failure rule matching a specific command.
    #[must_use]
    pub fn failure_for(cmd: impl Into<String>, exit_code: i32, stderr: impl Into<String>) -> Self {
        Self {
            match_cmd: Some(cmd.into()),
            stdout: String::new(),
            stderr: stderr.into(),
            exit_code,
        }
    }
}

/// Creates temp shell scripts that return predetermined output.
///
/// Instead of intercepting the dispatcher's exec path (which uses real shell),
/// this provides a `PATH`-override approach: generate wrapper scripts in a
/// temp directory that shadow real commands.
pub struct ExecFixture {
    dir: PathBuf,
    rules: Vec<ExecRule>,
}

impl ExecFixture {
    /// Create an exec fixture under the given temp directory.
    #[must_use]
    pub fn new(base_dir: &Path) -> Self {
        let dir = base_dir.join("exec_mocks");
        let _ = std::fs::create_dir_all(&dir);
        Self {
            dir,
            rules: Vec::new(),
        }
    }

    /// Add a rule and create the corresponding mock script.
    pub fn add_rule(&mut self, command_name: &str, rule: &ExecRule) {
        let script_path = self.dir.join(command_name);
        let script = format!(
            "#!/bin/sh\nprintf '%s' '{}' >&2\nprintf '%s' '{}'\nexit {}\n",
            rule.stderr.replace('\'', "'\\''"),
            rule.stdout.replace('\'', "'\\''"),
            rule.exit_code,
        );
        std::fs::write(&script_path, script).expect("write mock script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod mock script");
        }
        self.rules.push(rule.clone());
    }

    /// Get the mock directory path (prepend to `PATH`).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Build a `PATH` string with mock dir prepended.
    #[must_use]
    pub fn path_env(&self) -> String {
        let existing = std::env::var("PATH").unwrap_or_default();
        format!("{}:{existing}", self.dir.display())
    }
}

// ─── VCR Cassette Builder ──────────────────────────────────────────────────

/// Programmatic builder for VCR cassettes.
///
/// Creates cassette JSON files compatible with the VCR playback system.
pub struct VcrCassetteBuilder {
    test_name: String,
    interactions: Vec<Value>,
}

impl VcrCassetteBuilder {
    /// Create a new cassette builder for a given test name.
    #[must_use]
    pub fn new(test_name: impl Into<String>) -> Self {
        Self {
            test_name: test_name.into(),
            interactions: Vec::new(),
        }
    }

    /// Add an HTTP interaction (request → response).
    #[must_use]
    pub fn add_interaction(
        mut self,
        method: &str,
        url: &str,
        request_body: Value,
        status: u16,
        response_body: Value,
    ) -> Self {
        self.interactions.push(serde_json::json!({
            "request": {
                "method": method,
                "url": url,
                "body": request_body,
                "headers": {}
            },
            "response": {
                "status": status,
                "headers": {
                    "content-type": "application/json"
                },
                "body": response_body
            }
        }));
        self
    }

    /// Add a streaming SSE interaction.
    #[must_use]
    pub fn add_sse_interaction(
        mut self,
        method: &str,
        url: &str,
        request_body: Value,
        events: Vec<String>,
    ) -> Self {
        let body = events.join("");
        self.interactions.push(serde_json::json!({
            "request": {
                "method": method,
                "url": url,
                "body": request_body,
                "headers": {}
            },
            "response": {
                "status": 200,
                "headers": {
                    "content-type": "text/event-stream"
                },
                "body": body
            }
        }));
        self
    }

    /// Build the cassette JSON.
    #[must_use]
    pub fn build(&self) -> Value {
        serde_json::json!({
            "version": 1,
            "test_name": self.test_name,
            "recorded_at": "2026-01-01T00:00:00Z",
            "interactions": self.interactions
        })
    }

    /// Write the cassette to a directory.
    pub fn write_to(&self, dir: &Path) -> PathBuf {
        let _ = std::fs::create_dir_all(dir);
        let path = dir.join(format!("{}.json", self.test_name));
        let json = serde_json::to_string_pretty(&self.build()).expect("serialize cassette");
        std::fs::write(&path, json).expect("write cassette");
        path
    }
}

// ─── FS Fixture ────────────────────────────────────────────────────────────

/// Pre-populated temporary file layout for testing.
pub struct FsFixture {
    root: PathBuf,
}

impl FsFixture {
    /// Create a fixture rooted at the given directory.
    #[must_use]
    pub fn new(root: &Path) -> Self {
        let _ = std::fs::create_dir_all(root);
        Self {
            root: root.to_path_buf(),
        }
    }

    /// Add a file with string content.
    pub fn add_file(&self, relative_path: &str, content: &str) -> PathBuf {
        let path = self.root.join(relative_path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, content).expect("write fixture file");
        path
    }

    /// Add a file with binary content.
    pub fn add_binary(&self, relative_path: &str, content: &[u8]) -> PathBuf {
        let path = self.root.join(relative_path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, content).expect("write fixture binary");
        path
    }

    /// Add an empty directory.
    pub fn add_dir(&self, relative_path: &str) -> PathBuf {
        let path = self.root.join(relative_path);
        std::fs::create_dir_all(&path).expect("create fixture dir");
        path
    }

    /// Root path of the fixture.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Assert a file exists with specific content.
    pub fn assert_file_content(&self, relative_path: &str, expected: &str) {
        let path = self.root.join(relative_path);
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("File not found: {}", path.display()));
        assert_eq!(
            content, expected,
            "File content mismatch at {}",
            relative_path
        );
    }

    /// Assert a file exists.
    pub fn assert_exists(&self, relative_path: &str) {
        let path = self.root.join(relative_path);
        assert!(path.exists(), "Expected file to exist: {}", path.display());
    }

    /// Assert a file does not exist.
    pub fn assert_not_exists(&self, relative_path: &str) {
        let path = self.root.join(relative_path);
        assert!(
            !path.exists(),
            "Expected file to not exist: {}",
            path.display()
        );
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    // Navigate up from mocks::tests → mocks → common/mod.rs to reach run_async.
    // This works both when included from integration tests (crate::common) and from
    // the lib crate (session_index::test_common).
    use super::super::run_async;

    #[test]
    fn scripted_ui_returns_queued_responses() {
        let handler: Arc<ScriptedUiHandler> = Arc::new(ScriptedUiHandler::new(vec![
            UiResponse::success("select", serde_json::json!("option_a")),
            UiResponse::cancelled("confirm"),
            UiResponse::any_success(serde_json::json!(42)),
        ]));

        // First call: select → "option_a"
        let resp = run_async({
            let h = Arc::clone(&handler);
            async move {
                h.request_ui(ExtensionUiRequest::new("r1", "select", Value::Null))
                    .await
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(resp.value, Some(serde_json::json!("option_a")));
        assert!(!resp.cancelled);

        // Second call: confirm → cancelled
        let resp = run_async({
            let h = Arc::clone(&handler);
            async move {
                h.request_ui(ExtensionUiRequest::new("r2", "confirm", Value::Null))
                    .await
            }
        })
        .unwrap()
        .unwrap();
        assert!(resp.cancelled);

        // Third call: any method → 42
        let resp = run_async({
            let h = Arc::clone(&handler);
            async move {
                h.request_ui(ExtensionUiRequest::new("r3", "input", Value::Null))
                    .await
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(resp.value, Some(serde_json::json!(42)));

        assert_eq!(handler.call_count(), 3);
        handler.assert_call_method(0, "select");
        handler.assert_call_method(1, "confirm");
        handler.assert_call_method(2, "input");
    }

    #[test]
    fn scripted_ui_empty_returns_none_for_non_response() {
        let handler: Arc<ScriptedUiHandler> = Arc::new(ScriptedUiHandler::empty());

        // toast doesn't expect a response
        let resp = run_async({
            let h = Arc::clone(&handler);
            async move {
                h.request_ui(ExtensionUiRequest::new("r1", "toast", Value::Null))
                    .await
            }
        })
        .unwrap();
        assert!(resp.is_none());
    }

    #[test]
    fn recording_session_tracks_operations() {
        let session: Arc<RecordingSession> = Arc::new(RecordingSession::new());

        run_async({
            let s = Arc::clone(&session);
            async move {
                s.set_name("test-session".to_string()).await.unwrap();
                s.set_model("anthropic".to_string(), "claude-3".to_string())
                    .await
                    .unwrap();
                s.append_custom_entry(
                    "bookmark".to_string(),
                    Some(serde_json::json!({"url": "https://example.com"})),
                )
                .await
                .unwrap();
                s.set_label("entry-1".to_string(), Some("important".to_string()))
                    .await
                    .unwrap();
            }
        });

        // Verify call recording
        let calls = session.calls();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0].op, "set_name");
        assert_eq!(calls[1].op, "set_model");

        // Verify assertion helpers
        session.assert_model_set("anthropic", "claude-3");
        session.assert_custom_entry("bookmark");
        session.assert_label_set("entry-1", Some("important"));
    }

    #[test]
    fn recording_session_with_initial_state() {
        let session: Arc<RecordingSession> =
            Arc::new(RecordingSession::with_state(serde_json::json!({
                "provider": "openai",
                "modelId": "gpt-4",
            })));

        let result: (Option<String>, Option<String>) = run_async({
            let s = Arc::clone(&session);
            async move { s.get_model().await }
        });
        assert_eq!(result.0.as_deref(), Some("openai"));
        assert_eq!(result.1.as_deref(), Some("gpt-4"));
    }

    #[test]
    fn hostcall_log_records_and_filters() {
        let log = HostcallLog::new();

        log.record(
            HostcallKind::Ui,
            serde_json::json!({"method": "select"}),
            serde_json::json!({"value": "a"}),
        );
        log.record(
            HostcallKind::Session,
            serde_json::json!({"op": "set_name"}),
            Value::Null,
        );
        log.record(
            HostcallKind::Ui,
            serde_json::json!({"method": "confirm"}),
            serde_json::json!({"value": true}),
        );

        assert_eq!(log.len(), 3);
        assert_eq!(log.entries_of(HostcallKind::Ui).len(), 2);
        assert_eq!(log.entries_of(HostcallKind::Session).len(), 1);
        assert_eq!(log.entries_of(HostcallKind::Exec).len(), 0);

        log.assert_count(HostcallKind::Ui, 2);
        log.assert_count(HostcallKind::Session, 1);
    }

    #[test]
    fn vcr_cassette_builder_creates_valid_json() {
        let cassette = VcrCassetteBuilder::new("test_chat")
            .add_interaction(
                "POST",
                "https://api.anthropic.com/v1/messages",
                serde_json::json!({"messages": [{"role": "user", "content": "hello"}]}),
                200,
                serde_json::json!({"content": [{"type": "text", "text": "hi"}]}),
            )
            .build();

        assert_eq!(cassette["version"], 1);
        assert_eq!(cassette["test_name"], "test_chat");
        assert_eq!(cassette["interactions"].as_array().unwrap().len(), 1);
        let interaction = &cassette["interactions"][0];
        assert_eq!(interaction["request"]["method"], "POST");
        assert_eq!(interaction["response"]["status"], 200);
    }

    #[test]
    fn vcr_cassette_builder_writes_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = VcrCassetteBuilder::new("write_test")
            .add_interaction(
                "GET",
                "https://example.com",
                Value::Null,
                200,
                serde_json::json!("ok"),
            )
            .write_to(dir.path());

        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["test_name"], "write_test");
    }

    #[test]
    fn fs_fixture_creates_and_verifies_files() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = FsFixture::new(dir.path());

        fixture.add_file("src/main.rs", "fn main() {}");
        fixture.add_file("nested/deep/file.txt", "hello");
        fixture.add_dir("empty_dir");

        fixture.assert_exists("src/main.rs");
        fixture.assert_file_content("src/main.rs", "fn main() {}");
        fixture.assert_exists("nested/deep/file.txt");
        fixture.assert_exists("empty_dir");
        fixture.assert_not_exists("nonexistent.txt");
    }

    #[test]
    fn exec_fixture_creates_mock_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let mut fixture = ExecFixture::new(dir.path());

        fixture.add_rule("echo_test", &ExecRule::success("hello world"));
        fixture.add_rule("fail_cmd", &ExecRule::failure(1, "something went wrong"));

        let echo_path = fixture.path().join("echo_test");
        assert!(echo_path.exists());

        // Verify the script is executable and returns correct output
        let output = std::process::Command::new(echo_path)
            .current_dir(dir.path())
            .output()
            .expect("run mock script");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hello world");
        assert!(output.status.success());

        let fail_path = fixture.path().join("fail_cmd");
        let output = std::process::Command::new(fail_path)
            .current_dir(dir.path())
            .output()
            .expect("run mock script");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "something went wrong"
        );
        assert_eq!(output.status.code(), Some(1));
    }

    #[test]
    fn scripted_ui_with_log_records_to_hostcall_log() {
        let log = HostcallLog::new();
        let handler: Arc<ScriptedUiHandler> = Arc::new(
            ScriptedUiHandler::new(vec![UiResponse::any_success(serde_json::json!("chosen"))])
                .with_log(log.clone()),
        );

        run_async({
            let h = Arc::clone(&handler);
            async move {
                h.request_ui(ExtensionUiRequest::new(
                    "r1",
                    "select",
                    serde_json::json!({"options": ["a", "b"]}),
                ))
                .await
                .unwrap();
            }
        });

        log.assert_count(HostcallKind::Ui, 1);
        let entry = &log.entries()[0];
        assert_eq!(entry.request["method"], "select");
    }

    #[test]
    fn recording_session_with_log_records_to_hostcall_log() {
        let log = HostcallLog::new();
        let session: Arc<RecordingSession> =
            Arc::new(RecordingSession::new().with_log(log.clone()));

        run_async({
            let s = Arc::clone(&session);
            async move {
                s.set_name("tracked".to_string()).await.unwrap();
            }
        });

        log.assert_count(HostcallKind::Session, 1);
        let entry = &log.entries()[0];
        assert_eq!(entry.request["op"], "set_name");
    }

    #[test]
    fn scripted_ui_method_specific_matching() {
        // Queue: confirm response first, then select response
        let handler: Arc<ScriptedUiHandler> = Arc::new(ScriptedUiHandler::new(vec![
            UiResponse::success("confirm", serde_json::json!(true)),
            UiResponse::success("select", serde_json::json!("item_1")),
        ]));

        // Call select first — should match the select response, not confirm
        let resp = run_async({
            let h = Arc::clone(&handler);
            async move {
                h.request_ui(ExtensionUiRequest::new("r1", "select", Value::Null))
                    .await
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(resp.value, Some(serde_json::json!("item_1")));

        // Now call confirm — should match the confirm response
        let resp = run_async({
            let h = Arc::clone(&handler);
            async move {
                h.request_ui(ExtensionUiRequest::new("r2", "confirm", Value::Null))
                    .await
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(resp.value, Some(serde_json::json!(true)));
    }
}
