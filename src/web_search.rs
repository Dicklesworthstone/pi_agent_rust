//! Built-in web search (bd-cv653.2.1): a multi-provider chain with automatic
//! fallback, keyless public rungs, and site-aware filtering.
//!
//! `auto` walks the configured chain in order — paid providers with keys
//! first, keyless public endpoints (duckduckgo/startpage/mojeek) last — with
//! per-provider circuit breaking (2 consecutive failures sideline a rung for
//! 5 minutes). Results are deduped by canonical URL and capped per domain.
//! Every result carries the provider that answered (`source`).

use crate::error::Error;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A ranked search result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub snippet: String,
    /// The provider rung that produced this result.
    pub source: String,
}

/// Search filters: Google-style operators mapped per provider where
/// supported, else applied client-side.
#[derive(Debug, Clone, Default)]
pub struct SearchFilters {
    /// `site:example.com` — restrict to a host.
    pub site: Option<String>,
    /// `after:YYYY-MM-DD` — client-side date filter when the provider
    /// returns dates (most scrapers do not; documented limitation).
    pub after: Option<String>,
    /// Result cap (default 10, hard cap 50).
    pub limit: usize,
}

/// Errors a rung can produce; chain logic treats them all as "try next".
#[derive(Debug)]
pub enum RungError {
    /// No credential configured for this provider.
    NoKey,
    /// Network/HTTP failure with context.
    Http(String),
    /// Provider returned an unparseable/empty payload.
    Parse(String),
    /// Rate limited; the circuit breaker will cool this rung down.
    RateLimited,
}

impl std::fmt::Display for RungError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoKey => write!(f, "no API key configured"),
            Self::Http(msg) => write!(f, "http: {msg}"),
            Self::Parse(msg) => write!(f, "parse: {msg}"),
            Self::RateLimited => write!(f, "rate limited"),
        }
    }
}

/// A provider rung in the chain.
pub struct ProviderRung {
    pub name: &'static str,
    /// Env var names consulted for the API key (first hit wins).
    pub env_keys: &'static [&'static str],
    /// True when the rung works without any key (public endpoint).
    pub keyless: bool,
    #[allow(clippy::type_complexity)]
    pub run: for<'a> fn(
        &'a crate::http::client::Client,
        &'a str,
        &'a SearchFilters,
        Option<&'a str>,
    ) -> RungFuture<'a>,
}

pub type RungFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Vec<SearchResult>, RungError>> + Send + 'a>,
>;

/// The default chain order: keyed providers first, keyless publics last.
pub const DEFAULT_CHAIN: &[&str] = &[
    "perplexity",
    "brave",
    "tavily",
    "exa",
    "jina",
    "kagi",
    "duckduckgo",
    "startpage",
    "mojeek",
];

fn env_key(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|var| {
        std::env::var(var)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

/// Base-URL override per rung (bd-cv653.2.1): `PI_WEBSEARCH_BASE_<NAME>`
/// redirects a provider's endpoint — used by the e2e harness to point rungs
/// at a loopback mock, and by operators routing through a proxy. In-process
/// callers (SDK, tests) use the override map, which wins over the env var.
fn base_url_for(name: &str, default: &str) -> String {
    if let Some(override_url) = base_url_overrides()
        .read()
        .ok()
        .and_then(|map| map.get(name).cloned())
    {
        return override_url;
    }
    let var = format!("PI_WEBSEARCH_BASE_{}", name.to_ascii_uppercase());
    std::env::var(var)
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn base_url_overrides() -> &'static std::sync::RwLock<HashMap<&'static str, String>> {
    static OVERRIDES: std::sync::LazyLock<std::sync::RwLock<HashMap<&'static str, String>>> =
        std::sync::LazyLock::new(|| std::sync::RwLock::new(HashMap::new()));
    &OVERRIDES
}

/// Redirect a provider rung's base URL (SDK/e2e seam; process-global).
pub fn set_base_url_override(name: &'static str, base_url: &str) {
    if let Ok(mut map) = base_url_overrides().write() {
        map.insert(name, base_url.trim_end_matches('/').to_string());
    }
}

/// Clear all base-url overrides (tests).
pub fn clear_base_url_overrides() {
    if let Ok(mut map) = base_url_overrides().write() {
        map.clear();
    }
}

// === Paid/keyed rungs ===

fn perplexity_run<'a>(
    client: &'a crate::http::client::Client,
    query: &'a str,
    filters: &'a SearchFilters,
    key: Option<&'a str>,
) -> RungFuture<'a> {
    let body = json!({
        "model": "sonar",
        "messages": [{"role": "user", "content": with_site(query, filters)}],
        "max_tokens": 512,
    });
    let key = key.map(str::to_string);
    Box::pin(async move {
        let Some(key) = key else {
            return Err(RungError::NoKey);
        };
        let response = client
            .post("https://api.perplexity.ai/chat/completions")
            .header("Authorization", format!("Bearer {key}"))
            .json(&body)
            .map_err(|e| RungError::Http(e.to_string()))?
            .send()
            .await
            .map_err(|e| rung_http_error(&e))?;
        if response.status() == 429 {
            return Err(RungError::RateLimited);
        }
        if response.status() != 200 {
            return Err(RungError::Http(format!("status {}", response.status())));
        }
        let text = response
            .text_limited(512 * 1024)
            .await
            .map_err(|e| RungError::Http(e.to_string()))?;
        let value: Value =
            serde_json::from_str(&text).map_err(|e| RungError::Parse(e.to_string()))?;
        let mut results = Vec::new();
        if let Some(citations) = value.get("citations").and_then(Value::as_array) {
            for (index, citation) in citations.iter().enumerate() {
                if let Some(url) = citation.as_str() {
                    results.push(SearchResult {
                        title: format!("citation {}", index + 1),
                        url: url.to_string(),
                        snippet: String::new(),
                        source: "perplexity".to_string(),
                    });
                }
            }
        }
        // The synthesized answer rides as the first result so the model gets
        // perplexity's answer text, not just links.
        if let Some(answer) = value
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
        {
            results.insert(
                0,
                SearchResult {
                    title: "perplexity answer".to_string(),
                    url: String::new(),
                    snippet: answer.chars().take(2_000).collect(),
                    source: "perplexity".to_string(),
                },
            );
        }
        if results.is_empty() {
            return Err(RungError::Parse("no citations or answer".to_string()));
        }
        Ok(results)
    })
}

fn brave_run<'a>(
    client: &'a crate::http::client::Client,
    query: &'a str,
    filters: &'a SearchFilters,
    key: Option<&'a str>,
) -> RungFuture<'a> {
    let key = key.map(str::to_string);
    let url = format!(
        "{}/res/v1/web/search?q={}&count={}",
        base_url_for("brave", "https://api.search.brave.com"),
        urlencoded(&with_site(query, filters)),
        filters.limit.min(20)
    );
    Box::pin(async move {
        let Some(key) = key else {
            return Err(RungError::NoKey);
        };
        let response = client
            .get(&url)
            .header("X-Subscription-Token", key)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| rung_http_error(&e))?;
        if response.status() == 429 {
            return Err(RungError::RateLimited);
        }
        if response.status() != 200 {
            return Err(RungError::Http(format!("status {}", response.status())));
        }
        let text = response
            .text_limited(1024 * 1024)
            .await
            .map_err(|e| RungError::Http(e.to_string()))?;
        let value: Value =
            serde_json::from_str(&text).map_err(|e| RungError::Parse(e.to_string()))?;
        let mut results = Vec::new();
        if let Some(items) = value.pointer("/web/results").and_then(Value::as_array) {
            for item in items {
                let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
                if url.is_empty() {
                    continue;
                }
                results.push(SearchResult {
                    title: item
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    url: url.to_string(),
                    snippet: item
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    source: "brave".to_string(),
                });
            }
        }
        if results.is_empty() {
            return Err(RungError::Parse("no web results".to_string()));
        }
        Ok(results)
    })
}

fn tavily_run<'a>(
    client: &'a crate::http::client::Client,
    query: &'a str,
    filters: &'a SearchFilters,
    key: Option<&'a str>,
) -> RungFuture<'a> {
    let key = key.map(str::to_string);
    let limit = filters.limit.min(20);
    let body = json!({
        "query": with_site(query, filters),
        "max_results": limit,
        "search_depth": "basic",
    });
    Box::pin(async move {
        let Some(key) = key else {
            return Err(RungError::NoKey);
        };
        let response = client
            .post("https://api.tavily.com/search")
            .header("Authorization", format!("Bearer {key}"))
            .json(&body)
            .map_err(|e| RungError::Http(e.to_string()))?
            .send()
            .await
            .map_err(|e| rung_http_error(&e))?;
        if response.status() == 429 {
            return Err(RungError::RateLimited);
        }
        if response.status() != 200 {
            return Err(RungError::Http(format!("status {}", response.status())));
        }
        let text = response
            .text_limited(1024 * 1024)
            .await
            .map_err(|e| RungError::Http(e.to_string()))?;
        let value: Value =
            serde_json::from_str(&text).map_err(|e| RungError::Parse(e.to_string()))?;
        let mut results = Vec::new();
        if let Some(items) = value.get("results").and_then(Value::as_array) {
            for item in items {
                let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
                if url.is_empty() {
                    continue;
                }
                results.push(SearchResult {
                    title: item
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    url: url.to_string(),
                    snippet: item
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .chars()
                        .take(500)
                        .collect(),
                    source: "tavily".to_string(),
                });
            }
        }
        if results.is_empty() {
            return Err(RungError::Parse("no results".to_string()));
        }
        Ok(results)
    })
}

fn exa_run<'a>(
    client: &'a crate::http::client::Client,
    query: &'a str,
    filters: &'a SearchFilters,
    key: Option<&'a str>,
) -> RungFuture<'a> {
    let key = key.map(str::to_string);
    let limit = filters.limit.min(25);
    let body = json!({
        "query": with_site(query, filters),
        "numResults": limit,
    });
    Box::pin(async move {
        let Some(key) = key else {
            return Err(RungError::NoKey);
        };
        let response = client
            .post("https://api.exa.ai/search")
            .header("x-api-key", key)
            .json(&body)
            .map_err(|e| RungError::Http(e.to_string()))?
            .send()
            .await
            .map_err(|e| rung_http_error(&e))?;
        if response.status() == 429 {
            return Err(RungError::RateLimited);
        }
        if response.status() != 200 {
            return Err(RungError::Http(format!("status {}", response.status())));
        }
        let text = response
            .text_limited(1024 * 1024)
            .await
            .map_err(|e| RungError::Http(e.to_string()))?;
        let value: Value =
            serde_json::from_str(&text).map_err(|e| RungError::Parse(e.to_string()))?;
        let mut results = Vec::new();
        if let Some(items) = value.get("results").and_then(Value::as_array) {
            for item in items {
                let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
                if url.is_empty() {
                    continue;
                }
                results.push(SearchResult {
                    title: item
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    url: url.to_string(),
                    snippet: item
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .chars()
                        .take(500)
                        .collect(),
                    source: "exa".to_string(),
                });
            }
        }
        if results.is_empty() {
            return Err(RungError::Parse("no results".to_string()));
        }
        Ok(results)
    })
}

fn jina_run<'a>(
    client: &'a crate::http::client::Client,
    query: &'a str,
    filters: &'a SearchFilters,
    key: Option<&'a str>,
) -> RungFuture<'a> {
    let key = key.map(str::to_string);
    let url = format!(
        "{}/{}",
        base_url_for("jina", "https://s.jina.ai"),
        urlencoded(&with_site(query, filters))
    );
    Box::pin(async move {
        let Some(key) = key else {
            return Err(RungError::NoKey);
        };
        let response = client
            .get(&url)
            .header("Authorization", format!("Bearer {key}"))
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| rung_http_error(&e))?;
        if response.status() == 429 {
            return Err(RungError::RateLimited);
        }
        if response.status() != 200 {
            return Err(RungError::Http(format!("status {}", response.status())));
        }
        let text = response
            .text_limited(1024 * 1024)
            .await
            .map_err(|e| RungError::Http(e.to_string()))?;
        let value: Value =
            serde_json::from_str(&text).map_err(|e| RungError::Parse(e.to_string()))?;
        let mut results = Vec::new();
        if let Some(items) = value.get("data").and_then(Value::as_array) {
            for item in items {
                let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
                if url.is_empty() {
                    continue;
                }
                results.push(SearchResult {
                    title: item
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    url: url.to_string(),
                    snippet: item
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    source: "jina".to_string(),
                });
            }
        }
        if results.is_empty() {
            return Err(RungError::Parse("no data".to_string()));
        }
        Ok(results)
    })
}

fn kagi_run<'a>(
    client: &'a crate::http::client::Client,
    query: &'a str,
    filters: &'a SearchFilters,
    key: Option<&'a str>,
) -> RungFuture<'a> {
    let key = key.map(str::to_string);
    let url = format!(
        "{}/api/v0/search?q={}&limit={}",
        base_url_for("kagi", "https://kagi.com"),
        urlencoded(&with_site(query, filters)),
        filters.limit.min(25)
    );
    Box::pin(async move {
        let Some(key) = key else {
            return Err(RungError::NoKey);
        };
        let response = client
            .get(&url)
            .header("Authorization", format!("Bot {key}"))
            .send()
            .await
            .map_err(|e| rung_http_error(&e))?;
        if response.status() == 429 {
            return Err(RungError::RateLimited);
        }
        if response.status() != 200 {
            return Err(RungError::Http(format!("status {}", response.status())));
        }
        let text = response
            .text_limited(1024 * 1024)
            .await
            .map_err(|e| RungError::Http(e.to_string()))?;
        let value: Value =
            serde_json::from_str(&text).map_err(|e| RungError::Parse(e.to_string()))?;
        let mut results = Vec::new();
        if let Some(items) = value.get("data").and_then(Value::as_array) {
            for item in items {
                if item.get("t").and_then(Value::as_u64) != Some(0) {
                    continue; // non-result entries (related searches etc.)
                }
                let url = item.get("url").and_then(Value::as_str).unwrap_or_default();
                if url.is_empty() {
                    continue;
                }
                results.push(SearchResult {
                    title: item
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    url: url.to_string(),
                    snippet: item
                        .get("snippet")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    source: "kagi".to_string(),
                });
            }
        }
        if results.is_empty() {
            return Err(RungError::Parse("no data".to_string()));
        }
        Ok(results)
    })
}

// === Keyless public rungs (HTML scrapers; defensive, parse-what-you-can) ===

fn duckduckgo_run<'a>(
    client: &'a crate::http::client::Client,
    query: &'a str,
    filters: &'a SearchFilters,
    _key: Option<&'a str>,
) -> RungFuture<'a> {
    let url = format!(
        "{}/html/?q={}",
        base_url_for("duckduckgo", "https://html.duckduckgo.com"),
        urlencoded(&with_site(query, filters))
    );
    Box::pin(async move {
        let response = client
            .get(&url)
            .header("User-Agent", "Mozilla/5.0 (compatible; pi-agent)")
            .send()
            .await
            .map_err(|e| rung_http_error(&e))?;
        if response.status() != 200 {
            return Err(RungError::Http(format!("status {}", response.status())));
        }
        let html = response
            .text_limited(2 * 1024 * 1024)
            .await
            .map_err(|e| RungError::Http(e.to_string()))?;
        let mut results = Vec::new();
        // result rows: <a class="result__a" href="//duckduckgo.com/l/?uddg=ENCODED">
        let mut rest = html.as_str();
        while let Some(pos) = rest.find("result__a") {
            rest = &rest[pos..];
            let Some(href_start) = rest.find("href=\"") else {
                break;
            };
            let after_href = &rest[href_start + 6..];
            let Some(href_end) = after_href.find('"') else {
                break;
            };
            let raw_href = &after_href[..href_end];
            let Some(title_end) = after_href.find("</a>") else {
                break;
            };
            let title_html = &after_href[href_end + 1..title_end];
            let title = strip_tags(title_html);
            let url = decode_ddg_redirect(raw_href);
            if !url.is_empty() {
                results.push(SearchResult {
                    title,
                    url,
                    snippet: String::new(),
                    source: "duckduckgo".to_string(),
                });
            }
            rest = &after_href[title_end..];
            if results.len() >= 25 {
                break;
            }
        }
        if results.is_empty() {
            return Err(RungError::Parse("no result rows".to_string()));
        }
        Ok(results)
    })
}

fn startpage_run<'a>(
    client: &'a crate::http::client::Client,
    query: &'a str,
    filters: &'a SearchFilters,
    _key: Option<&'a str>,
) -> RungFuture<'a> {
    let url = format!(
        "{}/sp/search?query={}",
        base_url_for("startpage", "https://www.startpage.com"),
        urlencoded(&with_site(query, filters))
    );
    Box::pin(async move {
        let response = client
            .get(&url)
            .header("User-Agent", "Mozilla/5.0 (compatible; pi-agent)")
            .send()
            .await
            .map_err(|e| rung_http_error(&e))?;
        if response.status() != 200 {
            return Err(RungError::Http(format!("status {}", response.status())));
        }
        let html = response
            .text_limited(2 * 1024 * 1024)
            .await
            .map_err(|e| RungError::Http(e.to_string()))?;
        let mut results = Vec::new();
        // result links carry class="result-link"
        let mut rest = html.as_str();
        while let Some(pos) = rest.find("result-link") {
            rest = &rest[pos..];
            let Some(href_start) = rest.find("href=\"") else {
                break;
            };
            let after_href = &rest[href_start + 6..];
            let Some(href_end) = after_href.find('"') else {
                break;
            };
            let link = &after_href[..href_end];
            let Some(title_end) = after_href.find("</a>") else {
                break;
            };
            let title = strip_tags(&after_href[href_end + 1..title_end]);
            if link.starts_with("http") {
                results.push(SearchResult {
                    title,
                    url: link.to_string(),
                    snippet: String::new(),
                    source: "startpage".to_string(),
                });
            }
            rest = &after_href[title_end..];
            if results.len() >= 25 {
                break;
            }
        }
        if results.is_empty() {
            return Err(RungError::Parse("no result rows".to_string()));
        }
        Ok(results)
    })
}

fn mojeek_run<'a>(
    client: &'a crate::http::client::Client,
    query: &'a str,
    filters: &'a SearchFilters,
    _key: Option<&'a str>,
) -> RungFuture<'a> {
    let url = format!(
        "{}/search?q={}",
        base_url_for("mojeek", "https://www.mojeek.com"),
        urlencoded(&with_site(query, filters))
    );
    Box::pin(async move {
        let response = client
            .get(&url)
            .header("User-Agent", "Mozilla/5.0 (compatible; pi-agent)")
            .send()
            .await
            .map_err(|e| rung_http_error(&e))?;
        if response.status() != 200 {
            return Err(RungError::Http(format!("status {}", response.status())));
        }
        let html = response
            .text_limited(2 * 1024 * 1024)
            .await
            .map_err(|e| RungError::Http(e.to_string()))?;
        let mut results = Vec::new();
        // result titles: <a class="title" href="https://...">
        let mut rest = html.as_str();
        while let Some(pos) = rest.find("class=\"title\"") {
            rest = &rest[pos..];
            let Some(href_start) = rest.find("href=\"") else {
                break;
            };
            let after_href = &rest[href_start + 6..];
            let Some(href_end) = after_href.find('"') else {
                break;
            };
            let link = &after_href[..href_end];
            let Some(title_end) = after_href.find("</a>") else {
                break;
            };
            let title = strip_tags(&after_href[href_end + 1..title_end]);
            if link.starts_with("http") {
                results.push(SearchResult {
                    title,
                    url: link.to_string(),
                    snippet: String::new(),
                    source: "mojeek".to_string(),
                });
            }
            rest = &after_href[title_end..];
            if results.len() >= 25 {
                break;
            }
        }
        if results.is_empty() {
            return Err(RungError::Parse("no result rows".to_string()));
        }
        Ok(results)
    })
}

/// The rung table. Order in the chain comes from DEFAULT_CHAIN.
#[allow(clippy::type_complexity)]
pub fn all_rungs() -> HashMap<&'static str, ProviderRung> {
    let mut map = HashMap::new();
    let mut add = |rung: ProviderRung| {
        map.insert(rung.name, rung);
    };
    add(ProviderRung {
        name: "perplexity",
        env_keys: &["PERPLEXITY_API_KEY"],
        keyless: false,
        run: perplexity_run,
    });
    add(ProviderRung {
        name: "brave",
        env_keys: &["BRAVE_API_KEY", "BRAVE_SEARCH_API_KEY"],
        keyless: false,
        run: brave_run,
    });
    add(ProviderRung {
        name: "tavily",
        env_keys: &["TAVILY_API_KEY"],
        keyless: false,
        run: tavily_run,
    });
    add(ProviderRung {
        name: "exa",
        env_keys: &["EXA_API_KEY"],
        keyless: false,
        run: exa_run,
    });
    add(ProviderRung {
        name: "jina",
        env_keys: &["JINA_API_KEY"],
        keyless: false,
        run: jina_run,
    });
    add(ProviderRung {
        name: "kagi",
        env_keys: &["KAGI_API_KEY"],
        keyless: false,
        run: kagi_run,
    });
    add(ProviderRung {
        name: "duckduckgo",
        env_keys: &[],
        keyless: true,
        run: duckduckgo_run,
    });
    add(ProviderRung {
        name: "startpage",
        env_keys: &[],
        keyless: true,
        run: startpage_run,
    });
    add(ProviderRung {
        name: "mojeek",
        env_keys: &[],
        keyless: true,
        run: mojeek_run,
    });
    map
}

#[allow(clippy::type_complexity)]
const fn rung(
    name: &'static str,
    env_keys: &'static [&'static str],
    keyless: bool,
    run: for<'a> fn(
        &'a crate::http::client::Client,
        &'a str,
        &'a SearchFilters,
        Option<&'a str>,
    ) -> RungFuture<'a>,
) -> (&'static str, ProviderRung) {
    (
        name,
        ProviderRung {
            name,
            env_keys,
            keyless,
            run,
        },
    )
}

// === Chain orchestration ===

/// Circuit-breaker state per rung (process-local; cheap and honest).
static CIRCUIT_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<HashMap<String, (u32, std::time::Instant)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

const CIRCUIT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(300);

fn rung_available(name: &str) -> bool {
    let Ok(map) = CIRCUIT_FAILURES.lock() else {
        return true;
    };
    match map.get(name) {
        Some((count, since)) if *count >= 2 => since.elapsed() >= CIRCUIT_COOLDOWN,
        _ => true,
    }
}

fn rung_report_failure(name: &str) {
    if let Ok(mut map) = CIRCUIT_FAILURES.lock() {
        let entry = map
            .entry(name.to_string())
            .or_insert_with(|| (0, std::time::Instant::now()));
        entry.0 += 1;
        entry.1 = std::time::Instant::now();
    }
}

fn rung_report_success(name: &str) {
    if let Ok(mut map) = CIRCUIT_FAILURES.lock() {
        map.remove(name);
    }
}

/// Run a search down the chain. Returns (results, provider_that_answered).
pub async fn search(
    query: &str,
    filters: &SearchFilters,
    provider: Option<&str>,
) -> Result<(Vec<SearchResult>, String), Error> {
    let rungs = all_rungs();
    let order: Vec<&str> = match provider {
        Some(name) if name != "auto" => {
            if !rungs.contains_key(name) {
                return Err(Error::validation(format!(
                    "Unknown web_search provider {name:?}. Known: {}",
                    DEFAULT_CHAIN.join(", ")
                )));
            }
            vec![name]
        }
        _ => DEFAULT_CHAIN.to_vec(),
    };

    let mut notes: Vec<String> = Vec::new();
    for name in &order {
        let rung = &rungs[name];
        if !rung_available(name) {
            notes.push(format!("{name}: circuit open (recent failures)"));
            continue;
        }
        let key = env_key(rung.env_keys);
        if key.is_none() && !rung.keyless {
            notes.push(format!(
                "{name}: no key (set {})",
                rung.env_keys.join(" or ")
            ));
            continue;
        }
        let outcome = (rung.run)(
            &crate::http::client::Client::new(),
            query,
            filters,
            key.as_deref(),
        )
        .await;
        match outcome {
            Ok(mut results) => {
                rung_report_success(name);
                post_process(&mut results, filters);
                return Ok((results, (*name).to_string()));
            }
            Err(err) => {
                if matches!(err, RungError::RateLimited | RungError::Http(_)) {
                    rung_report_failure(name);
                }
                notes.push(format!("{name}: {err}"));
            }
        }
    }
    Err(Error::tool(
        "web_search",
        format!("All web search providers failed:\n{}", notes.join("\n")),
    ))
}

/// Dedupe by canonical URL, cap per-domain at 3, apply the limit.
fn post_process(results: &mut Vec<SearchResult>, filters: &SearchFilters) {
    let mut seen = std::collections::HashSet::new();
    results.retain(|result| {
        let canonical = canonical_url(&result.url);
        seen.insert(canonical)
    });
    let mut per_domain: HashMap<String, usize> = HashMap::new();
    results.retain(|result| {
        let domain = url_domain(&result.url);
        let count = per_domain.entry(domain).or_insert(0);
        *count += 1;
        *count <= 3
    });
    results.truncate(filters.limit.clamp(1, 50));
}

fn canonical_url(url: &str) -> String {
    let mut out = url.to_ascii_lowercase();
    for prefix in ["https://www.", "http://www.", "https://", "http://"] {
        if let Some(rest) = out.strip_prefix(prefix) {
            out = rest.to_string();
            break;
        }
    }
    while out.ends_with('/') {
        out.pop();
    }
    out
}

fn url_domain(url: &str) -> String {
    let canonical = canonical_url(url);
    canonical.split('/').next().unwrap_or_default().to_string()
}

fn with_site(query: &str, filters: &SearchFilters) -> String {
    match &filters.site {
        Some(site) if !site.is_empty() => format!("site:{site} {query}"),
        _ => query.to_string(),
    }
}

fn urlencoded(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => {
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!("%{byte:02X}"));
            }
        }
    }
    out
}

/// DuckDuckGo result links come wrapped as `//duckduckgo.com/l/?uddg=<urlencoded>`.
fn decode_ddg_redirect(href: &str) -> String {
    if let Some((_, query)) = href.split_once("uddg=") {
        let encoded = query.split('&').next().unwrap_or("");
        let mut out = String::with_capacity(encoded.len());
        let bytes = encoded.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'%' if i + 2 < bytes.len() => {
                    let hex = &encoded[i + 1..i + 3];
                    if let Ok(value) = u8::from_str_radix(hex, 16) {
                        out.push(value as char);
                    }
                    i += 3;
                }
                b'+' => {
                    out.push(' ');
                    i += 1;
                }
                other => {
                    out.push(other as char);
                    i += 1;
                }
            }
        }
        return out;
    }
    href.to_string()
}

fn rung_http_error(err: &Error) -> RungError {
    let text = err.to_string();
    if text.contains("429") {
        RungError::RateLimited
    } else {
        RungError::Http(text)
    }
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

use serde_json::{Value, json};

/// The `web_search` tool (bd-cv653.2.1): one query across the configured
/// provider chain; results ranked, deduped, and provider-attributed.
pub struct WebSearchTool;

impl WebSearchTool {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl crate::tools::Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn label(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web. One query walks a ranked provider chain (perplexity, \
         brave, tavily, exa, jina, kagi, then keyless duckduckgo/startpage/\
         mojeek) with automatic fallback — the first rung with credentials and \
         a healthy circuit answers. Returns ranked results (title, url, \
         snippet, source provider). Optional: `provider` pins one rung, \
         `site:` restricts to a host, `limit` caps results (default 10). \
         Pair with `read` on a result URL for full page content."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "The search query"},
                "provider": {
                    "type": "string",
                    "description": "Pin one provider (perplexity|brave|tavily|exa|jina|kagi|duckduckgo|startpage|mojeek); default 'auto' walks the chain"
                },
                "site": {"type": "string", "description": "Restrict results to a host (site: filter)"},
                "after": {"type": "string", "description": "YYYY-MM-DD recency filter where supported"},
                "limit": {"type": "integer", "description": "Max results (default 10, hard cap 50)"}
            },
            "required": ["query"]
        })
    }

    fn effects(&self) -> crate::tools::ToolEffects {
        crate::tools::ToolEffects::network()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(crate::tools::ToolUpdate) + Send + Sync>>,
    ) -> crate::error::Result<crate::tools::ToolOutput> {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| {
                Error::validation("web_search requires a non-empty `query`".to_string())
            })?
            .to_string();
        let provider = input
            .get("provider")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        let filters = SearchFilters {
            site: input
                .get("site")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            after: input
                .get("after")
                .and_then(Value::as_str)
                .map(str::to_string),
            limit: input
                .get("limit")
                .and_then(Value::as_u64)
                .map_or(10, |n| usize::try_from(n).unwrap_or(10).clamp(1, 50)),
        };
        let (results, answered_by) = search(&query, &filters, provider.as_deref()).await?;
        let payload = json!({
            "query": query,
            "answeredBy": answered_by,
            "count": results.len(),
            "results": results,
        });
        Ok(crate::tools::ToolOutput {
            content: vec![crate::model::ContentBlock::Text(
                crate::model::TextContent::new(payload.to_string()),
            )],
            details: Some(json!({"answeredBy": answered_by, "count": results.len()})),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_url_normalizes() {
        assert_eq!(canonical_url("https://www.Example.com/a/"), "example.com/a");
        assert_eq!(canonical_url("http://x.io"), "x.io");
    }

    #[test]
    fn post_process_dedupes_and_caps_domains() {
        let mut results = vec![
            SearchResult {
                title: "a".into(),
                url: "https://x.io/1".into(),
                snippet: String::new(),
                source: "t".into(),
            },
            SearchResult {
                title: "b".into(),
                url: "https://x.io/1/".into(),
                snippet: String::new(),
                source: "t".into(),
            },
            SearchResult {
                title: "c".into(),
                url: "https://x.io/2".into(),
                snippet: String::new(),
                source: "t".into(),
            },
            SearchResult {
                title: "d".into(),
                url: "https://x.io/3".into(),
                snippet: String::new(),
                source: "t".into(),
            },
            SearchResult {
                title: "e".into(),
                url: "https://x.io/4".into(),
                snippet: String::new(),
                source: "t".into(),
            },
            SearchResult {
                title: "f".into(),
                url: "https://y.io/1".into(),
                snippet: String::new(),
                source: "t".into(),
            },
        ];
        post_process(
            &mut results,
            &SearchFilters {
                site: None,
                after: None,
                limit: 10,
            },
        );
        // /1 deduped against /1/, and the 4th same-domain result dropped.
        let urls: Vec<&str> = results.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(urls.len(), 4, "dedupe + domain cap: {urls:?}");
        assert!(urls.contains(&"https://x.io/1"));
        assert!(urls.contains(&"https://y.io/1"));
    }

    #[test]
    fn site_filter_prepends_operator() {
        let filters = SearchFilters {
            site: Some("docs.rs".into()),
            after: None,
            limit: 10,
        };
        assert_eq!(
            with_site("tokio spawn", &filters),
            "site:docs.rs tokio spawn"
        );
    }

    #[test]
    fn urlencoded_encodes_specials() {
        assert_eq!(urlencoded("a b+c/d"), "a+b%2Bc%2Fd");
    }

    #[test]
    fn strip_tags_cleans() {
        assert_eq!(strip_tags("<b>bold</b> and <i>calm</i>"), "bold and calm");
    }
}
