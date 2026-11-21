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
fn editable_fork_drafts_deliver_exact_media_and_current_text_to_the_native_turn() {
    for edited in [
        crate::session::fork_tests::MULTIMODAL_TEXT,
        "Revise @missing-original.wav",
        "/web unpair",
        "",
    ] {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let provider = Arc::new(Probe::default());
            let mut session = handle(Arc::clone(&provider));
            let owner = session
                .with_session(|session| session.header.id.clone())
                .await
                .unwrap();
            let controls: TurnControlSlot = Arc::default();
            let mut model = model(&owner, &controls);
            let (submit, received) = mpsc::channel();
            model.submit_tx = Some(submit);
            let original = crate::session::fork_tests::multimodal_content();
            let _ = model.handle_agent(PiMsg::RestoreForkInput {
                owner_session_id: owner,
                branch_anchor: None,
                text: crate::session::fork_tests::MULTIMODAL_TEXT.to_string(),
                content: original.clone(),
            });
            assert!(model.goal_draft.is_some());
            model.input.set_text(edited);
            model.submit_input();
            let UiCommand::RestoredGoalInput(request) = received.try_recv().unwrap() else {
                panic!("fork must use native prepared input");
            };
            let expected = UserContent::Blocks(crate::session::edit_fork_content(
                &original,
                crate::session::fork_tests::MULTIMODAL_TEXT,
                edited,
            ));
            {
                let pending = request.pending.lock().unwrap();
                assert_eq!(
                    pending[0].text, edited,
                    "authored source must match the edit"
                );
                same_payload(&pending[0].content, &expected);
            }
            let (events, returned) = mpsc::channel();
            let (tx, _rx) = mpsc::channel();
            run_restored(
                &mut session,
                request,
                &tx,
                &events,
                &controls,
                Arc::default(),
            )
            .await;
            assert!(returned.try_recv().is_err());
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let user = requests[0]
                .iter()
                .find_map(|message| match message {
                    Message::User(user) => Some(&user.content),
                    _ => None,
                })
                .expect("provider received native fork input");
            same_payload(user, &expected);
        });
    }
}

#[test]
fn cancelled_fork_turn_recovers_text_follow_up_without_fork_edit_semantics() {
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
        let request = RestoredInput::for_fork(
            owner.clone(),
            None,
            crate::session::fork_tests::MULTIMODAL_TEXT.into(),
            crate::session::fork_tests::multimodal_content(),
        )
        .unwrap();
        let controls: TurnControlSlot = Arc::default();
        let (events, returned) = mpsc::channel();
        let (tx, _rx) = mpsc::channel();
        let mut replay = Box::pin(run_restored(
            &mut session,
            request,
            &tx,
            &events,
            &controls,
            Arc::default(),
        ));
        assert!(futures::poll!(replay.as_mut()).is_pending());
        let control = controls.lock().unwrap().current.clone().unwrap();
        assert!(control.initial_input_handed_off());
        control.follow_up("ordinary text follow-up").unwrap();
        drop(replay);
        let Event::Pending(returned) = returned.try_recv().unwrap() else {
            panic!("unclaimed follow-up remains recoverable");
        };
        assert!(!returned.editable_fork);
        let mut model = model(&owner, &controls);
        let (submit, received) = mpsc::channel();
        model.submit_tx = Some(submit);
        model.show_goal_draft(returned);
        model.submit_input();
        let UiCommand::RestoredGoalInput(request) = received.try_recv().unwrap() else {
            panic!("ordinary recovered follow-up can still be submitted");
        };
        let pending = request.pending.lock().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].text, "ordinary text follow-up");
        same_payload(
            &pending[0].content,
            &UserContent::Text("ordinary text follow-up".into()),
        );
    });
}

#[test]
fn late_fork_input_is_parked_until_its_own_session_returns() {
    let controls: TurnControlSlot = Arc::default();
    let mut model = model("replacement", &controls);
    model.input.set_text("newer draft");
    let _ = model.handle_agent(PiMsg::RestoreForkInput {
        owner_session_id: "original".into(),
        branch_anchor: None,
        text: crate::session::fork_tests::MULTIMODAL_TEXT.into(),
        content: crate::session::fork_tests::multimodal_content(),
    });
    assert_eq!(model.input.text(), "newer draft");
    assert_eq!(model.pending_goal_inputs.len(), 1);
    model.displayed_session_id = Some("original".into());
    model.input.set_text("");
    model.apply_goal_input_restores();
    assert_eq!(
        model.input.text(),
        crate::session::fork_tests::MULTIMODAL_TEXT
    );
    assert!(model.goal_draft.as_ref().unwrap().request.editable_fork);
}

#[test]
fn fork_drafts_preserve_edits_when_another_editor_or_failed_submission_takes_over() {
    let controls: TurnControlSlot = Arc::default();
    let mut model = model("one", &controls);
    let _ = model.handle_agent(PiMsg::RestoreForkInput {
        owner_session_id: "one".into(),
        branch_anchor: None,
        text: crate::session::fork_tests::MULTIMODAL_TEXT.into(),
        content: crate::session::fork_tests::multimodal_content(),
    });
    model.input.set_text("edited fork text");
    // No driver: failure must retain the editable native draft.
    assert!(model.submit_goal_draft());
    assert!(model.goal_draft.is_some());
    assert_eq!(model.input.text(), "edited fork text");
    model.set_prompt_editor_text("newer draft");
    assert!(model.goal_draft.is_none());
    assert_eq!(model.input.text(), "newer draft");
    assert_eq!(model.pending_goal_inputs.len(), 1);
    model.input.set_text("");
    model.apply_goal_input_restores();
    assert_eq!(model.input.text(), "edited fork text");
    model.start_login_input("private provider".into(), false);
    model.input.set_text("private credential");
    model.apply_goal_input_restores();
    assert_eq!(model.input.text(), "private credential");
    assert!(model.goal_draft.is_none());
    model.finish_login_input(false);
    model.apply_goal_input_restores();
    assert_eq!(model.input.text(), "edited fork text");
    let pending = model
        .goal_draft
        .as_ref()
        .unwrap()
        .request
        .pending
        .lock()
        .unwrap();
    assert_eq!(pending[0].text, "edited fork text");
    assert!(
        !serde_json::to_string(&pending[0].content)
            .unwrap()
            .contains("private credential")
    );
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
fn rejected_recovered_input_keeps_its_first_payload_and_fifo_tail() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let provider = Arc::new(Probe::default());
        let mut session = handle(Arc::clone(&provider));
        let owner = session
            .with_session(|session| session.header.id.clone())
            .await
            .unwrap();
        let mut pending = detached(&mut session, &payload(), "first original @file");
        pending.extend(detached(
            &mut session,
            &UserContent::Text("second payload".into()),
            "second source",
        ));
        pending.extend(detached(
            &mut session,
            &UserContent::Text("third payload".into()),
            "third source",
        ));
        let request = RestoredInput::new(uuid::Uuid::new_v4(), owner, None, pending);
        let old_command = request.clone();
        session
            .session()
            .provider_admission_gate()
            .block("fixture requires durable session recovery".to_string());
        let controls: TurnControlSlot = Arc::default();
        let (events, received) = mpsc::channel();
        let (tx, _rx) = mpsc::channel();
        run_restored(
            &mut session,
            request,
            &tx,
            &events,
            &controls,
            Arc::default(),
        )
        .await;

        assert!(provider.requests.lock().unwrap().is_empty());
        assert!(session.session().agent.messages().is_empty());
        let Event::Pending(returned) = received.try_recv().unwrap() else {
            panic!("recover all unclaimed payloads");
        };
        {
            let pending = returned.pending.lock().unwrap();
            assert_eq!(
                pending
                    .iter()
                    .map(|input| input.text.as_str())
                    .collect::<Vec<_>>(),
                ["first original @file", "second source", "third source"]
            );
            same_payload(&pending[0].content, &payload());
            same_payload(
                &pending[1].content,
                &UserContent::Text("second payload".into()),
            );
            same_payload(
                &pending[2].content,
                &UserContent::Text("third payload".into()),
            );
        }
        assert_ne!(returned.id, old_command.id);
        assert!(old_command.pending.lock().unwrap().is_empty());
        assert!(received.try_recv().is_err());

        // Recovery has a new one-use identity. A retained UI command cannot
        // reacquire or duplicate any part of the returned input batch.
        run_restored(
            &mut session,
            old_command,
            &tx,
            &events,
            &controls,
            Arc::default(),
        )
        .await;
        assert!(received.try_recv().is_err());
        assert!(provider.requests.lock().unwrap().is_empty());
    });
}

#[test]
fn interrupted_restore_startup_preserves_first_input_before_native_handoff() {
    for outcome in ["drop", "reject", "cancel"] {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let provider = Arc::new(Probe::default());
            let mut session =
                handle(Arc::clone(&provider)).with_failover(Some(crate::sdk::FailoverOptions {
                    chains: std::collections::HashMap::new(),
                    available_models: Vec::new(),
                    auth: crate::auth::AuthStorage::empty_at("unused-auth.json".into()),
                    cli_api_key: None,
                    cooldown_secs: 60,
                }));
            let owner = session
                .with_session(|session| session.header.id.clone())
                .await
                .unwrap();
            let mut pending = detached(&mut session, &payload(), "first original @file");
            pending.extend(detached(
                &mut session,
                &UserContent::Text("tail payload".into()),
                "tail source",
            ));
            let request = RestoredInput::new(uuid::Uuid::new_v4(), owner, None, pending);
            let old_command = request.clone();
            let controls: TurnControlSlot = Arc::default();
            let (events, received) = mpsc::channel();
            let (tx, _rx) = mpsc::channel();

            // The SDK has accepted control, but primary restoration cannot
            // finish while another provider transition holds admission.
            let admission = session.session().provider_admission_gate();
            let cx = crate::agent_cx::AgentCx::for_request();
            let authority = admission
                .acquire_transition_authority(cx.cx())
                .await
                .unwrap();
            let mut replay = Box::pin(run_restored(
                &mut session,
                request,
                &tx,
                &events,
                &controls,
                Arc::default(),
            ));
            assert!(futures::poll!(replay.as_mut()).is_pending());
            let control = controls.lock().unwrap().current.clone().unwrap();
            assert!(!control.initial_input_handed_off());
            assert!(old_command.pending.lock().unwrap().is_empty());
            assert!(provider.requests.lock().unwrap().is_empty());

            if outcome == "drop" {
                drop(replay);
                drop(authority);
            } else {
                if outcome == "reject" {
                    admission.block("another transition could not establish durability".into());
                } else {
                    control.abort();
                }
                drop(authority);
                replay.await;
            }

            assert!(!control.initial_input_handed_off());
            assert!(provider.requests.lock().unwrap().is_empty());
            assert!(session.session().agent.messages().is_empty());
            let Event::Pending(returned) = received.try_recv().unwrap() else {
                panic!("return input whose native handoff never occurred");
            };
            let pending = returned.pending.lock().unwrap();
            assert_eq!(pending.len(), 2);
            assert_eq!(pending[0].text, "first original @file");
            same_payload(&pending[0].content, &payload());
            assert_eq!(pending[1].text, "tail source");
            same_payload(
                &pending[1].content,
                &UserContent::Text("tail payload".into()),
            );
            assert_ne!(returned.id, old_command.id);
            assert!(old_command.pending.lock().unwrap().is_empty());
            assert!(received.try_recv().is_err());
        });
    }
}

#[test]
fn input_hook_refusal_does_not_replay_first_input_even_without_provider_events() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let entry = temp.path().join("consume-input.mjs");
        std::fs::write(
            &entry,
            r#"export default function (pi) {
                let handled = 0;
                pi.on("input", () => {
                    handled += 1;
                    return { action: "handled", reason: "input already handled by extension" };
                });
                pi.registerCommand("handled-count", { handler: async () => handled });
            }"#,
        )
        .unwrap();
        let provider = Arc::new(Probe::default());
        let mut session = handle(Arc::clone(&provider));
        session
            .session_mut()
            .enable_extensions(&[], temp.path(), None, &[entry])
            .await
            .unwrap();
        let manager = session.extension_manager().unwrap().clone();
        let owner = session
            .with_session(|session| session.header.id.clone())
            .await
            .unwrap();
        let mut pending = detached(
            &mut session,
            &UserContent::Text("already consumed by hook".into()),
            "first source",
        );
        pending.extend(detached(&mut session, &payload(), "unclaimed tail"));
        let request = RestoredInput::new(uuid::Uuid::new_v4(), owner, None, pending);
        let controls: TurnControlSlot = Arc::default();
        let (events, received) = mpsc::channel();
        let (tx, _rx) = mpsc::channel();
        run_restored(
            &mut session,
            request,
            &tx,
            &events,
            &controls,
            Arc::default(),
        )
        .await;

        assert_eq!(
            manager
                .execute_command("handled-count", "", 5000)
                .await
                .unwrap(),
            serde_json::json!(1)
        );
        assert!(provider.requests.lock().unwrap().is_empty());
        assert!(session.session().agent.messages().is_empty());
        let Event::Pending(returned) = received.try_recv().unwrap() else {
            panic!("return only input the hook never consumed");
        };
        {
            let pending = returned.pending.lock().unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].text, "unclaimed tail");
            same_payload(&pending[0].content, &payload());
        }
        assert!(received.try_recv().is_err());
        assert!(manager.shutdown(std::time::Duration::from_secs(5)).await);
    });
}

#[test]
fn native_failure_after_handoff_does_not_restore_the_dispatched_first_input() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let provider = Arc::new(Probe {
            fail: true,
            ..Default::default()
        });
        let mut session = handle(Arc::clone(&provider));
        let owner = session
            .with_session(|session| session.header.id.clone())
            .await
            .unwrap();
        let mut pending = detached(
            &mut session,
            &UserContent::Text("deliver first once".into()),
            "first source",
        );
        pending.extend(detached(&mut session, &payload(), "unclaimed tail"));
        let request = RestoredInput::new(uuid::Uuid::new_v4(), owner, None, pending);
        let controls: TurnControlSlot = Arc::default();
        let (events, received) = mpsc::channel();
        let (tx, _rx) = mpsc::channel();
        run_restored(
            &mut session,
            request,
            &tx,
            &events,
            &controls,
            Arc::default(),
        )
        .await;

        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        let Event::Pending(returned) = received.try_recv().unwrap() else {
            panic!("recover only the unclaimed tail");
        };
        let pending = returned.pending.lock().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].text, "unclaimed tail");
        same_payload(&pending[0].content, &payload());
        assert!(received.try_recv().is_err());
    });
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
        let mut replay = Box::pin(run_restored(
            &mut session,
            request,
            &tx,
            &events,
            &controls,
            Arc::default(),
        ));
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

#[test]
fn ordinary_recovery_keeps_native_boundaries_and_never_rereads_file_sources() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let provider = Arc::new(Probe::default());
        let mut session = handle(Arc::clone(&provider));
        let owner = session
            .with_session(|session| session.header.id.clone())
            .await
            .unwrap();
        let turn = session.prompt_controlled("unused".into(), |_| {});
        let control = turn.control();
        control
            .steer_with_content(&payload(), "review @no-longer-present.png")
            .unwrap();
        control
            .follow_up_with_content(
                &UserContent::Text("captured second file body".into()),
                "then @no-longer-present.txt",
            )
            .unwrap();
        drop(turn);
        let request = RestoredInput::for_turn(owner.clone(), None, control.take_pending());
        {
            let pending = request.pending.lock().unwrap();
            assert_eq!(pending[0].kind, InputKind::Steering);
            assert_eq!(pending[1].kind, InputKind::FollowUp);
        }
        let controls: TurnControlSlot = Arc::default();
        let (submit, submitted) = mpsc::channel();
        let mut model = model(&owner, &controls).with_submit_channel(submit);
        model.input.set_text("newer draft");
        model.restore_native_input(request.clone());
        assert_eq!(model.input.text(), "newer draft");
        assert!(model.goal_draft.is_none());
        assert_eq!(model.pending_goal_inputs.len(), 1);
        model.input.set_text("");
        model.apply_goal_input_restores();
        let original = model.input.text();
        model.input.set_text("edited recovery");
        model.submit_input();
        assert!(
            submitted.try_recv().is_err(),
            "edits cannot rebuild captured files"
        );
        assert!(model.recall_goal_input());
        assert_eq!(model.input.text(), original);
        model.submit_input();
        let UiCommand::RestoredGoalInput(submitted) = submitted.try_recv().unwrap() else {
            panic!("native recovery command");
        };
        let (events, returned) = mpsc::channel();
        let (tx, received) = mpsc::channel();
        run_restored(
            &mut session,
            submitted,
            &tx,
            &events,
            &controls,
            Arc::default(),
        )
        .await;
        assert!(returned.try_recv().is_err());
        {
            let requests = provider.requests.lock().unwrap();
            assert_eq!(
                requests.len(),
                2,
                "two native messages, not one joined draft"
            );
            let users = requests
                .last()
                .unwrap()
                .iter()
                .filter_map(|message| match message {
                    Message::User(user) => Some(&user.content),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(users.len(), 2);
            same_payload(users[0], &payload());
            same_payload(
                users[1],
                &UserContent::Text("captured second file body".into()),
            );
        }
        let lifecycle = received.try_iter().collect::<Vec<_>>();
        assert!(matches!(lifecycle.last(),
            Some(PiMsg::PreparedInputFinished { request_id }) if *request_id == request.id));
        for event in lifecycle {
            let _ = model.update(PiFtuiMsg::Agent(event));
        }
        assert!(model.replaying_input.is_none());
        assert!(model.busy.is_none());
        assert!(controls.lock().unwrap().current_origin.is_none());
        run_restored(
            &mut session,
            request,
            &tx,
            &events,
            &controls,
            Arc::default(),
        )
        .await;
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
        assert!(
            returned.try_recv().is_err(),
            "a stale command cannot acquire another batch"
        );
    });
}

#[test]
fn compaction_recovery_requires_separate_review_of_before_command_and_after() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let provider = Arc::new(Probe::default());
        let mut session = handle(Arc::clone(&provider));
        let owner = session
            .with_session(|session| session.header.id.clone())
            .await
            .unwrap();
        let before = detached(
            &mut session,
            &UserContent::Text("captured before".into()),
            "before @old.txt",
        );
        let after = detached(&mut session, &payload(), "after @old.png");
        let request =
            RestoredInput::for_compaction(owner, None, before, "/shake".into(), None, true, after);
        let stale = request.clone();
        let controls: TurnControlSlot = Arc::default();
        let (tx, _rx) = mpsc::channel();
        let (events, received) = mpsc::channel();
        run_restored(
            &mut session,
            request,
            &tx,
            &events,
            &controls,
            Arc::default(),
        )
        .await;
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        let Event::Pending(command) = received.try_recv().unwrap() else {
            panic!("maintenance must wait for explicit review");
        };
        assert_eq!(command.text(), "/shake");
        assert!(command.pending.lock().unwrap().is_empty());
        assert_eq!(command.following.lock().unwrap().len(), 1);
        assert!(received.try_recv().is_err());
        run_restored(&mut session, stale, &tx, &events, &controls, Arc::default()).await;
        assert!(
            received.try_recv().is_err(),
            "old before-step cannot replay maintenance"
        );
        run_restored(
            &mut session,
            command,
            &tx,
            &events,
            &controls,
            Arc::default(),
        )
        .await;
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            1,
            "shake is not a model prompt"
        );
        let Event::Pending(after) = received.try_recv().unwrap() else {
            panic!("after-input requires its own explicit review");
        };
        assert_eq!(after.text(), "after @old.png");
        same_payload(&after.pending.lock().unwrap()[0].content, &payload());
        assert!(received.try_recv().is_err());
        run_restored(&mut session, after, &tx, &events, &controls, Arc::default()).await;
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
        assert!(received.try_recv().is_err());
        assert!(provider.requests.lock().unwrap().iter().all(|messages| {
            messages.iter().all(|message| {
                !matches!(message,
                Message::User(user) if matches!(&user.content,
                    UserContent::Text(text) if text == "/shake"))
            })
        }));
    });
}

#[test]
fn native_and_maintenance_recovery_keep_escape_before_dequeue_and_during_admission() {
    for maintenance in [false, true] {
        for phase in ["before_dequeue", "session_admission"] {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let provider = Arc::new(Probe::default());
                let mut session = handle(Arc::clone(&provider));
                let owner = session
                    .with_session(|session| session.header.id.clone())
                    .await
                    .unwrap();
                let pending = detached(&mut session, &payload(), "captured @missing.png");
                let request = if maintenance {
                    RestoredInput::for_compaction(
                        owner.clone(),
                        None,
                        Vec::new(),
                        "/compact retain decisions".into(),
                        Some("retain decisions".into()),
                        false,
                        pending,
                    )
                } else {
                    RestoredInput::for_turn(owner.clone(), None, pending)
                };
                let stale = request.clone();
                let controls: TurnControlSlot = Arc::default();
                let aborts: TurnAbortSlot = Arc::default();
                let (submit, submitted) = mpsc::channel();
                let mut model = model(&owner, &controls)
                    .with_turn_abort(Arc::clone(&aborts))
                    .with_submit_channel(submit);
                model.show_goal_draft(request);
                model.submit_input();
                let UiCommand::RestoredGoalInput(request) = submitted.try_recv().unwrap() else {
                    panic!("native recovery command");
                };
                let id = request.id;
                let signal = request.abort_handle().signal();
                model.input.set_text("newer unsent draft");
                model.submit_input();
                assert!(
                    submitted.try_recv().is_err(),
                    "a queued recovery keeps UI ownership"
                );
                let _ = model.update(PiFtuiMsg::Agent(PiMsg::PreparedInputFinished {
                    request_id: uuid::Uuid::new_v4(),
                }));
                assert!(
                    model.replaying_input.is_some(),
                    "stale completion has no authority"
                );

                let store = session.session_store();
                let held = (phase == "session_admission").then(|| store.try_lock().unwrap());
                let (tx, lifecycle) = mpsc::channel();
                let (events, received) = mpsc::channel();
                let mut replay = Box::pin(run_restored(
                    &mut session,
                    request,
                    &tx,
                    &events,
                    &controls,
                    Arc::clone(&aborts),
                ));
                if phase == "session_admission" {
                    assert!(futures::poll!(replay.as_mut()).is_pending());
                    assert!(aborts.lock().unwrap().is_some());
                }
                let _ = model.update(key(KeyCode::Escape, Modifiers::empty()));
                assert!(signal.is_aborted());
                replay.await;
                drop(held);
                assert!(provider.requests.lock().unwrap().is_empty());
                assert!(aborts.lock().unwrap().is_none());
                let Event::Pending(returned) = received.try_recv().unwrap() else {
                    panic!("retain cancelled native content");
                };
                assert_ne!(returned.id, id);
                assert!(!returned.abort_handle().signal().is_aborted());
                if maintenance {
                    assert_eq!(returned.text(), "/compact retain decisions");
                    let following = returned.following.lock().unwrap();
                    assert_eq!(following.len(), 1);
                    same_payload(&following[0].pending.lock().unwrap()[0].content, &payload());
                } else {
                    same_payload(&returned.pending.lock().unwrap()[0].content, &payload());
                }
                let _ = model.update(PiFtuiMsg::Goal(Event::Pending(returned)));
                assert!(model.goal_draft.is_none(), "early recovery stays parked");
                let lifecycle = lifecycle.try_iter().collect::<Vec<_>>();
                assert!(matches!(lifecycle.last(),
                    Some(PiMsg::PreparedInputFinished { request_id }) if *request_id == id));
                for message in lifecycle {
                    let _ = model.update(PiFtuiMsg::Agent(message));
                }
                assert!(model.replaying_input.is_none());
                assert!(model.busy.is_none());
                assert_eq!(model.input.text(), "newer unsent draft");
                model.input.set_text("");
                model.apply_goal_input_restores();
                assert!(model.goal_draft.is_some());
                run_restored(&mut session, stale, &tx, &events, &controls, Arc::default()).await;
                assert!(received.try_recv().is_err());
                assert!(provider.requests.lock().unwrap().is_empty());
            });
        }
    }
}

#[test]
fn replay_keeps_its_maintenance_chain_when_nested_recall_and_compaction_are_refused() {
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
        let mut before = detached(
            &mut session,
            &UserContent::Text("dispatch first".into()),
            "first source",
        );
        before.extend(detached(&mut session, &payload(), "second source"));
        let after = detached(
            &mut session,
            &UserContent::Text("after maintenance".into()),
            "last source",
        );
        let request = RestoredInput::for_compaction(
            owner.clone(),
            None,
            before,
            "/shake".into(),
            None,
            true,
            after,
        );
        let controls: TurnControlSlot = Arc::default();
        let aborts: TurnAbortSlot = Arc::default();
        let (submit, submitted) = mpsc::channel();
        let mut model = model(&owner, &controls)
            .with_turn_abort(Arc::clone(&aborts))
            .with_submit_channel(submit);
        model.show_goal_draft(request);
        model.submit_input();
        let UiCommand::RestoredGoalInput(request) = submitted.try_recv().unwrap() else {
            panic!("native recovery command");
        };
        let (tx, lifecycle) = mpsc::channel();
        let (events, received) = mpsc::channel();
        let mut replay = Box::pin(run_restored(
            &mut session,
            request,
            &tx,
            &events,
            &controls,
            Arc::clone(&aborts),
        ));
        assert!(futures::poll!(replay.as_mut()).is_pending());
        for message in lifecycle.try_iter() {
            let _ = model.update(PiFtuiMsg::Agent(message));
        }
        let control = controls.lock().unwrap().current.clone().unwrap();
        let snapshot = control.snapshot();
        assert_eq!(snapshot.pending_follow_up, 1);
        model.input.set_text("/compact nested");
        model.submit_mid_turn(false);
        assert_eq!(model.input.text(), "/compact nested");
        assert!(!control.has_yield_barrier());
        assert!(submitted.try_recv().is_err());
        model.restore_queued_input();
        assert_eq!(model.input.text(), "/compact nested");
        assert_eq!(control.snapshot(), snapshot);
        assert!(model.pending_goal_inputs.is_empty());
        let _ = model.update(key(KeyCode::Escape, Modifiers::empty()));
        replay.await;
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        let Event::Pending(returned) = received.try_recv().unwrap() else {
            panic!("return native tail before maintenance");
        };
        assert_eq!(returned.text(), "second source");
        same_payload(&returned.pending.lock().unwrap()[0].content, &payload());
        let following = returned.following.lock().unwrap();
        assert_eq!(following.len(), 2);
        assert_eq!(following[0].text(), "/shake");
        assert_eq!(following[1].text(), "last source");
        assert!(received.try_recv().is_err());
    });
}

#[test]
fn stopped_native_recovery_does_not_need_a_second_session_store_read() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut session = handle(Arc::default());
        let origin = session
            .with_session(|session| {
                (
                    session.header.id.clone(),
                    session.leaf_id().map(str::to_string),
                )
            })
            .await
            .unwrap();
        let pending = detached(&mut session, &payload(), "original source");
        let result = super::super::ControlledTurnResult {
            origin: Some(origin.clone()),
            pending,
            initial_restore: None,
            stopped: true,
        };
        let store = session.session_store();
        let held = store.try_lock().unwrap();
        let controls: TurnControlSlot = Arc::default();
        let (tx, received) = mpsc::channel();
        let mut finish = Box::pin(super::super::finish_prompt_sequence(
            &mut session,
            result,
            &tx,
            &controls,
            None,
        ));
        assert!(futures::poll!(finish.as_mut()).is_ready());
        drop(finish);
        drop(held);
        let PiMsg::RestorePreparedInput(request) = received.try_recv().unwrap() else {
            panic!("native payload survives unavailable final receipt");
        };
        assert_eq!(request.owner_session_id, origin.0);
        assert_eq!(request.branch_anchor, origin.1);
        assert_eq!(request.text(), "original source");
        same_payload(&request.pending.lock().unwrap()[0].content, &payload());
    });
}

#[test]
fn dropping_compaction_display_refresh_never_replays_an_installed_command() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let provider = Arc::new(Probe::default());
        let mut stored = Session::in_memory();
        for index in 0..3 {
            stored.append_message(crate::session::SessionMessage::User {
                content: UserContent::Text(format!("question {index} ").repeat(100)),
                timestamp: Some(index),
            });
            stored.append_message(crate::session::SessionMessage::Assistant {
                message: AssistantMessage {
                    content: vec![ContentBlock::Text(TextContent::new(format!(
                        "answer {index}"
                    )))],
                    stop_reason: StopReason::Stop,
                    ..Default::default()
                },
            });
        }
        let mut agent = Agent::new(
            provider.clone(),
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig::default(),
        );
        agent.replace_messages(stored.to_messages_for_current_path());
        let mut session = AgentSessionHandle::from_session_with_listeners(
            AgentSession::new(
                agent,
                Arc::new(asupersync::sync::Mutex::new(stored)),
                false,
                ResolvedCompactionSettings {
                    enabled: false,
                    keep_recent_tokens: 1,
                    ..Default::default()
                },
            ),
            EventListeners::default(),
        );
        let owner = session
            .with_session(|session| session.header.id.clone())
            .await
            .unwrap();
        let after = detached(&mut session, &payload(), "after committed compaction");
        let request = RestoredInput::for_compaction(
            owner,
            None,
            Vec::new(),
            "/compact keep decisions".into(),
            Some("keep decisions".into()),
            false,
            after,
        );
        let stale = request.clone();
        let store = session.session_store();
        let held = Arc::new(Mutex::new(None));
        let held_on_end = Arc::clone(&held);
        let store_on_end = Arc::clone(&store);
        session.subscribe(move |event| {
            if matches!(
                event,
                AgentEvent::ManualCompactionEnd {
                    result: Some(_),
                    aborted: false,
                    ..
                }
            ) {
                *held_on_end.lock().unwrap() =
                    Some(Arc::clone(&store_on_end).try_lock_owned().unwrap());
            }
        });
        let controls: TurnControlSlot = Arc::default();
        let aborts: TurnAbortSlot = Arc::default();
        let (tx, _rx) = mpsc::channel();
        let (events, received) = mpsc::channel();
        let mut replay = Box::pin(run_restored(
            &mut session,
            request,
            &tx,
            &events,
            &controls,
            Arc::clone(&aborts),
        ));
        assert!(futures::poll!(replay.as_mut()).is_pending());
        assert!(
            held.lock().unwrap().is_some(),
            "installation completed before display waits"
        );
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        drop(replay);
        assert!(aborts.lock().unwrap().is_none());
        let Event::Pending(returned) = received.try_recv().unwrap() else {
            panic!("return only the native input after installed maintenance");
        };
        assert_eq!(returned.text(), "after committed compaction");
        assert!(returned.compaction.is_none());
        assert!(returned.following.lock().unwrap().is_empty());
        same_payload(&returned.pending.lock().unwrap()[0].content, &payload());
        drop(held.lock().unwrap().take());
        assert!(
            store
                .try_lock()
                .unwrap()
                .entries
                .iter()
                .any(|entry| { matches!(entry, crate::session::SessionEntry::Compaction(_)) })
        );
        run_restored(&mut session, stale, &tx, &events, &controls, Arc::default()).await;
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        assert!(received.try_recv().is_err());
    });
}
