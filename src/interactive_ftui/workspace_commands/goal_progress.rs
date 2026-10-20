//! Display and invocation-owned cancellation for terminal goal commands.
//!
//! A native AgentEnd finishes one prompt, not the containing goal invocation.
//! Keep the model working until the command has settled its journal and report.
//! The host explicitly binds ONE GoalControl before dispatch. Observed output
//! loss pauses that run; it never authorizes another prompt or changes tools.

use std::sync::mpsc::{SendError, Sender};
use std::sync::{Arc, Mutex};

use crate::agent::AgentEvent;
use crate::goal::GoalControl;
use crate::interactive::PiMsg;
use crate::sdk::{EventListeners, SubscriptionId};

#[derive(Default)]
struct DisplayState {
    closed: bool,
    started: bool,
    text_seen: bool,
    thinking_seen: bool,
    last_end: Option<PiMsg>,
    control: Option<GoalControl>,
}

struct Display {
    sender: Sender<PiMsg>,
    state: Mutex<DisplayState>,
}

impl Display {
    fn bind_control(&self, control: GoalControl) {
        let refuse = {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closed || state.control.is_some() {
                Some(control)
            } else {
                state.control = Some(control);
                None
            }
        };
        // Never invoke a native control while holding the display mutex.
        // Abort may wake a task which immediately emits another SDK event.
        if let Some(control) = refuse {
            control.pause();
        }
    }

    fn observe(&self, event: &AgentEvent) {
        self.messages(crate::interactive_ftui::agent_event_to_pi_msgs(event));
    }

    fn messages(&self, messages: Vec<PiMsg>) {
        let pause = {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closed {
                return;
            }
            if self.forward(&mut state, messages).is_err() {
                state.closed = true;
                state.last_end = None;
                state.control.take()
            } else {
                None
            }
        };
        if let Some(control) = pause {
            // This is the explicitly bound invocation, not another session
            // or an SDK subscriber's unrelated work. Pause seals continuation
            // and asks the current native prompt to abort-and-drain.
            control.pause();
        }
    }

    fn forward(&self, state: &mut DisplayState, messages: Vec<PiMsg>) -> Result<(), SendError<PiMsg>> {
        for message in messages {
            let outgoing = match message {
                PiMsg::AgentStart => {
                    state.last_end = None;
                    if state.started {
                        // Both buffers remain native model buffers. Separate
                        // adjacent prompts without manufacturing a tool card,
                        // user input, turn-end or synthetic assistant verdict.
                        if state.text_seen {
                            self.sender.send(PiMsg::TextDelta("\n\n".into()))?;
                        }
                        if state.thinking_seen {
                            self.sender.send(PiMsg::ThinkingDelta("\n\n".into()))?;
                        }
                        state.text_seen = false;
                        state.thinking_seen = false;
                        None
                    } else {
                        state.started = true;
                        Some(PiMsg::AgentStart)
                    }
                }
                // Lifecycle commands have no native prompt. Do not turn
                // unrelated non-turn notifications into execution evidence.
                _ if !state.started => None,
                message @ PiMsg::AgentDone { .. } => {
                    state.last_end = Some(message);
                    None
                }
                PiMsg::TextDelta(text) => {
                    state.text_seen |= !text.is_empty();
                    Some(PiMsg::TextDelta(text))
                }
                PiMsg::ThinkingDelta(text) => {
                    state.thinking_seen |= !text.is_empty();
                    Some(PiMsg::ThinkingDelta(text))
                }
                // These variants currently come from host code, not the
                // shared translator. Retain the run boundary if it expands.
                PiMsg::AgentError(error) => Some(PiMsg::SystemNote(error)),
                PiMsg::AgentTimeCap { usage, message } => {
                    state.last_end = Some(PiMsg::AgentDone {
                        usage,
                        stop_reason: crate::model::StopReason::Stop,
                        error_message: None,
                    });
                    Some(PiMsg::SystemNote(message))
                }
                other => Some(other),
            };
            if let Some(message) = outgoing {
                self.sender.send(message)?;
            }
        }
        Ok(())
    }

    fn close(&self, completed: bool) {
        let pause = {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closed {
                return;
            }
            // Close under the same lock used for delivery. An SDK callback may
            // already have been cloned by EventListeners before unsubscribe.
            state.closed = true;
            let control = state.control.take();
            if state.started {
                let message = if completed {
                    state.last_end.take().unwrap_or_else(|| PiMsg::AgentError(
                        "Goal command ended without a native turn-end event; inspect its report and session history.".into(),
                    ))
                } else {
                    PiMsg::AgentError(
                        "Goal command was interrupted; review its journal and side effects before resuming.".into(),
                    )
                };
                let _ = self.sender.send(message);
            }
            if completed { None } else { control }
        };
        if let Some(control) = pause {
            control.pause();
        }
    }
}

/// One temporary subscription, retaining no borrow of the live SDK handle.
/// Drop retires even an abandoned command; late cloned callbacks are inert.
pub(super) struct Observation {
    listeners: EventListeners,
    subscription: Option<SubscriptionId>,
    display: Arc<Display>,
}

impl Observation {
    pub(super) fn attach(listeners: &EventListeners, sender: &Sender<PiMsg>) -> Self {
        let display = Arc::new(Display {
            sender: sender.clone(),
            state: Mutex::new(DisplayState::default()),
        });
        let observed = Arc::clone(&display);
        let subscription = listeners.subscribe(Arc::new(move |event| observed.observe(&event)));
        Self {
            listeners: listeners.clone(),
            subscription: Some(subscription),
            display,
        }
    }

    /// Used once, before the goal driver is polled. Closing this observation
    /// before publication still pauses the incoming run without dispatch.
    pub(super) fn control_publisher(&self) -> impl FnOnce(GoalControl) + Send + 'static {
        let display = Arc::clone(&self.display);
        move |control| display.bind_control(control)
    }

    pub(super) fn finish(mut self) {
        self.retire(true);
    }

    fn retire(&mut self, completed: bool) {
        if let Some(subscription) = self.subscription.take() {
            self.display.close(completed);
            let _ = self.listeners.unsubscribe(subscription);
        }
    }
}

impl Drop for Observation {
    fn drop(&mut self) {
        self.retire(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interactive_ftui::{AgentUiState, PiFtuiModel};
    use crate::model::{StopReason, Usage};
    use std::sync::mpsc;

    fn done(error: Option<&str>) -> PiMsg {
        PiMsg::AgentDone {
            usage: Some(Usage::default()),
            stop_reason: if error.is_some() { StopReason::Error } else { StopReason::Stop },
            error_message: error.map(str::to_owned),
        }
    }

    #[test]
    fn native_prompt_end_does_not_unlock_the_model_before_goal_settlement() {
        let listeners = EventListeners::default();
        let (sender, receiver) = mpsc::channel();
        let observation = Observation::attach(&listeners, &sender);
        let (_unused, input) = mpsc::channel();
        let mut model = PiFtuiModel::new(input);
        observation.display.messages(vec![
            PiMsg::AgentStart,
            PiMsg::ThinkingDelta("first reasoning".into()),
            PiMsg::TextDelta("first answer".into()),
            done(None),
        ]);
        for message in receiver.try_iter() {
            let _ = model.handle_agent(message);
            assert_eq!(model.state, AgentUiState::Working);
        }
        observation.display.messages(vec![
            PiMsg::AgentStart,
            PiMsg::TextDelta("second answer".into()),
            done(None),
        ]);
        for message in receiver.try_iter() {
            let _ = model.handle_agent(message);
            assert_eq!(model.state, AgentUiState::Working);
        }
        assert_eq!(model.streaming, "first answer\n\nsecond answer");
        observation.finish();
        let last = receiver.try_recv().unwrap();
        assert!(matches!(last, PiMsg::AgentDone { .. }));
        let _ = model.handle_agent(last);
        assert_eq!(model.state, AgentUiState::Ready);
        assert!(model.streaming.is_empty());
        assert!(receiver.try_recv().is_err(), "only one outer completion");
    }

    #[test]
    fn retirement_unsubscribes_and_cloned_late_callbacks_cannot_reopen_output() {
        let listeners = EventListeners::default();
        let unrelated = listeners.subscribe(Arc::new(|_| {}));
        let (sender, receiver) = mpsc::channel();
        let observation = Observation::attach(&listeners, &sender);
        let subscription = observation.subscription.unwrap();
        let late = Arc::clone(&observation.display);
        observation.finish();
        late.messages(vec![PiMsg::AgentStart, PiMsg::TextDelta("late".into()), done(None)]);
        assert!(receiver.try_recv().is_err());
        assert!(!listeners.unsubscribe(subscription));
        assert!(listeners.unsubscribe(unrelated), "other subscriptions survive");
    }

    #[test]
    fn abandoning_live_observation_is_not_reported_as_success() {
        let listeners = EventListeners::default();
        let (sender, receiver) = mpsc::channel();
        let observation = Observation::attach(&listeners, &sender);
        let subscription = observation.subscription.unwrap();
        observation.display.messages(vec![PiMsg::AgentStart, done(None)]);
        assert!(matches!(receiver.try_recv().unwrap(), PiMsg::AgentStart));
        drop(observation);
        assert!(matches!(receiver.try_recv().unwrap(), PiMsg::AgentError(text) if text.contains("interrupted")));
        assert!(!listeners.unsubscribe(subscription));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn native_failure_and_tool_cards_use_the_existing_message_vocabulary() {
        let listeners = EventListeners::default();
        let (sender, receiver) = mpsc::channel();
        let observation = Observation::attach(&listeners, &sender);
        observation.display.messages(vec![
            PiMsg::AgentStart,
            PiMsg::ToolStart { name: "read".into(), tool_id: "tool-1".into() },
            PiMsg::ToolEnd {
                name: "read".into(), tool_id: "tool-1".into(),
                is_error: false, output: Some("file contents".into()),
            },
            done(Some("provider rejected the request")),
        ]);
        let live: Vec<_> = receiver.try_iter().collect();
        assert_eq!(live.len(), 3);
        assert!(matches!(live[1], PiMsg::ToolStart { .. }));
        assert!(matches!(live[2], PiMsg::ToolEnd { .. }));
        observation.finish();
        assert!(matches!(receiver.try_recv().unwrap(), PiMsg::AgentDone {
            stop_reason: StopReason::Error, error_message: Some(text), ..
        } if text == "provider rejected the request"));
    }

    #[test]
    fn a_started_prompt_without_its_end_cannot_reuse_the_previous_success() {
        let listeners = EventListeners::default();
        let (sender, receiver) = mpsc::channel();
        let observation = Observation::attach(&listeners, &sender);
        observation.display.messages(vec![PiMsg::AgentStart, done(None), PiMsg::AgentStart]);
        assert!(matches!(receiver.try_recv().unwrap(), PiMsg::AgentStart));
        observation.finish();
        assert!(matches!(receiver.try_recv().unwrap(), PiMsg::AgentError(text) if text.contains("without a native turn-end")));
    }

    fn with_unpolled_control(check: impl FnOnce(GoalControl)) {
        use crate::agent::{Agent, AgentConfig, AgentSession};
        use crate::agent_cx::AgentCx;
        use crate::compaction::ResolvedCompactionSettings;
        use crate::goal::{GoalBudget, GoalDecision};
        use crate::providers::openai::OpenAIProvider;
        use crate::sdk::AgentSessionHandle;
        use crate::session::Session;
        use crate::tools::ToolRegistry;
        use std::path::Path;
        use std::time::Duration;

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let agent = Agent::new(
            Arc::new(OpenAIProvider::new("unpolled-goal-control").with_base_url("http://127.0.0.1:1/v1")),
            ToolRegistry::new(&[], Path::new("."), None), AgentConfig::default(),
        );
        let session = AgentSession::new(agent, Arc::new(asupersync::sync::Mutex::new(Session::in_memory())),
            false, ResolvedCompactionSettings::default());
        let mut handle = AgentSessionHandle::from_session_with_listeners(session, EventListeners::default());
        runtime.block_on(async {
            let owner = AgentCx::for_current_or_request();
            let created = handle.create_goal(&owner, "never dispatch this fixture", GoalBudget::default()).await.unwrap();
            let run = handle.run_goal(&owner, &created.goal, Duration::from_secs(30), |_, _| {},
                |_, _| GoalDecision::Pause { reason: "test".into() }).unwrap();
            check(run.control());
            drop(run);
            assert_eq!(handle.current_goal().unwrap().unwrap().prompts_started(), 0);
        });
    }

    #[test]
    fn detected_receiver_loss_pauses_only_the_bound_run() {
        with_unpolled_control(|control| {
            let listeners = EventListeners::default();
            let (sender, receiver) = mpsc::channel();
            let observation = Observation::attach(&listeners, &sender);
            observation.control_publisher()(control.clone());
            drop(receiver);
            observation.display.messages(vec![PiMsg::AgentStart]);
            assert!(!control.pause(), "output loss must already have latched pause");
            assert!(!control.is_finished(), "pause is not proof of completed cleanup");
            assert!(observation.display.state.lock().unwrap().control.is_none());
        });
    }

    #[test]
    fn publication_after_retirement_never_admits_an_unobserved_run() {
        with_unpolled_control(|control| {
            let listeners = EventListeners::default();
            let (sender, _receiver) = mpsc::channel();
            let observation = Observation::attach(&listeners, &sender);
            let publish = observation.control_publisher();
            drop(observation);
            publish(control.clone());
            assert!(!control.pause());
        });
    }

    #[test]
    fn abandoned_observation_pauses_but_normal_finish_does_not_cancel() {
        for finish in [false, true] {
            with_unpolled_control(|control| {
                let listeners = EventListeners::default();
                let (sender, _receiver) = mpsc::channel();
                let observation = Observation::attach(&listeners, &sender);
                observation.control_publisher()(control.clone());
                if finish { observation.finish(); } else { drop(observation); }
                assert_eq!(control.pause(), finish);
            });
        }
    }
}
