//! **Live E2E integration tests** — hit real provider APIs.
//!
//! These tests are gated behind `PI_E2E_TESTS=1` (or `CI_E2E_TESTS=1`) so they
//! never run in normal
//! `cargo test`.  They exercise the full streaming pipeline using real API keys
//! from `~/.pi/agent/models.json`.
//!
//! # Running
//!
//! ```bash
//! PI_E2E_TESTS=1 cargo test e2e_live -- --nocapture
//! CI_E2E_TESTS=1 cargo test e2e_live::azure_openai -- --nocapture # CI lane
//! ```
//!
//! # Cost control
//!
//! Every prompt is deliberately tiny ("Say just the word hello") so each call
//! uses ≈20–50 tokens.  Estimated total cost for running the full suite once
//! against all seven providers: < $0.01.

mod common;

use common::TestHarness;
use futures::StreamExt;
use pi::auth::AuthStorage;
use pi::config::Config;
use pi::model::{Message, StopReason, StreamEvent, UserContent, UserMessage};
use pi::models::{ModelEntry, ModelRegistry, default_models_path};
use pi::provider::{Context, Provider, StreamOptions};
use pi::provider_metadata::provider_auth_env_keys;
use pi::providers::anthropic::AnthropicProvider;
use pi::providers::azure::AzureOpenAIProvider;
use pi::providers::gemini::GeminiProvider;
use pi::providers::openai::OpenAIProvider;
use pi::providers::openai_responses::OpenAIResponsesProvider;
use pi::providers::{normalize_openai_base, normalize_openai_responses_base};
use serde::Serialize;
use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;
use url::Url;

// ---------------------------------------------------------------------------
// Gate: skip entire module unless PI_E2E_TESTS=1 or CI_E2E_TESTS=1
// ---------------------------------------------------------------------------

fn e2e_enabled() -> bool {
    fn enabled_from(name: &str) -> bool {
        env::var(name)
            .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
    }

    enabled_from("PI_E2E_TESTS") || enabled_from("CI_E2E_TESTS")
}

macro_rules! skip_unless_e2e {
    () => {
        if !e2e_enabled() {
            eprintln!("SKIPPED (set PI_E2E_TESTS=1 or CI_E2E_TESTS=1 to run)");
            return;
        }
    };
}

const LIVE_PROVIDER_ORDER: [&str; 7] = [
    "anthropic",
    "openai",
    "azure-openai",
    "google",
    "openrouter",
    "xai",
    "deepseek",
];

#[derive(Debug, Clone)]
struct LiveProviderConfig {
    provider: String,
    model_id: String,
    api: String,
    base_url: String,
    api_key: String,
    auth_source: String,
}

#[derive(Debug, Clone, Serialize)]
struct ProviderDiscoveryRow {
    provider: String,
    model_id: Option<String>,
    api: Option<String>,
    base_url: Option<String>,
    auth_source: String,
    required_fields: Vec<String>,
    enabled: bool,
    disabled_reason: Option<String>,
}

#[derive(Debug, Clone)]
struct ProviderDiscovery {
    models_path: PathBuf,
    auth_path: PathBuf,
    rows: Vec<ProviderDiscoveryRow>,
    configs: BTreeMap<String, LiveProviderConfig>,
}

static DISCOVERY_CACHE: OnceLock<Result<ProviderDiscovery, String>> = OnceLock::new();

fn provider_discovery() -> Result<ProviderDiscovery, String> {
    DISCOVERY_CACHE.get_or_init(load_provider_discovery).clone()
}

#[allow(clippy::too_many_lines)]
fn load_provider_discovery() -> Result<ProviderDiscovery, String> {
    let agent_dir = Config::global_dir();
    let models_path = default_models_path(&agent_dir);
    if !models_path.exists() {
        return Err(format!(
            "models.json is required for live E2E discovery but was not found at {}",
            models_path.display()
        ));
    }

    let models_content = std::fs::read_to_string(&models_path).map_err(|err| {
        format!(
            "failed to read models.json at {}: {err}",
            models_path.display()
        )
    })?;
    let models_json: serde_json::Value = serde_json::from_str(&models_content)
        .map_err(|err| format!("invalid models.json at {}: {err}", models_path.display()))?;
    let providers_obj = models_json
        .get("providers")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            format!(
                "models.json at {} must contain a top-level `providers` object",
                models_path.display()
            )
        })?;

    let auth_path = Config::auth_path();
    let auth = AuthStorage::load(auth_path.clone())
        .map_err(|err| format!("failed to load auth store {}: {err}", auth_path.display()))?;
    let registry = ModelRegistry::load(&auth, Some(models_path.clone()));
    if let Some(err) = registry.error() {
        return Err(format!(
            "failed to load model registry from {}: {err}",
            models_path.display()
        ));
    }

    let mut rows = Vec::new();
    let mut configs = BTreeMap::new();
    let mut provider_ids: Vec<String> = providers_obj.keys().cloned().collect();
    provider_ids.sort_unstable();

    for provider_id in provider_ids {
        let selection = select_model_entry_for_provider(&registry, &provider_id);
        let required_fields = match &selection {
            ModelSelection::Selected(entry) => {
                required_fields_for_provider_api(&provider_id, &entry.model.api)
            }
            ModelSelection::RequestedModelMissing(_) | ModelSelection::NoModels => Vec::new(),
        };

        let row = match selection {
            ModelSelection::Selected(entry) => {
                let (api_key, auth_source) =
                    resolve_api_key_with_source(&auth, &provider_id, &entry);
                if let Some(key) = api_key {
                    let config = LiveProviderConfig {
                        provider: provider_id.clone(),
                        model_id: entry.model.id.clone(),
                        api: entry.model.api.clone(),
                        base_url: entry.model.base_url.clone(),
                        api_key: key,
                        auth_source: auth_source.clone(),
                    };
                    configs.insert(provider_id.clone(), config);
                    ProviderDiscoveryRow {
                        provider: provider_id,
                        model_id: Some(entry.model.id),
                        api: Some(entry.model.api),
                        base_url: Some(entry.model.base_url),
                        auth_source,
                        required_fields,
                        enabled: true,
                        disabled_reason: None,
                    }
                } else {
                    let env_hint = provider_env_var_names(&provider_id).join(" or ");
                    ProviderDiscoveryRow {
                        provider: provider_id,
                        model_id: Some(entry.model.id),
                        api: Some(entry.model.api),
                        base_url: Some(entry.model.base_url),
                        auth_source: "missing".to_string(),
                        required_fields,
                        enabled: false,
                        disabled_reason: Some(format!(
                            "no credentials found in env/auth/models (set {env_hint} or configure auth/models)"
                        )),
                    }
                }
            }
            ModelSelection::RequestedModelMissing(requested) => ProviderDiscoveryRow {
                provider: provider_id,
                model_id: Some(requested.clone()),
                api: None,
                base_url: None,
                auth_source: "unknown".to_string(),
                required_fields,
                enabled: false,
                disabled_reason: Some(format!(
                    "requested model override '{requested}' not found for this provider"
                )),
            },
            ModelSelection::NoModels => ProviderDiscoveryRow {
                provider: provider_id,
                model_id: None,
                api: None,
                base_url: None,
                auth_source: "missing".to_string(),
                required_fields,
                enabled: false,
                disabled_reason: Some("no models registered for provider".to_string()),
            },
        };

        rows.push(row);
    }

    Ok(ProviderDiscovery {
        models_path,
        auth_path,
        rows,
        configs,
    })
}

fn write_discovery_artifacts(
    harness: &TestHarness,
    discovery: &ProviderDiscovery,
) -> std::io::Result<()> {
    let json_path = harness.temp_path("e2e_live_provider_discovery.json");
    let json_content =
        serde_json::to_string_pretty(&discovery.rows).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(&json_path, json_content)?;
    harness.record_artifact("e2e_live_provider_discovery.json", &json_path);

    let mut markdown = String::new();
    markdown.push_str("# Live E2E Provider Discovery\n\n");
    let _ = write!(
        markdown,
        "- models: `{}`\n- auth: `{}`\n\n",
        discovery.models_path.display(),
        discovery.auth_path.display()
    );
    markdown.push_str("| provider | model | api | auth_source | enabled | reason |\n");
    markdown.push_str("| --- | --- | --- | --- | --- | --- |\n");
    for row in &discovery.rows {
        let model = row.model_id.as_deref().unwrap_or("-");
        let api = row.api.as_deref().unwrap_or("-");
        let reason = row.disabled_reason.as_deref().unwrap_or("-");
        let _ = writeln!(
            markdown,
            "| {} | {} | {} | {} | {} | {} |",
            row.provider,
            model,
            api,
            row.auth_source,
            if row.enabled { "yes" } else { "no" },
            reason.replace('|', "\\|")
        );
    }
    let markdown_path = harness.temp_path("e2e_live_provider_discovery.md");
    std::fs::write(&markdown_path, markdown)?;
    harness.record_artifact("e2e_live_provider_discovery.md", &markdown_path);
    Ok(())
}

fn provider_config_or_skip(provider: &str, harness: &TestHarness) -> Option<LiveProviderConfig> {
    let discovery = match provider_discovery() {
        Ok(discovery) => discovery,
        Err(err) => panic!("live provider discovery failed: {err}"),
    };
    if let Err(err) = write_discovery_artifacts(harness, &discovery) {
        panic!("failed to write discovery artifacts: {err}");
    }

    let Some(row) = discovery.rows.iter().find(|row| row.provider == provider) else {
        eprintln!("SKIPPED: provider '{provider}' not present in models.json");
        return None;
    };
    if !row.enabled {
        eprintln!(
            "SKIPPED: provider '{provider}' disabled ({})",
            row.disabled_reason
                .as_deref()
                .unwrap_or("unknown discovery reason")
        );
        return None;
    }

    let config = discovery.configs.get(provider).cloned()?;
    if is_azure_provider(provider)
        && let Err(reason) = resolve_azure_runtime_config(&config)
    {
        eprintln!("SKIPPED: provider '{provider}' disabled ({reason})");
        return None;
    }
    Some(config)
}

fn is_azure_provider(provider: &str) -> bool {
    matches!(
        provider,
        "azure-openai" | "azure" | "azure-cognitive-services"
    )
}

fn required_fields_for_provider_api(provider: &str, api: &str) -> Vec<String> {
    if is_azure_provider(provider) {
        return vec![
            "model_id".to_string(),
            "api_key".to_string(),
            "api".to_string(),
            "base_url".to_string(),
            "AZURE_OPENAI_RESOURCE".to_string(),
            "AZURE_OPENAI_DEPLOYMENT".to_string(),
        ];
    }

    match api {
        "anthropic-messages"
        | "google-generative-ai"
        | "openai-completions"
        | "openai-responses" => vec![
            "model_id".to_string(),
            "api_key".to_string(),
            "api".to_string(),
            "base_url".to_string(),
        ],
        _ => vec!["model_id".to_string(), "api_key".to_string()],
    }
}

fn provider_env_var_names(provider: &str) -> &'static [&'static str] {
    provider_auth_env_keys(provider)
}

#[test]
fn provider_env_var_names_uses_canonical_metadata_for_oai_compat() {
    assert_eq!(
        provider_env_var_names("openrouter"),
        &["OPENROUTER_API_KEY"]
    );
    assert_eq!(provider_env_var_names("xai"), &["XAI_API_KEY"]);
    assert_eq!(provider_env_var_names("deepseek"), &["DEEPSEEK_API_KEY"]);
    assert_eq!(
        provider_env_var_names("dashscope"),
        &["DASHSCOPE_API_KEY", "QWEN_API_KEY"]
    );
    assert_eq!(
        provider_env_var_names("kimi"),
        &["MOONSHOT_API_KEY", "KIMI_API_KEY"]
    );
}

#[test]
fn provider_env_var_names_support_azure_cognitive_alias() {
    assert_eq!(provider_env_var_names("azure"), &["AZURE_OPENAI_API_KEY"]);
    assert_eq!(
        provider_env_var_names("azure-cognitive-services"),
        &["AZURE_OPENAI_API_KEY"]
    );
}

#[test]
fn provider_env_var_names_support_cloudflare_ids() {
    assert_eq!(
        provider_env_var_names("cloudflare-ai-gateway"),
        &["CLOUDFLARE_API_TOKEN"]
    );
    assert_eq!(
        provider_env_var_names("cloudflare-workers-ai"),
        &["CLOUDFLARE_API_TOKEN"]
    );
}

#[test]
fn oai_auth_failure_script_matrix_maps_to_taxonomy() {
    let cases = [
        (
            "openrouter",
            "You didn't provide an API key in the Authorization header",
            pi::error::AuthDiagnosticCode::MissingApiKey,
        ),
        (
            "xai",
            "Malformed API key: expected Bearer token format",
            pi::error::AuthDiagnosticCode::InvalidApiKey,
        ),
        (
            "deepseek",
            "API key revoked for this project",
            pi::error::AuthDiagnosticCode::InvalidApiKey,
        ),
        (
            "openai",
            "HTTP 429 insufficient_quota: You exceeded your current quota",
            pi::error::AuthDiagnosticCode::QuotaExceeded,
        ),
    ];

    for (provider, message, expected_code) in cases {
        let err = pi::Error::provider(provider, message);
        let diagnostic = err
            .auth_diagnostic()
            .unwrap_or_else(|| panic!("expected auth diagnostic for {provider}: {message}"));
        assert_eq!(diagnostic.code, expected_code, "provider {provider}");

        let hints = err.hints();
        assert!(
            hints
                .context
                .iter()
                .any(|(key, value)| key == "provider" && value == provider),
            "provider context missing for {provider}"
        );
        assert!(
            hints
                .context
                .iter()
                .any(|(key, _)| key == "diagnostic_code"),
            "diagnostic_code context missing for {provider}"
        );
    }
}

fn resolve_api_key_with_source(
    auth: &AuthStorage,
    provider: &str,
    entry: &ModelEntry,
) -> (Option<String>, String) {
    for env_var in provider_env_var_names(provider) {
        if let Ok(value) = env::var(env_var)
            && !value.trim().is_empty()
        {
            return (Some(value), format!("env:{env_var}"));
        }
    }

    if let Some(value) = auth.api_key(provider) {
        return (Some(value), "auth_store".to_string());
    }

    if let Some(value) = entry.api_key.clone()
        && !value.trim().is_empty()
    {
        return (Some(value), "models_json".to_string());
    }

    (None, "missing".to_string())
}

fn provider_model_override_var(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_TEST_MODEL"),
        "openai" => Some("OPENAI_TEST_MODEL"),
        "azure-openai" | "azure" | "azure-cognitive-services" => Some("AZURE_OPENAI_DEPLOYMENT"),
        "google" => Some("GOOGLE_TEST_MODEL"),
        "openrouter" => Some("OPENROUTER_TEST_MODEL"),
        "xai" => Some("XAI_TEST_MODEL"),
        "deepseek" => Some("DEEPSEEK_TEST_MODEL"),
        _ => None,
    }
}

fn provider_preferred_models(provider: &str) -> &'static [&'static str] {
    match provider {
        "anthropic" => &[
            "claude-haiku-4-5",
            "claude-3-5-haiku-20241022",
            "claude-sonnet-4-5",
        ],
        "openai" => &["gpt-4o-mini", "gpt-4o", "gpt-5.1-codex"],
        "google" => &["gemini-2.0-flash", "gemini-2.5-flash", "gemini-1.5-flash"],
        "openrouter" => &["anthropic/claude-sonnet-4", "deepseek/deepseek-chat"],
        "xai" => &["grok-3-mini", "grok-2-1212"],
        "deepseek" => &["deepseek-chat", "deepseek-coder"],
        _ => &[],
    }
}

enum ModelSelection {
    Selected(Box<ModelEntry>),
    RequestedModelMissing(String),
    NoModels,
}

type StreamingSummaryRow = (String, String, bool, bool, bool, u64, u64, usize, u128);

fn select_model_entry_for_provider(registry: &ModelRegistry, provider: &str) -> ModelSelection {
    if let Some(override_var) = provider_model_override_var(provider)
        && let Ok(override_model) = env::var(override_var)
    {
        let override_model = override_model.trim().to_string();
        if !override_model.is_empty() {
            return registry.find(provider, &override_model).map_or(
                ModelSelection::RequestedModelMissing(override_model),
                |entry| ModelSelection::Selected(Box::new(entry)),
            );
        }
    }

    for preferred in provider_preferred_models(provider) {
        if let Some(entry) = registry.find(provider, preferred) {
            return ModelSelection::Selected(Box::new(entry));
        }
    }

    registry
        .models()
        .iter()
        .find(|entry| entry.model.provider == provider)
        .cloned()
        .map_or(ModelSelection::NoModels, |entry| {
            ModelSelection::Selected(Box::new(entry))
        })
}

fn build_provider(config: &LiveProviderConfig) -> Box<dyn Provider> {
    match config.api.as_str() {
        "openai-completions" if is_azure_provider(&config.provider) => {
            let runtime = resolve_azure_runtime_config(config).unwrap_or_else(|err| {
                panic!(
                    "azure provider '{}' config resolution failed for live e2e: {err}",
                    config.provider
                )
            });
            let mut provider = AzureOpenAIProvider::new(runtime.resource, runtime.deployment);
            if let Some(api_version) = runtime.api_version {
                provider = provider.with_api_version(api_version);
            }
            Box::new(provider)
        }
        "anthropic-messages" => Box::new(
            AnthropicProvider::new(config.model_id.clone()).with_base_url(config.base_url.clone()),
        ),
        "google-generative-ai" => Box::new(
            GeminiProvider::new(config.model_id.clone()).with_base_url(config.base_url.clone()),
        ),
        "openai-responses" => Box::new(
            OpenAIResponsesProvider::new(config.model_id.clone())
                .with_provider_name(config.provider.clone())
                .with_base_url(normalize_openai_responses_base(&config.base_url)),
        ),
        "openai-completions" => Box::new(
            OpenAIProvider::new(config.model_id.clone())
                .with_provider_name(config.provider.clone())
                .with_base_url(normalize_openai_base(&config.base_url)),
        ),
        other => panic!(
            "unsupported API '{}' for provider '{}' model '{}'",
            other, config.provider, config.model_id
        ),
    }
}

#[derive(Debug, Clone)]
struct AzureRuntimeConfig {
    resource: String,
    deployment: String,
    api_version: Option<String>,
}

fn optional_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_azure_base_url_details(
    base_url: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    let Ok(url) = Url::parse(base_url) else {
        return (None, None, None);
    };

    let resource = url
        .host_str()
        .and_then(|host| {
            host.strip_suffix(".openai.azure.com")
                .or_else(|| host.strip_suffix(".cognitiveservices.azure.com"))
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);

    let mut deployment = None;
    if let Some(segments) = url.path_segments() {
        let mut iter = segments;
        while let Some(segment) = iter.next() {
            if segment == "deployments" {
                deployment = iter
                    .next()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string);
                break;
            }
        }
    }

    let api_version = url
        .query_pairs()
        .find(|(key, _)| key == "api-version")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.trim().is_empty());

    (resource, deployment, api_version)
}

fn resolve_azure_runtime_config(config: &LiveProviderConfig) -> Result<AzureRuntimeConfig, String> {
    resolve_azure_runtime_config_with_lookup(config, optional_env)
}

fn resolve_azure_runtime_config_with_lookup<F>(
    config: &LiveProviderConfig,
    mut env_lookup: F,
) -> Result<AzureRuntimeConfig, String>
where
    F: FnMut(&str) -> Option<String>,
{
    let (base_resource, base_deployment, base_api_version) =
        parse_azure_base_url_details(&config.base_url);

    let resource = env_lookup("AZURE_OPENAI_RESOURCE")
        .or(base_resource)
        .ok_or_else(|| {
            format!(
                "missing Azure resource: set AZURE_OPENAI_RESOURCE or use a parseable base_url (got '{}')",
                config.base_url
            )
        })?;

    let deployment = env_lookup("AZURE_OPENAI_DEPLOYMENT")
        .or(base_deployment)
        .or_else(|| {
            let deployment = config.model_id.trim();
            (!deployment.is_empty()).then(|| deployment.to_string())
        })
        .ok_or_else(|| {
            format!(
                "missing Azure deployment: set AZURE_OPENAI_DEPLOYMENT or configure model_id/base_url (model_id='{}', base_url='{}')",
                config.model_id, config.base_url
            )
        })?;

    let api_version = env_lookup("AZURE_OPENAI_API_VERSION").or(base_api_version);

    Ok(AzureRuntimeConfig {
        resource,
        deployment,
        api_version,
    })
}

#[test]
fn parse_azure_base_url_details_supports_cognitive_services_host() {
    let (resource, deployment, api_version) = parse_azure_base_url_details(
        "https://myresource.cognitiveservices.azure.com/openai/deployments/deploy-123/chat/completions?api-version=2024-10-21",
    );
    assert_eq!(resource.as_deref(), Some("myresource"));
    assert_eq!(deployment.as_deref(), Some("deploy-123"));
    assert_eq!(api_version.as_deref(), Some("2024-10-21"));
}

#[test]
fn resolve_azure_runtime_config_prefers_base_url_deployment_over_model_id() {
    let config = LiveProviderConfig {
        provider: "azure-openai".to_string(),
        model_id: "model-fallback".to_string(),
        api: "openai-completions".to_string(),
        base_url: "https://myresource.openai.azure.com/openai/deployments/base-deploy/chat/completions?api-version=2024-10-21".to_string(),
        api_key: "test-key".to_string(),
        auth_source: "env:AZURE_OPENAI_API_KEY".to_string(),
    };

    let runtime = resolve_azure_runtime_config(&config).expect("resolve azure runtime config");
    assert_eq!(runtime.resource, "myresource");
    assert_eq!(runtime.deployment, "base-deploy");
    assert_eq!(runtime.api_version.as_deref(), Some("2024-10-21"));
}

#[test]
fn resolve_azure_runtime_config_env_deployment_overrides_base_url_and_model_id() {
    let config = LiveProviderConfig {
        provider: "azure-openai".to_string(),
        model_id: "model-fallback".to_string(),
        api: "openai-completions".to_string(),
        base_url: "https://myresource.openai.azure.com/openai/deployments/base-deploy/chat/completions?api-version=2024-10-21".to_string(),
        api_key: "test-key".to_string(),
        auth_source: "env:AZURE_OPENAI_API_KEY".to_string(),
    };

    let runtime = resolve_azure_runtime_config_with_lookup(&config, |name| match name {
        "AZURE_OPENAI_DEPLOYMENT" => Some("env-deploy".to_string()),
        _ => None,
    })
    .expect("resolve azure runtime config");
    assert_eq!(runtime.resource, "myresource");
    assert_eq!(runtime.deployment, "env-deploy");
    assert_eq!(runtime.api_version.as_deref(), Some("2024-10-21"));
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn user_text(text: &str) -> Message {
    Message::User(UserMessage {
        content: UserContent::Text(text.to_string()),
        timestamp: 0,
    })
}

fn simple_context(prompt: &str) -> Context<'static> {
    Context::owned(
        Some("You are a test harness. Respond concisely.".to_string()),
        vec![user_text(prompt)],
        vec![],
    )
}

/// Build stream options with API key set via `api_key` field.
/// Providers read this field and construct the appropriate auth header themselves.
fn simple_options(api_key: &str) -> StreamOptions {
    StreamOptions {
        api_key: Some(api_key.to_string()),
        max_tokens: Some(64),
        temperature: Some(0.0),
        ..Default::default()
    }
}

/// Collect all stream events from a provider, logging each one.
async fn collect_stream(
    provider: &dyn Provider,
    context: &Context<'_>,
    options: &StreamOptions,
    harness: &TestHarness,
) -> (Vec<StreamEvent>, Option<String>) {
    let start = Instant::now();
    harness
        .log()
        .info_ctx("stream", "Starting provider stream", |ctx| {
            ctx.push(("provider".into(), provider.name().to_string()));
            ctx.push(("model".into(), provider.model_id().to_string()));
            ctx.push(("api".into(), provider.api().to_string()));
        });

    let stream_result = provider.stream(context, options).await;
    let elapsed_connect = start.elapsed();

    harness.log().info_ctx("stream", "Stream opened", |ctx| {
        ctx.push((
            "connect_ms".into(),
            format!("{}", elapsed_connect.as_millis()),
        ));
        ctx.push(("ok".into(), format!("{}", stream_result.is_ok())));
    });

    let mut stream = match stream_result {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("{e}");
            harness
                .log()
                .error("stream", format!("Stream error: {msg}"));
            return (vec![], Some(msg));
        }
    };

    let mut events = Vec::new();
    let mut text_accum = String::new();
    let mut stream_error = None;
    let mut event_count = 0u32;

    while let Some(item) = stream.next().await {
        event_count += 1;
        match item {
            Ok(event) => {
                match &event {
                    StreamEvent::TextDelta { delta, .. } => text_accum.push_str(delta),
                    StreamEvent::Done { reason, message } => {
                        harness.log().info_ctx("stream", "Stream done", |ctx| {
                            ctx.push(("stop_reason".into(), format!("{reason:?}")));
                            ctx.push(("input_tokens".into(), format!("{}", message.usage.input)));
                            ctx.push(("output_tokens".into(), format!("{}", message.usage.output)));
                        });
                    }
                    _ => {}
                }
                events.push(event);
            }
            Err(e) => {
                stream_error = Some(format!("{e}"));
                harness.log().error("stream", format!("Event error: {e}"));
                break;
            }
        }
    }

    let elapsed_total = start.elapsed();
    harness.log().info_ctx("stream", "Stream complete", |ctx| {
        ctx.push(("total_ms".into(), format!("{}", elapsed_total.as_millis())));
        ctx.push(("event_count".into(), format!("{event_count}")));
        ctx.push(("text_length".into(), format!("{}", text_accum.len())));
        ctx.push((
            "text_preview".into(),
            text_accum.chars().take(200).collect::<String>(),
        ));
    });

    (events, stream_error)
}

/// Assert basic streaming success: got events, no error, non-empty text.
fn assert_basic_stream_success(
    events: &[StreamEvent],
    stream_error: Option<&str>,
    harness: &TestHarness,
    test_name: &str,
) {
    if let Some(err) = stream_error {
        harness
            .log()
            .error("assert", format!("{test_name}: stream error: {err}"));
        panic!("{test_name}: unexpected stream error: {err}");
    }

    assert!(
        !events.is_empty(),
        "{test_name}: expected at least one event"
    );

    // Accept TextDelta, TextEnd, or ThinkingDelta as "content" events.
    // Some providers (e.g. Gemini 2.5 Flash) deliver all text in the first
    // SSE chunk, which the Gemini provider emits as a Start event.  In that
    // case there are zero TextDelta events, but the Done message still
    // contains the accumulated text.  So we also accept non-empty text in the
    // Done message as proof of content.
    let has_delta_content = events.iter().any(|e| {
        matches!(
            e,
            StreamEvent::TextDelta { .. }
                | StreamEvent::TextEnd { .. }
                | StreamEvent::ThinkingDelta { .. }
        )
    });
    let done_has_text = events.iter().any(|e| match e {
        StreamEvent::Done { message, .. } => message
            .content
            .iter()
            .any(|c| matches!(c, pi::model::ContentBlock::Text(tc) if !tc.text.is_empty())),
        _ => false,
    });
    assert!(
        has_delta_content || done_has_text,
        "{test_name}: expected content events or non-empty text in Done message"
    );

    let has_done = events.iter().any(|e| matches!(e, StreamEvent::Done { .. }));
    assert!(has_done, "{test_name}: expected a Done event");

    harness.log().info("assert", format!("{test_name}: PASSED"));
}

// ---------------------------------------------------------------------------
// Anthropic E2E Tests
// ---------------------------------------------------------------------------

mod anthropic {
    use super::*;

    #[test]
    fn basic_message() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_anthropic_basic_message");
        let Some(config) = provider_config_or_skip("anthropic", &harness) else {
            return;
        };
        harness
            .log()
            .info_ctx("discovery", "Using resolved provider config", |ctx| {
                ctx.push(("provider".into(), config.provider.clone()));
                ctx.push(("model".into(), config.model_id.clone()));
                ctx.push(("api".into(), config.api.clone()));
                ctx.push(("auth_source".into(), config.auth_source.clone()));
            });

        common::run_async(async move {
            let provider = build_provider(&config);
            let context = simple_context("Say just the word hello");
            let options = simple_options(&config.api_key);

            let (events, error) =
                collect_stream(provider.as_ref(), &context, &options, &harness).await;
            assert_basic_stream_success(&events, error.as_deref(), &harness, "anthropic_basic");

            // Verify text contains "hello" (case-insensitive)
            let text: String = events
                .iter()
                .filter_map(|e| match e {
                    StreamEvent::TextDelta { delta, .. } => Some(delta.as_str()),
                    _ => None,
                })
                .collect();
            assert!(
                text.to_lowercase().contains("hello"),
                "Expected 'hello' in response, got: {text}"
            );
        });
    }

    #[test]
    fn streaming_event_order() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_anthropic_streaming_order");
        let Some(config) = provider_config_or_skip("anthropic", &harness) else {
            return;
        };

        common::run_async(async move {
            let provider = build_provider(&config);
            let context = simple_context("Say just the word hello");
            let options = simple_options(&config.api_key);

            let (events, error) =
                collect_stream(provider.as_ref(), &context, &options, &harness).await;
            assert_basic_stream_success(&events, error.as_deref(), &harness, "anthropic_order");

            // Verify Start comes first
            assert!(
                matches!(events.first(), Some(StreamEvent::Start { .. })),
                "First event should be Start, got {:?}",
                events.first()
            );

            // Verify Done comes last
            assert!(
                matches!(events.last(), Some(StreamEvent::Done { .. })),
                "Last event should be Done, got {:?}",
                events.last()
            );

            // Count text deltas
            let text_count = events
                .iter()
                .filter(|e| matches!(e, StreamEvent::TextDelta { .. }))
                .count();
            harness
                .log()
                .info("verify", format!("Text deltas: {text_count}"));
            assert!(text_count >= 1, "Expected at least 1 text delta");
        });
    }

    #[test]
    fn stop_reason_end_turn() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_anthropic_stop_reason");
        let Some(config) = provider_config_or_skip("anthropic", &harness) else {
            return;
        };

        common::run_async(async move {
            let provider = build_provider(&config);
            let context = simple_context("Say just the word hello");
            let options = simple_options(&config.api_key);

            let (events, _) = collect_stream(provider.as_ref(), &context, &options, &harness).await;

            let done = events.iter().find_map(|e| match e {
                StreamEvent::Done { reason, message } => Some((reason, message)),
                _ => None,
            });
            assert!(done.is_some(), "Expected Done event");
            let (reason, message) = done.unwrap();
            assert_eq!(*reason, StopReason::Stop, "Expected Stop reason");
            assert!(message.usage.input > 0, "Expected non-zero input tokens");
            assert!(message.usage.output > 0, "Expected non-zero output tokens");
        });
    }
}

// ---------------------------------------------------------------------------
// OpenAI E2E Tests (Responses API)
// ---------------------------------------------------------------------------

mod openai {
    use super::*;

    #[test]
    fn basic_message() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_openai_basic_message");
        let Some(config) = provider_config_or_skip("openai", &harness) else {
            return;
        };

        common::run_async(async move {
            let provider = build_provider(&config);
            let context = simple_context("Say just the word hello");
            // Use simple_options: provider reads api_key and builds Authorization header itself.
            let options = simple_options(&config.api_key);

            let (events, error) =
                collect_stream(provider.as_ref(), &context, &options, &harness).await;
            assert_basic_stream_success(&events, error.as_deref(), &harness, "openai_basic");
        });
    }

    #[test]
    fn streaming_events() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_openai_streaming");
        let Some(config) = provider_config_or_skip("openai", &harness) else {
            return;
        };

        common::run_async(async move {
            let provider = build_provider(&config);
            let context = simple_context("Count from 1 to 5, one number per line");
            let options = simple_options(&config.api_key);

            let (events, error) =
                collect_stream(provider.as_ref(), &context, &options, &harness).await;
            assert_basic_stream_success(&events, error.as_deref(), &harness, "openai_streaming");

            let text: String = events
                .iter()
                .filter_map(|e| match e {
                    StreamEvent::TextDelta { delta, .. } => Some(delta.as_str()),
                    _ => None,
                })
                .collect();
            // Should contain at least "1" and "5"
            assert!(text.contains('1'), "Expected '1' in response");
            assert!(text.contains('5'), "Expected '5' in response");
        });
    }
}

// ---------------------------------------------------------------------------
// Azure OpenAI E2E Tests
// ---------------------------------------------------------------------------

mod azure_openai {
    use super::*;

    fn azure_tool_context(prompt: &str) -> Context<'static> {
        Context::owned(
            Some("You are a test harness assistant. Use tools when explicitly asked.".to_string()),
            vec![user_text(prompt)],
            vec![pi::provider::ToolDef {
                name: "list_dir".to_string(),
                description: "List files in a directory".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            }],
        )
    }

    #[test]
    fn basic_message() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_azure_openai_basic_message");
        let Some(config) = provider_config_or_skip("azure-openai", &harness) else {
            return;
        };

        common::run_async(async move {
            let provider = build_provider(&config);
            let context = simple_context("Say just the word hello");
            let options = simple_options(&config.api_key);

            let (events, error) =
                collect_stream(provider.as_ref(), &context, &options, &harness).await;
            assert_basic_stream_success(&events, error.as_deref(), &harness, "azure_openai_basic");
        });
    }

    #[test]
    fn streaming_events() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_azure_openai_streaming");
        let Some(config) = provider_config_or_skip("azure-openai", &harness) else {
            return;
        };

        common::run_async(async move {
            let provider = build_provider(&config);
            let context = simple_context("Count from 1 to 5, one number per line");
            let options = simple_options(&config.api_key);

            let (events, error) =
                collect_stream(provider.as_ref(), &context, &options, &harness).await;
            assert_basic_stream_success(
                &events,
                error.as_deref(),
                &harness,
                "azure_openai_streaming",
            );

            assert!(
                matches!(events.first(), Some(StreamEvent::Start { .. })),
                "Azure stream should start with Start event"
            );
            assert!(
                matches!(events.last(), Some(StreamEvent::Done { .. })),
                "Azure stream should end with Done event"
            );
        });
    }

    #[test]
    fn tool_call_when_supported() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_azure_openai_tool_call");
        let Some(config) = provider_config_or_skip("azure-openai", &harness) else {
            return;
        };

        common::run_async(async move {
            let provider = build_provider(&config);
            let context = azure_tool_context(
                "Use the list_dir tool with path '.' before answering. Do not answer without a tool call.",
            );
            let options = simple_options(&config.api_key);

            let (events, error) =
                collect_stream(provider.as_ref(), &context, &options, &harness).await;
            assert_basic_stream_success(
                &events,
                error.as_deref(),
                &harness,
                "azure_openai_tool_call",
            );

            let tool_calls: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    StreamEvent::ToolCallEnd { tool_call, .. } => Some(tool_call.clone()),
                    _ => None,
                })
                .collect();

            if tool_calls.is_empty() {
                harness.log().warn(
                    "tool_use",
                    format!(
                        "Azure deployment '{}' did not emit tool calls; skipping tool-use assertion",
                        config.model_id
                    ),
                );
                eprintln!(
                    "SKIPPED: azure deployment '{}' did not emit tool calls in this run",
                    config.model_id
                );
                return;
            }

            assert!(
                tool_calls
                    .iter()
                    .all(|tool_call| !tool_call.name.is_empty()),
                "Expected non-empty tool names in tool call events"
            );
            assert!(
                tool_calls
                    .iter()
                    .all(|tool_call| tool_call.arguments.is_object()),
                "Expected tool call arguments to be JSON objects"
            );

            let done_reason = events.iter().find_map(|event| match event {
                StreamEvent::Done { reason, .. } => Some(*reason),
                _ => None,
            });
            assert!(
                matches!(done_reason, Some(StopReason::ToolUse | StopReason::Stop)),
                "Expected done reason to be ToolUse or Stop, got {done_reason:?}"
            );
        });
    }

    #[test]
    fn invalid_deployment_has_actionable_error() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_azure_openai_invalid_deployment");
        let Some(config) = provider_config_or_skip("azure-openai", &harness) else {
            return;
        };

        common::run_async(async move {
            let azure = resolve_azure_runtime_config(&config)
                .unwrap_or_else(|err| panic!("resolve azure runtime config: {err}"));
            let invalid_deployment = "__pi_e2e_invalid_deployment__";
            let provider = azure.api_version.as_ref().map_or_else(
                || AzureOpenAIProvider::new(azure.resource.clone(), invalid_deployment),
                |api_version| {
                    AzureOpenAIProvider::new(azure.resource.clone(), invalid_deployment)
                        .with_api_version(api_version.clone())
                },
            );

            let context = simple_context("Say just hello.");
            let options = simple_options(&config.api_key);

            let (events, error) = collect_stream(&provider, &context, &options, &harness).await;
            if let Some(err) = error {
                let err_lower = err.to_ascii_lowercase();
                assert!(
                    err_lower.contains("deployment")
                        || err_lower.contains("not found")
                        || err_lower.contains("404")
                        || err_lower.contains("invalid"),
                    "expected actionable deployment error, got: {err}"
                );
                return;
            }

            panic!(
                "expected invalid deployment to fail with actionable diagnostics, got {} events",
                events.len()
            );
        });
    }
}

// ---------------------------------------------------------------------------
// Google Gemini E2E Tests
// ---------------------------------------------------------------------------

mod gemini {
    use super::*;

    #[test]
    fn basic_message() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_gemini_basic_message");
        let Some(config) = provider_config_or_skip("google", &harness) else {
            return;
        };

        common::run_async(async move {
            let provider = build_provider(&config);
            let context = simple_context("Say just the word hello");
            let options = simple_options(&config.api_key);

            let (events, error) =
                collect_stream(provider.as_ref(), &context, &options, &harness).await;
            assert_basic_stream_success(&events, error.as_deref(), &harness, "gemini_basic");
        });
    }

    #[test]
    fn streaming_events() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_gemini_streaming");
        let Some(config) = provider_config_or_skip("google", &harness) else {
            return;
        };

        common::run_async(async move {
            let provider = build_provider(&config);
            let context = simple_context("Say just the word hello");
            let options = simple_options(&config.api_key);

            let (events, error) =
                collect_stream(provider.as_ref(), &context, &options, &harness).await;
            assert_basic_stream_success(&events, error.as_deref(), &harness, "gemini_streaming");

            // Verify we got Start event
            let has_start = events
                .iter()
                .any(|e| matches!(e, StreamEvent::Start { .. }));
            assert!(has_start, "Expected Start event from Gemini");
        });
    }
}

// ---------------------------------------------------------------------------
// OpenRouter E2E Tests (OpenAI-compat)
// ---------------------------------------------------------------------------

mod openrouter {
    use super::*;

    #[test]
    fn basic_message() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_openrouter_basic_message");
        let Some(config) = provider_config_or_skip("openrouter", &harness) else {
            return;
        };

        common::run_async(async move {
            let provider = build_provider(&config);
            let context = simple_context("Say just the word hello");
            let options = simple_options(&config.api_key);

            let (events, error) =
                collect_stream(provider.as_ref(), &context, &options, &harness).await;
            assert_basic_stream_success(&events, error.as_deref(), &harness, "openrouter_basic");
        });
    }
}

// ---------------------------------------------------------------------------
// xAI/Grok E2E Tests (OpenAI-compat)
// ---------------------------------------------------------------------------

mod xai {
    use super::*;

    #[test]
    fn basic_message() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_xai_basic_message");
        let Some(config) = provider_config_or_skip("xai", &harness) else {
            return;
        };

        common::run_async(async move {
            let provider = build_provider(&config);
            let context = simple_context("Say just the word hello");
            let options = simple_options(&config.api_key);

            let (events, error) =
                collect_stream(provider.as_ref(), &context, &options, &harness).await;
            assert_basic_stream_success(&events, error.as_deref(), &harness, "xai_basic");
        });
    }
}

// ---------------------------------------------------------------------------
// DeepSeek E2E Tests (OpenAI-compat)
// ---------------------------------------------------------------------------

mod deepseek {
    use super::*;

    #[test]
    fn basic_message() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_deepseek_basic_message");
        let Some(config) = provider_config_or_skip("deepseek", &harness) else {
            return;
        };

        common::run_async(async move {
            let provider = build_provider(&config);
            let context = simple_context("Say just the word hello");
            let options = simple_options(&config.api_key);

            let (events, error) =
                collect_stream(provider.as_ref(), &context, &options, &harness).await;
            assert_basic_stream_success(&events, error.as_deref(), &harness, "deepseek_basic");
        });
    }
}

// ---------------------------------------------------------------------------
// Cross-provider comparison
// ---------------------------------------------------------------------------

mod cross_provider {
    use super::*;

    #[test]
    fn all_available_providers_respond() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_cross_provider_all_respond");
        let discovery = provider_discovery()
            .unwrap_or_else(|err| panic!("live provider discovery failed: {err}"));
        if let Err(err) = write_discovery_artifacts(&harness, &discovery) {
            panic!("failed to write discovery artifacts: {err}");
        }

        common::run_async(async move {
            let prompt = "Say just the word hello";
            let mut results: Vec<(String, bool, u128)> = Vec::new();

            for provider_name in LIVE_PROVIDER_ORDER {
                let Some(config) = discovery.configs.get(provider_name).cloned() else {
                    continue;
                };
                let provider = build_provider(&config);
                let start = Instant::now();
                let (events, error) = collect_stream(
                    provider.as_ref(),
                    &simple_context(prompt),
                    &simple_options(&config.api_key),
                    &harness,
                )
                .await;
                let ms = start.elapsed().as_millis();
                let ok = error.is_none() && !events.is_empty();
                results.push((provider_name.to_string(), ok, ms));
            }

            // Log summary table
            harness
                .log()
                .info("summary", "=== Cross-Provider Results ===");
            for (name, ok, ms) in &results {
                let status = if *ok { "PASS" } else { "FAIL" };
                harness
                    .log()
                    .info_ctx("summary", format!("{name}: {status}"), |ctx| {
                        ctx.push(("latency_ms".into(), format!("{ms}")));
                    });
            }

            let all_passed = results.iter().all(|(_, ok, _)| *ok);
            assert!(all_passed, "Not all providers succeeded: {results:?}");
            assert!(
                !results.is_empty(),
                "No providers were available for testing"
            );
        });
    }

    /// Verify all providers emit compatible `StreamEvent` sequences:
    /// Start → (TextDelta|ThinkingDelta|TextEnd)+ → Done
    /// Logs a comparison table of timing, token usage, and response lengths.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn streaming_event_parity() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_cross_provider_streaming_parity");
        let discovery = provider_discovery()
            .unwrap_or_else(|err| panic!("live provider discovery failed: {err}"));

        common::run_async(async move {
            let prompt = "Say just the word hello";
            let mut summary: Vec<StreamingSummaryRow> = Vec::new();

            for provider_name in LIVE_PROVIDER_ORDER {
                let Some(config) = discovery.configs.get(provider_name).cloned() else {
                    continue;
                };
                let provider = build_provider(&config);
                let start = Instant::now();
                let (events, error) = collect_stream(
                    provider.as_ref(),
                    &simple_context(prompt),
                    &simple_options(&config.api_key),
                    &harness,
                )
                .await;
                let ms = start.elapsed().as_millis();

                if error.is_some() || events.is_empty() {
                    harness.log().warn(
                        "parity",
                        format!(
                            "{provider_name}: SKIPPED (error or empty): {:?}",
                            error.as_deref().unwrap_or("no events")
                        ),
                    );
                    continue;
                }

                let has_start = matches!(events.first(), Some(StreamEvent::Start { .. }));
                let has_done = matches!(events.last(), Some(StreamEvent::Done { .. }));
                let has_delta_content = events.iter().any(|e| {
                    matches!(
                        e,
                        StreamEvent::TextDelta { .. }
                            | StreamEvent::TextEnd { .. }
                            | StreamEvent::ThinkingDelta { .. }
                    )
                });
                let done_has_text = events.iter().any(|e| match e {
                    StreamEvent::Done { message, .. } => message.content.iter().any(
                        |c| matches!(c, pi::model::ContentBlock::Text(tc) if !tc.text.is_empty()),
                    ),
                    _ => false,
                });
                let has_content = has_delta_content || done_has_text;

                // Extract token usage and text length from Done event
                let (input_tok, output_tok, text_len) = events
                    .iter()
                    .find_map(|e| match e {
                        StreamEvent::Done { message, .. } => {
                            let text: String = message
                                .content
                                .iter()
                                .filter_map(|c| match c {
                                    pi::model::ContentBlock::Text(tc) => Some(tc.text.as_str()),
                                    _ => None,
                                })
                                .collect();
                            Some((message.usage.input, message.usage.output, text.len()))
                        }
                        _ => None,
                    })
                    .unwrap_or((0, 0, 0));

                summary.push((
                    provider_name.to_string(),
                    config.model_id.clone(),
                    has_start,
                    has_content,
                    has_done,
                    input_tok,
                    output_tok,
                    text_len,
                    ms,
                ));
            }

            // Log comparison table
            harness
                .log()
                .info("parity", "=== Streaming Event Parity ===");
            harness.log().info(
                "parity",
                "provider | model | Start | Content | Done | in_tok | out_tok | text_len | ms",
            );
            for (name, model, start, content, done, in_t, out_t, tlen, ms) in &summary {
                harness.log().info(
                    "parity",
                    format!(
                        "{name:12} | {model:30} | {start:5} | {content:7} | {done:5} | {in_t:6} | {out_t:7} | {tlen:8} | {ms}ms"
                    ),
                );
            }

            // Every provider that responded must have Start → Content → Done
            let mut failures = Vec::new();
            for (name, _, has_start, has_content, has_done, _, _, _, _) in &summary {
                if !has_start {
                    failures.push(format!("{name}: missing Start event"));
                }
                if !has_content {
                    failures.push(format!("{name}: missing content events"));
                }
                if !has_done {
                    failures.push(format!("{name}: missing Done event"));
                }
            }
            assert!(
                failures.is_empty(),
                "Streaming event parity failures:\n{}",
                failures.join("\n")
            );
            assert!(
                !summary.is_empty(),
                "No providers were available for parity testing"
            );
        });
    }

    /// Send an intentionally invalid model ID to each provider and verify all
    /// return an error (either a stream error or an Error event).
    #[test]
    fn error_handling_parity() {
        skip_unless_e2e!();
        let harness = TestHarness::new("e2e_cross_provider_error_parity");
        let discovery = provider_discovery()
            .unwrap_or_else(|err| panic!("live provider discovery failed: {err}"));

        common::run_async(async move {
            let prompt = "Say hello";
            let mut results: Vec<(String, bool, String)> = Vec::new();

            for provider_name in LIVE_PROVIDER_ORDER {
                let Some(config) = discovery.configs.get(provider_name).cloned() else {
                    continue;
                };

                // Construct a provider with an invalid model ID to trigger an error
                let bad_config = LiveProviderConfig {
                    model_id: "__pi_e2e_nonexistent_model__".to_string(),
                    ..config.clone()
                };
                let provider = build_provider(&bad_config);

                let (events, stream_error) = collect_stream(
                    provider.as_ref(),
                    &simple_context(prompt),
                    &simple_options(&config.api_key),
                    &harness,
                )
                .await;

                // Check if we got an error via stream_error or an Error event
                let has_stream_error = stream_error.is_some();
                let has_error_event = events
                    .iter()
                    .any(|e| matches!(e, StreamEvent::Error { .. }));
                let got_error = has_stream_error || has_error_event;

                let error_desc = stream_error.as_ref().map_or_else(
                    || {
                        if has_error_event {
                            "Error event in stream".to_string()
                        } else {
                            "no error (unexpected success)".to_string()
                        }
                    },
                    |err| format!("stream_error: {}", &err[..err.len().min(120)]),
                );

                harness.log().info_ctx(
                    "error_parity",
                    format!("{provider_name}: got_error={got_error}"),
                    |ctx| {
                        ctx.push(("error_desc".into(), error_desc.clone()));
                    },
                );

                results.push((provider_name.to_string(), got_error, error_desc));
            }

            // Log summary
            harness
                .log()
                .info("error_parity", "=== Error Handling Parity ===");
            for (name, got_error, desc) in &results {
                let status = if *got_error { "PASS" } else { "FAIL" };
                harness
                    .log()
                    .info("error_parity", format!("{name:12}: {status} — {desc}"));
            }

            // All providers should return an error for a nonexistent model
            let failures: Vec<_> = results
                .iter()
                .filter(|(_, got_error, _)| !got_error)
                .map(|(name, _, desc)| format!("{name}: {desc}"))
                .collect();
            assert!(
                failures.is_empty(),
                "Error handling parity failures (expected error for invalid model):\n{}",
                failures.join("\n")
            );
            assert!(
                !results.is_empty(),
                "No providers were available for error parity testing"
            );
        });
    }
}
