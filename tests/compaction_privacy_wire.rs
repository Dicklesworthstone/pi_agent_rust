//! Compaction privacy through the native OpenAI-compatible HTTP/SSE boundary.
//! Only the remote endpoint is a fixture; projections, prompt construction,
//! provider serialization, streaming, and result screening are production code.
#![recursion_limit = "256"]

use asupersync::runtime::RuntimeBuilder;
use asupersync::runtime::reactor::create_reactor;
use pi::compaction::{
    CompactionPreparation, CompactionPrivacy, FileOperations, ResolvedCompactionSettings, compact,
    compaction_preparation_to_value,
};
use pi::model::{AssistantMessage, ContentBlock, StopReason, ToolCall, UserContent};
use pi::provider::{Provider, StreamOptions};
use pi::session::SessionMessage;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const NUMERIC_KEY: u64 = 1_234_567_890_123_456;
const PATTERN_KEY: u64 = 731_942;
const AUTH: &str = "test-only-credential";
const WAIT: Duration = Duration::from_secs(15);

fn run_async<F: std::future::Future>(future: F) -> F::Output {
    let runtime = RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("reactor"))
        .build()
        .expect("runtime");
    runtime.block_on(Box::pin(async {
        asupersync::time::timeout(
            asupersync::time::wall_now(),
            Duration::from_secs(45),
            future,
        )
        .await
        .expect("compaction exceeded the wire test deadline")
    }))
}

fn provider(url: &str) -> Arc<dyn Provider> {
    let mut entry =
        pi::models::ad_hoc_model_entry("openai", "compaction-wire").expect("model entry");
    entry.model.api = "openai-completions".to_string();
    entry.model.base_url = url.to_string();
    pi::providers::create_provider(&entry, None).expect("native provider")
}

fn options() -> StreamOptions {
    StreamOptions {
        api_key: Some(AUTH.to_string()),
        ..StreamOptions::default()
    }
}

const fn user(text: String) -> SessionMessage {
    SessionMessage::User {
        content: UserContent::Text(text),
        timestamp: Some(0),
    }
}

fn preparation(arguments: Value) -> CompactionPreparation {
    CompactionPreparation {
        first_kept_entry_id: "unchanged-cut".to_string(),
        messages_to_summarize: vec![user(format!(
            "completed history: earlier echoes {NUMERIC_KEY} and {PATTERN_KEY}"
        ))],
        turn_prefix_messages: vec![SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "unchanged-call".to_string(),
                    name: "configure".to_string(),
                    arguments,
                    thought_signature: None,
                })],
                stop_reason: StopReason::ToolUse,
                ..AssistantMessage::default()
            },
        }],
        is_split_turn: true,
        tokens_before: 600,
        previous_summary: Some(format!("previous summary: {NUMERIC_KEY}")),
        file_ops: FileOperations::default(),
        settings: ResolvedCompactionSettings::default(),
    }
}

struct CapturedRequest {
    authorization: String,
    body: Value,
}

/// Bound both bytes and elapsed time. Socket timeouts are retried against one
/// deadline; on macOS they may surface as `WouldBlock` rather than `TimedOut`.
fn read_request(stream: &mut TcpStream, deadline: Instant) -> CapturedRequest {
    const LIMIT: usize = 256 * 1024;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        assert!(
            Instant::now() < deadline,
            "request exceeded fixture deadline"
        );
        match stream.read(&mut buffer) {
            Ok(0) => panic!("request ended before its declared body"),
            Ok(count) => bytes.extend_from_slice(&buffer[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => panic!("fixture request read failed: {error}"),
        }
        assert!(bytes.len() <= LIMIT, "oversized fixture request");
        let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&bytes[..end]).expect("request headers");
        assert!(headers.starts_with("POST /v1/chat/completions "));
        let header = |name: &str| {
            headers.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case(name).then(|| value.trim())
            })
        };
        let length = header("content-length")
            .expect("content length")
            .parse::<usize>()
            .expect("numeric content length");
        assert!(length <= LIMIT - (end + 4));
        let body_end = end + 4 + length;
        if bytes.len() >= body_end {
            return CapturedRequest {
                authorization: header("authorization").expect("authorization").to_string(),
                body: serde_json::from_slice(&bytes[end + 4..body_end]).expect("request JSON"),
            };
        }
    }
}

struct WireFixture {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<Vec<CapturedRequest>>>,
}

impl WireFixture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let url = format!("http://{}/v1", listener.local_addr().expect("address"));
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for label in ["HISTORY", "PREFIX"] {
                let deadline = Instant::now() + WAIT;
                let mut stream = loop {
                    if stopped.load(Ordering::SeqCst) {
                        return requests;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "fixture request timed out");
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("fixture accept failed: {error}"),
                    }
                };
                stream
                    .set_nonblocking(false)
                    .expect("blocking accepted socket");
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .expect("read timeout");
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .expect("write timeout");
                requests.push(read_request(&mut stream, deadline));
                // A secondary model can echo a remembered value. Its answer
                // must pass result screening before it becomes the summary.
                let chunk = json!({
                    "id": "compaction-fixture", "object": "chat.completion.chunk",
                    "created": 0, "model": "compaction-wire",
                    "choices": [{"index": 0, "delta": {
                        "role": "assistant", "content": format!("{label}: {NUMERIC_KEY} {PATTERN_KEY}")
                    }, "finish_reason": null}]
                });
                let done = json!({"id": "compaction-fixture", "choices": [
                    {"index": 0, "delta": {}, "finish_reason": "stop"}
                ]});
                let body = format!("data: {chunk}\n\ndata: {done}\n\ndata: [DONE]\n\n");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len(),
                )
                .expect("write SSE response");
                stream.flush().expect("flush response");
            }
            requests
        });
        Self {
            url,
            stop,
            worker: Some(worker),
        }
    }

    fn finish(&mut self) -> Vec<CapturedRequest> {
        self.stop.store(true, Ordering::SeqCst);
        self.worker
            .take()
            .expect("fixture worker")
            .join()
            .expect("fixture completed")
    }
}

impl Drop for WireFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn user_prompt(body: &Value) -> String {
    let mut text = String::new();
    for message in body["messages"].as_array().expect("serialized messages") {
        if message["role"] != "user" {
            continue;
        }
        match &message["content"] {
            Value::String(content) => text.push_str(content),
            Value::Array(parts) => {
                for part in parts {
                    assert_eq!(part["type"], "text", "summarization is text-only");
                    text.push_str(part["text"].as_str().expect("text part"));
                }
            }
            _ => panic!("invalid user prompt content"),
        }
    }
    text
}

#[test]
fn numeric_secrets_compact_through_both_native_requests_without_losing_source_types() {
    for mode in ["obfuscate", "off"] {
        let mut fixture = WireFixture::new();
        let privacy = CompactionPrivacy::from_settings(Some(&pi::secrets::SecretsSettings {
            mode: Some(mode.to_string()),
            extra_patterns: Some(vec![r"^731942$".to_string()]),
        }));
        let prep = preparation(json!({
            "api_key": NUMERIC_KEY, "project_code": PATTERN_KEY,
            "limit": 3, "recursive": true, "optional": null
        }));
        let original = compaction_preparation_to_value(&prep);
        let result = run_async(compact(
            prep.clone(),
            provider(&fixture.url),
            &options(),
            Some("Preserve the active constraints"),
            &privacy,
        ));
        let requests = fixture.finish();
        let result = result.expect("numeric credentials must not prevent compaction");
        assert_eq!(requests.len(), 2, "split turns need both summaries");
        assert_eq!(result.first_kept_entry_id, "unchanged-cut");
        assert_eq!(result.tokens_before, 600);
        assert!(result.summary.contains("HISTORY"));
        assert!(result.summary.contains("PREFIX"));
        assert!(result.summary.contains("<pi-secret:redacted>"));
        for secret in [NUMERIC_KEY.to_string(), PATTERN_KEY.to_string()] {
            assert!(
                !result.summary.contains(&secret),
                "provider echo escaped screening"
            );
            for request in &requests {
                assert!(
                    !request.body.to_string().contains(&secret),
                    "secret reached HTTP"
                );
            }
        }
        for request in &requests {
            assert_eq!(request.authorization, format!("Bearer {AUTH}"));
            assert!(user_prompt(&request.body).contains("<pi-secret:redacted>"));
            assert!(!request.body.to_string().contains("<pi-secret:000"));
            assert_eq!(request.body["model"], "compaction-wire");
        }
        assert!(user_prompt(&requests[0].body).contains("completed history"));
        assert!(user_prompt(&requests[1].body).contains("configure"));
        assert_eq!(compaction_preparation_to_value(&prep), original);
        let SessionMessage::Assistant { message } = &prep.turn_prefix_messages[0] else {
            panic!("source assistant retained");
        };
        let ContentBlock::ToolCall(call) = &message.content[0] else {
            panic!("source tool call retained");
        };
        assert_eq!(call.arguments["api_key"], json!(NUMERIC_KEY));
        assert_eq!(call.arguments["limit"], json!(3));
        assert_eq!(call.arguments["recursive"], json!(true));
        assert!(call.arguments["optional"].is_null());
    }
}

#[test]
fn block_mode_refuses_numeric_boolean_and_null_secrets_before_any_http_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind refusal probe");
    listener.set_nonblocking(true).expect("nonblocking probe");
    let url = format!("http://{}/v1", listener.local_addr().expect("address"));
    let privacy = CompactionPrivacy::from_settings(Some(&pi::secrets::SecretsSettings {
        mode: Some("block".to_string()),
        extra_patterns: Some(vec![r"^(731942|true|null)$".to_string()]),
    }));
    for arguments in [
        json!({"api_key": NUMERIC_KEY}),
        json!({"project_code": PATTERN_KEY}),
        json!({"enabled": true}),
        json!({"optional": null}),
    ] {
        let prep = preparation(arguments);
        let original = compaction_preparation_to_value(&prep);
        let error = run_async(compact(
            prep.clone(),
            provider(&url),
            &options(),
            None,
            &privacy,
        ))
        .expect_err("block policy must be enforced before the first split request");
        assert!(error.to_string().contains("PI_SECRET_BLOCK"));
        assert!(!error.to_string().contains(&NUMERIC_KEY.to_string()));
        assert_eq!(compaction_preparation_to_value(&prep), original);
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }
}

#[test]
fn colliding_redacted_tool_argument_keys_refuse_without_sending_or_mutating_history() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind refusal probe");
    listener.set_nonblocking(true).expect("nonblocking probe");
    let url = format!("http://{}/v1", listener.local_addr().expect("address"));
    let privacy = CompactionPrivacy::from_settings(Some(&pi::secrets::SecretsSettings {
        mode: Some("obfuscate".to_string()),
        extra_patterns: Some(vec![r"^ACME-[0-9]{4}$".to_string()]),
    }));
    let prep = preparation(json!({"ACME-1111": 1, "ACME-2222": 2}));
    let original = compaction_preparation_to_value(&prep);
    let error = run_async(compact(
        prep.clone(),
        provider(&url),
        &options(),
        None,
        &privacy,
    ))
    .expect_err("ambiguous redaction cannot silently discard an argument");
    assert!(error.to_string().contains("PI_AUXILIARY_KEY_COLLISION"));
    assert!(!error.to_string().contains("ACME-"));
    assert_eq!(compaction_preparation_to_value(&prep), original);
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
}
