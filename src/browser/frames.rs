//! Explicit, per-operation frame selection for the native browser backend.
//!
//! The frame tree, not a page-provided selector or a cached execution context,
//! establishes membership in the selected tab. Pin the entire document ancestry
//! before creating an isolated world. Navigation or reparenting invalidates the
//! operation instead of falling back to the top-level document.

use super::cdp::Cdp;
use super::{output, policy, required};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde_json::{Value, json};
use std::collections::BTreeMap;

const MAX_FRAMES: usize = 256;
const MAX_DEPTH: usize = 32;

fn error(message: &'static str) -> Error {
    Error::tool("browser", message)
}

fn identifier(value: &Value, key: &str, empty: bool) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| {
            (empty || !text.is_empty()) && text.len() <= 256 && !text.chars().any(char::is_control)
        })
        .map(str::to_owned)
        .ok_or_else(|| error("frame metadata contains an invalid identifier"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    id: String,
    parent: Option<String>,
    loader: String,
    url: String,
}

struct Frame {
    identity: Identity,
    name: String,
}

struct Tree {
    frames: BTreeMap<String, Frame>,
    order: Vec<String>,
}

impl Tree {
    fn parse(response: &Value) -> Result<Self> {
        let root = response
            .get("frameTree")
            .filter(|value| value.is_object())
            .ok_or_else(|| error("Page.getFrameTree returned no frame tree"))?;
        let mut tree = Self {
            frames: BTreeMap::new(),
            order: Vec::new(),
        };
        tree.visit(root, None, 0)?;
        Ok(tree)
    }

    fn visit(&mut self, node: &Value, parent: Option<&str>, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH || self.frames.len() >= MAX_FRAMES {
            return Err(error(
                "browser frame tree exceeds its depth or frame-count limit",
            ));
        }
        let frame = node
            .get("frame")
            .filter(|value| value.is_object())
            .ok_or_else(|| error("frame tree contains no frame metadata"))?;
        let id = identifier(frame, "id", false)?;
        let loader = identifier(frame, "loaderId", true)?;
        if frame.get("parentId").is_some()
            && Some(identifier(frame, "parentId", false)?.as_str()) != parent
        {
            return Err(error("frame tree contains inconsistent parent metadata"));
        }
        let url = frame
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty() && url.len() <= 64 * 1024)
            .filter(|url| !url.chars().any(char::is_control))
            .ok_or_else(|| error("frame metadata contains an invalid URL"))?
            .to_owned();
        let name = match frame.get("name") {
            None => String::new(),
            Some(Value::String(name)) => name.chars().take(1000).collect(),
            _ => return Err(error("frame metadata contains an invalid name")),
        };
        if self.frames.contains_key(&id) {
            return Err(error("frame tree contains duplicate frame IDs"));
        }
        self.order.push(id.clone());
        self.frames.insert(
            id.clone(),
            Frame {
                identity: Identity {
                    id: id.clone(),
                    parent: parent.map(str::to_owned),
                    loader,
                    url,
                },
                name,
            },
        );
        if let Some(children) = node.get("childFrames") {
            let children = children
                .as_array()
                .ok_or_else(|| error("frame tree children must be an array"))?;
            if children.len() > MAX_FRAMES.saturating_sub(self.frames.len()) {
                return Err(error("browser frame tree exceeds its frame-count limit"));
            }
            for child in children {
                self.visit(child, Some(&id), depth + 1)?;
            }
        }
        Ok(())
    }

    fn path(&self, id: &str) -> Result<Vec<Identity>> {
        let mut next = Some(id);
        let mut path = Vec::new();
        while let Some(id) = next {
            if path.len() > MAX_DEPTH {
                return Err(error("browser frame ancestry exceeds its depth limit"));
            }
            let frame = self
                .frames
                .get(id)
                .ok_or_else(|| error("frame is not in the selected tab; use list_frames again"))?;
            path.push(frame.identity.clone());
            next = frame.identity.parent.as_deref();
        }
        path.reverse();
        Ok(path)
    }
}

fn admit_path(path: &[Identity], allowlist: Option<&[String]>) -> Result<()> {
    for (index, frame) in path.iter().enumerate() {
        // srcdoc has its embedding document's authority. Checking every
        // ancestor also prevents about:blank from laundering a blocked origin.
        if index > 0 && frame.url == "about:srcdoc" {
            continue;
        }
        policy::check_navigation(&frame.url, allowlist)
            .map_err(|_| error("selected frame or ancestor is blocked by browser URL policy"))?;
    }
    Ok(())
}

pub(super) struct Scope {
    ancestry: Vec<Identity>,
    context: u64,
}

impl Scope {
    pub(super) const fn context_id(&self) -> u64 {
        self.context
    }

    pub(super) fn check(&self, response: &Value) -> Result<Value> {
        let selected = self
            .ancestry
            .last()
            .ok_or_else(|| error("frame selection has no document"))?;
        let current = Tree::parse(response)?.path(&selected.id)?;
        if current != self.ancestry {
            return Err(error(
                "selected frame or ancestor navigated; list frames and take a new snapshot before retrying",
            ));
        }
        Ok(json!({
            "id": selected.id, "loaderId": selected.loader, "url": selected.url
        }))
    }
}

/// Validate before connecting or launching. Tab-level actions must not silently
/// ignore a frame, and native key/wheel events need an explicit target element.
pub(super) fn validate(args: &Value) -> Result<()> {
    let Some(frame) = args.get("frame") else {
        return Ok(());
    };
    if frame
        .as_str()
        .is_none_or(|id| id.is_empty() || id.len() > 256 || id.chars().any(char::is_control))
    {
        return Err(error(
            "frame must be a nonempty frame ID of at most 256 bytes",
        ));
    }
    if args
        .get("tab")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(error("frame selection requires an explicit tab"));
    }
    let action = required(args, "action")?;
    if !matches!(
        action,
        "snapshot"
            | "ax_tree"
            | "evaluate"
            | "click"
            | "type"
            | "fill"
            | "press"
            | "scroll"
            | "wait_for"
    ) {
        return Err(error(
            "frame selection is supported by inspection, evaluation and input actions only",
        ));
    }
    if matches!(action, "press" | "scroll")
        && args
            .get("selector")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err(error(
            "frame-scoped press and scroll require an explicit selector",
        ));
    }
    if args.get("dialog_response").is_some() {
        return Err(error("frame-scoped dialog responses are not supported"));
    }
    Ok(())
}

pub(super) async fn select(
    owner: &AgentCx,
    cdp: &mut Cdp,
    id: &str,
    allowlist: Option<&[String]>,
) -> Result<Scope> {
    let response = cdp.command(owner, "Page.getFrameTree", json!({})).await?;
    let ancestry = Tree::parse(&response)?.path(id)?;
    admit_path(&ancestry, allowlist)?;
    let world = cdp
        .command(
            owner,
            "Page.createIsolatedWorld",
            json!({"frameId": id, "worldName": "pi-browser-tools"}),
        )
        .await?;
    let context = world["executionContextId"]
        .as_u64()
        .filter(|id| *id > 0)
        .ok_or_else(|| error("selected frame has no isolated execution context"))?;
    let scope = Scope { ancestry, context };
    // Creating the world can race a navigation. Do not authorize the new
    // document merely because Chromium reused a frame ID.
    scope.check(&cdp.command(owner, "Page.getFrameTree", json!({})).await?)?;
    Ok(scope)
}

pub(super) async fn list(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    allowlist: Option<&[String]>,
) -> Result<ToolOutput> {
    let response = cdp.command(owner, "Page.getFrameTree", json!({})).await?;
    let tree = Tree::parse(&response)?;
    let mut rows = Vec::with_capacity(tree.order.len());
    for id in &tree.order {
        let frame = &tree.frames[id];
        let allowed = admit_path(&tree.path(id)?, allowlist).is_ok();
        rows.push(json!({
            "frame_id": id,
            "parent_id": frame.identity.parent,
            "is_main": frame.identity.parent.is_none(),
            "allowed": allowed,
            "url": allowed.then_some(frame.identity.url.as_str()),
            "name": allowed.then_some(frame.name.as_str()),
        }));
    }
    let lines = rows
        .iter()
        .map(|row| {
            format!(
                "- [{}] parent={} url={}{}",
                row["frame_id"],
                row["parent_id"],
                row["url"],
                if row["allowed"] == true {
                    ""
                } else {
                    " [blocked]"
                },
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(output(
        format!("Frames in tab {tab} ({}):\n{lines}", rows.len()),
        json!({"tab": tab, "frames": rows, "backend": "cdp"}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, url: &str) -> Value {
        json!({"frame":{"id":id,"loaderId":format!("loader-{id}"),"url":url}})
    }

    fn tree() -> Value {
        let mut root = node("main", "https://example.com/");
        let mut child = node("child", "https://example.com/form");
        child["childFrames"] = json!([node("nested", "about:srcdoc")]);
        root["childFrames"] = json!([child, node("sibling", "about:blank")]);
        json!({"frameTree":root})
    }

    #[test]
    fn nested_frames_preserve_membership_order_and_ancestry() {
        let tree = Tree::parse(&tree()).unwrap();
        assert_eq!(tree.order, ["main", "child", "nested", "sibling"]);
        let path = tree.path("nested").unwrap();
        assert_eq!(path.len(), 3);
        assert_eq!(path[0].id, "main");
        assert_eq!(path[2].parent.as_deref(), Some("child"));
        assert!(tree.path("other-tab-frame").is_err());
    }

    #[test]
    fn duplicate_ids_inconsistent_parents_and_malformed_trees_are_rejected() {
        let mut duplicate = tree();
        duplicate["frameTree"]["childFrames"][1]["frame"]["id"] = json!("child");
        assert!(Tree::parse(&duplicate).is_err());
        let mut wrong_parent = tree();
        wrong_parent["frameTree"]["childFrames"][0]["frame"]["parentId"] = json!("unrelated");
        assert!(Tree::parse(&wrong_parent).is_err());
        let mut bad_children = tree();
        bad_children["frameTree"]["childFrames"] = json!({});
        assert!(Tree::parse(&bad_children).is_err());
        assert!(Tree::parse(&json!({})).is_err());
        let mut bad_id = tree();
        bad_id["frameTree"]["frame"]["id"] = json!("bad\nidentifier");
        assert!(Tree::parse(&bad_id).is_err());
    }

    #[test]
    fn frame_tree_count_and_depth_are_bounded() {
        let mut root = node("main", "about:blank");
        root["childFrames"] = Value::Array(
            (0..MAX_FRAMES)
                .map(|i| node(&format!("child-{i}"), "about:blank"))
                .collect(),
        );
        assert!(Tree::parse(&json!({"frameTree":root})).is_err());
        let mut nested = node("leaf", "about:blank");
        for i in 0..=MAX_DEPTH {
            let mut parent = node(&format!("parent-{i}"), "about:blank");
            parent["childFrames"] = json!([nested]);
            nested = parent;
        }
        assert!(Tree::parse(&json!({"frameTree":nested})).is_err());
    }

    #[test]
    fn blank_and_srcdoc_frames_cannot_launder_blocked_ancestors() {
        let rules = vec!["example.com".to_string()];
        let source = tree();
        let parsed = Tree::parse(&source).unwrap();
        assert!(admit_path(&parsed.path("nested").unwrap(), Some(&rules)).is_ok());
        let mut blocked = source;
        blocked["frameTree"]["childFrames"][0]["frame"]["url"] =
            json!("https://blocked.test/private");
        let parsed = Tree::parse(&blocked).unwrap();
        let error = admit_path(&parsed.path("nested").unwrap(), Some(&rules)).unwrap_err();
        assert!(!error.to_string().contains("private"));
        assert!(admit_path(&parsed.path("sibling").unwrap(), Some(&rules)).is_ok());
        assert!(admit_path(&parsed.path("main").unwrap(), Some(&[])).is_err());
    }

    #[test]
    fn scope_pins_selected_document_and_every_ancestor() {
        let source = tree();
        let scope = Scope {
            ancestry: Tree::parse(&source).unwrap().path("nested").unwrap(),
            context: 7,
        };
        assert_eq!(scope.check(&source).unwrap()["id"], "nested");
        for pointer in [
            "/frameTree/frame/loaderId",
            "/frameTree/childFrames/0/frame/loaderId",
            "/frameTree/childFrames/0/childFrames/0/frame/loaderId",
            "/frameTree/childFrames/0/frame/url",
        ] {
            let mut changed = source.clone();
            *changed.pointer_mut(pointer).unwrap() = json!("changed");
            assert!(scope.check(&changed).is_err(), "{pointer}");
        }
        let mut detached = source;
        detached["frameTree"]["childFrames"] = json!([]);
        assert!(scope.check(&detached).is_err());
    }

    #[test]
    fn explicit_parent_metadata_must_agree_with_nesting() {
        let mut source = tree();
        source["frameTree"]["childFrames"][0]["frame"]["parentId"] = json!("main");
        assert!(Tree::parse(&source).is_ok());
        source["frameTree"]["childFrames"][0]["frame"]["parentId"] = Value::Null;
        assert!(Tree::parse(&source).is_err());
    }

    #[test]
    fn frame_selection_is_explicit_and_never_ignored_by_unsupported_actions() {
        for action in [
            "snapshot", "ax_tree", "evaluate", "click", "type", "fill", "press", "scroll",
            "wait_for",
        ] {
            assert!(
                validate(
                    &json!({"action":action,"tab":"work","frame":"child","selector":"#control"})
                )
                .is_ok()
            );
        }
        for action in [
            "open",
            "goto",
            "close",
            "list_tabs",
            "list_frames",
            "upload",
            "download",
            "screenshot",
            "print_pdf",
            "stop",
        ] {
            assert!(validate(&json!({"action":action,"tab":"work","frame":"child"})).is_err());
        }
        assert!(validate(&json!({"action":"snapshot","frame":"child"})).is_err());
        assert!(validate(&json!({"action":"snapshot","tab":"work","frame":null})).is_err());
        assert!(
            validate(
                &json!({"action":"evaluate","tab":"work","frame":"child","dialog_response":{}})
            )
            .is_err()
        );
        assert!(validate(&json!({"action":"snapshot"})).is_ok());
    }

    #[test]
    fn frame_key_and_wheel_actions_cannot_target_ambient_focus_or_the_main_viewport() {
        for action in ["press", "scroll"] {
            let mut args = json!({"action":action,"tab":"work","frame":"child"});
            assert!(validate(&args).is_err());
            args["selector"] = json!("");
            assert!(validate(&args).is_err());
            args["selector"] = json!("@e42");
            assert!(validate(&args).is_ok());
        }
        assert!(validate(&json!({"action":"press","key":"Tab"})).is_ok());
    }
}
