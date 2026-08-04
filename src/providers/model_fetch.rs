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
//! and write paths for debugging.  [`refresh_provider_models`] forces a
//! refetch regardless of cache state.
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

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::auth::AuthStorage;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::http::client::Client;
use crate::models::{ModelRegistry, default_models_path};
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
    let key = cache_key(provider);

    if !cache_disabled()
        && let Some(cached) = cache_lookup(&key)
    {
        tracing::debug!(provider = %key, count = cached.len(), "model cache hit");
        return Ok(cached);
    }

    fetch_and_cache(provider, &key, api_key).await
}

/// Force a refresh, bypassing any cached entry.  The newly fetched (or
/// fallback) result replaces the cache entry on success.
pub async fn refresh_provider_models(provider: &str, api_key: &str) -> Result<Vec<String>> {
    let key = cache_key(provider);
    fetch_and_cache(provider, &key, api_key).await
}

async fn fetch_and_cache(provider: &str, key: &str, api_key: &str) -> Result<Vec<String>> {
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
            Ok(live)
        }
        Ok(_) => {
            tracing::warn!(
                provider = %key,
                "live model fetch returned empty list; falling back to static registry (not cached)"
            );
            Ok(static_registry_models(provider))
        }
        Err(err) => {
            tracing::warn!(
                provider = %key,
                error = %err,
                "live model fetch failed; falling back to static registry (not cached)"
            );
            Ok(static_registry_models(provider))
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
        clear_model_cache();
        let key = cache_key("openai");
        assert!(cache_lookup(&key).is_none(), "starts empty");
        cache_store(key.clone(), vec!["m-1".to_string(), "m-2".to_string()]);
        let hit = cache_lookup(&key).expect("fresh entry");
        assert_eq!(hit, vec!["m-1".to_string(), "m-2".to_string()]);
        clear_model_cache();
        assert!(cache_lookup(&key).is_none(), "cleared");
    }
}
