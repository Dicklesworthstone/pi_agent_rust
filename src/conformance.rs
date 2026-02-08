//! Conformance utilities for fixture- and diff-based validation.
//!
//! This module is primarily intended for test harnesses that compare outputs
//! across runtimes (e.g., TS oracle vs Rust implementation) in a way that is
//! robust to irrelevant differences like ordering or float representation.
#![forbid(unsafe_code)]

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

const FLOAT_EPSILON: f64 = 1e-10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DiffKind {
    Root,
    Registration,
    Hostcall,
    Event,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffItem {
    kind: DiffKind,
    path: String,
    message: String,
}

impl DiffItem {
    fn new(kind: DiffKind, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind,
            path: path.into(),
            message: message.into(),
        }
    }
}

/// Compare two conformance outputs (TS vs Rust) with semantic rules.
///
/// Rules (high-level):
/// - Registration lists are compared ignoring ordering, keyed by their identity
///   field (e.g., command `name`, shortcut `key_id`).
/// - Hostcall logs are compared with order preserved.
/// - Objects are compared ignoring key ordering.
/// - Missing vs `null` is treated as equivalent.
/// - Missing vs empty array (`[]`) is treated as equivalent.
/// - Floats compare with epsilon (`1e-10`).
///
/// Returns `Ok(())` if semantically equal; otherwise returns a human-readable
/// diff report.
pub fn compare_conformance_output(expected: &Value, actual: &Value) -> Result<(), String> {
    let mut diffs = Vec::new();
    compare_conformance_output_inner(expected, actual, &mut diffs);
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(render_diffs(&diffs))
    }
}

fn compare_conformance_output_inner(expected: &Value, actual: &Value, diffs: &mut Vec<DiffItem>) {
    compare_string_field(expected, actual, "extension_id", DiffKind::Root, diffs);
    compare_string_field(expected, actual, "name", DiffKind::Root, diffs);
    compare_string_field(expected, actual, "version", DiffKind::Root, diffs);

    // Registrations
    let expected_regs = expected.get("registrations");
    let actual_regs = actual.get("registrations");
    compare_registrations(expected_regs, actual_regs, diffs);

    // Hostcall log (order matters)
    compare_hostcall_log(
        expected.get("hostcall_log"),
        actual.get("hostcall_log"),
        diffs,
    );

    // Optional: event results, if the runner includes them.
    compare_optional_semantic_value(
        expected.get("events"),
        actual.get("events"),
        "events",
        DiffKind::Event,
        diffs,
    );
}

fn compare_string_field(
    expected: &Value,
    actual: &Value,
    key: &str,
    kind: DiffKind,
    diffs: &mut Vec<DiffItem>,
) {
    let left = expected.get(key).and_then(Value::as_str);
    let right = actual.get(key).and_then(Value::as_str);
    if left != right {
        diffs.push(DiffItem::new(
            kind,
            key,
            format!("expected {left:?}, got {right:?}"),
        ));
    }
}

fn compare_registrations(
    expected: Option<&Value>,
    actual: Option<&Value>,
    diffs: &mut Vec<DiffItem>,
) {
    let expected = expected.unwrap_or(&Value::Null);
    let actual = actual.unwrap_or(&Value::Null);
    let Some(expected_obj) = expected.as_object() else {
        diffs.push(DiffItem::new(
            DiffKind::Registration,
            "registrations",
            "expected an object",
        ));
        return;
    };
    let Some(actual_obj) = actual.as_object() else {
        diffs.push(DiffItem::new(
            DiffKind::Registration,
            "registrations",
            "actual is not an object",
        ));
        return;
    };

    compare_keyed_registration_list(
        expected_obj.get("commands"),
        actual_obj.get("commands"),
        "name",
        "registrations.commands",
        diffs,
    );
    compare_keyed_registration_list(
        expected_obj.get("tool_defs"),
        actual_obj.get("tool_defs"),
        "name",
        "registrations.tool_defs",
        diffs,
    );
    compare_keyed_registration_list(
        expected_obj.get("flags"),
        actual_obj.get("flags"),
        "name",
        "registrations.flags",
        diffs,
    );
    compare_keyed_registration_list(
        expected_obj.get("providers"),
        actual_obj.get("providers"),
        "name",
        "registrations.providers",
        diffs,
    );
    compare_keyed_registration_list(
        expected_obj.get("shortcuts"),
        actual_obj.get("shortcuts"),
        "key_id",
        "registrations.shortcuts",
        diffs,
    );
    compare_keyed_registration_list(
        expected_obj.get("models"),
        actual_obj.get("models"),
        "id",
        "registrations.models",
        diffs,
    );

    // event_hooks: treat as set of strings.
    let expected_hooks = expected_obj.get("event_hooks");
    let actual_hooks = actual_obj.get("event_hooks");
    compare_string_set(
        expected_hooks,
        actual_hooks,
        "registrations.event_hooks",
        diffs,
    );
}

fn compare_keyed_registration_list(
    expected: Option<&Value>,
    actual: Option<&Value>,
    key_field: &str,
    path: &str,
    diffs: &mut Vec<DiffItem>,
) {
    let expected_items = value_as_array_or_empty(expected, path, diffs, DiffKind::Registration);
    let actual_items = value_as_array_or_empty(actual, path, diffs, DiffKind::Registration);

    let expected_map = index_by_string_key(&expected_items, key_field, path, diffs);
    let actual_map = index_by_string_key(&actual_items, key_field, path, diffs);

    let mut keys = BTreeSet::new();
    keys.extend(expected_map.keys().cloned());
    keys.extend(actual_map.keys().cloned());

    for key in keys {
        let expected_value = expected_map.get(&key);
        let actual_value = actual_map.get(&key);
        match (expected_value, actual_value) {
            (Some(_), None) => diffs.push(DiffItem::new(
                DiffKind::Registration,
                format!("{path}[{key_field}={key}]"),
                "missing in actual",
            )),
            (None, Some(_)) => diffs.push(DiffItem::new(
                DiffKind::Registration,
                format!("{path}[{key_field}={key}]"),
                "extra in actual",
            )),
            (Some(left), Some(right)) => {
                compare_semantic_value(
                    left,
                    right,
                    &format!("{path}[{key_field}={key}]"),
                    Some(key_field),
                    DiffKind::Registration,
                    diffs,
                );
            }
            (None, None) => {}
        }
    }
}

fn compare_string_set(
    expected: Option<&Value>,
    actual: Option<&Value>,
    path: &str,
    diffs: &mut Vec<DiffItem>,
) {
    let expected_items = value_as_array_or_empty(expected, path, diffs, DiffKind::Registration);
    let actual_items = value_as_array_or_empty(actual, path, diffs, DiffKind::Registration);

    let expected_set = expected_items
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    let actual_set = actual_items
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<BTreeSet<_>>();

    if expected_set == actual_set {
        return;
    }

    let missing = expected_set
        .difference(&actual_set)
        .cloned()
        .collect::<Vec<_>>();
    let extra = actual_set
        .difference(&expected_set)
        .cloned()
        .collect::<Vec<_>>();

    if !missing.is_empty() {
        diffs.push(DiffItem::new(
            DiffKind::Registration,
            path,
            format!("missing: {}", missing.join(", ")),
        ));
    }
    if !extra.is_empty() {
        diffs.push(DiffItem::new(
            DiffKind::Registration,
            path,
            format!("extra: {}", extra.join(", ")),
        ));
    }
}

fn compare_hostcall_log(
    expected: Option<&Value>,
    actual: Option<&Value>,
    diffs: &mut Vec<DiffItem>,
) {
    let path = "hostcall_log";
    let expected_items = value_as_array_or_empty(expected, path, diffs, DiffKind::Hostcall);
    let actual_items = value_as_array_or_empty(actual, path, diffs, DiffKind::Hostcall);

    if expected_items.len() != actual_items.len() {
        diffs.push(DiffItem::new(
            DiffKind::Hostcall,
            path,
            format!(
                "length mismatch: expected {}, got {}",
                expected_items.len(),
                actual_items.len()
            ),
        ));
    }

    let count = expected_items.len().min(actual_items.len());
    for idx in 0..count {
        let left = &expected_items[idx];
        let right = &actual_items[idx];
        compare_semantic_value(
            left,
            right,
            &format!("{path}[{idx}]"),
            None,
            DiffKind::Hostcall,
            diffs,
        );
    }
}

fn compare_optional_semantic_value(
    expected: Option<&Value>,
    actual: Option<&Value>,
    path: &str,
    kind: DiffKind,
    diffs: &mut Vec<DiffItem>,
) {
    if expected.is_none() && actual.is_none() {
        return;
    }
    let left = expected.unwrap_or(&Value::Null);
    let right = actual.unwrap_or(&Value::Null);
    compare_semantic_value(left, right, path, None, kind, diffs);
}

fn value_as_array_or_empty(
    value: Option<&Value>,
    path: &str,
    diffs: &mut Vec<DiffItem>,
    kind: DiffKind,
) -> Vec<Value> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(other) => {
            diffs.push(DiffItem::new(
                kind,
                path,
                format!("expected array, got {}", json_type_name(other)),
            ));
            Vec::new()
        }
    }
}

fn index_by_string_key(
    items: &[Value],
    key_field: &str,
    path: &str,
    diffs: &mut Vec<DiffItem>,
) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for (idx, item) in items.iter().enumerate() {
        let key = item
            .get(key_field)
            .and_then(Value::as_str)
            .map_or("", str::trim);
        if key.is_empty() {
            diffs.push(DiffItem::new(
                DiffKind::Registration,
                format!("{path}[{idx}]"),
                format!("missing string key field {key_field:?}"),
            ));
            continue;
        }
        if out.contains_key(key) {
            diffs.push(DiffItem::new(
                DiffKind::Registration,
                format!("{path}[{key_field}={key}]"),
                "duplicate key",
            ));
            continue;
        }
        out.insert(key.to_string(), item.clone());
    }
    out
}

fn compare_semantic_value(
    expected: &Value,
    actual: &Value,
    path: &str,
    parent_key: Option<&str>,
    kind: DiffKind,
    diffs: &mut Vec<DiffItem>,
) {
    // Missing vs null / empty array equivalence is handled at object-key union sites.

    match (expected, actual) {
        (Value::Null, Value::Null) => {}
        (Value::Bool(left), Value::Bool(right)) => {
            if left != right {
                diffs.push(DiffItem::new(kind, path, format!("{left} != {right}")));
            }
        }
        (Value::Number(left), Value::Number(right)) => {
            if !numbers_equal(left, right) {
                diffs.push(DiffItem::new(
                    kind,
                    path,
                    format!("expected {left}, got {right}"),
                ));
            }
        }
        (Value::String(left), Value::String(right)) => {
            if left != right {
                diffs.push(DiffItem::new(
                    kind,
                    path,
                    format!("expected {left:?}, got {right:?}"),
                ));
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            if array_order_insensitive(parent_key) {
                compare_unordered_array(left, right, path, kind, diffs);
            } else {
                compare_ordered_array(left, right, path, kind, diffs);
            }
        }
        (Value::Object(left), Value::Object(right)) => {
            let mut keys = BTreeSet::new();
            keys.extend(left.keys().cloned());
            keys.extend(right.keys().cloned());

            for key in keys {
                let left_value = left.get(&key);
                let right_value = right.get(&key);
                if missing_equals_null_or_empty_array(left_value, right_value) {
                    continue;
                }
                let left_value = left_value.unwrap_or(&Value::Null);
                let right_value = right_value.unwrap_or(&Value::Null);
                compare_semantic_value(
                    left_value,
                    right_value,
                    &format!("{path}.{key}"),
                    Some(key.as_str()),
                    kind,
                    diffs,
                );
            }
        }
        _ => {
            if missing_equals_null_or_empty_array(Some(expected), Some(actual)) {
                return;
            }
            diffs.push(DiffItem::new(
                kind,
                path,
                format!(
                    "type mismatch: expected {}, got {}",
                    json_type_name(expected),
                    json_type_name(actual)
                ),
            ));
        }
    }
}

fn compare_ordered_array(
    expected: &[Value],
    actual: &[Value],
    path: &str,
    kind: DiffKind,
    diffs: &mut Vec<DiffItem>,
) {
    if expected.len() != actual.len() {
        diffs.push(DiffItem::new(
            kind,
            path,
            format!(
                "length mismatch: expected {}, got {}",
                expected.len(),
                actual.len()
            ),
        ));
    }
    let count = expected.len().min(actual.len());
    for idx in 0..count {
        compare_semantic_value(
            &expected[idx],
            &actual[idx],
            &format!("{path}[{idx}]"),
            None,
            kind,
            diffs,
        );
    }
}

fn compare_unordered_array(
    expected: &[Value],
    actual: &[Value],
    path: &str,
    kind: DiffKind,
    diffs: &mut Vec<DiffItem>,
) {
    let mut left = expected.to_vec();
    let mut right = actual.to_vec();
    left.sort_by_key(stable_value_key);
    right.sort_by_key(stable_value_key);
    compare_ordered_array(&left, &right, path, kind, diffs);
}

fn stable_value_key(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(v) => format!("bool:{v}"),
        Value::Number(v) => format!("num:{v}"),
        Value::String(v) => format!("str:{v}"),
        Value::Array(items) => {
            let mut out = String::new();
            out.push_str("arr:[");
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                out.push_str(&stable_value_key(item));
            }
            out.push(']');
            out
        }
        Value::Object(map) => {
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            let mut out = String::new();
            out.push_str("obj:{");
            for key in keys {
                out.push_str(&key);
                out.push('=');
                if let Some(value) = map.get(&key) {
                    out.push_str(&stable_value_key(value));
                }
                out.push(';');
            }
            out.push('}');
            out
        }
    }
}

fn array_order_insensitive(parent_key: Option<&str>) -> bool {
    matches!(parent_key, Some("required" | "input" | "event_hooks"))
}

fn missing_equals_null_or_empty_array(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (left, right) {
        (None | Some(Value::Null), None) | (None, Some(Value::Null)) => true,
        (None, Some(Value::Array(items))) | (Some(Value::Array(items)), None) => items.is_empty(),
        _ => false,
    }
}

fn numbers_equal(left: &serde_json::Number, right: &serde_json::Number) -> bool {
    if left == right {
        return true;
    }
    let left = left.as_f64();
    let right = right.as_f64();
    match (left, right) {
        (Some(left), Some(right)) => (left - right).abs() <= FLOAT_EPSILON + f64::EPSILON,
        _ => false,
    }
}

const fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn render_diffs(diffs: &[DiffItem]) -> String {
    let mut grouped: BTreeMap<DiffKind, Vec<&DiffItem>> = BTreeMap::new();
    for diff in diffs {
        grouped.entry(diff.kind).or_default().push(diff);
    }

    let mut out = String::new();
    for (kind, items) in grouped {
        let header = match kind {
            DiffKind::Root => "ROOT",
            DiffKind::Registration => "REGISTRATION",
            DiffKind::Hostcall => "HOSTCALL",
            DiffKind::Event => "EVENT",
        };
        let _ = writeln!(out, "== {header} DIFFS ==");
        for item in items {
            let _ = writeln!(out, "- {}: {}", item.path, item.message);
        }
        out.push('\n');
    }
    out
}

// ============================================================================
// Conformance Report Generation (bd-2jha)
// ============================================================================

pub mod report {
    use chrono::{SecondsFormat, Utc};
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;
    use std::fmt::Write as _;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ConformanceStatus {
        Pass,
        Fail,
        Skip,
        Error,
    }

    impl ConformanceStatus {
        #[must_use]
        pub const fn as_upper_str(self) -> &'static str {
            match self {
                Self::Pass => "PASS",
                Self::Fail => "FAIL",
                Self::Skip => "SKIP",
                Self::Error => "ERROR",
            }
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ConformanceDiffEntry {
        pub category: String,
        pub path: String,
        pub message: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ExtensionConformanceResult {
        pub id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tier: Option<u32>,
        pub status: ConformanceStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ts_time_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub rust_time_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub diffs: Vec<ConformanceDiffEntry>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub notes: Option<String>,
    }

    fn ratio(passed: u64, total: u64) -> f64 {
        if total == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                passed as f64 / total as f64
            }
        }
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct TierSummary {
        pub total: u64,
        pub passed: u64,
        pub failed: u64,
        pub skipped: u64,
        pub errors: u64,
        pub pass_rate: f64,
    }

    impl TierSummary {
        fn from_results(results: &[ExtensionConformanceResult]) -> Self {
            let total = results.len() as u64;
            let passed = results
                .iter()
                .filter(|r| r.status == ConformanceStatus::Pass)
                .count() as u64;
            let failed = results
                .iter()
                .filter(|r| r.status == ConformanceStatus::Fail)
                .count() as u64;
            let skipped = results
                .iter()
                .filter(|r| r.status == ConformanceStatus::Skip)
                .count() as u64;
            let errors = results
                .iter()
                .filter(|r| r.status == ConformanceStatus::Error)
                .count() as u64;

            let pass_rate = ratio(passed, total);

            Self {
                total,
                passed,
                failed,
                skipped,
                errors,
                pass_rate,
            }
        }
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ConformanceSummary {
        pub total: u64,
        pub passed: u64,
        pub failed: u64,
        pub skipped: u64,
        pub errors: u64,
        pub pass_rate: f64,
        pub by_tier: BTreeMap<String, TierSummary>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ConformanceReport {
        pub run_id: String,
        pub timestamp: String,
        pub summary: ConformanceSummary,
        pub extensions: Vec<ExtensionConformanceResult>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ExtensionRegression {
        pub id: String,
        pub previous: ConformanceStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub current: Option<ConformanceStatus>,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ConformanceRegression {
        /// Number of extensions compared for pass-rate deltas.
        pub compared_total: u64,
        pub previous_passed: u64,
        pub current_passed: u64,
        pub previous_pass_rate: f64,
        pub current_pass_rate: f64,
        pub pass_rate_delta: f64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub regressed_extensions: Vec<ExtensionRegression>,
    }

    impl ConformanceRegression {
        #[must_use]
        pub fn has_regression(&self) -> bool {
            const EPS: f64 = 1e-12;
            self.pass_rate_delta < -EPS || !self.regressed_extensions.is_empty()
        }
    }

    fn tier_key(tier: Option<u32>) -> String {
        tier.map_or_else(|| "tier_unknown".to_string(), |tier| format!("tier{tier}"))
    }

    fn now_timestamp_string() -> String {
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    /// Build a report from per-extension results.
    ///
    /// - `timestamp` defaults to `Utc::now()` if `None`.
    /// - Results are sorted by (tier, id) for deterministic output.
    #[must_use]
    pub fn generate_report(
        run_id: impl Into<String>,
        timestamp: Option<String>,
        mut results: Vec<ExtensionConformanceResult>,
    ) -> ConformanceReport {
        results.sort_by(|left, right| {
            let left_tier = left.tier.unwrap_or(u32::MAX);
            let right_tier = right.tier.unwrap_or(u32::MAX);
            (left_tier, &left.id).cmp(&(right_tier, &right.id))
        });

        let total = results.len() as u64;
        let passed = results
            .iter()
            .filter(|r| r.status == ConformanceStatus::Pass)
            .count() as u64;
        let failed = results
            .iter()
            .filter(|r| r.status == ConformanceStatus::Fail)
            .count() as u64;
        let skipped = results
            .iter()
            .filter(|r| r.status == ConformanceStatus::Skip)
            .count() as u64;
        let errors = results
            .iter()
            .filter(|r| r.status == ConformanceStatus::Error)
            .count() as u64;

        let pass_rate = ratio(passed, total);

        let mut by_tier: BTreeMap<String, Vec<ExtensionConformanceResult>> = BTreeMap::new();
        for result in &results {
            by_tier
                .entry(tier_key(result.tier))
                .or_default()
                .push(result.clone());
        }

        let by_tier = by_tier
            .into_iter()
            .map(|(key, items)| (key, TierSummary::from_results(&items)))
            .collect::<BTreeMap<_, _>>();

        ConformanceReport {
            run_id: run_id.into(),
            timestamp: timestamp.unwrap_or_else(now_timestamp_string),
            summary: ConformanceSummary {
                total,
                passed,
                failed,
                skipped,
                errors,
                pass_rate,
                by_tier,
            },
            extensions: results,
        }
    }

    /// Compute regression signals between a previous and current report.
    ///
    /// Semantics:
    /// - Pass-rate deltas are computed over the *previous* extension set only, so that
    ///   newly-added extensions do not count as regressions.
    /// - An extension regresses if it was `PASS` previously and is now non-`PASS` (or
    ///   missing).
    #[must_use]
    pub fn compute_regression(
        previous: &ConformanceReport,
        current: &ConformanceReport,
    ) -> ConformanceRegression {
        let compared_total = previous.extensions.len() as u64;
        let previous_passed = previous
            .extensions
            .iter()
            .filter(|r| r.status == ConformanceStatus::Pass)
            .count() as u64;

        let current_by_id = current
            .extensions
            .iter()
            .map(|r| (r.id.as_str(), r.status))
            .collect::<BTreeMap<_, _>>();

        let mut current_passed = 0u64;
        let mut regressed_extensions = Vec::new();
        for result in &previous.extensions {
            let current_status = current_by_id.get(result.id.as_str()).copied();
            if matches!(current_status, Some(ConformanceStatus::Pass)) {
                current_passed = current_passed.saturating_add(1);
            }

            if result.status == ConformanceStatus::Pass
                && !matches!(current_status, Some(ConformanceStatus::Pass))
            {
                regressed_extensions.push(ExtensionRegression {
                    id: result.id.clone(),
                    previous: result.status,
                    current: current_status,
                });
            }
        }

        let previous_pass_rate = ratio(previous_passed, compared_total);
        let current_pass_rate = ratio(current_passed, compared_total);
        let pass_rate_delta = current_pass_rate - previous_pass_rate;

        ConformanceRegression {
            compared_total,
            previous_passed,
            current_passed,
            previous_pass_rate,
            current_pass_rate,
            pass_rate_delta,
            regressed_extensions,
        }
    }

    impl ConformanceReport {
        /// Render a human-readable Markdown report.
        #[must_use]
        pub fn render_markdown(&self) -> String {
            let mut out = String::new();
            let pass_rate_pct = self.summary.pass_rate * 100.0;
            let _ = writeln!(out, "# Extension Conformance Report");
            let _ = writeln!(out, "Generated: {}", self.timestamp);
            let _ = writeln!(out, "Run ID: {}", self.run_id);
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "Pass Rate: {:.1}% ({}/{})",
                pass_rate_pct, self.summary.passed, self.summary.total
            );
            let _ = writeln!(out);
            let _ = writeln!(out, "## Summary");
            let _ = writeln!(out, "- Total: {}", self.summary.total);
            let _ = writeln!(out, "- Passed: {}", self.summary.passed);
            let _ = writeln!(out, "- Failed: {}", self.summary.failed);
            let _ = writeln!(out, "- Skipped: {}", self.summary.skipped);
            let _ = writeln!(out, "- Errors: {}", self.summary.errors);
            let _ = writeln!(out);

            let _ = writeln!(out, "## By Tier");
            for (tier, summary) in &self.summary.by_tier {
                let tier_label = match tier.strip_prefix("tier") {
                    Some(num) if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) => {
                        format!("Tier {num}")
                    }
                    _ => tier.clone(),
                };
                let _ = writeln!(
                    out,
                    "### {tier_label}: {:.1}% ({}/{})",
                    summary.pass_rate * 100.0,
                    summary.passed,
                    summary.total
                );
                let _ = writeln!(out);
                let _ = writeln!(out, "| Extension | Status | TS Time | Rust Time | Notes |");
                let _ = writeln!(out, "|---|---|---:|---:|---|");
                for result in self.extensions.iter().filter(|r| &tier_key(r.tier) == tier) {
                    let ts_time = result
                        .ts_time_ms
                        .map_or_else(String::new, |v| format!("{v}ms"));
                    let rust_time = result
                        .rust_time_ms
                        .map_or_else(String::new, |v| format!("{v}ms"));
                    let notes = result.notes.as_deref().unwrap_or("");
                    let _ = writeln!(
                        out,
                        "| {} | {} | {} | {} | {} |",
                        result.id,
                        result.status.as_upper_str(),
                        ts_time,
                        rust_time,
                        notes
                    );
                }
                let _ = writeln!(out);
            }

            let failures = self
                .extensions
                .iter()
                .filter(|r| matches!(r.status, ConformanceStatus::Fail | ConformanceStatus::Error))
                .collect::<Vec<_>>();

            let _ = writeln!(out, "## Failures");
            if failures.is_empty() {
                let _ = writeln!(out, "(none)");
                return out;
            }

            for failure in failures {
                let tier = failure
                    .tier
                    .map_or_else(|| "unknown".to_string(), |v| v.to_string());
                let _ = writeln!(out, "### {} (Tier {})", failure.id, tier);
                if let Some(notes) = failure.notes.as_deref().filter(|v| !v.is_empty()) {
                    let _ = writeln!(out, "**Notes**: {notes}");
                }
                if failure.diffs.is_empty() {
                    let _ = writeln!(out, "- (no diff details)");
                } else {
                    for diff in &failure.diffs {
                        let _ = writeln!(out, "- `{}`: {}", diff.path, diff.message);
                    }
                }
                let _ = writeln!(out);
            }

            out
        }
    }
}

// ============================================================================
// Snapshot Protocol (bd-1pqf)
// ============================================================================

/// Snapshot protocol for extension conformance artifacts.
///
/// This module codifies the canonical folder layout, naming conventions,
/// metadata requirements, and integrity checks that all extension artifacts
/// must satisfy.  Acquisition tasks (templates, GitHub releases, npm tarballs)
/// **MUST** use this protocol so that the conformance test infrastructure can
/// discover and verify artifacts automatically.
///
/// # Folder layout
///
/// ```text
/// tests/ext_conformance/artifacts/
/// ├── <official-extension-id>/         ← top-level = official-pi-mono
/// ├── community/<extension-id>/        ← community tier
/// ├── npm/<extension-id>/              ← npm-registry tier
/// ├── third-party/<extension-id>/      ← third-party-github tier
/// ├── agents-mikeastock/<id>/          ← agents-mikeastock tier
/// ├── templates/<id>/                  ← templates tier (future)
/// ├── CATALOG.json                     ← quick-reference metadata
/// ├── SHA256SUMS.txt                   ← per-file checksums
/// └── (test fixture dirs excluded)
/// ```
///
/// # Naming conventions
///
/// - Extension IDs: lowercase ASCII, digits, hyphens.  No spaces, underscores
///   or uppercase.  Forward slashes allowed only for tier-scoped paths
///   (e.g. `community/my-ext`).
/// - Directory name == extension ID (within its tier prefix).
///
/// # Integrity
///
/// Every artifact must have a deterministic SHA-256 directory digest computed
/// by [`digest_artifact_dir`].  This digest is stored in both
/// `extension-master-catalog.json` and `extension-artifact-provenance.json`.
pub mod snapshot {
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    use std::io;
    use std::path::{Path, PathBuf};

    // === Layout constants ===

    /// Root directory for all extension artifacts (relative to repo root).
    pub const ARTIFACT_ROOT: &str = "tests/ext_conformance/artifacts";

    /// Reserved tier-scoped subdirectories.  Extensions placed under these
    /// directories use `<tier>/<extension-id>/` layout.
    pub const TIER_SCOPED_DIRS: &[&str] = &[
        "community",
        "npm",
        "third-party",
        "agents-mikeastock",
        "templates",
    ];

    /// Directories excluded from conformance testing entirely.
    pub const EXCLUDED_DIRS: &[&str] = &[
        "plugins-official",
        "plugins-community",
        "plugins-ariff",
        "agents-wshobson",
        "templates-davila7",
    ];

    /// Non-extension directories that contain test fixtures, not artifacts.
    pub const FIXTURE_DIRS: &[&str] = &[
        "base_fixtures",
        "diff",
        "files",
        "negative-denied-caps",
        "reports",
    ];

    // === Source tier ===

    /// Classification of where an extension artifact was obtained.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum SourceTier {
        OfficialPiMono,
        Community,
        NpmRegistry,
        ThirdPartyGithub,
        AgentsMikeastock,
        Templates,
    }

    impl SourceTier {
        /// Directory prefix for this tier (`None` means top-level / official).
        #[must_use]
        pub const fn directory_prefix(self) -> Option<&'static str> {
            match self {
                Self::OfficialPiMono => None,
                Self::Community => Some("community"),
                Self::NpmRegistry => Some("npm"),
                Self::ThirdPartyGithub => Some("third-party"),
                Self::AgentsMikeastock => Some("agents-mikeastock"),
                Self::Templates => Some("templates"),
            }
        }

        /// Determine tier from a directory path relative to the artifact root.
        #[must_use]
        pub fn from_directory(dir: &str) -> Self {
            if dir.starts_with("community/") {
                Self::Community
            } else if dir.starts_with("npm/") {
                Self::NpmRegistry
            } else if dir.starts_with("third-party/") {
                Self::ThirdPartyGithub
            } else if dir.starts_with("agents-mikeastock/") {
                Self::AgentsMikeastock
            } else if dir.starts_with("templates/") {
                Self::Templates
            } else {
                Self::OfficialPiMono
            }
        }
    }

    // === Artifact source ===

    /// Where an artifact was obtained from.  Stored in the provenance manifest.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case", tag = "type")]
    pub enum ArtifactSource {
        /// Cloned from a git repository.
        Git {
            repo: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            path: Option<String>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            commit: Option<String>,
        },
        /// Downloaded from the npm registry.
        Npm {
            package: String,
            version: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            url: Option<String>,
        },
        /// Downloaded from a direct URL.
        Url { url: String },
    }

    // === Artifact spec ===

    /// Specification for a new artifact to be added to the conformance corpus.
    ///
    /// Acquisition tasks construct an `ArtifactSpec`, validate it with
    /// [`validate_artifact_spec`], write the files, then compute and record
    /// the directory digest.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ArtifactSpec {
        /// Unique extension ID (lowercase, hyphens, digits).
        pub id: String,
        /// Relative directory under the artifact root.
        pub directory: String,
        /// Human-readable name.
        pub name: String,
        /// Source tier classification.
        pub source_tier: SourceTier,
        /// License identifier (SPDX short form, or `"UNKNOWN"`).
        pub license: String,
        /// Where the artifact was obtained from.
        pub source: ArtifactSource,
    }

    // === Naming validation ===

    /// Validate that an extension ID follows naming conventions.
    ///
    /// Rules:
    /// - Non-empty
    /// - Lowercase ASCII letters, digits, hyphens, forward slashes
    /// - Must not start or end with a hyphen
    pub fn validate_id(id: &str) -> Result<(), String> {
        if id.is_empty() {
            return Err("extension ID must not be empty".into());
        }
        if id != id.to_ascii_lowercase() {
            return Err(format!("extension ID must be lowercase: {id:?}"));
        }
        for ch in id.chars() {
            if !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && ch != '-' && ch != '/' {
                return Err(format!(
                    "extension ID contains invalid character {ch:?}: {id:?}"
                ));
            }
        }
        if id.starts_with('-') || id.ends_with('-') {
            return Err(format!(
                "extension ID must not start or end with hyphen: {id:?}"
            ));
        }
        Ok(())
    }

    /// Validate that a directory follows layout conventions for the given tier.
    pub fn validate_directory(dir: &str, tier: SourceTier) -> Result<(), String> {
        if dir.is_empty() {
            return Err("directory must not be empty".into());
        }
        match tier.directory_prefix() {
            Some(prefix) => {
                if !dir.starts_with(&format!("{prefix}/")) {
                    return Err(format!(
                        "directory {dir:?} must start with \"{prefix}/\" for tier {tier:?}"
                    ));
                }
            }
            None => {
                for scoped in TIER_SCOPED_DIRS {
                    if dir.starts_with(&format!("{scoped}/")) {
                        return Err(format!(
                            "official extension directory {dir:?} must not be under \
                             tier-scoped dir \"{scoped}/\""
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    // === Integrity ===

    fn hex_lower(bytes: &[u8]) -> String {
        let mut output = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            let _ = write!(&mut output, "{byte:02x}");
        }
        output
    }

    fn collect_files_recursive(dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            let path = entry.path();
            if ft.is_dir() {
                collect_files_recursive(&path, files)?;
            } else if ft.is_file() {
                files.push(path);
            }
        }
        Ok(())
    }

    fn relative_posix(root: &Path, path: &Path) -> String {
        let rel = path.strip_prefix(root).unwrap_or(path);
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }

    /// Compute a deterministic SHA-256 digest of an artifact directory.
    ///
    /// Algorithm:
    /// 1. Recursively collect all regular files.
    /// 2. Sort by POSIX relative path (ensures cross-platform determinism).
    /// 3. For each file, feed `"file\0"`, relative path, `"\0"`, file bytes,
    ///    `"\0"` into the hasher.
    /// 4. Return lowercase hex digest (64 chars).
    pub fn digest_artifact_dir(dir: &Path) -> io::Result<String> {
        let mut files = Vec::new();
        collect_files_recursive(dir, &mut files)?;
        files.sort_by_key(|p| relative_posix(dir, p));

        let mut hasher = Sha256::new();
        for path in &files {
            let rel = relative_posix(dir, path);
            hasher.update(b"file\0");
            hasher.update(rel.as_bytes());
            hasher.update(b"\0");
            hasher.update(&std::fs::read(path)?);
            hasher.update(b"\0");
        }
        Ok(hex_lower(&hasher.finalize()))
    }

    /// Verify an artifact directory's checksum matches an expected value.
    ///
    /// Returns `Ok(Ok(()))` if the checksum matches, `Ok(Err(msg))` on
    /// mismatch, or `Err(io_err)` if the directory cannot be read.
    pub fn verify_integrity(dir: &Path, expected_sha256: &str) -> io::Result<Result<(), String>> {
        let actual = digest_artifact_dir(dir)?;
        if actual == expected_sha256 {
            Ok(Ok(()))
        } else {
            Ok(Err(format!(
                "checksum mismatch for {}: expected {expected_sha256}, got {actual}",
                dir.display()
            )))
        }
    }

    /// Validate a new artifact spec against all protocol rules.
    ///
    /// Returns an empty vec on success, or a list of human-readable errors.
    #[must_use]
    pub fn validate_artifact_spec(spec: &ArtifactSpec) -> Vec<String> {
        let mut errors = Vec::new();

        if let Err(e) = validate_id(&spec.id) {
            errors.push(e);
        }
        if let Err(e) = validate_directory(&spec.directory, spec.source_tier) {
            errors.push(e);
        }
        if spec.name.is_empty() {
            errors.push("name must not be empty".into());
        }
        if spec.license.is_empty() {
            errors.push("license must not be empty (use \"UNKNOWN\" if unknown)".into());
        }

        match &spec.source {
            ArtifactSource::Git { repo, .. } => {
                if repo.is_empty() {
                    errors.push("git source must have non-empty repo URL".into());
                }
            }
            ArtifactSource::Npm {
                package, version, ..
            } => {
                if package.is_empty() {
                    errors.push("npm source must have non-empty package name".into());
                }
                if version.is_empty() {
                    errors.push("npm source must have non-empty version".into());
                }
            }
            ArtifactSource::Url { url } => {
                if url.is_empty() {
                    errors.push("url source must have non-empty URL".into());
                }
            }
        }

        errors
    }

    /// Check whether a directory name is reserved (excluded, fixture, or
    /// tier-scoped) and therefore not a direct extension directory.
    #[must_use]
    pub fn is_reserved_dir(name: &str) -> bool {
        EXCLUDED_DIRS.contains(&name)
            || FIXTURE_DIRS.contains(&name)
            || TIER_SCOPED_DIRS.contains(&name)
    }
}

// ============================================================================
// Normalization Contract (bd-k5q5.1.1)
// ============================================================================

/// Canonical event schema and normalization contract for conformance testing.
///
/// This module formally defines which fields in conformance events are
/// **semantic** (must match across runtimes), **transport** (non-deterministic
/// noise that must be normalized before comparison), or **derived**
/// (computed from other fields and ignored during comparison).
///
/// # Schema version
///
/// The schema is versioned so that changes to normalization rules can be
/// tracked.  Fixtures and baselines record which schema version they were
/// generated against.
///
/// # Usage
///
/// ```rust,ignore
/// use pi::conformance::normalization::*;
///
/// let contract = NormalizationContract::default();
/// let ctx = NormalizationContext::from_cwd(std::path::Path::new("/tmp"));
/// let mut event: serde_json::Value = serde_json::from_str(raw_line)?;
/// contract.normalize(&mut event, &ctx);
/// ```
pub mod normalization {
    use regex::Regex;
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    // ── Schema version ─────────────────────────────────────────────────

    /// Current schema version.  Bump when normalization rules change.
    pub const SCHEMA_VERSION: &str = "1.0.0";

    // ── Placeholder constants ──────────────────────────────────────────
    //
    // Canonical placeholder strings used to replace transport-noise values.
    // These were previously scattered in `tests/ext_conformance.rs`; now
    // they live in the library so both test code and CI tooling share one
    // source of truth.

    pub const PLACEHOLDER_TIMESTAMP: &str = "<TIMESTAMP>";
    pub const PLACEHOLDER_HOST: &str = "<HOST>";
    pub const PLACEHOLDER_SESSION_ID: &str = "<SESSION_ID>";
    pub const PLACEHOLDER_RUN_ID: &str = "<RUN_ID>";
    pub const PLACEHOLDER_ARTIFACT_ID: &str = "<ARTIFACT_ID>";
    pub const PLACEHOLDER_TRACE_ID: &str = "<TRACE_ID>";
    pub const PLACEHOLDER_SPAN_ID: &str = "<SPAN_ID>";
    pub const PLACEHOLDER_UUID: &str = "<UUID>";
    pub const PLACEHOLDER_PI_MONO_ROOT: &str = "<PI_MONO_ROOT>";
    pub const PLACEHOLDER_PROJECT_ROOT: &str = "<PROJECT_ROOT>";
    pub const PLACEHOLDER_PORT: &str = "<PORT>";
    pub const PLACEHOLDER_PID: &str = "<PID>";

    // ── Field classification ───────────────────────────────────────────

    /// How a conformance event field is treated during comparison.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum FieldClassification {
        /// Field carries meaning that MUST match between TS and Rust runtimes.
        ///
        /// Examples: `extension_id`, `event`, `level`, `schema`,
        /// registration contents, hostcall payloads.
        Semantic,

        /// Field is non-deterministic transport noise that MUST be normalized
        /// (replaced with a placeholder) before comparison.
        ///
        /// Examples: timestamps, PIDs, session IDs, UUIDs, absolute paths,
        /// ANSI escape sequences, hostnames, port numbers.
        Transport,

        /// Field is derived from other fields and is skipped during comparison.
        ///
        /// Examples: computed durations, cache keys, internal sequence numbers.
        Derived,
    }

    /// Describes a single field's normalization rule.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct FieldRule {
        /// JSON path pattern (dot-separated, `*` for any key at that level).
        ///
        /// Examples: `"ts"`, `"correlation.session_id"`, `"source.pid"`,
        /// `"*.timestamp"` (matches `timestamp` at any depth).
        pub path_pattern: String,

        /// Classification for this field.
        pub classification: FieldClassification,

        /// Placeholder to substitute for transport fields.
        ///
        /// Only meaningful when `classification == Transport`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub placeholder: Option<String>,
    }

    /// Describes a string-level rewrite applied to all string values.
    #[derive(Debug, Clone)]
    pub struct StringRewriteRule {
        /// Human-readable name for this rule.
        pub name: &'static str,
        /// Regex pattern to match within string values.
        pub regex: &'static OnceLock<Regex>,
        /// Replacement string (may contain `$1` etc. for captures).
        pub replacement: &'static str,
    }

    // ── Normalization context ──────────────────────────────────────────

    /// Environment-specific values needed for path canonicalization.
    ///
    /// Promoted from `tests/ext_conformance.rs` to the library so that both
    /// test code, CI tooling, and future replay infrastructure share one
    /// implementation.
    #[derive(Debug, Clone)]
    pub struct NormalizationContext {
        /// Absolute path to the pi_agent_rust repository root.
        pub project_root: String,
        /// Absolute path to `legacy_pi_mono_code/pi-mono`.
        pub pi_mono_root: String,
        /// Working directory used during the conformance run.
        pub cwd: String,
    }

    impl NormalizationContext {
        /// Build from a working directory, auto-detecting project roots.
        #[must_use]
        pub fn from_cwd(cwd: &Path) -> Self {
            let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
                .display()
                .to_string();
            let pi_mono_root = PathBuf::from(&project_root)
                .join("legacy_pi_mono_code")
                .join("pi-mono")
                .canonicalize()
                .unwrap_or_else(|_| {
                    PathBuf::from(&project_root)
                        .join("legacy_pi_mono_code")
                        .join("pi-mono")
                })
                .display()
                .to_string();
            let cwd = cwd
                .canonicalize()
                .unwrap_or_else(|_| cwd.to_path_buf())
                .display()
                .to_string();
            Self {
                project_root,
                pi_mono_root,
                cwd,
            }
        }

        /// Build with explicit paths (for deterministic testing).
        #[must_use]
        pub const fn new(project_root: String, pi_mono_root: String, cwd: String) -> Self {
            Self {
                project_root,
                pi_mono_root,
                cwd,
            }
        }
    }

    // ── Lazy-initialized regexes ───────────────────────────────────────

    static ANSI_REGEX: OnceLock<Regex> = OnceLock::new();
    static RUN_ID_REGEX: OnceLock<Regex> = OnceLock::new();
    static UUID_REGEX: OnceLock<Regex> = OnceLock::new();
    static OPENAI_BASE_REGEX: OnceLock<Regex> = OnceLock::new();

    fn ansi_regex() -> &'static Regex {
        ANSI_REGEX.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").expect("ansi regex"))
    }

    fn run_id_regex() -> &'static Regex {
        RUN_ID_REGEX.get_or_init(|| Regex::new(r"\brun-[0-9a-fA-F-]{36}\b").expect("run id regex"))
    }

    fn uuid_regex() -> &'static Regex {
        UUID_REGEX.get_or_init(|| {
            Regex::new(
                r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
            )
            .expect("uuid regex")
        })
    }

    fn openai_base_regex() -> &'static Regex {
        OPENAI_BASE_REGEX
            .get_or_init(|| Regex::new(r"http://127\.0\.0\.1:\d+/v1").expect("openai base regex"))
    }

    // ── Key-name classification ────────────────────────────────────────
    //
    // The canonical set of JSON key names whose *presence* determines
    // transport classification.  This replaces the ad-hoc `matches!` chains
    // that were scattered in the test file.

    /// Key names that indicate a timestamp (string → placeholder, number → 0).
    const TIMESTAMP_KEYS: &[&str] = &[
        "timestamp",
        "started_at",
        "finished_at",
        "created_at",
        "createdAt",
        "ts",
    ];

    /// Key names for string-valued transport IDs.
    const TRANSPORT_ID_KEYS: &[(&str, &str)] = &[
        ("session_id", PLACEHOLDER_SESSION_ID),
        ("sessionId", PLACEHOLDER_SESSION_ID),
        ("run_id", PLACEHOLDER_RUN_ID),
        ("runId", PLACEHOLDER_RUN_ID),
        ("artifact_id", PLACEHOLDER_ARTIFACT_ID),
        ("artifactId", PLACEHOLDER_ARTIFACT_ID),
        ("trace_id", PLACEHOLDER_TRACE_ID),
        ("traceId", PLACEHOLDER_TRACE_ID),
        ("span_id", PLACEHOLDER_SPAN_ID),
        ("spanId", PLACEHOLDER_SPAN_ID),
    ];

    /// Key names replaced unconditionally with a fixed placeholder.
    const FIXED_PLACEHOLDER_KEYS: &[(&str, &str)] = &[
        ("cwd", PLACEHOLDER_PI_MONO_ROOT),
        ("host", PLACEHOLDER_HOST),
    ];

    /// Numeric key names that are zeroed out.
    const ZEROED_NUMBER_KEYS: &[&str] = &["pid"];

    // ── Normalization contract ──────────────────────────────────────────

    /// The canonical normalization contract.
    ///
    /// Encapsulates all rules needed to transform a raw conformance event
    /// into its normalized form suitable for comparison.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct NormalizationContract {
        /// Schema version this contract was defined against.
        pub schema_version: String,
        /// Field-level rules (by key name).
        pub field_rules: Vec<FieldRule>,
    }

    impl Default for NormalizationContract {
        fn default() -> Self {
            let mut field_rules = Vec::new();

            // Timestamp fields → Transport
            for &key in TIMESTAMP_KEYS {
                field_rules.push(FieldRule {
                    path_pattern: format!("*.{key}"),
                    classification: FieldClassification::Transport,
                    placeholder: Some(PLACEHOLDER_TIMESTAMP.to_string()),
                });
            }

            // Transport ID fields
            for &(key, placeholder) in TRANSPORT_ID_KEYS {
                field_rules.push(FieldRule {
                    path_pattern: format!("*.{key}"),
                    classification: FieldClassification::Transport,
                    placeholder: Some(placeholder.to_string()),
                });
            }

            // Fixed placeholder fields
            for &(key, placeholder) in FIXED_PLACEHOLDER_KEYS {
                field_rules.push(FieldRule {
                    path_pattern: format!("*.{key}"),
                    classification: FieldClassification::Transport,
                    placeholder: Some(placeholder.to_string()),
                });
            }

            // Numeric transport fields
            for &key in ZEROED_NUMBER_KEYS {
                field_rules.push(FieldRule {
                    path_pattern: format!("*.{key}"),
                    classification: FieldClassification::Transport,
                    placeholder: Some("0".to_string()),
                });
            }

            // Semantic fields (documented for downstream consumers)
            for key in &[
                "schema",
                "level",
                "event",
                "message",
                "extension_id",
                "data",
            ] {
                field_rules.push(FieldRule {
                    path_pattern: (*key).to_string(),
                    classification: FieldClassification::Semantic,
                    placeholder: None,
                });
            }

            Self {
                schema_version: SCHEMA_VERSION.to_string(),
                field_rules,
            }
        }
    }

    impl NormalizationContract {
        /// Normalize a conformance event in-place.
        ///
        /// Applies all rules from this contract: key-based field replacement,
        /// path canonicalization, ANSI stripping, UUID/run-ID/port rewriting.
        pub fn normalize(&self, value: &mut Value, ctx: &NormalizationContext) {
            normalize_value(value, None, ctx);
        }

        /// Normalize and canonicalize (sort keys) for stable comparison.
        #[must_use]
        pub fn normalize_and_canonicalize(
            &self,
            value: Value,
            ctx: &NormalizationContext,
        ) -> Value {
            let mut v = value;
            self.normalize(&mut v, ctx);
            canonicalize_json_keys(&v)
        }
    }

    // ── Core normalization functions ────────────────────────────────────
    //
    // Promoted from `tests/ext_conformance.rs` to library code.

    /// Normalize a JSON value in-place according to the canonical rules.
    ///
    /// - Timestamp keys (string or number) → placeholder / zero
    /// - Transport ID keys → placeholder
    /// - Fixed keys (cwd, host) → placeholder
    /// - Numeric transport keys (pid) → zero
    /// - All strings: ANSI stripping, path canonicalization, UUID/run-ID
    ///   rewriting
    pub fn normalize_value(value: &mut Value, key: Option<&str>, ctx: &NormalizationContext) {
        match value {
            Value::Null | Value::Bool(_) => {}
            Value::String(s) => {
                // Key-based transport replacement (string timestamps)
                if matches_any_key(key, TIMESTAMP_KEYS) {
                    *s = PLACEHOLDER_TIMESTAMP.to_string();
                    return;
                }
                // Transport ID fields
                if let Some(placeholder) = transport_id_placeholder(key) {
                    *s = placeholder.to_string();
                    return;
                }
                // Fixed placeholder fields
                if let Some(placeholder) = fixed_placeholder(key) {
                    *s = placeholder.to_string();
                    return;
                }
                // General string normalization
                *s = normalize_string(s, ctx);
            }
            Value::Array(items) => {
                for item in items {
                    normalize_value(item, None, ctx);
                }
            }
            Value::Object(map) => {
                for (k, item) in map.iter_mut() {
                    normalize_value(item, Some(k.as_str()), ctx);
                }
                // Canonicalize UI operation method names so that
                // `setStatus`/`set_status`/`status` all compare equal.
                canonicalize_ui_method(map);
            }
            Value::Number(_) => {
                if matches_any_key(key, TIMESTAMP_KEYS) || matches_any_key(key, ZEROED_NUMBER_KEYS)
                {
                    *value = Value::Number(0.into());
                }
            }
        }
    }

    /// Normalize a string value: strip ANSI, rewrite paths, replace UUIDs/run-IDs/ports.
    #[must_use]
    pub fn normalize_string(input: &str, ctx: &NormalizationContext) -> String {
        // 1) Strip ANSI escape sequences
        let without_ansi = ansi_regex().replace_all(input, "");

        // 2) Path canonicalization (order matters: most-specific first)
        let mut out = without_ansi.to_string();
        out = replace_path_variants(&out, &ctx.pi_mono_root, PLACEHOLDER_PI_MONO_ROOT);
        out = replace_path_variants(&out, &ctx.cwd, PLACEHOLDER_PI_MONO_ROOT);
        out = replace_path_variants(&out, &ctx.project_root, PLACEHOLDER_PROJECT_ROOT);

        // 3) Run-ID rewriting
        out = run_id_regex()
            .replace_all(&out, PLACEHOLDER_RUN_ID)
            .into_owned();

        // 4) OpenAI base URL port normalization
        out = openai_base_regex()
            .replace_all(&out, format!("http://127.0.0.1:{PLACEHOLDER_PORT}/v1"))
            .into_owned();

        // 5) UUID rewriting
        out = uuid_regex()
            .replace_all(&out, PLACEHOLDER_UUID)
            .into_owned();

        out
    }

    /// Sort JSON object keys recursively for stable serialization.
    #[must_use]
    pub fn canonicalize_json_keys(value: &Value) -> Value {
        match value {
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
            Value::Array(items) => Value::Array(items.iter().map(canonicalize_json_keys).collect()),
            Value::Object(map) => {
                let mut keys = map.keys().cloned().collect::<Vec<_>>();
                keys.sort();
                let mut out = serde_json::Map::new();
                for key in keys {
                    if let Some(v) = map.get(&key) {
                        out.insert(key, canonicalize_json_keys(v));
                    }
                }
                Value::Object(out)
            }
        }
    }

    /// Replace path and its backslash variant with a placeholder.
    fn replace_path_variants(input: &str, path: &str, placeholder: &str) -> String {
        if path.is_empty() {
            return input.to_string();
        }
        let mut out = input.replace(path, placeholder);
        let path_backslashes = path.replace('/', "\\");
        if path_backslashes != path {
            out = out.replace(&path_backslashes, placeholder);
        }
        out
    }

    // ── Helpers ─────────────────────────────────────────────────────────

    fn matches_any_key(key: Option<&str>, candidates: &[&str]) -> bool {
        key.is_some_and(|k| candidates.contains(&k))
    }

    fn transport_id_placeholder(key: Option<&str>) -> Option<&'static str> {
        let k = key?;
        TRANSPORT_ID_KEYS
            .iter()
            .find(|(name, _)| *name == k)
            .map(|(_, placeholder)| *placeholder)
    }

    fn fixed_placeholder(key: Option<&str>) -> Option<&'static str> {
        let k = key?;
        FIXED_PLACEHOLDER_KEYS
            .iter()
            .find(|(name, _)| *name == k)
            .map(|(_, placeholder)| *placeholder)
    }

    /// If `map` represents an `extension_ui_request` event, replace its
    /// `method` value with the canonical short form via [`canonicalize_op_name`].
    fn canonicalize_ui_method(map: &mut serde_json::Map<String, Value>) {
        let is_ui_request = map
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t == "extension_ui_request");
        if !is_ui_request {
            return;
        }
        if let Some(Value::String(method)) = map.get_mut("method") {
            let canonical = canonicalize_op_name(method);
            if canonical != method.as_str() {
                *method = canonical.to_string();
            }
        }
    }

    // ── Alias mapping (unblocks bd-k5q5.1.4) ──────────────────────────

    /// Known UI operation aliases that map to a canonical name.
    ///
    /// Extensions may use either form; the normalization contract treats
    /// them as equivalent during comparison.  The canonical form is the
    /// short verb (e.g. `"status"`) so that `setStatus`, `set_status`,
    /// and `status` all compare equal after normalization.
    pub const UI_OP_ALIASES: &[(&str, &str)] = &[
        ("setStatus", "status"),
        ("set_status", "status"),
        ("setLabel", "label"),
        ("set_label", "label"),
        ("setWidget", "widget"),
        ("set_widget", "widget"),
        ("setTitle", "title"),
        ("set_title", "title"),
    ];

    /// Resolve an operation name to its canonical form.
    #[must_use]
    pub fn canonicalize_op_name(op: &str) -> &str {
        UI_OP_ALIASES
            .iter()
            .find(|(alias, _)| *alias == op)
            .map_or(op, |(_, canonical)| canonical)
    }

    // ── Path canonicalization for conformance assertions (bd-k5q5.1.2) ─

    /// Returns `true` if `key` names a JSON field whose values are
    /// filesystem paths (e.g. `promptPaths`, `filePath`, `cwd`).
    ///
    /// The heuristic matches keys that end with common path suffixes or
    /// are well-known path keys.  This is intentionally conservative;
    /// it is better to match too little (and fail a test explicitly) than
    /// too much (and mask a real mismatch).
    #[must_use]
    pub fn is_path_key(key: &str) -> bool {
        key.ends_with("Path")
            || key.ends_with("Paths")
            || key.ends_with("path")
            || key.ends_with("paths")
            || key.ends_with("Dir")
            || key.ends_with("dir")
            || key == "cwd"
    }

    /// Path-aware suffix match for conformance assertions.
    ///
    /// Returns `true` when `actual` and `expected` refer to the same file:
    ///
    /// - If they are identical → `true`.
    /// - If `expected` is *relative* (no leading `/` or `\`) and `actual`
    ///   ends with `/<expected>` → `true`.
    /// - Otherwise → `false`.
    ///
    /// This handles the common case where fixtures record relative
    /// filenames (`SKILL.md`) while the runtime returns absolute paths
    /// (`/data/projects/.../SKILL.md`).
    #[must_use]
    pub fn path_suffix_match(actual: &str, expected: &str) -> bool {
        if actual == expected {
            return true;
        }
        // Only apply suffix matching when expected is relative.
        if expected.starts_with('/') || expected.starts_with('\\') {
            return false;
        }
        // Normalize backslashes for cross-platform comparison.
        let actual_norm = actual.replace('\\', "/");
        let expected_norm = expected.replace('\\', "/");
        actual_norm.ends_with(&format!("/{expected_norm}"))
    }

    // ── Tests ──────────────────────────────────────────────────────────

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        #[test]
        fn schema_version_is_set() {
            assert!(!SCHEMA_VERSION.is_empty());
            assert_eq!(
                SCHEMA_VERSION.split('.').count(),
                3,
                "semver format expected"
            );
        }

        #[test]
        fn default_contract_has_field_rules() {
            let contract = NormalizationContract::default();
            assert!(
                !contract.field_rules.is_empty(),
                "default contract must have rules"
            );
            assert_eq!(contract.schema_version, SCHEMA_VERSION);
        }

        #[test]
        fn field_classification_serde_roundtrip() {
            for class in [
                FieldClassification::Semantic,
                FieldClassification::Transport,
                FieldClassification::Derived,
            ] {
                let json = serde_json::to_string(&class).unwrap();
                let back: FieldClassification = serde_json::from_str(&json).unwrap();
                assert_eq!(class, back);
            }
        }

        #[test]
        fn normalize_timestamp_string() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let mut val = json!({"ts": "2026-02-03T03:01:02.123Z"});
            normalize_value(&mut val, None, &ctx);
            assert_eq!(val["ts"], PLACEHOLDER_TIMESTAMP);
        }

        #[test]
        fn normalize_timestamp_number() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let mut val = json!({"ts": 1_700_000_000_000_u64});
            normalize_value(&mut val, None, &ctx);
            assert_eq!(val["ts"], 0);
        }

        #[test]
        fn normalize_transport_ids() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let mut val = json!({
                "session_id": "sess-abc",
                "run_id": "run-xyz",
                "artifact_id": "art-123",
                "trace_id": "tr-456",
                "span_id": "sp-789"
            });
            normalize_value(&mut val, None, &ctx);
            assert_eq!(val["session_id"], PLACEHOLDER_SESSION_ID);
            assert_eq!(val["run_id"], PLACEHOLDER_RUN_ID);
            assert_eq!(val["artifact_id"], PLACEHOLDER_ARTIFACT_ID);
            assert_eq!(val["trace_id"], PLACEHOLDER_TRACE_ID);
            assert_eq!(val["span_id"], PLACEHOLDER_SPAN_ID);
        }

        #[test]
        fn normalize_camel_case_variants() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let mut val = json!({
                "sessionId": "sess-abc",
                "runId": "run-xyz",
                "artifactId": "art-123",
                "traceId": "tr-456",
                "spanId": "sp-789",
                "createdAt": "2026-01-01"
            });
            normalize_value(&mut val, None, &ctx);
            assert_eq!(val["sessionId"], PLACEHOLDER_SESSION_ID);
            assert_eq!(val["runId"], PLACEHOLDER_RUN_ID);
            assert_eq!(val["artifactId"], PLACEHOLDER_ARTIFACT_ID);
            assert_eq!(val["traceId"], PLACEHOLDER_TRACE_ID);
            assert_eq!(val["spanId"], PLACEHOLDER_SPAN_ID);
            assert_eq!(val["createdAt"], PLACEHOLDER_TIMESTAMP);
        }

        #[test]
        fn normalize_fixed_keys() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let mut val = json!({"cwd": "/some/path", "host": "myhost.local"});
            normalize_value(&mut val, None, &ctx);
            assert_eq!(val["cwd"], PLACEHOLDER_PI_MONO_ROOT);
            assert_eq!(val["host"], PLACEHOLDER_HOST);
        }

        #[test]
        fn normalize_pid() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let mut val = json!({"source": {"pid": 42}});
            normalize_value(&mut val, None, &ctx);
            assert_eq!(val["source"]["pid"], 0);
        }

        #[test]
        fn normalize_string_strips_ansi() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let input = "\x1b[31mERROR\x1b[0m: something failed";
            let out = normalize_string(input, &ctx);
            assert_eq!(out, "ERROR: something failed");
        }

        #[test]
        fn normalize_string_rewrites_uuids() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let input = "id=123e4567-e89b-12d3-a456-426614174000";
            let out = normalize_string(input, &ctx);
            assert!(out.contains(PLACEHOLDER_UUID), "got: {out}");
        }

        #[test]
        fn normalize_string_rewrites_run_ids() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let input = "run-123e4567-e89b-12d3-a456-426614174000";
            let out = normalize_string(input, &ctx);
            assert!(out.contains(PLACEHOLDER_RUN_ID), "got: {out}");
        }

        #[test]
        fn normalize_string_rewrites_ports() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let input = "http://127.0.0.1:4887/v1/chat";
            let out = normalize_string(input, &ctx);
            assert!(
                out.contains(&format!("http://127.0.0.1:{PLACEHOLDER_PORT}/v1")),
                "got: {out}"
            );
        }

        #[test]
        fn normalize_string_rewrites_paths() {
            let ctx = NormalizationContext::new(
                "/repo/pi".to_string(),
                "/repo/pi/legacy_pi_mono_code/pi-mono".to_string(),
                "/tmp/work".to_string(),
            );
            let input = "opened /tmp/work/file.txt and /repo/pi/src/main.rs";
            let out = normalize_string(input, &ctx);
            assert!(
                out.contains(&format!("{PLACEHOLDER_PI_MONO_ROOT}/file.txt")),
                "got: {out}"
            );
            assert!(
                out.contains(&format!("{PLACEHOLDER_PROJECT_ROOT}/src/main.rs")),
                "got: {out}"
            );
        }

        #[test]
        fn canonicalize_json_keys_sorts_recursively() {
            let input = json!({"z": 1, "a": {"c": 3, "b": 2}});
            let out = canonicalize_json_keys(&input);
            let serialized = serde_json::to_string(&out).unwrap();
            assert_eq!(serialized, r#"{"a":{"b":2,"c":3},"z":1}"#);
        }

        #[test]
        fn contract_normalize_and_canonicalize() {
            let contract = NormalizationContract::default();
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let input = json!({
                "z_field": "hello",
                "ts": "2026-01-01",
                "a_field": 42,
                "session_id": "sess-x"
            });
            let out = contract.normalize_and_canonicalize(input, &ctx);
            assert_eq!(out["ts"], PLACEHOLDER_TIMESTAMP);
            assert_eq!(out["session_id"], PLACEHOLDER_SESSION_ID);
            // Keys should be sorted
            let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
            let mut sorted = keys.clone();
            sorted.sort();
            assert_eq!(keys, sorted);
        }

        #[test]
        fn canonicalize_op_name_resolves_aliases() {
            assert_eq!(canonicalize_op_name("setStatus"), "status");
            assert_eq!(canonicalize_op_name("set_status"), "status");
            assert_eq!(canonicalize_op_name("setLabel"), "label");
            assert_eq!(canonicalize_op_name("set_label"), "label");
            assert_eq!(canonicalize_op_name("setWidget"), "widget");
            assert_eq!(canonicalize_op_name("set_widget"), "widget");
            assert_eq!(canonicalize_op_name("setTitle"), "title");
            assert_eq!(canonicalize_op_name("set_title"), "title");
            // Already-canonical and unknown ops pass through
            assert_eq!(canonicalize_op_name("status"), "status");
            assert_eq!(canonicalize_op_name("notify"), "notify");
            assert_eq!(canonicalize_op_name("unknown_op"), "unknown_op");
        }

        #[test]
        fn normalize_canonicalizes_ui_method() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let mut input = json!({
                "type": "extension_ui_request",
                "id": "req-1",
                "method": "setStatus",
                "statusKey": "demo",
                "statusText": "Ready"
            });
            normalize_value(&mut input, None, &ctx);
            assert_eq!(
                input["method"], "status",
                "setStatus should be canonicalized to status"
            );
        }

        #[test]
        fn normalize_skips_non_ui_request_method() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let mut input = json!({
                "type": "http_request",
                "method": "setStatus"
            });
            normalize_value(&mut input, None, &ctx);
            assert_eq!(
                input["method"], "setStatus",
                "non-ui-request method should NOT be canonicalized"
            );
        }

        #[test]
        fn normalize_and_canonicalize_handles_ui_aliases() {
            let contract = NormalizationContract::default();
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            // Two events that differ only by method naming
            let event_camel = json!({
                "type": "extension_ui_request",
                "method": "setStatus",
                "statusKey": "k"
            });
            let event_snake = json!({
                "type": "extension_ui_request",
                "method": "set_status",
                "statusKey": "k"
            });
            let a = contract.normalize_and_canonicalize(event_camel, &ctx);
            let b = contract.normalize_and_canonicalize(event_snake, &ctx);
            assert_eq!(
                a, b,
                "setStatus and set_status should normalize identically"
            );
            assert_eq!(a["method"], "status");
        }

        #[test]
        fn contract_serializes_to_json() {
            let contract = NormalizationContract::default();
            let json = serde_json::to_string_pretty(&contract).unwrap();
            assert!(json.contains("schema_version"));
            assert!(json.contains("field_rules"));
            // Roundtrip
            let back: NormalizationContract = serde_json::from_str(&json).unwrap();
            assert_eq!(back.schema_version, SCHEMA_VERSION);
            assert_eq!(back.field_rules.len(), contract.field_rules.len());
        }

        #[test]
        fn default_contract_covers_all_transport_keys() {
            let contract = NormalizationContract::default();
            let transport_rules: Vec<_> = contract
                .field_rules
                .iter()
                .filter(|r| r.classification == FieldClassification::Transport)
                .collect();
            // At minimum: 6 timestamp + 10 transport IDs + 2 fixed + 1 pid = 19
            assert!(
                transport_rules.len() >= 19,
                "expected >= 19 transport rules, got {}",
                transport_rules.len()
            );
        }

        #[test]
        fn default_contract_has_semantic_rules() {
            let contract = NormalizationContract::default();
            assert!(
                contract
                    .field_rules
                    .iter()
                    .any(|r| r.classification == FieldClassification::Semantic),
                "contract should document semantic fields"
            );
        }

        // ── Path canonicalization tests (bd-k5q5.1.2) ────────────────

        #[test]
        fn is_path_key_matches_common_suffixes() {
            assert!(is_path_key("promptPaths"));
            assert!(is_path_key("skillPaths"));
            assert!(is_path_key("themePaths"));
            assert!(is_path_key("filePath"));
            assert!(is_path_key("cwd"));
            assert!(is_path_key("workingDir"));
            assert!(!is_path_key("method"));
            assert!(!is_path_key("statusKey"));
            assert!(!is_path_key("name"));
        }

        #[test]
        fn path_suffix_match_exact() {
            assert!(path_suffix_match("SKILL.md", "SKILL.md"));
            assert!(path_suffix_match("/a/b/c.txt", "/a/b/c.txt"));
        }

        #[test]
        fn path_suffix_match_relative_in_absolute() {
            assert!(path_suffix_match(
                "/data/projects/pi/tests/ext_conformance/artifacts/dynamic-resources/SKILL.md",
                "SKILL.md"
            ));
            assert!(path_suffix_match(
                "/data/projects/pi/tests/ext_conformance/artifacts/dynamic-resources/dynamic.md",
                "dynamic.md"
            ));
        }

        #[test]
        fn path_suffix_match_multi_component_relative() {
            assert!(path_suffix_match(
                "/data/projects/ext/sub/dir/file.ts",
                "dir/file.ts"
            ));
            assert!(!path_suffix_match(
                "/data/projects/ext/sub/dir/file.ts",
                "other/file.ts"
            ));
        }

        #[test]
        fn path_suffix_match_rejects_when_expected_is_absolute() {
            // Two different absolute paths should not match via suffix.
            assert!(!path_suffix_match("/a/b/c.txt", "/x/y/c.txt"));
        }

        #[test]
        fn path_suffix_match_handles_backslashes() {
            assert!(path_suffix_match(
                "C:\\Users\\dev\\project\\SKILL.md",
                "SKILL.md"
            ));
        }

        // ── Harness unit-test expansion (bd-k5q5.7.2) ──────────────────

        #[test]
        fn normalize_deeply_nested_mixed_fields() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let mut val = json!({
                "outer": {
                    "inner": {
                        "session_id": "sess-deep",
                        "semantic_data": "keep me",
                        "ts": "2026-01-01T00:00:00Z",
                        "nested_array": [
                            { "pid": 99, "name": "tool-a" },
                            { "host": "deep-host", "value": 42 }
                        ]
                    }
                }
            });
            normalize_value(&mut val, None, &ctx);
            assert_eq!(val["outer"]["inner"]["session_id"], PLACEHOLDER_SESSION_ID);
            assert_eq!(val["outer"]["inner"]["semantic_data"], "keep me");
            assert_eq!(val["outer"]["inner"]["ts"], PLACEHOLDER_TIMESTAMP);
            assert_eq!(val["outer"]["inner"]["nested_array"][0]["pid"], 0);
            assert_eq!(val["outer"]["inner"]["nested_array"][0]["name"], "tool-a");
            assert_eq!(
                val["outer"]["inner"]["nested_array"][1]["host"],
                PLACEHOLDER_HOST
            );
            assert_eq!(val["outer"]["inner"]["nested_array"][1]["value"], 42);
        }

        #[test]
        fn normalize_array_of_events() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let mut val = json!([
                { "ts": "2026-01-01", "session_id": "s1", "event": "start" },
                { "ts": "2026-01-02", "session_id": "s2", "event": "end" }
            ]);
            normalize_value(&mut val, None, &ctx);
            assert_eq!(val[0]["ts"], PLACEHOLDER_TIMESTAMP);
            assert_eq!(val[0]["session_id"], PLACEHOLDER_SESSION_ID);
            assert_eq!(val[0]["event"], "start");
            assert_eq!(val[1]["ts"], PLACEHOLDER_TIMESTAMP);
            assert_eq!(val[1]["session_id"], PLACEHOLDER_SESSION_ID);
            assert_eq!(val[1]["event"], "end");
        }

        #[test]
        fn normalize_empty_structures_unchanged() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let mut empty_obj = json!({});
            normalize_value(&mut empty_obj, None, &ctx);
            assert_eq!(empty_obj, json!({}));

            let mut empty_arr = json!([]);
            normalize_value(&mut empty_arr, None, &ctx);
            assert_eq!(empty_arr, json!([]));

            let mut null_val = Value::Null;
            normalize_value(&mut null_val, None, &ctx);
            assert!(null_val.is_null());

            let mut bool_val = json!(true);
            normalize_value(&mut bool_val, None, &ctx);
            assert_eq!(bool_val, true);
        }

        #[test]
        fn normalize_string_combined_patterns() {
            let ctx = NormalizationContext::new(
                "/repo/pi".to_string(),
                "/repo/pi/legacy".to_string(),
                "/tmp/work".to_string(),
            );
            let input = "\x1b[31mrun-123e4567-e89b-12d3-a456-426614174000\x1b[0m at /tmp/work/test.rs with id=deadbeef-dead-beef-dead-beefdeadbeef http://127.0.0.1:9999/v1/api";
            let out = normalize_string(input, &ctx);
            assert!(!out.contains("\x1b["), "ANSI should be stripped");
            assert!(out.contains(PLACEHOLDER_RUN_ID), "run-ID: {out}");
            assert!(out.contains(PLACEHOLDER_PI_MONO_ROOT), "path: {out}");
            assert!(out.contains(PLACEHOLDER_UUID), "UUID: {out}");
            assert!(out.contains(PLACEHOLDER_PORT), "port: {out}");
        }

        #[test]
        fn normalize_path_canonicalization_overlapping_roots() {
            // When cwd is inside pi_mono_root, both should be replaced correctly.
            let ctx = NormalizationContext::new(
                "/repo".to_string(),
                "/repo/legacy/pi-mono".to_string(),
                "/repo/legacy/pi-mono/test-dir".to_string(),
            );
            // The cwd path should be replaced first (most-specific match).
            let input = "file at /repo/legacy/pi-mono/test-dir/output.txt";
            let out = normalize_string(input, &ctx);
            assert!(
                out.contains(PLACEHOLDER_PI_MONO_ROOT),
                "cwd inside pi_mono: {out}"
            );
            assert!(
                !out.contains("/repo/legacy/pi-mono/test-dir"),
                "original cwd should be gone: {out}"
            );
        }

        #[test]
        fn normalize_idempotent() {
            let ctx = NormalizationContext::new(
                "/repo".to_string(),
                "/repo/legacy".to_string(),
                "/tmp/work".to_string(),
            );
            let contract = NormalizationContract::default();
            let input = json!({
                "ts": "2026-01-01",
                "session_id": "sess-x",
                "host": "myhost",
                "pid": 42,
                "message": "\x1b[31m/tmp/work/file.txt\x1b[0m"
            });
            let first = contract.normalize_and_canonicalize(input, &ctx);
            let second = contract.normalize_and_canonicalize(first.clone(), &ctx);
            assert_eq!(first, second, "normalization must be idempotent");
        }

        #[test]
        fn normalize_preserves_all_semantic_fields() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let mut val = json!({
                "schema": "pi.ext.log.v1",
                "level": "info",
                "event": "tool_call.start",
                "extension_id": "ext.demo",
                "data": { "key": "value", "nested": [1, 2, 3] }
            });
            let original = val.clone();
            normalize_value(&mut val, None, &ctx);
            assert_eq!(val["schema"], original["schema"]);
            assert_eq!(val["level"], original["level"]);
            assert_eq!(val["event"], original["event"]);
            assert_eq!(val["extension_id"], original["extension_id"]);
            assert_eq!(val["data"]["key"], "value");
            assert_eq!(val["data"]["nested"], json!([1, 2, 3]));
        }

        #[test]
        fn normalize_all_timestamp_key_variants() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            for key in &[
                "timestamp",
                "started_at",
                "finished_at",
                "created_at",
                "createdAt",
                "ts",
            ] {
                let mut val =
                    serde_json::from_str(&format!(r#"{{"{key}": "2026-01-01T00:00:00Z"}}"#))
                        .unwrap();
                normalize_value(&mut val, None, &ctx);
                assert_eq!(
                    val[key], PLACEHOLDER_TIMESTAMP,
                    "key {key} should be normalized"
                );
            }
        }

        #[test]
        fn normalize_numeric_timestamp_keys_zeroed() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            for key in &["timestamp", "started_at", "finished_at", "ts"] {
                let mut val = serde_json::from_str(&format!(r#"{{"{key}": 1700000000}}"#)).unwrap();
                normalize_value(&mut val, None, &ctx);
                assert_eq!(val[key], 0, "numeric {key} should be zeroed");
            }
        }

        #[test]
        fn canonicalize_json_keys_preserves_array_order() {
            let input = json!({"items": [3, 1, 2], "z": "last", "a": "first"});
            let out = canonicalize_json_keys(&input);
            // Keys sorted, but array order preserved
            let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
            assert_eq!(keys, &["a", "items", "z"]);
            assert_eq!(out["items"], json!([3, 1, 2]));
        }

        #[test]
        fn canonicalize_json_keys_nested_arrays_of_objects() {
            let input = json!({
                "b": [
                    {"z": 1, "a": 2},
                    {"y": 3, "b": 4}
                ],
                "a": "first"
            });
            let out = canonicalize_json_keys(&input);
            let serialized = serde_json::to_string(&out).unwrap();
            // Top-level keys sorted: "a" before "b"
            // Object keys inside array sorted: "a" before "z", "b" before "y"
            assert_eq!(
                serialized,
                r#"{"a":"first","b":[{"a":2,"z":1},{"b":4,"y":3}]}"#
            );
        }

        #[test]
        fn canonicalize_json_keys_scalar_values_unchanged() {
            assert_eq!(canonicalize_json_keys(&json!(42)), json!(42));
            assert_eq!(canonicalize_json_keys(&json!("hello")), json!("hello"));
            assert_eq!(canonicalize_json_keys(&json!(true)), json!(true));
            assert_eq!(canonicalize_json_keys(&json!(null)), json!(null));
        }

        #[test]
        fn normalize_string_no_match_returns_unchanged() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let input = "plain text with no special patterns";
            let out = normalize_string(input, &ctx);
            assert_eq!(out, input);
        }

        #[test]
        fn normalize_string_multiple_uuids() {
            let ctx = NormalizationContext::new(String::new(), String::new(), String::new());
            let input = "ids: 11111111-2222-3333-4444-555555555555 and 66666666-7777-8888-9999-aaaaaaaaaaaa";
            let out = normalize_string(input, &ctx);
            let count = out.matches(PLACEHOLDER_UUID).count();
            assert_eq!(count, 2, "both UUIDs should be replaced: {out}");
        }

        #[test]
        fn is_path_key_additional_patterns() {
            assert!(is_path_key("outputPath"));
            assert!(is_path_key("inputDir"));
            assert!(is_path_key("rootdir"));
            assert!(is_path_key("filePaths"));
            assert!(!is_path_key("method"));
            assert!(!is_path_key("status"));
            assert!(!is_path_key(""));
        }

        #[test]
        fn path_suffix_match_empty_strings() {
            assert!(path_suffix_match("", ""));
            assert!(!path_suffix_match("file.txt", ""));
            assert!(!path_suffix_match("", "file.txt"));
        }

        #[test]
        fn path_suffix_match_partial_filename_no_match() {
            // "LL.md" should not match "SKILL.md" via suffix
            assert!(!path_suffix_match("/path/to/SKILL.md", "LL.md"));
        }

        #[test]
        fn replace_path_variants_empty_path_noop() {
            let result = super::replace_path_variants("some input text", "", "PLACEHOLDER");
            assert_eq!(result, "some input text");
        }

        #[test]
        fn replace_path_variants_backslash_form() {
            let result =
                super::replace_path_variants("C:\\repo\\pi\\file.rs", "/repo/pi", "<ROOT>");
            // The forward-slash form won't match, but the backslash variant should.
            // Actually, replace_path_variants replaces both forward and backslash.
            // Input has backslashes, path is forward-slash. The backslash variant
            // of path (\repo\pi) should match.
            assert!(
                result.contains("<ROOT>"),
                "backslash variant should match: {result}"
            );
        }

        #[test]
        fn context_new_explicit_paths() {
            let ctx =
                NormalizationContext::new("/a".to_string(), "/b".to_string(), "/c".to_string());
            assert_eq!(ctx.project_root, "/a");
            assert_eq!(ctx.pi_mono_root, "/b");
            assert_eq!(ctx.cwd, "/c");
        }

        #[test]
        fn canonicalize_ui_method_non_ui_type_untouched() {
            let mut map = serde_json::Map::new();
            map.insert("type".into(), json!("rpc_request"));
            map.insert("method".into(), json!("setStatus"));
            super::canonicalize_ui_method(&mut map);
            assert_eq!(map["method"], "setStatus");
        }

        #[test]
        fn canonicalize_ui_method_missing_type_untouched() {
            let mut map = serde_json::Map::new();
            map.insert("method".into(), json!("setStatus"));
            super::canonicalize_ui_method(&mut map);
            assert_eq!(map["method"], "setStatus");
        }

        #[test]
        fn canonicalize_ui_method_unknown_method_untouched() {
            let mut map = serde_json::Map::new();
            map.insert("type".into(), json!("extension_ui_request"));
            map.insert("method".into(), json!("customOp"));
            super::canonicalize_ui_method(&mut map);
            assert_eq!(map["method"], "customOp");
        }

        #[test]
        fn canonicalize_ui_method_all_aliases() {
            for &(alias, canonical) in UI_OP_ALIASES {
                let mut map = serde_json::Map::new();
                map.insert("type".into(), json!("extension_ui_request"));
                map.insert("method".into(), json!(alias));
                super::canonicalize_ui_method(&mut map);
                assert_eq!(
                    map["method"].as_str().unwrap(),
                    canonical,
                    "alias {alias} should canonicalize to {canonical}"
                );
            }
        }

        #[test]
        fn matches_any_key_none_returns_false() {
            assert!(!super::matches_any_key(None, &["ts", "pid"]));
        }

        #[test]
        fn transport_id_placeholder_known_keys() {
            assert_eq!(
                super::transport_id_placeholder(Some("session_id")),
                Some(PLACEHOLDER_SESSION_ID)
            );
            assert_eq!(
                super::transport_id_placeholder(Some("sessionId")),
                Some(PLACEHOLDER_SESSION_ID)
            );
            assert_eq!(
                super::transport_id_placeholder(Some("run_id")),
                Some(PLACEHOLDER_RUN_ID)
            );
            assert_eq!(super::transport_id_placeholder(Some("unknown")), None);
            assert_eq!(super::transport_id_placeholder(None), None);
        }

        #[test]
        fn fixed_placeholder_known_keys() {
            assert_eq!(
                super::fixed_placeholder(Some("cwd")),
                Some(PLACEHOLDER_PI_MONO_ROOT)
            );
            assert_eq!(
                super::fixed_placeholder(Some("host")),
                Some(PLACEHOLDER_HOST)
            );
            assert_eq!(super::fixed_placeholder(Some("other")), None);
            assert_eq!(super::fixed_placeholder(None), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::compare_conformance_output;
    use super::report::compute_regression;
    use super::report::generate_report;
    use super::report::{ConformanceDiffEntry, ConformanceStatus, ExtensionConformanceResult};
    use super::snapshot::{
        self, ArtifactSource, ArtifactSpec, SourceTier, validate_artifact_spec, validate_directory,
        validate_id,
    };
    use serde_json::json;

    #[test]
    fn ignores_registration_ordering_by_key() {
        let expected = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": {
                "commands": [
                    { "name": "b", "description": "B" },
                    { "name": "a", "description": "A" }
                ],
                "shortcuts": [
                    { "key_id": "ctrl+a", "description": "A" }
                ],
                "flags": [],
                "providers": [],
                "tool_defs": [],
                "models": [],
                "event_hooks": ["on_message", "on_tool"]
            },
            "hostcall_log": []
        });

        let actual = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": {
                "commands": [
                    { "name": "a", "description": "A" },
                    { "name": "b", "description": "B" }
                ],
                "shortcuts": [
                    { "description": "A", "key_id": "ctrl+a" }
                ],
                "flags": [],
                "providers": [],
                "tool_defs": [],
                "models": [],
                "event_hooks": ["on_tool", "on_message"]
            },
            "hostcall_log": []
        });

        compare_conformance_output(&expected, &actual).unwrap();
    }

    #[test]
    fn hostcall_log_is_order_sensitive() {
        let expected = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": {
                "commands": [],
                "shortcuts": [],
                "flags": [],
                "providers": [],
                "tool_defs": [],
                "models": [],
                "event_hooks": []
            },
            "hostcall_log": [
                { "op": "get_state", "result": { "a": 1 } },
                { "op": "set_name", "payload": { "name": "x" } }
            ]
        });

        let actual = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": {
                "commands": [],
                "shortcuts": [],
                "flags": [],
                "providers": [],
                "tool_defs": [],
                "models": [],
                "event_hooks": []
            },
            "hostcall_log": [
                { "op": "set_name", "payload": { "name": "x" } },
                { "op": "get_state", "result": { "a": 1 } }
            ]
        });

        let err = compare_conformance_output(&expected, &actual).unwrap_err();
        assert!(err.contains("HOSTCALL"), "missing HOSTCALL header: {err}");
        assert!(
            err.contains("hostcall_log[0].op"),
            "expected path to mention index 0 op: {err}"
        );
    }

    #[test]
    fn treats_missing_as_null_and_empty_array_equivalent() {
        let expected = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": {
                "commands": [
                    { "name": "a", "description": null }
                ],
                "shortcuts": [],
                "flags": [],
                "providers": [],
                "tool_defs": [],
                "models": [],
                "event_hooks": []
            },
            "hostcall_log": []
        });

        let actual = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": {
                "commands": [
                    { "name": "a" }
                ],
                "shortcuts": [],
                "flags": [],
                "providers": [],
                "tool_defs": [],
                "models": [],
                "event_hooks": []
            }
        });

        compare_conformance_output(&expected, &actual).unwrap();
    }

    #[test]
    fn compares_numbers_with_tolerance() {
        let expected = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": {
                "commands": [],
                "shortcuts": [],
                "flags": [],
                "providers": [],
                "tool_defs": [
                    { "name": "t", "parameters": { "precision": 0.1 } }
                ],
                "models": [],
                "event_hooks": []
            },
            "hostcall_log": []
        });

        let actual = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": {
                "commands": [],
                "shortcuts": [],
                "flags": [],
                "providers": [],
                "tool_defs": [
                    { "name": "t", "parameters": { "precision": 0.100_000_000_000_01 } }
                ],
                "models": [],
                "event_hooks": []
            },
            "hostcall_log": []
        });

        compare_conformance_output(&expected, &actual).unwrap();
    }

    #[test]
    fn required_array_order_does_not_matter() {
        let expected = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": {
                "commands": [],
                "shortcuts": [],
                "flags": [],
                "providers": [],
                "tool_defs": [
                    { "name": "t", "parameters": { "required": ["b", "a"] } }
                ],
                "models": [],
                "event_hooks": []
            },
            "hostcall_log": []
        });

        let actual = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": {
                "commands": [],
                "shortcuts": [],
                "flags": [],
                "providers": [],
                "tool_defs": [
                    { "name": "t", "parameters": { "required": ["a", "b"] } }
                ],
                "models": [],
                "event_hooks": []
            },
            "hostcall_log": []
        });

        compare_conformance_output(&expected, &actual).unwrap();
    }

    #[test]
    fn conformance_report_summarizes_and_renders_markdown() {
        let results = vec![
            ExtensionConformanceResult {
                id: "hello".to_string(),
                tier: Some(1),
                status: ConformanceStatus::Pass,
                ts_time_ms: Some(42),
                rust_time_ms: Some(38),
                diffs: Vec::new(),
                notes: None,
            },
            ExtensionConformanceResult {
                id: "event-bus".to_string(),
                tier: Some(2),
                status: ConformanceStatus::Fail,
                ts_time_ms: Some(55),
                rust_time_ms: Some(60),
                diffs: vec![ConformanceDiffEntry {
                    category: "registration.event_hooks".to_string(),
                    path: "registrations.event_hooks".to_string(),
                    message: "extra hook in Rust".to_string(),
                }],
                notes: Some("registration mismatch".to_string()),
            },
            ExtensionConformanceResult {
                id: "ui-heavy".to_string(),
                tier: Some(6),
                status: ConformanceStatus::Skip,
                ts_time_ms: None,
                rust_time_ms: None,
                diffs: Vec::new(),
                notes: Some("ignored in CI".to_string()),
            },
        ];

        let report = generate_report(
            "run-test",
            Some("2026-02-05T00:00:00Z".to_string()),
            results,
        );

        assert_eq!(report.summary.total, 3);
        assert_eq!(report.summary.passed, 1);
        assert_eq!(report.summary.failed, 1);
        assert_eq!(report.summary.skipped, 1);
        assert_eq!(report.summary.errors, 0);
        assert!(report.summary.by_tier.contains_key("tier1"));
        assert!(report.summary.by_tier.contains_key("tier2"));
        assert!(report.summary.by_tier.contains_key("tier6"));

        let md = report.render_markdown();
        assert!(md.contains("# Extension Conformance Report"));
        assert!(md.contains("Run ID: run-test"));
        assert!(md.contains("| hello | PASS | 42ms | 38ms |"));
        assert!(md.contains("## Failures"));
        assert!(md.contains("### event-bus (Tier 2)"));
    }

    #[test]
    fn conformance_regression_ignores_new_extensions_for_pass_rate() {
        let previous = generate_report(
            "run-prev",
            Some("2026-02-05T00:00:00Z".to_string()),
            vec![
                ExtensionConformanceResult {
                    id: "a".to_string(),
                    tier: Some(1),
                    status: ConformanceStatus::Pass,
                    ts_time_ms: None,
                    rust_time_ms: None,
                    diffs: Vec::new(),
                    notes: None,
                },
                ExtensionConformanceResult {
                    id: "b".to_string(),
                    tier: Some(1),
                    status: ConformanceStatus::Fail,
                    ts_time_ms: None,
                    rust_time_ms: None,
                    diffs: Vec::new(),
                    notes: None,
                },
            ],
        );

        let current = generate_report(
            "run-cur",
            Some("2026-02-06T00:00:00Z".to_string()),
            vec![
                ExtensionConformanceResult {
                    id: "a".to_string(),
                    tier: Some(1),
                    status: ConformanceStatus::Pass,
                    ts_time_ms: None,
                    rust_time_ms: None,
                    diffs: Vec::new(),
                    notes: None,
                },
                ExtensionConformanceResult {
                    id: "b".to_string(),
                    tier: Some(1),
                    status: ConformanceStatus::Fail,
                    ts_time_ms: None,
                    rust_time_ms: None,
                    diffs: Vec::new(),
                    notes: None,
                },
                // New failing extension: should not count as regression.
                ExtensionConformanceResult {
                    id: "c".to_string(),
                    tier: Some(1),
                    status: ConformanceStatus::Fail,
                    ts_time_ms: None,
                    rust_time_ms: None,
                    diffs: Vec::new(),
                    notes: None,
                },
            ],
        );

        let regression = compute_regression(&previous, &current);
        assert!(!regression.has_regression());
        assert_eq!(regression.compared_total, 2);
        assert_eq!(regression.previous_passed, 1);
        assert_eq!(regression.current_passed, 1);
    }

    #[test]
    fn conformance_regression_flags_pass_to_fail() {
        let previous = generate_report(
            "run-prev",
            Some("2026-02-05T00:00:00Z".to_string()),
            vec![ExtensionConformanceResult {
                id: "a".to_string(),
                tier: Some(1),
                status: ConformanceStatus::Pass,
                ts_time_ms: None,
                rust_time_ms: None,
                diffs: Vec::new(),
                notes: None,
            }],
        );

        let current = generate_report(
            "run-cur",
            Some("2026-02-06T00:00:00Z".to_string()),
            vec![ExtensionConformanceResult {
                id: "a".to_string(),
                tier: Some(1),
                status: ConformanceStatus::Fail,
                ts_time_ms: None,
                rust_time_ms: None,
                diffs: vec![ConformanceDiffEntry {
                    category: "root".to_string(),
                    path: "x".to_string(),
                    message: "changed".to_string(),
                }],
                notes: None,
            }],
        );

        let regression = compute_regression(&previous, &current);
        assert!(regression.has_regression());
        assert_eq!(regression.regressed_extensions.len(), 1);
        assert_eq!(regression.regressed_extensions[0].id, "a");
        assert_eq!(
            regression.regressed_extensions[0].current,
            Some(ConformanceStatus::Fail)
        );
    }

    // ================================================================
    // Snapshot protocol unit tests (bd-1pqf)
    // ================================================================

    #[test]
    fn snapshot_validate_id_accepts_valid_ids() {
        assert!(validate_id("hello").is_ok());
        assert!(validate_id("auto-commit-on-exit").is_ok());
        assert!(validate_id("my-ext-2").is_ok());
        assert!(validate_id("agents-mikeastock/extensions").is_ok());
    }

    #[test]
    fn snapshot_validate_id_rejects_invalid_ids() {
        assert!(validate_id("").is_err());
        assert!(validate_id("Hello").is_err());
        assert!(validate_id("my_ext").is_err());
        assert!(validate_id("-leading").is_err());
        assert!(validate_id("trailing-").is_err());
        assert!(validate_id("has space").is_err());
    }

    #[test]
    fn snapshot_validate_directory_official_tier() {
        assert!(validate_directory("hello", SourceTier::OfficialPiMono).is_ok());
        assert!(validate_directory("community/x", SourceTier::OfficialPiMono).is_err());
        assert!(validate_directory("npm/x", SourceTier::OfficialPiMono).is_err());
    }

    #[test]
    fn snapshot_validate_directory_scoped_tiers() {
        assert!(validate_directory("community/my-ext", SourceTier::Community).is_ok());
        assert!(validate_directory("my-ext", SourceTier::Community).is_err());

        assert!(validate_directory("npm/some-pkg", SourceTier::NpmRegistry).is_ok());
        assert!(validate_directory("some-pkg", SourceTier::NpmRegistry).is_err());

        assert!(validate_directory("third-party/repo", SourceTier::ThirdPartyGithub).is_ok());
        assert!(validate_directory("repo", SourceTier::ThirdPartyGithub).is_err());

        assert!(validate_directory("templates/my-tpl", SourceTier::Templates).is_ok());
    }

    #[test]
    fn snapshot_validate_directory_empty_rejected() {
        assert!(validate_directory("", SourceTier::OfficialPiMono).is_err());
    }

    #[test]
    fn snapshot_source_tier_from_directory() {
        assert_eq!(
            SourceTier::from_directory("hello"),
            SourceTier::OfficialPiMono
        );
        assert_eq!(
            SourceTier::from_directory("community/foo"),
            SourceTier::Community
        );
        assert_eq!(
            SourceTier::from_directory("npm/bar"),
            SourceTier::NpmRegistry
        );
        assert_eq!(
            SourceTier::from_directory("third-party/baz"),
            SourceTier::ThirdPartyGithub
        );
        assert_eq!(
            SourceTier::from_directory("agents-mikeastock/ext"),
            SourceTier::AgentsMikeastock
        );
        assert_eq!(
            SourceTier::from_directory("templates/tpl"),
            SourceTier::Templates
        );
    }

    #[test]
    fn snapshot_validate_spec_valid() {
        let spec = ArtifactSpec {
            id: "my-ext".into(),
            directory: "community/my-ext".into(),
            name: "My Extension".into(),
            source_tier: SourceTier::Community,
            license: "MIT".into(),
            source: ArtifactSource::Git {
                repo: "https://github.com/user/repo".into(),
                path: Some("extensions/my-ext.ts".into()),
                commit: None,
            },
        };
        assert!(validate_artifact_spec(&spec).is_empty());
    }

    #[test]
    fn snapshot_validate_spec_collects_multiple_errors() {
        let spec = ArtifactSpec {
            id: String::new(),
            directory: "my-ext".into(),
            name: String::new(),
            source_tier: SourceTier::Community,
            license: String::new(),
            source: ArtifactSource::Git {
                repo: String::new(),
                path: None,
                commit: None,
            },
        };
        let errors = validate_artifact_spec(&spec);
        assert!(errors.len() >= 4, "expected at least 4 errors: {errors:?}");
    }

    #[test]
    fn snapshot_validate_spec_npm_source() {
        let spec = ArtifactSpec {
            id: "npm-ext".into(),
            directory: "npm/npm-ext".into(),
            name: "NPM Extension".into(),
            source_tier: SourceTier::NpmRegistry,
            license: "UNKNOWN".into(),
            source: ArtifactSource::Npm {
                package: "npm-ext".into(),
                version: "1.0.0".into(),
                url: None,
            },
        };
        assert!(validate_artifact_spec(&spec).is_empty());

        // Missing package name
        let bad = ArtifactSpec {
            source: ArtifactSource::Npm {
                package: String::new(),
                version: "1.0.0".into(),
                url: None,
            },
            ..spec
        };
        assert!(!validate_artifact_spec(&bad).is_empty());
    }

    #[test]
    fn snapshot_validate_spec_url_source() {
        let spec = ArtifactSpec {
            id: "url-ext".into(),
            directory: "third-party/url-ext".into(),
            name: "URL Extension".into(),
            source_tier: SourceTier::ThirdPartyGithub,
            license: "Apache-2.0".into(),
            source: ArtifactSource::Url {
                url: "https://example.com/ext.ts".into(),
            },
        };
        assert!(validate_artifact_spec(&spec).is_empty());
    }

    #[test]
    fn snapshot_digest_artifact_dir_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("hello.ts"), b"console.log('hi');").unwrap();
        std::fs::write(tmp.path().join("index.ts"), b"export default function() {}").unwrap();

        let d1 = snapshot::digest_artifact_dir(tmp.path()).unwrap();
        let d2 = snapshot::digest_artifact_dir(tmp.path()).unwrap();
        assert_eq!(d1, d2, "digest must be deterministic");
        assert_eq!(d1.len(), 64, "SHA-256 hex must be 64 chars");
    }

    #[test]
    fn snapshot_digest_artifact_dir_changes_with_content() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.ts"), b"version1").unwrap();
        let d1 = snapshot::digest_artifact_dir(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("a.ts"), b"version2").unwrap();
        let d2 = snapshot::digest_artifact_dir(tmp.path()).unwrap();

        assert_ne!(d1, d2, "different content must produce different digest");
    }

    #[test]
    fn snapshot_verify_integrity_pass() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.ts"), b"hello").unwrap();
        let digest = snapshot::digest_artifact_dir(tmp.path()).unwrap();

        let result = snapshot::verify_integrity(tmp.path(), &digest).unwrap();
        assert!(result.is_ok());
    }

    #[test]
    fn snapshot_verify_integrity_fail() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.ts"), b"hello").unwrap();

        let result = snapshot::verify_integrity(
            tmp.path(),
            "0000000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("checksum mismatch"));
    }

    #[test]
    fn snapshot_is_reserved_dir() {
        assert!(snapshot::is_reserved_dir("base_fixtures"));
        assert!(snapshot::is_reserved_dir("community"));
        assert!(snapshot::is_reserved_dir("plugins-official"));
        assert!(!snapshot::is_reserved_dir("hello"));
        assert!(!snapshot::is_reserved_dir("my-ext"));
    }

    #[test]
    fn snapshot_source_tier_roundtrip_serde() {
        let tier = SourceTier::ThirdPartyGithub;
        let json = serde_json::to_string(&tier).unwrap();
        assert_eq!(json, "\"third-party-github\"");
        let parsed: SourceTier = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, tier);
    }

    #[test]
    fn snapshot_artifact_spec_serde_roundtrip() {
        let spec = ArtifactSpec {
            id: "test-ext".into(),
            directory: "community/test-ext".into(),
            name: "Test".into(),
            source_tier: SourceTier::Community,
            license: "MIT".into(),
            source: ArtifactSource::Git {
                repo: "https://github.com/user/repo".into(),
                path: None,
                commit: Some("abc123".into()),
            },
        };
        let json = serde_json::to_string_pretty(&spec).unwrap();
        let parsed: ArtifactSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "test-ext");
        assert_eq!(parsed.source_tier, SourceTier::Community);
    }

    // ════════════════════════════════════════════════════════════════════
    // Harness unit-test expansion: comparison functions (bd-k5q5.7.2)
    // ════════════════════════════════════════════════════════════════════

    #[allow(clippy::needless_pass_by_value)]
    fn base_output(
        registrations: serde_json::Value,
        hostcall_log: serde_json::Value,
    ) -> serde_json::Value {
        json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": registrations,
            "hostcall_log": hostcall_log
        })
    }

    fn empty_registrations() -> serde_json::Value {
        json!({
            "commands": [],
            "shortcuts": [],
            "flags": [],
            "providers": [],
            "tool_defs": [],
            "models": [],
            "event_hooks": []
        })
    }

    #[test]
    fn compare_detects_root_extension_id_mismatch() {
        let expected = base_output(empty_registrations(), json!([]));
        let mut actual = expected.clone();
        actual["extension_id"] = json!("other");
        let err = compare_conformance_output(&expected, &actual).unwrap_err();
        assert!(err.contains("ROOT"), "should report ROOT diff: {err}");
        assert!(err.contains("extension_id"), "should mention field: {err}");
    }

    #[test]
    fn compare_detects_root_name_mismatch() {
        let expected = base_output(empty_registrations(), json!([]));
        let mut actual = expected.clone();
        actual["name"] = json!("Different");
        let err = compare_conformance_output(&expected, &actual).unwrap_err();
        assert!(err.contains("ROOT"), "diff kind: {err}");
    }

    #[test]
    fn compare_detects_root_version_mismatch() {
        let expected = base_output(empty_registrations(), json!([]));
        let mut actual = expected.clone();
        actual["version"] = json!("2.0.0");
        let err = compare_conformance_output(&expected, &actual).unwrap_err();
        assert!(err.contains("version"), "diff: {err}");
    }

    #[test]
    fn compare_detects_extra_registration_item() {
        let expected = base_output(
            json!({
                "commands": [{"name": "a", "description": "A"}],
                "shortcuts": [], "flags": [], "providers": [],
                "tool_defs": [], "models": [], "event_hooks": []
            }),
            json!([]),
        );
        let actual = base_output(
            json!({
                "commands": [
                    {"name": "a", "description": "A"},
                    {"name": "b", "description": "B"}
                ],
                "shortcuts": [], "flags": [], "providers": [],
                "tool_defs": [], "models": [], "event_hooks": []
            }),
            json!([]),
        );
        let err = compare_conformance_output(&expected, &actual).unwrap_err();
        assert!(err.contains("extra"), "should report extra: {err}");
        assert!(err.contains("name=b"), "should name the extra item: {err}");
    }

    #[test]
    fn compare_detects_missing_registration_item() {
        let expected = base_output(
            json!({
                "commands": [
                    {"name": "a", "description": "A"},
                    {"name": "b", "description": "B"}
                ],
                "shortcuts": [], "flags": [], "providers": [],
                "tool_defs": [], "models": [], "event_hooks": []
            }),
            json!([]),
        );
        let actual = base_output(
            json!({
                "commands": [{"name": "a", "description": "A"}],
                "shortcuts": [], "flags": [], "providers": [],
                "tool_defs": [], "models": [], "event_hooks": []
            }),
            json!([]),
        );
        let err = compare_conformance_output(&expected, &actual).unwrap_err();
        assert!(err.contains("missing"), "should report missing: {err}");
    }

    #[test]
    fn compare_detects_string_value_mismatch_in_registration() {
        let expected = base_output(
            json!({
                "commands": [{"name": "a", "description": "original"}],
                "shortcuts": [], "flags": [], "providers": [],
                "tool_defs": [], "models": [], "event_hooks": []
            }),
            json!([]),
        );
        let actual = base_output(
            json!({
                "commands": [{"name": "a", "description": "changed"}],
                "shortcuts": [], "flags": [], "providers": [],
                "tool_defs": [], "models": [], "event_hooks": []
            }),
            json!([]),
        );
        let err = compare_conformance_output(&expected, &actual).unwrap_err();
        assert!(err.contains("description"), "should identify field: {err}");
    }

    #[test]
    fn compare_type_mismatch_produces_clear_diff() {
        let expected = base_output(
            json!({
                "commands": [{"name": "a", "value": "string"}],
                "shortcuts": [], "flags": [], "providers": [],
                "tool_defs": [], "models": [], "event_hooks": []
            }),
            json!([]),
        );
        let actual = base_output(
            json!({
                "commands": [{"name": "a", "value": 42}],
                "shortcuts": [], "flags": [], "providers": [],
                "tool_defs": [], "models": [], "event_hooks": []
            }),
            json!([]),
        );
        let err = compare_conformance_output(&expected, &actual).unwrap_err();
        assert!(
            err.contains("type mismatch"),
            "should report type mismatch: {err}"
        );
    }

    #[test]
    fn compare_event_hooks_order_insensitive() {
        let expected = base_output(
            json!({
                "commands": [], "shortcuts": [], "flags": [],
                "providers": [], "tool_defs": [], "models": [],
                "event_hooks": ["on_tool", "on_message", "on_session"]
            }),
            json!([]),
        );
        let actual = base_output(
            json!({
                "commands": [], "shortcuts": [], "flags": [],
                "providers": [], "tool_defs": [], "models": [],
                "event_hooks": ["on_session", "on_tool", "on_message"]
            }),
            json!([]),
        );
        compare_conformance_output(&expected, &actual).unwrap();
    }

    #[test]
    fn compare_event_hooks_detects_mismatch() {
        let expected = base_output(
            json!({
                "commands": [], "shortcuts": [], "flags": [],
                "providers": [], "tool_defs": [], "models": [],
                "event_hooks": ["on_tool", "on_message"]
            }),
            json!([]),
        );
        let actual = base_output(
            json!({
                "commands": [], "shortcuts": [], "flags": [],
                "providers": [], "tool_defs": [], "models": [],
                "event_hooks": ["on_tool", "on_session"]
            }),
            json!([]),
        );
        let err = compare_conformance_output(&expected, &actual).unwrap_err();
        assert!(
            err.contains("on_message") || err.contains("on_session"),
            "should report hook difference: {err}"
        );
    }

    #[test]
    fn compare_empty_outputs_equal() {
        let a = base_output(empty_registrations(), json!([]));
        compare_conformance_output(&a, &a).unwrap();
    }

    #[test]
    fn compare_null_registration_reports_error() {
        // When registrations is null, compare_registrations expects an
        // object and will report a diff — this verifies it doesn't panic.
        let expected = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": null,
            "hostcall_log": []
        });
        let actual = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": null,
            "hostcall_log": []
        });
        // Both null → both report "expected an object" → diffs cancel
        // Actually registrations expected=null, actual=null both fail as_object check.
        // The function pushes a diff for expected being non-object.
        let result = compare_conformance_output(&expected, &actual);
        assert!(
            result.is_err(),
            "null registrations should produce diff (expected object)"
        );
    }

    #[test]
    fn compare_both_missing_registrations_passes() {
        // When neither side has any registrations at all (no key), the
        // comparison should pass since missing == missing.
        let expected = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "hostcall_log": []
        });
        let actual = expected.clone();
        compare_conformance_output(&expected, &actual).unwrap();
    }

    #[test]
    fn compare_hostcall_log_length_mismatch() {
        let expected = base_output(
            empty_registrations(),
            json!([
                {"op": "get_state", "result": {}},
                {"op": "set_name", "payload": {"name": "x"}}
            ]),
        );
        let actual = base_output(
            empty_registrations(),
            json!([{"op": "get_state", "result": {}}]),
        );
        let err = compare_conformance_output(&expected, &actual).unwrap_err();
        assert!(
            err.contains("length mismatch"),
            "should report length: {err}"
        );
    }

    #[test]
    fn compare_float_within_epsilon_equal() {
        let expected = base_output(
            json!({
                "commands": [], "shortcuts": [], "flags": [],
                "providers": [], "models": [], "event_hooks": [],
                "tool_defs": [{"name": "t", "score": 0.1}]
            }),
            json!([]),
        );
        let actual = base_output(
            json!({
                "commands": [], "shortcuts": [], "flags": [],
                "providers": [], "models": [], "event_hooks": [],
                "tool_defs": [{"name": "t", "score": 0.100_000_000_000_001}]
            }),
            json!([]),
        );
        compare_conformance_output(&expected, &actual).unwrap();
    }

    #[test]
    fn compare_float_beyond_epsilon_differs() {
        let expected = base_output(
            json!({
                "commands": [], "shortcuts": [], "flags": [],
                "providers": [], "models": [], "event_hooks": [],
                "tool_defs": [{"name": "t", "score": 0.1}]
            }),
            json!([]),
        );
        let actual = base_output(
            json!({
                "commands": [], "shortcuts": [], "flags": [],
                "providers": [], "models": [], "event_hooks": [],
                "tool_defs": [{"name": "t", "score": 0.2}]
            }),
            json!([]),
        );
        let err = compare_conformance_output(&expected, &actual).unwrap_err();
        assert!(err.contains("score"), "should mention score: {err}");
    }

    #[test]
    fn compare_integer_exact_match() {
        let expected = base_output(
            json!({
                "commands": [], "shortcuts": [], "flags": [],
                "providers": [], "models": [], "event_hooks": [],
                "tool_defs": [{"name": "t", "count": 42}]
            }),
            json!([]),
        );
        let actual = base_output(
            json!({
                "commands": [], "shortcuts": [], "flags": [],
                "providers": [], "models": [], "event_hooks": [],
                "tool_defs": [{"name": "t", "count": 42}]
            }),
            json!([]),
        );
        compare_conformance_output(&expected, &actual).unwrap();
    }

    #[test]
    fn compare_with_events_section() {
        let expected = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": {
                "commands": [], "shortcuts": [], "flags": [],
                "providers": [], "tool_defs": [], "models": [],
                "event_hooks": []
            },
            "hostcall_log": [],
            "events": { "count": 3, "types": ["start", "end"] }
        });
        let actual = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": {
                "commands": [], "shortcuts": [], "flags": [],
                "providers": [], "tool_defs": [], "models": [],
                "event_hooks": []
            },
            "hostcall_log": [],
            "events": { "count": 3, "types": ["start", "end"] }
        });
        compare_conformance_output(&expected, &actual).unwrap();
    }

    #[test]
    fn compare_events_mismatch_detected() {
        let expected = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": {
                "commands": [], "shortcuts": [], "flags": [],
                "providers": [], "tool_defs": [], "models": [],
                "event_hooks": []
            },
            "hostcall_log": [],
            "events": { "count": 3 }
        });
        let actual = json!({
            "extension_id": "ext",
            "name": "Ext",
            "version": "1.0.0",
            "registrations": {
                "commands": [], "shortcuts": [], "flags": [],
                "providers": [], "tool_defs": [], "models": [],
                "event_hooks": []
            },
            "hostcall_log": [],
            "events": { "count": 5 }
        });
        let err = compare_conformance_output(&expected, &actual).unwrap_err();
        assert!(err.contains("EVENT"), "should be EVENT diff: {err}");
    }

    #[test]
    fn compare_missing_events_both_sides_ok() {
        let a = base_output(empty_registrations(), json!([]));
        // Neither has events section → should pass
        compare_conformance_output(&a, &a).unwrap();
    }

    #[test]
    fn compare_shortcuts_keyed_by_key_id() {
        let expected = base_output(
            json!({
                "commands": [], "flags": [], "providers": [],
                "tool_defs": [], "models": [], "event_hooks": [],
                "shortcuts": [
                    {"key_id": "ctrl+b", "label": "Bold"},
                    {"key_id": "ctrl+a", "label": "All"}
                ]
            }),
            json!([]),
        );
        let actual = base_output(
            json!({
                "commands": [], "flags": [], "providers": [],
                "tool_defs": [], "models": [], "event_hooks": [],
                "shortcuts": [
                    {"key_id": "ctrl+a", "label": "All"},
                    {"key_id": "ctrl+b", "label": "Bold"}
                ]
            }),
            json!([]),
        );
        // Order should not matter for shortcuts (keyed by key_id)
        compare_conformance_output(&expected, &actual).unwrap();
    }

    #[test]
    fn compare_models_keyed_by_id() {
        let expected = base_output(
            json!({
                "commands": [], "shortcuts": [], "flags": [],
                "providers": [], "tool_defs": [], "event_hooks": [],
                "models": [
                    {"id": "m2", "name": "Model 2"},
                    {"id": "m1", "name": "Model 1"}
                ]
            }),
            json!([]),
        );
        let actual = base_output(
            json!({
                "commands": [], "shortcuts": [], "flags": [],
                "providers": [], "tool_defs": [], "event_hooks": [],
                "models": [
                    {"id": "m1", "name": "Model 1"},
                    {"id": "m2", "name": "Model 2"}
                ]
            }),
            json!([]),
        );
        compare_conformance_output(&expected, &actual).unwrap();
    }

    #[test]
    fn report_empty_results() {
        let report = generate_report(
            "run-empty",
            Some("2026-02-07T00:00:00Z".to_string()),
            vec![],
        );
        assert_eq!(report.summary.total, 0);
        assert_eq!(report.summary.passed, 0);
        assert!(report.summary.pass_rate.abs() < f64::EPSILON);
        assert!(report.summary.by_tier.is_empty());
    }

    #[test]
    fn report_all_pass() {
        let results = vec![
            ExtensionConformanceResult {
                id: "a".into(),
                tier: Some(1),
                status: ConformanceStatus::Pass,
                ts_time_ms: Some(10),
                rust_time_ms: Some(8),
                diffs: vec![],
                notes: None,
            },
            ExtensionConformanceResult {
                id: "b".into(),
                tier: Some(1),
                status: ConformanceStatus::Pass,
                ts_time_ms: Some(20),
                rust_time_ms: Some(15),
                diffs: vec![],
                notes: None,
            },
        ];
        let report = generate_report(
            "run-pass",
            Some("2026-02-07T00:00:00Z".to_string()),
            results,
        );
        assert_eq!(report.summary.total, 2);
        assert_eq!(report.summary.passed, 2);
        assert!((report.summary.pass_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn regression_no_overlap_flags_missing_extension() {
        // When a previously-passing extension disappears from current,
        // compute_regression treats it as a regression (Pass → None).
        let previous = generate_report(
            "prev",
            Some("2026-02-05T00:00:00Z".to_string()),
            vec![ExtensionConformanceResult {
                id: "old-ext".into(),
                tier: Some(1),
                status: ConformanceStatus::Pass,
                ts_time_ms: None,
                rust_time_ms: None,
                diffs: vec![],
                notes: None,
            }],
        );
        let current = generate_report(
            "cur",
            Some("2026-02-06T00:00:00Z".to_string()),
            vec![ExtensionConformanceResult {
                id: "new-ext".into(),
                tier: Some(1),
                status: ConformanceStatus::Fail,
                ts_time_ms: None,
                rust_time_ms: None,
                diffs: vec![],
                notes: None,
            }],
        );
        let regression = compute_regression(&previous, &current);
        // compared_total is based on previous.extensions.len() = 1
        assert_eq!(regression.compared_total, 1);
        // old-ext was Pass but is now absent → counted as regression
        assert!(regression.has_regression());
        assert_eq!(regression.regressed_extensions.len(), 1);
        assert_eq!(regression.regressed_extensions[0].id, "old-ext");
        assert_eq!(regression.regressed_extensions[0].current, None);
    }

    #[test]
    fn regression_all_passing_to_passing() {
        let results = vec![ExtensionConformanceResult {
            id: "a".into(),
            tier: Some(1),
            status: ConformanceStatus::Pass,
            ts_time_ms: None,
            rust_time_ms: None,
            diffs: vec![],
            notes: None,
        }];
        let previous = generate_report(
            "prev",
            Some("2026-02-05T00:00:00Z".to_string()),
            results.clone(),
        );
        let current = generate_report("cur", Some("2026-02-06T00:00:00Z".to_string()), results);
        let regression = compute_regression(&previous, &current);
        assert!(!regression.has_regression());
        assert_eq!(regression.compared_total, 1);
        assert!((regression.pass_rate_delta).abs() < f64::EPSILON);
    }

    #[test]
    fn conformance_status_as_upper_str() {
        assert_eq!(ConformanceStatus::Pass.as_upper_str(), "PASS");
        assert_eq!(ConformanceStatus::Fail.as_upper_str(), "FAIL");
        assert_eq!(ConformanceStatus::Skip.as_upper_str(), "SKIP");
        assert_eq!(ConformanceStatus::Error.as_upper_str(), "ERROR");
    }
}
