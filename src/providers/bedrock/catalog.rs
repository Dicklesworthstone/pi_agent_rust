//! Read-only Amazon Bedrock control-plane model discovery.
//!
//! The Bedrock Runtime Converse URL is not a model-list endpoint. Use the
//! regional Bedrock control plane, with the same structured AWS credential
//! resolver as inference. Never cache or persist these unscoped account results.

use super::{BedrockAuth, BedrockAuthContext, BedrockProvider, build_sigv4_headers_for_method};
use crate::error::{Error, Result};
use crate::http::client::{Client, effective_default_request_timeout};
use crate::models::{
    MAX_FETCHED_CATALOG_BYTES, MAX_FETCHED_MODELS_PER_PROVIDER, ModelCatalogProviderConfig,
};
use crate::provider::StreamOptions;
use chrono::Utc;
use std::collections::HashSet;
use url::Url;

const MAX_PAGES: usize = 16;
const MAX_CURSOR_BYTES: usize = 2048;

fn catalog_root(base_url: &str, region: &str) -> Result<Url> {
    // Runtime overrides may be gateways for inference, not control-plane APIs.
    // Never send AWS credentials to an arbitrary configured model endpoint.
    if !base_url.trim().is_empty() {
        return Err(Error::config(
            "Bedrock native model discovery requires the regional AWS control plane; remove the custom models.json baseUrl to use the default AWS endpoint",
        ));
    }
    if region.is_empty()
        || region.len() > 64
        || region.starts_with('-')
        || region.ends_with('-')
        || !region.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(Error::config("invalid AWS region for Bedrock model discovery"));
    }
    Url::parse(&format!("https://bedrock.{region}.amazonaws.com"))
        .map_err(|_| Error::config("invalid regional Bedrock control-plane URL"))
}

fn parse_foundation_models(body: &[u8]) -> Result<Vec<String>> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| Error::api("Bedrock returned malformed foundation-model JSON"))?;
    let rows = value.get("modelSummaries").and_then(serde_json::Value::as_array)
        .ok_or_else(|| Error::api("Bedrock foundation-model listing lacks modelSummaries"))?;
    if rows.len() > MAX_FETCHED_MODELS_PER_PROVIDER {
        return Err(Error::api("Bedrock foundation-model listing exceeds model count limit"));
    }
    let mut ids = Vec::new();
    for row in rows {
        if row.get("modelLifecycle").and_then(|value| value.get("status"))
            .and_then(serde_json::Value::as_str) == Some("END_OF_LIFE")
        {
            continue;
        }
        let streams = row.get("responseStreamingSupported")
            .and_then(serde_json::Value::as_bool) == Some(true);
        let text_output = row.get("outputModalities")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|modalities| modalities.iter().any(|item| item.as_str() == Some("TEXT")));
        let on_demand = row.get("inferenceTypesSupported")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|types| types.iter().any(|item| item.as_str() == Some("ON_DEMAND")));
        if !(streams && text_output && on_demand) {
            continue;
        }
        let id = row.get("modelId").and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::api("Bedrock foundation-model row lacks modelId"))?;
        ids.push(id.to_string());
    }
    Ok(ids)
}

fn parse_inference_profiles(body: &[u8]) -> Result<(Vec<String>, Option<String>)> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| Error::api("Bedrock returned malformed inference-profile JSON"))?;
    let rows = value.get("inferenceProfileSummaries").and_then(serde_json::Value::as_array)
        .ok_or_else(|| Error::api("Bedrock inference-profile listing lacks inferenceProfileSummaries"))?;
    if rows.len() > MAX_FETCHED_MODELS_PER_PROVIDER {
        return Err(Error::api("Bedrock inference-profile listing exceeds model count limit"));
    }
    let mut ids = Vec::new();
    for row in rows {
        if row.get("status").and_then(serde_json::Value::as_str) != Some("ACTIVE") {
            continue;
        }
        let id = row.get("inferenceProfileId").and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::api("Bedrock active inference-profile row lacks inferenceProfileId"))?;
        ids.push(id.to_string());
    }
    let cursor = match value.get("nextToken") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value)) if value.is_empty() => None,
        Some(serde_json::Value::String(value)) if value.len() <= MAX_CURSOR_BYTES => {
            Some(value.clone())
        }
        _ => return Err(Error::api("Bedrock inference-profile cursor is invalid or oversized")),
    };
    Ok((ids, cursor))
}

async fn fetch_page(
    client: &Client,
    root: &Url,
    path: &str,
    cursor: Option<&str>,
    auth: &BedrockAuthContext,
) -> Result<Vec<u8>> {
    let mut url = root.clone();
    url.set_path(path);
    url.set_query(None);
    if path == "/inference-profiles" {
        url.query_pairs_mut().append_pair("maxResults", "100");
        if let Some(cursor) = cursor {
            url.query_pairs_mut().append_pair("nextToken", cursor);
        }
    }
    let url_text = url.to_string();
    let mut request = client.get(&url_text)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json");
    match &auth.auth {
        BedrockAuth::Bearer { token } => {
            request = request.try_header("Authorization", format!("Bearer {token}"))?;
        }
        BedrockAuth::Sigv4 { access_key_id, secret_access_key, session_token } => {
            let signed = build_sigv4_headers_for_method(
                "GET", &url, b"", access_key_id, secret_access_key,
                session_token.as_deref(), &auth.region, Utc::now(),
            )?;
            request = request
                .try_header("Authorization", signed.authorization)?
                .try_header("x-amz-date", signed.amz_date)?
                .try_header("x-amz-content-sha256", signed.payload_hash)?;
            if let Some(token) = signed.security_token {
                request = request.try_header("x-amz-security-token", token)?;
            }
        }
    }
    let operation = async move {
        let response = request.send().await?;
        let status = response.status();
        let body = response.bytes_limited(MAX_FETCHED_CATALOG_BYTES).await?;
        Ok::<_, Error>((status, body))
    };
    let (status, body) = if let Some(timeout) = effective_default_request_timeout(&url_text) {
        asupersync::time::timeout(asupersync::time::wall_now(), timeout, Box::pin(operation))
            .await.map_err(|_| Error::api("Bedrock model-list request timed out"))??
    } else {
        operation.await?
    };
    if !(200..300).contains(&status) {
        // Provider responses sometimes echo authorization inputs; do not log
        // response bodies, endpoint queries, AWS account IDs, or credentials.
        return Err(Error::api(format!(
            "Bedrock control-plane model listing returned HTTP {status}"
        )));
    }
    Ok(body.to_vec())
}

/// Discover model IDs and active inference-profile IDs with the inference
/// provider's AWS credential chain. No global cross-account cache is used.
pub(super) async fn discover_model_ids(
    api_key: &str,
    route: &ModelCatalogProviderConfig,
) -> Result<Vec<String>> {
    if route.auth_header || !route.headers.is_empty() {
        return Err(Error::config(
            "Bedrock native discovery cannot use custom Authorization or unsigned route headers",
        ));
    }
    let options = StreamOptions {
        api_key: (!api_key.trim().is_empty()).then(|| api_key.trim().to_string()),
        ..StreamOptions::default()
    };
    let auth = BedrockProvider::new("").resolve_auth_context(&options).await?;
    let root = catalog_root(&route.base_url, &auth.region)?;
    let client = Client::new();
    let foundation = fetch_page(&client, &root, "/foundation-models", None, &auth).await?;
    let mut ids = parse_foundation_models(&foundation)?;
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    for page in 0..MAX_PAGES {
        let body = fetch_page(&client, &root, "/inference-profiles", cursor.as_deref(), &auth).await?;
        let (profiles, next) = parse_inference_profiles(&body)?;
        ids.extend(profiles);
        if ids.len() > MAX_FETCHED_MODELS_PER_PROVIDER {
            return Err(Error::api("Bedrock discovered catalog exceeds model count limit"));
        }
        let Some(next) = next else {
            return Ok(ids);
        };
        if !seen.insert(next.clone()) || page + 1 == MAX_PAGES {
            return Err(Error::api("Bedrock inference-profile pagination cycle or page limit"));
        }
        cursor = Some(next);
    }
    Err(Error::api("Bedrock inference-profile page limit exceeded"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foundation_list_only_advertises_streaming_text_on_demand_models() {
        let body = br#"{"modelSummaries":[
          {"modelId":"usable","responseStreamingSupported":true,"outputModalities":["TEXT"],"inferenceTypesSupported":["ON_DEMAND"],"modelLifecycle":{"status":"ACTIVE"}},
          {"modelId":"image","responseStreamingSupported":true,"outputModalities":["IMAGE"],"inferenceTypesSupported":["ON_DEMAND"]},
          {"modelId":"batch","responseStreamingSupported":true,"outputModalities":["TEXT"],"inferenceTypesSupported":["PROVISIONED"]},
          {"modelId":"nonstreaming","responseStreamingSupported":false,"outputModalities":["TEXT"],"inferenceTypesSupported":["ON_DEMAND"]},
          {"modelId":"retired","responseStreamingSupported":true,"outputModalities":["TEXT"],"inferenceTypesSupported":["ON_DEMAND"],"modelLifecycle":{"status":"END_OF_LIFE"}}
        ]}"#;
        assert_eq!(parse_foundation_models(body).unwrap(), vec!["usable"]);
        assert!(parse_foundation_models(br#"{"models":[]}"#).is_err());
    }

    #[test]
    fn active_inference_profiles_preserve_pagination_without_inventing_ids() {
        let body = br#"{"inferenceProfileSummaries":[
          {"inferenceProfileId":"us.anthropic.model","status":"ACTIVE"},
          {"inferenceProfileId":"disabled","status":"INACTIVE"}
        ],"nextToken":"page2"}"#;
        let (models, cursor) = parse_inference_profiles(body).unwrap();
        assert_eq!(models, vec!["us.anthropic.model"]);
        assert_eq!(cursor.as_deref(), Some("page2"));
        assert!(parse_inference_profiles(br#"{"nextToken":"page2"}"#).is_err());
        assert!(parse_inference_profiles(br#"{"inferenceProfileSummaries":[],"nextToken":17}"#).is_err());
    }

    #[test]
    fn discovery_only_targets_the_regional_aws_control_plane() {
        assert_eq!(
            catalog_root("", "us-east-1").unwrap().as_str(),
            "https://bedrock.us-east-1.amazonaws.com/"
        );
        assert!(catalog_root("https://proxy.example", "us-east-1").is_err());
        assert!(catalog_root("", "us-east-1.evil.example").is_err());
        assert!(catalog_root("", "us-east-1/other").is_err());
    }

    #[test]
    fn sigv4_get_uses_get_canonical_method_without_changing_post_signatures() {
        let url = Url::parse("https://bedrock.us-east-1.amazonaws.com/foundation-models").unwrap();
        let now = Utc::now();
        let get = build_sigv4_headers_for_method("GET", &url, b"", "TESTKEY", "TESTSECRET", None, "us-east-1", now).unwrap();
        let post = super::super::build_sigv4_headers(&url, b"", "TESTKEY", "TESTSECRET", None, "us-east-1", now).unwrap();
        assert_ne!(get.authorization, post.authorization);
        assert!(get.authorization.contains("us-east-1/bedrock/aws4_request"));
    }
}
