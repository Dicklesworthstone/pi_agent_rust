//! Dynamic provider-model discovery with in-memory TTL caching and a
//! static-registry fallback.
//!
//! This module implements GitHub issue #92: the runtime can query a
//! provider's live model catalog instead of relying solely on the bundled
//! `built_in_models()` snapshot.  The fetch is performed against the
//! widely-implemented `GET /v1/models` endpoint (OpenAI specification), which
//! is honoured by every provider whose [`ProviderRoutingDefaults::base_url`]
//! already points at an OpenAI-compatible root (OpenAI, Groq, DeepSeek,
//! OpenRouter, Together, Moonshot, Mistral, Fireworks, Perplexity, xAI, etc.).
//!
//! ## Cache strategy
//!
//! A process-local cache (`std::sync::Mutex<HashMap<…>>` behind a
//! `std::sync::OnceLock`) keys results by a SHA-256 identity over the
//! canonical provider, effective route/headers, and credential actually sent.
//! Provider aliases share an entry, while catalogs from distinct routes or
//! accounts never bleed together; keyless providers use one
//! credential-independent entry per route. The cache is capped and entries
//! expire after [`MODEL_CACHE_TTL`] (5 minutes). It benefits repeated calls in
//! one long-lived process; separate CLI invocations never share it. Hits within
//! the TTL window do **not** issue a network call. Setting
//! `PI_DISABLE_MODEL_CACHE=1` (or `true`/`yes`/`on`) bypasses both the read
//! and write paths for debugging. [`refresh_provider_models`] forces a strict
//! live refetch regardless of cache state and returns an error rather than a
//! static fallback when that refresh fails.
//!
//! ## Fallback
//!
//! When the live fetch fails (network error, non-2xx response, unparseable
//! body), the function logs a `tracing::warn!` describing the failure and
//! returns the static model IDs known to [`ModelRegistry`]. Invalid, unsafe, or
//! resource-exceeding local catalog data is rejected instead of being emitted
//! as a misleading fallback.
//!
//! ## Extending to non-OpenAI endpoints
//!
//! Providers that do not speak `/v1/models` (e.g. Google Gemini's
//! `/v1beta/models?key=…`, Vertex AI, Bedrock listing APIs, Anthropic's
//! `x-api-key` + `anthropic-version` flavoured `/v1/models`) can be added by
//! branching inside [`fetch_live_models`] on the canonical provider id and
//! supplying a bespoke request builder + JSON shape parser.  Keep the cache
//! key + fallback paths unchanged; only the network call shape varies.

use std::collections::{HashMap, HashSet};
#[cfg(any(unix, windows))]
use std::fs::File;
#[cfg(any(unix, windows))]
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::{SeqAccess, Visitor};
use sha2::{Digest, Sha256};
#[cfg(not(unix))]
use tempfile::NamedTempFile;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::http::client::{Client, effective_default_request_timeout};
#[cfg(test)]
use crate::models::FETCHED_MODELS_SCHEMA;
use crate::models::{
    MAX_FETCHED_CATALOG_BYTES, MAX_FETCHED_MODEL_BYTES_PER_PROVIDER, MAX_FETCHED_MODEL_ID_BYTES,
    MAX_FETCHED_MODELS_PER_PROVIDER, MAX_FETCHED_PROVIDER_ID_BYTES, ModelCatalogProviderConfig,
    ModelRegistry, PersistedFetchedCatalog, PersistedFetchedModel, PersistedFetchedProvider,
    PreparedModelCatalogProviderConfig, canonicalize_model_id_for_provider, default_models_path,
    effective_model_catalog_api_key, ensure_model_catalog_persistence_access, fetched_models_path,
    is_safe_model_catalog_identifier, model_catalog_provider_route_shape,
    model_catalog_route_fingerprint, model_catalog_route_is_persistable, normalized_registry_key,
    parse_persisted_fetched_catalog, prepare_model_catalog_provider_config,
    validate_persisted_fetched_catalog,
};
use crate::provider_metadata::{canonical_provider_id, provider_routing_defaults};
use crate::providers::normalize_openai_base;

/// TTL applied to every cache entry.  Five minutes balances staleness against
/// rate-limit pressure on provider model catalogs.
pub const MODEL_CACHE_TTL: Duration = Duration::from_mins(5);
const MODEL_CACHE_MAX_ENTRIES: usize = 16;
const MODEL_CACHE_MAX_MODEL_ID_BYTES: usize = 8 * 1024 * 1024;

/// Environment variable that disables the cache entirely.  Useful for
/// debugging and for ad-hoc verification of provider catalog changes without
/// restarting the process.
pub const DISABLE_CACHE_ENV: &str = "PI_DISABLE_MODEL_CACHE";

#[derive(Debug, Clone)]
struct CacheEntry {
    models: Vec<String>,
    fetched_at_unix_ms: u64,
    inserted: Instant,
}

/// Provenance for a model catalog returned by dynamic discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCatalogSource {
    /// The provider answered the network request in this call.
    Live,
    /// A successful earlier live response was reused from the process cache.
    Cache,
    /// Live discovery failed and the bundled/on-disk static registry was used.
    StaticFallback,
}

/// A discovered provider catalog together with its non-secret provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModelCatalog {
    provider: String,
    models: Vec<String>,
    source: ModelCatalogSource,
    route_fingerprint: Option<String>,
    route_persistable: bool,
    fetched_at_unix_ms: Option<u64>,
}

impl ProviderModelCatalog {
    /// Canonical provider ID whose endpoint produced this catalog.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Model IDs in the discovered catalog.
    pub fn models(&self) -> &[String] {
        &self.models
    }

    /// Whether the catalog came from a live response, its verified cache, or
    /// the static fallback.
    pub const fn source(&self) -> ModelCatalogSource {
        self.source
    }

    /// Consume the catalog and return its model IDs.
    pub fn into_models(self) -> Vec<String> {
        self.models
    }
}

/// A resolved model-catalog route whose fallback credential remains deferred.
///
/// CLI callers can inspect [`Self::requires_runtime_api_key`] before touching
/// auth storage. Route header values are resolved only once, and a configured
/// `models.json` fallback key is evaluated only if runtime resolution is both
/// necessary and unsuccessful.
#[derive(Debug)]
pub struct ProviderModelCatalogFetchPlan {
    provider: String,
    route: Option<PreparedModelCatalogProviderConfig>,
}

impl ProviderModelCatalogFetchPlan {
    /// Whether this route needs Pi to resolve a runtime credential for a
    /// generated Authorization header.
    pub fn requires_runtime_api_key(&self) -> bool {
        self.route
            .as_ref()
            .is_some_and(PreparedModelCatalogProviderConfig::requires_runtime_api_key)
    }

    /// Execute this prepared fetch, optionally requiring a strict live refresh.
    pub async fn fetch(self, api_key: &str, refresh: bool) -> Result<ProviderModelCatalog> {
        let Self { provider, route } = self;
        let route = route.map(|prepared| prepared.into_route(api_key.trim().is_empty()));
        if refresh {
            let route = route.ok_or_else(|| {
                Error::api(format!(
                    "provider {provider:?} has no built-in or models.json routing configuration"
                ))
            })?;
            refresh_provider_model_catalog_with_route(&provider, api_key, route).await
        } else {
            fetch_provider_model_catalog_with_route(&provider, api_key, route).await
        }
    }
}

fn cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_disabled() -> bool {
    std::env::var(DISABLE_CACHE_ENV).is_ok_and(|raw| {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn canonical_provider_key(provider: &str) -> String {
    canonical_provider_id(provider)
        .unwrap_or_else(|| provider.trim())
        .to_ascii_lowercase()
}

fn validate_provider_id(provider: &str) -> Result<&str> {
    let provider = provider.trim();
    if !is_safe_model_catalog_identifier(provider, MAX_FETCHED_PROVIDER_ID_BYTES) {
        return Err(Error::validation(format!(
            "provider must be a non-empty printable-ASCII ID of at most {MAX_FETCHED_PROVIDER_ID_BYTES} bytes"
        )));
    }
    Ok(provider)
}

fn sorted_route_headers(route: &ModelCatalogProviderConfig) -> Vec<(&str, &str)> {
    let mut headers = route
        .headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    headers.sort_unstable_by(|(left, _), (right, _)| {
        left.to_ascii_lowercase()
            .cmp(&right.to_ascii_lowercase())
            .then_with(|| left.cmp(right))
    });
    headers
}

fn validate_route_headers(route: &ModelCatalogProviderConfig) -> Result<()> {
    let mut names = HashSet::with_capacity(route.headers.len());
    for name in route.headers.keys() {
        if !names.insert(name.to_ascii_lowercase()) {
            return Err(Error::config(format!(
                "model catalog route contains duplicate case-insensitive HTTP header name {name:?}"
            )));
        }
    }
    Ok(())
}

fn custom_authorization_header(route: &ModelCatalogProviderConfig) -> Option<&str> {
    route.headers.iter().find_map(|(name, value)| {
        name.eq_ignore_ascii_case("Authorization")
            .then_some(value.trim())
            .filter(|value| !value.is_empty())
    })
}

fn cache_key(provider: &str, api_key: &str, route: &ModelCatalogProviderConfig) -> String {
    let mut hasher = Sha256::new();
    let route_fingerprint = model_catalog_route_fingerprint(provider, route);
    let transmitted_api_key = if route.auth_header && custom_authorization_header(route).is_none() {
        api_key.trim()
    } else {
        ""
    };
    for component in [
        b"pi.models.fetch-cache.v1".as_slice(),
        route_fingerprint.as_bytes(),
        route.base_url.as_bytes(),
        route.api.as_bytes(),
        transmitted_api_key.as_bytes(),
    ] {
        hasher.update((component.len() as u64).to_le_bytes());
        hasher.update(component);
    }
    for (name, value) in sorted_route_headers(route) {
        for component in [name.as_bytes(), value.as_bytes()] {
            hasher.update((component.len() as u64).to_le_bytes());
            hasher.update(component);
        }
    }
    format!(
        "{}:{:x}",
        canonical_provider_key(provider),
        hasher.finalize()
    )
}

fn normalize_model_ids(
    provider: &str,
    models: impl IntoIterator<Item = String>,
) -> Result<Vec<String>> {
    let mut model_ids = Vec::new();
    for model in models {
        let model = model.trim();
        if model.is_empty() {
            continue;
        }
        if !is_safe_model_catalog_identifier(model, MAX_FETCHED_MODEL_ID_BYTES) {
            return Err(Error::validation(format!(
                "provider {provider:?} returned a model ID that is not printable ASCII or exceeds {MAX_FETCHED_MODEL_ID_BYTES} bytes"
            )));
        }
        model_ids.push(canonicalize_model_id_for_provider(provider, model));
    }
    model_ids.sort_unstable_by(|left, right| {
        let left_identity = normalized_registry_key(provider, left);
        let right_identity = normalized_registry_key(provider, right);
        left_identity
            .cmp(&right_identity)
            .then_with(|| (left != &left_identity.1).cmp(&(right != &right_identity.1)))
            .then_with(|| left.cmp(right))
    });
    model_ids.dedup_by(|left, right| {
        normalized_registry_key(provider, left) == normalized_registry_key(provider, right)
    });
    if model_ids.len() > MAX_FETCHED_MODELS_PER_PROVIDER {
        return Err(Error::validation(format!(
            "provider {provider:?} returned {} distinct model IDs; maximum is {MAX_FETCHED_MODELS_PER_PROVIDER}",
            model_ids.len()
        )));
    }
    let total_model_id_bytes = model_ids
        .iter()
        .try_fold(0usize, |total, model| total.checked_add(model.len()))
        .ok_or_else(|| Error::validation("provider model-ID size overflow"))?;
    if total_model_id_bytes > MAX_FETCHED_MODEL_BYTES_PER_PROVIDER {
        return Err(Error::validation(format!(
            "provider {provider:?} returned {total_model_id_bytes} model-ID bytes; maximum is {MAX_FETCHED_MODEL_BYTES_PER_PROVIDER}"
        )));
    }
    Ok(model_ids)
}

fn cache_lookup(key: &str) -> Option<(Vec<String>, u64)> {
    let mut guard = cache().lock().ok()?;
    let now = Instant::now();
    if guard.get(key).is_some_and(|entry| {
        now.checked_duration_since(entry.inserted)
            .is_none_or(|elapsed| elapsed >= MODEL_CACHE_TTL)
    }) {
        guard.remove(key);
        return None;
    }
    guard
        .get(key)
        .map(|entry| (entry.models.clone(), entry.fetched_at_unix_ms))
}

fn model_id_bytes(models: &[String]) -> usize {
    models
        .iter()
        .fold(0usize, |total, model| total.saturating_add(model.len()))
}

fn cache_store(key: String, models: Vec<String>, fetched_at_unix_ms: u64) {
    let incoming_bytes = model_id_bytes(&models);
    if incoming_bytes > MODEL_CACHE_MAX_MODEL_ID_BYTES {
        return;
    }
    if let Ok(mut guard) = cache().lock() {
        let now = Instant::now();
        guard.retain(|_, entry| {
            now.checked_duration_since(entry.inserted)
                .is_some_and(|elapsed| elapsed < MODEL_CACHE_TTL)
        });
        guard.remove(&key);
        while guard.len() >= MODEL_CACHE_MAX_ENTRIES
            || guard.values().fold(incoming_bytes, |total, entry| {
                total.saturating_add(model_id_bytes(&entry.models))
            }) > MODEL_CACHE_MAX_MODEL_ID_BYTES
        {
            let Some(oldest_key) = guard
                .iter()
                .min_by(|(left_key, left), (right_key, right)| {
                    left.inserted
                        .cmp(&right.inserted)
                        .then_with(|| left_key.cmp(right_key))
                })
                .map(|(oldest_key, _)| oldest_key.clone())
            else {
                break;
            };
            guard.remove(&oldest_key);
        }
        guard.insert(
            key,
            CacheEntry {
                models,
                fetched_at_unix_ms,
                inserted: now,
            },
        );
    }
}

/// Clear the entire in-memory cache.  Primarily intended for tests; callers
/// who only want to invalidate a single provider should prefer
/// [`refresh_provider_models`].
pub fn clear_model_cache() {
    if let Ok(mut guard) = cache().lock() {
        guard.clear();
    }
}

fn current_unix_timestamp_ms() -> Result<u64> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| Error::api(format!("system clock precedes Unix epoch: {error}")))?;
    u64::try_from(elapsed.as_millis())
        .map_err(|_| Error::api("current Unix timestamp does not fit in u64 milliseconds"))
}

/// Fetch the live model catalog for `provider`, returning cached results when fresh.
///
/// On any failure to talk to the provider, fall back to the bundled
/// static registry and log a warning so operators can see why the dynamic
/// path degraded.
///
/// `api_key` should be the user's credential for providers that require one.
///
/// An empty value skips the live call for authenticated providers, while
/// keyless local OpenAI-compatible providers may still be queried. A safe
/// static registry can return an empty `Vec`; malformed or resource-exceeding
/// local catalog data is returned as an error.
pub async fn fetch_provider_models(provider: &str, api_key: &str) -> Result<Vec<String>> {
    Ok(fetch_provider_model_catalog(provider, api_key)
        .await?
        .into_models())
}

/// Return whether `provider` has a built-in or `models.json` route suitable
/// for live model discovery, without resolving credentials or header values.
pub fn provider_model_catalog_route_is_configured(provider: &str) -> Result<bool> {
    let provider = validate_provider_id(provider)?;
    let models_path = default_models_path(&Config::global_dir());
    provider_model_catalog_route_is_configured_at_path(provider, &models_path)
}

fn provider_model_catalog_route_is_configured_at_path(
    provider: &str,
    models_path: &Path,
) -> Result<bool> {
    let Some((base_url, api)) = model_catalog_provider_route_shape(provider, models_path)? else {
        return Ok(false);
    };
    Ok(openai_compat_models_url(&base_url, &api).is_some())
}

/// Resolve a provider's catalog route while leaving every unused credential
/// source untouched.
pub fn prepare_provider_model_catalog_fetch(
    provider: &str,
) -> Result<ProviderModelCatalogFetchPlan> {
    let provider = validate_provider_id(provider)?;
    let models_path = default_models_path(&Config::global_dir());
    let route = prepare_model_catalog_provider_config(provider, &models_path)?;
    Ok(ProviderModelCatalogFetchPlan {
        provider: provider.to_string(),
        route,
    })
}

/// Fetch a provider catalog while retaining whether the rows came from a
/// successful live call, the in-process cache, or the static fallback.
pub async fn fetch_provider_model_catalog(
    provider: &str,
    api_key: &str,
) -> Result<ProviderModelCatalog> {
    prepare_provider_model_catalog_fetch(provider)?
        .fetch(api_key, false)
        .await
}

async fn fetch_provider_model_catalog_with_route(
    provider: &str,
    api_key: &str,
    route: Option<ModelCatalogProviderConfig>,
) -> Result<ProviderModelCatalog> {
    let canonical_provider = canonical_provider_key(provider);
    let effective_api_key = route.as_ref().map_or_else(
        || api_key.trim().to_string(),
        |route| effective_model_catalog_api_key(api_key, route),
    );
    let route_fingerprint = route
        .as_ref()
        .map(|route| model_catalog_route_fingerprint(provider, route));
    let route_persistable = route
        .as_ref()
        .is_some_and(model_catalog_route_is_persistable);
    let key = route
        .as_ref()
        .map(|route| cache_key(provider, &effective_api_key, route));

    if !cache_disabled()
        && let Some(key) = key.as_deref()
        && let Some((cached, fetched_at_unix_ms)) = cache_lookup(key)
    {
        tracing::debug!(
            provider = %canonical_provider_key(provider),
            count = cached.len(),
            "model cache hit"
        );
        return Ok(ProviderModelCatalog {
            provider: canonical_provider,
            models: cached,
            source: ModelCatalogSource::Cache,
            route_fingerprint,
            route_persistable,
            fetched_at_unix_ms: Some(fetched_at_unix_ms),
        });
    }

    fetch_and_cache(
        provider,
        key.as_deref(),
        &effective_api_key,
        route.as_ref(),
        route_fingerprint.as_deref(),
    )
    .await
}

/// Force a refresh, bypassing any cached entry. Only a successful, non-empty
/// live response replaces the cache entry; failures are returned to the caller.
pub async fn refresh_provider_models(provider: &str, api_key: &str) -> Result<Vec<String>> {
    Ok(refresh_provider_model_catalog(provider, api_key)
        .await?
        .into_models())
}

/// Force a genuinely live refresh, bypassing the cache and rejecting network,
/// authentication, parse, and empty-catalog failures.
///
/// This strict behavior prevents callers from mistaking a static fallback for
/// a fresh provider response.
pub async fn refresh_provider_model_catalog(
    provider: &str,
    api_key: &str,
) -> Result<ProviderModelCatalog> {
    prepare_provider_model_catalog_fetch(provider)?
        .fetch(api_key, true)
        .await
}

async fn refresh_provider_model_catalog_with_route(
    provider: &str,
    api_key: &str,
    route: ModelCatalogProviderConfig,
) -> Result<ProviderModelCatalog> {
    let effective_api_key = effective_model_catalog_api_key(api_key, &route);
    let key = cache_key(provider, &effective_api_key, &route);
    let route_fingerprint = model_catalog_route_fingerprint(provider, &route);
    let route_persistable = model_catalog_route_is_persistable(&route);
    let live = fetch_live_models(provider, &effective_api_key, &route).await?;
    if live.is_empty() {
        return Err(Error::api(format!(
            "live model fetch for {provider:?} returned an empty catalog"
        )));
    }
    let fetched_at_unix_ms = current_unix_timestamp_ms()?;
    if !cache_disabled() {
        cache_store(key, live.clone(), fetched_at_unix_ms);
    }
    Ok(ProviderModelCatalog {
        provider: canonical_provider_key(provider),
        models: live,
        source: ModelCatalogSource::Live,
        route_fingerprint: Some(route_fingerprint),
        route_persistable,
        fetched_at_unix_ms: Some(fetched_at_unix_ms),
    })
}

async fn fetch_and_cache(
    provider: &str,
    key: Option<&str>,
    api_key: &str,
    route: Option<&ModelCatalogProviderConfig>,
    route_fingerprint: Option<&str>,
) -> Result<ProviderModelCatalog> {
    let canonical_provider = canonical_provider_key(provider);
    // Only cache results from a successful live fetch — caching the
    // static-registry fallback would pin a stale answer for 5 minutes and
    // silently swallow the next call even after the user adds the missing
    // API key. The fallback path stays correct (callers always get a list)
    // without poisoning the next live attempt.
    let live_result = match route {
        Some(route) => fetch_live_models(provider, api_key, route).await,
        None => Err(Error::api(format!(
            "provider {provider:?} has no built-in or models.json routing configuration"
        ))),
    };
    match live_result {
        Ok(live) if !live.is_empty() => {
            let fetched_at_unix_ms = current_unix_timestamp_ms()?;
            if !cache_disabled()
                && let Some(key) = key
            {
                cache_store(key.to_string(), live.clone(), fetched_at_unix_ms);
            }
            Ok(ProviderModelCatalog {
                provider: canonical_provider,
                models: live,
                source: ModelCatalogSource::Live,
                route_fingerprint: route_fingerprint.map(ToString::to_string),
                route_persistable: route.is_some_and(model_catalog_route_is_persistable),
                fetched_at_unix_ms: Some(fetched_at_unix_ms),
            })
        }
        Ok(_) => {
            tracing::warn!(
                provider = %canonical_provider,
                "live model fetch returned empty list; falling back to static registry (not cached)"
            );
            Ok(ProviderModelCatalog {
                provider: canonical_provider,
                models: static_registry_models(provider)?,
                source: ModelCatalogSource::StaticFallback,
                route_fingerprint: None,
                route_persistable: false,
                fetched_at_unix_ms: None,
            })
        }
        Err(err) => {
            tracing::warn!(
                provider = %canonical_provider,
                error = %err,
                "live model fetch failed; falling back to static registry (not cached)"
            );
            Ok(ProviderModelCatalog {
                provider: canonical_provider,
                models: static_registry_models(provider)?,
                source: ModelCatalogSource::StaticFallback,
                route_fingerprint: None,
                route_persistable: false,
                fetched_at_unix_ms: None,
            })
        }
    }
}

/// Return the static model IDs known to the bundled registry for `provider`.
///
/// Used as the fallback when a live fetch fails.  Loads the on-disk
/// `models.json` (if any) so user-defined catalog overrides are honoured.
pub fn static_registry_models(provider: &str) -> Result<Vec<String>> {
    let provider = validate_provider_id(provider)?;
    let models_path = Some(default_models_path(&Config::global_dir()));
    let registry = ModelRegistry::load_for_listing_with_credential_resolver(models_path, |_| None);
    if let Some(error) = registry.error() {
        return Err(Error::config(format!(
            "Failed to load static model registry for {provider:?}: {error}"
        )));
    }
    let canonical = canonical_provider_id(provider).unwrap_or(provider);
    let ids: Vec<String> = registry
        .models()
        .iter()
        .filter(|entry| {
            let entry_provider = entry.model.provider.as_str();
            entry_provider.eq_ignore_ascii_case(provider)
                || entry_provider.eq_ignore_ascii_case(canonical)
                || canonical_provider_id(entry_provider)
                    .is_some_and(|c| c.eq_ignore_ascii_case(canonical))
        })
        .map(|entry| entry.model.id.clone())
        .collect();
    normalize_model_ids(provider, ids).map_err(|error| {
        Error::config(format!(
            "Invalid static model registry for {provider:?}: {error}"
        ))
    })
}

/// Atomically persist one successfully discovered provider catalog beside the
/// user's `models.json`.
///
/// This never reads or modifies `models.json` itself. Static fallback rows are
/// rejected here, at the library boundary, so non-CLI callers cannot
/// accidentally turn bundled fallback data into a purportedly live generated
/// catalog.
///
/// Existing generated catalogs for other providers are retained.
///
/// The generated schema encodes provider/model IDs, a fetch timestamp, and a
/// SHA-256 endpoint/transport identity. Credential values, URL query values,
/// and header values are excluded so the digest cannot become an offline
/// credential verifier. Non-empty values outside recognized credential
/// channels make the route ineligible for persistence.
pub fn persist_provider_model_catalog(
    models_path: &Path,
    catalog: &ProviderModelCatalog,
) -> Result<PathBuf> {
    if matches!(catalog.source, ModelCatalogSource::StaticFallback) {
        return Err(Error::validation(format!(
            "refusing to persist the static model fallback for {:?}; a successful live or cached discovery is required",
            catalog.provider
        )));
    }
    if !catalog.route_persistable {
        return Err(Error::validation(format!(
            "refusing to persist model membership for {:?}: the catalog route contains a non-empty query/header value outside a recognized credential channel, so tenant or deployment identity cannot be bound without persisting an offline verifier",
            catalog.provider
        )));
    }
    let route_fingerprint = catalog.route_fingerprint.as_deref().ok_or_else(|| {
        Error::validation(format!(
            "refusing to persist model membership for {:?} without a verified endpoint/transport binding",
            catalog.provider
        ))
    })?;
    let fetched_at_unix_ms = catalog.fetched_at_unix_ms.ok_or_else(|| {
        Error::validation(format!(
            "refusing to persist model membership for {:?} without a verified fetch timestamp",
            catalog.provider
        ))
    })?;
    persist_provider_model_catalog_rows(
        models_path,
        &catalog.provider,
        &catalog.models,
        route_fingerprint,
        fetched_at_unix_ms,
    )
}

fn persist_provider_model_catalog_rows(
    models_path: &Path,
    provider: &str,
    models: &[String],
    route_fingerprint: &str,
    fetched_at_unix_ms: u64,
) -> Result<PathBuf> {
    persist_provider_model_catalog_rows_with_hook(
        models_path,
        provider,
        models,
        route_fingerprint,
        fetched_at_unix_ms,
        |_| Ok(()),
    )
}

fn persist_provider_model_catalog_rows_with_hook<F>(
    models_path: &Path,
    provider: &str,
    models: &[String],
    route_fingerprint: &str,
    fetched_at_unix_ms: u64,
    before_replace: F,
) -> Result<PathBuf>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    persist_provider_model_catalog_rows_with_hooks(
        models_path,
        provider,
        models,
        route_fingerprint,
        fetched_at_unix_ms,
        before_replace,
        |_| Ok(()),
    )
}

fn persist_provider_model_catalog_rows_with_hooks<F, G>(
    models_path: &Path,
    provider: &str,
    models: &[String],
    route_fingerprint: &str,
    fetched_at_unix_ms: u64,
    before_replace: F,
    after_replace: G,
) -> Result<PathBuf>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
    G: FnOnce(&Path) -> std::io::Result<()>,
{
    let provider = validate_provider_id(provider)?;

    let model_ids = normalize_model_ids(provider, models.iter().cloned())?;
    if model_ids.is_empty() {
        return Err(Error::validation(
            "refusing to persist an empty fetched model catalog",
        ));
    }

    let path = fetched_models_path(models_path);
    let parent = catalog_parent(&path);
    ensure_model_catalog_persistence_access(&path).map_err(|error| {
        Error::config(format!(
            "Fetched model catalog persistence preflight failed for {}: {error}",
            path.display()
        ))
    })?;

    let request = CatalogPersistenceRequest {
        path: &path,
        parent,
        provider,
        model_ids,
        route_fingerprint,
        fetched_at_unix_ms,
    };
    persist_provider_model_catalog_rows_platform(request, before_replace, after_replace)?;
    Ok(path)
}

struct CatalogPersistenceRequest<'a> {
    path: &'a Path,
    parent: &'a Path,
    provider: &'a str,
    model_ids: Vec<String>,
    route_fingerprint: &'a str,
    fetched_at_unix_ms: u64,
}

fn merge_provider_catalog(
    mut catalog: PersistedFetchedCatalog,
    provider: &str,
    model_ids: Vec<String>,
    route_fingerprint: &str,
    fetched_at_unix_ms: u64,
) -> Result<PersistedFetchedCatalog> {
    let provider = canonical_provider_key(provider);
    catalog
        .providers
        .retain(|existing, _| canonical_provider_key(existing) != provider);
    catalog.providers.insert(
        provider,
        PersistedFetchedProvider {
            route_fingerprint: route_fingerprint.to_string(),
            fetched_at_unix_ms,
            models: model_ids
                .into_iter()
                .map(|id| PersistedFetchedModel { id })
                .collect(),
        },
    );
    validate_persisted_fetched_catalog(&catalog)?;
    Ok(catalog)
}

fn fetched_catalog_persist_lock() -> &'static Mutex<()> {
    static PERSIST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    PERSIST_LOCK.get_or_init(|| Mutex::new(()))
}

fn parse_persisted_catalog_contents(
    path: &Path,
    contents: &str,
) -> Result<PersistedFetchedCatalog> {
    #[derive(Deserialize)]
    struct CatalogSchemaProbe {
        schema: String,
    }

    if serde_json::from_str::<CatalogSchemaProbe>(contents)
        .is_ok_and(|probe| probe.schema == "pi.models.fetched.v1")
    {
        let backup_path = path.with_extension("v1.backup.json");
        return Err(Error::config(format!(
            "Refusing to overwrite legacy generated model catalog {}: schema pi.models.fetched.v1 has no endpoint/transport provenance. Move it aside to {} as a backup, then rerun the verified live --fetch-models ... --refresh-models --persist-models command",
            path.display(),
            backup_path.display()
        )));
    }
    parse_persisted_fetched_catalog(contents).map_err(|error| {
        Error::config(format!(
            "Refusing to overwrite malformed or unrecognized fetched model catalog {}: {error}",
            path.display()
        ))
    })
}

#[cfg(any(test, not(any(unix, windows))))]
fn load_persisted_catalog(path: &Path) -> Result<PersistedFetchedCatalog> {
    let contents = match crate::models::read_generated_catalog(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PersistedFetchedCatalog::default());
        }
        Err(error) => {
            return Err(Error::config(format!(
                "Failed to read fetched model catalog {}: {error}",
                path.display()
            )));
        }
    };
    parse_persisted_catalog_contents(path, &contents)
}

fn serialize_persisted_catalog(catalog: &PersistedFetchedCatalog) -> Result<Vec<u8>> {
    let mut contents = serde_json::to_string_pretty(catalog).map_err(Error::from)?;
    contents.push('\n');
    if contents.len() > MAX_FETCHED_CATALOG_BYTES {
        return Err(Error::config(format!(
            "Refusing to persist generated model catalog: serialized size {} exceeds {MAX_FETCHED_CATALOG_BYTES} bytes",
            contents.len()
        )));
    }
    Ok(contents.into_bytes())
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnixCatalogTargetState {
    Missing,
    Existing { device: u64, inode: u64 },
}

#[cfg(unix)]
fn unix_catalog_target_state(metadata: &std::fs::Metadata) -> UnixCatalogTargetState {
    use std::os::unix::fs::MetadataExt as _;

    UnixCatalogTargetState::Existing {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(unix)]
fn catalog_target_parts(path: &Path) -> std::io::Result<(&Path, &std::ffi::OsStr)> {
    let parent = catalog_parent(path);
    let target_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "generated model catalog path has no filename: {}",
                path.display()
            ),
        )
    })?;
    Ok((parent, target_name))
}

#[cfg(unix)]
fn open_catalog_child_nofollow(
    directory: &File,
    name: &std::ffi::OsStr,
    display_path: &Path,
    create: bool,
    access_context: &crate::platform::EffectiveModeAccessContext,
) -> std::io::Result<File> {
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    let descriptor = match rustix::fs::openat(directory, name, flags, rustix::fs::Mode::empty()) {
        Ok(child) => child,
        Err(rustix::io::Errno::NOENT) if create => {
            access_context.ensure(
                &directory.metadata()?,
                display_path,
                crate::platform::UNIX_ACCESS_READ
                    | crate::platform::UNIX_ACCESS_WRITE
                    | crate::platform::UNIX_ACCESS_SEARCH,
                "generated model catalog directory creation",
            )?;
            match rustix::fs::mkdirat(
                directory,
                name,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
            ) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(std::io::Error::from(error)),
            }
            rustix::fs::openat(directory, name, flags, rustix::fs::Mode::empty())
                .map_err(std::io::Error::from)?
        }
        Err(rustix::io::Errno::LOOP) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "generated model catalog path traverses a symlink: {}",
                    display_path.join(name).display()
                ),
            ));
        }
        Err(error) => return Err(std::io::Error::from(error)),
    };
    Ok(File::from(descriptor))
}

#[cfg(unix)]
fn open_catalog_directory_nofollow(
    path: &Path,
    create: bool,
    access_context: &crate::platform::EffectiveModeAccessContext,
) -> std::io::Result<File> {
    use std::path::Component;

    let descriptor = rustix::fs::open(
        if path.is_absolute() { "/" } else { "." },
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let mut directory = File::from(descriptor);
    let mut display_path = if path.is_absolute() {
        PathBuf::from("/")
    } else {
        PathBuf::from(".")
    };

    access_context.ensure(
        &directory.metadata()?,
        &display_path,
        crate::platform::UNIX_ACCESS_SEARCH,
        "generated model catalog path traversal",
    )?;

    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "secure generated model catalog paths must not contain parent or prefix components: {}",
                        path.display()
                    ),
                ));
            }
        };

        let child =
            open_catalog_child_nofollow(&directory, name, &display_path, create, access_context)?;
        display_path.push(name);
        directory = child;
        let metadata = directory.metadata()?;
        if !metadata.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!(
                    "generated model catalog path ancestor is not a directory: {}",
                    display_path.display()
                ),
            ));
        }
        access_context.ensure(
            &metadata,
            &display_path,
            crate::platform::UNIX_ACCESS_SEARCH,
            "generated model catalog path traversal",
        )?;
    }

    access_context.ensure(
        &directory.metadata()?,
        path,
        crate::platform::UNIX_ACCESS_READ
            | crate::platform::UNIX_ACCESS_WRITE
            | crate::platform::UNIX_ACCESS_SEARCH,
        "generated model catalog creation, replacement, and directory sync",
    )?;
    Ok(directory)
}

#[cfg(unix)]
fn catalog_parent_identity_matches(
    parent: &Path,
    expected: &File,
    access_context: &crate::platform::EffectiveModeAccessContext,
) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;

    let current = open_catalog_directory_nofollow(parent, false, access_context)?;
    let expected_metadata = expected.metadata()?;
    let current_metadata = current.metadata()?;
    Ok(expected_metadata.dev() == current_metadata.dev()
        && expected_metadata.ino() == current_metadata.ino())
}

#[cfg(unix)]
fn read_persisted_catalog_at(
    directory: &File,
    target_name: &std::ffi::OsStr,
    path: &Path,
    access_context: &crate::platform::EffectiveModeAccessContext,
) -> Result<(PersistedFetchedCatalog, UnixCatalogTargetState)> {
    let descriptor = match rustix::fs::openat(
        directory,
        target_name,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => {
            return Ok((
                PersistedFetchedCatalog::default(),
                UnixCatalogTargetState::Missing,
            ));
        }
        Err(error) => {
            return Err(Error::config(format!(
                "Failed to securely open fetched model catalog {}: {}",
                path.display(),
                std::io::Error::from(error)
            )));
        }
    };
    let mut file = File::from(descriptor);
    let metadata = file.metadata().map_err(|error| {
        Error::config(format!(
            "Failed to inspect fetched model catalog {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(Error::config(format!(
            "Fetched model catalog must be a regular non-link file: {}",
            path.display()
        )));
    }
    access_context
        .ensure(
            &metadata,
            path,
            crate::platform::UNIX_ACCESS_READ | crate::platform::UNIX_ACCESS_WRITE,
            "generated model catalog read-write access",
        )
        .map_err(|error| {
            Error::config(format!(
                "Fetched model catalog persistence preflight failed for {}: {error}",
                path.display()
            ))
        })?;
    if metadata.len() > MAX_FETCHED_CATALOG_BYTES as u64 {
        return Err(Error::config(format!(
            "Failed to read fetched model catalog {}: generated model catalog exceeds {MAX_FETCHED_CATALOG_BYTES} bytes",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take((MAX_FETCHED_CATALOG_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            Error::config(format!(
                "Failed to read fetched model catalog {}: {error}",
                path.display()
            ))
        })?;
    if bytes.len() > MAX_FETCHED_CATALOG_BYTES {
        return Err(Error::config(format!(
            "Failed to read fetched model catalog {}: generated model catalog exceeds {MAX_FETCHED_CATALOG_BYTES} bytes",
            path.display()
        )));
    }
    let contents = String::from_utf8(bytes).map_err(|error| {
        Error::config(format!(
            "Failed to read fetched model catalog {}: generated model catalog is not valid UTF-8: {error}",
            path.display()
        ))
    })?;
    let state = unix_catalog_target_state(&metadata);
    let catalog = parse_persisted_catalog_contents(path, &contents)?;
    Ok((catalog, state))
}

#[cfg(unix)]
fn ensure_catalog_target_unchanged_at(
    directory: &File,
    target_name: &std::ffi::OsStr,
    path: &Path,
    expected: UnixCatalogTargetState,
    access_context: &crate::platform::EffectiveModeAccessContext,
) -> Result<()> {
    match expected {
        UnixCatalogTargetState::Missing => {
            match rustix::fs::statat(
                directory,
                target_name,
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            ) {
                Err(rustix::io::Errno::NOENT) => Ok(()),
                Ok(_) => Err(Error::config(format!(
                    "Fetched model catalog changed from missing before replacement: {}",
                    path.display()
                ))),
                Err(error) => Err(Error::config(format!(
                    "Failed to revalidate fetched model catalog {}: {}",
                    path.display(),
                    std::io::Error::from(error)
                ))),
            }
        }
        UnixCatalogTargetState::Existing { .. } => {
            let descriptor = rustix::fs::openat(
                directory,
                target_name,
                rustix::fs::OFlags::RDWR
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(|error| {
                Error::config(format!(
                    "Fetched model catalog changed before replacement at {}: {}",
                    path.display(),
                    std::io::Error::from(error)
                ))
            })?;
            let file = File::from(descriptor);
            let metadata = file.metadata().map_err(|error| {
                Error::config(format!(
                    "Failed to revalidate fetched model catalog {}: {error}",
                    path.display()
                ))
            })?;
            if !metadata.is_file() || unix_catalog_target_state(&metadata) != expected {
                return Err(Error::config(format!(
                    "Fetched model catalog changed before replacement: {}",
                    path.display()
                )));
            }
            access_context
                .ensure(
                    &metadata,
                    path,
                    crate::platform::UNIX_ACCESS_READ | crate::platform::UNIX_ACCESS_WRITE,
                    "generated model catalog read-write access",
                )
                .map_err(|error| {
                    Error::config(format!(
                        "Fetched model catalog persistence preflight failed for {}: {error}",
                        path.display()
                    ))
                })?;
            Ok(())
        }
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct UnixCatalogTempFile {
    file: File,
    directory: File,
    name: std::ffi::OsString,
    persisted: bool,
}

#[cfg(unix)]
impl UnixCatalogTempFile {
    fn persist_to(&mut self, target_name: &std::ffi::OsStr) -> std::io::Result<()> {
        rustix::fs::renameat(&self.directory, &self.name, &self.directory, target_name)
            .map_err(std::io::Error::from)?;
        self.persisted = true;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for UnixCatalogTempFile {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = rustix::fs::unlinkat(&self.directory, &self.name, rustix::fs::AtFlags::empty());
        }
    }
}

#[cfg(unix)]
fn create_catalog_temp_file(
    directory: &File,
    target_name: &std::ffi::OsStr,
) -> std::io::Result<UnixCatalogTempFile> {
    let owned_directory = directory.try_clone()?;
    for _ in 0..16 {
        let mut name = std::ffi::OsString::from(".");
        name.push(target_name);
        name.push(format!(".tmp-{}", uuid::Uuid::new_v4().simple()));
        match rustix::fs::openat(
            directory,
            &name,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        ) {
            Ok(descriptor) => {
                return Ok(UnixCatalogTempFile {
                    file: File::from(descriptor),
                    directory: owned_directory,
                    name,
                    persisted: false,
                });
            }
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(std::io::Error::from(error)),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique fetched model catalog temporary file",
    ))
}

#[cfg(unix)]
struct UnixCatalogPersistenceContext {
    directory: File,
    target_name: std::ffi::OsString,
    access_context: crate::platform::EffectiveModeAccessContext,
}

#[cfg(unix)]
fn open_unix_catalog_persistence_context(
    path: &Path,
    parent: &Path,
) -> Result<UnixCatalogPersistenceContext> {
    let access_context =
        crate::platform::EffectiveModeAccessContext::current().map_err(|error| {
            Error::config(format!(
                "Failed to resolve effective identity for fetched model catalog {}: {error}",
                path.display()
            ))
        })?;
    let (_, target_name) = catalog_target_parts(path).map_err(|error| {
        Error::config(format!(
            "Invalid fetched model catalog path {}: {error}",
            path.display()
        ))
    })?;
    let directory =
        open_catalog_directory_nofollow(parent, true, &access_context).map_err(|error| {
            Error::config(format!(
                "Failed to securely create or open fetched model catalog directory {}: {error}",
                parent.display()
            ))
        })?;
    ensure_model_catalog_persistence_access(path).map_err(|error| {
        Error::config(format!(
            "Fetched model catalog persistence preflight failed for {}: {error}",
            path.display()
        ))
    })?;
    Ok(UnixCatalogPersistenceContext {
        directory,
        target_name: target_name.to_os_string(),
        access_context,
    })
}

#[cfg(unix)]
fn ensure_unix_catalog_parent_unchanged(
    parent: &Path,
    context: &UnixCatalogPersistenceContext,
    phase: &str,
) -> Result<()> {
    if catalog_parent_identity_matches(parent, &context.directory, &context.access_context)
        .map_err(|error| {
            Error::config(format!(
                "Failed to revalidate fetched model catalog directory {}: {error}",
                parent.display()
            ))
        })?
    {
        return Ok(());
    }
    Err(Error::config(format!(
        "Fetched model catalog directory changed {phase}: {}",
        parent.display()
    )))
}

#[cfg(unix)]
fn create_and_write_unix_catalog_temp(
    context: &UnixCatalogPersistenceContext,
    parent: &Path,
    contents: &[u8],
) -> Result<UnixCatalogTempFile> {
    let mut temporary = create_catalog_temp_file(&context.directory, &context.target_name)
        .map_err(|error| {
            Error::config(format!(
                "Failed to create temporary fetched model catalog in {}: {error}",
                parent.display()
            ))
        })?;
    temporary.file.write_all(contents).map_err(|error| {
        Error::config(format!(
            "Failed to write temporary fetched model catalog: {error}"
        ))
    })?;
    temporary.file.sync_all().map_err(|error| {
        Error::config(format!(
            "Failed to sync temporary fetched model catalog: {error}"
        ))
    })?;
    Ok(temporary)
}

#[cfg(unix)]
fn verify_unix_catalog_parent_after_persist(
    parent: &Path,
    context: &UnixCatalogPersistenceContext,
) -> Result<()> {
    match catalog_parent_identity_matches(parent, &context.directory, &context.access_context) {
        Ok(true) => Ok(()),
        Ok(false) => Err(Error::config(format!(
            "Fetched model catalog was persisted and synced in the pinned directory, but its configured parent path changed before completion: {}",
            parent.display()
        ))),
        Err(error) => Err(Error::config(format!(
            "Fetched model catalog was persisted and synced in the pinned directory, but its configured parent path could not be revalidated: {}: {error}",
            parent.display()
        ))),
    }
}

#[cfg(unix)]
fn persist_provider_model_catalog_rows_platform<F, G>(
    request: CatalogPersistenceRequest<'_>,
    before_replace: F,
    after_replace: G,
) -> Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
    G: FnOnce(&Path) -> std::io::Result<()>,
{
    let CatalogPersistenceRequest {
        path,
        parent,
        provider,
        model_ids,
        route_fingerprint,
        fetched_at_unix_ms,
    } = request;
    let context = open_unix_catalog_persistence_context(path, parent)?;

    let _process_guard = fetched_catalog_persist_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _file_guard = crate::file_lock::DirLockAt::acquire_for(
        &context.directory,
        &context.target_name,
        Duration::from_secs(30),
    )
    .map_err(|error| {
        Error::config(format!(
            "Failed to lock fetched model catalog {}: {error}",
            path.display()
        ))
    })?;
    ensure_model_catalog_persistence_access(path).map_err(|error| {
        Error::config(format!(
            "Fetched model catalog persistence preflight failed for {}: {error}",
            path.display()
        ))
    })?;
    ensure_unix_catalog_parent_unchanged(parent, &context, "before merge")?;

    let (catalog, target_state) = read_persisted_catalog_at(
        &context.directory,
        &context.target_name,
        path,
        &context.access_context,
    )?;
    let catalog = merge_provider_catalog(
        catalog,
        provider,
        model_ids,
        route_fingerprint,
        fetched_at_unix_ms,
    )?;
    let contents = serialize_persisted_catalog(&catalog)?;
    let mut temporary = create_and_write_unix_catalog_temp(&context, parent, &contents)?;
    let temporary_path = parent.join(&temporary.name);
    before_replace(&temporary_path).map_err(|error| {
        Error::config(format!(
            "Fetched model catalog replacement preparation failed for {}: {error}",
            path.display()
        ))
    })?;
    ensure_unix_catalog_parent_unchanged(parent, &context, "before replacement")?;
    ensure_catalog_target_unchanged_at(
        &context.directory,
        &context.target_name,
        path,
        target_state,
        &context.access_context,
    )?;
    temporary
        .persist_to(&context.target_name)
        .map_err(|error| {
            Error::config(format!(
                "Failed to atomically persist fetched model catalog to {}: {error}",
                path.display()
            ))
        })?;
    context.directory.sync_all().map_err(|error| {
        Error::config(format!(
            "Failed to sync fetched model catalog directory for {}: {error}",
            path.display()
        ))
    })?;
    after_replace(path).map_err(|error| {
        Error::config(format!(
            "Fetched model catalog was persisted and synced in the pinned directory, but post-persist verification preparation failed for {}: {error}",
            path.display()
        ))
    })?;
    verify_unix_catalog_parent_after_persist(parent, &context)
}

#[cfg(windows)]
#[derive(Debug)]
struct WindowsCatalogDirectoryGuard {
    path: PathBuf,
    creation_time: u64,
    handle: File,
}

#[cfg(windows)]
fn open_or_create_windows_catalog_parent(
    path: &Path,
) -> std::io::Result<(PathBuf, Vec<WindowsCatalogDirectoryGuard>)> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::path::Component;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;

    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = catalog_parent(&absolute_path);
    let mut current = PathBuf::new();
    let mut guards = Vec::new();
    for component in parent.components() {
        match component {
            Component::Prefix(prefix) => {
                current.push(prefix.as_os_str());
                continue;
            }
            Component::RootDir => {
                current.push(component.as_os_str());
                continue;
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "secure generated model catalog paths must not contain parent components: {}",
                        path.display()
                    ),
                ));
            }
            Component::Normal(name) => current.push(name),
        }

        let initial_metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                std::fs::symlink_metadata(&current)?
            }
            Err(error) => return Err(error),
        };
        if !initial_metadata.is_dir()
            || initial_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "generated model catalog path traverses a non-directory or Windows reparse point: {}",
                    current.display()
                ),
            ));
        }
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&current)?;
        let opened_metadata = handle.metadata()?;
        if !opened_metadata.is_dir()
            || opened_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || opened_metadata.creation_time() != initial_metadata.creation_time()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "generated model catalog directory changed while it was being opened: {}",
                    current.display()
                ),
            ));
        }
        guards.push(WindowsCatalogDirectoryGuard {
            path: current.clone(),
            creation_time: opened_metadata.creation_time(),
            handle,
        });
    }
    validate_windows_catalog_parent(&guards)?;
    Ok((absolute_path, guards))
}

#[cfg(windows)]
fn validate_windows_catalog_parent(guards: &[WindowsCatalogDirectoryGuard]) -> std::io::Result<()> {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    for guard in guards {
        let handle_metadata = guard.handle.metadata()?;
        let path_metadata = std::fs::symlink_metadata(&guard.path)?;
        if !handle_metadata.is_dir()
            || !path_metadata.is_dir()
            || handle_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || path_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || handle_metadata.creation_time() != guard.creation_time
            || path_metadata.creation_time() != guard.creation_time
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "generated model catalog directory changed during persistence: {}",
                    guard.path.display()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsCatalogTargetState {
    Missing,
    Existing { creation_time: u64 },
}

#[cfg(windows)]
fn read_persisted_catalog_windows(
    path: &Path,
) -> Result<(PersistedFetchedCatalog, WindowsCatalogTargetState)> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let initial_metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((
                PersistedFetchedCatalog::default(),
                WindowsCatalogTargetState::Missing,
            ));
        }
        Err(error) => {
            return Err(Error::config(format!(
                "Failed to inspect fetched model catalog {}: {error}",
                path.display()
            )));
        }
    };
    if !initial_metadata.is_file()
        || initial_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(Error::config(format!(
            "Fetched model catalog must be a regular non-reparse file: {}",
            path.display()
        )));
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| {
            Error::config(format!(
                "Failed to securely open fetched model catalog {}: {error}",
                path.display()
            ))
        })?;
    let opened_metadata = file.metadata().map_err(|error| {
        Error::config(format!(
            "Failed to inspect opened fetched model catalog {}: {error}",
            path.display()
        ))
    })?;
    let current_metadata = std::fs::symlink_metadata(path).map_err(|error| {
        Error::config(format!(
            "Failed to revalidate fetched model catalog {}: {error}",
            path.display()
        ))
    })?;
    if !opened_metadata.is_file()
        || opened_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || current_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || opened_metadata.creation_time() != initial_metadata.creation_time()
        || current_metadata.creation_time() != initial_metadata.creation_time()
    {
        return Err(Error::config(format!(
            "Fetched model catalog changed while it was being opened: {}",
            path.display()
        )));
    }
    if opened_metadata.len() > MAX_FETCHED_CATALOG_BYTES as u64 {
        return Err(Error::config(format!(
            "Failed to read fetched model catalog {}: generated model catalog exceeds {MAX_FETCHED_CATALOG_BYTES} bytes",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take((MAX_FETCHED_CATALOG_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            Error::config(format!(
                "Failed to read fetched model catalog {}: {error}",
                path.display()
            ))
        })?;
    if bytes.len() > MAX_FETCHED_CATALOG_BYTES {
        return Err(Error::config(format!(
            "Failed to read fetched model catalog {}: generated model catalog exceeds {MAX_FETCHED_CATALOG_BYTES} bytes",
            path.display()
        )));
    }
    let contents = String::from_utf8(bytes).map_err(|error| {
        Error::config(format!(
            "Failed to read fetched model catalog {}: generated model catalog is not valid UTF-8: {error}",
            path.display()
        ))
    })?;
    let catalog = parse_persisted_catalog_contents(path, &contents)?;
    Ok((
        catalog,
        WindowsCatalogTargetState::Existing {
            creation_time: opened_metadata.creation_time(),
        },
    ))
}

#[cfg(windows)]
fn ensure_windows_catalog_target_unchanged(
    path: &Path,
    expected: WindowsCatalogTargetState,
) -> Result<()> {
    let (_, current) = read_persisted_catalog_windows(path)?;
    if current != expected {
        return Err(Error::config(format!(
            "Fetched model catalog changed before replacement: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn persist_provider_model_catalog_rows_platform<F, G>(
    request: CatalogPersistenceRequest<'_>,
    before_replace: F,
    after_replace: G,
) -> Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
    G: FnOnce(&Path) -> std::io::Result<()>,
{
    let CatalogPersistenceRequest {
        path,
        provider,
        model_ids,
        route_fingerprint,
        fetched_at_unix_ms,
        ..
    } = request;
    let (operation_path, parent_guards) =
        open_or_create_windows_catalog_parent(path).map_err(|error| {
            Error::config(format!(
                "Failed to securely create or open fetched model catalog directory for {}: {error}",
                path.display()
            ))
        })?;
    ensure_model_catalog_persistence_access(&operation_path).map_err(|error| {
        Error::config(format!(
            "Fetched model catalog persistence preflight failed for {}: {error}",
            operation_path.display()
        ))
    })?;
    let _process_guard = fetched_catalog_persist_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    validate_windows_catalog_parent(&parent_guards).map_err(|error| {
        Error::config(format!(
            "Failed to revalidate fetched model catalog directory: {error}"
        ))
    })?;
    let _file_guard =
        crate::file_lock::DirLock::acquire_for(&operation_path, Duration::from_secs(30)).map_err(
            |error| {
                Error::config(format!(
                    "Failed to lock fetched model catalog {}: {error}",
                    operation_path.display()
                ))
            },
        )?;
    ensure_model_catalog_persistence_access(&operation_path).map_err(|error| {
        Error::config(format!(
            "Fetched model catalog persistence preflight failed for {}: {error}",
            operation_path.display()
        ))
    })?;
    validate_windows_catalog_parent(&parent_guards).map_err(|error| {
        Error::config(format!(
            "Failed to revalidate fetched model catalog directory: {error}"
        ))
    })?;
    let (catalog, target_state) = read_persisted_catalog_windows(&operation_path)?;
    let catalog = merge_provider_catalog(
        catalog,
        provider,
        model_ids,
        route_fingerprint,
        fetched_at_unix_ms,
    )?;
    let contents = serialize_persisted_catalog(&catalog)?;
    let parent = catalog_parent(&operation_path);
    let mut temporary = NamedTempFile::new_in(parent).map_err(|error| {
        Error::config(format!(
            "Failed to create temporary fetched model catalog in {}: {error}",
            parent.display()
        ))
    })?;
    temporary.write_all(&contents).map_err(|error| {
        Error::config(format!(
            "Failed to write temporary fetched model catalog: {error}"
        ))
    })?;
    temporary.as_file().sync_all().map_err(|error| {
        Error::config(format!(
            "Failed to sync temporary fetched model catalog: {error}"
        ))
    })?;
    before_replace(temporary.path()).map_err(|error| {
        Error::config(format!(
            "Fetched model catalog replacement preparation failed for {}: {error}",
            operation_path.display()
        ))
    })?;
    validate_windows_catalog_parent(&parent_guards).map_err(|error| {
        Error::config(format!(
            "Failed to revalidate fetched model catalog directory: {error}"
        ))
    })?;
    ensure_windows_catalog_target_unchanged(&operation_path, target_state)?;
    temporary.persist(&operation_path).map_err(|error| {
        Error::config(format!(
            "Failed to atomically persist fetched model catalog to {}: {}",
            operation_path.display(),
            error.error
        ))
    })?;
    after_replace(&operation_path).map_err(|error| {
        Error::config(format!(
            "Fetched model catalog was persisted, but post-persist verification preparation failed for {}: {error}",
            operation_path.display()
        ))
    })?;
    validate_windows_catalog_parent(&parent_guards).map_err(|error| {
        Error::config(format!(
            "Fetched model catalog was persisted, but its configured parent path could not be revalidated: {error}"
        ))
    })?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn persist_provider_model_catalog_rows_platform<F, G>(
    request: CatalogPersistenceRequest<'_>,
    before_replace: F,
    after_replace: G,
) -> Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
    G: FnOnce(&Path) -> std::io::Result<()>,
{
    let CatalogPersistenceRequest {
        path,
        parent,
        provider,
        model_ids,
        route_fingerprint,
        fetched_at_unix_ms,
    } = request;
    std::fs::create_dir_all(parent).map_err(|error| {
        Error::config(format!(
            "Failed to create fetched model catalog directory {}: {error}",
            parent.display()
        ))
    })?;
    ensure_model_catalog_persistence_access(path).map_err(|error| {
        Error::config(format!(
            "Fetched model catalog persistence preflight failed for {}: {error}",
            path.display()
        ))
    })?;
    let _process_guard = fetched_catalog_persist_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _file_guard = crate::file_lock::DirLock::acquire_for(path, Duration::from_secs(30))
        .map_err(|error| {
            Error::config(format!(
                "Failed to lock fetched model catalog {}: {error}",
                path.display()
            ))
        })?;
    ensure_model_catalog_persistence_access(path).map_err(|error| {
        Error::config(format!(
            "Fetched model catalog persistence preflight failed for {}: {error}",
            path.display()
        ))
    })?;
    let catalog = merge_provider_catalog(
        load_persisted_catalog(path)?,
        provider,
        model_ids,
        route_fingerprint,
        fetched_at_unix_ms,
    )?;
    let contents = serialize_persisted_catalog(&catalog)?;

    let mut temporary = NamedTempFile::new_in(parent).map_err(|error| {
        Error::config(format!(
            "Failed to create temporary fetched model catalog in {}: {error}",
            parent.display()
        ))
    })?;
    temporary.write_all(&contents).map_err(|error| {
        Error::config(format!(
            "Failed to write temporary fetched model catalog: {error}"
        ))
    })?;
    temporary.as_file().sync_all().map_err(|error| {
        Error::config(format!(
            "Failed to sync temporary fetched model catalog: {error}"
        ))
    })?;
    before_replace(temporary.path()).map_err(|error| {
        Error::config(format!(
            "Fetched model catalog replacement preparation failed for {}: {error}",
            path.display()
        ))
    })?;
    temporary.persist(path).map_err(|error| {
        Error::config(format!(
            "Failed to atomically persist fetched model catalog to {}: {}",
            path.display(),
            error.error
        ))
    })?;
    after_replace(path).map_err(|error| {
        Error::config(format!(
            "Fetched model catalog was persisted, but post-persist verification preparation failed for {}: {error}",
            path.display()
        ))
    })?;
    Ok(())
}

fn catalog_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// JSON shape returned by an OpenAI-compatible `/v1/models` endpoint.
#[derive(Debug, Deserialize)]
struct OpenAiModelsResponse {
    #[serde(deserialize_with = "deserialize_openai_model_rows")]
    data: Vec<OpenAiModelRow>,
}

#[derive(Debug, Deserialize)]
struct OpenAiModelRow {
    #[serde(deserialize_with = "deserialize_live_model_id")]
    id: String,
}

fn deserialize_live_model_id<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let id = String::deserialize(deserializer)?;
    let id = id.trim();
    if !is_safe_model_catalog_identifier(id, MAX_FETCHED_MODEL_ID_BYTES) {
        return Err(serde::de::Error::custom(format!(
            "model ID is not printable ASCII or exceeds {MAX_FETCHED_MODEL_ID_BYTES} bytes"
        )));
    }
    Ok(id.to_string())
}

fn deserialize_openai_model_rows<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<OpenAiModelRow>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct RowsVisitor;

    impl<'de> Visitor<'de> for RowsVisitor {
        type Value = Vec<OpenAiModelRow>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded sequence of provider model rows")
        }

        fn visit_seq<A>(self, mut rows: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut models = Vec::with_capacity(
                rows.size_hint()
                    .unwrap_or_default()
                    .min(MAX_FETCHED_MODELS_PER_PROVIDER),
            );
            let mut total_bytes = 0usize;
            while let Some(model) = rows.next_element::<OpenAiModelRow>()? {
                if models.len() >= MAX_FETCHED_MODELS_PER_PROVIDER {
                    return Err(serde::de::Error::custom(format!(
                        "more than {MAX_FETCHED_MODELS_PER_PROVIDER} raw model rows"
                    )));
                }
                total_bytes = total_bytes
                    .checked_add(model.id.len())
                    .ok_or_else(|| serde::de::Error::custom("provider model-ID size overflow"))?;
                if total_bytes > MAX_FETCHED_MODEL_BYTES_PER_PROVIDER {
                    return Err(serde::de::Error::custom(format!(
                        "more than {MAX_FETCHED_MODEL_BYTES_PER_PROVIDER} raw model-ID bytes"
                    )));
                }
                models.push(model);
            }
            Ok(models)
        }
    }

    deserializer.deserialize_seq(RowsVisitor)
}

async fn fetch_live_models(
    provider: &str,
    api_key: &str,
    route: &ModelCatalogProviderConfig,
) -> Result<Vec<String>> {
    let provider = validate_provider_id(provider)?;
    validate_route_headers(route)?;
    let custom_authorization = custom_authorization_header(route);

    if route.auth_header && custom_authorization.is_none() && api_key.trim().is_empty() {
        return Err(Error::api(
            "no api_key supplied; skipping live provider model fetch",
        ));
    }

    let url = openai_compat_models_url(&route.base_url, &route.api).ok_or_else(|| {
        Error::api(format!(
            "provider {provider:?} is not configured with an OpenAI-compatible model catalog endpoint"
        ))
    })?;

    let client = Client::new();
    let mut request = client.get(&url).header("Accept", "application/json");
    if route.auth_header && custom_authorization.is_none() {
        request = request.try_header("Authorization", format!("Bearer {}", api_key.trim()))?;
    }
    for (name, value) in sorted_route_headers(route) {
        if name.eq_ignore_ascii_case("Authorization") && value.trim().is_empty() {
            continue;
        }
        request = request.try_header(name, value)?;
    }

    let request_and_body = async move {
        let response = request.send().await?;
        let status = response.status();
        let body = response.bytes_limited(MAX_FETCHED_CATALOG_BYTES).await?;
        Ok::<_, Error>((status, body))
    };
    let result = if let Some(timeout) = effective_default_request_timeout(&url) {
        asupersync::time::timeout(
            asupersync::time::wall_now(),
            timeout,
            Box::pin(request_and_body),
        )
        .await
        .map_err(|_| {
            Error::api(format!(
                "model catalog request for {provider:?} timed out after the configured {timeout:?} overall deadline"
            ))
        })?
    } else {
        request_and_body.await
    };
    let (status, body) = result?;
    let body = decode_model_catalog_body(provider, &body)?;
    if !(200..300).contains(&status) {
        let mut secrets = route
            .headers
            .values()
            .map(String::as_str)
            .collect::<Vec<_>>();
        secrets.push(api_key);
        let may_include_body =
            model_catalog_error_body_is_credential_free(provider, api_key, route);
        let snippet = response_error_snippet(&url, body, &secrets, may_include_body);
        return Err(Error::api(format!(
            "provider {provider:?} returned HTTP {status} from its model catalog endpoint: {snippet}"
        )));
    }

    parse_openai_model_ids(provider, body)
}

fn parse_openai_model_ids(provider: &str, body: &str) -> Result<Vec<String>> {
    let mut deserializer = serde_json::Deserializer::from_str(body);
    let parsed = OpenAiModelsResponse::deserialize(&mut deserializer).map_err(|err| {
        Error::api(format!(
            "failed to parse /v1/models response for {provider:?}: {err}"
        ))
    })?;
    deserializer.end().map_err(|err| {
        Error::api(format!(
            "failed to parse /v1/models response for {provider:?}: {err}"
        ))
    })?;

    normalize_model_ids(provider, parsed.data.into_iter().map(|row| row.id)).map_err(|error| {
        Error::api(format!(
            "invalid /v1/models catalog for {provider:?}: {error}"
        ))
    })
}

fn decode_model_catalog_body<'a>(provider: &str, body: &'a [u8]) -> Result<&'a str> {
    std::str::from_utf8(body).map_err(|_| {
        Error::api(format!(
            "provider {provider:?} returned a /v1/models body that is not valid UTF-8"
        ))
    })
}

fn sanitized_response_snippet(body: &str, secrets: &[&str]) -> String {
    const MAX_SCAN_BYTES: usize = 8 * 1024;
    let mut secrets = secrets
        .iter()
        .map(|secret| secret.trim())
        .filter(|secret| !secret.is_empty())
        .collect::<Vec<_>>();
    secrets
        .sort_unstable_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    secrets.dedup();

    let mut snippet = String::with_capacity(200);
    let mut remaining = body;
    let mut scanned_bytes = 0usize;
    while !remaining.is_empty() && snippet.len() < 200 && scanned_bytes < MAX_SCAN_BYTES {
        if let Some(secret) = secrets
            .iter()
            .find(|secret| remaining.starts_with(**secret))
        {
            const REDACTION: &str = "[REDACTED]";
            let available = 200 - snippet.len();
            snippet.extend(REDACTION.chars().take(available));
            remaining = &remaining[secret.len()..];
            scanned_bytes = scanned_bytes.saturating_add(secret.len());
            continue;
        }

        let Some(character) = remaining.chars().next() else {
            break;
        };
        remaining = &remaining[character.len_utf8()..];
        scanned_bytes = scanned_bytes.saturating_add(character.len_utf8());
        match character {
            '\n' | '\r' | '\t' => snippet.push(' '),
            character if character.is_ascii() && !character.is_control() => {
                snippet.push(character);
            }
            _ => {}
        }
    }
    snippet
}

fn response_error_snippet(
    url: &str,
    body: &str,
    secrets: &[&str],
    may_include_body: bool,
) -> String {
    if !may_include_body || url::Url::parse(url).is_ok_and(|parsed| parsed.query().is_some()) {
        return "[response body omitted because the request may contain credentials]".to_string();
    }
    sanitized_response_snippet(body, secrets)
}

fn model_catalog_error_body_is_credential_free(
    provider: &str,
    api_key: &str,
    route: &ModelCatalogProviderConfig,
) -> bool {
    api_key.trim().is_empty()
        && !route.auth_header
        && route
            .api_key
            .as_deref()
            .is_none_or(|configured| configured.trim().is_empty())
        && route.headers.is_empty()
        && provider_routing_defaults(provider).is_some_and(|defaults| {
            defaults.api == route.api && defaults.base_url == route.base_url
        })
}

/// Derive an OpenAI-compatible `/v1/models` URL from a provider's routing
/// defaults.  Returns `None` for endpoints whose `base_url` does not look
/// like an OpenAI-compatible root (e.g. Anthropic's `…/v1/messages` or
/// Google's `/v1beta` Gemini endpoint, which need bespoke handlers).
fn openai_compat_models_url(base_url: &str, api: &str) -> Option<String> {
    if base_url.trim().is_empty() || !matches!(api, "openai-completions" | "openai-responses") {
        return None;
    }
    let mut explicit = url::Url::parse(base_url.trim()).ok()?;
    if !matches!(explicit.scheme(), "http" | "https")
        || explicit.cannot_be_a_base()
        || !explicit.username().is_empty()
        || explicit.password().is_some()
        || explicit.path().trim_end_matches('/').ends_with("/messages")
        || explicit.path().contains("/v1beta")
        || explicit.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("googleapis.com")
                || host.to_ascii_lowercase().ends_with(".googleapis.com")
        })
    {
        return None;
    }
    explicit.set_fragment(None);
    if explicit.path().trim_end_matches('/').ends_with("/models") {
        let path = explicit.path().trim_end_matches('/').to_string();
        explicit.set_path(&path);
        return Some(explicit.to_string());
    }

    let mut endpoint = url::Url::parse(&normalize_openai_base(base_url)).ok()?;
    endpoint.set_fragment(None);
    let request_path = endpoint.path().trim_end_matches('/');
    let root = request_path.strip_suffix("/chat/completions")?;
    endpoint.set_path(&format!("{root}/models"));
    Some(endpoint.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn built_in_route(provider: &str) -> ModelCatalogProviderConfig {
        let defaults = provider_routing_defaults(provider).expect("provider routing defaults");
        ModelCatalogProviderConfig {
            base_url: defaults.base_url.to_string(),
            api: defaults.api.to_string(),
            api_key: None,
            headers: HashMap::new(),
            auth_header: defaults.auth_header,
        }
    }

    fn test_catalog_route(base_url: &str) -> ModelCatalogProviderConfig {
        ModelCatalogProviderConfig {
            base_url: base_url.to_string(),
            api: "openai-completions".to_string(),
            api_key: None,
            headers: HashMap::new(),
            auth_header: false,
        }
    }

    fn verified_catalog_for_route(
        provider: &str,
        route: &ModelCatalogProviderConfig,
        models: Vec<String>,
    ) -> ProviderModelCatalog {
        ProviderModelCatalog {
            provider: canonical_provider_key(provider),
            models,
            source: ModelCatalogSource::Live,
            route_fingerprint: Some(model_catalog_route_fingerprint(provider, route)),
            route_persistable: model_catalog_route_is_persistable(route),
            fetched_at_unix_ms: Some(1_800_000_000_000),
        }
    }

    fn verified_catalog(provider: &str, models: Vec<String>) -> ProviderModelCatalog {
        let route = built_in_route(provider);
        verified_catalog_for_route(provider, &route, models)
    }

    #[cfg(unix)]
    #[test]
    fn sap_route_preflight_requires_effective_endpoint_without_resolving_credentials() {
        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("models.json");
        let marker_path = directory.path().join("credential-was-resolved");
        let quoted_marker = marker_path.to_string_lossy().replace('\'', "'\\''");
        let credential_command = format!("!touch '{quoted_marker}'");

        assert!(
            !provider_model_catalog_route_is_configured_at_path("sap-ai-core", &models_path)
                .expect("probe missing SAP route")
        );

        std::fs::write(
            &models_path,
            serde_json::to_vec(&serde_json::json!({
                "providers": {
                    "sap-ai-core": {
                        "baseUrl": "not-a-url",
                        "api": "openai-completions",
                        "apiKey": &credential_command
                    }
                }
            }))
            .expect("serialize invalid route"),
        )
        .expect("write invalid route");
        assert!(
            !provider_model_catalog_route_is_configured_at_path("sap-ai-core", &models_path)
                .expect("probe invalid SAP route")
        );
        assert!(
            !marker_path.exists(),
            "route preflight must not evaluate configured credentials"
        );

        std::fs::write(
            &models_path,
            serde_json::to_vec(&serde_json::json!({
                "providers": {
                    "sap-ai-core": {
                        "baseUrl": "https://sap.example/v1",
                        "api": "openai-completions",
                        "apiKey": credential_command
                    }
                }
            }))
            .expect("serialize valid route"),
        )
        .expect("write valid route");
        assert!(
            provider_model_catalog_route_is_configured_at_path("sap-ai-core", &models_path)
                .expect("probe configured SAP route")
        );
        assert!(
            !marker_path.exists(),
            "route preflight must remain credential-side-effect free"
        );
    }

    #[cfg(unix)]
    struct UnixModeGuard {
        path: PathBuf,
        original: std::fs::Permissions,
    }

    #[cfg(unix)]
    impl UnixModeGuard {
        fn set(path: &Path, mode: u32) -> Self {
            use std::os::unix::fs::PermissionsExt as _;

            let original = std::fs::metadata(path)
                .expect("stat permission fixture")
                .permissions();
            let mut restricted = original.clone();
            restricted.set_mode(mode);
            std::fs::set_permissions(path, restricted).expect("restrict permission fixture");
            Self {
                path: path.to_path_buf(),
                original,
            }
        }
    }

    #[cfg(unix)]
    impl Drop for UnixModeGuard {
        fn drop(&mut self) {
            if let Err(error) = std::fs::set_permissions(&self.path, self.original.clone()) {
                eprintln!(
                    "failed to restore permissions for {}: {error}",
                    self.path.display()
                );
            }
        }
    }

    #[cfg(unix)]
    fn assert_no_catalog_transaction_artifacts(directory: &Path) {
        let entries = std::fs::read_dir(directory)
            .expect("read catalog transaction directory")
            .map(|entry| {
                entry
                    .expect("read catalog transaction entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        assert!(
            !entries
                .iter()
                .any(|name| name == "models.fetched.json.lock"),
            "descriptor-relative lock must be released: {entries:?}"
        );
        assert!(
            !entries
                .iter()
                .any(|name| name.starts_with(".models.fetched.json.tmp-")),
            "descriptor-relative temporary files must be removed: {entries:?}"
        );
    }

    fn cache_test_lock() -> &'static Mutex<()> {
        static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        TEST_LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn cache_key_canonicalizes_providers_and_isolates_credentials() {
        let openai = built_in_route("openai");
        let first = cache_key("OpenAI", "credential-a", &openai);
        assert_eq!(first, cache_key("openai", "credential-a", &openai));
        assert_ne!(first, cache_key("openai", "credential-b", &openai));
        assert!(!first.contains("credential-a"));
        let ollama = built_in_route("ollama");
        assert_eq!(
            cache_key("ollama", "unused-credential-a", &ollama),
            cache_key("ollama", "unused-credential-b", &ollama),
            "credentials that are never sent must not amplify keyless-provider cache entries"
        );

        let mut custom = openai;
        custom.base_url = "https://gateway.example/v1".to_string();
        assert_ne!(first, cache_key("openai", "credential-a", &custom));
        custom
            .headers
            .insert("x-tenant".to_string(), "tenant-a".to_string());
        let custom_key = cache_key("openai", "credential-a", &custom);
        assert_ne!(first, custom_key);
        assert!(!custom_key.contains("tenant-a"));

        custom
            .headers
            .insert("Authorization".to_string(), "Token route-owned".to_string());
        assert_eq!(
            cache_key("openai", "unused-caller-a", &custom),
            cache_key("openai", "unused-caller-b", &custom),
            "a complete custom Authorization header must bind the credential actually sent, not an unused caller key"
        );
    }

    #[test]
    fn explicit_or_resolved_auth_precedes_models_json_catalog_key() {
        let mut route = built_in_route("openai");
        route.api_key = Some("models-json-key".to_string());
        assert_eq!(
            effective_model_catalog_api_key("", &route),
            "models-json-key"
        );
        assert_eq!(
            effective_model_catalog_api_key("caller-resolved-key", &route),
            "caller-resolved-key"
        );
    }

    #[test]
    fn openai_compat_url_for_openai() {
        let defaults = provider_routing_defaults("openai").expect("openai defaults");
        let url = openai_compat_models_url(defaults.base_url, defaults.api)
            .expect("openai is openai-compatible");
        assert_eq!(url, "https://api.openai.com/v1/models");
    }

    #[test]
    fn openai_compat_url_for_groq() {
        let defaults = provider_routing_defaults("groq").expect("groq defaults");
        let url = openai_compat_models_url(defaults.base_url, defaults.api)
            .expect("groq is openai-compatible");
        assert_eq!(url, "https://api.groq.com/openai/v1/models");
    }

    #[test]
    fn openai_compat_url_for_openrouter() {
        let defaults = provider_routing_defaults("openrouter").expect("openrouter defaults");
        let url = openai_compat_models_url(defaults.base_url, defaults.api)
            .expect("openrouter is openai-compatible");
        assert_eq!(url, "https://openrouter.ai/api/v1/models");
    }

    #[test]
    fn openai_compat_url_normalizes_supported_inference_endpoint_forms() {
        for (base_url, expected) in [
            ("https://api.openai.com", "https://api.openai.com/v1/models"),
            (
                "https://api.openai.com/v1/chat/completions",
                "https://api.openai.com/v1/models",
            ),
            (
                "https://api.openai.com/v1/responses",
                "https://api.openai.com/v1/models",
            ),
            (
                "https://proxy.example/openai/v1/chat/completions?tenant=a#fragment",
                "https://proxy.example/openai/v1/models?tenant=a",
            ),
            (
                "https://proxy.example/openai/v1/models?tenant=a#fragment",
                "https://proxy.example/openai/v1/models?tenant=a",
            ),
        ] {
            assert_eq!(
                openai_compat_models_url(base_url, "openai-completions").as_deref(),
                Some(expected),
                "base URL {base_url:?}"
            );
        }
    }

    #[test]
    fn openai_compat_url_rejects_anthropic_messages_endpoint() {
        let defaults = provider_routing_defaults("anthropic").expect("anthropic defaults");
        assert!(openai_compat_models_url(defaults.base_url, defaults.api).is_none());
    }

    #[test]
    fn openai_compat_url_rejects_non_openai_native_adapters() {
        let cohere = provider_routing_defaults("cohere").expect("cohere defaults");
        let cursor = provider_routing_defaults("cursor").expect("cursor defaults");
        assert!(openai_compat_models_url(cohere.base_url, cohere.api).is_none());
        assert!(openai_compat_models_url(cursor.base_url, cursor.api).is_none());
    }

    #[test]
    fn openai_compat_url_rejects_embedded_credentials() {
        assert!(
            openai_compat_models_url(
                "https://catalog-user:catalog-secret@proxy.example/v1",
                "openai-completions"
            )
            .is_none()
        );
    }

    #[test]
    fn openai_compat_url_rejects_non_http_schemes_without_echoing_the_url() {
        assert!(
            openai_compat_models_url(
                "ftp://proxy.example/v1?api_key=must-not-appear",
                "openai-completions"
            )
            .is_none()
        );
    }

    #[test]
    fn empty_api_key_short_circuits() {
        // We don't make a network call so this should fail with the
        // empty-key sentinel rather than a transport error.
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let route = built_in_route("openai");
        let err = rt
            .block_on(fetch_live_models("openai", "  ", &route))
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("api_key"), "unexpected error: {msg}");
    }

    #[test]
    fn cache_round_trip_respects_ttl() {
        let _guard = cache_test_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_model_cache();
        let key = cache_key("openai", "test-key", &built_in_route("openai"));
        assert!(cache_lookup(&key).is_none(), "starts empty");
        cache_store(key.clone(), vec!["m-1".to_string(), "m-2".to_string()], 123);
        let hit = cache_lookup(&key).expect("fresh entry");
        assert_eq!(hit, (vec!["m-1".to_string(), "m-2".to_string()], 123));
        clear_model_cache();
        assert!(cache_lookup(&key).is_none(), "cleared");
    }

    #[test]
    fn catalog_reports_cache_provenance() {
        let _guard = cache_test_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_model_cache();
        let route = built_in_route("openai");
        let key = cache_key("openai", "", &route);
        cache_store(key, vec!["cached-model".to_string()], 456);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let catalog = runtime
            .block_on(fetch_provider_model_catalog_with_route(
                "openai",
                "",
                Some(route),
            ))
            .expect("cached catalog");
        assert_eq!(catalog.models, vec!["cached-model"]);
        assert_eq!(catalog.source, ModelCatalogSource::Cache);
        assert_eq!(catalog.fetched_at_unix_ms, Some(456));
        clear_model_cache();
    }

    #[test]
    fn cache_evicts_expired_entries_on_lookup_and_store() {
        let _guard = cache_test_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_model_cache();
        let expired_at = Instant::now()
            .checked_sub(MODEL_CACHE_TTL + Duration::from_secs(1))
            .expect("test clock supports a five-minute lookback");
        {
            let mut guard = cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for key in ["expired-lookup", "expired-store"] {
                guard.insert(
                    key.to_string(),
                    CacheEntry {
                        models: vec!["stale-model".to_string()],
                        fetched_at_unix_ms: 1,
                        inserted: expired_at,
                    },
                );
            }
        }

        assert!(cache_lookup("expired-lookup").is_none());
        assert!(
            !cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key("expired-lookup"),
            "an expired lookup must remove its stale entry"
        );
        cache_store(
            "fresh-store".to_string(),
            vec!["fresh-model".to_string()],
            2,
        );
        let guard = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !guard.contains_key("expired-store"),
            "storing a fresh catalog must prune unrelated expired entries"
        );
        assert!(guard.contains_key("fresh-store"));
        drop(guard);
        clear_model_cache();
    }

    #[test]
    fn cache_cardinality_is_hard_bounded() {
        let _guard = cache_test_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_model_cache();
        for index in 0..(MODEL_CACHE_MAX_ENTRIES + 8) {
            cache_store(
                format!("bounded-entry-{index:04}"),
                vec![format!("model-{index}")],
                3,
            );
        }
        let guard = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(guard.len(), MODEL_CACHE_MAX_ENTRIES);
        assert!(guard.contains_key(&format!("bounded-entry-{:04}", MODEL_CACHE_MAX_ENTRIES + 7)));
        drop(guard);
        clear_model_cache();
    }

    #[test]
    fn cache_total_model_id_bytes_are_hard_bounded() {
        let _guard = cache_test_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_model_cache();
        let one_mebibyte = "x".repeat(1024 * 1024);
        for index in 0..10 {
            cache_store(
                format!("byte-budget-{index:02}"),
                vec![one_mebibyte.clone()],
                4,
            );
        }
        let guard = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let retained_bytes = guard
            .values()
            .map(|entry| model_id_bytes(&entry.models))
            .sum::<usize>();
        assert!(retained_bytes <= MODEL_CACHE_MAX_MODEL_ID_BYTES);
        assert!(guard.contains_key("byte-budget-09"));
        drop(guard);
        clear_model_cache();
    }

    #[test]
    fn bare_catalog_paths_use_the_current_directory() {
        assert_eq!(
            catalog_parent(Path::new("models.fetched.json")),
            Path::new(".")
        );
        assert_eq!(
            catalog_parent(Path::new("nested/models.fetched.json")),
            Path::new("nested")
        );
    }

    #[test]
    fn refresh_without_credentials_is_strict_instead_of_falling_back() {
        let _guard = cache_test_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_model_cache();
        let route = built_in_route("openai");
        let key = cache_key("openai", "", &route);
        cache_store(key.clone(), vec!["stale-cache".to_string()], 5);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(refresh_provider_model_catalog_with_route(
                "openai", "", route,
            ))
            .expect_err("refresh must require a live response");
        assert!(error.to_string().contains("api_key"), "{error}");
        assert_eq!(
            cache_lookup(&key),
            Some((vec!["stale-cache".to_string()], 5)),
            "failed refresh must not replace or disguise the previous cache"
        );
        clear_model_cache();
    }

    #[test]
    fn model_ids_and_error_snippets_are_safe_for_line_oriented_output() {
        assert_eq!(
            normalize_model_ids(
                "openai",
                [
                    " valid/model ".to_string(),
                    "valid/model".to_string(),
                    "  ".to_string(),
                ],
            )
            .expect("normalize safe IDs"),
            vec!["valid/model"]
        );

        assert_eq!(
            normalize_model_ids(
                "openrouter",
                [
                    "GPT-4O-MINI".to_string(),
                    "openai/gpt-4o-mini".to_string(),
                    "AUTO".to_string(),
                    "openrouter/auto".to_string(),
                ],
            )
            .expect("normalize OpenRouter aliases"),
            vec!["openai/gpt-4o-mini", "openrouter/auto"]
        );

        for invalid in [
            "bad model".to_string(),
            "bad\nmodel".to_string(),
            "\u{1b}[31m".to_string(),
            "model\u{202e}spoof".to_string(),
            "model\u{200b}hidden".to_string(),
            "mødel".to_string(),
            "x".repeat(MAX_FETCHED_MODEL_ID_BYTES + 1),
        ] {
            assert!(
                normalize_model_ids("openai", [invalid]).is_err(),
                "unsafe or oversized IDs must fail the whole catalog"
            );
        }

        let snippet = sanitized_response_snippet(
            "failed for secret-key\n\u{1b}[31mterminal injection",
            &["secret-key"],
        );
        assert_eq!(snippet, "failed for [REDACTED] [31mterminal injection");
        assert!(!snippet.chars().any(char::is_control));

        let overlapping = sanitized_response_snippet(
            "request rejected for sk-super-secret",
            &["sk", "sk-super-secret"],
        );
        assert_eq!(overlapping, "request rejected for [REDACTED]");
        assert!(!overlapping.contains("super-secret"));

        assert_eq!(
            response_error_snippet(
                "https://proxy.example/v1/models?api_key=query-secret",
                "provider echoed query-secret",
                &[],
                true,
            ),
            "[response body omitted because the request may contain credentials]"
        );
        assert_eq!(
            response_error_snippet(
                "https://proxy.example/token-in-path/v1/models",
                "provider echoed token-in-path",
                &[],
                false,
            ),
            "[response body omitted because the request may contain credentials]"
        );

        let mut route = built_in_route("openai");
        route.auth_header = false;
        route
            .headers
            .insert("x-secret".to_string(), "sec\"ret".to_string());
        assert!(!model_catalog_error_body_is_credential_free(
            "openai", "", &route
        ));
        assert_eq!(
            response_error_snippet(
                "https://api.openai.com/v1/models",
                r#"gateway echoed {"x-secret":"sec\"ret"}"#,
                &["sec\"ret"],
                model_catalog_error_body_is_credential_free("openai", "", &route),
            ),
            "[response body omitted because the request may contain credentials]"
        );

        let amplified = sanitized_response_snippet(&"x".repeat(MAX_FETCHED_CATALOG_BYTES), &["x"]);
        assert_eq!(amplified, "[REDACTED]".repeat(20));
    }

    #[test]
    fn malformed_authorization_value_fails_before_network_io() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let route = built_in_route("openai");
        let error = runtime
            .block_on(fetch_live_models(
                "openai",
                "secret\r\nInjected: value",
                &route,
            ))
            .expect_err("header injection bytes must be rejected locally");
        assert!(error.to_string().contains("forbidden control character"));
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn custom_authorization_header_is_a_complete_catalog_credential_override() {
        use std::io::{Read as _, Write as _};
        use std::time::Duration;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind catalog server");
        let address = listener.local_addr().expect("catalog server address");
        listener
            .set_nonblocking(true)
            .expect("make catalog accept bounded");
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "catalog request timed out");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept catalog request: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("bound catalog request read");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).expect("read catalog request");
                assert!(count > 0, "catalog request ended before its headers");
                request.extend_from_slice(&chunk[..count]);
            }
            let body = br#"{"data":[{"id":"configured-model"}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("write catalog response headers");
            stream.write_all(body).expect("write catalog response");
            String::from_utf8(request).expect("request headers are UTF-8")
        });

        let mut route = built_in_route("openai");
        route.base_url = format!("http://{address}/v1");
        route.headers.insert(
            "Authorization".to_string(),
            "Token configured-only".to_string(),
        );
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        assert_eq!(
            runtime
                .block_on(fetch_live_models("openai", "", &route))
                .expect("custom Authorization is sufficient"),
            vec!["configured-model"]
        );
        let request = server.join().expect("catalog fixture thread");
        let request_lower = request.to_ascii_lowercase();
        assert!(
            request_lower.contains("authorization: token configured-only\r\n"),
            "{request}"
        );
        assert_eq!(request_lower.matches("authorization:").count(), 1);
    }

    #[test]
    fn blank_or_ambiguous_custom_authorization_does_not_bypass_catalog_auth() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let mut blank = built_in_route("openai");
        blank
            .headers
            .insert("Authorization".to_string(), "   ".to_string());
        let error = runtime
            .block_on(fetch_live_models("openai", "", &blank))
            .expect_err("blank Authorization must still require an API key");
        assert!(error.to_string().contains("api_key"), "{error}");

        let mut ambiguous = built_in_route("openai");
        ambiguous
            .headers
            .insert("Authorization".to_string(), "Token first".to_string());
        ambiguous
            .headers
            .insert("authorization".to_string(), "Token second".to_string());
        let error = runtime
            .block_on(fetch_live_models("openai", "unused", &ambiguous))
            .expect_err("case-insensitive duplicate auth headers must fail closed");
        assert!(error.to_string().contains("duplicate case-insensitive"));
        assert!(!error.to_string().contains("Token"));
    }

    #[test]
    fn invalid_utf8_model_catalog_is_rejected_without_lossy_repair() {
        let body = b"{\"data\":[{\"id\":\"x\xffy\"}]}";
        let error = decode_model_catalog_body("openai", body)
            .expect_err("structured model IDs must not be synthesized from invalid UTF-8");
        assert!(error.to_string().contains("not valid UTF-8"), "{error}");
    }

    #[test]
    fn live_model_catalog_rejects_duplicate_json_keys() {
        let error = parse_openai_model_ids(
            "openai",
            r#"{"data":[{"id":"first"}],"data":[{"id":"second"}]}"#,
        )
        .expect_err("duplicate response keys must not select an attacker-controlled value");
        assert!(error.to_string().contains("duplicate field"), "{error}");
    }

    #[test]
    fn live_model_catalog_rejects_excess_raw_rows_before_normalization() {
        let rows = (0..=MAX_FETCHED_MODELS_PER_PROVIDER)
            .map(|_| serde_json::json!({"id": "duplicate"}))
            .collect::<Vec<_>>();
        let body = serde_json::to_string(&serde_json::json!({"data": rows}))
            .expect("serialize oversized raw catalog");
        let error = parse_openai_model_ids("openai", &body)
            .expect_err("raw rows must be bounded even when every ID is a duplicate");
        assert!(error.to_string().contains("raw model rows"), "{error}");
    }

    #[test]
    fn persisted_catalog_is_secret_free_and_preserves_other_providers() {
        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("models.json");
        let manual_bytes = br#"{
  "futureUserField": {"unknown": true},
  "providers": {"manual": {"apiKey": "do-not-touch"}}
}"#;
        std::fs::write(&models_path, manual_bytes).expect("write manual models.json");

        let mut openrouter_route = built_in_route("openrouter");
        openrouter_route
            .headers
            .insert("x-api-key".to_string(), "do-not-persist-header".to_string());
        let openrouter_catalog = verified_catalog_for_route(
            "OpenRouter",
            &openrouter_route,
            vec![
                "z/model".to_string(),
                "a/model".to_string(),
                "a/model".to_string(),
                "  ".to_string(),
            ],
        );
        let fetched_path = persist_provider_model_catalog(&models_path, &openrouter_catalog)
            .expect("persist OpenRouter catalog");
        let groq_catalog = verified_catalog("groq", vec!["groq-model".to_string()]);
        persist_provider_model_catalog(&models_path, &groq_catalog).expect("persist Groq catalog");

        assert_eq!(
            std::fs::read(&models_path).expect("read manual models.json"),
            manual_bytes,
            "persistence must never rewrite or normalize user models.json"
        );
        let encoded = std::fs::read_to_string(&fetched_path).expect("read fetched catalog");
        let catalog: PersistedFetchedCatalog =
            serde_json::from_str(&encoded).expect("parse fetched catalog");
        assert_eq!(catalog.schema, FETCHED_MODELS_SCHEMA);
        assert_eq!(
            catalog.providers["openrouter"]
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a/model", "z/model"]
        );
        assert_eq!(catalog.providers["groq"].models[0].id, "groq-model");
        assert!(
            catalog.providers.values().all(|provider| {
                provider.route_fingerprint.starts_with("sha256:") && provider.fetched_at_unix_ms > 0
            }),
            "every persisted provider must carry non-secret endpoint/transport provenance and fetch time"
        );
        assert!(!encoded.contains("apiKey"));
        assert!(!encoded.contains("do-not-touch"));
        assert!(!encoded.contains("test-catalog-key"));
        assert!(!encoded.contains("do-not-persist-header"));
    }

    #[test]
    fn persisted_custom_provider_requires_its_manual_route_on_reload() {
        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("models.json");
        let backup_path = directory.path().join("models.json.preserved");
        let manual = br#"{
  "providers": {
    "acme": {
      "api": "openai-completions",
      "baseUrl": "https://acme.example/v1",
      "apiKey": "manual-secret",
      "authHeader": true
    }
  }
        }
"#;
        std::fs::write(&models_path, manual).expect("write custom provider route");
        let route = ModelCatalogProviderConfig {
            base_url: "https://acme.example/v1".to_string(),
            api: "openai-completions".to_string(),
            api_key: Some("manual-secret".to_string()),
            headers: HashMap::new(),
            auth_header: true,
        };
        let catalog = verified_catalog_for_route("acme", &route, vec!["acme-model".to_string()]);
        persist_provider_model_catalog(&models_path, &catalog)
            .expect("persist custom provider membership");

        let configured =
            ModelRegistry::load_with_credential_resolver(Some(models_path.clone()), |_| None)
                .find("acme", "acme-model")
                .expect("manual route makes persisted membership routable");
        assert_eq!(configured.model.base_url, "https://acme.example/v1");
        assert_eq!(configured.api_key.as_deref(), Some("manual-secret"));

        std::fs::rename(&models_path, &backup_path).expect("preserve manual route elsewhere");
        let missing_route =
            ModelRegistry::load_with_credential_resolver(Some(models_path.clone()), |_| None);
        assert!(
            missing_route.find("acme", "acme-model").is_none(),
            "generated IDs must not synthesize an unsafe default route"
        );

        std::fs::write(&models_path, "{ malformed").expect("write malformed manual route");
        let malformed_route =
            ModelRegistry::load_with_credential_resolver(Some(models_path.clone()), |_| None);
        assert!(malformed_route.error().is_some());
        assert!(malformed_route.find("acme", "acme-model").is_none());

        std::fs::write(&models_path, manual).expect("restore valid manual route bytes");
        let restored = ModelRegistry::load_with_credential_resolver(Some(models_path), |_| None)
            .find("acme", "acme-model")
            .expect("restoring the manual route restores generated membership");
        assert_eq!(restored.model.base_url, "https://acme.example/v1");
    }

    #[test]
    fn persisted_membership_is_bound_to_endpoint_shape_without_a_credential_verifier() {
        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("models.json");
        let write_manual_route = |base_url: &str, api_key: &str| {
            std::fs::write(
                &models_path,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "providers": {
                        "acme": {
                            "api": "openai-completions",
                            "baseUrl": base_url,
                            "apiKey": api_key,
                            "authHeader": true
                        }
                    }
                }))
                .expect("serialize manual route"),
            )
            .expect("write manual route");
        };

        write_manual_route("https://gateway-a.example/v1", "route-fallback-key");
        let route_a = ModelCatalogProviderConfig {
            base_url: "https://gateway-a.example/v1".to_string(),
            api: "openai-completions".to_string(),
            api_key: Some("route-fallback-key".to_string()),
            headers: HashMap::new(),
            auth_header: true,
        };
        let catalog_a =
            verified_catalog_for_route("acme", &route_a, vec!["route-a-model".to_string()]);
        persist_provider_model_catalog(&models_path, &catalog_a)
            .expect("persist route-A membership");

        let matching =
            ModelRegistry::load_with_credential_resolver(Some(models_path.clone()), |_| {
                Some("account-a-runtime-key".to_string())
            });
        assert!(matching.error().is_none(), "{:?}", matching.error());
        let matching_model = matching
            .find("acme", "route-a-model")
            .expect("matching route membership");
        assert_eq!(
            matching_model.api_key.as_deref(),
            Some("account-a-runtime-key"),
            "inference must retain the current runtime credential"
        );

        write_manual_route("https://gateway-b.example/v1", "route-fallback-key");
        let route_mismatch =
            ModelRegistry::load_with_credential_resolver(Some(models_path.clone()), |_| {
                Some("account-a-runtime-key".to_string())
            });
        assert!(route_mismatch.find("acme", "route-a-model").is_none());
        assert!(
            route_mismatch
                .error()
                .is_some_and(|error| error.contains("saved endpoint/transport binding")),
            "route mismatch must be explicit: {:?}",
            route_mismatch.error()
        );

        write_manual_route("https://gateway-a.example/v1", "route-fallback-key");
        let rotated_credential =
            ModelRegistry::load_with_credential_resolver(Some(models_path.clone()), |_| {
                Some("account-b-key".to_string())
            });
        assert!(rotated_credential.error().is_none());
        assert_eq!(
            rotated_credential
                .find("acme", "route-a-model")
                .expect("credential rotation must retain route-bound membership")
                .api_key
                .as_deref(),
            Some("account-b-key"),
            "the generated catalog must not pin or verify the prior credential"
        );
    }

    #[test]
    fn public_persistence_rejects_static_fallback_without_filesystem_mutation() {
        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("nested").join("models.json");
        let fallback = ProviderModelCatalog {
            provider: "openai".to_string(),
            models: vec!["fallback-model".to_string()],
            source: ModelCatalogSource::StaticFallback,
            route_fingerprint: None,
            route_persistable: false,
            fetched_at_unix_ms: None,
        };

        let error = persist_provider_model_catalog(&models_path, &fallback)
            .expect_err("fallback membership must never be persisted");
        assert!(
            error.to_string().contains("static model fallback"),
            "{error}"
        );
        assert!(
            !models_path.parent().expect("nested parent").exists(),
            "provenance rejection must occur before directory creation"
        );
    }

    #[test]
    fn unsafe_value_routed_live_catalog_is_returned_but_not_persisted() {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind catalog server");
        let address = listener.local_addr().expect("catalog server address");
        listener
            .set_nonblocking(true)
            .expect("make catalog accept bounded");
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "catalog request timed out");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept catalog request: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("bound catalog request read");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).expect("read catalog request");
                assert!(count > 0, "catalog request ended before its headers");
                request.extend_from_slice(&chunk[..count]);
            }
            let body = br#"{"data":[{"id":"tenant-model"}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("write catalog response headers");
            stream.write_all(body).expect("write catalog response");
            String::from_utf8(request).expect("request headers are UTF-8")
        });

        let mut route = test_catalog_route(&format!("http://{address}/v1?tenant=tenant-a"));
        route
            .headers
            .insert("X-Deployment".to_string(), "blue".to_string());
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let catalog = runtime
            .block_on(fetch_provider_model_catalog_with_route(
                "acme",
                "",
                Some(route),
            ))
            .expect("unsafe routing values do not prevent an explicitly live fetch");
        assert_eq!(catalog.models(), &["tenant-model"]);
        assert_eq!(catalog.source(), ModelCatalogSource::Live);
        let request = server.join().expect("catalog fixture thread");
        let request_lower = request.to_ascii_lowercase();
        assert!(request_lower.contains("?tenant=tenant-a"), "{request}");
        assert!(request_lower.contains("x-deployment: blue"), "{request}");

        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("nested").join("models.json");
        let error = persist_provider_model_catalog(&models_path, &catalog)
            .expect_err("unverifiable tenant/deployment membership must not persist");
        assert!(
            error
                .to_string()
                .contains("outside a recognized credential channel"),
            "{error}"
        );
        assert!(
            !models_path.parent().expect("nested parent").exists(),
            "route-safety rejection must occur before filesystem mutation"
        );
    }

    #[cfg(unix)]
    #[test]
    fn persistence_denies_read_only_existing_catalog_before_lock_or_replacement() {
        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("models.json");
        let initial = verified_catalog("openai", vec!["initial-model".to_string()]);
        let fetched_path = persist_provider_model_catalog(&models_path, &initial)
            .expect("persist initial catalog");
        let original = std::fs::read(&fetched_path).expect("read initial bytes");
        let mode_guard = UnixModeGuard::set(&fetched_path, 0o400);

        let replacement = verified_catalog("openai", vec!["replacement-model".to_string()]);
        let error = persist_provider_model_catalog(&models_path, &replacement)
            .expect_err("owner read-only catalog must fail before atomic replacement");
        assert!(error.to_string().contains("Permission denied"), "{error}");
        assert_eq!(
            std::fs::read(&fetched_path).expect("read preserved bytes"),
            original
        );
        assert!(
            !crate::file_lock::lock_path_for(&fetched_path).exists(),
            "permission preflight must run before DirLock acquisition"
        );
        drop(mode_guard);
    }

    #[cfg(unix)]
    #[test]
    fn persistence_denies_missing_target_in_owner_unwritable_nearest_ancestor_without_mutation() {
        let directory = tempfile::tempdir().expect("tempdir");
        let boundary = directory.path().join("owner-unwritable");
        std::fs::create_dir(&boundary).expect("create boundary");
        let models_path = boundary.join("missing").join("models.json");
        let fetched_path = fetched_models_path(&models_path);
        let mode_guard = UnixModeGuard::set(&boundary, 0o577);
        let catalog = verified_catalog("openai", vec!["new-model".to_string()]);

        let error = persist_provider_model_catalog(&models_path, &catalog)
            .expect_err("owner class without write must deny directory creation even to UID 0");
        assert!(error.to_string().contains("Permission denied"), "{error}");
        assert!(
            !boundary.join("missing").exists(),
            "preflight failure must not leave a partially-created directory tree"
        );
        assert!(!fetched_path.exists());
        assert!(!crate::file_lock::lock_path_for(&fetched_path).exists());
        drop(mode_guard);
    }

    #[cfg(unix)]
    #[test]
    fn persistence_denies_write_search_only_parent_before_directory_sync_mutation() {
        let directory = tempfile::tempdir().expect("tempdir");
        let boundary = directory.path().join("write-search-only");
        std::fs::create_dir(&boundary).expect("create boundary");
        let models_path = boundary.join("models.json");
        let fetched_path = fetched_models_path(&models_path);
        let mode_guard = UnixModeGuard::set(&boundary, 0o300);
        let catalog = verified_catalog("openai", vec!["new-model".to_string()]);

        let error = persist_provider_model_catalog(&models_path, &catalog)
            .expect_err("directory durability requires owner read permission");
        assert!(error.to_string().contains("Permission denied"), "{error}");
        assert!(!fetched_path.exists());
        assert!(!crate::file_lock::lock_path_for(&fetched_path).exists());
        drop(mode_guard);
    }

    #[cfg(unix)]
    #[test]
    fn persistence_rejects_valid_and_dangling_final_symlinks_without_replacing_them() {
        use std::os::unix::fs::symlink;

        for dangling in [false, true] {
            let directory = tempfile::tempdir().expect("tempdir");
            let models_path = directory.path().join("models.json");
            let fetched_path = fetched_models_path(&models_path);
            let target = directory.path().join("target.json");
            if !dangling {
                std::fs::write(&target, b"preserve target\n").expect("write symlink target");
            }
            symlink(&target, &fetched_path).expect("create generated-catalog symlink");
            let catalog = verified_catalog("openai", vec!["new-model".to_string()]);

            let error = persist_provider_model_catalog(&models_path, &catalog)
                .expect_err("generated catalog symlinks must never be replaced");
            assert!(
                error.to_string().contains("must not be a symlink"),
                "{error}"
            );
            assert!(
                std::fs::symlink_metadata(&fetched_path)
                    .expect("symlink remains")
                    .file_type()
                    .is_symlink()
            );
            if !dangling {
                assert_eq!(
                    std::fs::read(&target).expect("read preserved target"),
                    b"preserve target\n"
                );
            }
            assert!(!crate::file_lock::lock_path_for(&fetched_path).exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn persistence_rejects_dangling_symlink_ancestors_before_creating_anything() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("tempdir");
        let dangling_parent = directory.path().join("catalog-parent");
        symlink(directory.path().join("missing-target"), &dangling_parent)
            .expect("create dangling parent symlink");
        let models_path = dangling_parent.join("nested").join("models.json");
        let fetched_path = fetched_models_path(&models_path);
        let catalog = verified_catalog("openai", vec!["new-model".to_string()]);

        let error = persist_provider_model_catalog(&models_path, &catalog)
            .expect_err("a dangling ancestor must fail before create_dir_all");
        assert!(
            error.to_string().contains("dangling symlink ancestor"),
            "{error}"
        );
        assert!(!directory.path().join("missing-target").exists());
        assert!(!fetched_path.exists());
        assert!(!crate::file_lock::lock_path_for(&fetched_path).exists());
    }

    #[cfg(unix)]
    #[test]
    fn persistence_parent_swap_cannot_redirect_read_merge_lock_temp_or_write() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("tempdir");
        let parent = directory.path().join("catalog");
        let moved_parent = directory.path().join("catalog-moved");
        let redirected_parent = directory.path().join("catalog-redirected");
        std::fs::create_dir(&parent).expect("create catalog parent");
        std::fs::create_dir(&redirected_parent).expect("create redirect destination");
        let models_path = parent.join("models.json");
        let initial = verified_catalog("openai", vec!["initial-model".to_string()]);
        let fetched_path = persist_provider_model_catalog(&models_path, &initial)
            .expect("persist initial catalog");
        let original = std::fs::read(&fetched_path).expect("read initial catalog");

        let replacement = verified_catalog("groq", vec!["must-not-persist".to_string()]);
        let error = persist_provider_model_catalog_rows_with_hook(
            &models_path,
            replacement.provider(),
            replacement.models(),
            replacement
                .route_fingerprint
                .as_deref()
                .expect("verified route fingerprint"),
            replacement
                .fetched_at_unix_ms
                .expect("verified fetch timestamp"),
            |_| {
                std::fs::rename(&parent, &moved_parent)?;
                symlink(&redirected_parent, &parent)?;
                Ok(())
            },
        )
        .expect_err("an ancestor swap must abort the descriptor-pinned transaction");
        assert!(
            error.to_string().contains("revalidate")
                || error.to_string().contains("directory changed"),
            "{error}"
        );

        let moved_catalog = moved_parent.join("models.fetched.json");
        assert_eq!(
            std::fs::read(&moved_catalog).expect("read preserved moved catalog"),
            original,
            "the merge must not replace the catalog in the moved pinned directory"
        );
        assert!(
            !redirected_parent.join("models.fetched.json").exists(),
            "the replacement path must not receive catalog bytes"
        );
        assert_no_catalog_transaction_artifacts(&moved_parent);
        assert_no_catalog_transaction_artifacts(&redirected_parent);
    }

    #[cfg(unix)]
    #[test]
    fn persistence_reports_parent_swap_after_descriptor_relative_commit() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("tempdir");
        let parent = directory.path().join("catalog");
        let moved_parent = directory.path().join("catalog-moved");
        let redirected_parent = directory.path().join("catalog-redirected");
        std::fs::create_dir(&parent).expect("create catalog parent");
        std::fs::create_dir(&redirected_parent).expect("create redirect destination");
        let models_path = parent.join("models.json");
        let initial = verified_catalog("openai", vec!["initial-model".to_string()]);
        persist_provider_model_catalog(&models_path, &initial).expect("persist initial catalog");

        let replacement = verified_catalog("groq", vec!["committed-model".to_string()]);
        let error = persist_provider_model_catalog_rows_with_hooks(
            &models_path,
            replacement.provider(),
            replacement.models(),
            replacement
                .route_fingerprint
                .as_deref()
                .expect("verified route fingerprint"),
            replacement
                .fetched_at_unix_ms
                .expect("verified fetch timestamp"),
            |_| Ok(()),
            |_| {
                std::fs::rename(&parent, &moved_parent)?;
                symlink(&redirected_parent, &parent)?;
                Ok(())
            },
        )
        .expect_err("a post-commit parent swap must be reported as partial success");

        assert!(
            error.to_string().contains("persisted and synced")
                && error.to_string().contains("configured parent path"),
            "the error must disclose that the catalog committed before path verification failed: {error}"
        );
        let moved_contents = std::fs::read_to_string(moved_parent.join("models.fetched.json"))
            .expect("read committed moved catalog");
        let moved_catalog =
            parse_persisted_fetched_catalog(&moved_contents).expect("parse committed catalog");
        assert!(
            moved_catalog.providers.contains_key("openai")
                && moved_catalog.providers.contains_key("groq"),
            "the completed merge must remain durable in the pinned directory"
        );
        assert!(
            !redirected_parent.join("models.fetched.json").exists(),
            "the replacement parent must not receive catalog bytes"
        );
        assert_no_catalog_transaction_artifacts(&moved_parent);
        assert_no_catalog_transaction_artifacts(&redirected_parent);
    }

    #[cfg(unix)]
    #[test]
    fn persistence_final_swap_is_rejected_without_following_or_replacing_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("models.json");
        let initial = verified_catalog("openai", vec!["initial-model".to_string()]);
        let fetched_path = persist_provider_model_catalog(&models_path, &initial)
            .expect("persist initial catalog");
        let original = std::fs::read(&fetched_path).expect("read initial catalog");
        let preserved_path = directory.path().join("models.fetched.preserved.json");
        let unrelated_path = directory.path().join("unrelated.json");
        std::fs::write(&unrelated_path, b"unrelated bytes\n").expect("write unrelated target");

        let replacement = verified_catalog("groq", vec!["must-not-persist".to_string()]);
        let error = persist_provider_model_catalog_rows_with_hook(
            &models_path,
            replacement.provider(),
            replacement.models(),
            replacement
                .route_fingerprint
                .as_deref()
                .expect("verified route fingerprint"),
            replacement
                .fetched_at_unix_ms
                .expect("verified fetch timestamp"),
            |_| {
                std::fs::rename(&fetched_path, &preserved_path)?;
                symlink(&unrelated_path, &fetched_path)?;
                Ok(())
            },
        )
        .expect_err("a final-component swap must abort atomic replacement");
        assert!(
            error.to_string().contains("changed before replacement"),
            "{error}"
        );
        assert!(
            std::fs::symlink_metadata(&fetched_path)
                .expect("swapped final symlink remains")
                .file_type()
                .is_symlink(),
            "the failed transaction must not replace the swapped link"
        );
        assert_eq!(
            std::fs::read(&unrelated_path).expect("read unrelated target"),
            b"unrelated bytes\n",
            "the transaction must never follow the swapped final symlink"
        );
        assert_eq!(
            std::fs::read(&preserved_path).expect("read preserved original catalog"),
            original
        );
        assert_no_catalog_transaction_artifacts(directory.path());
    }

    #[test]
    fn malformed_generated_catalog_is_not_overwritten() {
        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("models.json");
        let fetched_path = fetched_models_path(&models_path);
        let original = b"not generated catalog json\n";
        std::fs::write(&fetched_path, original).expect("write malformed fetched catalog");

        let catalog = verified_catalog("openrouter", vec!["openai/gpt-test".to_string()]);
        let error = persist_provider_model_catalog(&models_path, &catalog)
            .expect_err("malformed generated catalog must fail closed");
        assert!(
            error.to_string().contains("Refusing to overwrite"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(&fetched_path).expect("re-read fetched catalog"),
            original,
            "failed persistence must preserve the existing bytes"
        );
    }

    #[test]
    fn legacy_v1_generated_catalog_is_preserved_with_actionable_recovery() {
        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("models.json");
        let fetched_path = fetched_models_path(&models_path);
        let original = br#"{
  "schema": "pi.models.fetched.v1",
  "providers": {"openai": {"models": [{"id": "legacy-model"}]}}
}
"#;
        std::fs::write(&fetched_path, original).expect("write legacy fetched catalog");

        let catalog = verified_catalog("openai", vec!["new-model".to_string()]);
        let error = persist_provider_model_catalog(&models_path, &catalog)
            .expect_err("legacy provenance-free catalog must not be silently overwritten");
        let message = error.to_string();
        assert!(message.contains("pi.models.fetched.v1"), "{message}");
        assert!(message.contains("Move it aside to"), "{message}");
        assert!(
            message.contains("models.fetched.v1.backup.json"),
            "{message}"
        );
        assert_eq!(
            std::fs::read(&fetched_path).expect("re-read legacy catalog"),
            original,
            "recovery guidance must not mutate the legacy file"
        );
    }

    #[test]
    fn duplicate_json_keys_are_rejected_without_overwriting() {
        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("models.json");
        let fetched_path = fetched_models_path(&models_path);
        let original = br#"{
  "schema": "pi.models.fetched.v2",
  "providers": {
    "openai": {"routeFingerprint": "sha256:0000000000000000000000000000000000000000000000000000000000000000", "fetchedAtUnixMs": 1, "models": [{"id": "first"}]},
    "openai": {"routeFingerprint": "sha256:0000000000000000000000000000000000000000000000000000000000000000", "fetchedAtUnixMs": 1, "models": [{"id": "second"}]}
  }
}
"#;
        std::fs::write(&fetched_path, original).expect("write duplicate-key catalog");

        let catalog = verified_catalog("groq", vec!["groq-model".to_string()]);
        let error = persist_provider_model_catalog(&models_path, &catalog)
            .expect_err("duplicate JSON keys must fail closed");
        assert!(error.to_string().contains("duplicate JSON object key"));
        assert_eq!(
            std::fs::read(&fetched_path).expect("re-read catalog"),
            original
        );
    }

    #[test]
    fn persistence_replaces_equivalent_provider_alias() {
        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("models.json");
        let fetched_path = fetched_models_path(&models_path);
        std::fs::write(
            &fetched_path,
            br#"{
  "schema": "pi.models.fetched.v2",
  "providers": {"OpenAI": {"routeFingerprint": "sha256:0000000000000000000000000000000000000000000000000000000000000000", "fetchedAtUnixMs": 1, "models": [{"id": "old-model"}]}}
}
"#,
        )
        .expect("write alias-key catalog");

        let catalog = verified_catalog("openai", vec!["new-model".to_string()]);
        persist_provider_model_catalog(&models_path, &catalog).expect("replace provider alias");
        let catalog = load_persisted_catalog(&fetched_path).expect("reload valid catalog");
        assert_eq!(catalog.providers.len(), 1);
        assert!(!catalog.providers.contains_key("OpenAI"));
        assert_eq!(catalog.providers["openai"].models[0].id, "new-model");
    }

    #[test]
    fn oversized_provider_id_is_rejected_without_touching_catalog() {
        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("models.json");
        let initial = verified_catalog("openai", vec!["existing-model".to_string()]);
        let fetched_path =
            persist_provider_model_catalog(&models_path, &initial).expect("write initial catalog");
        let original = std::fs::read(&fetched_path).expect("read initial catalog");

        let oversized_provider = "p".repeat(MAX_FETCHED_PROVIDER_ID_BYTES + 1);
        let route = built_in_route("openai");
        let oversized =
            verified_catalog_for_route(&oversized_provider, &route, vec!["new-model".to_string()]);
        let error = persist_provider_model_catalog(&models_path, &oversized)
            .expect_err("oversized provider ID must fail");
        assert!(error.to_string().contains("at most"), "{error}");
        assert_eq!(
            std::fs::read(&fetched_path).expect("re-read catalog"),
            original
        );
    }

    #[test]
    fn oversized_serialized_catalog_does_not_replace_valid_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("models.json");
        let models = (0..MAX_FETCHED_MODELS_PER_PROVIDER)
            .map(|index| format!("{index:04}-{}", "x".repeat(495)))
            .collect::<Vec<_>>();
        let openai = verified_catalog("openai", models.clone());
        let fetched_path = persist_provider_model_catalog(&models_path, &openai)
            .expect("one bounded provider should fit");
        let original = std::fs::read(&fetched_path).expect("read first catalog");

        let groq = verified_catalog("groq", models);
        let error = persist_provider_model_catalog(&models_path, &groq)
            .expect_err("combined serialized catalog must exceed the byte limit");
        assert!(error.to_string().contains("serialized size"), "{error}");
        assert_eq!(
            std::fs::read(&fetched_path).expect("re-read preserved catalog"),
            original,
            "a rejected oversized update must preserve the prior valid catalog"
        );
    }

    #[test]
    fn semantically_invalid_generated_catalog_is_not_overwritten() {
        let invalid_catalogs = [
            serde_json::json!({
                "schema": FETCHED_MODELS_SCHEMA,
                "providers": {"openai": {
                    "routeFingerprint": format!("sha256:{}", "0".repeat(64)),
                    "fetchedAtUnixMs": 1,
                    "models": []
                }}
            }),
            serde_json::json!({
                "schema": FETCHED_MODELS_SCHEMA,
                "providers": {
                    "openai": {
                        "routeFingerprint": format!("sha256:{}", "0".repeat(64)),
                        "fetchedAtUnixMs": 1,
                        "models": [{"id": "valid-model"}]
                    },
                    "OpenAI": {
                        "routeFingerprint": format!("sha256:{}", "0".repeat(64)),
                        "fetchedAtUnixMs": 1,
                        "models": [{"id": "another-model"}]
                    }
                }
            }),
            serde_json::json!({
                "schema": FETCHED_MODELS_SCHEMA,
                "providers": {"openai": {
                    "routeFingerprint": format!("sha256:{}", "0".repeat(64)),
                    "fetchedAtUnixMs": 1,
                    "models": [{"id": "bad\nmodel"}]
                }}
            }),
            serde_json::json!({
                "schema": FETCHED_MODELS_SCHEMA,
                "providers": {
                    "openai": {
                        "routeFingerprint": format!("sha256:{}", "0".repeat(64)),
                        "fetchedAtUnixMs": 1,
                        "models": [{"id": "gpt-5.6"}, {"id": "GPT-5.6"}]
                    }
                }
            }),
            serde_json::json!({
                "schema": FETCHED_MODELS_SCHEMA,
                "providers": {
                    "openrouter": {
                        "routeFingerprint": format!("sha256:{}", "0".repeat(64)),
                        "fetchedAtUnixMs": 1,
                        "models": [{"id": "gpt-4o-mini"}, {"id": "openai/gpt-4o-mini"}]
                    }
                }
            }),
        ];

        for invalid_catalog in invalid_catalogs {
            let directory = tempfile::tempdir().expect("tempdir");
            let models_path = directory.path().join("models.json");
            let fetched_path = fetched_models_path(&models_path);
            let original = serde_json::to_vec_pretty(&invalid_catalog).expect("serialize fixture");
            std::fs::write(&fetched_path, &original).expect("write invalid fetched catalog");

            let catalog = verified_catalog("groq", vec!["groq-model".to_string()]);
            let error = persist_provider_model_catalog(&models_path, &catalog)
                .expect_err("semantically invalid generated catalog must fail closed");
            assert!(
                error.to_string().contains("Refusing to overwrite"),
                "{error}"
            );
            assert_eq!(
                std::fs::read(&fetched_path).expect("re-read fetched catalog"),
                original,
                "failed persistence must preserve the existing bytes"
            );
        }
    }
}
