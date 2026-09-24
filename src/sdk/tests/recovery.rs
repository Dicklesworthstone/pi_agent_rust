//! Production SDK entrypoint regressions using the existing provider fixture.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug)]
enum Entrypoint {
    Prompt,
    PromptWithAbort,
    Continue,
    ContinueWithAbort,
}

const ENTRYPOINTS: [Entrypoint; 4] = [
    Entrypoint::Prompt,
    Entrypoint::PromptWithAbort,
    Entrypoint::Continue,
    Entrypoint::ContinueWithAbort,
];

async fn invoke(
    handle: &mut AgentSessionHandle,
    entrypoint: Entrypoint,
    callback: EventSubscriber,
) -> Result<AssistantMessage> {
    let (_abort, signal) = AbortHandle::new();
    match entrypoint {
        Entrypoint::Prompt => {
            handle
                .prompt("one user input", move |event| callback(event))
                .await
        }
        Entrypoint::PromptWithAbort => {
            handle
                .prompt_with_abort("one user input", signal, move |event| callback(event))
                .await
        }
        Entrypoint::Continue => handle.continue_turn(move |event| callback(event)).await,
        Entrypoint::ContinueWithAbort => {
            handle
                .continue_turn_with_abort(signal, move |event| callback(event))
                .await
        }
    }
}

#[test]
fn every_entrypoint_resumes_instead_of_replaying_user_input() {
    for entrypoint in ENTRYPOINTS {
        let (handle, calls) = flaky_handle(1);
        let mut handle = handle.with_retry(Some(fast_retry_policy(2)));
        let message = run_async(invoke(&mut handle, entrypoint, Arc::new(|_| {})))
            .expect("retry completes");
        assert_eq!(message.stop_reason, StopReason::Stop, "{entrypoint:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "{entrypoint:?}");
        let messages = run_async(handle.messages()).expect("session messages");
        let users = messages
            .iter()
            .filter(|message| matches!(message, Message::User(_)))
            .count();
        let expected_users = usize::from(matches!(
            entrypoint,
            Entrypoint::Prompt | Entrypoint::PromptWithAbort
        ));
        assert_eq!(users, expected_users, "{entrypoint:?}: input must not replay");
        assert!(
            !messages.iter().any(|message| matches!(
                message,
                Message::Assistant(assistant) if assistant.stop_reason == StopReason::Error
            )),
            "{entrypoint:?}: incomplete error tail must be removed"
        );
    }
}

#[test]
fn no_policy_preserves_the_first_error_on_every_entrypoint() {
    for entrypoint in ENTRYPOINTS {
        let (mut handle, calls) = flaky_handle(1);
        let recoveries = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&recoveries);
        let message = run_async(invoke(
            &mut handle,
            entrypoint,
            Arc::new(move |event| {
                if matches!(
                    event,
                    AgentEvent::AutoRetryStart { .. } | AgentEvent::FailoverStart { .. }
                ) {
                    observed.fetch_add(1, Ordering::SeqCst);
                }
            }),
        ))
        .expect("first errored message");
        assert_eq!(message.stop_reason, StopReason::Error, "{entrypoint:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{entrypoint:?}");
        assert_eq!(recoveries.load(Ordering::SeqCst), 0, "{entrypoint:?}");
    }
}

#[test]
fn recovery_events_reach_subscribers_without_double_firing_typed_hooks() {
    for entrypoint in ENTRYPOINTS {
        let (handle, _) = flaky_handle(1);
        let mut handle = handle.with_retry(Some(fast_retry_policy(2)));
        let subscribed = Arc::new(Mutex::new(Vec::<Value>::new()));
        let per_prompt = Arc::new(Mutex::new(Vec::<Value>::new()));
        let streams = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&subscribed);
        handle.subscribe(move |event| {
            observed
                .lock()
                .unwrap()
                .push(serde_json::to_value(event).unwrap());
        });
        let observed_streams = Arc::clone(&streams);
        handle.listeners_mut().on_stream_event = Some(Arc::new(move |_| {
            observed_streams.fetch_add(1, Ordering::SeqCst);
        }));
        let observed = Arc::clone(&per_prompt);
        run_async(invoke(
            &mut handle,
            entrypoint,
            Arc::new(move |event| {
                observed
                    .lock()
                    .unwrap()
                    .push(serde_json::to_value(event).unwrap());
            }),
        ))
        .expect("retry completes");
        let subscribed = subscribed.lock().unwrap();
        let per_prompt = per_prompt.lock().unwrap();
        assert_eq!(*subscribed, *per_prompt, "{entrypoint:?}: one shared fan-out");
        for name in ["auto_retry_start", "auto_retry_end"] {
            assert_eq!(
                subscribed.iter().filter(|event| event["type"] == name).count(),
                1,
                "{entrypoint:?}: missing or duplicated {name}"
            );
        }
        assert_eq!(
            streams.load(Ordering::SeqCst),
            2,
            "{entrypoint:?}: one error and one done stream event, not double-dispatched"
        );
    }
}

#[test]
fn pre_aborted_entrypoints_do_not_append_or_contact_a_provider() {
    for continuation in [false, true] {
        let (handle, calls) = flaky_handle(0);
        let mut handle = handle.with_retry(Some(fast_retry_policy(2)));
        let (abort, signal) = AbortHandle::new();
        abort.abort();
        let seen = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&seen);
        let result = run_async(async {
            if continuation {
                handle
                    .continue_turn_with_abort(signal, move |_| {
                        observed.fetch_add(1, Ordering::SeqCst);
                    })
                    .await
            } else {
                handle
                    .prompt_with_abort("must not append", signal, move |_| {
                        observed.fetch_add(1, Ordering::SeqCst);
                    })
                    .await
            }
        });
        assert!(matches!(result, Err(Error::Aborted)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(seen.load(Ordering::SeqCst), 0);
        assert!(run_async(handle.messages()).unwrap().is_empty());
        assert!(handle.session.agent.messages().is_empty());
    }
}

#[test]
fn aborting_at_retry_start_returns_abort_not_the_original_capacity_error() {
    for continuation in [false, true] {
        let (handle, calls) = flaky_handle(1);
        let mut handle = handle.with_retry(Some(fast_retry_policy(3)));
        let (abort, signal) = AbortHandle::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&events);
        let callback = move |event| {
            if matches!(&event, AgentEvent::AutoRetryStart { .. }) {
                abort.abort();
            }
            observed
                .lock()
                .unwrap()
                .push(serde_json::to_value(event).unwrap());
        };
        let result = run_async(async {
            if continuation {
                handle.continue_turn_with_abort(signal, callback).await
            } else {
                handle.prompt_with_abort("hello", signal, callback).await
            }
        });
        assert!(matches!(result, Err(Error::Aborted)));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no replay after cancellation");
        let events = events.lock().unwrap();
        let ends: Vec<_> = events
            .iter()
            .filter(|event| event["type"] == "auto_retry_end")
            .collect();
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0]["success"], false);
        assert!(
            ends[0]["finalError"]
                .as_str()
                .unwrap()
                .to_ascii_lowercase()
                .contains("abort")
        );
    }
}

#[test]
fn a_public_continuation_restores_the_primary_before_provider_reentry() {
    let mut handle = handle_after_one_failover(0);
    let (abort, signal) = AbortHandle::new();
    let restored = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&restored);
    handle.subscribe(move |event| {
        if matches!(
            event,
            AgentEvent::FailoverEnd { restored_primary: true, .. }
        ) {
            observed.fetch_add(1, Ordering::SeqCst);
        }
    });
    let result = run_async(handle.continue_turn_with_abort(signal, move |event| {
        if matches!(
            event,
            AgentEvent::FailoverEnd { restored_primary: true, .. }
        ) {
            // Cancel at the preflight boundary: no external primary request.
            abort.abort();
        }
    }));
    assert!(matches!(result, Err(Error::Aborted)));
    assert_eq!(handle.model().1, "claude-3-5-haiku-latest");
    assert_eq!(restored.load(Ordering::SeqCst), 1);
    assert!(handle.failover_state.primary().is_none());
}

#[test]
fn known_model_capacity_blocks_silent_overflow_recovery_on_every_entrypoint() {
    for entrypoint in ENTRYPOINTS {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(FlakyThenOkProvider {
            failures: 1,
            calls: Arc::clone(&calls),
            name: "test-provider".to_string(),
            model: "test-model".to_string(),
            input_tokens: 8_193,
        });
        let agent = Agent::new(
            provider,
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig::default(),
        );
        let mut stored = Session::in_memory();
        stored.header.provider = Some("test-provider".to_string());
        stored.header.model_id = Some("test-model".to_string());
        let mut session = AgentSession::new(
            agent,
            Arc::new(AsyncMutex::new(stored)),
            false,
            ResolvedCompactionSettings {
                enabled: false,
                ..ResolvedCompactionSettings::default()
            },
        );
        let dir = tempdir().unwrap();
        let auth = AuthStorage::empty_at(dir.path().join("auth.json"));
        let mut registry = ModelRegistry::load(&auth, None);
        let mut entry = crate::models::ad_hoc_model_entry("openai", "test-model").unwrap();
        entry.model.provider = "test-provider".to_string();
        entry.model.context_window = 8_192;
        registry.merge_entries(vec![entry]);
        session.set_model_registry(registry);
        let mut handle =
            AgentSessionHandle::from_session_with_listeners(session, EventListeners::new())
                .with_retry(Some(fast_retry_policy(3)));
        assert_eq!(
            handle.session.current_model_entry().unwrap().model.context_window,
            8_192
        );
        let message = run_async(invoke(&mut handle, entrypoint, Arc::new(|_| {}))).unwrap();
        assert_eq!(message.stop_reason, StopReason::Error, "{entrypoint:?}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{entrypoint:?}: an oversized request cannot recover by replay"
        );
    }
}
