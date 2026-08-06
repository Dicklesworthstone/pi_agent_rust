//! Comprehensive provider metadata and factory routing tests.
//!
//! Covers canonical ID resolution, alias mapping, routing defaults,
//! factory dispatch, and structural invariants for every provider
//! in `PROVIDER_METADATA`.
//!
//! bd-3uqg.8.1

mod common;

use pi::provider_metadata::{
    PROVIDER_METADATA, ProviderOnboardingMode, canonical_provider_id, provider_auth_env_keys,
    provider_metadata, provider_routing_defaults,
};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

fn provider_reference_strings(
    document: &Value,
    pointer: &str,
    field: &str,
) -> BTreeMap<String, Vec<String>> {
    document
        .pointer(pointer)
        .and_then(Value::as_array)
        .expect("provider reference must contain an entries array")
        .iter()
        .map(|entry| {
            let canonical_id = entry["canonical_id"]
                .as_str()
                .expect("provider reference entry needs canonical_id")
                .to_string();
            let values = entry[field]
                .as_array()
                .expect("provider reference entry needs the requested string array")
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .expect("provider reference value must be a string")
                        .to_string()
                })
                .collect();
            (canonical_id, values)
        })
        .collect()
}

fn validate_method_reference_counts(
    definitions: &serde_json::Map<String, Value>,
    references: &BTreeMap<String, Vec<String>>,
    label: &str,
) {
    for referenced in references.keys() {
        assert!(
            definitions.contains_key(referenced),
            "{label} references undefined method '{referenced}'"
        );
    }

    for (method, definition) in definitions {
        let actual_providers = references.get(method).cloned().unwrap_or_default();
        let expected_count = definition["providers_count"]
            .as_u64()
            .unwrap_or_else(|| panic!("{label} method '{method}' needs providers_count"));
        assert_eq!(
            actual_providers.len() as u64,
            expected_count,
            "{label} method '{method}' provider count drifted"
        );

        if let Some(documented) = definition.get("providers") {
            let mut documented = documented
                .as_array()
                .unwrap_or_else(|| panic!("{label} method '{method}' providers must be an array"))
                .iter()
                .map(|provider| {
                    provider
                        .as_str()
                        .unwrap_or_else(|| {
                            panic!("{label} method '{method}' provider must be a string")
                        })
                        .to_string()
                })
                .collect::<Vec<_>>();
            let mut actual = actual_providers;
            documented.sort();
            actual.sort();
            assert_eq!(
                actual, documented,
                "{label} method '{method}' provider list drifted"
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Structural invariants
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn all_canonical_ids_are_unique() {
    let mut seen = HashSet::new();
    for meta in PROVIDER_METADATA {
        assert!(
            seen.insert(meta.canonical_id),
            "duplicate canonical_id: {}",
            meta.canonical_id
        );
    }
}

#[test]
fn no_alias_collides_with_canonical_id() {
    let canonicals: HashSet<&str> = PROVIDER_METADATA.iter().map(|m| m.canonical_id).collect();
    for meta in PROVIDER_METADATA {
        for alias in meta.aliases {
            // An alias may NOT shadow a different canonical_id (it would create ambiguity).
            if let Some(other) = provider_metadata(alias) {
                assert_eq!(
                    other.canonical_id, meta.canonical_id,
                    "alias '{}' resolves to '{}' but belongs to '{}'",
                    alias, other.canonical_id, meta.canonical_id
                );
            }
        }
    }
    // Also confirm no alias is another entry's canonical_id (unless they share the same entry).
    for meta in PROVIDER_METADATA {
        for alias in meta.aliases {
            if canonicals.contains(alias) {
                // Must be the same entry's canonical_id (self-alias) - currently not used, but guard.
                assert_eq!(
                    *alias, meta.canonical_id,
                    "alias '{alias}' shadows canonical_id of a different provider"
                );
            }
        }
    }
}

#[test]
fn no_duplicate_aliases_across_entries() {
    let mut alias_to_canonical: HashMap<&str, &str> = HashMap::new();
    for meta in PROVIDER_METADATA {
        for alias in meta.aliases {
            if let Some(prev) = alias_to_canonical.insert(alias, meta.canonical_id) {
                assert_eq!(
                    prev, meta.canonical_id,
                    "alias '{}' claimed by both '{}' and '{}'",
                    alias, prev, meta.canonical_id
                );
            }
        }
    }
}

#[test]
fn every_canonical_id_is_lowercase_trimmed() {
    for meta in PROVIDER_METADATA {
        assert_eq!(
            meta.canonical_id,
            meta.canonical_id.trim(),
            "canonical_id '{}' has leading/trailing whitespace",
            meta.canonical_id
        );
        assert_eq!(
            meta.canonical_id,
            meta.canonical_id.to_lowercase(),
            "canonical_id '{}' must be lowercase",
            meta.canonical_id
        );
    }
}

#[test]
fn every_alias_is_lowercase_trimmed() {
    for meta in PROVIDER_METADATA {
        for alias in meta.aliases {
            assert_eq!(
                *alias,
                alias.trim(),
                "alias '{}' (of '{}') has whitespace",
                alias,
                meta.canonical_id
            );
            assert_eq!(
                *alias,
                alias.to_lowercase(),
                "alias '{}' (of '{}') must be lowercase",
                alias,
                meta.canonical_id
            );
        }
    }
}

#[test]
fn every_remote_provider_has_at_least_one_auth_env_key() {
    // Local and dedicated OAuth providers legitimately have no API-key env keys.
    let auth_env_optional_providers = [
        "google-antigravity",
        "google-gemini-cli",
        "llamacpp",
        "mistralrs",
        "ollama",
        "openai-codex",
    ];
    for meta in PROVIDER_METADATA {
        if auth_env_optional_providers.contains(&meta.canonical_id) {
            continue;
        }
        assert!(
            !meta.auth_env_keys.is_empty(),
            "provider '{}' has no auth env keys",
            meta.canonical_id
        );
    }
}

#[test]
fn auth_env_keys_are_screaming_snake_case() {
    for meta in PROVIDER_METADATA {
        for key in meta.auth_env_keys {
            assert!(
                key.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
                "env key '{}' (provider '{}') must be SCREAMING_SNAKE_CASE",
                key,
                meta.canonical_id
            );
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn provider_auth_reference_artifacts_match_runtime_metadata() {
    let expected_aliases: BTreeMap<String, Vec<String>> = PROVIDER_METADATA
        .iter()
        .map(|meta| {
            (
                meta.canonical_id.to_string(),
                meta.aliases.iter().map(ToString::to_string).collect(),
            )
        })
        .collect();
    let crosswalk: Value =
        serde_json::from_str(include_str!("../docs/provider-auth-crosswalk.json"))
            .expect("provider auth crosswalk must be valid JSON");
    let policy: Value =
        serde_json::from_str(include_str!("../docs/provider-canonical-id-policy.json"))
            .expect("provider canonical ID policy must be valid JSON");

    assert_eq!(PROVIDER_METADATA.len(), 94);
    assert_eq!(crosswalk["schema"], "pi.docs.provider_auth_crosswalk.v2");
    assert!(crosswalk.get("api_key_resolution_precedence").is_none());
    assert!(crosswalk.get("auth_method_patterns").is_none());
    assert_eq!(
        crosswalk["schema_migration_from_v1"]["renamed_sections"]["api_key_resolution_precedence"],
        "generic_direct_credential_precedence"
    );
    assert_eq!(
        crosswalk["schema_migration_from_v1"]["renamed_sections"]["auth_method_patterns"],
        "request_auth_methods"
    );
    assert_eq!(crosswalk["providers"].as_array().map(Vec::len), Some(94));
    assert_eq!(
        policy["canonical_id_map"]["entries"]
            .as_array()
            .map(Vec::len),
        Some(94)
    );
    assert_eq!(
        provider_reference_strings(&crosswalk, "/providers", "aliases"),
        expected_aliases
    );
    assert_eq!(
        provider_reference_strings(&policy, "/canonical_id_map/entries", "aliases"),
        expected_aliases
    );
    let expected_env_keys: BTreeMap<String, Vec<String>> = PROVIDER_METADATA
        .iter()
        .map(|meta| {
            (
                meta.canonical_id.to_string(),
                meta.auth_env_keys.iter().map(ToString::to_string).collect(),
            )
        })
        .collect();
    assert_eq!(
        provider_reference_strings(&crosswalk, "/providers", "env_keys"),
        expected_env_keys
    );

    let precedence: Vec<&str> = crosswalk["generic_direct_credential_precedence"]["steps"]
        .as_array()
        .expect("auth crosswalk must contain precedence steps")
        .iter()
        .map(|step| {
            step["source"]
                .as_str()
                .expect("precedence source must be a string")
        })
        .collect();
    assert_eq!(
        precedence,
        [
            "Explicit override parameter (--api-key CLI flag or programmatic override)",
            "Stored OAuth or bearer credential in auth.json",
            "Direct-token environment variables (checked in provider order)",
            "Stored API key in auth.json",
            "Supported external coding-CLI credential",
        ]
    );

    let provider_rows = crosswalk["providers"]
        .as_array()
        .expect("auth crosswalk must contain provider rows");
    let request_definitions = crosswalk["request_auth_methods"]
        .as_object()
        .expect("auth crosswalk must define request auth methods");
    let mut request_references = BTreeMap::<String, Vec<String>>::new();
    for provider in provider_rows {
        let provider_id = provider["canonical_id"]
            .as_str()
            .expect("provider row needs canonical_id");
        let primary = provider["auth_method"]
            .as_str()
            .expect("provider row needs a primary request auth method");
        let mut methods = vec![primary];
        if let Some(additional) = provider.get("additional_auth_methods") {
            methods.extend(
                additional
                    .as_array()
                    .expect("additional_auth_methods must be an array")
                    .iter()
                    .map(|method| {
                        method
                            .as_str()
                            .expect("additional request auth method must be a string")
                    }),
            );
        }
        let mut unique = HashSet::new();
        for method in methods {
            assert!(
                unique.insert(method),
                "provider '{provider_id}' repeats request auth method '{method}'"
            );
            request_references
                .entry(method.to_string())
                .or_default()
                .push(provider_id.to_string());
        }
    }
    validate_method_reference_counts(request_definitions, &request_references, "request auth");

    let acquisition = crosswalk["credential_acquisition"]
        .as_object()
        .expect("auth crosswalk must define credential acquisition");
    let acquisition_definitions = acquisition["methods"]
        .as_object()
        .expect("credential acquisition must define methods");
    let default_acquisition = acquisition["default_methods"]
        .as_array()
        .expect("credential acquisition must define defaults");
    let acquisition_overrides = acquisition["overrides"]
        .as_object()
        .expect("credential acquisition overrides must be an object");
    for provider_id in acquisition_overrides.keys() {
        assert!(
            expected_aliases.contains_key(provider_id),
            "credential acquisition override references unknown provider '{provider_id}'"
        );
    }

    let mut acquisition_references = BTreeMap::<String, Vec<String>>::new();
    for provider in provider_rows {
        let provider_id = provider["canonical_id"]
            .as_str()
            .expect("provider row needs canonical_id");
        let methods =
            acquisition_overrides
                .get(provider_id)
                .map_or(default_acquisition, |methods| {
                    methods
                        .as_array()
                        .expect("credential acquisition override must be an array")
                });
        assert!(
            !methods.is_empty(),
            "provider '{provider_id}' needs at least one credential acquisition method"
        );
        let mut unique = HashSet::new();
        for method in methods {
            let method = method
                .as_str()
                .expect("credential acquisition method must be a string");
            assert!(
                unique.insert(method),
                "provider '{provider_id}' repeats credential acquisition method '{method}'"
            );
            acquisition_references
                .entry(method.to_string())
                .or_default()
                .push(provider_id.to_string());
        }
    }
    validate_method_reference_counts(
        acquisition_definitions,
        &acquisition_references,
        "credential acquisition",
    );

    let provider_request_methods = |provider_id: &str| {
        let provider = provider_rows
            .iter()
            .find(|provider| provider["canonical_id"] == provider_id)
            .unwrap_or_else(|| panic!("missing provider row '{provider_id}'"));
        let mut methods = vec![
            provider["auth_method"]
                .as_str()
                .expect("provider auth_method"),
        ];
        if let Some(additional) = provider.get("additional_auth_methods") {
            methods.extend(
                additional
                    .as_array()
                    .expect("additional request methods")
                    .iter()
                    .map(|method| method.as_str().expect("request method string")),
            );
        }
        methods
    };
    let provider_acquisition_methods = |provider_id: &str| {
        acquisition_overrides
            .get(provider_id)
            .map_or(default_acquisition, |methods| {
                methods.as_array().expect("credential acquisition methods")
            })
            .iter()
            .map(|method| method.as_str().expect("acquisition method string"))
            .collect::<Vec<_>>()
    };

    assert_eq!(
        provider_request_methods("anthropic"),
        ["x_api_key", "bearer_token"]
    );
    assert_eq!(
        provider_acquisition_methods("anthropic"),
        ["direct_api_key", "oauth_login", "external_cli_credentials"]
    );
    assert_eq!(
        provider_request_methods("kimi-for-coding"),
        ["x_api_key", "bearer_token"]
    );
    assert_eq!(
        provider_acquisition_methods("kimi-for-coding"),
        [
            "direct_api_key",
            "kimi_device_oauth",
            "external_cli_credentials"
        ]
    );
    assert_eq!(
        provider_request_methods("amazon-bedrock"),
        ["aws_sigv4", "bearer_token"]
    );
    assert_eq!(
        provider_acquisition_methods("amazon-bedrock"),
        ["aws_credential_chain", "direct_bearer_override"]
    );
    assert_eq!(provider_request_methods("sap-ai-core"), ["bearer_token"]);
    assert_eq!(
        provider_acquisition_methods("sap-ai-core"),
        ["sap_client_credentials_exchange", "direct_bearer_override"]
    );
    assert_eq!(provider_request_methods("github-copilot"), ["bearer_token"]);
    assert_eq!(
        provider_acquisition_methods("github-copilot"),
        [
            "direct_api_key",
            "oauth_login",
            "copilot_device_oauth",
            "copilot_session_exchange"
        ]
    );
    assert_eq!(provider_request_methods("gitlab"), ["bearer_token"]);
    assert_eq!(
        provider_acquisition_methods("gitlab"),
        ["direct_api_key", "oauth_login"]
    );
    for local in ["ollama", "llamacpp", "mistralrs"] {
        assert_eq!(provider_request_methods(local), ["no_auth_required"]);
        assert_eq!(provider_acquisition_methods(local), ["no_credentials"]);
    }

    assert!(!expected_aliases.contains_key("github-models"));
    assert!(expected_aliases.contains_key("github-copilot"));
}

// ═══════════════════════════════════════════════════════════════════════
// Canonical ID / alias resolution
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn every_canonical_id_resolves_to_itself() {
    for meta in PROVIDER_METADATA {
        let resolved = canonical_provider_id(meta.canonical_id);
        assert_eq!(
            resolved,
            Some(meta.canonical_id),
            "canonical_provider_id('{}') should return itself",
            meta.canonical_id
        );
    }
}

#[test]
fn every_alias_resolves_to_its_canonical_id() {
    for meta in PROVIDER_METADATA {
        for alias in meta.aliases {
            let resolved = canonical_provider_id(alias);
            assert_eq!(
                resolved,
                Some(meta.canonical_id),
                "alias '{}' should resolve to '{}'",
                alias,
                meta.canonical_id
            );
        }
    }
}

#[test]
fn auth_env_keys_accessible_via_aliases() {
    for meta in PROVIDER_METADATA {
        let canonical_keys = provider_auth_env_keys(meta.canonical_id);
        for alias in meta.aliases {
            let alias_keys = provider_auth_env_keys(alias);
            assert_eq!(
                canonical_keys, alias_keys,
                "auth env keys for alias '{}' must match canonical '{}'",
                alias, meta.canonical_id
            );
        }
    }
}

#[test]
fn unknown_provider_returns_none() {
    assert!(provider_metadata("nonexistent-provider-xyz").is_none());
    assert!(canonical_provider_id("nonexistent-provider-xyz").is_none());
    assert!(provider_routing_defaults("nonexistent-provider-xyz").is_none());
}

// ═══════════════════════════════════════════════════════════════════════
// Routing defaults invariants
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn oai_compatible_providers_have_routing_defaults() {
    for meta in PROVIDER_METADATA {
        if meta.onboarding == ProviderOnboardingMode::OpenAICompatiblePreset {
            assert!(
                meta.routing_defaults.is_some(),
                "OAI-compatible provider '{}' must have routing_defaults",
                meta.canonical_id
            );
        }
    }
}

#[test]
fn native_adapter_providers_routing_defaults_are_consistent() {
    // Native adapter providers MAY have routing_defaults (for context_window,
    // max_tokens, etc.) but their base_url is typically empty because they
    // construct URLs from provider-specific config (project/region/deployment).
    for meta in PROVIDER_METADATA {
        if meta.onboarding == ProviderOnboardingMode::NativeAdapterRequired
            && let Some(defaults) = &meta.routing_defaults
        {
            // Native providers with routing_defaults should have an
            // API identifier (non-empty).
            assert!(
                !defaults.api.is_empty(),
                "native-adapter provider '{}' has routing_defaults but empty api",
                meta.canonical_id
            );
        }
    }
}

#[test]
fn all_oai_compatible_base_urls_are_nonempty() {
    // Native providers (BuiltInNative, NativeAdapterRequired) may have empty
    // base_url because they construct endpoints from provider-specific config.
    // Only OpenAI-compatible presets require a non-empty base_url.
    for meta in PROVIDER_METADATA {
        if meta.onboarding == ProviderOnboardingMode::OpenAICompatiblePreset
            && let Some(defaults) = &meta.routing_defaults
        {
            assert!(
                !defaults.base_url.is_empty(),
                "OAI-compatible provider '{}' has empty base_url",
                meta.canonical_id
            );
        }
    }
}

#[test]
fn all_oai_compatible_base_urls_are_unique() {
    let mut url_to_provider: HashMap<&str, &str> = HashMap::new();
    let shared_endpoint_pairs: HashSet<(&str, &str)> = HashSet::from([
        ("minimax", "minimax-coding-plan"),
        ("minimax-coding-plan", "minimax"),
        ("minimax-cn", "minimax-cn-coding-plan"),
        ("minimax-cn-coding-plan", "minimax-cn"),
        // Local OpenAI-compatible servers that both default to 127.0.0.1:1234.
        ("lmstudio", "mistralrs"),
        ("mistralrs", "lmstudio"),
    ]);
    for meta in PROVIDER_METADATA {
        if let Some(defaults) = &meta.routing_defaults {
            // Skip empty base_urls (native providers construct URLs differently).
            if defaults.base_url.is_empty() {
                continue;
            }
            if let Some(prev) = url_to_provider.insert(defaults.base_url, meta.canonical_id) {
                assert!(
                    shared_endpoint_pairs.contains(&(prev, meta.canonical_id)),
                    "base_url '{}' used by both '{}' and '{}'",
                    defaults.base_url,
                    prev,
                    meta.canonical_id
                );
            }
        }
    }
}

#[test]
fn oai_compatible_defaults_use_known_api_family() {
    let known_apis = [
        "openai-completions",
        "openai-responses",
        "anthropic-messages",
        "cohere-chat",
        "google-generative-ai",
        // Native provider API families:
        "google-vertex",
        "bedrock-converse-stream",
        "gitlab-chat",
        "copilot-openai",
        "cursor-agent",
    ];
    for meta in PROVIDER_METADATA {
        if let Some(defaults) = &meta.routing_defaults {
            assert!(
                known_apis.contains(&defaults.api),
                "provider '{}' has unknown api '{}', expected one of {:?}",
                meta.canonical_id,
                defaults.api,
                known_apis
            );
        }
    }
}

#[test]
fn context_window_and_max_tokens_are_positive() {
    for meta in PROVIDER_METADATA {
        if let Some(defaults) = &meta.routing_defaults {
            assert!(
                defaults.context_window > 0,
                "provider '{}' context_window must be > 0",
                meta.canonical_id
            );
            assert!(
                defaults.max_tokens > 0,
                "provider '{}' max_tokens must be > 0",
                meta.canonical_id
            );
            assert!(
                defaults.max_tokens <= defaults.context_window,
                "provider '{}' max_tokens ({}) exceeds context_window ({})",
                meta.canonical_id,
                defaults.max_tokens,
                defaults.context_window
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Factory routing: every provider dispatches without error
// ═══════════════════════════════════════════════════════════════════════

/// Helper: build a `ModelEntry` for an OAI-compatible provider.
fn oai_entry(provider: &str, api: &str, base_url: &str) -> pi::models::ModelEntry {
    use pi::provider::{InputType, Model, ModelCost};
    pi::models::ModelEntry {
        model: Model {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            api: api.to_string(),
            provider: provider.to_string(),
            base_url: base_url.to_string(),
            reasoning: false,
            input: vec![InputType::Text],
            cost: ModelCost {
                input: 0.001,
                output: 0.002,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 128_000,
            max_tokens: 16_384,
            headers: std::collections::HashMap::new(),
        },
        api_key: Some("test-key".to_string()),
        headers: std::collections::HashMap::new(),
        auth_header: true,
        compat: None,
        oauth_config: None,
    }
}

#[test]
fn factory_dispatches_every_oai_compatible_provider() {
    use pi::providers::create_provider;

    for meta in PROVIDER_METADATA {
        if meta.onboarding != ProviderOnboardingMode::OpenAICompatiblePreset {
            continue;
        }
        let defaults = meta
            .routing_defaults
            .expect("OAI provider must have defaults");
        let entry = oai_entry(meta.canonical_id, defaults.api, defaults.base_url);
        let provider = create_provider(&entry, None)
            .unwrap_or_else(|e| panic!("factory failed for '{}': {e}", meta.canonical_id));
        assert_eq!(
            provider.api(),
            defaults.api,
            "factory api mismatch for '{}'",
            meta.canonical_id
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn factory_dispatches_native_established_providers() {
    use pi::providers::create_provider;

    // Anthropic
    let anthropic_entry = {
        use pi::provider::{InputType, Model, ModelCost};
        pi::models::ModelEntry {
            model: Model {
                id: "claude-sonnet-4-5".to_string(),
                name: "Claude Sonnet".to_string(),
                api: "anthropic-messages".to_string(),
                provider: "anthropic".to_string(),
                base_url: "https://api.anthropic.com".to_string(),
                reasoning: false,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.003,
                    output: 0.015,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 200_000,
                max_tokens: 8_192,
                headers: std::collections::HashMap::new(),
            },
            api_key: Some("test-key".to_string()),
            headers: std::collections::HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        }
    };
    let p = create_provider(&anthropic_entry, None).expect("anthropic factory");
    assert_eq!(p.api(), "anthropic-messages");

    // Google/Gemini
    let google_entry = {
        use pi::provider::{InputType, Model, ModelCost};
        pi::models::ModelEntry {
            model: Model {
                id: "gemini-2.0-flash".to_string(),
                name: "Gemini Flash".to_string(),
                api: "google-generative-ai".to_string(),
                provider: "google".to_string(),
                base_url: "https://generativelanguage.googleapis.com".to_string(),
                reasoning: false,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.001,
                    output: 0.004,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 1_000_000,
                max_tokens: 8_192,
                headers: std::collections::HashMap::new(),
            },
            api_key: Some("test-key".to_string()),
            headers: std::collections::HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        }
    };
    let p = create_provider(&google_entry, None).expect("google factory");
    assert_eq!(p.api(), "google-generative-ai");

    // Cohere
    let cohere_entry = {
        use pi::provider::{InputType, Model, ModelCost};
        pi::models::ModelEntry {
            model: Model {
                id: "command-r-plus".to_string(),
                name: "Command R+".to_string(),
                api: "cohere-chat".to_string(),
                provider: "cohere".to_string(),
                base_url: "https://api.cohere.com".to_string(),
                reasoning: false,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.003,
                    output: 0.015,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 4_096,
                headers: std::collections::HashMap::new(),
            },
            api_key: Some("test-key".to_string()),
            headers: std::collections::HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        }
    };
    let p = create_provider(&cohere_entry, None).expect("cohere factory");
    assert_eq!(p.api(), "cohere-chat");

    // Amazon Bedrock
    let bedrock_entry = {
        use pi::provider::{InputType, Model, ModelCost};
        pi::models::ModelEntry {
            model: Model {
                id: "anthropic.claude-3-5-sonnet-20240620-v1:0".to_string(),
                name: "Claude Sonnet via Bedrock".to_string(),
                api: "bedrock-converse-stream".to_string(),
                provider: "amazon-bedrock".to_string(),
                base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".to_string(),
                reasoning: true,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 200_000,
                max_tokens: 8_192,
                headers: std::collections::HashMap::new(),
            },
            api_key: Some("test-bedrock-token".to_string()),
            headers: std::collections::HashMap::new(),
            auth_header: false,
            compat: None,
            oauth_config: None,
        }
    };
    let p = create_provider(&bedrock_entry, None).expect("bedrock factory");
    assert_eq!(p.api(), "bedrock-converse-stream");
}

// ═══════════════════════════════════════════════════════════════════════
// Coverage assertion: provider count
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn provider_metadata_count_is_at_least_56() {
    // Guard against accidental removal of entries.
    assert!(
        PROVIDER_METADATA.len() >= 56,
        "expected at least 56 provider entries, found {}",
        PROVIDER_METADATA.len()
    );
}

#[test]
fn total_aliases_count_is_consistent() {
    let total_aliases: usize = PROVIDER_METADATA.iter().map(|m| m.aliases.len()).sum();
    // Sanity check: we have known aliases (gemini, fireworks-ai, kimi, moonshot,
    // dashscope, qwen, vertexai, bedrock, sap, azure, azure-cognitive-services,
    // copilot, gitlab-duo). At least 13.
    assert!(
        total_aliases >= 13,
        "expected at least 13 aliases, found {total_aliases}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Artifact generation: canonical ID + alias table (JSON)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn generate_canonical_id_alias_table_json() {
    use serde_json::json;

    let mut entries: Vec<Value> = Vec::new();
    for meta in PROVIDER_METADATA {
        let onboarding = match meta.onboarding {
            ProviderOnboardingMode::BuiltInNative => "built-in-native",
            ProviderOnboardingMode::NativeAdapterRequired => "native-adapter-required",
            ProviderOnboardingMode::OpenAICompatiblePreset => "oai-compatible-preset",
        };

        let mut entry = json!({
            "canonical_id": meta.canonical_id,
            "aliases": meta.aliases,
            "onboarding_mode": onboarding,
            "auth_env_keys": meta.auth_env_keys,
        });

        if let Some(defaults) = &meta.routing_defaults {
            entry["routing"] = json!({
                "api": defaults.api,
                "base_url": defaults.base_url,
                "auth_header": defaults.auth_header,
                "context_window": defaults.context_window,
                "max_tokens": defaults.max_tokens,
            });
        }

        entries.push(entry);
    }

    let table = json!({
        "schema_version": "1.0",
        "bead_id": "bd-3uqg.9.1.1",
        "description": "Canonical provider ID + alias table generated from PROVIDER_METADATA",
        "total_providers": PROVIDER_METADATA.len(),
        "total_aliases": PROVIDER_METADATA.iter().map(|m| m.aliases.len()).sum::<usize>(),
        "providers": entries,
    });

    let json_str = serde_json::to_string_pretty(&table).expect("JSON serialization");

    // Ordinary test runs verify the committed table without mutating source.
    // Maintainers can opt into regeneration explicitly when metadata changes.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs/provider-canonical-id-table.json");
    let generate = matches!(
        std::env::var("PI_GENERATE_PROVIDER_CANONICAL_ID_TABLE").as_deref(),
        Ok("1")
    );
    if generate {
        std::fs::write(&path, &json_str).expect("write canonical ID table");
    }

    // Verify the committed/generated file round-trips and exactly represents
    // the metadata used by the running test binary.
    let readback = std::fs::read_to_string(&path).expect("read canonical ID table");
    let parsed: Value = serde_json::from_str(&readback).expect("parse back");
    assert_eq!(
        parsed, table,
        "committed provider canonical ID table is stale; regenerate explicitly with \
         PI_GENERATE_PROVIDER_CANONICAL_ID_TABLE=1 cargo test \
         --test provider_metadata_comprehensive generate_canonical_id_alias_table_json -- --exact"
    );
    assert_eq!(
        usize::try_from(parsed["total_providers"].as_u64().unwrap()).unwrap(),
        PROVIDER_METADATA.len()
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Drift-prevention guardrails (bd-3uqg.11.10.4)
//
// These tests use hard-coded snapshots so that ANY addition, removal,
// or mutation of provider metadata produces a clear test failure that
// forces the developer to intentionally update the snapshot.
// ═══════════════════════════════════════════════════════════════════════

/// Hard-coded sorted snapshot of every `canonical_id` in `PROVIDER_METADATA`.
/// Adding or removing a provider without updating this list will fail the
/// test with a diff showing exactly what changed.
#[test]
#[allow(clippy::too_many_lines)]
fn canonical_id_snapshot_detects_additions_and_removals() {
    // ── Snapshot: 94 canonical IDs (sorted) ─────────────────────────────
    // To update: run the failing test, copy the "actual" list printed
    // below, and replace this array.
    const EXPECTED: &[&str] = &[
        "302ai",
        "abacus",
        "aihubmix",
        "alibaba",
        "alibaba-cn",
        "alibaba-us",
        "amazon-bedrock",
        "anthropic",
        "atlascloud",
        "azure-openai",
        "bailing",
        "baseten",
        "berget",
        "cerebras",
        "chutes",
        "cloudflare-ai-gateway",
        "cloudflare-workers-ai",
        "cohere",
        "cortecs",
        "cursor",
        "deepinfra",
        "deepseek",
        "fastrouter",
        "fireworks",
        "firmware",
        "friendli",
        "github-copilot",
        "gitlab",
        "google",
        "google-antigravity",
        "google-gemini-cli",
        "google-vertex",
        "groq",
        "helicone",
        "huggingface",
        "iflowcn",
        "inception",
        "inference",
        "io-net",
        "jiekou",
        "kimi-for-coding",
        "llama",
        "llamacpp",
        "lmstudio",
        "lucidquery",
        "minimax",
        "minimax-cn",
        "minimax-cn-coding-plan",
        "minimax-coding-plan",
        "mistral",
        "mistralrs",
        "moark",
        "modelscope",
        "moonshotai",
        "moonshotai-cn",
        "morph",
        "nano-gpt",
        "nebius",
        "nova",
        "novita-ai",
        "nvidia",
        "ollama",
        "ollama-cloud",
        "openai",
        "openai-codex",
        "opencode",
        "openrouter",
        "ovhcloud",
        "perplexity",
        "poe",
        "privatemode-ai",
        "requesty",
        "sap-ai-core",
        "scaleway",
        "siliconflow",
        "siliconflow-cn",
        "stackit",
        "submodel",
        "synthetic",
        "togetherai",
        "upstage",
        "v0",
        "venice",
        "vercel",
        "vivgrid",
        "vultr",
        "wandb",
        "xai",
        "xiaomi",
        "zai",
        "zai-coding-plan",
        "zenmux",
        "zhipuai",
        "zhipuai-coding-plan",
    ];

    let mut actual: Vec<&str> = PROVIDER_METADATA.iter().map(|m| m.canonical_id).collect();
    actual.sort_unstable();

    // Compute diff for readable failure message
    let expected_set: HashSet<&str> = EXPECTED.iter().copied().collect();
    let actual_set: HashSet<&str> = actual.iter().copied().collect();
    let added: Vec<&&str> = actual_set.difference(&expected_set).collect();
    let removed: Vec<&&str> = expected_set.difference(&actual_set).collect();

    assert_eq!(
        actual.as_slice(),
        EXPECTED,
        "\n\nCanonical ID snapshot mismatch!\n  Added:   {added:?}\n  Removed: {removed:?}\n\n\
         Update the EXPECTED array in this test to match the current PROVIDER_METADATA.\n\
         Actual (copy-paste ready):\n{actual:#?}\n"
    );
}

/// Snapshot of alias -> `canonical_id` mappings. Catches silent alias
/// additions, removals, or re-assignments.
#[test]
fn alias_mapping_snapshot_is_current() {
    // ── Snapshot: alias → canonical_id (sorted by alias) ────────────────
    const EXPECTED_ALIASES: &[(&str, &str)] = &[
        ("antigravity", "google-antigravity"),
        ("atlas", "atlascloud"),
        ("atlas-cloud", "atlascloud"),
        ("azure", "azure-openai"),
        ("azure-cognitive-services", "azure-openai"),
        ("azure-openai-responses", "azure-openai"),
        ("azure_openai", "azure-openai"),
        ("bedrock", "amazon-bedrock"),
        ("chatgpt-codex", "openai-codex"),
        ("codex", "openai-codex"),
        ("copilot", "github-copilot"),
        ("cursor-agent", "cursor"),
        ("dashscope", "alibaba"),
        ("deep-infra", "deepinfra"),
        ("deep-seek", "deepseek"),
        ("fireworks-ai", "fireworks"),
        ("gemini", "google"),
        ("gemini-cli", "google-gemini-cli"),
        ("github-copilot-enterprise", "github-copilot"),
        ("gitlab-duo", "gitlab"),
        ("glm", "zhipuai"),
        ("google-vertex-anthropic", "google-vertex"),
        ("grok", "xai"),
        ("hf", "huggingface"),
        ("hugging-face", "huggingface"),
        ("kimi", "moonshotai"),
        ("kimi-code", "kimi-for-coding"),
        ("kimi-coding", "kimi-for-coding"),
        ("llama-cpp", "llamacpp"),
        ("llama-server", "llamacpp"),
        ("llama.cpp", "llamacpp"),
        ("lm-studio", "lmstudio"),
        ("mistral-rs", "mistralrs"),
        ("mistral.rs", "mistralrs"),
        ("mistralai", "mistral"),
        ("moonshot", "moonshotai"),
        ("nanogpt", "nano-gpt"),
        ("nim", "nvidia"),
        ("novita", "novita-ai"),
        ("nvidia-nim", "nvidia"),
        ("open-router", "openrouter"),
        ("pplx", "perplexity"),
        ("qwen", "alibaba"),
        ("sap", "sap-ai-core"),
        ("silicon-flow", "siliconflow"),
        ("together", "togetherai"),
        ("together-ai", "togetherai"),
        ("vercel-ai-gateway", "vercel"),
        ("vertexai", "google-vertex"),
        ("x-ai", "xai"),
        ("zhipu", "zhipuai"),
    ];

    let mut actual: Vec<(&str, &str)> = Vec::new();
    for meta in PROVIDER_METADATA {
        for alias in meta.aliases {
            actual.push((alias, meta.canonical_id));
        }
    }
    actual.sort_by_key(|(alias, _)| *alias);

    assert_eq!(
        actual.len(),
        EXPECTED_ALIASES.len(),
        "Alias count mismatch: expected {}, got {}.\nActual:\n{actual:#?}",
        EXPECTED_ALIASES.len(),
        actual.len()
    );

    for (i, (actual_pair, expected_pair)) in actual.iter().zip(EXPECTED_ALIASES.iter()).enumerate()
    {
        assert_eq!(
            actual_pair, expected_pair,
            "Alias mapping mismatch at index {i}: actual {actual_pair:?} != expected {expected_pair:?}\n\
             Full actual list:\n{actual:#?}"
        );
    }
}

/// Snapshot of base URLs for key providers. Catches silent endpoint
/// changes that could break user configurations.
#[test]
fn base_url_snapshot_for_key_providers() {
    // ── Snapshot: canonical_id → base_url for providers where URL ────────
    // stability matters most (gap providers + established).
    const URL_SNAPSHOT: &[(&str, &str)] = &[
        (
            "alibaba",
            "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        ),
        (
            "alibaba-cn",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
        ),
        (
            "alibaba-us",
            "https://dashscope-us.aliyuncs.com/compatible-mode/v1",
        ),
        ("anthropic", "https://api.anthropic.com/v1/messages"),
        ("cerebras", "https://api.cerebras.ai/v1"),
        ("deepinfra", "https://api.deepinfra.com/v1/openai"),
        ("deepseek", "https://api.deepseek.com"),
        ("groq", "https://api.groq.com/openai/v1"),
        ("mistral", "https://api.mistral.ai/v1"),
        ("moonshotai", "https://api.moonshot.ai/v1"),
        ("moonshotai-cn", "https://api.moonshot.cn/v1"),
        ("openai", "https://api.openai.com/v1"),
        ("openrouter", "https://openrouter.ai/api/v1"),
        ("togetherai", "https://api.together.xyz/v1"),
    ];

    let mut failures = Vec::new();
    for (provider, expected_url) in URL_SNAPSHOT {
        let defaults = provider_routing_defaults(provider);
        match defaults {
            Some(d) if d.base_url == *expected_url => {} // OK
            Some(d) => {
                failures.push(format!(
                    "  {provider}: expected '{expected_url}', got '{}'",
                    d.base_url
                ));
            }
            None => {
                failures.push(format!(
                    "  {provider}: no routing_defaults (expected url '{expected_url}')"
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "\n\nBase URL snapshot mismatch!\n{}\n\n\
         If URLs changed intentionally, update URL_SNAPSHOT in this test.\n",
        failures.join("\n")
    );
}

/// Validates that core providers (established + `wave_b1`) have at least
/// a `simple_text` VCR fixture. Catches new provider entries that lack
/// basic test coverage.
#[test]
fn vcr_fixture_coverage_for_core_providers() {
    // Providers that must have VCR fixtures. Uses the VCR naming
    // convention (which may use aliases like "gemini", "copilot").
    const CORE_VCR_PROVIDERS: &[&str] = &[
        "alibaba",
        "anthropic",
        "cerebras",
        "cohere",
        "deepinfra",
        "gemini", // alias for google
        "groq",
        "huggingface",
        "mistral",
        "moonshotai",
        "nvidia",
        "ollama-cloud",
        "openai",
        "openrouter",
        "stackit",
        "togetherai",
    ];

    let fixture_dir = format!("{}/tests/fixtures/vcr", env!("CARGO_MANIFEST_DIR"));
    let mut missing = Vec::new();
    for provider in CORE_VCR_PROVIDERS {
        let simple_text = format!("{fixture_dir}/verify_{provider}_simple_text.json");
        if !std::path::Path::new(&simple_text).exists() {
            missing.push(*provider);
        }
    }

    assert!(
        missing.is_empty(),
        "\n\nCore providers missing VCR simple_text fixture: {missing:?}\n\
         Add fixtures at tests/fixtures/vcr/verify_<provider>_simple_text.json\n"
    );
}

/// Validates that gap providers (OAI-compatible with specific setup
/// documentation) have corresponding setup docs in docs/.
#[test]
fn gap_providers_have_setup_documentation() {
    // Gap providers that should have dedicated setup docs.
    const GAP_PROVIDERS_WITH_DOCS: &[(&str, &str)] = &[
        ("cerebras", "docs/provider-cerebras-setup.json"),
        ("groq", "docs/provider-groq-setup.json"),
        ("moonshotai", "docs/provider-kimi-setup.json"),
        ("alibaba", "docs/provider-qwen-setup.json"),
        ("alibaba-us", "docs/provider-qwen-setup.json"),
        ("openrouter", "docs/provider-openrouter-setup.json"),
    ];

    let root = env!("CARGO_MANIFEST_DIR");
    let mut missing = Vec::new();
    for (provider, doc_path) in GAP_PROVIDERS_WITH_DOCS {
        let full_path = format!("{root}/{doc_path}");
        if !std::path::Path::new(&full_path).exists() {
            missing.push(format!("  {provider}: {doc_path}"));
        }
    }

    assert!(
        missing.is_empty(),
        "\n\nGap providers missing setup documentation:\n{}\n",
        missing.join("\n")
    );
}

/// Validates that no two providers share identical (api, `base_url`)
/// routing defaults unless they are intentional pairs (e.g. minimax
/// and minimax-coding-plan). Catches copy-paste errors in new entries.
#[test]
fn no_accidental_duplicate_routing_defaults() {
    let mut seen: HashMap<(&str, &str), Vec<&str>> = HashMap::new();
    for meta in PROVIDER_METADATA {
        if let Some(defaults) = &meta.routing_defaults {
            seen.entry((defaults.api, defaults.base_url))
                .or_default()
                .push(meta.canonical_id);
        }
    }

    // Known intentional duplicates: coding-plan variants share base_url
    // with their parent, and mistral.rs deliberately defaults to LM Studio's
    // localhost port (both are local OpenAI-compatible servers on
    // 127.0.0.1:1234).
    let intentional_pairs: HashSet<&str> = [
        "minimax-coding-plan",
        "minimax-cn-coding-plan",
        "zai-coding-plan",
        "zhipuai-coding-plan",
        "mistralrs",
    ]
    .iter()
    .copied()
    .collect();

    let mut violations = Vec::new();
    for ((api, url), providers) in &seen {
        if providers.len() > 1 {
            // If more than one non-intentional provider shares the
            // same (api, base_url), flag it.
            if providers
                .iter()
                .filter(|p| !intentional_pairs.contains(**p))
                .count()
                > 1
            {
                violations.push(format!("  ({api}, {url}): {providers:?}"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "\n\nProviders sharing identical (api, base_url) routing defaults:\n{}\n\n\
         If intentional, add the new provider to intentional_pairs in this test.\n",
        violations.join("\n")
    );
}
