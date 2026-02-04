//! Extension conformance harness utilities (normalization + diff triage).
//!
//! This is the first building block for the planned `tests/ext_conformance/` suite
//! described in `CONFORMANCE.md` and `EXTENSIONS.md`.
//!
//! The core idea:
//! - Extension logs (JSONL) must be comparable across runs.
//! - We normalize known non-deterministic fields (timestamps, pids, run/session IDs, etc.).
//! - We canonicalize JSON key ordering for stable diffs.
//! - Diffs are grouped by `event` and correlation IDs to speed triage.
#![forbid(unsafe_code)]

use regex::Regex;
use serde_json::{Value, json};
use similar::ChangeTag;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use tempfile::NamedTempFile;
use tracing::trace;

const PLACEHOLDER_TIMESTAMP: &str = "<TIMESTAMP>";
const PLACEHOLDER_HOST: &str = "<HOST>";
const PLACEHOLDER_SESSION_ID: &str = "<SESSION_ID>";
const PLACEHOLDER_RUN_ID: &str = "<RUN_ID>";
const PLACEHOLDER_ARTIFACT_ID: &str = "<ARTIFACT_ID>";
const PLACEHOLDER_TRACE_ID: &str = "<TRACE_ID>";
const PLACEHOLDER_SPAN_ID: &str = "<SPAN_ID>";
const PLACEHOLDER_UUID: &str = "<UUID>";
const PLACEHOLDER_PI_MONO_ROOT: &str = "<PI_MONO_ROOT>";
const PLACEHOLDER_PROJECT_ROOT: &str = "<PROJECT_ROOT>";
const PLACEHOLDER_PORT: &str = "<PORT>";

static ANSI_REGEX: OnceLock<Regex> = OnceLock::new();
static RUN_ID_REGEX: OnceLock<Regex> = OnceLock::new();
static UUID_REGEX: OnceLock<Regex> = OnceLock::new();
static OPENAI_BASE_REGEX: OnceLock<Regex> = OnceLock::new();

fn ansi_regex() -> &'static Regex {
    ANSI_REGEX.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").expect("ansi regex"))
}

#[derive(Debug, Clone)]
struct NormalizationContext {
    project_root: String,
    pi_mono_root: String,
    cwd: String,
}

impl NormalizationContext {
    fn from_cwd(cwd: &Path) -> Self {
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
}

fn normalize_ext_log_line(mut value: Value, ctx: &NormalizationContext) -> Value {
    normalize_value(&mut value, None, ctx);
    canonicalize_json_keys(&value)
}

fn normalize_value(value: &mut Value, key: Option<&str>, ctx: &NormalizationContext) {
    match value {
        Value::Null | Value::Bool(_) => {}
        Value::String(s) => {
            if matches!(
                key,
                Some(
                    "timestamp" | "started_at" | "finished_at" | "created_at" | "createdAt" | "ts"
                )
            ) {
                *s = PLACEHOLDER_TIMESTAMP.to_string();
                return;
            }
            if matches!(key, Some("cwd")) {
                *s = PLACEHOLDER_PI_MONO_ROOT.to_string();
                return;
            }
            if matches!(key, Some("host")) {
                *s = PLACEHOLDER_HOST.to_string();
                return;
            }
            if matches!(key, Some("session_id" | "sessionId")) {
                *s = PLACEHOLDER_SESSION_ID.to_string();
                return;
            }
            if matches!(key, Some("run_id" | "runId")) {
                *s = PLACEHOLDER_RUN_ID.to_string();
                return;
            }
            if matches!(key, Some("artifact_id" | "artifactId")) {
                *s = PLACEHOLDER_ARTIFACT_ID.to_string();
                return;
            }
            if matches!(key, Some("trace_id" | "traceId")) {
                *s = PLACEHOLDER_TRACE_ID.to_string();
                return;
            }
            if matches!(key, Some("span_id" | "spanId")) {
                *s = PLACEHOLDER_SPAN_ID.to_string();
                return;
            }
            *s = normalize_string(s, ctx);
        }
        Value::Array(items) => {
            for item in items {
                normalize_value(item, None, ctx);
            }
        }
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                normalize_value(item, Some(key.as_str()), ctx);
            }
        }
        Value::Number(_) => {
            if matches!(
                key,
                Some(
                    "timestamp"
                        | "started_at"
                        | "finished_at"
                        | "created_at"
                        | "createdAt"
                        | "ts"
                        | "pid"
                )
            ) {
                *value = Value::Number(0.into());
            }
        }
    }
}

fn normalize_string(input: &str, ctx: &NormalizationContext) -> String {
    let run_id_re =
        RUN_ID_REGEX.get_or_init(|| Regex::new(r"\brun-[0-9a-fA-F-]{36}\b").expect("run id regex"));
    let uuid_re = UUID_REGEX.get_or_init(|| {
        Regex::new(
            r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
        )
        .expect("uuid regex")
    });
    let openai_base_re = OPENAI_BASE_REGEX
        .get_or_init(|| Regex::new(r"http://127\.0\.0\.1:\d+/v1").expect("openai base regex"));

    // 1) Strip ANSI escape sequences (keeps plain text).
    // Covers CSI sequences like: ESC[31m, ESC[0m, ESC[2K, etc.
    let without_ansi = ansi_regex().replace_all(input, "");

    // 2) Normalize absolute paths under cwd to "<cwd>/...".
    let mut out = without_ansi.to_string();
    out = replace_path_variants(&out, &ctx.pi_mono_root, PLACEHOLDER_PI_MONO_ROOT);
    out = replace_path_variants(&out, &ctx.cwd, PLACEHOLDER_PI_MONO_ROOT);
    out = replace_path_variants(&out, &ctx.project_root, PLACEHOLDER_PROJECT_ROOT);
    out = run_id_re.replace_all(&out, PLACEHOLDER_RUN_ID).into_owned();
    out = openai_base_re
        .replace_all(&out, format!("http://127.0.0.1:{PLACEHOLDER_PORT}/v1"))
        .into_owned();
    out = uuid_re.replace_all(&out, PLACEHOLDER_UUID).into_owned();
    out
}

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

fn canonicalize_json_keys(value: &Value) -> Value {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.clone(),
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_json_keys).collect()),
        Value::Object(map) => {
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            let mut out = serde_json::Map::new();
            for key in keys {
                if let Some(value) = map.get(&key) {
                    out.insert(key, canonicalize_json_keys(value));
                }
            }
            Value::Object(out)
        }
    }
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
            "path": cwd.join("dir/sub/file.rs").display().to_string(),
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
    assert!(msg.contains("<PI_MONO_ROOT>/file.txt"));
    assert!(!msg.contains(&cwd.display().to_string()));
    assert!(!msg.contains("\u{1b}["));
    assert!(msg.contains("ERR"));

    let path = normalized["data"]["path"].as_str().unwrap_or_default();
    assert!(path.contains("<PI_MONO_ROOT>/dir/sub/file.rs"));
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
    let out = normalize_string(&input, &ctx);
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
