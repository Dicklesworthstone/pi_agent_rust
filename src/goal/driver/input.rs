//! Live goal input uses the current native turn's bounded mailbox. There is
//! no second queue, payload conversion, attachment expansion or replay loop.

use super::GoalControl;
use crate::error::Result;
use crate::model::UserContent;
use crate::session_control::{ControlSnapshot, InputId, InputKind, PendingInput, TransferredInput};

impl GoalControl {
    /// Queue ordered native text/image/media blocks at a steering boundary.
    /// `authored_source` is the original user prose, not expanded attachment
    /// contents. Native validation, queue limits and tool policy still apply.
    /// No file or URL is read here. Admission is not provider acknowledgement.
    ///
    /// Input before the first native prompt or during host verification fails
    /// with GOAL_INPUT_IDLE. Paused/finished goals reject further admission;
    /// no request is silently redirected to a later prompt or goal run.
    pub fn steer_with_content(
        &self,
        content: &UserContent,
        authored_source: &str,
    ) -> Result<InputId> {
        self.current_turn()?
            .steer_with_content(content, authored_source)
    }

    /// Queue native user content for a follow-up in the current native prompt.
    /// This does not authorize another goal-driver prompt, change goal limits,
    /// or acknowledge completion. Unclaimed content survives abort/drop intact.
    pub fn follow_up_with_content(
        &self,
        content: &UserContent,
        authored_source: &str,
    ) -> Result<InputId> {
        self.current_turn()?
            .follow_up_with_content(content, authored_source)
    }

    /// Atomically admit a batch to ONE current native turn. Each tuple is
    /// (steering/follow-up lane, native content, original authored source).
    /// Validation or capacity failure admits none of the batch. Only queue
    /// admission is atomic: native execution can consume part before stopping.
    pub fn enqueue_batch_with_content(
        &self,
        inputs: &[(InputKind, UserContent, String)],
    ) -> Result<Vec<InputId>> {
        self.current_turn()?.enqueue_batch_with_content(inputs)
    }

    /// Explicitly restore detached, unclaimed input to this goal's currently
    /// accepting native turn. Success empties the supplied vector and returns
    /// fresh identity mappings; failure leaves both source and mailbox intact.
    /// Lanes, attachment blocks and authored source survive revalidation.
    ///
    /// This never resumes an idle/paused goal or targets a future prompt. Use
    /// only after reviewing the prior run's outcome and choosing to resend its
    /// unclaimed input. It is not durable deduplication or permission to replay
    /// already-delivered messages; only native batch admission is atomic.
    pub fn restore_pending(
        &self,
        pending: &mut Vec<PendingInput>,
    ) -> Result<Vec<TransferredInput>> {
        self.current_turn()?.restore_pending(pending)
    }

    /// Correct one unclaimed message in place without moving it to the tail.
    /// A new identity retires the old one. None means the item is no longer
    /// pending, NOT permission to resend it. Closed goals cannot edit input.
    pub fn replace_pending(&self, id: InputId, text: &str) -> Result<Option<InputId>> {
        self.current_turn()?.replace_pending(id, text)
    }

    /// Attachment-aware in-place correction with native bounds and authorship.
    /// The original lane and position survive. Failure leaves the old message
    /// untouched; already-delivered content can never be changed retroactively.
    pub fn replace_pending_with_content(
        &self,
        id: InputId,
        content: &UserContent,
        authored_source: &str,
    ) -> Result<Option<InputId>> {
        self.current_turn()?
            .replace_pending_with_content(id, content, authored_source)
    }

    /// Recover exactly one unclaimed input without draining its neighbours.
    /// Allowed after pause or abandonment; the original payload and authored
    /// text are returned once. A report that already owns the item wins instead.
    pub fn retract(&self, id: InputId) -> Option<PendingInput> {
        self.lock().turn.as_ref()?.retract(id)
    }

    /// Non-authoritative snapshot of the current native mailbox, not lifetime
    /// goal totals. None means no native turn is installed (including verifier
    /// gaps and normally completed reports). After abandonment a retained closed
    /// mailbox remains inspectable until its owning control handles are dropped.
    /// Use is_finished() for the outer goal's lifetime. Input can race a snapshot;
    /// the actual admission/retraction result is always authoritative.
    pub fn turn_snapshot(&self) -> Option<ControlSnapshot> {
        let state = self.lock();
        let mut snapshot = state.turn.as_ref()?.snapshot();
        snapshot.accepting_input &= !state.paused && !state.finished;
        drop(state);
        Some(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{GoalDecision, GoalRunStop, RunGuard};
    use super::*;
    use crate::agent::{Agent, AgentConfig, AgentEvent, AgentSession};
    use crate::agent_cx::AgentCx;
    use crate::compaction::ResolvedCompactionSettings;
    use crate::goal::{GoalBudget, GoalStatus};
    use crate::model::{
        AssistantMessage, ContentBlock, ImageContent, MediaContent, Message, StopReason,
        StreamEvent, TextContent,
    };
    use crate::provider::{Context, Provider, StreamOptions};
    use crate::sdk::{AgentSessionHandle, EventListeners};
    use crate::session::Session;
    use crate::tools::ToolRegistry;
    use serde_json::json;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    struct Replies {
        requests: Mutex<Vec<Vec<Message>>>,
    }

    #[async_trait::async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for Replies {
        fn name(&self) -> &str {
            "goal-input-test"
        }
        fn api(&self) -> &str {
            "goal-input-test"
        }
        fn model_id(&self) -> &str {
            "controlled"
        }

        async fn stream(
            &self,
            context: &Context<'_>,
            _options: &StreamOptions,
        ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
            self.requests
                .lock()
                .unwrap()
                .push(context.messages.to_vec());
            let message = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("checked"))],
                stop_reason: StopReason::Stop,
                ..AssistantMessage::default()
            };
            Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            })])))
        }
    }

    fn fixture() -> (AgentSessionHandle, Arc<Replies>) {
        let provider = Arc::new(Replies::default());
        let agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig::default(),
        );
        let session = AgentSession::new(
            agent,
            Arc::new(asupersync::sync::Mutex::new(Session::in_memory())),
            false,
            ResolvedCompactionSettings::default(),
        );
        (
            AgentSessionHandle::from_session_with_listeners(session, EventListeners::default()),
            provider,
        )
    }

    fn payload() -> UserContent {
        UserContent::Blocks(vec![
            ContentBlock::Text(TextContent::new("expanded document; not authored keywords")),
            ContentBlock::Image(ImageContent {
                data: "aGVsbG8=".into(),
                mime_type: "image/png".into(),
            }),
            ContentBlock::Media(MediaContent {
                data: "d29ybGQ=".into(),
                mime_type: "audio/wav".into(),
                name: Some("clip.wav".into()),
            }),
        ])
    }

    fn event(case: &str) {
        eprintln!("{}", json!({"schema":"pi.goal.input.test.v1","case":case}));
    }

    fn same_content(left: &UserContent, right: &UserContent) {
        assert_eq!(
            serde_json::to_value(left).unwrap(),
            serde_json::to_value(right).unwrap()
        );
    }

    #[test]
    fn native_payload_batch_correction_and_retraction_share_one_mailbox() {
        event("native_payload_mailbox");
        let (mut handle, provider) = fixture();
        let turn = handle.prompt_controlled("unused".into(), |_| {});
        let native = turn.control();
        let goal = GoalControl::new();
        goal.install(&native);
        let first = goal.follow_up("first").unwrap();
        let original = payload();
        let ids = goal
            .enqueue_batch_with_content(&[
                (
                    InputKind::Steering,
                    original.clone(),
                    "  authored screenshot  ".into(),
                ),
                (
                    InputKind::FollowUp,
                    UserContent::Text("third".into()),
                    "third".into(),
                ),
            ])
            .unwrap();
        assert_eq!(goal.turn_snapshot(), Some(native.snapshot()));
        assert_eq!(native.snapshot().pending_steering, 1);
        assert_eq!(native.snapshot().pending_follow_up, 2);
        let replacement = UserContent::Blocks(vec![ContentBlock::Image(ImageContent {
            data: "bmV3".into(),
            mime_type: "image/png".into(),
        })]);
        let corrected = goal
            .replace_pending_with_content(ids[0], &replacement, "corrected source")
            .unwrap()
            .unwrap();
        assert_ne!(corrected, ids[0]);
        assert!(goal.retract(ids[0]).is_none());
        assert!(
            goal.replace_pending(ids[0], "stale edit")
                .unwrap()
                .is_none()
        );
        assert_eq!(goal.retract(ids[1]).unwrap().text, "third");
        let pending = native.take_pending();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].id, first);
        assert_eq!(pending[1].id, corrected);
        assert_eq!(pending[1].kind, InputKind::Steering);
        assert_eq!(pending[1].text, "corrected source");
        same_content(&pending[1].content, &replacement);
        same_content(&original, &payload());
        assert!(goal.take_pending().is_empty());
        assert_eq!(goal.turn_snapshot().unwrap().pending_bytes, 0);
        assert!(
            provider.requests.lock().unwrap().is_empty(),
            "queue operations never poll the provider"
        );
        drop(turn);
    }

    #[test]
    fn failed_batch_and_correction_leave_existing_input_unchanged() {
        event("atomic_refusal");
        let (mut handle, _) = fixture();
        let turn = handle.prompt_controlled("unused".into(), |_| {});
        let goal = GoalControl::new();
        goal.install(&turn.control());
        let id = goal
            .steer_with_content(&payload(), "keep original")
            .unwrap();
        let before = goal.turn_snapshot();
        assert!(
            goal.enqueue_batch_with_content(&[
                (
                    InputKind::FollowUp,
                    UserContent::Text("valid first".into()),
                    "valid".into()
                ),
                (
                    InputKind::Steering,
                    UserContent::Blocks(Vec::new()),
                    String::new()
                ),
            ])
            .is_err()
        );
        assert_eq!(goal.turn_snapshot(), before);
        assert!(
            goal.replace_pending_with_content(id, &UserContent::Blocks(Vec::new()), "invalid")
                .is_err()
        );
        assert_eq!(goal.turn_snapshot(), before);
        for _ in 0..98 {
            goal.follow_up("queued").unwrap();
        }
        let full = goal.turn_snapshot();
        assert!(
            goal.enqueue_batch_with_content(&[
                (
                    InputKind::FollowUp,
                    UserContent::Text("one".into()),
                    "one".into()
                ),
                (
                    InputKind::FollowUp,
                    UserContent::Text("two".into()),
                    "two".into()
                ),
            ])
            .is_err()
        );
        assert_eq!(goal.turn_snapshot(), full);
        let recovered = goal.retract(id).unwrap();
        assert_eq!(recovered.text, "keep original");
        same_content(&recovered.content, &payload());
    }

    #[test]
    fn paused_and_abandoned_controls_refuse_edits_but_recover_native_payloads() {
        event("closed_recovery");
        let (mut handle, _) = fixture();
        let turn = handle.prompt_controlled("unused".into(), |_| {});
        let goal = GoalControl::new();
        goal.install(&turn.control());
        let id = goal
            .follow_up_with_content(&payload(), "private authored prose")
            .unwrap();
        assert!(goal.pause());
        assert!(!goal.turn_snapshot().unwrap().accepting_input);
        assert!(goal.steer_with_content(&payload(), "late").is_err());
        assert!(goal.replace_pending(id, "late edit").is_err());
        let guard = RunGuard(goal.clone());
        drop(guard);
        drop(turn);
        assert!(goal.is_finished());
        assert!(goal.turn_snapshot().unwrap().finished);
        let recovered = goal.retract(id).unwrap();
        assert_eq!(recovered.text, "private authored prose");
        same_content(&recovered.content, &payload());
        assert!(goal.take_pending().is_empty());
        assert!(goal.retract(id).is_none());
        let debug = format!("{goal:?}");
        assert!(!debug.contains("private authored prose"));
        assert!(debug.contains("prompt_live: false"));
    }

    #[test]
    fn idle_and_old_goal_handles_cannot_target_a_replacement_turn() {
        event("stale_control");
        let goal = GoalControl::new();
        assert!(goal.turn_snapshot().is_none());
        assert!(
            goal.steer_with_content(&payload(), "idle")
                .unwrap_err()
                .to_string()
                .contains("GOAL_INPUT_IDLE")
        );
        let (mut handle, _) = fixture();
        let first = handle.prompt_controlled("first".into(), |_| {});
        goal.install(&first.control());
        let old_id = goal.steer("old").unwrap();
        drop(RunGuard(goal.clone()));
        drop(first);
        let second = handle.prompt_controlled("second".into(), |_| {});
        let replacement = GoalControl::new();
        replacement.install(&second.control());
        let new_id = replacement.steer("new").unwrap();
        assert!(goal.follow_up_with_content(&payload(), "stale").is_err());
        assert!(goal.retract(new_id).is_none());
        assert!(replacement.retract(old_id).is_none());
        assert!(!goal.pause());
        assert!(replacement.turn_snapshot().unwrap().accepting_input);
        assert_eq!(goal.retract(old_id).unwrap().text, "old");
        assert_eq!(replacement.retract(new_id).unwrap().text, "new");
    }

    #[test]
    fn concurrent_retraction_and_drain_transfer_each_input_once() {
        event("concurrent_recovery");
        let (mut handle, _) = fixture();
        let turn = handle.prompt_controlled("unused".into(), |_| {});
        let goal = GoalControl::new();
        goal.install(&turn.control());
        let first = goal.steer_with_content(&payload(), "first").unwrap();
        let second = goal.follow_up("second").unwrap();
        drop(RunGuard(goal.clone()));
        drop(turn);
        let ready = Arc::new(Barrier::new(3));
        let retracting = goal.clone();
        let start = Arc::clone(&ready);
        let single = std::thread::spawn(move || {
            start.wait();
            retracting.retract(first)
        });
        let draining = goal.clone();
        let start = Arc::clone(&ready);
        let all = std::thread::spawn(move || {
            start.wait();
            draining.take_pending()
        });
        ready.wait();
        let mut recovered = all.join().unwrap();
        recovered.extend(single.join().unwrap());
        assert_eq!(recovered.len(), 2);
        assert_eq!(
            recovered.iter().filter(|input| input.id == first).count(),
            1
        );
        assert_eq!(
            recovered.iter().filter(|input| input.id == second).count(),
            1
        );
        assert!(goal.take_pending().is_empty());
    }

    #[test]
    fn end_of_turn_input_reaches_goal_report_intact_without_another_prompt() {
        event("native_goal_report");
        let (mut handle, provider) = fixture();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let owner = AgentCx::for_current_or_request();
            let created = handle
                .create_goal(&owner, "inspect work", GoalBudget::default())
                .await
                .unwrap();
            let run = handle
                .run_goal(
                    &owner,
                    &created.goal,
                    Duration::from_secs(30),
                    |control, event| {
                        if matches!(event, AgentEvent::AgentEnd { .. }) {
                            control
                                .steer_with_content(&payload(), "user screenshot")
                                .unwrap();
                            control
                                .follow_up_with_content(
                                    &UserContent::Text("next task".into()),
                                    "next task",
                                )
                                .unwrap();
                        }
                    },
                    |_, _| {
                        panic!(
                            "unclaimed user input must prevent automatic verification/continuation"
                        )
                    },
                )
                .unwrap();
            let control = run.control();
            let report = run.await;
            assert_eq!(report.stop, GoalRunStop::Paused);
            assert_eq!(report.goal.status(), GoalStatus::Paused);
            assert_eq!(report.goal.prompts_started(), 1);
            assert_eq!(report.pending.len(), 2);
            assert_eq!(report.pending[0].kind, InputKind::Steering);
            assert_eq!(report.pending[0].text, "user screenshot");
            same_content(&report.pending[0].content, &payload());
            assert_eq!(report.pending[1].kind, InputKind::FollowUp);
            assert!(
                control.take_pending().is_empty(),
                "report owns the payload exactly once"
            );
            assert!(control.turn_snapshot().is_none());
            assert!(control.retract(report.pending[0].id).is_none());
            assert!(control.is_finished());
        });
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn typed_follow_up_is_delivered_through_the_native_goal_prompt() {
        event("native_goal_delivery");
        let (mut handle, provider) = fixture();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let content = UserContent::Blocks(vec![ContentBlock::Text(TextContent::new(
            "expanded follow-up content",
        ))]);
        let queued = content.clone();
        runtime.block_on(async {
            let owner = AgentCx::for_current_or_request();
            let created = handle
                .create_goal(&owner, "inspect work", GoalBudget::default())
                .await
                .unwrap();
            let report = handle
                .run_goal(
                    &owner,
                    &created.goal,
                    Duration::from_secs(30),
                    move |control, event| {
                        if matches!(event, AgentEvent::AgentStart { .. }) {
                            control
                                .follow_up_with_content(&queued, "original authored instruction")
                                .unwrap();
                        }
                    },
                    |_, _| GoalDecision::Pause {
                        reason: "review native outcome".into(),
                    },
                )
                .unwrap()
                .await;
            assert_eq!(report.stop, GoalRunStop::Paused);
            assert_eq!(
                report.goal.prompts_started(),
                1,
                "native follow-ups are not extra goal prompts"
            );
            assert!(report.pending.is_empty());
            assert!(report.error.is_none());
        });
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].iter().any(|message| {
            matches!(message, Message::User(user) if serde_json::to_value(&user.content).unwrap() == serde_json::to_value(&content).unwrap())
        }));
        drop(requests);
    }

    #[test]
    fn detached_recovery_refuses_idle_or_closed_goals_without_consuming_input() {
        event("goal_detached_admission");
        let (mut handle, provider) = fixture();
        let old = handle.prompt_controlled("old".into(), |_| {});
        let source = old.control();
        let previous_id = source
            .follow_up_with_content(&payload(), "recovered image")
            .unwrap();
        drop(old);
        let mut pending = source.take_pending();
        let goal = GoalControl::new();
        assert!(
            goal.restore_pending(&mut pending)
                .unwrap_err()
                .to_string()
                .contains("GOAL_INPUT_IDLE")
        );
        assert_eq!(pending[0].id, previous_id);
        same_content(&pending[0].content, &payload());
        let target = handle.prompt_controlled("target".into(), |_| {});
        goal.install(&target.control());
        let closed = GoalControl::new();
        closed.pause();
        assert!(
            closed
                .restore_pending(&mut pending)
                .unwrap_err()
                .to_string()
                .contains("GOAL_INPUT_CLOSED")
        );
        assert_eq!(pending[0].id, previous_id);
        let receipts = goal.restore_pending(&mut pending).unwrap();
        assert!(pending.is_empty());
        assert_eq!(receipts[0].previous_id, previous_id);
        assert_ne!(receipts[0].new_id, previous_id);
        assert!(goal.retract(previous_id).is_none());
        let recovered = goal.retract(receipts[0].new_id).unwrap();
        same_content(&recovered.content, &payload());
        assert_eq!(recovered.text, "recovered image");
        assert!(
            provider.requests.lock().unwrap().is_empty(),
            "restoration alone dispatches no prompt"
        );
    }

    #[test]
    fn reviewed_goal_report_inputs_can_be_restored_into_one_later_native_turn() {
        event("goal_report_restoration");
        let (mut handle, provider) = fixture();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let content = UserContent::Blocks(vec![ContentBlock::Text(TextContent::new(
            "retained expanded instruction",
        ))]);
        let for_first = content.clone();
        runtime.block_on(async {
            let owner = AgentCx::for_current_or_request();
            let created = handle
                .create_goal(&owner, "inspect work", GoalBudget::default())
                .await
                .unwrap();
            let first = handle
                .run_goal(
                    &owner,
                    &created.goal,
                    Duration::from_secs(30),
                    move |control, event| {
                        if matches!(event, AgentEvent::AgentEnd { .. }) {
                            control
                                .follow_up_with_content(&for_first, "user instruction to retain")
                                .unwrap();
                        }
                    },
                    |_, _| panic!("unclaimed input must stop verification"),
                )
                .unwrap();
            let old_control = first.control();
            let mut report = first.await;
            assert_eq!(report.pending.len(), 1);
            let old_id = report.pending[0].id;
            assert!(old_control.restore_pending(&mut report.pending).is_err());
            assert_eq!(report.pending[0].id, old_id);
            // Explicit host review/resume, not automatic replay or a new goal.
            let resumed = handle.resume_goal(&owner, &report.goal).await.unwrap();
            let pending = Arc::new(Mutex::new(report.pending));
            let source = Arc::clone(&pending);
            let restored = Arc::new(Mutex::new(Vec::new()));
            let receipts = Arc::clone(&restored);
            let next = handle
                .run_goal(
                    &owner,
                    &resumed.goal,
                    Duration::from_secs(30),
                    move |control, event| {
                        if matches!(event, AgentEvent::AgentStart { .. }) {
                            let mappings = control
                                .restore_pending(&mut source.lock().unwrap())
                                .unwrap();
                            receipts.lock().unwrap().extend(mappings);
                        }
                    },
                    |_, _| GoalDecision::Pause {
                        reason: "review restored input outcome".into(),
                    },
                )
                .unwrap()
                .await;
            assert_eq!(next.stop, GoalRunStop::Paused);
            assert_eq!(next.goal.id(), created.goal.id());
            assert_eq!(next.goal.prompts_started(), 2);
            assert!(next.pending.is_empty());
            assert!(next.error.is_none());
            assert!(pending.lock().unwrap().is_empty());
            let receipts = restored.lock().unwrap();
            assert_eq!(receipts.len(), 1);
            assert_eq!(receipts[0].previous_id, old_id);
            assert_ne!(receipts[0].new_id, old_id);
        });
        let requests = provider.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            3,
            "one old prompt and one resumed prompt with one restored follow-up"
        );
        assert_eq!(requests.last().unwrap().iter().filter(|message| {
            matches!(message, Message::User(user) if serde_json::to_value(&user.content).unwrap() == serde_json::to_value(&content).unwrap())
        }).count(), 1, "the pending input must be delivered once, not regenerated or duplicated");
    }
}
