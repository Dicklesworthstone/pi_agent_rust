#![allow(clippy::significant_drop_tightening, clippy::needless_pass_by_value)]

//! End-to-end control tests through the real OpenAI adapter, HTTP/SSE parser,
//! agent loop, read tool, and SDK session wrapper. The local peer supplies wire
//! fixtures only; it does not replace any of those production implementations.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use asupersync::runtime::RuntimeBuilder;
use serde_json::{Value, json};

use super::*;
use crate::agent::{Agent, AgentConfig, AgentSession};
use crate::compaction::ResolvedCompactionSettings;
use crate::model::StopReason;
use crate::provider::StreamOptions;
use crate::providers::openai::OpenAIProvider;
use crate::sdk::EventListeners;
use crate::session::Session;
use crate::tools::ToolRegistry;

#[derive(Clone, Copy)]
enum Reply {
    Tool,
    Text,
    Error,
    Hanging,
}

struct Peer {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

fn read_request(stream: &TcpStream) -> Value {
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
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "request ended in headers"
        );
        header_bytes += line.len();
        assert!(
            header_bytes <= 64 * 1024,
            "oversized fixture request headers"
        );
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
    }
    let length = length.expect("provider sends a fixed JSON body");
    assert!(length <= 2 * 1024 * 1024);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn event(delta: Value, finish: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id":"control-fixture", "object":"chat.completion.chunk",
            "created":0, "model":"control-fixture",
            "choices":[{"index":0, "delta":delta, "finish_reason":finish}]
        })
    )
}

fn respond(stream: &mut TcpStream, reply: Reply, stop: &AtomicBool) {
    if matches!(reply, Reply::Hanging) {
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
        let chunk = event(
            json!({"role":"assistant", "content":"partial"}),
            Value::Null,
        );
        write!(stream, "{:x}\r\n{chunk}\r\n", chunk.len()).unwrap();
        stream.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        // EOF without a terminal marker is intentional. A failed abort test
        // becomes a finite stream error instead of hanging the test runner.
        return;
    }
    let (status, content_type, body) = match reply {
        Reply::Error => (
            "500 Internal Server Error",
            "application/json",
            json!({"error":{"message":"local fixture failure"}}).to_string(),
        ),
        Reply::Tool => {
            let start = event(
                json!({"role":"assistant", "tool_calls":[{
                    "index":0, "id":"read-once", "type":"function",
                    "function":{"name":"read", "arguments":"{\"path\":\"note.txt\"}"}
                }]}),
                Value::Null,
            );
            let end = event(json!({}), json!("tool_calls"));
            (
                "200 OK",
                "text/event-stream",
                format!("{start}{end}data: [DONE]\n\n"),
            )
        }
        Reply::Text => {
            let start = event(
                json!({"role":"assistant", "content":"complete"}),
                Value::Null,
            );
            let end = event(json!({}), json!("stop"));
            (
                "200 OK",
                "text/event-stream",
                format!("{start}{end}data: [DONE]\n\n"),
            )
        }
        Reply::Hanging => unreachable!(),
    };
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    stream.flush().unwrap();
}

impl Peer {
    fn new(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let captured = Arc::clone(&requests);
        let stopping = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            for reply in replies {
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if stopping.load(Ordering::SeqCst) {
                        return;
                    }
                    assert!(Instant::now() < deadline, "fixture request deadline");
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            lock(&captured).push(read_request(&stream));
                            respond(&mut stream, reply, &stopping);
                            break;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("accept failed: {error}"),
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

    fn session(&self, cwd: &std::path::Path) -> ControllableSession {
        self.handle(cwd).into_controllable()
    }

    fn handle(&self, cwd: &std::path::Path) -> AgentSessionHandle {
        let provider = Arc::new(OpenAIProvider::new("control-fixture").with_base_url(&self.url));
        let agent = Agent::new(
            provider,
            ToolRegistry::new(&["read"], cwd, None),
            AgentConfig {
                max_tool_iterations: 4,
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

fn request_has(request: &Value, text: &str) -> bool {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["content"].to_string().contains(text))
}

#[test]
fn mid_tool_thread_can_steer_and_follow_up_without_replaying_the_tool() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("note.txt"), "real read tool fixture").unwrap();
    let peer = Peer::new(vec![Reply::Tool, Reply::Text, Reply::Text]);
    let mut session = peer.session(temp.path());
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    runtime.block_on(async {
        let turn = session
            .prompt_with_control("read note.txt".to_string(), move |control, event| {
                if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                    let control = control.clone();
                    std::thread::spawn(move || {
                        control.steer("focus on constraints").unwrap();
                        control.follow_up("then explain tradeoffs").unwrap();
                    })
                    .join()
                    .unwrap();
                }
                lock(&captured).push(event);
            })
            .unwrap();
        let control = turn.control();
        assert_eq!(turn.await.unwrap().stop_reason, StopReason::Stop);
        assert!(control.snapshot().finished);
        assert_eq!(control.snapshot().handed_to_agent, 2);
        assert!(control.take_pending().is_empty());
    });
    let requests = lock(&peer.requests);
    assert_eq!(requests.len(), 3);
    assert!(!request_has(&requests[0], "focus on constraints"));
    assert!(request_has(&requests[1], "focus on constraints"));
    assert!(!request_has(&requests[1], "then explain tradeoffs"));
    assert!(request_has(&requests[2], "then explain tradeoffs"));
    assert_eq!(
        lock(&events)
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolExecutionStart { .. }))
            .count(),
        1
    );
    drop(requests);
    let store = session.session_mut().session_store();
    runtime.block_on(async {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let state = store.lock(cx.cx()).await.unwrap();
        let messages = state.to_messages_for_current_path();
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, Message::User(_)))
                .count(),
            3
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, Message::ToolResult(_)))
                .count(),
            1
        );
    });
}

/// `prompt_controlled` on a borrowed handle, the way the FTUI driver uses it:
/// the control lane is published through a shared slot, a steer and a
/// follow-up sent mid-tool reach the next requests without replaying the tool,
/// and a finished turn's control cannot feed a later turn on the same handle.
#[test]
fn borrowed_handle_turn_steers_mid_tool_and_retires_its_control() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("note.txt"), "real read tool fixture").unwrap();
    let peer = Peer::new(vec![Reply::Tool, Reply::Text, Reply::Text, Reply::Text]);
    let mut handle = peer.handle(temp.path());
    let published: Arc<Mutex<Option<SessionControlHandle>>> = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&published);
    runtime.block_on(async {
        let turn = handle.prompt_controlled("read note.txt".to_string(), move |event| {
            if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                let control = lock(&seen).clone().expect("control published");
                std::thread::spawn(move || {
                    control.steer("focus on constraints").unwrap();
                    control.follow_up("then explain tradeoffs").unwrap();
                })
                .join()
                .unwrap();
            }
        });
        *lock(&published) = Some(turn.control());
        assert_eq!(turn.await.unwrap().stop_reason, StopReason::Stop);
    });
    let old = lock(&published).take().unwrap();
    assert!(old.snapshot().finished);
    assert_eq!(old.snapshot().handed_to_agent, 2);
    assert!(
        old.steer("late input").is_err(),
        "a finished turn's control must refuse new input"
    );

    // A second turn on the same handle installs fresh fetchers; nothing from
    // the old lane leaks into it.
    runtime.block_on(async {
        let turn = handle.prompt_controlled("and now?".to_string(), |_| {});
        assert_eq!(turn.await.unwrap().stop_reason, StopReason::Stop);
    });
    let requests = lock(&peer.requests);
    assert_eq!(requests.len(), 4);
    assert!(!request_has(&requests[0], "focus on constraints"));
    assert!(request_has(&requests[1], "focus on constraints"));
    assert!(request_has(&requests[2], "then explain tradeoffs"));
    assert!(request_has(&requests[3], "and now?"));
    assert!(!request_has(&requests[3], "late input"));
}

#[test]
fn control_abort_interrupts_a_live_unfinished_provider_stream() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![Reply::Hanging]);
    let mut session = peer.session(temp.path());
    runtime.block_on(async {
        let turn = session
            .prompt_with_control("stream".to_string(), |control, event| {
                if matches!(
                    event,
                    AgentEvent::MessageStart {
                        message: Message::Assistant(_)
                    }
                ) {
                    control.follow_up("recover after abort").unwrap();
                    assert!(control.abort());
                }
            })
            .unwrap();
        let control = turn.control();
        assert_eq!(turn.await.unwrap().stop_reason, StopReason::Aborted);
        assert!(control.snapshot().finished);
        assert_eq!(control.take_pending()[0].text, "recover after abort");
    });
    assert_eq!(lock(&peer.requests).len(), 1);
}

#[test]
fn provider_failure_keeps_unclaimed_follow_up_out_of_the_next_prompt() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![Reply::Error, Reply::Text]);
    let mut session = peer.session(temp.path());
    runtime.block_on(async {
        let turn = session.prompt("first".to_string(), |_| {}).unwrap();
        let old = turn.control();
        old.follow_up("unclaimed-private-follow-up").unwrap();
        let result = turn.await;
        assert!(result.is_err() || result.unwrap().stop_reason == StopReason::Error);
        assert!(old.snapshot().finished);
        let next = session.prompt("second".to_string(), |_| {}).unwrap();
        assert!(!old.abort());
        assert_eq!(next.await.unwrap().stop_reason, StopReason::Stop);
        assert_eq!(old.take_pending()[0].text, "unclaimed-private-follow-up");
    });
    let requests = lock(&peer.requests);
    assert_eq!(requests.len(), 2);
    assert!(!request_has(&requests[1], "unclaimed-private-follow-up"));
}

#[test]
fn unpolled_public_turn_can_be_dropped_then_replaced_without_a_request() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![Reply::Text]);
    let mut session = peer.session(temp.path());
    let unused = session
        .prompt("must not be sent".to_string(), |_| {})
        .unwrap();
    let old = unused.control();
    old.steer("recover me").unwrap();
    drop(unused);
    assert!(old.snapshot().finished);
    runtime.block_on(async {
        let next = session.prompt("real prompt".to_string(), |_| {}).unwrap();
        assert_eq!(next.await.unwrap().stop_reason, StopReason::Stop);
    });
    assert_eq!(old.take_pending()[0].text, "recover me");
    let requests = lock(&peer.requests);
    assert_eq!(requests.len(), 1);
    assert!(!request_has(&requests[0], "must not be sent"));
}

#[test]
fn controlled_continuation_does_not_append_the_original_prompt_twice() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![Reply::Error, Reply::Text]);
    let mut session = peer.session(temp.path());
    runtime.block_on(async {
        let first = session
            .prompt("original prompt".to_string(), |_| {})
            .unwrap()
            .await;
        assert!(first.is_err() || first.unwrap().stop_reason == StopReason::Error);
        let resumed = session.continue_turn(|_| {}).unwrap();
        let control = resumed.control();
        assert_eq!(resumed.await.unwrap().stop_reason, StopReason::Stop);
        assert!(control.snapshot().finished);
    });
    let requests = lock(&peer.requests);
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user"
                && message["content"].to_string().contains("original prompt"))
            .count(),
        1
    );
}

#[test]
fn a_delayed_old_fetch_cannot_dequeue_new_turn_input() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let (old, guard, _) = super::tests::live();
    let active = Arc::clone(&guard.active);
    let source = fetcher(&active, InputKind::Steering);
    old.steer("old input").unwrap();
    let delayed = source();
    assert_eq!(old.snapshot().pending_steering, 1);
    drop(guard);
    let (new, _new_guard, _) = super::tests::live();
    *lock(&active) = Arc::downgrade(&new.run);
    new.steer("new input").unwrap();
    runtime.block_on(async {
        assert!(delayed.await.is_empty());
        assert_eq!(source().await[0].text_for_display(), Some("new input"));
    });
    assert_eq!(old.take_pending()[0].text, "old input");
}

#[test]
fn queued_native_image_reaches_the_real_provider_without_flattening() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("note.txt"), "fixture").unwrap();
    let peer = Peer::new(vec![Reply::Tool, Reply::Text]);
    let mut session = peer.session(temp.path());
    runtime.block_on(async {
        let turn = session.prompt_with_control("read note.txt".to_string(), |control, event| {
            if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                let content = UserContent::Blocks(vec![
                    crate::model::ContentBlock::Text(crate::model::TextContent::new("inspect image")),
                    crate::model::ContentBlock::Image(crate::model::ImageContent {
                        data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4//8/AAX+Av4N70a4AAAAAElFTkSuQmCC".to_string(),
                        mime_type: "image/png".to_string(),
                    }),
                    crate::model::ContentBlock::Text(crate::model::TextContent::new("trailing context")),
                ]);
                control.steer_with_content(&content, "inspect image").unwrap();
            }
        }).unwrap();
        assert_eq!(turn.await.unwrap().stop_reason, StopReason::Stop);
    });
    let requests = lock(&peer.requests);
    assert_eq!(requests.len(), 2);
    let parts = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .find(|parts| parts.iter().any(|part| part["type"] == "image_url"))
        .expect("native image in provider request");
    assert_eq!(parts[0]["text"], "inspect image");
    assert!(
        parts[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    assert_eq!(parts[2]["text"], "trailing context");
}

mod deadline_recovery;
