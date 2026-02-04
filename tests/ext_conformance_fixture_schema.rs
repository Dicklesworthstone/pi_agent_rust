#![forbid(unsafe_code)]
#![allow(clippy::too_many_lines)]

mod common;

use common::TestHarness;
use regex::Regex;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

const EXT_FIXTURE_SCHEMA: &str = "pi.ext.legacy_fixtures.v1";

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/ext_conformance/fixtures")
}

fn list_json_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = fs::read_dir(dir)
        .expect("read_dir failed")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn require_pointer<'a>(value: &'a Value, pointer: &str) -> Result<&'a Value, String> {
    value
        .pointer(pointer)
        .ok_or_else(|| format!("missing {pointer}"))
}

fn require_str<'a>(value: &'a Value, pointer: &str) -> Result<&'a str, String> {
    require_pointer(value, pointer)?
        .as_str()
        .ok_or_else(|| format!("{pointer} must be string"))
}

fn require_array<'a>(value: &'a Value, pointer: &str) -> Result<&'a [Value], String> {
    require_pointer(value, pointer)?
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| format!("{pointer} must be array"))
}

fn require_object<'a>(
    value: &'a Value,
    pointer: &str,
) -> Result<&'a serde_json::Map<String, Value>, String> {
    require_pointer(value, pointer)?
        .as_object()
        .ok_or_else(|| format!("{pointer} must be object"))
}

fn is_hex_lower(s: &str) -> bool {
    s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

fn validate_fixture(path: &Path, value: &Value) -> Result<(), String> {
    let schema = require_str(value, "/schema")?;
    if schema != EXT_FIXTURE_SCHEMA {
        return Err(format!(
            "/schema must be {EXT_FIXTURE_SCHEMA}, got {schema}"
        ));
    }

    let extension_id = require_str(value, "/extension/id")?;
    let file_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("<unknown>");
    if extension_id != file_stem {
        return Err(format!(
            "/extension/id must match filename ({file_stem}), got {extension_id}"
        ));
    }

    let sha = require_str(value, "/extension/checksum/sha256")?;
    if sha.len() != 64 || !is_hex_lower(sha) {
        return Err(format!(
            "/extension/checksum/sha256 must be 64-char lowercase hex, got {sha}"
        ));
    }

    let _legacy = require_object(value, "/legacy")?;
    let _capture = require_object(value, "/capture")?;

    let scenarios = require_array(value, "/scenarios")?;
    if scenarios.is_empty() {
        return Err("/scenarios must be non-empty".to_string());
    }

    for (idx, _scenario) in scenarios.iter().enumerate() {
        let ptr = |suffix: &str| format!("/scenarios/{idx}/{suffix}");
        let scenario_id = require_str(value, &ptr("id"))?;
        if scenario_id.trim().is_empty() {
            return Err(format!("{} must be non-empty", ptr("id")));
        }
        let kind = require_str(value, &ptr("kind"))?;
        let summary = require_str(value, &ptr("summary"))?;
        if summary.trim().is_empty() {
            return Err(format!("{} must be non-empty", ptr("summary")));
        }

        let event_name = value.pointer(&ptr("event_name")).and_then(Value::as_str);
        let _tool_name = value.pointer(&ptr("tool_name")).and_then(Value::as_str);
        let command_name = value.pointer(&ptr("command_name")).and_then(Value::as_str);
        let provider_id = value.pointer(&ptr("provider_id")).and_then(Value::as_str);

        match kind {
            "event" => {
                if event_name.is_none() {
                    return Err(format!(
                        "{}/event_name must be string for kind=event",
                        ptr("")
                    ));
                }
            }
            "command" => {
                if command_name.is_none() {
                    return Err(format!(
                        "{}/command_name must be string for kind=command",
                        ptr("")
                    ));
                }
            }
            "provider" => {
                if provider_id.is_none() {
                    return Err(format!(
                        "{}/provider_id must be string for kind=provider",
                        ptr("")
                    ));
                }
            }
            _ => {}
        }

        let input_ptr = ptr("input");
        let input_value = require_pointer(value, &input_ptr)?;
        if !(input_value.is_object() || input_value.is_null()) {
            return Err(format!("{input_ptr} must be object or null"));
        }
        require_object(value, &ptr("expect"))?;
        let outputs_ptr = ptr("outputs");
        require_object(value, &outputs_ptr)?;

        let stdout_ptr = format!("{outputs_ptr}/stdout_normalized_jsonl");
        let stdout_lines = require_array(value, &stdout_ptr)?;
        for (line_idx, line_value) in stdout_lines.iter().enumerate() {
            let line = line_value
                .as_str()
                .ok_or_else(|| format!("{stdout_ptr}/{line_idx} must be string"))?;
            serde_json::from_str::<Value>(line)
                .map_err(|err| format!("{stdout_ptr}/{line_idx} invalid JSON: {err}"))?;
        }

        let meta_ptr = format!("{outputs_ptr}/meta_normalized");
        require_object(value, &meta_ptr)?;

        let capture_log_ptr = format!("{outputs_ptr}/capture_log_normalized_jsonl");
        let capture_log_lines = require_array(value, &capture_log_ptr)?;
        for (line_idx, line_value) in capture_log_lines.iter().enumerate() {
            let line = line_value
                .as_str()
                .ok_or_else(|| format!("{capture_log_ptr}/{line_idx} must be string"))?;
            serde_json::from_str::<Value>(line)
                .map_err(|err| format!("{capture_log_ptr}/{line_idx} invalid JSON: {err}"))?;
        }
    }

    Ok(())
}

#[test]
fn ext_conformance_fixtures_parse_and_match_schema_and_normalization_rules() {
    let harness = TestHarness::new("ext_conformance_fixtures_parse_and_match_schema");
    let dir = fixtures_dir();
    let files = list_json_files(&dir);
    assert!(
        !files.is_empty(),
        "no fixture files found in {}",
        dir.display()
    );

    let ansi = Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").expect("ansi regex");
    let uuid = Regex::new(
        r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
    )
    .expect("uuid regex");
    let project_root = env!("CARGO_MANIFEST_DIR");

    for path in files {
        harness
            .log()
            .debug("fixture", format!("validating {}", path.display()));
        let content = fs::read_to_string(&path).expect("read fixture");
        let value: Value = serde_json::from_str(&content).expect("parse fixture");

        validate_fixture(&path, &value).expect("validate fixture");

        // Basic normalization sanity checks: no raw ANSI and no raw absolute project root or UUIDs.
        if let Some(lines) = value
            .pointer("/scenarios")
            .and_then(Value::as_array)
            .into_iter()
            .flat_map(|scenarios| scenarios.iter())
            .filter_map(|scenario| {
                scenario
                    .pointer("/outputs/stdout_normalized_jsonl")
                    .and_then(Value::as_array)
            })
            .flatten()
            .find_map(Value::as_str)
        {
            assert!(
                !ansi.is_match(lines),
                "{}: ANSI escape in stdout_normalized_jsonl",
                path.display()
            );
            assert!(
                !lines.contains(project_root),
                "{}: absolute project root leaked into stdout_normalized_jsonl",
                path.display()
            );
            assert!(
                !uuid.is_match(lines),
                "{}: raw UUID leaked into stdout_normalized_jsonl",
                path.display()
            );
        }
    }
}

#[test]
fn ext_conformance_fixture_validation_rejects_missing_schema() {
    let value = serde_json::json!({
        "extension": {"id":"hello","checksum":{"sha256":"6d4d5a97ada168817b9cb3808e51013969c2ce7c2c4d1536b4d630ad3f8b78f1"}},
        "legacy": {},
        "capture": {},
        "scenarios": [{"id":"scn-1","kind":"tool","summary":"x","tool_name":"hello","event_name":null,"command_name":null,"provider_id":null,"input":{},"expect":{},"outputs":{"stdout_normalized_jsonl":["{}"],"meta_normalized":{},"capture_log_normalized_jsonl":["{}"]}}]
    });
    let err = validate_fixture(Path::new("hello.json"), &value).unwrap_err();
    assert!(err.contains("missing /schema"), "unexpected error: {err}");
}
