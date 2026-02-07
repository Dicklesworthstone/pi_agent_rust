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
            id: "".into(),
            directory: "my-ext".into(),
            name: "".into(),
            source_tier: SourceTier::Community,
            license: "".into(),
            source: ArtifactSource::Git {
                repo: "".into(),
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
                package: "".into(),
                version: "1.0.0".into(),
                url: None,
            },
            ..spec.clone()
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
}
