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

use std::sync::OnceLock;
use std::sync::RwLock;

/// Transport used to reach the proxy itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyScheme {
    /// Plain HTTP hop to the proxy (`http://…`).
    Http,
    /// TLS hop to the proxy (`https://…`).
    Https,
}

/// A resolved proxy endpoint for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyEndpoint {
    /// Transport used to reach the proxy.
    pub scheme: ProxyScheme,
    /// Proxy host (no brackets for IPv6 — ready for `TcpStream::connect`).
    pub host: String,
    /// Proxy port.
    pub port: u16,
    /// `Proxy-Authorization` header value derived from the URL's userinfo.
    pub authorization: Option<String>,
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
        let scheme = match self.scheme {
            ProxyScheme::Http => "http",
            ProxyScheme::Https => "https",
        };
        format!("{scheme}://{}", self.authority())
    }
}

/// `[http]` section of settings.json (`Config::http`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// its subdomains, and `host:port` restricts the match to that port.
    #[serde(alias = "noProxy")]
    pub no_proxy: Option<Vec<String>>,
    /// Ignore the ambient `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` /
    /// `NO_PROXY` variables; only the settings above (and `PI_*_PROXY`) apply.
    #[serde(alias = "ignoreEnvProxy")]
    pub ignore_env_proxy: Option<bool>,
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

/// A fully merged proxy configuration: settings.json plus the environment,
/// resolved once and then consulted per request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyConfig {
    https: Option<ProxyEndpoint>,
    http: Option<ProxyEndpoint>,
    no_proxy: Vec<String>,
    /// `*` (or `NO_PROXY=*`) — everything is direct.
    bypass_all: bool,
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

        let pick = |explicit: &[Option<&str>],
                    pi_vars: &[&str],
                    std_vars: &[&str],
                    warnings: &mut Vec<String>| {
            for value in explicit.iter().flatten() {
                if is_disable_value(value) {
                    return None;
                }
                match parse_proxy_url(value) {
                    Ok(endpoint) => return Some(endpoint),
                    Err(err) => {
                        warnings.push(format!("ignoring http proxy setting {value:?}: {err}"));
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
                        warnings.push(format!("ignoring {name}={value:?}: {err}"));
                    }
                }
            }
            None
        };

        let https = pick(
            &[settings.https_proxy.as_deref(), settings.proxy.as_deref()],
            &PI_HTTPS_PROXY_VARS,
            &STD_HTTPS_PROXY_VARS,
            &mut warnings,
        );
        let http = pick(
            &[settings.http_proxy.as_deref(), settings.proxy.as_deref()],
            &PI_HTTP_PROXY_VARS,
            &STD_HTTP_PROXY_VARS,
            &mut warnings,
        );

        let no_proxy_raw = settings.no_proxy.map_or_else(
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
        );
        let bypass_all = no_proxy_raw.iter().any(|entry| entry == "*");
        let no_proxy = no_proxy_raw
            .into_iter()
            .map(|entry| entry.to_ascii_lowercase())
            .collect();

        (
            Self {
                https,
                http,
                no_proxy,
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
        let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
        self.no_proxy.iter().any(|entry| {
            let (pattern, entry_port) = match entry.rsplit_once(':') {
                // Only treat the tail as a port when it parses AND the head is
                // not an unbracketed IPv6 literal.
                Some((head, tail)) if !head.contains(':') => tail
                    .parse::<u16>()
                    .map_or((entry.as_str(), None), |value| (head, Some(value))),
                _ => (entry.as_str(), None),
            };
            if entry_port.is_some_and(|expected| expected != port) {
                return false;
            }
            let pattern = pattern.trim_matches(['[', ']']);
            if pattern.is_empty() {
                return false;
            }
            let bare = pattern.strip_prefix('.').unwrap_or(pattern);
            host == bare || host.ends_with(&format!(".{bare}"))
        })
    }
}

/// Parse a proxy URL. Accepts `host:port` shorthand (assumed `http://`), which
/// is what people usually put in `HTTPS_PROXY`.
///
/// # Errors
///
/// Returns a human-readable message for an unsupported scheme (SOCKS is not
/// implemented), a missing host, or an unparseable port.
pub fn parse_proxy_url(raw: &str) -> std::result::Result<ProxyEndpoint, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty proxy URL".to_string());
    }
    let (scheme, rest) = match raw.split_once("://") {
        Some((scheme, rest)) => {
            let scheme = match scheme.to_ascii_lowercase().as_str() {
                "http" => ProxyScheme::Http,
                "https" => ProxyScheme::Https,
                other => {
                    return Err(format!(
                        "unsupported proxy scheme {other:?} (only http:// and https:// are supported)"
                    ));
                }
            };
            (scheme, rest)
        }
        None => (ProxyScheme::Http, raw),
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
        let port = match tail.strip_prefix(':') {
            Some(port) => Some(
                port.parse::<u16>()
                    .map_err(|_| format!("invalid proxy port {port:?}"))?,
            ),
            None => None,
        };
        (host.to_string(), port)
    } else if let Some((host, port)) = hostport.rsplit_once(':')
        && !host.contains(':')
    {
        (
            host.to_string(),
            Some(
                port.parse::<u16>()
                    .map_err(|_| format!("invalid proxy port {port:?}"))?,
            ),
        )
    } else {
        (hostport, None)
    };

    if host.is_empty() {
        return Err("proxy URL has no host".to_string());
    }

    let port = port.unwrap_or(match scheme {
        ProxyScheme::Http => 80,
        ProxyScheme::Https => 443,
    });

    let authorization = userinfo.filter(|info| !info.is_empty()).map(|info| {
        let decoded = percent_decode_userinfo(&info);
        format!("Basic {}", base64_encode(decoded.as_bytes()))
    });

    Ok(ProxyEndpoint {
        scheme,
        host,
        port,
        authorization,
    })
}

/// Percent-decode a `user:password` pair (proxy credentials commonly encode
/// `@` and `:` this way).
fn percent_decode_userinfo(raw: &str) -> String {
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
    String::from_utf8_lossy(&out).into_owned()
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
        assert_eq!(endpoint.scheme, ProxyScheme::Http);
        assert_eq!(endpoint.host, "127.0.0.1");
        assert_eq!(endpoint.port, 2080);
        assert_eq!(endpoint.authorization, None);
        assert_eq!(endpoint.authority(), "127.0.0.1:2080");
    }

    #[test]
    fn bare_authority_defaults_to_http() {
        let endpoint = parse_proxy_url("proxy.corp:3128").expect("parse");
        assert_eq!(endpoint.scheme, ProxyScheme::Http);
        assert_eq!(endpoint.host, "proxy.corp");
        assert_eq!(endpoint.port, 3128);
    }

    #[test]
    fn scheme_default_ports_apply() {
        assert_eq!(parse_proxy_url("http://p.example").expect("http").port, 80);
        assert_eq!(
            parse_proxy_url("https://p.example").expect("https").port,
            443
        );
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
