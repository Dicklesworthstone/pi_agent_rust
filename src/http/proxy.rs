//! Outbound HTTP proxy resolution for every request Pi makes (#210).
//!
//! Pi's HTTP client is hand-rolled, so proxy support has to be explicit: this
//! module owns *which* proxy a given request URL should go through, and
//! [`crate::http::client`] owns the wire mechanics (CONNECT tunnel for
//! `https://`, absolute-form request line for `http://`).
//!
//! # Precedence
//!
//! For a request to `<scheme>://<host>:<port>`:
//!
//! 1. A bypass match (`http.noProxy` in settings.json, else `NO_PROXY` /
//!    `no_proxy`) wins over everything — the request goes direct.
//! 2. `http.httpsProxy` / `http.httpProxy` from settings.json (scheme-specific).
//! 3. `http.proxy` from settings.json (both schemes).
//! 4. `PI_HTTPS_PROXY` / `PI_HTTP_PROXY` — the pi-prefixed, unambiguous
//!    environment override.
//! 5. The standard `HTTPS_PROXY` / `https_proxy` (for `https://` targets),
//!    `HTTP_PROXY` / `http_proxy` (for `http://` targets), then `ALL_PROXY` /
//!    `all_proxy`.
//!
//! Step 5 is what every other developer tool does, so Pi honors it by default.
//! Environments where those variables are set for an unrelated tool (a capture
//! proxy, a stale VPN helper) can switch the inheritance off with
//! `"http": { "ignoreEnvProxy": true }` or `PI_HTTP_PROXY=off`, which leaves
//! only the explicit settings above in play.
//!
//! Lowercase variants are accepted for every standard name. `ALL_PROXY` is
//! frequently pointed at a SOCKS endpoint; unsupported schemes coming from the
//! environment are ignored (with a warning) rather than failing the request,
//! while an unsupported scheme written explicitly into settings.json is
//! reported as a configuration warning at startup.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::OnceLock;
use std::sync::RwLock;

/// A resolved proxy endpoint for one request.
///
/// Always a plain-HTTP hop: the origin's TLS runs end-to-end inside a CONNECT
/// tunnel, so the hop to the proxy itself is never TLS. A `https://` proxy URL
/// is rejected by [`parse_proxy_url`] rather than silently downgraded.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyEndpoint {
    /// Proxy host (no brackets for IPv6 — ready for `TcpStream::connect`).
    pub host: String,
    /// Proxy port.
    pub port: u16,
    /// `Proxy-Authorization` header value derived from the URL's userinfo.
    pub authorization: Option<String>,
}

impl fmt::Debug for ProxyEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyEndpoint")
            .field("host", &self.host)
            .field("port", &self.port)
            .field(
                "authorization",
                &self.authorization.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl ProxyEndpoint {
    /// `host:port` in authority form (IPv6 hosts bracketed).
    #[must_use]
    pub fn authority(&self) -> String {
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The proxy URL with any credentials removed — safe to log.
    #[must_use]
    pub fn redacted_url(&self) -> String {
        format!("http://{}", self.authority())
    }
}

/// `[http]` section of settings.json (`Config::http`).
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HttpSettings {
    /// Proxy for both `http://` and `https://` requests.
    pub proxy: Option<String>,
    /// Proxy for `https://` requests only; overrides [`Self::proxy`].
    #[serde(alias = "httpsProxy")]
    pub https_proxy: Option<String>,
    /// Proxy for `http://` requests only; overrides [`Self::proxy`].
    #[serde(alias = "httpProxy")]
    pub http_proxy: Option<String>,
    /// Hosts that must never go through a proxy. Replaces `NO_PROXY` when set.
    ///
    /// Entries match the standard way: `*` bypasses everything, a leading dot
    /// (`.example.com`) or a bare domain (`example.com`) matches the domain and
    /// its subdomains, and `host:port` restricts the match to that port. IPv4
    /// and IPv6 CIDR ranges match literal request addresses without DNS
    /// resolution. Malformed entries never widen the bypass policy.
    #[serde(alias = "noProxy")]
    pub no_proxy: Option<Vec<String>>,
    /// Ignore the ambient `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` /
    /// `NO_PROXY` variables; only the settings above (and `PI_*_PROXY`) apply.
    #[serde(alias = "ignoreEnvProxy")]
    pub ignore_env_proxy: Option<bool>,
}

impl fmt::Debug for HttpSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Even malformed URLs can contain credentials. Do not attempt to
        // parse them to decide which parts are safe to print.
        f.debug_struct("HttpSettings")
            .field("proxy", &self.proxy.as_ref().map(|_| "<redacted>"))
            .field(
                "https_proxy",
                &self.https_proxy.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "http_proxy",
                &self.http_proxy.as_ref().map(|_| "<redacted>"),
            )
            .field("no_proxy", &self.no_proxy)
            .field("ignore_env_proxy", &self.ignore_env_proxy)
            .finish()
    }
}

/// Environment variable names read for proxy configuration, most specific
/// first within each group.
const PI_HTTPS_PROXY_VARS: [&str; 2] = ["PI_HTTPS_PROXY", "PI_HTTP_PROXY"];
const PI_HTTP_PROXY_VARS: [&str; 1] = ["PI_HTTP_PROXY"];
const STD_HTTPS_PROXY_VARS: [&str; 2] = ["HTTPS_PROXY", "https_proxy"];
const STD_HTTP_PROXY_VARS: [&str; 2] = ["HTTP_PROXY", "http_proxy"];
const STD_ALL_PROXY_VARS: [&str; 2] = ["ALL_PROXY", "all_proxy"];
const STD_NO_PROXY_VARS: [&str; 2] = ["NO_PROXY", "no_proxy"];

/// Values that mean "no proxy, and do not inherit one from the environment".
fn is_disable_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "off" | "no" | "none" | "false" | "direct"
    )
}

/// Parsed once at configuration time, not once per outbound request. Network
/// rules intentionally match only literal IPs: resolving a hostname locally
/// to decide whether to use a proxy would leak DNS and change routing policy.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NoProxyRule {
    Domain { name: String, port: Option<u16> },
    Address { address: IpAddr, port: Option<u16> },
    Network { address: IpAddr, prefix: u8 },
}

impl NoProxyRule {
    fn parse(entry: &str) -> Option<Self> {
        if let Some((network, prefix)) = entry.split_once('/') {
            if prefix.is_empty() || !prefix.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let prefix = prefix.parse::<u8>().ok()?;
            let address: IpAddr = if let Some(inner) = network.strip_prefix('[') {
                inner.strip_suffix(']')?.parse::<Ipv6Addr>().ok()?.into()
            } else {
                network.parse().ok()?
            };
            let bits = if address.is_ipv4() { 32 } else { 128 };
            return (prefix <= bits).then_some(Self::Network { address, prefix });
        }

        // An unbracketed IPv6 literal must be recognized before looking for
        // a port; its final numeric component is part of the address.
        if let Ok(address) = entry.parse::<IpAddr>() {
            return Some(Self::Address {
                address,
                port: None,
            });
        }

        if let Some(inner) = entry.strip_prefix('[') {
            let (address, tail) = inner.split_once(']')?;
            let address = address.parse::<Ipv6Addr>().ok()?.into();
            let port = if tail.is_empty() {
                None
            } else {
                // Never turn an invalid port/suffix into an unqualified
                // bypass. `[::1]:oops` must not mean "all ports on ::1".
                Some(parse_proxy_port(tail.strip_prefix(':')?).ok()?)
            };
            return Some(Self::Address { address, port });
        }

        let (name, port) = if let Some((name, port)) = entry.split_once(':') {
            (name, Some(parse_proxy_port(port).ok()?))
        } else {
            (entry, None)
        };
        let name = name.strip_prefix('.').unwrap_or(name);
        let name = name.strip_suffix('.').unwrap_or(name);
        if let Ok(address) = name.parse::<IpAddr>() {
            return Some(Self::Address { address, port });
        }
        if name.split('.').any(str::is_empty)
            || !name
                .chars()
                .all(|ch| ch.is_alphanumeric() || matches!(ch, '.' | '-' | '_'))
        {
            return None;
        }
        Some(Self::Domain {
            name: name.to_string(),
            port,
        })
    }

    fn matches(&self, host: &str, address: Option<IpAddr>, port: u16) -> bool {
        match self {
            Self::Domain {
                name,
                port: rule_port,
            } => {
                address.is_none()
                    && rule_port.is_none_or(|expected| expected == port)
                    && (host == name.as_str()
                        || host
                            .strip_suffix(name.as_str())
                            .is_some_and(|prefix| prefix.ends_with('.')))
            }
            Self::Address {
                address: expected,
                port: rule_port,
            } => address == Some(*expected) && rule_port.is_none_or(|expected| expected == port),
            Self::Network {
                address: network,
                prefix,
            } => match (*network, address) {
                (IpAddr::V4(network), Some(IpAddr::V4(address))) => {
                    // checked_shl maps /0 to a zero mask; /32 and /128 use
                    // shift zero. parse() bounds prefixes to their family.
                    let mask = u32::MAX.checked_shl(32 - u32::from(*prefix)).unwrap_or(0);
                    (u32::from(network) & mask) == (u32::from(address) & mask)
                }
                (IpAddr::V6(network), Some(IpAddr::V6(address))) => {
                    let mask = u128::MAX.checked_shl(128 - u32::from(*prefix)).unwrap_or(0);
                    (u128::from(network) & mask) == (u128::from(address) & mask)
                }
                _ => false,
            },
        }
    }
}

/// A fully merged proxy configuration: settings.json plus the environment,
/// resolved once and then consulted per request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyConfig {
    https: Option<ProxyEndpoint>,
    http: Option<ProxyEndpoint>,
    no_proxy: Vec<String>,
    no_proxy_rules: Vec<NoProxyRule>,
    /// `*` (or `NO_PROXY=*`) — everything is direct.
    bypass_all: bool,
}

fn pick_proxy_endpoint(
    explicit: &[Option<&str>],
    pi_vars: &[&str],
    std_vars: &[&str],
    ignore_env: bool,
    env: &dyn Fn(&str) -> Option<String>,
    warnings: &mut Vec<String>,
) -> Option<ProxyEndpoint> {
    for value in explicit.iter().flatten() {
        if is_disable_value(value) {
            return None;
        }
        match parse_proxy_url(value) {
            Ok(endpoint) => return Some(endpoint),
            Err(err) => {
                warnings.push(format!("ignoring http proxy setting: {err}"));
            }
        }
    }
    let env_names: Vec<&str> = if ignore_env {
        pi_vars.to_vec()
    } else {
        pi_vars
            .iter()
            .chain(std_vars.iter())
            .chain(STD_ALL_PROXY_VARS.iter())
            .copied()
            .collect()
    };
    for name in env_names {
        let Some(value) = env(name) else { continue };
        if is_disable_value(&value) {
            return None;
        }
        match parse_proxy_url(&value) {
            Ok(endpoint) => return Some(endpoint),
            Err(err) => {
                // An unusable ambient value must not fail requests:
                // `ALL_PROXY` is routinely a SOCKS endpoint meant for
                // other tools. Warn once and keep looking.
                warnings.push(format!("ignoring {name}: {err}"));
            }
        }
    }
    None
}

fn resolve_no_proxy_raw(
    settings_no_proxy: Option<Vec<String>>,
    ignore_env: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    settings_no_proxy.map_or_else(
        || {
            if ignore_env {
                Vec::new()
            } else {
                STD_NO_PROXY_VARS
                    .iter()
                    .find_map(|name| env(name))
                    .map(|value| {
                        value
                            .split(',')
                            .map(str::trim)
                            .filter(|entry| !entry.is_empty())
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            }
        },
        |entries| {
            entries
                .into_iter()
                .map(|entry| entry.trim().to_string())
                .filter(|entry| !entry.is_empty())
                .collect()
        },
    )
}

impl ProxyConfig {
    /// Merge settings and environment into the effective configuration.
    ///
    /// `env` looks up an environment variable by name; the indirection keeps
    /// this pure and unit-testable (and keeps tests from mutating process
    /// state that other threads observe). Returns the config plus any
    /// human-facing warnings the caller should surface once at startup.
    #[must_use]
    pub fn resolve(
        settings: Option<&HttpSettings>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let settings = settings.cloned().unwrap_or_default();

        // A `PI_HTTP_PROXY` set to a disable value is an explicit opt-out of
        // ambient proxy inheritance, matching `ignoreEnvProxy`.
        let pi_disable = PI_HTTPS_PROXY_VARS
            .iter()
            .filter_map(|name| env(name))
            .any(|value| is_disable_value(&value));
        let ignore_env = settings.ignore_env_proxy.unwrap_or(false) || pi_disable;

        let https = pick_proxy_endpoint(
            &[settings.https_proxy.as_deref(), settings.proxy.as_deref()],
            &PI_HTTPS_PROXY_VARS,
            &STD_HTTPS_PROXY_VARS,
            ignore_env,
            env,
            &mut warnings,
        );
        let http = pick_proxy_endpoint(
            &[settings.http_proxy.as_deref(), settings.proxy.as_deref()],
            &PI_HTTP_PROXY_VARS,
            &STD_HTTP_PROXY_VARS,
            ignore_env,
            env,
            &mut warnings,
        );

        let no_proxy_raw = resolve_no_proxy_raw(settings.no_proxy, ignore_env, env);
        // The two `pick` passes share `http.proxy` and `ALL_PROXY`, so an
        // unusable value would otherwise be reported twice.
        warnings.dedup();

        let bypass_all = no_proxy_raw.iter().any(|entry| entry == "*");
        let no_proxy: Vec<String> = no_proxy_raw
            .into_iter()
            .map(|entry| entry.to_ascii_lowercase())
            .collect();
        let no_proxy_rules = no_proxy
            .iter()
            .filter_map(|entry| NoProxyRule::parse(entry))
            .collect();

        (
            Self {
                https,
                http,
                no_proxy,
                no_proxy_rules,
                bypass_all,
            },
            warnings,
        )
    }

    /// Whether any proxy is configured at all (used for child-process env
    /// injection and for cheap early-outs).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.https.is_none() && self.http.is_none()
    }

    /// The proxy a request to `https`/`http` `host:port` must use, if any.
    #[must_use]
    pub fn endpoint_for(&self, https: bool, host: &str, port: u16) -> Option<&ProxyEndpoint> {
        if self.bypass_all || self.matches_no_proxy(host, port) {
            return None;
        }
        if https {
            self.https.as_ref()
        } else {
            self.http.as_ref()
        }
    }

    /// The proxy URL (credentials stripped) to advertise to child processes
    /// for the given scheme, if one is configured.
    #[must_use]
    pub fn redacted_url_for(&self, https: bool) -> Option<String> {
        let endpoint = if https {
            self.https.as_ref()
        } else {
            self.http.as_ref()
        };
        endpoint.map(ProxyEndpoint::redacted_url)
    }

    /// The bypass list, normalized to lowercase.
    #[must_use]
    pub fn no_proxy_entries(&self) -> &[String] {
        &self.no_proxy
    }

    fn matches_no_proxy(&self, host: &str, port: u16) -> bool {
        if self.no_proxy_rules.is_empty() {
            return false;
        }
        let host = if let Some(inner) = host.strip_prefix('[') {
            let Some(host) = inner.strip_suffix(']') else {
                return false;
            };
            if host.parse::<Ipv6Addr>().is_err() {
                return false;
            }
            host
        } else {
            host
        };
        let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
        let address = host.parse::<IpAddr>().ok();
        self.no_proxy_rules
            .iter()
            .any(|rule| rule.matches(&host, address, port))
    }
}

/// Parse a proxy URL. Accepts `host:port` shorthand (assumed `http://`), which
/// is what people usually put in `HTTPS_PROXY`.
///
/// Note that `HTTPS_PROXY=http://…` is the normal spelling: the variable names
/// the traffic being proxied, not the hop to the proxy.
///
/// # Errors
///
/// Returns a human-readable message for an unsupported scheme (`https://`
/// proxies would need TLS-in-TLS, and SOCKS is not implemented), a missing
/// host, a malformed authority, an invalid port, or invalid Basic credentials.
/// Error messages never include input values, which may contain secrets even
/// when the URL is malformed.
pub fn parse_proxy_url(raw: &str) -> std::result::Result<ProxyEndpoint, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty proxy URL".to_string());
    }
    if raw.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
        return Err("proxy URL contains unescaped whitespace or control characters".to_string());
    }
    let rest = match raw.split_once("://") {
        Some((scheme, rest)) => {
            if !scheme.eq_ignore_ascii_case("http") {
                return Err(
                    "unsupported proxy scheme (only http:// proxy endpoints are supported; \
                     https:// targets are proxied through an http:// proxy with CONNECT)"
                        .to_string(),
                );
            }
            rest
        }
        None => raw,
    };
    // Drop any path/query the value carries; a proxy is an authority.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_string();
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((userinfo, hostport)) => (Some(userinfo.to_string()), hostport.to_string()),
        None => (None, authority),
    };
    if hostport.is_empty() {
        return Err("proxy URL has no host".to_string());
    }

    let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
        // Bracketed IPv6 literal.
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| "unterminated IPv6 proxy host".to_string())?;
        let host = host
            .parse::<Ipv6Addr>()
            .map_err(|_| "invalid IPv6 proxy host".to_string())?;
        let port = if tail.is_empty() {
            None
        } else {
            let port = tail
                .strip_prefix(':')
                .ok_or_else(|| "unexpected suffix after IPv6 proxy host".to_string())?;
            Some(parse_proxy_port(port)?)
        };
        (host.to_string(), port)
    } else if let Some((host, port)) = hostport.rsplit_once(':') {
        if host.contains(':') {
            return Err("IPv6 proxy hosts must be bracketed".to_string());
        }
        (host.to_string(), Some(parse_proxy_port(port)?))
    } else {
        (hostport, None)
    };

    if host.is_empty() {
        return Err("proxy URL has no host".to_string());
    }
    if host.contains(['[', ']', '@', '\\', '%']) {
        return Err("invalid proxy host".to_string());
    }

    let port = port.unwrap_or(80);

    let authorization = userinfo
        .filter(|info| !info.is_empty())
        .map(|info| {
            // Split before decoding: an escaped colon in a password is data,
            // not the username/password delimiter. Basic usernames cannot
            // contain a colon; a username alone implies an empty password.
            let (username, password) = info.split_once(':').unwrap_or((info.as_str(), ""));
            let mut decoded = percent_decode_userinfo(username);
            if decoded.contains(&b':') {
                return Err("proxy username must not contain a colon".to_string());
            }
            decoded.push(b':');
            decoded.extend(percent_decode_userinfo(password));
            if decoded.iter().any(u8::is_ascii_control) {
                return Err("proxy credentials contain control characters".to_string());
            }
            Ok(format!("Basic {}", base64_encode(&decoded)))
        })
        .transpose()?;

    Ok(ProxyEndpoint {
        host,
        port,
        authorization,
    })
}

fn parse_proxy_port(raw: &str) -> std::result::Result<u16, String> {
    raw.parse::<u16>()
        .ok()
        .filter(|port| *port != 0 && raw.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| "invalid proxy port (expected an integer from 1 to 65535)".to_string())
}

/// Percent-decode credential bytes without lossy UTF-8 conversion. Basic
/// authentication encodes octets; replacing a non-UTF-8 password byte changes
/// the credential and causes authentication to fail.
fn percent_decode_userinfo(raw: &str) -> Vec<u8> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (
                (bytes[index + 1] as char).to_digit(16),
                (bytes[index + 2] as char).to_digit(16),
            )
        {
            #[allow(clippy::cast_possible_truncation)]
            out.push((hi * 16 + lo) as u8);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

/// Minimal standard base64 encoder (proxy credentials only; no dependency).
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Process-wide effective proxy configuration.
///
/// Installed once at startup from settings.json + the environment
/// ([`configure`]); every [`crate::http::client::Client`] consults it, so a
/// proxy applies uniformly to provider calls, OAuth, update checks, URL reads,
/// and package fetches without threading configuration through ~50 call sites.
static PROXY_CONFIG: OnceLock<RwLock<ProxyConfig>> = OnceLock::new();

fn slot() -> &'static RwLock<ProxyConfig> {
    PROXY_CONFIG.get_or_init(|| {
        // Lazy default: environment only. A process that never calls
        // `configure` (tests, library embedders) still honors the standard
        // variables.
        let (config, _warnings) = ProxyConfig::resolve(None, &|name| std::env::var(name).ok());
        RwLock::new(config)
    })
}

/// Install the effective proxy configuration from settings.json + environment.
///
/// Returns warnings for unusable values so the caller can surface them once.
/// Call during startup, before the first HTTP request.
pub fn configure(settings: Option<&HttpSettings>) -> Vec<String> {
    let (config, warnings) = ProxyConfig::resolve(settings, &|name| std::env::var(name).ok());
    install(config);
    warnings
}

/// Replace the process-wide configuration (startup wiring and tests).
pub fn install(config: ProxyConfig) {
    let mut guard = slot()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = config;
}

/// A snapshot of the process-wide configuration.
#[must_use]
pub fn active() -> ProxyConfig {
    slot()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Environment overrides to hand to a child process so tools Pi shells out to
/// (`git`, `curl`, `npm`, …) reach the network the same way Pi does (#210).
///
/// Only returns entries when a proxy is actually configured; values are
/// credential-free.
#[must_use]
pub fn child_process_env() -> Vec<(String, String)> {
    let config = active();
    let mut vars = Vec::new();
    if let Some(url) = config.redacted_url_for(true) {
        vars.push(("HTTPS_PROXY".to_string(), url.clone()));
        vars.push(("https_proxy".to_string(), url));
    }
    if let Some(url) = config.redacted_url_for(false) {
        vars.push(("HTTP_PROXY".to_string(), url.clone()));
        vars.push(("http_proxy".to_string(), url));
    }
    if !vars.is_empty() && !config.no_proxy_entries().is_empty() {
        let joined = config.no_proxy_entries().join(",");
        vars.push(("NO_PROXY".to_string(), joined.clone()));
        vars.push(("no_proxy".to_string(), joined));
    }
    vars
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn resolve(settings: Option<&HttpSettings>, pairs: &[(&str, &str)]) -> ProxyConfig {
        ProxyConfig::resolve(settings, &env_from(pairs)).0
    }

    // ─── URL parsing ────────────────────────────────────────────────────

    #[test]
    fn parses_scheme_host_and_port() {
        let endpoint = parse_proxy_url("http://127.0.0.1:2080").expect("parse");
        assert_eq!(endpoint.host, "127.0.0.1");
        assert_eq!(endpoint.port, 2080);
        assert_eq!(endpoint.authorization, None);
        assert_eq!(endpoint.authority(), "127.0.0.1:2080");
    }

    #[test]
    fn bare_authority_defaults_to_http() {
        let endpoint = parse_proxy_url("proxy.corp:3128").expect("parse");
        assert_eq!(endpoint.host, "proxy.corp");
        assert_eq!(endpoint.port, 3128);
    }

    #[test]
    fn port_defaults_to_80() {
        assert_eq!(parse_proxy_url("http://p.example").expect("http").port, 80);
    }

    /// A TLS hop to the proxy would need TLS-in-TLS; reject it where the value
    /// is read rather than at request time.
    #[test]
    fn https_proxy_endpoints_are_rejected_at_parse_time() {
        let err = parse_proxy_url("https://proxy:8443").expect_err("https hop unsupported");
        assert!(err.contains("only http:// proxy endpoints"), "{err}");
    }

    #[test]
    fn credentials_become_basic_authorization() {
        let endpoint = parse_proxy_url("http://user:p%40ss@proxy:8080").expect("parse");
        assert_eq!(endpoint.host, "proxy");
        // base64("user:p@ss")
        assert_eq!(
            endpoint.authorization.as_deref(),
            Some("Basic dXNlcjpwQHNz")
        );
        assert_eq!(
            endpoint.redacted_url(),
            "http://proxy:8080",
            "credentials must never appear in the loggable URL"
        );
    }

    #[test]
    fn ipv6_literals_round_trip() {
        let endpoint = parse_proxy_url("http://[::1]:2080").expect("parse");
        assert_eq!(endpoint.host, "::1");
        assert_eq!(endpoint.port, 2080);
        assert_eq!(endpoint.authority(), "[::1]:2080");
    }

    #[test]
    fn path_and_query_are_dropped() {
        let endpoint = parse_proxy_url("http://proxy:8080/pac?x=1").expect("parse");
        assert_eq!(endpoint.host, "proxy");
        assert_eq!(endpoint.port, 8080);
    }

    #[test]
    fn unsupported_scheme_and_bad_port_are_errors() {
        let err = parse_proxy_url("socks5://127.0.0.1:1080").expect_err("socks unsupported");
        assert!(err.contains("unsupported proxy scheme"), "{err}");
        assert!(parse_proxy_url("http://proxy:notaport").is_err());
        assert!(parse_proxy_url("   ").is_err());
        assert!(parse_proxy_url("http://").is_err());
    }

    #[test]
    fn malformed_authorities_are_rejected_before_connecting() {
        for raw in [
            "http://[::1]ignored",
            "http://[::1]:",
            "http://[not-an-ip]:8080",
            "http://[]:8080",
            "http://::1",
            "http://proxy]:8080",
            "http://proxy\\other:8080",
            "http://proxy%0ahost:8080",
            "http://proxy host:8080",
            "http://proxy\r\nInjected:8080",
            "http://proxy:0",
            "http://proxy:+8080",
            "http://proxy:65536",
        ] {
            assert!(parse_proxy_url(raw).is_err(), "accepted {raw:?}");
        }
        let endpoint = parse_proxy_url("http://[0:0:0:0:0:0:0:1]:65535").expect("IPv6");
        assert_eq!(endpoint.authority(), "[::1]:65535");
    }

    #[test]
    fn basic_credentials_preserve_octets_and_empty_passwords() {
        for (raw, expected) in [
            ("http://user@proxy", "Basic dXNlcjo="),
            ("http://user:%FF%3A%FE@proxy", "Basic dXNlcjr/Ov4="),
            ("http://u:p%40ss%3Aword@proxy", "Basic dTpwQHNzOndvcmQ="),
        ] {
            let endpoint = parse_proxy_url(raw).expect("valid credentials");
            assert_eq!(endpoint.authorization.as_deref(), Some(expected));
        }
    }

    #[test]
    fn basic_credentials_reject_ambiguous_usernames_and_controls() {
        for raw in [
            "http://user%3Aother:password@proxy",
            "http://user:password%0D%0A@proxy",
            "http://user%00:password@proxy",
            "http://user:password%7F@proxy",
        ] {
            assert!(parse_proxy_url(raw).is_err(), "accepted {raw:?}");
        }
    }

    #[test]
    fn proxy_diagnostics_never_echo_malformed_values() {
        for raw in [
            "socks5://sentinel-user:sentinel-secret@proxy:1080",
            "sentinel-scheme://proxy:8080",
            "http://user:sentinel-secret@proxy:sentinel-port",
            "http://user:sentinel-secret@[::1]sentinel-suffix",
            "http://user:sentinel-secret@proxy\r\nInjected:8080",
        ] {
            let error = parse_proxy_url(raw).expect_err("invalid proxy");
            assert!(!error.contains("sentinel"), "{error}");
            let settings = HttpSettings {
                proxy: Some(raw.to_string()),
                ..HttpSettings::default()
            };
            let (_, warnings) = ProxyConfig::resolve(Some(&settings), &env_from(&[]));
            assert!(!warnings.is_empty());
            assert!(!format!("{warnings:?} {settings:?}").contains("sentinel"));
            let (_, warnings) = ProxyConfig::resolve(
                None,
                &env_from(&[("HTTPS_PROXY", raw), ("HTTP_PROXY", raw)]),
            );
            assert!(!warnings.is_empty());
            assert!(warnings.iter().all(|warning| !warning.contains("sentinel")));
            assert!(
                warnings
                    .iter()
                    .any(|warning| warning.contains("HTTPS_PROXY"))
            );
        }
    }

    #[test]
    fn debug_redacts_plaintext_and_encoded_proxy_credentials() {
        let settings = HttpSettings {
            proxy: Some("http://sentinel-user:sentinel-secret@proxy:8080".to_string()),
            https_proxy: Some("http://sentinel-user:sentinel-secret@proxy:8080".to_string()),
            http_proxy: Some("http://sentinel-user:sentinel-secret@proxy:8080".to_string()),
            ..HttpSettings::default()
        };
        let config = resolve(Some(&settings), &[]);
        let endpoint = config
            .endpoint_for(true, "api.example.com", 443)
            .expect("proxy");
        let authorization = endpoint.authorization.as_deref().expect("credentials");
        let diagnostic = format!("{settings:?} {config:?} {endpoint:?}");
        assert!(!diagnostic.contains("sentinel"));
        assert!(!diagnostic.contains(authorization));
        assert!(diagnostic.contains("<redacted>"));
        assert_eq!(endpoint.redacted_url(), "http://proxy:8080");
    }

    // ─── Precedence ─────────────────────────────────────────────────────

    #[test]
    fn settings_proxy_beats_environment() {
        let settings = HttpSettings {
            proxy: Some("http://settings:1111".to_string()),
            ..HttpSettings::default()
        };
        let config = resolve(
            Some(&settings),
            &[
                ("HTTPS_PROXY", "http://env:2222"),
                ("PI_HTTP_PROXY", "http://pi:3333"),
            ],
        );
        assert_eq!(
            config
                .endpoint_for(true, "api.example.com", 443)
                .map(ProxyEndpoint::redacted_url),
            Some("http://settings:1111".to_string())
        );
    }

    #[test]
    fn scheme_specific_settings_beat_generic_setting() {
        let settings = HttpSettings {
            proxy: Some("http://generic:1111".to_string()),
            https_proxy: Some("http://secure:2222".to_string()),
            ..HttpSettings::default()
        };
        let config = resolve(Some(&settings), &[]);
        assert_eq!(
            config
                .endpoint_for(true, "api.example.com", 443)
                .map(ProxyEndpoint::redacted_url),
            Some("http://secure:2222".to_string())
        );
        assert_eq!(
            config
                .endpoint_for(false, "api.example.com", 80)
                .map(ProxyEndpoint::redacted_url),
            Some("http://generic:1111".to_string())
        );
    }

    #[test]
    fn pi_prefixed_env_beats_standard_env() {
        let config = resolve(
            None,
            &[
                ("PI_HTTP_PROXY", "http://pi:3333"),
                ("HTTPS_PROXY", "http://std:4444"),
            ],
        );
        assert_eq!(
            config
                .endpoint_for(true, "api.example.com", 443)
                .map(ProxyEndpoint::redacted_url),
            Some("http://pi:3333".to_string())
        );
    }

    #[test]
    fn standard_env_is_honored_per_scheme_then_all_proxy() {
        let config = resolve(
            None,
            &[
                ("HTTPS_PROXY", "http://secure:2222"),
                ("http_proxy", "http://plain:1111"),
            ],
        );
        assert_eq!(
            config
                .endpoint_for(true, "h", 443)
                .map(ProxyEndpoint::redacted_url),
            Some("http://secure:2222".to_string())
        );
        assert_eq!(
            config
                .endpoint_for(false, "h", 80)
                .map(ProxyEndpoint::redacted_url),
            Some("http://plain:1111".to_string())
        );

        let all_only = resolve(None, &[("ALL_PROXY", "http://all:9999")]);
        assert_eq!(
            all_only
                .endpoint_for(true, "h", 443)
                .map(ProxyEndpoint::redacted_url),
            Some("http://all:9999".to_string())
        );
        assert_eq!(
            all_only
                .endpoint_for(false, "h", 80)
                .map(ProxyEndpoint::redacted_url),
            Some("http://all:9999".to_string())
        );
    }

    #[test]
    fn ignore_env_proxy_drops_ambient_values_but_keeps_settings() {
        let settings = HttpSettings {
            proxy: Some("http://settings:1111".to_string()),
            ignore_env_proxy: Some(true),
            ..HttpSettings::default()
        };
        let config = resolve(Some(&settings), &[("HTTPS_PROXY", "http://env:2222")]);
        assert_eq!(
            config
                .endpoint_for(true, "h", 443)
                .map(ProxyEndpoint::redacted_url),
            Some("http://settings:1111".to_string())
        );

        let env_only = HttpSettings {
            ignore_env_proxy: Some(true),
            ..HttpSettings::default()
        };
        let config = resolve(Some(&env_only), &[("HTTPS_PROXY", "http://env:2222")]);
        assert!(config.is_empty(), "ambient proxy must be ignored");
    }

    #[test]
    fn pi_http_proxy_off_disables_env_inheritance() {
        let config = resolve(
            None,
            &[("PI_HTTP_PROXY", "off"), ("HTTPS_PROXY", "http://env:2222")],
        );
        assert!(config.is_empty());
    }

    #[test]
    fn unusable_env_value_is_skipped_with_a_warning_not_an_error() {
        let (config, warnings) = ProxyConfig::resolve(
            None,
            &env_from(&[
                ("ALL_PROXY", "socks5://127.0.0.1:1080"),
                ("HTTPS_PROXY", "http://good:8080"),
            ]),
        );
        assert_eq!(
            config
                .endpoint_for(true, "h", 443)
                .map(ProxyEndpoint::redacted_url),
            Some("http://good:8080".to_string())
        );
        // http:// targets fall through to ALL_PROXY, which is unusable here.
        assert_eq!(config.endpoint_for(false, "h", 80), None);
        assert!(
            warnings.iter().any(|w| w.contains("ALL_PROXY")),
            "expected a warning about the SOCKS value: {warnings:?}"
        );
    }

    // ─── Bypass ─────────────────────────────────────────────────────────

    #[test]
    fn no_proxy_matches_exact_suffix_and_port() {
        let config = resolve(
            None,
            &[
                ("HTTPS_PROXY", "http://p:8080"),
                ("NO_PROXY", "localhost, .internal.example, api.direct:8443"),
            ],
        );
        assert!(config.endpoint_for(true, "localhost", 443).is_none());
        assert!(
            config
                .endpoint_for(true, "svc.internal.example", 443)
                .is_none()
        );
        assert!(config.endpoint_for(true, "internal.example", 443).is_none());
        assert!(config.endpoint_for(true, "api.direct", 8443).is_none());
        // Same host on a different port is NOT bypassed.
        assert!(config.endpoint_for(true, "api.direct", 443).is_some());
        // A suffix that is not a domain boundary must not match.
        assert!(
            config
                .endpoint_for(true, "notinternal.example", 443)
                .is_some()
        );
        assert!(config.endpoint_for(true, "api.example.com", 443).is_some());
    }

    #[test]
    fn no_proxy_star_bypasses_everything() {
        let config = resolve(None, &[("HTTPS_PROXY", "http://p:8080"), ("NO_PROXY", "*")]);
        assert!(config.endpoint_for(true, "api.example.com", 443).is_none());
    }

    #[test]
    fn settings_no_proxy_replaces_the_environment_list() {
        let settings = HttpSettings {
            proxy: Some("http://p:8080".to_string()),
            no_proxy: Some(vec!["only.internal".to_string()]),
            ..HttpSettings::default()
        };
        let config = resolve(Some(&settings), &[("NO_PROXY", "everything.example")]);
        assert!(config.endpoint_for(true, "only.internal", 443).is_none());
        assert!(
            config
                .endpoint_for(true, "everything.example", 443)
                .is_some()
        );
    }

    #[test]
    fn ipv6_host_bypass_matches_without_brackets() {
        let config = resolve(
            None,
            &[("HTTPS_PROXY", "http://p:8080"), ("NO_PROXY", "::1")],
        );
        assert!(config.endpoint_for(true, "::1", 443).is_none());
        assert!(config.endpoint_for(true, "[::1]", 443).is_none());

        let with_port = resolve(
            None,
            &[("HTTPS_PROXY", "http://p:8080"), ("NO_PROXY", "[::1]:8443")],
        );
        assert!(with_port.endpoint_for(true, "::1", 8443).is_none());
        assert!(
            with_port.endpoint_for(true, "::1", 443).is_some(),
            "a port-qualified bypass entry must not match other ports"
        );
    }

    #[test]
    fn no_proxy_ipv4_cidr_matches_subnet_boundaries() {
        let config = resolve(
            None,
            &[
                ("ALL_PROXY", "http://p:8080"),
                ("NO_PROXY", "10.16.0.0/12,192.0.2.9/32"),
            ],
        );
        for (host, bypass) in [
            ("10.15.255.255", false),
            ("10.16.0.0", true),
            ("10.31.255.255", true),
            ("10.32.0.0", false),
            ("192.0.2.9", true),
            ("192.0.2.8", false),
            ("service.10.16.0.1", false),
        ] {
            for (https, port) in [(true, 443), (false, 80)] {
                assert_eq!(
                    config.endpoint_for(https, host, port).is_none(),
                    bypass,
                    "{host}:{port}"
                );
            }
        }
    }

    #[test]
    fn no_proxy_ipv6_cidr_matches_subnets_and_single_addresses() {
        for rules in ["2001:db8::/32,fc00::1/128", "[2001:db8::]/32,fc00::1/128"] {
            let config = resolve(None, &[("ALL_PROXY", "http://p:8080"), ("NO_PROXY", rules)]);
            for (host, bypass) in [
                ("2001:db7:ffff:ffff::1", false),
                ("[2001:db8::]", true),
                ("2001:db8:ffff:ffff:ffff:ffff:ffff:ffff", true),
                ("2001:db9::", false),
                ("fc00:0:0:0:0:0:0:1", true),
                ("fc00::2", false),
                ("192.0.2.1", false),
            ] {
                assert_eq!(
                    config.endpoint_for(true, host, 443).is_none(),
                    bypass,
                    "{host}"
                );
            }
        }
    }

    #[test]
    fn no_proxy_zero_prefixes_are_family_specific_and_never_resolve_names() {
        for (rule, direct, proxied) in [
            ("0.0.0.0/0", "203.0.113.9", "::1"),
            ("::/0", "::ffff:192.0.2.1", "127.0.0.1"),
        ] {
            let config = resolve(None, &[("ALL_PROXY", "http://p:8080"), ("NO_PROXY", rule)]);
            assert!(config.endpoint_for(true, direct, 443).is_none());
            assert!(config.endpoint_for(true, proxied, 443).is_some());
            assert!(config.endpoint_for(true, "localhost", 443).is_some());
            assert!(config.endpoint_for(true, "api.example.com", 443).is_some());
        }
    }

    #[test]
    fn no_proxy_addresses_use_numeric_equality_not_dns_suffixes() {
        let config = resolve(
            None,
            &[
                ("ALL_PROXY", "http://p:8080"),
                ("NO_PROXY", "127.0.0.1:8080,[::1]:8443"),
            ],
        );
        assert!(config.endpoint_for(false, "127.0.0.1", 8080).is_none());
        assert!(config.endpoint_for(false, "127.0.0.1", 80).is_some());
        assert!(
            config
                .endpoint_for(true, "[0:0:0:0:0:0:0:1]", 8443)
                .is_none()
        );
        assert!(config.endpoint_for(true, "0:0:0:0:0:0:0:1", 443).is_some());
        assert!(config.endpoint_for(false, "leak.127.0.0.1", 8080).is_some());
    }

    #[test]
    fn invalid_no_proxy_entries_cannot_disable_the_proxy() {
        for rule in [
            "[::1]:oops",
            "[::1]:",
            "[::1]:65536",
            "[::1]:+443",
            "[::1]ignored",
            "[::1",
            "[[::1]]",
            "[127.0.0.1]",
            "127.0.0.1:invalid",
            "127.0.0.1:0",
            "127.0.0.0/33",
            "127.0.0.0/-1",
            "127.0.0.0/+1",
            "127.0.0.0/8:443",
            "::1/129",
            "::1/256",
            "::1/",
            "api.example.com/0",
            "*/0",
        ] {
            let config = resolve(None, &[("ALL_PROXY", "http://p:8080"), ("NO_PROXY", rule)]);
            for host in ["::1", "127.0.0.1", "api.example.com"] {
                for port in [80, 443, 8443] {
                    assert!(
                        config.endpoint_for(true, host, port).is_some(),
                        "invalid rule {rule:?} bypassed {host}:{port}"
                    );
                }
            }
        }
    }

    #[test]
    fn no_proxy_dns_names_normalize_case_and_root_dots() {
        let settings = HttpSettings {
            proxy: Some("http://p:8080".to_string()),
            no_proxy: Some(vec![
                ".Corp.Example.".to_string(),
                "127.0.0.0/8".to_string(),
            ]),
            ..HttpSettings::default()
        };
        let config = resolve(Some(&settings), &[("NO_PROXY", "ignored.example")]);
        for host in [
            "CORP.EXAMPLE",
            "corp.example.",
            "api.Corp.Example.",
            "127.2.3.4",
        ] {
            assert!(config.endpoint_for(true, host, 443).is_none(), "{host}");
        }
        for host in [
            "notcorp.example",
            "corp.example.attacker",
            "ignored.example",
        ] {
            assert!(config.endpoint_for(true, host, 443).is_some(), "{host}");
        }
        assert_eq!(
            config.no_proxy_entries(),
            &[".corp.example.".to_string(), "127.0.0.0/8".to_string()]
        );
    }

    #[test]
    fn no_proxy_network_masks_cover_every_valid_prefix_width() {
        let network_v4 = 0xc000_0281_u32;
        for prefix in 0..=32 {
            let rule = NoProxyRule::parse(&format!("192.0.2.129/{prefix}")).expect("IPv4 CIDR");
            for value in [
                0,
                u32::MAX,
                network_v4,
                network_v4 ^ 1,
                network_v4 ^ 0x8000_0000,
            ] {
                let address = IpAddr::V4(std::net::Ipv4Addr::from(value));
                // Independent reference: count common leading bits rather
                // than constructing the mask used by the implementation.
                let expected = (network_v4 ^ value).leading_zeros() >= prefix;
                assert_eq!(
                    rule.matches("", Some(address), 443),
                    expected,
                    "{address}/{prefix}"
                );
            }
        }
        let network_v6 = u128::from("2001:db8::1".parse::<Ipv6Addr>().expect("IPv6"));
        for prefix in 0..=128 {
            let rule = NoProxyRule::parse(&format!("2001:db8::1/{prefix}")).expect("IPv6 CIDR");
            for value in [
                0,
                u128::MAX,
                network_v6,
                network_v6 ^ 1,
                network_v6 ^ (1 << 127),
            ] {
                let address = IpAddr::V6(Ipv6Addr::from(value));
                let expected = (network_v6 ^ value).leading_zeros() >= prefix;
                assert_eq!(
                    rule.matches("", Some(address), 443),
                    expected,
                    "{address}/{prefix}"
                );
            }
        }
    }

    // ─── Child-process env ──────────────────────────────────────────────

    #[test]
    fn child_env_carries_credential_free_urls_and_bypass_list() {
        let settings = HttpSettings {
            proxy: Some("http://user:secret@127.0.0.1:2080".to_string()),
            no_proxy: Some(vec!["localhost".to_string()]),
            ignore_env_proxy: Some(true),
            ..HttpSettings::default()
        };
        let config = resolve(Some(&settings), &[]);
        install(config);
        let vars = child_process_env();
        let lookup = |key: &str| {
            vars.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(lookup("HTTPS_PROXY"), "http://127.0.0.1:2080");
        assert_eq!(lookup("https_proxy"), "http://127.0.0.1:2080");
        assert_eq!(lookup("HTTP_PROXY"), "http://127.0.0.1:2080");
        assert_eq!(lookup("NO_PROXY"), "localhost");
        assert!(
            !vars.iter().any(|(_, v)| v.contains("secret")),
            "credentials must not leak into child environments: {vars:?}"
        );

        install(ProxyConfig::default());
        assert!(
            child_process_env().is_empty(),
            "no proxy configured means no injected variables"
        );
    }

    #[test]
    fn settings_deserialize_from_camel_case_json() {
        let settings: HttpSettings = serde_json::from_str(
            r#"{"proxy":"http://127.0.0.1:2080","noProxy":["localhost"],"ignoreEnvProxy":true}"#,
        )
        .expect("deserialize");
        assert_eq!(settings.proxy.as_deref(), Some("http://127.0.0.1:2080"));
        assert_eq!(settings.no_proxy, Some(vec!["localhost".to_string()]));
        assert_eq!(settings.ignore_env_proxy, Some(true));
    }
}
