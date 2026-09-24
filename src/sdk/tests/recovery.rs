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
            AgentEvent::FailoverEnd {
                restored_primary: true,
                ..
            }
        ) {
            observed.fetch_add(1, Ordering::SeqCst);
        }
    });
    let result = run_async(handle.continue_turn_with_abort(signal, move |event| {
        if matches!(
            event,
            AgentEvent::FailoverEnd {
                restored_primary: true,
                ..
            }
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
        observed
            .lock()
            .unwrap()
            .push(serde_json::to_value(event).unwrap());
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
    {
        let events = events.lock().unwrap();
        assert_eq!(
            events.iter().filter(|event| event["type"] == "auto_retry_end").count(),
            1
        );
        assert_eq!(events.last().unwrap()["type"], "agent_end");
        assert!(
            events.last().unwrap()["error"]
                .as_str()
                .unwrap()
                .contains(Error::SESSION_PERSISTENCE_PREFIX)
        );
    }
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
        observed
            .lock()
            .unwrap()
            .push(serde_json::to_value(event).unwrap());
    });
    let result = run_async(handle.try_chain_failover(&Ok(first), true, None, 1, &callback));
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
        observed
            .lock()
            .unwrap()
            .push(serde_json::to_value(event).unwrap());
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

fn event_log() -> (Arc<Mutex<Vec<Value>>>, EventSubscriber) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&events);
    let callback: EventSubscriber = Arc::new(move |event| {
        recorded
            .lock()
            .unwrap()
            .push(serde_json::to_value(event).unwrap());
    });
    (events, callback)
}

fn lifecycle_names(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event["type"].as_str()? {
            "agent_start" => Some("agent_start".to_string()),
            "agent_end" => Some("agent_end".to_string()),
            "auto_retry_start" => Some(format!("retry_start:{}", event["attempt"])),
            "auto_retry_end" => Some(format!(
                "retry_end:{}:{}",
                event["attempt"], event["success"]
            )),
            "failover_start" => Some(format!(
                "failover_start:{}:{}:{}",
                event["attempt"],
                event["chainIndex"],
                event["toModel"].as_str().unwrap(),
            )),
            "failover_end" => Some(format!(
                "failover_end:{}:{}",
                event["model"].as_str().unwrap(),
                event["success"],
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn every_logical_turn_has_one_terminal_event_after_all_recovery_events() {
    for entrypoint in ENTRYPOINTS {
        let (handle, calls) = flaky_handle(2);
        let mut handle = handle.with_retry(Some(fast_retry_policy(3)));
        let (events, callback) = event_log();
        let result = run_async(invoke(&mut handle, entrypoint, callback)).unwrap();
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let events = events.lock().unwrap();
        assert_eq!(
            lifecycle_names(&events),
            [
                "agent_start",
                "retry_start:1",
                "retry_end:1:false",
                "retry_start:2",
                "retry_end:2:true",
                "agent_end",
            ],
            "{entrypoint:?}"
        );
        let terminal = events.last().unwrap();
        assert_eq!(terminal["type"], "agent_end");
        assert!(terminal.get("error").is_none());
        assert_eq!(
            terminal["messages"],
            serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap(),
            "terminal payload retains completed work, not reverted error attempts"
        );
        assert_eq!(events[0]["sessionId"], terminal["sessionId"]);
    }
}

#[test]
fn the_terminal_event_excludes_history_from_earlier_public_calls() {
    let (mut handle, _) = flaky_handle(0);
    run_async(handle.prompt("earlier input", |_| {})).unwrap();
    let (events, callback) = event_log();
    run_async(handle.prompt("new input", move |event| callback(event))).unwrap();
    let events = events.lock().unwrap();
    let new_messages = events.last().unwrap()["messages"].as_array().unwrap();
    assert_eq!(new_messages.len(), 2);
    assert!(!serde_json::to_string(new_messages).unwrap().contains("earlier input"));
    assert_eq!(run_async(handle.messages()).unwrap().len(), 4);
}

#[test]
fn an_aborted_retry_reports_one_failed_terminal_event_not_a_premature_503_end() {
    let (handle, calls) = flaky_handle(1);
    let mut handle = handle.with_retry(Some(fast_retry_policy(2)));
    let (abort, signal) = AbortHandle::new();
    let (events, callback) = event_log();
    let result = run_async(handle.prompt_with_abort("hello", signal, move |event| {
        if matches!(event, AgentEvent::AutoRetryStart { .. }) {
            abort.abort();
        }
        callback(event);
    }));
    assert!(matches!(result, Err(Error::Aborted)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let events = events.lock().unwrap();
    assert_eq!(
        lifecycle_names(&events),
        ["agent_start", "retry_start:1", "retry_end:1:false", "agent_end"]
    );
    assert!(
        events.last().unwrap()["error"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("abort")
    );
}

/// A bounded local HTTP fixture: exercises the real provider factory, request
/// serialization, SSE parsing and AgentSession loop, without live credentials.
struct RecoveryHttpFixture {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl RecoveryHttpFixture {
    #[allow(clippy::too_many_lines)]
    fn new(responses: Vec<(u16, &'static str, String)>) -> Self {
        use std::io::{Read as _, Write as _};
        use std::time::{Duration, Instant};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            for (status, content_type, body) in responses {
                let deadline = Instant::now() + Duration::from_secs(30);
                let mut stream = loop {
                    if stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "request fixture timed out");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("accept failed: {error}"),
                    }
                };
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                let (header_end, content_length) = loop {
                    assert!(Instant::now() < deadline, "request headers timed out");
                    let read = stream.read(&mut buffer).unwrap();
                    assert!(read > 0, "request ended before headers");
                    request.extend_from_slice(&buffer[..read]);
                    assert!(request.len() <= 128 * 1024, "oversized fixture request");
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&request[..end]).unwrap();
                        assert!(headers.lines().next().unwrap().contains("/chat/completions"));
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .expect("content-length header");
                        assert!(length <= 128 * 1024, "oversized fixture body");
                        break (end + 4, length);
                    }
                };
                while request.len() < header_end + content_length {
                    assert!(Instant::now() < deadline, "request body timed out");
                    let read = stream.read(&mut buffer).unwrap();
                    assert!(read > 0, "request ended before body");
                    request.extend_from_slice(&buffer[..read]);
                    assert!(request.len() <= 256 * 1024);
                }
                let value = serde_json::from_slice(&request[header_end..header_end + content_length])
                    .expect("provider request JSON");
                captured.lock().unwrap().push(value);
                let reason = if status == 200 { "OK" } else { "Fixture Error" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len(),
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn finish(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().expect("HTTP fixture worker");
    }
}

impl Drop for RecoveryHttpFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn capacity_response() -> (u16, &'static str, String) {
    (
        503,
        "application/json",
        r#"{"error":{"message":"503 service unavailable","type":"server_error"}}"#.to_string(),
    )
}

fn completion_response() -> (u16, &'static str, String) {
    (
        200,
        "text/event-stream",
        concat!(
            "data: {\"id\":\"chatcmpl-fixture\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fallback-b\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Recovered\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string(),
    )
}

fn http_chain_handle(url: &str, cap: u32) -> (AgentSessionHandle, Arc<AtomicUsize>) {
    let (handle, calls) = flaky_handle_as(usize::MAX, "anthropic", "claude-x");
    let entries = ["fallback-a", "fallback-b"]
        .into_iter()
        .map(|model| {
            let mut entry = crate::models::ad_hoc_model_entry("openai", model).unwrap();
            entry.model.api = "openai-completions".to_string();
            entry.model.base_url = url.to_string();
            entry
        })
        .collect();
    let handle = handle
        .with_retry(Some(crate::failover::RetryPolicy {
            max_retries: 0,
            max_failovers_per_turn: cap,
            base_delay_ms: 0,
            max_delay_ms: 0,
        }))
        .with_failover(Some(FailoverOptions {
            chains: HashMap::from([(
                "default".to_string(),
                vec![
                    "anthropic/claude-x".to_string(),
                    "not-a-spec".to_string(),
                    "openai/fallback-a".to_string(),
                    "OPENAI/FALLBACK-A".to_string(),
                    "openai/fallback-b".to_string(),
                ],
            )]),
            available_models: entries,
            auth: AuthStorage::empty_at(PathBuf::from("unused-fixture-auth.json")),
            cli_api_key: Some("test-key".to_string()),
            cooldown_secs: 300,
        }));
    (handle, calls)
}

#[test]
fn real_transport_multi_hop_failover_pairs_each_committed_hop_before_terminal_end() {
    let mut server = RecoveryHttpFixture::new(vec![capacity_response(), completion_response()]);
    let (mut handle, calls) = http_chain_handle(&server.url, 2);
    let (events, callback) = event_log();
    let result = run_async(handle.prompt("recover across the chain", move |event| callback(event)))
        .expect("second fallback succeeds");
    server.finish();
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(handle.model().1, "fallback-b");
    let requests = server.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|request| request["model"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["fallback-a", "fallback-b"]
    );
    let events = events.lock().unwrap();
    assert_eq!(
        lifecycle_names(&events),
        [
            "agent_start",
            "failover_start:1:2:fallback-a",
            "failover_end:fallback-a:false",
            "failover_start:2:4:fallback-b",
            "failover_end:fallback-b:true",
            "agent_end",
        ]
    );
    assert!(events.last().unwrap().get("error").is_none());
    assert_eq!(
        events.last().unwrap()["messages"],
        serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap()
    );
}

#[test]
fn abort_after_the_second_swap_closes_both_hops_without_contacting_its_provider() {
    let mut server = RecoveryHttpFixture::new(vec![capacity_response(), completion_response()]);
    let (mut handle, _) = http_chain_handle(&server.url, 2);
    let (abort, signal) = AbortHandle::new();
    let (events, callback) = event_log();
    let result = run_async(handle.prompt_with_abort("hello", signal, move |event| {
        if matches!(event, AgentEvent::FailoverStart { attempt: 2, .. }) {
            abort.abort();
        }
        callback(event);
    }));
    server.finish();
    assert!(matches!(result, Err(Error::Aborted)));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    let events = events.lock().unwrap();
    assert_eq!(
        lifecycle_names(&events),
        [
            "agent_start",
            "failover_start:1:2:fallback-a",
            "failover_end:fallback-a:false",
            "failover_start:2:4:fallback-b",
            "failover_end:fallback-b:false",
            "agent_end",
        ]
    );
    assert!(
        events.last().unwrap()["error"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("abort")
    );
}

#[test]
fn a_swap_cap_counts_commits_not_skipped_specs_or_terminal_events() {
    let mut server = RecoveryHttpFixture::new(vec![capacity_response(), completion_response()]);
    let (mut handle, _) = http_chain_handle(&server.url, 1);
    let (events, callback) = event_log();
    let result = run_async(handle.prompt("bounded chain", move |event| callback(event)));
    server.finish();
    assert!(
        result.is_err()
            || result.as_ref().is_ok_and(|message| message.stop_reason == StopReason::Error)
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    assert_eq!(handle.model().1, "fallback-a");
    let events = events.lock().unwrap();
    assert_eq!(
        lifecycle_names(&events),
        [
            "agent_start",
            "failover_start:1:2:fallback-a",
            "failover_end:fallback-a:false",
            "agent_end",
        ]
    );
    assert!(events.last().unwrap()["error"].is_string());
}

fn write_tool_response() -> (u16, &'static str, String) {
    let arguments = serde_json::json!({"path": "result.txt", "content": "saved exactly once"})
        .to_string();
    let chunk = serde_json::json!({
        "id": "chatcmpl-tool-fixture", "object": "chat.completion.chunk", "created": 0,
        "model": "fallback-b",
        "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
            "index": 0, "id": "write-once", "type": "function",
            "function": {"name": "write", "arguments": arguments}
        }]}, "finish_reason": null}]
    });
    let end = serde_json::json!({
        "id": "chatcmpl-tool-fixture",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
    });
    (
        200,
        "text/event-stream",
        format!("data: {chunk}\n\ndata: {end}\n\ndata: [DONE]\n\n"),
    )
}

#[test]
fn real_tool_work_survives_retry_once_and_terminal_end_follows_persistence() {
    let dir = tempdir().unwrap();
    let mut server = RecoveryHttpFixture::new(vec![
        write_tool_response(),
        capacity_response(),
        completion_response(),
    ]);
    let mut entry = crate::models::ad_hoc_model_entry("openai", "fallback-b").unwrap();
    entry.model.api = "openai-completions".to_string();
    entry.model.base_url = server.url.clone();
    let provider = crate::providers::create_provider(&entry, None).unwrap();
    let agent = Agent::new(
        provider,
        ToolRegistry::new(&["write"], dir.path(), None),
        AgentConfig {
            stream_options: StreamOptions {
                api_key: Some("test-key".to_string()),
                ..Default::default()
            },
            ..AgentConfig::default()
        },
    );
    let mut stored = Session::create_with_dir(Some(dir.path().join("sessions")));
    stored.header.cwd = dir.path().display().to_string();
    stored.header.provider = Some("openai".to_string());
    stored.header.model_id = Some("fallback-b".to_string());
    let session = AgentSession::new(
        agent,
        Arc::new(AsyncMutex::new(stored)),
        true,
        ResolvedCompactionSettings {
            enabled: false,
            ..Default::default()
        },
    );
    let mut handle =
        AgentSessionHandle::from_session_with_listeners(session, EventListeners::new())
            .with_retry(Some(fast_retry_policy(1)));
    let (events, callback) = event_log();
    let store = handle.session_store();
    let result = run_async(handle.prompt("write result.txt once", move |event| {
        if matches!(event, AgentEvent::AgentEnd { .. }) {
            let stored = store.try_lock().expect("terminal callback must not hold session lock");
            let bytes = std::fs::read_to_string(stored.path.as_ref().unwrap()).unwrap();
            assert!(bytes.contains("Recovered"), "AgentEnd preceded final persistence");
        }
        callback(event);
    }))
    .expect("recovered tool turn");
    server.finish();
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("result.txt")).unwrap(),
        "saved exactly once"
    );
    let events = events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "tool_execution_start")
            .count(),
        1
    );
    assert_eq!(
        lifecycle_names(&events),
        ["agent_start", "retry_start:1", "retry_end:1:true", "agent_end"]
    );
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let resumed = requests[2]["messages"].as_array().unwrap();
    assert_eq!(resumed.iter().filter(|message| message["role"] == "tool").count(), 1);
    assert_eq!(resumed.iter().filter(|message| message["role"] == "user").count(), 1);
    let path = handle.session_store().try_lock().unwrap().path.clone().unwrap();
    let reopened = run_async(Session::open(&path.display().to_string())).unwrap();
    assert_eq!(
        events.last().unwrap()["messages"],
        serde_json::to_value(reopened.to_messages_for_current_path()).unwrap()
    );
}

struct TransportDropProvider;

#[async_trait::async_trait]
impl Provider for TransportDropProvider {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "anthropic"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn api(&self) -> &str {
        "test-api"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn model_id(&self) -> &str {
        "claude-x"
    }

    async fn stream(
        &self,
        _context: &crate::provider::Context<'_>,
        _options: &StreamOptions,
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
        Err(Error::Io(Box::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "wire",
        ))))
    }
}

#[test]
fn a_typed_transport_failure_can_fail_over_without_transient_words_in_display() {
    let mut server = RecoveryHttpFixture::new(vec![completion_response()]);
    let (mut handle, _) = http_chain_handle(&server.url, 1);
    handle.session.agent.set_provider(Arc::new(TransportDropProvider));
    let (events, callback) = event_log();
    let result = run_async(handle.prompt("recover the wire", move |event| callback(event)))
        .expect("typed transport recovery");
    server.finish();
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    assert_eq!(handle.model().1, "fallback-a");
    let events = events.lock().unwrap();
    let start = events.iter().find(|event| event["type"] == "failover_start").unwrap();
    assert_eq!(start["class"], "transient");
    assert_eq!(start["attempt"], 1);
    assert_eq!(
        lifecycle_names(&events),
        [
            "agent_start",
            "failover_start:1:2:fallback-a",
            "failover_end:fallback-a:true",
            "agent_end",
        ]
    );
}
