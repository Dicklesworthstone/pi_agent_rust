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
//! `std::sync::OnceLock`) keys results by [`canonical_provider_id`] so that
//! `"anthropic"`, `"Anthropic"`, and any registered alias share a single
//! entry.  Entries expire after [`MODEL_CACHE_TTL`] (5 minutes).  Hits within
//! the TTL window do **not** issue a network call.  Setting
//! `PI_DISABLE_MODEL_CACHE=1` (or `true`/`yes`/`on`) bypasses both the read
//! and write paths for debugging. [`refresh_provider_models`] forces a strict
//! live refetch regardless of cache state and returns an error rather than a
//! static fallback when that refresh fails.
//!
//! ## Fallback
//!
//! When the live fetch fails (network error, non-2xx response, unparseable
//! body), the function logs a `tracing::warn!` describing the failure and
//! returns the static model IDs known to [`ModelRegistry`].  Callers therefore
//! always receive a non-empty list when the provider has any built-in models.
//!
//! ## Extending to non-OpenAI endpoints
//!
//! Providers that do not speak `/v1/models` (e.g. Google Gemini's
//! `/v1beta/models?key=…`, Vertex AI, Bedrock listing APIs, Anthropic's
//! `x-api-key` + `anthropic-version` flavoured `/v1/models`) can be added by
//! branching inside [`fetch_live_models`] on the canonical provider id and
//! supplying a bespoke request builder + JSON shape parser.  Keep the cache
//! key + fallback paths unchanged; only the network call shape varies.

use std::collections::{BTreeMap, HashMap};
#[cfg(unix)]
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::auth::AuthStorage;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::models::{ModelRegistry, default_models_path, fetched_models_path};
use crate::provider_metadata::{
    ProviderRoutingDefaults, canonical_provider_id, provider_routing_defaults,
};

/// TTL applied to every cache entry.  Five minutes balances staleness against
/// rate-limit pressure on provider model catalogs.
pub const MODEL_CACHE_TTL: Duration = Duration::from_mins(5);

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

const FETCHED_MODELS_SCHEMA: &str = "pi.models.fetched.v1";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedFetchedCatalog {
    schema: String,
    providers: BTreeMap<String, PersistedFetchedProvider>,
}

impl Default for PersistedFetchedCatalog {
    fn default() -> Self {
        Self {
            schema: FETCHED_MODELS_SCHEMA.to_string(),
            providers: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedFetchedProvider {
    models: Vec<PersistedFetchedModel>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedFetchedModel {
    id: String,
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

fn cache_key(provider: &str) -> String {
    canonical_provider_id(provider)
        .unwrap_or_else(|| provider.trim())
        .to_ascii_lowercase()
}

fn cache_lookup(key: &str) -> Option<Vec<String>> {
    let guard = cache().lock().ok()?;
    // Extract owned data so the lock guard can be released immediately rather
    // than held across the return (clippy::significant_drop_tightening).
    let cached = guard
        .get(key)
        .filter(|entry| entry.inserted.elapsed() < MODEL_CACHE_TTL)
        .map(|entry| entry.models.clone());
    drop(guard);
    cached
}

fn cache_store(key: String, models: Vec<String>) {
    if let Ok(mut guard) = cache().lock() {
        guard.insert(
            key,
            CacheEntry {
                models,
                inserted: Instant::now(),
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

/// Fetch the live model catalog for `provider`, returning cached results when fresh.
///
/// On any failure to talk to the provider, fall back to the bundled
/// static registry and log a warning so operators can see why the dynamic
/// path degraded.
///
/// `api_key` should be the user's credential for the provider; when empty,
/// the fetch is skipped immediately and the static registry result is used.
/// The static-registry fallback never errors as long as the provider is
/// known — at worst it returns an empty `Vec`.
pub async fn fetch_provider_models(provider: &str, api_key: &str) -> Result<Vec<String>> {
    Ok(fetch_provider_model_catalog(provider, api_key).await?.models)
}

/// Fetch a provider catalog while retaining whether the rows came from a
/// successful live call, the in-process cache, or the static fallback.
pub async fn fetch_provider_model_catalog(
    provider: &str,
    api_key: &str,
) -> Result<ProviderModelCatalog> {
    let key = cache_key(provider);

    if !cache_disabled()
        && let Some(cached) = cache_lookup(&key)
    {
        tracing::debug!(provider = %key, count = cached.len(), "model cache hit");
        return Ok(ProviderModelCatalog {
            models: cached,
            source: ModelCatalogSource::Cache,
        });
    }

    fetch_and_cache(provider, &key, api_key).await
}

/// Force a refresh, bypassing any cached entry. Only a successful, non-empty
/// live response replaces the cache entry; failures are returned to the caller.
pub async fn refresh_provider_models(provider: &str, api_key: &str) -> Result<Vec<String>> {
    Ok(
        refresh_provider_model_catalog(provider, api_key)
            .await?
            .models,
    )
}

/// Force a genuinely live refresh, bypassing the cache and rejecting network,
/// authentication, parse, and empty-catalog failures. This strict behavior
/// prevents callers from mistaking a static fallback for a fresh provider
/// response.
pub async fn refresh_provider_model_catalog(
    provider: &str,
    api_key: &str,
) -> Result<ProviderModelCatalog> {
    let key = cache_key(provider);
    let live = fetch_live_models(provider, api_key).await?;
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
    key: &str,
    api_key: &str,
) -> Result<ProviderModelCatalog> {
    // Only cache results from a successful live fetch — caching the
    // static-registry fallback would pin a stale answer for 5 minutes and
    // silently swallow the next call even after the user adds the missing
    // API key. The fallback path stays correct (callers always get a list)
    // without poisoning the next live attempt.
    match fetch_live_models(provider, api_key).await {
        Ok(live) if !live.is_empty() => {
            if !cache_disabled() {
                cache_store(key.to_string(), live.clone());
            }
            Ok(ProviderModelCatalog {
                models: live,
                source: ModelCatalogSource::Live,
            })
        }
        Ok(_) => {
            tracing::warn!(
                provider = %key,
                "live model fetch returned empty list; falling back to static registry (not cached)"
            );
            Ok(ProviderModelCatalog {
                models: static_registry_models(provider),
                source: ModelCatalogSource::StaticFallback,
            })
        }
        Err(err) => {
            tracing::warn!(
                provider = %key,
                error = %err,
                "live model fetch failed; falling back to static registry (not cached)"
            );
            Ok(ProviderModelCatalog {
                models: static_registry_models(provider),
                source: ModelCatalogSource::StaticFallback,
            })
        }
    }
}

/// Return the static model IDs known to the bundled registry for `provider`.
///
/// Used as the fallback when a live fetch fails.  Loads the on-disk
/// `models.json` (if any) so user-defined catalog overrides are honoured.
pub fn static_registry_models(provider: &str) -> Vec<String> {
    let Ok(auth) = AuthStorage::load(Config::auth_path()) else {
        return Vec::new();
    };
    let models_path = Some(default_models_path(&Config::global_dir()));
    let registry = ModelRegistry::load_for_listing(&auth, models_path);
    let canonical = canonical_provider_id(provider).unwrap_or(provider);
    let mut ids: Vec<String> = registry
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
    ids.sort();
    ids.dedup();
    ids
}

/// Atomically persist one successfully discovered provider catalog beside the
/// user's `models.json`, without ever reading or modifying `models.json`
/// itself. Existing generated catalogs for other providers are retained.
///
/// The generated schema can only encode provider IDs and model IDs, so API
/// keys, headers, and other credentials cannot be written accidentally.
pub fn persist_provider_model_catalog(
    models_path: &Path,
    provider: &str,
    models: &[String],
) -> Result<PathBuf> {
    let provider = provider.trim();
    if provider.is_empty() {
        return Err(Error::validation("provider must not be empty"));
    }

    let mut model_ids = models
        .iter()
        .map(|model| model.trim())
        .filter(|model| !model.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    model_ids.sort_unstable();
    model_ids.dedup();
    if model_ids.is_empty() {
        return Err(Error::validation(
            "refusing to persist an empty fetched model catalog",
        ));
    }

    let path = fetched_models_path(models_path);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.as_os_str().is_empty() {
        std::fs::create_dir_all(parent).map_err(|error| {
            Error::config(format!(
                "Failed to create fetched model catalog directory {}: {error}",
                parent.display()
            ))
        })?;
    }

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
    let provider = cache_key(provider);
    catalog.providers.insert(
        provider,
        PersistedFetchedProvider {
            models: model_ids
                .into_iter()
                .map(|id| PersistedFetchedModel { id })
                .collect(),
        },
    );
    write_persisted_catalog_atomic(&path, &catalog)?;
    Ok(path)
}

fn fetched_catalog_persist_lock() -> &'static Mutex<()> {
    static PERSIST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    PERSIST_LOCK.get_or_init(|| Mutex::new(()))
}

fn load_persisted_catalog(path: &Path) -> Result<PersistedFetchedCatalog> {
    let contents = match std::fs::read_to_string(path) {
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
    let catalog: PersistedFetchedCatalog = serde_json::from_str(&contents).map_err(|error| {
        Error::config(format!(
            "Refusing to overwrite malformed or unrecognized fetched model catalog {}: {error}",
            path.display()
        ))
    })?;
    if catalog.schema != FETCHED_MODELS_SCHEMA {
        return Err(Error::config(format!(
            "Refusing to overwrite fetched model catalog {} with unsupported schema {:?}",
            path.display(),
            catalog.schema
        )));
    }
    Ok(catalog)
}

fn write_persisted_catalog_atomic(path: &Path, catalog: &PersistedFetchedCatalog) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut contents = serde_json::to_string_pretty(catalog).map_err(Error::from)?;
    contents.push('\n');

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

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// JSON shape returned by an OpenAI-compatible `/v1/models` endpoint.
#[derive(Debug, Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<OpenAiModelRow>,
}

#[derive(Debug, Deserialize)]
struct OpenAiModelRow {
    id: String,
}

async fn fetch_live_models(provider: &str, api_key: &str) -> Result<Vec<String>> {
    if api_key.trim().is_empty() {
        return Err(Error::api(
            "no api_key supplied; skipping live provider model fetch",
        ));
    }

    let defaults = provider_routing_defaults(provider).ok_or_else(|| {
        Error::api(format!(
            "provider {provider:?} has no routing defaults; cannot fetch /v1/models"
        ))
    })?;

    let url = openai_compat_models_url(&defaults).ok_or_else(|| {
        Error::api(format!(
            "provider {provider:?} base_url ({}) is not OpenAI-compatible /v1; \
             add a custom branch in fetch_live_models to support its catalog endpoint",
            defaults.base_url
        ))
    })?;

    let client = Client::new();
    let request = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", api_key.trim()))
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(15));

    let response = request.send().await?;
    let status = response.status();
    if !(200..300).contains(&status) {
        let body = response.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(200).collect();
        return Err(Error::api(format!(
            "provider {provider:?} returned HTTP {status} from {url}: {snippet}"
        )));
    }

    let body = response.text().await?;
    let parsed: OpenAiModelsResponse = serde_json::from_str(&body).map_err(|err| {
        Error::api(format!(
            "failed to parse /v1/models response for {provider:?}: {err}"
        ))
    })?;

    let mut ids: Vec<String> = parsed
        .data
        .into_iter()
        .map(|row| row.id)
        .filter(|id| !id.trim().is_empty())
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Derive an OpenAI-compatible `/v1/models` URL from a provider's routing
/// defaults.  Returns `None` for endpoints whose `base_url` does not look
/// like an OpenAI-compatible root (e.g. Anthropic's `…/v1/messages` or
/// Google's `/v1beta` Gemini endpoint, which need bespoke handlers).
fn openai_compat_models_url(defaults: &ProviderRoutingDefaults) -> Option<String> {
    let base = defaults.base_url.trim_end_matches('/');
    if base.is_empty() {
        return None;
    }

    // Skip endpoints whose schema is not OpenAI-compatible.  Anthropic's
    // base_url terminates in `/v1/messages`; Google's terminates in
    // `/v1beta`; Bedrock/Vertex are not HTTP REST in the same shape.
    if base.ends_with("/messages") || base.contains("/v1beta") || base.contains("googleapis.com") {
        return None;
    }

    Some(format!("{base}/models"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_test_lock() -> &'static Mutex<()> {
        static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        TEST_LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn cache_key_canonicalizes_aliases() {
        // anthropic has no aliases, but provider IDs should round-trip lowercased
        assert_eq!(cache_key("OpenAI"), "openai");
        assert_eq!(cache_key("openai"), "openai");
    }

    #[test]
    fn openai_compat_url_for_openai() {
        let defaults = provider_routing_defaults("openai").expect("openai defaults");
        let url = openai_compat_models_url(&defaults).expect("openai is openai-compatible");
        assert_eq!(url, "https://api.openai.com/v1/models");
    }

    #[test]
    fn openai_compat_url_for_groq() {
        let defaults = provider_routing_defaults("groq").expect("groq defaults");
        let url = openai_compat_models_url(&defaults).expect("groq is openai-compatible");
        assert_eq!(url, "https://api.groq.com/openai/v1/models");
    }

    #[test]
    fn openai_compat_url_for_openrouter() {
        let defaults = provider_routing_defaults("openrouter").expect("openrouter defaults");
        let url = openai_compat_models_url(&defaults).expect("openrouter is openai-compatible");
        assert_eq!(url, "https://openrouter.ai/api/v1/models");
    }

    #[test]
    fn openai_compat_url_rejects_anthropic_messages_endpoint() {
        let defaults = provider_routing_defaults("anthropic").expect("anthropic defaults");
        assert!(openai_compat_models_url(&defaults).is_none());
    }

    #[test]
    fn empty_api_key_short_circuits() {
        // We don't make a network call so this should fail with the
        // empty-key sentinel rather than a transport error.
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let err = rt.block_on(fetch_live_models("openai", "  ")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("api_key"), "unexpected error: {msg}");
    }

    #[test]
    fn cache_round_trip_respects_ttl() {
        let _guard = cache_test_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_model_cache();
        let key = cache_key("openai");
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
        cache_store("openai".to_string(), vec!["cached-model".to_string()]);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let catalog = runtime
            .block_on(fetch_provider_model_catalog("openai", ""))
            .expect("cached catalog");
        assert_eq!(catalog.models, vec!["cached-model"]);
        assert_eq!(catalog.source, ModelCatalogSource::Cache);
        clear_model_cache();
    }

    #[test]
    fn refresh_without_credentials_is_strict_instead_of_falling_back() {
        let _guard = cache_test_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_model_cache();
        cache_store("openai".to_string(), vec!["stale-cache".to_string()]);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(refresh_provider_model_catalog("openai", ""))
            .expect_err("refresh must require a live response");
        assert!(error.to_string().contains("api_key"), "{error}");
        assert_eq!(
            cache_lookup("openai"),
            Some(vec!["stale-cache".to_string()]),
            "failed refresh must not replace or disguise the previous cache"
        );
        clear_model_cache();
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
        persist_provider_model_catalog(
            &models_path,
            "groq",
            &["groq-model".to_string()],
        )
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
        assert!(error.to_string().contains("Refusing to overwrite"), "{error}");
        assert_eq!(
            std::fs::read(&fetched_path).expect("re-read fetched catalog"),
            original,
            "failed persistence must preserve the existing bytes"
        );
    }
}
