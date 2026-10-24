//! End-to-end proxy wiring for the HTTP client (#210).
//!
//! Loopback HTTP peers prove which endpoint receives the request and whether
//! the wire uses origin-form or proxy absolute-form. Every peer is bounded and
//! joined, including when a routing assertion panics.

mod common;

use pi::http::client::Client;
use pi::http::proxy::{HttpSettings, ProxyConfig, install};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const PEER_TIMEOUT: Duration = Duration::from_secs(10);

/// The proxy configuration is process-global, so these tests must not overlap.
fn proxy_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct RestoreProxy(ProxyConfig);

impl RestoreProxy {
    fn install(config: ProxyConfig) -> Self {
        let restore = Self(pi::http::proxy::active());
        install(config);
        restore
    }
}

impl Drop for RestoreProxy {
    fn drop(&mut self) {
        install(std::mem::take(&mut self.0));
    }
}

/// Settings that pin exactly one proxy and ignore the ambient environment.
fn pinned_settings(port: u16) -> HttpSettings {
    HttpSettings {
        proxy: Some(format!("http://user:pass@127.0.0.1:{port}")),
        ignore_env_proxy: Some(true),
        ..HttpSettings::default()
    }
}

struct HttpPeer {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<io::Result<Option<Vec<String>>>>>,
}

impl HttpPeer {
    fn spawn(listener: TcpListener, body: &'static str) -> Self {
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + PEER_TIMEOUT;
            loop {
                if stopped.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Ok(None);
                }
                match listener.accept() {
                    Ok((stream, _)) => return serve_peer(stream, body, &stopped).map(Some),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }

    fn finish(mut self) -> io::Result<Option<Vec<String>>> {
        self.stop.store(true, Ordering::Release);
        self.worker
            .take()
            .expect("peer worker remains owned")
            .join()
            .map_err(|_| io::Error::other("HTTP peer panicked"))?
    }
}

impl Drop for HttpPeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve_peer(mut stream: TcpStream, body: &str, stop: &AtomicBool) -> io::Result<Vec<String>> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_millis(50)))?;
    stream.set_write_timeout(Some(PEER_TIMEOUT))?;
    let deadline = Instant::now() + PEER_TIMEOUT;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 1024];
    let head_end = loop {
        if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "HTTP peer stopped"));
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "request ended before its headers",
                ));
            }
            Ok(count) => {
                if bytes.len().saturating_add(count) > 16 * 1024 {
                    return Err(io::Error::other("request headers exceed fixture limit"));
                }
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    break end;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    };
    let head = std::str::from_utf8(&bytes[..head_end])
        .map_err(io::Error::other)?
        .split("\r\n")
        .map(str::to_string)
        .collect();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(head)
}

fn request(url: String) -> pi::error::Result<(u16, String)> {
    common::run_async(async move {
        let client = Client::new();
        let response = client.get(&url).timeout(PEER_TIMEOUT).send().await?;
        Ok((response.status(), response.text().await?))
    })
}

/// A loopback proxy must still carry requests whose destination is remote.
#[test]
fn http_request_goes_through_the_configured_proxy() {
    let _guard = proxy_guard();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake proxy");
    let port = listener.local_addr().expect("addr").port();
    let proxy = HttpPeer::spawn(listener, "proxied-ok");
    let (config, warnings) = ProxyConfig::resolve(Some(&pinned_settings(port)), &|_| None);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    let _restore = RestoreProxy::install(config);

    // A successful response for this unresolvable origin proves proxy routing.
    let result = request("http://example.invalid/api/test?x=1".to_string());
    let head = proxy.finish().expect("join proxy").expect("proxy request");
    assert_eq!(
        result.expect("proxied response"),
        (200, "proxied-ok".to_string())
    );
    assert_eq!(
        head.first().map(String::as_str),
        Some("GET http://example.invalid/api/test?x=1 HTTP/1.1")
    );
    assert!(head.iter().any(|line| line == "Host: example.invalid"));
    assert!(
        head.iter()
            .any(|line| line == "Proxy-Authorization: Basic dXNlcjpwYXNz")
    );
}

#[derive(Clone, Copy)]
enum ExpectedRoute {
    Direct,
    Proxy,
}

fn assert_loopback_route(expected: ExpectedRoute, configure: impl FnOnce(u16) -> ProxyConfig) {
    let _guard = proxy_guard();
    let origin_listener = TcpListener::bind("127.0.0.1:0").expect("bind local model");
    let origin_address = origin_listener.local_addr().expect("origin address");
    let proxy_listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
    let proxy_port = proxy_listener.local_addr().expect("proxy address").port();
    let origin = HttpPeer::spawn(origin_listener, "origin-ok");
    let proxy = HttpPeer::spawn(proxy_listener, "proxied-ok");
    let _restore = RestoreProxy::install(configure(proxy_port));
    let url = format!("http://{origin_address}/v1/models?x=1");
    let result = request(url.clone());
    let origin_head = origin.finish().expect("join origin");
    let proxy_head = proxy.finish().expect("join proxy");
    let (status, body) = result.expect("HTTP request completes");
    assert_eq!(status, 200);

    let (head, request_line) = match expected {
        ExpectedRoute::Direct => {
            assert_eq!(body, "origin-ok");
            assert!(
                proxy_head.is_none(),
                "the proxy must not receive local traffic"
            );
            let head = origin_head.expect("local model received the request");
            assert!(
                !head.iter().any(|line| {
                    line.to_ascii_lowercase()
                        .starts_with("proxy-authorization:")
                }),
                "proxy credentials must not reach the local model: {head:?}"
            );
            (head, "GET /v1/models?x=1 HTTP/1.1".to_string())
        }
        ExpectedRoute::Proxy => {
            assert_eq!(body, "proxied-ok");
            assert!(
                origin_head.is_none(),
                "the origin must not be dialed directly"
            );
            let head = proxy_head.expect("proxy received the request");
            assert!(
                head.iter()
                    .any(|line| line == "Proxy-Authorization: Basic dXNlcjpwYXNz")
            );
            (head, format!("GET {url} HTTP/1.1"))
        }
    };
    assert_eq!(head.first(), Some(&request_line));
    assert!(
        head.iter()
            .any(|line| line == &format!("Host: {origin_address}")),
        "the origin Host header must survive routing: {head:?}"
    );
}

#[test]
fn local_model_request_bypasses_explicit_proxy_by_default() {
    for scheme in ["http", "socks5", "socks5h"] {
        assert_loopback_route(ExpectedRoute::Direct, |port| {
            let mut settings = pinned_settings(port);
            settings.proxy = Some(format!("{scheme}://user:pass@127.0.0.1:{port}"));
            ProxyConfig::resolve(Some(&settings), &|_| None).0
        });
    }
}

#[test]
fn local_model_request_bypasses_ambient_proxy_by_default() {
    assert_loopback_route(ExpectedRoute::Direct, |port| {
        let endpoint = format!("http://user:pass@127.0.0.1:{port}");
        ProxyConfig::resolve(None, &|name| {
            (name == "ALL_PROXY").then(|| endpoint.clone())
        })
        .0
    });
}

#[test]
fn explicit_loopback_proxy_opt_in_uses_proxy_wire_format() {
    assert_loopback_route(ExpectedRoute::Proxy, |port| {
        let mut settings = pinned_settings(port);
        settings.proxy_loopback = Some(true);
        ProxyConfig::resolve(Some(&settings), &|_| None).0
    });
}

#[test]
fn no_proxy_host_bypasses_the_proxy() {
    assert_loopback_route(ExpectedRoute::Direct, |port| {
        let mut settings = pinned_settings(port);
        settings.proxy_loopback = Some(true);
        settings.no_proxy = Some(vec!["127.0.0.1".to_string()]);
        ProxyConfig::resolve(Some(&settings), &|_| None).0
    });
}
