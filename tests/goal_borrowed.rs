//! Borrowed goals use the real SDK, session and OpenAI wire implementation.
//! The loopback peer supplies protocol responses, not a replacement provider.
#![recursion_limit = "512"]

use pi::agent::{Agent, AgentConfig, AgentSession};
use pi::agent_cx::AgentCx;
use pi::compaction::ResolvedCompactionSettings;
use pi::goal::{GoalBudget, GoalDecision, GoalRunStop, GoalStatus};
use pi::provider::StreamOptions;
use pi::providers::openai::OpenAIProvider;
use pi::sdk::{AgentSessionHandle, EventListeners};
use pi::session::Session;
use pi::tools::ToolRegistry;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(asupersync::runtime::reactor::create_reactor().expect("reactor"))
        .build()
        .expect("runtime")
        .block_on(future)
}

fn handle(url: &str, cwd: &std::path::Path) -> AgentSessionHandle {
    let provider = Arc::new(OpenAIProvider::new("goal-fixture").with_base_url(url));
    let agent = Agent::new(
        provider,
        ToolRegistry::new(&[], cwd, None),
        AgentConfig {
            stream_options: StreamOptions {
                api_key: Some("local-goal-fixture".into()),
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

struct Peer {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

fn read_request(stream: &TcpStream) -> Value {
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("read timeout");
    stream.set_write_timeout(Some(Duration::from_secs(5))).expect("write timeout");
    let mut reader = BufReader::new(stream.try_clone().expect("reader"));
    let mut length = None;
    let mut header_bytes = 0;
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).expect("header") > 0);
        header_bytes += line.len();
        assert!(header_bytes <= 64 * 1024);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>().expect("length"));
        }
    }
    let length = length.expect("fixed request body");
    assert!(length <= 2 * 1024 * 1024);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).expect("body");
    serde_json::from_slice(&body).expect("request JSON")
}

impl Peer {
    fn new(failures: Vec<bool>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let url = format!("http://{}/v1", listener.local_addr().expect("address"));
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
                    assert!(Instant::now() < until, "goal request deadline");
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        result => {
                            result.expect("loopback accept");
                        }
                    }
                };
                captured.lock().expect("requests").push(read_request(&stream));
                let (status, content_type, body) = if failure {
                    (
                        "400 Bad Request",
                        "application/json",
                        json!({"error":{"message":"fixture rejected the request"}}).to_string(),
                    )
                } else {
                    let event = |delta: Value, reason: Value, usage: Value| {
                        format!("data: {}\n\n", json!({
                            "id":"goal-response", "object":"chat.completion.chunk",
                            "created":0, "model":"goal-fixture",
                            "choices":[{"index":0,"delta":delta,"finish_reason":reason}],
                            "usage":usage
                        }))
                    };
                    let start = event(json!({"role":"assistant","content":"Goal complete!"}), Value::Null, Value::Null);
                    let end = event(json!({}), json!("stop"), json!({"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}));
                    ("200 OK", "text/event-stream", format!("{start}{end}data: [DONE]\n\n"))
                };
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("response");
                stream.flush().expect("flush");
            }
        });
        Self { url, requests, stop, worker: Some(worker) }
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
    GoalDecision::Complete { evidence: "host checked deliverables".into() }
}

#[test]
fn borrowed_goal_preserves_history_and_allows_an_ordinary_turn_afterwards() {
    let peer = Peer::new(vec![false, false, false]);
    let root = tempfile::tempdir().expect("workspace");
    let mut handle = handle(&peer.url, root.path());
    run(async {
        let owner = AgentCx::for_current_or_request();
        let goal = handle.create_goal(&owner, "Keep the same agent and history", GoalBudget {
            max_prompts: 3, token_budget: 1000,
        }).await.expect("goal");
        let mut verdicts = 0;
        let report = handle.run_goal(
            &owner, &goal.goal, Duration::from_secs(10), |_, _| {},
            |_, _| {
                verdicts += 1;
                if verdicts == 1 {
                    GoalDecision::Continue { instruction: "Implement the second step".into() }
                } else {
                    complete()
                }
            },
        ).expect("run").await;
        assert_eq!(verdicts, 2, "model prose cannot complete a goal");
        assert_eq!(report.stop, GoalRunStop::Complete);
        assert_eq!(report.goal.prompts_started(), 2);
        assert_eq!(report.goal.tokens_spent(), 30);
        assert!(report.error.is_none());
        assert!(report.pending.is_empty());
        assert_eq!(handle.current_goal().expect("current").expect("present").id(), goal.goal.id());
        handle.prompt_controlled("An ordinary turn after the goal".into(), |_| {})
            .await.expect("ordinary turn");
    });
    let requests = peer.requests.lock().expect("requests");
    assert_eq!(requests.len(), 3);
    assert!(requests[1]["messages"].as_array().expect("history").len()
        > requests[0]["messages"].as_array().expect("history").len());
    assert!(requests[2].to_string().contains("An ordinary turn after the goal"));
}

#[test]
fn unpolled_borrowed_goal_retires_control_without_starting_an_attempt() {
    let root = tempfile::tempdir().expect("workspace");
    let mut handle = handle("http://127.0.0.1:1/v1", root.path());
    run(async {
        let owner = AgentCx::for_current_or_request();
        let goal = handle.create_goal(&owner, "No implicit dispatch", GoalBudget::default())
            .await.expect("goal").goal;
        let work = handle.run_goal(&owner, &goal, Duration::from_secs(10), |_, _| {}, |_, _| complete())
            .expect("run");
        let control = work.control();
        drop(work);
        assert!(control.is_finished());
        assert!(control.steer("too late").is_err());
        let current = handle.current_goal().expect("current").expect("present");
        assert_eq!(current.prompts_started(), 0);
        assert!(!current.interrupted());
        assert_eq!(current.status(), GoalStatus::Active);
    });
}

#[test]
fn stale_borrowed_snapshot_is_rejected_without_provider_work() {
    let root = tempfile::tempdir().expect("workspace");
    let mut handle = handle("http://127.0.0.1:1/v1", root.path());
    run(async {
        let owner = AgentCx::for_current_or_request();
        let goal = handle.create_goal(&owner, "Old review", GoalBudget::default())
            .await.expect("goal").goal;
        handle.pause_goal(&owner, &goal, "changed after review").await.expect("pause");
        let report = handle.run_goal(&owner, &goal, Duration::from_secs(10), |_, _| {}, |_, _| complete())
            .expect("run").await;
        assert_eq!(report.stop, GoalRunStop::Failed);
        assert!(report.error.expect("stale error").to_string().contains("GOAL_STALE"));
        assert!(report.completion.is_none());
        assert_eq!(handle.current_goal().expect("current").expect("present").prompts_started(), 0);
    });
}

#[test]
fn provider_failure_never_reaches_the_completion_verifier() {
    let peer = Peer::new(vec![true]);
    let root = tempfile::tempdir().expect("workspace");
    let mut handle = handle(&peer.url, root.path());
    run(async {
        let owner = AgentCx::for_current_or_request();
        let goal = handle.create_goal(&owner, "Keep failures visible", GoalBudget::default())
            .await.expect("goal").goal;
        let mut verified = false;
        let report = handle.run_goal(&owner, &goal, Duration::from_secs(10), |_, _| {}, |_, _| {
            verified = true;
            complete()
        }).expect("run").await;
        assert!(!verified);
        assert_eq!(report.stop, GoalRunStop::NativeFailure);
        assert_eq!(report.goal.status(), GoalStatus::Paused);
        assert_eq!(report.goal.prompts_started(), 1);
        assert!(report.completion.is_some());
    });
    assert_eq!(peer.requests.lock().expect("requests").len(), 1);
}
