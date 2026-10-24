//! Semantic locators resolve to the same document-owned backend IDs as CSS.
//!
//! Role and label names come from Chromium's accessibility computation. DOM
//! locators run in an isolated world, never interpolate input as JavaScript,
//! and reject ambiguity instead of silently clicking the first matching node.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::{Value, json};

use super::{Cdp, Document, document};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};

const MAX_SELECTOR_BYTES: usize = 16 * 1024;
const MAX_AX_NODES: usize = 20_000;
const MAX_CANDIDATES: usize = 128;
const DOM_QUERY: &str = include_str!("selectors.js");
const NODE_IN_DOCUMENT: &str = "function() { return this.nodeType === 1 && this.isConnected && this.ownerDocument === document; }";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Text,
    Label,
    Placeholder,
    Testid,
    Alt,
    Title,
    Role,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub(super) struct Locator {
    kind: Kind,
    value: String,
    name: Option<String>,
    exact: bool,
}

fn error(code: &'static str) -> Error {
    Error::tool("browser", code)
}

fn invalid() -> Error {
    error("BROWSER_SELECTOR_USAGE: malformed or empty semantic selector")
}

fn normalized(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Unknown prefixes remain ordinary CSS. Known prefixes cannot fall back to
/// CSS after a syntax error. No selector text is echoed by parser diagnostics.
pub(super) fn parse(selector: &str) -> Result<Option<Locator>> {
    let Some((prefix, value)) = selector.split_once('/') else {
        return Ok(None);
    };
    let kind = match prefix {
        "text" => Kind::Text,
        "label" => Kind::Label,
        "placeholder" => Kind::Placeholder,
        "testid" => Kind::Testid,
        "alt" => Kind::Alt,
        "title" => Kind::Title,
        "role" => Kind::Role,
        _ => return Ok(None),
    };
    if selector.len() > MAX_SELECTOR_BYTES || selector.contains('\0') {
        return Err(invalid());
    }
    if kind != Kind::Role {
        let value = if kind == Kind::Testid {
            value.to_string()
        } else {
            normalized(value)
        };
        if value.is_empty() {
            return Err(invalid());
        }
        return Ok(Some(Locator {
            kind,
            value,
            name: None,
            exact: kind == Kind::Testid,
        }));
    }
    let (role, filter) = value
        .split_once('[')
        .map_or((value, None), |(role, filter)| (role, Some(filter)));
    if role.is_empty()
        || role.len() > 64
        || !role
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
    {
        return Err(invalid());
    }
    let (name, exact) = if let Some(filter) = filter {
        let encoded = filter.strip_prefix("name=").ok_or_else(invalid)?;
        // A JSON string supplies unambiguous quoting and Unicode escapes.
        let mut values = serde_json::Deserializer::from_str(encoded).into_iter::<String>();
        let name = values.next().ok_or_else(invalid)?.map_err(|_| invalid())?;
        let suffix = &encoded[values.byte_offset()..];
        let exact = if suffix.trim() == "]" {
            false
        } else if suffix.starts_with(char::is_whitespace) && suffix.trim() == "exact]" {
            true
        } else {
            return Err(invalid());
        };
        let name = normalized(&name);
        if name.is_empty() || name.contains('\0') {
            return Err(invalid());
        }
        (Some(name), exact)
    } else {
        (None, false)
    };
    Ok(Some(Locator {
        kind,
        value: role.to_string(),
        name,
        exact,
    }))
}

/// Runs before browser launch and mock dispatch. Never silently ignore a
/// semantic selector on an action that does not actually consume selectors.
pub(in crate::browser) fn validate_input(args: &Value) -> Result<bool> {
    let Some(selector) = args.get("selector").and_then(Value::as_str) else {
        return Ok(false);
    };
    if parse(selector)?.is_none() {
        return Ok(false);
    }
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !matches!(
        action,
        "click"
            | "hover"
            | "drag"
            | "type"
            | "fill"
            | "press"
            | "wait_for"
            | "select_option"
            | "set_checked"
            | "upload"
    ) && !(action == "scroll" && args.get("frame").is_some())
    {
        return Err(error(
            "BROWSER_SELECTOR_USAGE: semantic selectors require an element input action; scroll requires an explicit frame",
        ));
    }
    Ok(true)
}

fn ax_candidates(nodes: &[Value], locator: &Locator) -> Result<BTreeSet<u64>> {
    if nodes.len() > MAX_AX_NODES {
        return Err(error(
            "BROWSER_SELECTOR_LIMIT: accessibility tree exceeds 20000 nodes",
        ));
    }
    let mut candidates = BTreeSet::new();
    let mut name_bytes = 0_usize;
    for node in nodes {
        if node["ignored"] == true {
            continue;
        }
        let role = node["role"]["value"].as_str().unwrap_or_default();
        let role_matches = if locator.kind == Kind::Role {
            role == locator.value.as_str()
        } else {
            matches!(
                role,
                "textbox"
                    | "searchbox"
                    | "combobox"
                    | "listbox"
                    | "checkbox"
                    | "radio"
                    | "switch"
                    | "slider"
                    | "spinbutton"
                    | "button"
            )
        };
        if !role_matches {
            continue;
        }
        let raw_name = node["name"]["value"].as_str().unwrap_or_default();
        name_bytes = name_bytes.saturating_add(raw_name.len());
        if raw_name.len() > MAX_SELECTOR_BYTES || name_bytes > 4 * 1024 * 1024 {
            return Err(error(
                "BROWSER_SELECTOR_LIMIT: accessibility names exceed the lookup budget",
            ));
        }
        let wanted = if locator.kind == Kind::Label {
            Some(locator.value.as_str())
        } else {
            locator.name.as_deref()
        };
        if let Some(wanted) = wanted {
            let actual = normalized(raw_name);
            if !(if locator.exact {
                actual == wanted
            } else {
                actual.contains(wanted)
            }) {
                continue;
            }
        }
        if let Some(id) = node["backendDOMNodeId"].as_u64().filter(|id| *id > 0) {
            candidates.insert(id);
            if candidates.len() > MAX_CANDIDATES {
                return Err(error(
                    "BROWSER_SELECTOR_LIMIT: narrow the role or accessible name",
                ));
            }
        }
    }
    Ok(candidates)
}

fn check_query(response: &Value) -> Result<()> {
    let Some(exception) = response.get("exceptionDetails") else {
        return Ok(());
    };
    let description = exception["exception"]["description"]
        .as_str()
        .unwrap_or_default();
    for code in ["BROWSER_SELECTOR_AMBIGUOUS", "BROWSER_SELECTOR_LIMIT"] {
        if description.contains(code) {
            return Err(error(code));
        }
    }
    Err(error(
        "BROWSER_SELECTOR_QUERY_FAILED: DOM lookup failed; take a fresh snapshot",
    ))
}

async fn verify_document(owner: &AgentCx, cdp: &mut Cdp, expected: &Document) -> Result<()> {
    owner
        .checkpoint()
        .map_err(|_| error("BROWSER_SELECTOR_CANCELLED"))?;
    if document(owner, cdp).await? != *expected {
        return Err(error(
            "BROWSER_SELECTOR_STALE: document changed during semantic lookup",
        ));
    }
    Ok(())
}

async fn release(owner: &AgentCx, cdp: &mut Cdp, object: &str) {
    let _ = cdp
        .command(owner, "Runtime.releaseObject", json!({"objectId":object}))
        .await;
}

/// Convert a live object into a backend ID, verifying that it belongs to the
/// captured document. A backend ID is retained; the temporary object is not.
async fn backend(owner: &AgentCx, cdp: &mut Cdp, object: &str) -> Result<Option<u64>> {
    let result =
        async {
            let response = cdp.command(owner, "Runtime.callFunctionOn", json!({
            "objectId":object, "functionDeclaration":NODE_IN_DOCUMENT, "returnByValue":true,
        })).await?;
            check_query(&response)?;
            match response["result"]["value"].as_bool() {
                Some(false) => return Ok(None),
                Some(true) => {}
                None => {
                    return Err(error(
                        "BROWSER_SELECTOR_PROTOCOL: missing node ownership result",
                    ));
                }
            }
            let described = cdp
                .command(
                    owner,
                    "DOM.describeNode",
                    json!({"objectId":object,"depth":0}),
                )
                .await?;
            described["node"]["backendNodeId"]
                .as_u64()
                .filter(|id| *id > 0)
                .map(Some)
                .ok_or_else(|| error("BROWSER_SELECTOR_PROTOCOL: missing backend node ID"))
        }
        .await;
    // Cancellation may prevent this cleanup; connection retirement releases
    // remaining handles, as it does for the existing CSS/frame lookup path.
    release(owner, cdp, object).await;
    result
}

pub(super) async fn resolve(
    owner: &AgentCx,
    cdp: &mut Cdp,
    locator: &Locator,
) -> Result<Option<u64>> {
    let expected = document(owner, cdp).await?;
    let world = cdp
        .command(
            owner,
            "Page.createIsolatedWorld",
            json!({
                "frameId":expected.frame, "worldName":"pi-browser-tools",
            }),
        )
        .await?;
    let context = world["executionContextId"]
        .as_u64()
        .filter(|id| *id > 0)
        .ok_or_else(|| error("BROWSER_SELECTOR_PROTOCOL: missing isolated world"))?;
    verify_document(owner, cdp, &expected).await?;
    let found =
        if matches!(locator.kind, Kind::Role | Kind::Label) {
            let response = cdp
                .command(
                    owner,
                    "Accessibility.getFullAXTree",
                    json!({"frameId":expected.frame}),
                )
                .await?;
            let nodes = response["nodes"]
                .as_array()
                .ok_or_else(|| error("BROWSER_SELECTOR_PROTOCOL: missing accessibility nodes"))?;
            let candidates = ax_candidates(nodes, locator)?;
            let mut found = None;
            for candidate in candidates {
                verify_document(owner, cdp, &expected).await?;
                let resolved = cdp
                    .command(
                        owner,
                        "DOM.resolveNode",
                        json!({
                            "backendNodeId":candidate, "executionContextId":context,
                        }),
                    )
                    .await?;
                let object = resolved["object"]["objectId"].as_str().ok_or_else(|| {
                    error("BROWSER_SELECTOR_STALE: accessibility node is no longer available")
                })?;
                if let Some(id) = backend(owner, cdp, object).await? {
                    if found.is_some_and(|previous| previous != id) {
                        return Err(error(
                            "BROWSER_SELECTOR_AMBIGUOUS: narrow the role or accessible name",
                        ));
                    }
                    found = Some(id);
                }
            }
            found
        } else {
            let response = cdp
                .command(
                    owner,
                    "Runtime.callFunctionOn",
                    json!({
                        "executionContextId":context, "functionDeclaration":DOM_QUERY,
                        "arguments":[{"value":locator}], "returnByValue":false,
                    }),
                )
                .await?;
            check_query(&response)?;
            if response["result"]["type"] == "object" && response["result"]["subtype"] == "null" {
                None
            } else {
                let object = response["result"]["objectId"]
                    .as_str()
                    .ok_or_else(|| error("BROWSER_SELECTOR_PROTOCOL: missing DOM object"))?;
                Some(backend(owner, cdp, object).await?.ok_or_else(|| {
                    error("BROWSER_SELECTOR_STALE: matching node left its document")
                })?)
            }
        };
    verify_document(owner, cdp, &expected).await?;
    Ok(found)
}

#[cfg(test)]
mod tests;
