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

fn saving_recovery_handle(dir: &Path) -> (AgentSessionHandle, Arc<AtomicUsize>) {
    let mut handle = saving_handle(dir);
    let calls = Arc::new(AtomicUsize::new(0));
    handle.session.agent.set_provider(Arc::new(FlakyThenOkProvider {
        failures: usize::MAX,
        calls: Arc::clone(&calls),
        name: "anthropic".to_string(),
        model: "claude-3-5-haiku-latest".to_string(),
        input_tokens: 0,
    }));
    (handle, calls)
}

fn assert_quarantined_entrypoints(handle: &mut AgentSessionHandle, calls: &AtomicUsize) {
    let before = calls.load(Ordering::SeqCst);
    for entrypoint in ENTRYPOINTS {
        let result = run_async(invoke(handle, entrypoint, Arc::new(|_| {})));
        assert!(
            result.as_ref().is_err_and(|error| error.is_session_persistence()),
            "{entrypoint:?}: uncertain durability must remain quarantined: {result:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), before, "{entrypoint:?}");
    }
}

#[test]
fn retry_save_failure_quarantines_later_calls_even_after_the_path_is_repaired() {
    let dir = tempdir().unwrap();
    let blocked = dir.path().join("cannot-replace-a-directory.jsonl");
    std::fs::create_dir(&blocked).unwrap();
    let (handle, calls) = saving_recovery_handle(dir.path());
    let mut handle = handle.with_retry(Some(fast_retry_policy(1)));
    let store = handle.session_store();
    let injected = Arc::new(Mutex::new(None::<(PathBuf, Value)>));
    let recorded = Arc::clone(&injected);
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observed = Arc::clone(&events);
    let result = run_async(handle.prompt("keep this input", move |event| {
        if matches!(&event, AgentEvent::AutoRetryStart { .. }) {
            // The failed attempt has already persisted. Only the subsequent
            // private retry candidate sees the injected filesystem failure.
            let mut session = store.try_lock().expect("between-attempt lock");
            *recorded.lock().unwrap() = Some((
                session.path.clone().expect("first attempt persisted"),
                serde_json::to_value(session.to_messages_for_current_path()).unwrap(),
            ));
            session.path = Some(blocked.clone());
        }
        observed.lock().unwrap().push(serde_json::to_value(event).unwrap());
    }));
    assert!(result.as_ref().is_err_and(|error| error.is_session_persistence()));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let (original_path, expected) = injected.lock().unwrap().clone().expect("fault injected");
    assert_eq!(
        serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap(),
        expected
    );
    let reopened = run_async(Session::open(&original_path.display().to_string())).unwrap();
    assert_eq!(
        serde_json::to_value(reopened.to_messages_for_current_path()).unwrap(),
        expected
    );
    assert_eq!(
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event["type"] == "auto_retry_end")
            .count(),
        1
    );
    handle.session_store().try_lock().unwrap().path = Some(original_path);
    assert_quarantined_entrypoints(&mut handle, &calls);
}

#[test]
fn failover_save_failure_preserves_source_state_and_quarantines_reentry() {
    let dir = tempdir().unwrap();
    let blocked = dir.path().join("blocked.jsonl");
    std::fs::create_dir(&blocked).unwrap();
    let (mut handle, calls) = saving_recovery_handle(dir.path());
    let first = run_async(handle.prompt("source input", |_| {})).unwrap();
    assert_eq!(first.stop_reason, StopReason::Error);
    let original_path = handle.session_store().try_lock().unwrap().path.clone().unwrap();
    let expected = serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap();
    let mut fallback = crate::models::ad_hoc_model_entry("openai", "gpt-4o-mini").unwrap();
    // This test calls the real candidate/commit path, not the target provider.
    fallback.model.base_url = "http://127.0.0.1:1/v1".to_string();
    handle = handle.with_failover(Some(FailoverOptions {
        chains: HashMap::from([(
            "default".to_string(),
            vec!["openai/gpt-4o-mini".to_string()],
        )]),
        available_models: vec![fallback],
        auth: AuthStorage::empty_at(dir.path().join("auth.json")),
        cli_api_key: Some("test-key".to_string()),
        cooldown_secs: 300,
    }));
    handle.session_store().try_lock().unwrap().path = Some(blocked);
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observed = Arc::clone(&events);
    let callback: EventSubscriber = Arc::new(move |event| {
        observed.lock().unwrap().push(serde_json::to_value(event).unwrap());
    });
    let result = run_async(handle.try_chain_failover(&Ok(first), true, None, &callback));
    assert!(result.as_ref().is_err_and(|error| error.is_session_persistence()));
    assert_eq!(handle.model().1, "claude-3-5-haiku-latest");
    assert!(handle.failover_state.primary().is_none());
    assert!(handle.failover_state.lifecycle_id().is_none());
    assert_eq!(handle.failover_state.chain_position(), 0);
    assert!(events.lock().unwrap().is_empty(), "no successful swap was published");
    assert_eq!(
        serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap(),
        expected
    );
    let reopened = run_async(Session::open(&original_path.display().to_string())).unwrap();
    assert_eq!(
        serde_json::to_value(reopened.to_messages_for_current_path()).unwrap(),
        expected
    );
    handle.session_store().try_lock().unwrap().path = Some(original_path);
    assert_quarantined_entrypoints(&mut handle, &calls);
}

fn saving_handle_after_failover(dir: &Path) -> (AgentSessionHandle, Arc<AtomicUsize>) {
    let (handle, calls) = saving_recovery_handle(dir);
    let mut handle = with_chain_cooldown(
        handle.with_retry(Some(crate::failover::RetryPolicy {
            max_retries: 0,
            max_failovers_per_turn: 1,
            base_delay_ms: 0,
            max_delay_ms: 0,
        })),
        "openai/gpt-4o-mini",
        0,
    );
    let _ = run_async(handle.prompt("fail over once", |_| {}));
    assert_eq!(handle.model().1, "gpt-4o-mini");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    (handle, calls)
}

#[test]
fn first_failover_uses_one_lifecycle_identity_in_memory_and_on_reopen() {
    let dir = tempdir().unwrap();
    let (handle, _) = saving_handle_after_failover(dir.path());
    let path = handle.session_store().try_lock().unwrap().path.clone().unwrap();
    let reopened = run_async(Session::open(&path.display().to_string())).unwrap();
    let provenance = reopened.active_failover_provenance_for_current_path().unwrap();
    assert!(provenance.lifecycle_id.is_some());
    assert_eq!(provenance.lifecycle_id.as_deref(), handle.failover_state.lifecycle_id());
    let reconstructed = crate::failover::FailoverState::reconstruct_from_session(
        &reopened,
        0,
        chrono::Utc::now(),
    );
    assert_eq!(reconstructed.lifecycle_id(), handle.failover_state.lifecycle_id());
    assert_eq!(reconstructed.chain_position(), handle.failover_state.chain_position());
}

#[test]
fn lenient_primary_restore_cannot_hide_indeterminate_persistence() {
    let dir = tempdir().unwrap();
    let (mut handle, calls) = saving_handle_after_failover(dir.path());
    let blocked = dir.path().join("blocked-primary-restore.jsonl");
    std::fs::create_dir(&blocked).unwrap();
    let original_path = handle.session_store().try_lock().unwrap().path.clone().unwrap();
    let expected = serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap();
    handle.session_store().try_lock().unwrap().path = Some(blocked);
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observed = Arc::clone(&events);
    let result = run_async(handle.prompt("must not reach the fallback", move |event| {
        observed.lock().unwrap().push(serde_json::to_value(event).unwrap());
    }));
    assert!(result.as_ref().is_err_and(|error| error.is_session_persistence()));
    assert_eq!(handle.model().1, "gpt-4o-mini");
    assert!(handle.failover_state.primary().is_some());
    assert!(
        events.lock().unwrap().is_empty(),
        "neither restoration nor a new turn committed"
    );
    assert_eq!(
        serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap(),
        expected
    );
    let reopened = run_async(Session::open(&original_path.display().to_string())).unwrap();
    assert!(reopened.active_failover_provenance_for_current_path().is_some());
    handle.session_store().try_lock().unwrap().path = Some(original_path);
    assert_quarantined_entrypoints(&mut handle, &calls);
}
