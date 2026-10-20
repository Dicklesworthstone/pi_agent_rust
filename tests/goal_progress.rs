//! Native goal-continuation coverage through HTTP/SSE and the real read tool.
//! No production Provider, Tool, goal driver or session store is mocked.
#![recursion_limit = "256"]

use pi::agent::{Agent, AgentConfig, AgentEvent, AgentSession};
use pi::agent_cx::AgentCx;
use pi::compaction::ResolvedCompactionSettings;
use pi::goal::{GoalBudget, GoalRunStop, GoalStatus};
use pi::provider::StreamOptions;
use pi::providers::openai::OpenAIProvider;
use pi::sdk::{AgentSessionHandle, EventListeners};
use pi::session::Session;
use pi::tools::ToolRegistry;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
enum Reply {
    Read(&'static str),
    Answer,
}

struct Peer {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

fn request(stream: &TcpStream) -> Value {
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut length = None;
    let mut header_bytes = 0;
    loop {
        let mut line = String::new();
        assert!(reader.by_ref().take(64 * 1024).read_line(&mut line).unwrap() > 0);
        header_bytes += line.len();
        assert!(header_bytes <= 64 * 1024);
        if line == "\r\n" { break; }
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

fn response(reply: Reply, index: usize) -> String {
    let (delta, reason) = match reply {
        Reply::Read(path) => (
            json!({"role":"assistant","tool_calls":[{
                "index":0,"id":format!("different-call-{index}"),"type":"function",
                "function":{"name":"read","arguments":json!({"path":path}).to_string()}
            }]}),
            "tool_calls",
        ),
        Reply::Answer => (json!({"role":"assistant","content":"Progress made; goal complete!"}), "stop"),
    };
    let event = |delta: Value, finish: Value| {
        format!("data: {}\n\n", json!({
            "id":format!("progress-{index}"),"object":"chat.completion.chunk",
            "created":0,"model":"goal-progress-fixture",
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
        }))
    };
    format!("{}{}data: [DONE]\n\n", event(delta, Value::Null), event(json!({}), json!(reason)))
}

impl Peer {
    fn new(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            for (index, reply) in replies.into_iter().enumerate() {
                let until = Instant::now() + Duration::from_secs(15);
                let mut stream = loop {
                    if stopping.load(Ordering::SeqCst) { return; }
                    assert!(Instant::now() < until, "goal wire request deadline");
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("accept: {error}"),
                    }
                };
                captured.lock().unwrap().push(request(&stream));
                let body = response(reply, index);
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                stream.flush().unwrap();
            }
        });
        Self { url, requests, stop, worker: Some(worker) }
    }

    fn handle(&self, cwd: &std::path::Path) -> AgentSessionHandle {
        let provider = Arc::new(OpenAIProvider::new("goal-progress-fixture").with_base_url(&self.url));
        let agent = Agent::new(
            provider,
            ToolRegistry::new(&["read"], cwd, None),
            AgentConfig {
                stream_options: StreamOptions {
                    api_key: Some("loopback-fixture".into()),
                    max_tokens: Some(128),
                    ..StreamOptions::default()
                },
                ..AgentConfig::default()
            },
        );
        AgentSessionHandle::from_session_with_listeners(
            AgentSession::new(
                agent,
                Arc::new(asupersync::sync::Mutex::new(Session::in_memory())),
                false,
                ResolvedCompactionSettings::default(),
            ),
            EventListeners::default(),
        )
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() { result.expect("local HTTP peer"); }
        }
    }
}

#[test]
fn unchanged_native_reads_pause_after_two_prompts_with_new_call_ids() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("a.txt"), "alpha\n").unwrap();
    let peer = Peer::new(vec![Reply::Read("a.txt"), Reply::Answer, Reply::Read("a.txt"), Reply::Answer]);
    let mut handle = peer.handle(temp.path());
    let observed = Arc::new(AtomicUsize::new(0));
    let callback_count = Arc::clone(&observed);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let created = handle.create_goal(&owner, "Inspect the file", GoalBudget::default()).await.unwrap();
        let run = handle.run_goal_until_review(&owner, &created.goal, 8, Duration::from_secs(30), move |_, event| {
            if let AgentEvent::ToolExecutionEnd { result, is_error, .. } = event {
                assert!(!is_error && !result.is_error, "real read must succeed");
                callback_count.fetch_add(1, Ordering::SeqCst);
            }
        }).unwrap();
        let control = run.control();
        let report = run.await;
        assert_eq!(report.stop, GoalRunStop::Paused);
        assert_eq!(report.goal.status(), GoalStatus::Paused);
        assert_eq!(report.goal.prompts_started(), 2);
        assert!(report.goal.note().contains("GOAL_REPEATED_TOOL_PROGRESS"));
        assert!(!report.goal.interrupted());
        assert!(report.completion.as_ref().unwrap().is_ok());
        assert!(report.error.is_none());
        assert!(control.is_finished());
        assert!(!control.pause());
        assert_eq!(handle.current_goal().unwrap().unwrap().note(), report.goal.note());
    });
    assert_eq!(observed.load(Ordering::SeqCst), 2);
    let requests = peer.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for index in [1, 3] {
        assert!(requests[index]["messages"].as_array().unwrap().iter().any(|message| {
            message["role"] == "tool" && message["content"].to_string().contains("alpha")
        }), "provider must receive native read output");
    }
}

#[test]
fn changed_native_work_continues_but_a_later_cycle_stops() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(temp.path().join("b.txt"), "beta\n").unwrap();
    let peer = Peer::new(vec![
        Reply::Read("a.txt"), Reply::Answer,
        Reply::Read("b.txt"), Reply::Answer,
        Reply::Read("a.txt"), Reply::Answer,
    ]);
    let mut handle = peer.handle(temp.path()).into_goal_session();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let created = handle.session_mut().create_goal(&owner, "Inspect both files", GoalBudget::default()).await.unwrap();
        let report = handle.run_goal_until_review(&owner, &created.goal, 8, Duration::from_secs(30), |_, _| {}).unwrap().await;
        assert_eq!(report.stop, GoalRunStop::Paused);
        assert_eq!(report.goal.prompts_started(), 3);
        assert!(report.goal.note().contains("GOAL_REPEATED_TOOL_PROGRESS"));
    });
    assert_eq!(peer.requests.lock().unwrap().len(), 6);
}

#[test]
fn a_failed_native_read_does_not_authorize_another_goal_prompt() {
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![Reply::Read("does-not-exist.txt"), Reply::Answer]);
    let mut handle = peer.handle(temp.path());
    let failures = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&failures);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let created = handle.create_goal(&owner, "Inspect the missing file", GoalBudget::default()).await.unwrap();
        let report = handle.run_goal_until_review(&owner, &created.goal, 8, Duration::from_secs(30), move |_, event| {
            if let AgentEvent::ToolExecutionEnd { result, is_error, .. } = event {
                assert!(is_error || result.is_error);
                counted.fetch_add(1, Ordering::SeqCst);
            }
        }).unwrap().await;
        assert_eq!(report.stop, GoalRunStop::Paused);
        assert_eq!(report.goal.prompts_started(), 1);
        assert!(report.goal.note().contains("GOAL_NO_TOOL_PROGRESS"));
    });
    assert_eq!(failures.load(Ordering::SeqCst), 1);
    assert_eq!(peer.requests.lock().unwrap().len(), 2);
}

#[test]
fn text_only_answers_pause_without_completion_and_resume_retains_budget() {
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![Reply::Answer, Reply::Answer]);
    let mut handle = peer.handle(temp.path());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let created = handle.create_goal(&owner, "Analyze and report", GoalBudget::default()).await.unwrap();
        let first = handle.run_goal_until_review(&owner, &created.goal, 8, Duration::from_secs(30), |_, _| {}).unwrap().await;
        assert_eq!(first.stop, GoalRunStop::Paused);
        assert!(first.goal.note().contains("GOAL_NO_TOOL_PROGRESS"));
        let resumed = handle.resume_goal(&owner, &first.goal).await.unwrap();
        let second = handle.run_goal_until_review(&owner, &resumed.goal, 8, Duration::from_secs(30), |_, _| {}).unwrap().await;
        assert_eq!(second.goal.status(), GoalStatus::Paused);
        assert_eq!(second.goal.prompts_started(), 2);
        assert!(second.completion.as_ref().unwrap().is_ok());
    });
    assert_eq!(peer.requests.lock().unwrap().len(), 2);
}

#[test]
fn invalid_limits_and_unpolled_drop_do_not_dispatch_or_mutate_a_goal() {
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(Vec::new());
    let mut handle = peer.handle(temp.path());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let created = handle.create_goal(&owner, "Never dispatched", GoalBudget::default()).await.unwrap();
        for steps in [0, 33] {
            let error = handle.run_goal_until_review(&owner, &created.goal, steps, Duration::from_secs(30), |_, _| {}).err().expect("invalid step budget");
            assert!(error.to_string().contains("GOAL_STEPS"));
        }
        let run = handle.run_goal_until_review(&owner, &created.goal, 3, Duration::from_secs(30), |_, _| panic!("unpolled dispatch")).unwrap();
        let control = run.control();
        drop(run);
        assert!(control.is_finished());
        let current = handle.current_goal().unwrap().unwrap();
        assert_eq!(current.prompts_started(), 0);
        assert!(!current.interrupted());
        assert_eq!(current.status(), GoalStatus::Active);
    });
    assert!(peer.requests.lock().unwrap().is_empty());
}

#[test]
fn native_goal_budget_still_stops_a_prompt_with_new_successful_work() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("a.txt"), "alpha\n").unwrap();
    let peer = Peer::new(vec![Reply::Read("a.txt"), Reply::Answer]);
    let mut handle = peer.handle(temp.path());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let created = handle.create_goal(&owner, "Inspect and then continue", GoalBudget { max_prompts: 1, token_budget: 100_000 }).await.unwrap();
        let report = handle.run_goal_until_review(&owner, &created.goal, 8, Duration::from_secs(30), |_, _| {}).unwrap().await;
        assert_eq!(report.stop, GoalRunStop::BudgetLimited);
        assert_eq!(report.goal.status(), GoalStatus::BudgetLimited);
        assert_eq!(report.goal.prompts_started(), 1);
    });
    assert_eq!(peer.requests.lock().unwrap().len(), 2);
}

#[test]
fn live_host_pause_wins_over_new_activity_and_never_submits_another_goal_prompt() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("a.txt"), "alpha\n").unwrap();
    let peer = Peer::new(vec![Reply::Read("a.txt"), Reply::Answer]);
    let mut handle = peer.handle(temp.path());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        let owner = AgentCx::for_current_or_request();
        let created = handle.create_goal(&owner, "Pause after the read", GoalBudget::default()).await.unwrap();
        let report = handle.run_goal_until_review(&owner, &created.goal, 8, Duration::from_secs(30), |control, event| {
            if matches!(event, AgentEvent::ToolExecutionEnd { .. }) {
                assert!(control.pause());
            }
        }).unwrap().await;
        assert_eq!(report.stop, GoalRunStop::Paused);
        assert_eq!(report.goal.status(), GoalStatus::Paused);
        assert_eq!(report.goal.prompts_started(), 1);
        assert!(report.completion.is_some(), "native cancellation outcome is retained");
    });
    let requests = peer.requests.lock().unwrap();
    assert!(!requests.is_empty() && requests.len() <= 2);
    for request in requests.iter() {
        assert_eq!(request["messages"].as_array().unwrap().iter().filter(|message| message["role"] == "user").count(), 1);
    }
}
