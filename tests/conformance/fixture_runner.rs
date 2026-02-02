//! Fixture-based conformance test runner.
//!
//! This module provides the infrastructure to run tests defined in JSON fixture files.

use crate::conformance::{FixtureFile, SetupStep, TestCase, TestResult, validate_expected};
use pi::model::ContentBlock;
use pi::tools::Tool;
use std::path::Path;
use tempfile::TempDir;

/// Run all test cases from a fixture file.
pub async fn run_fixture_tests(fixture: &FixtureFile) -> Vec<TestResult> {
    let mut results = Vec::new();

    for case in &fixture.cases {
        let result = run_test_case(&fixture.tool, case).await;
        results.push(result);
    }

    results
}

/// Run a single test case.
async fn run_test_case(tool_name: &str, case: &TestCase) -> TestResult {
    // Create a temporary directory for the test
    let temp_dir = match TempDir::new() {
        Ok(dir) => dir,
        Err(e) => {
            return TestResult::fail(&case.name, format!("Failed to create temp dir: {e}"));
        }
    };

    // Run setup steps
    if let Err(e) = run_setup_steps(&case.setup, temp_dir.path()) {
        return TestResult::fail(&case.name, format!("Setup failed: {e}"));
    }

    // Create the tool
    let tool: Box<dyn Tool> = match tool_name {
        "read" => Box::new(pi::tools::ReadTool::new(temp_dir.path())),
        "bash" => Box::new(pi::tools::BashTool::new(temp_dir.path())),
        "edit" => Box::new(pi::tools::EditTool::new(temp_dir.path())),
        "write" => Box::new(pi::tools::WriteTool::new(temp_dir.path())),
        "grep" => Box::new(pi::tools::GrepTool::new(temp_dir.path())),
        "find" => Box::new(pi::tools::FindTool::new(temp_dir.path())),
        "ls" => Box::new(pi::tools::LsTool::new(temp_dir.path())),
        _ => {
            return TestResult::fail(&case.name, format!("Unknown tool: {tool_name}"));
        }
    };

    // Execute the tool
    let result = tool.execute("test-id", case.input.clone(), None).await;

    // Handle expected errors
    if case.expect_error {
        match result {
            Err(e) => {
                let error_msg = e.to_string();
                if let Some(expected_substr) = &case.error_contains {
                    if error_msg
                        .to_lowercase()
                        .contains(&expected_substr.to_lowercase())
                    {
                        return TestResult::pass(&case.name);
                    }
                    return TestResult::fail(
                        &case.name,
                        format!(
                            "Error message '{error_msg}' does not contain expected '{expected_substr}'"
                        ),
                    );
                }
                return TestResult::pass(&case.name);
            }
            Ok(_) => {
                return TestResult::fail(&case.name, "Expected error but tool succeeded");
            }
        }
    }

    // Check for unexpected errors
    let output = match result {
        Ok(o) => o,
        Err(e) => {
            return TestResult::fail(&case.name, format!("Unexpected error: {e}"));
        }
    };

    // Extract text content
    let content = extract_text_content(&output.content);

    // Validate expected results
    match validate_expected(&case.expected, &content, output.details.as_ref()) {
        Ok(()) => {
            let mut result = TestResult::pass(&case.name);
            result.actual_content = Some(content);
            result.actual_details = output.details;
            result
        }
        Err(msg) => {
            let mut result = TestResult::fail(&case.name, msg);
            result.actual_content = Some(content);
            result.actual_details = output.details;
            result
        }
    }
}

/// Run setup steps for a test case.
fn run_setup_steps(steps: &[SetupStep], dir: &Path) -> Result<(), String> {
    for step in steps {
        match step {
            SetupStep::CreateFile { path, content } => {
                let file_path = dir.join(path);
                if let Some(parent) = file_path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("Failed to create parent dirs: {e}"))?;
                }
                std::fs::write(&file_path, content)
                    .map_err(|e| format!("Failed to create file {path}: {e}"))?;
            }
            SetupStep::CreateDir { path } => {
                let dir_path = dir.join(path);
                std::fs::create_dir_all(&dir_path)
                    .map_err(|e| format!("Failed to create dir {path}: {e}"))?;
            }
            SetupStep::RunCommand { command } => {
                let output = std::process::Command::new("bash")
                    .arg("-c")
                    .arg(command)
                    .current_dir(dir)
                    .output()
                    .map_err(|e| format!("Failed to run command: {e}"))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(format!("Setup command failed: {stderr}"));
                }
            }
        }
    }
    Ok(())
}

/// Extract text content from tool output.
fn extract_text_content(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| {
            if let ContentBlock::Text(text) = block {
                Some(text.text.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run truncation conformance tests.
pub fn run_truncation_tests(fixture: &FixtureFile) -> Vec<TestResult> {
    let mut results = Vec::new();

    for case in &fixture.cases {
        let result = run_truncation_test_case(case);
        results.push(result);
    }

    results
}

/// Run a single truncation test case.
fn run_truncation_test_case(case: &TestCase) -> TestResult {
    use pi::tools::{truncate_head, truncate_tail};

    let content = case.input["content"].as_str().unwrap_or("");
    let max_lines = case.input["max_lines"].as_u64().unwrap_or(2000) as usize;
    let max_bytes = case.input["max_bytes"].as_u64().unwrap_or(50000) as usize;

    // Determine if this is a head or tail test based on the name
    let result = if case.name.contains("tail") {
        truncate_tail(content, max_lines, max_bytes)
    } else {
        truncate_head(content, max_lines, max_bytes)
    };

    // Build details JSON for validation
    let details = serde_json::json!({
        "truncated": result.truncated,
        "truncated_by": result.truncated_by.map(|t| match t {
            pi::tools::TruncatedBy::Lines => "lines",
            pi::tools::TruncatedBy::Bytes => "bytes",
        }),
        "total_lines": result.total_lines,
        "output_lines": result.output_lines,
        "total_bytes": result.total_bytes,
        "output_bytes": result.output_bytes,
        "first_line_exceeds_limit": result.first_line_exceeds_limit,
        "last_line_partial": result.last_line_partial,
    });

    match validate_expected(&case.expected, &result.content, Some(&details)) {
        Ok(()) => {
            let mut test_result = TestResult::pass(&case.name);
            test_result.actual_content = Some(result.content);
            test_result.actual_details = Some(details);
            test_result
        }
        Err(msg) => {
            let mut test_result = TestResult::fail(&case.name, msg);
            test_result.actual_content = Some(result.content);
            test_result.actual_details = Some(details);
            test_result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_setup_create_file() {
        let temp_dir = TempDir::new().unwrap();
        let steps = vec![SetupStep::CreateFile {
            path: "test.txt".to_string(),
            content: "hello".to_string(),
        }];

        run_setup_steps(&steps, temp_dir.path()).unwrap();

        let content = std::fs::read_to_string(temp_dir.path().join("test.txt")).unwrap();
        assert_eq!(content, "hello");
    }

    #[test]
    fn test_setup_create_nested_file() {
        let temp_dir = TempDir::new().unwrap();
        let steps = vec![SetupStep::CreateFile {
            path: "nested/dir/test.txt".to_string(),
            content: "content".to_string(),
        }];

        run_setup_steps(&steps, temp_dir.path()).unwrap();

        let content = std::fs::read_to_string(temp_dir.path().join("nested/dir/test.txt")).unwrap();
        assert_eq!(content, "content");
    }

    #[test]
    fn test_setup_create_dir() {
        let temp_dir = TempDir::new().unwrap();
        let steps = vec![SetupStep::CreateDir {
            path: "mydir".to_string(),
        }];

        run_setup_steps(&steps, temp_dir.path()).unwrap();

        assert!(temp_dir.path().join("mydir").is_dir());
    }
}
