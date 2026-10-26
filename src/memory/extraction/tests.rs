use super::*;
use crate::agent::{Agent, AgentConfig, InputSource};
use crate::model::{AssistantMessage, StreamEvent, TextContent};
use crate::provider::{Provider, StreamOptions};
use crate::sdk::{AgentSessionHandle, EventListeners};
use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
        .build()
        .unwrap()
        .block_on(future)
}

#[derive(Default)]
struct Probe {
    primary_calls: AtomicUsize,
    extraction_calls: AtomicUsize,
    replies: StdMutex<VecDeque<(StopReason, String)>>,
    requests: StdMutex<Vec<(String, StreamOptions)>>,
}

impl Probe {
    fn reply(&self, reason: StopReason, text: impl Into<String>) {
        self.replies
            .lock()
            .unwrap()
            .push_back((reason, text.into()));
    }
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for Probe {
    fn name(&self) -> &str {
        "memory-probe"
    }
    fn api(&self) -> &str {
        "memory-probe"
    }
    fn model_id(&self) -> &str {
        "memory-model"
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
        let extraction = context.system_prompt.as_deref() == Some(SYSTEM_PROMPT);
        let (reason, text) = if extraction {
            self.extraction_calls.fetch_add(1, Ordering::SeqCst);
            assert!(context.tools.is_empty());
            assert_eq!(context.messages.len(), 1);
            let Message::User(UserMessage {
                content: UserContent::Text(prompt),
                ..
            }) = &context.messages[0]
            else {
                panic!("extraction must have one isolated text input");
            };
            self.requests
                .lock()
                .unwrap()
                .push((prompt.clone(), options.clone()));
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| (StopReason::Stop, "{\"memories\":[]}".to_string()))
        } else {
            self.primary_calls.fetch_add(1, Ordering::SeqCst);
            (
                StopReason::Stop,
                "Completed the requested work.".to_string(),
            )
        };
        Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Done {
            reason,
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(text))],
                stop_reason: reason,
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                api: self.api().to_string(),
                ..Default::default()
            },
        })])))
    }
}

fn settings(automatic: bool, cadence: u32) -> MemorySettings {
    MemorySettings {
        backend: Some("local".to_string()),
        auto_extract: Some(automatic),
        extract_every_turns: Some(cadence),
    }
}

fn stored(root: &std::path::Path) -> Session {
    let mut session = Session::create_with_dir(Some(root.join("sessions")));
    session.header.cwd = root.display().to_string();
    session
}

fn completed(session: &mut Session, input: &str, output: &str) -> String {
    let id = session.append_model_message(Message::User(UserMessage {
        content: UserContent::Text(input.to_string()),
        timestamp: now_ms(),
    }));
    record_authored_input(session, &id).unwrap();
    session.append_model_message(Message::assistant(AssistantMessage {
        content: vec![ContentBlock::Text(TextContent::new(output))],
        stop_reason: StopReason::Stop,
        ..Default::default()
    }))
}

fn handle(
    session: Session,
    root: &std::path::Path,
    probe: Arc<Probe>,
    save: bool,
    settings: &MemorySettings,
) -> AgentSessionHandle {
    let mut agent = Agent::new(
        probe,
        crate::tools::ToolRegistry::new(&[], root, None),
        AgentConfig::default(),
    );
    agent.stream_options_mut().api_key = Some("native-provider-pin".to_string());
    agent
        .stream_options_mut()
        .headers
        .insert("x-native-session".to_string(), "bound-options".to_string());
    let session = AgentSession::new(
        agent,
        Arc::new(asupersync::sync::Mutex::new(session)),
        save,
        crate::compaction::ResolvedCompactionSettings {
            enabled: false,
            ..Default::default()
        },
    )
    .with_memory_settings(Some(settings));
    AgentSessionHandle::from_session_with_listeners(session, EventListeners::default())
}

fn one_fact(content: &str, supersedes: Option<i64>) -> String {
    json!({"memories": [{"kind": "fact", "content": content, "tags": ["architecture"], "supersedes": supersedes}]}).to_string()
}

#[test]
fn native_sync_uses_live_binding_and_survives_reopen_without_rebilling() {
    run(async {
        let root = tempfile::tempdir().unwrap();
        let mut session = stored(root.path());
        completed(
            &mut session,
            "The parser uses a bounded queue.",
            "Verified in the source.",
        );
        session.save().await.unwrap();
        let path = session.path.clone().unwrap();
        let probe = Arc::new(Probe::default());
        probe.reply(
            StopReason::Stop,
            one_fact("The parser uses a bounded queue.", None),
        );
        let handle = handle(
            session,
            root.path(),
            Arc::clone(&probe),
            true,
            &settings(false, 3),
        );
        let report = handle.sync_memory().await.unwrap();
        assert_eq!(report.retained, 1);
        assert_eq!(handle.sync_memory().await.unwrap().status, "unchanged");
        let reopened = Session::open(path.to_str().unwrap()).await.unwrap();
        let reopened = self::handle(
            reopened,
            root.path(),
            Arc::clone(&probe),
            true,
            &settings(false, 3),
        );
        assert_eq!(reopened.sync_memory().await.unwrap().status, "unchanged");
        assert_eq!(probe.extraction_calls.load(Ordering::SeqCst), 1);
        assert_eq!(probe.primary_calls.load(Ordering::SeqCst), 0);
        let requests = probe.requests.lock().unwrap();
        assert_eq!(
            requests[0].1.api_key.as_deref(),
            Some("native-provider-pin")
        );
        assert_eq!(
            requests[0]
                .1
                .headers
                .get("x-native-session")
                .map(String::as_str),
            Some("bound-options")
        );
        assert_eq!(requests[0].1.max_tokens, Some(2048));
        let bank = MemoryStore::open(root.path()).unwrap();
        assert!(bank.mental_model().unwrap().contains("bounded queue"));
        let memory = bank.list(1).unwrap().remove(0);
        bank.edit(memory.id, crate::memory::MemoryEditOp::Forget, None)
            .unwrap();
        assert!(bank.list(1).unwrap().is_empty());
        assert!(bank.recall("bounded", 1).unwrap().is_empty());
        bank.with_conn(|conn| {
            assert!(
                conn.query_sync("SELECT memory_id FROM memory_generated", &[])
                    .unwrap()
                    .is_empty()
            );
            Ok(())
        })
        .unwrap();
        drop(requests);
        assert_eq!(reopened.sync_memory().await.unwrap().status, "unchanged");
        assert_eq!(probe.extraction_calls.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn automatic_cadence_counts_authored_turns_and_ignores_extension_user_rows() {
    run(async {
        let root = tempfile::tempdir().unwrap();
        let probe = Arc::new(Probe::default());
        let mut handle = handle(
            stored(root.path()),
            root.path(),
            Arc::clone(&probe),
            true,
            &settings(true, 2),
        );
        handle
            .session_mut()
            .run_text("First authored turn".to_string(), |_| {})
            .await
            .unwrap();
        assert_eq!(probe.extraction_calls.load(Ordering::SeqCst), 0);
        handle
            .session_mut()
            .run_text("Second authored turn".to_string(), |_| {})
            .await
            .unwrap();
        assert_eq!(probe.extraction_calls.load(Ordering::SeqCst), 1);
        handle
            .session_mut()
            .set_input_source(InputSource::Extension);
        handle
            .session_mut()
            .run_text("Generated continuation".to_string(), |_| {})
            .await
            .unwrap();
        assert_eq!(probe.extraction_calls.load(Ordering::SeqCst), 1);
        handle
            .session_mut()
            .set_input_source(InputSource::Interactive);
        handle
            .session_mut()
            .run_text("Third authored turn".to_string(), |_| {})
            .await
            .unwrap();
        assert_eq!(probe.extraction_calls.load(Ordering::SeqCst), 1);
        handle
            .session_mut()
            .run_text("Fourth authored turn".to_string(), |_| {})
            .await
            .unwrap();
        assert_eq!(probe.extraction_calls.load(Ordering::SeqCst), 2);
        assert_eq!(probe.primary_calls.load(Ordering::SeqCst), 5);
    });
}

#[test]
fn unsuccessful_extraction_does_not_fail_foreground_or_advance_checkpoint() {
    run(async {
        let root = tempfile::tempdir().unwrap();
        let probe = Arc::new(Probe::default());
        probe.reply(
            StopReason::Length,
            one_fact("Never commit this incomplete extraction.", None),
        );
        let mut handle = handle(
            stored(root.path()),
            root.path(),
            Arc::clone(&probe),
            true,
            &settings(true, 1),
        );
        let message = handle
            .session_mut()
            .run_text("Perform the task".to_string(), |_| {})
            .await
            .unwrap();
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert!(
            MemoryStore::open(root.path())
                .unwrap()
                .list(10)
                .unwrap()
                .is_empty()
        );
        probe.reply(StopReason::Stop, one_fact("Retry completed safely.", None));
        assert_eq!(handle.sync_memory().await.unwrap().retained, 1);
        assert_eq!(probe.extraction_calls.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn processed_branch_ledger_preserves_a_b_a_idempotency() {
    run(async {
        let root = tempfile::tempdir().unwrap();
        let mut session = stored(root.path());
        let branch_a = completed(&mut session, "Branch A input", "Branch A completed");
        session.save().await.unwrap();
        let probe = Arc::new(Probe::default());
        let handle = handle(
            session,
            root.path(),
            Arc::clone(&probe),
            true,
            &settings(false, 3),
        );
        assert_eq!(handle.sync_memory().await.unwrap().status, "updated");
        let owner = AgentCx::for_current_or_request();
        {
            let mut session = OwnedMutexGuard::lock(handle.session_store(), owner.cx())
                .await
                .unwrap();
            completed(&mut session, "Branch B input", "Branch B completed");
            session.save().await.unwrap();
        }
        assert_eq!(handle.sync_memory().await.unwrap().status, "updated");
        {
            let mut session = OwnedMutexGuard::lock(handle.session_store(), owner.cx())
                .await
                .unwrap();
            assert!(session.navigate_to(&branch_a));
            session.save().await.unwrap();
        }
        assert_eq!(handle.sync_memory().await.unwrap().status, "unchanged");
        assert_eq!(probe.extraction_calls.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn old_omitted_secret_assignment_protects_recent_echo_and_saved_result() {
    run(async {
        let root = tempfile::tempdir().unwrap();
        let mut session = stored(root.path());
        let secret = "historicalOpaqueCredential987654321";
        completed(&mut session, &format!("api_key={secret}"), "Acknowledged.");
        for index in 0..MAX_MESSAGES {
            completed(
                &mut session,
                &format!("Unrelated task {index}"),
                "Completed unrelated task.",
            );
        }
        completed(
            &mut session,
            &format!("The bare value was {secret}"),
            "Finished.",
        );
        session.save().await.unwrap();
        let probe = Arc::new(Probe::default());
        probe.reply(
            StopReason::Stop,
            one_fact(&format!("The fixture value is {secret}"), None),
        );
        let handle = handle(
            session,
            root.path(),
            Arc::clone(&probe),
            true,
            &settings(false, 3),
        );
        let report = handle.sync_memory().await.unwrap();
        assert_eq!(report.source_messages, MAX_MESSAGES);
        let requests = probe.requests.lock().unwrap();
        assert!(!requests[0].0.contains(secret));
        assert!(!requests[0].0.contains("api_key="));
        assert!(requests[0].0.contains("The bare value was"));
        let facts = MemoryStore::open(root.path()).unwrap().list(10).unwrap();
        assert_eq!(facts.len(), 1);
        assert!(!facts[0].content.contains(secret));
    });
}

#[test]
fn sync_uses_current_session_attachment_and_refuses_ephemeral_sessions() {
    run(async {
        let root = tempfile::tempdir().unwrap();
        let attached = tempfile::tempdir().unwrap();
        let mut session = stored(root.path());
        completed(
            &mut session,
            "Record the project layout.",
            "The parser is in src/parser.rs.",
        );
        crate::session_workdir::attach_session_workdir(&mut session, attached.path()).unwrap();
        session.save().await.unwrap();
        let probe = Arc::new(Probe::default());
        let ephemeral = handle(
            session.clone(),
            root.path(),
            Arc::clone(&probe),
            false,
            &settings(true, 1),
        );
        assert!(ephemeral.sync_memory().await.is_err());
        assert_eq!(probe.extraction_calls.load(Ordering::SeqCst), 0);
        probe.reply(
            StopReason::Stop,
            one_fact("The parser is in src/parser.rs.", None),
        );
        let persistent = handle(
            session,
            root.path(),
            Arc::clone(&probe),
            true,
            &settings(false, 3),
        );
        assert_eq!(persistent.sync_memory().await.unwrap().retained, 1);
        assert!(
            MemoryStore::open(root.path())
                .unwrap()
                .list(10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            MemoryStore::open(attached.path())
                .unwrap()
                .list(10)
                .unwrap()
                .len(),
            1
        );
    });
}

fn bank_fixture(root: &std::path::Path) -> MemoryStore {
    MemoryStore {
        db_path: root.join("memory.sqlite"),
        project_key: "isolated-test".to_string(),
        project_root: root.to_path_buf(),
        secret_patterns: Vec::new(),
    }
}

fn source_fixture(root: &std::path::Path, version: &str) -> Source {
    Source {
        key: "session-source".to_string(),
        digest: version.to_string(),
        workdir: root.to_path_buf(),
        session_id: "session".to_string(),
        authored: vec![version.to_string()],
        messages: vec![json!({"text": version})],
    }
}

fn claimed(store: &MemoryStore, source: &Source, owner: &str) -> Vec<Existing> {
    let Admission::Run(existing) = claim(store, source, owner, false, 1).unwrap() else {
        panic!("fixture lease must be admitted");
    };
    existing
}

#[test]
fn incremental_consolidation_preserves_audited_history_and_exact_deduplication() {
    let root = tempfile::tempdir().unwrap();
    let store = bank_fixture(root.path());
    let first = source_fixture(root.path(), "one");
    let existing = claimed(&store, &first, "first");
    let lease = Lease {
        store: &store,
        owner: "first".to_string(),
    };
    let facts =
        parse_facts(serde_json::from_str(&one_fact("The parser uses format one.", None)).unwrap())
            .unwrap();
    assert_eq!(
        commit(&store, &first, "first", &existing, &facts, 1)
            .unwrap()
            .retained,
        1
    );
    drop(lease);
    let old = store.list(10).unwrap().remove(0);
    let second = source_fixture(root.path(), "two");
    let existing = claimed(&store, &second, "second");
    let _lease = Lease {
        store: &store,
        owner: "second".to_string(),
    };
    let facts = parse_facts(json!({"memories": [
        {"kind": "fact", "content": "The parser uses format two.", "supersedes": old.id},
        {"kind": "fact", "content": "The parser uses format two."}
    ]}))
    .unwrap();
    let report = commit(&store, &second, "second", &existing, &facts, 1).unwrap();
    assert_eq!(
        (report.retained, report.superseded, report.duplicates),
        (1, 1, 1)
    );
    let all = store.list(10).unwrap();
    assert_eq!(all.len(), 2);
    assert!(
        all.iter()
            .any(|memory| memory.id == old.id && memory.status == "superseded")
    );
    assert!(store.mental_model().unwrap().contains("format two"));
    assert!(!store.mental_model().unwrap().contains("format one"));
}

#[test]
fn manual_same_timestamp_edit_retires_generation_and_invalid_batch_rolls_back() {
    let root = tempfile::tempdir().unwrap();
    let store = bank_fixture(root.path());
    let first = source_fixture(root.path(), "one");
    let existing = claimed(&store, &first, "first");
    let lease = Lease {
        store: &store,
        owner: "first".to_string(),
    };
    let facts =
        parse_facts(serde_json::from_str(&one_fact("Original generated fact.", None)).unwrap())
            .unwrap();
    commit(&store, &first, "first", &existing, &facts, 1).unwrap();
    drop(lease);
    let id = store.list(10).unwrap()[0].id;
    store
        .with_write_conn(|conn| {
            conn.execute_sync(
                "UPDATE memories SET content = ?1 WHERE id = ?2",
                &[
                    sql_text("Human correction in the same millisecond."),
                    SqliteValue::Integer(id),
                ],
            )
            .map_err(|_| storage_error())?;
            Ok(())
        })
        .unwrap();
    let second = source_fixture(root.path(), "two");
    let existing = claimed(&store, &second, "second");
    let _lease = Lease {
        store: &store,
        owner: "second".to_string(),
    };
    assert!(!existing[0].can_supersede);
    let facts = parse_facts(json!({"memories": [
        {"kind": "fact", "content": "This earlier write must roll back."},
        {"kind": "fact", "content": "Must not overwrite the human correction.", "supersedes": id}
    ]}))
    .unwrap();
    assert!(commit(&store, &second, "second", &existing, &facts, 1).is_err());
    assert_eq!(store.list(10).unwrap().len(), 1);
    store
        .with_conn(|conn| {
            let rows = conn
                .query_sync(
                    "SELECT COUNT(*) FROM memory_extraction_checkpoint WHERE digest = 'two'",
                    &[],
                )
                .map_err(|_| storage_error())?;
            assert_eq!(row_i64(&rows[0], 0)?, 0);
            Ok(())
        })
        .unwrap();
}

#[test]
fn project_lease_excludes_duplicate_requests_and_releases_failed_owner() {
    let root = tempfile::tempdir().unwrap();
    let store = bank_fixture(root.path());
    let source = source_fixture(root.path(), "one");
    claimed(&store, &source, "first");
    let lease = Lease {
        store: &store,
        owner: "first".to_string(),
    };
    assert!(
        matches!(claim(&store, &source, "second", false, 1).unwrap(), Admission::Skip(report) if report.status == "busy")
    );
    drop(lease);
    claimed(&store, &source, "second");
}

#[test]
fn structured_output_and_config_bounds_are_strict() {
    for value in [
        json!({"memories": [], "instructions": "unexpected"}),
        json!({"memories": [{"kind": "fact", "content": "", "supersedes": null}]}),
        json!({"memories": [{"kind": "fact", "content": "x".repeat(2049)}]}),
        json!({"memories": [{"kind": "fact", "content": "A", "supersedes": 1}, {"kind": "fact", "content": "B", "supersedes": 1}]}),
    ] {
        assert!(parse_facts(value).is_err());
    }
    let settings: MemorySettings = serde_json::from_value(
        json!({"backend": "local", "autoExtract": true, "extractEveryTurns": 0}),
    )
    .unwrap();
    assert!(settings.enabled());
    assert_eq!(settings.auto_extract, Some(true));
    assert_eq!(settings.extraction_cadence(), 1);
    assert_eq!(MemorySettings::default().extraction_cadence(), 3);
    assert!(!MemorySettings::default().enabled());
}

#[test]
fn authored_marker_preserves_advisor_ancestry_and_stable_replay() {
    run(async {
        let root = tempfile::tempdir().unwrap();
        let mut session = stored(root.path());
        let input = session.append_model_message(Message::User(UserMessage {
            content: UserContent::Text("Authored queued input".to_string()),
            timestamp: now_ms(),
        }));
        let reset = crate::agent::append_advisor_input_reset(&mut session, &input).unwrap();
        let marker = record_authored_input(&mut session, &input).unwrap();
        assert_eq!(record_authored_input(&mut session, &input).unwrap(), marker);
        let ancestry = session.entries_for_current_path();
        assert!(
            ancestry
                .iter()
                .any(|entry| entry.base().id.as_deref() == Some(reset.as_str()))
        );
        assert_eq!(
            ancestry
                .iter()
                .filter(|entry| entry.base().id.as_deref() == Some(marker.as_str()))
                .count(),
            1
        );
        session.append_model_message(Message::assistant(AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("Completed."))],
            ..Default::default()
        }));
        session.save().await.unwrap();
        assert_eq!(capture(&session).unwrap().authored, vec![input]);
    });
}

#[test]
fn expired_budget_and_oversized_discovery_refuse_provider_admission() {
    run(async {
        let root = tempfile::tempdir().unwrap();
        let mut session = stored(root.path());
        completed(&mut session, "Completed source", "Verified.");
        session.save().await.unwrap();
        let probe = Arc::new(Probe::default());
        let handle = handle(
            session,
            root.path(),
            Arc::clone(&probe),
            true,
            &settings(false, 3),
        );
        assert!(
            synchronize(
                handle.session(),
                &settings(false, 3),
                false,
                None,
                Some(Duration::ZERO),
            )
            .await
            .is_err()
        );
        assert_eq!(probe.extraction_calls.load(Ordering::SeqCst), 0);
        let owner = AgentCx::for_current_or_request();
        {
            let mut session = OwnedMutexGuard::lock(handle.session_store(), owner.cx())
                .await
                .unwrap();
            completed(
                &mut session,
                &"x".repeat(MAX_DISCOVERY_BYTES / 2 + 1),
                "Complete oversized input.",
            );
            session.save().await.unwrap();
        }
        assert!(handle.sync_memory().await.is_err());
        assert_eq!(probe.extraction_calls.load(Ordering::SeqCst), 0);
        assert!(
            MemoryStore::open(root.path())
                .unwrap()
                .list(10)
                .unwrap()
                .is_empty()
        );
    });
}
