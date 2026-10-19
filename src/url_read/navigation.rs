//! Source-aware document retrieval. Every actual target is admitted before I/O.
//! Source rewrites are not conversion results; only downloaded bytes are returned.

use std::collections::HashSet;
use std::net::{Ipv4Addr, Ipv6Addr};

use futures::StreamExt as _;
use url::{Host, Url};

use super::{MAX_DOWNLOAD_BYTES, SsrfPolicy, UrlContentKind, UrlReadMode, UrlReadOutcome};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::http::client::{Client, effective_default_request_timeout};

const MAX_URL_BYTES: usize = 16 * 1024;
const MAX_REDIRECTS: usize = 5;

fn error(code: &str, message: &str) -> Error {
    Error::tool("read", format!("[{code}] {message}"))
}

fn checkpoint(owner: &AgentCx) -> Result<()> {
    owner.checkpoint().map_err(|_| error("URL_READ_CANCELLED", "document retrieval was cancelled"))
}

fn http_url(input: &str) -> Result<Url> {
    if input.len() > MAX_URL_BYTES || input.chars().any(char::is_control) || input.contains('\\') {
        return Err(error("URL_READ_URL", "URL exceeds 16 KiB or contains control/backslash characters"));
    }
    let mut url = Url::parse(input)
        .map_err(|_| error("URL_READ_URL", "invalid absolute HTTP(S) URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none()
        || !url.username().is_empty() || url.password().is_some()
    {
        return Err(error("URL_READ_URL", "use an HTTP(S) URL without embedded credentials"));
    }
    url.set_fragment(None);
    if url.as_str().len() > MAX_URL_BYTES {
        return Err(error("URL_READ_URL", "normalized URL exceeds 16 KiB"));
    }
    Ok(url)
}

fn private_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified()
        || ip.is_broadcast() || ip.is_documentation() || ip.is_multicast()
        || a == 0 || a >= 240
        || a == 100 && b & 0xc0 == 0x40
        || a == 198 && matches!(b, 18 | 19)
        || a == 192 && b == 0 && c == 0
}

fn private_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return private_v4(v4);
    }
    let segments = ip.segments();
    ip.is_loopback() || ip.is_unspecified() || ip.is_multicast()
        || segments[0] & 0xffc0 == 0xfe80
        || segments[0] & 0xffc0 == 0xfec0
        || segments[0] & 0xfe00 == 0xfc00
        || segments[0] == 0x2001 && segments[1] == 0x0db8
}

fn private_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => private_v4(ip),
        Some(Host::Ipv6(ip)) => private_v6(ip),
        Some(Host::Domain(host)) => {
            let host = host.trim_end_matches('.');
            host == "localhost" || host.ends_with(".localhost")
        }
        None => true,
    }
}

/// Syntactic host admission, not DNS address pinning. Hostname resolution still
/// belongs to the shared HTTP client; this does not prevent DNS rebinding.
#[must_use]
pub fn host_is_private_target(input: &str) -> bool {
    http_url(input).map_or(true, |url| private_host(&url))
}

fn admit(url: &Url, policy: SsrfPolicy) -> Result<()> {
    if policy == SsrfPolicy::BlockPrivateTargets && private_host(url) {
        return Err(error("SSRF_BLOCKED", "refusing a private/loopback/link-local target; read.urlAllowPrivateTargets=true explicitly opts out"));
    }
    Ok(())
}

/// Preserve encoded ref/path segments. UI queries and line fragments are not
/// forwarded to another host. Nondefault ports and lookalike hosts never map.
#[must_use]
pub fn github_blob_to_raw(input: &str) -> Option<String> {
    let url = http_url(input).ok()?;
    if url.host_str() != Some("github.com") || url.port().is_some() {
        return None;
    }
    let parts = url.path_segments()?.collect::<Vec<_>>();
    if parts.len() < 5 || parts[2] != "blob" || parts.iter().any(|part| part.is_empty()) {
        return None;
    }
    for component in &parts[..2] {
        if matches!(*component, "." | "..")
            || !component.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return None;
        }
    }
    let target = format!("https://raw.githubusercontent.com/{}/{}/{}/{}",
        parts[0], parts[1], parts[3], parts[4..].join("/"));
    http_url(&target).ok().map(|url| url.to_string())
}

pub(super) fn resolve_redirect(base: &str, location: &str) -> Result<String> {
    if location.len() > MAX_URL_BYTES || location.chars().any(char::is_control) || location.contains('\\') {
        return Err(error("URL_READ_REDIRECT", "invalid or oversized Location header"));
    }
    let next = http_url(base)?.join(location.trim())
        .map_err(|_| error("URL_READ_REDIRECT", "cannot resolve Location header"))?;
    http_url(next.as_str()).map(|url| url.to_string())
}

struct Route {
    current: Url,
    visited: HashSet<String>,
    redirects: usize,
    mode: UrlReadMode,
    policy: SsrfPolicy,
    github_source: bool,
}

impl Route {
    fn new(input: &str, policy: SsrfPolicy, mode: UrlReadMode) -> Result<Self> {
        let mut route = Self {
            current: http_url(input)?, visited: HashSet::new(), redirects: 0,
            mode, policy, github_source: false,
        };
        route.enter()?;
        Ok(route)
    }

    fn remember(&mut self) -> Result<()> {
        admit(&self.current, self.policy)?;
        if !self.visited.insert(self.current.as_str().to_owned()) {
            return Err(error("URL_READ_REDIRECT_LOOP", "redirect revisits an already requested document"));
        }
        Ok(())
    }

    fn enter(&mut self) -> Result<()> {
        self.remember()?;
        if self.mode == UrlReadMode::Reader
            && let Some(raw) = github_blob_to_raw(self.current.as_str())
        {
            self.current = http_url(&raw)?;
            self.github_source = true;
            self.remember()?;
        }
        Ok(())
    }

    fn follow(&mut self, location: &str) -> Result<()> {
        if self.redirects >= MAX_REDIRECTS {
            return Err(error("URL_READ_REDIRECT_LIMIT", "more than five redirects"));
        }
        self.current = http_url(&resolve_redirect(self.current.as_str(), location)?)?;
        self.redirects += 1;
        self.enter()
    }

    fn finish(&self, content_type: &str, bytes: &[u8], truncated: bool) -> Result<UrlReadOutcome> {
        if self.mode == UrlReadMode::Raw {
            return Ok(super::raw_bytes(self.current.as_str(), content_type, bytes, truncated));
        }
        // Repository HTML is source code, not a page to strip. Documents retain
        // their specialized readers; ordinary files stay text.
        if self.github_source
            && !matches!(super::classify(self.current.as_str(), content_type, bytes),
                UrlContentKind::Pdf | UrlContentKind::Notebook
                    | UrlContentKind::Docx | UrlContentKind::Pptx | UrlContentKind::Xlsx)
        {
            return Ok(UrlReadOutcome {
                content: String::from_utf8_lossy(bytes).into_owned(),
                kind: UrlContentKind::PlainText, extractor: "github-blob-raw",
                final_url: self.current.as_str().to_owned(),
                wire_content_type: content_type.to_owned(), download_truncated: truncated,
            });
        }
        super::convert_bytes(self.current.as_str(), content_type, bytes, truncated)
    }
}

/// Resolve known source URLs and redirects under the original effective HTTP
/// timeout. Explicitly disabled timeouts remain disabled. No auth/cookies are
/// synthesized, and no unsuccessful retrieval becomes a successful placeholder.
pub async fn fetch(input: &str, policy: SsrfPolicy, mode: UrlReadMode) -> Result<UrlReadOutcome> {
    let route = Route::new(input, policy, mode)?;
    let timeout = effective_default_request_timeout(route.current.as_str());
    let owner = AgentCx::for_current_or_request();
    let operation = retrieve(route, &owner);
    match timeout {
        Some(timeout) => crate::text_completion::with_timeout(timeout, operation).await
            .map_err(|stop| match stop {
                crate::text_completion::RequestStop::TimedOut => error("URL_READ_TIMEOUT", "document retrieval exceeded its whole-operation deadline"),
                crate::text_completion::RequestStop::Cancelled => error("URL_READ_CANCELLED", "document retrieval was cancelled"),
                crate::text_completion::RequestStop::TimeUnavailable => error("URL_READ_DENIED", "document retrieval requires time authority"),
            })?,
        None => owner.with_current(operation).await,
    }
}

async fn retrieve(mut route: Route, owner: &AgentCx) -> Result<UrlReadOutcome> {
    let client = Client::new();
    loop {
        checkpoint(owner)?;
        let response = client.get(route.current.as_str())
            .header("User-Agent", "pi_agent_rust/0.2 (url read)")
            .header("Accept", "text/html,application/pdf,text/plain,application/json,*/*")
            .send().await
            .map_err(|_| error("URL_READ_FETCH", "HTTP request failed"))?;
        checkpoint(owner)?;
        let status = response.status();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let mut locations = response.headers().iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("location"));
            let location = locations.next()
                .ok_or_else(|| error("URL_READ_REDIRECT", "redirect response has no Location header"))?;
            if locations.next().is_some() {
                return Err(error("URL_READ_REDIRECT", "ambiguous Location headers"));
            }
            route.follow(&location.1)?;
            // Drop this body/connection before making the next request.
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(Error::tool("read", format!("[URL_READ_HTTP] HTTP {status} fetching document")));
        }
        let content_type = response.headers().iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.clone()).unwrap_or_default();
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        let mut truncated = false;
        let mut chunks = 0usize;
        while let Some(chunk) = stream.next().await {
            checkpoint(owner)?;
            let chunk = chunk.map_err(|_| error("URL_READ_BODY", "response body did not complete"))?;
            let remaining = MAX_DOWNLOAD_BYTES.saturating_sub(bytes.len());
            if chunk.len() > remaining {
                bytes.extend_from_slice(&chunk[..remaining]);
                truncated = true;
                break;
            }
            bytes.extend_from_slice(&chunk);
            chunks += 1;
            if chunks.is_multiple_of(64) {
                // Ready chunk streams must yield to the enclosing owner timer.
                let mut yielded = false;
                std::future::poll_fn(|cx| {
                    if yielded { std::task::Poll::Ready(()) } else {
                        yielded = true;
                        cx.waker().wake_by_ref();
                        std::task::Poll::Pending
                    }
                }).await;
            }
        }
        drop(stream);
        checkpoint(owner)?;
        // Native document conversion can parse and decompress multiple parts.
        // Keep that bounded CPU work off the async executor so the enclosing
        // owner deadline and cancellation remain observable while it runs.
        return asupersync::runtime::spawn_blocking_io(move || {
            route.finish(&content_type, &bytes, truncated)
        })
        .await;
    }
}

#[cfg(test)]
mod tests;
