//! Contract tests for Vertex Model Garden catalog candidates.
//!
//! These IDs are *not* evidence that the project has access to invoke a model.

use super::catalog::{
    catalog_endpoint, catalog_scope_with_env, parse_publisher_page,
};

#[test]
fn vertex_scope_requires_a_project_and_canonical_location() {
    let scope = catalog_scope_with_env(|name| match name {
        "GOOGLE_CLOUD_PROJECT" => Some("candidate-project-1".to_string()),
        "VERTEX_LOCATION" => Some("us-central1".to_string()),
        _ => None,
    }).unwrap();
    assert_eq!(scope.project, "candidate-project-1");
    assert_eq!(scope.location, "us-central1");

    assert!(catalog_scope_with_env(|_| None).is_err());
    assert!(catalog_scope_with_env(|name| (name == "GOOGLE_CLOUD_PROJECT")
        .then(|| "project\nInjected: secret".to_string())).is_err());
    assert!(catalog_scope_with_env(|name| match name {
        "GOOGLE_CLOUD_PROJECT" => Some("valid-project".to_string()),
        "GOOGLE_CLOUD_LOCATION" => Some("us-central1.attacker.example".to_string()),
        _ => None,
    }).is_err());
}

#[test]
fn vertex_catalog_endpoint_never_follows_custom_inference_routes() {
    let normal = catalog_endpoint("", "us-central1", "google").unwrap();
    assert_eq!(normal.as_str(), "https://us-central1-aiplatform.googleapis.com/v1beta1/publishers/google/models");

    let global = catalog_endpoint("", "global", "anthropic").unwrap();
    assert_eq!(global.as_str(), "https://aiplatform.googleapis.com/v1beta1/publishers/anthropic/models");

    assert!(catalog_endpoint("https://proxy.example/v1", "us-central1", "google").is_err());
    assert!(catalog_endpoint("", "us-central1", "unknown").is_err());
    assert!(catalog_endpoint("", "bad/location", "google").is_err());
}

#[test]
fn vertex_google_catalog_filters_non_chat_and_foreign_publisher_ids() {
    let body = br#"{"publisherModels":[
      {"name":"publishers/google/models/gemini-2.5-pro"},
      {"name":"publishers/google/models/gemini-embedding-001"},
      {"name":"publishers/google/models/imagen-4"},
      {"name":"publishers/anthropic/models/claude-opus-4"},
      {"name":"publishers/google/models/gemini-2.5-flash@001"}
    ],"nextPageToken":"more"}"#;
    let (models, next) = parse_publisher_page("google", body).unwrap();
    assert_eq!(models, vec!["gemini-2.5-pro", "gemini-2.5-flash@001"]);
    assert_eq!(next.as_deref(), Some("more"));
}

#[test]
fn vertex_anthropic_catalog_only_exposes_claude_model_ids() {
    let body = br#"{"publisherModels":[
      {"name":"publishers/anthropic/models/claude-opus-4-1"},
      {"name":"publishers/anthropic/models/claude-sonnet-4-5"},
      {"name":"publishers/google/models/gemini-2.5-pro"}
    ]}"#;
    let (models, next) = parse_publisher_page("anthropic", body).unwrap();
    assert_eq!(models, vec!["claude-opus-4-1", "claude-sonnet-4-5"]);
    assert!(next.is_none());
}

#[test]
fn vertex_catalog_rejects_malformed_responses_and_cursor_overflows() {
    assert!(parse_publisher_page("google", br#"{"models":[]}"#).is_err());
    assert!(parse_publisher_page("google", br#"{"publisherModels":"not-an-array"}"#).is_err());
    assert!(parse_publisher_page("google", br#"{"publisherModels":[{"name":42}]}"#).is_err());
    let token = "a".repeat(3000);
    let oversized = format!(r#"{{"publisherModels":[],"nextPageToken":"{token}"}}"#);
    assert!(parse_publisher_page("google", oversized.as_bytes()).is_err());
    assert!(parse_publisher_page("google", br#"{"publisherModels":[],"nextPageToken":9}"#).is_err());
}
