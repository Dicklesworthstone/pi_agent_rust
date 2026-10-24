//! Native goal runner probes: real session, agent, OpenAI wire adapter and
//! bounded loopback HTTP peer. The peer supplies responses; it does not replace
//! any production Provider, Tool, persistence or goal implementation.

use super::*;
use crate::agent::{Agent, AgentConfig, AgentSession};
use crate::compaction::ResolvedCompactionSettings;
use crate::goal::GoalBudget;
use crate::model::TextContent;
use crate::provider::StreamOptions;
use crate::providers::openai::OpenAIProvider;
use crate::sdk::EventListeners;
use crate::session::Session;
use crate::tools::ToolRegistry;
use asupersync::runtime::RuntimeBuilder;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap()
}

fn handle(url: &str, cwd: &std::path::Path) -> AgentSessionHandle {
    handle_with_store(url, cwd, Session::in_memory(), false)
}

fn handle_with_store(
    url: &str,
    cwd: &std::path::Path,
    store: Session,
    save: bool,
) -> AgentSessionHandle {
    let provider = Arc::new(OpenAIProvider::new("goal-fixture").with_base_url(url));
    let agent = Agent::new(
        provider,
        ToolRegistry::new(&[], cwd, None),
        AgentConfig {
            stream_options: StreamOptions {
                api_key: Some("local-goal-fixture".to_string()),
                max_tokens: Some(128),
                ..StreamOptions::default()
            },
            ..AgentConfig::default()
        },
    );
    let session = AgentSession::new(
        agent,
        Arc::new(asupersync::sync::Mutex::new(store)),
        save,
        ResolvedCompactionSettings::default(),
    );
    AgentSessionHandle::from_session_with_listeners(session, EventListeners::default())
}

struct Peer {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

fn request(stream: &TcpStream) -> Value {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut length = None;
    let mut total = 0;
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        total += line.len();
        assert!(total <= 64 * 1024);
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let length = length.expect("fixed request body");
    assert!(length <= 2 * 1024 * 1024);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

impl Peer {
    fn new(failures: Vec<bool>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let captured = Arc::clone(&requests);
        let stopping = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            for failure in failures {
                let until = Instant::now() + Duration::from_secs(15);
                let mut stream = loop {
                    if stopping.load(Ordering::SeqCst) {
                        return;
                    }
                    assert!(Instant::now() < until, "goal wire request deadline");
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("accept: {error}"),
                    }
                };
                lock(&captured).push(request(&stream));
                let (status, content_type, body) = if failure {
                    (
                        "400 Bad Request",
                        "application/json",
                        json!({"error":{"message":"goal fixture rejected the request"}})
                            .to_string(),
                    )
                } else {
                    let event = |delta: Value, reason: Value, usage: Value| {
                        format!(
                            "data: {}\n\n",
                            json!({
                                "id":"goal-response", "object":"chat.completion.chunk",
                                "created":0, "model":"goal-fixture",
                                "choices":[{"index":0,"delta":delta,"finish_reason":reason}],
                                "usage":usage
                            })
                        )
                    };
                    let start = event(
                        json!({"role":"assistant","content":"Goal complete!"}),
                        Value::Null,
                        Value::Null,
                    );
                    let end = event(
                        json!({}),
                        json!("stop"),
                        json!({"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}),
                    );
                    (
                        "200 OK",
                        "text/event-stream",
                        format!("{start}{end}data: [DONE]\n\n"),
                    )
                };
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
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
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() {
                result.expect("goal wire peer");
            }
        }
    }
}

fn complete() -> GoalDecision {
    GoalDecision::Complete {
        evidence: "host checked the requested deliverables".into(),
    }
}

fn continue_work() -> GoalDecision {
    GoalDecision::Continue {
        instruction: "Implement and verify the second step".into(),
    }
}

#[test]
fn two_native_prompts_preserve_history_and_require_host_completion() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![false, false]);
    let mut session = handle(&peer.url, temp.path());
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let created = session
            .create_goal(&owner, "Implement both steps", GoalBudget::default())
            .await
            .unwrap();
        let mut driver = session.into_goal_session();
        let mut verdicts = 0;
        let run = driver
            .run_goal(
                &owner,
                &created.goal,
                Duration::from_secs(30),
                |_, _| {},
                |_, _| {
                    verdicts += 1;
                    if verdicts == 1 {
                        continue_work()
                    } else {
                        complete()
                    }
                },
            )
            .unwrap();
        let control = run.control();
        let report = run.await;
        assert_eq!(
            verdicts, 2,
            "assistant prose alone must never complete the objective"
        );
        assert_eq!(report.stop, GoalRunStop::Complete);
        assert_eq!(report.goal.status(), GoalStatus::Complete);
        assert_eq!(report.goal.prompts_started(), 2);
        assert_eq!(report.goal.tokens_spent(), 30);
        assert!(matches!(report.completion, Some(Ok(_))));
        assert!(report.error.is_none());
        assert!(control.is_finished());
        assert!(!control.pause());
        assert!(control.steer("stale input").is_err());
        let current = driver.session_mut().current_goal().unwrap().unwrap();
        assert_eq!(current.status(), GoalStatus::Complete);
        assert!(!current.interrupted());
    });
    let requests = lock(&peer.requests);
    assert_eq!(requests.len(), 2);
    let users = |request: &Value| {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "user")
            .count()
    };
    assert_eq!(users(&requests[0]), 1);
    assert_eq!(users(&requests[1]), 2);
    assert!(requests[1].to_string().contains("second step"));
    drop(requests);
}

#[test]
fn prompt_and_token_limits_stop_without_fabricating_completion() {
    for budget in [
        GoalBudget {
            max_prompts: 1,
            token_budget: 1000,
        },
        GoalBudget {
            max_prompts: 4,
            token_budget: 15,
        },
    ] {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let peer = Peer::new(vec![false]);
        let mut session = handle(&peer.url, temp.path());
        runtime.block_on(async {
            let owner = AgentCx::for_current_or_request();
            let created = session
                .create_goal(&owner, "more work remains", budget)
                .await
                .unwrap();
            let mut driver = session.into_goal_session();
            let report = driver
                .run_goal(
                    &owner,
                    &created.goal,
                    Duration::from_secs(30),
                    |_, _| {},
                    |_, _| continue_work(),
                )
                .unwrap()
                .await;
            assert_eq!(report.stop, GoalRunStop::BudgetLimited);
            assert_eq!(report.goal.status(), GoalStatus::BudgetLimited);
            assert_eq!(report.goal.prompts_started(), 1);
            assert_eq!(report.goal.tokens_spent(), 15);
            assert!(
                driver
                    .session_mut()
                    .resume_goal(&owner, &report.goal)
                    .await
                    .is_err()
            );
        });
        assert_eq!(lock(&peer.requests).len(), 1);
    }
}

#[test]
fn native_failure_pauses_without_a_new_driver_retry_or_completion_verdict() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![true]);
    let mut session = handle(&peer.url, temp.path());
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let created = session
            .create_goal(&owner, "do work", GoalBudget::default())
            .await
            .unwrap();
        let mut driver = session.into_goal_session();
        let report = driver
            .run_goal(
                &owner,
                &created.goal,
                Duration::from_secs(30),
                |_, _| {},
                |_, _| panic!("failure cannot authorize verification"),
            )
            .unwrap()
            .await;
        assert_eq!(report.stop, GoalRunStop::NativeFailure);
        assert_eq!(report.goal.status(), GoalStatus::Paused);
        assert_eq!(report.goal.prompts_started(), 1);
        assert!(!report.goal.interrupted());
        match report.completion.as_ref().unwrap() {
            Err(TurnDeadlineError::Turn(_)) => {}
            Ok(message) => assert!(!successful(message)),
            other => panic!("unexpected native outcome: {other:?}"),
        }
    });
    assert_eq!(lock(&peer.requests).len(), 1);
}

#[test]
fn callback_pause_wins_late_complete_and_old_control_cannot_stop_resumed_run() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![false, false]);
    let mut session = handle(&peer.url, temp.path());
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let created = session
            .create_goal(&owner, "review each step", GoalBudget::default())
            .await
            .unwrap();
        let mut driver = session.into_goal_session();
        let slot = Arc::new(Mutex::new(None::<GoalControl>));
        let callback_slot = Arc::clone(&slot);
        let run = driver
            .run_goal(
                &owner,
                &created.goal,
                Duration::from_secs(30),
                |_, _| {},
                move |_, _| {
                    assert!(lock(&callback_slot).as_ref().unwrap().pause());
                    complete()
                },
            )
            .unwrap();
        let old = run.control();
        *lock(&slot) = Some(old.clone());
        let paused = run.await;
        assert_eq!(paused.stop, GoalRunStop::Paused);
        assert_eq!(paused.goal.status(), GoalStatus::Paused);
        let resumed = driver
            .session_mut()
            .resume_goal(&owner, &paused.goal)
            .await
            .unwrap();
        let run = driver
            .run_goal(
                &owner,
                &resumed.goal,
                Duration::from_secs(30),
                |_, _| {},
                |_, _| complete(),
            )
            .unwrap();
        assert!(!old.pause());
        assert!(old.follow_up("stale").is_err());
        let report = run.await;
        assert_eq!(report.stop, GoalRunStop::Complete);
        assert_eq!(report.goal.prompts_started(), 2);
        assert_eq!(report.goal.tokens_spent(), 30);
    });
    assert_eq!(lock(&peer.requests).len(), 2);
}

#[test]
fn unused_drop_and_prepaused_run_dispatch_nothing_and_leave_no_intent() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut session = handle("http://127.0.0.1:1/v1", temp.path());
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let created = session
            .create_goal(&owner, "unused", GoalBudget::default())
            .await
            .unwrap();
        let mut driver = session.into_goal_session();
        let run = driver
            .run_goal(
                &owner,
                &created.goal,
                Duration::from_secs(30),
                |_, _| panic!("unused"),
                |_, _| panic!("unused"),
            )
            .unwrap();
        let control = run.control();
        drop(run);
        assert!(control.is_finished());
        assert!(!control.pause());
        let run = driver
            .run_goal(
                &owner,
                &created.goal,
                Duration::from_secs(30),
                |_, _| panic!("paused"),
                |_, _| panic!("paused"),
            )
            .unwrap();
        assert!(run.control().pause());
        let report = run.await;
        assert_eq!(report.stop, GoalRunStop::Paused);
        assert!(report.completion.is_none());
        let goal = driver.session_mut().current_goal().unwrap().unwrap();
        assert_eq!(goal.checkpoint_id, created.goal.checkpoint_id);
        assert_eq!(goal.prompts_started(), 0);
        assert!(!goal.interrupted());
    });
}

#[test]
fn driver_guard_preserves_unclaimed_native_input_when_run_is_dropped() {
    let temp = tempfile::tempdir().unwrap();
    let mut session = handle("http://127.0.0.1:1/v1", temp.path()).into_controllable();
    let turn = session.prompt("never polled".into(), |_| {}).unwrap();
    let control = GoalControl::new();
    control.install(&turn.control());
    let guard = RunGuard(control.clone());
    control.follow_up("recover this private input").unwrap();
    drop(turn);
    drop(guard);
    assert!(control.is_finished());
    let pending = control.take_pending();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].text, "recover this private input");
    assert!(control.take_pending().is_empty());
}

#[test]
fn invalid_host_verdict_pauses_instead_of_looping_or_completing() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![false]);
    let mut session = handle(&peer.url, temp.path());
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let created = session
            .create_goal(&owner, "test invalid verdict", GoalBudget::default())
            .await
            .unwrap();
        let mut driver = session.into_goal_session();
        let report = driver
            .run_goal(
                &owner,
                &created.goal,
                Duration::from_secs(30),
                |_, _| {},
                |_, _| GoalDecision::Complete {
                    evidence: " ".into(),
                },
            )
            .unwrap()
            .await;
        assert_eq!(report.stop, GoalRunStop::Failed);
        assert_eq!(report.goal.status(), GoalStatus::Paused);
        assert!(
            report
                .error
                .as_ref()
                .unwrap()
                .to_string()
                .contains("GOAL_TEXT")
        );
        assert!(report.completion.as_ref().unwrap().is_ok());
    });
    assert_eq!(lock(&peer.requests).len(), 1);
}

#[test]
fn stale_goal_snapshot_is_refused_before_native_execution() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut session = handle("http://127.0.0.1:1/v1", temp.path());
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let created = session
            .create_goal(&owner, "stale", GoalBudget::default())
            .await
            .unwrap();
        let _ = session
            .pause_goal(&owner, &created.goal, "host paused")
            .await
            .unwrap();
        let mut driver = session.into_goal_session();
        let report = driver
            .run_goal(
                &owner,
                &created.goal,
                Duration::from_secs(30),
                |_, _| panic!("stale dispatch"),
                |_, _| panic!("stale verdict"),
            )
            .unwrap()
            .await;
        assert_eq!(report.stop, GoalRunStop::Failed);
        assert!(report.error.unwrap().to_string().contains("GOAL_STALE"));
        assert!(report.completion.is_none());
        assert_eq!(
            driver
                .session_mut()
                .current_goal()
                .unwrap()
                .unwrap()
                .prompts_started(),
            0
        );
    });
}

#[test]
fn intent_is_visible_before_dispatch_and_cannot_be_replayed_without_resume() {
    let mut state = Record::new("work", GoalBudget::default()).unwrap();
    state.tokens_spent = 44;
    let state = prepare_intent(state).unwrap();
    assert_eq!(state.effective_status(), GoalStatus::Paused);
    assert!(state.in_flight);
    assert_eq!(state.prompts_started, 1);
    assert_eq!(state.tokens_spent, 44);
    assert!(prepare_intent(state).is_err());
}

#[test]
fn settlement_never_turns_a_budget_boundary_into_automatic_completion() {
    let budget = GoalBudget {
        max_prompts: 1,
        token_budget: 10,
    };
    let state = prepare_intent(Record::new("work", budget).unwrap()).unwrap();
    let limited = settle_record(state.clone(), continue_work()).unwrap();
    assert_eq!(limited.effective_status(), GoalStatus::BudgetLimited);
    assert!(!limited.in_flight);
    let complete = settle_record(state, complete()).unwrap();
    assert_eq!(
        complete.effective_status(),
        GoalStatus::Complete,
        "explicit host evidence may complete at the limit"
    );
}

#[test]
fn only_a_clean_final_assistant_answer_reaches_host_verification() {
    let message = AssistantMessage {
        content: vec![ContentBlock::Text(TextContent::new("answer"))],
        ..AssistantMessage::default()
    };
    assert!(successful(&message));
    for reason in [
        StopReason::Length,
        StopReason::ToolUse,
        StopReason::Error,
        StopReason::Aborted,
        StopReason::Refusal,
        StopReason::PauseTurn,
    ] {
        let mut bad = message.clone();
        bad.stop_reason = reason;
        assert!(!successful(&bad));
    }
    let mut bad = message.clone();
    bad.error_message = Some("failed".into());
    assert!(!successful(&bad));
    let mut bad = message;
    bad.content.clear();
    assert!(!successful(&bad));
}

#[test]
fn run_control_without_a_live_prompt_rejects_input_and_pause_is_idempotent() {
    let control = GoalControl::new();
    assert!(control.steer("idle").is_err());
    assert!(control.follow_up("idle").is_err());
    assert!(control.pause());
    assert!(!control.pause());
    control.finish();
    assert!(control.is_finished());
    assert!(!control.pause());
}

#[test]
fn pause_at_the_commit_boundary_cannot_be_overwritten_by_a_prepared_verdict() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut session = handle("http://127.0.0.1:1/v1", temp.path());
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let created = session
            .create_goal(&owner, "pause race", GoalBudget::default())
            .await
            .unwrap();
        let intent = start_attempt(&mut session, &owner, &created.goal)
            .await
            .unwrap();
        let control = GoalControl::new();
        let deadline = TurnDeadline::after(&owner, Duration::from_secs(30)).unwrap();
        assert!(control.pause());
        let (settled, late_stop) = settle_attempt(
            &mut session,
            &owner,
            &intent.goal,
            complete(),
            &deadline,
            &control,
        )
        .await
        .unwrap();
        assert_eq!(late_stop, Some(GoalRunStop::Paused));
        assert_eq!(settled.goal.status(), GoalStatus::Paused);
        assert!(!settled.goal.interrupted());
        assert_eq!(settled.goal.prompts_started(), 1);
    });
}

#[test]
fn unconfirmed_intent_save_stops_before_native_work() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut store = Session::in_memory();
    store.path = Some(temp.path().to_path_buf()); // Directory, never a writable session file.
    store.session_dir = Some(temp.path().to_path_buf());
    let mut session = handle_with_store("http://127.0.0.1:1/v1", temp.path(), store, true);
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let created = session
            .create_goal(&owner, "save must succeed first", GoalBudget::default())
            .await
            .unwrap();
        assert!(matches!(
            created.persistence,
            GoalPersistence::Unconfirmed { .. }
        ));
        let mut driver = session.into_goal_session();
        let report = driver
            .run_goal(
                &owner,
                &created.goal,
                Duration::from_secs(30),
                |_, _| panic!("unsaved dispatch"),
                |_, _| panic!("unsaved verdict"),
            )
            .unwrap()
            .await;
        assert_eq!(report.stop, GoalRunStop::PersistenceUnconfirmed);
        assert!(report.completion.is_none());
        assert!(report.goal.interrupted());
        assert_eq!(report.goal.prompts_started(), 1);
        assert!(temp.path().is_dir());
    });
}

#[test]
fn native_session_reopen_preserves_interrupted_goal_and_requires_fresh_resume() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("goal.jsonl");
    let mut store = Session::in_memory();
    store.path = Some(path.clone());
    store.session_dir = Some(temp.path().to_path_buf());
    let mut session = handle_with_store("http://127.0.0.1:1/v1", temp.path(), store, true);
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let created = session
            .create_goal(&owner, "durable objective", GoalBudget::default())
            .await
            .unwrap();
        assert_eq!(created.persistence, GoalPersistence::Saved);
        let intent = start_attempt(&mut session, &owner, &created.goal)
            .await
            .unwrap();
        assert_eq!(intent.persistence, GoalPersistence::Saved);
        let loaded = Session::open(path.to_str().unwrap()).await.unwrap();
        let mut reopened = handle_with_store("http://127.0.0.1:1/v1", temp.path(), loaded, true);
        let current = reopened.current_goal().unwrap().unwrap();
        assert!(current.interrupted());
        assert_eq!(current.status(), GoalStatus::Paused);
        assert_eq!(current.prompts_started(), 1);
        assert_eq!(current.objective(), "durable objective");
        assert!(reopened.resume_goal(&owner, &intent.goal).await.is_err());
        let resumed = reopened.resume_goal(&owner, &current).await.unwrap();
        assert_eq!(resumed.persistence, GoalPersistence::Saved);
        assert_eq!(resumed.goal.status(), GoalStatus::Active);
        assert_eq!(resumed.goal.prompts_started(), 1);
        assert!(!resumed.goal.interrupted());
    });
}
