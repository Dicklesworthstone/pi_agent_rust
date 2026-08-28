#![forbid(unsafe_code)]

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const PI_TEST_RUNNER: &str = env!("CARGO_BIN_EXE_pi");
const RPC_TEST_PROVIDER: &str = "ollama";
const RPC_TEST_MODEL: &str = "qwen2.5:0.5b";
const UI_SCENARIOS: &str =
    include_str!("dropin_extension_ui_differential/fixtures/g05_extension_ui_scenarios.json");
const UI_SCENARIO_TIMEOUT: Duration = Duration::from_secs(15);

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

fn scenario_command_name(scenario: &Value) -> String {
    let id = scenario["id"].as_str().expect("scenario id");
    format!("g05-{id}")
}

fn write_scenario_extension(
    root: &Path,
    scenario: &Value,
    command_name: &str,
) -> TestResult<PathBuf> {
    let scenario_id = scenario["id"].as_str().expect("scenario id");
    let requests = scenario["requests"].as_array().expect("scenario requests");
    let requests_json = serde_json::to_string(requests)?;
    let command_name_json = serde_json::to_string(command_name)?;
    let description_json =
        serde_json::to_string(&format!("Exercise extension UI scenario {scenario_id}"))?;
    let source = format!(
        r#"export default function init(pi) {{
  const steps = {requests_json};
  pi.registerCommand({command_name_json}, {{
    description: {description_json},
    handler: async () => {{
      const results = [];
      for (const step of steps) {{
        if (step.type !== "prompt") {{
          continue;
        }}
        const payload = Object.assign({{}}, step.ui_payload || {{}});
        if (typeof step.timeout_ms === "number") {{
          payload.timeout = step.timeout_ms;
        }}
        results.push(await pi.ui(step.ui_method, payload));
      }}
      return {{ display: JSON.stringify(results), results }};
    }}
  }});
}}
"#
    );

    let extension_path = root.join(format!("{command_name}.mjs"));
    std::fs::write(&extension_path, source)?;
    Ok(extension_path)
}

fn ui_request_expects_response(event: &Value) -> bool {
    matches!(
        event.get("method").and_then(Value::as_str),
        Some(
            "select"
                | "confirm"
                | "input"
                | "editor"
                | "custom"
                | "getEditorText"
                | "get_editor_text"
                | "getAllThemes"
                | "get_all_themes"
                | "getTheme"
                | "get_theme"
                | "setTheme"
                | "set_theme"
        )
    )
}

fn first_string(array_value: Option<&Value>) -> Option<&str> {
    array_value?
        .as_array()?
        .iter()
        .find_map(serde_json::Value::as_str)
}

fn response_for_ui_event(event: &Value, index: usize) -> Value {
    let request_id = event["id"]
        .as_str()
        .expect("extension UI request id should be a string");
    let request_generation = event["requestGeneration"]
        .as_u64()
        .expect("response-bearing extension UI request should carry a generation");
    let mut response = json!({
        "id": format!("ui-response-{index}"),
        "type": "extension_ui_response",
        "requestId": request_id,
        "requestGeneration": request_generation,
    });

    let object = response
        .as_object_mut()
        .expect("extension UI response should be an object");
    match event.get("method").and_then(Value::as_str) {
        Some("confirm") => {
            object.insert("confirmed".to_string(), json!(true));
        }
        Some("select") => {
            let selected = first_string(event.get("options"))
                .or_else(|| first_string(event.get("items")))
                .unwrap_or("Option A");
            object.insert("value".to_string(), json!(selected));
        }
        Some("input") => {
            object.insert("value".to_string(), json!("test input"));
        }
        Some("editor") => {
            let edited = event
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("edited content");
            object.insert("value".to_string(), json!(edited));
        }
        Some("custom") => {
            object.insert("value".to_string(), json!({ "accepted": true }));
        }
        _ => {
            object.insert("value".to_string(), Value::Null);
        }
    }

    response
}

fn send_json_line(stdin: &mut ChildStdin, value: &Value) -> TestResult<()> {
    writeln!(stdin, "{value}")?;
    stdin.flush()?;
    Ok(())
}

fn expected_ui_request_count(scenario: &Value) -> usize {
    scenario["expected_patterns"]
        .as_array()
        .expect("expected patterns")
        .iter()
        .filter(|pattern| {
            pattern.get("type").and_then(Value::as_str) == Some("extension_ui_request")
        })
        .count()
}

struct RpcChild {
    child: Child,
    stdin: ChildStdin,
    line_rx: Receiver<io::Result<String>>,
}

struct ScenarioProgress {
    expected_ui_requests: usize,
    observed_ui_requests: usize,
    sent_ui_responses: usize,
    acknowledged_ui_responses: usize,
    response_index: usize,
    saw_prompt_response: bool,
}

impl ScenarioProgress {
    const fn new(expected_ui_requests: usize) -> Self {
        Self {
            expected_ui_requests,
            observed_ui_requests: 0,
            sent_ui_responses: 0,
            acknowledged_ui_responses: 0,
            response_index: 0,
            saw_prompt_response: false,
        }
    }

    fn observe_response(&mut self, response: &Value, stdin: &mut ChildStdin) -> TestResult<()> {
        if response.get("type").and_then(Value::as_str) == Some("extension_ui_request") {
            self.observed_ui_requests = self.observed_ui_requests.saturating_add(1);
            if ui_request_expects_response(response) {
                self.response_index = self.response_index.saturating_add(1);
                let ui_response = response_for_ui_event(response, self.response_index);
                send_json_line(stdin, &ui_response)?;
                self.sent_ui_responses = self.sent_ui_responses.saturating_add(1);
            }
        }

        if response.get("type").and_then(Value::as_str) == Some("response")
            && response.get("command").and_then(Value::as_str) == Some("prompt")
        {
            self.saw_prompt_response = true;
        }

        if response.get("type").and_then(Value::as_str) == Some("response")
            && response.get("command").and_then(Value::as_str) == Some("extension_ui_response")
            && response.get("success").and_then(Value::as_bool) == Some(true)
        {
            self.acknowledged_ui_responses = self.acknowledged_ui_responses.saturating_add(1);
        }

        Ok(())
    }

    const fn is_complete(&self) -> bool {
        self.saw_prompt_response
            && self.observed_ui_requests >= self.expected_ui_requests
            && self.acknowledged_ui_responses >= self.sent_ui_responses
    }

    fn validate_complete(&self, scenario_id: &str, responses: &[Value]) -> TestResult<()> {
        if !self.saw_prompt_response {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("extension UI scenario {scenario_id} timed out before prompt response"),
            )
            .into());
        }
        if self.observed_ui_requests < self.expected_ui_requests {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "extension UI scenario {scenario_id} observed {}/{} expected UI requests; responses={}",
                    self.observed_ui_requests,
                    self.expected_ui_requests,
                    json!(responses)
                ),
            )
            .into());
        }
        if self.acknowledged_ui_responses < self.sent_ui_responses {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "extension UI scenario {scenario_id} acknowledged {}/{} UI responses; responses={}",
                    self.acknowledged_ui_responses,
                    self.sent_ui_responses,
                    json!(responses)
                ),
            )
            .into());
        }
        Ok(())
    }
}

fn spawn_output_reader(stdout: ChildStdout) -> Receiver<io::Result<String>> {
    let (line_tx, line_rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    line_rx
}

fn spawn_rpc_child(root: &Path, extension_path: &Path) -> TestResult<RpcChild> {
    let mut child = Command::new(PI_TEST_RUNNER)
        .args(["--mode", "rpc", "--print", "-e"])
        .arg(extension_path)
        .args([
            "--extension-policy",
            "permissive",
            "--provider",
            RPC_TEST_PROVIDER,
            "--model",
            RPC_TEST_MODEL,
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
        ])
        .env("PI_CODING_AGENT_DIR", root.join("agent").as_os_str())
        .env("PI_CONFIG_PATH", root.join("settings.json").as_os_str())
        .env("PI_SESSIONS_DIR", root.join("sessions").as_os_str())
        .env("PI_PACKAGE_DIR", root.join("packages").as_os_str())
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let stdin = child
        .stdin
        .take()
        .expect("extension UI RPC child stdin pipe");
    let stdout = child
        .stdout
        .take()
        .expect("extension UI RPC child stdout pipe");

    Ok(RpcChild {
        child,
        stdin,
        line_rx: spawn_output_reader(stdout),
    })
}

fn collect_ui_scenario_responses(
    stdin: &mut ChildStdin,
    line_rx: &Receiver<io::Result<String>>,
    scenario: &Value,
) -> TestResult<Vec<Value>> {
    let scenario_id = scenario["id"].as_str().expect("scenario id");
    let mut responses = Vec::new();
    let started_at = Instant::now();
    let mut progress = ScenarioProgress::new(expected_ui_request_count(scenario));

    while started_at.elapsed() < UI_SCENARIO_TIMEOUT {
        let remaining = UI_SCENARIO_TIMEOUT.saturating_sub(started_at.elapsed());
        let line = match line_rx.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => return Err(error.into()),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        if let Ok(response) = serde_json::from_str::<Value>(&line) {
            progress.observe_response(&response, stdin)?;
            responses.push(response);
            if progress.is_complete() {
                break;
            }
        }
    }

    progress.validate_complete(scenario_id, &responses)?;
    Ok(responses)
}

/// Extension UI differential test harness for testing request/response round-trip parity
struct ExtensionUiDifferentialTester {
    temp_dir: TempDir,
}

impl ExtensionUiDifferentialTester {
    fn new() -> TestResult<Self> {
        let temp_dir = tempfile::tempdir()?;

        Ok(Self { temp_dir })
    }

    fn execute_ui_scenario(&self, scenario: &Value) -> TestResult<Value> {
        let command_name = scenario_command_name(scenario);
        let extension_path =
            write_scenario_extension(self.temp_dir.path(), scenario, &command_name)?;
        let RpcChild {
            mut child,
            mut stdin,
            line_rx,
        } = spawn_rpc_child(self.temp_dir.path(), &extension_path)?;

        let scenario_id = scenario["id"].as_str().expect("scenario id");
        let prompt = json!({
            "id": scenario_id,
            "type": "prompt",
            "message": format!("/{command_name}"),
        });
        let result = send_json_line(&mut stdin, &prompt)
            .and_then(|()| collect_ui_scenario_responses(&mut stdin, &line_rx, scenario));

        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();

        result.map(|responses| json!(responses))
    }

    fn validate_ui_scenario(scenario: &Value, actual_responses: &Value) -> bool {
        let expected_patterns = scenario["expected_patterns"]
            .as_array()
            .expect("expected patterns");
        let responses = actual_responses.as_array().expect("actual responses array");

        // For each expected pattern, check if we find a matching response
        for pattern in expected_patterns {
            let pattern_type = pattern["type"].as_str().expect("pattern type");
            let found = responses
                .iter()
                .any(|response| Self::matches_pattern(response, pattern, pattern_type));

            if !found {
                return false;
            }
        }

        true
    }

    fn matches_pattern(response: &Value, pattern: &Value, pattern_type: &str) -> bool {
        match pattern_type {
            "extension_ui_request" => {
                response.get("type") == Some(&json!("extension_ui_request"))
                    && pattern
                        .get("method")
                        .is_none_or(|m| response.get("method") == Some(m))
                    && pattern
                        .get("has_timeout")
                        .is_none_or(|_| response.get("timeout").is_some())
            }
            "response_success" => {
                response.get("type") == Some(&json!("response"))
                    && response.get("success") == Some(&json!(true))
            }
            "response_error" => {
                response.get("type") == Some(&json!("response"))
                    && response.get("success") == Some(&json!(false))
            }
            _ => false,
        }
    }
}

/// Canonicalizes extension UI responses by removing volatile fields
fn canonicalize_ui_response(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_ui_response).collect()),
        Value::Object(object) => {
            let mut canonicalized = BTreeMap::new();
            for (key, value) in object {
                // Skip volatile fields specific to extension UI
                if matches!(
                    key.as_str(),
                    "timestamp" | "requestId" | "requestGeneration" | "id" | "timeout"
                ) {
                    continue;
                }
                canonicalized.insert(key.clone(), canonicalize_ui_response(value));
            }
            Value::Object(canonicalized.into_iter().collect())
        }
        primitive => primitive.clone(),
    }
}

#[test]
fn g05_extension_ui_differential_fixture_validation() {
    // Validate fixture structure
    let scenarios: Value = serde_json::from_str(UI_SCENARIOS).expect("UI scenarios JSON");

    assert_eq!(
        scenarios["schema"],
        "pi.dropin.extension_ui_differential_scenarios.v1"
    );
    assert_eq!(scenarios["bead"], "bd-lnmtp.2.4");
    assert!(
        scenarios["canonicalization"]["volatile_fields"]
            .as_array()
            .is_some_and(|fields| {
                fields
                    .iter()
                    .any(|field| field.as_str() == Some("requestGeneration"))
            }),
        "requestGeneration must be canonicalized across independently assigned RPC epochs"
    );

    let ui_scenarios = scenarios["scenarios"]
        .as_array()
        .expect("UI scenarios array");
    assert!(
        ui_scenarios.len() >= 10,
        "bd-lnmtp.2.4 requires at least 10 UI scenarios, got {}",
        ui_scenarios.len()
    );

    // Validate each scenario has required fields
    for scenario in ui_scenarios {
        let id = scenario["id"].as_str().expect("scenario id");
        assert!(
            scenario.get("description").is_some(),
            "{id} missing description"
        );
        assert!(scenario.get("requests").is_some(), "{id} missing requests");
        assert!(
            scenario.get("expected_patterns").is_some(),
            "{id} missing expected_patterns"
        );
        for request in scenario["requests"].as_array().expect("scenario requests") {
            if request["type"] == "extension_ui_response"
                && (request.get("requestId").is_some() || request.get("id").is_some())
            {
                assert!(
                    request
                        .get("requestGeneration")
                        .and_then(Value::as_u64)
                        .is_some(),
                    "{id} response must carry an unsigned requestGeneration"
                );
            }
        }
    }
}

#[test]
fn g05_extension_ui_canonicalization_stable() {
    let test_cases = [
        json!({
            "type": "extension_ui_request",
            "id": "req-123",
            "requestGeneration": 7,
            "method": "confirm",
            "title": "Continue?",
            "timestamp": "2026-04-23T00:00:00Z"
        }),
        json!({
            "type": "response",
            "command": "extension_ui_response",
            "success": true,
            "requestId": "req-456",
            "requestGeneration": 8,
            "timestamp": "2026-04-23T00:00:01Z"
        }),
    ];

    for (i, test_case) in test_cases.iter().enumerate() {
        let canonical_once = canonicalize_ui_response(test_case);
        let canonical_twice = canonicalize_ui_response(&canonical_once);

        assert_eq!(
            canonical_once, canonical_twice,
            "Canonicalization not stable for test case {i}"
        );

        // Verify volatile fields are removed
        if let Value::Object(obj) = &canonical_once {
            assert!(
                !obj.contains_key("timestamp"),
                "timestamp should be removed"
            );
            assert!(
                !obj.contains_key("requestId"),
                "requestId should be removed"
            );
            assert!(
                !obj.contains_key("requestGeneration"),
                "requestGeneration should be removed"
            );
            assert!(!obj.contains_key("id"), "id should be removed");
        }
    }
}

#[test]
fn g05_extension_ui_differential_basic_scenarios() {
    let scenarios: Value = serde_json::from_str(UI_SCENARIOS).expect("UI scenarios JSON");
    let ui_scenarios = scenarios["scenarios"]
        .as_array()
        .expect("UI scenarios array");

    let tester =
        ExtensionUiDifferentialTester::new().expect("create extension UI differential tester");

    let mut successful_scenarios = 0;
    let mut failed_scenarios = Vec::new();
    let total_scenarios = ui_scenarios.len().min(5); // Test first 5 scenarios

    for scenario in ui_scenarios.iter().take(5) {
        let scenario_id = scenario["id"].as_str().unwrap_or("unknown");
        let scenario_type = scenario["type"].as_str().unwrap_or("unknown");

        match tester.execute_ui_scenario(scenario) {
            Ok(responses) => {
                if ExtensionUiDifferentialTester::validate_ui_scenario(scenario, &responses) {
                    successful_scenarios += 1;
                    println!("✓ {scenario_id}: {scenario_type} - PASS");
                } else {
                    failed_scenarios.push(format!(
                        "{scenario_id}: {scenario_type} - Pattern mismatch; responses={responses}"
                    ));
                    println!("✗ {scenario_id}: {scenario_type} - FAIL");
                }
            }
            Err(e) => {
                failed_scenarios.push(format!(
                    "{scenario_id}: {scenario_type} - Execution error: {e}"
                ));
                println!("✗ {scenario_id}: {scenario_type} - ERROR: {e}");
            }
        }
    }

    assert!(
        total_scenarios > 0,
        "Should have tested at least one UI scenario"
    );

    let successful_scenarios_u32 =
        u32::try_from(successful_scenarios).expect("scenario success count fits in u32");
    let total_scenarios_u32 = u32::try_from(total_scenarios).expect("scenario count fits in u32");
    let success_rate =
        (f64::from(successful_scenarios_u32) / f64::from(total_scenarios_u32)) * 100.0;

    println!(
        "\n=== G05 Extension UI Basic Differential Test Summary ===\n\
         Tested scenarios: {}\n\
         Successful: {}\n\
         Failed: {}\n\
         Success rate: {:.1}%\n",
        total_scenarios,
        successful_scenarios,
        failed_scenarios.len(),
        success_rate
    );

    if !failed_scenarios.is_empty() {
        println!("Failed scenarios:");
        for failure in &failed_scenarios {
            println!("  - {failure}");
        }
    }

    assert!(
        failed_scenarios.is_empty(),
        "extension UI differential scenarios failed: {}",
        failed_scenarios.join("; ")
    );
}
