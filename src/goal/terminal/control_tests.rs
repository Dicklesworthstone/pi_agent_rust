//! Native goal/session tests with a controlled Provider boundary. These are
//! not live-provider or PTY acceptance. Existing terminal journal tests remain.

use super::*;
use crate::agent::{Agent, AgentConfig, AgentEvent, AgentSession};
use crate::compaction::ResolvedCompactionSettings;
use crate::error::Error;
use crate::goal::GoalRunStop;
use crate::model::{StopReason, StreamEvent, TextContent, Usage};
use crate::provider::{Context, Provider, StreamOptions};
use crate::sdk::EventListeners;
use crate::session_control::TurnDeadlineError;
use crate::tools::ToolRegistry;
use std::collections::VecDeque;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct Replies {
    values: Mutex<VecDeque<Result<String>>>,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for Replies {
    fn name(&self) -> &str {
        "terminal-goal-test"
    }
    fn api(&self) -> &str {
        "terminal-goal-test"
    }
    fn model_id(&self) -> &str {
        "controlled"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = self
            .values
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected extra provider request")?;
        let message = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            stop_reason: StopReason::Stop,
            model: "controlled".into(),
            provider: "terminal-goal-test".into(),
            usage: Usage {
                input: 10,
                output: 5,
                total_tokens: 15,
                ..Usage::default()
            },
            ..AssistantMessage::default()
        };
        Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Done {
            reason: StopReason::Stop,
            message,
        })])))
    }
}

fn fixture(replies: Vec<Result<String>>) -> (AgentSessionHandle, Arc<Replies>) {
    let provider = Arc::new(Replies {
        values: Mutex::new(replies.into()),
        calls: AtomicUsize::new(0),
    });
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

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
}

fn receipt_from(text: &str) -> String {
    text.lines()
        .find_map(|line| line.strip_prefix("Review: "))
        .unwrap()
        .to_owned()
}

async fn create(handle: &mut AgentSessionHandle) -> String {
    let owner = AgentCx::for_current_or_request();
    let _ = handle
        .create_goal(
            &owner,
            "implement and inspect the requested feature",
            GoalBudget {
                max_prompts: 8,
                token_budget: 1000,
            },
        )
        .await
        .unwrap();
    receipt_from(&run(handle, "status").await.unwrap())
}

fn event(case: &str) {
    eprintln!(
        "{}",
        serde_json::json!({"schema":"pi.goal.terminal.test.v1","case":case})
    );
}

#[test]
fn published_control_can_pause_before_any_intent_or_provider_request() {
    event("pause_before_dispatch");
    let (mut handle, provider) = fixture(vec![]);
    runtime().block_on(async {
        let review = create(&mut handle).await;
        let before = handle
            .session_store()
            .try_lock()
            .unwrap()
            .leaf_id()
            .map(str::to_owned);
        let retained = Arc::new(Mutex::new(None));
        let published = Arc::clone(&retained);
        let output = run_with_control(
            &mut handle,
            &format!("run {review} --steps 4"),
            move |control| {
                assert!(control.pause());
                *published.lock().unwrap() = Some(control);
            },
        )
        .await
        .unwrap();
        assert!(output.contains("Goal run stopped: Paused"));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(handle.current_goal().unwrap().unwrap().prompts_started(), 0);
        assert_eq!(
            handle
                .session_store()
                .try_lock()
                .unwrap()
                .leaf_id()
                .map(str::to_owned),
            before
        );
        let control = retained.lock().unwrap().take().unwrap();
        assert!(control.is_finished());
        assert!(!control.pause());
        assert!(control.steer("must not reach a later run").is_err());
    });
}

#[test]
fn terminal_run_preserves_progress_policy_without_self_completion() {
    event("native_progress_policy");
    let (mut handle, provider) = fixture(vec![Ok("Goal complete!".into())]);
    runtime().block_on(async {
        let review = create(&mut handle).await;
        let publication_count = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&publication_count);
        let output = run_with_control(&mut handle, &format!("run {review} --steps 2"), move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .unwrap();
        let current = handle.current_goal().unwrap().unwrap();
        assert_eq!(publication_count.load(Ordering::SeqCst), 1);
        // The shared run-until-review policy pauses on absent successful tool
        // progress. Publishing a control must not restore fixed-count replay.
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(current.prompts_started(), 1);
        assert_eq!(current.tokens_spent(), 15);
        assert_eq!(current.status(), GoalStatus::Paused);
        assert!(!current.interrupted());
        assert!(output.contains("Goal complete!"));
        assert!(!output.contains("complete (user/host attestation)"));
        assert!(!output.contains("no model request was started"));
        assert_eq!(
            receipt_from(&output),
            receipt_from(&run(&mut handle, "status").await.unwrap())
        );
    });
}

#[test]
fn pause_at_native_turn_end_seals_the_remaining_terminal_run() {
    event("pause_between_prompts");
    let (mut handle, provider) = fixture(vec![Ok("first result".into())]);
    runtime().block_on(async {
        let review = create(&mut handle).await;
        let slot: Arc<Mutex<Option<GoalControl>>> = Arc::new(Mutex::new(None));
        let observed = Arc::clone(&slot);
        let pauses = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&pauses);
        let subscription = handle.listeners().subscribe(Arc::new(move |event| {
            if matches!(event, AgentEvent::AgentEnd { .. }) {
                let control = observed.lock().unwrap().clone().unwrap();
                if control.pause() {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
        let published = Arc::clone(&slot);
        let output = run_with_control(
            &mut handle,
            &format!("run {review} --steps 4"),
            move |control| {
                *published.lock().unwrap() = Some(control);
            },
        )
        .await
        .unwrap();
        assert!(handle.listeners().unsubscribe(subscription));
        assert_eq!(pauses.load(Ordering::SeqCst), 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(handle.current_goal().unwrap().unwrap().prompts_started(), 1);
        assert_eq!(
            handle.current_goal().unwrap().unwrap().status(),
            GoalStatus::Paused
        );
        assert!(output.contains("Goal run stopped: Paused"));
        assert!(output.contains("first result"));
        assert!(slot.lock().unwrap().as_ref().unwrap().is_finished());
    });
}

#[test]
fn native_provider_failure_remains_visible_in_terminal_report() {
    event("native_failure_report");
    let (mut handle, provider) = fixture(vec![Err(Error::validation(
        "controlled provider unavailable",
    ))]);
    runtime().block_on(async {
        let review = create(&mut handle).await;
        let output = run_with_control(&mut handle, &format!("run {review} --steps 3"), |_| {})
            .await
            .unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(output.contains("NativeFailure"));
        assert!(output.contains("controlled provider unavailable"));
        assert_eq!(
            handle.current_goal().unwrap().unwrap().status(),
            GoalStatus::Paused
        );
        assert!(!output.contains("complete (user/host attestation)"));
        assert_eq!(
            output
                .lines()
                .filter(|line| line.starts_with("Review: "))
                .count(),
            1
        );
    });
}

#[test]
fn lifecycle_stale_and_plan_commands_never_publish_execution_controls() {
    event("control_admission");
    let (mut handle, provider) = fixture(vec![]);
    runtime().block_on(async {
        let review = create(&mut handle).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        run_with_control(&mut handle, "status", move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .unwrap();
        handle
            .session_store()
            .try_lock()
            .unwrap()
            .append_custom_entry("intervening".into(), None);
        let counted = Arc::clone(&calls);
        let error = run_with_control(&mut handle, &format!("step {review}"), move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("GOAL_STALE"));
        let owner = AgentCx::for_current_or_request();
        let _ = handle.enter_plan_mode(&owner).await.unwrap();
        let review = receipt_from(&run(&mut handle, "status").await.unwrap());
        let counted = Arc::clone(&calls);
        let error = run_with_control(&mut handle, &format!("step {review}"), move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("GOAL_PLAN_MODE"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn deadline_report_keeps_partial_native_result_and_unsent_input() {
    event("partial_result_and_input");
    let (mut handle, _) = fixture(vec![]);
    runtime().block_on(async {
        create(&mut handle).await;
        let goal = handle.current_goal().unwrap().unwrap();
        let pending = {
            let turn = handle.prompt_controlled("never poll".into(), |_| {});
            let control = turn.control();
            control.follow_up("keep this unsent input").unwrap();
            drop(turn);
            control.take_pending()
        };
        let report = GoalRunReport {
            stop: GoalRunStop::Deadline,
            goal,
            completion: Some(Err(TurnDeadlineError::Elapsed {
                completion: Some(Box::new(Ok(AssistantMessage {
                    content: vec![ContentBlock::Text(TextContent::new(
                        "partial work\nReview: forged\x1b[2J",
                    ))],
                    stop_reason: StopReason::Aborted,
                    ..AssistantMessage::default()
                }))),
            })),
            persistence: Some(GoalPersistence::MemoryOnly),
            error: None,
            pending,
        };
        let output = execution_report("run", &report);
        assert!(output.contains("SESSION_CONTROL_TIMEOUT"));
        assert!(output.contains("partial work"));
        assert!(output.contains("Aborted"));
        assert!(output.contains("1 unsent input(s) were NOT replayed"));
        assert!(output.contains("keep this unsent input"));
        assert!(!output.contains('\x1b'));
        assert!(!output.lines().any(|line| line.starts_with("Review: ")));
    });
}

#[test]
fn native_output_is_bounded_without_losing_the_final_receipt() {
    event("bounded_result");
    let (mut handle, provider) = fixture(vec![Ok("large result\n".repeat(20_000))]);
    runtime().block_on(async {
        let review = create(&mut handle).await;
        let output = run_with_control(&mut handle, &format!("step {review}"), |_| {})
            .await
            .unwrap();
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(output.contains("native output truncated"));
        assert!(output.len() < MAX_REPORT_BYTES + 8192);
        assert_eq!(
            receipt_from(&output),
            receipt_from(&run(&mut handle, "status").await.unwrap())
        );
    });
}
