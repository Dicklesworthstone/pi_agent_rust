//! Golden corpus test runner (bd-3vraw / DROPIN-152).
//!
//! Loads golden transcript fixtures from `tests/golden_corpus/` and verifies
//! that the `pi` binary produces outputs matching the golden expectations.
//! Each fixture encodes its own VCR cassette, CLI arguments, stdin, and
//! expected outcomes, making the corpus self-contained and reproducible.
//!
//! Run:
//! ```bash
//! cargo test --test e2e_golden_corpus
//! ```

#![allow(clippy::too_many_lines)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::similar_names)]
#![allow(clippy::module_name_repetitions)]

mod common;

use common::TestHarness;
use serde::Deserialize;
use serde_json::Value;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ═══════════════════════════════════════════════════════════════════════
// Schema types
// ═══════════════════════════════════════════════════════════════════════

const GOLDEN_SCHEMA: &str = "pi.golden_corpus.v1";
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// A single golden corpus fixture loaded from JSON.
#[derive(Debug, Deserialize)]
struct GoldenFixture {
    schema: String,
    scenario: String,
    #[allow(dead_code)]
    surface: String,
    description: String,
    cli_args: Vec<String>,
    stdin: Option<String>,
    #[serde(default)]
    env_overrides: BTreeMap<String, String>,
    cassette: Option<Value>,
    #[serde(default)]
    cassette_dynamic: bool,
    cassette_template: Option<Value>,
    #[serde(default)]
    temp_files: BTreeMap<String, String>,
    expected: GoldenExpected,
}

/// Expected outcomes for a golden test.
#[derive(Debug, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
struct GoldenExpected {
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    exit_code_nonzero: bool,
    #[serde(default)]
    stdout_contains: Vec<String>,
    #[serde(default)]
    stdout_not_contains: Vec<String>,
    #[serde(default)]
    stderr_not_contains: Vec<String>,
    #[serde(default)]
    no_session_files: bool,
    #[serde(default)]
    json_event_sequence: Vec<String>,
    #[serde(default)]
    json_session_header: bool,
    #[serde(default)]
    json_agent_start_has_session_id: bool,
    #[serde(default)]
    json_agent_end_has_messages: bool,
    #[serde(default)]
    json_line_count: Option<usize>,
}

// ═══════════════════════════════════════════════════════════════════════
// Test harness
// ═══════════════════════════════════════════════════════════════════════

struct GoldenTestHarness {
    harness: TestHarness,
    binary_path: PathBuf,
    base_env: BTreeMap<String, String>,
    run_seq: Cell<usize>,
}

impl GoldenTestHarness {
    fn new(name: &str) -> Self {
        let harness = TestHarness::new(name);
        let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_pi"));

        let mut base_env = BTreeMap::new();
        let env_root = harness.temp_path("pi-env");
        let _ = fs::create_dir_all(&env_root);

        // Fully isolate global/project state for determinism.
        base_env.insert("HOME".to_string(), env_root.display().to_string());
        base_env.insert("USERPROFILE".to_string(), env_root.display().to_string());
        base_env.insert(
            "PI_CODING_AGENT_DIR".to_string(),
            env_root.join("agent").display().to_string(),
        );
        base_env.insert(
            "PI_CONFIG_PATH".to_string(),
            env_root.join("settings.json").display().to_string(),
        );
        base_env.insert(
            "PI_SESSIONS_DIR".to_string(),
            env_root.join("sessions").display().to_string(),
        );
        base_env.insert(
            "PI_PACKAGE_DIR".to_string(),
            env_root.join("packages").display().to_string(),
        );
        base_env.insert("npm_config_audit".to_string(), "false".to_string());
        base_env.insert("npm_config_fund".to_string(), "false".to_string());
        base_env.insert(
            "npm_config_update_notifier".to_string(),
            "false".to_string(),
        );

        Self {
            harness,
            binary_path,
            base_env,
            run_seq: Cell::new(0),
        }
    }

    fn run_fixture(&self, fixture: &GoldenFixture) -> CliOutput {
        let seq = self.run_seq.get();
        self.run_seq.set(seq + 1);

        self.harness
            .log()
            .info("golden", format!("Running fixture: {}", fixture.scenario));

        // Create temp files if specified.
        let mut arg_replacements: BTreeMap<String, String> = BTreeMap::new();
        for (placeholder, content) in &fixture.temp_files {
            let file_name = format!("tempfile_{seq}_{placeholder}.tmp");
            let path = self.harness.create_file(&file_name, content.as_bytes());
            arg_replacements.insert(placeholder.clone(), path.display().to_string());
        }

        let mut run_env = self.base_env.clone();

        // Apply env overrides.
        for (key, value) in &fixture.env_overrides {
            run_env.insert(key.clone(), value.clone());
        }

        // Set up VCR cassette if provided.
        if let Some(cassette) = &fixture.cassette {
            let cassette = substitute_json_placeholders(cassette, &arg_replacements);
            self.setup_vcr(&fixture.scenario, &cassette, &mut run_env);
        } else if fixture.cassette_dynamic {
            let Some(template) = &fixture.cassette_template else {
                return CliOutput {
                    exit_code: i32::MIN,
                    stdout: String::new(),
                    stderr: format!(
                        "[{}] cassette_dynamic=true requires cassette_template",
                        fixture.scenario
                    ),
                    duration: Duration::ZERO,
                };
            };
            let cassette = substitute_json_placeholders(template, &arg_replacements);
            self.setup_vcr(&fixture.scenario, &cassette, &mut run_env);
        }

        // Build CLI args with placeholder substitution.
        let args: Vec<String> = fixture
            .cli_args
            .iter()
            .map(|arg| {
                let mut result = arg.clone();
                for (placeholder, value) in &arg_replacements {
                    result = result.replace(placeholder, value);
                }
                result
            })
            .collect();
        let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

        self.run_binary(&args_refs, fixture.stdin.as_deref(), &run_env)
    }

    fn setup_vcr(&self, scenario: &str, cassette: &Value, env: &mut BTreeMap<String, String>) {
        let cassette_dir = self.harness.temp_path("vcr-cassettes");
        fs::create_dir_all(&cassette_dir).expect("create cassette dir");

        let cassette_name = cassette
            .get("test_name")
            .and_then(Value::as_str)
            .unwrap_or(scenario);
        let cassette_path = cassette_dir.join(format!("{cassette_name}.json"));
        fs::write(
            &cassette_path,
            serde_json::to_string_pretty(cassette).expect("serialize cassette"),
        )
        .expect("write cassette");

        env.insert("VCR_MODE".to_string(), "playback".to_string());
        env.insert(
            "VCR_CASSETTE_DIR".to_string(),
            cassette_dir.display().to_string(),
        );
        env.insert("PI_VCR_TEST_NAME".to_string(), cassette_name.to_string());
        env.insert("ANTHROPIC_API_KEY".to_string(), "test-vcr-key".to_string());
        env.insert("PI_TEST_MODE".to_string(), "1".to_string());
        env.insert("VCR_DEBUG_BODY".to_string(), "1".to_string());
    }

    fn run_binary(
        &self,
        args: &[&str],
        stdin: Option<&str>,
        env: &BTreeMap<String, String>,
    ) -> CliOutput {
        self.harness
            .log()
            .info("action", format!("CLI: pi {}", args.join(" ")));

        let start = Instant::now();
        let mut command = Command::new(&self.binary_path); // ubs:ignore Cargo-provided test binary path, not user input.
        command.env_remove("ANTHROPIC_API_KEY");
        command.env_remove("OPENAI_API_KEY");
        command.env_remove("GEMINI_API_KEY");
        command.env_remove("GROQ_API_KEY");
        command.env_remove("KIMI_API_KEY");
        command.env_remove("AZURE_OPENAI_API_KEY");
        command.env_remove("PI_OPENROUTER_API_KEY");
        command.env_remove("PI_AWS_ACCESS_KEY_ID");
        command
            .args(args)
            .envs(env)
            .current_dir(self.harness.temp_dir())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if stdin.is_some() {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }

        let mut child = command.spawn().expect("spawn pi binary");
        let mut child_stdout = child.stdout.take().expect("child stdout");
        let mut child_stderr = child.stderr.take().expect("child stderr");

        let stdout_handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = child_stdout.read_to_end(&mut buf);
            buf
        });
        let stderr_handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = child_stderr.read_to_end(&mut buf);
            buf
        });

        if let Some(input) = stdin
            && let Some(mut child_stdin) = child.stdin.take()
        {
            child_stdin
                .write_all(input.as_bytes())
                .expect("write stdin");
        }

        let timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(err) => panic!("try_wait failed: {err}"), // ubs:ignore test harness assertion, not production runtime.
            }
            if start.elapsed() > timeout {
                let _ = child.kill();
                panic!("golden test timed out after {}s", timeout.as_secs()); // ubs:ignore test harness assertion, not production runtime.
            }
            std::thread::sleep(Duration::from_millis(50));
        };

        let duration = start.elapsed();
        let stdout_bytes = stdout_handle.join().expect("stdout thread");
        let stderr_bytes = stderr_handle.join().expect("stderr thread");
        let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
        let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
        let exit_code = status.code().unwrap_or(-1);

        self.harness
            .log()
            .info_ctx("result", "CLI output captured", |ctx| {
                ctx.push(("exit_code".into(), exit_code.to_string()));
                ctx.push(("stdout_len".into(), stdout.len().to_string()));
                ctx.push(("stderr_len".into(), stderr.len().to_string()));
                ctx.push(("duration_ms".into(), duration.as_millis().to_string()));
            });

        CliOutput {
            exit_code,
            stdout,
            stderr,
            duration,
        }
    }
}

struct CliOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
    #[allow(dead_code)]
    duration: Duration,
}

// ═══════════════════════════════════════════════════════════════════════
// Assertion helpers
// ═══════════════════════════════════════════════════════════════════════

fn test_fail(message: impl std::fmt::Display) -> ! {
    panic!("{message}"); // ubs:ignore test harness assertion, not production runtime.
}

fn assert_golden(harness: &TestHarness, fixture: &GoldenFixture, output: &CliOutput) {
    let scenario = &fixture.scenario;

    // Exit code.
    assert!(
        !(fixture.expected.exit_code.is_some() && fixture.expected.exit_code_nonzero),
        "[{scenario}] fixture sets both exit_code and exit_code_nonzero"
    );
    if let Some(expected_code) = fixture.expected.exit_code {
        assert_eq!(
            output.exit_code, expected_code,
            "[{scenario}] expected exit code {expected_code}, got {}.\nstderr:\n{}\nstdout:\n{}",
            output.exit_code, output.stderr, output.stdout,
        );
    }
    if fixture.expected.exit_code_nonzero {
        assert_ne!(
            output.exit_code, 0,
            "[{scenario}] expected non-zero exit code, got 0.\nstdout:\n{}\nstderr:\n{}",
            output.stdout, output.stderr,
        );
    }
    if fixture.expected.exit_code.is_none() && !fixture.expected.exit_code_nonzero {
        assert_eq!(
            output.exit_code, 0,
            "[{scenario}] expected exit code 0 by default, got {}.\nstdout:\n{}\nstderr:\n{}",
            output.exit_code, output.stdout, output.stderr,
        );
    }

    // stdout contains.
    for needle in &fixture.expected.stdout_contains {
        assert!(
            output.stdout.contains(needle.as_str()),
            "[{scenario}] stdout should contain '{needle}'.\nstdout:\n{}",
            output.stdout,
        );
    }

    // stdout not contains.
    for needle in &fixture.expected.stdout_not_contains {
        let lower = output.stdout.to_lowercase();
        assert!(
            !lower.contains(&needle.to_lowercase()),
            "[{scenario}] stdout should NOT contain '{needle}'.\nstdout:\n{}",
            output.stdout,
        );
    }

    // stderr not contains.
    for needle in &fixture.expected.stderr_not_contains {
        let lower = output.stderr.to_lowercase();
        assert!(
            !lower.contains(&needle.to_lowercase()),
            "[{scenario}] stderr should NOT contain '{needle}'.\nstderr:\n{}",
            output.stderr,
        );
    }

    // Session files.
    if fixture.expected.no_session_files {
        let sessions_dir = harness.temp_path("pi-env/sessions");
        let jsonl_count = count_jsonl_files(&sessions_dir);
        assert_eq!(
            jsonl_count, 0,
            "[{scenario}] expected no session files, found {jsonl_count}"
        );
    }

    // JSON mode validations.
    if fixture.expected.json_session_header
        || !fixture.expected.json_event_sequence.is_empty()
        || fixture.expected.json_line_count.is_some()
    {
        let lines = parse_json_lines(&output.stdout).unwrap_or_else(|err| {
            test_fail(format!(
                "[{scenario}] stdout JSON parse failed: {err}\nstdout:\n{}",
                output.stdout
            ));
        });

        if let Some(expected_count) = fixture.expected.json_line_count {
            assert_eq!(
                lines.len(),
                expected_count,
                "[{scenario}] expected {expected_count} JSON lines, got {}",
                lines.len(),
            );
        }

        if fixture.expected.json_session_header {
            assert!(
                !lines.is_empty(),
                "[{scenario}] expected JSON session header but got no JSON lines"
            );
            assert_eq!(
                lines[0]["type"], "session",
                "[{scenario}] first JSON line must be session header"
            );
            assert!(
                lines[0]["id"].as_str().is_some_and(|s| !s.is_empty()),
                "[{scenario}] session header must include non-empty id"
            );
        }

        if !fixture.expected.json_event_sequence.is_empty() {
            let actual_types: Vec<&str> = lines
                .iter()
                .filter_map(|v| v.get("type").and_then(Value::as_str))
                .collect();
            assert_json_event_order(
                scenario,
                &fixture.expected.json_event_sequence,
                &actual_types,
            );
        }

        if fixture.expected.json_agent_start_has_session_id {
            let agent_start = lines.iter().find(|v| v["type"] == "agent_start"); // ubs:ignore JSON event type comparison, not a secret.
            let Some(event) = agent_start else {
                panic!("[{scenario}] expected agent_start event but none was found"); // ubs:ignore test harness assertion, not production runtime.
            };
            assert!(
                event["sessionId"].as_str().is_some_and(|s| !s.is_empty()),
                "[{scenario}] agent_start must have non-empty sessionId"
            );
        }

        if fixture.expected.json_agent_end_has_messages {
            let agent_end = lines.iter().find(|v| v["type"] == "agent_end"); // ubs:ignore JSON event type comparison, not a secret.
            let Some(event) = agent_end else {
                panic!("[{scenario}] expected agent_end event but none was found"); // ubs:ignore test harness assertion, not production runtime.
            };
            assert!(
                event["messages"].is_array(),
                "[{scenario}] agent_end must have messages array"
            );
        }
    }

    harness
        .log()
        .info("golden", format!("[{scenario}] All assertions passed"));
}

fn assert_json_event_order(scenario: &str, expected: &[String], actual: &[&str]) {
    // Verify that all expected event types appear in order (allowing gaps).
    let mut actual_idx = 0;
    for expected_type in expected {
        let found = actual[actual_idx..].iter().position(|t| t == expected_type); // ubs:ignore JSON event type comparison, not a secret.
        match found {
            Some(offset) => {
                actual_idx += offset + 1;
            }
            None => {
                test_fail(format!(
                    "[{scenario}] expected event '{expected_type}' not found in order.\n\
                     Expected sequence: {expected:?}\n\
                     Actual types: {actual:?}"
                ));
            }
        }
    }
}

fn parse_json_lines(stdout: &str) -> Result<Vec<Value>, String> {
    let mut lines = Vec::new();
    for (idx, line) in stdout.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let sanitized = line.strip_suffix('\r').unwrap_or(line);
        match serde_json::from_str::<Value>(sanitized) {
            Ok(value) => lines.push(value),
            Err(err) => {
                return Err(format!("line {}: {err}", idx + 1));
            }
        }
    }
    Ok(lines)
}

fn substitute_json_placeholders(value: &Value, replacements: &BTreeMap<String, String>) -> Value {
    match value {
        Value::String(text) => Value::String(substitute_string_placeholders(text, replacements)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| substitute_json_placeholders(item, replacements))
                .collect(),
        ),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    (
                        substitute_string_placeholders(key, replacements),
                        substitute_json_placeholders(value, replacements),
                    )
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn substitute_string_placeholders(text: &str, replacements: &BTreeMap<String, String>) -> String {
    let mut result = text.to_string();
    for (placeholder, value) in replacements {
        result = result.replace(placeholder, value);
    }
    result
}

fn count_jsonl_files(dir: &Path) -> usize {
    if !dir.exists() {
        return 0;
    }
    let mut count = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                stack.push(entry_path);
                continue;
            }
            if entry_path.extension().is_some_and(|ext| ext == "jsonl") {
                count += 1;
            }
        }
    }
    count
}

// ═══════════════════════════════════════════════════════════════════════
// Fixture loading
// ═══════════════════════════════════════════════════════════════════════

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden_corpus")
}

fn load_fixture(path: &Path) -> GoldenFixture {
    let content = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display())); // ubs:ignore test harness assertion, not production runtime.
    let fixture: GoldenFixture = serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("failed to parse fixture {}: {e}", path.display())); // ubs:ignore test harness assertion, not production runtime.
    assert_eq!(
        fixture.schema,
        GOLDEN_SCHEMA,
        "fixture {} has unsupported schema '{}'",
        path.display(),
        fixture.schema,
    );
    fixture
}

fn discover_fixtures(surface: &str) -> Vec<PathBuf> {
    let dir = corpus_dir().join(surface);
    if !dir.exists() {
        return Vec::new();
    }
    let mut fixtures: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("read corpus surface dir")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    fixtures.sort();
    fixtures
}

#[allow(unused_assignments)]
fn run_surface_fixtures(surface: &str) {
    let fixtures = discover_fixtures(surface);
    assert!(
        !fixtures.is_empty(),
        "no golden fixtures found for surface '{surface}' in {}",
        corpus_dir().join(surface).display(),
    );

    let mut pass_count = 0;
    let mut fail_count = 0;
    let total = fixtures.len();

    for fixture_path in &fixtures {
        let fixture = load_fixture(fixture_path);
        assert_eq!(
            fixture.surface,
            surface,
            "fixture {} surface mismatch: expected '{surface}', got '{}'",
            fixture_path.display(),
            fixture.surface
        );
        let test_name = format!("golden_{surface}_{}", fixture.scenario);
        let harness = GoldenTestHarness::new(&test_name);

        harness.harness.log().info_ctx(
            "golden",
            format!("Loading fixture: {}", fixture.scenario),
            |ctx| {
                ctx.push(("surface".into(), surface.to_string()));
                ctx.push(("fixture_path".into(), fixture_path.display().to_string()));
                ctx.push(("description".into(), fixture.description.clone()));
            },
        );

        let output = harness.run_fixture(&fixture);
        // We use a catch_unwind-like approach: run assertions and collect results.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_golden(&harness.harness, &fixture, &output);
        }));

        match result {
            Ok(()) => {
                pass_count += 1;
                harness.harness.log().info(
                    "golden",
                    format!(
                        "PASS: {} ({}/{})",
                        fixture.scenario,
                        pass_count + fail_count,
                        total
                    ),
                );
            }
            Err(err) => {
                fail_count += 1;
                let msg = err
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| err.downcast_ref::<&str>().map(|s| (*s).to_string()))
                    .unwrap_or_else(|| "unknown panic".to_string());
                harness
                    .harness
                    .log()
                    .info("golden", format!("FAIL: {} — {msg}", fixture.scenario));
                // Re-panic to fail the test.
                std::panic::resume_unwind(err);
            }
        }
    }

    eprintln!("[golden/{surface}] {pass_count}/{total} passed, {fail_count} failed");
}

// ═══════════════════════════════════════════════════════════════════════
// Test entry points — one per execution surface
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn golden_corpus_print_text() {
    run_surface_fixtures("print_text");
}

#[test]
fn golden_corpus_print_stdin() {
    run_surface_fixtures("print_stdin");
}

#[test]
fn golden_corpus_json_mode() {
    run_surface_fixtures("json_mode");
}

#[test]
fn golden_corpus_json_mode_stdin() {
    run_surface_fixtures("json_mode_stdin");
}

#[test]
fn golden_corpus_error_cases() {
    run_surface_fixtures("error_cases");
}

#[test]
fn golden_corpus_rpc_mode() {
    run_surface_fixtures("rpc_mode");
}

#[test]
fn golden_corpus_at_file_expansion() {
    run_surface_fixtures("at_file_expansion");
}

// ═══════════════════════════════════════════════════════════════════════
// Manifest / inventory test
// ═══════════════════════════════════════════════════════════════════════

/// Verify the corpus is non-trivial and covers the required surfaces.
#[test]
fn golden_corpus_manifest_coverage() {
    let required_surfaces = [
        "print_text",
        "print_stdin",
        "json_mode",
        "json_mode_stdin",
        "rpc_mode",
        "error_cases",
        "at_file_expansion",
    ];

    let mut total_fixtures = 0;
    for surface in &required_surfaces {
        let fixtures = discover_fixtures(surface);
        assert!(
            !fixtures.is_empty(),
            "golden corpus missing required surface '{surface}'"
        );
        total_fixtures += fixtures.len();
    }

    // Require a minimum corpus size to prevent trivial compliance.
    assert!(
        total_fixtures >= 5,
        "golden corpus is too small ({total_fixtures} fixtures); expected >= 5"
    );

    eprintln!(
        "[golden/manifest] {total_fixtures} fixtures across {} surfaces",
        required_surfaces.len()
    );
}

#[test]
fn parse_json_lines_rejects_invalid_lines() {
    let stdout = r#"
{"type":"session","id":"abc"}
not-json
{"type":"agent_start"}
"#;
    let err = parse_json_lines(stdout).expect_err("invalid JSON line should fail");
    assert!(err.contains("line 3"));
}

#[test]
fn parse_json_lines_ignores_blank_lines() {
    let stdout = r#"

{"type":"session","id":"abc"}

{"type":"agent_end"}

"#;
    let lines = parse_json_lines(stdout).expect("valid JSON lines should parse");
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[1]["type"], "agent_end");
}

#[test]
fn parse_json_lines_accepts_crlf() {
    let stdout = "{\"type\":\"session\",\"id\":\"abc\"}\r\n{\"type\":\"agent_end\"}\r\n";
    let lines = parse_json_lines(stdout).expect("CRLF JSON lines should parse");
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[1]["type"], "agent_end");
}

#[test]
fn cassette_template_replaces_runtime_file_placeholders() {
    let mut replacements = BTreeMap::new();
    replacements.insert(
        "__TEMP_FILE_1__".to_string(),
        "/tmp/pi-agent/file.rs".to_string(),
    );
    let template = serde_json::json!({
        "request": {
            "body": {
                "messages": [
                    {
                        "content": [
                            {
                                "type": "text",
                                "text": "<file name=\"__TEMP_FILE_1__\">"
                            }
                        ]
                    }
                ]
            }
        }
    });

    let actual = substitute_json_placeholders(&template, &replacements);

    assert_eq!(
        actual["request"]["body"]["messages"][0]["content"][0]["text"],
        "<file name=\"/tmp/pi-agent/file.rs\">"
    );
}

#[test]
fn json_event_order_allows_gaps() {
    let expected = vec!["session".to_string(), "agent_end".to_string()];
    let actual = ["session", "agent_start", "tool_start", "agent_end"];
    assert_json_event_order("gap_test", &expected, &actual);
}

#[test]
fn json_session_header_requires_lines() {
    let harness = TestHarness::new("json_session_header_requires_lines");
    let fixture = GoldenFixture {
        schema: GOLDEN_SCHEMA.to_string(),
        scenario: "json_session_header_requires_lines".to_string(),
        surface: "json_mode".to_string(),
        description: String::new(),
        cli_args: Vec::new(),
        stdin: None,
        env_overrides: BTreeMap::new(),
        cassette: None,
        cassette_dynamic: false,
        cassette_template: None,
        temp_files: BTreeMap::new(),
        expected: GoldenExpected {
            exit_code: None,
            exit_code_nonzero: false,
            stdout_contains: Vec::new(),
            stdout_not_contains: Vec::new(),
            stderr_not_contains: Vec::new(),
            no_session_files: false,
            json_event_sequence: Vec::new(),
            json_session_header: true,
            json_agent_start_has_session_id: false,
            json_agent_end_has_messages: false,
            json_line_count: None,
        },
    };
    let output = CliOutput {
        exit_code: 0,
        stdout: String::new(),
        stderr: String::new(),
        duration: Duration::from_secs(0),
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_golden(&harness, &fixture, &output);
    }));
    assert!(result.is_err(), "expected missing session header to fail");
}

#[test]
fn json_agent_start_requires_event() {
    let harness = TestHarness::new("json_agent_start_requires_event");
    let fixture = GoldenFixture {
        schema: GOLDEN_SCHEMA.to_string(),
        scenario: "json_agent_start_requires_event".to_string(),
        surface: "json_mode".to_string(),
        description: String::new(),
        cli_args: Vec::new(),
        stdin: None,
        env_overrides: BTreeMap::new(),
        cassette: None,
        cassette_dynamic: false,
        cassette_template: None,
        temp_files: BTreeMap::new(),
        expected: GoldenExpected {
            exit_code: None,
            exit_code_nonzero: false,
            stdout_contains: Vec::new(),
            stdout_not_contains: Vec::new(),
            stderr_not_contains: Vec::new(),
            no_session_files: false,
            json_event_sequence: Vec::new(),
            json_session_header: true,
            json_agent_start_has_session_id: true,
            json_agent_end_has_messages: false,
            json_line_count: None,
        },
    };
    let output = CliOutput {
        exit_code: 0,
        stdout: r#"{"type":"session","id":"abc"}"#.to_string(),
        stderr: String::new(),
        duration: Duration::from_secs(0),
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_golden(&harness, &fixture, &output);
    }));
    assert!(result.is_err(), "expected missing agent_start to fail");
}

#[test]
fn json_agent_end_requires_event() {
    let harness = TestHarness::new("json_agent_end_requires_event");
    let fixture = GoldenFixture {
        schema: GOLDEN_SCHEMA.to_string(),
        scenario: "json_agent_end_requires_event".to_string(),
        surface: "json_mode".to_string(),
        description: String::new(),
        cli_args: Vec::new(),
        stdin: None,
        env_overrides: BTreeMap::new(),
        cassette: None,
        cassette_dynamic: false,
        cassette_template: None,
        temp_files: BTreeMap::new(),
        expected: GoldenExpected {
            exit_code: None,
            exit_code_nonzero: false,
            stdout_contains: Vec::new(),
            stdout_not_contains: Vec::new(),
            stderr_not_contains: Vec::new(),
            no_session_files: false,
            json_event_sequence: Vec::new(),
            json_session_header: true,
            json_agent_start_has_session_id: false,
            json_agent_end_has_messages: true,
            json_line_count: None,
        },
    };
    let output = CliOutput {
        exit_code: 0,
        stdout: r#"{"type":"session","id":"abc"}"#.to_string(),
        stderr: String::new(),
        duration: Duration::from_secs(0),
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_golden(&harness, &fixture, &output);
    }));
    assert!(result.is_err(), "expected missing agent_end to fail");
}
