//! Bounded control-state projection from Chromium's accessibility response.
//!
//! Only known primitive states leave this module. Editable values, relation
//! text, AX value sources and arbitrary token strings are never copied. This
//! is an observation of reported accessibility state, not input authorization.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{Value, json};

const MAX_PROPERTIES: usize = 64;

#[derive(Debug, Default, PartialEq, Serialize)]
pub(super) struct Observation {
    pub(super) states: BTreeMap<&'static str, Value>,
    pub(super) incomplete: bool,
}

#[derive(Clone, Copy)]
enum Kind {
    Boolean,
    Tristate,
    Token(&'static [&'static str]),
    Level,
}

fn property(name: &str) -> Option<(&'static str, Kind)> {
    use Kind::{Boolean, Level, Token, Tristate};
    Some(match name {
        "busy" => ("busy", Boolean),
        "disabled" => ("disabled", Boolean),
        "focusable" => ("focusable", Boolean),
        "focused" => ("focused", Boolean),
        "settable" => ("settable", Boolean),
        "atomic" => ("atomic", Boolean),
        "multiselectable" => ("multiselectable", Boolean),
        "multiline" => ("multiline", Boolean),
        "readonly" => ("readonly", Boolean),
        "required" => ("required", Boolean),
        "expanded" => ("expanded", Boolean),
        "modal" => ("modal", Boolean),
        "selected" => ("selected", Boolean),
        "checked" => ("checked", Tristate),
        "pressed" => ("pressed", Tristate),
        "editable" => ("editable", Token(&["plaintext", "richtext"])),
        "invalid" => ("invalid", Token(&["false", "true", "grammar", "spelling"])),
        "live" => ("live", Token(&["off", "polite", "assertive"])),
        "autocomplete" => ("autocomplete", Token(&["none", "inline", "list", "both"])),
        "hasPopup" => ("hasPopup", Token(&["false", "true", "menu", "listbox", "tree", "grid", "dialog"])),
        "orientation" => ("orientation", Token(&["horizontal", "vertical"])),
        "level" => ("level", Level),
        _ => return None,
    })
}

/// Ok(None) is an explicitly undefined state, not false or a protocol defect.
fn primitive(name: &str, kind: Kind, wire: &Value) -> Result<Option<Value>, ()> {
    let ty = wire.get("type").and_then(Value::as_str).ok_or(())?;
    let value = wire.get("value");
    if value.is_none() && matches!(ty, "valueUndefined" | "booleanOrUndefined") {
        return Ok(None);
    }
    let value = value.ok_or(())?;
    let projected = match kind {
        Kind::Boolean if matches!(ty, "boolean" | "booleanOrUndefined") => {
            if let Some(value) = value.as_bool() {
                Value::Bool(value)
            } else if name == "busy" && ty == "boolean" {
                // Chromium emits this specific boolean state as integer 0/1.
                // Do not generalize it to string, float or arbitrary truthiness.
                match value.as_u64() {
                    Some(0) => Value::Bool(false),
                    Some(1) => Value::Bool(true),
                    _ => return Err(()),
                }
            } else {
                return Err(());
            }
        }
        Kind::Tristate if ty == "tristate" => match value.as_str() {
            Some("true") => Value::Bool(true),
            Some("false") => Value::Bool(false),
            Some("mixed") => json!("mixed"),
            _ => return Err(()),
        },
        Kind::Token(allowed) if ty == "token" => {
            let token = value.as_str().ok_or(())?;
            let token = allowed.iter().copied().find(|allowed| *allowed == token).ok_or(())?;
            // Serialize the allowlisted constant, never an arbitrary source token.
            json!(token)
        }
        Kind::Level if ty == "integer" => {
            let level = value.as_u64().filter(|level| (1..=1_000_000).contains(level)).ok_or(())?;
            json!(level)
        }
        _ => return Err(()),
    };
    Ok(Some(projected))
}

pub(super) fn project(node: &Value) -> Observation {
    let mut observation = Observation::default();
    let Some(properties) = node.get("properties") else {
        return observation;
    };
    let Some(properties) = properties.as_array().filter(|properties| properties.len() <= MAX_PROPERTIES) else {
        observation.incomplete = true;
        return observation;
    };
    let mut seen = BTreeSet::new();
    for entry in properties {
        let Some(name) = entry.get("name").and_then(Value::as_str) else {
            observation.incomplete = true;
            continue;
        };
        let Some((name, kind)) = property(name) else {
            continue;
        };
        if !seen.insert(name) {
            // Duplicate state names are ambiguous, even when one occurrence
            // was undefined or malformed. A third duplicate cannot re-add it.
            observation.states.remove(name);
            observation.incomplete = true;
            continue;
        }
        match primitive(name, kind, &entry["value"]) {
            Ok(Some(value)) => { observation.states.insert(name, value); }
            Ok(None) => {}
            Err(()) => observation.incomplete = true,
        }
    }
    observation
}

impl Observation {
    pub(super) fn suffix(&self) -> String {
        let mut fields: Vec<String> = self.states.iter().map(|(name, value)| {
            format!("{name}={value}")
        }).collect();
        if self.incomplete {
            fields.push("state-projection-incomplete".to_string());
        }
        if fields.is_empty() {
            String::new()
        } else {
            format!(" [{}]", fields.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, ty: &str, value: Value) -> Value {
        json!({"name":name, "value":{"type":ty, "value":value}})
    }

    #[test]
    fn chromium_control_states_preserve_false_and_mixed() {
        let state = project(&json!({"properties":[
            entry("checked", "tristate", json!("mixed")),
            entry("expanded", "booleanOrUndefined", json!(false)),
            entry("disabled", "boolean", json!(true)),
            entry("selected", "booleanOrUndefined", json!(false)),
            entry("hasPopup", "token", json!("menu")),
            entry("level", "integer", json!(3)),
        ]}));
        assert_eq!(serde_json::to_value(&state).unwrap(), json!({"states":{
            "checked":"mixed", "expanded":false, "disabled":true,
            "selected":false, "hasPopup":"menu", "level":3,
        }, "incomplete":false}));
    }

    #[test]
    fn chromium_numeric_busy_is_a_narrow_exception_not_truthiness() {
        for (wire, expected) in [(json!(0), false), (json!(1), true), (json!(true), true)] {
            let state = project(&json!({"properties":[entry("busy", "boolean", wire)]}));
            assert_eq!(state.states["busy"], expected);
            assert!(!state.incomplete);
        }
        for (name, wire) in [("busy", json!(2)), ("busy", json!("1")),
            ("busy", json!(1.0)), ("disabled", json!(1))] {
            let state = project(&json!({"properties":[entry(name, "boolean", wire)]}));
            assert!(state.incomplete);
            assert!(state.states.is_empty());
        }
    }

    #[test]
    fn missing_undefined_and_explicit_null_are_distinct() {
        assert_eq!(project(&json!({})), Observation::default());
        let undefined = project(&json!({"properties":[
            {"name":"expanded","value":{"type":"booleanOrUndefined"}},
            {"name":"checked","value":{"type":"valueUndefined"}},
        ]}));
        assert_eq!(undefined, Observation::default());
        let null = project(&json!({"properties":[entry("expanded", "booleanOrUndefined", Value::Null)]}));
        assert!(null.incomplete);
        assert!(null.states.is_empty());
    }

    #[test]
    fn duplicates_never_choose_one_reported_state() {
        let state = project(&json!({"properties":[
            entry("checked", "tristate", json!("true")),
            entry("checked", "tristate", json!("false")),
            entry("checked", "tristate", json!("true")),
            entry("required", "boolean", json!(true)),
        ]}));
        assert!(state.incomplete);
        assert!(!state.states.contains_key("checked"));
        assert_eq!(state.states["required"], true);
    }

    #[test]
    fn values_sources_relations_and_unknown_tokens_never_enter_state_output() {
        let state = project(&json!({"value":{"value":"PRIVATE-CANARY"}, "properties":[
            entry("valuetext", "string", json!("PRIVATE-CANARY")),
            entry("roledescription", "string", json!("PRIVATE-CANARY")),
            entry("url", "string", json!("PRIVATE-CANARY")),
            {"name":"labelledby","value":{"type":"nodeList","relatedNodes":[{"text":"PRIVATE-CANARY"}]}},
            entry("invalid", "token", json!("PRIVATE-CANARY")),
            {"name":"disabled","value":{"type":"boolean","value":true,"sources":[{"value":"PRIVATE-CANARY"}]}},
        ]}));
        assert_eq!(state.states.len(), 1);
        assert_eq!(state.states["disabled"], true);
        assert!(state.incomplete);
        assert!(!serde_json::to_string(&state).unwrap().contains("PRIVATE-CANARY"));
        assert!(!state.suffix().contains("PRIVATE-CANARY"));
    }

    #[test]
    fn oversized_property_lists_are_omitted_instead_of_hiding_tail_conflicts() {
        let mut entries = vec![entry("disabled", "boolean", json!(true))];
        entries.extend((0..MAX_PROPERTIES).map(|_| entry("unknown", "string", json!("x"))));
        let state = project(&json!({"properties":entries}));
        assert!(state.incomplete);
        assert!(state.states.is_empty());
        assert!(project(&json!({"properties":null})).incomplete);
        assert!(project(&json!({"properties":[null]})).incomplete);
    }

    #[test]
    fn wire_types_are_checked_before_extracting_primitives() {
        for entry in [entry("selected", "string", json!(true)),
            entry("checked", "boolean", json!(true)), entry("level", "integer", json!(-1)),
            entry("level", "integer", json!(1_000_001)), entry("level", "integer", json!(2.5)),
            entry("orientation", "token", json!("diagonal"))] {
            let state = project(&json!({"properties":[entry]}));
            assert!(state.incomplete);
            assert!(state.states.is_empty());
        }
    }

    #[test]
    fn state_annotations_are_sorted_and_absence_is_not_invented_false() {
        let state = project(&json!({"properties":[
            entry("selected", "booleanOrUndefined", json!(false)),
            entry("checked", "tristate", json!("true")),
        ]}));
        assert_eq!(state.suffix(), " [checked=true, selected=false]");
        assert!(!state.states.contains_key("disabled"));
        assert_eq!(Observation::default().suffix(), "");
    }
}
