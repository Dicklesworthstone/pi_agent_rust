#![recursion_limit = "256"]

//! Public controlled-content entrypoints through the native SDK, OpenAI
//! transport, SSE parser, read tool and session store. Only the remote HTTP
//! peer is a fixture; no Provider, Agent, Tool or persistence implementation
//! is replaced. Every socket wait is bounded and the peer is joined on drop.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use asupersync::runtime::RuntimeBuilder;
use pi::agent::{Agent, AgentConfig, AgentEvent, AgentSession};
use pi::agent_cx::AgentCx;
use pi::compaction::ResolvedCompactionSettings;
use pi::model::{
    ContentBlock, ImageContent, MediaContent, Message, StopReason, TextContent, ThinkingContent,
};
use pi::provider::StreamOptions;
use pi::providers::openai::OpenAIProvider;
use pi::sdk::{AgentSessionHandle, EventListeners};
use pi::session::Session;
use pi::session_control::ControllableSession;
use pi::tools::ToolRegistry;
use serde_json::{Value, json};

struct Peer {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

fn read_request(stream: &TcpStream) -> Value {
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut header_bytes = 0;
    let mut length = None;
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        header_bytes += line.len();
        assert!(header_bytes <= 64 * 1024, "bounded fixture headers");
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
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn event(delta: Value, finish: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": "native-control", "object": "chat.completion.chunk",
            "created": 0, "model": "native-control",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        })
    )
}

impl Peer {
    fn new(read_tool_first: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let captured = Arc::clone(&requests);
        let stopping = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut index = 0;
            while !stopping.load(Ordering::SeqCst) {
                assert!(Instant::now() < deadline, "fixture lifetime exceeded");
                let (mut stream, _) = match listener.accept() {
                    Ok(accepted) => accepted,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                };
                let request = read_request(&stream);
                captured.lock().unwrap().push(request);
                let (delta, reason) = if read_tool_first && index == 0 {
                    (
                        json!({"role":"assistant", "tool_calls":[{
                            "index":0, "id":"read-once", "type":"function",
                            "function":{"name":"read", "arguments":"{\"path\":\"note.txt\"}"}
                        }]}),
                        "tool_calls",
                    )
                } else {
                    (json!({"role":"assistant", "content":"complete"}), "stop")
                };
                let body = format!(
                    "{}{}data: [DONE]\n\n",
                    event(delta, Value::Null),
                    event(json!({}), json!(reason))
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                stream.flush().unwrap();
                index += 1;
                assert!(index <= 8, "unexpected repeated provider requests");
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn handle(&self, cwd: &Path, accepts_images: bool) -> AgentSessionHandle {
        let provider = Arc::new(OpenAIProvider::new("native-control").with_base_url(&self.url));
        let agent = Agent::new(
            provider,
            ToolRegistry::new(&["read"], cwd, None),
            AgentConfig {
                max_tool_iterations: 4,
                model_accepts_images: accepts_images,
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

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() {
                result.expect("HTTP fixture worker");
            }
        }
    }
}

fn native_input(label: &str) -> Vec<ContentBlock> {
    vec![
        ContentBlock::Text(TextContent::new(format!("  {label}\n"))),
        ContentBlock::Image(ImageContent {
            data: "YQ==".to_string(),
            mime_type: "image/png".to_string(),
        }),
        ContentBlock::Text(TextContent::new("\ntrailing context  ")),
    ]
}

async fn stored_users(session: &mut ControllableSession) -> Vec<Value> {
    let store = session.session_mut().session_store();
    let owner = AgentCx::for_current_or_request();
    let state = store.lock(owner.cx()).await.unwrap();
    let messages = state.to_messages_for_current_path();
    drop(state);
    messages
        .into_iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(serde_json::to_value(user.content).unwrap()),
            _ => None,
        })
        .collect()
}

fn first_user(request: &Value) -> &Value {
    &request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
}

#[test]
fn native_prompt_preserves_order_and_media_in_session_with_normal_wire_degradation() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(false);
    let mut session = peer.handle(temp.path(), true).into_controllable();
    let mut blocks = native_input("inspect");
    for (mime_type, name, data) in [
        ("audio/wav", "clip.wav", "YXVkaW8="),
        ("video/mp4", "clip.mp4", "dmlkZW8="),
    ] {
        blocks.push(ContentBlock::Media(MediaContent {
            data: data.to_string(),
            mime_type: mime_type.to_string(),
            name: Some(name.to_string()),
        }));
    }
    let expected = serde_json::to_value(&blocks).unwrap();
    runtime.block_on(async {
        let turn = session.prompt_with_content(blocks, |_| {}).unwrap();
        let control = turn.control();
        assert_eq!(turn.await.unwrap().stop_reason, StopReason::Stop);
        assert!(control.snapshot().finished);
        assert_eq!(stored_users(&mut session).await, vec![expected]);
    });
    let requests = peer.requests();
    assert_eq!(requests.len(), 1);
    let content = first_user(&requests[0]).as_array().unwrap();
    assert_eq!(content[0], json!({"type":"text", "text":"  inspect\n"}));
    assert_eq!(content[1], json!({
        "type":"image_url", "image_url":{"url":"data:image/png;base64,YQ=="}
    }));
    assert_eq!(content[2], json!({"type":"text", "text":"\ntrailing context  "}));
    let wire = requests[0].to_string();
    assert!(wire.contains("clip.wav") && wire.contains("clip.mp4"));
    assert!(!wire.contains("YXVkaW8=") && !wire.contains("dmlkZW8="));
}

#[test]
fn native_callback_steers_and_follows_up_without_replaying_the_tool() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("note.txt"), "native prompt read result").unwrap();
    let peer = Peer::new(true);
    let mut session = peer.handle(temp.path(), true).into_controllable();
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&tool_calls);
    runtime.block_on(async {
        let turn = session
            .prompt_with_content_and_control(native_input("read note.txt"), move |control, event| {
                if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                    assert_eq!(observed.fetch_add(1, Ordering::SeqCst), 0);
                    control.steer("focus on constraints").unwrap();
                    control.follow_up("then explain tradeoffs").unwrap();
                }
            })
            .unwrap();
        let control = turn.control();
        assert_eq!(turn.await.unwrap().stop_reason, StopReason::Stop);
        assert!(control.snapshot().finished);
        assert_eq!(control.snapshot().handed_to_agent, 2);
        assert!(control.take_pending().is_empty());
        assert_eq!(stored_users(&mut session).await.len(), 3);
    });
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    let requests = peer.requests();
    assert_eq!(requests.len(), 3);
    assert!(!requests[0].to_string().contains("focus on constraints"));
    assert!(requests[1].to_string().contains("focus on constraints"));
    assert!(!requests[1].to_string().contains("then explain tradeoffs"));
    assert!(requests[2].to_string().contains("then explain tradeoffs"));
    assert_eq!(first_user(&requests[0]), first_user(&requests[2]));
}

#[test]
fn unused_and_precancelled_native_turns_preserve_pending_input_without_dispatch() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(false);
    let mut session = peer.handle(temp.path(), true).into_controllable();
    runtime.block_on(async {
        for cancel in [false, true] {
            let turn = session.prompt_with_content(native_input("never sent"), |_| {}).unwrap();
            let control = turn.control();
            let id = control.follow_up("recover only this input").unwrap();
            if cancel {
                assert!(control.abort());
                assert!(turn.await.is_err());
            } else {
                drop(turn);
            }
            assert!(control.snapshot().finished);
            assert_eq!(control.snapshot().handed_to_agent, 0);
            assert_eq!(control.take_pending()[0].id, id);
            assert!(control.steer("stale input").is_err());
            assert!(stored_users(&mut session).await.is_empty());
        }
        assert_eq!(
            session.prompt("ordinary text".to_string(), |_| {}).unwrap().await.unwrap().stop_reason,
            StopReason::Stop
        );
        assert_eq!(stored_users(&mut session).await, vec![json!("ordinary text")]);
    });
    let requests = peer.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(first_user(&requests[0]), "ordinary text");
    assert!(!requests[0].to_string().contains("never sent"));
    assert!(!requests[0].to_string().contains("recover only this input"));
}

#[test]
fn invalid_native_prompt_retires_control_and_does_not_poison_the_next_turn() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(false);
    let mut session = peer.handle(temp.path(), true).into_controllable();
    runtime.block_on(async {
        let turn = session
            .prompt_with_content(vec![ContentBlock::Thinking(ThinkingContent {
                thinking: "assistant-only input".to_string(),
                thinking_signature: None,
            })], |_| {})
            .unwrap();
        let control = turn.control();
        let id = control.follow_up("still recoverable").unwrap();
        assert!(turn.await.is_err());
        assert!(control.snapshot().finished);
        assert_eq!(control.take_pending()[0].id, id);
        assert!(stored_users(&mut session).await.is_empty());
        assert_eq!(
            session.prompt_with_content(native_input("valid"), |_| {}).unwrap().await.unwrap().stop_reason,
            StopReason::Stop
        );
    });
    let requests = peer.requests();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].to_string().contains("assistant-only input"));
    assert!(!requests[0].to_string().contains("still recoverable"));
}

#[test]
fn native_turn_after_handle_replacement_uses_only_the_new_session_and_control_lane() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let first = Peer::new(false);
    let second = Peer::new(false);
    let mut session = first.handle(temp.path(), true).into_controllable();
    runtime.block_on(async {
        let turn = session.prompt_with_content(native_input("old session"), |_| {}).unwrap();
        let old = turn.control();
        assert_eq!(turn.await.unwrap().stop_reason, StopReason::Stop);
        let previous = std::mem::replace(session.session_mut(), second.handle(temp.path(), true));
        let turn = session.prompt_with_content(native_input("new session"), |_| {}).unwrap();
        let current = turn.control();
        current.follow_up("new follow-up").unwrap();
        assert!(!old.abort());
        assert!(old.steer("stale input").is_err());
        assert_eq!(turn.await.unwrap().stop_reason, StopReason::Stop);
        assert_eq!(current.snapshot().handed_to_agent, 1);
        assert!(current.snapshot().finished);
        let users = stored_users(&mut session).await;
        assert_eq!(users.len(), 2);
        assert_eq!(users[0], serde_json::to_value(native_input("new session")).unwrap());
        assert_eq!(users[1], json!("new follow-up"));
        drop(previous);
    });
    assert_eq!(first.requests().len(), 1);
    let requests = second.requests();
    assert_eq!(requests.len(), 2);
    assert!(!requests[1].to_string().contains("old session"));
    assert!(!requests[1].to_string().contains("stale input"));
}

#[test]
fn controlled_content_does_not_bypass_text_only_model_filtering() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let peer = Peer::new(false);
    let mut session = peer.handle(temp.path(), false).into_controllable();
    let content = native_input("text-only model");
    let expected = serde_json::to_value(&content).unwrap();
    runtime.block_on(async {
        let turn = session.prompt_with_content(content, |_| {}).unwrap();
        assert_eq!(turn.await.unwrap().stop_reason, StopReason::Stop);
        assert_eq!(stored_users(&mut session).await, vec![expected]);
    });
    let requests = peer.requests();
    assert_eq!(requests.len(), 1);
    let wire = requests[0].to_string();
    assert!(!wire.contains("image_url") && !wire.contains("YQ=="));
    assert!(wire.contains("text-only model") && wire.contains("trailing context"));
}
