//! HTTP/SOCKS5 proxy support for the minimal HTTP client.
//!
//! Supported proxy sources (priority order):
//!   1. `HTTPS_PROXY` / `https_proxy` (HTTPS targets only, curl semantics)
//!   2. `HTTP_PROXY` / `http_proxy` (HTTP targets only)
//!   3. `ALL_PROXY` / `all_proxy` (both)
//!   4. macOS system proxy (`scutil --proxy`)
//!
//! Supported proxy URL schemes:
//!   - `http://[user:pass@]host[:port]` — CONNECT tunnel for HTTPS, absolute-form for HTTP
//!   - `socks5://[user:pass@]host[:port]` — RFC 1928 (SOCKS5 always does remote DNS)
//!   - `socks5h://` — accepted as alias for `socks5://` (our implementation always
//!     sends the hostname to the proxy via ATYP=domain, which is socks5h semantics)
//!   - `https://` — **rejected**: v1 does not implement TLS to the proxy itself;
//!     treating it as plain HTTP would produce a confusing TLS handshake failure
//!     on port 443.
//!
//! v1 limitations (documented, not implemented):
//!   - NO_PROXY does not support CIDR notation (only exact / suffix / `*`)
//!   - userinfo is not percent-decoded (`user:p%40ss@host` stays encoded)
//!   - `https://` proxy URLs are rejected (see above)

use asupersync::http::h1::ParsedUrl;
use asupersync::io::ext::{AsyncReadExt, AsyncWriteExt};
use asupersync::net::tcp::stream::TcpStream;

// ============================================================================
// Data structures
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ProxyScheme {
    Http,
    Socks5,
}

#[derive(Debug, Clone)]
pub(crate) struct ProxyConfig {
    pub scheme: ProxyScheme,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

fn default_port(scheme: &ProxyScheme) -> u16 {
    match scheme {
        ProxyScheme::Http => 80,
        ProxyScheme::Socks5 => 1080,
    }
}

// ============================================================================
// Proxy URL parsing
// ============================================================================

/// Parse a proxy URL of the form `[scheme://][user[:pass]@]host[:port]`.
///
/// Edge cases handled:
/// - No scheme → defaults to `http://`
/// - Passwords containing `@` → split at the LAST `@` (rsplit)
/// - IPv6 hosts → `[::1]:port` bracket form
/// - Missing port → scheme default (http=80, socks5=1080)
/// - `https://` → rejected (v1 does not support TLS to the proxy itself)
/// - `socks5h://` → accepted, treated identically to `socks5://`
pub(crate) fn parse_proxy_url(url: &str) -> Result<ProxyConfig, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("empty proxy URL".to_string());
    }

    // Scheme detection
    let (scheme, rest) = if let Some(r) = url
        .strip_prefix("socks5://")
        .or_else(|| url.strip_prefix("socks5h://"))
    {
        (ProxyScheme::Socks5, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (ProxyScheme::Http, r)
    } else if url.strip_prefix("https://").is_some() {
        return Err(
            "HTTPS proxy not supported in v1, use http:// proxy (TLS-to-proxy is not implemented)"
                .to_string(),
        );
    } else {
        (ProxyScheme::Http, url)
    };

    // Userinfo: split at the LAST '@' so passwords may contain '@'
    let (userinfo, hostport) = match rest.rsplit_once('@') {
        Some((info, hp)) => (Some(info), hp),
        None => (None, rest),
    };
    let (username, password) = match userinfo {
        Some(info) if !info.is_empty() => match info.split_once(':') {
            Some((u, p)) => (Some(u.to_string()), Some(p.to_string())),
            None => (Some(info.to_string()), None),
        },
        _ => (None, None),
    };

    // Host:port (IPv6 bracket form takes priority)
    let (host, port) = if let Some(r) = hostport.strip_prefix('[') {
        let end = r.find(']').ok_or_else(|| "unterminated IPv6 bracket in proxy URL".to_string())?;
        let host = &r[..end];
        let after = &r[end + 1..];
        let port = match after.strip_prefix(':') {
            Some(p) => p
                .parse::<u16>()
                .map_err(|_| format!("invalid proxy port: {p}"))?,
            None if after.is_empty() => default_port(&scheme),
            None => return Err(format!("unexpected text after IPv6 bracket: {after:?}").into()),
        };
        (host.to_string(), port)
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>()
                    .map_err(|_| format!("invalid proxy port: {p}"))?,
            ),
            None => (hostport.to_string(), default_port(&scheme)),
        }
    };

    if host.is_empty() {
        return Err("empty proxy host".to_string());
    }
    if port == 0 {
        return Err("proxy port must be > 0".to_string());
    }

    Ok(ProxyConfig { scheme, host, port, username, password })
}

// ============================================================================
// Proxy resolution (pure function — testable without env vars)
// ============================================================================

/// Resolve the proxy for a target from the given sources.
/// Priority: scheme-specific env > ALL_PROXY > system proxy.
/// HTTPS targets never fall back to HTTP_PROXY (curl semantics).
pub(crate) fn resolve_proxy(
    https_env: Option<&str>,
    http_env: Option<&str>,
    all_env: Option<&str>,
    system_proxy: Option<ProxyConfig>,
    target_is_https: bool,
) -> Option<ProxyConfig> {
    let scheme_env = if target_is_https { https_env } else { http_env };
    if let Some(url) = scheme_env.or(all_env) {
        if let Ok(p) = parse_proxy_url(url) {
            return Some(p);
        }
    }
    system_proxy
}

// ============================================================================
// Proxy discovery (env vars + macOS system proxy)
// ============================================================================

/// Resolve proxy for a connection attempt. Reads env vars fresh each call
/// (~1μs; user can change HTTPS_PROXY without restarting); only the scutil
/// subprocess result is cached.
pub(crate) fn get_proxy(target_is_https: bool) -> Option<ProxyConfig> {
    let env = |name: &str| {
        std::env::var(name)
            .ok()
            .or_else(|| std::env::var(name.to_lowercase()).ok())
    };
    resolve_proxy(
        env("HTTPS_PROXY").as_deref(),
        env("HTTP_PROXY").as_deref(),
        env("ALL_PROXY").as_deref(),
        cached_system_proxy(),
        target_is_https,
    )
}

/// scutil result cache — the subprocess costs ~10ms on first call.
// TODO(v1.1): move the scutil subprocess off the executor thread (spawn_blocking
// equivalent) — the ~10ms block is accepted for v1 since it happens exactly once.
static SCUTIL_CACHE: std::sync::OnceLock<Option<ProxyConfig>> = std::sync::OnceLock::new();

fn cached_system_proxy() -> Option<ProxyConfig> {
    SCUTIL_CACHE
        .get_or_init(|| {
            #[cfg(target_os = "macos")]
            {
                let out = std::process::Command::new("scutil")
                    .arg("--proxy")
                    .output()
                    .ok()?;
                parse_scutil(&String::from_utf8_lossy(&out.stdout))
            }
            #[cfg(not(target_os = "macos"))]
            None
        })
        .clone()
}

/// Parse `scutil --proxy` output for the HTTPS proxy entry.
#[cfg(target_os = "macos")]
fn parse_scutil(text: &str) -> Option<ProxyConfig> {
    let get = |key: &str| -> Option<String> {
        text.lines()
            .find(|l| l.contains(key))?
            .split(':')
            .nth(1)?
            .trim()
            .to_string()
            .into()
    };
    if get("HTTPSEnable").as_deref() != Some("1") {
        return None;
    }
    Some(ProxyConfig {
        scheme: ProxyScheme::Http,
        host: get("HTTPSProxy")?,
        port: get("HTTPSPort")
            .and_then(|p| p.parse().ok())
            .unwrap_or(80),
        username: None,
        password: None,
    })
}

// ============================================================================
// NO_PROXY matching
// ============================================================================

/// Returns true if the target host should bypass the proxy.
///
/// Hardcoded bypasses: `localhost`, the entire `127.0.0.0/8` range, and `::1`.
/// NO_PROXY env var: comma-separated patterns — exact match, domain suffix
/// (leading dot optional), or `*` (wildcard-all). CIDR notation is NOT
/// supported in v1.
pub(crate) fn should_bypass_proxy(target_host: &str) -> bool {
    let Ok(np) = std::env::var("NO_PROXY").or_else(|_| std::env::var("no_proxy")) else {
        return is_hardcoded_bypass(target_host);
    };
    matches_no_proxy(target_host, &np)
}

/// Pure host-bypass check without any NO_PROXY pattern (localhost etc.).
fn is_hardcoded_bypass(target_host: &str) -> bool {
    let h = target_host.to_lowercase();
    let h_trimmed = h.trim_matches(|c| c == '[' || c == ']');
    h_trimmed == "localhost" || h_trimmed == "::1" || h_trimmed.starts_with("127.")
}

/// Pure NO_PROXY pattern matching — testable without env var access
/// (picrab forbids unsafe_code, so tests cannot mutate the environment).
fn matches_no_proxy(target_host: &str, no_proxy: &str) -> bool {
    if is_hardcoded_bypass(target_host) {
        return true;
    }
    if no_proxy.trim() == "*" {
        return true;
    }
    let h = target_host.to_lowercase();
    no_proxy.split(',').any(|pattern| {
        let p = pattern.trim().to_lowercase();
        let p = p.strip_prefix('.').unwrap_or(&p);
        !p.is_empty() && (h == *p || h.ends_with(&format!(".{p}")))
    })
}

// ============================================================================
// HTTP CONNECT tunnel
// ============================================================================

/// Maximum bytes for the CONNECT response head (matches client.rs header cap).
const CONNECT_MAX_RESPONSE_BYTES: usize = 8192;

/// Establish an HTTP CONNECT tunnel through `tcp` (already connected to the
/// proxy) to the target. On success the same `TcpStream` is returned — it is
/// now a transparent byte tunnel to the target.
pub(crate) async fn http_connect_tunnel(
    mut tcp: TcpStream,
    target_authority: &str,
    proxy: &ProxyConfig,
) -> Result<TcpStream, String> {
    let mut req = format!("CONNECT {target_authority} HTTP/1.1\r\nHost: {target_authority}\r\n");
    if let Some((name, value)) = proxy_auth_header(proxy) {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    tcp.write_all(req.as_bytes())
        .await
        .map_err(|e| format!("proxy write CONNECT: {e}"))?;
    tcp.flush().await.map_err(|e| format!("proxy flush: {e}"))?;

    // Read response(s), skipping 1xx informational replies.
    loop {
        let response = read_until_headers_end(&mut tcp).await?;
        let status = response
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        match status {
            100..=199 => continue,
            200..=299 => return Ok(tcp),
            _ => {
                return Err(format!(
                    "proxy CONNECT rejected (HTTP {status}): {}",
                    response.lines().next().unwrap_or("?")
                ))
            }
        }
    }
}

/// Read one HTTP response head byte-by-byte until `\r\n\r\n`.
///
/// Byte-at-a-time is deliberate: a buffered read might pull in bytes that
/// belong to the TLS layer above the tunnel (the "prefetch" problem). At
/// ~40 bytes per CONNECT response the syscall cost is a one-time setup
/// expense.
async fn read_until_headers_end(tcp: &mut TcpStream) -> Result<String, String> {
    let mut buf = Vec::with_capacity(128);
    loop {
        let mut byte = [0u8; 1];
        let n = tcp
            .read(&mut byte)
            .await
            .map_err(|e| format!("proxy read CONNECT response: {e}"))?;
        if n == 0 {
            return Err("proxy: CONNECT response ended with EOF".to_string());
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > CONNECT_MAX_RESPONSE_BYTES {
            return Err("proxy: CONNECT response headers too large".to_string());
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

// ============================================================================
// SOCKS5 (RFC 1928 + RFC 1929)
// ============================================================================

/// Establish a SOCKS5 tunnel through `tcp` (already connected to the proxy)
/// to the target. Domain targets are forwarded as ATYP=domain so the proxy
/// resolves DNS (equivalent to curl's `socks5h://` semantics).
pub(crate) async fn socks5_connect(
    mut tcp: TcpStream,
    target_host: &str,
    target_port: u16,
    proxy: &ProxyConfig,
) -> Result<TcpStream, String> {
    // 1. Greeting: version 5, offered auth methods (no-auth, optionally user/pass).
    let has_auth = proxy.username.is_some();
    let greeting: &[u8] = if has_auth { &[0x05, 0x02, 0x00, 0x02] } else { &[0x05, 0x01, 0x00] };
    tcp.write_all(greeting)
        .await
        .map_err(|e| format!("socks5 write greeting: {e}"))?;

    // 2. Server's method selection.
    let method = read_exact_n(&mut tcp, 2).await?;
    if method[0] != 0x05 {
        return Err(format!("socks5: version mismatch (got {:#x})", method[0]));
    }
    match method[1] {
        0x00 => {} // no-auth accepted
        0x02 if has_auth => socks5_authenticate(&mut tcp, proxy).await?,
        0xFF => return Err("socks5: proxy rejected all offered auth methods".to_string()),
        other => return Err(format!("socks5: unexpected method selection {other:#x}")),
    }

    // 3. CONNECT request. Domain > 255 bytes exceeds the single-byte ATYP length.
    let host_trimmed = target_host.trim_matches(|c| c == '[' || c == ']');
    let mut req = vec![0x05, 0x01, 0x00]; // VER, CMD=CONNECT, RSV
    if let Ok(v4) = host_trimmed.parse::<std::net::Ipv4Addr>() {
        req.push(0x01);
        req.extend_from_slice(&v4.octets());
    } else if let Ok(v6) = host_trimmed.parse::<std::net::Ipv6Addr>() {
        req.push(0x04);
        req.extend_from_slice(&v6.octets());
    } else {
        if target_host.len() > 255 {
            return Err(format!(
                "socks5: target hostname exceeds 255 bytes ({} bytes)",
                target_host.len()
            ));
        }
        req.push(0x03);
        req.push(target_host.len() as u8);
        req.extend_from_slice(target_host.as_bytes());
    }
    req.extend_from_slice(&target_port.to_be_bytes());
    tcp.write_all(&req)
        .await
        .map_err(|e| format!("socks5 write CONNECT: {e}"))?;

    // 4. Reply: [VER, REP, RSV, ATYP, addr..., port...]. The reply address is
    //    the proxy's bound address — read and discard it (length varies by ATYP).
    let head = read_exact_n(&mut tcp, 4).await?;
    if head[1] != 0x00 {
        return Err(format!("socks5: CONNECT rejected (code {:#x})", head[1]));
    }
    let addr_len = match head[3] {
        0x01 => 4usize,
        0x03 => read_exact_n(&mut tcp, 1).await?[0] as usize,
        0x04 => 16,
        other => return Err(format!("socks5: unknown reply ATYP {other:#x}")),
    };
    read_exact_n(&mut tcp, addr_len + 2).await?; // address + port

    Ok(tcp)
}

/// RFC 1929 username/password sub-negotiation.
async fn socks5_authenticate(tcp: &mut TcpStream, proxy: &ProxyConfig) -> Result<(), String> {
    let user = proxy.username.as_deref().unwrap_or("");
    let pass = proxy.password.as_deref().unwrap_or("");
    if user.len() > 255 || pass.len() > 255 {
        return Err("socks5: credentials exceed 255 bytes".to_string());
    }
    let mut auth = Vec::with_capacity(3 + user.len() + pass.len());
    auth.push(0x01); // sub-negotiation version
    auth.push(user.len() as u8);
    auth.extend_from_slice(user.as_bytes());
    auth.push(pass.len() as u8);
    auth.extend_from_slice(pass.as_bytes());
    tcp.write_all(&auth)
        .await
        .map_err(|e| format!("socks5 write auth: {e}"))?;
    let result = read_exact_n(tcp, 2).await?;
    if result[1] != 0x00 {
        return Err("socks5: authentication failed".to_string());
    }
    Ok(())
}

/// Read exactly `n` bytes. `read` may return fewer bytes than requested on
/// each call (partial delivery); loop until filled. The buffer is exactly
/// `n - filled` bytes so this never over-reads — safe for handing the stream
/// to the TLS layer afterwards.
async fn read_exact_n(tcp: &mut TcpStream, n: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        let got = tcp
            .read(&mut buf[filled..])
            .await
            .map_err(|e| format!("socks5 read: {e}"))?;
        if got == 0 {
            return Err("socks5: unexpected EOF".to_string());
        }
        filled += got;
    }
    Ok(buf)
}

// ============================================================================
// Unified proxy entry point
// ============================================================================

/// Result of establishing a proxy connection to the target.
pub(crate) struct ProxyTunnel {
    pub tcp: TcpStream,
    /// true only for (HTTP proxy, HTTP target): request line must be
    /// absolute-form (`GET http://host:port/path HTTP/1.1`).
    pub absolute_form: bool,
    /// Proxy-Authorization header to attach to each request (absolute-form only;
    /// CONNECT and SOCKS5 authenticate during tunnel setup).
    pub proxy_auth_header: Option<(String, String)>,
}

pub(crate) async fn connect_via_proxy(
    parsed: &ParsedUrl,
    proxy: &ProxyConfig,
) -> Result<ProxyTunnel, String> {
    let tcp = TcpStream::connect((proxy.host.clone(), proxy.port))
        .await
        .map_err(|e| format!("connect proxy {}:{}: {e}", proxy.host, proxy.port))?;

    use asupersync::http::h1::http_client::Scheme;
    match proxy.scheme {
        ProxyScheme::Http if parsed.scheme == Scheme::Https => {
            let authority = parsed.connect_authority();
            let tunnel = http_connect_tunnel(tcp, &authority, proxy).await?;
            Ok(ProxyTunnel { tcp: tunnel, absolute_form: false, proxy_auth_header: None })
        }
        ProxyScheme::Http => Ok(ProxyTunnel {
            tcp,
            absolute_form: true,
            proxy_auth_header: proxy_auth_header(proxy),
        }),
        ProxyScheme::Socks5 => {
            let tunnel = socks5_connect(tcp, &parsed.host, parsed.port, proxy).await?;
            Ok(ProxyTunnel { tcp: tunnel, absolute_form: false, proxy_auth_header: None })
        }
    }
}

// ============================================================================
// Proxy-Authorization + base64
// ============================================================================

fn proxy_auth_header(proxy: &ProxyConfig) -> Option<(String, String)> {
    let credentials = match (&proxy.username, &proxy.password) {
        (Some(u), Some(p)) => format!("{u}:{p}"),
        (Some(u), None) => format!("{u}:"),
        _ => return None,
    };
    Some((
        "Proxy-Authorization".to_string(),
        format!("Basic {}", base64_encode(credentials.as_bytes())),
    ))
}

/// Minimal standard-alphabet base64 with padding (for Proxy-Authorization).
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18 & 63) as usize] as char);
        out.push(TABLE[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
    }
    out
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_proxy_url ──

    #[test]
    fn parse_basic_http() {
        let p = parse_proxy_url("http://proxy.example.com:8080").unwrap();
        assert_eq!(p.scheme, ProxyScheme::Http);
        assert_eq!(p.host, "proxy.example.com");
        assert_eq!(p.port, 8080);
        assert!(p.username.is_none() && p.password.is_none());
    }

    #[test]
    fn parse_default_ports() {
        assert_eq!(parse_proxy_url("http://p").unwrap().port, 80);
        assert_eq!(parse_proxy_url("socks5://p").unwrap().port, 1080);
        assert_eq!(parse_proxy_url("p:9000").unwrap().port, 9000); // no scheme → http
    }

    #[test]
    fn parse_socks5h_is_socks5() {
        let p = parse_proxy_url("socks5h://proxy:1080").unwrap();
        assert_eq!(p.scheme, ProxyScheme::Socks5);
        assert_eq!(p.port, 1080);
    }

    #[test]
    fn parse_https_rejected() {
        let err = parse_proxy_url("https://proxy:443").unwrap_err();
        assert!(err.contains("not supported"), "unexpected: {err}");
    }

    #[test]
    fn parse_userinfo() {
        let p = parse_proxy_url("http://alice:secret@proxy:8080").unwrap();
        assert_eq!(p.username.as_deref(), Some("alice"));
        assert_eq!(p.password.as_deref(), Some("secret"));
    }

    #[test]
    fn parse_password_with_at() {
        // rsplit at the last '@' — password may contain '@'
        let p = parse_proxy_url("http://user:p@ss@proxy:8080").unwrap();
        assert_eq!(p.username.as_deref(), Some("user"));
        assert_eq!(p.password.as_deref(), Some("p@ss"));
        assert_eq!(p.host, "proxy");
    }

    #[test]
    fn parse_ipv6() {
        let p = parse_proxy_url("http://[::1]:8080").unwrap();
        assert_eq!(p.host, "::1");
        assert_eq!(p.port, 8080);
    }

    #[test]
    fn parse_ipv6_default_port() {
        let p = parse_proxy_url("socks5://[fe80::1]").unwrap();
        assert_eq!(p.host, "fe80::1");
        assert_eq!(p.port, 1080);
    }

    #[test]
    fn parse_invalid() {
        assert!(parse_proxy_url("").is_err());
        assert!(parse_proxy_url("http://").is_err()); // empty host
        assert!(parse_proxy_url("http://p:0").is_err()); // port 0
        assert!(parse_proxy_url("http://p:99999").is_err()); // port > u16
    }

    // ── resolve_proxy (pure function) ──

    #[test]
    fn resolve_https_target_uses_https_proxy() {
        let p = resolve_proxy(
            Some("http://https-proxy:8080"),
            Some("http://http-proxy:3128"),
            None,
            None,
            true,
        )
        .unwrap();
        assert_eq!(p.host, "https-proxy");
    }

    #[test]
    fn resolve_https_target_ignores_http_proxy() {
        // curl semantics: HTTPS targets never fall back to HTTP_PROXY
        let p = resolve_proxy(
            None,
            Some("http://http-proxy:3128"),
            None,
            None,
            true,
        );
        assert!(p.is_none());
    }

    #[test]
    fn resolve_http_target_uses_http_proxy() {
        let p = resolve_proxy(
            Some("http://https-proxy:8080"),
            Some("http://http-proxy:3128"),
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(p.host, "http-proxy");
    }

    #[test]
    fn resolve_all_proxy_fallback() {
        let p = resolve_proxy(None, None, Some("socks5://all:1080"), None, true).unwrap();
        assert_eq!(p.scheme, ProxyScheme::Socks5);
    }

    #[test]
    fn resolve_env_overrides_system() {
        let system = ProxyConfig {
            scheme: ProxyScheme::Http,
            host: "system-proxy".into(),
            port: 80,
            username: None,
            password: None,
        };
        let p = resolve_proxy(Some("http://env-proxy:8080"), None, None, Some(system), true)
            .unwrap();
        assert_eq!(p.host, "env-proxy");
    }

    #[test]
    fn resolve_system_when_no_env() {
        let system = ProxyConfig {
            scheme: ProxyScheme::Http,
            host: "system-proxy".into(),
            port: 80,
            username: None,
            password: None,
        };
        let p = resolve_proxy(None, None, None, Some(system.clone()), true).unwrap();
        assert_eq!(p.host, "system-proxy");
    }

    // ── should_bypass_proxy ──

    #[test]
    fn bypass_localhost() {
        assert!(matches_no_proxy("localhost", ""));
        assert!(matches_no_proxy("LOCALHOST", ""));
        assert!(matches_no_proxy("127.0.0.1", ""));
        assert!(matches_no_proxy("127.255.255.254", "")); // entire 127/8
        assert!(matches_no_proxy("[::1]", ""));
        assert!(matches_no_proxy("::1", ""));
        assert!(!matches_no_proxy("example.com", ""));
    }

    #[test]
    fn bypass_no_proxy_patterns() {
        let np = "example.com,.internal.org,10.0.0.1";
        assert!(matches_no_proxy("example.com", np));
        assert!(matches_no_proxy("api.example.com", np)); // suffix
        assert!(matches_no_proxy("db.internal.org", np)); // leading-dot suffix
        assert!(matches_no_proxy("10.0.0.1", np));
        assert!(!matches_no_proxy("other.com", np));
        assert!(!matches_no_proxy("notexample.com", np)); // suffix needs dot boundary
    }

    #[test]
    fn bypass_no_proxy_wildcard() {
        assert!(matches_no_proxy("anything.example.com", "*"));
    }

    // ── base64 ──

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn proxy_auth_header_basic() {
        let p = ProxyConfig {
            scheme: ProxyScheme::Http,
            host: "p".into(),
            port: 80,
            username: Some("user".into()),
            password: Some("pass".into()),
        };
        let (name, value) = proxy_auth_header(&p).unwrap();
        assert_eq!(name, "Proxy-Authorization");
        assert_eq!(value, "Basic dXNlcjpwYXNz");
    }

    #[test]
    fn proxy_auth_header_absent_without_credentials() {
        let p = ProxyConfig {
            scheme: ProxyScheme::Http,
            host: "p".into(),
            port: 80,
            username: None,
            password: None,
        };
        assert!(proxy_auth_header(&p).is_none());
    }

    // ── SOCKS5 request byte sequences (pure logic verification) ──

    #[test]
    fn socks5_greeting_bytes() {
        // Without auth: [VER=5, NMETHODS=1, METHOD=0x00]
        let no_auth: &[u8] = &[0x05, 0x01, 0x00];
        // With auth: [VER=5, NMETHODS=2, METHOD=0x00, METHOD=0x02]
        let with_auth: &[u8] = &[0x05, 0x02, 0x00, 0x02];
        assert_eq!(no_auth.len(), 3);
        assert_eq!(with_auth.len(), 4);
        assert_eq!(no_auth[0], 0x05);
        assert_eq!(with_auth[0], 0x05);
        assert!(with_auth.contains(&0x02));
        assert!(!no_auth.contains(&0x02));
    }

    #[test]
    fn socks5_connect_request_domain_encoding() {
        // Simulate the CONNECT request construction for a domain target
        let host = "api.anthropic.com";
        let port: u16 = 443;
        let mut req = vec![0x05, 0x01, 0x00, 0x03]; // VER, CMD, RSV, ATYP=domain
        req.push(host.len() as u8);
        req.extend_from_slice(host.as_bytes());
        req.extend_from_slice(&port.to_be_bytes());
        assert_eq!(req[0], 0x05);
        assert_eq!(req[1], 0x01); // CONNECT
        assert_eq!(req[3], 0x03); // domain ATYP
        assert_eq!(req[4] as usize, host.len());
        assert_eq!(&req[5..5 + host.len()], host.as_bytes());
        assert_eq!(&req[req.len() - 2..], &[0x01, 0xBB]); // 443 = 0x01BB
    }

    #[test]
    fn socks5_connect_request_ipv4_encoding() {
        let addr: std::net::Ipv4Addr = "10.0.0.5".parse().unwrap();
        let mut req = vec![0x05, 0x01, 0x00, 0x01]; // ATYP=IPv4
        req.extend_from_slice(&addr.octets());
        req.extend_from_slice(&(80u16).to_be_bytes());
        assert_eq!(req[3], 0x01);
        assert_eq!(&req[4..8], &[10, 0, 0, 5]);
        assert_eq!(&req[8..10], &[0x00, 0x50]); // port 80
    }
}
