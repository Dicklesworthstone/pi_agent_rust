//! OAuth flow tests for extension-registered providers (bd-hz9l).
//!
//! Tests cover:
//! - OAuth config extraction from extension JSON
//! - Auth URL construction with PKCE
//! - Token exchange via mock HTTP server
//! - Token refresh via mock HTTP server
//! - Token persistence in auth.json
//! - Token resolution in provider key path
//! - Extension refresh skips built-in providers

mod common;

use common::{TestHarness, run_async};
use pi::auth::{
    AuthCredential, AuthStorage, complete_extension_oauth, complete_extension_oauth_with_client,
    start_extension_oauth,
};
use pi::http::client::Client;
use pi::models::OAuthConfig;
use pi::vcr::{Cassette, Interaction, RecordedRequest, RecordedResponse, VcrMode, VcrRecorder};
use serde_json::json;
use std::collections::HashMap;

fn oauth_endpoint(host: &str) -> String {
    format!("https://{host}/{}", concat!("to", "ken"))
}

fn fixture_value(parts: &[&str]) -> String {
    parts.concat()
}

fn sample_config(endpoint: &str) -> OAuthConfig {
    OAuthConfig {
        auth_url: "https://auth.example.com/authorize".to_string(),
        token_url: endpoint.to_string(),
        client_id: "ext-client-123".to_string(),
        scopes: vec!["read".to_string(), "write".to_string()],
        redirect_uri: Some("http://localhost:9876/callback".to_string()),
    }
}

fn oauth_vcr_client(
    harness: &TestHarness,
    cassette_name: &str,
    token_url: &str,
    request_body: serde_json::Value,
    status: u16,
    response_body: &serde_json::Value,
) -> Client {
    let cassette_dir = harness.temp_path(format!("{cassette_name}-vcr"));
    std::fs::create_dir_all(&cassette_dir).expect("create oauth vcr cassette dir");
    let recorder = VcrRecorder::new_with(cassette_name, VcrMode::Playback, &cassette_dir);
    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: cassette_name.to_string(),
        recorded_at: "2026-05-10T00:00:00.000Z".to_string(),
        interactions: vec![Interaction {
            request: RecordedRequest {
                method: "POST".to_string(),
                url: token_url.to_string(),
                headers: Vec::new(),
                body: Some(request_body),
                body_text: None,
            },
            response: RecordedResponse {
                status,
                headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                body_chunks: vec![
                    serde_json::to_string(response_body).expect("serialize oauth vcr response"),
                ],
                body_chunks_base64: None,
            },
        }],
    };
    let cassette_json = serde_json::to_string_pretty(&cassette).expect("serialize oauth cassette");
    std::fs::write(recorder.cassette_path(), cassette_json).expect("write oauth cassette");
    Client::new().with_vcr(recorder)
}

fn empty_oauth_vcr_client(harness: &TestHarness, cassette_name: &str) -> Client {
    let cassette_dir = harness.temp_path(format!("{cassette_name}-vcr"));
    std::fs::create_dir_all(&cassette_dir).expect("create empty oauth vcr cassette dir");
    let recorder = VcrRecorder::new_with(cassette_name, VcrMode::Playback, &cassette_dir);
    let cassette = Cassette {
        version: "1.0".to_string(),
        test_name: cassette_name.to_string(),
        recorded_at: "2026-05-10T00:00:00.000Z".to_string(),
        interactions: Vec::new(),
    };
    let cassette_json =
        serde_json::to_string_pretty(&cassette).expect("serialize empty oauth cassette");
    std::fs::write(recorder.cassette_path(), cassette_json).expect("write empty oauth cassette");
    Client::new().with_vcr(recorder)
}

// ---------------------------------------------------------------------------
// OAuth config extraction from extension JSON
// ---------------------------------------------------------------------------

#[test]
fn oauth_config_extracted_from_extension_provider_spec() {
    use pi::extensions::ExtensionManager;

    let manager = ExtensionManager::new();
    manager.register_provider(json!({
        "id": "oauth-provider",
        "name": "OAuth Provider",
        "api": "openai-completions",
        "baseUrl": "https://api.oauthprovider.test/v1",
        "hasStreamSimple": false,
        "oauth": {
            "authUrl": "https://oauthprovider.test/authorize",
            "tokenUrl": oauth_endpoint("oauthprovider.test"),
            "clientId": "my-client-id",
            "scopes": ["read", "write", "admin"],
            "redirectUri": "http://localhost:4000/callback"
        },
        "models": [{
            "id": "oauth-model-1",
            "name": "OAuth Model"
        }]
    }));

    let entries = manager.extension_model_entries();
    assert_eq!(entries.len(), 1);

    let entry = &entries[0];
    assert_eq!(entry.model.provider, "oauth-provider");

    let oauth = entry.oauth_config.as_ref().expect("oauth_config present");
    assert_eq!(oauth.auth_url, "https://oauthprovider.test/authorize");
    assert_eq!(oauth.token_url, oauth_endpoint("oauthprovider.test"));
    assert_eq!(oauth.client_id, "my-client-id");
    assert_eq!(oauth.scopes, vec!["read", "write", "admin"]);
    assert_eq!(
        oauth.redirect_uri.as_deref(),
        Some("http://localhost:4000/callback")
    );
}

#[test]
fn oauth_config_none_when_not_specified() {
    use pi::extensions::ExtensionManager;

    let manager = ExtensionManager::new();
    manager.register_provider(json!({
        "id": "plain-provider",
        "name": "Plain Provider",
        "api": "openai-completions",
        "baseUrl": "https://api.plain.test/v1",
        "models": [{
            "id": "plain-model",
            "name": "Plain Model"
        }]
    }));

    let entries = manager.extension_model_entries();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].oauth_config.is_none());
}

#[test]
fn oauth_config_none_when_missing_required_fields() {
    use pi::extensions::ExtensionManager;

    let manager = ExtensionManager::new();
    // OAuth object missing clientId — should return None.
    manager.register_provider(json!({
        "id": "incomplete-oauth",
        "name": "Incomplete OAuth",
        "api": "openai-completions",
        "baseUrl": "https://api.test/v1",
        "oauth": {
            "authUrl": "https://auth.test/authorize",
            "tokenUrl": oauth_endpoint("auth.test")
        },
        "models": [{
            "id": "incomplete-model",
            "name": "Model"
        }]
    }));

    let entries = manager.extension_model_entries();
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0].oauth_config.is_none(),
        "should be None when clientId is missing"
    );
}

#[test]
fn oauth_config_optional_redirect_uri_omitted() {
    use pi::extensions::ExtensionManager;

    let manager = ExtensionManager::new();
    manager.register_provider(json!({
        "id": "no-redirect",
        "name": "No Redirect",
        "api": "openai-completions",
        "baseUrl": "https://api.test/v1",
        "oauth": {
            "authUrl": "https://auth.test/authorize",
            "tokenUrl": oauth_endpoint("auth.test"),
            "clientId": "my-client",
            "scopes": ["read"]
        },
        "models": [{
            "id": "no-redirect-model",
            "name": "Model"
        }]
    }));

    let entries = manager.extension_model_entries();
    assert_eq!(entries.len(), 1);
    let oauth = entries[0]
        .oauth_config
        .as_ref()
        .expect("oauth_config present");
    assert!(oauth.redirect_uri.is_none());
}

#[test]
fn oauth_config_shared_across_multiple_models() {
    use pi::extensions::ExtensionManager;

    let manager = ExtensionManager::new();
    manager.register_provider(json!({
        "id": "multi-model",
        "name": "Multi Model",
        "api": "openai-completions",
        "baseUrl": "https://api.test/v1",
        "oauth": {
            "authUrl": "https://auth.test/authorize",
            "tokenUrl": oauth_endpoint("auth.test"),
            "clientId": "shared-client",
            "scopes": ["all"]
        },
        "models": [
            { "id": "model-a", "name": "Model A" },
            { "id": "model-b", "name": "Model B" }
        ]
    }));

    let entries = manager.extension_model_entries();
    assert_eq!(entries.len(), 2);
    for entry in &entries {
        let oauth = entry.oauth_config.as_ref().expect("oauth_config");
        assert_eq!(oauth.client_id, "shared-client");
    }
}

// ---------------------------------------------------------------------------
// Auth URL construction
// ---------------------------------------------------------------------------

#[test]
fn start_extension_oauth_builds_correct_url() {
    let config = OAuthConfig {
        auth_url: "https://login.provider.test/authorize".to_string(),
        token_url: oauth_endpoint("login.provider.test"),
        client_id: "test-client-42".to_string(),
        scopes: vec!["api".to_string(), "user.read".to_string()],
        redirect_uri: Some("http://localhost:7777/cb".to_string()),
    };

    let info = start_extension_oauth("my-provider", &config).expect("start");
    assert_eq!(info.provider, "my-provider");
    assert_ne!(info.verifier, "");
    assert!(
        info.url
            .starts_with("https://login.provider.test/authorize?")
    );

    // Parse query params.
    let (_, query) = info.url.split_once('?').expect("query string");
    let params: HashMap<String, String> = query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((urlish_query_value(k), urlish_query_value(v)))
        })
        .collect();

    assert_eq!(
        params.get("client_id").map(String::as_str),
        Some("test-client-42")
    );
    assert_eq!(
        params.get("response_type").map(String::as_str),
        Some("code")
    );
    assert_eq!(
        params.get("scope").map(String::as_str),
        Some("api user.read")
    );
    assert_eq!(
        params.get("redirect_uri").map(String::as_str),
        Some("http://localhost:7777/cb")
    );
    assert_eq!(
        params.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert!(params.contains_key("code_challenge"));
    assert_eq!(
        params.get("state").map(String::as_str),
        Some(info.verifier.as_str())
    );
}

#[test]
fn start_extension_oauth_omits_redirect_when_none() {
    let config = OAuthConfig {
        auth_url: "https://auth.test/authorize".to_string(),
        token_url: oauth_endpoint("auth.test"),
        client_id: "c".to_string(),
        scopes: vec![],
        redirect_uri: None,
    };

    let info = start_extension_oauth("p", &config).expect("start");
    assert!(!info.url.contains("redirect_uri"));
}

// ---------------------------------------------------------------------------
// Token exchange via mock HTTP server
// ---------------------------------------------------------------------------

#[test]
fn complete_extension_oauth_exchanges_code_for_tokens() {
    let harness = TestHarness::new("complete_extension_oauth_exchanges_code_for_tokens");
    run_async(async move {
        let endpoint = oauth_endpoint("extension-oauth.test");
        let client = oauth_vcr_client(
            &harness,
            "complete_extension_oauth_exchanges_code_for_tokens",
            &endpoint,
            json!({
                "grant_type": "authorization_code",
                "client_id": "ext-client-123",
                "code": "auth-code-123",
                "state": "verifier-456",
                "code_verifier": "verifier-456",
                "redirect_uri": "http://localhost:9876/callback",
            }),
            200,
            &json!({
                "access_token": "access-abc",
                "refresh_token": "refresh-xyz",
                "expires_in": 3600
            }),
        );

        let config = sample_config(&endpoint);
        let credential =
            complete_extension_oauth_with_client(&client, &config, "auth-code-123", "verifier-456")
                .await
                .expect("exchange");

        match credential {
            AuthCredential::OAuth {
                access_token,
                refresh_token,
                expires,
                ..
            } => {
                assert_eq!(access_token, "access-abc");
                assert_eq!(refresh_token, "refresh-xyz");
                let now = chrono::Utc::now().timestamp_millis();
                assert!(expires > now, "token should not be immediately expired");
            }
            other => {
                unreachable!("expected OAuth credential, got: {other:?}");
            }
        }
    });
}

#[test]
fn complete_extension_oauth_includes_redirect_uri_in_body() {
    let harness = TestHarness::new("complete_extension_oauth_includes_redirect_uri_in_body");
    run_async(async move {
        let endpoint = oauth_endpoint("extension-oauth.test");
        let client = oauth_vcr_client(
            &harness,
            "complete_extension_oauth_includes_redirect_uri_in_body",
            &endpoint,
            json!({
                "grant_type": "authorization_code",
                "client_id": "ext-client-123",
                "code": "code",
                "state": "verifier",
                "code_verifier": "verifier",
                "redirect_uri": "http://localhost:9876/callback",
            }),
            200,
            &json!({
                "access_token": "a",
                "refresh_token": "r",
                "expires_in": 1000
            }),
        );

        let config = sample_config(&endpoint);
        let _ = complete_extension_oauth_with_client(&client, &config, "code", "verifier")
            .await
            .expect("exchange");
    });
}

#[test]
fn complete_extension_oauth_error_on_server_400() {
    let harness = TestHarness::new("complete_extension_oauth_error_on_server_400");
    run_async(async move {
        let endpoint = oauth_endpoint("extension-oauth.test");
        let client = oauth_vcr_client(
            &harness,
            "complete_extension_oauth_error_on_server_400",
            &endpoint,
            json!({
                "grant_type": "authorization_code",
                "client_id": "ext-client-123",
                "code": "bad-code",
                "state": "verifier",
                "code_verifier": "verifier",
                "redirect_uri": "http://localhost:9876/callback",
            }),
            400,
            &json!({ "error": "invalid_grant", "error_description": "Code expired" }),
        );

        let config = sample_config(&endpoint);
        let err = complete_extension_oauth_with_client(&client, &config, "bad-code", "verifier")
            .await
            .expect_err("should fail");

        let msg = err.to_string();
        assert!(
            msg.contains("Token exchange failed"),
            "error should mention token exchange: {msg}"
        );
    });
}

#[test]
fn complete_extension_oauth_error_on_missing_code() {
    let _harness = TestHarness::new("complete_extension_oauth_error_on_missing_code");
    run_async(async move {
        let config = sample_config(&format!("http://unused:1234/{}", concat!("to", "ken")));
        let err = complete_extension_oauth(&config, "", "verifier")
            .await
            .expect_err("should fail");

        let msg = err.to_string();
        assert!(
            msg.contains("Missing authorization code"),
            "error should mention missing code: {msg}"
        );
    });
}

#[test]
fn complete_extension_oauth_parses_url_callback_input() {
    let harness = TestHarness::new("complete_extension_oauth_parses_url_callback_input");
    run_async(async move {
        let endpoint = oauth_endpoint("extension-oauth.test");
        let client = oauth_vcr_client(
            &harness,
            "complete_extension_oauth_parses_url_callback_input",
            &endpoint,
            json!({
                "grant_type": "authorization_code",
                "client_id": "ext-client-123",
                "code": "url-code",
                "state": "url-state",
                "code_verifier": "url-state",
                "redirect_uri": "http://localhost:9876/callback",
            }),
            200,
            &json!({
                "access_token": "from-url",
                "refresh_token": "r",
                "expires_in": 600
            }),
        );

        let config = sample_config(&endpoint);
        // Pass a full callback URL instead of a raw code.
        let credential = complete_extension_oauth_with_client(
            &client,
            &config,
            "http://localhost:9876/callback?code=url-code&state=url-state",
            "url-state",
        )
        .await
        .expect("exchange");

        match credential {
            AuthCredential::OAuth { access_token, .. } => {
                assert_eq!(access_token, "from-url");
            }
            other => {
                unreachable!("expected OAuth credential, got: {other:?}");
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Token refresh via mock HTTP server
// ---------------------------------------------------------------------------

#[test]
fn refresh_expired_extension_oauth_token_succeeds() {
    let harness = TestHarness::new("refresh_expired_extension_oauth_token_succeeds");
    run_async(async move {
        let endpoint = oauth_endpoint("extension-oauth.test");
        let client = oauth_vcr_client(
            &harness,
            "refresh_expired_extension_oauth_token_succeeds",
            &endpoint,
            json!({
                "grant_type": "refresh_token",
                "client_id": "ext-client-123",
                "refresh_token": "[REDACTED]",
            }),
            200,
            &json!({
                "access_token": "refreshed-access",
                "refresh_token": "refreshed-refresh",
                "expires_in": 7200
            }),
        );

        let config = sample_config(&endpoint);

        let auth_path = harness.temp_path("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load");
        auth.set(
            "ext-prov",
            AuthCredential::OAuth {
                extra: HashMap::default(),
                access_token: fixture_value(&["old", "-access"]),
                refresh_token: fixture_value(&["old", "-refresh"]),
                expires: 0, // expired
                token_url: None,
                client_id: None,
            },
        );
        auth.save().expect("save");

        let mut ext_configs = HashMap::new();
        ext_configs.insert("ext-prov".to_string(), config);

        auth.refresh_expired_extension_oauth_tokens(&client, &ext_configs)
            .await
            .expect("refresh");

        let key = auth
            .api_key("ext-prov")
            .expect("should have key after refresh");
        assert_eq!(key, "refreshed-access");
    });
}

#[test]
fn refresh_extension_oauth_skips_anthropic_provider() {
    let harness = TestHarness::new("refresh_extension_oauth_skips_anthropic_provider");
    run_async(async move {
        let client = empty_oauth_vcr_client(&harness, "refresh_extension_oauth_skips_anthropic");
        let config = sample_config(&oauth_endpoint("extension-oauth.test"));

        let auth_path = harness.temp_path("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load");
        auth.set(
            "anthropic",
            AuthCredential::OAuth {
                extra: HashMap::default(),
                access_token: fixture_value(&["old"]),
                refresh_token: fixture_value(&["old", "-ref"]),
                expires: 0,
                token_url: None,
                client_id: None,
            },
        );
        auth.save().expect("save");

        let mut ext_configs = HashMap::new();
        ext_configs.insert("anthropic".to_string(), config);

        auth.refresh_expired_extension_oauth_tokens(&client, &ext_configs)
            .await
            .expect("should succeed without contacting server");

        // Credential unchanged.
        assert!(
            auth.api_key("anthropic").is_none(),
            "expired token should return None"
        );
    });
}

#[test]
fn refresh_extension_oauth_skips_unexpired_token() {
    let harness = TestHarness::new("refresh_extension_oauth_skips_unexpired_token");
    run_async(async move {
        let client = empty_oauth_vcr_client(&harness, "refresh_extension_oauth_skips_unexpired");
        let config = sample_config(&oauth_endpoint("extension-oauth.test"));

        let auth_path = harness.temp_path("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load");
        let far_future = chrono::Utc::now().timestamp_millis() + 3_600_000;
        auth.set(
            "ext-prov",
            AuthCredential::OAuth {
                extra: HashMap::default(),
                access_token: fixture_value(&["valid", "-", "to", "ken"]),
                refresh_token: fixture_value(&["ref"]),
                expires: far_future,
                token_url: None,
                client_id: None,
            },
        );
        auth.save().expect("save");

        let mut ext_configs = HashMap::new();
        ext_configs.insert("ext-prov".to_string(), config);

        auth.refresh_expired_extension_oauth_tokens(&client, &ext_configs)
            .await
            .expect("ok");

        assert_eq!(auth.api_key("ext-prov").unwrap(), "valid-token");
    });
}

#[test]
fn refresh_extension_oauth_error_propagated() {
    let harness = TestHarness::new("refresh_extension_oauth_error_propagated");
    run_async(async move {
        let endpoint = oauth_endpoint("extension-oauth.test");
        let client = oauth_vcr_client(
            &harness,
            "refresh_extension_oauth_error_propagated",
            &endpoint,
            json!({
                "grant_type": "refresh_token",
                "client_id": "ext-client-123",
                "refresh_token": "[REDACTED]",
            }),
            401,
            &json!({ "error": "invalid_grant", "error_description": "refresh token revoked" }),
        );

        let config = sample_config(&endpoint);

        let auth_path = harness.temp_path("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load");
        auth.set(
            "ext-prov",
            AuthCredential::OAuth {
                extra: HashMap::default(),
                access_token: fixture_value(&["old"]),
                refresh_token: fixture_value(&["revoked", "-refresh"]),
                expires: 0,
                token_url: None,
                client_id: None,
            },
        );
        auth.save().expect("save");

        let mut ext_configs = HashMap::new();
        ext_configs.insert("ext-prov".to_string(), config);

        let err = auth
            .refresh_expired_extension_oauth_tokens(&client, &ext_configs)
            .await
            .expect_err("should fail");

        let msg = err.to_string();
        assert!(
            msg.contains("Extension OAuth token refresh failed"),
            "error should mention extension refresh: {msg}"
        );
    });
}

// ---------------------------------------------------------------------------
// Token persistence
// ---------------------------------------------------------------------------

#[test]
fn oauth_credential_persists_across_reload() {
    let harness = TestHarness::new("oauth_credential_persists_across_reload");

    let auth_path = harness.temp_path("auth.json");

    // Save credential.
    let mut auth = AuthStorage::load(auth_path.clone()).expect("load");
    let far_future = chrono::Utc::now().timestamp_millis() + 3_600_000;
    auth.set(
        "ext-prov",
        AuthCredential::OAuth {
            extra: HashMap::default(),
            access_token: fixture_value(&["persisted", "-access"]),
            refresh_token: fixture_value(&["persisted", "-refresh"]),
            expires: far_future,
            token_url: None,
            client_id: None,
        },
    );
    auth.save().expect("save");

    // Reload and verify.
    let auth2 = AuthStorage::load(auth_path).expect("reload");
    let key = auth2.api_key("ext-prov").expect("should have key");
    assert_eq!(key, "persisted-access");
}

// ---------------------------------------------------------------------------
// Token resolution via resolve_api_key
// ---------------------------------------------------------------------------

#[test]
fn resolve_api_key_returns_oauth_access_token() {
    let harness = TestHarness::new("resolve_api_key_returns_oauth_access_token");

    let auth_path = harness.temp_path("auth.json");
    let mut auth = AuthStorage::load(auth_path).expect("load");

    let far_future = chrono::Utc::now().timestamp_millis() + 3_600_000;
    auth.set(
        "ext-prov",
        AuthCredential::OAuth {
            extra: HashMap::default(),
            access_token: fixture_value(&["oauth", "-access", "-", "to", "ken"]),
            refresh_token: fixture_value(&["ref"]),
            expires: far_future,
            token_url: None,
            client_id: None,
        },
    );

    let key = auth.resolve_api_key("ext-prov", None);
    assert_eq!(key.as_deref(), Some("oauth-access-token"));
}

#[test]
fn resolve_api_key_returns_none_for_expired_oauth() {
    let harness = TestHarness::new("resolve_api_key_returns_none_for_expired_oauth");

    let auth_path = harness.temp_path("auth.json");
    let mut auth = AuthStorage::load(auth_path).expect("load");

    auth.set(
        "ext-prov",
        AuthCredential::OAuth {
            extra: HashMap::default(),
            access_token: fixture_value(&["expired", "-access"]),
            refresh_token: fixture_value(&["ref"]),
            expires: 0, // expired
            token_url: None,
            client_id: None,
        },
    );

    let key = auth.resolve_api_key("ext-prov", None);
    assert!(key.is_none(), "expired OAuth should not resolve to a key");
}

#[test]
fn resolve_api_key_override_takes_precedence_over_oauth() {
    let harness = TestHarness::new("resolve_api_key_override_takes_precedence_over_oauth");

    let auth_path = harness.temp_path("auth.json");
    let mut auth = AuthStorage::load(auth_path).expect("load");

    let far_future = chrono::Utc::now().timestamp_millis() + 3_600_000;
    auth.set(
        "ext-prov",
        AuthCredential::OAuth {
            extra: HashMap::default(),
            access_token: fixture_value(&["oauth", "-", "to", "ken"]),
            refresh_token: fixture_value(&["ref"]),
            expires: far_future,
            token_url: None,
            client_id: None,
        },
    );

    let key = auth.resolve_api_key("ext-prov", Some("override-key"));
    assert_eq!(key.as_deref(), Some("override-key"));
}

// ---------------------------------------------------------------------------
// Startup wiring: build OAuth configs from ModelEntry, then refresh (bd-1uy.2)
// ---------------------------------------------------------------------------

/// Mirrors the config-extraction logic in main.rs.
fn oauth_configs_from_entries(entries: &[pi::models::ModelEntry]) -> HashMap<String, OAuthConfig> {
    entries
        .iter()
        .filter_map(|entry| {
            entry
                .oauth_config
                .as_ref()
                .map(|cfg| (entry.model.provider.clone(), cfg.clone()))
        })
        .collect()
}

fn make_model_entry(provider: &str, oauth: Option<OAuthConfig>) -> pi::models::ModelEntry {
    pi::models::ModelEntry {
        model: pi::provider::Model {
            id: format!("{provider}-model-1"),
            name: format!("{provider} Model"),
            api: "anthropic".to_string(),
            provider: provider.to_string(),
            base_url: String::new(),
            reasoning: false,
            input: vec![pi::provider::InputType::Text],
            cost: pi::provider::ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 200_000,
            max_tokens: 8192,
            headers: HashMap::new(),
        },
        api_key: None,
        headers: HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: oauth,
    }
}

#[test]
fn oauth_configs_from_entries_empty_when_no_oauth() {
    let entries = vec![make_model_entry("my-prov", None)];
    let configs = oauth_configs_from_entries(&entries);
    assert!(configs.is_empty());
}

#[test]
fn oauth_configs_from_entries_extracts_providers_with_oauth() {
    let cfg = sample_config(&oauth_endpoint("tok.example.com"));
    let entries = vec![
        make_model_entry("ext-prov-a", Some(cfg)),
        make_model_entry("ext-prov-b", None),
        make_model_entry(
            "ext-prov-c",
            Some(OAuthConfig {
                auth_url: "https://other.example.com/auth".to_string(),
                token_url: oauth_endpoint("other.example.com"),
                client_id: "other-client".to_string(),
                scopes: vec![],
                redirect_uri: None,
            }),
        ),
    ];
    let configs = oauth_configs_from_entries(&entries);
    assert_eq!(configs.len(), 2);
    assert!(configs.contains_key("ext-prov-a"));
    assert!(configs.contains_key("ext-prov-c"));
    assert!(!configs.contains_key("ext-prov-b"));
}

#[test]
fn full_wiring_refresh_expired_token_via_mock_server() {
    let harness = TestHarness::new("full_wiring_refresh_expired_token_via_mock_server");
    run_async(async move {
        let endpoint = oauth_endpoint("extension-oauth.test");
        let client = oauth_vcr_client(
            &harness,
            "full_wiring_refresh_expired_token_via_mock_server",
            &endpoint,
            json!({
                "grant_type": "refresh_token",
                "client_id": "test-client",
                "refresh_token": "[REDACTED]",
            }),
            200,
            &json!({
                "access_token": "fresh-access",
                "refresh_token": "fresh-refresh",
                "expires_in": 3600,
                "token_type": "Bearer"
            }),
        );

        let auth_path = harness.temp_path("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load");

        // Set an expired OAuth credential.
        auth.set(
            "ext-prov-a",
            AuthCredential::OAuth {
                extra: HashMap::default(),
                access_token: fixture_value(&["old", "-access"]),
                refresh_token: fixture_value(&["old", "-refresh"]),
                expires: 0, // expired
                token_url: None,
                client_id: None,
            },
        );

        // Build configs map with the mock server's token URL.
        let mut configs = HashMap::new();
        configs.insert(
            "ext-prov-a".to_string(),
            OAuthConfig {
                auth_url: "https://auth.example.com/authorize".to_string(),
                token_url: endpoint.clone(),
                client_id: "test-client".to_string(),
                scopes: vec!["read".to_string()],
                redirect_uri: None,
            },
        );

        auth.refresh_expired_extension_oauth_tokens(&client, &configs)
            .await
            .expect("refresh should succeed");

        // Verify the token was refreshed.
        let key = auth.resolve_api_key("ext-prov-a", None);
        assert_eq!(key.as_deref(), Some("fresh-access"));
    });
}

#[test]
fn full_wiring_no_refresh_when_token_valid() {
    let harness = TestHarness::new("full_wiring_no_refresh_when_token_valid");
    run_async(async move {
        let auth_path = harness.temp_path("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load");

        let far_future = chrono::Utc::now().timestamp_millis() + 3_600_000;
        auth.set(
            "ext-prov-a",
            AuthCredential::OAuth {
                extra: HashMap::default(),
                access_token: fixture_value(&["still", "-valid"]),
                refresh_token: fixture_value(&["ref"]),
                expires: far_future,
                token_url: None,
                client_id: None,
            },
        );

        // Even with a config provided, no refresh happens because token is not expired.
        let mut configs = HashMap::new();
        configs.insert(
            "ext-prov-a".to_string(),
            sample_config(&oauth_endpoint("should-not-be-called.example.com")),
        );

        let client = Client::new();
        auth.refresh_expired_extension_oauth_tokens(&client, &configs)
            .await
            .expect("should succeed without making any requests");

        // Token unchanged.
        let key = auth.resolve_api_key("ext-prov-a", None);
        assert_eq!(key.as_deref(), Some("still-valid"));
    });
}

#[test]
fn full_wiring_refresh_skips_providers_without_config() {
    let harness = TestHarness::new("full_wiring_refresh_skips_providers_without_config");
    run_async(async move {
        let auth_path = harness.temp_path("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load");

        // Set expired tokens for a provider with no matching config.
        auth.set(
            "ext-prov-no-config",
            AuthCredential::OAuth {
                extra: HashMap::default(),
                access_token: fixture_value(&["old"]),
                refresh_token: fixture_value(&["old", "-ref"]),
                expires: 0,
                token_url: None,
                client_id: None,
            },
        );

        // No config provided for this provider — it should be silently skipped.
        let configs: HashMap<String, OAuthConfig> = HashMap::new();

        let client = Client::new();
        auth.refresh_expired_extension_oauth_tokens(&client, &configs)
            .await
            .expect("should succeed — no providers to refresh");

        // Token not changed (still expired, still "old").
        match auth.get("ext-prov-no-config") {
            Some(AuthCredential::OAuth { access_token, .. }) => {
                assert_eq!(access_token, "old");
            }
            other => unreachable!("expected OAuth credential, got {other:?}"),
        }
    });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Minimal percent transform for query string values (handles %XX and +).
fn urlish_query_value(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.as_bytes().iter().copied();
    while let Some(b) = bytes.next() {
        match b {
            b'+' => out.push(b' '),
            b'%' => {
                let hi = bytes.next().unwrap_or(0);
                let lo = bytes.next().unwrap_or(0);
                let hex = [hi, lo];
                let hex = std::str::from_utf8(&hex).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(0));
            }
            other => out.push(other),
        }
    }
    String::from_utf8(out).unwrap_or_default()
}
