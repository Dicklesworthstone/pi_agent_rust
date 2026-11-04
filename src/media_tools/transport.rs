//! Shared transport for native media adapters. No retries, redirects, or mock fallback.
//! An explicit owner covers connect, response-body consumption, and the request deadline.

use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::http::client::Client;
use futures::future::{Either, select};
use serde_json::Value;
use std::io::Read;
use std::path::Path;
use std::time::Duration;
use url::Url;

#[derive(Default)]
pub(super) struct Transport {
    client: Client,
    base_url: Option<String>,
    auth: super::MediaAuthScope,
}

impl Transport {
    pub(super) fn bind_auth_scope(&mut self, scope: super::MediaAuthScope) {
        self.auth = scope;
    }

    pub(super) fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    pub(super) fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = Some(base_url);
        self
    }

    pub(super) fn api(
        &self,
        tool: &'static str,
        provider: &'static str,
        explicit_key: Option<&str>,
        timeout: Duration,
    ) -> Result<Api<'_>> {
        let owner = AgentCx::for_current_or_request();
        check_owner(tool, &owner)?;
        let (default_url, env_name) = match provider {
            "openai" => ("https://api.openai.com/v1/", "PI_MEDIA_OPENAI_BASE_URL"),
            "anthropic" => (
                "https://api.anthropic.com/v1/",
                "PI_MEDIA_ANTHROPIC_BASE_URL",
            ),
            "gemini" => (
                "https://generativelanguage.googleapis.com/v1beta/",
                "PI_MEDIA_GEMINI_BASE_URL",
            ),
            "xai" => ("https://api.x.ai/v1/", "PI_MEDIA_XAI_BASE_URL"),
            _ => return Err(Error::tool(tool, "unsupported media provider")),
        };
        let env_url = std::env::var(env_name).ok();
        let base = endpoint(
            tool,
            self.base_url
                .as_deref()
                .or(env_url.as_deref())
                .unwrap_or(default_url),
        )?;
        let auth = if explicit_key.is_some() {
            None
        } else {
            self.auth
                .snapshot()
                .map_err(|message| Error::tool(tool, message))?
        };
        let track_rate_limits = explicit_key.is_none() && auth.is_some();
        let key = if explicit_key.is_some() {
            // Even an empty explicit key is authoritative: never replace a
            // caller's deliberate denial with a saved or ambient credential.
            resolve_key(tool, provider, explicit_key, |name| {
                std::env::var(name).ok()
            })?
        } else if let Some(auth) = &auth {
            auth.resolve_media_api_key_status(provider)
                .into_result(provider)
                .map_err(|error| Error::tool(tool, error.to_string()))?
                .ok_or_else(|| missing_key(tool, provider))?
        } else {
            resolve_key(tool, provider, None, |name| std::env::var(name).ok())?
        };
        // Stored command-backed keys can take time to resolve. Recheck the
        // invocation before constructing a request after that bounded work.
        check_owner(tool, &owner)?;
        Ok(Api {
            transport: self,
            tool,
            provider,
            key,
            track_rate_limits,
            base,
            owner,
            timeout,
        })
    }
}

pub(super) struct Api<'a> {
    transport: &'a Transport,
    tool: &'static str,
    provider: &'static str,
    key: String,
    track_rate_limits: bool,
    base: Url,
    pub(super) owner: AgentCx,
    timeout: Duration,
}

pub(super) struct Response {
    pub(super) bytes: Vec<u8>,
    pub(super) content_type: String,
}

impl Response {
    pub(super) fn json(self, tool: &str) -> Result<Value> {
        if !self.content_type.is_empty()
            && self.content_type != "application/json"
            && !self.content_type.ends_with("+json")
        {
            return Err(Error::tool(tool, "provider returned a non-JSON response"));
        }
        serde_json::from_slice(&self.bytes)
            .map_err(|_| Error::tool(tool, "provider returned invalid JSON"))
    }
}

impl Api<'_> {
    pub(super) fn scrub(&self, message: &str, limit: usize) -> String {
        crate::auth::redact_known_secrets_bounded(message, &[self.key.as_str()], limit)
            .chars()
            .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
            .collect()
    }

    // Keep the bounded HTTP exchange inside its shared cancellation/deadline race.
    #[allow(clippy::too_many_lines)]
    pub(super) async fn post(&self, path: &str, payload: &Value, limit: usize) -> Result<Response> {
        // Paths are built by adapters, never accepted as model-facing arguments.
        let url = self
            .base
            .join(path)
            .map_err(|_| Error::tool(self.tool, "invalid media endpoint path"))?;
        if url.origin() != self.base.origin() || !url.path().starts_with(self.base.path()) {
            return Err(Error::tool(
                self.tool,
                "media request escaped its configured API base",
            ));
        }
        let operation = async {
            check_owner(self.tool, &self.owner)?;
            let started_at = std::time::Instant::now();
            let client = self.owner.http().bind(&self.transport.client);
            let request = client
                .post(url.as_str())
                .timeout(self.timeout)
                .json(payload)?;
            let request = match self.provider {
                "gemini" => request.try_header("x-goog-api-key", &self.key),
                "anthropic" => request
                    .try_header("x-api-key", &self.key)
                    .and_then(|request| request.try_header("anthropic-version", "2023-06-01")),
                _ => request.try_header("Authorization", format!("Bearer {}", self.key)),
            }
            .map_err(|_| Error::tool(self.tool, "invalid media authentication header"))?;
            let response = request.send().await.map_err(|error| {
                Error::tool(
                    self.tool,
                    format!(
                        "{} request failed: {}",
                        self.provider,
                        self.scrub(&error.to_string(), 2048)
                    ),
                )
            })?;
            let status = response.status();
            if !(200..300).contains(&status) {
                if status == 429 && self.track_rate_limits {
                    // Record backoff for the next invocation; never replay a
                    // potentially billable generation or synthesis request.
                    crate::auth::report_provider_rate_limit(self.provider, &self.key);
                }
                let detail = response.text_limited(16 * 1024).await.map_or_else(
                    |_| "error body unavailable or too large".to_string(),
                    |body| self.scrub(&body, 2048),
                );
                return Err(Error::tool(
                    self.tool,
                    format!("{} HTTP {status}: {detail}", self.provider),
                ));
            }
            let content_type = response
                .headers()
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                .map(|(_, value)| {
                    value
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                })
                .unwrap_or_default();
            let bytes = response.bytes_limited(limit).await.map_err(|error| {
                Error::tool(
                    self.tool,
                    format!(
                        "{} response failed: {}",
                        self.provider,
                        self.scrub(&error.to_string(), 2048)
                    ),
                )
            })?;
            check_owner(self.tool, &self.owner)?;
            if bytes.is_empty() {
                return Err(Error::tool(
                    self.tool,
                    "provider returned an empty response",
                ));
            }
            if self.track_rate_limits {
                crate::auth::report_provider_success(self.provider, &self.key, started_at);
            }
            Ok(Response {
                bytes,
                content_type,
            })
        };
        let cancelled = async {
            let (sender, mut receiver) = asupersync::channel::oneshot::channel::<()>();
            let _ = receiver.recv(self.owner.cx()).await;
            drop(sender);
        };
        let watchdog = async {
            match select(
                Box::pin(self.owner.time().sleep(self.timeout)),
                Box::pin(cancelled),
            )
            .await
            {
                Either::Left(_) => {
                    "media request timed out; the provider may already have processed it"
                }
                Either::Right(_) => {
                    "media request cancelled; the provider may already have processed it"
                }
            }
        };
        match select(Box::pin(operation), Box::pin(watchdog)).await {
            Either::Left((result, _)) => result,
            Either::Right((message, pending)) => {
                drop(pending);
                Err(Error::tool(self.tool, message))
            }
        }
    }
}

pub(super) fn check_owner(tool: &str, owner: &AgentCx) -> Result<()> {
    if !owner.capabilities().io || !owner.capabilities().time {
        return Err(Error::tool(
            tool,
            "media operations require I/O and timer capabilities",
        ));
    }
    owner
        .checkpoint()
        .map_err(|_| Error::tool(tool, "media operation cancelled"))
}

fn endpoint(tool: &str, value: &str) -> Result<Url> {
    let mut url =
        Url::parse(value.trim()).map_err(|_| Error::tool(tool, "invalid media API base URL"))?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    if url.host().is_none()
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::tool(
            tool,
            "media API base must use HTTPS (HTTP only on loopback), without credentials, query or fragment",
        ));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

pub(super) fn provider(tool: &str, value: &str) -> Result<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "openai" => Ok("openai"),
        "anthropic" => Ok("anthropic"),
        "gemini" | "google" => Ok("gemini"),
        "xai" | "x-ai" => Ok("xai"),
        _ => Err(Error::tool(
            tool,
            "unsupported media provider (use openai, anthropic, gemini or xai)",
        )),
    }
}

fn resolve_key(
    tool: &str,
    provider: &str,
    explicit: Option<&str>,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> Result<String> {
    let vars = key_environment_variables(provider);
    // An explicit empty key is a deliberate denial, not permission to fall back.
    let key = explicit.map_or_else(
        || {
            vars.iter()
                .find_map(|name| lookup(name).filter(|key| !key.trim().is_empty()))
        },
        |key| Some(key.to_string()),
    );
    key.map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
        .ok_or_else(|| missing_key(tool, provider))
}

fn key_environment_variables(provider: &str) -> &'static [&'static str] {
    match provider {
        "openai" => &["OPENAI_API_KEY"],
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "gemini" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "xai" => &["XAI_API_KEY"],
        _ => &[],
    }
}

fn missing_key(tool: &str, provider: &str) -> Error {
    let purpose = match tool {
        "inspect_image" => "vision provider",
        "generate_image" => "image generation provider",
        _ => "TTS synthesis provider",
    };
    Error::tool(
        tool,
        format!(
            "missing API key for {purpose} {provider} (save a provider API key in the session's auth.json or set {}; media tools do not use OAuth or subscription credentials)",
            key_environment_variables(provider).join(" or ")
        ),
    )
}

pub(super) fn optional<'a>(args: &'a Value, tool: &str, field: &str) -> Result<Option<&'a str>> {
    args.get(field).map_or(Ok(None), |value| {
        value
            .as_str()
            .map(Some)
            .ok_or_else(|| Error::tool(tool, format!("{field} must be a string")))
    })
}

pub(super) fn required<'a>(args: &'a Value, tool: &str, field: &str) -> Result<&'a str> {
    let value = optional(args, tool, field)?
        .ok_or_else(|| Error::tool(tool, format!("missing required {field} parameter")))?;
    if value.trim().is_empty() || value.len() > 128 * 1024 {
        return Err(Error::tool(
            tool,
            format!("{field} must be nonempty and at most 128 KiB"),
        ));
    }
    Ok(value)
}

pub(super) fn model_id<'a>(tool: &str, value: &'a str) -> Result<&'a str> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || b"-._/".contains(&ch))
    {
        return Err(Error::tool(
            tool,
            "model must be a nonempty identifier of at most 256 ASCII bytes",
        ));
    }
    Ok(value)
}

pub(super) fn gemini_path(tool: &str, model: &str) -> Result<String> {
    let model = model.strip_prefix("models/").unwrap_or(model);
    model_id(tool, model)?;
    if model.contains('/') || matches!(model, "." | "..") {
        return Err(Error::tool(
            tool,
            "Gemini model must be a model ID, not a path",
        ));
    }
    Ok(format!("models/{model}:generateContent"))
}

pub(super) fn timeout(args: &Value, tool: &str, default_ms: u64) -> Result<Duration> {
    let ms = match args.get("timeout_ms") {
        None => default_ms,
        Some(value) => value
            .as_u64()
            .filter(|ms| (1..=300_000).contains(ms))
            .ok_or_else(|| Error::tool(tool, "timeout_ms must be an integer in 1..=300000"))?,
    };
    Ok(Duration::from_millis(ms))
}

pub(super) fn read_capped(path: &Path, tool: &str, limit: u64) -> Result<Vec<u8>> {
    let initial = std::fs::metadata(path)
        .map_err(|error| Error::tool(tool, format!("cannot stat media file: {error}")))?;
    if !initial.is_file() || initial.len() > limit {
        return Err(Error::tool(
            tool,
            format!("media input must be a regular file of at most {limit} bytes"),
        ));
    }
    // A regular file can be replaced between metadata and open. NONBLOCK keeps
    // a concurrent FIFO swap from hanging before the descriptor can be checked.
    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    let file = {
        use rustix::fs::{Mode, OFlags};
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|error| Error::tool(tool, format!("cannot read media file: {error}")))?;
        std::fs::File::from(fd)
    };
    #[cfg(not(all(unix, not(any(target_os = "espidf", target_os = "redox")))))]
    let file = std::fs::File::open(path)
        .map_err(|error| Error::tool(tool, format!("cannot read media file: {error}")))?;
    let metadata = file
        .metadata()
        .map_err(|error| Error::tool(tool, format!("cannot stat media file: {error}")))?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(Error::tool(
            tool,
            format!("media input must be a regular file of at most {limit} bytes"),
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| Error::tool(tool, format!("cannot read media file: {error}")))?;
    if bytes.is_empty() || bytes.len() as u64 > limit {
        return Err(Error::tool(
            tool,
            "media input is empty or grew beyond its byte limit",
        ));
    }
    Ok(bytes)
}

/// Basic container checks, not a full image decoder or decompression-bomb guard.
pub(super) fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 45
        && bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        && bytes.get(12..16) == Some(b"IHDR".as_slice())
        && bytes.ends_with(b"\0\0\0\0IEND\xaeB`\x82")
    {
        Some("image/png")
    } else if bytes.len() > 4 && bytes.starts_with(b"\xff\xd8\xff") && bytes.ends_with(b"\xff\xd9")
    {
        Some("image/jpeg")
    } else if bytes.len() >= 20
        && bytes.starts_with(b"RIFF")
        && bytes.get(8..12) == Some(b"WEBP".as_slice())
    {
        Some("image/webp")
    } else if bytes.len() >= 14
        && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"))
        && bytes.last() == Some(&b';')
    {
        Some("image/gif")
    } else {
        None
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};
    use std::time::Instant;

    pub struct Captured {
        pub headers: String,
        pub body: Value,
    }

    // A real TCP peer with a canned protocol response, not a replacement client.
    // Both accept and I/O are bounded so a failed assertion cannot hang the suite.
    pub fn peer(status: u16, content_type: &str, body: Vec<u8>) -> (String, JoinHandle<Captured>) {
        let (endpoint, listener) = peer_listener();
        let content_type = content_type.to_string();
        let worker = thread::spawn(move || exchange(&listener, status, &content_type, &body));
        (endpoint, worker)
    }

    pub fn peer_responses(
        responses: Vec<(u16, &str, Vec<u8>)>,
    ) -> (String, JoinHandle<Vec<Captured>>) {
        let (endpoint, listener) = peer_listener();
        let responses = responses
            .into_iter()
            .map(|(status, content_type, body)| (status, content_type.to_string(), body))
            .collect::<Vec<_>>();
        let worker = thread::spawn(move || {
            responses
                .into_iter()
                .map(|(status, content_type, body)| {
                    exchange(&listener, status, &content_type, &body)
                })
                .collect()
        });
        (endpoint, worker)
    }

    fn peer_listener() -> (String, TcpListener) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let endpoint = format!("http://{}/v1/", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        (endpoint, listener)
    }

    fn exchange(listener: &TcpListener, status: u16, content_type: &str, body: &[u8]) -> Captured {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "adapter never connected");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept: {error}"),
            }
        };
        // 250ms is the POLLING interval; `deadline` is the budget for the
        // whole exchange. A read timeout means "nothing yet", not "fail":
        // macOS reports one as EAGAIN/WouldBlock, so panicking on it fails
        // the test whenever the client has merely not been scheduled, which
        // is common under a parallel suite (bd-eg6ng).
        socket
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let read_patiently = |socket: &mut std::net::TcpStream, chunk: &mut [u8]| loop {
            match socket.read(chunk) {
                Ok(n) => return n,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "fixture timed out waiting for the request"
                    );
                }
                // ubs:ignore an unexpected socket error in a fixture is an assertion failure
                Err(error) => panic!("fixture read failed: {error}"),
            }
        };
        let mut request = Vec::new();
        let header_end = loop {
            let mut chunk = [0; 4096];
            let n = read_patiently(&mut socket, &mut chunk);
            assert!(n > 0, "request ended before headers");
            request.extend_from_slice(&chunk[..n]);
            assert!(request.len() < 2 * 1024 * 1024);
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
        let length: usize = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .expect("content-length");
        assert!(length < 2 * 1024 * 1024);
        while request.len() - header_end < length {
            let mut chunk = [0; 4096];
            let n = read_patiently(&mut socket, &mut chunk);
            assert!(n > 0, "request ended before body");
            request.extend_from_slice(&chunk[..n]);
        }
        let captured = Captured {
            headers,
            body: serde_json::from_slice(&request[header_end..header_end + length]).unwrap(),
        };
        write!(socket, "HTTP/1.1 {status} Response\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        socket.write_all(body).unwrap();
        captured
    }

    #[test]
    fn credentials_are_provider_scoped_and_explicit_empty_never_falls_back() {
        assert!(
            resolve_key("inspect_image", "openai", Some(" "), |_| Some(
                "ambient".into()
            ))
            .is_err()
        );
        assert_eq!(
            resolve_key("inspect_image", "gemini", None, |name| {
                (name == "GOOGLE_API_KEY").then(|| " google-token ".into())
            })
            .unwrap(),
            "google-token"
        );
        assert!(
            resolve_key("inspect_image", "anthropic", None, |name| {
                (name == "OPENAI_API_KEY").then(|| "wrong-provider".into())
            })
            .is_err()
        );
    }

    #[test]
    fn endpoints_and_model_paths_cannot_redirect_credentials() {
        for bad in [
            "http://example.com/v1",
            "https://user:secret@example.com/",
            "https://example.com/?key=x",
            "file:///tmp/api",
            "https://example.com/#x",
        ] {
            assert!(endpoint("inspect_image", bad).is_err(), "{bad}");
        }
        assert_eq!(
            endpoint("inspect_image", "http://127.0.0.1:1234/v1")
                .unwrap()
                .path(),
            "/v1/"
        );
        for bad in [
            "../../attack",
            "models/../attack",
            "x?key=secret",
            "https://evil",
        ] {
            assert!(gemini_path("inspect_image", bad).is_err());
        }
        assert_eq!(
            gemini_path("inspect_image", "models/gemini-2.5-flash").unwrap(),
            "models/gemini-2.5-flash:generateContent"
        );
    }

    #[test]
    fn bounded_file_reader_rejects_empty_and_oversized_inputs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_capped(dir.path(), "inspect_image", 4).is_err());
        let path = dir.path().join("image.png");
        std::fs::write(&path, []).unwrap();
        assert!(read_capped(&path, "inspect_image", 4).is_err());
        std::fs::write(&path, b"12345").unwrap();
        assert!(read_capped(&path, "inspect_image", 4).is_err());
        assert_eq!(read_capped(&path, "inspect_image", 5).unwrap(), b"12345");
    }
}
