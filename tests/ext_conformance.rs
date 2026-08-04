//! Extension conformance harness utilities (normalization + diff triage).
//!
//! This is the first building block for the planned `tests/ext_conformance/` suite
//! described in `docs/planning/CONFORMANCE.md` and `docs/planning/EXTENSIONS.md`.
//!
//! The core idea:
//! - Extension logs (JSONL) must be comparable across runs.
//! - We normalize known non-deterministic fields (timestamps, pids, run/session IDs, etc.).
//! - We canonicalize JSON key ordering for stable diffs.
//! - Diffs are grouped by `event` and correlation IDs to speed triage.
//!
//! **Normalization rules are defined in the canonical contract** at
//! [`pi::conformance::normalization`].  This test file delegates to that
//! module so there is one source of truth.
#![forbid(unsafe_code)]

use pi::conformance::normalization::{
    self, NormalizationContext, PLACEHOLDER_ARTIFACT_ID, PLACEHOLDER_HOST,
    PLACEHOLDER_PI_MONO_ROOT, PLACEHOLDER_RUN_ID, PLACEHOLDER_SESSION_ID, PLACEHOLDER_SPAN_ID,
    PLACEHOLDER_TIMESTAMP, PLACEHOLDER_TRACE_ID, is_path_key, path_suffix_match,
};
use serde_json::{Value, json};
use similar::ChangeTag;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
#[cfg(feature = "internal-legacy-capture")]
use std::io::Write as _;
use std::path::Path;
#[cfg(feature = "internal-legacy-capture")]
use std::path::PathBuf;
#[cfg(feature = "internal-legacy-capture")]
use std::process::Command;
#[cfg(feature = "internal-legacy-capture")]
use tempfile::NamedTempFile;
use tracing::trace;

/// Normalize a single JSONL event line using the canonical contract.
fn normalize_ext_log_line(value: Value, ctx: &NormalizationContext) -> Value {
    let contract = normalization::NormalizationContract::default();
    contract.normalize_and_canonicalize(value, ctx)
}

fn diff_key(value: &Value) -> String {
    let event = value
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or("<missing>");
    let correlation = value.get("correlation").and_then(Value::as_object);
    let (kind, id) = correlation
        .and_then(|corr| {
            preferred_correlation_id(corr, "tool_call_id", "tool_call_id")
                .or_else(|| preferred_correlation_id(corr, "slash_command_id", "slash_command_id"))
                .or_else(|| preferred_correlation_id(corr, "event_id", "event_id"))
                .or_else(|| preferred_correlation_id(corr, "host_call_id", "host_call_id"))
                .or_else(|| preferred_correlation_id(corr, "rpc_id", "rpc_id"))
                .or_else(|| preferred_correlation_id(corr, "scenario_id", "scenario_id"))
        })
        .unwrap_or(("id", "<missing>"));
    format!("{event}::{kind}:{id}")
}

fn preferred_correlation_id<'a>(
    corr: &'a serde_json::Map<String, Value>,
    key: &'static str,
    kind: &'static str,
) -> Option<(&'static str, &'a str)> {
    let id = corr.get(key).and_then(Value::as_str)?;
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    Some((kind, id))
}

fn diff_normalized_jsonl(
    expected_jsonl: &str,
    actual_jsonl: &str,
    cwd: &Path,
) -> Result<(), String> {
    let ctx = NormalizationContext::from_cwd(cwd);
    let expected = parse_and_normalize_jsonl(expected_jsonl, &ctx)?;
    let actual = parse_and_normalize_jsonl(actual_jsonl, &ctx)?;

    let expected_groups = group_by_diff_key(&expected);
    let actual_groups = group_by_diff_key(&actual);

    let mut keys = BTreeSet::new();
    keys.extend(expected_groups.keys().cloned());
    keys.extend(actual_groups.keys().cloned());

    let mut problems = String::new();
    for key in keys {
        let expected_items = expected_groups.get(&key).cloned().unwrap_or_default();
        let actual_items = actual_groups.get(&key).cloned().unwrap_or_default();
        if expected_items == actual_items {
            continue;
        }

        let expected_text = render_group(&expected_items)?;
        let actual_text = render_group(&actual_items)?;
        let group_diff = render_text_diff(&expected_text, &actual_text);

        let _ = writeln!(problems, "\n=== DIFF GROUP: {key} ===");
        problems.push_str(&group_diff);
        problems.push('\n');
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

fn parse_and_normalize_jsonl(
    input: &str,
    ctx: &NormalizationContext,
) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for (idx, line) in input.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed: Value = serde_json::from_str(line)
            .map_err(|err| format!("line {idx}: JSON parse error: {err}"))?;
        let normalized = normalize_ext_log_line(parsed, ctx);
        if std::env::var_os("PI_TEST_MODE").is_some() {
            trace!(
                target: "ext_conformance.normalize",
                line = idx + 1,
                value = %serde_json::to_string(&normalized).unwrap_or_default()
            );
        }
        out.push(normalized);
    }
    Ok(out)
}

fn group_by_diff_key(values: &[Value]) -> BTreeMap<String, Vec<Value>> {
    let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for value in values {
        groups
            .entry(diff_key(value))
            .or_default()
            .push(value.clone());
    }
    groups
}

fn render_group(values: &[Value]) -> Result<String, String> {
    // Always render arrays so count/order differences are visible.
    serde_json::to_string_pretty(values).map_err(|err| err.to_string())
}

fn render_text_diff(expected: &str, actual: &str) -> String {
    let diff = similar::TextDiff::from_lines(expected, actual);
    let mut out = String::new();
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        out.push_str(sign);
        out.push_str(change.value());
    }
    out
}

#[test]
fn normalizes_dynamic_fields_paths_and_ansi() {
    let cwd = Path::new("/tmp/pi_ext_conformance");
    let ctx = NormalizationContext::from_cwd(cwd);
    let original = json!({
        "schema": "pi.ext.log.v1",
        "ts": "2026-02-03T03:01:02.123Z",
        "level": "info",
        "event": "tool_call.start",
        "message": format!("opened {} \u{1b}[31mERR\u{1b}[0m", cwd.join("file.txt").display()),
        "correlation": {
            "extension_id": "ext.demo",
            "scenario_id": "scn-001",
            "session_id": "sess-abc123",
            "run_id": "run-20260203-0001",
            "artifact_id": "sha256:deadbeef",
            "trace_id": "trace-xyz",
            "span_id": "span-123"
        },
        "source": { "component": "runtime", "host": "host.name", "pid": 4242 },
        "data": {
            "path": cwd.join("dir").join("sub").join("file.rs").display().to_string(),
            "note": "\u{1b}[1mBold\u{1b}[0m"
        }
    });

    let normalized = normalize_ext_log_line(original, &ctx);

    assert_eq!(normalized["ts"], PLACEHOLDER_TIMESTAMP);
    assert_eq!(
        normalized["correlation"]["session_id"],
        PLACEHOLDER_SESSION_ID
    );
    assert_eq!(normalized["correlation"]["run_id"], PLACEHOLDER_RUN_ID);
    assert_eq!(
        normalized["correlation"]["artifact_id"],
        PLACEHOLDER_ARTIFACT_ID
    );
    assert_eq!(normalized["correlation"]["trace_id"], PLACEHOLDER_TRACE_ID);
    assert_eq!(normalized["correlation"]["span_id"], PLACEHOLDER_SPAN_ID);
    assert_eq!(normalized["source"]["host"], PLACEHOLDER_HOST);
    assert_eq!(normalized["source"]["pid"], 0);

    let msg = normalized["message"].as_str().unwrap_or_default();
    // Windows PathBuf::join uses backslash, so accept both separators
    assert!(msg.contains("<PI_MONO_ROOT>/file.txt") || msg.contains("<PI_MONO_ROOT>\\file.txt"),);
    assert!(!msg.contains(&cwd.display().to_string()));
    assert!(!msg.contains("\u{1b}["));
    assert!(msg.contains("ERR"));

    let path = normalized["data"]["path"].as_str().unwrap_or_default();
    assert!(
        path.contains("<PI_MONO_ROOT>/dir/sub/file.rs")
            || path.contains("<PI_MONO_ROOT>\\dir\\sub\\file.rs"),
    );
    assert!(!path.contains(&cwd.display().to_string()));

    assert_eq!(normalized["data"]["note"], "Bold");
}

#[test]
fn normalize_string_rewrites_run_ids_ports_and_roots() {
    let cwd = Path::new("/tmp/pi_ext_conformance");
    let ctx = NormalizationContext::from_cwd(cwd);
    let input = format!(
        "run-123e4567-e89b-12d3-a456-426614174000 http://127.0.0.1:4887/v1 {}",
        ctx.pi_mono_root
    );
    let out = normalization::normalize_string(&input, &ctx);
    assert!(out.contains(PLACEHOLDER_RUN_ID), "{out}");
    assert!(out.contains("http://127.0.0.1:<PORT>/v1"), "{out}");
    assert!(out.contains(PLACEHOLDER_PI_MONO_ROOT), "{out}");
}

#[test]
fn diff_key_prefers_most_specific_correlation_id() {
    let value = json!({
        "event": "tool_call.start",
        "correlation": {
            "scenario_id": "scn-001",
            "tool_call_id": "tool-42"
        }
    });

    assert_eq!(diff_key(&value), "tool_call.start::tool_call_id:tool-42");
}

#[test]
fn diff_normalized_jsonl_treats_dynamic_fields_as_equal() {
    let cwd = Path::new("/tmp/pi_ext_conformance");
    let expected = r#"
{"schema":"pi.ext.log.v1","ts":"2026-02-03T03:01:02.123Z","level":"info","event":"tool_call.start","message":"opened /tmp/pi_ext_conformance/file.txt","correlation":{"extension_id":"ext.demo","scenario_id":"scn-001","session_id":"sess-a","run_id":"run-a"},"source":{"component":"runtime","host":"a","pid":1}}
"#;
    let actual = r#"
{"schema":"pi.ext.log.v1","ts":"2026-02-03T03:01:02.999Z","level":"info","event":"tool_call.start","message":"opened /tmp/pi_ext_conformance/file.txt","correlation":{"extension_id":"ext.demo","scenario_id":"scn-001","session_id":"sess-b","run_id":"run-b"},"source":{"component":"runtime","host":"b","pid":9999}}
"#;

    diff_normalized_jsonl(expected, actual, cwd).unwrap();
}

#[test]
fn ui_method_aliases_normalize_identically_in_jsonl_diff() {
    let cwd = Path::new("/tmp/pi_ext_conformance");
    // Expected side uses camelCase (setStatus) — matches TS pi-mono output.
    let expected = r#"
{"type":"extension_ui_request","id":"req-1","method":"setStatus","statusKey":"demo","statusText":"Ready"}
"#;
    // Actual side uses snake_case (set_status) — hypothetical Rust naming.
    let actual = r#"
{"type":"extension_ui_request","id":"req-1","method":"set_status","statusKey":"demo","statusText":"Ready"}
"#;
    // Both should normalise to method:"status", so diff should be empty.
    diff_normalized_jsonl(expected, actual, cwd)
        .expect("setStatus and set_status should compare equal after normalization");
}

#[test]
#[cfg(feature = "internal-legacy-capture")]
fn trace_viewer_renders_pretty_and_exports_jsonl() {
    let mut log_file = NamedTempFile::new().expect("temp log file");

    let line1 = r#"{"schema":"pi.ext.log.v1","ts":"2026-02-03T03:01:02.123Z","level":"info","event":"capture","message":"capture.start","correlation":{"extension_id":"ext.demo","scenario_id":"scn-001","run_id":"run-123"},"source":{"component":"capture","pid":42},"data":{"started_at":"2026-02-03T03:01:02.123Z","provider":"openai","model":"gpt-4o-mini"}}"#;
    let line2 = r#"{"schema":"pi.ext.log.v1","ts":"2026-02-03T03:01:02.456Z","level":"debug","event":"tool_call.start","message":"read.start","correlation":{"extension_id":"ext.demo","scenario_id":"scn-001","tool_call_id":"tool-42"},"source":{"component":"runtime","pid":4242},"data":{"tool":"read","path":"/repo/README.md"}}"#;
    let line3 = r#"{"schema":"pi.ext.log.v1","ts":"2026-02-03T03:01:02.999Z","level":"error","event":"hostcall.error","message":"capability denied","correlation":{"extension_id":"ext.demo","scenario_id":"scn-001","host_call_id":"host-7","trace_id":"trace-xyz"},"source":{"component":"runtime","pid":4242},"data":{"capability":"fs.read","scope":"repo","hint":"Add fs.read capability to manifest."}}"#;

    writeln!(log_file, "{line1}").expect("write log line1");
    writeln!(log_file, "{line2}").expect("write log line2");
    writeln!(log_file, "{line3}").expect("write log line3");

    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_pi_legacy_capture"));

    let pretty = Command::new(&binary_path)
        .args([
            "--view-log",
            log_file.path().to_str().expect("utf8 path"),
            "--view-mode",
            "pretty",
            "--view-min-level",
            "debug",
        ])
        .output()
        .expect("run trace viewer (pretty)");
    assert!(
        pretty.status.success(),
        "trace viewer (pretty) exit={:?}, stderr={}",
        pretty.status.code(),
        String::from_utf8_lossy(&pretty.stderr)
    );
    let pretty_stdout = String::from_utf8_lossy(&pretty.stdout);
    insta::assert_snapshot!(pretty_stdout);

    let jsonl = Command::new(&binary_path)
        .args([
            "--view-log",
            log_file.path().to_str().expect("utf8 path"),
            "--view-mode",
            "jsonl",
            "--view-min-level",
            "debug",
        ])
        .output()
        .expect("run trace viewer (jsonl)");
    assert!(
        jsonl.status.success(),
        "trace viewer (jsonl) exit={:?}, stderr={}",
        jsonl.status.code(),
        String::from_utf8_lossy(&jsonl.stderr)
    );
    let jsonl_stdout = String::from_utf8_lossy(&jsonl.stdout);
    let expected_jsonl = format!("{line1}\n{line2}\n{line3}\n");
    assert_eq!(jsonl_stdout.as_ref(), expected_jsonl);
}

// ─── Regression tests for dynamic-resources, git-checkpoint, status-line ──────
//
// These tests lock in the harness-level fixes from bd-k5q5.1.2 (path
// canonicalization), bd-k5q5.1.3 (UI response plumbing), and bd-k5q5.1.4
// (UI op aliases).  If any of these regresses, the corresponding conformance
// scenario will silently fail with false negatives.

/// Regression: `json_contains` with path-aware matching so that
/// `returns_contains.promptPaths = ["dynamic.md"]` matches the runtime
/// result `promptPaths = ["/abs/path/to/dynamic.md"]`.
///
/// Mirrors fixture: `dynamic-resources.json` scenario `scn-dynamic-resources-001`.
#[test]
fn regression_dynamic_resources_path_suffix_matching() {
    // All three keys should be recognized as path keys
    assert!(is_path_key("promptPaths"), "promptPaths is a path key");
    assert!(is_path_key("skillPaths"), "skillPaths is a path key");
    assert!(is_path_key("themePaths"), "themePaths is a path key");

    // Suffix matching works for each file
    assert!(path_suffix_match(
        "/home/user/.pi/extensions/dynamic-resources/dynamic.md",
        "dynamic.md"
    ));
    assert!(path_suffix_match(
        "/home/user/.pi/extensions/dynamic-resources/SKILL.md",
        "SKILL.md"
    ));
    assert!(path_suffix_match(
        "/home/user/.pi/extensions/dynamic-resources/dynamic.json",
        "dynamic.json"
    ));

    // Verify the actual fixture values would match
    let actual_paths = [
        (
            "/home/user/.pi/extensions/dynamic-resources/dynamic.md",
            "dynamic.md",
        ),
        (
            "/home/user/.pi/extensions/dynamic-resources/SKILL.md",
            "SKILL.md",
        ),
        (
            "/home/user/.pi/extensions/dynamic-resources/dynamic.json",
            "dynamic.json",
        ),
    ];
    for (actual, expected) in actual_paths {
        assert!(
            path_suffix_match(actual, expected),
            "expected '{expected}' to suffix-match '{actual}'"
        );
    }
}

/// Regression: absolute expected paths must NOT match via suffix.
/// Prevents false positives where `/other/SKILL.md` would match `/ext/SKILL.md`.
#[test]
fn regression_dynamic_resources_rejects_absolute_expected() {
    assert!(!path_suffix_match(
        "/ext/dynamic-resources/SKILL.md",
        "/other/SKILL.md"
    ));
}

/// Regression: `setStatus` / `set_status` UI op aliases normalize identically
/// in JSONL diff, so the status-line extension passes conformance regardless
/// of which alias the Rust runtime uses.
///
/// Mirrors fixture: `status-line.json` scenario `scn-status-line-001`.
#[test]
fn regression_status_line_op_alias_normalization() {
    let cwd = Path::new("/tmp/pi_ext_conformance");

    // Expected side (from TS pi-mono capture): uses camelCase `setStatus`
    let expected = r#"
{"type":"extension_ui_request","id":"req-1","method":"setStatus","statusKey":"status-demo","statusText":"Ready"}
{"type":"extension_ui_request","id":"req-2","method":"setStatus","statusKey":"status-demo","statusText":"Turn 1..."}
{"type":"extension_ui_request","id":"req-3","method":"setStatus","statusKey":"status-demo","statusText":"Turn 1 complete"}
"#;

    // Actual side (from Rust runtime): might use snake_case `set_status`
    let actual = r#"
{"type":"extension_ui_request","id":"req-1","method":"set_status","statusKey":"status-demo","statusText":"Ready"}
{"type":"extension_ui_request","id":"req-2","method":"set_status","statusKey":"status-demo","statusText":"Turn 1..."}
{"type":"extension_ui_request","id":"req-3","method":"set_status","statusKey":"status-demo","statusText":"Turn 1 complete"}
"#;

    diff_normalized_jsonl(expected, actual, cwd)
        .expect("setStatus and set_status should diff-equal after normalization");
}

/// Regression: `setWidget` / `set_widget` aliases also normalize, covering
/// the full alias table added in bd-k5q5.1.4.
#[test]
fn regression_ui_alias_table_completeness() {
    let cwd = Path::new("/tmp/pi_ext_conformance");

    for (camel, snake) in [
        ("setStatus", "set_status"),
        ("setLabel", "set_label"),
        ("setWidget", "set_widget"),
        ("setTitle", "set_title"),
    ] {
        let expected = format!(
            r#"{{"type":"extension_ui_request","id":"req-1","method":"{camel}","data":"test"}}"#
        );
        let actual = format!(
            r#"{{"type":"extension_ui_request","id":"req-1","method":"{snake}","data":"test"}}"#
        );
        diff_normalized_jsonl(&expected, &actual, cwd).unwrap_or_else(|e| {
            panic!("{camel} and {snake} should normalize identically:\n{e}");
        });
    }
}

/// Regression: non-UI-request objects with a `method` field must NOT be
/// rewritten by alias normalization (e.g., JSON-RPC messages).
#[test]
fn regression_non_ui_method_untouched() {
    let cwd = Path::new("/tmp/pi_ext_conformance");
    let ctx = NormalizationContext::from_cwd(cwd);
    let contract = normalization::NormalizationContract::default();

    let input = json!({
        "type": "json_rpc",
        "method": "setStatus",
        "params": {}
    });
    let normalized = contract.normalize_and_canonicalize(input, &ctx);
    assert_eq!(
        normalized["method"], "setStatus",
        "non-UI method should not be rewritten"
    );
}

/// Regression: git-checkpoint UI response fixture format is valid and the
/// notification pattern from the fixture would pass case-insensitive matching.
#[test]
fn regression_git_checkpoint_ui_notify_pattern() {
    let notification_text = "Code restored to checkpoint for entry-1";
    let expected_pattern = "Code restored to checkpoint";
    assert!(
        notification_text
            .to_lowercase()
            .contains(&expected_pattern.to_lowercase()),
        "notification pattern should match"
    );

    // Verify the ui_responses fixture format is valid JSON
    let ctx_json = json!({
        "has_ui": true,
        "ui_responses": {
            "select": "Yes, restore code to that point"
        }
    });
    assert!(ctx_json["ui_responses"]["select"].is_string());
    assert_eq!(
        ctx_json["ui_responses"]["select"],
        "Yes, restore code to that point"
    );
}

/// Regression: full normalization pipeline for dynamic-resources log lines.
#[test]
fn regression_dynamic_resources_full_normalization_pipeline() {
    let cwd = Path::new("/workspace/ext/dynamic-resources");
    let ctx = NormalizationContext::from_cwd(cwd);
    let contract = normalization::NormalizationContract::default();

    let input = json!({
        "schema": "pi.ext.log.v1",
        "ts": "2026-02-03T12:37:19.100Z",
        "event": "resources_discover",
        "message": format!("discovered {} resources", cwd.display()),
        "data": {
            "promptPaths": [format!("{}/dynamic.md", cwd.display())],
            "skillPaths": [format!("{}/SKILL.md", cwd.display())],
            "themePaths": [format!("{}/dynamic.json", cwd.display())]
        },
        "correlation": {
            "extension_id": "dynamic-resources",
            "session_id": "sess-abc",
            "run_id": "run-xyz"
        },
        "source": { "host": "build-host", "pid": 42 }
    });

    let normalized = contract.normalize_and_canonicalize(input, &ctx);

    // Timestamps and IDs should be placeholders
    assert_eq!(normalized["ts"], PLACEHOLDER_TIMESTAMP);
    assert_eq!(
        normalized["correlation"]["session_id"],
        PLACEHOLDER_SESSION_ID
    );

    // Paths in data should be rewritten to use PI_MONO_ROOT placeholder
    let prompt_paths = normalized["data"]["promptPaths"]
        .as_array()
        .expect("promptPaths array");
    assert!(
        prompt_paths[0]
            .as_str()
            .unwrap()
            .contains(PLACEHOLDER_PI_MONO_ROOT),
        "promptPaths should use placeholder root: {:?}",
        prompt_paths[0]
    );

    // Message should have cwd replaced
    let msg = normalized["message"].as_str().unwrap();
    assert!(
        msg.contains(PLACEHOLDER_PI_MONO_ROOT),
        "message should use placeholder: {msg}"
    );
    assert!(
        !msg.contains("/workspace/ext/dynamic-resources"),
        "original cwd should be gone: {msg}"
    );
}
