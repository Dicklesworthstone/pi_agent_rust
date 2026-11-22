//! RPC plan control delegates to the same guarded lifecycle as the SDK.
//!
//! A receipt names the exact proposal delivered by this connection. It is not
//! execution authority: the shared domain still checks proposal and session
//! incarnation under the decision lock before changing the gate or prompt.

use super::session::{self, Change};
use super::{PlanChange, PlanMode, PlanPersistence, SessionPlanReview};
use crate::agent::AgentSession;
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use serde_json::{Value, json};

#[derive(Default)]
pub struct RpcPlanControl {
    presented: Option<(String, SessionPlanReview)>,
}

impl RpcPlanControl {
    pub fn handles(command: &str) -> bool {
        matches!(
            command,
            "get_plan"
                | "set_plan_mode"
                | "approve_plan"
                | "reject_plan"
                | "save_plan"
                | "restore_plan"
        )
    }

    /// Run before RPC recovery preflight so malformed requests cannot write
    /// session history as a side effect of being rejected.
    pub fn validate(command: &str, payload: &Value) -> Result<()> {
        let fields = payload
            .as_object()
            .ok_or_else(|| Error::validation("Plan command must be an object"))?;
        if !Self::handles(command) {
            return Err(Error::validation("Unknown plan command"));
        }
        for field in fields.keys() {
            let allowed = matches!(field.as_str(), "type" | "id")
                || (command == "set_plan_mode" && field == "mode")
                || (matches!(command, "approve_plan" | "reject_plan") && field == "reviewId");
            if !allowed {
                return Err(Error::validation(format!(
                    "Unexpected field in {command}: {field}"
                )));
            }
        }
        if command == "set_plan_mode" {
            match payload.get("mode") {
                None => {}
                Some(Value::String(mode)) if matches!(mode.as_str(), "on" | "off") => {}
                _ => return Err(Error::validation("Plan mode must be the string on or off")),
            }
        }
        if matches!(command, "approve_plan" | "reject_plan") {
            let valid = payload
                .get("reviewId")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty() && value.len() <= 64);
            if !valid {
                return Err(Error::validation(
                    "[PLAN_REVIEW_REQUIRED] Present get_plan and echo its reviewId",
                ));
            }
        }
        Ok(())
    }

    fn state(&mut self, handle: &AgentSession) -> Result<Value> {
        let view = session::snapshot(handle)?;
        let review_id = if let Some(review) = view.review {
            let current = self
                .presented
                .as_ref()
                .filter(|(_, previous)| previous.same_submission(&review));
            if let Some((id, _)) = current {
                Some(id.clone())
            } else {
                let id = format!("p1-{}", uuid::Uuid::new_v4());
                self.presented = Some((id.clone(), review));
                Some(id)
            }
        } else {
            self.presented = None;
            None
        };
        Ok(json!({
            "planMode": view.mode.as_str(),
            "plan": view.plan.as_deref(),
            "reviewId": review_id,
        }))
    }

    fn review(&self, payload: &Value) -> Result<&SessionPlanReview> {
        let id = payload.get("reviewId").and_then(Value::as_str);
        self.presented
            .as_ref()
            .filter(|(presented, _)| Some(presented.as_str()) == id)
            .map(|(_, review)| review)
            .ok_or_else(|| {
                Error::validation(
                    "[PLAN_REVIEW_STALE] This review is no longer current; present get_plan again",
                )
            })
    }

    /// A committed transition remains a success even if a later observer
    /// cannot obtain a snapshot. Retrying a save never repeats an approval.
    fn outcome(
        &mut self,
        handle: &AgentSession,
        mode: PlanMode,
        changed: bool,
        persistence: PlanPersistence,
    ) -> Value {
        let mut result = self.state(handle).unwrap_or_else(|error| {
            json!({
                "planMode": mode.as_str(),
                "plan": null,
                "reviewId": null,
                "stateError": error.to_string(),
            })
        });
        result["changed"] = json!(changed);
        result["persistence"] = match persistence {
            PlanPersistence::Saved => json!({"status": "saved"}),
            PlanPersistence::MemoryOnly => json!({"status": "memory_only"}),
            PlanPersistence::Unchanged => json!({"status": "unchanged"}),
            PlanPersistence::Unconfirmed { reason } => {
                json!({"status": "unconfirmed", "reason": reason})
            }
        };
        result
    }

    pub async fn command(
        &mut self,
        handle: &mut AgentSession,
        owner: &AgentCx,
        command: &str,
        payload: &Value,
    ) -> Result<Value> {
        Self::validate(command, payload)?;
        if command == "get_plan" {
            return self.state(handle);
        }
        if command == "save_plan" {
            // Capture the live mode before the save. Observation afterwards is
            // best effort, so a successful save cannot be reported as rolled back.
            let mode = session::snapshot(handle)?.mode;
            let persistence = session::checkpoint_plan(handle, owner).await?;
            return Ok(self.outcome(handle, mode, false, persistence));
        }

        let mut approved_plan = None;
        let change: PlanChange = match command {
            "set_plan_mode" => {
                let action = if payload.get("mode").and_then(Value::as_str) == Some("off") {
                    Change::Exit
                } else {
                    Change::Enter
                };
                session::change(handle, owner, action).await?
            }
            "approve_plan" => {
                let review = self.review(payload)?;
                let plan = review.text().to_string();
                let changed = session::change(handle, owner, Change::Approve(review)).await?;
                approved_plan = Some(plan);
                changed
            }
            "reject_plan" => {
                let review = self.review(payload)?;
                session::change(handle, owner, Change::Reject(review)).await?
            }
            "restore_plan" => session::restore_plan_checkpoint(handle, owner).await?,
            _ => return Err(Error::validation("Unknown plan command")),
        };
        // Approval, rejection, exit and recovery retire the displayed decision
        // before attempting any optional post-transition observation.
        self.presented = None;
        let mut result = self.outcome(handle, change.mode, change.changed, change.persistence);
        if let Some(plan) = approved_plan {
            result["approved"] = json!(true);
            result["plan"] = json!(plan);
        }
        if command == "reject_plan" {
            result["rejected"] = json!(true);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentConfig};
    use crate::session::Session;
    use crate::tools::{Tool, ToolEffects, ToolRegistry};
    use std::sync::Arc;

    const PLAN: &str =
        "Goal: fix the parser. Steps: edit its grammar. Verification: run parser cases.";

    fn run<F: std::future::Future>(future: F) -> F::Output {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
            .build()
            .unwrap()
            .block_on(future)
    }

    fn fixture(stored: Session, save: bool) -> AgentSession {
        let provider = Arc::new(
            crate::providers::openai::OpenAIProvider::new("rpc-plan-fixture")
                .with_base_url("http://127.0.0.1:1/v1"),
        );
        let agent = Agent::new(
            provider,
            ToolRegistry::new(&[], std::path::Path::new("."), None),
            AgentConfig {
                system_prompt: Some("base instructions".to_string()),
                ..AgentConfig::default()
            },
        );
        AgentSession::new(
            agent,
            Arc::new(asupersync::sync::Mutex::new(stored)),
            save,
            crate::compaction::ResolvedCompactionSettings::default(),
        )
    }

    fn command(
        control: &mut RpcPlanControl,
        handle: &mut AgentSession,
        name: &str,
        payload: Value,
    ) -> Value {
        run(control.command(handle, &AgentCx::for_request(), name, &payload)).unwrap()
    }

    fn submit(handle: &AgentSession) {
        let tool = crate::plan::SubmitPlanTool::new(handle.agent.plan_state(), false);
        assert!(
            !run(tool.execute("submit", json!({"plan": PLAN}), None))
                .unwrap()
                .is_error
        );
    }

    fn pending(handle: &mut AgentSession, control: &mut RpcPlanControl) -> String {
        command(control, handle, "set_plan_mode", json!({"mode": "on"}));
        submit(handle);
        command(control, handle, "get_plan", json!({}))["reviewId"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn same_text_resubmission_retires_both_approval_and_rejection_receipts() {
        let mut handle = fixture(Session::in_memory(), false);
        let mut control = RpcPlanControl::default();
        let first = pending(&mut handle, &mut control);
        let repeated = command(&mut control, &mut handle, "get_plan", json!({}));
        assert_eq!(repeated["reviewId"], first);
        command(
            &mut control,
            &mut handle,
            "reject_plan",
            json!({"reviewId": first}),
        );
        submit(&handle);
        for name in ["approve_plan", "reject_plan"] {
            assert!(
                run(control.command(
                    &mut handle,
                    &AgentCx::for_request(),
                    name,
                    &json!({"reviewId": first}),
                ))
                .is_err()
            );
        }
        let second = command(&mut control, &mut handle, "get_plan", json!({}));
        assert_ne!(second["reviewId"], first);
        let approved = command(
            &mut control,
            &mut handle,
            "approve_plan",
            json!({"reviewId": second["reviewId"]}),
        );
        assert_eq!(approved["persistence"]["status"], "memory_only");
        assert_eq!(approved["plan"], PLAN);
        assert!(handle.agent.system_prompt().unwrap().contains(PLAN));
        command(
            &mut control,
            &mut handle,
            "set_plan_mode",
            json!({"mode": "off"}),
        );
        assert_eq!(handle.agent.system_prompt(), Some("base instructions"));
    }

    #[test]
    fn old_review_is_rejected_even_without_a_refresh_after_direct_replacement() {
        let mut handle = fixture(Session::in_memory(), false);
        let mut control = RpcPlanControl::default();
        let old = pending(&mut handle, &mut control);
        let state = handle.agent.plan_state();
        assert!(state.reject());
        submit(&handle);
        let before = handle.session.try_lock().unwrap().entries.len();
        let error = run(control.command(
            &mut handle,
            &AgentCx::for_request(),
            "approve_plan",
            &json!({"reviewId": old}),
        ))
        .unwrap_err();
        assert!(error.to_string().contains("PLAN_REVIEW_STALE"));
        assert_eq!(handle.session.try_lock().unwrap().entries.len(), before);
        assert!(!state.allows_effects(ToolEffects::write()));
        assert_eq!(handle.agent.system_prompt(), Some("base instructions"));
    }

    #[test]
    fn malformed_commands_and_pending_reentry_cannot_mutate_the_plan() {
        let mut handle = fixture(Session::in_memory(), false);
        let mut control = RpcPlanControl::default();
        let review = pending(&mut handle, &mut control);
        let before = handle.session.try_lock().unwrap().entries.len();
        for (name, payload) in [
            ("set_plan_mode", json!({"mode": "false"})),
            ("set_plan_mode", json!({"mode": true})),
            ("set_plan_mode", json!({"mode": "on", "extra": 1})),
            ("approve_plan", json!({})),
            ("approve_plan", json!({"reviewId": 7})),
            ("reject_plan", json!({"reviewId": "  "})),
            ("save_plan", json!({"reviewId": review})),
            ("restore_plan", json!({"force": true})),
            ("get_plan", Value::Null),
            ("set_plan_mode", json!({"mode": "on"})),
        ] {
            assert!(
                run(control.command(&mut handle, &AgentCx::for_request(), name, &payload)).is_err(),
                "{name}: {payload}"
            );
        }
        assert_eq!(handle.session.try_lock().unwrap().entries.len(), before);
        assert_eq!(
            command(&mut control, &mut handle, "get_plan", json!({}))["reviewId"],
            review
        );
    }

    #[test]
    fn saved_approval_restores_for_fresh_review_without_replaying_authority() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        let mut stored = Session::in_memory();
        stored.path = Some(path.clone());
        let mut handle = fixture(stored, true);
        let mut control = RpcPlanControl::default();
        let review = pending(&mut handle, &mut control);
        let approved = command(
            &mut control,
            &mut handle,
            "approve_plan",
            json!({"reviewId": review}),
        );
        assert_eq!(approved["persistence"]["status"], "saved");
        drop(handle);

        let opened = run(Session::open(path.to_str().unwrap())).unwrap();
        let replayed = crate::plan::replayed_plan_mode(&opened);
        let mut resumed = fixture(opened, true);
        resumed.agent.reset_session_scoped_state(replayed);
        let restored = command(&mut control, &mut resumed, "restore_plan", json!({}));
        assert_eq!(restored["planMode"], "pending_approval");
        assert_eq!(restored["plan"], PLAN);
        assert_ne!(restored["reviewId"], review);
        assert_eq!(restored["persistence"]["status"], "saved");
        assert!(
            run(control.command(
                &mut resumed,
                &AgentCx::for_request(),
                "approve_plan",
                &json!({"reviewId": review}),
            ))
            .is_err()
        );
        assert_eq!(resumed.agent.system_prompt(), Some("base instructions"));
        assert!(
            !resumed
                .agent
                .plan_state()
                .allows_effects(ToolEffects::write())
        );
    }

    #[test]
    fn unconfirmed_save_keeps_live_approval_and_save_plan_does_not_repeat_it() {
        let temp = tempfile::tempdir().unwrap();
        let blocker = temp.path().join("blocked-parent");
        std::fs::write(&blocker, b"sentinel").unwrap();
        let mut stored = Session::in_memory();
        stored.path = Some(blocker.join("session.jsonl"));
        let mut handle = fixture(stored, true);
        let mut control = RpcPlanControl::default();
        let review = pending(&mut handle, &mut control);
        let approved = command(
            &mut control,
            &mut handle,
            "approve_plan",
            json!({"reviewId": review}),
        );
        assert_eq!(approved["planMode"], "approved");
        assert_eq!(approved["persistence"]["status"], "unconfirmed");
        assert!(approved["persistence"]["reason"].is_string());
        let prompt = handle.agent.system_prompt().unwrap().to_string();
        let before = handle.session.try_lock().unwrap().entries.len();
        let path = temp.path().join("recovered.jsonl");
        handle.session.try_lock().unwrap().path = Some(path.clone());
        let saved = command(&mut control, &mut handle, "save_plan", json!({}));
        assert_eq!(saved["persistence"]["status"], "saved");
        assert_eq!(saved["changed"], false);
        assert_eq!(handle.session.try_lock().unwrap().entries.len(), before);
        assert_eq!(handle.agent.system_prompt(), Some(prompt.as_str()));
        assert!(path.is_file());
        assert_eq!(std::fs::read(blocker).unwrap(), b"sentinel");
    }

    #[test]
    fn nested_native_fork_restores_an_older_selected_checkpoint_for_fresh_review() {
        let mut source = fixture(Session::in_memory(), false);
        let mut control = RpcPlanControl::default();
        let review = pending(&mut source, &mut control);
        command(
            &mut control,
            &mut source,
            "approve_plan",
            json!({"reviewId": review}),
        );
        let store = Arc::clone(&source.session);
        let mut saved = store.try_lock().unwrap();
        let source_id = saved.header.id.clone();
        let selected = saved.append_message(crate::session::SessionMessage::User {
            content: crate::model::UserContent::Text("selected old input".to_string()),
            timestamp: Some(0),
        });
        let before = serde_json::to_vec(&saved.entries).unwrap();
        let mut target = Session::in_memory();
        target.init_from_fork_plan(saved.plan_fork_current_context().unwrap());
        crate::plan::inherit_fork_checkpoint(&mut target, &source_id).unwrap();
        for entry in &target.entries {
            if let crate::session::SessionEntry::Custom(custom) = entry
                && custom.custom_type == "plan_checkpoint"
            {
                let data = custom.data.as_ref().unwrap();
                assert_eq!(data["sessionId"], target.header.id);
                assert_ne!(data["mode"], "approved");
            }
        }
        let tip = target.leaf_id().unwrap().to_string();
        let target_before = serde_json::to_vec(&target.entries).unwrap();
        let mut nested = Session::in_memory();
        nested.init_from_fork_plan(target.plan_fork_from_user_message(&selected).unwrap());
        assert!(
            nested.get_entry(&tip).is_none(),
            "select before the first fork's appended checkpoint"
        );
        crate::plan::inherit_fork_checkpoint(&mut nested, &target.header.id).unwrap();
        assert_eq!(
            serde_json::to_vec(&target.entries).unwrap(),
            target_before,
            "the intermediate source is unchanged"
        );
        assert_eq!(
            serde_json::to_vec(&saved.entries).unwrap(),
            before,
            "the original source is unchanged"
        );
        drop(saved);
        let mut nested = fixture(nested, false);
        let result = command(&mut control, &mut nested, "restore_plan", json!({}));
        assert_eq!(result["planMode"], "pending_approval");
        assert_eq!(result["plan"], PLAN);
        assert_ne!(result["reviewId"], review);
        assert!(
            !nested
                .agent
                .plan_state()
                .allows_effects(ToolEffects::write())
        );
        assert_eq!(nested.agent.system_prompt(), Some("base instructions"));
    }

    #[test]
    fn fork_checkpoint_validation_is_staged_before_any_target_rebinding() {
        let mut source = fixture(Session::in_memory(), false);
        let mut control = RpcPlanControl::default();
        let review = pending(&mut source, &mut control);
        command(
            &mut control,
            &mut source,
            "approve_plan",
            json!({"reviewId": review}),
        );
        let store = Arc::clone(&source.session);
        let saved = store.try_lock().unwrap();
        let before = serde_json::to_vec(&saved.entries).unwrap();
        for damage in ["foreign", "malformed"] {
            let mut target = Session::in_memory();
            target.init_from_fork_plan(saved.plan_fork_current_context().unwrap());
            let earlier = target
                .entries
                .iter()
                .find_map(|entry| match entry {
                    crate::session::SessionEntry::Custom(custom)
                        if custom.custom_type == "plan_checkpoint" =>
                    {
                        custom.base.id.clone()
                    }
                    _ => None,
                })
                .unwrap();
            let crate::session::SessionEntry::Custom(custom) =
                target.get_entry_mut(&earlier).unwrap()
            else {
                unreachable!("selected checkpoint")
            };
            let data = custom.data.as_mut().unwrap();
            if damage == "foreign" {
                data["sessionId"] = json!("another-session");
            } else {
                data["mode"] = json!("unsupported-mode");
            }
            let damaged = serde_json::to_vec(&target.entries).unwrap();
            let leaf = target.leaf_id().map(str::to_string);
            let error =
                crate::plan::inherit_fork_checkpoint(&mut target, &saved.header.id).unwrap_err();
            assert!(error.to_string().contains("PLAN_CHECKPOINT_INVALID"));
            assert_eq!(serde_json::to_vec(&target.entries).unwrap(), damaged);
            assert_eq!(target.leaf_id(), leaf.as_deref());
        }
        assert_eq!(serde_json::to_vec(&saved.entries).unwrap(), before);
    }

    #[test]
    fn fork_rebinding_preserves_newer_raw_mode_supersession() {
        let mut source = fixture(Session::in_memory(), false);
        let mut control = RpcPlanControl::default();
        let review = pending(&mut source, &mut control);
        command(
            &mut control,
            &mut source,
            "approve_plan",
            json!({"reviewId": review}),
        );
        let store = Arc::clone(&source.session);
        let mut saved = store.try_lock().unwrap();
        let superseding =
            saved.append_custom_entry("plan_mode".to_string(), Some(json!({"mode": "off"})));
        let before = serde_json::to_vec(&saved.entries).unwrap();
        let mut target = Session::in_memory();
        target.init_from_fork_plan(saved.plan_fork_current_context().unwrap());
        let count = target.entries.len();
        crate::plan::inherit_fork_checkpoint(&mut target, &saved.header.id).unwrap();
        assert_eq!(target.entries.len(), count);
        assert_eq!(target.leaf_id(), Some(superseding.as_str()));
        assert_eq!(crate::plan::replayed_plan_mode(&target), PlanMode::Off);
        assert_eq!(serde_json::to_vec(&saved.entries).unwrap(), before);
        drop(saved);
        let mut target = fixture(target, false);
        let error = run(control.command(
            &mut target,
            &AgentCx::for_request(),
            "restore_plan",
            &json!({}),
        ))
        .unwrap_err();
        assert!(error.to_string().contains("PLAN_CHECKPOINT_MISSING"));
        assert_eq!(target.agent.plan_state().mode(), PlanMode::Off);
        assert_eq!(target.agent.system_prompt(), Some("base instructions"));
    }

    #[test]
    fn session_replacement_prepares_only_owned_pin_cleanup_and_refuses_tampering() {
        let mut handle = fixture(Session::in_memory(), false);
        let mut control = RpcPlanControl::default();
        let review = pending(&mut handle, &mut control);
        command(
            &mut control,
            &mut handle,
            "approve_plan",
            json!({"reviewId": review}),
        );
        let prompt = handle.agent.system_prompt().unwrap().to_string();
        handle
            .agent
            .set_system_prompt(Some(format!("prefix {prompt} suffix")));
        let store = Arc::clone(&handle.session);
        let source = store.try_lock().unwrap();
        assert_eq!(
            crate::plan::replacement_system_prompt(&handle.agent, &store, &source)
                .unwrap()
                .as_deref(),
            Some("prefix base instructions suffix")
        );
        handle
            .agent
            .set_system_prompt(Some("rewritten pin".to_string()));
        assert!(crate::plan::replacement_system_prompt(&handle.agent, &store, &source).is_err());
    }
}
