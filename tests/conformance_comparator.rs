//! Semantic JSON comparator for extension conformance testing (bd-2ylf).
//!
//! Compares two `ConformanceOutput` JSON structures semantically, handling:
//! - JSON key ordering (canonicalized before comparison)
//! - Numeric precision (f64 tolerance 1e-10)
//! - `null` vs missing field equivalence
//! - Set-based array comparison for registrations (tools, commands, flags, shortcuts)
//! - Ordered array comparison for hostcall sequences
//! - Human-readable diff output for failures

mod common;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ─── Comparison Result ───────────────────────────────────────────────────────

/// Status of a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareStatus {
    Pass,
    Fail,
    Error,
}

/// A single diff between two JSON values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffEntry {
    /// Dotted path to the differing field (e.g., `registrations.commands[0].name`).
    pub path: String,
    /// Category of the diff.
    pub category: String,
    /// Human-readable description of the difference.
    pub message: String,
    /// Expected value (from TS output).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<Value>,
    /// Actual value (from Rust output).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<Value>,
}

/// Result of comparing two conformance outputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompareResult {
    pub status: CompareStatus,
    pub registration_match: bool,
    pub hostcall_match: bool,
    pub diffs: Vec<DiffEntry>,
}

impl CompareResult {
    const fn pass() -> Self {
        Self {
            status: CompareStatus::Pass,
            registration_match: true,
            hostcall_match: true,
            diffs: Vec::new(),
        }
    }
}

// ─── Numeric Tolerance ───────────────────────────────────────────────────────

const FLOAT_TOLERANCE: f64 = 1e-10;

fn numbers_equal(a: f64, b: f64) -> bool {
    (a - b).abs() <= FLOAT_TOLERANCE + f64::EPSILON
}

// ─── Canonicalization ────────────────────────────────────────────────────────

/// Canonicalize a JSON value: sort object keys recursively, normalize nulls.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by_key(|(key, _)| *key);
            let mut out = serde_json::Map::with_capacity(entries.len());
            for (key, val) in entries {
                let canonical = canonicalize(val);
                // Skip null values (null ≡ missing for conformance)
                if canonical != Value::Null {
                    out.insert(key.clone(), canonical);
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

// ─── Deep Comparison ─────────────────────────────────────────────────────────

/// Compare two JSON values deeply with path tracking.
#[allow(clippy::too_many_lines)]
fn deep_compare(expected: &Value, actual: &Value, path: &str, diffs: &mut Vec<DiffEntry>) {
    match (expected, actual) {
        (Value::Null, Value::Null) => {}
        (Value::Bool(a), Value::Bool(b)) if a == b => {}
        (Value::Number(a), Value::Number(b)) => {
            let af = a.as_f64().unwrap_or(0.0);
            let bf = b.as_f64().unwrap_or(0.0);
            if !numbers_equal(af, bf) {
                diffs.push(DiffEntry {
                    path: path.to_string(),
                    category: "value".to_string(),
                    message: format!("numeric mismatch: {af} vs {bf}"),
                    expected: Some(expected.clone()),
                    actual: Some(actual.clone()),
                });
            }
        }
        (Value::String(a), Value::String(b)) if a == b => {}
        (Value::Object(a_map), Value::Object(b_map)) => {
            // Check all keys in expected
            for (key, a_val) in a_map {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                match b_map.get(key) {
                    Some(b_val) => {
                        deep_compare(a_val, b_val, &child_path, diffs);
                    }
                    None => {
                        // Missing key — only a diff if expected value is not null
                        if *a_val != Value::Null {
                            diffs.push(DiffEntry {
                                path: child_path,
                                category: "missing_key".to_string(),
                                message: format!("key '{key}' missing in actual"),
                                expected: Some(a_val.clone()),
                                actual: None,
                            });
                        }
                    }
                }
            }
            // Check for extra keys in actual
            for (key, b_val) in b_map {
                if !a_map.contains_key(key) && *b_val != Value::Null {
                    let child_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    diffs.push(DiffEntry {
                        path: child_path,
                        category: "extra_key".to_string(),
                        message: format!("unexpected key '{key}' in actual"),
                        expected: None,
                        actual: Some(b_val.clone()),
                    });
                }
            }
        }
        (Value::Array(a_arr), Value::Array(b_arr)) => {
            // Ordered array comparison
            let max_len = a_arr.len().max(b_arr.len());
            for i in 0..max_len {
                let child_path = format!("{path}[{i}]");
                match (a_arr.get(i), b_arr.get(i)) {
                    (Some(a_val), Some(b_val)) => {
                        deep_compare(a_val, b_val, &child_path, diffs);
                    }
                    (Some(a_val), None) => {
                        diffs.push(DiffEntry {
                            path: child_path,
                            category: "array_length".to_string(),
                            message: format!(
                                "expected array element at index {i}, actual array too short"
                            ),
                            expected: Some(a_val.clone()),
                            actual: None,
                        });
                    }
                    (None, Some(b_val)) => {
                        diffs.push(DiffEntry {
                            path: child_path,
                            category: "array_length".to_string(),
                            message: format!(
                                "unexpected array element at index {i}, expected array shorter"
                            ),
                            expected: None,
                            actual: Some(b_val.clone()),
                        });
                    }
                    (None, None) => unreachable!(),
                }
            }
        }
        // Type mismatch or value mismatch
        _ => {
            // null vs missing/empty array equivalence
            let is_null_empty = matches!(
                (expected, actual),
                (Value::Null, Value::Array(arr)) if arr.is_empty()
            ) || matches!(
                (expected, actual),
                (Value::Array(arr), Value::Null) if arr.is_empty()
            );

            if !is_null_empty {
                diffs.push(DiffEntry {
                    path: path.to_string(),
                    category: "value".to_string(),
                    message: format!(
                        "value mismatch: expected {}, actual {}",
                        value_summary(expected),
                        value_summary(actual)
                    ),
                    expected: Some(expected.clone()),
                    actual: Some(actual.clone()),
                });
            }
        }
    }
}

/// Brief human-readable summary of a JSON value.
fn value_summary(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            if s.len() > 40 {
                format!("\"{}...\"", &s[..37])
            } else {
                format!("\"{s}\"")
            }
        }
        Value::Array(arr) => format!("array[{}]", arr.len()),
        Value::Object(map) => format!("object{{{} keys}}", map.len()),
    }
}

// ─── Set-Based Array Comparison ──────────────────────────────────────────────

/// Compare two arrays as sets, matching elements by a key field.
/// Returns diffs for: missing items, extra items, and per-item field diffs.
fn compare_arrays_by_key(
    expected: &[Value],
    actual: &[Value],
    path: &str,
    key_field: &str,
    diffs: &mut Vec<DiffEntry>,
) {
    // Index both arrays by key
    let expected_map: std::collections::HashMap<String, &Value> = expected
        .iter()
        .filter_map(|v| {
            v.get(key_field)
                .and_then(Value::as_str)
                .map(|k| (k.to_string(), v))
        })
        .collect();

    let actual_map: std::collections::HashMap<String, &Value> = actual
        .iter()
        .filter_map(|v| {
            v.get(key_field)
                .and_then(Value::as_str)
                .map(|k| (k.to_string(), v))
        })
        .collect();

    // Check for missing items (in expected but not actual)
    let mut expected_keys: Vec<_> = expected_map.keys().collect();
    expected_keys.sort();
    for key in &expected_keys {
        let child_path = format!("{path}[{key_field}={key}]");
        match actual_map.get(key.as_str()) {
            Some(actual_val) => {
                // Compare the matched items
                let canon_exp = canonicalize(expected_map[key.as_str()]);
                let canon_act = canonicalize(actual_val);
                deep_compare(&canon_exp, &canon_act, &child_path, diffs);
            }
            None => {
                diffs.push(DiffEntry {
                    path: child_path,
                    category: "missing_item".to_string(),
                    message: format!("item with {key_field}='{key}' missing in actual"),
                    expected: Some(expected_map[key.as_str()].clone()),
                    actual: None,
                });
            }
        }
    }

    // Check for extra items (in actual but not expected)
    let mut actual_keys: Vec<_> = actual_map.keys().collect();
    actual_keys.sort();
    for key in &actual_keys {
        if !expected_map.contains_key(key.as_str()) {
            let child_path = format!("{path}[{key_field}={key}]");
            diffs.push(DiffEntry {
                path: child_path,
                category: "extra_item".to_string(),
                message: format!("unexpected item with {key_field}='{key}' in actual"),
                expected: None,
                actual: Some(actual_map[key.as_str()].clone()),
            });
        }
    }
}

// ─── Registration Comparison ─────────────────────────────────────────────────

/// Compare registration structures semantically.
#[allow(clippy::too_many_lines)]
fn compare_registrations(expected: &Value, actual: &Value, diffs: &mut Vec<DiffEntry>) {
    let path = "registrations";

    // Commands: set comparison by "name"
    if let (Some(exp_cmds), Some(act_cmds)) = (
        expected.get("commands").and_then(Value::as_array),
        actual.get("commands").and_then(Value::as_array),
    ) {
        compare_arrays_by_key(
            exp_cmds,
            act_cmds,
            &format!("{path}.commands"),
            "name",
            diffs,
        );
    } else {
        deep_compare(
            expected.get("commands").unwrap_or(&Value::Null),
            actual.get("commands").unwrap_or(&Value::Null),
            &format!("{path}.commands"),
            diffs,
        );
    }

    // Shortcuts: set comparison by "key_id" or "keyId"
    if let (Some(exp_sc), Some(act_sc)) = (
        expected.get("shortcuts").and_then(Value::as_array),
        actual.get("shortcuts").and_then(Value::as_array),
    ) {
        // Determine key field
        let key_field = if exp_sc.first().and_then(|v| v.get("key_id")).is_some() {
            "key_id"
        } else {
            "keyId"
        };
        compare_arrays_by_key(
            exp_sc,
            act_sc,
            &format!("{path}.shortcuts"),
            key_field,
            diffs,
        );
    } else {
        deep_compare(
            expected.get("shortcuts").unwrap_or(&Value::Null),
            actual.get("shortcuts").unwrap_or(&Value::Null),
            &format!("{path}.shortcuts"),
            diffs,
        );
    }

    // Flags: set comparison by "name"
    if let (Some(exp_flags), Some(act_flags)) = (
        expected.get("flags").and_then(Value::as_array),
        actual.get("flags").and_then(Value::as_array),
    ) {
        compare_arrays_by_key(
            exp_flags,
            act_flags,
            &format!("{path}.flags"),
            "name",
            diffs,
        );
    } else {
        deep_compare(
            expected.get("flags").unwrap_or(&Value::Null),
            actual.get("flags").unwrap_or(&Value::Null),
            &format!("{path}.flags"),
            diffs,
        );
    }

    // Providers: set comparison by "id"
    if let (Some(exp_prov), Some(act_prov)) = (
        expected.get("providers").and_then(Value::as_array),
        actual.get("providers").and_then(Value::as_array),
    ) {
        compare_arrays_by_key(
            exp_prov,
            act_prov,
            &format!("{path}.providers"),
            "id",
            diffs,
        );
    } else {
        deep_compare(
            expected.get("providers").unwrap_or(&Value::Null),
            actual.get("providers").unwrap_or(&Value::Null),
            &format!("{path}.providers"),
            diffs,
        );
    }

    // Tool defs: set comparison by "name"
    if let (Some(exp_tools), Some(act_tools)) = (
        expected.get("tool_defs").and_then(Value::as_array),
        actual.get("tool_defs").and_then(Value::as_array),
    ) {
        compare_arrays_by_key(
            exp_tools,
            act_tools,
            &format!("{path}.tool_defs"),
            "name",
            diffs,
        );
    } else {
        deep_compare(
            expected.get("tool_defs").unwrap_or(&Value::Null),
            actual.get("tool_defs").unwrap_or(&Value::Null),
            &format!("{path}.tool_defs"),
            diffs,
        );
    }

    // Models: set comparison by "id"
    if let (Some(exp_models), Some(act_models)) = (
        expected.get("models").and_then(Value::as_array),
        actual.get("models").and_then(Value::as_array),
    ) {
        compare_arrays_by_key(
            exp_models,
            act_models,
            &format!("{path}.models"),
            "id",
            diffs,
        );
    } else {
        deep_compare(
            expected.get("models").unwrap_or(&Value::Null),
            actual.get("models").unwrap_or(&Value::Null),
            &format!("{path}.models"),
            diffs,
        );
    }

    // Event hooks: set comparison (simple string arrays)
    if let (Some(exp_hooks), Some(act_hooks)) = (
        expected.get("event_hooks").and_then(Value::as_array),
        actual.get("event_hooks").and_then(Value::as_array),
    ) {
        let mut exp_set: Vec<String> = exp_hooks
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
        let mut act_set: Vec<String> = act_hooks
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
        exp_set.sort();
        act_set.sort();
        if exp_set != act_set {
            diffs.push(DiffEntry {
                path: format!("{path}.event_hooks"),
                category: "set_mismatch".to_string(),
                message: format!("event hooks differ: expected {exp_set:?}, actual {act_set:?}"),
                expected: Some(serde_json::to_value(&exp_set).unwrap()),
                actual: Some(serde_json::to_value(&act_set).unwrap()),
            });
        }
    }
}

// ─── Hostcall Comparison ─────────────────────────────────────────────────────

/// Compare hostcall logs in order.
fn compare_hostcalls(expected: &Value, actual: &Value, diffs: &mut Vec<DiffEntry>) {
    let exp_arr = expected.as_array().cloned().unwrap_or_default();
    let act_arr = actual.as_array().cloned().unwrap_or_default();

    let max_len = exp_arr.len().max(act_arr.len());
    for i in 0..max_len {
        let path = format!("hostcall_log[{i}]");
        match (exp_arr.get(i), act_arr.get(i)) {
            (Some(exp), Some(act)) => {
                let canon_exp = canonicalize(exp);
                let canon_act = canonicalize(act);
                deep_compare(&canon_exp, &canon_act, &path, diffs);
            }
            (Some(exp), None) => {
                diffs.push(DiffEntry {
                    path,
                    category: "missing_hostcall".to_string(),
                    message: format!("expected hostcall at index {i} not found in actual"),
                    expected: Some(exp.clone()),
                    actual: None,
                });
            }
            (None, Some(act)) => {
                diffs.push(DiffEntry {
                    path,
                    category: "extra_hostcall".to_string(),
                    message: format!("unexpected hostcall at index {i} in actual"),
                    expected: None,
                    actual: Some(act.clone()),
                });
            }
            (None, None) => unreachable!(),
        }
    }
}

// ─── Top-Level Compare ───────────────────────────────────────────────────────

/// Compare two conformance outputs semantically.
///
/// Uses set-based comparison for registrations and ordered comparison
/// for hostcall sequences. Returns a structured `CompareResult`.
#[must_use]
pub fn compare_conformance_outputs(expected: &Value, actual: &Value) -> CompareResult {
    let mut diffs = Vec::new();

    // Compare metadata
    deep_compare(
        expected.get("extension_id").unwrap_or(&Value::Null),
        actual.get("extension_id").unwrap_or(&Value::Null),
        "extension_id",
        &mut diffs,
    );

    // Compare registrations (set-based)
    let exp_reg = expected.get("registrations").unwrap_or(&Value::Null);
    let act_reg = actual.get("registrations").unwrap_or(&Value::Null);
    let reg_diffs_before = diffs.len();
    compare_registrations(exp_reg, act_reg, &mut diffs);
    let registration_match = diffs.len() == reg_diffs_before;

    // Compare hostcall log (ordered)
    let exp_log = expected.get("hostcall_log").unwrap_or(&Value::Null);
    let act_log = actual.get("hostcall_log").unwrap_or(&Value::Null);
    let hc_diffs_before = diffs.len();
    compare_hostcalls(exp_log, act_log, &mut diffs);
    let hostcall_match = diffs.len() == hc_diffs_before;

    let status = if diffs.is_empty() {
        CompareStatus::Pass
    } else {
        CompareStatus::Fail
    };

    CompareResult {
        status,
        registration_match,
        hostcall_match,
        diffs,
    }
}

/// Format a `CompareResult` as a human-readable report.
#[must_use]
pub fn format_compare_report(result: &CompareResult) -> String {
    use std::fmt::Write as _;

    let mut report = String::new();
    let _ = writeln!(
        report,
        "Status: {:?} | Registrations: {} | Hostcalls: {}",
        result.status,
        if result.registration_match {
            "MATCH"
        } else {
            "DIFFER"
        },
        if result.hostcall_match {
            "MATCH"
        } else {
            "DIFFER"
        },
    );

    if result.diffs.is_empty() {
        report.push_str("No differences found.\n");
    } else {
        let _ = writeln!(report, "{} difference(s):", result.diffs.len());
        for (i, diff) in result.diffs.iter().enumerate() {
            let _ = writeln!(
                report,
                "  {}. [{}] {} — {}",
                i + 1,
                diff.category,
                diff.path,
                diff.message
            );
        }
    }

    report
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn identical_outputs_produce_pass() {
    let output = serde_json::json!({
        "extension_id": "hello",
        "registrations": {
            "commands": [{"name": "hello", "description": "Say hello"}],
            "shortcuts": [],
            "flags": [],
            "providers": [],
            "tool_defs": [],
            "models": [],
            "event_hooks": []
        },
        "hostcall_log": []
    });

    let result = compare_conformance_outputs(&output, &output);
    assert_eq!(result.status, CompareStatus::Pass);
    assert!(result.registration_match);
    assert!(result.hostcall_match);
    assert!(result.diffs.is_empty());
}

#[test]
fn key_ordering_does_not_matter() {
    let expected = serde_json::json!({
        "extension_id": "hello",
        "registrations": {
            "commands": [{"name": "hello", "description": "Say hello"}],
            "shortcuts": [],
            "flags": [],
            "providers": [],
            "tool_defs": [],
            "models": [],
            "event_hooks": []
        },
        "hostcall_log": []
    });

    // Same data but with different key ordering
    let actual = serde_json::json!({
        "extension_id": "hello",
        "registrations": {
            "commands": [{"description": "Say hello", "name": "hello"}],
            "shortcuts": [],
            "flags": [],
            "providers": [],
            "models": [],
            "tool_defs": [],
            "event_hooks": []
        },
        "hostcall_log": []
    });

    let result = compare_conformance_outputs(&expected, &actual);
    assert_eq!(result.status, CompareStatus::Pass);
    assert!(result.diffs.is_empty());
}

#[test]
fn null_vs_missing_treated_as_equal() {
    let expected = serde_json::json!({
        "extension_id": "hello",
        "registrations": {
            "commands": [{"name": "hello", "description": "Say hello", "extra": null}],
            "shortcuts": [],
            "flags": [],
            "providers": [],
            "tool_defs": [],
            "models": [],
            "event_hooks": []
        },
        "hostcall_log": []
    });

    let actual = serde_json::json!({
        "extension_id": "hello",
        "registrations": {
            "commands": [{"name": "hello", "description": "Say hello"}],
            "shortcuts": [],
            "flags": [],
            "providers": [],
            "tool_defs": [],
            "models": [],
            "event_hooks": []
        },
        "hostcall_log": []
    });

    let result = compare_conformance_outputs(&expected, &actual);
    assert_eq!(result.status, CompareStatus::Pass);
}

#[test]
fn numeric_precision_tolerance() {
    let expected = serde_json::json!({
        "extension_id": "hello",
        "registrations": {
            "commands": [{"name": "t", "value": 1.000_000_000_1}],
            "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": []
    });

    let actual = serde_json::json!({
        "extension_id": "hello",
        "registrations": {
            "commands": [{"name": "t", "value": 1.0}],
            "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": []
    });

    let result = compare_conformance_outputs(&expected, &actual);
    assert_eq!(result.status, CompareStatus::Pass);
}

#[test]
fn numeric_significant_difference_detected() {
    let expected = serde_json::json!({
        "extension_id": "hello",
        "registrations": {
            "commands": [{"name": "t", "value": 100}],
            "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": []
    });

    let actual = serde_json::json!({
        "extension_id": "hello",
        "registrations": {
            "commands": [{"name": "t", "value": 200}],
            "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": []
    });

    let result = compare_conformance_outputs(&expected, &actual);
    assert_eq!(result.status, CompareStatus::Fail);
    assert!(!result.registration_match);
    assert!(result.diffs.iter().any(|d| d.category == "value"));
}

#[test]
fn set_based_registration_order_independent() {
    let expected = serde_json::json!({
        "extension_id": "multi",
        "registrations": {
            "commands": [
                {"name": "cmd-b", "description": "B"},
                {"name": "cmd-a", "description": "A"}
            ],
            "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": []
    });

    // Reversed order
    let actual = serde_json::json!({
        "extension_id": "multi",
        "registrations": {
            "commands": [
                {"name": "cmd-a", "description": "A"},
                {"name": "cmd-b", "description": "B"}
            ],
            "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": []
    });

    let result = compare_conformance_outputs(&expected, &actual);
    assert_eq!(result.status, CompareStatus::Pass);
    assert!(result.registration_match);
}

#[test]
fn missing_registration_detected() {
    let expected = serde_json::json!({
        "extension_id": "test",
        "registrations": {
            "commands": [
                {"name": "cmd-a", "description": "A"},
                {"name": "cmd-b", "description": "B"}
            ],
            "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": []
    });

    // Missing cmd-b
    let actual = serde_json::json!({
        "extension_id": "test",
        "registrations": {
            "commands": [
                {"name": "cmd-a", "description": "A"}
            ],
            "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": []
    });

    let result = compare_conformance_outputs(&expected, &actual);
    assert_eq!(result.status, CompareStatus::Fail);
    assert!(!result.registration_match);
    assert!(
        result
            .diffs
            .iter()
            .any(|d| d.category == "missing_item" && d.path.contains("cmd-b"))
    );
}

#[test]
fn extra_registration_detected() {
    let expected = serde_json::json!({
        "extension_id": "test",
        "registrations": {
            "commands": [{"name": "cmd-a", "description": "A"}],
            "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": []
    });

    // Extra cmd-b
    let actual = serde_json::json!({
        "extension_id": "test",
        "registrations": {
            "commands": [
                {"name": "cmd-a", "description": "A"},
                {"name": "cmd-b", "description": "B"}
            ],
            "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": []
    });

    let result = compare_conformance_outputs(&expected, &actual);
    assert_eq!(result.status, CompareStatus::Fail);
    assert!(!result.registration_match);
    assert!(
        result
            .diffs
            .iter()
            .any(|d| d.category == "extra_item" && d.path.contains("cmd-b"))
    );
}

#[test]
fn registration_field_diff_detected() {
    let expected = serde_json::json!({
        "extension_id": "test",
        "registrations": {
            "commands": [{"name": "hello", "description": "Say hello"}],
            "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": []
    });

    // Different description
    let actual = serde_json::json!({
        "extension_id": "test",
        "registrations": {
            "commands": [{"name": "hello", "description": "Say goodbye"}],
            "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": []
    });

    let result = compare_conformance_outputs(&expected, &actual);
    assert_eq!(result.status, CompareStatus::Fail);
    assert!(!result.registration_match);
    assert!(result.diffs.iter().any(|d| d.path.contains("description")));
}

#[test]
fn hostcall_order_matters() {
    let expected = serde_json::json!({
        "extension_id": "test",
        "registrations": {
            "commands": [], "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": [
            {"op": "get_state", "result": {}},
            {"op": "set_name", "payload": {"name": "test"}}
        ]
    });

    // Reversed hostcall order
    let actual = serde_json::json!({
        "extension_id": "test",
        "registrations": {
            "commands": [], "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": [
            {"op": "set_name", "payload": {"name": "test"}},
            {"op": "get_state", "result": {}}
        ]
    });

    let result = compare_conformance_outputs(&expected, &actual);
    assert_eq!(result.status, CompareStatus::Fail);
    assert!(!result.hostcall_match);
}

#[test]
fn hostcall_length_mismatch_detected() {
    let expected = serde_json::json!({
        "extension_id": "test",
        "registrations": {
            "commands": [], "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": [
            {"op": "get_state"},
            {"op": "set_name", "payload": {"name": "test"}}
        ]
    });

    // Only one hostcall
    let actual = serde_json::json!({
        "extension_id": "test",
        "registrations": {
            "commands": [], "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": [
            {"op": "get_state"}
        ]
    });

    let result = compare_conformance_outputs(&expected, &actual);
    assert_eq!(result.status, CompareStatus::Fail);
    assert!(!result.hostcall_match);
    assert!(
        result
            .diffs
            .iter()
            .any(|d| d.category == "missing_hostcall")
    );
}

#[test]
fn event_hooks_set_comparison() {
    let expected = serde_json::json!({
        "extension_id": "test",
        "registrations": {
            "commands": [], "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [],
            "event_hooks": ["tool_start", "tool_end", "agent_start"]
        },
        "hostcall_log": []
    });

    // Same hooks, different order
    let actual = serde_json::json!({
        "extension_id": "test",
        "registrations": {
            "commands": [], "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [],
            "event_hooks": ["agent_start", "tool_start", "tool_end"]
        },
        "hostcall_log": []
    });

    let result = compare_conformance_outputs(&expected, &actual);
    assert_eq!(result.status, CompareStatus::Pass);
    assert!(result.registration_match);
}

#[test]
fn event_hooks_diff_detected() {
    let expected = serde_json::json!({
        "extension_id": "test",
        "registrations": {
            "commands": [], "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [],
            "event_hooks": ["tool_start", "tool_end"]
        },
        "hostcall_log": []
    });

    // Different hooks
    let actual = serde_json::json!({
        "extension_id": "test",
        "registrations": {
            "commands": [], "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [],
            "event_hooks": ["agent_start"]
        },
        "hostcall_log": []
    });

    let result = compare_conformance_outputs(&expected, &actual);
    assert_eq!(result.status, CompareStatus::Fail);
    assert!(!result.registration_match);
    assert!(result.diffs.iter().any(|d| d.category == "set_mismatch"));
}

#[test]
fn compare_result_serializes_to_json() {
    let result = CompareResult::pass();
    let json = serde_json::to_string_pretty(&result).expect("serialize");
    let parsed: CompareResult = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed.status, CompareStatus::Pass);
    assert!(parsed.diffs.is_empty());
}

#[test]
fn format_report_readable() {
    let result = CompareResult {
        status: CompareStatus::Fail,
        registration_match: false,
        hostcall_match: true,
        diffs: vec![DiffEntry {
            path: "registrations.commands[name=hello].description".to_string(),
            category: "value".to_string(),
            message: "value mismatch: \"Say hello\" vs \"Say goodbye\"".to_string(),
            expected: Some(Value::String("Say hello".to_string())),
            actual: Some(Value::String("Say goodbye".to_string())),
        }],
    };

    let report = format_compare_report(&result);
    assert!(report.contains("Fail"));
    assert!(report.contains("DIFFER"));
    assert!(report.contains("MATCH"));
    assert!(report.contains("registrations.commands"));
    assert!(report.contains("1 difference(s)"));
}

#[test]
fn full_complex_comparison_passes() {
    let output = serde_json::json!({
        "extension_id": "multi",
        "name": "multi-ext",
        "version": "1.0.0",
        "registrations": {
            "commands": [
                {"name": "cmd-a", "description": "A", "userFacing": true},
                {"name": "cmd-b", "description": "B", "userFacing": false}
            ],
            "shortcuts": [
                {"key_id": "ctrl+m", "description": "Multi shortcut"}
            ],
            "flags": [
                {"name": "verbose", "type": "boolean", "default": false},
                {"name": "format", "type": "string", "default": "json"}
            ],
            "providers": [
                {"id": "mock-provider", "baseUrl": "https://api.test/v1"}
            ],
            "tool_defs": [
                {"name": "hello", "description": "Greeting tool"}
            ],
            "models": [
                {"id": "fast", "contextWindow": 32000, "maxTokens": 4096}
            ],
            "event_hooks": ["tool_start", "tool_end"]
        },
        "hostcall_log": [
            {"op": "get_state", "result": {"sessionName": "test"}},
            {"op": "set_name", "payload": {"name": "new-name"}}
        ]
    });

    let result = compare_conformance_outputs(&output, &output);
    assert_eq!(result.status, CompareStatus::Pass);
    assert!(result.registration_match);
    assert!(result.hostcall_match);
    assert!(result.diffs.is_empty());
}

#[test]
fn empty_array_vs_null_treated_as_equal() {
    let expected = serde_json::json!({
        "extension_id": "test",
        "registrations": {
            "commands": [], "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": null
    });

    let actual = serde_json::json!({
        "extension_id": "test",
        "registrations": {
            "commands": [], "shortcuts": [], "flags": [], "providers": [],
            "tool_defs": [], "models": [], "event_hooks": []
        },
        "hostcall_log": []
    });

    let result = compare_conformance_outputs(&expected, &actual);
    assert_eq!(result.status, CompareStatus::Pass);
}
