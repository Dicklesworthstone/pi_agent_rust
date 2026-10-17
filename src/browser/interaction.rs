//! Real accessibility snapshots and DOM-backed input dispatch. References name
//! backend node IDs in one document, not CSS selectors that may bind replacements.

use super::cdp::{Cdp, evaluation_value};
use super::{BrowserElementRef, BrowserSnapshot, output, required};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

mod frame_input;
pub(super) mod upload;

const ELEMENT_FUNCTION: &str = include_str!("dom.js");
const MAX_ELEMENTS: usize = 200;
const MAX_AX_NODES: usize = 1000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Document {
    frame: String,
    loader: String,
}

#[derive(Default)]
pub(super) struct References {
    document: Option<Document>,
    nodes: BTreeMap<String, u64>,
}

async fn document(owner: &AgentCx, cdp: &mut Cdp) -> Result<Document> {
    let frame = cdp.frame_document(owner).await?;
    Ok(Document {
        frame: required(&frame, "id")?.into(),
        loader: required(&frame, "loaderId")?.into(),
    })
}

/// Walks the accessibility tree, describes each element and assigns stable
/// `@eN` references, reusing names from `previous` while the document is
/// unchanged. Returns the compact element inventory plus whether it hit the
/// `MAX_ELEMENTS` cap.
async fn collect_elements(
    owner: &AgentCx,
    cdp: &mut Cdp,
    nodes: &[Value],
    previous: Option<&References>,
    doc: &Document,
    next_ref: &mut u64,
) -> Result<(Vec<BrowserElementRef>, References, bool)> {
    let old_ids: BTreeMap<u64, &String> = previous
        .filter(|refs| refs.document.as_ref() == Some(doc))
        .map(|refs| refs.nodes.iter().map(|(name, id)| (*id, name)).collect())
        .unwrap_or_default();
    let mut refs = References {
        document: Some(doc.clone()),
        nodes: BTreeMap::new(),
    };
    let mut elements = Vec::new();
    let mut seen = BTreeSet::new();
    let mut truncated = false;
    for node in nodes {
        if node["ignored"] == true {
            continue;
        }
        let role = node["role"]["value"].as_str().unwrap_or_default();
        if matches!(
            role,
            "" | "none" | "generic" | "RootWebArea" | "InlineTextBox"
        ) {
            continue;
        }
        let Some(backend_id) = node["backendDOMNodeId"].as_u64() else {
            continue;
        };
        if !seen.insert(backend_id) {
            continue;
        }
        if elements.len() >= MAX_ELEMENTS {
            truncated = true;
            break;
        }
        let described = cdp
            .command(
                owner,
                "DOM.describeNode",
                json!({"backendNodeId": backend_id, "depth": 0}),
            )
            .await?;
        let tag = described["node"]["localName"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| described["node"]["nodeName"].as_str())
            .ok_or_else(|| Error::tool("browser", "DOM node has no name"))?
            .to_owned();
        let id = if let Some(id) = old_ids.get(&backend_id) {
            (*id).clone()
        } else {
            *next_ref = next_ref
                .checked_add(1)
                .ok_or_else(|| Error::tool("browser", "element reference IDs exhausted"))?;
            format!("@e{}", *next_ref)
        };
        refs.nodes.insert(id.clone(), backend_id);
        // Accessible names are useful context. Do not copy editable values or
        // password contents into the transcript as a side effect of inspection.
        let text = node["name"]["value"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .take(1000)
            .collect();
        elements.push(BrowserElementRef {
            ref_id: id.clone(),
            tag,
            role: role.into(),
            text,
            selector: id,
        });
    }
    Ok((elements, refs, truncated))
}

pub(super) async fn snapshot(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    previous: Option<&References>,
    next_ref: &mut u64,
    include_tree: bool,
) -> Result<(ToolOutput, References)> {
    let doc = document(owner, cdp).await?;
    let response = cdp
        .command(
            owner,
            "Accessibility.getFullAXTree",
            json!({"frameId": doc.frame}),
        )
        .await?;
    let nodes = response
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::tool("browser", "accessibility tree has no nodes"))?;
    let (elements, refs, truncated) =
        collect_elements(owner, cdp, nodes, previous, &doc, next_ref).await?;
    let metadata = cdp
        .evaluate(owner, "({url: location.href, title: document.title})")
        .await?;
    if document(owner, cdp).await? != doc {
        return Err(Error::tool(
            "browser",
            "page navigated during snapshot; take a new snapshot",
        ));
    }
    let url = required(&metadata, "url")?.to_owned();
    let title = required(&metadata, "title")?.to_owned();
    let lines = elements
        .iter()
        .map(|e| {
            format!(
                "- [{}] <{}> (role: {}) {}",
                e.ref_id,
                e.tag,
                e.role,
                json!(e.text)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let summary = format!(
        "Page Snapshot for [{tab}] {title:?}:\n{lines}{}",
        if truncated {
            "\n[Snapshot truncated]"
        } else {
            ""
        }
    );
    let snapshot = BrowserSnapshot {
        url,
        title,
        elements,
        summary: summary.clone(),
    };
    let reference_scope = if cdp.frame_selected() {
        "selected-frame document"
    } else {
        "main-frame document"
    };
    let mut details = json!({
        "snapshot": snapshot, "truncated": truncated, "backend": "cdp",
        "reference_scope": reference_scope, "frame_id": doc.frame,
    });
    if include_tree {
        // Return the actual accessibility tree, bounded independently of the
        // compact element inventory. Values are omitted for the same reason above.
        let tree: Vec<_> = nodes.iter().take(MAX_AX_NODES).map(|node| json!({
            "nodeId": node["nodeId"], "parentId": node["parentId"], "childIds": node["childIds"],
            "role": node["role"], "name": node["name"], "ignored": node["ignored"],
            "backendDOMNodeId": node["backendDOMNodeId"]
        })).collect();
        details["ax_tree"] = json!({"nodes": tree, "truncated": nodes.len() > MAX_AX_NODES});
    }
    Ok((output(summary, details), refs))
}

async fn resolve(
    owner: &AgentCx,
    cdp: &mut Cdp,
    selector: &str,
    refs: Option<&References>,
) -> Result<Option<u64>> {
    if selector.starts_with('@') {
        let refs = refs.ok_or_else(|| {
            Error::tool(
                "browser",
                "no snapshot references for this tab; take a snapshot first",
            )
        })?;
        if refs.document.as_ref() != Some(&document(owner, cdp).await?) {
            return Err(Error::tool(
                "browser",
                "stale element reference after navigation; take a new snapshot",
            ));
        }
        return refs.nodes.get(selector).copied().map(Some).ok_or_else(|| {
            Error::tool(
                "browser",
                "unknown or expired element reference; take a new snapshot",
            )
        });
    }
    if cdp.frame_selected() {
        return frame_input::resolve(owner, cdp, selector).await;
    }
    let root = cdp
        .command(owner, "DOM.getDocument", json!({"depth": 0}))
        .await?;
    let root_id = root["root"]["nodeId"]
        .as_u64()
        .ok_or_else(|| Error::tool("browser", "document has no node ID"))?;
    let found = cdp
        .command(
            owner,
            "DOM.querySelector",
            json!({"nodeId": root_id, "selector": selector}),
        )
        .await?;
    match found["nodeId"].as_u64() {
        Some(0) => Ok(None),
        Some(id) => {
            let described = cdp
                .command(owner, "DOM.describeNode", json!({"nodeId": id, "depth": 0}))
                .await?;
            described["node"]["backendNodeId"]
                .as_u64()
                .map(Some)
                .ok_or_else(|| Error::tool("browser", "element has no backend node ID"))
        }
        None => Err(Error::tool("browser", "selector query returned no node ID")),
    }
}

async fn element_call(
    owner: &AgentCx,
    cdp: &mut Cdp,
    backend_id: u64,
    action: &str,
    options: Value,
) -> Result<Value> {
    let doc = document(owner, cdp).await?;
    let world = cdp
        .command(
            owner,
            "Page.createIsolatedWorld",
            json!({"frameId": doc.frame, "worldName": "pi-browser-tools"}),
        )
        .await?;
    let context = world["executionContextId"]
        .as_u64()
        .ok_or_else(|| Error::tool("browser", "isolated world has no execution context"))?;
    if cdp.frame_selected() && document(owner, cdp).await? != doc {
        return Err(Error::tool(
            "browser",
            "selected frame navigated before element access",
        ));
    }
    let resolved = cdp
        .command(
            owner,
            "DOM.resolveNode",
            json!({"backendNodeId": backend_id, "executionContextId": context}),
        )
        .await?;
    let object_id = required(&resolved["object"], "objectId")?;
    let response = cdp
        .command(
            owner,
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id, "functionDeclaration": ELEMENT_FUNCTION,
                "arguments": [{"value": action}, {"value": options}], "returnByValue": true
            }),
        )
        .await;
    // Release promptly during polling. Dropping the connection also releases its
    // object handles if cancellation prevents this best-effort cleanup.
    let _ = cdp
        .command(
            owner,
            "Runtime.releaseObject",
            json!({"objectId": object_id}),
        )
        .await;
    evaluation_value(&response?)
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
    let scoped = cdp.frame_selected();
    if scoped && matches!(action, "press" | "type" | "fill") {
        cdp.command(owner, "Page.bringToFront", json!({})).await?;
    }
    if action == "press" {
        if let Some(selector) = args.get("selector").and_then(Value::as_str) {
            let id = resolve(owner, cdp, selector, refs)
                .await?
                .ok_or_else(|| Error::tool("browser", "selector did not match an element"))?;
            element_call(owner, cdp, id, "focus", json!({})).await?;
            if scoped {
                frame_input::ensure_focus(owner, cdp, id).await?;
            }
        } else if scoped {
            return Err(Error::tool(
                "browser",
                "frame-scoped keypress requires a selector",
            ));
        }
        let key = required(args, "key")?;
        press(owner, cdp, key).await?;
        return Ok(output(
            format!("Dispatched keypress {key} in tab {tab}"),
            json!({"action": action, "key": key, "tab": tab, "backend": "cdp"}),
        ));
    }
    if action == "scroll" {
        let x = delta(args, "delta_x", 0.0)?;
        let y = delta(args, "delta_y", 600.0)?;
        if scoped {
            let selector = required(args, "selector")?;
            let id = resolve(owner, cdp, selector, refs)
                .await?
                .ok_or_else(|| Error::tool("browser", "selector did not match an element"))?;
            cdp.command(
                owner,
                "DOM.scrollIntoViewIfNeeded",
                json!({"backendNodeId": id}),
            )
            .await?;
            let (point_x, point_y) = frame_input::click_point(owner, cdp, id).await?;
            cdp.command(
                owner,
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseWheel", "x": point_x, "y": point_y, "deltaX": x, "deltaY": y,
                }),
            )
            .await?;
            return Ok(output(
                format!("Scrolled over {selector} in the selected frame of tab {tab}"),
                json!({"action": action, "tab": tab, "selector": selector, "delta_x": x, "delta_y": y, "backend": "cdp"}),
            ));
        }
        let metrics = cdp
            .command(owner, "Page.getLayoutMetrics", json!({}))
            .await?;
        let viewport = metrics
            .get("cssVisualViewport")
            .or_else(|| metrics.get("visualViewport"))
            .ok_or_else(|| Error::tool("browser", "page has no viewport metrics"))?;
        let width = viewport["clientWidth"]
            .as_f64()
            .filter(|v| *v > 0.0)
            .ok_or_else(|| Error::tool("browser", "invalid viewport width"))?;
        let height = viewport["clientHeight"]
            .as_f64()
            .filter(|v| *v > 0.0)
            .ok_or_else(|| Error::tool("browser", "invalid viewport height"))?;
        cdp.command(owner, "Input.dispatchMouseEvent", json!({"type": "mouseWheel", "x": width / 2.0, "y": height / 2.0, "deltaX": x, "deltaY": y})).await?;
        return Ok(output(
            format!("Scrolled tab {tab} viewport"),
            json!({"action": action, "tab": tab, "delta_x": x, "delta_y": y, "backend": "cdp"}),
        ));
    }
    let selector = required(args, "selector")?;
    if action == "wait_for" {
        loop {
            if let Some(id) = resolve(owner, cdp, selector, refs).await?
                && element_call(owner, cdp, id, "visible", json!({})).await? == true
            {
                return Ok(output(
                    format!("Selector {selector} is visible in tab {tab}"),
                    json!({"selector": selector, "found": true, "backend": "cdp"}),
                ));
            }
            owner.time().sleep(Duration::from_millis(100)).await;
        }
    }
    let id = resolve(owner, cdp, selector, refs)
        .await?
        .ok_or_else(|| Error::tool("browser", "selector did not match an element"))?;
    cdp.command(
        owner,
        "DOM.scrollIntoViewIfNeeded",
        json!({"backendNodeId": id}),
    )
    .await?;
    match action {
        "click" => {
            let (x, y) = if scoped {
                let (x, y) = frame_input::click_point(owner, cdp, id).await?;
                (f64::from(x), f64::from(y))
            } else {
                let point = element_call(owner, cdp, id, "point", json!({})).await?;
                let x = point["x"]
                    .as_f64()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| {
                        Error::tool("browser", "element has no clickable x coordinate")
                    })?;
                let y = point["y"]
                    .as_f64()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| {
                        Error::tool("browser", "element has no clickable y coordinate")
                    })?;
                (x, y)
            };
            for (kind, buttons) in [("mousePressed", 1), ("mouseReleased", 0)] {
                cdp.command(owner, "Input.dispatchMouseEvent", json!({"type": kind, "x": x, "y": y, "button": "left", "buttons": buttons, "clickCount": 1})).await?;
            }
            Ok(output(
                format!("Clicked element {selector} in tab {tab}"),
                json!({"action": action, "selector": selector, "tab": tab, "backend": "cdp"}),
            ))
        }
        "type" | "fill" => {
            let text = required(args, "text")?;
            element_call(owner, cdp, id, "edit", json!({"replace": action == "fill"})).await?;
            if scoped {
                frame_input::ensure_focus(owner, cdp, id).await?;
            }
            if text.is_empty() && action == "fill" {
                press(owner, cdp, "Backspace").await?;
            } else if !text.is_empty() {
                cdp.command(owner, "Input.insertText", json!({"text": text}))
                    .await?;
            }
            if scoped {
                document(owner, cdp).await?;
            }
            if action == "fill"
                && element_call(owner, cdp, id, "verify_fill", json!({"text": text})).await? != true
            {
                return Err(Error::tool(
                    "browser",
                    "field did not retain the requested text; the page may have rejected or transformed the input",
                ));
            }
            let count = text.chars().count();
            Ok(output(
                format!("Dispatched text input to {selector} in tab {tab} ({count} chars)"),
                json!({"action": action, "selector": selector, "char_count": count, "backend": "cdp"}),
            ))
        }
        _ => Err(Error::tool(
            "browser",
            format!("unsupported input action: {action}"),
        )),
    }
}

pub(super) fn delta(args: &Value, key: &str, default: f64) -> Result<f64> {
    args.get(key).map_or(Ok(default), |value| {
        value
            .as_f64()
            .filter(|v| v.is_finite() && v.abs() <= 100_000.0)
            .ok_or_else(|| {
                Error::tool(
                    "browser",
                    format!("{key} must be a finite number in -100000..=100000"),
                )
            })
    })
}

pub(super) fn key_event(key: &str) -> Result<Value> {
    let mut parts: Vec<_> = if key == "+" {
        vec![key]
    } else {
        key.split('+').collect()
    };
    let base = parts
        .pop()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::tool("browser", "missing key name"))?;
    let mut modifiers = 0_u8;
    for modifier in parts {
        let bit = match modifier.to_ascii_lowercase().as_str() {
            "alt" => 1,
            "ctrl" | "control" => 2,
            "meta" | "cmd" | "command" => 4,
            "shift" => 8,
            _ => {
                return Err(Error::tool(
                    "browser",
                    format!("unknown key modifier: {modifier}"),
                ));
            }
        };
        if modifiers & bit != 0 {
            return Err(Error::tool("browser", "duplicate key modifier"));
        }
        modifiers |= bit;
    }
    let (name, code, virtual_key, text) = match base {
        "Enter" | "Return" => ("Enter".into(), "Enter".into(), 13, Some("\r".into())),
        "Tab" => ("Tab".into(), "Tab".into(), 9, None),
        "Escape" | "Esc" => ("Escape".into(), "Escape".into(), 27, None),
        "Backspace" => (base.into(), base.into(), 8, None),
        "Delete" => (base.into(), base.into(), 46, None),
        "ArrowLeft" => (base.into(), base.into(), 37, None),
        "ArrowUp" => (base.into(), base.into(), 38, None),
        "ArrowRight" => (base.into(), base.into(), 39, None),
        "ArrowDown" => (base.into(), base.into(), 40, None),
        "Home" => (base.into(), base.into(), 36, None),
        "End" => (base.into(), base.into(), 35, None),
        "PageUp" => (base.into(), base.into(), 33, None),
        "PageDown" => (base.into(), base.into(), 34, None),
        "Space" | " " => (" ".into(), "Space".into(), 32, Some(" ".into())),
        _ if base.chars().count() == 1 => {
            let character = base
                .chars()
                .next()
                .ok_or_else(|| Error::tool("browser", "missing key"))?;
            let name = if modifiers & 8 != 0 {
                character.to_uppercase().collect::<String>()
            } else {
                base.to_owned()
            };
            let code = if character.is_ascii_alphabetic() {
                format!("Key{}", character.to_ascii_uppercase())
            } else if character.is_ascii_digit() {
                format!("Digit{character}")
            } else {
                String::new()
            };
            let vk = if character.is_ascii_alphanumeric() {
                u32::from(character.to_ascii_uppercase())
            } else {
                0
            };
            (name.clone(), code, vk, Some(name))
        }
        _ => return Err(Error::tool("browser", format!("unsupported key: {base}"))),
    };
    let mut event = json!({"key": name, "code": code, "windowsVirtualKeyCode": virtual_key, "modifiers": modifiers});
    if modifiers.trailing_zeros() >= 3
        && let Some(text) = text
    {
        event["text"] = json!(text);
    }
    if modifiers & 6 != 0 && base.eq_ignore_ascii_case("a") {
        event["commands"] = json!(["selectAll"]);
    }
    Ok(event)
}

async fn press(owner: &AgentCx, cdp: &mut Cdp, key: &str) -> Result<()> {
    let mut event = key_event(key)?;
    event["type"] = json!("keyDown");
    cdp.command(owner, "Input.dispatchKeyEvent", event.clone())
        .await?;
    let object = event
        .as_object_mut()
        .ok_or_else(|| Error::tool("browser", "invalid key event"))?;
    object.remove("text");
    object.remove("commands");
    event["type"] = json!("keyUp");
    cdp.command(owner, "Input.dispatchKeyEvent", event).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_chords_preserve_modifiers_without_inserting_shortcut_text() {
        let key = key_event("Ctrl+Shift+a").unwrap();
        assert_eq!(key["modifiers"], 10);
        assert_eq!(key["key"], "A");
        assert!(key.get("text").is_none());
        assert_eq!(key_event("Enter").unwrap()["text"], "\r");
        assert_eq!(key_event("+").unwrap()["text"], "+");
        assert!(key_event("Ctrl+Control+a").is_err());
        assert!(key_event("Hyper+a").is_err());
        assert!(key_event("Ctrl+").is_err());
    }

    #[test]
    fn invalid_scroll_parameters_are_not_silently_accepted() {
        assert!(delta(&json!({"delta_y": "far"}), "delta_y", 600.0).is_err());
        assert!(delta(&json!({"delta_y": 100_001}), "delta_y", 600.0).is_err());
        assert_eq!(delta(&json!({}), "delta_y", 600.0).unwrap(), 600.0);
    }
}
