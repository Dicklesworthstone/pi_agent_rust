//! Multimodal SDK prompts and live host control through production transport paths.
#![recursion_limit = "256"]

use asupersync::runtime::RuntimeBuilder;
use asupersync::runtime::reactor::create_reactor;
use asupersync::sync::Mutex as AsyncMutex;
use pi::failover::RetryPolicy;
use pi::sdk::{
    AbortHandle, Agent, AgentConfig, AgentEvent, AgentSession, AgentSessionHandle, ContentBlock,
    Error, EventListeners, FailoverOptions, ImageContent, InputType, MediaContent, Message,
    ResolvedCompactionSettings, Session, SessionPromptResult, SessionTransport,
    SessionTransportEvent, StopReason, StreamEvent, StreamOptions, TextContent, ToolRegistry,
    UserContent,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+j5mEAAAAASUVORK5CYII=";
const GIF: &str = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";

fn images() -> Vec<ImageContent> {
    vec![
        ImageContent {
            data: PNG.to_string(),
            mime_type: "image/png".to_string(),
        },
        ImageContent {
            data: GIF.to_string(),
            mime_type: "image/gif".to_string(),
        },
    ]
}

fn ordered_native_content() -> Vec<ContentBlock> {
    vec![
        ContentBlock::Text(TextContent::new("  compare the opening\n")),
        ContentBlock::Media(MediaContent {
            data: "YQ==".to_string(),
            mime_type: "audio/wav".to_string(),
            name: Some("voice.wav".to_string()),
        }),
        ContentBlock::Text(TextContent::new("with this frame")),
        ContentBlock::Image(ImageContent {
            data: PNG.to_string(),
            mime_type: "image/png".to_string(),
        }),
        ContentBlock::Media(MediaContent {
            data: "Yg".to_string(),
            mime_type: "video/mp4".to_string(),
            name: Some("motion.mp4".to_string()),
        }),
        ContentBlock::Text(TextContent::new(" \t\n")),
    ]
}

fn run_async<F: std::future::Future>(future: F) -> F::Output {
    let runtime = RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("reactor"))
        .build()
        .expect("runtime");
    runtime.block_on(Box::pin(future))
}

/// Only the remote API is a fixture. Production provider selection, request
/// serialization, SSE parsing, cancellation, recovery and persistence all run.
struct ApiFixture {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ApiFixture {
    fn new(statuses: Vec<u16>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let url = format!("http://{}/v1", listener.local_addr().expect("address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            for status in statuses {
                let deadline = Instant::now() + Duration::from_secs(15);
                let mut stream = loop {
                    if stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "fixture request timed out");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("fixture accept failed: {error}"),
                    }
                };
                // An accepted socket can inherit the listener's non-blocking
                // mode (macOS), which would bypass the read timeout below.
                stream
                    .set_nonblocking(false)
                    .expect("blocking request socket");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("read timeout");
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .expect("write timeout");
                let request = read_request(&mut stream, deadline);
                captured.lock().expect("capture lock").push(request);
                let (content_type, body) = response_body(status);
                write!(
                    stream,
                    "HTTP/1.1 {status} Fixture\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len(),
                )
                .expect("write fixture response");
                stream.flush().expect("flush fixture response");
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn finish(&mut self, expected_requests: usize) -> Vec<Value> {
        self.stop.store(true, Ordering::SeqCst);
        self.worker
            .take()
            .expect("fixture worker")
            .join()
            .expect("fixture worker completed");
        let requests = self.requests.lock().expect("capture lock").clone();
        assert_eq!(requests.len(), expected_requests);
        requests
    }
}

impl Drop for ApiFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// One read where a socket timeout means "nothing yet", not failure: macOS
/// reports it as `WouldBlock` (bd-eg6ng). Callers loop on a wall deadline.
fn read_some(stream: &mut TcpStream, buffer: &mut [u8], what: &str) -> Option<usize> {
    match stream.read(buffer) {
        Ok(read) => Some(read),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            None
        }
        Err(error) => panic!("{what}: {error}"),
    }
}

fn read_request(stream: &mut TcpStream, deadline: Instant) -> Value {
    const LIMIT: usize = 256 * 1024;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let (header_end, body_len) = loop {
        assert!(Instant::now() < deadline, "fixture headers timed out");
        let Some(read) = read_some(stream, &mut buffer, "read request headers") else {
            continue;
        };
        assert!(read > 0, "request ended before headers");
        bytes.extend_from_slice(&buffer[..read]);
        assert!(bytes.len() <= LIMIT, "oversized fixture request");
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).expect("ASCII headers");
            assert!(
                headers
                    .lines()
                    .next()
                    .expect("request line")
                    .contains("/chat/completions")
            );
            let body_len = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .expect("content-length header");
            assert!(body_len <= LIMIT);
            break (end + 4, body_len);
        }
    };
    while bytes.len() < header_end + body_len {
        assert!(Instant::now() < deadline, "fixture body timed out");
        let Some(read) = read_some(stream, &mut buffer, "read request body") else {
            continue;
        };
        assert!(read > 0, "request ended before body");
        bytes.extend_from_slice(&buffer[..read]);
        assert!(bytes.len() <= LIMIT + 16 * 1024);
    }
    serde_json::from_slice(&bytes[header_end..header_end + body_len]).expect("request JSON")
}

fn response_body(status: u16) -> (&'static str, String) {
    if status != 200 {
        return (
            "application/json",
            json!({"error": {"message": "503 service unavailable", "type": "server_error"}})
                .to_string(),
        );
    }
    let start = json!({
        "id": "vision-fixture", "object": "chat.completion.chunk", "created": 0,
        "model": "vision-primary",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Images received"}, "finish_reason": null}]
    });
    let end = json!({
        "id": "vision-fixture",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    (
        "text/event-stream",
        format!("data: {start}\n\ndata: {end}\n\ndata: [DONE]\n\n"),
    )
}

fn model_entry(url: &str, model: &str) -> pi::sdk::ModelEntry {
    let mut entry = pi::models::ad_hoc_model_entry("openai", model).expect("model entry");
    entry.model.api = "openai-completions".to_string();
    entry.model.base_url = url.to_string();
    entry.model.input = vec![InputType::Text, InputType::Image];
    entry
}

fn handle(url: &str, root: &Path, block_images: bool) -> AgentSessionHandle {
    let entry = model_entry(url, "vision-primary");
    let provider = pi::providers::create_provider(&entry, None).expect("real provider");
    let mut stored = Session::create_with_dir(Some(root.join("sessions")));
    stored.header.cwd = root.display().to_string();
    stored.header.provider = Some("openai".to_string());
    stored.header.model_id = Some("vision-primary".to_string());
    let agent = Agent::new(
        provider,
        ToolRegistry::new(&[], root, None),
        AgentConfig {
            block_images,
            model_accepts_images: true,
            stream_options: StreamOptions {
                api_key: Some("fixture-key".to_string()),
                session_id: Some(stored.header.id.clone()),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let session = AgentSession::new(
        agent,
        Arc::new(AsyncMutex::new(stored)),
        true,
        ResolvedCompactionSettings {
            enabled: false,
            ..Default::default()
        },
    );
    AgentSessionHandle::from_session_with_listeners(session, EventListeners::default())
}

const fn retry_policy(retries: u32, failovers: u32) -> RetryPolicy {
    RetryPolicy {
        max_retries: retries,
        max_failovers_per_turn: failovers,
        base_delay_ms: 1,
        max_delay_ms: 1,
    }
}

fn user_wire_content(request: &Value) -> &Value {
    let users = request["messages"]
        .as_array()
        .expect("wire messages")
        .iter()
        .filter(|message| message["role"] == "user")
        .collect::<Vec<_>>();
    assert_eq!(
        users.len(),
        1,
        "recovery must not duplicate the user prompt"
    );
    &users[0]["content"]
}

fn assert_wire_images(request: &Value, text: &str) {
    let blocks = user_wire_content(request)
        .as_array()
        .expect("multimodal wire content");
    let texts = blocks
        .iter()
        .filter(|block| block["type"] == "text")
        .map(|block| block["text"].as_str().expect("wire text"))
        .collect::<Vec<_>>();
    assert_eq!(texts.join("\n"), text);
    let urls = blocks
        .iter()
        .filter(|block| block["type"] == "image_url")
        .map(|block| block["image_url"]["url"].as_str().expect("image URL"))
        .collect::<Vec<_>>();
    assert_eq!(
        urls,
        [
            format!("data:image/png;base64,{PNG}"),
            format!("data:image/gif;base64,{GIF}")
        ]
    );
}

fn assert_stored_images(messages: &[Message]) {
    let users = messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(user),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(users.len(), 1);
    let UserContent::Blocks(blocks) = &users[0].content else {
        panic!("attachments were flattened to text");
    };
    let attached = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image(image) => Some((image.mime_type.as_str(), image.data.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(attached, [("image/png", PNG), ("image/gif", GIF)]);
}

fn reopen(handle: &AgentSessionHandle) -> Session {
    let path = handle
        .session_store()
        .try_lock()
        .expect("session lock")
        .path
        .clone()
        .expect("saved path");
    run_async(Session::open(&path.display().to_string())).expect("reopen session")
}

fn capture_stream_events(handle: &mut AgentSessionHandle) -> Arc<Mutex<Vec<StreamEvent>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&events);
    handle.listeners_mut().on_stream_event = Some(Arc::new(move |event| {
        observed
            .lock()
            .expect("stream event lock")
            .push(event.clone());
    }));
    events
}

fn terminal_reasons(events: &[StreamEvent]) -> Vec<StopReason> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Done { reason, .. } | StreamEvent::Error { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect()
}

#[test]
fn sdk_mml_img_retry_preserves_wire_attachments_and_one_durable_prompt() {
    for explicit_abort in [false, true] {
        let root = tempfile::tempdir().expect("tempdir");
        let mut server = ApiFixture::new(vec![503, 200]);
        let mut handle =
            handle(&server.url, root.path(), false).with_retry(Some(retry_policy(1, 0)));
        let streams = capture_stream_events(&mut handle);
        let subscribed = Arc::new(Mutex::new(Vec::<Value>::new()));
        let observed = Arc::clone(&subscribed);
        handle.subscribe(move |event| {
            observed
                .lock()
                .unwrap()
                .push(serde_json::to_value(event).unwrap());
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let callback = move |event| captured.lock().expect("event lock").push(event);
        let message = run_async(async {
            if explicit_abort {
                let (_abort, signal) = AbortHandle::new();
                handle
                    .prompt_with_images_with_abort(
                        "compare these images",
                        images(),
                        signal,
                        callback,
                    )
                    .await
            } else {
                handle
                    .prompt_with_images("compare these images", images(), callback)
                    .await
            }
        })
        .expect("image turn recovered");
        assert_eq!(message.stop_reason, StopReason::Stop);
        {
            let streams = streams.lock().unwrap();
            assert_eq!(
                terminal_reasons(&streams),
                [StopReason::Error, StopReason::Stop]
            );
            let Some(StreamEvent::Done {
                message: terminal, ..
            }) = streams.last()
            else {
                panic!("typed stream must finish with the recovered response");
            };
            assert_eq!(
                serde_json::to_value(terminal).unwrap(),
                serde_json::to_value(&message).unwrap()
            );
            let text = streams
                .iter()
                .filter_map(|event| match event {
                    StreamEvent::TextDelta { delta, .. } => Some(delta.as_str()),
                    _ => None,
                })
                .collect::<String>();
            assert_eq!(text, "Images received", "deltas must not be replayed");
        }
        for request in server.finish(2) {
            assert_wire_images(&request, "compare these images");
        }
        let stored = reopen(&handle).to_messages_for_current_path();
        assert_stored_images(&stored);
        assert_eq!(stored.len(), 2, "only user and final assistant remain");
        let events = events.lock().expect("event lock").clone();
        assert_eq!(
            *subscribed.lock().unwrap(),
            events
                .iter()
                .map(|event| serde_json::to_value(event).unwrap())
                .collect::<Vec<_>>(),
            "typed terminal delivery must not alter generic SDK fan-out"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::AgentEnd { .. }))
                .count(),
            1
        );
        let Some(AgentEvent::AgentEnd {
            messages, error, ..
        }) = events.last()
        else {
            panic!("terminal event must follow recovery");
        };
        assert!(error.is_none());
        assert_eq!(
            serde_json::to_value(messages).unwrap(),
            serde_json::to_value(stored).unwrap()
        );
    }
}

#[test]
fn sdk_mml_img_failover_keeps_the_original_images_on_the_new_model() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut server = ApiFixture::new(vec![503, 200]);
    let mut handle = handle(&server.url, root.path(), false)
        .with_retry(Some(retry_policy(0, 1)))
        .with_failover(Some(FailoverOptions {
            chains: HashMap::from([(
                "default".to_string(),
                vec!["openai/vision-fallback".to_string()],
            )]),
            available_models: vec![model_entry(&server.url, "vision-fallback")],
            auth: pi::auth::AuthStorage::empty_at(root.path().join("auth.json")),
            cli_api_key: Some("fixture-key".to_string()),
            cooldown_secs: 300,
        }));
    let message = run_async(handle.prompt_with_images("inspect both", images(), |_| {}))
        .expect("fallback completes");
    assert_eq!(message.stop_reason, StopReason::Stop);
    let requests = server.finish(2);
    assert_eq!(requests[0]["model"], "vision-primary");
    assert_eq!(requests[1]["model"], "vision-fallback");
    for request in &requests {
        assert_wire_images(request, "inspect both");
    }
    assert_eq!(handle.model().1, "vision-fallback");
    assert_stored_images(&reopen(&handle).to_messages_for_current_path());
}

#[test]
fn sdk_mml_img_preabort_has_no_provider_or_session_side_effects() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut server = ApiFixture::new(vec![200]);
    let mut handle = handle(&server.url, root.path(), false);
    let streams = capture_stream_events(&mut handle);
    let (abort, signal) = AbortHandle::new();
    abort.abort();
    let result = run_async(handle.prompt_with_images_with_abort(
        "discard",
        images(),
        signal,
        |_| panic!("a pre-aborted prompt must not emit a started lifecycle"),
    ));
    assert!(matches!(result, Err(Error::Aborted)));
    assert!(streams.lock().unwrap().is_empty());
    server.finish(0);
    assert!(run_async(handle.messages()).unwrap().is_empty());
    assert!(handle.session_store().try_lock().unwrap().path.is_none());
}

#[test]
fn sdk_mml_img_backoff_abort_keeps_attachments_without_reissuing() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut server = ApiFixture::new(vec![503, 200]);
    let mut handle = handle(&server.url, root.path(), false).with_retry(Some(retry_policy(2, 0)));
    let streams = capture_stream_events(&mut handle);
    let (abort, signal) = AbortHandle::new();
    let result = run_async(handle.prompt_with_images_with_abort(
        "keep images",
        images(),
        signal,
        move |event| {
            if matches!(event, AgentEvent::AutoRetryStart { .. }) {
                abort.abort();
            }
        },
    ));
    assert!(matches!(result, Err(Error::Aborted)));
    assert_eq!(
        terminal_reasons(&streams.lock().unwrap()),
        [StopReason::Error]
    );
    assert_wire_images(&server.finish(1)[0], "keep images");
    assert_stored_images(&reopen(&handle).to_messages_for_current_path());
}

#[test]
fn sdk_mml_img_stream_abort_delivers_one_error_terminal_and_no_success() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut server = ApiFixture::new(vec![200, 200]);
    let mut handle = handle(&server.url, root.path(), false).with_retry(Some(retry_policy(2, 0)));
    let streams = capture_stream_events(&mut handle);
    let (abort, signal) = AbortHandle::new();
    let message = run_async(handle.prompt_with_images_with_abort(
        "abort this response",
        images(),
        signal,
        move |event| {
            if matches!(
                event,
                AgentEvent::MessageStart {
                    message: Message::Assistant(_)
                }
            ) {
                abort.abort();
            }
        },
    ))
    .expect("stream cancellation returns its assistant outcome");
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(
        terminal_reasons(&streams.lock().unwrap()),
        [StopReason::Aborted]
    );
    assert_wire_images(&server.finish(1)[0], "abort this response");
    assert_stored_images(&reopen(&handle).to_messages_for_current_path());
}

#[test]
fn sdk_mml_img_transport_supports_image_only_prompts() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut server = ApiFixture::new(vec![200]);
    let mut transport =
        SessionTransport::InProcess(Box::new(handle(&server.url, root.path(), false)));
    let result = run_async(transport.prompt_with_images("", images(), |event| {
        assert!(matches!(event, SessionTransportEvent::InProcess(_)));
    }))
    .expect("image-only prompt");
    let SessionPromptResult::InProcess(message) = result else {
        panic!("in-process result");
    };
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_wire_images(&server.finish(1)[0], "");
}

#[test]
fn sdk_mml_native_transport_retries_preserve_order_and_one_durable_prompt() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut server = ApiFixture::new(vec![503, 200]);
    let handle = handle(&server.url, root.path(), false).with_retry(Some(retry_policy(1, 0)));
    let mut transport = SessionTransport::InProcess(Box::new(handle));
    let content = ordered_native_content();
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&events);
    let result = run_async(
        transport.prompt_with_content(content.clone(), move |event| {
            let SessionTransportEvent::InProcess(event) = event else {
                panic!("in-process event expected");
            };
            observed.lock().unwrap().push(*event);
        }),
    )
    .expect("native content prompt retries");
    let SessionPromptResult::InProcess(message) = result else {
        panic!("in-process result expected");
    };
    assert_eq!(message.stop_reason, StopReason::Stop);
    let expected_wire = json!([
        {"type":"text", "text":"  compare the opening\n"},
        {"type":"text", "text":"[media omitted: voice.wav, audio/wav, 1 B]"},
        {"type":"text", "text":"with this frame"},
        {"type":"image_url", "image_url":{"url":format!("data:image/png;base64,{PNG}")}},
        {"type":"text", "text":"[media omitted: motion.mp4, video/mp4, 1 B]"},
        {"type":"text", "text":" \t\n"}
    ]);
    for request in server.finish(2) {
        assert_eq!(user_wire_content(&request), &expected_wire);
    }
    let session = reopen(transport.as_in_process_mut().expect("in-process handle"));
    let messages = session.to_messages_for_current_path();
    let users = messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(&user.content),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(users.len(), 1, "recovery must persist native input once");
    assert_eq!(
        serde_json::to_value(users[0]).unwrap(),
        serde_json::to_value(content).unwrap()
    );
    let events = events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::AgentEnd { .. }))
            .count(),
        1
    );
    assert!(matches!(events.last(), Some(AgentEvent::AgentEnd { .. })));
}

#[test]
fn sdk_mml_explicit_retry_keeps_native_content_and_the_abandoned_branch() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut server = ApiFixture::new(vec![200, 200]);
    let mut handle = handle(&server.url, root.path(), false);
    let content = ordered_native_content();
    run_async(handle.prompt_with_content(content.clone(), |_| {})).expect("original turn");
    let original = reopen(&handle);
    let original_leaf = original.leaf_id().map(str::to_string);
    let original_users = original
        .entries
        .iter()
        .filter_map(|entry| match entry {
            pi::session::SessionEntry::Message(entry)
                if matches!(&entry.message, pi::session::SessionMessage::User { .. }) =>
            {
                entry.base.id.clone()
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(original_users.len(), 1);
    let expected_parent = original
        .get_entry(&original_users[0])
        .expect("original user entry")
        .base()
        .parent_id
        .clone();

    let error = run_async(handle.prepare_retry()).expect_err("text API cannot drop media");
    assert!(error.to_string().contains("prepare_retry_content"));
    assert_eq!(
        reopen(&handle).leaf_id(),
        original_leaf.as_deref(),
        "rejecting text-only retry must not move the durable leaf"
    );
    let prepared = run_async(handle.prepare_retry_content()).expect("native retry plan");
    assert_eq!(
        serde_json::to_value(&prepared).unwrap(),
        serde_json::to_value(&content).unwrap()
    );
    let rewound = reopen(&handle);
    assert_eq!(
        rewound.leaf_id(),
        expected_parent.as_deref(),
        "the parent leaf is already durable"
    );
    assert!(rewound.to_messages_for_current_path().is_empty());
    assert!(rewound.get_entry(&original_users[0]).is_some());
    assert!(
        rewound
            .get_entry(original_leaf.as_deref().unwrap())
            .is_some()
    );

    let UserContent::Blocks(blocks) = prepared else {
        panic!("native input must not flatten to text");
    };
    run_async(handle.prompt_with_content(blocks, |_| {})).expect("sibling retry turn");
    let requests = server.finish(2);
    assert_eq!(
        user_wire_content(&requests[0]),
        user_wire_content(&requests[1])
    );
    let retried = reopen(&handle);
    let users = retried
        .entries
        .iter()
        .filter_map(|entry| match entry {
            pi::session::SessionEntry::Message(entry) => match &entry.message {
                pi::session::SessionMessage::User { content, .. } => {
                    Some((&entry.base.parent_id, content))
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        users.len(),
        2,
        "both original and retried input stay in the file"
    );
    assert_eq!(users[0].0, users[1].0, "the retry must be a sibling");
    for (_, stored_content) in users {
        assert_eq!(
            serde_json::to_value(stored_content).unwrap(),
            serde_json::to_value(&content).unwrap()
        );
    }
    assert_eq!(
        retried
            .to_messages_for_current_path()
            .iter()
            .filter(|message| matches!(message, Message::User(_)))
            .count(),
        1,
        "only the retried input remains in the active context"
    );
}

#[test]
fn sdk_mml_img_empty_attachments_preserve_plain_text_content() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut server = ApiFixture::new(vec![200]);
    let mut handle = handle(&server.url, root.path(), false);
    run_async(handle.prompt_with_images("  exact\nspacing  ", Vec::new(), |_| {}))
        .expect("plain text prompt");
    server.finish(1);
    let messages = reopen(&handle).to_messages_for_current_path();
    assert!(matches!(
        &messages[0],
        Message::User(user) if matches!(&user.content, UserContent::Text(text) if text == "  exact\nspacing  ")
    ));
}

#[test]
fn sdk_mml_img_provider_image_blocking_still_applies() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut server = ApiFixture::new(vec![200]);
    let mut handle = handle(&server.url, root.path(), true);
    run_async(handle.prompt_with_images("images blocked", images(), |_| {}))
        .expect("blocked-image placeholder prompt");
    let encoded = serde_json::to_string(&server.finish(1)[0]).unwrap();
    assert!(!encoded.contains(PNG));
    assert!(!encoded.contains(GIF));
    assert!(!encoded.contains("image_url"));
}

fn assert_turn_save_failure_fences_every_entrypoint(with_images: bool, retry: bool) {
    let root = tempfile::tempdir().expect("tempdir");
    // Keep an extra response available so an illegal later provider call is
    // observable rather than disguised as an unavailable fixture server.
    let statuses = if retry {
        vec![503, 200, 200]
    } else {
        vec![200, 200]
    };
    let mut server = ApiFixture::new(statuses);
    let policy = retry.then_some(retry_policy(1, 0));
    let mut handle = handle(&server.url, root.path(), false).with_retry(policy);
    let streams = capture_stream_events(&mut handle);
    let events = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observed_events = Arc::clone(&events);
    let blocked = root.path().join("directory-not-session.jsonl");
    std::fs::create_dir(&blocked).expect("blocked persistence path");
    let store = handle.session_store();
    let original = Arc::new(Mutex::new(None::<PathBuf>));
    let captured = Arc::clone(&original);
    let callback = move |event| {
        if matches!(
            &event,
            AgentEvent::MessageEnd {
                message: Message::Assistant(message)
            } if message.stop_reason == StopReason::Stop
        ) {
            // Inject after the successful provider response, immediately
            // before its turn artifacts are saved. In the retry case this is
            // the continuation's save, NOT the failed-tail restoration save.
            let mut session = store.try_lock().expect("before turn-save session lock");
            let mut original = captured.lock().expect("path lock");
            assert!(original.is_none(), "inject the fault exactly once");
            *original = Some(session.path.clone().expect("user prompt was saved"));
            session.path = Some(blocked.clone());
        }
        observed_events
            .lock()
            .unwrap()
            .push(serde_json::to_value(event).unwrap());
    };
    let result = run_async(async {
        if with_images {
            handle
                .prompt_with_images("persist once", images(), callback)
                .await
        } else {
            handle.prompt("persist once", callback).await
        }
    });
    assert!(
        result.as_ref().is_err_and(Error::is_session_persistence),
        "completed provider work must not hide failed persistence: {result:?}"
    );
    let expected_terminals = if retry {
        vec![StopReason::Error, StopReason::Stop]
    } else {
        vec![StopReason::Stop]
    };
    let stream_count = streams.lock().unwrap().len();
    assert_eq!(
        terminal_reasons(&streams.lock().unwrap()),
        expected_terminals
    );
    {
        let events = events.lock().unwrap();
        let terminal = events.last().expect("logical terminal event");
        assert_eq!(terminal["type"], "agent_end");
        assert!(
            terminal["error"]
                .as_str()
                .unwrap()
                .contains(Error::SESSION_PERSISTENCE_PREFIX),
            "provider Done is not a successful durability acknowledgement"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "agent_end")
                .count(),
            1
        );
    }
    let original = original
        .lock()
        .unwrap()
        .clone()
        .expect("fault was injected");
    handle.session_store().try_lock().unwrap().path = Some(original.clone());
    let disk_before = std::fs::read(&original).expect("read original durable transcript");
    let before = serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap();
    let expected_requests = if retry { 2 } else { 1 };
    assert_eq!(server.requests.lock().unwrap().len(), expected_requests);

    for entrypoint in 0..6 {
        let (_abort, signal) = AbortHandle::new();
        let callback = |_| panic!("quarantined entrypoint must not emit events");
        let again = run_async(async {
            match entrypoint {
                0 => handle.prompt("must not append", callback).await,
                1 => {
                    handle
                        .prompt_with_abort("must not append", signal, callback)
                        .await
                }
                2 => {
                    handle
                        .prompt_with_images("must not append", images(), callback)
                        .await
                }
                3 => {
                    handle
                        .prompt_with_images_with_abort(
                            "must not append",
                            images(),
                            signal,
                            callback,
                        )
                        .await
                }
                4 => handle.continue_turn(callback).await,
                _ => handle.continue_turn_with_abort(signal, callback).await,
            }
        });
        assert!(
            again.as_ref().is_err_and(Error::is_session_persistence),
            "entrypoint {entrypoint}: repaired paths must not clear quarantine: {again:?}"
        );
        assert_eq!(
            before,
            serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap(),
            "entrypoint {entrypoint}: live history must not change"
        );
        assert_eq!(
            disk_before,
            std::fs::read(&original).unwrap(),
            "entrypoint {entrypoint}: durable history must not change"
        );
        assert_eq!(server.requests.lock().unwrap().len(), expected_requests);
        assert_eq!(streams.lock().unwrap().len(), stream_count);
    }
    if with_images {
        assert_stored_images(&reopen(&handle).to_messages_for_current_path());
    }
    for request in server.finish(expected_requests) {
        if with_images {
            assert_wire_images(&request, "persist once");
        } else {
            assert_eq!(user_wire_content(&request), "persist once");
        }
    }
}

#[test]
fn sdk_mml_img_failed_retry_save_fences_later_image_prompts() {
    assert_turn_save_failure_fences_every_entrypoint(true, true);
}

#[test]
fn sdk_mml_img_failed_first_turn_save_fences_every_entrypoint() {
    assert_turn_save_failure_fences_every_entrypoint(true, false);
}

#[test]
fn sdk_mml_text_failed_retry_turn_save_fences_every_entrypoint() {
    assert_turn_save_failure_fences_every_entrypoint(false, true);
}

#[test]
fn sdk_mml_text_failed_first_turn_save_fences_every_entrypoint() {
    assert_turn_save_failure_fences_every_entrypoint(false, false);
}

#[cfg(unix)]
#[test]
fn sdk_mml_img_rpc_transport_sends_images_and_streams_callbacks() {
    // Exercise the public process-boundary SDK adapter, using the same shell
    // protocol-fixture technique as sdk_api.rs. No external provider is used.
    let script = r#"
IFS= read -r frame || exit 1
id=$(printf '%s\n' "$frame" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
printf '{"type":"response","command":"prompt","id":"%s","success":true}\n' "$id"
printf '{"type":"agent_start","sessionId":"vision-rpc"}\n'
printf '{"type":"fixture_input","frame":%s}\n' "$frame"
printf '{"type":"agent_end","sessionId":"vision-rpc","messages":[]}\n'
"#;
    let mut transport = SessionTransport::rpc_subprocess(pi::sdk::RpcTransportOptions {
        binary_path: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), script.to_string()],
        cwd: None,
    })
    .expect("RPC transport");
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let result = run_async(transport.prompt_with_images(
        "compare \"one\"\nwith two",
        images(),
        move |event| {
            let SessionTransportEvent::Rpc(event) = event else {
                panic!("RPC event expected");
            };
            captured.lock().expect("event lock").push(event);
        },
    ))
    .expect("RPC image prompt");
    let SessionPromptResult::RpcEvents(returned) = result else {
        panic!("RPC result expected");
    };
    assert_eq!(*events.lock().unwrap(), returned);
    let frame = &returned
        .iter()
        .find(|event| event["type"] == "fixture_input")
        .unwrap()["frame"];
    assert_eq!(frame["message"], "compare \"one\"\nwith two");
    assert_eq!(frame["images"], serde_json::to_value(images()).unwrap());
    assert_eq!(returned.last().unwrap()["type"], "agent_end");
    transport.shutdown().expect("shutdown transport");
}

#[cfg(unix)]
mod native_rpc {
    use super::*;
    use pi::sdk::{RpcTransportClient, RpcTransportOptions, ThinkingContent, ToolCall};

    fn client(script: &str) -> RpcTransportClient {
        RpcTransportClient::connect(RpcTransportOptions {
            // Bash's timed read bounds live-control regressions even when a
            // broken callback never sends the frame the peer is waiting for.
            binary_path: PathBuf::from("/bin/bash"),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: None,
        })
        .expect("native RPC fixture")
    }

    #[test]
    fn ordered_content_keeps_its_wire_shape_and_live_control() {
        let client = client(
            r#"
IFS= read -r -t 5 frame || exit 1
printf '{"type":"agent_start","sessionId":"native-rpc"}\n'
printf '{"type":"response","command":"prompt","id":"rpc-1","success":true}\n'
printf '{"type":"fixture_input","frame":%s}\n' "$frame"
IFS= read -r -t 5 control || exit 1
printf '{"type":"response","command":"abort","id":"rpc-2","success":true}\n'
printf '{"type":"agent_end","sessionId":"native-rpc","messages":[],"control":%s}\n' "$control"
"#,
        );
        let control = client.control_handle();
        let mut transport = SessionTransport::RpcSubprocess(client);
        let content = ordered_native_content();
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&events);
        let result = run_async(
            transport.prompt_with_content(content.clone(), move |event| {
                let SessionTransportEvent::Rpc(event) = event else {
                    panic!("RPC event expected");
                };
                if event["type"] == "fixture_input" {
                    assert_eq!(control.abort().expect("live abort"), "rpc-2");
                }
                observed.lock().unwrap().push(event);
            }),
        )
        .expect("native RPC turn");
        let SessionPromptResult::RpcEvents(returned) = result else {
            panic!("RPC result expected");
        };
        assert_eq!(*events.lock().unwrap(), returned);
        assert_eq!(
            returned
                .iter()
                .map(|event| event["type"].clone())
                .collect::<Vec<_>>(),
            [
                json!("agent_start"),
                json!("fixture_input"),
                json!("agent_end")
            ]
        );
        assert_eq!(
            returned[1]["frame"],
            json!({"type":"prompt", "id":"rpc-1", "content":content})
        );
        assert_eq!(
            returned[2]["control"],
            json!({"type":"abort", "id":"rpc-2"})
        );
        transport.shutdown().expect("shutdown transport");
    }

    #[test]
    fn media_only_content_is_normalized_and_streaming_options_are_preserved() {
        for streaming in [false, true] {
            let mut client = client(
                r#"
IFS= read -r -t 5 frame || exit 1
printf '{"type":"response","command":"prompt","id":"rpc-1","success":true}\n'
printf '{"type":"fixture_input","frame":%s}\n' "$frame"
printf '{"type":"agent_end","sessionId":"native-rpc","messages":[]}\n'
"#,
            );
            let content = vec![ContentBlock::Media(MediaContent {
                data: "Yg".to_string(),
                mime_type: "VIDEO/MP4".to_string(),
                name: Some("  motion\n.mp4  ".to_string()),
            })];
            let mut delivered = Vec::new();
            let events = if streaming {
                run_async(
                    client.prompt_with_content_streaming(content, Some("steer"), |event| {
                        delivered.push(event);
                    }),
                )
            } else {
                run_async(client.prompt_with_content(content))
            }
            .expect("media-only prompt");
            let mut expected = json!({
                "type":"prompt", "id":"rpc-1",
                "content":[{"type":"media", "data":"Yg", "mimeType":"video/mp4", "name":"motion.mp4"}]
            });
            if streaming {
                expected["streamingBehavior"] = json!("steer");
                assert_eq!(delivered, events);
            }
            assert_eq!(events[0]["frame"], expected);
            assert_eq!(events.last().unwrap()["type"], "agent_end");
            client.shutdown().expect("shutdown fixture");
        }
    }

    #[test]
    fn ordinary_media_echoes_fit_response_and_pre_ack_bounds() {
        use base64::Engine as _;

        // Both clips fit the default 5 MiB per-file policy. Their combined
        // encoded prompt exceeds the old 8 MiB response-line bound and the old
        // 4 MiB pre-ACK budget; testing it does not allocate near the new caps.
        let audio = base64::engine::general_purpose::STANDARD.encode(vec![0_u8; 5 * 1024 * 1024]);
        let video = base64::engine::general_purpose::STANDARD.encode(vec![1_u8; 1024 * 1024]);
        let content = vec![
            ContentBlock::Media(MediaContent {
                data: audio.clone(),
                mime_type: "audio/wav".to_string(),
                name: Some("voice.wav".to_string()),
            }),
            ContentBlock::Media(MediaContent {
                data: video.clone(),
                mime_type: "video/mp4".to_string(),
                name: Some("motion.mp4".to_string()),
            }),
        ];
        let mut client = client(
            r#"
IFS= read -r -t 30 frame || exit 1
printf '{"type":"message_start","message":%s}\n' "$frame"
printf '{"type":"response","command":"prompt","id":"rpc-1","success":true}\n'
printf '{"type":"agent_end","messages":[%s]}\n' "$frame"
"#,
        );
        let mut observed = Vec::new();
        let events = run_async(
            client.prompt_with_content_streaming(content, None, |event| {
                observed.push(event["type"].as_str().unwrap().to_string());
            }),
        )
        .expect("bounded native media echoes must be readable");
        assert_eq!(observed, ["message_start", "agent_end"]);
        for frame in [&events[0]["message"], &events[1]["messages"][0]] {
            assert_eq!(frame["content"].as_array().unwrap().len(), 2);
            assert!(frame["content"][0]["data"].as_str() == Some(audio.as_str()));
            assert!(frame["content"][1]["data"].as_str() == Some(video.as_str()));
        }
        client.shutdown().expect("shutdown fixture");
    }

    #[test]
    fn invalid_content_never_writes_or_allocates_request_ids() {
        let mut client = client(
            r#"
IFS= read -r -t 5 frame || exit 1
printf '{"type":"response","command":"probe","id":"rpc-1","success":true,"data":%s}\n' "$frame"
"#,
        );
        let invalid = vec![
            Vec::new(),
            vec![ContentBlock::Thinking(ThinkingContent {
                thinking: "assistant-only".to_string(),
                thinking_signature: None,
            })],
            vec![ContentBlock::ToolCall(ToolCall {
                id: "not-user-input".to_string(),
                name: "bash".to_string(),
                arguments: json!({"command":"must not run"}),
                thought_signature: None,
            })],
            vec![ContentBlock::Media(MediaContent {
                data: "YR==".to_string(),
                mime_type: "audio/wav".to_string(),
                name: None,
            })],
            vec![ContentBlock::Media(MediaContent {
                data: "YQ==".to_string(),
                mime_type: "application/json".to_string(),
                name: None,
            })],
            vec![ContentBlock::Image(ImageContent {
                data: String::new(),
                mime_type: "image/png".to_string(),
            })],
            vec![ContentBlock::Text(TextContent::new("bounded")); 257],
        ];
        for content in invalid {
            assert!(run_async(client.prompt_with_content(content.clone())).is_err());
            assert!(
                run_async(client.prompt_with_content_streaming(content, None, |_| {
                    panic!("invalid input must not deliver events");
                }))
                .is_err()
            );
        }
        let frame = run_async(client.request("probe", serde_json::Map::new()))
            .expect("invalid native content must not consume IDs or write stray frames");
        assert_eq!(frame, json!({"type":"probe", "id":"rpc-1"}));
        client.shutdown().expect("shutdown fixture");
    }

    #[test]
    fn failed_native_prompt_ack_keeps_speculative_events_private() {
        let client = client(
            r#"
IFS= read -r -t 5 frame || exit 1
printf '{"type":"agent_start","sessionId":"not-admitted"}\n'
printf '{"type":"response","command":"prompt","id":"rpc-1","success":false,"error":"media rejected"}\n'
"#,
        );
        let mut transport = SessionTransport::RpcSubprocess(client);
        let invoked = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&invoked);
        let result = run_async(transport.prompt_with_content(
            ordered_native_content(),
            move |_| {
                observed.store(true, Ordering::SeqCst);
            },
        ));
        assert!(result.is_err());
        assert!(!invoked.load(Ordering::SeqCst));
        transport.shutdown().expect("shutdown fixture");
    }
}

#[cfg(unix)]
mod live_ui {
    use super::*;
    use pi::ask::{AskAnswer, AskResponse};
    use pi::sdk::{RpcExtensionUiResponse, RpcTransportClient, RpcTransportOptions};

    fn client(script: String) -> RpcTransportClient {
        RpcTransportClient::connect(RpcTransportOptions {
            binary_path: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), script],
            cwd: None,
        })
        .expect("spawn protocol fixture")
    }

    /// The subprocess cannot send `AgentEnd` until it reads each control reply.
    /// Echo raw frames so the test checks the real writer's JSON, not a second
    /// copy of the SDK's serialization logic. Control acknowledgements are
    /// interleaved to exercise the prompt's single stdout reader.
    fn bridge(requests: &[(Value, &str)]) -> SessionTransport {
        use std::fmt::Write as _;

        let mut script = String::from(
            r#"
IFS= read -r prompt || exit 1
id=$(printf '%s\n' "$prompt" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
printf '{"type":"response","command":"prompt","id":"%s","success":true}\n' "$id"
printf '{"type":"agent_start","sessionId":"host-ui"}\n'
printf '{"type":"fixture_prompt","frame":%s}\n' "$prompt"
"#,
        );
        for (index, (request, command)) in requests.iter().enumerate() {
            let quoted = request.to_string().replace('\'', "'\\''");
            writeln!(script, "printf '%s\\n' '{quoted}'").expect("write fixture event");
            script.push_str(
                r#"
IFS= read -r reply || exit 1
printf '{"type":"fixture_reply","frame":%s}\n' "$reply"
"#,
            );
            let response = json!({
                "type": "response", "id": format!("rpc-{}", index + 2),
                "command": command, "success": true, "data": {"resolved": true}
            });
            writeln!(script, "printf '%s\\n' '{response}'").expect("write fixture ack");
        }
        script.push_str(
            r#"
printf '{"type":"agent_end","sessionId":"host-ui","messages":[]}\n'
IFS= read -r query || exit 1
id=$(printf '%s\n' "$query" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
printf '{"type":"response","command":"get_state","id":"%s","success":true,"data":{"sessionId":"host-ui-after-prompt"}}\n' "$id"
"#,
        );
        SessionTransport::RpcSubprocess(client(script))
    }

    fn finish(transport: &mut SessionTransport) {
        let state = run_async(transport.as_rpc_mut().unwrap().get_state())
            .expect("control acknowledgements must not corrupt the next request");
        assert_eq!(state.session_id, "host-ui-after-prompt");
        transport.shutdown().expect("shutdown protocol fixture");
    }

    #[test]
    fn extension_responses_unblock_live_images_and_preserve_exact_generations() {
        let responses = [
            (
                RpcExtensionUiResponse::Confirmed { confirmed: true },
                "confirmed",
                json!(true),
            ),
            (
                RpcExtensionUiResponse::Confirmed { confirmed: false },
                "confirmed",
                json!(false),
            ),
            (RpcExtensionUiResponse::Cancelled, "cancelled", json!(true)),
            (
                RpcExtensionUiResponse::Value { value: Value::Null },
                "value",
                Value::Null,
            ),
            (
                RpcExtensionUiResponse::Value {
                    value: json!({"text": "quote: \"line\"\n雪", "id": "nested-id", "type": "not-a-command"}),
                },
                "value",
                json!({"text": "quote: \"line\"\n雪", "id": "nested-id", "type": "not-a-command"}),
            ),
        ];
        for (response, field, expected) in responses {
            let request_id = "same-public-id:'\\雪";
            let generations = [41_u64, u64::MAX];
            let requests = generations.map(|generation| {
                (
                    json!({
                        "type": "extension_ui_request", "id": request_id,
                        "requestGeneration": generation, "method": "confirm",
                        "title": "An explicit host decision is required"
                    }),
                    "extension_ui_response",
                )
            });
            let mut transport = bridge(&requests);
            let control = transport.as_rpc_mut().unwrap().control_handle();
            let sent = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&sent);
            let result = run_async(transport.prompt_with_images(
                "inspect with host decisions",
                images(),
                move |event| {
                    let SessionTransportEvent::Rpc(event) = event else {
                        panic!("RPC event expected");
                    };
                    if event["type"] == "extension_ui_request" {
                        let id = control
                            .extension_ui_response(
                                event["id"].as_str().expect("UI request ID"),
                                event["requestGeneration"].as_u64().expect("generation"),
                                response.clone(),
                            )
                            .expect("write and flush UI response during prompt");
                        recorded.lock().unwrap().push(id);
                    }
                },
            ))
            .expect("live image prompt must not wait for its own mutable client borrow");
            let SessionPromptResult::RpcEvents(events) = result else {
                panic!("RPC result expected");
            };
            let frames = events
                .iter()
                .filter(|event| event["type"] == "fixture_reply")
                .map(|event| &event["frame"])
                .collect::<Vec<_>>();
            assert_eq!(frames.len(), 2);
            assert_eq!(*sent.lock().unwrap(), ["rpc-2", "rpc-3"]);
            for (index, frame) in frames.iter().enumerate() {
                assert_eq!(frame["id"], format!("rpc-{}", index + 2));
                assert_eq!(frame["type"], "extension_ui_response");
                assert_eq!(frame["requestId"], request_id);
                assert_eq!(frame["requestGeneration"], generations[index]);
                assert_eq!(frame[field], expected);
                assert_eq!(
                    frame.as_object().unwrap().len(),
                    5,
                    "exactly one answer field"
                );
            }
            let prompt = &events
                .iter()
                .find(|event| event["type"] == "fixture_prompt")
                .unwrap()["frame"];
            assert_eq!(prompt["id"], "rpc-1");
            assert_eq!(prompt["images"], serde_json::to_value(images()).unwrap());
            assert_eq!(events.last().unwrap()["type"], "agent_end");
            finish(&mut transport);
        }
    }

    #[test]
    fn question_cards_accept_an_explicit_response_from_a_separate_ui_thread() {
        let choices = [
            AskResponse {
                answers: vec![
                    AskAnswer {
                        question_id: "multi".to_string(),
                        selected: vec!["one".to_string(), "two".to_string()],
                        other: None,
                    },
                    AskAnswer {
                        question_id: "free-text".to_string(),
                        selected: Vec::new(),
                        other: Some("Use \"this\"\n雪".to_string()),
                    },
                ],
                dismissed: false,
            },
            AskResponse {
                answers: vec![AskAnswer {
                    question_id: "permission".to_string(),
                    selected: vec!["Deny".to_string()],
                    other: None,
                }],
                dismissed: false,
            },
            AskResponse {
                // A cancelled card must not accidentally send a stale approval.
                answers: vec![AskAnswer {
                    question_id: "permission".to_string(),
                    selected: vec!["Allow once".to_string()],
                    other: None,
                }],
                dismissed: true,
            },
        ];
        for choice in choices {
            let expected = choice.clone();
            let requests = [(
                json!({
                    "type": "ask_request", "id": "host-card", "timeoutMs": 10000,
                    "questions": [{"id": "permission", "question": "Allow?",
                        "options": [{"label": "Deny"}, {"label": "Allow once"}]}]
                }),
                "ask_response",
            )];
            let mut transport = bridge(&requests);
            let control = transport.as_rpc_mut().unwrap().control_handle();
            let (request_tx, request_rx) = std::sync::mpsc::channel::<Value>();
            let ui = std::thread::spawn(move || {
                let event = request_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("UI thread receives pending question");
                control
                    .ask_response(event["id"].as_str().expect("ask ID"), choice)
                    .expect("UI thread writes explicit response")
            });
            let result = run_async(transport.prompt_with_images(
                "host-driven question",
                images(),
                move |event| {
                    if let SessionTransportEvent::Rpc(event) = event
                        && event["type"] == "ask_request"
                    {
                        request_tx.send(event).expect("deliver question to UI");
                    }
                },
            ))
            .expect("question must resolve while the prompt is active");
            assert_eq!(ui.join().expect("UI thread joined"), "rpc-2");
            let SessionPromptResult::RpcEvents(events) = result else {
                panic!("RPC result expected");
            };
            let frame = &events
                .iter()
                .find(|event| event["type"] == "fixture_reply")
                .unwrap()["frame"];
            assert_eq!(frame["id"], "rpc-2");
            assert_eq!(frame["type"], "ask_response");
            assert_eq!(frame["requestId"], "host-card");
            if expected.dismissed {
                assert_eq!(frame["dismissed"], true);
                assert!(frame.get("answers").is_none());
            } else {
                assert_eq!(
                    frame["answers"],
                    serde_json::to_value(expected.answers).unwrap()
                );
                assert!(frame.get("dismissed").is_none());
            }
            assert_eq!(frame.as_object().unwrap().len(), 4);
            assert_eq!(events.last().unwrap()["type"], "agent_end");
            finish(&mut transport);
        }
    }

    #[test]
    fn blank_ui_ids_are_rejected_before_writing_or_allocating_request_ids() {
        let mut client = client(
            r#"
IFS= read -r frame || exit 1
printf '{"type":"response","command":"probe","id":"rpc-1","success":true,"data":%s}\n' "$frame"
"#
            .to_string(),
        );
        let control = client.control_handle();
        for request_id in ["", " ", "\n\t"] {
            assert!(
                control
                    .extension_ui_response(request_id, 1, RpcExtensionUiResponse::Cancelled)
                    .is_err()
            );
            assert!(
                control
                    .ask_response(
                        request_id,
                        AskResponse {
                            answers: Vec::new(),
                            dismissed: true,
                        },
                    )
                    .is_err()
            );
        }
        let frame = run_async(client.request("probe", serde_json::Map::new()))
            .expect("invalid UI IDs must not consume IDs or write stray frames");
        assert_eq!(frame, json!({"type": "probe", "id": "rpc-1"}));
        client.shutdown().expect("shutdown fixture");
    }

    #[test]
    fn failed_prompt_ack_does_not_release_speculative_ui_requests() {
        let client = client(
            r#"
IFS= read -r frame || exit 1
printf '{"type":"extension_ui_request","id":"speculative","requestGeneration":17,"method":"confirm"}\n'
printf '{"type":"response","command":"prompt","id":"rpc-1","success":false,"error":"prompt refused"}\n'
"#
            .to_string(),
        );
        let control = client.control_handle();
        let invoked = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&invoked);
        let mut transport = SessionTransport::RpcSubprocess(client);
        let result = run_async(
            transport.prompt_with_images("refused", images(), move |event| {
                observed.store(true, Ordering::SeqCst);
                if let SessionTransportEvent::Rpc(event) = event
                    && event["type"] == "extension_ui_request"
                {
                    let _ = control.extension_ui_response(
                        event["id"].as_str().unwrap(),
                        event["requestGeneration"].as_u64().unwrap(),
                        RpcExtensionUiResponse::Cancelled,
                    );
                }
            }),
        );
        assert!(result.is_err());
        assert!(!invoked.load(Ordering::SeqCst));
        transport.shutdown().expect("shutdown fixture");
    }

    #[test]
    fn ui_control_reports_a_closed_pipe_instead_of_claiming_delivery() {
        let mut client = client("IFS= read -r frame".to_string());
        let control = client.control_handle();
        client.shutdown().expect("stop subprocess before reply");
        assert!(
            control
                .extension_ui_response("pending", 17, RpcExtensionUiResponse::Cancelled)
                .is_err()
        );
        assert!(
            control
                .ask_response(
                    "pending-card",
                    AskResponse {
                        answers: Vec::new(),
                        dismissed: true,
                    },
                )
                .is_err()
        );
    }
}
