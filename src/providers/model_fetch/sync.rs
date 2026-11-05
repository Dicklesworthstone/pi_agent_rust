//! Bounded freshness-based provider catalog synchronization.
//! Only authenticated, supported routes are queried. A failed refresh does
//! not turn a static fallback into a successful persisted model list.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::AuthStorage;
use crate::models::{MAX_FETCHED_CATALOG_BYTES, ModelRegistry, fetched_models_path};

use super::{
    persist_provider_model_catalog, prepare_provider_model_catalog_fetch,
    provider_model_catalog_route_is_configured,
};

pub const MAX_CATALOG_AGE_MS: u64 = 6 * 60 * 60 * 1000;

fn stale_timestamp(last: Option<u64>, now: Option<u64>) -> bool {
    match (last, now) {
        (Some(last), Some(now)) if now >= last => now - last >= MAX_CATALOG_AGE_MS,
        _ => true,
    }
}

/// Whether this provider's verified catalog entry is absent, stale or invalid.
/// The read never exceeds the generated-catalog size limit.
pub fn provider_catalog_refresh_due(models_path: &Path, provider: &str) -> bool {
    let path = fetched_models_path(models_path);
    let Ok(metadata) = fs::symlink_metadata(&path) else { return true; };
    if !metadata.is_file() || metadata.len() > MAX_FETCHED_CATALOG_BYTES as u64 {
        return true;
    }
    let Ok(contents) = crate::models::read_generated_catalog(&path) else {
        return true;
    };
    let Ok(catalog) = crate::models::parse_persisted_fetched_catalog(&contents) else {
        return true;
    };
    let last = catalog.providers.get(provider).map(|record| record.fetched_at_unix_ms);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
    stale_timestamp(last, now)
}

/// Refresh configured, ready providers; leave manually declared models alone.
/// The caller can reload the registry from disk if any provider was persisted.
pub async fn sync_ready_model_catalogs(
    auth: &AuthStorage,
    registry: &ModelRegistry,
    models_path: &Path,
    force: bool,
) -> usize {
    if !force && std::env::var("PI_DISABLE_AUTO_MODEL_SYNC")
        .is_ok_and(|value| matches!(
            value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"
        ))
    {
        return 0;
    }
    let providers = registry.available_models().into_iter()
        .map(|entry| entry.model.provider.clone())
        .collect::<BTreeSet<_>>();
    let mut persisted = 0;
    for provider in providers {
        // These native discovery results are scoped to a cloud account or
        // project and cannot be bound safely in the route-only disk schema.
        // Avoid polling endpoints we must refuse to persist.
        if matches!(
            super::canonical_provider_key(&provider).as_str(),
            "amazon-bedrock" | "google-vertex"
        ) {
            continue;
        }
        if !force && !provider_catalog_refresh_due(models_path, &provider) {
            continue;
        }
        if !provider_model_catalog_route_is_configured(&provider).unwrap_or(false) {
            continue;
        }
        let Ok(plan) = prepare_provider_model_catalog_fetch(&provider) else {
            continue;
        };
        let key = if plan.requires_runtime_api_key() {
            auth.resolve_api_key(&provider, None).unwrap_or_default()
        } else {
            String::new()
        };
        if plan.requires_runtime_api_key() && key.is_empty() {
            continue;
        }
        match plan.fetch(&key, true).await {
            Ok(catalog) => match persist_provider_model_catalog(models_path, &catalog) {
                Ok(_) => persisted += 1,
                Err(error) => tracing::warn!(
                    provider, error = %error, "model sync could not persist membership"
                ),
            },
            Err(error) => tracing::warn!(
                provider, error = %error, "model sync failed; keeping last verified catalog"
            ),
        }
    }
    persisted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_catalog_is_due() {
        let dir = tempfile::tempdir().unwrap();
        assert!(provider_catalog_refresh_due(&dir.path().join("models.json"), "openai"));
    }

    #[test]
    fn timestamp_must_be_present_trusted_and_old_enough() {
        let now = 1_900_000_000_000_u64;
        assert!(stale_timestamp(None, Some(now)));
        assert!(stale_timestamp(Some(now + 1), Some(now)));
        assert!(!stale_timestamp(Some(now), Some(now)));
        assert!(!stale_timestamp(Some(now - MAX_CATALOG_AGE_MS + 1), Some(now)));
        assert!(stale_timestamp(Some(now - MAX_CATALOG_AGE_MS), Some(now)));
    }
}
