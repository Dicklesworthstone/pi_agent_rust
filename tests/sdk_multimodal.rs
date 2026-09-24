//! Multimodal SDK prompts through the real provider, HTTP/SSE and session paths.
#![recursion_limit = "256"]

use asupersync::runtime::RuntimeBuilder;
use asupersync::runtime::reactor::create_reactor;
use asupersync::sync::Mutex as AsyncMutex;
use pi::failover::RetryPolicy;
use pi::sdk::{
    AbortHandle, Agent, AgentConfig, AgentEvent, AgentSession, AgentSessionHandle, ContentBlock,
    Error, EventListeners, FailoverOptions, ImageContent, InputType, Message,
    ResolvedCompactionSettings, Session, SessionPromptResult, SessionTransport,
    SessionTransportEvent, StopReason, StreamOptions, ToolRegistry, UserContent,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+j5mEAAAAASUVORK5CYII=";
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

fn read_request(stream: &mut TcpStream, deadline: Instant) -> Value {
    const LIMIT: usize = 256 * 1024;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let (header_end, body_len) = loop {
        assert!(Instant::now() < deadline, "fixture headers timed out");
        let read = stream.read(&mut buffer).expect("read request headers");
        assert!(read > 0, "request ended before headers");
        bytes.extend_from_slice(&buffer[..read]);
        assert!(bytes.len() <= LIMIT, "oversized fixture request");
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).expect("ASCII headers");
            assert!(headers.lines().next().expect("request line").contains("/chat/completions"));
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
        let read = stream.read(&mut buffer).expect("read request body");
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

fn retry_policy(retries: u32, failovers: u32) -> RetryPolicy {
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
    assert_eq!(users.len(), 1, "recovery must not duplicate the user prompt");
    &users[0]["content"]
}

fn assert_wire_images(request: &Value, text: &str) {
    let blocks = user_wire_content(request).as_array().expect("multimodal wire content");
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
        [format!("data:image/png;base64,{PNG}"), format!("data:image/gif;base64,{GIF}")]
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
    let path = handle.session_store().try_lock().expect("session lock").path.clone().expect("saved path");
    run_async(Session::open(&path.display().to_string())).expect("reopen session")
}

#[test]
fn sdk_mml_img_retry_preserves_wire_attachments_and_one_durable_prompt() {
    for explicit_abort in [false, true] {
        let root = tempfile::tempdir().expect("tempdir");
        let mut server = ApiFixture::new(vec![503, 200]);
        let mut handle = handle(&server.url, root.path(), false)
            .with_retry(Some(retry_policy(1, 0)));
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let callback = move |event| captured.lock().expect("event lock").push(event);
        let message = run_async(async {
            if explicit_abort {
                let (_abort, signal) = AbortHandle::new();
                handle
                    .prompt_with_images_with_abort("compare these images", images(), signal, callback)
                    .await
            } else {
                handle.prompt_with_images("compare these images", images(), callback).await
            }
        })
        .expect("image turn recovered");
        assert_eq!(message.stop_reason, StopReason::Stop);
        for request in server.finish(2) {
            assert_wire_images(&request, "compare these images");
        }
        let stored = reopen(&handle).to_messages_for_current_path();
        assert_stored_images(&stored);
        assert_eq!(stored.len(), 2, "only user and final assistant remain");
        let events = events.lock().expect("event lock");
        assert_eq!(events.iter().filter(|event| matches!(event, AgentEvent::AgentEnd { .. })).count(), 1);
        let Some(AgentEvent::AgentEnd { messages, error, .. }) = events.last() else {
            panic!("terminal event must follow recovery");
        };
        assert!(error.is_none());
        assert_eq!(serde_json::to_value(messages).unwrap(), serde_json::to_value(stored).unwrap());
    }
}

#[test]
fn sdk_mml_img_failover_keeps_the_original_images_on_the_new_model() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut server = ApiFixture::new(vec![503, 200]);
    let mut handle = handle(&server.url, root.path(), false)
        .with_retry(Some(retry_policy(0, 1)))
        .with_failover(Some(FailoverOptions {
            chains: HashMap::from([("default".to_string(), vec!["openai/vision-fallback".to_string()])]),
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
    let (abort, signal) = AbortHandle::new();
    abort.abort();
    let result = run_async(handle.prompt_with_images_with_abort("discard", images(), signal, |_| {
        panic!("a pre-aborted prompt must not emit a started lifecycle");
    }));
    assert!(matches!(result, Err(Error::Aborted)));
    server.finish(0);
    assert!(run_async(handle.messages()).unwrap().is_empty());
    assert!(handle.session_store().try_lock().unwrap().path.is_none());
}

#[test]
fn sdk_mml_img_backoff_abort_keeps_attachments_without_reissuing() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut server = ApiFixture::new(vec![503, 200]);
    let mut handle = handle(&server.url, root.path(), false)
        .with_retry(Some(retry_policy(2, 0)));
    let (abort, signal) = AbortHandle::new();
    let result = run_async(handle.prompt_with_images_with_abort("keep images", images(), signal, move |event| {
        if matches!(event, AgentEvent::AutoRetryStart { .. }) {
            abort.abort();
        }
    }));
    assert!(matches!(result, Err(Error::Aborted)));
    assert_wire_images(&server.finish(1)[0], "keep images");
    assert_stored_images(&reopen(&handle).to_messages_for_current_path());
}

#[test]
fn sdk_mml_img_transport_supports_image_only_prompts() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut server = ApiFixture::new(vec![200]);
    let mut transport = SessionTransport::InProcess(Box::new(handle(&server.url, root.path(), false)));
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

#[test]
fn sdk_mml_img_failed_retry_save_fences_later_image_prompts() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut server = ApiFixture::new(vec![503, 200]);
    let mut handle = handle(&server.url, root.path(), false)
        .with_retry(Some(retry_policy(1, 0)));
    let blocked = root.path().join("directory-not-session.jsonl");
    std::fs::create_dir(&blocked).expect("blocked persistence path");
    let store = handle.session_store();
    let original = Arc::new(Mutex::new(None::<PathBuf>));
    let captured = Arc::clone(&original);
    let result = run_async(handle.prompt_with_images("persist once", images(), move |event| {
        if matches!(event, AgentEvent::AutoRetryStart { .. }) {
            let mut session = store.try_lock().expect("between-attempt session lock");
            *captured.lock().expect("path lock") = session.path.clone();
            session.path = Some(blocked.clone());
        }
    }));
    assert!(result.as_ref().is_err_and(Error::is_session_persistence));
    handle.session_store().try_lock().unwrap().path = original.lock().unwrap().clone();
    let before = serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap();
    let again = run_async(handle.prompt_with_images("must not append", images(), |_| {}));
    assert!(again.as_ref().is_err_and(Error::is_session_persistence));
    assert_eq!(before, serde_json::to_value(run_async(handle.messages()).unwrap()).unwrap());
    assert_stored_images(&reopen(&handle).to_messages_for_current_path());
    server.finish(1);
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
    let result = run_async(transport.prompt_with_images("compare \"one\"\nwith two", images(), move |event| {
        let SessionTransportEvent::Rpc(event) = event else {
            panic!("RPC event expected");
        };
        captured.lock().expect("event lock").push(event);
    }))
    .expect("RPC image prompt");
    let SessionPromptResult::RpcEvents(returned) = result else {
        panic!("RPC result expected");
    };
    assert_eq!(*events.lock().unwrap(), returned);
    let frame = &returned.iter().find(|event| event["type"] == "fixture_input").unwrap()["frame"];
    assert_eq!(frame["message"], "compare \"one\"\nwith two");
    assert_eq!(frame["images"], serde_json::to_value(images()).unwrap());
    assert_eq!(returned.last().unwrap()["type"], "agent_end");
    transport.shutdown().expect("shutdown transport");
}
