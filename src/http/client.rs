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
            timeout: None,
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
            let crate::vcr::RecordedResponse {
                status,
                headers: response_headers,
                body_chunks,
            } = recorded;
            let stream =
                stream::iter(body_chunks.into_iter().map(|chunk| Ok(chunk.into_bytes()))).boxed();
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

            let sleep_fut = sleep(wall_now(), duration).fuse();
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
        let _ = std::fmt::Write::write_fmt(&mut out, format_args!("{name}: {value}\r\n"));
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
