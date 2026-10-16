//! One document/focus contract for main-page and frame-local text input.
//! Selection is resolved once. A failed key acknowledgement never replays down.

use super::{Cdp, Document, References, document, element_call, frame_input, key_event, resolve};
use crate::agent_cx::AgentCx;
use crate::browser::{output, required};
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde::Deserialize;
use serde_json::{Value, json};

fn error(message: &'static str) -> Error {
    Error::tool("browser", message)
}

fn admit_document(value: &Document) -> Result<()> {
    for id in [&value.frame, &value.loader] {
        if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
            return Err(error("BROWSER_INPUT_DOCUMENT: incomplete document identity"));
        }
    }
    Ok(())
}

async fn check_document(owner: &AgentCx, cdp: &mut Cdp, expected: &Document) -> Result<()> {
    owner.checkpoint().map_err(|_| error("BROWSER_INPUT_CANCELLED"))?;
    if document(owner, cdp).await? != *expected {
        return Err(error("BROWSER_INPUT_STALE: document changed during input; no replacement target was selected"));
    }
    Ok(())
}

async fn check_focus(owner: &AgentCx, cdp: &mut Cdp, expected: &Document, node: u64) -> Result<()> {
    check_document(owner, cdp, expected).await?;
    // The existing isolated-world check includes document.hasFocus and shadow
    // root activeElement. Main documents need it just as much as child frames:
    // select/input handlers can redirect focus after edit preparation returns.
    frame_input::ensure_focus(owner, cdp, node).await?;
    check_document(owner, cdp, expected).await
}

fn key_pair(key: &str) -> Result<(Value, Value)> {
    let mut down = key_event(key)?;
    down["type"] = json!("keyDown");
    let mut up = down.clone();
    let fields = up.as_object_mut().ok_or_else(|| error("invalid key event"))?;
    fields.remove("text");
    fields.remove("commands");
    up["type"] = json!("keyUp");
    Ok((down, up))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ValueFill {
    kind: String,
    value: String,
    changed: bool,
}

impl ValueFill {
    fn parse(value: Value) -> Result<Self> {
        let result: Self = serde_json::from_value(value)
            .map_err(|_| error("BROWSER_VALUE_PROTOCOL: invalid fill result"))?;
        if !matches!(result.kind.as_str(), "date" | "time" | "month" | "week" |
            "datetime-local" | "color" | "range") || result.value.len() > 4096
            || result.value.chars().any(char::is_control)
            || (matches!(result.kind.as_str(), "color" | "range") && result.value.is_empty())
        {
            return Err(error("BROWSER_VALUE_PROTOCOL: invalid fill result"));
        }
        Ok(result)
    }
}

fn value_result(tab: &str, selector: &str, requested: &str, filled: &ValueFill) -> ToolOutput {
    output(
        format!("Filled {} control {selector} in tab {tab}; retained value verified using DOM setters, not trusted keyboard input", filled.kind),
        json!({"action":"fill","selector":selector,"tab":tab,"backend":"cdp",
            "input_method":"dom-value","input_type":filled.kind,"changed":filled.changed,
            "value_normalized":filled.value != requested,"char_count":requested.chars().count()}),
    )
}

async fn press(owner: &AgentCx, cdp: &mut Cdp, key: &str) -> Result<()> {
    let (down, up) = key_pair(key)?;
    let page = cdp.page_session_id()?.to_owned();
    let pressed = cdp.command(owner, "Input.dispatchKeyEvent", down).await;
    let released = if cdp.frame_selected() || pressed.is_err() {
        // Keydown may navigate/detach the selected child or return an ambiguous
        // failure. Release the key on the original page, not a replacement child.
        // Scoped operations do not permit implicit/expected dialog handling.
        cdp.frame_command(owner, &page, "Input.dispatchKeyEvent", up).await
    } else {
        // Preserve explicit expected-dialog handling for main-document presses.
        cdp.command(owner, "Input.dispatchKeyEvent", up).await
    };
    if pressed.is_err() || released.is_err() {
        return Err(error("BROWSER_KEY_INTERRUPTED: a key pair was not fully acknowledged; one release was attempted on the original page without replay. Effects may already have occurred"));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(super) async fn execute(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    refs: Option<&References>,
    args: &Value,
) -> Result<ToolOutput> {
    let action = required(args, "action")?;
    if !matches!(action, "press" | "type" | "fill") {
        return Err(error("BROWSER_INPUT_USAGE: expected press, type or fill"));
    }
    let expected = document(owner, cdp).await?;
    admit_document(&expected)?;
    cdp.command(owner, "Page.bringToFront", json!({})).await?;
    check_document(owner, cdp, &expected).await?;

    if action == "press" {
        if let Some(selector) = args.get("selector").and_then(Value::as_str) {
            let node = resolve(owner, cdp, selector, refs).await?
                .ok_or_else(|| error("selector did not match an element"))?;
            check_document(owner, cdp, &expected).await?;
            if element_call(owner, cdp, node, "focus", json!({})).await? != true {
                return Err(error("BROWSER_INPUT_FOCUS: control could not receive focus"));
            }
            check_focus(owner, cdp, &expected, node).await?;
        } else if cdp.frame_selected() {
            return Err(error("frame-scoped keypress requires a selector"));
        }
        let key = required(args, "key")?;
        press(owner, cdp, key).await?;
        if cdp.frame_selected() {
            check_document(owner, cdp, &expected).await?;
        }
        // Main-document Enter/Tab can intentionally navigate or move focus.
        // The optional navigation wait owns destination verification, not keyup.
        return Ok(output(
            format!("Dispatched keypress {key} in tab {tab}"),
            json!({"action":action,"key":key,"tab":tab,"backend":"cdp"}),
        ));
    }

    let selector = required(args, "selector")?;
    let text = required(args, "text")?;
    let node = resolve(owner, cdp, selector, refs).await?
        .ok_or_else(|| error("selector did not match an element"))?;
    check_document(owner, cdp, &expected).await?;
    cdp.command(owner, "DOM.scrollIntoViewIfNeeded", json!({"backendNodeId":node})).await?;
    check_document(owner, cdp, &expected).await?;
    let mut options = json!({"replace":action == "fill"});
    if action == "fill" { options["text"] = json!(text); }
    let prepared = element_call(owner, cdp, node, "edit", options).await?;
    if prepared.is_object() && action == "fill" {
        let filled = ValueFill::parse(prepared)?;
        check_document(owner, cdp, &expected).await?;
        // A separate protocol turn observes microtasks from input/change.
        // Never resolve the selector again or retry a rejected mutation.
        if element_call(owner, cdp, node, "verify_value", json!({
            "kind":filled.kind,"value":filled.value,
        })).await? != true {
            return Err(error("BROWSER_VALUE_REJECTED: control did not retain the requested fill; no retry"));
        }
        check_document(owner, cdp, &expected).await?;
        return Ok(value_result(tab, selector, text, &filled));
    }
    if prepared != true {
        return Err(error("BROWSER_INPUT_EDIT: control rejected edit preparation"));
    }
    check_focus(owner, cdp, &expected, node).await?;
    if text.is_empty() && action == "fill" {
        press(owner, cdp, "Backspace").await?;
    } else if !text.is_empty() {
        cdp.command(owner, "Input.insertText", json!({"text":text})).await?;
    }
    check_document(owner, cdp, &expected).await?;
    if action == "fill" {
        if element_call(owner, cdp, node, "verify_fill", json!({"text":text})).await? != true {
            return Err(error("field did not retain the requested text; the page may have rejected or transformed the input"));
        }
        check_document(owner, cdp, &expected).await?;
    }
    let count = text.chars().count();
    Ok(output(
        format!("Dispatched text input to {selector} in tab {tab} ({count} chars)"),
        json!({"action":action,"selector":selector,"char_count":count,"backend":"cdp"}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_fill_results_are_typed_bounded_and_never_fall_back_to_key_input() {
        for kind in ["date", "time", "month", "week", "datetime-local", "color", "range"] {
            assert!(ValueFill::parse(json!({"kind":kind,"value":"1","changed":true})).is_ok());
        }
        for invalid in [json!(true), json!({"kind":"date","value":"2026-10-16"}),
            json!({"kind":"text","value":"PRIVATE-CANARY","changed":true}),
            json!({"kind":"date","value":"x".repeat(4097),"changed":true}),
            json!({"kind":"date","value":"PRIVATE-CANARY\n","changed":false}),
            json!({"kind":"color","value":"","changed":false}),
            json!({"kind":"date","value":"","changed":"true"}),
            json!({"kind":"date","value":"","changed":true,"extra":1})] {
            let failure = ValueFill::parse(invalid).unwrap_err().to_string();
            assert!(failure.contains("BROWSER_VALUE_PROTOCOL"));
            assert!(!failure.contains("PRIVATE-CANARY"));
        }
    }

    #[test]
    fn value_fill_output_reports_normalization_without_returning_field_values() {
        let filled = ValueFill { kind:"datetime-local".into(), value:"PRIVATE-CANARY".into(), changed:false };
        let result = value_result("app", "#date", "PRIVATE-CANARY-REQUEST", &filled);
        let details = result.details.unwrap();
        assert_eq!(details["input_method"], "dom-value");
        assert_eq!(details["changed"], false);
        assert_eq!(details["value_normalized"], true);
        assert!(!details.to_string().contains("PRIVATE-CANARY"));
        for block in result.content {
            if let crate::model::ContentBlock::Text(text) = block {
                assert!(!text.text.contains("PRIVATE-CANARY"));
            }
        }
    }

    #[test]
    fn key_release_retains_identity_without_repeating_text_or_editing_commands() {
        for key in ["Enter", "Tab", "Ctrl+a", "Ctrl+Shift+a", "+", "🦀"] {
            let (down, up) = key_pair(key).unwrap();
            assert_eq!(down["type"], "keyDown");
            assert_eq!(up["type"], "keyUp");
            for field in ["key", "code", "windowsVirtualKeyCode", "modifiers"] {
                assert_eq!(down[field], up[field]);
            }
            assert!(up.get("text").is_none());
            assert!(up.get("commands").is_none());
        }
        assert_eq!(key_pair("Ctrl+a").unwrap().0["commands"], json!(["selectAll"]));
        assert!(key_pair("Ctrl+").is_err());
    }

    #[test]
    fn input_pins_complete_bounded_document_incarnations() {
        let good = Document { frame: "frame".into(), loader: "loader".into() };
        assert!(admit_document(&good).is_ok());
        for id in [String::new(), "x".repeat(257), "PRIVATE-CANARY\n".into()] {
            for bad in [Document { frame: id.clone(), ..good.clone() },
                Document { loader: id.clone(), ..good.clone() }] {
                let failure = admit_document(&bad).unwrap_err();
                assert!(!failure.to_string().contains("PRIVATE-CANARY"));
            }
        }
    }
}
