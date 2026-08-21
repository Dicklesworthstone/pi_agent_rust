//! Unit tests for model registry: loading, filtering, and scoped selection.
//!
//! Tests cover:
//! - Built-in model loading from `AuthStorage`
//! - Custom models.json parsing and application
//! - Model filtering by availability (API key presence)
//! - Model lookup by provider/id
//! - Error handling for malformed JSON
//! - Environment variable resolution
//! - Header merging and resolution

mod common;

use common::harness::TestHarness;
use pi::auth::AuthStorage;
use pi::models::{ModelRegistry, default_models_path, model_requires_configured_credential};

// ============================================================================
// Built-in Models Tests
// ============================================================================

#[test]
fn test_built_in_models_without_api_keys() {
    let harness = TestHarness::new("test_built_in_models_without_api_keys");
    harness.section("Setup");

    // Create empty auth storage (no API keys)
    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");

    harness.section("Load registry");
    let registry =
        ModelRegistry::load_with_credential_resolver(None, |provider| auth.api_key(provider));

    harness.section("Verify");
    harness
        .log()
        .info_ctx("verify", "Checking built-in models", |ctx| {
            ctx.push(("model_count".into(), registry.models().len().to_string()));
        });

    // Should have built-in models from all three providers
    let models = registry.models();
    assert!(!models.is_empty(), "Should have built-in models");

    assert!(
        models.iter().any(|m| m.model.provider == "anthropic"),
        "Should have Anthropic built-in models"
    );

    assert!(
        models.iter().any(|m| m.model.provider == "openai"),
        "Should have OpenAI built-in models"
    );

    assert!(
        models.iter().any(|m| m.model.provider == "google"),
        "Should have Google built-in models"
    );

    // Without API keys, only models from providers that resolve credentials at
    // request time (or need none at all — e.g. ollama/llamacpp/mistralrs,
    // amazon-bedrock, sap-ai-core) should be available.
    let available = registry.get_available();
    assert!(
        available
            .iter()
            .all(|m| !model_requires_configured_credential(m)),
        "Only keyless/request-time-credential models should be available without API keys"
    );
    for provider in ["anthropic", "openai", "google"] {
        assert!(
            !available.iter().any(|m| m.model.provider == provider),
            "No {provider} models should be available without API keys"
        );
    }

    // No errors should be reported
    assert!(registry.error().is_none(), "No error expected");
}

#[test]
fn test_built_in_models_with_anthropic_key() {
    let harness = TestHarness::new("test_built_in_models_with_anthropic_key");
    harness.section("Setup");

    // Create auth with Anthropic API key
    let auth_content = r#"{"anthropic": {"type": "api_key", "key": "sk-ant-test-key"}}"#;
    let auth_path = harness.create_file("auth.json", auth_content);
    let auth = AuthStorage::load(auth_path).expect("load auth");

    harness.section("Load registry");
    let registry =
        ModelRegistry::load_with_credential_resolver(None, |provider| auth.api_key(provider));

    harness.section("Verify");
    // Anthropic models should have API keys
    let anthropic_models: Vec<_> = registry
        .models()
        .iter()
        .filter(|m| m.model.provider == "anthropic")
        .collect();
    assert!(
        anthropic_models.iter().all(|m| m.api_key.is_some()),
        "All Anthropic models should have API key"
    );

    // OpenAI/Google models should not have API keys
    let openai_models: Vec<_> = registry
        .models()
        .iter()
        .filter(|m| m.model.provider == "openai")
        .collect();
    assert!(
        openai_models.iter().all(|m| m.api_key.is_none()),
        "OpenAI models should not have API key"
    );

    // Available models should be exactly: all Anthropic models plus the
    // always-available keyless/request-time-credential providers.
    let available = registry.get_available();
    let available_anthropic = available
        .iter()
        .filter(|m| m.model.provider == "anthropic")
        .count();
    assert_eq!(
        available_anthropic,
        anthropic_models.len(),
        "All Anthropic models should be available"
    );
    assert!(
        available
            .iter()
            .all(|m| m.model.provider == "anthropic" || !model_requires_configured_credential(m)),
        "Only Anthropic and keyless models should be available"
    );
}

#[test]
fn test_model_fields_populated_correctly() {
    let harness = TestHarness::new("test_model_fields_populated_correctly");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");
    let registry = ModelRegistry::load(&auth, None);

    harness.section("Verify Claude models");
    let claude_sonnet = registry.find("anthropic", "claude-sonnet-4-5");
    assert!(claude_sonnet.is_some(), "Claude Sonnet 4.5 should exist");
    let model = claude_sonnet.unwrap();

    harness
        .log()
        .info_ctx("verify", "Claude Sonnet 4.5 fields", |ctx| {
            ctx.push(("id".into(), model.model.id.clone()));
            ctx.push(("name".into(), model.model.name.clone()));
            ctx.push(("reasoning".into(), model.model.reasoning.to_string()));
            ctx.push((
                "context_window".into(),
                model.model.context_window.to_string(),
            ));
        });

    assert_eq!(model.model.id, "claude-sonnet-4-5");
    assert_eq!(
        model.model.name, "Claude Sonnet 4.5 (latest)",
        "built-in registry models should preserve catalog display names"
    );
    assert!(
        model.model.reasoning,
        "Claude Sonnet should support reasoning"
    );
    assert!(model.model.context_window > 0, "Should have context window");
    assert!(model.model.max_tokens > 0, "Should have max tokens");
    assert!(!model.model.input.is_empty(), "Should have input types");

    harness.section("Verify non-reasoning model");
    let gpt4o = registry.find("openai", "gpt-4o");
    assert!(gpt4o.is_some(), "GPT-4o should exist");
    let gpt4o = gpt4o.unwrap();
    assert!(
        !gpt4o.model.reasoning,
        "GPT-4o should not support reasoning"
    );
}

// ============================================================================
// Custom models.json Tests
// ============================================================================

#[test]
fn test_custom_models_json_adds_new_provider() {
    let harness = TestHarness::new("test_custom_models_json_adds_new_provider");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");

    // Create models.json with a custom provider
    let models_json = r#"{
        "providers": {
            "lmstudio": {
                "baseUrl": "http://localhost:1234/v1",
                "api": "openai-completions",
                "models": [
                    {
                        "id": "llama3-8b",
                        "name": "Llama 3 8B",
                        "contextWindow": 8192,
                        "maxTokens": 4096
                    }
                ]
            }
        }
    }"#;
    let models_path = harness.create_file("models.json", models_json);

    harness.section("Load registry");
    let registry = ModelRegistry::load(&auth, Some(models_path));

    harness.section("Verify");
    assert!(registry.error().is_none(), "No error expected");

    let lmstudio_model = registry.find("lmstudio", "llama3-8b");
    assert!(lmstudio_model.is_some(), "LMStudio model should exist");

    let model = lmstudio_model.unwrap();
    harness
        .log()
        .info_ctx("verify", "Custom model fields", |ctx| {
            ctx.push(("id".into(), model.model.id.clone()));
            ctx.push(("base_url".into(), model.model.base_url.clone()));
            ctx.push((
                "context_window".into(),
                model.model.context_window.to_string(),
            ));
        });

    assert_eq!(model.model.id, "llama3-8b");
    assert_eq!(model.model.name, "Llama 3 8B");
    assert_eq!(model.model.base_url, "http://localhost:1234/v1");
    assert_eq!(model.model.context_window, 8192);
    assert_eq!(model.model.max_tokens, 4096);
}

#[test]
fn test_custom_models_json_overrides_provider_config() {
    let harness = TestHarness::new("test_custom_models_json_overrides_provider_config");
    harness.section("Setup");

    // Create auth with Anthropic key
    let auth_content = r#"{"anthropic": {"type": "api_key", "key": "sk-ant-test-key"}}"#;
    let auth_path = harness.create_file("auth.json", auth_content);
    let auth = AuthStorage::load(auth_path).expect("load auth");

    // Create models.json that overrides Anthropic base URL (e.g., for proxy)
    let models_json = r#"{
        "providers": {
            "anthropic": {
                "baseUrl": "https://my-proxy.example.com/v1/messages"
            }
        }
    }"#;
    let models_path = harness.create_file("models.json", models_json);

    harness.section("Load registry");
    let registry = ModelRegistry::load(&auth, Some(models_path));

    harness.section("Verify");
    assert!(registry.error().is_none(), "No error expected");

    // All Anthropic models should have the overridden base URL
    let anthropic_models: Vec<_> = registry
        .models()
        .iter()
        .filter(|m| m.model.provider == "anthropic")
        .collect();

    assert!(!anthropic_models.is_empty(), "Should have Anthropic models");
    for model in anthropic_models {
        assert_eq!(
            model.model.base_url, "https://my-proxy.example.com/v1/messages",
            "Base URL should be overridden for {}",
            model.model.id
        );
    }
}

#[test]
fn test_custom_models_json_replaces_provider_models() {
    let harness = TestHarness::new("test_custom_models_json_replaces_provider_models");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");

    // Create models.json that fully replaces OpenAI models
    let models_json = r#"{
        "providers": {
            "openai": {
                "baseUrl": "https://api.openai.com/v1",
                "models": [
                    {"id": "custom-gpt", "name": "Custom GPT"}
                ]
            }
        }
    }"#;
    let models_path = harness.create_file("models.json", models_json);

    harness.section("Load registry");
    let registry = ModelRegistry::load(&auth, Some(models_path));

    harness.section("Verify");
    // Built-in OpenAI models should be replaced
    let openai_models: Vec<_> = registry
        .models()
        .iter()
        .filter(|m| m.model.provider == "openai")
        .collect();

    harness
        .log()
        .info_ctx("verify", "OpenAI models after replace", |ctx| {
            ctx.push(("count".into(), openai_models.len().to_string()));
            for m in &openai_models {
                ctx.push(("model".into(), m.model.id.clone()));
            }
        });

    assert_eq!(openai_models.len(), 1, "Should only have the custom model");
    assert_eq!(openai_models[0].model.id, "custom-gpt");

    // Original GPT-4o should not exist
    assert!(
        registry.find("openai", "gpt-4o").is_none(),
        "Built-in gpt-4o should be replaced"
    );
}

#[test]
fn test_model_with_reasoning_flag() {
    let harness = TestHarness::new("test_model_with_reasoning_flag");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");

    let models_json = r#"{
        "providers": {
            "custom": {
                "baseUrl": "http://localhost:8080",
                "models": [
                    {"id": "reasoning-model", "name": "Reasoning Model", "reasoning": true},
                    {"id": "basic-model", "name": "Basic Model", "reasoning": false}
                ]
            }
        }
    }"#;
    let models_path = harness.create_file("models.json", models_json);

    harness.section("Load and verify");
    let registry = ModelRegistry::load(&auth, Some(models_path));

    let reasoning = registry.find("custom", "reasoning-model").unwrap();
    let basic = registry.find("custom", "basic-model").unwrap();

    assert!(reasoning.model.reasoning, "Should support reasoning");
    assert!(!basic.model.reasoning, "Should not support reasoning");
}

#[test]
fn test_model_with_input_types() {
    let harness = TestHarness::new("test_model_with_input_types");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");

    let models_json = r#"{
        "providers": {
            "custom": {
                "baseUrl": "http://localhost:8080",
                "models": [
                    {"id": "text-only", "input": ["text"]},
                    {"id": "multimodal", "input": ["text", "image"]}
                ]
            }
        }
    }"#;
    let models_path = harness.create_file("models.json", models_json);

    harness.section("Load and verify");
    let registry = ModelRegistry::load(&auth, Some(models_path));

    let text_only = registry.find("custom", "text-only").unwrap();
    let multimodal = registry.find("custom", "multimodal").unwrap();

    assert_eq!(
        text_only.model.input.len(),
        1,
        "Text-only should have 1 input type"
    );
    assert_eq!(
        multimodal.model.input.len(),
        2,
        "Multimodal should have 2 input types"
    );
}

#[test]
fn test_model_with_cost_config() {
    let harness = TestHarness::new("test_model_with_cost_config");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");

    let models_json = r#"{
        "providers": {
            "custom": {
                "baseUrl": "http://localhost:8080",
                "models": [
                    {
                        "id": "priced-model",
                        "cost": {
                            "input": 0.003,
                            "output": 0.015,
                            "cacheRead": 0.001,
                            "cacheWrite": 0.002
                        }
                    }
                ]
            }
        }
    }"#;
    let models_path = harness.create_file("models.json", models_json);

    harness.section("Load and verify");
    let registry = ModelRegistry::load(&auth, Some(models_path));

    let model = registry.find("custom", "priced-model").unwrap();
    assert!((model.model.cost.input - 0.003).abs() < f64::EPSILON);
    assert!((model.model.cost.output - 0.015).abs() < f64::EPSILON);
    assert!((model.model.cost.cache_read - 0.001).abs() < f64::EPSILON);
    assert!((model.model.cost.cache_write - 0.002).abs() < f64::EPSILON);
}

#[test]
fn test_model_with_headers() {
    let harness = TestHarness::new("test_model_with_headers");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");

    let models_json = r#"{
        "providers": {
            "custom": {
                "baseUrl": "http://localhost:8080",
                "headers": {
                    "X-Provider-Header": "provider-value"
                },
                "models": [
                    {
                        "id": "header-model",
                        "headers": {
                            "X-Model-Header": "model-value"
                        }
                    }
                ]
            }
        }
    }"#;
    let models_path = harness.create_file("models.json", models_json);

    harness.section("Load and verify");
    let registry = ModelRegistry::load(&auth, Some(models_path));

    let model = registry.find("custom", "header-model").unwrap();

    harness.log().info_ctx("verify", "Model headers", |ctx| {
        for (k, v) in &model.headers {
            ctx.push((k.clone(), v.clone()));
        }
    });

    // Headers should be merged (provider + model)
    assert_eq!(
        model.headers.get("X-Provider-Header"),
        Some(&"provider-value".to_string())
    );
    assert_eq!(
        model.headers.get("X-Model-Header"),
        Some(&"model-value".to_string())
    );
}

#[test]
fn test_model_with_compat_config() {
    let harness = TestHarness::new("test_model_with_compat_config");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");

    let models_json = r#"{
        "providers": {
            "custom": {
                "baseUrl": "http://localhost:8080",
                "compat": {
                    "supportsStore": true,
                    "supportsDeveloperRole": false
                },
                "models": [
                    {"id": "compat-model"}
                ]
            }
        }
    }"#;
    let models_path = harness.create_file("models.json", models_json);

    harness.section("Load and verify");
    let registry = ModelRegistry::load(&auth, Some(models_path));

    let model = registry.find("custom", "compat-model").unwrap();
    assert!(model.compat.is_some(), "Should have compat config");

    let compat = model.compat.as_ref().unwrap();
    assert_eq!(compat.supports_store, Some(true));
    assert_eq!(compat.supports_developer_role, Some(false));
}

// ============================================================================
// Error Handling Tests
// ============================================================================

#[test]
fn test_malformed_models_json_reports_error() {
    let harness = TestHarness::new("test_malformed_models_json_reports_error");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");

    // Create invalid JSON
    let models_path = harness.create_file("models.json", "{ invalid json }");

    harness.section("Load registry");
    let registry = ModelRegistry::load(&auth, Some(models_path.clone()));

    harness.section("Verify");
    assert!(
        registry.error().is_some(),
        "Should report error for invalid JSON"
    );

    let error = registry.error().unwrap();
    harness.log().info_ctx("verify", "Error message", |ctx| {
        ctx.push(("error".into(), error.to_string()));
    });

    assert!(
        error.contains(&models_path.display().to_string()),
        "Error should mention file path"
    );

    // Built-in models should still be available
    assert!(
        !registry.models().is_empty(),
        "Built-in models should still load on error"
    );
}

#[test]
fn test_missing_models_json_no_error() {
    let harness = TestHarness::new("test_missing_models_json_no_error");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");

    // Point to non-existent file
    let models_path = harness.temp_path("nonexistent_models.json");

    harness.section("Load registry");
    let registry = ModelRegistry::load(&auth, Some(models_path));

    harness.section("Verify");
    assert!(
        registry.error().is_none(),
        "Missing file should not report error"
    );
    assert!(!registry.models().is_empty(), "Built-in models should load");
}

#[test]
fn test_empty_providers_models_json_no_error() {
    let harness = TestHarness::new("test_empty_providers_models_json_no_error");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");

    // Valid JSON with empty providers
    let models_path = harness.create_file("models.json", r#"{"providers": {}}"#);

    harness.section("Load registry");
    let registry = ModelRegistry::load(&auth, Some(models_path));

    harness.section("Verify");
    assert!(
        registry.error().is_none(),
        "Empty providers should not error"
    );
    assert!(!registry.models().is_empty(), "Built-in models should load");
}

#[test]
fn test_invalid_models_json_structure_reports_error() {
    let harness = TestHarness::new("test_invalid_models_json_structure_reports_error");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");

    // JSON missing required 'providers' field
    let models_path = harness.create_file("models.json", "{}");

    harness.section("Load registry");
    let registry = ModelRegistry::load(&auth, Some(models_path));

    harness.section("Verify");
    // Missing required field should cause error
    assert!(
        registry.error().is_some(),
        "Missing 'providers' should report error"
    );
    // But built-in models should still load
    assert!(
        !registry.models().is_empty(),
        "Built-in models should still load"
    );
}

// ============================================================================
// Find and Filter Tests
// ============================================================================

#[test]
fn test_find_model_by_provider_and_id() {
    let harness = TestHarness::new("test_find_model_by_provider_and_id");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");
    let registry = ModelRegistry::load(&auth, None);

    harness.section("Find existing models");
    assert!(registry.find("anthropic", "claude-sonnet-4-5").is_some());
    assert!(registry.find("openai", "gpt-4o").is_some());
    assert!(registry.find("google", "gemini-2.5-pro").is_some());

    harness.section("Find non-existing models");
    assert!(registry.find("anthropic", "nonexistent").is_none());
    assert!(registry.find("nonexistent", "claude-sonnet-4-5").is_none());
}

#[test]
fn test_get_available_filters_by_api_key() {
    let harness = TestHarness::new("test_get_available_filters_by_api_key");
    harness.section("Setup");

    // Create auth with only OpenAI key
    let auth_content = r#"{"openai": {"type": "api_key", "key": "sk-test-key"}}"#;
    let auth_path = harness.create_file("auth.json", auth_content);
    let auth = AuthStorage::load(auth_path).expect("load auth");
    let registry =
        ModelRegistry::load_with_credential_resolver(None, |provider| auth.api_key(provider));

    harness.section("Verify");
    let available = registry.get_available();

    harness.log().info_ctx("verify", "Available models", |ctx| {
        ctx.push(("count".into(), available.len().to_string()));
        for m in &available {
            ctx.push((
                "model".into(),
                format!("{}/{}", m.model.provider, m.model.id),
            ));
        }
    });

    // Only OpenAI models (plus always-available keyless/request-time-credential
    // providers) should be available.
    assert!(
        available
            .iter()
            .all(|m| m.model.provider == "openai" || !model_requires_configured_credential(m)),
        "Only OpenAI and keyless models should be available"
    );
    assert!(
        available.iter().any(|m| m.model.provider == "openai"),
        "OpenAI models should be available"
    );
    for provider in ["anthropic", "google"] {
        assert!(
            !available.iter().any(|m| m.model.provider == provider),
            "No {provider} models should be available"
        );
    }
}

// ============================================================================
// API Key Resolution Tests
// ============================================================================

#[test]
fn test_auth_header_flag() {
    let harness = TestHarness::new("test_auth_header_flag");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");

    let models_json = r#"{
        "providers": {
            "bearer-provider": {
                "baseUrl": "http://localhost:8080",
                "authHeader": true,
                "models": [{"id": "bearer-model"}]
            },
            "custom-auth-provider": {
                "baseUrl": "http://localhost:8081",
                "authHeader": false,
                "models": [{"id": "custom-auth-model"}]
            }
        }
    }"#;
    let models_path = harness.create_file("models.json", models_json);

    harness.section("Load and verify");
    let registry = ModelRegistry::load(&auth, Some(models_path));

    let bearer = registry.find("bearer-provider", "bearer-model").unwrap();
    let custom = registry
        .find("custom-auth-provider", "custom-auth-model")
        .unwrap();

    assert!(bearer.auth_header, "Should use Authorization header");
    assert!(!custom.auth_header, "Should not use Authorization header");
}

// ============================================================================
// Default Path Tests
// ============================================================================

#[test]
fn test_default_models_path() {
    let harness = TestHarness::new("test_default_models_path");
    harness.section("Setup");

    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    harness.section("Verify");
    let path = default_models_path(&agent_dir);

    harness.log().info_ctx("verify", "Default path", |ctx| {
        ctx.push(("path".into(), path.display().to_string()));
    });

    assert!(path.ends_with("models.json"), "Should end with models.json");
    assert!(
        path.starts_with(&agent_dir),
        "Should be inside agent directory"
    );
}

// ============================================================================
// Model Name Defaults
// ============================================================================

#[test]
fn test_model_name_defaults_to_id() {
    let harness = TestHarness::new("test_model_name_defaults_to_id");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");

    let models_json = r#"{
        "providers": {
            "custom": {
                "baseUrl": "http://localhost:8080",
                "models": [
                    {"id": "no-name-model"}
                ]
            }
        }
    }"#;
    let models_path = harness.create_file("models.json", models_json);

    harness.section("Load and verify");
    let registry = ModelRegistry::load(&auth, Some(models_path));

    let model = registry.find("custom", "no-name-model").unwrap();
    assert_eq!(
        model.model.name, "no-name-model",
        "Name should default to ID"
    );
}

// ============================================================================
// Context Window and Max Tokens Defaults
// ============================================================================

#[test]
fn test_context_window_and_max_tokens_defaults() {
    let harness = TestHarness::new("test_context_window_and_max_tokens_defaults");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");

    let models_json = r#"{
        "providers": {
            "custom": {
                "baseUrl": "http://localhost:8080",
                "models": [
                    {"id": "defaults-model"}
                ]
            }
        }
    }"#;
    let models_path = harness.create_file("models.json", models_json);

    harness.section("Load and verify");
    let registry = ModelRegistry::load(&auth, Some(models_path));

    let model = registry.find("custom", "defaults-model").unwrap();

    harness.log().info_ctx("verify", "Default values", |ctx| {
        ctx.push((
            "context_window".into(),
            model.model.context_window.to_string(),
        ));
        ctx.push(("max_tokens".into(), model.model.max_tokens.to_string()));
    });

    // Should have reasonable defaults
    assert_eq!(
        model.model.context_window, 128_000,
        "Default context window"
    );
    assert_eq!(model.model.max_tokens, 16_384, "Default max tokens");
}

// ============================================================================
// Deterministic Model Ordering
// ============================================================================

#[test]
fn test_model_order_is_stable() {
    let harness = TestHarness::new("test_model_order_is_stable");
    harness.section("Setup");

    let auth_path = harness.create_file("auth.json", "{}");
    let auth = AuthStorage::load(auth_path).expect("load auth");

    // Load registry twice
    let registry1 = ModelRegistry::load(&auth, None);
    let registry2 = ModelRegistry::load(&auth, None);

    harness.section("Verify");
    let models1: Vec<_> = registry1.models().iter().map(|m| &m.model.id).collect();
    let models2: Vec<_> = registry2.models().iter().map(|m| &m.model.id).collect();

    assert_eq!(models1, models2, "Model ordering should be stable");
}

// ============================================================================
// Multiple Providers with API Keys
// ============================================================================

#[test]
fn test_multiple_providers_with_keys() {
    let harness = TestHarness::new("test_multiple_providers_with_keys");
    harness.section("Setup");

    let auth_content = r#"{
        "anthropic": {"type": "api_key", "key": "sk-ant-test"},
        "openai": {"type": "api_key", "key": "sk-openai-test"}
    }"#;
    let auth_path = harness.create_file("auth.json", auth_content);
    let auth = AuthStorage::load(auth_path).expect("load auth");
    let registry =
        ModelRegistry::load_with_credential_resolver(None, |provider| auth.api_key(provider));

    harness.section("Verify");
    let available = registry.get_available();

    let providers: std::collections::HashSet<_> = available
        .iter()
        .map(|m| m.model.provider.as_str())
        .collect();

    harness
        .log()
        .info_ctx("verify", "Available providers", |ctx| {
            for p in &providers {
                ctx.push(("provider".to_string(), (*p).to_string()));
            }
        });

    assert!(
        providers.contains("anthropic"),
        "Anthropic should be available"
    );
    assert!(providers.contains("openai"), "OpenAI should be available");
    assert!(
        !providers.contains("google"),
        "Google should not be available"
    );
}
