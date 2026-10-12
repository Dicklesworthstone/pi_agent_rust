//! Structured, verified form operations on one pinned DOM node.
//!
//! The public selector is resolved once. Every subsequent access uses that
//! backend node and document, including verification after page event handlers.

use super::{Cdp, Document, References, document, resolve};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const FUNCTION: &str = include_str!("forms.js");
const MAX_OPTIONS: usize = 100;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_DOM_OPTIONS: u64 = 4096;

fn error(message: &'static str) -> Error {
    Error::tool("browser", message)
}

pub(in crate::browser) fn is_action(action: &str) -> bool {
    matches!(action, "select_option")
}

/// Run before connection/launch. Unknown fields are refused, not silently
/// ignored (especially dialog consent and options intended for another action).
pub(in crate::browser) fn validate(args: &Value) -> Result<()> {
    let object = args
        .as_object()
        .ok_or_else(|| error("BROWSER_FORM_INVALID: expected an object"))?;
    if args["action"] != "select_option"
        || object.keys().any(|key| {
            !matches!(key.as_str(), "action" | "tab" | "frame" | "selector" | "options" | "timeout_ms")
        })
        || args["selector"]
            .as_str()
            .is_none_or(|value| value.is_empty() || value.len() > MAX_TEXT_BYTES)
    {
        return Err(error("BROWSER_FORM_INVALID: select_option requires a selector and only its documented fields"));
    }
    let options = args["options"]
        .as_array()
        .filter(|options| options.len() <= MAX_OPTIONS)
        .ok_or_else(|| error("BROWSER_FORM_INVALID: options must be an array of at most 100 selectors"))?;
    let mut remaining = MAX_TEXT_BYTES;
    for option in options {
        let object = option
            .as_object()
            .filter(|object| object.len() == 1)
            .ok_or_else(|| error("BROWSER_FORM_INVALID: each option needs exactly one value, label or index"))?;
        let (key, value) = object.iter().next().ok_or_else(|| error("BROWSER_FORM_INVALID: empty option"))?;
        match key.as_str() {
            "value" | "label" => {
                let text = value.as_str().ok_or_else(|| error("BROWSER_FORM_INVALID: option value and label must be strings"))?;
                remaining = remaining.checked_sub(text.len()).ok_or_else(|| error("BROWSER_FORM_LIMIT: option selectors exceed 64 KiB"))?;
            }
            "index" if value.as_u64().is_some_and(|index| index < MAX_DOM_OPTIONS) => {}
            _ => return Err(error("BROWSER_FORM_INVALID: option index must be an integer in 0..4096")),
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SelectedOption {
    index: u64,
    value: String,
    label: String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Selection {
    multiple: bool,
    selected: Vec<SelectedOption>,
}

fn selection(mut value: Value) -> Result<(Selection, Option<bool>)> {
    let object = value.as_object_mut().ok_or_else(|| error("BROWSER_FORM_PROTOCOL: missing selection state"))?;
    let changed = object.remove("changed").map(|value| {
        value.as_bool().ok_or_else(|| error("BROWSER_FORM_PROTOCOL: invalid changed flag"))
    }).transpose()?;
    let state: Selection = serde_json::from_value(value)
        .map_err(|_| error("BROWSER_FORM_PROTOCOL: malformed selection state"))?;
    if state.selected.len() > MAX_OPTIONS || (!state.multiple && state.selected.len() > 1) {
        return Err(error("BROWSER_FORM_PROTOCOL: invalid selection count"));
    }
    let mut previous = None;
    let mut remaining = MAX_TEXT_BYTES;
    for option in &state.selected {
        if option.index >= MAX_DOM_OPTIONS || previous.is_some_and(|index| index >= option.index) {
            return Err(error("BROWSER_FORM_PROTOCOL: selection indices are not unique document order"));
        }
        previous = Some(option.index);
        remaining = remaining.checked_sub(option.value.len())
            .and_then(|bytes| bytes.checked_sub(option.label.len()))
            .ok_or_else(|| error("BROWSER_FORM_LIMIT: selected option text exceeds 64 KiB"))?;
    }
    Ok((state, changed))
}

fn matches_request(args: &Value, state: &Selection) -> bool {
    let Some(requested) = args["options"].as_array() else { return false };
    if requested.len() != state.selected.len() { return false; }
    let mut matched = BTreeSet::new();
    for request in requested {
        let mut matches = state.selected.iter().filter(|option| {
            request.get("index").is_some_and(|value| value.as_u64() == Some(option.index))
                || request.get("value").is_some_and(|value| value.as_str() == Some(option.value.as_str()))
                || request.get("label").is_some_and(|value| value.as_str() == Some(option.label.as_str()))
        });
        let Some(option) = matches.next() else { return false };
        if matches.next().is_some() || !matched.insert(option.index) { return false; }
    }
    true
}

fn helper_value(response: &Value) -> Result<Value> {
    if let Some(exception) = response.get("exceptionDetails") {
        // Echo only exact codes from our helper, never arbitrary page exception
        // text, option labels, source snippets or entered credentials.
        let first = exception["exception"]["description"].as_str()
            .and_then(|text| text.lines().next()).unwrap_or_default();
        let code = first.strip_prefix("Error: ").unwrap_or(first);
        const CODES: &[&str] = &[
            "BROWSER_FORM_STALE_ELEMENT", "BROWSER_FORM_DISABLED", "BROWSER_FORM_INERT",
            "BROWSER_FORM_ANCESTRY_LIMIT", "BROWSER_FORM_NOT_SELECT", "BROWSER_FORM_UNKNOWN_ACTION",
            "BROWSER_FORM_OPTION_TEXT_LIMIT", "BROWSER_FORM_OPTION_COUNT_LIMIT",
            "BROWSER_FORM_OPTIONS_CHANGED", "BROWSER_FORM_SELECTED_COUNT_LIMIT",
            "BROWSER_FORM_SELECTED_TEXT_LIMIT", "BROWSER_FORM_INVALID_OPTIONS",
            "BROWSER_FORM_NOT_MULTIPLE", "BROWSER_FORM_INVALID_OPTION",
            "BROWSER_FORM_OPTION_NOT_FOUND", "BROWSER_FORM_AMBIGUOUS_OPTION",
            "BROWSER_FORM_DUPLICATE_OPTION", "BROWSER_FORM_DISABLED_OPTION",
            "BROWSER_FORM_SELECTION_NOT_RETAINED",
        ];
        let code = CODES.iter().copied().find(|allowed| *allowed == code)
            .unwrap_or("BROWSER_FORM_OPERATION_FAILED");
        return Err(error(code));
    }
    response["result"].get("value").filter(|value| value.is_object()).cloned()
        .ok_or_else(|| error("BROWSER_FORM_PROTOCOL: helper returned no object"))
}

async fn check_document(owner: &AgentCx, cdp: &mut Cdp, expected: &Document) -> Result<()> {
    if &document(owner, cdp).await? != expected {
        return Err(error("BROWSER_FORM_DOCUMENT_CHANGED: take a new snapshot before retrying"));
    }
    Ok(())
}

async fn invoke(
    owner: &AgentCx,
    cdp: &mut Cdp,
    expected: &Document,
    id: u64,
    action: &str,
    arguments: Value,
) -> Result<Value> {
    check_document(owner, cdp, expected).await?;
    let world = cdp.command(owner, "Page.createIsolatedWorld", json!({
        "frameId": expected.frame, "worldName": "pi-browser-tools",
    })).await?;
    let context = world["executionContextId"].as_u64().filter(|id| *id > 0)
        .ok_or_else(|| error("BROWSER_FORM_PROTOCOL: isolated world has no context"))?;
    check_document(owner, cdp, expected).await?;
    let resolved = cdp.command(owner, "DOM.resolveNode", json!({
        "backendNodeId": id, "executionContextId": context,
    })).await?;
    let object = resolved["object"]["objectId"].as_str().filter(|id| !id.is_empty())
        .ok_or_else(|| error("BROWSER_FORM_STALE_ELEMENT"))?;
    let result = async {
        check_document(owner, cdp, expected).await?;
        owner.checkpoint().map_err(|_| error("browser form operation cancelled"))?;
        let response = cdp.command(owner, "Runtime.callFunctionOn", json!({
            "objectId": object, "functionDeclaration": FUNCTION,
            "arguments": [{"value": action}, {"value": arguments}],
            "returnByValue": true, "userGesture": false,
        })).await?;
        check_document(owner, cdp, expected).await?;
        helper_value(&response)
    }.await;
    // Best effort under the original operation deadline. Connection teardown
    // also releases handles; a cleanup failure never replays a mutation.
    let _ = cdp.command(owner, "Runtime.releaseObject", json!({"objectId": object})).await;
    result
}

pub(in crate::browser) async fn execute(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    refs: Option<&References>,
    args: &Value,
) -> Result<ToolOutput> {
    validate(args)?;
    let expected = document(owner, cdp).await?;
    let selector = args["selector"].as_str().ok_or_else(|| error("BROWSER_FORM_INVALID: missing selector"))?;
    let id = resolve(owner, cdp, selector, refs).await?
        .ok_or_else(|| error("BROWSER_FORM_OPTION_NOT_FOUND: selector did not match a control"))?;
    let changed = invoke(owner, cdp, &expected, id, "form_select", args["options"].clone()).await?;
    let (selected, changed) = selection(changed)?;
    let changed = changed.ok_or_else(|| error("BROWSER_FORM_PROTOCOL: missing changed flag"))?;
    if !matches_request(args, &selected) {
        return Err(error("BROWSER_FORM_SELECTION_NOT_RETAINED"));
    }
    // A second protocol command also observes microtasks queued by input/change
    // handlers. Verify the same node; never re-resolve a replacement selector.
    let confirmed = invoke(owner, cdp, &expected, id, "form_selection", Value::Null).await?;
    let (confirmed, mutation) = selection(confirmed)?;
    if mutation.is_some() || confirmed != selected {
        return Err(error("BROWSER_FORM_SELECTION_NOT_RETAINED"));
    }
    Ok(super::super::output(
        format!("Selected {} option(s) in tab {tab}{}", selected.selected.len(),
            if changed { "" } else { " (already selected)" }),
        json!({"action":"select_option", "tab":tab, "selector":selector,
            "changed":changed, "multiple":selected.multiple, "selected":selected.selected,
            "backend":"cdp", "input_method":"dom-selection"}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(options: Value) -> Value {
        json!({"action":"select_option","selector":"#choice","options":options})
    }

    #[test]
    fn option_selectors_are_explicit_bounded_and_preserve_literal_strings() {
        for options in [json!([]), json!([{"value":""}]), json!([{"label":"  雪  "}]),
            json!([{"index":0}]), json!([{"index":4095}]), json!([{"value":"a"},{"label":"b"}])] {
            assert!(validate(&args(options)).is_ok());
        }
        for options in [Value::Null, json!("a"), json!(["a"]), json!([{}]),
            json!([{"value":"a","label":"a"}]), json!([{"index":-1}]), json!([{"index":0.5}]),
            json!([{"index":4096}]), json!([{"label":true}]), json!([{"unknown":"a"}])] {
            assert!(validate(&args(options)).is_err());
        }
        assert!(validate(&args(json!([{"value":"x".repeat(MAX_TEXT_BYTES)}]))).is_ok());
        assert!(validate(&args(json!([{"value":"雪".repeat(MAX_TEXT_BYTES / 3 + 1)}]))).is_err());
        assert!(validate(&args(json!(vec![json!({"value":"a"}); MAX_OPTIONS + 1]))).is_err());
    }

    #[test]
    fn form_actions_do_not_ignore_foreign_options_or_dialog_consent() {
        for field in ["text","checked","script","dialog_response","accept","files"] {
            let mut value = args(json!([]));
            value[field] = json!("PRIVATE-CANARY");
            let error = validate(&value).unwrap_err().to_string();
            assert!(!error.contains("PRIVATE-CANARY"));
        }
        for selector in [Value::Null, json!(""), json!(3)] {
            let mut value = args(json!([])); value["selector"] = selector;
            assert!(validate(&value).is_err());
        }
    }

    #[test]
    fn selection_results_require_unique_ordered_bounded_indices() {
        for selected in [json!([{"index":4096,"value":"a","label":"a"}]),
            json!([{"index":1,"value":"a","label":"a"},{"index":1,"value":"b","label":"b"}]),
            json!([{"index":2,"value":"a","label":"a"},{"index":1,"value":"b","label":"b"}]),
            json!([{"index":0,"value":null,"label":"a"}])] {
            assert!(selection(json!({"multiple":true,"selected":selected})).is_err());
        }
        assert!(selection(json!({"multiple":false,"selected":[],"changed":"yes"})).is_err());
    }

    #[test]
    fn verification_matches_requested_options_not_only_self_consistent_replies() {
        let (state, _) = selection(json!({"multiple":true,"selected":[
            {"index":0,"value":"a","label":"First"},{"index":2,"value":"b","label":"Second"}
        ]})).unwrap();
        assert!(matches_request(&args(json!([{"label":"Second"},{"value":"a"}])), &state));
        for options in [json!([]), json!([{"value":"a"}]),
            json!([{"value":"a"},{"index":0}]), json!([{"value":"x"},{"index":2}])] {
            assert!(!matches_request(&args(options), &state));
        }
    }

    #[test]
    fn helper_errors_echo_only_known_static_codes() {
        let failure = |message: &str| json!({"exceptionDetails":{"exception":{"description":message}}});
        assert!(helper_value(&failure("Error: BROWSER_FORM_AMBIGUOUS_OPTION\n at helper"))
            .unwrap_err().to_string().contains("BROWSER_FORM_AMBIGUOUS_OPTION"));
        let error = helper_value(&failure("Error: PRIVATE-CANARY")).unwrap_err().to_string();
        assert!(!error.contains("PRIVATE-CANARY"));
        assert!(error.contains("BROWSER_FORM_OPERATION_FAILED"));
        assert!(helper_value(&json!({"result":{"value":true}})).is_err());
    }
}
