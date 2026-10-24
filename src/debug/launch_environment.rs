//! Per-launch debuggee environment. Overrides go into DAP arguments, never
//! the host environment or adapter-process spawn options. Do not log values.

use std::fmt;

use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use crate::error::Result;

const MAX_NAMES: usize = 128;
const MAX_NAME_BYTES: usize = 256;
const MAX_BYTES: usize = 64 * 1024;
const INVALID: &str = "env must be an object of literal, NUL-free string values with portable variable names; null and arrays are not supported";
const LIMIT: &str = "env exceeds 128 entries, 256-byte variable names or 64 KiB total names/values including separators";

pub(super) struct LaunchEnvironment {
    values: Map<String, Value>,
}

impl fmt::Debug for LaunchEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchEnvironment")
            .field("entries", &self.values.len())
            .finish_non_exhaustive()
    }
}

fn validate(values: &Map<String, Value>) -> std::result::Result<(), &'static str> {
    if values.len() > MAX_NAMES {
        return Err(LIMIT);
    }
    let mut bytes = 0usize;
    let mut names = std::collections::BTreeSet::new();
    for (name, value) in values {
        if name.len() > MAX_NAME_BYTES {
            return Err(LIMIT);
        }
        let first = name.as_bytes().first().copied();
        if !first.is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(INVALID);
        }
        if cfg!(windows) && !names.insert(name.to_ascii_uppercase()) {
            return Err("env contains case-insensitive duplicate names on Windows");
        }
        let value = value.as_str().ok_or(INVALID)?;
        if value.contains('\0') {
            return Err(INVALID);
        }
        bytes = bytes
            .checked_add(name.len())
            .and_then(|bytes| bytes.checked_add(value.len()))
            .and_then(|bytes| bytes.checked_add(2))
            .filter(|bytes| *bytes <= MAX_BYTES)
            .ok_or(LIMIT)?;
    }
    Ok(())
}

/// Missing input uses serde's field default; explicit null must not silently
/// become absence. Parse through Value so a type error cannot echo a secret.
pub(super) fn deserialize<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<LaunchEnvironment>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error as _;
    let value = Value::deserialize(deserializer)?;
    let Value::Object(values) = value else {
        return Err(D::Error::custom(INVALID));
    };
    validate(&values).map_err(D::Error::custom)?;
    Ok(Some(LaunchEnvironment { values }))
}

/// Explicit entries override object-shaped profile defaults. Validate both
/// maps and the complete result before changing launch arguments or spawning.
/// Omission intentionally leaves existing adapter-specific defaults alone.
pub(super) fn apply(environment: Option<&LaunchEnvironment>, arguments: &mut Value) -> Result<()> {
    let Some(environment) = environment else {
        return Ok(());
    };
    let arguments = arguments
        .as_object_mut()
        .ok_or_else(|| super::tool_err("DAP_ENV_CONFIG", "launch arguments must be an object"))?;
    let mut merged = match arguments.get("env") {
        None => Map::new(),
        Some(Value::Object(defaults)) => {
            validate(defaults).map_err(|message| super::tool_err("DAP_ENV_CONFIG", message))?;
            defaults.clone()
        }
        Some(_) => {
            return Err(super::tool_err(
                "DAP_ENV_CONFIG",
                "explicit env requires object-shaped launchDefaults.env; omit env to retain an adapter-specific representation",
            ));
        }
    };
    for (name, value) in &environment.values {
        if cfg!(windows) {
            let old = merged
                .keys()
                .find(|old| old.eq_ignore_ascii_case(name))
                .cloned();
            if let Some(old) = old {
                merged.remove(&old);
            }
        }
        merged.insert(name.clone(), value.clone());
    }
    validate(&merged).map_err(|message| super::tool_err("DAP_ENV", message))?;
    arguments.insert("env".into(), Value::Object(merged));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug::{DebugInput, DebugTool};
    use crate::tools::Tool;
    use serde_json::json;
    use std::path::Path;

    #[allow(clippy::needless_pass_by_value)] // callers pass json! temporaries
    fn parsed(value: Value) -> LaunchEnvironment {
        serde_json::from_value::<DebugInput>(json!({"action":"launch", "env":value}))
            .unwrap()
            .env
            .unwrap()
    }

    fn event(case: &str) {
        eprintln!(
            "{}",
            json!({"schema":"pi.debug.environment.test.v1","case":case})
        );
    }

    #[test]
    fn explicit_values_are_literal_and_override_only_matching_defaults() {
        event("literal_override");
        let value = "  雪\n$HOME ${OTHER} $(no-shell) = ";
        let input = parsed(json!({"OVERRIDE":value,"EMPTY":""}));
        let mut arguments =
            json!({"program":"app.py","env":{"KEEP":"configured","OVERRIDE":"old"}});
        apply(Some(&input), &mut arguments).unwrap();
        assert_eq!(
            arguments,
            json!({"program":"app.py","env":{"KEEP":"configured","OVERRIDE":value,"EMPTY":""}})
        );
        assert_eq!(input.values["OVERRIDE"], value);
        let first = arguments.clone();
        apply(Some(&parsed(json!({}))), &mut arguments).unwrap();
        assert_eq!(arguments, first);
    }

    #[test]
    fn absence_preserves_adapter_defaults_but_explicit_null_is_refused() {
        event("presence");
        let input: DebugInput = serde_json::from_value(json!({"action":"launch"})).unwrap();
        assert!(input.env.is_none());
        for defaults in [
            json!(["ADAPTER_SPECIFIC=value"]),
            Value::Null,
            json!({"REMOVE":null}),
        ] {
            let mut arguments = json!({"env":defaults});
            let before = arguments.clone();
            apply(None, &mut arguments).unwrap();
            assert_eq!(arguments, before);
            assert!(apply(Some(&parsed(json!({}))), &mut arguments).is_err());
            assert_eq!(arguments, before);
        }
        assert!(
            serde_json::from_value::<DebugInput>(json!({"action":"launch","env":null})).is_err()
        );
    }

    #[test]
    fn malformed_names_values_and_container_shapes_never_echo_payloads() {
        const SECRET: &str = "PRIVATE_ENV_CANARY";
        event("redacted_refusal");
        for value in [
            json!(SECRET),
            json!([SECRET]),
            json!(true),
            json!(42),
            json!({"":SECRET}),
            json!({"9BAD":SECRET}),
            json!({"BAD=NAME":SECRET}),
            json!({"BAD\u{0000}NAME":SECRET}),
            json!({"BAD-NAME":SECRET}),
            json!({"TOKEN":format!("{SECRET}\0")}),
            json!({"TOKEN":[SECRET]}),
            json!({"TOKEN":null}),
            json!({"TOKEN":true}),
            json!({"TOKEN":7}),
        ] {
            let error =
                serde_json::from_value::<DebugInput>(json!({"action":"launch","env":value}))
                    .unwrap_err()
                    .to_string();
            assert!(!error.contains(SECRET));
            assert!(error.contains("env"));
        }
        let input = parsed(json!({"PRIVATE_NAME":SECRET}));
        let rendered = format!("{input:?}");
        assert!(!rendered.contains(SECRET));
        assert!(!rendered.contains("PRIVATE_NAME"));
        assert!(rendered.contains("entries: 1"));
    }

    #[test]
    fn environment_is_launch_only_and_advertised_as_a_bounded_object() {
        event("scope_and_schema");
        for action in [
            "attach",
            "sessions",
            "evaluate",
            "set_breakpoint",
            "terminal_input",
        ] {
            let input: DebugInput =
                serde_json::from_value(json!({"action":action,"env":{}})).unwrap();
            assert!(
                input
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("env is launch-only")
            );
        }
        let input: DebugInput =
            serde_json::from_value(json!({"action":"launch","env":{}})).unwrap();
        input.validate().unwrap();
        let schema = DebugTool::new(Path::new("."), None).parameters();
        assert_eq!(schema["properties"]["env"]["type"], "object");
        assert_eq!(schema["properties"]["env"]["maxProperties"], MAX_NAMES);
        assert_eq!(
            schema["properties"]["env"]["additionalProperties"]["type"],
            "string"
        );
    }

    #[test]
    fn complete_utf8_byte_and_entry_limits_apply_before_admission() {
        event("input_limits");
        let full = json!({"A":"x".repeat(MAX_BYTES - 3)});
        parsed(full);
        for value in [
            json!({"A":"x".repeat(MAX_BYTES - 2)}),
            json!({"A":"雪".repeat(MAX_BYTES / 3)}),
            Value::Object(
                std::iter::once((String::from("A").repeat(MAX_NAME_BYTES + 1), json!("x")))
                    .collect(),
            ),
            Value::Object(
                (0..=MAX_NAMES)
                    .map(|index| (format!("V{index}"), json!("x")))
                    .collect(),
            ),
        ] {
            assert!(
                serde_json::from_value::<DebugInput>(json!({"action":"launch","env":value}))
                    .is_err()
            );
        }
        parsed(Value::Object(
            (0..MAX_NAMES)
                .map(|index| (format!("V{index}"), json!("")))
                .collect(),
        ));
    }

    #[test]
    fn merged_limits_and_invalid_defaults_leave_arguments_untouched() {
        event("atomic_merge");
        let added = parsed(json!({"EXTRA":"x"}));
        let count_full: Map<String, Value> = (0..MAX_NAMES)
            .map(|index| (format!("V{index}"), json!("")))
            .collect();
        for defaults in [
            Value::Object(count_full),
            json!({"A":"x".repeat(MAX_BYTES - 3)}),
            json!({"TOKEN":"DEFAULT_SECRET\u{0000}"}),
            json!({"TOKEN":17}),
        ] {
            let mut arguments = json!({"program":"kept","env":defaults});
            let before = arguments.clone();
            let error = apply(Some(&added), &mut arguments).unwrap_err().to_string();
            assert!(!error.contains("DEFAULT_SECRET"));
            assert_eq!(arguments, before);
        }
        let mut full = json!({"env":{"A":"x".repeat(MAX_BYTES - 3)}});
        apply(Some(&parsed(json!({"A":"small"}))), &mut full).unwrap();
        assert_eq!(full["env"], json!({"A":"small"}));
    }

    #[test]
    fn case_rules_match_the_host_without_ambiguous_windows_entries() {
        event("platform_case");
        let both = serde_json::from_value::<DebugInput>(
            json!({"action":"launch","env":{"PATH":"a","Path":"b"}}),
        );
        assert_eq!(both.is_err(), cfg!(windows));
        let mut arguments = json!({"env":{"PATH":"default"}});
        apply(Some(&parsed(json!({"Path":"explicit"}))), &mut arguments).unwrap();
        assert_eq!(arguments["env"]["Path"], "explicit");
        assert_eq!(arguments["env"].get("PATH").is_none(), cfg!(windows));
    }
}

#[cfg(all(test, unix))]
#[path = "launch_environment_native_tests.rs"]
mod native_tests;
