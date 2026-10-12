//! Explicit, per-operation frame selection for the native browser backend.
//!
//! The frame tree, not a page-provided selector or a cached execution context,
//! establishes membership in the selected tab. Pin the entire document ancestry
//! before creating an isolated world. Navigation or reparenting invalidates the
//! operation instead of falling back to the top-level document.

use super::cdp::Cdp;
use super::interaction::forms;
use super::{output, policy, required};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde_json::{Value, json};
use std::collections::BTreeMap;

mod pointer;
pub(super) use pointer::project as project_pointer;
mod targets;
pub(super) use targets::Targets;
#[cfg(test)]
mod tests_transport;

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
    session: String,
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
    #[cfg(test)]
    fn parse(response: &Value) -> Result<Self> {
        Self::parse_in_session(response, "")
    }

    fn parse_in_session(response: &Value, session: &str) -> Result<Self> {
        let root = response
            .get("frameTree")
            .filter(|value| value.is_object())
            .ok_or_else(|| error("Page.getFrameTree returned no frame tree"))?;
        let mut tree = Self {
            frames: BTreeMap::new(),
            order: Vec::new(),
        };
        tree.visit(root, None, 0, session)?;
        Ok(tree)
    }

    fn visit(&mut self, node: &Value, parent: Option<&str>, depth: usize, session: &str) -> Result<()> {
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
                    session: session.to_owned(),
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
                self.visit(child, Some(&id), depth + 1, session)?;
            }
        }
        Ok(())
    }

    /// Chromium's Page tree contains only local frames. A related iframe's
    /// root has an external parentId, which must belong to its owning parent
    /// session before that subtree may join this page's frame inventory.
    fn append_remote(&mut self, response: &Value, link: &targets::Link) -> Result<()> {
        let root = response
            .get("frameTree")
            .filter(|value| value.is_object())
            .ok_or_else(|| error("iframe session returned no frame tree"))?;
        let frame = &root["frame"];
        if identifier(frame, "id", false)? != link.target {
            return Err(error("iframe target does not match its root frame"));
        }
        let parent = identifier(frame, "parentId", false)?;
        if self
            .frames
            .get(&parent)
            .is_none_or(|frame| frame.identity.session != link.parent)
        {
            return Err(error("iframe root is not owned by its attached parent session"));
        }
        let depth = self.path(&parent)?.len();
        self.visit(root, Some(&parent), depth, &link.session)
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

async fn inventory(owner: &AgentCx, cdp: &mut Cdp) -> Result<Tree> {
    let root = cdp.page_session_id()?.to_owned();
    let links = targets::discover(owner, cdp).await?;
    let generation = cdp.frame_targets.generation();
    let response = cdp
        .frame_command(owner, &root, "Page.getFrameTree", json!({}))
        .await?;
    let mut tree = Tree::parse_in_session(&response, &root)?;
    for link in &links {
        let response = cdp
            .frame_command(owner, &link.session, "Page.getFrameTree", json!({}))
            .await?;
        tree.append_remote(&response, link)?;
    }
    // Never return a silently incomplete snapshot when a process swap or
    // newly attached subtree races the collection of its component trees.
    if cdp.frame_targets.generation() != generation {
        return Err(error(
            "iframe targets changed during discovery; use list_frames again",
        ));
    }
    Ok(tree)
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

#[derive(Clone)]
pub(super) struct Scope {
    ancestry: Vec<Identity>,
    context: u64,
    routes: Vec<targets::Link>,
}

impl Scope {
    fn pin(tree: &Tree, id: &str, targets: &Targets) -> Result<Self> {
        let ancestry = tree.path(id)?;
        let root = ancestry
            .first()
            .ok_or_else(|| error("frame selection has no page"))?;
        let routes: Vec<_> = targets
            .ordered(&root.session)?
            .into_iter()
            .filter(|link| ancestry.iter().any(|frame| frame.session == link.session))
            .collect();
        if ancestry.iter().any(|frame| {
            frame.session != root.session
                && !routes.iter().any(|link| link.session == frame.session)
        }) {
            return Err(error("selected frame has no owned renderer session"));
        }
        Ok(Self {
            ancestry,
            context: 0,
            routes,
        })
    }

    pub(super) const fn context_id(&self) -> u64 {
        self.context
    }

    fn selected(&self) -> Result<&Identity> {
        self.ancestry
            .last()
            .ok_or_else(|| error("frame selection has no document"))
    }

    pub(super) fn is_remote(&self) -> bool {
        self.ancestry
            .first()
            .zip(self.ancestry.last())
            .is_some_and(|(root, selected)| root.session != selected.session)
    }

    /// Pointer actions use project_pointer before dispatch. File transfer,
    /// navigation, exports and dialogs still have no frame-scoped contract.
    pub(super) fn admit_action(&self, action: &str) -> Result<()> {
        if self.is_remote()
            && !matches!(
                action,
                "snapshot" | "ax_tree" | "evaluate" | "wait_for"
                    | "click" | "scroll" | "fill" | "type" | "press"
            )
            && !forms::is_action(action)
        {
            return Err(error("action is not supported in a cross-process frame"));
        }
        Ok(())
    }

    pub(super) fn command_session(&self, method: &str) -> Result<&str> {
        // Native input belongs to the page. Pointer callers first project
        // their renderer-local candidate and verify every embedding boundary;
        // keyboard callers verify focus in the selected isolated world.
        if method.starts_with("Input.") || method == "Page.bringToFront" {
            self.ancestry
                .first()
                .map(|root| root.session.as_str())
                .ok_or_else(|| error("frame selection has no page"))
        } else {
            Ok(&self.selected()?.session)
        }
    }

    pub(super) fn check_targets(&self, cdp: &Cdp) -> Result<()> {
        if self.ancestry.first().map(|root| root.session.as_str()) != Some(cdp.page_session_id()?)
            || !cdp.frame_targets.preserves(&self.routes)
        {
            return Err(error(
                "selected frame renderer detached or changed; take a new snapshot",
            ));
        }
        Ok(())
    }

    /// Rebuild just this document's process ancestry, not unrelated iframe
    /// targets. A sibling's reload cannot invalidate the selection, but every
    /// selected renderer incarnation, parent, loader and URL remains pinned.
    pub(super) async fn check_current(&self, owner: &AgentCx, cdp: &mut Cdp) -> Result<Value> {
        self.check_targets(cdp)?;
        let root = cdp.page_session_id()?.to_owned();
        let response = cdp
            .frame_command(owner, &root, "Page.getFrameTree", json!({}))
            .await?;
        let mut tree = Tree::parse_in_session(&response, &root)?;
        for link in &self.routes {
            let response = cdp
                .frame_command(owner, &link.session, "Page.getFrameTree", json!({}))
                .await?;
            tree.append_remote(&response, link)?;
        }
        self.check_targets(cdp)?;
        self.check_tree(&tree)
    }

    #[cfg(test)]
    pub(super) fn check(&self, response: &Value) -> Result<Value> {
        self.check_tree(&Tree::parse(response)?)
    }

    fn check_tree(&self, tree: &Tree) -> Result<Value> {
        let selected = self.selected()?;
        let current = tree.path(&selected.id)?;
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
    ) && !forms::is_action(action) {
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
    let tree = inventory(owner, cdp).await?;
    let mut scope = Scope::pin(&tree, id, &cdp.frame_targets)?;
    admit_path(&scope.ancestry, allowlist)?;
    scope.check_current(owner, cdp).await?;
    let session = scope.selected()?.session.clone();
    let world = cdp
        .frame_command(
            owner,
            &session,
            "Page.createIsolatedWorld",
            json!({"frameId": id, "worldName": "pi-browser-tools"}),
        )
        .await?;
    scope.context = world["executionContextId"]
        .as_u64()
        .filter(|id| *id > 0)
        .ok_or_else(|| error("selected frame has no isolated execution context"))?;
    // Creating the world can race a navigation. Do not authorize the new
    // document merely because Chromium reused a frame ID.
    scope.check_current(owner, cdp).await?;
    Ok(scope)
}

pub(super) async fn list(
    owner: &AgentCx,
    cdp: &mut Cdp,
    tab: &str,
    allowlist: Option<&[String]>,
) -> Result<ToolOutput> {
    let tree = inventory(owner, cdp).await?;
    let mut rows = Vec::with_capacity(tree.order.len());
    for id in &tree.order {
        let frame = &tree.frames[id];
        let allowed = admit_path(&tree.path(id)?, allowlist).is_ok();
        rows.push(json!({
            "frame_id": id,
            "parent_id": frame.identity.parent,
            "is_main": frame.identity.parent.is_none(),
            "out_of_process": frame.identity.session != cdp.page_session_id()?,
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

    #[test]
    fn structured_forms_keep_explicit_tab_and_owned_renderer_requirements() {
        let (tree, targets) = scoped_remote_tree();
        let scope = Scope::pin(&tree, "remote-nested", &targets).unwrap();
        assert!(scope.admit_action("select_option").is_ok());
        assert!(validate(&json!({"action":"select_option","tab":"work","frame":"child"})).is_ok());
        assert!(validate(&json!({"action":"select_option","frame":"child"})).is_err());
        assert!(validate(&json!({"action":"select_option","tab":"work","frame":"child","dialog_response":{}})).is_err());
    }

    fn scoped_remote_tree() -> (Tree, Targets) {
        let mut tree = Tree::parse_in_session(&tree(), "page").unwrap();
        let mut targets = Targets::default();
        for (parent, session, target) in [
            ("page", "remote-session", "remote"),
            ("remote-session", "nested-session", "remote-nested"),
            ("page", "sibling-session", "remote-sibling"),
        ] {
            targets.observe(&json!({
                "sessionId": parent, "method": "Target.attachedToTarget",
                "params": {"sessionId": session, "waitingForDebugger": false,
                    "targetInfo": {"targetId": target, "type": "iframe"}},
            }), Some("page")).unwrap();
        }
        for (parent, session, target, frame_parent) in [
            ("page", "remote-session", "remote", "child"),
            ("remote-session", "nested-session", "remote-nested", "remote"),
            ("page", "sibling-session", "remote-sibling", "main"),
        ] {
            tree.append_remote(
                &remote_tree(target, frame_parent, "https://example.com/"),
                &remote_link(parent, session, target),
            ).unwrap();
        }
        (tree, targets)
    }

    #[test]
    fn selection_routes_dom_and_worlds_to_the_owner_but_keys_to_the_page() {
        let (tree, targets) = scoped_remote_tree();
        let scope = Scope::pin(&tree, "remote-nested", &targets).unwrap();
        assert_eq!(scope.routes.len(), 2);
        for method in [
            "Runtime.evaluate", "Runtime.callFunctionOn", "Runtime.releaseObject",
            "DOM.resolveNode", "DOM.describeNode", "Accessibility.getFullAXTree",
            "Page.createIsolatedWorld",
        ] {
            assert_eq!(scope.command_session(method).unwrap(), "nested-session");
        }
        for method in [
            "Input.insertText", "Input.dispatchKeyEvent", "Input.dispatchMouseEvent", "Page.bringToFront",
        ] {
            assert_eq!(scope.command_session(method).unwrap(), "page");
        }
        for action in [
            "snapshot", "ax_tree", "evaluate", "wait_for", "fill", "type", "press", "click", "scroll",
        ] {
            assert!(scope.admit_action(action).is_ok());
        }
        for action in ["upload", "download", "goto", "handle_dialog"] {
            assert!(scope.admit_action(action).is_err());
        }
        let local = Scope::pin(&tree, "child", &targets).unwrap();
        assert!(local.routes.is_empty());
        assert_eq!(local.command_session("Input.dispatchMouseEvent").unwrap(), "page");
        assert!(local.admit_action("click").is_ok());
        assert!(Scope::pin(&tree, "other-tab-frame", &targets).is_err());
    }

    #[test]
    fn selected_process_and_every_ancestor_document_are_pinned() {
        for frame_id in ["main", "child", "remote", "remote-nested"] {
            for field in ["loader", "url", "session", "parent"] {
                let (mut tree, targets) = scoped_remote_tree();
                let scope = Scope::pin(&tree, "remote-nested", &targets).unwrap();
                assert!(scope.check_tree(&tree).is_ok());
                let identity = &mut tree.frames.get_mut(frame_id).unwrap().identity;
                match field {
                    "loader" => identity.loader = "new-loader".into(),
                    "url" => identity.url = "https://example.com/new".into(),
                    "session" => identity.session = "replacement-process".into(),
                    _ => identity.parent = Some("foreign-parent".into()),
                }
                assert!(scope.check_tree(&tree).is_err(), "{frame_id} {field}");
            }
        }
        let (mut tree, targets) = scoped_remote_tree();
        let scope = Scope::pin(&tree, "remote-nested", &targets).unwrap();
        tree.frames.get_mut("remote-sibling").unwrap().identity.loader = "new-sibling".into();
        assert!(scope.check_tree(&tree).is_ok());
        tree.frames.remove("remote");
        assert!(scope.check_tree(&tree).is_err());
    }

    #[test]
    fn selection_requires_owned_routes_and_ignores_unrelated_detach() {
        let (tree, mut targets) = scoped_remote_tree();
        let scope = Scope::pin(&tree, "remote-nested", &targets).unwrap();
        assert!(targets.preserves(&scope.routes));
        targets.observe(&json!({"sessionId":"page", "method":"Target.detachedFromTarget",
            "params":{"sessionId":"sibling-session"}}), Some("page")).unwrap();
        assert!(targets.preserves(&scope.routes));
        targets.observe(&json!({"sessionId":"page", "method":"Target.detachedFromTarget",
            "params":{"sessionId":"remote-session"}}), Some("page")).unwrap();
        assert!(!targets.preserves(&scope.routes));
        assert!(Scope::pin(&tree, "remote-nested", &targets).is_err());
    }

    fn remote_link(parent: &str, session: &str, target: &str) -> targets::Link {
        let mut targets = Targets::default();
        targets.observe(&json!({
            "sessionId": parent, "method": "Target.attachedToTarget",
            "params": {"sessionId": session, "waitingForDebugger": false,
                "targetInfo": {"targetId": target, "type": "iframe"}},
        }), Some(parent)).unwrap();
        targets.ordered(parent).unwrap().remove(0)
    }

    fn remote_tree(id: &str, parent: &str, url: &str) -> Value {
        let mut root = node(id, url);
        root["frame"]["parentId"] = json!(parent);
        json!({"frameTree": root})
    }

    #[test]
    fn remote_subtrees_join_only_through_their_owning_frame_and_session() {
        let mut tree = Tree::parse_in_session(&tree(), "page").unwrap();
        let remote = remote_link("page", "remote-session", "remote");
        let mut response = remote_tree("remote", "child", "https://other.example/");
        response["frameTree"]["childFrames"] = json!([node("remote-local", "about:blank")]);
        tree.append_remote(&response, &remote).unwrap();
        let nested = remote_link("remote-session", "nested-session", "remote-nested");
        tree.append_remote(
            &remote_tree("remote-nested", "remote-local", "https://nested.example/"),
            &nested,
        ).unwrap();
        let path = tree.path("remote-nested").unwrap();
        assert_eq!(path.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
            ["main", "child", "remote", "remote-local", "remote-nested"]);
        assert_eq!(path[2].session, "remote-session");
        assert_eq!(path[3].session, "remote-session");
        assert_eq!(path[4].session, "nested-session");
    }

    #[test]
    fn remote_roots_cannot_import_foreign_reparented_or_duplicate_frames() {
        let link = remote_link("page", "remote-session", "remote");
        for response in [
            remote_tree("wrong-target", "child", "https://example.com"),
            remote_tree("remote", "other-tab-frame", "https://example.com"),
            remote_tree("remote", "remote", "https://example.com"),
            json!({"frameTree": node("remote", "https://example.com")}),
        ] {
            let mut tree = Tree::parse_in_session(&tree(), "page").unwrap();
            assert!(tree.append_remote(&response, &link).is_err());
        }
        let mut tree = Tree::parse_in_session(&tree(), "page").unwrap();
        let response = remote_tree("remote", "child", "https://example.com");
        let foreign = remote_link("foreign-session", "remote-session", "remote");
        assert!(tree.append_remote(&response, &foreign).is_err());
        tree.append_remote(&response, &link).unwrap();
        assert!(tree.append_remote(&response, &link).is_err());
    }

    #[test]
    fn remote_frames_keep_the_same_ancestor_url_policy() {
        let mut source = tree();
        source["frameTree"]["childFrames"][0]["frame"]["url"] =
            json!("https://blocked.test/PRIVATE-CANARY");
        let mut tree = Tree::parse_in_session(&source, "page").unwrap();
        tree.append_remote(
            &remote_tree("remote", "child", "https://example.com/allowed"),
            &remote_link("page", "remote-session", "remote"),
        ).unwrap();
        let rules = vec!["example.com".to_owned()];
        let error = admit_path(&tree.path("remote").unwrap(), Some(&rules)).unwrap_err();
        assert!(!error.to_string().contains("PRIVATE-CANARY"));
        assert!(admit_path(&tree.path("remote").unwrap(), None).is_ok());
    }

    #[test]
    fn frame_count_budget_is_shared_across_process_trees() {
        let mut tree = Tree::parse_in_session(&json!({"frameTree": node("main", "about:blank")}), "page").unwrap();
        for i in 1..MAX_FRAMES {
            let id = format!("remote-{i}");
            let session = format!("session-{i}");
            tree.append_remote(&remote_tree(&id, "main", "about:blank"), &remote_link("page", &session, &id)).unwrap();
        }
        assert_eq!(tree.frames.len(), MAX_FRAMES);
        assert!(tree.append_remote(&remote_tree("overflow", "main", "about:blank"), &remote_link("page", "overflow", "overflow")).is_err());
    }

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
            routes: Vec::new(),
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
