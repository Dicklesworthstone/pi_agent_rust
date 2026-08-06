//! Minimal streaming HTTP client for Pi.
//!
//! This is intentionally small and purpose-built for provider streaming (SSE).
//! It avoids Node/Bun-style ambient APIs and is designed to pair with
//! asupersync for TLS + cancel-correctness.

use crate::error::{Error, Result};
use crate::vcr::{RecordedRequest, VcrRecorder};
use asupersync::http::h1::ParsedUrl;
use asupersync::http::h1::http_client::Scheme;
use asupersync::io::ext::AsyncWriteExt;
use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use asupersync::net::tcp::stream::TcpStream;
use asupersync::tls::{TlsConnector, TlsConnectorBuilder, TlsError};
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream::{self, BoxStream};
use std::pin::Pin;
use std::task::{Context, Poll};

const DEFAULT_USER_AGENT: &str = concat!("pi_agent_rust/", env!("CARGO_PKG_VERSION"));
const ANTIGRAVITY_VERSION_ENV: &str = "PI_AI_ANTIGRAVITY_VERSION";
const MAX_HEADER_BYTES: usize = 64 * 1024;
const READ_CHUNK_BYTES: usize = 16 * 1024;
const MAX_BUFFERED_BYTES: usize = 256 * 1024;
const MAX_TEXT_BODY_BYTES: usize = 50 * 1024 * 1024;
/// Maximum number of outbound request headers to prevent DoS.
const MAX_REQUEST_HEADERS: usize = 100;

/// Maximum number of consecutive `Ok(0)` returns from `poll_write` before we
/// give up and surface `ErrorKind::WriteZero`.  TLS transports can temporarily
/// return 0 when internal buffers are full; a short backoff usually unblocks
/// the next write.
const WRITE_ZERO_MAX_RETRIES: usize = 10;

/// Initial backoff duration when a write returns `Ok(0)`.
const WRITE_ZERO_BACKOFF: std::time::Duration = std::time::Duration::from_millis(10);
/// Environment variable that overrides the request timeout, in seconds.
///
/// Applies to all providers; `0` disables the timeout entirely (unbounded).
/// Clap binds the `--request-timeout` CLI flag and the `requestTimeoutSecs`
/// setting to this same env var, so the three configuration surfaces share a
/// single resolution path.
pub const REQUEST_TIMEOUT_ENV: &str = "PI_HTTP_REQUEST_TIMEOUT_SECS";

/// Default request timeout for remote (cloud) providers.
///
/// Covers connect + request-write + response-header latency. 60s is generous
/// for any healthy cloud API; if a remote provider has not produced response
/// headers within a minute something is wrong.
#[cfg(not(test))]
const DEFAULT_REMOTE_REQUEST_TIMEOUT_SECS: u64 = 60;

/// Default request timeout for *local* providers (Ollama, LM Studio, etc.).
///
/// Local inference servers frequently incur a large first-request latency: the
/// model has to be loaded from disk into RAM/VRAM, which for a multi-GB model
/// on a cold cache can take well over a minute (sometimes several). The cloud
/// 60s default was too short for this and caused `pi --provider ollama ...` to
/// fail with "Request timed out" while Ollama was still loading the model
/// (pi_agent_rust#90).
///
/// 600s (10 minutes) is long enough to absorb realistic cold-start model loads
/// while still bounding a truly hung/unreachable server so we never hang
/// forever. Users who load enormous models on slow disks can raise it (or set
/// it to `0` for unbounded) via `PI_HTTP_REQUEST_TIMEOUT_SECS` /
/// `--request-timeout` / `requestTimeoutSecs`.
#[cfg(not(test))]
const DEFAULT_LOCAL_REQUEST_TIMEOUT_SECS: u64 = 600;

/// How the request timeout should be resolved for a [`RequestBuilder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestTimeout {
    /// Resolve a provider-aware default at send time based on the target URL
    /// (longer for local providers like Ollama, shorter for cloud APIs),
    /// unless overridden by `PI_HTTP_REQUEST_TIMEOUT_SECS`.
    Default,
    /// Explicit timeout duration (from `.timeout()` or the global env override).
    Explicit(std::time::Duration),
    /// Explicitly unbounded (from `.no_timeout()` or `PI_HTTP_REQUEST_TIMEOUT_SECS=0`).
    Disabled,
}

/// Process-global request-timeout override set explicitly by the application
/// (from the `--request-timeout` CLI flag or the `requestTimeoutSecs` setting)
/// before any provider request is made.
///
/// Sentinel `u64::MAX` means "unset" so this can be a plain atomic without a
/// lock. `0` means "no timeout" (unbounded).
#[cfg(not(test))]
static REQUEST_TIMEOUT_OVERRIDE_SECS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX);

/// Set the process-global request-timeout override, in seconds.
///
/// `0` disables the timeout entirely (unbounded). Takes precedence over the
/// provider-aware defaults but is itself lower precedence than a per-request
/// `.timeout()` / `.no_timeout()` call. Should be called once during startup,
/// before any HTTP request is issued. See pi_agent_rust#90.
#[cfg(not(test))]
pub fn set_request_timeout_override(secs: u64) {
    // Reserve u64::MAX as the "unset" sentinel; clamp the (absurd) edge case so
    // callers asking for that exact value still get a finite timeout.
    let stored = if secs == u64::MAX { secs - 1 } else { secs };
    REQUEST_TIMEOUT_OVERRIDE_SECS.store(stored, std::sync::atomic::Ordering::Relaxed);
}

/// No-op override setter under `cfg(test)`, where timeouts are disabled.
#[cfg(test)]
#[allow(clippy::missing_const_for_fn)]
pub fn set_request_timeout_override(_secs: u64) {}

/// Read the global timeout override, if any.
///
/// Resolution order: an explicit application override (set via
/// [`set_request_timeout_override`]) first, then the
/// `PI_HTTP_REQUEST_TIMEOUT_SECS` environment variable. In both cases `0` =>
/// [`RequestTimeout::Disabled`]; any other value => an explicit duration.
/// Returns `None` when neither is set so the provider-aware default applies.
#[cfg(not(test))]
fn timeout_override(env_lookup: impl FnOnce() -> Option<String>) -> Option<RequestTimeout> {
    let secs = match REQUEST_TIMEOUT_OVERRIDE_SECS.load(std::sync::atomic::Ordering::Relaxed) {
        u64::MAX => env_lookup()?.trim().parse::<u64>().ok()?,
        explicit => explicit,
    };
    Some(if secs == 0 {
        RequestTimeout::Disabled
    } else {
        RequestTimeout::Explicit(std::time::Duration::from_secs(secs))
    })
}

/// Returns `true` when the URL targets a local/loopback inference server.
///
/// Local providers (Ollama on `127.0.0.1:11434`, LM Studio on
/// `127.0.0.1:1234`, etc.) can have very high first-request latency from
/// on-demand model loading, so they get a more generous default timeout.
fn url_is_local_provider(url: &str) -> bool {
    let Ok(parsed) = ParsedUrl::parse(url) else {
        return false;
    };
    let host = parsed.host.trim_matches(|c| c == '[' || c == ']');
    host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host.starts_with("127.")
        || host == "::1"
        || host == "0.0.0.0"
        || host.eq_ignore_ascii_case("localhost.localdomain")
}

/// Resolve the effective timeout for a request, honoring the global env
/// override first, then falling back to a provider-aware default.
#[cfg(not(test))]
fn resolve_timeout(setting: RequestTimeout, url: &str) -> Option<std::time::Duration> {
    let resolved = match setting {
        RequestTimeout::Explicit(duration) => RequestTimeout::Explicit(duration),
        RequestTimeout::Disabled => RequestTimeout::Disabled,
        RequestTimeout::Default => timeout_override(|| std::env::var(REQUEST_TIMEOUT_ENV).ok())
            .unwrap_or_else(|| {
                let secs = if url_is_local_provider(url) {
                    DEFAULT_LOCAL_REQUEST_TIMEOUT_SECS
                } else {
                    DEFAULT_REMOTE_REQUEST_TIMEOUT_SECS
                };
                RequestTimeout::Explicit(std::time::Duration::from_secs(secs))
            }),
    };
    match resolved {
        RequestTimeout::Explicit(duration) => Some(duration),
        RequestTimeout::Disabled | RequestTimeout::Default => None,
    }
}

/// During unit tests, timeouts are disabled to prevent `asupersync`'s virtual
/// timer from instantly fast-forwarding and failing mock server requests.
#[cfg(test)]
#[allow(clippy::missing_const_for_fn)]
fn resolve_timeout(_setting: RequestTimeout, _url: &str) -> Option<std::time::Duration> {
    None
}

/// Resolve the same provider-aware/global timeout used by an otherwise
/// unmodified [`RequestBuilder`]. Finite, non-streaming operations can use this
/// to add an overall wall-clock deadline in addition to the client's
/// connect/write/header and body-idle bounds.
pub(crate) fn effective_default_request_timeout(url: &str) -> Option<std::time::Duration> {
    resolve_timeout(RequestTimeout::Default, url)
}

/// Build a self-documenting timeout error message that tells the user the
/// timeout that fired and how to raise it. Adds Ollama/local-provider-specific
/// guidance (cold-start model load, model not pulled) when the target is a
/// loopback inference server. See pi_agent_rust#90.
fn timeout_error_message(url: &str, duration: std::time::Duration) -> String {
    let secs = duration.as_secs();
    let mut msg = format!(
        "Request timed out after {secs}s. Raise the timeout with \
         {REQUEST_TIMEOUT_ENV}=<seconds> (or `--request-timeout <seconds>`, or \
         `requestTimeoutSecs` in settings.json); set it to 0 for no timeout."
    );
    if url_is_local_provider(url) {
        msg.push_str(
            " For local providers like Ollama, the first request often blocks \
             while the model loads into memory (a cold start can take minutes), \
             and the model must already be pulled — try `ollama pull <model>` \
             and confirm the server is reachable (`ollama list`).",
        );
    }
    msg
}

#[derive(Debug, Clone)]
pub struct Client {
    tls: std::result::Result<TlsConnector, String>,
    user_agent: String,
    vcr: Option<VcrRecorder>,
}

/// Process-global cache of the built TLS connector.
///
/// Building the connector pulls in the root trust store, which is the single
/// most expensive part of constructing a [`Client`]. `Client::new()` is called
/// from many hot paths (every provider constructor, the version check, etc.),
/// so without caching each call rebuilt the trust store from scratch. We now
/// build the connector once per process and clone it (`TlsConnector` is cheap
/// to clone) on every subsequent call. See pi_agent_rust#101.
///
/// The `Err` variant carries the build error as a `String` so a transient or
/// configuration failure is still observed identically on every call.
static TLS_CONNECTOR: std::sync::OnceLock<std::result::Result<TlsConnector, String>> =
    std::sync::OnceLock::new();

/// Build (or fetch the cached) TLS connector backed by the bundled webpki
/// root certificates.
///
/// Using webpki roots avoids hitting the OS trust store, which on macOS calls
/// into Security.framework (`SecTrustSettingsCopyTrustSettings`) and can spend
/// many seconds at high CPU parsing the system cert trust plist on startup.
/// See pi_agent_rust#101.
fn shared_tls_connector() -> std::result::Result<TlsConnector, String> {
    TLS_CONNECTOR
        .get_or_init(|| {
            TlsConnectorBuilder::new()
                .with_webpki_roots()
                .alpn_protocols(vec![b"http/1.1".to_vec()])
                .build()
                .map_err(|e| e.to_string())
        })
        .clone()
}

impl Client {
    #[must_use]
    pub fn new() -> Self {
        let tls = shared_tls_connector();

        let user_agent = std::env::var(ANTIGRAVITY_VERSION_ENV).map_or_else(
            |_| DEFAULT_USER_AGENT.to_string(),
            |v| format!("{DEFAULT_USER_AGENT} Antigravity/{v}"),
        );

        Self {
            tls,
            user_agent,
            vcr: None,
        }
    }

    pub fn post(&self, url: &str) -> RequestBuilder<'_> {
        RequestBuilder::new(self, Method::Post, url)
    }

    pub fn get(&self, url: &str) -> RequestBuilder<'_> {
        RequestBuilder::new(self, Method::Get, url)
    }

    #[must_use]
    pub fn with_vcr(mut self, recorder: VcrRecorder) -> Self {
        self.vcr = Some(recorder);
        self
    }

    pub const fn vcr(&self) -> Option<&VcrRecorder> {
        self.vcr.as_ref()
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy)]
enum Method {
    Get,
    Post,
}

impl Method {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

pub struct RequestBuilder<'a> {
    client: &'a Client,
    method: Method,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout: RequestTimeout,
}

impl<'a> RequestBuilder<'a> {
    fn new(client: &'a Client, method: Method, url: &str) -> Self {
        Self {
            client,
            method,
            url: url.to_string(),
            headers: Vec::new(),
            body: Vec::new(),
            // Resolved at send time so the timeout can be provider-aware
            // (longer default for local providers like Ollama).
            timeout: RequestTimeout::Default,
        }
    }

    #[must_use]
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let key = key.into();
        let value = value.into();
        self.upsert_header(key, value);
        self
    }

    pub fn try_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Result<Self> {
        let key = key.into();
        let value = value.into();
        validate_request_header(&key, &value)?;
        let replaces_existing = self
            .headers
            .iter()
            .any(|(existing_key, _)| existing_key.eq_ignore_ascii_case(&key));
        if !replaces_existing && self.headers.len() >= MAX_REQUEST_HEADERS {
            return Err(Error::api(format!(
                "HTTP request exceeds the {MAX_REQUEST_HEADERS}-header limit"
            )));
        }
        self.upsert_header(key, value);
        Ok(self)
    }

    fn upsert_header(&mut self, key: String, value: String) {
        if let Some((existing_key, existing_value)) = self
            .headers
            .iter_mut()
            .find(|(existing_key, _)| existing_key.eq_ignore_ascii_case(&key))
        {
            *existing_key = key;
            *existing_value = value;
        } else if self.headers.len() < MAX_REQUEST_HEADERS {
            self.headers.push((key, value));
        }
        // Silently drop headers beyond the limit to prevent DoS
    }

    #[must_use]
    pub const fn timeout(mut self, duration: std::time::Duration) -> Self {
        self.timeout = RequestTimeout::Explicit(duration);
        self
    }

    /// Remove the timeout entirely. Use for requests that are expected to take
    /// an arbitrarily long time (e.g. long-polling SSE streams).
    #[must_use]
    pub const fn no_timeout(mut self) -> Self {
        self.timeout = RequestTimeout::Disabled;
        self
    }

    /// Set raw body bytes.
    #[must_use]
    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    pub fn json<T: serde::Serialize>(mut self, payload: &T) -> Result<Self> {
        self = self.header("Content-Type", "application/json");
        self.body = serde_json::to_vec(payload)?;
        Ok(self)
    }

    pub async fn send(self) -> Result<Response> {
        let RequestBuilder {
            client,
            method,
            url,
            headers,
            body,
            timeout,
        } = self;

        if let Some(recorder) = client.vcr() {
            let recorded_request = build_recorded_request(method, &url, &headers, &body);
            let recorded = recorder
                .request_streaming_with(recorded_request, || async {
                    let (status, response_headers, stream) =
                        send_parts(client, method, &url, &headers, &body).await?;
                    Ok((status, response_headers, stream))
                })
                .await?;
            let status = recorded.status;
            let response_headers = recorded.headers.clone();
            let stream = recorded.into_byte_stream();
            return Ok(Response {
                status,
                headers: response_headers,
                stream,
                timeout_info: None,
            });
        }

        let send_fut = send_parts(client, method, &url, &headers, &body);
        let resolved_timeout = resolve_timeout(timeout, &url);

        let (status, response_headers, stream, timeout_info) = if let Some(duration) =
            resolved_timeout
        {
            use asupersync::time::{sleep, wall_now};
            use futures::future::{Either, FutureExt, select};

            let asupersync_now = asupersync::Cx::current()
                .and_then(|cx| cx.timer_driver())
                .map_or_else(wall_now, |timer| timer.now());

            let sleep_fut = sleep(asupersync_now, duration).fuse();
            let send_fut = send_fut.fuse();
            futures::pin_mut!(sleep_fut, send_fut);

            let (status, response_headers, stream) = match select(send_fut, sleep_fut).await {
                Either::Left((res, _)) => res?,
                Either::Right(_) => return Err(Error::api(timeout_error_message(&url, duration))),
            };
            (
                status,
                response_headers,
                stream,
                Some((asupersync_now, duration)),
            )
        } else {
            let (status, response_headers, stream) = send_fut.await?;
            (status, response_headers, stream, None)
        };

        Ok(Response {
            status,
            headers: response_headers,
            stream,
            timeout_info,
        })
    }
}

/// Like `write_all`, but retries on `Ok(0)` with exponential backoff instead
/// of immediately failing with `ErrorKind::WriteZero`.
///
/// TLS transports (and, less commonly, TCP under memory pressure) can return
/// `Ok(0)` from `write()` when internal buffers are temporarily full.  The
/// standard `write_all` implementation treats this as an unrecoverable error,
/// which causes spurious "IO error: write zero" failures — especially for
/// large request bodies such as resumed session contexts.
async fn write_all_with_retry<W: AsyncWrite + Unpin>(
    writer: &mut W,
    mut buf: &[u8],
) -> std::io::Result<()> {
    use asupersync::time::{sleep, wall_now};

    let mut consecutive_zeros: usize = 0;
    let mut backoff = WRITE_ZERO_BACKOFF;

    while !buf.is_empty() {
        let n = futures::future::poll_fn(|cx| Pin::new(&mut *writer).poll_write(cx, buf)).await?;

        if n == 0 {
            consecutive_zeros += 1;
            if consecutive_zeros > WRITE_ZERO_MAX_RETRIES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    format!(
                        "transport returned Ok(0) {} consecutive times ({} bytes remaining)",
                        consecutive_zeros,
                        buf.len(),
                    ),
                ));
            }
            tracing::debug!(
                attempt = consecutive_zeros,
                remaining = buf.len(),
                backoff_ms = backoff.as_millis(),
                "write returned Ok(0), backing off before retry"
            );

            // Flushing the writer is crucial when TLS buffers are full, otherwise
            // we will sleep and retry without any progress being made. If flush
            // itself fails, surface that real transport error immediately rather
            // than misreporting the retry loop as a generic write-zero failure.
            futures::future::poll_fn(|cx| Pin::new(&mut *writer).poll_flush(cx)).await?;

            let now = asupersync::Cx::current()
                .and_then(|cx| cx.timer_driver())
                .map_or_else(wall_now, |timer| timer.now());
            sleep(now, backoff).await;

            // Exponential backoff: 10ms, 20ms, 40ms, …
            backoff = backoff.saturating_mul(2);
        } else {
            // Successful partial write — advance the buffer and reset retry state.
            buf = &buf[n..];
            consecutive_zeros = 0;
            backoff = WRITE_ZERO_BACKOFF;
        }
    }
    Ok(())
}

async fn send_parts(
    client: &Client,
    method: Method,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<(
    u16,
    Vec<(String, String)>,
    BoxStream<'static, std::io::Result<Vec<u8>>>,
)> {
    let parsed = ParsedUrl::parse(url).map_err(|e| Error::api(format!("Invalid URL: {e}")))?;
    let mut transport = connect_transport(&parsed, client).await?;

    let request_bytes = build_request_bytes(method, &parsed, &client.user_agent, headers, body);
    write_all_with_retry(&mut transport, &request_bytes).await?;
    if !body.is_empty() {
        write_all_with_retry(&mut transport, body).await?;
    }
    transport.flush().await?;

    let (status, response_headers, leftover) = Box::pin(read_response_head(&mut transport)).await?;
    let body_kind = body_kind_from_response(status, &response_headers)?;

    let state = BodyStreamState::new(transport, body_kind, leftover);
    let stream = stream::try_unfold(state, |mut state| async move {
        match Box::pin(state.next_bytes()).await {
            Ok(Some(chunk)) => Ok(Some((chunk, state))),
            Ok(None) => {
                state.shutdown_transport_best_effort().await;
                Ok(None)
            }
            Err(err) => {
                state.shutdown_transport_best_effort().await;
                Err(err)
            }
        }
    })
    .boxed();

    Ok((status, response_headers, stream))
}

fn build_recorded_request(
    method: Method,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> RecordedRequest {
    let mut body_value = None;
    let mut body_text = None;

    if !body.is_empty() {
        let is_json = headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("content-type")
                && value.to_ascii_lowercase().contains("application/json")
        });

        if is_json {
            match serde_json::from_slice::<serde_json::Value>(body) {
                Ok(value) => body_value = Some(value),
                Err(_) => body_text = Some(String::from_utf8_lossy(body).to_string()),
            }
        } else {
            body_text = Some(String::from_utf8_lossy(body).to_string());
        }
    }

    RecordedRequest {
        method: method.as_str().to_string(),
        url: url.to_string(),
        headers: headers.to_vec(),
        body: body_value,
        body_text,
    }
}

pub struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    stream: Pin<Box<dyn Stream<Item = std::io::Result<Vec<u8>>> + Send>>,
    timeout_info: Option<(asupersync::Time, std::time::Duration)>,
}

fn wrap_stream_with_idle_timeout(
    stream: Pin<Box<dyn Stream<Item = std::io::Result<Vec<u8>>> + Send>>,
    timeout_info: Option<(asupersync::Time, std::time::Duration)>,
) -> Pin<Box<dyn Stream<Item = std::io::Result<Vec<u8>>> + Send>> {
    let Some((start_time, timeout)) = timeout_info else {
        return stream;
    };

    Box::pin(futures::stream::unfold(
        (stream, start_time, timeout),
        |(mut stream, mut last_activity, timeout)| async move {
            use asupersync::time::{sleep, wall_now};
            use futures::future::{Either, FutureExt, select};

            let asupersync_now = asupersync::Cx::current()
                .and_then(|cx| cx.timer_driver())
                .map_or_else(wall_now, |timer| timer.now());

            let elapsed =
                std::time::Duration::from_nanos(asupersync_now.duration_since(last_activity));
            if elapsed >= timeout {
                return Some((
                    Err(std::io::Error::other(
                        "Request timed out reading body stream",
                    )),
                    (stream, last_activity, timeout),
                ));
            }

            let remaining = timeout.checked_sub(elapsed).unwrap_or_default();
            let sleep_fut = sleep(asupersync_now, remaining).fuse();
            let next_fut = stream.next().fuse();
            futures::pin_mut!(sleep_fut, next_fut);

            match select(next_fut, sleep_fut).await {
                Either::Left((Some(res), _)) => {
                    let now = asupersync::Cx::current()
                        .and_then(|cx| cx.timer_driver())
                        .map_or_else(wall_now, |timer| timer.now());
                    last_activity = now;
                    Some((res, (stream, last_activity, timeout)))
                }
                Either::Left((None, _)) => None,
                Either::Right(_) => Some((
                    Err(std::io::Error::other(
                        "Request timed out reading body stream",
                    )),
                    (stream, last_activity, timeout),
                )),
            }
        },
    ))
}

impl Response {
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    #[must_use]
    pub fn bytes_stream(self) -> Pin<Box<dyn Stream<Item = std::io::Result<Vec<u8>>> + Send>> {
        wrap_stream_with_idle_timeout(self.stream, self.timeout_info)
    }

    pub async fn text(self) -> Result<String> {
        self.text_limited(MAX_TEXT_BODY_BYTES).await
    }

    /// Collect the response body with a caller-specific byte limit.
    ///
    /// This is useful for small structured endpoints whose safe bound is much
    /// tighter than the generic response limit.
    pub async fn text_limited(self, max_bytes: usize) -> Result<String> {
        let bytes = self.bytes_limited(max_bytes).await?;

        match String::from_utf8(bytes) {
            Ok(s) => Ok(s),
            Err(e) => Ok(String::from_utf8_lossy(e.as_bytes()).into_owned()),
        }
    }

    /// Collect response bytes without changing invalid UTF-8, bounded by a
    /// caller-specific limit.
    pub async fn bytes_limited(self, max_bytes: usize) -> Result<Vec<u8>> {
        let stream = wrap_stream_with_idle_timeout(self.stream, self.timeout_info);
        stream
            .try_fold(Vec::new(), |mut acc, chunk| async move {
                if acc.len().saturating_add(chunk.len()) > max_bytes {
                    return Err(std::io::Error::other("response body too large"));
                }
                acc.extend_from_slice(&chunk);
                Ok::<_, std::io::Error>(acc)
            })
            .await
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::Other
                    && err
                        .to_string()
                        .contains("Request timed out reading body stream")
                {
                    Error::api("Request timed out reading body")
                } else {
                    Error::from(err)
                }
            })
    }
}

/// Windows Winsock error code for "Socket is not connected" (`WSAENOTCONN`).
///
/// Layered Winsock providers (VPN clients, antivirus, firewall LSPs) can
/// report an outbound TCP connect as complete — `getpeername` succeeds — while
/// the base provider socket has not actually finished connecting, so the first
/// send on the socket fails with 10057 (pi_agent_rust#106, previously #66 /
/// asupersync#35). The upstream asupersync peer_addr readiness probe (0.3.2+)
/// is not sufficient in those environments, so the connect path retries the
/// whole connect with a fresh socket.
const WSAENOTCONN: i32 = 10057;

/// Backoff schedule for retrying a connect that failed with a "socket not
/// connected" error: three attempts total (initial + one retry per entry).
const NOT_CONNECTED_RETRY_BACKOFFS: [std::time::Duration; 2] = [
    std::time::Duration::from_millis(250),
    std::time::Duration::from_millis(750),
];

/// Whether an I/O error means the OS reported the socket as not connected
/// (`WSAENOTCONN` / os error 10057 on Windows, `ENOTCONN` elsewhere).
///
/// During an *outbound* connect / TLS handshake this is never a legitimate
/// terminal state — we just created the socket ourselves — so it is always
/// worth retrying with a fresh connection on any platform. Other error kinds
/// (refused, reset, DNS failures, certificate errors, ...) must NOT be
/// retried here.
///
/// Layered transports sometimes wrap the original socket error in another
/// error, so the inner errors are inspected too, not just the top-level one.
/// Two wrapping shapes are walked:
///   * `io::Error` wrapping another `io::Error` — the inner error is reachable
///     via [`std::io::Error::get_ref`]. (Note: `io::Error`'s `Error::source`
///     intentionally returns the *inner's* source, not the inner error itself,
///     so a `get_ref` walk is required to see a wrapped `io::Error`.)
///   * a non-`io` error wrapping an `io::Error` — reachable via the generic
///     [`std::error::Error::source`] chain.
fn is_retryable_not_connected(err: &std::io::Error) -> bool {
    fn matches_not_connected(err: &std::io::Error) -> bool {
        err.kind() == std::io::ErrorKind::NotConnected || err.raw_os_error() == Some(WSAENOTCONN)
    }
    // Walk the `io::Error` -> `io::Error` `get_ref` chain.
    let mut current = Some(err);
    while let Some(io_err) = current {
        if matches_not_connected(io_err) {
            return true;
        }
        current = io_err
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<std::io::Error>());
    }
    // Walk the generic `Error::source` chain for non-`io` wrappers, downcasting
    // each link back to `io::Error` where possible.
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>()
            && matches_not_connected(io_err)
        {
            return true;
        }
        source = cause.source();
    }
    false
}

/// Whether a TLS connect error is caused by a retryable "socket not
/// connected" I/O error (see [`is_retryable_not_connected`]).
///
/// The common shape is `TlsError::Io(io::Error)`, but a layered Winsock
/// provider can surface the WSAENOTCONN through the TLS library itself (e.g.
/// `TlsError::Rustls` / `TlsError::Handshake`), where the originating
/// `io::Error` is only reachable via the [`std::error::Error::source`] chain.
/// We therefore check the direct `Io` variant *and* walk the source chain so
/// the fresh-socket retry fires regardless of which variant the connector
/// chose to report (pi_agent_rust#111 / #106).
fn is_retryable_not_connected_tls(err: &TlsError) -> bool {
    if let TlsError::Io(io_err) = err
        && is_retryable_not_connected(io_err)
    {
        return true;
    }
    // Walk the generic source chain; any link that is (or wraps) a
    // "socket not connected" io::Error makes the connect retryable.
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>()
            && is_retryable_not_connected(io_err)
        {
            return true;
        }
        source = cause.source();
    }
    false
}

/// A failed connect attempt: the user-facing error plus whether it is a
/// retryable "socket not connected" failure (classified on the typed error
/// *before* it is flattened into a message string).
struct ConnectAttemptError {
    error: Error,
    retryable_not_connected: bool,
}

/// One full connect attempt: fresh TCP connection plus (for HTTPS) a fresh
/// TLS handshake. On failure the partially-connected stream is dropped, which
/// closes the socket.
async fn connect_transport_once(
    parsed: &ParsedUrl,
    client: &Client,
) -> std::result::Result<Transport, ConnectAttemptError> {
    let addr = (parsed.host.clone(), parsed.port);
    let tcp = TcpStream::connect(addr)
        .await
        .map_err(|e| ConnectAttemptError {
            retryable_not_connected: is_retryable_not_connected(&e),
            error: Error::from(e),
        })?;
    match parsed.scheme {
        Scheme::Http => Ok(Transport::Tcp(tcp)),
        Scheme::Https => {
            let tls = client.tls.as_ref().map_err(|e| ConnectAttemptError {
                error: Error::api(format!("TLS configuration error: {e}")),
                retryable_not_connected: false,
            })?;
            let tls_stream =
                tls.clone()
                    .connect(&parsed.host, tcp)
                    .await
                    .map_err(|e| ConnectAttemptError {
                        retryable_not_connected: is_retryable_not_connected_tls(&e),
                        error: Error::api(format!("TLS connect failed: {e}")),
                    })?;
            Ok(Transport::Tls(Box::new(tls_stream)))
        }
    }
}

async fn connect_transport(parsed: &ParsedUrl, client: &Client) -> Result<Transport> {
    use asupersync::time::{sleep, wall_now};

    let mut backoffs = NOT_CONNECTED_RETRY_BACKOFFS.iter();
    loop {
        let failure = match connect_transport_once(parsed, client).await {
            Ok(transport) => return Ok(transport),
            Err(failure) => failure,
        };
        if !failure.retryable_not_connected {
            return Err(failure.error);
        }
        let Some(&backoff) = backoffs.next() else {
            return Err(failure.error);
        };
        tracing::warn!(
            host = %parsed.host,
            error = %failure.error,
            backoff_ms = backoff.as_millis(),
            "connect reported socket-not-connected (WSAENOTCONN 10057); \
             retrying with a fresh connection (pi_agent_rust#106)"
        );
        // The failed transport was dropped (socket closed) when the attempt
        // returned; back off briefly, then redo the full TCP + TLS connect.
        let now = asupersync::Cx::current()
            .and_then(|cx| cx.timer_driver())
            .map_or_else(wall_now, |timer| timer.now());
        sleep(now, backoff).await;
    }
}

/// Strip CR/LF from header values to prevent HTTP header injection.
fn sanitize_header_value(value: &str) -> String {
    value.chars().filter(|&c| c != '\r' && c != '\n').collect()
}

/// Preserve only RFC 9110 token characters in outbound header names.
fn sanitize_header_name(name: &str) -> String {
    name.bytes()
        .filter(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    *b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
        .map(char::from)
        .collect()
}

fn validate_request_header(name: &str, value: &str) -> Result<()> {
    if name.is_empty() || sanitize_header_name(name) != name {
        return Err(Error::api(format!(
            "Invalid HTTP request header name: {name:?}"
        )));
    }
    if sanitize_header_value(value) != value || value.contains('\0') {
        return Err(Error::api(
            "Invalid HTTP request header value: contains a forbidden control character",
        ));
    }
    Ok(())
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().rev().find_map(|(key, value)| {
        if key.eq_ignore_ascii_case(name) {
            Some(value.as_str())
        } else {
            None
        }
    })
}

fn build_request_bytes(
    method: Method,
    parsed: &ParsedUrl,
    user_agent: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Vec<u8> {
    let mut out = String::new();
    let effective_user_agent =
        sanitize_header_value(header_value(headers, "user-agent").unwrap_or(user_agent));
    let host_header = host_header_value(parsed);
    let _ = std::fmt::Write::write_fmt(
        &mut out,
        format_args!("{} {} HTTP/1.1\r\n", method.as_str(), parsed.path),
    );
    let _ = std::fmt::Write::write_fmt(&mut out, format_args!("Host: {host_header}\r\n"));
    let _ = std::fmt::Write::write_fmt(
        &mut out,
        format_args!("User-Agent: {effective_user_agent}\r\n"),
    );
    let _ =
        std::fmt::Write::write_fmt(&mut out, format_args!("Content-Length: {}\r\n", body.len()));

    for (name, value) in headers {
        let clean_name = sanitize_header_name(name);
        if clean_name.is_empty()
            || clean_name.eq_ignore_ascii_case("host")
            || clean_name.eq_ignore_ascii_case("user-agent")
            || clean_name.eq_ignore_ascii_case("content-length")
            // This client only emits fixed-length request bodies, so
            // caller-supplied transfer codings would lie about the wire format.
            || clean_name.eq_ignore_ascii_case("transfer-encoding")
        {
            continue;
        }
        let clean_value = sanitize_header_value(value);
        let _ =
            std::fmt::Write::write_fmt(&mut out, format_args!("{clean_name}: {clean_value}\r\n"));
    }

    out.push_str("\r\n");
    out.into_bytes()
}

fn host_header_value(parsed: &ParsedUrl) -> String {
    let host = if parsed.host.contains(':') && !parsed.host.starts_with('[') {
        format!("[{}]", parsed.host)
    } else {
        parsed.host.clone()
    };

    let default_port = match parsed.scheme {
        Scheme::Http => 80,
        Scheme::Https => 443,
    };

    if parsed.port == default_port {
        host
    } else {
        format!("{host}:{}", parsed.port)
    }
}

async fn read_response_head(
    transport: &mut Transport,
) -> Result<(u16, Vec<(String, String)>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(8192);
    let mut scratch = [0u8; READ_CHUNK_BYTES];
    let mut search_start = 0;

    loop {
        if buf.len() > MAX_HEADER_BYTES {
            return Err(Error::api("HTTP response headers too large"));
        }

        let haystack = &buf[search_start..];
        if let Some(pos) = find_headers_end(haystack) {
            let absolute_pos = search_start + pos;
            let head = &buf[..absolute_pos];
            let leftover = buf[absolute_pos..].to_vec();
            let (status, headers) = parse_response_head(head)?;
            return Ok((status, headers, leftover));
        }

        let n = read_some(transport, &mut scratch).await?;
        if n == 0 {
            // A 0-byte read before the header terminator is an unexpected EOF:
            // the peer dropped the connection mid-response. This is transient
            // and safe to retry with a fresh connection, so it is tagged for
            // the retry classifier (pi_agent_rust#118).
            return Err(Error::api(
                "HTTP connection closed before headers (transient connection drop)",
            ));
        }
        let old_len = buf.len();
        buf.extend_from_slice(&scratch[..n]);
        search_start = old_len.saturating_sub(3);
    }
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i..].starts_with(b"\r\n\r\n") {
            return Some(i + 4);
        }
        if buf[i..].starts_with(b"\n\n") {
            return Some(i + 2);
        }
    }
    None
}

fn parse_response_head(head: &[u8]) -> Result<(u16, Vec<(String, String)>)> {
    let text =
        std::str::from_utf8(head).map_err(|e| Error::api(format!("Invalid HTTP headers: {e}")))?;
    let mut lines = text.lines();

    let status_line = lines
        .next()
        .ok_or_else(|| Error::api("Missing HTTP status line"))?;
    let mut parts = status_line.split_whitespace();
    let _version = parts
        .next()
        .ok_or_else(|| Error::api("Invalid HTTP status line"))?;
    let status_str = parts
        .next()
        .ok_or_else(|| Error::api("Invalid HTTP status line"))?;
    let status: u16 = status_str
        .parse()
        .map_err(|_| Error::api("Invalid HTTP status code"))?;

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| Error::api("Invalid HTTP header line"))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    Ok((status, headers))
}

#[derive(Debug, Clone, Copy)]
enum BodyKind {
    Empty,
    ContentLength(usize),
    Chunked,
    Eof,
}

fn body_kind_from_response(status: u16, headers: &[(String, String)]) -> Result<BodyKind> {
    if matches!(status, 100..=199 | 204 | 205 | 304) {
        return Ok(BodyKind::Empty);
    }
    body_kind_from_headers(headers)
}

fn body_kind_from_headers(headers: &[(String, String)]) -> Result<BodyKind> {
    let mut content_length = None;
    let mut transfer_encodings = Vec::new();
    let mut saw_transfer_encoding = false;

    for (name, value) in headers {
        let name_lc = name.to_ascii_lowercase();
        if name_lc == "content-length" {
            for part in value.split(',') {
                let parsed = part
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| Error::api("Invalid HTTP Content-Length header"))?;
                if let Some(existing) = content_length {
                    if existing != parsed {
                        return Err(Error::api("Conflicting HTTP Content-Length headers"));
                    }
                } else {
                    content_length = Some(parsed);
                }
            }
        } else if name_lc == "transfer-encoding" {
            saw_transfer_encoding = true;
            transfer_encodings.extend(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_ascii_lowercase),
            );
        }
    }

    if saw_transfer_encoding {
        let Some(last) = transfer_encodings.last() else {
            return Err(Error::api("Invalid HTTP Transfer-Encoding header"));
        };
        if last != "chunked" {
            return Err(Error::api("Unsupported HTTP Transfer-Encoding header"));
        }
        if transfer_encodings.len() != 1 {
            return Err(Error::api("Unsupported HTTP Transfer-Encoding header"));
        }
        return Ok(BodyKind::Chunked);
    }

    Ok(match content_length {
        Some(0) => BodyKind::Empty,
        Some(n) => BodyKind::ContentLength(n),
        None => BodyKind::Eof,
    })
}

struct Buffer {
    bytes: Vec<u8>,
    pos: usize,
}

impl Buffer {
    const fn new(initial: Vec<u8>) -> Self {
        Self {
            bytes: initial,
            pos: 0,
        }
    }

    fn available(&self) -> &[u8] {
        &self.bytes[self.pos..]
    }

    fn len(&self) -> usize {
        self.available().len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn consume(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n).min(self.bytes.len());
        if self.pos == self.bytes.len() {
            self.bytes.clear();
            self.pos = 0;
        } else if self.pos > 0 && self.pos >= self.bytes.len() / 2 {
            self.bytes.drain(..self.pos);
            self.pos = 0;
        }
    }

    fn extend(&mut self, data: &[u8]) -> Result<()> {
        if self.bytes.len().saturating_add(data.len()) > MAX_BUFFERED_BYTES {
            return Err(Error::api("HTTP body buffer exceeded"));
        }
        self.bytes.extend_from_slice(data);
        Ok(())
    }

    fn split_to_vec(&mut self, n: usize) -> Vec<u8> {
        let n = n.min(self.len());
        let out = self.available()[..n].to_vec();
        self.consume(n);
        out
    }
}

enum ChunkedState {
    SizeLine,
    Data { remaining: usize },
    DataCrlf,
    Trailers,
    Done,
}

struct BodyStreamState {
    transport: Transport,
    kind: BodyKind,
    buf: Buffer,
    chunked_state: ChunkedState,
    remaining: usize,
    transport_closed: bool,
}

impl BodyStreamState {
    const fn new(transport: Transport, kind: BodyKind, leftover: Vec<u8>) -> Self {
        let remaining = match kind {
            BodyKind::ContentLength(n) => n,
            _ => 0,
        };
        Self {
            transport,
            kind,
            buf: Buffer::new(leftover),
            chunked_state: ChunkedState::SizeLine,
            remaining,
            transport_closed: false,
        }
    }

    async fn next_bytes(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        match self.kind {
            BodyKind::Empty => Ok(None),
            BodyKind::Eof => Box::pin(self.next_eof()).await,
            BodyKind::ContentLength(_) => Box::pin(self.next_content_length()).await,
            BodyKind::Chunked => Box::pin(self.next_chunked()).await,
        }
    }

    async fn shutdown_transport_best_effort(&mut self) {
        if self.transport_closed {
            return;
        }
        self.transport_closed = true;
        let _ = self.transport.shutdown().await;
    }

    async fn read_more(&mut self) -> std::io::Result<usize> {
        let mut scratch = [0u8; READ_CHUNK_BYTES];
        let n = read_some(&mut self.transport, &mut scratch).await?;
        if n > 0
            && let Err(err) = self.buf.extend(&scratch[..n])
        {
            return Err(std::io::Error::other(err.to_string()));
        }
        Ok(n)
    }

    async fn next_eof(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        if !self.buf.is_empty() {
            return Ok(Some(self.buf.split_to_vec(self.buf.len())));
        }

        let n = Box::pin(self.read_more()).await?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(self.buf.split_to_vec(self.buf.len())))
    }

    async fn next_content_length(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        if self.remaining == 0 {
            return Ok(None);
        }

        if self.buf.is_empty() {
            let n = Box::pin(self.read_more()).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unexpected EOF reading content-length body",
                ));
            }
        }

        let to_take = self.remaining.min(self.buf.len()).min(READ_CHUNK_BYTES);
        let out = self.buf.split_to_vec(to_take);
        self.remaining = self.remaining.saturating_sub(out.len());
        Ok(Some(out))
    }

    #[allow(clippy::too_many_lines)]
    async fn next_chunked(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            match self.chunked_state {
                ChunkedState::SizeLine => {
                    if let Some((line_end, len)) = find_crlf(self.buf.available()) {
                        let line = &self.buf.available()[..line_end];
                        let line_str = std::str::from_utf8(line).map_err(std::io::Error::other)?;
                        let size_part = line_str.split(';').next().unwrap_or("").trim();
                        if size_part.is_empty() {
                            return Err(std::io::Error::other("invalid chunk size"));
                        }
                        let chunk_size = usize::from_str_radix(size_part, 16)
                            .map_err(|_| std::io::Error::other("invalid chunk size"))?;
                        self.buf.consume(line_end + len);
                        if chunk_size == 0 {
                            self.chunked_state = ChunkedState::Trailers;
                        } else {
                            self.chunked_state = ChunkedState::Data {
                                remaining: chunk_size,
                            };
                        }
                        continue;
                    }

                    let n = Box::pin(self.read_more()).await?;
                    if n == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "unexpected EOF reading chunk size",
                        ));
                    }
                }

                ChunkedState::Data { remaining } => {
                    if remaining == 0 {
                        self.chunked_state = ChunkedState::DataCrlf;
                        continue;
                    }

                    if self.buf.is_empty() {
                        let n = Box::pin(self.read_more()).await?;
                        if n == 0 {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "unexpected EOF reading chunk data",
                            ));
                        }
                    }

                    let to_take = remaining.min(self.buf.len()).min(READ_CHUNK_BYTES);
                    let out = self.buf.split_to_vec(to_take);
                    self.chunked_state = ChunkedState::Data {
                        remaining: remaining.saturating_sub(out.len()),
                    };
                    return Ok(Some(out));
                }

                ChunkedState::DataCrlf => {
                    if self.buf.len() < 2 {
                        let n = Box::pin(self.read_more()).await?;
                        if n == 0 && self.buf.is_empty() {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "unexpected EOF reading chunk CRLF",
                            ));
                        }
                        // Continue to let starts_with handle single byte \n or full \r\n
                    }

                    let bytes = self.buf.available();
                    if bytes.starts_with(b"\r\n") {
                        self.buf.consume(2);
                        self.chunked_state = ChunkedState::SizeLine;
                    } else if bytes.starts_with(b"\n") {
                        self.buf.consume(1);
                        self.chunked_state = ChunkedState::SizeLine;
                    } else if bytes.len() >= 2 {
                        return Err(std::io::Error::other("invalid chunk CRLF"));
                    } else {
                        // wait for more data
                        let n = Box::pin(self.read_more()).await?;
                        if n == 0 {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "unexpected EOF reading chunk CRLF",
                            ));
                        }
                    }
                }

                ChunkedState::Trailers => {
                    // Trailers are terminated by an empty line. When there are no trailers,
                    // the terminator is a single CRLF (`0\r\n\r\n` total, with the final
                    // `\r\n` remaining after consuming the size line).
                    let bytes = self.buf.available();
                    if bytes.starts_with(b"\r\n") {
                        self.buf.consume(2);
                        self.chunked_state = ChunkedState::Done;
                        return Ok(None);
                    }
                    if bytes.starts_with(b"\n") {
                        self.buf.consume(1);
                        self.chunked_state = ChunkedState::Done;
                        return Ok(None);
                    }
                    if let Some(end) = find_headers_end(self.buf.available()) {
                        self.buf.consume(end);
                        self.chunked_state = ChunkedState::Done;
                        return Ok(None);
                    }

                    let n = Box::pin(self.read_more()).await?;
                    if n == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "unexpected EOF reading trailers",
                        ));
                    }
                }

                ChunkedState::Done => return Ok(None),
            }
        }
    }
}

fn find_crlf(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len() {
        if buf[i..].starts_with(b"\r\n") {
            return Some((i, 2));
        }
        if buf[i..].starts_with(b"\n") {
            return Some((i, 1));
        }
    }
    None
}

async fn read_some<R: AsyncRead + Unpin>(reader: &mut R, dst: &mut [u8]) -> std::io::Result<usize> {
    futures::future::poll_fn(|cx| {
        let mut read_buf = ReadBuf::new(dst);
        match Pin::new(&mut *reader).poll_read(cx, &mut read_buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
        }
    })
    .await
}

#[derive(Debug)]
enum Transport {
    Tcp(TcpStream),
    Tls(Box<asupersync::tls::TlsStream<TcpStream>>),
}

impl Unpin for Transport {}

impl AsyncRead for Transport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Tcp(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(&mut **stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut *self {
            Self::Tcp(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(&mut **stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Tcp(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(&mut **stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Tcp(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(&mut **stream).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;

    // ── Method ──────────────────────────────────────────────────────────
    #[test]
    fn method_as_str_get() {
        assert_eq!(Method::Get.as_str(), "GET");
    }

    #[test]
    fn method_as_str_post() {
        assert_eq!(Method::Post.as_str(), "POST");
    }

    // ── is_retryable_not_connected (#106 / #66 / asupersync#35) ─────────
    #[test]
    fn retryable_not_connected_kind() {
        let err = std::io::Error::new(std::io::ErrorKind::NotConnected, "Socket is not connected");
        assert!(is_retryable_not_connected(&err));
    }

    #[test]
    fn retryable_wsaenotconn_raw_os_error() {
        // On Windows from_raw_os_error(10057) also maps kind() to
        // NotConnected; on Unix the kind is uncategorized and only the raw
        // code matches. Both paths must classify as retryable.
        let err = std::io::Error::from_raw_os_error(WSAENOTCONN);
        assert!(is_retryable_not_connected(&err));
    }

    #[test]
    fn retryable_not_connected_wrapped_in_source_chain() {
        // A layered transport may wrap the original socket error in another
        // io::Error with a different kind; the source chain must be walked.
        let inner = std::io::Error::from_raw_os_error(WSAENOTCONN);
        let outer = std::io::Error::other(inner);
        assert!(is_retryable_not_connected(&outer));

        let inner = std::io::Error::new(std::io::ErrorKind::NotConnected, "not connected");
        let outer = std::io::Error::new(std::io::ErrorKind::BrokenPipe, inner);
        assert!(is_retryable_not_connected(&outer));
    }

    #[test]
    fn not_retryable_wrapped_other_error() {
        let inner = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let outer = std::io::Error::other(inner);
        assert!(!is_retryable_not_connected(&outer));
    }

    #[test]
    fn not_retryable_connection_refused() {
        let err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert!(!is_retryable_not_connected(&err));
    }

    #[test]
    fn not_retryable_generic_io_error() {
        let err = std::io::Error::other("boom");
        assert!(!is_retryable_not_connected(&err));
    }

    #[test]
    fn retryable_tls_io_not_connected() {
        let err = TlsError::Io(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "Socket is not connected",
        ));
        assert!(is_retryable_not_connected_tls(&err));
    }

    #[test]
    fn retryable_tls_io_wsaenotconn_raw() {
        let err = TlsError::Io(std::io::Error::from_raw_os_error(WSAENOTCONN));
        assert!(is_retryable_not_connected_tls(&err));
    }

    #[test]
    fn not_retryable_tls_handshake_failure() {
        let err = TlsError::Handshake("handshake failure".to_string());
        assert!(!is_retryable_not_connected_tls(&err));
    }

    #[test]
    fn retryable_tls_io_source_chain_wsaenotconn() {
        // A layered Winsock provider can wrap the originating WSAENOTCONN
        // io::Error inside another io::Error reported as `TlsError::Io`; the
        // raw os error is only reachable via the get_ref/source chain
        // (pi_agent_rust#111). The classifier must still treat it as
        // retryable.
        let inner = std::io::Error::from_raw_os_error(WSAENOTCONN);
        let wrapped = std::io::Error::other(inner);
        let err = TlsError::Io(wrapped);
        assert!(is_retryable_not_connected_tls(&err));
    }

    #[test]
    fn not_retryable_tls_io_other_kind() {
        let err = TlsError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ));
        assert!(!is_retryable_not_connected_tls(&err));
    }

    // ── find_headers_end ────────────────────────────────────────────────
    #[test]
    fn find_headers_end_present() {
        let buf = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let pos = find_headers_end(buf).unwrap();
        assert_eq!(&buf[pos..], b"hello");
    }

    #[test]
    fn find_headers_end_absent() {
        assert!(find_headers_end(b"HTTP/1.1 200 OK\r\nFoo: bar\r\n").is_none());
    }

    #[test]
    fn find_headers_end_empty() {
        assert!(find_headers_end(b"").is_none());
    }

    #[test]
    fn find_headers_end_just_separator() {
        let buf = b"\r\n\r\n";
        assert_eq!(find_headers_end(buf), Some(4));
    }

    // ── find_crlf ──────────────────────────────────────────────────────
    #[test]
    fn find_crlf_present() {
        assert_eq!(find_crlf(b"abc\r\ndef"), Some((3, 2)));
    }

    #[test]
    fn find_crlf_present_lf() {
        assert_eq!(find_crlf(b"abc\ndef"), Some((3, 1)));
    }

    #[test]
    fn find_crlf_absent() {
        assert!(find_crlf(b"abcdef").is_none());
    }

    #[test]
    fn find_crlf_at_start() {
        assert_eq!(find_crlf(b"\r\ndata"), Some((0, 2)));
    }

    #[test]
    fn find_crlf_at_start_lf() {
        assert_eq!(find_crlf(b"\ndata"), Some((0, 1)));
    }

    // ── parse_response_head ────────────────────────────────────────────
    #[test]
    fn parse_response_head_200() {
        let head = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n";
        let (status, headers) = parse_response_head(head).unwrap();
        assert_eq!(status, 200);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "Content-Type");
        assert_eq!(headers[0].1, "text/plain");
    }

    #[test]
    fn parse_response_head_404() {
        let head = b"HTTP/1.1 404 Not Found\r\n\r\n";
        let (status, headers) = parse_response_head(head).unwrap();
        assert_eq!(status, 404);
        assert!(headers.is_empty());
    }

    #[test]
    fn parse_response_head_multiple_headers() {
        let head = b"HTTP/1.1 200 OK\r\nA: 1\r\nB: 2\r\nC: 3\r\n\r\n";
        let (status, headers) = parse_response_head(head).unwrap();
        assert_eq!(status, 200);
        assert_eq!(headers.len(), 3);
        assert_eq!(headers[0], ("A".to_string(), "1".to_string()));
        assert_eq!(headers[1], ("B".to_string(), "2".to_string()));
        assert_eq!(headers[2], ("C".to_string(), "3".to_string()));
    }

    #[test]
    fn parse_response_head_header_value_with_colon() {
        // Header value contains a colon (e.g., a URL)
        let head = b"HTTP/1.1 200 OK\r\nLocation: http://example.com:8080/path\r\n\r\n";
        let (status, headers) = parse_response_head(head).unwrap();
        assert_eq!(status, 200);
        assert_eq!(headers[0].0, "Location");
        assert_eq!(headers[0].1, "http://example.com:8080/path");
    }

    #[test]
    fn parse_response_head_invalid_status_code() {
        let head = b"HTTP/1.1 abc OK\r\n\r\n";
        assert!(parse_response_head(head).is_err());
    }

    #[test]
    fn parse_response_head_missing_status() {
        let head = b"HTTP/1.1\r\n\r\n";
        assert!(parse_response_head(head).is_err());
    }

    #[test]
    fn parse_response_head_empty() {
        let head = b"";
        assert!(parse_response_head(head).is_err());
    }

    // ── body_kind_from_headers ─────────────────────────────────────────
    #[test]
    fn body_kind_content_length() {
        let headers = vec![("Content-Length".to_string(), "42".to_string())];
        assert!(matches!(
            body_kind_from_headers(&headers).unwrap(),
            BodyKind::ContentLength(42)
        ));
    }

    #[test]
    fn body_kind_content_length_zero() {
        let headers = vec![("Content-Length".to_string(), "0".to_string())];
        assert!(matches!(
            body_kind_from_headers(&headers).unwrap(),
            BodyKind::Empty
        ));
    }

    #[test]
    fn body_kind_chunked() {
        let headers = vec![("Transfer-Encoding".to_string(), "chunked".to_string())];
        assert!(matches!(
            body_kind_from_headers(&headers).unwrap(),
            BodyKind::Chunked
        ));
    }

    #[test]
    fn body_kind_rejects_chunked_with_additional_transfer_codings() {
        let headers = vec![("Transfer-Encoding".to_string(), "gzip, chunked".to_string())];
        assert!(body_kind_from_headers(&headers).is_err());
    }

    #[test]
    fn body_kind_rejects_repeated_transfer_encoding_headers_with_extra_codings() {
        let headers = vec![
            ("Transfer-Encoding".to_string(), "gzip".to_string()),
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
        ];
        assert!(body_kind_from_headers(&headers).is_err());
    }

    #[test]
    fn body_kind_rejects_repeated_chunked_transfer_encoding() {
        let headers = vec![
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
        ];
        assert!(body_kind_from_headers(&headers).is_err());
    }

    #[test]
    fn body_kind_rejects_transfer_encoding_when_chunked_is_not_final() {
        let headers = vec![
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
            ("Transfer-Encoding".to_string(), "gzip".to_string()),
        ];
        assert!(body_kind_from_headers(&headers).is_err());
    }

    #[test]
    fn body_kind_rejects_non_chunked_transfer_encoding() {
        let headers = vec![("Transfer-Encoding".to_string(), "gzip".to_string())];
        assert!(body_kind_from_headers(&headers).is_err());
    }

    #[test]
    fn body_kind_chunked_overrides_content_length() {
        // When both present, chunked wins
        let headers = vec![
            ("Content-Length".to_string(), "100".to_string()),
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
        ];
        assert!(matches!(
            body_kind_from_headers(&headers).unwrap(),
            BodyKind::Chunked
        ));
    }

    #[test]
    fn body_kind_eof_no_headers() {
        let headers: Vec<(String, String)> = Vec::new();
        assert!(matches!(
            body_kind_from_headers(&headers).unwrap(),
            BodyKind::Eof
        ));
    }

    #[test]
    fn body_kind_case_insensitive() {
        let headers = vec![("content-length".to_string(), "10".to_string())];
        assert!(matches!(
            body_kind_from_headers(&headers).unwrap(),
            BodyKind::ContentLength(10)
        ));
    }

    #[test]
    fn body_kind_response_204_without_headers_is_empty() {
        let headers: Vec<(String, String)> = Vec::new();
        assert!(matches!(
            body_kind_from_response(204, &headers).unwrap(),
            BodyKind::Empty
        ));
    }

    #[test]
    fn body_kind_response_304_ignores_content_length() {
        let headers = vec![("Content-Length".to_string(), "7".to_string())];
        assert!(matches!(
            body_kind_from_response(304, &headers).unwrap(),
            BodyKind::Empty
        ));
    }

    #[test]
    fn body_kind_response_205_without_headers_is_empty() {
        let headers: Vec<(String, String)> = Vec::new();
        assert!(matches!(
            body_kind_from_response(205, &headers).unwrap(),
            BodyKind::Empty
        ));
    }

    // ── build_request_bytes ────────────────────────────────────────────
    #[test]
    fn build_request_bytes_get() {
        let parsed = ParsedUrl::parse("http://example.com/api/test").unwrap();
        let bytes = build_request_bytes(Method::Get, &parsed, "test-agent", &[], &[]);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("GET /api/test HTTP/1.1\r\n"));
        assert!(text.contains("Host: example.com\r\n"));
        assert!(text.contains("User-Agent: test-agent\r\n"));
        assert!(text.contains("Content-Length: 0\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn build_request_bytes_post_with_body() {
        let parsed = ParsedUrl::parse("https://api.example.com/v1/messages").unwrap();
        let body = b"hello world";
        let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        let bytes = build_request_bytes(Method::Post, &parsed, "pi/0.1", &headers, body);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("POST /v1/messages HTTP/1.1\r\n"));
        assert!(text.contains("Host: api.example.com\r\n"));
        assert!(text.contains("Content-Length: 11\r\n"));
        assert!(text.contains("Content-Type: application/json\r\n"));
    }

    #[test]
    fn build_request_bytes_custom_headers() {
        let parsed = ParsedUrl::parse("http://localhost/test").unwrap();
        let headers = vec![
            ("Authorization".to_string(), "Bearer sk-test".to_string()),
            ("X-Custom".to_string(), "value".to_string()),
        ];
        let bytes = build_request_bytes(Method::Post, &parsed, "agent", &headers, &[]);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("Authorization: Bearer sk-test\r\n"));
        assert!(text.contains("X-Custom: value\r\n"));
    }

    #[test]
    fn build_request_bytes_reserved_headers_are_canonicalized() {
        let parsed = ParsedUrl::parse("https://api.example.com/v1/messages").unwrap();
        let headers = vec![
            ("Host".to_string(), "spoofed.example.com".to_string()),
            ("User-Agent".to_string(), "custom-agent".to_string()),
            ("Content-Length".to_string(), "999".to_string()),
            ("X-Test".to_string(), "1".to_string()),
        ];
        let body = b"hello";
        let bytes = build_request_bytes(Method::Post, &parsed, "default-agent", &headers, body);
        let text = String::from_utf8(bytes).unwrap();

        assert_eq!(text.matches("Host: ").count(), 1);
        assert!(text.contains("Host: api.example.com\r\n"));
        assert!(!text.contains("Host: spoofed.example.com\r\n"));

        assert_eq!(text.matches("User-Agent: ").count(), 1);
        assert!(text.contains("User-Agent: custom-agent\r\n"));
        assert!(!text.contains("User-Agent: default-agent\r\n"));

        assert_eq!(text.matches("Content-Length: ").count(), 1);
        assert!(text.contains("Content-Length: 5\r\n"));
        assert!(!text.contains("Content-Length: 999\r\n"));

        assert!(text.contains("X-Test: 1\r\n"));
    }

    #[test]
    fn build_request_bytes_non_default_port_includes_port_in_host_header() {
        let parsed = ParsedUrl::parse("http://example.com:8080/api/test").unwrap();
        let bytes = build_request_bytes(Method::Get, &parsed, "agent", &[], &[]);
        let text = String::from_utf8(bytes).unwrap();

        assert!(text.contains("Host: example.com:8080\r\n"));
    }

    #[test]
    fn build_request_bytes_sanitizes_overridden_user_agent() {
        let parsed = ParsedUrl::parse("http://example.com/test").unwrap();
        let headers = vec![(
            "User-Agent".to_string(),
            "custom-agent\r\nX-Injected: nope".to_string(),
        )];
        let bytes = build_request_bytes(Method::Get, &parsed, "agent", &headers, &[]);
        let text = String::from_utf8(bytes).unwrap();

        assert!(text.contains("User-Agent: custom-agentX-Injected: nope\r\n"));
        assert_eq!(text.matches("User-Agent: ").count(), 1);
        assert!(!text.contains("\r\nX-Injected: nope\r\n"));
    }

    // ── build_recorded_request ─────────────────────────────────────────
    #[test]
    fn build_recorded_request_empty_body() {
        let req = build_recorded_request(Method::Post, "https://api.test.com/v1", &[], &[]);
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://api.test.com/v1");
        assert!(req.body.is_none());
        assert!(req.body_text.is_none());
    }

    #[test]
    fn build_recorded_request_json_body() {
        let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        let body = serde_json::to_vec(&json!({"model": "test"})).unwrap();
        let req = build_recorded_request(Method::Post, "https://api.test.com/v1", &headers, &body);
        assert!(req.body.is_some());
        assert_eq!(req.body.unwrap()["model"], "test");
        assert!(req.body_text.is_none());
    }

    #[test]
    fn build_recorded_request_text_body() {
        let headers = vec![("Content-Type".to_string(), "text/plain".to_string())];
        let body = b"hello world";
        let req = build_recorded_request(Method::Post, "https://api.test.com/v1", &headers, body);
        assert!(req.body.is_none());
        assert_eq!(req.body_text.as_deref(), Some("hello world"));
    }

    #[test]
    fn build_recorded_request_invalid_json_body_falls_back_to_text() {
        let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        let body = b"not json {{{";
        let req = build_recorded_request(Method::Post, "https://api.test.com/v1", &headers, body);
        assert!(req.body.is_none());
        assert_eq!(req.body_text.as_deref(), Some("not json {{{"));
    }

    #[test]
    fn build_recorded_request_preserves_headers() {
        let headers = vec![
            ("Authorization".to_string(), "Bearer key".to_string()),
            ("X-Trace".to_string(), "abc123".to_string()),
        ];
        let req = build_recorded_request(Method::Get, "https://test.com", &headers, &[]);
        assert_eq!(req.headers.len(), 2);
        assert_eq!(req.headers[0].0, "Authorization");
    }

    // ── Buffer ─────────────────────────────────────────────────────────
    #[test]
    fn buffer_new_empty() {
        let buf = Buffer::new(Vec::new());
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn buffer_new_with_data() {
        let buf = Buffer::new(vec![1, 2, 3]);
        assert!(!buf.is_empty());
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.available(), &[1, 2, 3]);
    }

    #[test]
    fn buffer_consume_partial() {
        let mut buf = Buffer::new(vec![1, 2, 3, 4, 5]);
        buf.consume(2);
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.available(), &[3, 4, 5]);
    }

    #[test]
    fn buffer_consume_all() {
        let mut buf = Buffer::new(vec![1, 2, 3]);
        buf.consume(3);
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn buffer_consume_triggers_compact() {
        // When pos >= len/2, the buffer compacts
        let mut buf = Buffer::new(vec![0; 10]);
        buf.consume(6); // pos=6, len=10, 6 >= 5 → compact
        assert_eq!(buf.len(), 4);
        assert_eq!(buf.available().len(), 4);
    }

    #[test]
    fn buffer_extend() {
        let mut buf = Buffer::new(vec![1, 2]);
        buf.extend(&[3, 4, 5]).unwrap();
        assert_eq!(buf.len(), 5);
        assert_eq!(buf.available(), &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn buffer_extend_overflow() {
        let mut buf = Buffer::new(Vec::new());
        let huge = vec![0u8; MAX_BUFFERED_BYTES + 1];
        assert!(buf.extend(&huge).is_err());
    }

    #[test]
    fn buffer_split_to_vec() {
        let mut buf = Buffer::new(vec![1, 2, 3, 4, 5]);
        let out = buf.split_to_vec(3);
        assert_eq!(out, vec![1, 2, 3]);
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.available(), &[4, 5]);
    }

    #[test]
    fn buffer_split_to_vec_more_than_available() {
        let mut buf = Buffer::new(vec![1, 2]);
        let out = buf.split_to_vec(10);
        assert_eq!(out, vec![1, 2]);
        assert!(buf.is_empty());
    }

    #[test]
    fn buffer_consume_then_extend() {
        let mut buf = Buffer::new(vec![1, 2, 3]);
        buf.consume(2);
        buf.extend(&[4, 5]).unwrap();
        // After consume(2), available = [3], then extend [4,5] → [3, 4, 5]
        assert_eq!(buf.available(), &[3, 4, 5]);
    }

    #[test]
    fn buffer_consume_exactly_all_clears() {
        let mut buf = Buffer::new(vec![1, 2, 3]);
        buf.consume(3);
        // pos == bytes.len() triggers clear
        assert!(buf.is_empty());
        assert_eq!(buf.available(), &[] as &[u8]);
    }

    // ── Client builder methods ─────────────────────────────────────────
    #[test]
    fn client_default() {
        let client = Client::default();
        assert!(client.vcr().is_none());
    }

    #[test]
    fn client_with_vcr() {
        let recorder = VcrRecorder::new_with(
            "test",
            crate::vcr::VcrMode::Playback,
            std::path::Path::new("/tmp"),
        );
        let client = Client::new().with_vcr(recorder);
        assert!(client.vcr().is_some());
    }

    // ── RequestBuilder ─────────────────────────────────────────────────
    #[test]
    fn request_builder_header_chaining() {
        let client = Client::new();
        let builder = client
            .post("https://api.example.com")
            .header("Authorization", "Bearer test")
            .header("X-Custom", "value");
        assert_eq!(builder.headers.len(), 2);
    }

    #[test]
    fn request_builder_header_replaces_case_insensitive_duplicate_names() {
        let client = Client::new();
        let builder = client
            .post("https://api.example.com")
            .header("Authorization", "Bearer first")
            .header("authorization", "Bearer second");

        assert_eq!(builder.headers.len(), 1);
        assert!(builder.headers[0].0.eq_ignore_ascii_case("authorization"));
        assert_eq!(builder.headers[0].1, "Bearer second");
    }

    #[test]
    fn request_builder_try_header_rejects_injection_bytes() {
        let client = Client::new();

        assert!(
            client
                .post("https://api.example.com")
                .try_header("X-GitLab-Feature", "duo-chat")
                .is_ok()
        );
        assert!(
            client
                .post("https://api.example.com")
                .try_header("Bad\r\nName", "value")
                .is_err()
        );
        assert!(
            client
                .post("https://api.example.com")
                .try_header("Bad:Name", "value")
                .is_err()
        );
        assert!(
            client
                .post("https://api.example.com")
                .try_header("X-Good", "ok\r\nX-Injected: yes")
                .is_err()
        );
        assert!(
            client
                .post("https://api.example.com")
                .try_header("X-Good", "bad\0value")
                .is_err()
        );
    }

    #[test]
    fn request_builder_header_bounds_prevent_dos() {
        // Test that header count is bounded to prevent DoS attacks.
        let client = Client::new();
        let mut builder = client.post("https://api.example.com");

        // Add headers up to the limit
        for i in 0..MAX_REQUEST_HEADERS {
            builder = builder.header(format!("X-Header-{i}"), "value");
        }
        assert_eq!(builder.headers.len(), MAX_REQUEST_HEADERS);

        // Additional headers should be silently dropped
        builder = builder
            .header("X-Over-Limit-1", "dropped")
            .header("X-Over-Limit-2", "also-dropped");
        assert_eq!(builder.headers.len(), MAX_REQUEST_HEADERS);

        // But replacing existing headers should still work
        builder = builder.header("X-Header-0", "replaced-value");
        assert_eq!(builder.headers.len(), MAX_REQUEST_HEADERS);
        assert_eq!(builder.headers[0].1, "replaced-value");
    }

    #[test]
    fn request_builder_try_header_reports_capacity_and_still_allows_replacement() {
        let client = Client::new();
        let mut builder = client.post("https://api.example.com");
        for i in 0..MAX_REQUEST_HEADERS {
            builder = builder
                .try_header(format!("X-Header-{i}"), "value")
                .expect("headers within the cap");
        }

        let Err(error) = builder.try_header("X-Over-Limit", "must-not-be-dropped-silently") else {
            panic!("a distinct header beyond the cap must be reported");
        };
        assert!(error.to_string().contains("header limit"), "{error}");

        let mut replaceable = client.post("https://api.example.com");
        for i in 0..MAX_REQUEST_HEADERS {
            replaceable = replaceable
                .try_header(format!("X-Header-{i}"), "value")
                .expect("headers within the cap");
        }
        replaceable = replaceable
            .try_header("x-header-0", "replacement")
            .expect("case-insensitive replacement at capacity");
        assert_eq!(replaceable.headers.len(), MAX_REQUEST_HEADERS);
        assert_eq!(replaceable.headers[0].1, "replacement");
    }

    #[test]
    fn request_builder_json() {
        let client = Client::new();
        let builder = client
            .post("https://api.example.com")
            .json(&json!({"key": "value"}))
            .unwrap();
        assert!(!builder.body.is_empty());
        // Should have auto-added Content-Type header
        assert!(
            builder
                .headers
                .iter()
                .any(|(k, v)| k == "Content-Type" && v == "application/json")
        );
    }

    #[test]
    fn request_builder_body() {
        let client = Client::new();
        let builder = client
            .post("https://api.example.com")
            .body(b"raw bytes".to_vec());
        assert_eq!(builder.body, b"raw bytes");
    }

    #[test]
    fn request_builder_default_timeout() {
        let client = Client::new();
        let builder = client.get("https://api.example.com");
        // The default is resolved lazily at send time so it can be
        // provider-aware (longer for local providers like Ollama).
        assert_eq!(builder.timeout, RequestTimeout::Default);
    }

    #[test]
    fn request_builder_timeout() {
        let client = Client::new();
        let builder = client
            .get("https://api.example.com")
            .timeout(std::time::Duration::from_secs(30));
        assert_eq!(
            builder.timeout,
            RequestTimeout::Explicit(std::time::Duration::from_secs(30))
        );
    }

    #[test]
    fn request_builder_no_timeout() {
        let client = Client::new();
        let builder = client.get("https://api.example.com").no_timeout();
        assert_eq!(builder.timeout, RequestTimeout::Disabled);
    }

    #[test]
    fn url_is_local_provider_detects_loopback() {
        assert!(url_is_local_provider("http://127.0.0.1:11434/v1"));
        assert!(url_is_local_provider("http://localhost:1234/v1"));
        assert!(url_is_local_provider("http://[::1]:11434/v1"));
        assert!(url_is_local_provider("http://0.0.0.0:11434/v1"));
        assert!(!url_is_local_provider("https://api.openai.com/v1"));
        assert!(!url_is_local_provider("https://api.anthropic.com/v1"));
        assert!(!url_is_local_provider("not a url"));
    }

    #[test]
    fn timeout_error_message_mentions_setting_and_ollama() {
        let local = timeout_error_message(
            "http://127.0.0.1:11434/v1",
            std::time::Duration::from_secs(600),
        );
        assert!(local.contains("600s"));
        assert!(local.contains("PI_HTTP_REQUEST_TIMEOUT_SECS"));
        assert!(local.contains("--request-timeout"));
        assert!(local.contains("requestTimeoutSecs"));
        assert!(local.contains("Ollama"));
        assert!(local.contains("pull"));

        let remote = timeout_error_message(
            "https://api.openai.com/v1",
            std::time::Duration::from_secs(60),
        );
        assert!(remote.contains("60s"));
        assert!(remote.contains("PI_HTTP_REQUEST_TIMEOUT_SECS"));
        // Cloud providers should not get Ollama-specific guidance.
        assert!(!remote.contains("Ollama"));
    }

    struct MockRetryWriter {
        writes: VecDeque<std::io::Result<usize>>,
        flushes: VecDeque<std::io::Result<()>>,
        written: Vec<u8>,
    }

    impl MockRetryWriter {
        fn new(
            writes: impl IntoIterator<Item = std::io::Result<usize>>,
            flushes: impl IntoIterator<Item = std::io::Result<()>>,
        ) -> Self {
            Self {
                writes: writes.into_iter().collect(),
                flushes: flushes.into_iter().collect(),
                written: Vec::new(),
            }
        }
    }

    impl AsyncWrite for MockRetryWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let result = self.writes.pop_front().unwrap_or(Ok(buf.len()));
            if let Ok(written) = result {
                self.written
                    .extend_from_slice(&buf[..written.min(buf.len())]);
            }
            Poll::Ready(result)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(self.flushes.pop_front().unwrap_or(Ok(())))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn write_all_with_retry_propagates_flush_error_after_zero_write() {
        asupersync::test_utils::run_test(|| async {
            let mut writer = MockRetryWriter::new(
                [Ok(0)],
                [Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "flush failed",
                ))],
            );

            let err = write_all_with_retry(&mut writer, b"hello")
                .await
                .expect_err("flush failure should not be swallowed");
            assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
            assert_eq!(err.to_string(), "flush failed");
            assert!(writer.written.is_empty());
        });
    }

    #[test]
    fn write_all_with_retry_recovers_after_zero_write_when_flush_succeeds() {
        asupersync::test_utils::run_test(|| async {
            let mut writer = MockRetryWriter::new([Ok(0), Ok(2), Ok(3)], [Ok(())]);

            write_all_with_retry(&mut writer, b"hello")
                .await
                .expect("retry helper should recover after transient zero write");
            assert_eq!(writer.written, b"hello");
        });
    }

    // ── Response ───────────────────────────────────────────────────────
    #[test]
    fn response_accessors() {
        let response = Response {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
            stream: Box::pin(futures::stream::empty()),
            timeout_info: None,
        };
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers().len(), 1);
        assert_eq!(response.headers()[0].0, "Content-Type");
    }

    #[test]
    fn response_text() {
        asupersync::test_utils::run_test(|| async {
            let chunks = vec![Ok(b"hello ".to_vec()), Ok(b"world".to_vec())];
            let response = Response {
                status: 200,
                headers: Vec::new(),
                stream: Box::pin(futures::stream::iter(chunks)),
                timeout_info: None,
            };
            let text = response.text().await.unwrap();
            assert_eq!(text, "hello world");
        });
    }

    #[test]
    fn response_text_empty() {
        asupersync::test_utils::run_test(|| async {
            let response = Response {
                status: 200,
                headers: Vec::new(),
                stream: Box::pin(futures::stream::empty()),
                timeout_info: None,
            };
            let text = response.text().await.unwrap();
            assert_eq!(text, "");
        });
    }

    #[test]
    fn response_bytes_stream() {
        asupersync::test_utils::run_test(|| async {
            let chunks = vec![Ok(b"data".to_vec())];
            let response = Response {
                status: 200,
                headers: Vec::new(),
                stream: Box::pin(futures::stream::iter(chunks)),
                timeout_info: None,
            };
            let mut stream = response.bytes_stream();
            let first = stream.next().await.unwrap().unwrap();
            assert_eq!(first, b"data");
            assert!(stream.next().await.is_none());
        });
    }

    // ── Body stream via Response (in-memory) ──────────────────────────
    #[test]
    fn body_stream_content_length_via_response() {
        asupersync::test_utils::run_test(|| async {
            // Simulate a content-length response by providing exact chunks
            let body = b"Hello, World!";
            let chunks: Vec<std::io::Result<Vec<u8>>> = vec![Ok(body.to_vec())];
            let response = Response {
                status: 200,
                headers: vec![("Content-Length".to_string(), "13".to_string())],
                stream: Box::pin(futures::stream::iter(chunks)),
                timeout_info: None,
            };
            let text = response.text().await.unwrap();
            assert_eq!(text, "Hello, World!");
        });
    }

    #[test]
    fn body_stream_multiple_chunks_via_response() {
        asupersync::test_utils::run_test(|| async {
            let chunks: Vec<std::io::Result<Vec<u8>>> = vec![
                Ok(b"chunk1".to_vec()),
                Ok(b"chunk2".to_vec()),
                Ok(b"chunk3".to_vec()),
            ];
            let response = Response {
                status: 200,
                headers: Vec::new(),
                stream: Box::pin(futures::stream::iter(chunks)),
                timeout_info: None,
            };
            let text = response.text().await.unwrap();
            assert_eq!(text, "chunk1chunk2chunk3");
        });
    }

    #[test]
    fn body_stream_error_propagation() {
        asupersync::test_utils::run_test(|| async {
            let chunks: Vec<std::io::Result<Vec<u8>>> = vec![
                Ok(b"data".to_vec()),
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "connection reset",
                )),
            ];
            let response = Response {
                status: 200,
                headers: Vec::new(),
                stream: Box::pin(futures::stream::iter(chunks)),
                timeout_info: None,
            };
            let result = response.text().await;
            assert!(result.is_err());
        });
    }

    // ── Edge cases ─────────────────────────────────────────────────────
    #[test]
    fn parse_response_head_trims_header_whitespace() {
        let head = b"HTTP/1.1 200 OK\r\n  X-Padded  :   value with spaces  \r\n\r\n";
        let (status, headers) = parse_response_head(head).unwrap();
        assert_eq!(status, 200);
        assert_eq!(headers[0].0, "X-Padded");
        assert_eq!(headers[0].1, "value with spaces");
    }

    #[test]
    fn parse_response_head_status_codes() {
        for (code, line) in [
            (100, "HTTP/1.1 100 Continue"),
            (201, "HTTP/1.1 201 Created"),
            (301, "HTTP/1.1 301 Moved Permanently"),
            (400, "HTTP/1.1 400 Bad Request"),
            (429, "HTTP/1.1 429 Too Many Requests"),
            (500, "HTTP/1.1 500 Internal Server Error"),
            (503, "HTTP/1.1 503 Service Unavailable"),
        ] {
            let head = format!("{line}\r\n\r\n");
            let (status, _) = parse_response_head(head.as_bytes()).unwrap();
            assert_eq!(status, code, "Failed to parse status {code}");
        }
    }

    #[test]
    fn body_kind_invalid_content_length_is_error() {
        let headers = vec![("Content-Length".to_string(), "not-a-number".to_string())];
        assert!(body_kind_from_headers(&headers).is_err());
    }

    #[test]
    fn body_kind_conflicting_content_length_headers_is_error() {
        let headers = vec![
            ("Content-Length".to_string(), "5".to_string()),
            ("content-length".to_string(), "7".to_string()),
        ];
        assert!(body_kind_from_headers(&headers).is_err());
    }

    #[test]
    fn body_kind_coalesced_identical_content_length_is_accepted() {
        let headers = vec![("Content-Length".to_string(), "5, 5".to_string())];
        assert!(matches!(
            body_kind_from_headers(&headers).unwrap(),
            BodyKind::ContentLength(5)
        ));
    }

    #[test]
    fn body_kind_coalesced_conflicting_content_length_is_error() {
        let headers = vec![("Content-Length".to_string(), "5, 7".to_string())];
        assert!(body_kind_from_headers(&headers).is_err());
    }

    #[test]
    fn build_request_bytes_empty_path() {
        let parsed = ParsedUrl::parse("http://example.com").unwrap();
        let bytes = build_request_bytes(Method::Get, &parsed, "agent", &[], &[]);
        let text = String::from_utf8(bytes).unwrap();
        // Should have "/" as path
        assert!(text.starts_with("GET /"));
    }

    #[test]
    fn build_recorded_request_content_type_case_insensitive() {
        let headers = vec![("content-type".to_string(), "APPLICATION/JSON".to_string())];
        let body = serde_json::to_vec(&json!({"test": true})).unwrap();
        let req = build_recorded_request(Method::Post, "https://test.com", &headers, &body);
        // Should detect JSON despite case differences
        assert!(req.body.is_some());
    }

    // ── CRLF header injection prevention ──────────────────────────────
    #[test]
    fn sanitize_header_value_strips_crlf() {
        assert_eq!(sanitize_header_value("normal value"), "normal value");
        assert_eq!(
            sanitize_header_value("injected\r\nEvil: header"),
            "injectedEvil: header"
        );
        assert_eq!(sanitize_header_value("bare\nnewline"), "barenewline");
        assert_eq!(sanitize_header_value("bare\rreturn"), "barereturn");
        assert_eq!(sanitize_header_value(""), "");
    }

    #[test]
    fn build_request_bytes_strips_crlf_from_headers() {
        let parsed = ParsedUrl::parse("http://example.com/test").unwrap();
        let headers = vec![(
            "X-Injected\r\nEvil".to_string(),
            "value\r\nX-Bad: smuggled".to_string(),
        )];
        let bytes = build_request_bytes(Method::Get, &parsed, "agent", &headers, &[]);
        let text = String::from_utf8(bytes).unwrap();
        // CRLF should be stripped — no injected header line
        assert!(text.contains("X-InjectedEvil: valueX-Bad: smuggled\r\n"));
        // The smuggled header must NOT appear as a separate line
        assert!(!text.contains("\r\nX-Bad: smuggled\r\n"));
    }

    #[test]
    fn build_request_bytes_strips_invalid_chars_from_header_names() {
        let parsed = ParsedUrl::parse("http://example.com/test").unwrap();
        let headers = vec![("X:Injected Header".to_string(), "value".to_string())];
        let bytes = build_request_bytes(Method::Get, &parsed, "agent", &headers, &[]);
        let text = String::from_utf8(bytes).unwrap();

        assert!(text.contains("XInjectedHeader: value\r\n"));
        assert!(!text.contains("X:Injected Header: value\r\n"));
    }

    #[test]
    fn build_request_bytes_drops_headers_that_normalize_to_reserved_names() {
        let parsed = ParsedUrl::parse("http://example.com/test").unwrap();
        let headers = vec![
            ("Host:".to_string(), "evil.example".to_string()),
            ("Content-Length ".to_string(), "999".to_string()),
            ("User-Agent:".to_string(), "spoofed".to_string()),
        ];
        let bytes = build_request_bytes(Method::Get, &parsed, "agent", &headers, &[]);
        let text = String::from_utf8(bytes).unwrap();

        assert!(text.contains("Host: example.com\r\n"));
        assert!(text.contains("User-Agent: agent\r\n"));
        assert!(text.contains("Content-Length: 0\r\n"));
        assert!(!text.contains("Host: evil.example\r\n"));
        assert!(!text.contains("Content-Length: 999\r\n"));
        assert!(!text.contains("User-Agent: spoofed\r\n"));
    }

    #[test]
    fn build_request_bytes_drops_transfer_encoding_header() {
        let parsed = ParsedUrl::parse("http://example.com/test").unwrap();
        let headers = vec![("Transfer-Encoding".to_string(), "chunked".to_string())];
        let body = b"hello";
        let bytes = build_request_bytes(Method::Post, &parsed, "agent", &headers, body);
        let text = String::from_utf8(bytes).unwrap();

        assert!(text.contains("Content-Length: 5\r\n"));
        assert!(!text.contains("Transfer-Encoding: chunked\r\n"));
    }

    // ── Response body size limit ──────────────────────────────────────
    #[test]
    fn response_text_rejects_oversized_body() {
        asupersync::test_utils::run_test(|| async {
            // Create a stream that would exceed MAX_TEXT_BODY_BYTES
            let big_chunk = vec![0u8; MAX_TEXT_BODY_BYTES + 1];
            let chunks: Vec<std::io::Result<Vec<u8>>> = vec![Ok(big_chunk)];
            let response = Response {
                status: 200,
                headers: Vec::new(),
                stream: Box::pin(futures::stream::iter(chunks)),
                timeout_info: None,
            };
            let result = response.text().await;
            assert!(result.is_err());
            let err_msg = format!("{}", result.unwrap_err());
            assert!(
                err_msg.contains("too large"),
                "error should mention size: {err_msg}"
            );
        });
    }

    #[test]
    fn response_text_accepts_body_at_limit() {
        asupersync::test_utils::run_test(|| async {
            let chunk = vec![b'a'; MAX_TEXT_BODY_BYTES];
            let chunks: Vec<std::io::Result<Vec<u8>>> = vec![Ok(chunk)];
            let response = Response {
                status: 200,
                headers: Vec::new(),
                stream: Box::pin(futures::stream::iter(chunks)),
                timeout_info: None,
            };
            let result = response.text().await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap().len(), MAX_TEXT_BODY_BYTES);
        });
    }

    #[test]
    fn response_text_limited_honors_caller_specific_limit() {
        asupersync::test_utils::run_test(|| async {
            let chunks: Vec<std::io::Result<Vec<u8>>> =
                vec![Ok(b"1234".to_vec()), Ok(b"5".to_vec())];
            let response = Response {
                status: 200,
                headers: Vec::new(),
                stream: Box::pin(futures::stream::iter(chunks)),
                timeout_info: None,
            };
            let error = response
                .text_limited(4)
                .await
                .expect_err("fifth byte must exceed the endpoint-specific limit");
            assert!(error.to_string().contains("too large"), "{error}");
        });
    }

    // ── PI_AI_ANTIGRAVITY_VERSION env var ─────────────────────────────

    #[test]
    fn antigravity_user_agent_format() {
        // Verify the format string used when PI_AI_ANTIGRAVITY_VERSION is set.
        let version = "1.2.3";
        let ua = format!("{DEFAULT_USER_AGENT} Antigravity/{version}");
        assert!(ua.starts_with("pi_agent_rust/"));
        assert!(ua.contains("Antigravity/1.2.3"));

        // Verify default user agent contains crate version.
        assert!(DEFAULT_USER_AGENT.starts_with("pi_agent_rust/"));
    }

    #[test]
    fn antigravity_user_agent_in_request_headers() {
        // Simulate the antigravity user agent being used in request building.
        let ua = format!("{DEFAULT_USER_AGENT} Antigravity/42.0");
        let parsed = ParsedUrl::parse("http://example.com/api").unwrap();
        let bytes = build_request_bytes(Method::Get, &parsed, &ua, &[], &[]);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains(&format!("User-Agent: {ua}\r\n")));
    }
}
