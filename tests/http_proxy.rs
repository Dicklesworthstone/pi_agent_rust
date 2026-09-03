//! End-to-end proxy wiring for the HTTP client (#210).
//!
//! No external network: a loopback `TcpListener` plays the proxy, so the test
//! proves the client actually dials the configured proxy, speaks absolute-form
//! for `http://` targets, and forwards the response back to the caller.

mod common;

use pi::http::client::Client;
use pi::http::proxy::{HttpSettings, ProxyConfig, install};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Mutex, MutexGuard, OnceLock, mpsc};

/// The proxy configuration is process-global, so these tests must not overlap.
fn proxy_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Settings that pin exactly one proxy and ignore the ambient environment, so
/// the test is independent of the machine it runs on.
fn pinned_settings(port: u16) -> HttpSettings {
    HttpSettings {
        proxy: Some(format!("http://user:pass@127.0.0.1:{port}")),
        ignore_env_proxy: Some(true),
        ..HttpSettings::default()
    }
}

/// Accept one connection, read the request head, reply with a fixed body, and
/// report the request line + headers back to the test.
fn spawn_fake_proxy(listener: TcpListener, sender: mpsc::Sender<Vec<String>>) {
    // ubs:ignore detached on purpose — the thread ends with its one connection
    // and the test synchronizes through the channel, not a JoinHandle.
    std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut reader = BufReader::new(stream);
        let mut head = Vec::new();
        loop {
            let mut line = String::new();
            // EOF and a read error both end the head; nothing else to collect.
            let Ok(read) = reader.read_line(&mut line) else {
                break;
            };
            if read == 0 {
                break;
            }
            let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
            if trimmed.is_empty() {
                break;
            }
            head.push(trimmed);
        }
        let mut stream = reader.into_inner();
        let body = b"proxied-ok";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.write_all(body);
        let _ = stream.flush();
        let _ = sender.send(head);
    });
}

/// The configured proxy receives the request (absolute-form, with credentials)
/// and its response reaches the caller — the origin host is never dialed.
#[test]
fn http_request_goes_through_the_configured_proxy() {
    let _guard = proxy_guard();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake proxy");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = mpsc::channel();
    spawn_fake_proxy(listener, tx);

    let (config, warnings) = ProxyConfig::resolve(Some(&pinned_settings(port)), &|_name| None);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert!(!config.is_empty(), "proxy must be configured");
    install(config);

    // `example.invalid` is guaranteed not to resolve (RFC 6761), so a request
    // that succeeds proves the proxy hop was used rather than a direct dial.
    let body = common::run_async(async move {
        let client = Client::new();
        let response = client
            .get("http://example.invalid/api/test?x=1")
            .send()
            .await
            .expect("request through proxy");
        assert_eq!(response.status(), 200);
        response.text().await.expect("body")
    });
    assert_eq!(body, "proxied-ok");

    let head = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("fake proxy received a request");
    assert_eq!(
        head.first().map(String::as_str),
        Some("GET http://example.invalid/api/test?x=1 HTTP/1.1"),
        "expected absolute-form request line: {head:?}"
    );
    assert!(
        head.iter().any(|line| line == "Host: example.invalid"),
        "origin Host header must be preserved: {head:?}"
    );
    assert!(
        head.iter()
            .any(|line| line == "Proxy-Authorization: Basic dXNlcjpwYXNz"),
        "proxy credentials must be sent: {head:?}"
    );

    install(ProxyConfig::default());
}

/// A `noProxy` match sends the request direct — proven by the request failing
/// to resolve `example.invalid` instead of reaching the listener.
#[test]
fn no_proxy_host_bypasses_the_proxy() {
    let _guard = proxy_guard();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake proxy");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = mpsc::channel();
    spawn_fake_proxy(listener, tx);

    let mut settings = pinned_settings(port);
    settings.no_proxy = Some(vec!["example.invalid".to_string()]);
    let (config, _warnings) = ProxyConfig::resolve(Some(&settings), &|_name| None);
    install(config);

    let result = common::run_async(async move {
        let client = Client::new();
        client
            .get("http://example.invalid/api/test")
            .send()
            .await
            .map(|_| ())
    });
    assert!(
        result.is_err(),
        "a bypassed host must be dialed directly (and fail to resolve)"
    );
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(200))
            .is_err(),
        "the proxy must not have been contacted"
    );

    install(ProxyConfig::default());
}
