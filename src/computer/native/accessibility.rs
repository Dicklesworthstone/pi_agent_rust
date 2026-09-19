//! AT-SPI bridge over a fixed, isolated Python/GI helper. Target selection uses
//! the X11 window's reported PID and exact title, never a guessed application.
//! Missing/ambiguous exposure fails. This correlation is not an OS sandbox.

use super::{active_window, check_owner, clean, error, output, parse_windows, process, query};
use crate::agent_cx::AgentCx;
use crate::computer::AxNode;
use crate::error::Result;
use crate::tools::ToolOutput;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const HELPER: &str = include_str!("atspi.py");
const MAX_NODES: usize = 512;
const MAX_DEPTH: usize = 8;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    root: AxNode,
    node_count: usize,
    truncated: bool,
    omitted_children: u64,
    values_included: bool,
}

pub(super) async fn execute(
    owner: &AgentCx,
    cwd: &Path,
    helpers: &BTreeMap<String, PathBuf>,
    args: &Value,
) -> Result<ToolOutput> {
    check_owner(owner)?;
    let active = active_window(owner, cwd, helpers).await?;
    let target = args
        .get("window_id")
        .and_then(Value::as_u64)
        .map(|id| u32::try_from(id).expect("validated window ID"))
        .or(active)
        .ok_or_else(|| error("accessibility inspection needs a window_id or a focused window"))?;
    let windows = parse_windows(
        &query(owner, cwd, helpers, "wmctrl", &["-u", "-lpGx"]).await?,
        active,
    )?;
    let window = windows
        .iter()
        .find(|window| window.info.id == target)
        .ok_or_else(|| error("accessibility target is not in the current window list"))?;
    if window.pid == 0 || window.raw_title.is_empty() || window.raw_title.len() > 8192 {
        return Err(error(
            "window lacks a reported PID or bounded exact title; refusing to guess an accessibility target",
        ));
    }
    // Display sanitization must never change target identity. Two windows can
    // have identical sanitized labels but different actual titles.
    let request = serde_json::to_vec(&json!({"pid":window.pid,"title":window.raw_title}))?;
    let response = process::run(
        owner,
        cwd,
        helpers,
        "python3",
        &process::strings(&["-I", "-c", HELPER]),
        &request,
        process::TEXT_LIMIT,
    )
    .await?;
    check_owner(owner)?;
    let snapshot = parse_response(&response)?;
    let summary = format!(
        "Accessibility tree for window {target} ({} nodes{}; editable values omitted):\n{}",
        snapshot.node_count,
        if snapshot.truncated { ", partial" } else { "" },
        serde_json::to_string_pretty(&snapshot.root)?,
    );
    Ok(output(
        summary,
        json!({
            "window_id":target,"pid":window.pid,"backend":"x11-atspi",
            "root":snapshot.root,"node_count":snapshot.node_count,"truncated":snapshot.truncated,
            "omitted_children":snapshot.omitted_children,"values_included":false
        }),
        false,
    ))
}

fn parse_response(bytes: &[u8]) -> Result<Snapshot> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| error("AT-SPI helper returned invalid JSON"))?;
    if let Some(failure) = value.get("error") {
        return Err(error(match failure.as_str() {
            Some("atspi_bindings_unavailable") => {
                "AT-SPI requires Python PyGObject and the Atspi 2.0 introspection package; install them for the selected python3 interpreter"
            }
            Some("target_not_unique_or_not_exposed") => {
                "window is not uniquely exposed by AT-SPI for its PID and title; enable application accessibility or choose another window"
            }
            Some("accessibility_bus_unavailable") => "the session accessibility bus is unavailable",
            Some("desktop_registry_limit" | "application_window_limit") => {
                "accessibility target search exceeded its bounded registry budget"
            }
            _ => "accessibility target became unavailable or could not be inspected",
        }));
    }
    let snapshot: Snapshot = serde_json::from_value(value)
        .map_err(|_| error("AT-SPI helper response does not match the snapshot schema"))?;
    if snapshot.values_included {
        return Err(error("AT-SPI helper unexpectedly included editable values"));
    }
    let mut count = 0;
    validate_node(&snapshot.root, 0, &mut count)?;
    if count != snapshot.node_count {
        return Err(error("AT-SPI node count does not match the received tree"));
    }
    Ok(snapshot)
}

fn validate_node(node: &AxNode, depth: usize, count: &mut usize) -> Result<()> {
    if depth > MAX_DEPTH || *count >= MAX_NODES || node.children.len() > 64 {
        return Err(error(
            "AT-SPI tree exceeded its node, depth or child budget",
        ));
    }
    *count += 1;
    if node.role.is_empty()
        || node.role != clean(&node.role, 64)
        || node
            .title
            .as_ref()
            .is_some_and(|title| *title != clean(title, 256))
        || node.value.is_some()
    {
        return Err(error(
            "AT-SPI tree contains invalid labels or forbidden editable values",
        ));
    }
    if node.role.eq_ignore_ascii_case("password text")
        && (node.title.is_some() || !node.children.is_empty())
    {
        return Err(error("AT-SPI password node contains unexpected content"));
    }
    for child in &node.children {
        validate_node(child, depth + 1, count)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node() -> Value {
        json!({"role":"push button","title":"Submit","value":null,
            "enabled":true,"focused":false,"children":[]})
    }

    fn reply(root: &Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"root":root,"node_count":1,"truncated":false,
            "omitted_children":0,"values_included":false}))
        .unwrap()
    }

    #[test]
    fn actual_snapshot_shape_round_trips_without_invented_nodes() {
        let snapshot = parse_response(&reply(&node())).unwrap();
        assert_eq!(snapshot.root.title.as_deref(), Some("Submit"));
        assert_eq!(snapshot.node_count, 1);
        assert!(!snapshot.truncated);
    }

    #[test]
    fn forbidden_values_and_password_contents_are_rejected() {
        let mut root = node();
        root["value"] = json!("private-secret");
        assert!(parse_response(&reply(&root)).is_err());
        let mut root = node();
        root["role"] = json!("password text");
        assert!(parse_response(&reply(&root)).is_err());
        root["title"] = Value::Null;
        assert!(parse_response(&reply(&root)).is_ok());
    }

    #[test]
    fn malformed_or_unavailable_trees_do_not_become_success() {
        for response in [
            br#"{"error":"atspi_bindings_unavailable"}"#.as_slice(),
            br#"{"error":"private-title-in-a-foreign-error"}"#.as_slice(),
            b"not json",
        ] {
            let failure = parse_response(response).err().expect("error response");
            assert!(!failure.to_string().contains("private-title"));
        }
        let mut root = node();
        root["children"] = json!([node()]);
        assert!(parse_response(&reply(&root)).is_err());
        let mut root = node();
        for _ in 0..10 {
            let mut parent = node();
            parent["children"] = json!([root]);
            root = parent;
        }
        assert!(parse_response(&reply(&root)).is_err());
    }
}
