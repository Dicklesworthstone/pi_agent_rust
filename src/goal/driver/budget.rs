//! Stop a goal inside its native prompt when completed assistant usage reaches
//! the remaining allowance. This is a continuation boundary, not a billing cap.
//! No preview estimates, second provider loop, or replacement usage journal.

use std::sync::atomic::{AtomicU64, Ordering};

use super::GoalControl;
use crate::agent::AgentEvent;
use crate::model::Message;

pub(super) struct PromptBudget {
    remaining: u64,
    observed: AtomicU64,
}

impl PromptBudget {
    pub(super) const fn new(remaining: u64) -> Self {
        Self {
            remaining,
            observed: AtomicU64::new(0),
        }
    }

    pub(super) fn observe(&self, control: &GoalControl, event: &AgentEvent) {
        let AgentEvent::MessageEnd {
            message: Message::Assistant(message),
        } = event
        else {
            return;
        };
        // Exactly the same conservative total/component normalization as the
        // durable selected-branch journal. Never count cumulative previews or
        // AgentEnd's repeated message list, and do not reset on retry events.
        let tokens = super::super::usage_tokens(&message.usage);
        let previous = self
            .observed
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |spent| {
                Some(spent.saturating_add(tokens))
            })
            .unwrap_or_else(|spent| spent);
        if previous.saturating_add(tokens) >= self.remaining {
            // Latch before returning to the native loop. It owns tool-call
            // cancellation results, fetcher closure, persistence and drain.
            // A user pause that already won is not relabelled as a budget stop.
            control.request_stop(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{GoalDecision, GoalRunStop, settle_attempt, start_attempt};
    use super::*;
    use crate::agent::{Agent, AgentConfig, AgentSession};
    use crate::agent_cx::AgentCx;
    use crate::compaction::ResolvedCompactionSettings;
    use crate::error::Result;
    use crate::goal::{GoalBudget, GoalStatus};
    use crate::model::{
        AssistantMessage, ContentBlock, ImageContent, StopReason, StreamEvent, TextContent,
        ToolCall, Usage, UserContent, UserMessage,
    };
    use crate::provider::{Context, Provider, StreamOptions};
    use crate::sdk::{AgentSessionHandle, EventListeners};
    use crate::session::Session;
    use crate::session_control::TurnDeadline;
    use crate::tools::ToolRegistry;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    struct Replies {
        replies: Mutex<VecDeque<AssistantMessage>>,
        requests: Mutex<Vec<Vec<Message>>>,
    }

    #[async_trait::async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for Replies {
        fn name(&self) -> &str {
            "goal-budget-test"
        }
        fn api(&self) -> &str {
            "goal-budget-test"
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
            let message = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("budget enforcement admitted an unexpected provider request");
            Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Done {
                reason: message.stop_reason,
                message,
            })])))
        }
    }

    fn fixture(cwd: &Path, replies: Vec<AssistantMessage>) -> (AgentSessionHandle, Arc<Replies>) {
        let provider = Arc::new(Replies {
            replies: Mutex::new(replies.into()),
            ..Replies::default()
        });
        let agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(&["write"], cwd, None),
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

    fn runtime() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap()
    }

    fn final_reply(tokens: u64) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(
                "Checked the requested deliverables.",
            ))],
            stop_reason: StopReason::Stop,
            usage: Usage {
                total_tokens: tokens,
                ..Usage::default()
            },
            ..AssistantMessage::default()
        }
    }

    fn write_reply(id: &str, path: &str, tokens: u64) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: id.into(),
                name: "write".into(),
                arguments: json!({"path":path,"content":"native tool side effect\n"}),
                thought_signature: None,
            })],
            stop_reason: StopReason::ToolUse,
            ..final_reply(tokens)
        }
    }

    fn event(case: &str) {
        eprintln!(
            "{}",
            json!({"schema":"pi.goal.token_boundary.test.v1","case":case})
        );
    }

    fn ended(message: AssistantMessage) -> AgentEvent {
        AgentEvent::MessageEnd {
            message: Message::assistant(message),
        }
    }

    fn budget(tokens: u64) -> GoalBudget {
        GoalBudget {
            max_prompts: 8,
            token_budget: tokens,
        }
    }

    #[test]
    fn terminal_usage_normalizes_components_without_counting_previews() {
        event("terminal_usage_only");
        let control = GoalControl::new();
        let meter = PromptBudget::new(11);
        let message = AssistantMessage {
            usage: Usage {
                input: 2,
                output: 3,
                cache_read: 4,
                cache_write: 1,
                total_tokens: 3,
                ..Usage::default()
            },
            ..final_reply(0)
        };
        meter.observe(
            &control,
            &AgentEvent::MessageStart {
                message: Message::assistant(message.clone()),
            },
        );
        meter.observe(
            &control,
            &AgentEvent::MessageEnd {
                message: Message::User(UserMessage {
                    content: UserContent::Text(
                        "large input is not an invented token estimate".into(),
                    ),
                    timestamp: 0,
                }),
            },
        );
        assert_eq!(meter.observed.load(Ordering::Relaxed), 0);
        meter.observe(&control, &ended(message));
        assert_eq!(meter.observed.load(Ordering::Relaxed), 10);
        assert!(!control.paused());
        meter.observe(&control, &ended(final_reply(1)));
        assert!(control.paused());
        assert!(control.token_limit_reached());
        assert!(control.follow_up("too late").is_err());
    }

    #[test]
    fn observed_attempts_saturate_and_first_stop_wins() {
        event("retry_and_saturation");
        let control = GoalControl::new();
        let meter = PromptBudget::new(u64::MAX);
        meter.observe(&control, &ended(final_reply(u64::MAX - 1)));
        meter.observe(
            &control,
            &AgentEvent::AgentStart {
                session_id: "retry".into(),
            },
        );
        assert!(!control.token_limit_reached());
        meter.observe(&control, &ended(final_reply(5)));
        assert_eq!(meter.observed.load(Ordering::Relaxed), u64::MAX);
        assert!(control.token_limit_reached());
        assert!(!control.pause());
        let user_paused = GoalControl::new();
        assert!(user_paused.pause());
        meter.observe(&user_paused, &ended(final_reply(1)));
        assert!(
            !user_paused.token_limit_reached(),
            "an earlier explicit pause retains its cause"
        );
        let finished = GoalControl::new();
        finished.finish();
        meter.observe(&finished, &ended(final_reply(1)));
        assert!(
            !finished.token_limit_reached(),
            "late observations cannot reopen a finished run"
        );
    }

    #[test]
    fn native_tool_loop_stops_before_the_over_budget_round_executes() {
        event("native_tool_loop");
        let root = tempfile::tempdir().unwrap();
        let (mut handle, provider) = fixture(
            root.path(),
            vec![
                write_reply("first", "first.txt", 6),
                write_reply("second", "second.txt", 6),
            ],
        );
        runtime().block_on(async {
            let owner = AgentCx::for_current_or_request();
            let goal = handle
                .create_goal(&owner, "write and verify both files", budget(10))
                .await
                .unwrap();
            let run = handle
                .run_goal(
                    &owner,
                    &goal.goal,
                    Duration::from_secs(30),
                    |_, _| {},
                    |_, _| panic!("an exhausted native loop cannot request a host verdict"),
                )
                .unwrap();
            let control = run.control();
            let report = run.await;
            assert_eq!(report.stop, GoalRunStop::BudgetLimited);
            assert_eq!(report.goal.status(), GoalStatus::BudgetLimited);
            assert_eq!(report.goal.prompts_started(), 1);
            assert_eq!(report.goal.tokens_spent(), 12);
            assert!(!report.goal.interrupted());
            assert!(
                report.completion.is_some(),
                "retain the native post-drain result"
            );
            assert!(control.is_finished());
            assert!(control.take_pending().is_empty());
            assert!(handle.resume_goal(&owner, &report.goal).await.is_err());
        });
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
        assert_eq!(
            std::fs::read_to_string(root.path().join("first.txt")).unwrap(),
            "native tool side effect\n"
        );
        assert!(
            !root.path().join("second.txt").exists(),
            "budget exhaustion must reach native tool admission"
        );
    }

    #[test]
    fn budget_stop_retains_followups_and_skips_verifier() {
        event("pending_input_retained");
        let root = tempfile::tempdir().unwrap();
        let (mut handle, provider) = fixture(root.path(), vec![final_reply(7)]);
        runtime().block_on(async {
            let owner = AgentCx::for_current_or_request();
            let goal = handle
                .create_goal(&owner, "inspect the work", budget(7))
                .await
                .unwrap();
            let report = handle
                .run_goal(
                    &owner,
                    &goal.goal,
                    Duration::from_secs(30),
                    |control, event| {
                        if matches!(event, AgentEvent::AgentStart { .. }) {
                            control
                                .follow_up_with_content(
                                    &UserContent::Blocks(vec![
                                        ContentBlock::Text(TextContent::new("inspect this next")),
                                        ContentBlock::Image(ImageContent {
                                            data: "aGVsbG8=".into(),
                                            mime_type: "image/png".into(),
                                        }),
                                    ]),
                                    "user-authored follow-up",
                                )
                                .unwrap();
                        }
                    },
                    |_, _| panic!("no verifier after the observed budget stop"),
                )
                .unwrap()
                .await;
            assert_eq!(report.stop, GoalRunStop::BudgetLimited);
            assert_eq!(report.pending.len(), 1);
            assert_eq!(report.pending[0].text, "user-authored follow-up");
            let UserContent::Blocks(blocks) = &report.pending[0].content else {
                panic!("native attachments retained");
            };
            assert!(matches!(&blocks[1], ContentBlock::Image(image) if image.data == "aGVsbG8="));
        });
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            1,
            "the follow-up was not dispatched"
        );
    }

    #[test]
    fn server_continuations_obey_the_same_token_boundary() {
        event("server_pause_turn");
        let root = tempfile::tempdir().unwrap();
        let (mut handle, provider) = fixture(
            root.path(),
            vec![AssistantMessage {
                stop_reason: StopReason::PauseTurn,
                ..final_reply(5)
            }],
        );
        runtime().block_on(async {
            let owner = AgentCx::for_current_or_request();
            let goal = handle
                .create_goal(&owner, "inspect remote work", budget(5))
                .await
                .unwrap();
            let report = handle
                .run_goal(
                    &owner,
                    &goal.goal,
                    Duration::from_secs(30),
                    |_, _| {},
                    |_, _| panic!("server continuation crossed the goal budget"),
                )
                .unwrap()
                .await;
            assert_eq!(report.stop, GoalRunStop::BudgetLimited);
            assert_eq!(report.goal.tokens_spent(), 5);
            assert_eq!(report.goal.prompts_started(), 1);
        });
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn under_budget_tool_work_can_still_be_verified_complete() {
        event("under_budget_native_completion");
        let root = tempfile::tempdir().unwrap();
        let (mut handle, provider) = fixture(
            root.path(),
            vec![write_reply("write", "result.txt", 3), final_reply(2)],
        );
        runtime().block_on(async {
            let owner = AgentCx::for_current_or_request();
            let goal = handle
                .create_goal(&owner, "write the result", budget(100))
                .await
                .unwrap();
            let report = handle
                .run_goal(
                    &owner,
                    &goal.goal,
                    Duration::from_secs(30),
                    |_, _| {},
                    |_, _| GoalDecision::Complete {
                        evidence: "host inspected the native result".into(),
                    },
                )
                .unwrap()
                .await;
            assert_eq!(report.stop, GoalRunStop::Complete);
            assert_eq!(report.goal.status(), GoalStatus::Complete);
            assert_eq!(
                report.goal.tokens_spent(),
                5,
                "MessageStart/TurnEnd/AgentEnd do not charge usage again"
            );
            assert!(report.error.is_none());
        });
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
        assert!(root.path().join("result.txt").is_file());
    }

    #[test]
    fn resumed_goals_get_only_remaining_allowance_until_explicit_revision() {
        event("resume_remaining_allowance");
        let root = tempfile::tempdir().unwrap();
        let (mut handle, provider) = fixture(
            root.path(),
            vec![final_reply(3), final_reply(5), final_reply(2)],
        );
        runtime().block_on(async {
            let owner = AgentCx::for_current_or_request();
            let created = handle
                .create_goal(&owner, "verify each stage", budget(8))
                .await
                .unwrap();
            let first = handle
                .run_goal(
                    &owner,
                    &created.goal,
                    Duration::from_secs(30),
                    |_, _| {},
                    |_, _| GoalDecision::Pause {
                        reason: "review first stage".into(),
                    },
                )
                .unwrap()
                .await;
            assert_eq!(first.goal.tokens_spent(), 3);
            let resumed = handle.resume_goal(&owner, &first.goal).await.unwrap();
            let run = handle
                .run_goal(
                    &owner,
                    &resumed.goal,
                    Duration::from_secs(30),
                    |_, _| {},
                    |_, _| panic!("only five tokens remained, not a fresh allowance of eight"),
                )
                .unwrap();
            let old_control = run.control();
            let limited = run.await;
            assert_eq!(limited.stop, GoalRunStop::BudgetLimited);
            assert_eq!(limited.goal.tokens_spent(), 8);
            assert_eq!(limited.goal.prompts_started(), 2);
            assert!(handle.resume_goal(&owner, &limited.goal).await.is_err());
            let revised = handle
                .set_goal_budget(
                    &owner,
                    &limited.goal,
                    budget(20),
                    "reviewed additional work",
                )
                .await
                .unwrap();
            assert_eq!(revised.goal.status(), GoalStatus::Paused);
            let resumed = handle.resume_goal(&owner, &revised.goal).await.unwrap();
            let next = handle
                .run_goal(
                    &owner,
                    &resumed.goal,
                    Duration::from_secs(30),
                    |_, _| {},
                    |_, _| GoalDecision::Complete {
                        evidence: "host checked the final stage".into(),
                    },
                )
                .unwrap();
            assert!(!old_control.pause());
            assert!(old_control.follow_up("stale authority").is_err());
            let done = next.await;
            assert_eq!(done.stop, GoalRunStop::Complete);
            assert_eq!(done.goal.tokens_spent(), 10);
            assert_eq!(done.goal.prompts_started(), 3);
            assert_eq!(done.goal.id(), created.goal.id());
        });
        assert_eq!(provider.requests.lock().unwrap().len(), 3);
    }

    #[test]
    fn observed_limit_wins_late_verdict_without_inventing_recorded_usage() {
        event("settlement_precedence");
        let root = tempfile::tempdir().unwrap();
        let (mut handle, provider) = fixture(root.path(), vec![]);
        runtime().block_on(async {
            let owner = AgentCx::for_current_or_request();
            let created = handle
                .create_goal(&owner, "inspect retained work", budget(10))
                .await
                .unwrap();
            let intent = start_attempt(&mut handle, &owner, &created.goal)
                .await
                .unwrap();
            let control = GoalControl::new();
            assert!(control.request_stop(true));
            let deadline = TurnDeadline::after(&owner, Duration::from_secs(30)).unwrap();
            let (change, stop) = settle_attempt(
                &mut handle,
                &owner,
                &intent.goal,
                GoalDecision::Complete {
                    evidence: "late verdict must lose".into(),
                },
                &deadline,
                &control,
            )
            .await
            .unwrap();
            assert_eq!(stop, Some(GoalRunStop::BudgetLimited));
            assert_eq!(change.goal.status(), GoalStatus::BudgetLimited);
            assert_eq!(
                change.goal.tokens_spent(),
                0,
                "an observation is not fabricated selected-branch usage"
            );
            assert!(!change.goal.interrupted());
            assert!(
                change
                    .goal
                    .note()
                    .contains("observed assistant token budget")
            );
            assert!(handle.resume_goal(&owner, &change.goal).await.is_err());
        });
        assert!(provider.requests.lock().unwrap().is_empty());
    }
}
