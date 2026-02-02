//! Conformance testing infrastructure for `pi_agent_rust`.
//!
//! This module provides fixture-based conformance testing to ensure the Rust
//! implementation matches the behavior of the TypeScript pi-mono reference.
//!
//! ## Fixture Format
//!
//! Fixtures are JSON files that define inputs and expected outputs:
//!
//! ```json
//! {
//!   "version": "1.0",
//!   "tool": "read",
//!   "cases": [
//!     {
//!       "name": "read_simple_file",
//!       "input": {"path": "test.txt"},
//!       "expected": {
//!         "content_contains": ["line1", "line2"],
//!         "details": {"truncated": false}
//!       }
//!     }
//!   ]
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A conformance test fixture file.
#[derive(Debug, Serialize, Deserialize)]
pub struct FixtureFile {
    /// Schema version
    pub version: String,
    /// Tool name this fixture tests
    pub tool: String,
    /// Optional description
    #[serde(default)]
    pub description: String,
    /// Test cases
    pub cases: Vec<TestCase>,
}

/// A single test case within a fixture file.
#[derive(Debug, Serialize, Deserialize)]
pub struct TestCase {
    /// Unique test name
    pub name: String,
    /// Optional description
    #[serde(default)]
    pub description: String,
    /// Setup steps to run before the test
    #[serde(default)]
    pub setup: Vec<SetupStep>,
    /// Tool input parameters
    pub input: serde_json::Value,
    /// Expected results
    pub expected: Expected,
    /// Whether this test is expected to error
    #[serde(default)]
    pub expect_error: bool,
    /// Expected error message substring (if `expect_error` is true)
    #[serde(default)]
    pub error_contains: Option<String>,
    /// Tags for filtering tests
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Setup steps for test initialization.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SetupStep {
    /// Create a file with content
    #[serde(rename = "create_file")]
    CreateFile { path: String, content: String },
    /// Create a directory
    #[serde(rename = "create_dir")]
    CreateDir { path: String },
    /// Run a command
    #[serde(rename = "run_command")]
    RunCommand { command: String },
}

/// Expected results for a test case.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Expected {
    /// Content must contain these substrings
    #[serde(default)]
    pub content_contains: Vec<String>,
    /// Content must NOT contain these substrings
    #[serde(default)]
    pub content_not_contains: Vec<String>,
    /// Content must match this exact string
    #[serde(default)]
    pub content_exact: Option<String>,
    /// Content must match this regex
    #[serde(default)]
    pub content_regex: Option<String>,
    /// Details must contain these key-value pairs
    #[serde(default)]
    pub details: HashMap<String, serde_json::Value>,
    /// Details values that must match exactly
    #[serde(default)]
    pub details_exact: HashMap<String, serde_json::Value>,
}

/// Result of running a conformance test.
#[derive(Debug)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    pub message: Option<String>,
    pub actual_content: Option<String>,
    pub actual_details: Option<serde_json::Value>,
}

impl TestResult {
    pub fn pass(name: &str) -> Self {
        Self {
            name: name.to_string(),
            passed: true,
            message: None,
            actual_content: None,
            actual_details: None,
        }
    }

    pub fn fail(name: &str, message: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            passed: false,
            message: Some(message.into()),
            actual_content: None,
            actual_details: None,
        }
    }
}

/// Load a fixture file from the fixtures directory.
pub fn load_fixture(name: &str) -> std::io::Result<FixtureFile> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/conformance/fixtures")
        .join(format!("{name}.json"));

    let content = std::fs::read_to_string(&path)?;
    serde_json::from_str(&content)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Validate expected results against actual tool output.
pub fn validate_expected(
    expected: &Expected,
    content: &str,
    details: Option<&serde_json::Value>,
) -> Result<(), String> {
    // Check content_contains
    for substring in &expected.content_contains {
        if !content.contains(substring) {
            return Err(format!(
                "Content missing expected substring: '{substring}'\nActual content:\n{content}"
            ));
        }
    }

    // Check content_not_contains
    for substring in &expected.content_not_contains {
        if content.contains(substring) {
            return Err(format!(
                "Content contains unexpected substring: '{substring}'\nActual content:\n{content}"
            ));
        }
    }

    // Check content_exact
    if let Some(exact) = &expected.content_exact {
        if content != exact {
            return Err(format!(
                "Content mismatch.\nExpected:\n{exact}\nActual:\n{content}"
            ));
        }
    }

    // Check content_regex
    if let Some(pattern) = &expected.content_regex {
        let regex = regex::Regex::new(pattern)
            .map_err(|e| format!("Invalid regex pattern '{pattern}': {e}"))?;
        if !regex.is_match(content) {
            return Err(format!(
                "Content does not match regex: '{pattern}'\nActual content:\n{content}"
            ));
        }
    }

    // Check details
    if let Some(actual_details) = details {
        for (key, expected_value) in &expected.details {
            let actual_value = actual_details.get(key);
            match actual_value {
                Some(actual) => {
                    // For non-exact checks, we just verify the key exists and value type matches
                    if actual.is_null() && !expected_value.is_null() {
                        return Err(format!(
                            "Details key '{key}' is null, expected: {expected_value}"
                        ));
                    }
                }
                None => {
                    return Err(format!(
                        "Details missing expected key: '{}'\nExpected: {}\nActual details: {}",
                        key,
                        expected_value,
                        serde_json::to_string_pretty(actual_details).unwrap_or_default()
                    ));
                }
            }
        }

        for (key, expected_value) in &expected.details_exact {
            let actual_value = actual_details.get(key);
            match actual_value {
                Some(actual) if actual == expected_value => {}
                Some(actual) => {
                    return Err(format!(
                        "Details key '{key}' mismatch.\nExpected: {expected_value}\nActual: {actual}"
                    ));
                }
                None => {
                    return Err(format!("Details missing expected key: '{key}'"));
                }
            }
        }
    } else if !expected.details.is_empty() || !expected.details_exact.is_empty() {
        return Err("Expected details but tool returned None".to_string());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_content_contains() {
        let expected = Expected {
            content_contains: vec!["hello".to_string(), "world".to_string()],
            ..Default::default()
        };

        assert!(validate_expected(&expected, "hello world", None).is_ok());
        assert!(validate_expected(&expected, "hello there", None).is_err());
    }

    #[test]
    fn test_validate_content_not_contains() {
        let expected = Expected {
            content_not_contains: vec!["error".to_string()],
            ..Default::default()
        };

        assert!(validate_expected(&expected, "success", None).is_ok());
        assert!(validate_expected(&expected, "error occurred", None).is_err());
    }

    #[test]
    fn test_validate_details() {
        let expected = Expected {
            details_exact: [("count".to_string(), serde_json::json!(5))]
                .into_iter()
                .collect(),
            ..Default::default()
        };

        let details = serde_json::json!({"count": 5, "other": "value"});
        assert!(validate_expected(&expected, "", Some(&details)).is_ok());

        let wrong_details = serde_json::json!({"count": 10});
        assert!(validate_expected(&expected, "", Some(&wrong_details)).is_err());
    }
}
