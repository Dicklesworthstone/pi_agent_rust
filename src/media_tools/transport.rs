//! Shared transport for native media adapters. No retries, redirects, or mock fallback.
//! An explicit owner covers credential helpers, HTTP, and response-body consumption.

use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::http::client::Client;
use futures::future::{Either, select};
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use url::Url;

#[derive(Default)]
pub(super) struct Transport {
    client: Client,
    base_url: Option<String>,
    auth: super::MediaAuthScope,
    cost_scope: crate::cost::CostScope,
}

impl Transport {
    pub(super) fn bind_auth_scope(&mut self, scope: super::MediaAuthScope) {
        self.auth = scope;
    }

    pub(super) fn bind_cost_scope(&mut self, scope: crate::cost::CostScope) {
        self.cost_scope = scope;
    }

    pub(super) fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }

    pub(super) fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = Some(base_url);
        self
    }

    /// Refuse native work before output preflight touches the filesystem.
    /// `api` still captures and checks the invocation account before helpers;
    /// this early check must not replace either request admission boundary.
    pub(super) fn check_admission(&self, tool: &str) -> Result<()> {
        check_owner(tool, &AgentCx::for_current_or_request())?;
        if let Some(budget) = self.cost_scope.current() {
            budget.admit()?;
        }
        Ok(())
    }

    pub(super) async fn api(
        &self,
        tool: &'static str,
        provider: &'static str,
        explicit_key: Option<&str>,
        timeout: Duration,
    ) -> Result<Api<'_>> {
        let started = Instant::now();
        let owner = AgentCx::for_current_or_request();
        check_owner(tool, &owner)?;
        // Capture the invocation's account before resolving a command-backed
        // credential. A later session switch must not move this call's authority.
        let cost_budget = self.cost_scope.current();
        if let Some(budget) = &cost_budget {
            budget.admit()?;
        }
        let model_costs = if cost_budget.is_some() {
            self.auth
                .model_costs()
                .map_err(|message| Error::tool(tool, message))?
        } else {
            std::sync::Arc::default()
        };
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
            auth.resolve_media_api_key_status_async(
                provider,
                &owner,
                remaining_timeout(tool, started, timeout)?,
            )
            .await?
            .into_result(provider)
            .map_err(|error| Error::tool(tool, error.to_string()))?
            .ok_or_else(|| missing_key(tool, provider))?
        } else {
            resolve_key(tool, provider, None, |name| std::env::var(name).ok())?
        };
        check_owner(tool, &owner)?;
        remaining_timeout(tool, started, timeout)?;
        Ok(Api {
            transport: self,
            tool,
            provider,
            key,
            track_rate_limits,
            cost_budget,
            model_costs,
            base,
            owner,
            started,
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
    cost_budget: Option<crate::cost::CostBudget>,
    model_costs: std::sync::Arc<super::MediaModelCosts>,
    base: Url,
    pub(super) owner: AgentCx,
    started: Instant,
    timeout: Duration,
}

pub(super) struct Response {
    pub(super) bytes: Vec<u8>,
    pub(super) content_type: String,
    parsed_json: Option<Value>,
}

#[derive(Clone, Copy)]
enum Billing<'a> {
    Vision(&'a str),
    Image(&'a str),
    Speech(&'a str),
}

/// Inspect only billing envelopes before `Value` can overwrite repeated keys.
/// Inside an envelope, arrays and nested objects retain the same uniqueness
/// rule. Other response content is skipped without changing its interpretation.
struct BillingKeys {
    envelope: bool,
}

impl<'de> DeserializeSeed<'de> for BillingKeys {
    type Value = bool;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<bool, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for BillingKeys {
    type Value = bool;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("native media billing metadata")
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<bool, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut names = BTreeSet::new();
        let mut duplicate = false;
        while let Some(name) = map.next_key::<String>()? {
            if !self.envelope || matches!(name.as_str(), "usage" | "usageMetadata") {
                duplicate |= !names.insert(name);
                duplicate |= map.next_value_seed(Self { envelope: false })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(duplicate)
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<bool, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut duplicate = false;
        if self.envelope {
            // A top-level array has no native billing envelope. Its ordinary
            // response validation remains responsible for that shape.
            while sequence.next_element::<IgnoredAny>()?.is_some() {}
        } else {
            while let Some(nested) = sequence.next_element_seed(Self { envelope: false })? {
                duplicate |= nested;
            }
        }
        Ok(duplicate)
    }

    fn visit_bool<E>(self, _value: bool) -> std::result::Result<bool, E> {
        Ok(false)
    }

    fn visit_i64<E>(self, _value: i64) -> std::result::Result<bool, E> {
        Ok(false)
    }

    fn visit_u64<E>(self, _value: u64) -> std::result::Result<bool, E> {
        Ok(false)
    }

    fn visit_f64<E>(self, _value: f64) -> std::result::Result<bool, E> {
        Ok(false)
    }

    fn visit_str<E>(self, _value: &str) -> std::result::Result<bool, E> {
        Ok(false)
    }

    fn visit_unit<E>(self) -> std::result::Result<bool, E> {
        Ok(false)
    }
}

fn reject_duplicate_billing(
    bytes: &[u8],
    attempt: Option<&mut crate::cost::CostAttempt>,
) -> Result<()> {
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    if let Ok(true) = (BillingKeys { envelope: true }).deserialize(&mut decoder)
        && decoder.end().is_ok()
    {
        // Do not select either conflicting bill or replace previously known
        // spend. The attempt's normal cleanup retains its valid observations.
        if let Some(attempt) = attempt {
            let _ = attempt.observe(f64::NAN);
        }
        return Err(Error::session(
            "native media response has duplicate billing fields",
        ));
    }
    // Syntax errors and invalid billing types retain their existing response
    // and provider-specific validation paths; this guard only adds uniqueness.
    Ok(())
}

impl Response {
    pub(super) fn json(self, tool: &str) -> Result<Value> {
        if !self.content_type.is_empty()
            && self.content_type != "application/json"
            && !self.content_type.ends_with("+json")
        {
            return Err(Error::tool(tool, "provider returned a non-JSON response"));
        }
        self.parsed_json.map_or_else(
            || {
                serde_json::from_slice(&self.bytes)
                    .map_err(|_| Error::tool(tool, "provider returned invalid JSON"))
            },
            Ok,
        )
    }
}

impl Api<'_> {
    pub(super) fn scrub(&self, message: &str, limit: usize) -> String {
        crate::auth::redact_known_secrets_bounded(message, &[self.key.as_str()], limit)
            .chars()
            .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
            .collect()
    }

    pub(super) async fn post(&self, path: &str, payload: &Value, limit: usize) -> Result<Response> {
        self.post_inner(path, payload, limit, None).await
    }

    /// Vision responses use the same token rates as ordinary model requests.
    /// Settle billing before interpreting the answer, so refusals, malformed
    /// content and failed artifact processing cannot erase a provider charge.
    pub(super) async fn post_vision(
        &self,
        model: &str,
        path: &str,
        payload: &Value,
        limit: usize,
    ) -> Result<Value> {
        self.post_inner(path, payload, limit, Some(Billing::Vision(model)))
            .await?
            .json(self.tool)
    }

    /// Retain the invocation's account through image generation and editing.
    /// Their image-token tariffs are separate from ordinary vision/text rates.
    pub(super) async fn post_image(
        &self,
        model: &str,
        path: &str,
        payload: &Value,
        limit: usize,
    ) -> Result<Value> {
        self.post_inner(path, payload, limit, Some(Billing::Image(model)))
            .await?
            .json(self.tool)
    }

    /// Speech SSE carries billing in its terminal audio event. Capture that
    /// receipt before codec validation or publication can reject the output.
    pub(super) async fn post_speech(
        &self,
        model: &str,
        path: &str,
        payload: &Value,
        limit: usize,
    ) -> Result<Response> {
        self.post_inner(path, payload, limit, Some(Billing::Speech(model)))
            .await
    }

    fn vision_cost(&self, model: &str, response: &Value) -> Result<Option<f64>> {
        let Some(mut usage) = super::vision::response_usage(self.provider, response)? else {
            return Ok(None);
        };
        let rates = self
            .model_costs
            .get(self.provider)
            .and_then(|models| models.get(&model.to_ascii_lowercase()))
            .filter(|rates| rates.is_priced());
        if let Some(rates) = rates {
            rates.price_usage(&mut usage)?;
        } else if matches!(
            usage.billing,
            Some(crate::model::BillingUsage::ReportedTotal { .. })
        ) {
            crate::provider::ModelCost::default().price_usage(&mut usage)?;
        } else {
            return Ok(None);
        }
        Ok(Some(usage.cost.total))
    }

    fn settle_usage(
        &self,
        billing: Billing<'_>,
        response: &Value,
        attempt: &mut crate::cost::CostAttempt,
    ) -> Result<()> {
        let cost = match billing {
            Billing::Vision(model) => self.vision_cost(model, response),
            Billing::Image(model) => {
                super::generation::billing::response_cost(self.provider, model, response)
            }
            Billing::Speech(model) => super::speech::response_cost(model, response),
        };
        match cost {
            Ok(Some(cost)) => attempt.finish(cost),
            // No observed charge can be invented from content or file size,
            // but finalizing this attempt can itself fail (for example when a
            // remote parent disconnects). Propagate that accounting failure.
            Ok(None) => attempt.finish(0.0),
            Err(error) => {
                // Invalid evidence permanently fences further admissions but
                // cannot prevent already-owned sibling attempts from settling.
                let _ = attempt.observe(f64::NAN);
                Err(error)
            }
        }
    }

    // Keep the bounded HTTP exchange inside its shared cancellation/deadline race.
    #[allow(clippy::too_many_lines)]
    async fn post_inner(
        &self,
        path: &str,
        payload: &Value,
        limit: usize,
        billing: Option<Billing<'_>>,
    ) -> Result<Response> {
        check_owner(self.tool, &self.owner)?;
        remaining_timeout(self.tool, self.started, self.timeout)?;
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
            let client = self.owner.http().bind(&self.transport.client);
            let request = client
                .post(url.as_str())
                .timeout(remaining_timeout(self.tool, self.started, self.timeout)?)
                .json(payload)?;
            let request = match self.provider {
                "gemini" => request.try_header("x-goog-api-key", &self.key),
                "anthropic" => request
                    .try_header("x-api-key", &self.key)
                    .and_then(|request| request.try_header("anthropic-version", "2023-06-01")),
                _ => request.try_header("Authorization", format!("Bearer {}", self.key)),
            }
            .map_err(|_| Error::tool(self.tool, "invalid media authentication header"))?;
            // Sibling work may exhaust or quarantine the captured account
            // while credentials and input payloads are being prepared.
            remaining_timeout(self.tool, self.started, self.timeout)?;
            let mut attempt = match (&self.cost_budget, billing) {
                (Some(budget), Some(_)) => Some(budget.start_attempt()?),
                (Some(budget), None) => {
                    budget.admit()?;
                    None
                }
                _ => None,
            };
            let started_at = std::time::Instant::now();
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
                let detail = match response.bytes_limited(16 * 1024).await {
                    Ok(body) => {
                        if let (Some(billing), Some(attempt)) = (billing, attempt.as_mut()) {
                            reject_duplicate_billing(&body, Some(&mut *attempt))?;
                            if let Ok(json) = serde_json::from_slice::<Value>(&body) {
                                self.settle_usage(billing, &json, attempt)?;
                            }
                        }
                        self.scrub(&String::from_utf8_lossy(&body), 2048)
                    }
                    Err(_) => "error body unavailable or too large".to_string(),
                };
                return Err(Error::tool(
                    self.tool,
                    format!("{} HTTP {status}: {detail}", self.provider),
                ));
            }
            let mut content_type = response
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
            let (bytes, parsed_json) = if let Some(billing @ Billing::Speech(model)) = billing {
                if content_type != "text/event-stream" {
                    return Err(Error::tool(
                        self.tool,
                        "speech provider did not return the requested SSE stream",
                    ));
                }
                let bytes = super::speech::read_stream(
                    response.bytes_stream(),
                    limit,
                    |raw, event| {
                        reject_duplicate_billing(raw, attempt.as_mut())?;
                        if let Some(attempt) = attempt.as_mut() {
                            self.settle_usage(billing, event, attempt)?;
                        } else {
                            super::speech::response_cost(model, event)?;
                        }
                        Ok(())
                    },
                )
                .await?;
                // The wrapper is SSE; the adapter validates the requested
                // audio codec against the reconstructed bytes themselves.
                content_type.clear();
                (bytes, None)
            } else {
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
                let parsed_json =
                    if let (Some(billing), Some(attempt)) = (billing, attempt.as_mut()) {
                        reject_duplicate_billing(&bytes, Some(&mut *attempt))?;
                        let json = serde_json::from_slice::<Value>(&bytes).map_err(|_| {
                            Error::tool(self.tool, "provider returned invalid JSON")
                        })?;
                        self.settle_usage(billing, &json, attempt)?;
                        Some(json)
                    } else {
                        None
                    };
                (bytes, parsed_json)
            };
            check_owner(self.tool, &self.owner)?;
            if self.started.elapsed() >= self.timeout {
                return Err(Error::tool(
                    self.tool,
                    "media request timed out; the provider may already have processed it",
                ));
            }
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
                parsed_json,
            })
        };
        let cancelled = async {
            let (sender, mut receiver) = asupersync::channel::oneshot::channel::<()>();
            let _ = receiver.recv(self.owner.cx()).await;
            drop(sender);
        };
        let watchdog = async {
            match select(
                Box::pin(
                    self.owner
                        .time()
                        .sleep(self.timeout.saturating_sub(self.started.elapsed())),
                ),
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

fn remaining_timeout(tool: &str, started: Instant, timeout: Duration) -> Result<Duration> {
    timeout
        .checked_sub(started.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| {
            Error::tool(
                tool,
                "media operation timed out before dispatch; no provider request was sent",
            )
        })
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

/// Resolve before probing metadata or credentials. Media has the same read
/// authority as the native read tool, including the agent resource directory.
pub(super) fn resolve_input(
    cwd: &Path,
    path: &Path,
    workspace: &crate::workspace::WorkspaceHandle,
    tool: &str,
) -> Result<PathBuf> {
    crate::tools::enforce_read_scope_with_roots(
        &cwd.join(path),
        cwd,
        &crate::config::Config::global_dir(),
        workspace,
    )
    .map_err(|error| Error::tool(tool, error.to_string()))
}

pub(super) fn read_capped(
    path: &Path,
    cwd: &Path,
    workspace: &crate::workspace::WorkspaceHandle,
    tool: &str,
    limit: u64,
) -> Result<Vec<u8>> {
    let path = resolve_input(cwd, path, workspace, tool)?;
    let initial = std::fs::metadata(&path)
        .map_err(|error| Error::tool(tool, format!("cannot stat media file: {error}")))?;
    if !initial.is_file() || initial.len() > limit {
        return Err(Error::tool(
            tool,
            format!("media input must be a regular file of at most {limit} bytes"),
        ));
    }
    // Re-snapshot live roots at the actual open. The native reader validates
    // the opened descriptor, rejects symlink/reparse swaps and FIFOs, and caps
    // the bytes even if an admitted file grows after the advisory stat above.
    let mut roots = workspace.snapshot_or(cwd).all();
    roots.push(crate::config::Config::global_dir());
    let bytes = crate::tools::read_file_capped_within_roots_sync(&path, &roots, limit)
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

    fn billed_wire_exchange(
        provider: &'static str,
        billing: Billing<'_>,
        status: u16,
        body: &str,
    ) -> (Result<Value>, crate::cost::CostBudget) {
        let (endpoint, worker) = peer(status, "application/json", body.as_bytes().to_vec());
        let budget = crate::cost::CostBudget::new(None, Some(10.0), 0.5).unwrap();
        let scope = crate::cost::CostScope::default();
        scope.set(Some(budget.clone()));
        let mut transport = Transport::default().with_base_url(endpoint);
        transport.bind_cost_scope(scope);
        // An already-owned sibling must retain acknowledged partial spending
        // even when the new wire response quarantines the shared account.
        let mut sibling = budget.start_attempt().unwrap();
        sibling.observe(0.25).unwrap();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let result = runtime.block_on(async {
            let name = match billing {
                Billing::Image(_) => "generate_image",
                Billing::Vision(_) => "inspect_image",
            };
            let api = transport
                .api(
                    name,
                    provider,
                    Some("billing-wire-key"),
                    Duration::from_secs(2),
                )
                .await?;
            match billing {
                Billing::Image(model) => {
                    api.post_image(model, "images/generations", &serde_json::json!({}), 4096)
                        .await
                }
                Billing::Vision(model) => {
                    api.post_vision(model, "chat/completions", &serde_json::json!({}), 4096)
                        .await
                }
            }
        });
        worker.join().unwrap();
        drop(sibling);
        (result, budget)
    }

    #[test]
    #[allow(clippy::float_cmp)] // Exact quarter-dollar fixtures retain prior spending.
    fn duplicate_billing_envelopes_quarantine_image_and_vision_accounts() {
        for (provider, billing, status, wire) in [
            (
                "openai",
                Billing::Image("gpt-image-1.5"),
                200,
                r#"{"usage":{"cost":1.25},"usage":{"cost":0},"data":[]}"#,
            ),
            (
                "openai",
                Billing::Vision("gpt-4.1-mini"),
                429,
                r#"{"error":{"message":"limited"},"usage":null,"usage":{"cost":0}}"#,
            ),
            (
                "gemini",
                Billing::Image("gemini-3.1-flash-image"),
                200,
                r#"{"usageMetadata":{"promptTokenCount":1,"totalTokenCount":1},
                    "usageMetadata":{"promptTokenCount":0,"totalTokenCount":0}}"#,
            ),
        ] {
            let (result, budget) = billed_wire_exchange(provider, billing, status, wire);
            assert!(matches!(result, Err(Error::Session(message))
                if message.contains("duplicate billing fields")));
            assert_eq!(budget.spent(), 0.75);
            assert_eq!(budget.snapshot().pending, 0.0);
            assert!(matches!(budget.admit(), Err(Error::Session(_))));
        }
    }

    #[test]
    #[allow(clippy::float_cmp)] // Exact quarter-dollar fixtures retain prior spending.
    fn duplicate_nested_billing_fields_cannot_hide_positive_bills() {
        for (provider, billing, status, wire) in [
            (
                "xai",
                Billing::Image("grok-imagine-image-2.0"),
                200,
                r#"{"usage":{"cost_in_usd_ticks":12500000000,"cost_in_usd_ticks":0}}"#,
            ),
            (
                "openai",
                Billing::Vision("gpt-4.1-mini"),
                200,
                r#"{"usage":{"cost":1.25,"co\u0073t":0}}"#,
            ),
            (
                "openai",
                Billing::Image("gpt-image-1.5"),
                429,
                r#"{"usage":{"input_tokens":0,"output_tokens":0,
                    "input_tokens_details":{"text_tokens":2,"text_tokens":0,"image_tokens":0}}}"#,
            ),
            (
                "gemini",
                Billing::Image("gemini-3.1-flash-image"),
                200,
                r#"{"usageMetadata":{"promptTokenCount":0,"candidatesTokenCount":0,
                    "totalTokenCount":0,"candidatesTokensDetails":[
                        {"modality":"IMAGE","tokenCount":1120,"tokenCount":0}]}}"#,
            ),
        ] {
            let (result, budget) = billed_wire_exchange(provider, billing, status, wire);
            assert!(matches!(result, Err(Error::Session(message))
                if message.contains("duplicate billing fields")));
            assert_eq!(budget.spent(), 0.75);
            assert_eq!(budget.snapshot().pending, 0.0);
            assert!(matches!(budget.admit(), Err(Error::Session(_))));
        }
    }

    #[test]
    #[allow(clippy::float_cmp)] // Exact quarter-dollar fixtures verify the known bill.
    fn duplicate_content_fields_do_not_quarantine_media_billing() {
        for billing in [
            Billing::Image("gpt-image-1.5"),
            Billing::Vision("gpt-4.1-mini"),
        ] {
            let wire = r#"{"usage":{"cost":1.25},"data":[{"label":"old","label":"new"}],
                "metadata":{"usage":{"cost":2,"cost":0}},"extra":1,"extra":2}"#;
            let (result, budget) = billed_wire_exchange("openai", billing, 200, wire);
            let response = result.expect("content duplicates must retain normal JSON behavior");
            assert_eq!(response["data"][0]["label"], "new");
            assert_eq!(budget.spent(), 2.0);
            assert_eq!(budget.snapshot().pending, 0.0);
            assert!(budget.admit().is_ok());
        }
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
    #[allow(clippy::too_many_lines)]
    fn native_media_tools_obey_the_live_registry_spend_scope() {
        use crate::cost::CostBudget;
        use crate::media_tools::{GenerateImageTool, InspectImageTool, TtsTool};
        use crate::tools::{Tool, ToolRegistry};
        use base64::Engine as _;

        for name in ["inspect_image", "generate_image", "tts"] {
            let directory = tempfile::tempdir().expect("media workspace");
            let png = super::super::MIN_VALID_PNG;
            std::fs::write(directory.path().join("input.png"), png).unwrap();
            let (mime, response, arguments) = match name {
                "inspect_image" => (
                    "application/json",
                    serde_json::to_vec(&serde_json::json!({"choices":[{
                        "finish_reason":"stop", "message":{"content":"A fixture image."}
                    }]}))
                    .unwrap(),
                    serde_json::json!({"path":"input.png", "provider":"openai",
                        "model":"gpt-4.1-mini", "timeout_ms":2000}),
                ),
                "generate_image" => (
                    "application/json",
                    serde_json::to_vec(&serde_json::json!({"data":[{
                        "b64_json":base64::engine::general_purpose::STANDARD.encode(png)
                    }]}))
                    .unwrap(),
                    serde_json::json!({"prompt":"Draw a square", "provider":"openai",
                        "model":"gpt-image-1.5", "output_path":"output.png", "timeout_ms":2000}),
                ),
                _ => {
                    let mut audio = super::super::MIN_VALID_WAV.to_vec();
                    audio[4..8].copy_from_slice(&40_u32.to_le_bytes());
                    audio[40..44].copy_from_slice(&4_u32.to_le_bytes());
                    audio.extend_from_slice(&[0x34, 0x12, 0xcd, 0xab]);
                    (
                        "audio/wav",
                        audio,
                        serde_json::json!({"text":"Hello", "provider":"openai", "model":"tts-1",
                            "voice":"alloy", "output_path":"output.wav", "timeout_ms":2000}),
                    )
                }
            };
            let (endpoint, worker) = peer(200, mime, response);
            let native: Box<dyn Tool> = match name {
                "inspect_image" => Box::new(
                    InspectImageTool::new(directory.path())
                        .with_mock(false)
                        .with_api_key(Some("media-cost-key".to_string()))
                        .with_base_url(endpoint),
                ),
                "generate_image" => Box::new(
                    GenerateImageTool::new(directory.path())
                        .with_mock(false)
                        .with_api_key(Some("media-cost-key".to_string()))
                        .with_base_url(endpoint),
                ),
                _ => Box::new(
                    TtsTool::new(directory.path())
                        .with_mock(false)
                        .with_api_key(Some("media-cost-key".to_string()))
                        .with_base_url(endpoint),
                ),
            };
            let registry = ToolRegistry::from_tools(vec![native]);
            let tool = registry.get(name).expect("native adapter");
            let scope = registry.cost_scope();
            scope.set(Some(CostBudget::new(None, Some(1.0), 1.0).unwrap()));
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let refused = runtime.block_on(tool.execute("refused", arguments.clone(), None));
            assert!(
                matches!(refused, Err(Error::CostCap { .. })),
                "{name}: {refused:?}"
            );

            let available = CostBudget::new(None, Some(1.0), 0.0).unwrap();
            scope.set(Some(available.clone()));
            let output = runtime
                .block_on(tool.execute("admitted", arguments.clone(), None))
                .expect("the retained native tool must observe the replacement scope");
            assert!(!output.is_error);
            assert!(
                available.spent().abs() < f64::EPSILON,
                "media responses must not invent USD charges"
            );
            available.record(1.0).unwrap();
            let refused = runtime.block_on(tool.execute("spent", arguments, None));
            assert!(
                matches!(refused, Err(Error::CostCap { .. })),
                "{name}: {refused:?}"
            );
            let captured = worker.join().expect("one admitted native request");
            assert!(captured.headers.contains("Bearer media-cost-key"));
        }
    }

    #[test]
    fn media_request_rechecks_its_captured_account_without_migrating_to_a_new_scope() {
        use crate::cost::{CostBudget, CostScope};

        for quarantine in [false, true] {
            let original = CostBudget::new(None, Some(1.0), 0.0).unwrap();
            let replacement = CostBudget::new(None, Some(1.0), 0.0).unwrap();
            let scope = CostScope::default();
            scope.set(Some(original.clone()));
            let (endpoint, listener) = peer_listener();
            let mut transport = Transport::default().with_base_url(endpoint);
            transport.bind_cost_scope(scope.clone());
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let api = transport
                    .api(
                        "tts",
                        "openai",
                        Some("captured-key"),
                        Duration::from_secs(2),
                    )
                    .await
                    .expect("initial account permits preparation");
                scope.set(Some(replacement));
                if quarantine {
                    assert!(original.record(f64::NAN).is_err());
                } else {
                    original.record(1.0).unwrap();
                }
                let error = api
                    .post("audio/speech", &serde_json::json!({}), 1024)
                    .await
                    .err()
                    .expect("captured account must be checked immediately before HTTP");
                if quarantine {
                    assert!(matches!(error, Error::Session(ref message)
                        if message.contains("model cost accounting unavailable")));
                } else {
                    assert!(matches!(error, Error::CostCap { .. }));
                }
                assert!(matches!(listener.accept(), Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock));

                let api = transport
                    .api(
                        "tts",
                        "openai",
                        Some("replacement-key"),
                        Duration::from_secs(2),
                    )
                    .await
                    .expect("next invocation captures the healthy replacement account");
                scope.set(Some(CostBudget::new(None, Some(1.0), 1.0).unwrap()));
                let worker =
                    thread::spawn(move || exchange(&listener, 200, "application/json", b"{}"));
                let response = api
                    .post("audio/speech", &serde_json::json!({}), 1024)
                    .await
                    .expect("a later scope change must not migrate an admitted invocation");
                assert_eq!(response.bytes, b"{}");
                let captured = worker.join().unwrap();
                assert!(captured.headers.contains("Bearer replacement-key"));
                assert!(matches!(
                    transport
                        .api("tts", "openai", Some("next-key"), Duration::from_secs(2))
                        .await,
                    Err(Error::CostCap { .. })
                ));
            });
        }
    }

    #[cfg(unix)]
    #[test]
    fn exhausted_and_quarantined_media_budgets_refuse_before_stored_key_commands() {
        use crate::auth::{AuthCredential, AuthStorage};
        use crate::cost::{CostBudget, CostScope};

        let directory = tempfile::tempdir().expect("command workspace");
        let mut auth = AuthStorage::empty_at(directory.path().join("auth.json"));
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "$CMD:printf invoked > credential-invoked; printf command-key".to_string(),
            },
        );
        auth.set_command_working_directory(directory.path())
            .unwrap();
        let auth_scope = super::super::MediaAuthScope::default();
        auth_scope.set(&std::sync::Arc::new(()), auth);
        let scope = CostScope::default();
        let mut transport =
            Transport::default().with_base_url("http://127.0.0.1:1/v1/".to_string());
        transport.bind_auth_scope(auth_scope);
        transport.bind_cost_scope(scope.clone());
        let owner = AgentCx::for_testing_with_io();
        futures::executor::block_on(owner.with_current(async {
            for quarantine in [false, true] {
                let budget = CostBudget::new(None, Some(1.0), 1.0).unwrap();
                if quarantine {
                    assert!(budget.record(f64::NAN).is_err());
                }
                scope.set(Some(budget));
                let error = transport
                    .api("tts", "openai", None, Duration::from_secs(1))
                    .await
                    .err()
                    .expect("refuse before stored credential resolution");
                if quarantine {
                    assert!(matches!(error, Error::Session(ref message)
                        if message.contains("model cost accounting unavailable")));
                } else {
                    assert!(matches!(error, Error::CostCap { .. }));
                }
                assert!(!directory.path().join("credential-invoked").exists());
            }
        }));
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
        let workspace = crate::workspace::WorkspaceHandle::single(dir.path());
        let read =
            |path: &Path, limit| read_capped(path, dir.path(), &workspace, "inspect_image", limit);
        assert!(read(dir.path(), 4).is_err());
        let path = dir.path().join("image.png");
        std::fs::write(&path, []).unwrap();
        assert!(read(&path, 4).is_err());
        std::fs::write(&path, b"12345").unwrap();
        assert!(read(&path, 4).is_err());
        assert_eq!(read(&path, 5).unwrap(), b"12345");
    }

    #[test]
    fn media_reads_follow_live_workspace_additions_and_revocations() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        let extra = parent.path().join("extra");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&extra).unwrap();
        let outside = extra.join("source.png");
        std::fs::write(&outside, b"pixels").unwrap();
        let mut workspace = crate::workspace::WorkspaceHandle::single(&root);
        let bound = workspace.clone();
        for path in [outside.as_path(), Path::new("../extra/source.png")] {
            let error = read_capped(path, &root, &bound, "generate_image", 6).unwrap_err();
            assert!(error.to_string().contains("Cannot read outside"), "{error}");
        }
        workspace.add_root(&extra);
        assert_eq!(
            read_capped(&outside, &root, &bound, "generate_image", 6).unwrap(),
            b"pixels"
        );
        assert!(workspace.remove_root(&extra));
        assert!(read_capped(&outside, &root, &bound, "generate_image", 6).is_err());

        #[cfg(unix)]
        {
            let linked = root.join("linked.png");
            std::os::unix::fs::symlink(&outside, &linked).unwrap();
            assert!(read_capped(&linked, &root, &bound, "generate_image", 6).is_err());
            std::fs::write(root.join("local.png"), b"local").unwrap();
            let local_link = root.join("local-link.png");
            std::os::unix::fs::symlink(root.join("local.png"), &local_link).unwrap();
            assert_eq!(
                read_capped(&local_link, &root, &bound, "generate_image", 6).unwrap(),
                b"local"
            );
        }
    }
}
