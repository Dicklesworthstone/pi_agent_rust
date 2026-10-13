#![allow(clippy::significant_drop_tightening, clippy::needless_pass_by_value)]

//! Native-input entrypoints through the real SDK, Agent, OpenAI adapter and
//! HTTP/SSE parser. The bounded local peer supplies wire replies, not mocks of
//! any production component. No live credentials or external endpoint is used.

use super::*;
use crate::agent::{Agent, AgentConfig, AgentSession};
use crate::compaction::ResolvedCompactionSettings;
use crate::model::{Message, StopReason};
use crate::provider::StreamOptions;
use crate::providers::openai::OpenAIProvider;
use crate::sdk::{AgentSessionHandle, EventListeners};
use crate::session::Session;
use crate::session_control::{TurnDeadline, lock};
use crate::tools::ToolRegistry;
use asupersync::runtime::RuntimeBuilder;
use asupersync::time::{TimerDriverHandle, VirtualClock};
use asupersync::types::Time;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct Peer {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

fn request(stream: &TcpStream) -> Value {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut length = None;
    let mut header_bytes = 0;
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0, "header EOF");
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
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut stream = loop {
                    if stopping.load(Ordering::SeqCst) {
                        return;
                    }
                    assert!(Instant::now() < deadline, "wire request deadline");
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
                        "500 Internal Server Error",
                        "application/json",
                        json!({"error":{"message":"fixture unavailable"}}).to_string(),
                    )
                } else {
                    let event = |delta: Value, reason: Value| {
                        format!(
                            "data: {}\n\n",
                            json!({
                                "id":"native-input", "object":"chat.completion.chunk",
                                "created":0, "model":"control-fixture",
                                "choices":[{"index":0,"delta":delta,"finish_reason":reason}]
                            })
                        )
                    };
                    let start = event(
                        json!({"role":"assistant","content":"complete"}),
                        Value::Null,
                    );
                    let end = event(json!({}), json!("stop"));
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

    fn session(&self, cwd: &std::path::Path) -> ControllableSession {
        let provider = Arc::new(OpenAIProvider::new("control-fixture").with_base_url(&self.url));
        let agent = Agent::new(
            provider,
            ToolRegistry::new(&[], cwd, None),
            AgentConfig {
                model_accepts_images: true,
                stream_options: StreamOptions {
                    api_key: Some("local-fixture-key".to_string()),
                    max_tokens: Some(128),
                    ..StreamOptions::default()
                },
                ..AgentConfig::default()
            },
        );
        let session = AgentSession::new(
            agent,
            Arc::new(asupersync::sync::Mutex::new(Session::in_memory())),
            false,
            ResolvedCompactionSettings::default(),
        );
        AgentSessionHandle::from_session_with_listeners(session, EventListeners::default())
            .into_controllable()
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() {
                result.expect("local wire peer");
            }
        }
    }
}

fn image() -> ImageContent {
    ImageContent {
        data: "YQ==".to_string(),
        mime_type: "image/png".to_string(),
    }
}

fn blocks() -> Vec<ContentBlock> {
    vec![
        ContentBlock::Text(TextContent::new("  inspect\n")),
        ContentBlock::Image(image()),
        ContentBlock::Text(TextContent::new("\ntrailing context  ")),
    ]
}

async fn users(session: &mut ControllableSession) -> Vec<UserContent> {
    let store = session.session_mut().session_store();
    let cx = AgentCx::for_current_or_request();
    let state = store.lock(cx.cx()).await.unwrap();
    state
        .to_messages_for_current_path()
        .into_iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(user.content),
            _ => None,
        })
        .collect()
}

fn wire_users(request: &Value) -> Vec<&Value> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .collect()
}

#[test]
fn reusable_native_prompt_preserves_wire_order_history_and_live_follow_up() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![false, false]);
    let mut session = peer.session(temp.path());
    let queued = AtomicBool::new(false);
    runtime.block_on(async {
        let turn = session
            .prompt_with_content_with_control(blocks(), move |control, event| {
                if matches!(
                    event,
                    AgentEvent::MessageStart {
                        message: Message::Assistant(_)
                    }
                ) && !queued.swap(true, Ordering::SeqCst)
                {
                    control.follow_up("explain next").unwrap();
                }
            })
            .unwrap();
        let control = turn.control();
        assert_eq!(turn.await.unwrap().stop_reason, StopReason::Stop);
        assert!(control.snapshot().finished);
        assert_eq!(control.snapshot().handed_to_agent, 1);
        assert!(control.take_pending().is_empty());
        assert!(!control.abort());
        let saved = users(&mut session).await;
        assert_eq!(saved.len(), 2);
        assert_eq!(
            serde_json::to_value(&saved[0]).unwrap(),
            serde_json::to_value(blocks()).unwrap()
        );
        assert!(matches!(&saved[1], UserContent::Text(text) if text == "explain next"));
    });
    let requests = lock(&peer.requests);
    assert_eq!(requests.len(), 2);
    let first = wire_users(&requests[0]);
    assert_eq!(first.len(), 1);
    assert_eq!(
        first[0]["content"],
        json!([
            {"type":"text","text":"  inspect\n"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,YQ=="}},
            {"type":"text","text":"\ntrailing context  "}
        ])
    );
    assert_eq!(wire_users(&requests[1]).len(), 2);
    assert!(wire_users(&requests[1])[1]["content"].to_string().contains("explain next"));
}

#[test]
fn image_only_and_empty_image_calls_reuse_the_session_without_empty_blocks() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![false, false]);
    let mut session = peer.session(temp.path());
    runtime.block_on(async {
        let turn = session
            .prompt_with_images_with_control(String::new(), vec![image()], |control, event| {
                if matches!(event, AgentEvent::MessageStart { .. }) {
                    assert!(control.snapshot().accepting_input);
                }
            })
            .unwrap();
        let old = turn.control();
        assert_eq!(turn.await.unwrap().stop_reason, StopReason::Stop);
        let next = session
            .prompt_with_images("plain text".to_string(), Vec::new(), |_| {})
            .unwrap();
        assert!(old.steer("stale input").is_err());
        assert!(!old.abort());
        assert_eq!(next.await.unwrap().stop_reason, StopReason::Stop);
        let saved = users(&mut session).await;
        assert_eq!(saved.len(), 2);
        assert_eq!(
            serde_json::to_value(&saved[0]).unwrap(),
            serde_json::to_value(vec![ContentBlock::Image(image())]).unwrap()
        );
        assert!(matches!(&saved[1], UserContent::Text(text) if text == "plain text"));
    });
    let requests = lock(&peer.requests);
    assert_eq!(requests.len(), 2);
    assert_eq!(
        wire_users(&requests[0])[0]["content"],
        json!([
            {"type":"image_url","image_url":{"url":"data:image/png;base64,YQ=="}}
        ])
    );
    assert!(!requests[1].to_string().contains("stale input"));
}

#[test]
fn unpolled_drop_abort_and_expiry_do_not_dispatch_native_input_or_poison_reuse() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![false]);
    let mut session = peer.session(temp.path());
    runtime.block_on(async {
        let unused = session.prompt_with_content(blocks(), |_| {}).unwrap();
        let old = unused.control();
        old.follow_up_with_content(&UserContent::Blocks(blocks()), "recover attachment")
            .unwrap();
        drop(unused);
        assert!(old.snapshot().finished);
        let aborted = session
            .prompt_with_images("never dispatched".to_string(), vec![image()], |_| {})
            .unwrap();
        let control = aborted.control();
        assert!(control.abort());
        assert!(aborted.await.is_err());
        assert!(control.snapshot().finished);
        let clock = Arc::new(VirtualClock::new());
        let deadline = TurnDeadline::from_timer(
            &AgentCx::for_current_or_request(),
            TimerDriverHandle::with_virtual_clock(Arc::clone(&clock)),
            Duration::from_secs(1),
        )
        .unwrap();
        let expired = session
            .prompt_with_content(blocks(), |_| {})
            .unwrap()
            .with_deadline(deadline);
        let control = expired.control();
        clock.advance_to(Time::from_secs(1));
        let error = expired.await.unwrap_err();
        assert!(error.is_elapsed());
        assert!(error.completion().is_none());
        assert!(control.snapshot().finished);
        assert!(users(&mut session).await.is_empty());
        let retained = old.take_pending();
        assert_eq!(retained.len(), 1);
        assert_eq!(
            serde_json::to_value(&retained[0].content).unwrap(),
            serde_json::to_value(blocks()).unwrap()
        );
        let next = session
            .prompt_with_content(
                vec![ContentBlock::Text(TextContent::new("actual prompt"))],
                |_| {},
            )
            .unwrap();
        assert_eq!(next.await.unwrap().stop_reason, StopReason::Stop);
        assert_eq!(users(&mut session).await.len(), 1);
    });
    let requests = lock(&peer.requests);
    assert_eq!(requests.len(), 1);
    assert!(requests[0].to_string().contains("actual prompt"));
    assert!(!requests[0].to_string().contains("never dispatched"));
    assert!(!requests[0].to_string().contains("image_url"));
}

#[test]
fn failed_native_prompt_continues_without_duplicating_input_or_leaking_old_mailbox() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![true, false]);
    let mut session = peer.session(temp.path());
    runtime.block_on(async {
        let first = session.prompt_with_content(blocks(), |_| {}).unwrap();
        let old = first.control();
        old.follow_up_with_content(&UserContent::Blocks(blocks()), "unclaimed attachment")
            .unwrap();
        let result = first.await;
        assert!(result.is_err() || result.unwrap().stop_reason == StopReason::Error);
        assert!(old.snapshot().finished);
        let next = session.continue_turn(|_| {}).unwrap();
        assert_eq!(next.await.unwrap().stop_reason, StopReason::Stop);
        assert_eq!(old.snapshot().handed_to_agent, 0);
        assert_eq!(old.take_pending()[0].text, "unclaimed attachment");
        let saved = users(&mut session).await;
        assert_eq!(saved.len(), 1);
        assert_eq!(
            serde_json::to_value(&saved[0]).unwrap(),
            serde_json::to_value(blocks()).unwrap()
        );
    });
    let requests = lock(&peer.requests);
    assert_eq!(requests.len(), 2);
    assert_eq!(wire_users(&requests[1]).len(), 1);
    assert_eq!(wire_users(&requests[0])[0], wire_users(&requests[1])[0]);
    assert!(!requests[1].to_string().contains("unclaimed attachment"));
}
