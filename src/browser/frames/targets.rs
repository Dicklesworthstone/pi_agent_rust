//! Per-operation ownership of flattened CDP iframe sessions. Only an attach
//! event emitted by the selected page or an already-owned iframe can admit a
//! child. Browser-wide target discovery, URL similarity and target IDs alone
//! never establish membership. Dropping the operation's socket detaches all
//! of these sessions; no debugger pause or cross-operation lease is retained.

use super::{Cdp, MAX_FRAMES, error, identifier};
use crate::agent_cx::AgentCx;
use crate::error::Result;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Link {
    pub(super) session: String,
    pub(super) parent: String,
    pub(super) target: String,
    // Detect detach/re-attach even if a transport reuses an opaque session ID.
    incarnation: u64,
}

#[derive(Default)]
pub(crate) struct Targets {
    links: BTreeMap<String, Link>,
    generation: u64,
}

impl Targets {
    pub(crate) fn admits(&self, root: Option<&str>, session: &str) -> bool {
        root == Some(session) || self.links.contains_key(session)
    }

    pub(super) const fn generation(&self) -> u64 {
        self.generation
    }

    fn advance(&mut self) -> Result<()> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| error("iframe target generation exhausted"))?;
        Ok(())
    }

    pub(crate) fn observe(&mut self, value: &Value, root: Option<&str>) -> Result<()> {
        let method = value["method"].as_str().unwrap_or_default();
        if !matches!(
            method,
            "Target.attachedToTarget" | "Target.detachedFromTarget"
        ) {
            return Ok(());
        }
        let params = &value["params"];
        if method == "Target.detachedFromTarget"
            && root.is_some()
            && params["sessionId"].as_str() == root
        {
            return Err(error(
                "selected page target detached during browser operation",
            ));
        }
        let Some(parent) = value["sessionId"].as_str() else {
            return Ok(());
        };
        if !self.admits(root, parent) {
            return Ok(());
        }
        if method == "Target.attachedToTarget" {
            if params["targetInfo"]["type"] != "iframe" {
                return Ok(());
            }
            let session = identifier(params, "sessionId", false)?;
            let target = identifier(&params["targetInfo"], "targetId", false)?;
            if root == Some(session.as_str()) || session == parent {
                return Err(error("iframe target contains a cyclic session identity"));
            }
            if params["waitingForDebugger"] == true {
                return Err(error("iframe target unexpectedly paused for debugger"));
            }
            if let Some(existing) = self.links.get(&session) {
                if existing.parent == parent && existing.target == target {
                    return Ok(());
                }
                return Err(error("iframe session changed ownership without detaching"));
            }
            if self.links.values().any(|link| link.target == target) {
                return Err(error("iframe target has conflicting live sessions"));
            }
            if self.links.len() >= MAX_FRAMES {
                return Err(error("browser exceeds its iframe target limit"));
            }
            self.advance()?;
            self.links.insert(
                session.clone(),
                Link {
                    session,
                    parent: parent.to_owned(),
                    target,
                    incarnation: self.generation,
                },
            );
        } else {
            let session = identifier(params, "sessionId", false)?;
            let Some(link) = self.links.get(&session) else {
                return Ok(());
            };
            if link.parent != parent {
                return Err(error("iframe detach came from a different parent session"));
            }
            self.advance()?;
            self.links.remove(&session);
            // Remove descendants too. A delayed child event from the detached
            // subtree cannot re-authorize it through a no-longer-owned parent.
            loop {
                let orphan = self
                    .links
                    .values()
                    .find(|link| !self.admits(root, &link.parent))
                    .map(|link| link.session.clone());
                let Some(orphan) = orphan else { break };
                self.links.remove(&orphan);
            }
        }
        Ok(())
    }

    pub(super) fn ordered(&self, root: &str) -> Result<Vec<Link>> {
        let mut known = BTreeSet::from([root.to_owned()]);
        let mut ordered = Vec::with_capacity(self.links.len());
        while ordered.len() < self.links.len() {
            let Some(link) = self
                .links
                .values()
                .find(|link| known.contains(&link.parent) && !known.contains(&link.session))
            else {
                return Err(error("iframe target graph is disconnected or cyclic"));
            };
            known.insert(link.session.clone());
            ordered.push(link.clone());
        }
        Ok(ordered)
    }

    pub(super) fn preserves(&self, links: &[Link]) -> bool {
        links
            .iter()
            .all(|link| self.links.get(&link.session) == Some(link))
    }
}

/// CDP auto-attach is scoped to the target session, and is not recursive.
/// Enable it once per related iframe, never on the browser session. Chromium
/// reports already-existing children before acknowledging setAutoAttach.
/// Bound configuration attempts too: repeated detach/attach churn must not
/// reset a budget merely because the live map stays small.
pub(super) async fn discover(owner: &AgentCx, cdp: &mut Cdp) -> Result<Vec<Link>> {
    let root = cdp.page_session_id()?.to_owned();
    let mut configured = BTreeSet::new();
    let mut next = Some((root.clone(), 0));
    while let Some((session, incarnation)) = next {
        if configured.len() > MAX_FRAMES {
            return Err(error("browser iframe discovery exceeded its work limit"));
        }
        cdp.frame_command(
            owner,
            &session,
            "Target.setAutoAttach",
            json!({
                "autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true,
                "filter": [{"type": "iframe", "exclude": false}, {"exclude": true}],
            }),
        )
        .await?;
        configured.insert((session, incarnation));
        next = cdp
            .frame_targets
            .ordered(&root)?
            .into_iter()
            .find(|link| !configured.contains(&(link.session.clone(), link.incarnation)))
            .map(|link| (link.session, link.incarnation));
    }
    cdp.frame_targets.ordered(&root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attach(parent: &str, session: &str, target: &str) -> Value {
        json!({
            "sessionId": parent, "method": "Target.attachedToTarget",
            "params": {"sessionId": session, "waitingForDebugger": false,
                "targetInfo": {"targetId": target, "type": "iframe"}},
        })
    }

    fn detach(parent: &str, session: &str) -> Value {
        json!({"sessionId": parent, "method": "Target.detachedFromTarget",
            "params": {"sessionId": session}})
    }

    #[test]
    fn only_owned_iframe_attachments_admit_sessions() {
        let mut targets = Targets::default();
        targets
            .observe(&attach("other-page", "foreign", "frame"), Some("page"))
            .unwrap();
        let mut worker = attach("page", "worker", "worker-target");
        worker["params"]["targetInfo"]["type"] = json!("worker");
        targets.observe(&worker, Some("page")).unwrap();
        let mut browser_event = attach("page", "foreign", "frame");
        browser_event.as_object_mut().unwrap().remove("sessionId");
        targets.observe(&browser_event, Some("page")).unwrap();
        assert!(targets.links.is_empty());
        targets
            .observe(&attach("page", "child", "child-frame"), Some("page"))
            .unwrap();
        targets
            .observe(&attach("child", "nested", "nested-frame"), Some("page"))
            .unwrap();
        assert!(targets.admits(Some("page"), "nested"));
        let links = targets.ordered("page").unwrap();
        assert_eq!(
            links
                .iter()
                .map(|link| link.session.as_str())
                .collect::<Vec<_>>(),
            ["child", "nested"]
        );
        assert!(targets.preserves(&links));
    }

    #[test]
    fn detach_retires_the_whole_subtree_and_reused_ids_are_new_incarnations() {
        let mut targets = Targets::default();
        for event in [
            attach("page", "child", "frame"),
            attach("child", "nested", "nested-frame"),
        ] {
            targets.observe(&event, Some("page")).unwrap();
        }
        let before = targets.ordered("page").unwrap();
        targets
            .observe(&detach("page", "child"), Some("page"))
            .unwrap();
        assert!(targets.links.is_empty());
        targets
            .observe(&attach("nested", "late", "late-frame"), Some("page"))
            .unwrap();
        assert!(targets.links.is_empty());
        targets
            .observe(&attach("page", "child", "frame"), Some("page"))
            .unwrap();
        assert!(!targets.preserves(&before[..1]));
        assert!(!targets.admits(Some("page"), "nested"));
    }

    #[test]
    fn conflicting_parent_target_and_root_identities_are_rejected() {
        for conflict in [
            attach("child", "child", "cycle"),
            attach("child", "page", "cycle"),
            attach("page", "child", "replacement"),
            attach("page", "another", "frame"),
            detach("child", "child"),
        ] {
            let mut targets = Targets::default();
            targets
                .observe(&attach("page", "child", "frame"), Some("page"))
                .unwrap();
            let generation = targets.generation();
            assert!(targets.observe(&conflict, Some("page")).is_err());
            assert_eq!(targets.generation(), generation);
            assert_eq!(targets.links.len(), 1);
        }
        let mut targets = Targets::default();
        assert!(
            targets
                .observe(&detach("browser", "page"), Some("page"))
                .is_err()
        );
    }

    #[test]
    fn duplicates_do_not_change_generation_but_metadata_and_live_count_are_bounded() {
        let mut targets = Targets::default();
        let event = attach("page", "child", "frame");
        targets.observe(&event, Some("page")).unwrap();
        let generation = targets.generation();
        targets.observe(&event, Some("page")).unwrap();
        assert_eq!(targets.generation(), generation);
        for invalid in [
            "".to_owned(),
            "x".repeat(257),
            "PRIVATE-CANARY\n".to_owned(),
        ] {
            let error = targets
                .observe(&attach("page", &invalid, "other"), Some("page"))
                .unwrap_err();
            assert!(!error.to_string().contains("PRIVATE-CANARY"));
        }
        for i in 1..MAX_FRAMES {
            targets
                .observe(
                    &attach("page", &format!("s-{i}"), &format!("f-{i}")),
                    Some("page"),
                )
                .unwrap();
        }
        assert_eq!(targets.links.len(), MAX_FRAMES);
        assert!(
            targets
                .observe(&attach("page", "overflow", "overflow"), Some("page"))
                .is_err()
        );
    }
}
