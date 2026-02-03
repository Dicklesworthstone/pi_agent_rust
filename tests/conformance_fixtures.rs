//! Conformance tests using JSON fixtures.
//!
//! This test module runs all fixture-based conformance tests to ensure
//! the Rust implementation matches the TypeScript reference.

#[path = "conformance/mod.rs"]
mod conformance;

#[path = "conformance/fixture_runner.rs"]
mod fixture_runner;

use conformance::load_fixture;

/// Helper macro to generate fixture tests for each tool.
macro_rules! fixture_test {
    ($name:ident, $fixture:literal) => {
        #[tokio::test]
        async fn $name() {
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
fixture_test!(test_cli_flag_fixtures, "cli_flags");

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
        "truncation",
        "cli_flags",
    ];

    for fixture_name in &expected_fixtures {
        load_fixture(fixture_name)
            .unwrap_or_else(|e| panic!("Missing fixture '{fixture_name}': {e}"));
    }

    println!("✓ All {} expected fixtures exist", expected_fixtures.len());
}
