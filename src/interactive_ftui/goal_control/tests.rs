use super::*;
use crate::agent::{Agent, AgentConfig, AgentEvent, AgentSession};
use crate::compaction::ResolvedCompactionSettings;
use crate::model::{
    AssistantMessage, ContentBlock, ImageContent, MediaContent, Message, StopReason, StreamEvent,
    TextContent,
};
use crate::provider::{Context, Provider, StreamOptions};
use crate::sdk::{AgentSessionHandle, EventListeners};
use crate::session::Session;
use crate::session_control::InputKind;
use crate::tools::ToolRegistry;
use ftui::runtime::simulator::ProgramSimulator;
use ftui::{KeyCode, KeyEvent, KeyEventKind, Model, Modifiers};
use std::path::Path;
use std::pin::Pin;
use std::sync::mpsc;

#[derive(Default)]
struct Probe {
    pending: bool,
    fail: bool,
    requests: Mutex<Vec<Vec<Message>>>,
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for Probe {
    fn name(&self) -> &str {
        "ftui-goal-control"
    }
    fn api(&self) -> &str {
        "ftui-goal-control"
    }
    fn model_id(&self) -> &str {
        "test"
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
        if self.pending {
            return futures::future::pending().await;
        }
        if self.fail {
            return Err(Error::validation("fixture provider failed"));
        }
        Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Done {
            reason: StopReason::Stop,
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("review this result"))],
                stop_reason: StopReason::Stop,
                ..Default::default()
            },
        })])))
    }
}

fn handle(provider: Arc<Probe>) -> AgentSessionHandle {
    let agent = Agent::new(
        provider,
        ToolRegistry::new(&[], Path::new("."), None),
        AgentConfig::default(),
    );
    let session = AgentSession::new(
        agent,
        Arc::new(asupersync::sync::Mutex::new(Session::in_memory())),
        false,
        ResolvedCompactionSettings::default(),
    );
    AgentSessionHandle::from_session_with_listeners(session, EventListeners::default())
}

fn review(text: &str) -> &str {
    text.lines()
        .find_map(|line| line.strip_prefix("Review: "))
        .unwrap()
}

async fn create_goal(handle: &mut AgentSessionHandle) -> (String, String) {
    let status = crate::goal::terminal::run(handle, "status").await.unwrap();
    let created = crate::goal::terminal::run(
        handle,
        &format!(
            "create {} --prompts 3 -- verify input ownership",
            review(&status)
        ),
    )
    .await
    .unwrap();
    let owner = handle
        .with_session(|session| session.header.id.clone())
        .await
        .unwrap();
    (owner, format!("run {} --steps 2", review(&created)))
}

fn model(owner: &str, slot: &TurnControlSlot) -> PiFtuiModel {
    let (_tx, rx) = mpsc::channel();
    let mut model = PiFtuiModel::new(rx).with_turn_control(Arc::clone(slot));
    model.displayed_session_id = Some(owner.to_string());
    model
}

fn key(code: KeyCode, modifiers: Modifiers) -> PiFtuiMsg {
    PiFtuiMsg::Term(ftui::Event::Key(KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
    }))
}

fn payload() -> UserContent {
    UserContent::Blocks(vec![
        ContentBlock::Text(TextContent::new("expanded file, not authored ultrathink")),
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

fn same_payload(left: &UserContent, right: &UserContent) {
    assert_eq!(
        serde_json::to_value(left).unwrap(),
        serde_json::to_value(right).unwrap()
    );
}

fn detached(
    handle: &mut AgentSessionHandle,
    content: &UserContent,
    text: &str,
) -> Vec<PendingInput> {
    let turn = handle.prompt_controlled("unused".into(), |_| {});
    let control = turn.control();
    control.follow_up_with_content(content, text).unwrap();
    drop(turn);
    control.take_pending()
}

#[test]
fn escape_and_ctrl_c_pause_the_owned_goal_and_recover_native_input() {
    for (code, modifiers) in [
        (KeyCode::Escape, Modifiers::empty()),
        (KeyCode::Char('c'), Modifiers::CTRL),
    ] {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let provider = Arc::new(Probe {
                pending: true,
                ..Default::default()
            });
            let mut handle = handle(Arc::clone(&provider));
            let (owner, command) = create_goal(&mut handle).await;
            let controls: TurnControlSlot = Arc::default();
            let (events, received) = mpsc::channel();
            let (tx, _rx) = mpsc::channel();
            let mut running = Box::pin(run(&mut handle, &command, &controls, &events, &tx));
            assert!(futures::poll!(running.as_mut()).is_pending());
            let invocation = controls.lock().unwrap().goal.clone().unwrap();
            let native = invocation.accepting_control().unwrap();
            assert!(native.turn_snapshot().unwrap().accepting_input);
            let mut model = model(&owner, &controls);
            for event in received.try_iter() {
                let _ = model.update(PiFtuiMsg::Goal(event));
            }
            assert_eq!(model.state, AgentUiState::Working);
            model.input.set_text("/compact keep the goal");
            model.submit_mid_turn(false);
            assert_eq!(model.input.text(), "/compact keep the goal");
            assert!(model.queued_compaction.is_none());
            model.input.set_text("steer authored");
            model.submit_mid_turn(false);
            model.input.set_text("follow authored");
            model.submit_mid_turn(true);
            model.submit_goal_content(&invocation, payload(), "image and audio", true);
            let queued = native.turn_snapshot().unwrap();
            assert_eq!(queued.pending_steering, 1);
            assert_eq!(queued.pending_follow_up, 2);
            model.input.set_text("unfinished ordinary draft");
            let _ = model.update(key(code, modifiers));
            assert!(!native.pause(), "the key already paused this goal");
            assert_eq!(model.input.text(), "unfinished ordinary draft");
            if modifiers == Modifiers::CTRL {
                assert!(model.last_ctrl_c.is_some());
                let mut simulation = ProgramSimulator::new(model);
                simulation.init();
                simulation.inject_event(ftui::Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: Modifiers::CTRL,
                    kind: KeyEventKind::Press,
                }));
                assert!(
                    !simulation.is_running(),
                    "a deliberate second Ctrl+C still exits while the goal drains"
                );
            }
            running.await;
            assert!(native.is_finished());
            let mut recovered = Vec::new();
            for event in received.try_iter() {
                if let Event::Pending(request) = event {
                    recovered.extend(std::mem::take(&mut *request.pending.lock().unwrap()));
                }
            }
            assert_eq!(recovered.len(), 3);
            assert_eq!(recovered[0].kind, InputKind::Steering);
            assert_eq!(recovered[0].text, "steer authored");
            assert_eq!(recovered[1].kind, InputKind::FollowUp);
            assert_eq!(recovered[2].text, "image and audio");
            same_payload(&recovered[2].content, &payload());
            assert!(native.take_pending().is_empty());
            assert_eq!(provider.requests.lock().unwrap().len(), 1);
            assert!(controls.lock().unwrap().goal.is_none());
        });
    }
}

#[test]
fn goal_input_is_returned_once_on_completion_provider_error_and_abandonment() {
    for mode in ["complete", "error", "drop"] {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let provider = Arc::new(Probe {
                pending: mode == "drop",
                fail: mode == "error",
                ..Default::default()
            });
            let mut handle = handle(provider);
            let (owner, command) = create_goal(&mut handle).await;
            let controls: TurnControlSlot = Arc::default();
            let during_event = Arc::clone(&controls);
            handle.subscribe(move |event| {
                let boundary = if mode == "complete" {
                    matches!(event, AgentEvent::AgentEnd { .. })
                } else {
                    matches!(event, AgentEvent::AgentStart { .. })
                };
                if boundary {
                    let invocation = during_event.lock().unwrap().goal.clone().unwrap();
                    invocation
                        .accepting_control()
                        .unwrap()
                        .follow_up_with_content(&payload(), "original @file")
                        .unwrap();
                }
            });
            let (events, received) = mpsc::channel();
            let (tx, _rx) = mpsc::channel();
            let mut running = Box::pin(run(&mut handle, &command, &controls, &events, &tx));
            if mode == "drop" {
                assert!(futures::poll!(running.as_mut()).is_pending());
                drop(running);
            } else {
                running.await;
            }
            let requests: Vec<_> = received
                .try_iter()
                .filter_map(|event| {
                    if let Event::Pending(request) = event {
                        Some(request)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(requests.len(), 1, "{mode}");
            assert_eq!(requests[0].owner_session_id, owner);
            let pending = requests[0].pending.lock().unwrap();
            assert_eq!(pending.len(), 1, "{mode}");
            assert_eq!(pending[0].text, "original @file");
            same_payload(&pending[0].content, &payload());
            assert!(controls.lock().unwrap().goal.is_none());
        });
    }
}

#[test]
fn stale_goal_events_cannot_unlock_a_new_goal_session_or_ordinary_turn() {
    let controls: TurnControlSlot = Arc::default();
    let (sender, _receiver) = mpsc::channel();
    let old = Binding::new(&controls, &sender, "one".into(), None);
    let old_id = old.invocation.id;
    let next = Binding::new(&controls, &sender, "one".into(), None);
    let next_id = next.invocation.id;
    let mut model = model("one", &controls);
    model.state = AgentUiState::Working;
    let stale = || Event::Message {
        invocation: old_id,
        owner_session_id: "one".into(),
        message: PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Stop,
            error_message: None,
        },
    };
    let _ = model.update(PiFtuiMsg::Goal(stale()));
    assert_eq!(model.state, AgentUiState::Working);
    drop(old);
    assert_eq!(
        controls.lock().unwrap().goal.as_ref().unwrap().id,
        next.invocation.id
    );
    model.displayed_session_id = Some("two".into());
    let _ = model.update(PiFtuiMsg::Goal(Event::Message {
        invocation: next.invocation.id,
        owner_session_id: "one".into(),
        message: PiMsg::System("stale result".into()),
    }));
    assert_eq!(model.state, AgentUiState::Working);
    drop(next);
    model.displayed_session_id = Some("one".into());
    let (submit, _submitted) = mpsc::channel();
    model.submit_tx = Some(submit);
    model.send_command(UiCommand::Fresh);
    let latest_stale = || Event::Message {
        invocation: next_id,
        owner_session_id: "one".into(),
        message: PiMsg::System("late goal result".into()),
    };
    let _ = model.update(PiFtuiMsg::Goal(latest_stale()));
    assert_eq!(model.state, AgentUiState::Working);
    assert!(
        model
            .transcript
            .iter()
            .all(|entry| !entry.text.contains("late goal result"))
    );
    controls.lock().unwrap().last_goal = Some(next_id);
    let mut session = handle(Arc::default());
    let turn = session.prompt_controlled("ordinary".into(), |_| {});
    controls.lock().unwrap().publish(turn.control());
    let _ = model.update(PiFtuiMsg::Goal(latest_stale()));
    assert_eq!(model.state, AgentUiState::Working);
    assert!(
        model
            .transcript
            .iter()
            .all(|entry| !entry.text.contains("stale result"))
    );
}

#[test]
fn recovered_media_waits_for_the_ordinary_editor_and_resubmits_without_text() {
    let controls: TurnControlSlot = Arc::default();
    let mut session = handle(Arc::default());
    let image = UserContent::Blocks(vec![ContentBlock::Image(ImageContent {
        data: "aGVsbG8=".into(),
        mime_type: "image/png".into(),
    })]);
    let pending = detached(&mut session, &image, "");
    let request = RestoredInput::new(uuid::Uuid::new_v4(), "one".into(), None, pending);
    let mut model = model("one", &controls);
    let (submit, received) = mpsc::channel();
    model.submit_tx = Some(submit);
    model.login_pending = Some(("private provider".into(), false));
    model.input.set_text("private code");
    let _ = model.update(PiFtuiMsg::Goal(Event::Pending(request.clone())));
    assert_eq!(model.input.text(), "private code");
    assert!(model.goal_draft.is_none());
    model.login_pending = None;
    model.input.set_text("");
    let _ = model.handle_agent(PiMsg::ExtensionUiRequest(
        crate::extensions::ExtensionUiRequest::new(
            "extension",
            "input",
            serde_json::json!({"title":"Extension answer"}),
        ),
    ));
    model.apply_goal_input_restores();
    assert!(model.goal_draft.is_none());
    model.cancel_active_ext();
    model.input.set_text("");
    model.apply_goal_input_restores();
    assert!(model.goal_draft.is_some());
    assert!(model.input.is_empty());
    model.submit_input();
    let UiCommand::RestoredGoalInput(submitted) = received.try_recv().unwrap() else {
        panic!("media-only input must use the native prepared path");
    };
    assert_eq!(submitted.id, request.id);
    let pending = submitted.pending.lock().unwrap();
    assert_eq!(pending[0].text, "");
    same_payload(&pending[0].content, &image);
}

#[test]
fn restored_input_cannot_cross_into_a_different_goal_or_session() {
    let controls: TurnControlSlot = Arc::default();
    let (sender, _receiver) = mpsc::channel();
    let mut session = handle(Arc::default());
    let request = RestoredInput::new(
        uuid::Uuid::new_v4(),
        "one".into(),
        Some("old branch".into()),
        detached(&mut session, &payload(), "old source"),
    );
    let newer = Binding::new(
        &controls,
        &sender,
        "one".into(),
        Some("other branch".into()),
    );
    let mut model = model("one", &controls);
    model.show_goal_draft(request.clone());
    model.state = AgentUiState::Working;
    assert!(model.submit_goal_draft());
    assert_eq!(request.pending.lock().unwrap().len(), 1);
    assert!(model.goal_draft.is_some());
    model.park_goal_draft_for("two");
    model.displayed_session_id = Some("two".into());
    model.state = AgentUiState::Ready;
    model.apply_goal_input_restores();
    assert!(model.goal_draft.is_none());
    assert!(model.input.is_empty());
    assert_eq!(model.pending_goal_inputs.len(), 1);
    drop(newer);
}

#[test]
fn ordinary_returned_text_waits_behind_a_recovered_native_goal_draft() {
    let controls: TurnControlSlot = Arc::default();
    let mut session = handle(Arc::default());
    let request = RestoredInput::new(
        uuid::Uuid::new_v4(),
        "one".into(),
        None,
        detached(&mut session, &payload(), "goal source"),
    );
    let mut model = model("one", &controls);
    let (submit, _received) = mpsc::channel();
    model.submit_tx = Some(submit);
    let _ = model.update(PiFtuiMsg::Goal(Event::Pending(request)));
    let _ = model.update(PiFtuiMsg::Agent(PiMsg::RestorePendingInput {
        owner_session_id: "one".into(),
        text: "ordinary returned text".into(),
    }));
    assert_eq!(model.input.text(), "goal source");
    assert_eq!(model.pending_input_restores.len(), 1);
    assert!(model.recall_goal_input());
    assert_eq!(model.input.text(), "goal source");
    model.submit_input();
    model.apply_pending_input_restores();
    assert_eq!(model.input.text(), "ordinary returned text");
    assert!(model.pending_input_restores.is_empty());
}

#[test]
fn dropping_an_explicit_native_resubmission_recovers_only_its_unclaimed_tail() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let provider = Arc::new(Probe {
            pending: true,
            ..Default::default()
        });
        let mut session = handle(Arc::clone(&provider));
        let owner = session
            .with_session(|session| session.header.id.clone())
            .await
            .unwrap();
        let mut pending = detached(
            &mut session,
            &UserContent::Text("first".into()),
            "first source",
        );
        pending.extend(detached(&mut session, &payload(), "tail source"));
        let request = RestoredInput::new(uuid::Uuid::new_v4(), owner, None, pending);
        let controls: TurnControlSlot = Arc::default();
        let (events, received) = mpsc::channel();
        let (tx, _rx) = mpsc::channel();
        let old_command = request.clone();
        let mut replay = Box::pin(run_restored(&mut session, request, &tx, &events, &controls));
        assert!(futures::poll!(replay.as_mut()).is_pending());
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        drop(replay);
        let Event::Pending(returned) = received.try_recv().unwrap() else {
            panic!("recover tail");
        };
        let pending = returned.pending.lock().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].text, "tail source");
        same_payload(&pending[0].content, &payload());
        assert_ne!(returned.id, old_command.id);
        assert!(old_command.pending.lock().unwrap().is_empty());
        assert!(received.try_recv().is_err());
    });
}

#[test]
fn pending_return_precedes_a_failed_post_run_receipt_read() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut handle = handle(Arc::default());
        let (_, command) = create_goal(&mut handle).await;
        let current: Arc<Mutex<Option<GoalControl>>> = Arc::default();
        let on_end = Arc::clone(&current);
        handle.subscribe(move |event| {
            if matches!(event, AgentEvent::AgentEnd { .. }) {
                on_end
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .follow_up_with_content(&payload(), "kept after receipt failure")
                    .unwrap();
            }
        });
        let store = handle.session_store();
        let mut held = None;
        let mut returned = Vec::new();
        let result = crate::goal::terminal::run_with_control_and_pending(
            &mut handle,
            &command,
            move |control| *current.lock().unwrap() = Some(control),
            |pending| {
                returned = pending;
                // Native execution and journaling have settled. Only the
                // terminal's final receipt read now loses the store race.
                held = Some(store.try_lock_owned().unwrap());
            },
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("GOAL_BUSY"));
        assert_eq!(returned.len(), 1);
        assert_eq!(returned[0].text, "kept after receipt failure");
        same_payload(&returned[0].content, &payload());
        drop(held);
    });
}

#[test]
fn shutdown_before_goal_publication_dispatches_no_provider_work() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let provider = Arc::new(Probe::default());
        let mut handle = handle(Arc::clone(&provider));
        let (_, command) = create_goal(&mut handle).await;
        let controls: TurnControlSlot = Arc::default();
        super::super::abort_driver_work(None, Some(&controls));
        let (events, _received) = mpsc::channel();
        let (tx, _rx) = mpsc::channel();
        run(&mut handle, &command, &controls, &events, &tx).await;
        assert!(provider.requests.lock().unwrap().is_empty());
        assert_eq!(handle.current_goal().unwrap().unwrap().prompts_started(), 0);
        assert!(controls.lock().unwrap().goal.is_none());
    });
}
