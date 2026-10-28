//! LLM provider abstraction layer.
//!
//! This module defines the [`Provider`] trait and shared request/response types used by all
//! backends (Anthropic/OpenAI/Gemini/etc).
//!
//! Providers are responsible for:
//! - Translating [`crate::model::Message`] history into provider-specific HTTP requests.
//! - Emitting [`StreamEvent`] values as SSE/HTTP chunks arrive.
//! - Advertising tool schemas to the model (so it can call [`crate::tools`] by name).

pub use crate::model::StreamEvent;
use crate::model::{Message, ThinkingLevel};
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

// ============================================================================
// Provider Trait
// ============================================================================

/// An LLM backend capable of streaming assistant output (and tool calls).
///
/// A `Provider` is typically configured for a specific API + model and is used by the agent loop
/// to produce a stream of [`StreamEvent`] updates.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Get the provider name.
    fn name(&self) -> &str;

    /// Get the API type.
    fn api(&self) -> &str;

    /// Get the model identifier used by this provider.
    fn model_id(&self) -> &str;

    /// Catalog pricing for the model this provider serves, when known
    /// (gh #221). The agent prices `usage` on every finished assistant
    /// message with these rates; `None` leaves any provider-reported cost
    /// (e.g. OpenRouter's `usage.cost`) as the only source.
    fn model_cost(&self) -> Option<ModelCost> {
        None
    }

    /// The session-tree budget guarding this provider, if one is attached.
    /// Auxiliary requests inherit this handle instead of starting a fresh cap.
    fn cost_budget(&self) -> Option<&crate::cost::CostBudget> {
        None
    }

    /// Remove only the outer spend guard when rebinding a provider to a new
    /// session. Transport, pricing, and authentication wrappers stay intact.
    #[doc(hidden)]
    fn unguarded_provider(&self) -> Option<Arc<dyn Provider>> {
        None
    }

    /// Start streaming a completion.
    ///
    /// Implementations should yield [`StreamEvent`] items as soon as they are decoded, and should
    /// stop promptly when the request is cancelled.
    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> crate::error::Result<Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>>;

    /// Run the provider's *native* server-side compaction over a fully-built
    /// request body and return the raw response JSON (gh #167).
    ///
    /// Only APIs with a real server-side compact endpoint implement this
    /// (currently `openai-responses`, which POSTs the body to the sibling
    /// `…/responses/compact` route). Credentials are resolved host-side from
    /// `options` exactly like [`Provider::stream`]; callers must sanitize
    /// `request_body` before handing it in (see the extension compaction
    /// bridge in `agent.rs`). The default implementation reports the API as
    /// unsupported so callers fail open to summary compaction.
    async fn compact_native(
        &self,
        _request_body: &serde_json::Value,
        _options: &StreamOptions,
    ) -> crate::error::Result<serde_json::Value> {
        Err(crate::error::Error::provider(
            self.name().to_string(),
            format!(
                "provider {} does not implement native compaction (api: {})",
                self.name(),
                self.api()
            ),
        ))
    }
}

/// Apply the session's spending limit at the actual provider boundary. A
/// replacement guard never nests with an old session's guard, so model and
/// session changes cannot charge the same response twice.
pub fn guard_with_cost_budget(
    provider: Arc<dyn Provider>,
    budget: crate::cost::CostBudget,
) -> Arc<dyn Provider> {
    let inner = provider.unguarded_provider().unwrap_or(provider);
    Arc::new(CostGuardedProvider { inner, budget })
}

struct CostGuardedProvider {
    inner: Arc<dyn Provider>,
    budget: crate::cost::CostBudget,
}

#[async_trait]
impl Provider for CostGuardedProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn api(&self) -> &str {
        self.inner.api()
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn model_cost(&self) -> Option<ModelCost> {
        self.inner.model_cost()
    }

    fn cost_budget(&self) -> Option<&crate::cost::CostBudget> {
        Some(&self.budget)
    }

    fn unguarded_provider(&self) -> Option<Arc<dyn Provider>> {
        Some(Arc::clone(&self.inner))
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> crate::error::Result<Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>>
    {
        let attempt = self.budget.start_attempt()?;
        let accounting = CostAccountingOwner::new(CostAccounting {
            attempt: Some(attempt),
            rates: self.inner.model_cost(),
            known_cost: 0.0,
            settled: false,
            failure: None,
        });
        let mut observed_options = options.clone();
        observed_options.usage_observer = Some(accounting.observer());
        CostMeteredSetup {
            inner: Some(self.inner.stream(context, &observed_options)),
            accounting: Some(accounting),
        }
        .await
    }

    async fn compact_native(
        &self,
        request_body: &serde_json::Value,
        options: &StreamOptions,
    ) -> crate::error::Result<serde_json::Value> {
        let mut attempt = self.budget.start_attempt()?;
        let response = self.inner.compact_native(request_body, options).await?;
        if let Some(usage) = response.get("usage") {
            let input = usage
                .get("input_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let cached = usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
                .min(input);
            let mut priced = crate::model::Usage {
                input: input.saturating_sub(cached),
                output: usage
                    .get("output_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                cache_read: cached,
                ..Default::default()
            };
            priced.cost.total = usage
                .get("cost")
                .map_or(0.0, |cost| cost.as_f64().unwrap_or(f64::NAN));
            // Invalid reported billing must quarantine the account, not be
            // replaced with a plausible catalog estimate.
            if priced.cost.total.is_finite()
                && priced.cost.total >= 0.0
                && let Some(rates) = self.inner.model_cost()
            {
                rates.price_usage(&mut priced);
            }
            if let Err(error) = attempt.finish(priced.cost.total) {
                tracing::warn!(%error, "native compaction spend accounting failed; budget quarantined");
            }
        }
        Ok(response)
    }
}

/// A provider can publish acknowledged cumulative usage without adding a
/// public stream event. Only token/cost data crosses this boundary. The
/// request owner closes the observer before returning or being dropped;
/// retained clones cannot charge a later request or reopen settled billing.
#[derive(Clone)]
pub struct UsageObserver {
    accounting: Arc<std::sync::Mutex<CostAccounting>>,
}

impl UsageObserver {
    pub fn observe(&self, usage: &crate::model::Usage) -> crate::error::Result<()> {
        self.accounting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .observe(&mut usage.clone(), false)
    }
}

impl std::fmt::Debug for UsageObserver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UsageObserver")
    }
}

struct CostAccounting {
    attempt: Option<crate::cost::CostAttempt>,
    rates: Option<ModelCost>,
    known_cost: f64,
    settled: bool,
    failure: Option<String>,
}

impl CostAccounting {
    fn observe(
        &mut self,
        usage: &mut crate::model::Usage,
        terminal: bool,
    ) -> crate::error::Result<()> {
        if self.settled {
            return Ok(());
        }
        if let Some(error) = self.failure() {
            return Err(error);
        }
        let result = self.observe_usage(usage, terminal);
        if let Err(error) = &result {
            self.failure = Some(error.to_string());
        }
        if terminal {
            // Install the authoritative bill and close under the same lock.
            // A retained observer cannot race in another provisional amount
            // between final pricing and settlement.
            self.settle();
        }
        result
    }

    fn observe_usage(
        &mut self,
        usage: &mut crate::model::Usage,
        terminal: bool,
    ) -> crate::error::Result<()> {
        // price_usage treats nonpositive/NaN totals as missing billing. The
        // spend guard must distinguish an absent zero from invalid usage
        // before that fallback can erase evidence of an accounting failure.
        if !usage.cost.total.is_finite() || usage.cost.total < 0.0 {
            self.known_cost = f64::NAN;
            return self
                .attempt
                .as_mut()
                .expect("active cost attempt")
                .observe(f64::NAN);
        }
        if let Some(rates) = &self.rates {
            rates.price_usage(usage);
        }
        if !usage.cost.total.is_finite() || usage.cost.total < 0.0 {
            self.known_cost = f64::NAN;
            return self
                .attempt
                .as_mut()
                .expect("active cost attempt")
                .observe(f64::NAN);
        }
        self.remember_cost(usage.cost.total, terminal && usage.cost.total > 0.0);
        if terminal {
            Ok(())
        } else {
            // The parent must own known partial usage before this stream can
            // be interrupted or its child process reaped. Terminal billing
            // replaces this estimate rather than charging it a second time.
            self.attempt
                .as_mut()
                .expect("active cost attempt")
                .observe(self.known_cost)
        }
    }

    fn remember_cost(&mut self, cost: f64, authoritative_terminal: bool) {
        if self.known_cost.is_finite() && (authoritative_terminal || cost > self.known_cost) {
            self.known_cost = cost;
        }
    }

    fn failure(&self) -> Option<crate::error::Error> {
        self.failure.as_ref().map(|message| {
            crate::error::Error::session(format!("model cost accounting failed: {message}"))
        })
    }

    fn settle(&mut self) {
        if !self.settled {
            self.settled = true;
            // Release remote accounting resources even if an adapter retains
            // observer clones after the request owner has finished.
            let mut attempt = self.attempt.take().expect("active cost attempt");
            if let Err(error) = attempt.finish(self.known_cost) {
                tracing::warn!(%error, "provider spend accounting failed; budget quarantined");
            }
        }
    }
}

/// Exactly one owner settles the attempt, even when observer clones outlive it.
struct CostAccountingOwner {
    accounting: Arc<std::sync::Mutex<CostAccounting>>,
}

impl CostAccountingOwner {
    fn new(accounting: CostAccounting) -> Self {
        Self {
            accounting: Arc::new(std::sync::Mutex::new(accounting)),
        }
    }

    fn observer(&self) -> UsageObserver {
        UsageObserver {
            accounting: Arc::clone(&self.accounting),
        }
    }

    fn observe(&self, usage: &mut crate::model::Usage, terminal: bool) -> crate::error::Result<()> {
        self.accounting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .observe(usage, terminal)
    }

    fn failure(&self) -> Option<crate::error::Error> {
        self.accounting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failure()
    }

    fn settle(&self) {
        self.accounting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .settle();
    }
}

impl Drop for CostAccountingOwner {
    fn drop(&mut self) {
        self.settle();
    }
}

type BoxedProviderStream = Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>;

/// Drop the provider's setup future before the accounting owner. Its cleanup
/// can still report usage, even when setup never produced a public stream.
struct CostMeteredSetup<'a> {
    inner: Option<
        Pin<
            Box<
                dyn std::future::Future<Output = crate::error::Result<BoxedProviderStream>>
                    + Send
                    + 'a,
            >,
        >,
    >,
    accounting: Option<CostAccountingOwner>,
}

impl std::future::Future for CostMeteredSetup<'_> {
    type Output = crate::error::Result<BoxedProviderStream>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let result = self
            .inner
            .as_mut()
            .expect("active provider setup")
            .as_mut()
            .poll(cx);
        if result.is_pending()
            && self
                .accounting
                .as_ref()
                .and_then(CostAccountingOwner::failure)
                .is_none()
        {
            return Poll::Pending;
        }
        drop(self.inner.take());
        let accounting = self
            .accounting
            .take()
            .expect("provider setup accounting owner");
        if let Some(error) = accounting.failure() {
            // A ready stream may itself publish usage during cleanup.
            drop(result);
            drop(accounting);
            return Poll::Ready(Err(error));
        }
        match result {
            Poll::Ready(Ok(inner)) => Poll::Ready(Ok(Box::pin(CostMeteredStream {
                inner: Some(inner),
                accounting,
            }))),
            Poll::Ready(Err(error)) => {
                drop(accounting);
                Poll::Ready(Err(error))
            }
            Poll::Pending => {
                unreachable!("pending setup without an accounting error returned above")
            }
        }
    }
}

impl Drop for CostMeteredSetup<'_> {
    fn drop(&mut self) {
        drop(self.inner.take());
        drop(self.accounting.take());
    }
}

/// Owns accounting through source cleanup. Usage snapshots update the shared
/// budget immediately, including frames that produce no public stream event.
struct CostMeteredStream {
    inner: Option<BoxedProviderStream>,
    accounting: CostAccountingOwner,
}

impl Stream for CostMeteredStream {
    type Item = crate::error::Result<StreamEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        if self.inner.is_none() {
            return Poll::Ready(None);
        }
        if let Some(error) = self.accounting.failure() {
            drop(self.inner.take());
            self.accounting.settle();
            return Poll::Ready(Some(Err(error)));
        }
        let item = self
            .inner
            .as_mut()
            .expect("active provider stream")
            .as_mut()
            .poll_next(cx);
        match item {
            Poll::Ready(Some(Ok(mut event))) => {
                let terminal =
                    matches!(&event, StreamEvent::Done { .. } | StreamEvent::Error { .. });
                if terminal {
                    drop(self.inner.take());
                }
                let observed = match &mut event {
                    StreamEvent::Start { partial } => {
                        self.accounting.observe(&mut partial.usage, false)
                    }
                    StreamEvent::Done { message, .. } => {
                        self.accounting.observe(&mut message.usage, true)
                    }
                    StreamEvent::Error { error, .. } => {
                        self.accounting.observe(&mut error.usage, true)
                    }
                    _ => self.accounting.failure().map_or(Ok(()), Err),
                };
                if let Err(error) = observed {
                    drop(self.inner.take());
                    self.accounting.settle();
                    return Poll::Ready(Some(Err(error)));
                }
                if terminal {
                    self.accounting.settle();
                }
                Poll::Ready(Some(Ok(event)))
            }
            Poll::Ready(None) => {
                drop(self.inner.take());
                let error = self.accounting.failure();
                self.accounting.settle();
                Poll::Ready(error.map(Err))
            }
            Poll::Ready(Some(Err(error))) => {
                drop(self.inner.take());
                let error = self.accounting.failure().unwrap_or(error);
                self.accounting.settle();
                Poll::Ready(Some(Err(error)))
            }
            Poll::Pending => {
                if let Some(error) = self.accounting.failure() {
                    drop(self.inner.take());
                    self.accounting.settle();
                    Poll::Ready(Some(Err(error)))
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

impl Drop for CostMeteredStream {
    fn drop(&mut self) {
        drop(self.inner.take());
        self.accounting.settle();
    }
}

// ============================================================================
// Context
// ============================================================================

/// Inputs to a single completion request.
///
/// The agent loop builds a `Context` from the current session state and tool registry, then hands
/// it to a [`Provider`] implementation to perform provider-specific request encoding.
///
/// Uses [`Cow`] for `messages` and `tools` to avoid deep-cloning the full conversation history on
/// every turn when no mutation is needed (the common case).
#[derive(Debug, Clone)]
pub struct Context<'a> {
    /// Provider-specific system prompt content.
    ///
    /// Uses [`Cow`] to borrow from `AgentConfig.system_prompt` on every turn without
    /// cloning.  Providers that need an owned `String` can call `.into_owned()`.
    pub system_prompt: Option<Cow<'a, str>>,
    /// Conversation history (user/assistant/tool results).
    pub messages: Cow<'a, [Message]>,
    /// Tool definitions available to the model for this request.
    pub tools: Cow<'a, [ToolDef]>,
}

impl Default for Context<'_> {
    fn default() -> Self {
        Self {
            system_prompt: None,
            messages: Cow::Owned(Vec::new()),
            tools: Cow::Owned(Vec::new()),
        }
    }
}

impl Context<'_> {
    /// Create a `Context` with fully-owned data (no borrowing).
    ///
    /// Convenient for tests and one-off callers that already have owned vectors.
    pub fn owned(
        system_prompt: Option<String>,
        messages: Vec<Message>,
        tools: Vec<ToolDef>,
    ) -> Context<'static> {
        Context {
            system_prompt: system_prompt.map(Cow::Owned),
            messages: Cow::Owned(messages),
            tools: Cow::Owned(tools),
        }
    }
}

// ============================================================================
// Tool Definition
// ============================================================================

/// A tool definition exposed to the model.
///
/// Providers translate this struct into the backend's tool/schema representation (typically JSON
/// Schema) so the model can emit tool calls that the host executes locally.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
}

// ============================================================================
// Before-Provider-Request Hook
// ============================================================================

/// Event handed to a [`BeforeProviderRequestHook`] just before a provider
/// sends its fully-built HTTP request body (gh #167 / bd-1q31s).
///
/// `payload` is the provider-specific request body exactly as it would be
/// serialized onto the wire. Auth headers are deliberately **not** part of
/// this event: credentials stay reachable only through their dedicated,
/// capability-gated surfaces.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BeforeProviderRequestEvent {
    pub provider: String,
    pub api: String,
    pub model: String,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    pub payload: serde_json::Value,
}

type BeforeProviderRequestFn = dyn Fn(
        BeforeProviderRequestEvent,
    ) -> Pin<Box<dyn std::future::Future<Output = Option<serde_json::Value>> + Send>>
    + Send
    + Sync;

/// Host-provided interceptor that may rewrite a provider request body.
///
/// It runs before the body is sent. Returning `None` keeps the original body;
/// returning `Some(value)` proposes a replacement, which the provider must
/// validate before use (fail-open to the original on rejection).
#[derive(Clone)]
pub struct BeforeProviderRequestHook(std::sync::Arc<BeforeProviderRequestFn>);

impl BeforeProviderRequestHook {
    pub fn new<F>(hook: F) -> Self
    where
        F: Fn(
                BeforeProviderRequestEvent,
            )
                -> Pin<Box<dyn std::future::Future<Output = Option<serde_json::Value>> + Send>>
            + Send
            + Sync
            + 'static,
    {
        Self(std::sync::Arc::new(hook))
    }

    /// Offer the request body for rewriting. `None` means "keep the original".
    pub async fn rewrite(&self, event: BeforeProviderRequestEvent) -> Option<serde_json::Value> {
        (self.0)(event).await
    }
}

impl std::fmt::Debug for BeforeProviderRequestHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BeforeProviderRequestHook")
    }
}

// ============================================================================
// Stream Options
// ============================================================================

/// Options that control streaming completion behavior.
///
/// Most options are passed through to the provider request (temperature, max tokens, headers).
/// Some fields are Pi-specific conveniences (e.g. `session_id` for logging/correlation).
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub api_key: Option<String>,
    pub cache_retention: CacheRetention,
    pub session_id: Option<String>,
    /// Cache-affinity key serialized as `prompt_cache_key` on OpenAI-shaped
    /// requests (chat completions, Azure, Responses). OpenAI-compatible
    /// backends route a request to a prompt-cache shard by prefix hash
    /// combined with this key; without it some stacks give streamed requests
    /// no cache affinity at all. Session-scoped by default (see
    /// `app::resolve_prompt_cache_key`); `None` omits the field so the wire
    /// format is unchanged for backends that reject unknown params.
    pub prompt_cache_key: Option<String>,
    /// Requested service tier (`/fast` sets `"priority"`). Only backends that
    /// honor a tier receive it: OpenAI, Codex and OpenRouter as
    /// `service_tier` (see [`openai_service_tier`]), and direct Anthropic,
    /// which realizes only `priority`, as `speed: "fast"`. Every other
    /// provider ignores it, so the request is unchanged there.
    pub service_tier: Option<String>,
    pub headers: HashMap<String, String>,
    pub thinking_level: Option<ThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Optional `before_provider_request` interceptor (gh #167 / bd-1q31s).
    pub before_provider_request: Option<BeforeProviderRequestHook>,
    /// Report cumulative provider-acknowledged usage independently of public
    /// stream events. Installed per attempt by the spending guard; providers
    /// must not estimate usage from generated text or share it across requests.
    #[doc(hidden)]
    pub usage_observer: Option<UsageObserver>,
}

/// The `service_tier` value an OpenAI-shaped request to `provider` should
/// carry, or `None` to omit the field.
///
/// Mirrors OMP's `shouldSendServiceTier`. OpenAI and Codex accept every tier
/// except `auto` (their default, so sending it is pointless). OpenRouter
/// forwards only `flex`, `scale` and `priority`. Every other OpenAI-compatible
/// backend gets nothing, because many of them reject unknown parameters.
pub fn openai_service_tier(provider: &str, tier: Option<&str>) -> Option<String> {
    let tier = tier.filter(|tier| !tier.is_empty() && *tier != "auto")?;
    let sends = match provider {
        "openai" | "openai-codex" => true,
        "openrouter" => matches!(tier, "flex" | "scale" | "priority"),
        _ => false,
    };
    sends.then(|| tier.to_string())
}

/// Cache retention policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CacheRetention {
    #[default]
    None,
    /// Provider-managed short-lived caching (provider-specific semantics).
    Short,
    /// Provider-managed long-lived caching (e.g. ~1 hour TTL on Anthropic).
    Long,
}

/// Custom thinking token budgets per level.
#[derive(Debug, Clone)]
pub struct ThinkingBudgets {
    pub minimal: u32,
    pub low: u32,
    pub medium: u32,
    pub high: u32,
    pub xhigh: u32,
    pub max: u32,
}

impl Default for ThinkingBudgets {
    fn default() -> Self {
        Self {
            minimal: 1024,
            low: 2048,
            medium: 8192,
            high: 16384,
            xhigh: 32768, // Default to double high, or model max? Let's pick a reasonable default.
            max: 65536,
        }
    }
}

// ============================================================================
// Model Definition
// ============================================================================

/// A model definition loaded from the models registry.
///
/// This struct is used to drive provider selection, request limits (context window/max tokens),
/// and cost accounting.
#[derive(Debug, Clone, Serialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub reasoning: bool,
    pub input: Vec<InputType>,
    pub cost: ModelCost,
    pub context_window: u32,
    pub max_tokens: u32,
    pub headers: HashMap<String, String>,
}

/// Input types supported by a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputType {
    Text,
    Image,
    /// Inline video input (`ContentBlock::Media` with a `video/*` MIME type).
    /// Declared by Gemini-family models (gh #212).
    Video,
    /// Inline audio input (`ContentBlock::Media` with an `audio/*` MIME type).
    /// Declared by Gemini-family models (gh #212).
    Audio,
}

impl InputType {
    /// Catalog spelling of this input type (`"text"`, `"image"`, `"video"`,
    /// `"audio"`) — the inverse of [`InputType::parse`].
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }

    /// Parse a catalog / `models.json` / extension-registration input type.
    /// Unknown labels return `None` so callers can decide between dropping
    /// and warning.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "text" => Some(Self::Text),
            "image" => Some(Self::Image),
            "video" => Some(Self::Video),
            "audio" => Some(Self::Audio),
            _ => None,
        }
    }
}

/// Model pricing per million tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl ModelCost {
    /// True when at least one rate is non-zero, i.e. the catalog actually
    /// knows this model's pricing.
    #[must_use]
    pub fn is_priced(&self) -> bool {
        [self.input, self.output, self.cache_read, self.cache_write]
            .iter()
            .any(|rate| *rate > 0.0)
    }

    /// Fill `usage.cost` from these per-million-token rates (gh #221).
    ///
    /// The component costs always come from the catalog rates. A provider-
    /// reported total (OpenRouter's `usage.cost`, the amount actually billed)
    /// is authoritative and kept as `total`; otherwise `total` is the sum of
    /// the components. Mirrors upstream pi's `calculateCost`.
    #[allow(clippy::cast_precision_loss)] // Token counts within practical range won't lose precision
    pub fn price_usage(&self, usage: &mut crate::model::Usage) {
        let per_million = |rate: f64, tokens: u64| (rate / 1_000_000.0) * tokens as f64;
        let provider_reported_total = usage.cost.total;
        usage.cost.input = per_million(self.input, usage.input);
        usage.cost.output = per_million(self.output, usage.output);
        usage.cost.cache_read = per_million(self.cache_read, usage.cache_read);
        usage.cost.cache_write = per_million(self.cache_write, usage.cache_write);
        usage.cost.total = if provider_reported_total > 0.0 {
            provider_reported_total
        } else {
            usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write
        };
    }
}

impl Model {
    /// Human-facing label for status messages: the `provider/id` identity,
    /// followed by the catalog/`models.json` `name` when it says something the
    /// id does not (`deepseek/deepseek-v4-pro (DeepSeek V4 Pro)`). The identity
    /// stays first so anything matching on `provider/id` keeps working;
    /// selection and lookup never read this (gh #214; upstream shows
    /// `name || id` in its model-switch status).
    pub fn display_label(&self) -> String {
        let identity = format!("{}/{}", self.provider, self.id);
        let name = self.name.trim();
        if name.is_empty() || name == self.id || name == identity {
            identity
        } else {
            format!("{identity} ({name})")
        }
    }

    /// The model as the status line shows it, following OMP's model segment:
    /// the catalog/`models.json` name, else the id, with a leading "Claude "
    /// dropped because the provider is already evident ("Opus 4.6").
    pub fn status_label(&self) -> String {
        let name = self.name.trim();
        let label = if name.is_empty() {
            self.id.as_str()
        } else {
            name
        };
        label.strip_prefix("Claude ").unwrap_or(label).to_string()
    }

    /// Calculate cost for usage.
    #[allow(clippy::cast_precision_loss)] // Token counts within practical range won't lose precision
    pub fn calculate_cost(
        &self,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> f64 {
        let input_cost = (self.cost.input / 1_000_000.0) * input as f64;
        let output_cost = (self.cost.output / 1_000_000.0) * output as f64;
        let cache_read_cost = (self.cost.cache_read / 1_000_000.0) * cache_read as f64;
        let cache_write_cost = (self.cost.cache_write / 1_000_000.0) * cache_write as f64;
        input_cost + output_cost + cache_read_cost + cache_write_cost
    }
}

// ============================================================================
// Known APIs and Providers
// ============================================================================

/// Known API types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Api {
    AnthropicMessages,
    OpenAICompletions,
    OpenAIResponses,
    OpenAICodexResponses,
    AzureOpenAIResponses,
    BedrockConverseStream,
    GoogleGenerativeAI,
    GoogleGeminiCli,
    GoogleVertex,
    Custom(String),
}

impl std::fmt::Display for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AnthropicMessages => write!(f, "anthropic-messages"),
            Self::OpenAICompletions => write!(f, "openai-completions"),
            Self::OpenAIResponses => write!(f, "openai-responses"),
            Self::OpenAICodexResponses => write!(f, "openai-codex-responses"),
            Self::AzureOpenAIResponses => write!(f, "azure-openai-responses"),
            Self::BedrockConverseStream => write!(f, "bedrock-converse-stream"),
            Self::GoogleGenerativeAI => write!(f, "google-generative-ai"),
            Self::GoogleGeminiCli => write!(f, "google-gemini-cli"),
            Self::GoogleVertex => write!(f, "google-vertex"),
            Self::Custom(s) => write!(f, "{s}"),
        }
    }
}

impl std::str::FromStr for Api {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "anthropic-messages" => Ok(Self::AnthropicMessages),
            "openai-completions" => Ok(Self::OpenAICompletions),
            "openai-responses" => Ok(Self::OpenAIResponses),
            "openai-codex-responses" => Ok(Self::OpenAICodexResponses),
            "azure-openai-responses" => Ok(Self::AzureOpenAIResponses),
            "bedrock-converse-stream" => Ok(Self::BedrockConverseStream),
            "google-generative-ai" => Ok(Self::GoogleGenerativeAI),
            "google-gemini-cli" => Ok(Self::GoogleGeminiCli),
            "google-vertex" => Ok(Self::GoogleVertex),
            other if !other.is_empty() => Ok(Self::Custom(other.to_string())),
            _ => Err("API identifier cannot be empty".to_string()),
        }
    }
}

/// Known providers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::upper_case_acronyms)] // These are proper names/brands
pub enum KnownProvider {
    Anthropic,
    OpenAI,
    Google,
    GoogleVertex,
    AmazonBedrock,
    AzureOpenAI,
    GithubCopilot,
    XAI,
    Groq,
    Cerebras,
    OpenRouter,
    Mistral,
    Custom(String),
}

impl std::fmt::Display for KnownProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Anthropic => write!(f, "anthropic"),
            Self::OpenAI => write!(f, "openai"),
            Self::Google => write!(f, "google"),
            Self::GoogleVertex => write!(f, "google-vertex"),
            Self::AmazonBedrock => write!(f, "amazon-bedrock"),
            Self::AzureOpenAI => write!(f, "azure-openai"),
            Self::GithubCopilot => write!(f, "github-copilot"),
            Self::XAI => write!(f, "xai"),
            Self::Groq => write!(f, "groq"),
            Self::Cerebras => write!(f, "cerebras"),
            Self::OpenRouter => write!(f, "openrouter"),
            Self::Mistral => write!(f, "mistral"),
            Self::Custom(s) => write!(f, "{s}"),
        }
    }
}

impl std::str::FromStr for KnownProvider {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "anthropic" => Ok(Self::Anthropic),
            "openai" => Ok(Self::OpenAI),
            "google" => Ok(Self::Google),
            "google-vertex" => Ok(Self::GoogleVertex),
            "amazon-bedrock" => Ok(Self::AmazonBedrock),
            "azure-openai" | "azure" | "azure-cognitive-services" => Ok(Self::AzureOpenAI),
            "github-copilot" => Ok(Self::GithubCopilot),
            "xai" => Ok(Self::XAI),
            "groq" => Ok(Self::Groq),
            "cerebras" => Ok(Self::Cerebras),
            "openrouter" => Ok(Self::OpenRouter),
            "mistral" => Ok(Self::Mistral),
            other if !other.is_empty() => Ok(Self::Custom(other.to_string())),
            _ => Err("Provider identifier cannot be empty".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

    struct SpendProbe {
        calls: std::sync::atomic::AtomicUsize,
        events: Vec<StreamEvent>,
        rates: Option<ModelCost>,
        native_cost: Option<serde_json::Value>,
    }

    #[allow(clippy::unnecessary_literal_bound)]
    #[async_trait]
    impl Provider for SpendProbe {
        fn name(&self) -> &str {
            "spend-probe"
        }
        fn api(&self) -> &str {
            "test"
        }
        fn model_id(&self) -> &str {
            "metered"
        }
        fn model_cost(&self) -> Option<ModelCost> {
            self.rates.clone()
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::pin(futures::stream::iter(
                self.events.clone().into_iter().map(Ok),
            )))
        }

        async fn compact_native(
            &self,
            _request_body: &serde_json::Value,
            _options: &StreamOptions,
        ) -> crate::error::Result<serde_json::Value> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut response = serde_json::json!({"usage": {
                "input_tokens": 1_000_000,
                "input_tokens_details": {"cached_tokens": 250_000},
                "output_tokens": 500_000
            }});
            if let Some(cost) = &self.native_cost {
                response["usage"]["cost"] = cost.clone();
            }
            Ok(response)
        }
    }

    fn cost_message(cost: f64, input: u64) -> crate::model::AssistantMessage {
        crate::model::AssistantMessage {
            content: vec![],
            api: "test".to_string(),
            provider: "spend-probe".to_string(),
            model: "metered".to_string(),
            usage: crate::model::Usage {
                input,
                cost: crate::model::Cost {
                    total: cost,
                    ..Default::default()
                },
                ..Default::default()
            },
            stop_reason: crate::model::StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        }
    }

    fn spend_probe(events: Vec<StreamEvent>, rates: Option<ModelCost>) -> Arc<SpendProbe> {
        Arc::new(SpendProbe {
            calls: std::sync::atomic::AtomicUsize::new(0),
            events,
            rates,
            native_cost: None,
        })
    }

    mod usage_observation {
        use super::*;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Clone, Copy)]
        enum Mode {
            SetupPending,
            SetupError,
            Pending,
            Error,
            Eof,
            Done,
            Invalid,
        }

        struct DropUsage {
            observer: UsageObserver,
            cost: f64,
            drops: Arc<AtomicUsize>,
        }

        impl Drop for DropUsage {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
                let _ = self.observer.observe(&cost_message(self.cost, 0).usage);
            }
        }

        struct Probe {
            mode: Mode,
            observer: Mutex<Option<UsageObserver>>,
            drops: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl Provider for Probe {
            fn name(&self) -> &str {
                "usage-probe"
            }
            fn api(&self) -> &str {
                "test"
            }
            fn model_id(&self) -> &str {
                "observed"
            }

            async fn stream(
                &self,
                _context: &Context<'_>,
                options: &StreamOptions,
            ) -> crate::error::Result<BoxedProviderStream> {
                let observer = options
                    .usage_observer
                    .clone()
                    .expect("per-attempt observer");
                *self.observer.lock().unwrap() = Some(observer.clone());
                observer.observe(&cost_message(0.5, 0).usage)?;
                let setup = matches!(self.mode, Mode::SetupPending | Mode::SetupError);
                let cleanup = DropUsage {
                    observer,
                    cost: if setup { 0.75 } else { 1.25 },
                    drops: Arc::clone(&self.drops),
                };
                if setup {
                    let _cleanup = cleanup;
                    if matches!(self.mode, Mode::SetupPending) {
                        std::future::pending::<()>().await;
                    }
                    return Err(crate::error::Error::api("setup failed"));
                }
                Ok(Box::pin(ObservedStream {
                    mode: self.mode,
                    cleanup,
                }))
            }
        }

        struct ObservedStream {
            mode: Mode,
            cleanup: DropUsage,
        }

        impl Stream for ObservedStream {
            type Item = crate::error::Result<StreamEvent>;

            fn poll_next(
                self: Pin<&mut Self>,
                _cx: &mut TaskContext<'_>,
            ) -> Poll<Option<Self::Item>> {
                self.cleanup
                    .observer
                    .observe(&cost_message(1.0, 0).usage)
                    .unwrap();
                match self.mode {
                    Mode::Pending => Poll::Pending,
                    Mode::Error => Poll::Ready(Some(Err(crate::error::Error::api("wire failed")))),
                    Mode::Eof => Poll::Ready(None),
                    Mode::Done => Poll::Ready(Some(Ok(StreamEvent::Done {
                        reason: crate::model::StopReason::Stop,
                        message: cost_message(0.25, 0),
                    }))),
                    Mode::Invalid => {
                        // Even an adapter that ignores observer failure must
                        // not hide invalid acknowledged billing with Pending.
                        let _ = self
                            .cleanup
                            .observer
                            .observe(&cost_message(f64::NAN, 0).usage);
                        Poll::Pending
                    }
                    Mode::SetupPending | Mode::SetupError => unreachable!("setup-only fixture"),
                }
            }
        }

        fn probe(mode: Mode) -> Arc<Probe> {
            Arc::new(Probe {
                mode,
                observer: Mutex::new(None),
                drops: Arc::new(AtomicUsize::new(0)),
            })
        }

        fn assert_closed(probe: &Probe, budget: &crate::cost::CostBudget, cost: f64) {
            assert_eq!(probe.drops.load(Ordering::SeqCst), 1);
            assert!((budget.spent() - cost).abs() < f64::EPSILON);
            assert_eq!(budget.snapshot().pending, 0.0);
            let revision = budget.snapshot().revision;
            let observer = probe.observer.lock().unwrap().clone().unwrap();
            assert!(observer.accounting.lock().unwrap().attempt.is_none());
            observer.observe(&cost_message(99.0, 0).usage).unwrap();
            observer.observe(&cost_message(f64::NAN, 0).usage).unwrap();
            assert_eq!(budget.snapshot().revision, revision);
            assert!((budget.spent() - cost).abs() < f64::EPSILON);
        }

        #[test]
        fn setup_cancellation_and_failure_keep_acknowledged_usage_and_close_observers() {
            for mode in [Mode::SetupPending, Mode::SetupError] {
                let budget = crate::cost::CostBudget::new(None, Some(0.4), 0.0).unwrap();
                let probe = probe(mode);
                let provider = guard_with_cost_budget(probe.clone(), budget.clone());
                let context = Context::default();
                let options = StreamOptions::default();
                let mut setup = Box::pin(provider.stream(&context, &options));
                let mut cx = TaskContext::from_waker(futures::task::noop_waker_ref());
                let result = std::future::Future::poll(setup.as_mut(), &mut cx);
                if matches!(mode, Mode::SetupPending) {
                    assert!(result.is_pending());
                    assert_eq!(budget.snapshot().pending, 0.5);
                    assert_eq!(probe.drops.load(Ordering::SeqCst), 0);
                } else {
                    assert!(matches!(result, Poll::Ready(Err(_))));
                }
                assert!(matches!(
                    budget.admit(),
                    Err(crate::error::Error::CostCap { .. })
                ));
                drop(setup);
                assert_closed(&probe, &budget, 0.75);
                assert!(matches!(
                    budget.admit(),
                    Err(crate::error::Error::CostCap { .. })
                ));
            }
        }

        #[test]
        fn pending_failure_eof_and_terminal_cleanup_settle_once_after_source_drop() {
            futures::executor::block_on(async {
                for mode in [
                    Mode::Pending,
                    Mode::Error,
                    Mode::Eof,
                    Mode::Done,
                    Mode::Invalid,
                ] {
                    let budget = crate::cost::CostBudget::new(None, Some(1.0), 0.0).unwrap();
                    let probe = probe(mode);
                    let provider = guard_with_cost_budget(probe.clone(), budget.clone());
                    let mut stream = provider
                        .stream(&Context::default(), &StreamOptions::default())
                        .await
                        .unwrap();
                    assert_eq!(budget.snapshot().pending, 0.5);
                    let mut cx = TaskContext::from_waker(futures::task::noop_waker_ref());
                    let result = stream.as_mut().poll_next(&mut cx);
                    if matches!(mode, Mode::Pending) {
                        assert!(result.is_pending());
                        assert_eq!(budget.snapshot().pending, 1.0);
                        assert!(budget.admit().is_err());
                        assert_eq!(probe.drops.load(Ordering::SeqCst), 0);
                    } else {
                        assert_eq!(probe.drops.load(Ordering::SeqCst), 1);
                        match mode {
                            Mode::Done => assert!(matches!(
                                result,
                                Poll::Ready(Some(Ok(StreamEvent::Done { .. })))
                            )),
                            Mode::Eof => assert!(matches!(result, Poll::Ready(None))),
                            _ => assert!(matches!(result, Poll::Ready(Some(Err(_))))),
                        }
                        assert!(matches!(
                            stream.as_mut().poll_next(&mut cx),
                            Poll::Ready(None)
                        ));
                    }
                    drop(stream);
                    let expected = match mode {
                        Mode::Done => 0.25,
                        Mode::Invalid => 1.0,
                        _ => 1.25,
                    };
                    assert_closed(&probe, &budget, expected);
                    match mode {
                        Mode::Done => budget.admit().unwrap(),
                        Mode::Invalid => assert!(matches!(
                            budget.admit(),
                            Err(crate::error::Error::Session(_))
                        )),
                        _ => assert!(matches!(
                            budget.admit(),
                            Err(crate::error::Error::CostCap { .. })
                        )),
                    }
                }
            });
        }
    }

    #[test]
    fn provider_cost_guard_refuses_next_attempt_and_prices_before_publishing() {
        futures::executor::block_on(async {
            let budget = crate::cost::CostBudget::new(Some(1.0), Some(3.0), 0.0).unwrap();
            let probe = spend_probe(
                vec![StreamEvent::Done {
                    reason: crate::model::StopReason::Stop,
                    message: cost_message(0.0, 1_000_000),
                }],
                Some(ModelCost {
                    input: 2.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                }),
            );
            let provider = guard_with_cost_budget(probe.clone(), budget.clone());
            for expected in [2.0, 4.0] {
                let mut stream = provider
                    .stream(&Context::default(), &StreamOptions::default())
                    .await
                    .unwrap();
                let StreamEvent::Done { message, .. } = stream.next().await.unwrap().unwrap()
                else {
                    panic!("terminal response");
                };
                assert!((message.usage.cost.total - 2.0).abs() < f64::EPSILON);
                assert!((budget.spent() - expected).abs() < f64::EPSILON);
                assert!(stream.next().await.is_none());
            }
            assert!(matches!(
                provider
                    .stream(&Context::default(), &StreamOptions::default())
                    .await,
                Err(crate::error::Error::CostCap { .. })
            ));
            assert_eq!(probe.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
            assert!(budget.take_warning().is_some());
            assert!(budget.take_warning().is_none());
        });
    }

    #[test]
    fn terminal_error_and_duplicate_terminal_charge_known_response_once() {
        futures::executor::block_on(async {
            let budget = crate::cost::CostBudget::new(None, Some(5.0), 0.0).unwrap();
            let mut failed = cost_message(0.75, 1_000_000);
            failed.stop_reason = crate::model::StopReason::Error;
            let probe = spend_probe(
                vec![
                    StreamEvent::Start {
                        partial: cost_message(0.0, 1_000_000),
                    },
                    StreamEvent::Error {
                        reason: crate::model::StopReason::Error,
                        error: failed.clone(),
                    },
                    StreamEvent::Error {
                        reason: crate::model::StopReason::Error,
                        error: failed,
                    },
                ],
                Some(ModelCost {
                    input: 2.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                }),
            );
            let provider = guard_with_cost_budget(probe, budget.clone());
            let mut stream = provider
                .stream(&Context::default(), &StreamOptions::default())
                .await
                .unwrap();
            while stream.next().await.is_some() {}
            drop(stream);
            assert!(
                (budget.spent() - 0.75).abs() < f64::EPSILON,
                "provider-reported bill wins over catalog and duplicate terminals"
            );
        });
    }

    #[test]
    fn dropped_and_unterminated_streams_keep_only_reported_partial_usage() {
        futures::executor::block_on(async {
            for drain in [false, true] {
                let budget = crate::cost::CostBudget::new(None, Some(5.0), 0.0).unwrap();
                let provider = guard_with_cost_budget(
                    spend_probe(
                        vec![
                            StreamEvent::Start {
                                partial: cost_message(0.25, 0),
                            },
                            StreamEvent::TextDelta {
                                content_index: 0,
                                delta: "not a billing estimate".to_string(),
                            },
                        ],
                        None,
                    ),
                    budget.clone(),
                );
                let mut stream = provider
                    .stream(&Context::default(), &StreamOptions::default())
                    .await
                    .unwrap();
                stream.next().await.unwrap().unwrap();
                if drain {
                    while stream.next().await.is_some() {}
                }
                drop(stream);
                assert!((budget.spent() - 0.25).abs() < f64::EPSILON);
            }
        });
    }

    #[test]
    fn rebinding_provider_replaces_the_previous_session_budget() {
        futures::executor::block_on(async {
            let old = crate::cost::CostBudget::new(None, Some(1.0), 1.0).unwrap();
            let current = crate::cost::CostBudget::new(None, Some(5.0), 0.0).unwrap();
            let provider = guard_with_cost_budget(
                spend_probe(
                    vec![StreamEvent::Done {
                        reason: crate::model::StopReason::Stop,
                        message: cost_message(0.5, 0),
                    }],
                    None,
                ),
                old.clone(),
            );
            let provider = guard_with_cost_budget(
                guard_with_cost_budget(provider, current.clone()),
                current.clone(),
            );
            let mut stream = provider
                .stream(&Context::default(), &StreamOptions::default())
                .await
                .unwrap();
            stream.next().await.unwrap().unwrap();
            drop(stream);
            assert!((old.spent() - 1.0).abs() < f64::EPSILON);
            assert!((current.spent() - 0.5).abs() < f64::EPSILON);
        });
    }

    #[test]
    fn invalid_reported_cost_cannot_be_hidden_by_catalog_pricing() {
        futures::executor::block_on(async {
            for reported in [-1.0, f64::NAN, f64::INFINITY] {
                for partial in [false, true] {
                    let budget = crate::cost::CostBudget::new(None, Some(5.0), 0.0).unwrap();
                    let mut events = Vec::new();
                    if partial {
                        events.push(StreamEvent::Start {
                            partial: cost_message(reported, 1_000_000),
                        });
                    }
                    events.push(StreamEvent::Done {
                        reason: crate::model::StopReason::Stop,
                        message: cost_message(if partial { 0.5 } else { reported }, 1_000_000),
                    });
                    let probe = spend_probe(
                        events,
                        Some(ModelCost {
                            input: 2.0,
                            output: 0.0,
                            cache_read: 0.0,
                            cache_write: 0.0,
                        }),
                    );
                    let provider = guard_with_cost_budget(probe.clone(), budget.clone());
                    let mut stream = provider
                        .stream(&Context::default(), &StreamOptions::default())
                        .await
                        .unwrap();
                    while stream.next().await.is_some() {}
                    drop(stream);
                    assert!(matches!(
                        provider
                            .stream(&Context::default(), &StreamOptions::default())
                            .await,
                        Err(crate::error::Error::Session(_))
                    ));
                    assert_eq!(probe.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
                    assert_eq!(
                        budget.spent(),
                        0.0,
                        "invalid usage was not repriced as valid spend"
                    );
                }
            }
        });
    }

    #[test]
    fn native_compaction_quarantines_invalid_reported_cost_before_pricing() {
        futures::executor::block_on(async {
            for reported in [serde_json::json!(-1.0), serde_json::json!("invalid")] {
                let budget = crate::cost::CostBudget::new(None, Some(5.0), 0.0).unwrap();
                let mut probe = spend_probe(
                    vec![],
                    Some(ModelCost {
                        input: 2.0,
                        output: 4.0,
                        cache_read: 1.0,
                        cache_write: 0.0,
                    }),
                );
                Arc::get_mut(&mut probe).unwrap().native_cost = Some(reported);
                let provider = guard_with_cost_budget(probe.clone(), budget.clone());
                provider
                    .compact_native(&serde_json::json!({}), &StreamOptions::default())
                    .await
                    .unwrap();
                assert!(matches!(
                    provider
                        .compact_native(&serde_json::json!({}), &StreamOptions::default())
                        .await,
                    Err(crate::error::Error::Session(_))
                ));
                assert_eq!(probe.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
                assert_eq!(budget.spent(), 0.0);
            }
        });
    }

    #[test]
    fn native_compaction_shares_admission_and_cache_aware_accounting() {
        futures::executor::block_on(async {
            let budget = crate::cost::CostBudget::new(None, Some(3.0), 0.0).unwrap();
            let probe = spend_probe(
                vec![],
                Some(ModelCost {
                    input: 2.0,
                    output: 4.0,
                    cache_read: 1.0,
                    cache_write: 0.0,
                }),
            );
            let provider = guard_with_cost_budget(probe.clone(), budget.clone());
            let body = serde_json::json!({});
            provider
                .compact_native(&body, &StreamOptions::default())
                .await
                .unwrap();
            assert!((budget.spent() - 3.75).abs() < f64::EPSILON);
            assert!(matches!(
                provider
                    .compact_native(&body, &StreamOptions::default())
                    .await,
                Err(crate::error::Error::CostCap { .. })
            ));
            assert_eq!(probe.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        });
    }

    // ========================================================================
    // Api enum: FromStr + Display round-trips
    // ========================================================================

    #[test]
    fn api_from_str_known_variants() {
        let cases = [
            ("anthropic-messages", Api::AnthropicMessages),
            ("openai-completions", Api::OpenAICompletions),
            ("openai-responses", Api::OpenAIResponses),
            ("openai-codex-responses", Api::OpenAICodexResponses),
            ("azure-openai-responses", Api::AzureOpenAIResponses),
            ("bedrock-converse-stream", Api::BedrockConverseStream),
            ("google-generative-ai", Api::GoogleGenerativeAI),
            ("google-gemini-cli", Api::GoogleGeminiCli),
            ("google-vertex", Api::GoogleVertex),
        ];
        for (input, expected) in &cases {
            let parsed: Api = input.parse().unwrap();
            assert_eq!(&parsed, expected, "from_str({input})");
        }
    }

    #[test]
    fn api_display_known_variants() {
        let cases = [
            (Api::AnthropicMessages, "anthropic-messages"),
            (Api::OpenAICompletions, "openai-completions"),
            (Api::OpenAIResponses, "openai-responses"),
            (Api::OpenAICodexResponses, "openai-codex-responses"),
            (Api::AzureOpenAIResponses, "azure-openai-responses"),
            (Api::BedrockConverseStream, "bedrock-converse-stream"),
            (Api::GoogleGenerativeAI, "google-generative-ai"),
            (Api::GoogleGeminiCli, "google-gemini-cli"),
            (Api::GoogleVertex, "google-vertex"),
        ];
        for (variant, expected) in &cases {
            assert_eq!(&variant.to_string(), expected, "display for {variant:?}");
        }
    }

    #[test]
    fn api_round_trip_all_known() {
        let variants = [
            Api::AnthropicMessages,
            Api::OpenAICompletions,
            Api::OpenAIResponses,
            Api::OpenAICodexResponses,
            Api::AzureOpenAIResponses,
            Api::BedrockConverseStream,
            Api::GoogleGenerativeAI,
            Api::GoogleGeminiCli,
            Api::GoogleVertex,
        ];
        for variant in &variants {
            let s = variant.to_string();
            let parsed: Api = s.parse().unwrap();
            assert_eq!(&parsed, variant, "round-trip failed for {variant:?} -> {s}");
        }
    }

    #[test]
    fn api_custom_variant() {
        let parsed: Api = "my-custom-api".parse().unwrap();
        assert_eq!(parsed, Api::Custom("my-custom-api".to_string()));
        assert_eq!(parsed.to_string(), "my-custom-api");
    }

    #[test]
    fn api_empty_string_rejected() {
        let result: Result<Api, _> = "".parse();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "API identifier cannot be empty");
    }

    // ========================================================================
    // KnownProvider enum: FromStr + Display round-trips
    // ========================================================================

    #[test]
    fn provider_from_str_known_variants() {
        let cases = [
            ("anthropic", KnownProvider::Anthropic),
            ("openai", KnownProvider::OpenAI),
            ("google", KnownProvider::Google),
            ("google-vertex", KnownProvider::GoogleVertex),
            ("amazon-bedrock", KnownProvider::AmazonBedrock),
            ("azure-openai", KnownProvider::AzureOpenAI),
            ("azure", KnownProvider::AzureOpenAI),
            ("azure-cognitive-services", KnownProvider::AzureOpenAI),
            ("github-copilot", KnownProvider::GithubCopilot),
            ("xai", KnownProvider::XAI),
            ("groq", KnownProvider::Groq),
            ("cerebras", KnownProvider::Cerebras),
            ("openrouter", KnownProvider::OpenRouter),
            ("mistral", KnownProvider::Mistral),
        ];
        for (input, expected) in &cases {
            let parsed: KnownProvider = input.parse().unwrap();
            assert_eq!(&parsed, expected, "from_str({input})");
        }
    }

    #[test]
    fn provider_display_known_variants() {
        let cases = [
            (KnownProvider::Anthropic, "anthropic"),
            (KnownProvider::OpenAI, "openai"),
            (KnownProvider::Google, "google"),
            (KnownProvider::GoogleVertex, "google-vertex"),
            (KnownProvider::AmazonBedrock, "amazon-bedrock"),
            (KnownProvider::AzureOpenAI, "azure-openai"),
            (KnownProvider::GithubCopilot, "github-copilot"),
            (KnownProvider::XAI, "xai"),
            (KnownProvider::Groq, "groq"),
            (KnownProvider::Cerebras, "cerebras"),
            (KnownProvider::OpenRouter, "openrouter"),
            (KnownProvider::Mistral, "mistral"),
        ];
        for (variant, expected) in &cases {
            assert_eq!(&variant.to_string(), expected, "display for {variant:?}");
        }
    }

    #[test]
    fn provider_round_trip_all_known() {
        let variants = [
            KnownProvider::Anthropic,
            KnownProvider::OpenAI,
            KnownProvider::Google,
            KnownProvider::GoogleVertex,
            KnownProvider::AmazonBedrock,
            KnownProvider::AzureOpenAI,
            KnownProvider::GithubCopilot,
            KnownProvider::XAI,
            KnownProvider::Groq,
            KnownProvider::Cerebras,
            KnownProvider::OpenRouter,
            KnownProvider::Mistral,
        ];
        for variant in &variants {
            let s = variant.to_string();
            let parsed: KnownProvider = s.parse().unwrap();
            assert_eq!(&parsed, variant, "round-trip failed for {variant:?} -> {s}");
        }
    }

    #[test]
    fn provider_custom_variant() {
        let parsed: KnownProvider = "my-custom-provider".parse().unwrap();
        assert_eq!(
            parsed,
            KnownProvider::Custom("my-custom-provider".to_string())
        );
        assert_eq!(parsed.to_string(), "my-custom-provider");
    }

    #[test]
    fn provider_empty_string_rejected() {
        let result: Result<KnownProvider, _> = "".parse();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Provider identifier cannot be empty");
    }

    // ========================================================================
    // Model::calculate_cost
    // ========================================================================

    fn test_model() -> Model {
        Model {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            reasoning: false,
            input: vec![InputType::Text],
            cost: ModelCost {
                input: 3.0,   // $3 per million input tokens
                output: 15.0, // $15 per million output tokens
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: 200_000,
            max_tokens: 8192,
            headers: HashMap::new(),
        }
    }

    /// gh #214: a distinct name leads the label and the identity follows; a
    /// name that is empty or just repeats the id adds nothing.
    #[test]
    fn display_label_shows_a_distinct_name_and_keeps_the_identity() {
        let mut model = test_model();
        assert_eq!(model.display_label(), "anthropic/test-model (Test Model)");
        model.name = "test-model".to_string();
        assert_eq!(model.display_label(), "anthropic/test-model");
        model.name = "  ".to_string();
        assert_eq!(model.display_label(), "anthropic/test-model");
        model.name = "anthropic/test-model".to_string();
        assert_eq!(model.display_label(), "anthropic/test-model");
    }

    /// gh #214 under the OMP rule: the name wins, the id is the fallback, and
    /// only a leading "Claude " is trimmed.
    #[test]
    fn status_label_prefers_the_name_like_omp() {
        let mut model = test_model();
        assert_eq!(model.status_label(), "Test Model");
        model.name = "Claude Opus 4.6".to_string();
        assert_eq!(model.status_label(), "Opus 4.6");
        model.name = "Not Claude Opus".to_string();
        assert_eq!(model.status_label(), "Not Claude Opus");
        model.name = "  ".to_string();
        assert_eq!(model.status_label(), "test-model");
    }

    /// gh #221: usage pricing fills every cost component from catalog rates.
    #[test]
    fn price_usage_fills_components_and_total_from_catalog_rates() {
        let model = test_model();
        let mut usage = crate::model::Usage {
            input: 1000,
            output: 500,
            cache_read: 2000,
            cache_write: 1000,
            total_tokens: 4500,
            cost: crate::model::Cost::default(),
        };
        model.cost.price_usage(&mut usage);
        let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
        assert!(close(usage.cost.input, 0.003));
        assert!(close(usage.cost.output, 0.0075));
        assert!(close(usage.cost.cache_read, 0.0006));
        assert!(close(usage.cost.cache_write, 0.00375));
        assert!(close(
            usage.cost.total,
            model.calculate_cost(1000, 500, 2000, 1000)
        ));
        assert!(model.cost.is_priced());
        assert!(
            !ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            }
            .is_priced()
        );
    }

    /// gh #221: a provider-reported total (OpenRouter `usage.cost`) is the
    /// billed amount and wins over the catalog sum; components still come
    /// from the catalog so the breakdown stays available.
    #[test]
    fn price_usage_keeps_provider_reported_total() {
        let model = test_model();
        let mut usage = crate::model::Usage {
            input: 1000,
            output: 500,
            cache_read: 0,
            cache_write: 0,
            total_tokens: 1500,
            cost: crate::model::Cost {
                total: 0.0421,
                ..crate::model::Cost::default()
            },
        };
        model.cost.price_usage(&mut usage);
        assert!((usage.cost.total - 0.0421).abs() < 1e-12);
        assert!((usage.cost.input - 0.003).abs() < 1e-12);
        assert!((usage.cost.output - 0.0075).abs() < 1e-12);
    }

    #[test]
    fn calculate_cost_basic() {
        let model = test_model();
        // 1000 input tokens at $3/M = $0.003
        // 500 output tokens at $15/M = $0.0075
        let cost = model.calculate_cost(1000, 500, 0, 0);
        let input_expected = (3.0 / 1_000_000.0) * 1000.0;
        let output_expected = (15.0 / 1_000_000.0) * 500.0;
        let expected = input_expected + output_expected;
        assert!(
            (cost - expected).abs() < f64::EPSILON,
            "expected {expected}, got {cost}"
        );
    }

    #[test]
    fn calculate_cost_with_cache() {
        let model = test_model();
        let cost = model.calculate_cost(1000, 500, 2000, 1000);
        let input_expected = (3.0 / 1_000_000.0) * 1000.0;
        let output_expected = (15.0 / 1_000_000.0) * 500.0;
        let cache_read_expected = (0.3 / 1_000_000.0) * 2000.0;
        let cache_write_expected = (3.75 / 1_000_000.0) * 1000.0;
        let expected =
            input_expected + output_expected + cache_read_expected + cache_write_expected;
        assert!(
            (cost - expected).abs() < 1e-12,
            "expected {expected}, got {cost}"
        );
    }

    #[test]
    fn calculate_cost_zero_tokens() {
        let model = test_model();
        let cost = model.calculate_cost(0, 0, 0, 0);
        assert!((cost).abs() < f64::EPSILON, "zero tokens should cost $0");
    }

    #[test]
    fn calculate_cost_large_token_count() {
        let model = test_model();
        // 1 million tokens each
        let cost = model.calculate_cost(1_000_000, 1_000_000, 0, 0);
        let expected = 3.0 + 15.0; // $3 input + $15 output
        assert!(
            (cost - expected).abs() < 1e-10,
            "expected {expected}, got {cost}"
        );
    }

    // ========================================================================
    // Default values
    // ========================================================================

    #[test]
    fn thinking_budgets_default() {
        let budgets = ThinkingBudgets::default();
        assert_eq!(budgets.minimal, 1024);
        assert_eq!(budgets.low, 2048);
        assert_eq!(budgets.medium, 8192);
        assert_eq!(budgets.high, 16384);
        assert_eq!(budgets.xhigh, 32768);
    }

    #[test]
    fn cache_retention_default_is_none() {
        assert_eq!(CacheRetention::default(), CacheRetention::None);
    }

    #[test]
    fn stream_options_default() {
        let opts = StreamOptions::default();
        assert!(opts.temperature.is_none());
        assert!(opts.max_tokens.is_none());
        assert!(opts.api_key.is_none());
        assert_eq!(opts.cache_retention, CacheRetention::None);
        assert!(opts.session_id.is_none());
        assert!(opts.headers.is_empty());
        assert!(opts.thinking_level.is_none());
        assert!(opts.thinking_budgets.is_none());
    }

    #[test]
    fn context_default() {
        let ctx = Context::default();
        assert!(ctx.system_prompt.is_none());
        assert!(ctx.messages.is_empty());
        assert!(ctx.tools.is_empty());
    }

    // ========================================================================
    // InputType serde
    // ========================================================================

    #[test]
    fn input_type_serialization() {
        let text_json = serde_json::to_string(&InputType::Text).unwrap();
        assert_eq!(text_json, "\"text\"");

        let image_json = serde_json::to_string(&InputType::Image).unwrap();
        assert_eq!(image_json, "\"image\"");

        let text: InputType = serde_json::from_str("\"text\"").unwrap();
        assert_eq!(text, InputType::Text);

        let image: InputType = serde_json::from_str("\"image\"").unwrap();
        assert_eq!(image, InputType::Image);
    }

    #[test]
    fn input_type_video_audio_serde_and_parse() {
        assert_eq!(
            serde_json::to_string(&InputType::Video).unwrap(),
            "\"video\""
        );
        assert_eq!(
            serde_json::to_string(&InputType::Audio).unwrap(),
            "\"audio\""
        );
        let video: InputType = serde_json::from_str("\"video\"").unwrap();
        assert_eq!(video, InputType::Video);
        for kind in [
            InputType::Text,
            InputType::Image,
            InputType::Video,
            InputType::Audio,
        ] {
            assert_eq!(InputType::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(InputType::parse(" AUDIO "), Some(InputType::Audio));
        assert_eq!(InputType::parse("hologram"), None);
    }

    // ========================================================================
    // ModelCost serde
    // ========================================================================

    #[test]
    fn model_cost_camel_case_serialization() {
        let cost = ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        };
        let json = serde_json::to_string(&cost).unwrap();
        assert!(
            json.contains("\"cacheRead\""),
            "should use camelCase: {json}"
        );
        assert!(
            json.contains("\"cacheWrite\""),
            "should use camelCase: {json}"
        );

        let parsed: ModelCost = serde_json::from_str(&json).unwrap();
        assert!((parsed.input - 3.0).abs() < f64::EPSILON);
        assert!((parsed.cache_read - 0.3).abs() < f64::EPSILON);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        fn arb_model(rate: f64) -> Model {
            Model {
                id: "m".to_string(),
                name: "m".to_string(),
                api: "anthropic-messages".to_string(),
                provider: "test".to_string(),
                base_url: String::new(),
                reasoning: false,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: rate,
                    output: rate,
                    cache_read: rate,
                    cache_write: rate,
                },
                context_window: 128_000,
                max_tokens: 8192,
                headers: HashMap::new(),
            }
        }

        // ====================================================================
        // calculate_cost
        // ====================================================================

        proptest! {
            #[test]
            fn cost_zero_tokens_is_zero(rate in 0.0f64..1000.0) {
                let m = arb_model(rate);
                let cost = m.calculate_cost(0, 0, 0, 0);
                assert!((cost).abs() < f64::EPSILON);
            }

            #[test]
            fn cost_non_negative(
                rate in 0.0f64..100.0,
                input in 0u64..10_000_000,
                output in 0u64..10_000_000,
                cr in 0u64..10_000_000,
                cw in 0u64..10_000_000,
            ) {
                let m = arb_model(rate);
                assert!(m.calculate_cost(input, output, cr, cw) >= 0.0);
            }

            #[test]
            fn cost_linearity(
                rate in 0.001f64..50.0,
                tokens in 1u64..1_000_000,
            ) {
                let m = arb_model(rate);
                let single = m.calculate_cost(tokens, 0, 0, 0);
                let double = m.calculate_cost(tokens * 2, 0, 0, 0);
                assert!((double - single * 2.0).abs() < 1e-6,
                    "doubling tokens should double cost: single={single}, double={double}");
            }

            #[test]
            fn cost_additivity(
                rate in 0.001f64..50.0,
                input in 0u64..1_000_000,
                output in 0u64..1_000_000,
            ) {
                let m = arb_model(rate);
                let combined = m.calculate_cost(input, output, 0, 0);
                let separate = m.calculate_cost(input, 0, 0, 0)
                    + m.calculate_cost(0, output, 0, 0);
                assert!((combined - separate).abs() < 1e-10,
                    "cost should be additive: combined={combined}, separate={separate}");
            }
        }

        // ====================================================================
        // Api FromStr + Display round-trip
        // ====================================================================

        proptest! {
            #[test]
            fn api_custom_round_trip(s in "[a-z][a-z0-9-]{0,20}") {
                let known = [
                    "anthropic-messages", "openai-completions", "openai-responses", "openai-codex-responses",
                    "azure-openai-responses", "bedrock-converse-stream",
                    "google-generative-ai", "google-gemini-cli", "google-vertex",
                ];
                if !known.contains(&s.as_str()) {
                    let parsed: Api = s.parse().unwrap();
                    assert_eq!(parsed.to_string(), s);
                }
            }

            #[test]
            fn api_never_panics(s in ".*") {
                let _ = s.parse::<Api>(); // must not panic
            }

            #[test]
            fn api_empty_always_rejected(ws in "[ \t]*") {
                if ws.is_empty() {
                    assert!(ws.parse::<Api>().is_err());
                }
            }
        }

        // ====================================================================
        // KnownProvider FromStr + Display round-trip
        // ====================================================================

        proptest! {
            #[test]
            fn provider_custom_round_trip(s in "[a-z][a-z0-9-]{0,20}") {
                let known = [
                    "anthropic", "openai", "google", "google-vertex",
                    "amazon-bedrock", "azure-openai", "azure",
                    "azure-cognitive-services", "github-copilot", "xai",
                    "groq", "cerebras", "openrouter", "mistral",
                ];
                if !known.contains(&s.as_str()) {
                    let parsed: KnownProvider = s.parse().unwrap();
                    assert_eq!(parsed.to_string(), s);
                }
            }

            #[test]
            fn provider_never_panics(s in ".*") {
                let _ = s.parse::<KnownProvider>(); // must not panic
            }
        }

        // ====================================================================
        // ModelCost serde round-trip
        // ====================================================================

        proptest! {
            #[test]
            fn model_cost_serde_round_trip(
                input in 0.0f64..1000.0,
                output in 0.0f64..1000.0,
                cr in 0.0f64..1000.0,
                cw in 0.0f64..1000.0,
            ) {
                let cost = ModelCost { input, output, cache_read: cr, cache_write: cw };
                let json = serde_json::to_string(&cost).unwrap();
                let parsed: ModelCost = serde_json::from_str(&json).unwrap();
                assert!((parsed.input - input).abs() < 1e-10);
                assert!((parsed.output - output).abs() < 1e-10);
                assert!((parsed.cache_read - cr).abs() < 1e-10);
                assert!((parsed.cache_write - cw).abs() < 1e-10);
            }
        }
    }
}
