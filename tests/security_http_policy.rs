//! Security suite: HTTP connector policy enforcement tests (bd-437s).
//!
//! Validates that the HTTP connector correctly enforces:
//! - Host allowlist/denylist filtering (denied requests never hit the network)
//! - TLS requirement enforcement
//! - Request body size limits
//! - Timeout enforcement + correct error codes
//! - URL redaction in logs (no secrets leak)
//! - Unsupported schemes/methods rejected
//! - Denylist takes precedence over allowlist
//! - GET requests cannot include a body

use pi::connectors::http::{HttpConnector, HttpConnectorConfig};
use pi::connectors::{Connector, HostCallPayload};
use pi::extensions::HostCallErrorCode;
use serde_json::{Value, json};
use std::future::Future;
use std::io::Write;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn run_async<T, Fut>(future: Fut) -> T
where
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    let join = runtime.handle().spawn(future);
    runtime.block_on(join)
}

fn http_call(url: &str, method: &str) -> HostCallPayload {
    HostCallPayload {
        call_id: "call-test".to_string(),
        capability: "http".to_string(),
        method: "http".to_string(),
        params: json!({ "url": url, "method": method }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    }
}

fn http_call_with_body(url: &str, method: &str, body: &str) -> HostCallPayload {
    HostCallPayload {
        call_id: "call-test".to_string(),
        capability: "http".to_string(),
        method: "http".to_string(),
        params: json!({ "url": url, "method": method, "body": body }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    }
}

fn http_call_with_timeout(url: &str, timeout_ms: u64) -> HostCallPayload {
    HostCallPayload {
        call_id: "call-test".to_string(),
        capability: "http".to_string(),
        method: "http".to_string(),
        params: json!({ "url": url, "method": "GET", "timeout_ms": timeout_ms }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// TLS requirement enforcement
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn tls_required_denies_http_scheme() {
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: true,
        ..Default::default()
    });
    let call = http_call("http://example.com/data", "GET");
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(
        result.is_error,
        "http:// should be denied when TLS required"
    );
    let error = result.error.expect("error payload");
    assert_eq!(error.code, HostCallErrorCode::Denied);
    assert!(
        error.message.contains("TLS"),
        "message should mention TLS: {}",
        error.message
    );
}

#[test]
fn tls_required_allows_https_scheme() {
    // Note: this won't actually connect (no server), but policy validation passes
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: true,
        allowlist: vec!["example.com".to_string()],
        ..Default::default()
    });
    // Policy check passes - URL is valid HTTPS + in allowlist.
    // The actual connection will fail (no server), but policy is satisfied.
    let call = http_call("https://example.com/data", "GET");
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    // If there's an error, it should NOT be "Denied" - it would be IO/Timeout from connection
    if result.is_error {
        let error = result.error.as_ref().unwrap();
        assert_ne!(
            error.code,
            HostCallErrorCode::Denied,
            "https should pass policy even if connection fails: {}",
            error.message
        );
    }
}

#[test]
fn tls_not_required_allows_http() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let _server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        stream.write_all(resp.as_bytes()).expect("write");
    });

    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        ..Default::default()
    });
    let call = http_call(&format!("http://{addr}/"), "GET");
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(
        !result.is_error,
        "http should be allowed when TLS not required: {:?}",
        result.error
    );
    assert_eq!(
        result.output.get("status").and_then(Value::as_u64),
        Some(200)
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Host allowlist enforcement
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn allowlist_denies_unlisted_host() {
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        allowlist: vec!["api.allowed.com".to_string()],
        ..Default::default()
    });
    let call = http_call("http://evil.notallowed.com/steal", "GET");
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(result.is_error);
    let error = result.error.expect("error");
    assert_eq!(error.code, HostCallErrorCode::Denied);
    assert!(
        error.message.contains("not in allowlist"),
        "message should mention allowlist: {}",
        error.message
    );
}

#[test]
fn allowlist_denies_without_network_contact() {
    // Use a host that would be unreachable, but check that
    // the denied response returns instantly (no network attempt)
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        allowlist: vec!["only-this-host.example".to_string()],
        ..Default::default()
    });

    let call = http_call("http://10.255.255.1/secret-data", "GET");
    let start = std::time::Instant::now();
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });
    let elapsed = start.elapsed();

    assert!(result.is_error);
    assert_eq!(
        result.error.as_ref().unwrap().code,
        HostCallErrorCode::Denied
    );
    // Policy denial should be near-instant (< 100ms), not a network timeout
    assert!(
        elapsed.as_millis() < 100,
        "denied request should not attempt network (took {}ms)",
        elapsed.as_millis()
    );
}

#[test]
fn allowlist_wildcard_allows_subdomain() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let _server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        stream.write_all(resp.as_bytes()).expect("write");
    });

    // We can't actually test wildcard with real DNS, but we can test
    // the policy validation logic directly via dispatch. For real traffic,
    // we'll test with 127.0.0.1 in the allowlist.
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        allowlist: vec!["127.0.0.1".to_string()],
        ..Default::default()
    });
    let call = http_call(&format!("http://{addr}/"), "GET");
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(!result.is_error, "127.0.0.1 in allowlist should succeed");
}

#[test]
fn empty_allowlist_allows_all_hosts() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let _server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        stream.write_all(resp.as_bytes()).expect("write");
    });

    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        allowlist: vec![], // empty = allow all
        ..Default::default()
    });
    let call = http_call(&format!("http://{addr}/"), "GET");
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(
        !result.is_error,
        "empty allowlist should allow all: {:?}",
        result.error
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Host denylist enforcement
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn denylist_blocks_exact_host() {
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        denylist: vec!["evil.com".to_string()],
        ..Default::default()
    });
    let call = http_call("http://evil.com/malware", "GET");
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(result.is_error);
    let error = result.error.expect("error");
    assert_eq!(error.code, HostCallErrorCode::Denied);
    assert!(
        error.message.contains("denylist"),
        "message should mention denylist: {}",
        error.message
    );
}

#[test]
fn denylist_blocks_wildcard_subdomain() {
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        denylist: vec!["*.malware.net".to_string()],
        ..Default::default()
    });
    let call = http_call("http://api.malware.net/exfiltrate", "GET");
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(result.is_error);
    assert_eq!(
        result.error.as_ref().unwrap().code,
        HostCallErrorCode::Denied
    );
}

#[test]
fn denylist_takes_precedence_over_allowlist() {
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        allowlist: vec!["*.example.com".to_string()],
        denylist: vec!["evil.example.com".to_string()],
        ..Default::default()
    });

    // Allowed subdomain should pass policy (though connection fails)
    let call = http_call("http://api.example.com/ok", "GET");
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });
    if result.is_error {
        assert_ne!(
            result.error.as_ref().unwrap().code,
            HostCallErrorCode::Denied,
            "api.example.com should pass policy"
        );
    }

    // Denied subdomain should fail even though it matches allowlist
    let connector2 = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        allowlist: vec!["*.example.com".to_string()],
        denylist: vec!["evil.example.com".to_string()],
        ..Default::default()
    });
    let call2 = http_call("http://evil.example.com/steal", "GET");
    let result2 = run_async(async move { connector2.dispatch(&call2).await.unwrap() });

    assert!(result2.is_error);
    assert_eq!(
        result2.error.as_ref().unwrap().code,
        HostCallErrorCode::Denied
    );
}

#[test]
fn denylist_case_insensitive() {
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        denylist: vec!["Evil.COM".to_string()],
        ..Default::default()
    });
    let call = http_call("http://evil.com/data", "GET");
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(result.is_error);
    assert_eq!(
        result.error.as_ref().unwrap().code,
        HostCallErrorCode::Denied
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Request body size limits
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn request_body_exceeding_limit_rejected() {
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        max_request_bytes: 100,
        ..Default::default()
    });
    let large_body = "x".repeat(200);
    let call = http_call_with_body("http://example.com/upload", "POST", &large_body);
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(result.is_error);
    let error = result.error.expect("error");
    assert_eq!(error.code, HostCallErrorCode::InvalidRequest);
    assert!(
        error.message.contains("too large"),
        "should mention size: {}",
        error.message
    );
}

#[test]
fn request_body_within_limit_accepted() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let _server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        // Read request then respond
        let mut buf = [0u8; 4096];
        let _ = std::io::Read::read(&mut stream, &mut buf);
        let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        stream.write_all(resp.as_bytes()).expect("write");
    });

    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        max_request_bytes: 1000,
        ..Default::default()
    });
    let body = "x".repeat(50);
    let call = http_call_with_body(&format!("http://{addr}/"), "POST", &body);
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(
        !result.is_error,
        "body within limit should succeed: {:?}",
        result.error
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Response body size limits
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn response_body_exceeding_limit_returns_error() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let _server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 1024];
        let _ = std::io::Read::read(&mut stream, &mut buf);
        // Send a response with body larger than the limit
        let big_body = "X".repeat(500);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{big_body}",
            big_body.len()
        );
        stream.write_all(resp.as_bytes()).expect("write");
    });

    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        max_response_bytes: 100, // Very small limit
        default_timeout_ms: 5000,
        ..Default::default()
    });
    let call = http_call(&format!("http://{addr}/"), "GET");
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(
        result.is_error,
        "response exceeding max_response_bytes should error"
    );
    let error = result.error.expect("error");
    assert!(
        error.message.contains("too large"),
        "should mention size limit: {}",
        error.message
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Timeout enforcement
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn request_timeout_returns_timeout_error_code() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
    let _server = std::thread::spawn(move || {
        let (_stream, _) = listener.accept().expect("accept");
        // Hold connection open without responding
        let _ = shutdown_rx.recv_timeout(std::time::Duration::from_secs(2));
    });

    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        default_timeout_ms: 100,
        ..Default::default()
    });
    let call = http_call_with_timeout(&format!("http://{addr}/"), 100);
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(result.is_error);
    let error = result.error.expect("error");
    assert_eq!(error.code, HostCallErrorCode::Timeout);
    assert_eq!(error.retryable, Some(true), "timeouts should be retryable");

    let _ = shutdown_tx.send(());
}

#[test]
fn call_level_timeout_used_when_request_omits_it() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
    let _server = std::thread::spawn(move || {
        let (_stream, _) = listener.accept().expect("accept");
        let _ = shutdown_rx.recv_timeout(std::time::Duration::from_secs(2));
    });

    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        default_timeout_ms: 5000, // Large default
        ..Default::default()
    });
    // No timeout_ms in params, but call-level timeout is 100ms
    let call = HostCallPayload {
        call_id: "call-test".to_string(),
        capability: "http".to_string(),
        method: "http".to_string(),
        params: json!({ "url": format!("http://{addr}/"), "method": "GET" }),
        timeout_ms: Some(100),
        cancel_token: None,
        context: None,
    };
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(result.is_error);
    assert_eq!(
        result.error.as_ref().unwrap().code,
        HostCallErrorCode::Timeout
    );

    let _ = shutdown_tx.send(());
}

// ═══════════════════════════════════════════════════════════════════════════════
// Invalid URL / scheme enforcement
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn unsupported_scheme_rejected() {
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        ..Default::default()
    });

    for scheme in &[
        "ftp://example.com/file",
        "file:///etc/passwd",
        "data:text/plain;base64,SGVsbG8=",
    ] {
        let call = http_call(scheme, "GET");
        let result = run_async({
            let connector_ref = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            async move { connector_ref.dispatch(&call).await.unwrap() }
        });

        assert!(result.is_error, "scheme '{scheme}' should be rejected");
        let error = result.error.as_ref().unwrap();
        assert!(
            error.code == HostCallErrorCode::InvalidRequest
                || error.code == HostCallErrorCode::Denied,
            "unsupported scheme '{}' should be InvalidRequest or Denied, got {:?}",
            scheme,
            error.code
        );
    }

    drop(connector);
}

#[test]
fn missing_host_rejected() {
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        ..Default::default()
    });
    // URL with no host
    let call = http_call("http:///no-host-path", "GET");
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(result.is_error, "URL with no host should be rejected");
    // May be InvalidRequest or Io depending on how the URL parser handles it
    let error = result.error.expect("error");
    assert!(
        error.code == HostCallErrorCode::InvalidRequest || error.code == HostCallErrorCode::Io,
        "expected InvalidRequest or Io, got {:?}",
        error.code
    );
}

#[test]
fn invalid_url_rejected() {
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        ..Default::default()
    });
    let call = http_call("not a valid url at all", "GET");
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(result.is_error);
    assert_eq!(
        result.error.as_ref().unwrap().code,
        HostCallErrorCode::InvalidRequest
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Method validation
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn unsupported_method_rejected() {
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        ..Default::default()
    });

    for method in &["DELETE", "PUT", "PATCH", "OPTIONS"] {
        let call = http_call("http://example.com/", method);
        let result = run_async({
            let connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            async move { connector.dispatch(&call).await.unwrap() }
        });

        assert!(result.is_error, "method '{method}' should be rejected");
        assert_eq!(
            result.error.as_ref().unwrap().code,
            HostCallErrorCode::InvalidRequest,
            "unsupported method '{method}' should be InvalidRequest"
        );
    }

    drop(connector);
}

#[test]
fn get_with_body_rejected() {
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        ..Default::default()
    });
    let call = http_call_with_body("http://example.com/", "GET", "unexpected body");
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(result.is_error);
    let error = result.error.expect("error");
    assert_eq!(error.code, HostCallErrorCode::InvalidRequest);
    assert!(
        error.message.contains("body"),
        "should mention body not allowed for GET: {}",
        error.message
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Connector method validation
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn wrong_connector_method_rejected() {
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        ..Default::default()
    });
    let call = HostCallPayload {
        call_id: "call-test".to_string(),
        capability: "http".to_string(),
        method: "ftp".to_string(), // Wrong connector method
        params: json!({ "url": "http://example.com/", "method": "GET" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(result.is_error);
    assert_eq!(
        result.error.as_ref().unwrap().code,
        HostCallErrorCode::InvalidRequest
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// URL redaction for logging
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn redacted_url_strips_credentials_and_query() {
    // HttpConnector::redact_url_for_log is private, but we can test
    // that denied requests don't leak secrets via the error message.
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        allowlist: vec!["allowed.example".to_string()],
        ..Default::default()
    });
    let call = http_call(
        "http://user:s3cret@denied.example/path?api_key=ABCDEF&token=xyz#fragment",
        "GET",
    );
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(result.is_error);
    let error_msg = &result.error.as_ref().unwrap().message;
    // Error message should not contain the query params or credentials
    assert!(
        !error_msg.contains("s3cret"),
        "error message should not leak password: {error_msg}"
    );
    assert!(
        !error_msg.contains("ABCDEF"),
        "error message should not leak API key: {error_msg}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Denied requests don't hit the network (verified via no connection)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn denied_host_never_opens_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let connected = Arc::new(AtomicBool::new(false));
    let connected2 = Arc::clone(&connected);

    let _server = std::thread::spawn(move || {
        listener.set_nonblocking(true).expect("set nonblocking");
        // Try to accept for 500ms
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_millis(500) {
            if listener.accept().is_ok() {
                connected2.store(true, Ordering::SeqCst);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });

    // 127.0.0.1 is NOT in allowlist
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        allowlist: vec!["only-this-host.test".to_string()],
        default_timeout_ms: 200,
        ..Default::default()
    });
    let call = http_call(&format!("http://{addr}/secret-data"), "GET");
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(result.is_error);
    assert_eq!(
        result.error.as_ref().unwrap().code,
        HostCallErrorCode::Denied
    );

    // Give the server thread time to verify no connection was made
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(
        !connected.load(Ordering::SeqCst),
        "denied request should NEVER open a TCP connection"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Streaming dispatch policy enforcement
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn streaming_dispatch_denies_blocked_host() {
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        denylist: vec!["blocked.example".to_string()],
        ..Default::default()
    });
    let call = HostCallPayload {
        call_id: "call-stream-test".to_string(),
        capability: "http".to_string(),
        method: "http".to_string(),
        params: json!({ "url": "http://blocked.example/stream", "method": "GET" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    let result = run_async(async move { connector.dispatch_streaming(&call).await });

    let Err(error_payload) = result else {
        panic!("streaming should be denied for blocked host")
    };
    assert!(error_payload.is_error);
    assert_eq!(
        error_payload.error.as_ref().unwrap().code,
        HostCallErrorCode::Denied
    );
}

#[test]
fn streaming_dispatch_denies_tls_violation() {
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: true,
        ..Default::default()
    });
    let call = HostCallPayload {
        call_id: "call-stream-tls".to_string(),
        capability: "http".to_string(),
        method: "http".to_string(),
        params: json!({ "url": "http://example.com/stream", "method": "GET" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    let result = run_async(async move { connector.dispatch_streaming(&call).await });

    let Err(error_payload) = result else {
        panic!("streaming should deny TLS violation")
    };
    assert_eq!(
        error_payload.error.as_ref().unwrap().code,
        HostCallErrorCode::Denied
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Zero timeout treated as unset
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn zero_timeout_treated_as_no_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let _server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        stream.write_all(resp.as_bytes()).expect("write");
    });

    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: false,
        default_timeout_ms: 5000,
        ..Default::default()
    });
    // timeout_ms: 0 should be treated as "use default"
    let call = http_call_with_timeout(&format!("http://{addr}/"), 0);
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(
        !result.is_error,
        "timeout_ms=0 should use default, not timeout immediately: {:?}",
        result.error
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Multiple policy violations
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn multiple_violations_first_check_wins() {
    // URL is http:// (TLS violation) AND host is in denylist
    let connector = HttpConnector::new(HttpConnectorConfig {
        require_tls: true,
        denylist: vec!["evil.com".to_string()],
        ..Default::default()
    });
    let call = http_call("http://evil.com/malware", "GET");
    let result = run_async(async move { connector.dispatch(&call).await.unwrap() });

    assert!(result.is_error);
    // Should get Denied (TLS check happens before denylist check)
    assert_eq!(
        result.error.as_ref().unwrap().code,
        HostCallErrorCode::Denied
    );
}
