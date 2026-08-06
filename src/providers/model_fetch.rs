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
//! `std::sync::OnceLock`) keys results by [`canonical_provider_id`] plus a
//! non-reversible credential fingerprint. Provider aliases share an entry,
//! while catalogs from distinct credentials never bleed across accounts;
//! keyless providers use one credential-independent entry. The cache is
//! capped and entries expire after [`MODEL_CACHE_TTL`] (5 minutes). Hits
//! within the TTL window do **not** issue a network call. Setting
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
#[cfg(unix)]
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::{SeqAccess, Visitor};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::auth::AuthStorage;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::http::client::{Client, effective_default_request_timeout};
#[cfg(test)]
use crate::models::FETCHED_MODELS_SCHEMA;
use crate::models::{
    MAX_FETCHED_CATALOG_BYTES, MAX_FETCHED_MODEL_BYTES_PER_PROVIDER, MAX_FETCHED_MODEL_ID_BYTES,
    MAX_FETCHED_MODELS_PER_PROVIDER, MAX_FETCHED_PROVIDER_ID_BYTES, ModelCatalogProviderConfig,
    ModelRegistry, PersistedFetchedCatalog, PersistedFetchedModel, PersistedFetchedProvider,
    canonicalize_model_id_for_provider, default_models_path, fetched_models_path,
    is_safe_model_catalog_identifier, normalized_registry_key, parse_persisted_fetched_catalog,
    read_generated_catalog, resolve_model_catalog_provider_config,
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
    pub models: Vec<String>,
    pub source: ModelCatalogSource,
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
    let canonical_provider = canonical_provider_key(provider);
    let mut hasher = Sha256::new();
    hasher.update(canonical_provider.as_bytes());
    hasher.update([0]);
    hasher.update(route.api.as_bytes());
    hasher.update([0]);
    hasher.update(route.base_url.as_bytes());
    hasher.update([u8::from(route.auth_header)]);
    let has_custom_authorization = custom_authorization_header(route).is_some();
    if route.auth_header && !has_custom_authorization {
        hasher.update(api_key.trim().as_bytes());
    }
    for (name, value) in sorted_route_headers(route) {
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    format!("{}:{:x}", canonical_provider, hasher.finalize())
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

fn cache_lookup(key: &str) -> Option<Vec<String>> {
    let mut guard = cache().lock().ok()?;
    let now = Instant::now();
    if guard.get(key).is_some_and(|entry| {
        now.checked_duration_since(entry.inserted)
            .is_none_or(|elapsed| elapsed >= MODEL_CACHE_TTL)
    }) {
        guard.remove(key);
        return None;
    }
    guard.get(key).map(|entry| entry.models.clone())
}

fn model_id_bytes(models: &[String]) -> usize {
    models
        .iter()
        .fold(0usize, |total, model| total.saturating_add(model.len()))
}

fn cache_store(key: String, models: Vec<String>) {
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

fn effective_catalog_api_key(
    caller_api_key: &str,
    route: Option<&ModelCatalogProviderConfig>,
) -> String {
    let caller_api_key = caller_api_key.trim();
    if caller_api_key.is_empty() {
        route
            .and_then(|route| route.api_key.as_deref())
            .map(str::trim)
            .filter(|api_key| !api_key.is_empty())
            .unwrap_or_default()
            .to_string()
    } else {
        caller_api_key.to_string()
    }
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
        .models)
}

/// Fetch a provider catalog while retaining whether the rows came from a
/// successful live call, the in-process cache, or the static fallback.
pub async fn fetch_provider_model_catalog(
    provider: &str,
    api_key: &str,
) -> Result<ProviderModelCatalog> {
    let provider = validate_provider_id(provider)?;
    let models_path = default_models_path(&Config::global_dir());
    let route = resolve_model_catalog_provider_config(provider, &models_path)?;
    fetch_provider_model_catalog_with_route(provider, api_key, route).await
}

async fn fetch_provider_model_catalog_with_route(
    provider: &str,
    api_key: &str,
    route: Option<ModelCatalogProviderConfig>,
) -> Result<ProviderModelCatalog> {
    let effective_api_key = effective_catalog_api_key(api_key, route.as_ref());
    let key = route
        .as_ref()
        .map(|route| cache_key(provider, &effective_api_key, route));

    if !cache_disabled()
        && let Some(key) = key.as_deref()
        && let Some(cached) = cache_lookup(key)
    {
        tracing::debug!(
            provider = %canonical_provider_key(provider),
            count = cached.len(),
            "model cache hit"
        );
        return Ok(ProviderModelCatalog {
            models: cached,
            source: ModelCatalogSource::Cache,
        });
    }

    fetch_and_cache(provider, key.as_deref(), &effective_api_key, route.as_ref()).await
}

/// Force a refresh, bypassing any cached entry. Only a successful, non-empty
/// live response replaces the cache entry; failures are returned to the caller.
pub async fn refresh_provider_models(provider: &str, api_key: &str) -> Result<Vec<String>> {
    Ok(refresh_provider_model_catalog(provider, api_key)
        .await?
        .models)
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
    let provider = validate_provider_id(provider)?;
    let models_path = default_models_path(&Config::global_dir());
    let route =
        resolve_model_catalog_provider_config(provider, &models_path)?.ok_or_else(|| {
            Error::api(format!(
                "provider {provider:?} has no built-in or models.json routing configuration"
            ))
        })?;
    refresh_provider_model_catalog_with_route(provider, api_key, route).await
}

async fn refresh_provider_model_catalog_with_route(
    provider: &str,
    api_key: &str,
    route: ModelCatalogProviderConfig,
) -> Result<ProviderModelCatalog> {
    let effective_api_key = effective_catalog_api_key(api_key, Some(&route));
    let key = cache_key(provider, &effective_api_key, &route);
    let live = fetch_live_models(provider, &effective_api_key, &route).await?;
    if live.is_empty() {
        return Err(Error::api(format!(
            "live model fetch for {provider:?} returned an empty catalog"
        )));
    }
    if !cache_disabled() {
        cache_store(key, live.clone());
    }
    Ok(ProviderModelCatalog {
        models: live,
        source: ModelCatalogSource::Live,
    })
}

async fn fetch_and_cache(
    provider: &str,
    key: Option<&str>,
    api_key: &str,
    route: Option<&ModelCatalogProviderConfig>,
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
            if !cache_disabled()
                && let Some(key) = key
            {
                cache_store(key.to_string(), live.clone());
            }
            Ok(ProviderModelCatalog {
                models: live,
                source: ModelCatalogSource::Live,
            })
        }
        Ok(_) => {
            tracing::warn!(
                provider = %canonical_provider,
                "live model fetch returned empty list; falling back to static registry (not cached)"
            );
            Ok(ProviderModelCatalog {
                models: static_registry_models(provider)?,
                source: ModelCatalogSource::StaticFallback,
            })
        }
        Err(err) => {
            tracing::warn!(
                provider = %canonical_provider,
                error = %err,
                "live model fetch failed; falling back to static registry (not cached)"
            );
            Ok(ProviderModelCatalog {
                models: static_registry_models(provider)?,
                source: ModelCatalogSource::StaticFallback,
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
    let auth = AuthStorage::load(Config::auth_path())?;
    let models_path = Some(default_models_path(&Config::global_dir()));
    let registry = ModelRegistry::load_for_listing(&auth, models_path);
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
/// user's `models.json`, without ever reading or modifying `models.json`
/// itself.
///
/// Existing generated catalogs for other providers are retained.
///
/// The generated schema can only encode provider IDs and model IDs, so API
/// keys, headers, and other credentials cannot be written accidentally.
pub fn persist_provider_model_catalog(
    models_path: &Path,
    provider: &str,
    models: &[String],
) -> Result<PathBuf> {
    let provider = validate_provider_id(provider)?;

    let model_ids = normalize_model_ids(provider, models.iter().cloned())?;
    if model_ids.is_empty() {
        return Err(Error::validation(
            "refusing to persist an empty fetched model catalog",
        ));
    }

    let path = fetched_models_path(models_path);
    let parent = catalog_parent(&path);
    std::fs::create_dir_all(parent).map_err(|error| {
        Error::config(format!(
            "Failed to create fetched model catalog directory {}: {error}",
            parent.display()
        ))
    })?;

    let _process_guard = fetched_catalog_persist_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _file_guard = crate::file_lock::DirLock::acquire_for(&path, Duration::from_secs(30))
        .map_err(|error| {
            Error::config(format!(
                "Failed to lock fetched model catalog {}: {error}",
                path.display()
            ))
        })?;

    let mut catalog = load_persisted_catalog(&path)?;
    let provider = canonical_provider_key(provider);
    catalog
        .providers
        .retain(|existing, _| canonical_provider_key(existing) != provider);
    catalog.providers.insert(
        provider,
        PersistedFetchedProvider {
            models: model_ids
                .into_iter()
                .map(|id| PersistedFetchedModel { id })
                .collect(),
        },
    );
    validate_persisted_fetched_catalog(&catalog)?;
    write_persisted_catalog_atomic(&path, &catalog)?;
    Ok(path)
}

fn fetched_catalog_persist_lock() -> &'static Mutex<()> {
    static PERSIST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    PERSIST_LOCK.get_or_init(|| Mutex::new(()))
}

fn load_persisted_catalog(path: &Path) -> Result<PersistedFetchedCatalog> {
    let contents = match read_generated_catalog(path) {
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
    let catalog = parse_persisted_fetched_catalog(&contents).map_err(|error| {
        Error::config(format!(
            "Refusing to overwrite malformed or unrecognized fetched model catalog {}: {error}",
            path.display()
        ))
    })?;
    Ok(catalog)
}

fn write_persisted_catalog_atomic(path: &Path, catalog: &PersistedFetchedCatalog) -> Result<()> {
    let parent = catalog_parent(path);
    let mut contents = serde_json::to_string_pretty(catalog).map_err(Error::from)?;
    contents.push('\n');
    if contents.len() > MAX_FETCHED_CATALOG_BYTES {
        return Err(Error::config(format!(
            "Refusing to persist generated model catalog: serialized size {} exceeds {MAX_FETCHED_CATALOG_BYTES} bytes",
            contents.len()
        )));
    }

    let mut temporary = NamedTempFile::new_in(parent).map_err(|error| {
        Error::config(format!(
            "Failed to create temporary fetched model catalog in {}: {error}",
            parent.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                Error::config(format!(
                    "Failed to secure temporary fetched model catalog: {error}"
                ))
            })?;
    }
    temporary.write_all(contents.as_bytes()).map_err(|error| {
        Error::config(format!(
            "Failed to write temporary fetched model catalog: {error}"
        ))
    })?;
    temporary.as_file().sync_all().map_err(|error| {
        Error::config(format!(
            "Failed to sync temporary fetched model catalog: {error}"
        ))
    })?;
    temporary.persist(path).map_err(|error| {
        Error::config(format!(
            "Failed to atomically persist fetched model catalog to {}: {}",
            path.display(),
            error.error
        ))
    })?;
    sync_parent_directory(path).map_err(|error| {
        Error::config(format!(
            "Failed to sync fetched model catalog directory for {}: {error}",
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

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    File::open(catalog_parent(path))?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
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
    }

    #[test]
    fn explicit_or_resolved_auth_precedes_models_json_catalog_key() {
        let mut route = built_in_route("openai");
        route.api_key = Some("models-json-key".to_string());
        assert_eq!(
            effective_catalog_api_key("", Some(&route)),
            "models-json-key"
        );
        assert_eq!(
            effective_catalog_api_key("caller-resolved-key", Some(&route)),
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
        cache_store(key.clone(), vec!["m-1".to_string(), "m-2".to_string()]);
        let hit = cache_lookup(&key).expect("fresh entry");
        assert_eq!(hit, vec!["m-1".to_string(), "m-2".to_string()]);
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
        cache_store(key, vec!["cached-model".to_string()]);
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
        cache_store("fresh-store".to_string(), vec!["fresh-model".to_string()]);
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
        cache_store(key.clone(), vec!["stale-cache".to_string()]);
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
            Some(vec!["stale-cache".to_string()]),
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

        let fetched_path = persist_provider_model_catalog(
            &models_path,
            "OpenRouter",
            &[
                "z/model".to_string(),
                "a/model".to_string(),
                "a/model".to_string(),
                "  ".to_string(),
            ],
        )
        .expect("persist OpenRouter catalog");
        persist_provider_model_catalog(&models_path, "groq", &["groq-model".to_string()])
            .expect("persist Groq catalog");

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
        assert!(!encoded.contains("apiKey"));
        assert!(!encoded.contains("do-not-touch"));
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
        persist_provider_model_catalog(&models_path, "acme", &["acme-model".to_string()])
            .expect("persist custom provider membership");
        let auth = AuthStorage::load(directory.path().join("auth.json")).expect("load empty auth");

        let configured = ModelRegistry::load(&auth, Some(models_path.clone()))
            .find("acme", "acme-model")
            .expect("manual route makes persisted membership routable");
        assert_eq!(configured.model.base_url, "https://acme.example/v1");
        assert_eq!(configured.api_key.as_deref(), Some("manual-secret"));

        std::fs::rename(&models_path, &backup_path).expect("preserve manual route elsewhere");
        let missing_route = ModelRegistry::load(&auth, Some(models_path.clone()));
        assert!(
            missing_route.find("acme", "acme-model").is_none(),
            "generated IDs must not synthesize an unsafe default route"
        );

        std::fs::write(&models_path, "{ malformed").expect("write malformed manual route");
        let malformed_route = ModelRegistry::load(&auth, Some(models_path.clone()));
        assert!(malformed_route.error().is_some());
        assert!(malformed_route.find("acme", "acme-model").is_none());

        std::fs::write(&models_path, manual).expect("restore valid manual route bytes");
        let restored = ModelRegistry::load(&auth, Some(models_path))
            .find("acme", "acme-model")
            .expect("restoring the manual route restores generated membership");
        assert_eq!(restored.model.base_url, "https://acme.example/v1");
    }

    #[test]
    fn malformed_generated_catalog_is_not_overwritten() {
        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("models.json");
        let fetched_path = fetched_models_path(&models_path);
        let original = b"not generated catalog json\n";
        std::fs::write(&fetched_path, original).expect("write malformed fetched catalog");

        let error = persist_provider_model_catalog(
            &models_path,
            "openrouter",
            &["openai/gpt-test".to_string()],
        )
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
    fn duplicate_json_keys_are_rejected_without_overwriting() {
        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("models.json");
        let fetched_path = fetched_models_path(&models_path);
        let original = br#"{
  "schema": "pi.models.fetched.v1",
  "providers": {
    "openai": {"models": [{"id": "first"}]},
    "openai": {"models": [{"id": "second"}]}
  }
}
"#;
        std::fs::write(&fetched_path, original).expect("write duplicate-key catalog");

        let error =
            persist_provider_model_catalog(&models_path, "groq", &["groq-model".to_string()])
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
  "schema": "pi.models.fetched.v1",
  "providers": {"OpenAI": {"models": [{"id": "old-model"}]}}
}
"#,
        )
        .expect("write alias-key catalog");

        persist_provider_model_catalog(&models_path, "openai", &["new-model".to_string()])
            .expect("replace provider alias");
        let catalog = load_persisted_catalog(&fetched_path).expect("reload valid catalog");
        assert_eq!(catalog.providers.len(), 1);
        assert!(!catalog.providers.contains_key("OpenAI"));
        assert_eq!(catalog.providers["openai"].models[0].id, "new-model");
    }

    #[test]
    fn oversized_provider_id_is_rejected_without_touching_catalog() {
        let directory = tempfile::tempdir().expect("tempdir");
        let models_path = directory.path().join("models.json");
        let fetched_path =
            persist_provider_model_catalog(&models_path, "openai", &["existing-model".to_string()])
                .expect("write initial catalog");
        let original = std::fs::read(&fetched_path).expect("read initial catalog");

        let error = persist_provider_model_catalog(
            &models_path,
            &"p".repeat(MAX_FETCHED_PROVIDER_ID_BYTES + 1),
            &["new-model".to_string()],
        )
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
        let fetched_path = persist_provider_model_catalog(&models_path, "openai", &models)
            .expect("one bounded provider should fit");
        let original = std::fs::read(&fetched_path).expect("read first catalog");

        let error = persist_provider_model_catalog(&models_path, "groq", &models)
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
                "providers": {"openai": {"models": []}}
            }),
            serde_json::json!({
                "schema": FETCHED_MODELS_SCHEMA,
                "providers": {
                    "openai": {"models": [{"id": "valid-model"}]},
                    "OpenAI": {"models": [{"id": "another-model"}]}
                }
            }),
            serde_json::json!({
                "schema": FETCHED_MODELS_SCHEMA,
                "providers": {"openai": {"models": [{"id": "bad\nmodel"}]}}
            }),
            serde_json::json!({
                "schema": FETCHED_MODELS_SCHEMA,
                "providers": {
                    "openai": {"models": [{"id": "gpt-5.6"}, {"id": "GPT-5.6"}]}
                }
            }),
            serde_json::json!({
                "schema": FETCHED_MODELS_SCHEMA,
                "providers": {
                    "openrouter": {
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

            let error =
                persist_provider_model_catalog(&models_path, "groq", &["groq-model".to_string()])
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
