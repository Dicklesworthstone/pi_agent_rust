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

#[cfg(test)]
mod tests {
    use super::compare_conformance_output;
    use super::report::generate_report;
    use super::report::{ConformanceDiffEntry, ConformanceStatus, ExtensionConformanceResult};
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
}
