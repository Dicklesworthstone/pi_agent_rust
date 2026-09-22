//! Contract tests through actual SDK sessions, native provider construction,
//! SubmitPlanTool, approval policy and temporary on-disk sessions. Control
//! operations must not start a provider turn; the adapter is never mocked.

use super::*;
use crate::approval::{ApprovalMode, ApprovalState};
use crate::tools::{Tool, ToolEffects};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

const PLAN: &str = "Goal: update code. Steps: edit the selected source, then verify it.";

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
        .build()
        .unwrap()
        .block_on(future)
}

fn fixture(stored: Session, save: bool) -> (AgentSessionHandle, Arc<AtomicUsize>) {
    let provider = Arc::new(
        crate::providers::openai::OpenAIProvider::new("plan-control-fixture")
            .with_base_url("http://127.0.0.1:1/v1"),
    );
    let policy = ApprovalState::new(ApprovalMode::AlwaysAsk, true, Vec::new());
    let agent = crate::agent::Agent::new(
        provider,
        crate::tools::ToolRegistry::new(&[], std::path::Path::new("."), None),
        crate::agent::AgentConfig {
            system_prompt: Some("original instructions".to_string()),
            approval_state: Some(policy),
            ..crate::agent::AgentConfig::default()
        },
    );
    let session = crate::agent::AgentSession::new(
        agent,
        Arc::new(Store::new(stored)),
        save,
        crate::compaction::ResolvedCompactionSettings::default(),
    );
    let events = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&events);
    let listeners = crate::sdk::EventListeners::default();
    let _ = listeners.subscribe(Arc::new(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    (
        AgentSessionHandle::from_session_with_listeners(session, listeners),
        events,
    )
}

fn submit(handle: &AgentSessionHandle) -> SessionPlanReview {
    let state = handle.session().agent.plan_state();
    let tool = crate::plan::SubmitPlanTool::new(state, false);
    let result = run(tool.execute(
        "proposal",
        json!({"plan": PLAN, "files": ["src/hello world.rs"]}),
        None,
    ))
    .unwrap();
    assert!(!result.is_error);
    handle.pending_plan_review().unwrap().unwrap()
}

fn modes(handle: &AgentSessionHandle) -> Vec<String> {
    let store = handle.session_store();
    let session = store.try_lock().unwrap();
    session
        .entries
        .iter()
        .filter_map(|entry| {
            let value = serde_json::to_value(entry).unwrap();
            (value["customType"] == "plan_mode")
                .then(|| value["data"]["mode"].as_str().unwrap().to_string())
        })
        .collect()
}

fn pending() -> (AgentSessionHandle, AgentCx, SessionPlanReview) {
    let (mut handle, _) = fixture(Session::in_memory(), false);
    let owner = AgentCx::for_request();
    let entered = run(handle.enter_plan_mode(&owner)).unwrap();
    assert_eq!(entered.persistence, PlanPersistence::MemoryOnly);
    let review = submit(&handle);
    (handle, owner, review)
}

#[test]
fn sdk_workflow_pins_the_reviewed_scope_and_preserves_independent_tool_policy() {
    let (mut handle, events) = fixture(Session::in_memory(), false);
    let owner = AgentCx::for_request();
    let state = handle.session().agent.plan_state();
    let _ = run(handle.enter_plan_mode(&owner)).unwrap();
    assert!(!state.allows_effects(ToolEffects::write()));
    assert!(!state.allows_effects(ToolEffects::process()));
    let review = submit(&handle);
    assert!(review.text().contains("Files: [\"src/hello world.rs\"]"));
    let again = handle.pending_plan_review().unwrap().unwrap();
    assert!(review.same_submission(&again));
    let change = run(handle.approve_plan_review(&owner, &review)).unwrap();
    assert!(change.changed);
    assert_eq!(change.mode, PlanMode::Approved);
    assert_eq!(change.persistence, PlanPersistence::MemoryOnly);
    assert!(handle.pending_plan_review().unwrap().is_none());
    assert!(
        handle
            .session()
            .agent
            .system_prompt()
            .unwrap()
            .contains(review.text())
    );
    assert!(state.allows_effects(ToolEffects::write()));
    let policy = handle.session().agent.approval_state().unwrap();
    assert_eq!(policy.mode(), ApprovalMode::AlwaysAsk);
    for (path, allowed) in [("src/hello world.rs", true), ("src/hello", false)] {
        let verdict = policy.evaluate(
            "write",
            &json!({"path": path}),
            ToolEffects::write(),
            Some(&state),
            None,
        );
        assert_eq!(verdict.is_auto_approved(), allowed);
    }
    let change = run(handle.exit_plan_mode(&owner)).unwrap();
    assert_eq!(change.mode, PlanMode::Off);
    assert_eq!(
        handle.session().agent.system_prompt(),
        Some("original instructions")
    );
    assert!(state.plan().is_none());
    assert_eq!(modes(&handle), ["planning", "approved", "off"]);
    assert_eq!(
        events.load(Ordering::SeqCst),
        0,
        "controls never start a turn"
    );
}

#[test]
fn repeated_enter_and_exit_are_noops_but_pending_enter_is_refused() {
    let (mut handle, _) = fixture(Session::in_memory(), false);
    let owner = AgentCx::for_request();
    assert!(!run(handle.exit_plan_mode(&owner)).unwrap().changed);
    let _ = run(handle.enter_plan_mode(&owner)).unwrap();
    let second = run(handle.enter_plan_mode(&owner)).unwrap();
    assert!(!second.changed);
    assert_eq!(second.persistence, PlanPersistence::Unchanged);
    let review = submit(&handle);
    assert!(run(handle.enter_plan_mode(&owner)).is_err());
    assert!(review.same_submission(&handle.pending_plan_review().unwrap().unwrap()));
    assert_eq!(modes(&handle), ["planning"]);
}

#[test]
fn identical_resubmissions_invalidate_both_kinds_of_decision() {
    let (mut handle, owner, old) = pending();
    let rejected = run(handle.reject_plan_review(&owner, &old)).unwrap();
    assert_eq!(rejected.mode, PlanMode::Planning);
    let current = submit(&handle);
    assert_eq!(old.text(), current.text());
    assert!(!old.same_submission(&current));
    assert!(run(handle.approve_plan_review(&owner, &old)).is_err());
    assert!(run(handle.reject_plan_review(&owner, &old)).is_err());
    assert_eq!(
        handle.session().agent.plan_state().mode(),
        PlanMode::PendingApproval
    );
    assert_eq!(
        handle.session().agent.system_prompt(),
        Some("original instructions")
    );
    let _ = run(handle.approve_plan_review(&owner, &current)).unwrap();
    assert!(run(handle.approve_plan_review(&owner, &current)).is_err());
    assert_eq!(modes(&handle), ["planning", "rejected", "approved"]);
}

#[test]
fn an_external_state_change_cannot_retarget_the_saved_review() {
    let (mut handle, owner, old) = pending();
    let state = handle.session().agent.plan_state();
    assert!(state.reject());
    assert!(state.submit_plan(old.text().to_string()));
    let new = handle.pending_plan_review().unwrap().unwrap();
    assert!(!old.same_submission(&new));
    assert!(run(handle.approve_plan_review(&owner, &old)).is_err());
    assert!(run(handle.reject_plan_review(&owner, &old)).is_err());
    assert_eq!(modes(&handle), ["planning"]);
}

#[test]
fn foreign_review_with_same_id_and_text_cannot_control_a_new_store() {
    let (mut first, owner, old) = pending();
    let id = first.session_store().try_lock().unwrap().header.id.clone();
    let mut stored = Session::in_memory();
    stored.header.id = id;
    let (mut other, _) = fixture(stored, false);
    let _ = run(other.enter_plan_mode(&owner)).unwrap();
    let new = submit(&other);
    assert_eq!(old.text(), new.text());
    assert!(!old.same_submission(&new));
    assert!(run(other.approve_plan_review(&owner, &old)).is_err());
    assert!(run(other.reject_plan_review(&owner, &old)).is_err());
    let _ = run(first.approve_plan_review(&owner, &old)).unwrap();
    assert_eq!(
        other.session().agent.system_prompt(),
        Some("original instructions")
    );
    assert_eq!(
        other.session().agent.plan_state().mode(),
        PlanMode::PendingApproval
    );
}

#[test]
fn identity_changes_in_the_same_store_also_invalidate_a_review() {
    let (mut handle, owner, review) = pending();
    handle.session_store().try_lock().unwrap().header.id = "replacement".to_string();
    assert!(run(handle.approve_plan_review(&owner, &review)).is_err());
    assert!(run(handle.reject_plan_review(&owner, &review)).is_err());
    assert_eq!(modes(&handle), ["planning"]);
}

#[test]
fn review_does_not_keep_a_session_alive_or_disclose_text_in_debug() {
    let (handle, _, review) = pending();
    let again = review.clone();
    assert!(review.same_submission(&again));
    assert!(!format!("{review:?}").contains(PLAN));
    let weak = Arc::downgrade(&handle.session_store());
    drop(handle);
    assert!(weak.upgrade().is_none());
    assert!(review.text().contains(PLAN));
}

#[test]
fn pin_cleanup_restores_absent_and_empty_prompts_distinctly() {
    for original in [None, Some(String::new()), Some("base".to_string())] {
        let (mut handle, owner, review) = pending();
        handle
            .session_mut()
            .agent
            .set_system_prompt(original.clone());
        let _ = run(handle.approve_plan_review(&owner, &review)).unwrap();
        let _ = run(handle.exit_plan_mode(&owner)).unwrap();
        assert_eq!(handle.session().agent.system_prompt(), original.as_deref());
    }
}

#[test]
fn unique_pin_removal_preserves_unrelated_prefix_suffix_and_base_changes() {
    let (mut handle, owner, review) = pending();
    let _ = run(handle.approve_plan_review(&owner, &review)).unwrap();
    let state = handle.session().agent.plan_state();
    let block = state
        .inner
        .read()
        .unwrap()
        .session_pin
        .as_ref()
        .unwrap()
        .block
        .clone();
    handle
        .session_mut()
        .agent
        .set_system_prompt(Some(format!("new prefix{block}new tail")));
    let _ = run(handle.exit_plan_mode(&owner)).unwrap();
    assert_eq!(
        handle.session().agent.system_prompt(),
        Some("new prefixnew tail")
    );
}

#[test]
fn missing_rewritten_or_duplicated_pin_never_overwrites_the_prompt() {
    for variant in 0..3 {
        let (mut handle, owner, review) = pending();
        let _ = run(handle.approve_plan_review(&owner, &review)).unwrap();
        let state = handle.session().agent.plan_state();
        let block = state
            .inner
            .read()
            .unwrap()
            .session_pin
            .as_ref()
            .unwrap()
            .block
            .clone();
        let modified = match variant {
            0 => String::from("replacement prompt"),
            1 => block.replace("Approved Plan", "Rewritten Plan"),
            _ => format!("{block}{block}"),
        };
        handle
            .session_mut()
            .agent
            .set_system_prompt(Some(modified.clone()));
        assert!(run(handle.exit_plan_mode(&owner)).is_err());
        assert!(run(handle.enter_plan_mode(&owner)).is_err());
        assert_eq!(
            handle.session().agent.system_prompt(),
            Some(modified.as_str())
        );
        assert_eq!(state.mode(), PlanMode::Approved);
        assert_eq!(modes(&handle), ["planning", "approved"]);
    }
}

#[test]
fn reentering_planning_removes_the_previous_pin_and_closes_the_gate() {
    let (mut handle, owner, review) = pending();
    let _ = run(handle.approve_plan_review(&owner, &review)).unwrap();
    let next = run(handle.enter_plan_mode(&owner)).unwrap();
    assert_eq!(next.mode, PlanMode::Planning);
    assert_eq!(
        handle.session().agent.system_prompt(),
        Some("original instructions")
    );
    assert!(
        !handle
            .session()
            .agent
            .plan_state()
            .allows_effects(ToolEffects::write())
    );
    let new = submit(&handle);
    assert!(!new.same_submission(&review));
    let _ = run(handle.approve_plan_review(&owner, &new)).unwrap();
    assert_eq!(
        handle
            .session()
            .agent
            .system_prompt()
            .unwrap()
            .matches(PLAN)
            .count(),
        1
    );
}

#[test]
fn raw_gate_exit_does_not_orphan_the_sdk_owned_context() {
    let (mut handle, owner, review) = pending();
    let _ = run(handle.approve_plan_review(&owner, &review)).unwrap();
    handle.session().agent.plan_state().exit();
    let cleanup = run(handle.exit_plan_mode(&owner)).unwrap();
    assert!(cleanup.changed);
    assert_eq!(
        handle.session().agent.system_prompt(),
        Some("original instructions")
    );
}

#[test]
fn raw_reentry_cannot_stack_new_approval_on_an_earlier_owned_pin() {
    let (mut handle, owner, review) = pending();
    let _ = run(handle.approve_plan_review(&owner, &review)).unwrap();
    let state = handle.session().agent.plan_state();
    state.enter_planning();
    let new = submit(&handle);
    assert!(run(handle.approve_plan_review(&owner, &new)).is_err());
    assert_eq!(state.mode(), PlanMode::PendingApproval);
    let _ = run(handle.exit_plan_mode(&owner)).unwrap();
    assert_eq!(
        handle.session().agent.system_prompt(),
        Some("original instructions")
    );
}

#[test]
fn raw_reentry_then_sdk_enter_cleans_the_pin_even_when_already_planning() {
    let (mut handle, owner, review) = pending();
    let _ = run(handle.approve_plan_review(&owner, &review)).unwrap();
    handle.session().agent.plan_state().enter_planning();
    let change = run(handle.enter_plan_mode(&owner)).unwrap();
    assert!(change.changed);
    assert_eq!(
        handle.session().agent.system_prompt(),
        Some("original instructions")
    );
}

#[test]
fn a_busy_store_never_blocks_or_partially_applies_a_control() {
    let (mut handle, owner, review) = pending();
    let store = handle.session_store();
    let held = store.try_lock().unwrap();
    assert!(handle.pending_plan_review().is_err());
    assert!(run(handle.approve_plan_review(&owner, &review)).is_err());
    assert!(run(handle.reject_plan_review(&owner, &review)).is_err());
    assert!(run(handle.exit_plan_mode(&owner)).is_err());
    assert_eq!(
        handle.session().agent.plan_state().mode(),
        PlanMode::PendingApproval
    );
    drop(held);
    assert_eq!(modes(&handle), ["planning"]);
    let _ = run(handle.approve_plan_review(&owner, &review)).unwrap();
}

#[test]
fn busy_or_poisoned_plan_lock_returns_an_error_without_mutation() {
    let (mut handle, owner, review) = pending();
    let state = handle.session().agent.plan_state();
    let held = state.inner.write().unwrap();
    assert!(handle.pending_plan_review().is_err());
    assert!(run(handle.approve_plan_review(&owner, &review)).is_err());
    drop(held);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = state.inner.write().unwrap();
        panic!("poison plan state");
    }));
    assert!(run(handle.exit_plan_mode(&owner)).is_err());
    assert_eq!(
        handle.session().agent.system_prompt(),
        Some("original instructions")
    );
    assert_eq!(modes(&handle), ["planning"]);
}

#[test]
fn no_session_changes_never_attempt_to_save() {
    let temp = tempfile::tempdir().unwrap();
    let mut stored = Session::in_memory();
    stored.path = Some(temp.path().to_path_buf());
    let (mut handle, _) = fixture(stored, false);
    let change = run(handle.enter_plan_mode(&AgentCx::for_request())).unwrap();
    assert_eq!(change.persistence, PlanPersistence::MemoryOnly);
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[test]
fn a_failed_save_is_a_committed_live_change_not_an_error_or_rollback() {
    let temp = tempfile::tempdir().unwrap();
    let blocker = temp.path().join("not-a-directory");
    std::fs::write(&blocker, b"sentinel").unwrap();
    let mut stored = Session::in_memory();
    stored.path = Some(blocker.join("session.jsonl"));
    let (mut handle, _) = fixture(stored, true);
    let owner = AgentCx::for_request();
    let entered = run(handle.enter_plan_mode(&owner)).unwrap();
    assert!(matches!(
        entered.persistence,
        PlanPersistence::Unconfirmed { .. }
    ));
    let review = submit(&handle);
    let approved = run(handle.approve_plan_review(&owner, &review)).unwrap();
    assert!(matches!(
        approved.persistence,
        PlanPersistence::Unconfirmed { .. }
    ));
    assert_eq!(
        handle.session().agent.plan_state().mode(),
        PlanMode::Approved
    );
    assert!(
        handle
            .session()
            .agent
            .system_prompt()
            .unwrap()
            .contains(review.text())
    );
    assert!(run(handle.approve_plan_review(&owner, &review)).is_err());
    assert_eq!(modes(&handle), ["planning", "approved"]);
    assert_eq!(std::fs::read(&blocker).unwrap(), b"sentinel");
}

#[test]
fn successful_save_contains_the_existing_transition_journal_shape() {
    let temp = tempfile::Builder::new()
        .prefix("pi-plan-save-")
        .tempdir_in("/tmp")
        .unwrap_or_else(|_| tempfile::tempdir().unwrap());
    let path = temp.path().join("session.jsonl");
    let mut stored = Session::in_memory();
    stored.path = Some(path.clone());
    let (mut handle, _) = fixture(stored, true);
    let change = run(handle.enter_plan_mode(&AgentCx::for_request())).unwrap();
    assert_eq!(change.persistence, PlanPersistence::Saved);
    let lines = std::fs::read_to_string(&path).unwrap();
    let rows: Vec<Value> = lines
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        rows.iter()
            .any(|row| { row["customType"] == "plan_mode" && row["data"]["mode"] == "planning" })
    );
}

#[test]
fn cancelled_controls_and_unpolled_controls_leave_no_transition() {
    let (mut handle, _, review) = pending();
    let owner = AgentCx::for_request();
    drop(handle.approve_plan_review(&owner, &review));
    assert_eq!(modes(&handle), ["planning"]);
    owner.cancel_with(asupersync::types::CancelKind::User, Some("test"));
    assert!(run(handle.approve_plan_review(&owner, &review)).is_err());
    assert!(run(handle.reject_plan_review(&owner, &review)).is_err());
    assert!(run(handle.exit_plan_mode(&owner)).is_err());
    assert_eq!(
        handle.session().agent.plan_state().mode(),
        PlanMode::PendingApproval
    );
    assert_eq!(modes(&handle), ["planning"]);
}

#[test]
fn persistent_transition_cannot_borrow_the_polling_callers_authority() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("session.jsonl");
    let mut stored = Session::in_memory();
    stored.path = Some(path.clone());
    let (mut handle, _) = fixture(stored, true);
    let owner = {
        let _guard = asupersync::Cx::for_request()
            .restrict::<asupersync::cx::cap::None>()
            .set_current_restricted();
        AgentCx::for_current_or_request()
    };
    let failure = run(handle.enter_plan_mode(&owner)).unwrap_err();
    assert!(failure.to_string().contains("PLAN_CAPABILITY"));
    assert_eq!(handle.session().agent.plan_state().mode(), PlanMode::Off);
    assert!(modes(&handle).is_empty());
    assert!(!path.exists());
}

#[test]
fn cancellation_before_save_reports_unconfirmed_not_saved() {
    let owner = AgentCx::for_request();
    let mut session = Session::in_memory();
    session.append_custom_entry("plan_mode".to_string(), Some(json!({"mode":"approved"})));
    owner.cancel_with(
        asupersync::types::CancelKind::User,
        Some("after live transition"),
    );
    assert!(matches!(
        run(persist(&mut session, &owner, true)),
        PlanPersistence::Unconfirmed { .. }
    ));
    assert_eq!(session.entries.len(), 1);
}

#[test]
fn control_futures_are_send_without_holding_std_guards_across_await() {
    fn assert_send<T: Send>(_: T) {}
    let (mut handle, owner, review) = pending();
    assert_send(handle.enter_plan_mode(&owner));
    assert_send(handle.approve_plan_review(&owner, &review));
    assert_send(handle.reject_plan_review(&owner, &review));
    assert_send(handle.exit_plan_mode(&owner));
}

#[test]
fn existing_controllable_sessions_can_use_the_same_idle_plan_api() {
    let (handle, _) = fixture(Session::in_memory(), false);
    let mut controlled = handle.into_controllable();
    let owner = AgentCx::for_request();
    let _ = run(controlled.session_mut().enter_plan_mode(&owner)).unwrap();
    let review = submit(controlled.session_mut());
    let _ = run(controlled
        .session_mut()
        .approve_plan_review(&owner, &review))
    .unwrap();
    let _ = run(controlled.session_mut().exit_plan_mode(&owner)).unwrap();
}

// A finite wire peer, not a replacement Provider or Tool implementation. The
// production OpenAI adapter, SDK prompt wrapper, agent loop and write tool all
// run. Warmup primes the tool schema cache before planning installs its helper.
mod wire {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    struct Peer {
        url: String,
        requests: Arc<Mutex<Vec<Value>>>,
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    fn read_request(stream: &TcpStream) -> Value {
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut length = None;
        let mut header_bytes = 0;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            header_bytes += line.len();
            assert!(header_bytes <= 64 * 1024);
            if line == "\r\n" {
                break;
            }
            if let Some((key, value)) = line.split_once(':')
                && key.eq_ignore_ascii_case("content-length")
            {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let length = length.expect("fixed JSON request body");
        assert!(length <= 2 * 1024 * 1024);
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn frame(delta: &Value, finish: &Value) -> String {
        format!(
            "data: {}\n\n",
            json!({
                "id":"plan-fixture", "object":"chat.completion.chunk",
                "created":0, "model":"plan-fixture",
                "choices":[{"index":0, "delta":delta, "finish_reason":finish}]
            })
        )
    }

    fn response(index: usize) -> String {
        let (delta, reason) = match index {
            1 | 4 => (
                json!({"role":"assistant", "tool_calls":[{
                    "index":0, "id":format!("write-{index}"), "type":"function",
                    "function":{"name":"write", "arguments":json!({
                        "path":"result.txt", "content":"approved mutation"
                    }).to_string()}
                }]}),
                "tool_calls",
            ),
            2 => (
                json!({"role":"assistant", "tool_calls":[{
                    "index":0, "id":"submit", "type":"function",
                    "function":{"name":"submit_plan", "arguments":json!({
                        "plan":PLAN, "files":["result.txt"]
                    }).to_string()}
                }]}),
                "tool_calls",
            ),
            _ => (json!({"role":"assistant", "content":"complete"}), "stop"),
        };
        format!(
            "{}{}data: [DONE]\n\n",
            frame(&delta, &Value::Null),
            frame(&json!({}), &json!(reason))
        )
    }

    impl Peer {
        fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}/v1", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&requests);
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = Arc::clone(&stop);
            let worker = std::thread::spawn(move || {
                for index in 0..6 {
                    let deadline = Instant::now() + Duration::from_secs(30);
                    loop {
                        if stopping.load(Ordering::SeqCst) {
                            return;
                        }
                        assert!(Instant::now() < deadline, "missing fixture request {index}");
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                stream
                                    .set_write_timeout(Some(Duration::from_secs(30)))
                                    .unwrap();
                                captured.lock().unwrap().push(read_request(&stream));
                                let body = response(index);
                                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                                stream.flush().unwrap();
                                break;
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(error) => panic!("fixture accept: {error}"),
                        }
                    }
                }
            });
            Self {
                url,
                requests,
                stop,
                worker: Some(worker),
            }
        }

        fn handle(&self, cwd: &std::path::Path) -> AgentSessionHandle {
            let provider = Arc::new(
                crate::providers::openai::OpenAIProvider::new("plan-fixture")
                    .with_base_url(&self.url),
            );
            let agent = crate::agent::Agent::new(
                provider,
                crate::tools::ToolRegistry::new(&["write"], cwd, None),
                crate::agent::AgentConfig {
                    max_tool_iterations: 8,
                    system_prompt: Some("base instructions".to_string()),
                    approval_state: Some(ApprovalState::new(
                        ApprovalMode::AlwaysAsk,
                        true,
                        Vec::new(),
                    )),
                    stream_options: crate::provider::StreamOptions {
                        api_key: Some("local-fixture-key".to_string()),
                        max_tokens: Some(256),
                        ..crate::provider::StreamOptions::default()
                    },
                    ..crate::agent::AgentConfig::default()
                },
            );
            let session = crate::agent::AgentSession::new(
                agent,
                Arc::new(Store::new(Session::in_memory())),
                false,
                crate::compaction::ResolvedCompactionSettings::default(),
            );
            AgentSessionHandle::from_session_with_listeners(
                session,
                crate::sdk::EventListeners::default(),
            )
        }
    }

    impl Drop for Peer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                let result = worker.join();
                if !std::thread::panicking() {
                    result.expect("wire fixture thread");
                }
            }
        }
    }

    fn has_submit_schema(request: &Value) -> bool {
        request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "submit_plan")
    }

    fn lifecycle(rebind: bool) {
        let peer = Peer::new();
        let temp = tempfile::Builder::new()
            .prefix("pi-plan-wire-")
            .tempdir_in("/tmp")
            .unwrap_or_else(|_| tempfile::tempdir().unwrap());
        let mut handle = peer.handle(temp.path());
        let state = handle.session().agent.plan_state();
        let foreign = PlanState::new();
        if rebind {
            foreign.enter_planning();
            state.enter_planning();
            handle
                .session_mut()
                .agent
                .extend_tools(std::iter::once(Box::new(crate::plan::SubmitPlanTool::new(
                    foreign.clone(),
                    true,
                )) as Box<dyn Tool>));
        }
        run(async {
            let owner = AgentCx::for_current_or_request();
            handle.prompt("warm schema cache", |_| {}).await.unwrap();
            assert_eq!(has_submit_schema(&peer.requests.lock().unwrap()[0]), rebind);
            let _ = handle.enter_plan_mode(&owner).await.unwrap();
            assert!(handle.has_tool("submit_plan"));
            handle.prompt("prepare a plan", |_| {}).await.unwrap();
            assert_eq!(state.mode(), PlanMode::PendingApproval);
            assert!(
                !temp.path().join("result.txt").exists(),
                "planning cannot write"
            );
            let review = handle.pending_plan_review().unwrap().unwrap();
            assert!(review.text().contains("Files: [\"result.txt\"]"));
            {
                let requests = peer.requests.lock().unwrap();
                assert_eq!(requests.len(), 4);
                assert!(
                    has_submit_schema(&requests[1]),
                    "invalidate the cached schema"
                );
                assert!(requests[2].to_string().contains("PLAN_MODE_BLOCKED"));
                drop(requests);
            }
            let _ = handle.approve_plan_review(&owner, &review).await.unwrap();
            assert_eq!(
                peer.requests.lock().unwrap().len(),
                4,
                "approval is not execution"
            );
            handle
                .prompt("execute the approved plan", |_| {})
                .await
                .unwrap();
            assert_eq!(
                std::fs::read(temp.path().join("result.txt")).unwrap(),
                b"approved mutation"
            );
            {
                let requests = peer.requests.lock().unwrap();
                assert_eq!(requests.len(), 6);
                assert!(
                    requests[4]["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|message| {
                            matches!(message["role"].as_str(), Some("system" | "developer"))
                                && message["content"]
                                    .as_str()
                                    .is_some_and(|s| s.contains(review.text()))
                        }),
                    "the actual execution request must carry the full reviewed plan"
                );
                drop(requests);
            }
            let _ = handle.exit_plan_mode(&owner).await.unwrap();
            assert_eq!(
                handle.session().agent.system_prompt(),
                Some("base instructions")
            );
        });
        if rebind {
            assert_eq!(foreign.mode(), PlanMode::Planning);
            assert!(
                foreign.plan().is_none(),
                "the foreign helper must never run"
            );
        }
    }

    #[test]
    fn sdk_plan_review_controls_a_real_provider_loop_and_file_mutation() {
        lifecycle(false);
    }

    #[test]
    fn manual_entry_rebinds_a_foreign_auto_approving_helper_even_when_already_planning() {
        lifecycle(true);
    }
}
