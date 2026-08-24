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
use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};

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
    /// Specification requirement IDs this case covers.
    #[serde(default)]
    pub requirement_ids: Vec<String>,
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

impl TestCase {
    pub fn display_name(&self) -> String {
        if self.requirement_ids.is_empty() {
            self.name.clone()
        } else {
            format!("[{}] {}", self.requirement_ids.join(", "), self.name)
        }
    }
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
    /// Set a file or directory modification time from a Unix timestamp.
    #[serde(rename = "set_modified")]
    SetModified { path: String, unix_seconds: i64 },
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
    /// Content must exactly match the committed golden text file.
    #[serde(default)]
    pub content_golden: Option<String>,
    /// Details must contain these key-value pairs
    #[serde(default)]
    pub details: HashMap<String, serde_json::Value>,
    /// Details values that must match exactly
    #[serde(default)]
    pub details_exact: HashMap<String, serde_json::Value>,
    /// Details must contain these substrings (typically within diff output)
    #[serde(default)]
    pub details_contains: Vec<String>,
    /// Details must exactly match the committed golden JSON file.
    #[serde(default)]
    pub details_golden: Option<String>,
    /// Require that tool returned no details (i.e., `details` is None).
    #[serde(default)]
    pub details_none: bool,
}

/// Result of running a conformance test.
#[derive(Debug)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    pub message: Option<String>,
    pub actual_content: Option<String>,
    pub actual_details: Option<serde_json::Value>,
    /// Tool error observed for an expected-error case. Retained so the
    /// structured conformance log records the actual error taxonomy.
    pub actual_error: Option<String>,
}

impl TestResult {
    pub fn pass(name: &str) -> Self {
        Self {
            name: name.to_string(),
            passed: true,
            message: None,
            actual_content: None,
            actual_details: None,
            actual_error: None,
        }
    }

    pub fn fail(name: &str, message: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            passed: false,
            message: Some(message.into()),
            actual_content: None,
            actual_details: None,
            actual_error: None,
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

fn conformance_golden_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/conformance/goldens")
}

fn resolve_golden_path(relative: &str) -> Result<PathBuf, String> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute() {
        return Err(format!(
            "Golden path must be relative to tests/conformance/goldens: {relative}"
        ));
    }

    for component in relative_path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "Golden path must not escape tests/conformance/goldens: {relative}"
                ));
            }
        }
    }

    Ok(conformance_golden_root().join(relative_path))
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
    if let Some(exact) = &expected.content_exact
        && content != exact
    {
        return Err(format!(
            "Content mismatch.\nExpected:\n{exact}\nActual:\n{content}"
        ));
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

    if expected.details_none {
        if details.is_some() {
            return Err("Expected details to be None but tool returned Some".to_string());
        }
        if !expected.details.is_empty()
            || !expected.details_exact.is_empty()
            || !expected.details_contains.is_empty()
            || expected.details_golden.is_some()
        {
            return Err(
                "Invalid fixture: details_none cannot be combined with details expectations"
                    .to_string(),
            );
        }
        return Ok(());
    }

    // Check details
    if let Some(actual_details) = details {
        for (key, expected_value) in &expected.details {
            let actual_value = actual_details.get(key);
            match actual_value {
                Some(actual) => {
                    match_json_subset(actual, expected_value).map_err(|reason| {
                        format!(
                            "Details key '{key}' mismatch.\nExpected subset: {}\nActual: {}\nReason: {reason}",
                            serde_json::to_string_pretty(expected_value).unwrap_or_default(),
                            serde_json::to_string_pretty(actual).unwrap_or_default(),
                        )
                    })?;
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
        if !expected.details_contains.is_empty() {
            let details_text = serde_json::to_string_pretty(actual_details).unwrap_or_default();
            for substring in &expected.details_contains {
                if !details_text.contains(substring) {
                    return Err(format!(
                        "Details missing expected substring: '{substring}'\nActual details:\n{details_text}"
                    ));
                }
            }
        }
    } else if !expected.details.is_empty()
        || !expected.details_exact.is_empty()
        || !expected.details_contains.is_empty()
    {
        return Err("Expected details but tool returned None".to_string());
    }

    Ok(())
}

pub fn validate_expected_with_goldens(
    expected: &Expected,
    content: &str,
    details: Option<&serde_json::Value>,
) -> Result<(), String> {
    validate_expected(expected, content, details)?;

    if let Some(relative_path) = &expected.content_golden {
        let golden_path = resolve_golden_path(relative_path)?;
        let golden_content = std::fs::read_to_string(&golden_path).map_err(|err| {
            format!(
                "Failed to read content golden '{}': {err}",
                golden_path.display()
            )
        })?;
        if !content_matches_golden_text(content, &golden_content) {
            return Err(format!(
                "Content mismatch against golden '{}'.\nExpected:\n{}\nActual:\n{}",
                golden_path.display(),
                golden_content,
                content
            ));
        }
    }

    if let Some(relative_path) = &expected.details_golden {
        let actual_details = details.ok_or_else(|| {
            format!("Expected details golden '{relative_path}' but tool returned None")
        })?;
        let golden_path = resolve_golden_path(relative_path)?;
        let golden_content = std::fs::read_to_string(&golden_path).map_err(|err| {
            format!(
                "Failed to read details golden '{}': {err}",
                golden_path.display()
            )
        })?;
        let golden_details: serde_json::Value =
            serde_json::from_str(&golden_content).map_err(|err| {
                format!(
                    "Invalid JSON in details golden '{}': {err}",
                    golden_path.display()
                )
            })?;
        let actual_canonical = canonicalize_json(actual_details);
        let golden_canonical = canonicalize_json(&golden_details);
        if actual_canonical != golden_canonical {
            return Err(format!(
                "Details mismatch against golden '{}'.\nExpected:\n{}\nActual:\n{}",
                golden_path.display(),
                serde_json::to_string_pretty(&golden_canonical).unwrap_or_default(),
                serde_json::to_string_pretty(&actual_canonical).unwrap_or_default()
            ));
        }
    }

    Ok(())
}

fn content_matches_golden_text(content: &str, golden_content: &str) -> bool {
    content == golden_content
        || golden_content
            .strip_suffix('\n')
            .is_some_and(|trimmed| content == trimmed)
}

fn match_json_subset(
    actual: &serde_json::Value,
    expected: &serde_json::Value,
) -> Result<(), String> {
    match expected {
        serde_json::Value::Object(expected_map) => {
            let actual_map = actual
                .as_object()
                .ok_or_else(|| format!("expected object, found {}", json_type_name(actual)))?;
            for (key, expected_child) in expected_map {
                let actual_child = actual_map
                    .get(key)
                    .ok_or_else(|| format!("missing nested key '{key}'"))?;
                match_json_subset(actual_child, expected_child)
                    .map_err(|reason| format!("at nested key '{key}': {reason}"))?;
            }
            Ok(())
        }
        serde_json::Value::Array(expected_items) => {
            let actual_items = actual
                .as_array()
                .ok_or_else(|| format!("expected array, found {}", json_type_name(actual)))?;
            let mut candidates: Vec<Vec<usize>> = Vec::with_capacity(expected_items.len());
            for (expected_idx, expected_item) in expected_items.iter().enumerate() {
                let mut matches = Vec::new();
                for (actual_idx, actual_item) in actual_items.iter().enumerate() {
                    if match_json_subset(actual_item, expected_item).is_ok() {
                        matches.push(actual_idx);
                    }
                }
                if matches.is_empty() {
                    let expected_repr = serde_json::to_string(expected_item)
                        .unwrap_or_else(|_| expected_item.to_string());
                    return Err(format!(
                        "missing array element at index {expected_idx}: {expected_repr}"
                    ));
                }
                candidates.push(matches);
            }

            let mut used = vec![false; actual_items.len()];
            if !array_subset_backtrack(0, &candidates, &mut used) {
                return Err(
                    "array subset match requires distinct elements; refine expected array"
                        .to_string(),
                );
            }
            Ok(())
        }
        _ => {
            if actual == expected {
                Ok(())
            } else {
                Err(format!("expected {expected}, found {actual}"))
            }
        }
    }
}

fn array_subset_backtrack(
    expected_idx: usize,
    candidates: &[Vec<usize>],
    used: &mut [bool],
) -> bool {
    if expected_idx == candidates.len() {
        return true;
    }
    for &candidate in &candidates[expected_idx] {
        if used[candidate] {
            continue;
        }
        used[candidate] = true;
        if array_subset_backtrack(expected_idx + 1, candidates, used) {
            return true;
        }
        used[candidate] = false;
    }
    false
}

const fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn canonicalize_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalize_json).collect())
        }
        serde_json::Value::Object(map) => {
            let canonical = map
                .iter()
                .map(|(key, value)| (key.clone(), canonicalize_json(value)))
                .collect::<BTreeMap<_, _>>();
            serde_json::Value::Object(canonical.into_iter().collect())
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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
            details_exact: std::iter::once(("count".to_string(), serde_json::json!(5))).collect(),
            ..Default::default()
        };

        let details = serde_json::json!({"count": 5, "other": "value"});
        assert!(validate_expected(&expected, "", Some(&details)).is_ok());

        let wrong_details = serde_json::json!({"count": 10});
        assert!(validate_expected(&expected, "", Some(&wrong_details)).is_err());
    }

    #[test]
    fn test_validate_details_subset_accepts_nested_superset() {
        let expected = Expected {
            details: std::iter::once((
                "truncation".to_string(),
                serde_json::json!({
                    "truncated": true,
                    "summary": {
                        "kind": "bytes"
                    }
                }),
            ))
            .collect(),
            ..Default::default()
        };

        let actual = serde_json::json!({
            "truncation": {
                "truncated": true,
                "summary": {
                    "kind": "bytes",
                    "limit": 1024
                },
                "extra": true
            },
            "unused": "field"
        });

        assert!(validate_expected(&expected, "", Some(&actual)).is_ok());
    }

    #[test]
    fn test_validate_details_subset_rejects_nested_mismatch() {
        let expected = Expected {
            details: std::iter::once((
                "truncation".to_string(),
                serde_json::json!({
                    "summary": {
                        "kind": "bytes"
                    }
                }),
            ))
            .collect(),
            ..Default::default()
        };

        let actual = serde_json::json!({
            "truncation": {
                "summary": {
                    "kind": "lines"
                }
            }
        });

        let err = validate_expected(&expected, "", Some(&actual))
            .expect_err("subset mismatch should fail");
        assert!(err.contains("nested key 'summary'"));
        assert!(err.contains("expected \"bytes\", found \"lines\""));
    }

    #[test]
    fn test_validate_details_subset_accepts_array_superset_out_of_order() {
        let expected = Expected {
            details: std::iter::once((
                "items".to_string(),
                serde_json::json!([{"id": 1}, {"id": 2}]),
            ))
            .collect(),
            ..Default::default()
        };

        let actual = serde_json::json!({
            "items": [
                {"id": 2, "extra": true},
                {"id": 3},
                {"id": 1, "note": "ok"}
            ]
        });

        assert!(validate_expected(&expected, "", Some(&actual)).is_ok());
    }

    #[test]
    fn test_validate_details_subset_rejects_missing_array_item() {
        let expected = Expected {
            details: std::iter::once(("items".to_string(), serde_json::json!([1, 2]))).collect(),
            ..Default::default()
        };

        let actual = serde_json::json!({ "items": [2, 3] });
        let err = validate_expected(&expected, "", Some(&actual))
            .expect_err("subset should require all expected array items");
        assert!(err.contains("missing array element"));
    }

    #[test]
    fn test_validate_details_subset_array_backtracks_for_ambiguous_matches() {
        let expected = Expected {
            details: std::iter::once((
                "items".to_string(),
                serde_json::json!([{"id": 1}, {"id": 1, "tag": "a"}]),
            ))
            .collect(),
            ..Default::default()
        };

        let actual = serde_json::json!({
            "items": [
                {"id": 1, "tag": "a"},
                {"id": 1, "tag": "b"}
            ]
        });

        assert!(validate_expected(&expected, "", Some(&actual)).is_ok());
    }

    #[test]
    fn test_validate_details_contains_diff() {
        let expected = Expected {
            details_contains: vec!["-1 Hello".to_string(), "+1 Hi".to_string()],
            ..Default::default()
        };

        let details = serde_json::json!({
            "diff": "-1 Hello\n+1 Hi",
            "firstChangedLine": 1
        });
        assert!(validate_expected(&expected, "", Some(&details)).is_ok());

        let wrong_details = serde_json::json!({
            "diff": "+1 Hi"
        });
        assert!(validate_expected(&expected, "", Some(&wrong_details)).is_err());
    }

    #[test]
    fn test_display_name_includes_requirement_ids() {
        let case = TestCase {
            name: "defaults".to_string(),
            description: String::new(),
            requirement_ids: vec!["CLI-DEFAULTS".to_string(), "CLI-SHAPE".to_string()],
            setup: Vec::new(),
            input: serde_json::json!({}),
            expected: Expected::default(),
            expect_error: false,
            error_contains: None,
            tags: Vec::new(),
        };

        assert_eq!(case.display_name(), "[CLI-DEFAULTS, CLI-SHAPE] defaults");
    }

    #[test]
    fn test_validate_expected_with_details_golden() {
        let expected = Expected {
            details_golden: Some("cli_flags/defaults.json".to_string()),
            ..Default::default()
        };
        let details_path = conformance_golden_root().join("cli_flags/defaults.json");
        let details: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(details_path).expect("read defaults golden"),
        )
        .expect("parse defaults golden");

        assert!(validate_expected_with_goldens(&expected, "", Some(&details)).is_ok());
    }

    #[test]
    fn test_validate_expected_with_details_golden_ignores_object_key_order() {
        let expected = Expected {
            details_golden: Some("cli_flags/defaults.json".to_string()),
            ..Default::default()
        };
        let details = serde_json::json!({
            "tools": "read,bash,edit,write,grep,find,ls,hashline_edit,web_search,ast_grep,ast_edit,lsp,debug,ask,todo,submit_plan,jobs,hub",
            "extension": [],
            "extension_flags": [],
            "theme_path": [],
            "prompt_template": [],
            "skill": [],
            "message_args": [],
            "file_args": [],
            "command": null,
            "list_providers": false,
            "list_models": null,
            "max_tool_iterations": null,
            "export": null,
            "hide_cwd_in_prompt": false,
            "no_themes": false,
            "theme": null,
            "no_prompt_templates": false,
            "no_skills": false,
            "explain_repair_policy": false,
            "repair_policy": null,
            "explain_extension_policy": false,
            "extension_policy": null,
            "no_extensions": false,
            "no_tools": false,
            "verbose": false,
            "acp": false,
            "print": false,
            "mode": null,
            "rpc": false,
            "no_migrations": false,
            "no_mouse_capture": false,
            "session_durability": null,
            "no_session": false,
            "session_dir": null,
            "session": null,
            "resume": false,
            "continue": false,
            "append_system_prompt": null,
            "system_prompt": null,
            "thinking": null,
            "models": null,
            "api_key": null,
            "model": null,
            "provider": null,
            "version": false
        });

        assert!(validate_expected_with_goldens(&expected, "", Some(&details)).is_ok());
    }

    #[test]
    fn test_validate_expected_with_content_golden() {
        let golden_path = conformance_golden_root().join("cli_flags/defaults.json");
        let golden_content =
            std::fs::read_to_string(golden_path).expect("read content golden fixture");
        let expected = Expected {
            content_golden: Some("cli_flags/defaults.json".to_string()),
            ..Default::default()
        };

        assert!(validate_expected_with_goldens(&expected, &golden_content, None).is_ok());
    }

    #[test]
    fn test_validate_expected_rejects_details_none_with_details_golden() {
        let expected = Expected {
            details_none: true,
            details_golden: Some("cli_flags/defaults.json".to_string()),
            ..Default::default()
        };

        let err = validate_expected_with_goldens(&expected, "", None)
            .expect_err("details_none must reject details goldens");
        assert!(err.contains("details_none"));
    }

    #[test]
    fn test_validate_expected_rejects_parent_dir_golden_path() {
        let expected = Expected {
            content_golden: Some("../escape.txt".to_string()),
            ..Default::default()
        };

        let err =
            validate_expected_with_goldens(&expected, "", None).expect_err("must reject escape");
        assert!(err.contains("must not escape"));
    }

    #[test]
    fn test_validate_expected_rejects_absolute_golden_path() {
        let expected = Expected {
            content_golden: Some("/tmp/absolute.txt".to_string()),
            ..Default::default()
        };

        let err = validate_expected_with_goldens(&expected, "", None)
            .expect_err("must reject absolute path");
        assert!(err.contains("relative to tests/conformance/goldens"));
    }

    #[test]
    fn test_match_json_subset_requires_distinct_array_elements() {
        let expected = serde_json::json!([
            { "id": 1 },
            { "id": 1 }
        ]);
        let actual = serde_json::json!([
            { "id": 1 }
        ]);

        let err = match_json_subset(&actual, &expected)
            .expect_err("duplicate expected elements should require distinct matches");
        assert!(err.contains("distinct elements"));
    }

    #[test]
    fn test_match_json_subset_allows_distinct_duplicate_matches() {
        let expected = serde_json::json!([
            { "id": 1 },
            { "id": 1 }
        ]);
        let actual = serde_json::json!([
            { "id": 1 },
            { "id": 1 }
        ]);

        assert!(match_json_subset(&actual, &expected).is_ok());
    }

    #[test]
    fn test_canonicalize_json_is_idempotent() {
        let sample = serde_json::json!({
            "b": 2,
            "a": {
                "d": [
                    {"z": 1, "y": 2},
                    3
                ],
                "c": "x"
            },
            "arr": [
                {"k": "v", "j": 1},
                ["nested", {"b": true, "a": false}]
            ]
        });

        let once = canonicalize_json(&sample);
        let twice = canonicalize_json(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn test_canonicalize_json_ignores_object_key_order() {
        let left = serde_json::json!({
            "b": 1,
            "a": 2
        });
        let right = serde_json::json!({
            "a": 2,
            "b": 1
        });

        assert_eq!(canonicalize_json(&left), canonicalize_json(&right));
    }

    proptest! {
        #[test]
        fn proptest_details_subset_is_monotonic_under_extra_fields(
            count in 0u64..1000,
            noise in any::<bool>(),
            tail in proptest::collection::vec(any::<u8>(), 0..8),
        ) {
            let expected = Expected {
                details: std::iter::once((
                    "meta".to_string(),
                    serde_json::json!({
                        "count": count,
                        "nested": {
                            "enabled": true
                        }
                    }),
                ))
                .collect(),
                ..Default::default()
            };

            let actual = serde_json::json!({
                "meta": {
                    "count": count,
                    "nested": {
                        "enabled": true,
                        "noise": noise
                    },
                    "tail": tail
                },
                "extra_top_level": {
                    "present": true
                }
            });

            prop_assert!(validate_expected(&expected, "", Some(&actual)).is_ok());
        }

        #[test]
        fn proptest_content_contains_is_monotonic_under_append(
            prefix in "[a-zA-Z0-9 _-]{0,12}",
            suffix in "[a-zA-Z0-9 _-]{0,12}",
            extra in "[a-zA-Z0-9 _-]{0,12}",
            needle_a in "[a-zA-Z]{1,6}",
            needle_b in "[a-zA-Z]{1,6}",
        ) {
            let expected = Expected {
                content_contains: vec![needle_a.clone(), needle_b.clone()],
                ..Default::default()
            };

            let base = format!("{prefix}{needle_a}:{needle_b}{suffix}");
            let extended = format!("{base}{extra}");

            prop_assert!(validate_expected(&expected, &base, None).is_ok());
            prop_assert!(validate_expected(&expected, &extended, None).is_ok());
        }

        #[test]
        fn proptest_content_not_contains_is_monotonic_under_safe_append(
            prefix in "[a-z0-9 _-]{0,12}",
            suffix in "[a-z0-9 _-]{0,12}",
            extra in "[a-z0-9 _-]{0,12}",
            needle in "[A-Z]{1,6}",
        ) {
            let expected = Expected {
                content_not_contains: vec![needle],
                ..Default::default()
            };

            let base = format!("{prefix}{suffix}");
            let extended = format!("{base}{extra}");

            prop_assert!(validate_expected(&expected, &base, None).is_ok());
            prop_assert!(validate_expected(&expected, &extended, None).is_ok());
        }

        #[test]
        fn proptest_details_subset_array_is_monotonic(
            head in proptest::collection::vec(any::<u8>(), 0..8),
            tail in proptest::collection::vec(any::<u8>(), 0..8),
        ) {
            let expected = Expected {
                details: std::iter::once(("items".to_string(), serde_json::json!(&head)))
                    .collect(),
                ..Default::default()
            };

            let mut actual_items = head;
            actual_items.extend(tail);
            let actual = serde_json::json!({ "items": actual_items });

            prop_assert!(validate_expected(&expected, "", Some(&actual)).is_ok());
        }

        #[test]
        fn proptest_canonicalize_json_invariant_to_key_order(
            map in proptest::collection::btree_map("[a-z]{1,6}", any::<u8>(), 0..12),
        ) {
            let mut forward = serde_json::Map::new();
            for (key, value) in &map {
                forward.insert(key.clone(), serde_json::json!(*value));
            }
            let mut reverse = serde_json::Map::new();
            for (key, value) in map.iter().rev() {
                reverse.insert(key.clone(), serde_json::json!(*value));
            }

            let left = serde_json::Value::Object(forward);
            let right = serde_json::Value::Object(reverse);
            prop_assert_eq!(canonicalize_json(&left), canonicalize_json(&right));
        }
    }
}
