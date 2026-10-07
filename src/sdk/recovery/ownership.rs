//! Abandonment is not a completed, persisted SDK turn.
//!
//! Dropping a future cannot run its post-turn save. Keep provider re-entry
//! closed until the caller explicitly reconstructs a session, rather than
//! rehydrating an older transcript and potentially repeating completed tools.

use std::sync::{Arc, Mutex};

use super::{LogicalTurn, LogicalTurnState};
use crate::agent::ProviderAdmissionGate;

const INTERRUPTED_TURN: &str = "SDK_TURN_INTERRUPTED: the SDK turn future was dropped before its outcome and persistence were observed; start a new or resumed session and reconcile any completed tool effects before prompting again";

/// Created only after SDK preflight, immediately before entering a turn.
/// Ordinary returned errors and cooperative aborts pass through `finish` and
/// remain reusable when persistence succeeded. An unpolled future owns no guard.
pub(super) struct TurnGuard {
    admission: ProviderAdmissionGate,
    state: Arc<Mutex<LogicalTurnState>>,
}

impl TurnGuard {
    pub(super) fn new(turn: &LogicalTurn, admission: ProviderAdmissionGate) -> Self {
        Self {
            admission,
            state: Arc::clone(&turn.state),
        }
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        let interrupted = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.finished {
                false
            } else {
                // Seal retained callbacks. Do not emit AgentEnd: neither an
                // attempt's raw terminal nor dropping its owner proves a save.
                state.finished = true;
                state.session_id = None;
                state.messages.clear();
                state.retry = None;
                state.fallback = None;
                true
            }
        };
        if interrupted && self.admission.reason().is_none() {
            // Never clear admission, run callbacks, spawn cleanup, or attempt
            // blocking persistence from Drop. A save/transition may already
            // have supplied a more specific failure reason.
            self.admission.block(INTERRUPTED_TURN.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::{Future, poll_fn};
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::Poll;
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::model::{AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, Usage};
    use crate::provider::{Context, Provider, StreamOptions};
    use crate::sdk::{Agent, AgentConfig, AgentEvent, AgentSession, AgentSessionHandle, Error, EventListeners, ImageContent, Result, Session};

    fn completed_message() -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("completed"))],
            api: "ownership-fixture".to_string(),
            provider: "ownership-fixture".to_string(),
            model: "ownership-fixture".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 1,
        }
    }

    #[derive(Default)]
    struct Probe {
        calls: AtomicUsize,
        dropped: AtomicBool,
    }

    struct PendingProvider(Arc<Probe>);

    struct PendingGuard(Arc<Probe>);

    impl Drop for PendingGuard {
        fn drop(&mut self) {
            self.0.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl Provider for PendingProvider {
        fn name(&self) -> &str { "ownership-fixture" }
        fn api(&self) -> &str { "ownership-fixture" }
        fn model_id(&self) -> &str { "ownership-fixture" }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
            if self.0.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let _guard = PendingGuard(Arc::clone(&self.0));
                futures::future::pending::<()>().await;
            }
            Ok(Box::pin(futures::stream::iter(vec![Ok(StreamEvent::Done {
                reason: StopReason::Stop,
                message: completed_message(),
            })])))
        }
    }

    fn handle(probe: Arc<Probe>) -> AgentSessionHandle {
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);
        let agent = Agent::new(Arc::new(PendingProvider(probe)), tools, AgentConfig::default());
        let session = AgentSession::new(
            agent,
            Arc::new(asupersync::sync::Mutex::new(Session::in_memory())),
            false,
            crate::compaction::ResolvedCompactionSettings {
                enabled: false,
                ..Default::default()
            },
        );
        AgentSessionHandle::from_session_with_listeners(session, EventListeners::default())
    }

    async fn bounded<F: Future>(future: F) -> F::Output {
        let owner = crate::agent_cx::AgentCx::for_current_or_request();
        let time = owner.time();
        match futures::future::select(
            Box::pin(future),
            Box::pin(time.sleep(Duration::from_secs(5))),
        ).await {
            futures::future::Either::Left((result, _)) => result,
            futures::future::Either::Right(((), pending)) => {
                drop(pending);
                panic!("SDK ownership test exceeded its watchdog");
            }
        }
    }

    #[test]
    fn raw_attempt_terminal_does_not_disarm_abandonment_or_forge_a_logical_end() {
        let gate = ProviderAdmissionGate::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let turn = LogicalTurn::new(Arc::new(move |event| captured.lock().unwrap().push(event)));
        let guard = TurnGuard::new(&turn, gate.clone());
        let retained = turn.callback();
        turn.emit(AgentEvent::AgentStart { session_id: Arc::from("owned") });
        turn.emit(AgentEvent::AgentEnd {
            session_id: Arc::from("owned"),
            messages: vec![Message::Assistant(Arc::new(completed_message()))],
            error: None,
        });
        assert_eq!(events.lock().unwrap().len(), 1);
        drop(guard);
        let error = gate.ensure_allowed().expect_err("unobserved persistence must fence");
        assert!(error.is_session_persistence());
        assert!(error.to_string().contains("SDK_TURN_INTERRUPTED"));
        retained(AgentEvent::AgentStart { session_id: Arc::from("late") });
        turn.finish(&Ok(completed_message()));
        assert_eq!(events.lock().unwrap().len(), 1, "no late events or fabricated save acknowledgement");
    }

    #[test]
    fn settled_success_and_cooperative_errors_do_not_quarantine_the_handle() {
        for outcome in [Ok(completed_message()), Err(Error::Aborted), Err(Error::api("503"))] {
            let gate = ProviderAdmissionGate::default();
            let turn = LogicalTurn::new(Arc::new(|_| {}));
            let guard = TurnGuard::new(&turn, gate.clone());
            turn.finish(&outcome);
            drop(guard);
            gate.ensure_allowed().expect("an observed outcome is not abandonment");
        }
    }

    #[test]
    fn abandonment_and_settlement_never_clear_an_existing_quarantine() {
        for settled in [false, true] {
            let gate = ProviderAdmissionGate::default();
            let turn = LogicalTurn::new(Arc::new(|_| {}));
            let guard = TurnGuard::new(&turn, gate.clone());
            gate.block("specific save failure".to_string());
            if settled { turn.finish(&Ok(completed_message())); }
            drop(guard);
            assert_eq!(gate.reason().as_deref(), Some("specific save failure"));
        }
    }

    #[test]
    fn dropping_an_unpolled_public_prompt_does_not_consume_the_session() {
        let probe = Arc::new(Probe::default());
        let mut handle = handle(Arc::clone(&probe));
        drop(Box::pin(handle.prompt("never admitted", |_| {})));
        handle.session.ensure_provider_reentry_allowed().unwrap();
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
        assert!(!probe.dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn abandoned_public_turn_fences_every_prompt_shape_and_continuation() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let probe = Arc::new(Probe::default());
        let mut handle = handle(Arc::clone(&probe));
        runtime.block_on(bounded(async {
            let mut prompt = Box::pin(handle.prompt("admitted exactly once", |_| {}));
            poll_fn(|cx| {
                assert!(prompt.as_mut().poll(cx).is_pending());
                if probe.calls.load(Ordering::SeqCst) == 1 { Poll::Ready(()) } else { Poll::Pending }
            }).await;
            drop(prompt);
            assert!(probe.dropped.load(Ordering::SeqCst), "the actual provider future must be dropped");
            let before = serde_json::to_value(handle.messages().await.unwrap()).unwrap();
            let results = [
                handle.prompt("must not append", |_| {}).await,
                handle.prompt_with_images("must not append", vec![ImageContent {
                    data: "aGVsbG8=".to_string(), mime_type: "image/png".to_string(),
                }], |_| {}).await,
                handle.prompt_with_content(vec![ContentBlock::Text(TextContent::new("must not append"))], |_| {}).await,
                handle.continue_turn(|_| {}).await,
            ];
            for result in results {
                let error = result.expect_err("abandoned turn requires deliberate recovery");
                assert!(error.is_session_persistence(), "{error}");
                assert!(error.to_string().contains("SDK_TURN_INTERRUPTED"), "{error}");
            }
            assert_eq!(serde_json::to_value(handle.messages().await.unwrap()).unwrap(), before);
            assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
            assert_ne!(before, json!([]), "the original input really entered the session");
        }));
    }
}
