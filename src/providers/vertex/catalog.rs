//! Read-only Vertex AI Model Garden publisher listing.
//!
//! This is a list of catalog *candidates*, not project entitlement or proof of
//! streaming inference compatibility. Account/project-specific discovery is not
//! safe to persist in Pi's route-only fetched catalog schema.

use crate::error::{Error, Result};
use crate::http::client::{Client, effective_default_request_timeout};
use crate::models::{
    MAX_FETCHED_CATALOG_BYTES, MAX_FETCHED_MODELS_PER_PROVIDER, ModelCatalogProviderConfig,
};
use std::collections::HashSet;
use url::Url;

const MAX_PAGES: usize = 16;
const MAX_CURSOR_BYTES: usize = 2048;
const DEFAULT_LOCATION: &str = "us-central1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CatalogScope {
    pub project: String,
    pub location: String,
}

fn safe_project(value: &str) -> bool {
    let bytes = value.as_bytes();
    (6..=63).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && (bytes[bytes.len() - 1].is_ascii_lowercase() || bytes[bytes.len() - 1].is_ascii_digit())
        && bytes.iter().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || *ch == b'-')
}

fn safe_location(value: &str) -> bool {
    value == "global"
        || (!value.is_empty() && value.len() <= 64
            && value.as_bytes()[0].is_ascii_lowercase()
            && value.bytes().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'-'))
}

/// Resolve only the same Google Cloud configuration fields used by inference.
/// These values intentionally never appear in request logs or a persisted
/// catalog. A project is required even though Model Garden listing itself is
/// publisher-scoped rather than project-scoped.
pub(super) fn catalog_scope_with_env(
    mut env: impl FnMut(&str) -> Option<String>,
) -> Result<CatalogScope> {
    let project = env("GOOGLE_CLOUD_PROJECT")
        .or_else(|| env("VERTEX_PROJECT"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::config("Google Vertex model discovery requires GOOGLE_CLOUD_PROJECT or VERTEX_PROJECT"))?;
    let location = env("GOOGLE_CLOUD_LOCATION")
        .or_else(|| env("VERTEX_LOCATION"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_LOCATION.to_string());
    if !safe_project(&project) {
        return Err(Error::config("invalid Google Cloud project ID for Vertex model discovery"));
    }
    if !safe_location(&location) {
        return Err(Error::config("invalid Google Cloud location for Vertex model discovery"));
    }
    Ok(CatalogScope { project, location })
}

pub(super) fn catalog_endpoint(
    base_url: &str,
    location: &str,
    publisher: &str,
) -> Result<Url> {
    // Inference routes are not Model Garden listing endpoints. Never send
    // Google Authorization credentials to a user-specified proxy or hostname.
    if !base_url.trim().is_empty() {
        return Err(Error::config("Vertex native model discovery requires its canonical Google endpoint, not a custom models.json baseUrl"));
    }
    if !safe_location(location) || !matches!(publisher, "google" | "anthropic") {
        return Err(Error::config("invalid Vertex location or publisher for model discovery"));
    }
    let host = if location == "global" {
        "aiplatform.googleapis.com".to_string()
    } else {
        format!("{location}-aiplatform.googleapis.com")
    };
    Url::parse(&format!("https://{host}/v1beta1/publishers/{publisher}/models"))
        .map_err(|_| Error::config("invalid Vertex Model Garden endpoint"))
}

pub(super) fn parse_publisher_page(
    publisher: &str,
    body: &[u8],
) -> Result<(Vec<String>, Option<String>)> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| Error::api("Vertex Model Garden returned invalid JSON"))?;
    let rows = value.get("publisherModels").and_then(serde_json::Value::as_array)
        .ok_or_else(|| Error::api("Vertex Model Garden listing lacks publisherModels array"))?;
    if rows.len() > MAX_FETCHED_MODELS_PER_PROVIDER {
        return Err(Error::api("Vertex Model Garden page exceeded model-count limit"));
    }
    let prefix = format!("publishers/{publisher}/models/");
    let mut models = Vec::new();
    for row in rows {
        let name = row.get("name").and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::api("Vertex Model Garden row lacks model name"))?;
        let Some(id) = name.strip_prefix(&prefix) else { continue };
        // Do not advertise embedding, image, video, or unfamiliar model
        // families merely because they occur in a publisher catalog.
        let supported = match publisher {
            "google" => id.starts_with("gemini-") && !id.contains("embedding"),
            "anthropic" => id.starts_with("claude-"),
            _ => false,
        };
        if supported {
            models.push(id.to_string());
        }
    }
    let cursor = match value.get("nextPageToken") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value)) if value.is_empty() => None,
        Some(serde_json::Value::String(value)) if value.len() <= MAX_CURSOR_BYTES => Some(value.clone()),
        _ => return Err(Error::api("Vertex Model Garden pagination cursor is invalid or too long")),
    };
    Ok((models, cursor))
}

fn explicit_authorization(
    route: &ModelCatalogProviderConfig,
    api_key: &str,
) -> Result<String> {
    let explicit = route.headers.iter()
        .find(|(name, value)| name.eq_ignore_ascii_case("Authorization") && !value.trim().is_empty())
        .map(|(_, value)| value.trim().to_string());
    if route.headers.keys().any(|name| !name.eq_ignore_ascii_case("Authorization")) {
        return Err(Error::config("Vertex Model Garden discovery only permits the Authorization route header"));
    }
    if let Some(explicit) = explicit {
        return Ok(explicit);
    }
    let token = api_key.trim();
    if token.is_empty() {
        return Err(Error::auth("Vertex Model Garden discovery needs a Google access token or Authorization header"));
    }
    Ok(format!("Bearer {token}"))
}

/// List publisher Model Garden candidates with a bounded, read-only API call.
///
/// Requires the same explicit Google access token mechanism as the current
/// Vertex inference adapter; it does not infer a service-account access token
/// from a service-account JSON file or run shell commands.
pub(super) async fn discover_model_ids(
    api_key: &str,
    route: &ModelCatalogProviderConfig,
) -> Result<Vec<String>> {
    let scope = catalog_scope_with_env(|name| std::env::var(name).ok())?;
    let authorization = explicit_authorization(route, api_key)?;
    let client = Client::new();
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for publisher in ["google", "anthropic"] {
        let endpoint = catalog_endpoint(&route.base_url, &scope.location, publisher)?;
        let mut cursor: Option<String> = None;
        let mut cursors = HashSet::new();
        for page in 0..MAX_PAGES {
            let mut url = endpoint.clone();
            url.query_pairs_mut().append_pair("pageSize", "100");
            if let Some(after) = cursor.as_deref() {
                url.query_pairs_mut().append_pair("pageToken", after);
            }
            let url_text = url.to_string();
            let request = client.get(&url_text)
                .header("Accept", "application/json")
                .try_header("Authorization", authorization.as_str())?;
            let operation = async move {
                let response = request.send().await?;
                let status = response.status();
                let body = response.bytes_limited(MAX_FETCHED_CATALOG_BYTES).await?;
                Ok::<_, Error>((status, body))
            };
            let (status, body) = if let Some(timeout) = effective_default_request_timeout(&url_text) {
                asupersync::time::timeout(asupersync::time::wall_now(), timeout, Box::pin(operation))
                    .await.map_err(|_| Error::api("Vertex Model Garden listing request timed out"))??
            } else {
                operation.await?
            };
            if !(200..300).contains(&status) {
                // No response body: provider responses may echo tokens or project IDs.
                return Err(Error::api(format!("Vertex Model Garden listing returned HTTP {status}")));
            }
            let (page_ids, next) = parse_publisher_page(publisher, &body)?;
            for id in page_ids {
                if seen.insert(id.clone()) {
                    ids.push(id);
                    if ids.len() > MAX_FETCHED_MODELS_PER_PROVIDER {
                        return Err(Error::api("Vertex Model Garden exceeded the model-count limit"));
                    }
                }
            }
            let Some(next) = next else { break };
            if !cursors.insert(next.clone()) || page + 1 == MAX_PAGES {
                return Err(Error::api("Vertex Model Garden pagination cycle or page limit"));
            }
            cursor = Some(next);
        }
    }
    Ok(ids)
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
