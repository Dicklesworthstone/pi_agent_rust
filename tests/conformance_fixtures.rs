//! Conformance tests using JSON fixtures.
//!
//! This test module runs all fixture-based conformance tests to ensure
//! the Rust implementation matches the TypeScript reference.

#![recursion_limit = "256"]

#[path = "conformance/mod.rs"]
mod conformance;

#[path = "conformance/fixture_runner.rs"]
mod fixture_runner;

#[path = "common/logging.rs"]
mod test_logging;

use conformance::load_fixture;
use std::io::Write as _;

static JSONL_APPEND_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn log_fixture_case(
    logger: &test_logging::TestLogger,
    fixture_name: &str,
    tool_name: &str,
    case: &conformance::TestCase,
    result: &conformance::TestResult,
) -> Result<(), String> {
    let mut redacted_input = case.input.clone();
    test_logging::redact_json_value(&mut redacted_input);
    let input = serde_json::to_string(&redacted_input)
        .map_err(|error| format!("serialize redacted fixture input: {error}"))?;
    let taxonomy = result
        .actual_error
        .as_deref()
        .or(result.message.as_deref())
        .unwrap_or("none");
    let artifact_path = result
        .actual_details
        .as_ref()
        .and_then(|details| details.get("saved_path").or_else(|| details.get("path")))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("none");
    let level = if result.passed {
        test_logging::LogLevel::Info
    } else {
        test_logging::LogLevel::Error
    };
    logger.with_context(
        level,
        "conformance.fixture_case",
        format!("Fixture case {}", result.name),
        |ctx| {
            ctx.push(("fixture".to_string(), fixture_name.to_string()));
            ctx.push(("tool".to_string(), tool_name.to_string()));
            ctx.push(("case".to_string(), case.name.clone()));
            ctx.push((
                "requirement_ids".to_string(),
                case.requirement_ids.join(","),
            ));
            ctx.push(("input".to_string(), input));
            ctx.push((
                "status".to_string(),
                if result.passed { "pass" } else { "fail" }.to_string(),
            ));
            ctx.push(("error_taxonomy".to_string(), taxonomy.to_string()));
            ctx.push(("artifact_path".to_string(), artifact_path.to_string()));
        },
    );
    Ok(())
}

fn append_fixture_logs(
    fixture_name: &str,
    fixture: &conformance::FixtureFile,
    results: &[conformance::TestResult],
) -> Result<(), String> {
    let logger = test_logging::TestLogger::new();
    logger.set_test_name(format!("conformance-fixture-{fixture_name}"));
    logger.info_ctx("harness", "Fixture suite started", |ctx| {
        ctx.push(("fixture".to_string(), fixture_name.to_string()));
        ctx.push(("tool".to_string(), fixture.tool.clone()));
        ctx.push(("case_count".to_string(), results.len().to_string()));
        ctx.push(("bead_id".to_string(), "bd-cv653.8.1".to_string()));
    });

    for (case, result) in fixture.cases.iter().zip(results) {
        log_fixture_case(&logger, fixture_name, &fixture.tool, case, result)?;
    }

    let jsonl = logger.dump_jsonl();
    let schema_errors = test_logging::validate_jsonl(&jsonl);
    if !schema_errors.is_empty() {
        return Err(format!(
            "structured fixture log schema validation failed: {schema_errors:?}"
        ));
    }

    let Ok(path) = std::env::var("TEST_LOG_JSONL_PATH") else {
        return Ok(());
    };
    let _guard = JSONL_APPEND_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = std::path::Path::new(&path);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create fixture log directory: {error}"))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("open fixture JSONL log {}: {error}", path.display()))?;
    file.write_all(jsonl.as_bytes())
        .map_err(|error| format!("append fixture JSONL log {}: {error}", path.display()))
}

/// Helper macro to generate fixture tests for each tool.
macro_rules! fixture_test {
    ($name:ident, $fixture:literal) => {
        #[test]
        fn $name() {
            asupersync::test_utils::run_test(|| async {
                let fixture = load_fixture($fixture)
                    .unwrap_or_else(|e| panic!("Failed to load fixture '{}': {}", $fixture, e));

                let results: Vec<conformance::TestResult> =
                    fixture_runner::run_fixture_tests(&fixture).await;

                let mut failures = Vec::new();
                for result in &results {
                    if !result.passed {
                        failures.push(format!(
                            "  {} FAILED: {}",
                            result.name,
                            result.message.as_deref().unwrap_or("unknown error")
                        ));
                    }
                }

                append_fixture_logs($fixture, &fixture, &results)
                    .unwrap_or_else(|error| panic!("Fixture logging failed: {error}"));

                if !failures.is_empty() {
                    panic!(
                        "Fixture tests for '{}' had failures:\n{}",
                        $fixture,
                        failures.join("\n")
                    );
                }

                println!(
                    "✓ {} fixture tests passed for '{}'",
                    results.len(),
                    $fixture
                );
            });
        }
    };
}

// Tool fixture tests
fixture_test!(test_read_fixtures, "read_tool");
fixture_test!(test_edit_fixtures, "edit_tool");
fixture_test!(test_bash_fixtures, "bash_tool");
fixture_test!(test_grep_fixtures, "grep_tool");
fixture_test!(test_write_fixtures, "write_tool");
fixture_test!(test_find_fixtures, "find_tool");
fixture_test!(test_ls_fixtures, "ls_tool");
fixture_test!(test_hashline_edit_fixtures, "hashline_edit_tool");
fixture_test!(test_ast_grep_fixtures, "ast_grep_tool");
fixture_test!(test_ast_edit_fixtures, "ast_edit_tool");
fixture_test!(test_xdev_fixtures, "xdev_tool");
fixture_test!(test_web_search_fixtures, "web_search_tool");
fixture_test!(test_cli_flag_fixtures, "cli_flags");
fixture_test!(test_media_fixtures, "media_tools");
fixture_test!(test_generate_image_fixtures, "generate_image_tool");
fixture_test!(test_tts_fixtures, "tts_tool");
fixture_test!(test_computer_fixtures, "computer_tool");
fixture_test!(test_browser_fixtures, "browser_tool");
fixture_test!(test_ask_fixtures, "ask_tool");
fixture_test!(test_todo_fixtures, "todo_tool");
fixture_test!(test_security_scan_fixtures, "security_scan_tool");

/// Run truncation tests from fixtures.
#[test]
fn test_truncation_fixtures() {
    let fixture = load_fixture("truncation")
        .unwrap_or_else(|e| panic!("Failed to load truncation fixture: {e}"));

    let results: Vec<conformance::TestResult> = fixture_runner::run_truncation_tests(&fixture);

    let mut failures = Vec::new();
    for result in &results {
        if !result.passed {
            failures.push(format!(
                "  {} FAILED: {}\n    Actual content: {:?}\n    Actual details: {:?}",
                result.name,
                result.message.as_deref().unwrap_or("unknown error"),
                result.actual_content,
                result.actual_details
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "Truncation fixture tests had failures:\n{}",
        failures.join("\n")
    );

    println!("✓ {} truncation tests passed", results.len());
}

/// Integration test that verifies all expected fixture files exist.
#[test]
fn test_all_fixtures_exist() {
    let expected_fixtures = [
        "read_tool",
        "edit_tool",
        "bash_tool",
        "grep_tool",
        "write_tool",
        "find_tool",
        "ls_tool",
        "hashline_edit_tool",
        "truncation",
        "cli_flags",
        "web_search_tool",
        "media_tools",
        "generate_image_tool",
        "tts_tool",
        "computer_tool",
        "browser_tool",
        "ask_tool",
        "todo_tool",
        "security_scan_tool",
    ];

    for fixture_name in &expected_fixtures {
        load_fixture(fixture_name)
            .unwrap_or_else(|e| panic!("Missing fixture '{fixture_name}': {e}"));
    }

    println!("✓ All {} expected fixtures exist", expected_fixtures.len());
}
