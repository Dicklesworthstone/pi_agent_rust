//! Real-path tests for the minimal streaming HTTP client (`src/http/client.rs`).
//!
//! These tests use a tiny local TCP server that returns canned HTTP/1.1 bytes
//! (no mocks of the client itself) to exercise request building, header parsing,
//! streaming body delivery, and timeout behavior.

mod common;

use common::TestHarness;
use pi::http::client::Client;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

struct OneShotServer {
    addr: SocketAddr,
    join: JoinHandle<()>,
}

impl OneShotServer {
    fn start(handler: impl FnOnce(TcpStream, Vec<u8>) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
        let addr = listener.local_addr().expect("server addr");
        let join = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
            let request = read_http_request(&mut stream);
            handler(stream, request);
        });
        Self { addr, join }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.addr.port())
    }

    fn join(self) {
        self.join.join().expect("server thread");
    }
}

fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut scratch = [0u8; 4096];

    loop {
        match stream.read(&mut scratch) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&scratch[..n]);
                if let Some(headers_end) = find_double_crlf(&buf) {
                    let body_len = parse_content_length(&buf[..headers_end]).unwrap_or(0);
                    while buf.len() < headers_end + body_len {
                        match stream.read(&mut scratch) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&scratch[..n]),
                        }
                    }
                    break;
                }
            }
        }
    }

    buf
}

fn parse_content_length(headers: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(headers);
    for line in text.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse::<usize>().ok();
        }
    }
    None
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn write_logs_artifact(harness: &TestHarness) {
    let path = harness.temp_path("logs.jsonl");
    harness
        .write_jsonl_logs_normalized(&path)
        .expect("write jsonl logs");
    harness.record_artifact("logs_jsonl", &path);
}

#[test]
fn request_builder_sends_headers_and_json_body() {
    let harness = TestHarness::new("http_client_request_builder_sends_headers_and_json_body");

    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_server = Arc::clone(&captured);
    let server = OneShotServer::start(move |mut stream, request| {
        *captured_server.lock().expect("capture lock") = request;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .expect("write response");
    });

    let url = server.url("/hello");
    common::run_async(async move {
        let payload = serde_json::json!({"x": 1});
        let response = Client::new()
            .post(&url)
            .header("X-Test", "1")
            .json(&payload)
            .expect("json body")
            .send()
            .await
            .expect("send");
        assert_eq!(response.status(), 200);
    });

    server.join();

    let request = captured.lock().expect("capture lock").clone();
    let request_path = harness.temp_path("request.bin");
    std::fs::write(&request_path, &request).expect("write request artifact");
    harness.record_artifact("request", &request_path);

    let request_text = String::from_utf8_lossy(&request);
    assert!(request_text.starts_with("POST /hello HTTP/1.1\r\n"));
    assert!(request_text.contains("Host: 127.0.0.1\r\n"));
    assert!(request_text.contains("User-Agent: pi_agent_rust/0.1\r\n"));
    assert!(request_text.contains("Content-Type: application/json\r\n"));
    assert!(request_text.contains("X-Test: 1\r\n"));

    write_logs_artifact(&harness);
}

#[test]
fn response_parses_status_headers_and_body_content_length() {
    let harness = TestHarness::new("http_client_response_parses_status_headers_and_body");

    let server = OneShotServer::start(|mut stream, _request| {
        let response = concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Length: 11\r\n",
            "X-Foo: bar\r\n",
            "\r\n",
            "hello world"
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    });

    let url = server.url("/ok");
    let body = common::run_async(async move {
        let response = Client::new().get(&url).send().await.expect("send");
        assert_eq!(response.status(), 200);
        assert!(
            response
                .headers()
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("x-foo") && v == "bar")
        );
        response.text().await.expect("text")
    });

    server.join();
    assert_eq!(body, "hello world");
    write_logs_artifact(&harness);
}

#[test]
fn response_streams_chunked_body() {
    let harness = TestHarness::new("http_client_response_streams_chunked_body");

    let server = OneShotServer::start(|mut stream, _request| {
        let head = concat!(
            "HTTP/1.1 200 OK\r\n",
            "Transfer-Encoding: chunked\r\n",
            "\r\n"
        );
        stream.write_all(head.as_bytes()).expect("write head");
        // "hello world" split across two chunks.
        stream
            .write_all(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n")
            .expect("write body");
    });

    let url = server.url("/chunked");
    let body = common::run_async(async move {
        let response = Client::new().get(&url).send().await.expect("send");
        assert_eq!(response.status(), 200);
        response.text().await.expect("text")
    });

    server.join();
    assert_eq!(body, "hello world");
    write_logs_artifact(&harness);
}

#[test]
fn malformed_header_line_is_error() {
    let harness = TestHarness::new("http_client_malformed_header_line_is_error");

    let server = OneShotServer::start(|mut stream, _request| {
        let response = concat!("HTTP/1.1 200 OK\r\n", "BadHeaderLine\r\n", "\r\n");
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    });

    let url = server.url("/bad-header");
    let err = common::run_async(async move {
        Client::new()
            .get(&url)
            .send()
            .await
            .err()
            .expect("expected error")
    });

    server.join();
    let message = err.to_string();
    assert!(
        message.contains("Invalid HTTP header line"),
        "unexpected error: {message}"
    );
    write_logs_artifact(&harness);
}

#[test]
fn oversized_response_headers_is_error() {
    let harness = TestHarness::new("http_client_oversized_response_headers_is_error");

    let server = OneShotServer::start(|mut stream, _request| {
        let padding = "a".repeat(70 * 1024);
        let response = format!("HTTP/1.1 200 OK\r\nX-Pad: {padding}\r\n\r\n");
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    });

    let url = server.url("/huge-headers");
    let err = common::run_async(async move {
        Client::new()
            .get(&url)
            .send()
            .await
            .err()
            .expect("expected error")
    });

    server.join();
    let message = err.to_string();
    assert!(
        message.contains("HTTP response headers too large"),
        "unexpected error: {message}"
    );
    write_logs_artifact(&harness);
}

#[test]
fn request_timeout_is_error() {
    let harness = TestHarness::new("http_client_request_timeout_is_error");

    let server = OneShotServer::start(|mut stream, _request| {
        // Hold the connection open without sending response bytes.
        thread::sleep(Duration::from_millis(150));
        let _ = stream.write_all(b"");
    });

    let url = server.url("/timeout");
    let err = common::run_async(async move {
        Client::new()
            .get(&url)
            .timeout(Duration::from_millis(30))
            .send()
            .await
            .err()
            .expect("expected timeout")
    });

    server.join();
    let message = err.to_string();
    assert!(
        message.contains("Request timed out"),
        "unexpected error: {message}"
    );
    write_logs_artifact(&harness);
}

#[test]
fn chunked_size_line_exceeding_buffer_is_error() {
    let harness = TestHarness::new("http_client_chunked_size_line_exceeding_buffer_is_error");

    let server = OneShotServer::start(|mut stream, _request| {
        let head = concat!(
            "HTTP/1.1 200 OK\r\n",
            "Transfer-Encoding: chunked\r\n",
            "\r\n"
        );
        stream.write_all(head.as_bytes()).expect("write head");
        // Send an invalid (unterminated) chunk size line that exceeds the client's
        // MAX_BUFFERED_BYTES, triggering a deterministic buffer limit error.
        let oversized = vec![b'a'; 300 * 1024];
        stream.write_all(&oversized).expect("write body");
    });

    let url = server.url("/chunked-oversize");
    let err = common::run_async(async move {
        let response = Client::new().get(&url).send().await.expect("send");
        response.text().await.expect_err("expected error")
    });

    server.join();
    let message = err.to_string();
    assert!(
        message.contains("HTTP body buffer exceeded"),
        "unexpected error: {message}"
    );
    write_logs_artifact(&harness);
}
