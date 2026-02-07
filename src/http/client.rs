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
use asupersync::tls::{TlsConnector, TlsConnectorBuilder};
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream::{self, BoxStream};
use std::pin::Pin;
use std::task::{Context, Poll};

const DEFAULT_USER_AGENT: &str = "pi_agent_rust/0.1";
const MAX_HEADER_BYTES: usize = 64 * 1024;
const READ_CHUNK_BYTES: usize = 16 * 1024;
const MAX_BUFFERED_BYTES: usize = 256 * 1024;
const MAX_TEXT_BODY_BYTES: usize = 50 * 1024 * 1024;
const DEFAULT_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Client {
    tls: std::result::Result<TlsConnector, String>,
    user_agent: String,
    vcr: Option<VcrRecorder>,
}

impl Client {
    #[must_use]
    pub fn new() -> Self {
        let tls = TlsConnectorBuilder::new()
            .with_native_roots()
            .and_then(|builder| builder.alpn_protocols(vec![b"http/1.1".to_vec()]).build())
            .map_err(|e| e.to_string());

        Self {
            tls,
            user_agent: DEFAULT_USER_AGENT.to_string(),
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
    timeout: Option<std::time::Duration>,
}

impl<'a> RequestBuilder<'a> {
    fn new(client: &'a Client, method: Method, url: &str) -> Self {
        Self {
            client,
            method,
            url: url.to_string(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: Some(DEFAULT_REQUEST_TIMEOUT),
        }
    }

    #[must_use]
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }

    #[must_use]
    pub const fn timeout(mut self, duration: std::time::Duration) -> Self {
        self.timeout = Some(duration);
        self
    }

    /// Remove the timeout entirely. Use for requests that are expected to take
    /// an arbitrarily long time (e.g. long-polling SSE streams).
    #[must_use]
    pub const fn no_timeout(mut self) -> Self {
        self.timeout = None;
        self
    }

    /// Set raw body bytes.
    #[must_use]
    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    pub fn json<T: serde::Serialize>(mut self, payload: &T) -> Result<Self> {
        self.headers
            .push(("Content-Type".to_string(), "application/json".to_string()));
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
            });
        }

        let send_fut = send_parts(client, method, &url, &headers, &body);

        let (status, response_headers, stream) = if let Some(duration) = timeout {
            use asupersync::time::{sleep, wall_now};
            use futures::future::{Either, FutureExt, select};

            let now = asupersync::Cx::current()
                .and_then(|cx| cx.timer_driver())
                .map_or_else(wall_now, |timer| timer.now());
            let sleep_fut = sleep(now, duration).fuse();
            let send_fut = send_fut.fuse();
            futures::pin_mut!(sleep_fut, send_fut);

            match select(send_fut, sleep_fut).await {
                Either::Left((res, _)) => res?,
                Either::Right(_) => return Err(Error::api("Request timed out")),
            }
        } else {
            send_fut.await?
        };

        Ok(Response {
            status,
            headers: response_headers,
            stream,
        })
    }
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
    transport.write_all(&request_bytes).await?;
    if !body.is_empty() {
        transport.write_all(body).await?;
    }
    transport.flush().await?;

    let (status, response_headers, leftover) = Box::pin(read_response_head(&mut transport)).await?;
    let body_kind = body_kind_from_headers(&response_headers);

    let state = BodyStreamState::new(transport, body_kind, leftover);
    let stream = stream::try_unfold(state, |mut state| async move {
        let chunk = Box::pin(state.next_bytes()).await?;
        Ok(chunk.map(|chunk| (chunk, state)))
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
        self.stream
    }

    pub async fn text(self) -> Result<String> {
        let bytes = self
            .stream
            .try_fold(Vec::new(), |mut acc, chunk| async move {
                if acc.len().saturating_add(chunk.len()) > MAX_TEXT_BODY_BYTES {
                    return Err(std::io::Error::other("response body too large"));
                }
                acc.extend_from_slice(&chunk);
                Ok::<_, std::io::Error>(acc)
            })
            .await
            .map_err(Error::from)?;

        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

async fn connect_transport(parsed: &ParsedUrl, client: &Client) -> Result<Transport> {
    let addr = (parsed.host.clone(), parsed.port);
    let tcp = TcpStream::connect(addr).await?;
    match parsed.scheme {
        Scheme::Http => Ok(Transport::Tcp(tcp)),
        Scheme::Https => {
            let tls = client
                .tls
                .as_ref()
                .map_err(|e| Error::api(format!("TLS configuration error: {e}")))?;
            let tls_stream = tls
                .clone()
                .connect(&parsed.host, tcp)
                .await
                .map_err(|e| Error::api(format!("TLS connect failed: {e}")))?;
            Ok(Transport::Tls(Box::new(tls_stream)))
        }
    }
}

/// Strip CR/LF from header names and values to prevent HTTP header injection.
fn sanitize_header_value(value: &str) -> String {
    value.chars().filter(|&c| c != '\r' && c != '\n').collect()
}

fn build_request_bytes(
    method: Method,
    parsed: &ParsedUrl,
    user_agent: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Vec<u8> {
    let mut out = String::new();
    let _ = std::fmt::Write::write_fmt(
        &mut out,
        format_args!("{} {} HTTP/1.1\r\n", method.as_str(), parsed.path),
    );
    let _ = std::fmt::Write::write_fmt(&mut out, format_args!("Host: {}\r\n", parsed.host));
    let _ = std::fmt::Write::write_fmt(&mut out, format_args!("User-Agent: {user_agent}\r\n"));
    let _ =
        std::fmt::Write::write_fmt(&mut out, format_args!("Content-Length: {}\r\n", body.len()));

    for (name, value) in headers {
        let clean_name = sanitize_header_value(name);
        let clean_value = sanitize_header_value(value);
        let _ =
            std::fmt::Write::write_fmt(&mut out, format_args!("{clean_name}: {clean_value}\r\n"));
    }

    out.push_str("\r\n");
    out.into_bytes()
}

async fn read_response_head(
    transport: &mut Transport,
) -> Result<(u16, Vec<(String, String)>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(8192);
    let mut scratch = [0u8; READ_CHUNK_BYTES];

    loop {
        if buf.len() > MAX_HEADER_BYTES {
            return Err(Error::api("HTTP response headers too large"));
        }

        if let Some(pos) = find_headers_end(&buf) {
            let head = &buf[..pos];
            let leftover = buf[pos..].to_vec();
            let (status, headers) = parse_response_head(head)?;
            return Ok((status, headers, leftover));
        }

        let n = read_some(transport, &mut scratch).await?;
        if n == 0 {
            return Err(Error::api("HTTP connection closed before headers"));
        }
        buf.extend_from_slice(&scratch[..n]);
    }
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn parse_response_head(head: &[u8]) -> Result<(u16, Vec<(String, String)>)> {
    let text =
        std::str::from_utf8(head).map_err(|e| Error::api(format!("Invalid HTTP headers: {e}")))?;
    let mut lines = text.split("\r\n");

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

fn body_kind_from_headers(headers: &[(String, String)]) -> BodyKind {
    let mut content_length = None;
    let mut transfer_encoding = None;

    for (name, value) in headers {
        let name_lc = name.to_ascii_lowercase();
        if name_lc == "content-length" {
            content_length = value.trim().parse::<usize>().ok();
        } else if name_lc == "transfer-encoding" {
            transfer_encoding = Some(value.to_ascii_lowercase());
        }
    }

    if let Some(te) = transfer_encoding {
        if te.split(',').any(|v| v.trim() == "chunked") {
            return BodyKind::Chunked;
        }
    }

    match content_length {
        Some(0) => BodyKind::Empty,
        Some(n) => BodyKind::ContentLength(n),
        None => BodyKind::Eof,
    }
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
        self.pos = self.pos.saturating_add(n);
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

    async fn read_more(&mut self) -> std::io::Result<usize> {
        let mut scratch = [0u8; READ_CHUNK_BYTES];
        let n = read_some(&mut self.transport, &mut scratch).await?;
        if n > 0 {
            if let Err(err) = self.buf.extend(&scratch[..n]) {
                return Err(std::io::Error::other(err.to_string()));
            }
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

    async fn next_chunked(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            match self.chunked_state {
                ChunkedState::SizeLine => {
                    if let Some(line_end) = find_crlf(self.buf.available()) {
                        let line = &self.buf.available()[..line_end];
                        let line_str = std::str::from_utf8(line).map_err(std::io::Error::other)?;
                        let size_part = line_str.split(';').next().unwrap_or("").trim();
                        if size_part.is_empty() {
                            return Err(std::io::Error::other("invalid chunk size"));
                        }
                        let chunk_size = usize::from_str_radix(size_part, 16)
                            .map_err(|_| std::io::Error::other("invalid chunk size"))?;
                        self.buf.consume(line_end + 2);
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
                        if n == 0 {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "unexpected EOF reading chunk CRLF",
                            ));
                        }
                        continue;
                    }

                    let bytes = self.buf.available();
                    if bytes[0] != b'\r' || bytes[1] != b'\n' {
                        return Err(std::io::Error::other("invalid chunk CRLF"));
                    }
                    self.buf.consume(2);
                    self.chunked_state = ChunkedState::SizeLine;
                }

                ChunkedState::Trailers => {
                    // Trailers are terminated by an empty line. When there are no trailers,
                    // the terminator is a single CRLF (`0\r\n\r\n` total, with the final
                    // `\r\n` remaining after consuming the size line).
                    let bytes = self.buf.available();
                    if bytes.len() >= 2 && bytes[0] == b'\r' && bytes[1] == b'\n' {
                        self.buf.consume(2);
                        self.chunked_state = ChunkedState::Done;
                        return Ok(None);
                    }
                    if let Some(end) = find_double_crlf(self.buf.available()) {
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

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
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

    // ── Method ──────────────────────────────────────────────────────────
    #[test]
    fn method_as_str_get() {
        assert_eq!(Method::Get.as_str(), "GET");
    }

    #[test]
    fn method_as_str_post() {
        assert_eq!(Method::Post.as_str(), "POST");
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
        assert_eq!(find_crlf(b"abc\r\ndef"), Some(3));
    }

    #[test]
    fn find_crlf_absent() {
        assert!(find_crlf(b"abcdef").is_none());
    }

    #[test]
    fn find_crlf_at_start() {
        assert_eq!(find_crlf(b"\r\ndata"), Some(0));
    }

    // ── find_double_crlf ───────────────────────────────────────────────
    #[test]
    fn find_double_crlf_present() {
        let buf = b"headers\r\n\r\nbody";
        assert_eq!(find_double_crlf(buf), Some(11));
    }

    #[test]
    fn find_double_crlf_absent() {
        assert!(find_double_crlf(b"headers\r\nbody").is_none());
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
            body_kind_from_headers(&headers),
            BodyKind::ContentLength(42)
        ));
    }

    #[test]
    fn body_kind_content_length_zero() {
        let headers = vec![("Content-Length".to_string(), "0".to_string())];
        assert!(matches!(body_kind_from_headers(&headers), BodyKind::Empty));
    }

    #[test]
    fn body_kind_chunked() {
        let headers = vec![("Transfer-Encoding".to_string(), "chunked".to_string())];
        assert!(matches!(
            body_kind_from_headers(&headers),
            BodyKind::Chunked
        ));
    }

    #[test]
    fn body_kind_chunked_mixed() {
        // Transfer-Encoding with multiple values
        let headers = vec![("Transfer-Encoding".to_string(), "gzip, chunked".to_string())];
        assert!(matches!(
            body_kind_from_headers(&headers),
            BodyKind::Chunked
        ));
    }

    #[test]
    fn body_kind_chunked_overrides_content_length() {
        // When both present, chunked wins
        let headers = vec![
            ("Content-Length".to_string(), "100".to_string()),
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
        ];
        assert!(matches!(
            body_kind_from_headers(&headers),
            BodyKind::Chunked
        ));
    }

    #[test]
    fn body_kind_eof_no_headers() {
        let headers: Vec<(String, String)> = Vec::new();
        assert!(matches!(body_kind_from_headers(&headers), BodyKind::Eof));
    }

    #[test]
    fn body_kind_case_insensitive() {
        let headers = vec![("content-length".to_string(), "10".to_string())];
        assert!(matches!(
            body_kind_from_headers(&headers),
            BodyKind::ContentLength(10)
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
        assert_eq!(builder.timeout, Some(DEFAULT_REQUEST_TIMEOUT));
    }

    #[test]
    fn request_builder_timeout() {
        let client = Client::new();
        let builder = client
            .get("https://api.example.com")
            .timeout(std::time::Duration::from_secs(30));
        assert_eq!(builder.timeout, Some(std::time::Duration::from_secs(30)));
    }

    #[test]
    fn request_builder_no_timeout() {
        let client = Client::new();
        let builder = client.get("https://api.example.com").no_timeout();
        assert_eq!(builder.timeout, None);
    }

    // ── Response ───────────────────────────────────────────────────────
    #[test]
    fn response_accessors() {
        let response = Response {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
            stream: Box::pin(futures::stream::empty()),
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
    fn body_kind_invalid_content_length_falls_to_eof() {
        let headers = vec![("Content-Length".to_string(), "not-a-number".to_string())];
        // parse fails, content_length stays None → Eof
        assert!(matches!(body_kind_from_headers(&headers), BodyKind::Eof));
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
            };
            let result = response.text().await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap().len(), MAX_TEXT_BODY_BYTES);
        });
    }
}
