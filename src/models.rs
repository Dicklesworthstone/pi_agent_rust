//! Model registry: built-in + models.json overrides.

use crate::auth::{AuthStorage, SapResolvedCredentials, resolve_sap_credentials};
use crate::error::Error;
use crate::provider::{Api, InputType, Model, ModelCost};
use crate::provider_metadata::{
    ProviderRoutingDefaults, canonical_provider_id, provider_routing_defaults,
};
use regex::Regex;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub model: Model,
    pub api_key: Option<String>,
    pub headers: HashMap<String, String>,
    pub auth_header: bool,
    pub compat: Option<CompatConfig>,
    /// OAuth config for extension-registered providers that require browser-based auth.
    pub oauth_config: Option<OAuthConfig>,
}

impl ModelEntry {
    /// Whether this model supports xhigh thinking level.
    pub fn supports_xhigh(&self) -> bool {
        matches!(
            self.model.id.as_str(),
            "gpt-5.1-codex-max"
                | "gpt-5.2"
                | "gpt-5.5"
                | "gpt-5.6"
                | "gpt-5.6-sol"
                | "gpt-5.6-terra"
                | "gpt-5.6-luna"
                | "gpt-5.4"
                | "gpt-5.2-codex"
                | "gpt-5.3-codex"
                | "gpt-5.3-codex-spark"
        ) || self.is_deepseek_reasoning_model()
            || self.is_anthropic_xhigh_effort_model()
    }

    /// Whether this is an Anthropic adaptive-thinking model whose modern
    /// `output_config.effort` accepts the `xhigh` tier.
    ///
    /// xhigh effort is supported on Claude Opus 4.7/4.8 and the Claude
    /// Fable/Mythos (5.x) families; Opus 4.6 and Sonnet 4.6 support adaptive
    /// thinking + effort but NOT the xhigh tier (so they correctly clamp
    /// `XHigh -> High`). Scoped to the `anthropic-messages` transport (native
    /// Anthropic and Anthropic-compatible providers that route through
    /// `AnthropicProvider`); the `claude-` id check additionally excludes
    /// Anthropic-compatible non-Claude models on that transport (e.g. MiniMax).
    ///
    /// Without this, the registry clamps `XHigh -> High` before
    /// `AnthropicProvider::build_request` runs and the transport's `"xhigh"`
    /// effort arm is dead at runtime (the same reasoning as the DeepSeek
    /// `is_deepseek_reasoning_model` path; gh #116).
    /// Ref: https://platform.claude.com/docs/en/build-with-claude/effort
    fn is_anthropic_xhigh_effort_model(&self) -> bool {
        if !self.model.reasoning || self.model.api != "anthropic-messages" {
            return false;
        }
        let id = self.model.id.to_ascii_lowercase();
        let Some(pos) = id.find("claude-") else {
            return false;
        };
        let id = &id[pos..];
        id.starts_with("claude-opus-4-7")
            || id.starts_with("claude-opus-4-8")
            || id.starts_with("claude-opus-5")
            || id.starts_with("claude-sonnet-5")
            || id.starts_with("claude-fable-")
            || id.starts_with("claude-mythos-")
    }

    /// Whether this model supports the `max` thinking level (gh #139).
    ///
    /// `max` is the top effort tier, above `xhigh`:
    /// - Anthropic adaptive-thinking models accept `output_config.effort:
    ///   "max"` on every effort-capable family — including Opus 4.6 and
    ///   Sonnet 4.6, which support `max` but NOT `xhigh` (the `xhigh` tier
    ///   arrived with Opus 4.7).
    ///   Ref: https://platform.claude.com/docs/en/build-with-claude/effort
    /// - DeepSeek reasoning models document `reasoning_effort: "max"` as
    ///   their top thinking tier (previously reachable only by pi's `xhigh`).
    ///
    /// OpenAI-family models are excluded unless their API metadata explicitly
    /// advertises a distinct `max` effort tier. GPT-5.6 is the first such
    /// family; older OpenAI models continue to clamp `Max` down to `XHigh`.
    /// A catalog `thinkingLevelMap` override can still re-map levels per model.
    pub fn supports_max(&self) -> bool {
        matches!(
            self.model.id.as_str(),
            "gpt-5.6" | "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna"
        ) || self.is_deepseek_reasoning_model()
            || self.is_anthropic_max_effort_model()
    }

    /// Whether this is an Anthropic adaptive-thinking model whose
    /// `output_config.effort` accepts the `max` tier.
    ///
    /// Same transport/id scoping rationale as
    /// [`is_anthropic_xhigh_effort_model`](Self::is_anthropic_xhigh_effort_model),
    /// plus the Opus 4.6 / Sonnet 4.6 families (which accept `max` without
    /// `xhigh`).
    fn is_anthropic_max_effort_model(&self) -> bool {
        if self.is_anthropic_xhigh_effort_model() {
            return true;
        }
        if !self.model.reasoning || self.model.api != "anthropic-messages" {
            return false;
        }
        let id = self.model.id.to_ascii_lowercase();
        let Some(pos) = id.find("claude-") else {
            return false;
        };
        let id = &id[pos..];
        id.starts_with("claude-opus-4-6") || id.starts_with("claude-sonnet-4-6")
    }

    /// Whether this is a DeepSeek reasoning model whose thinking-mode API accepts
    /// `reasoning_effort: "max"`.
    ///
    /// DeepSeek reasoning models route through the DeepSeek thinking format on
    /// the chat-completions transport (see `OpenAIProvider::reasoning_style`), and
    /// DeepSeek maps the `xhigh` thinking level to `reasoning_effort: "max"` in
    /// thinking mode (gh #114; https://api-docs.deepseek.com/guides/thinking_mode).
    /// They therefore genuinely support xhigh — without this the registry clamps
    /// `XHigh -> High` before `build_request()` runs and the serializer's `"max"`
    /// arm is dead at runtime.
    ///
    /// Detected the same way the transport detects DeepSeek (provider id
    /// `deepseek`, or a `deepseek.com` base URL) AND restricted to reasoning
    /// models, so the non-thinking `deepseek-chat` / V3 family is never enabled
    /// (those are additionally excluded upstream, since `available_thinking_levels`
    /// and `clamp_thinking_level` short-circuit on non-reasoning models).
    fn is_deepseek_reasoning_model(&self) -> bool {
        if !self.model.reasoning {
            return false;
        }
        let provider_is_deepseek = canonical_provider_id(&self.model.provider)
            .is_some_and(|canonical| canonical == "deepseek")
            || self.model.provider.eq_ignore_ascii_case("deepseek");
        let base_is_deepseek = self
            .model
            .base_url
            .to_ascii_lowercase()
            .contains("deepseek.com");
        provider_is_deepseek || base_is_deepseek
    }

    /// Return the thinking levels that should be exposed for this model.
    pub fn available_thinking_levels(&self) -> Vec<crate::model::ThinkingLevel> {
        use crate::model::ThinkingLevel;

        if !self.model.reasoning {
            return vec![ThinkingLevel::Off];
        }

        let mut levels = vec![
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ];
        if self.supports_xhigh() {
            levels.push(ThinkingLevel::XHigh);
        }
        if self.supports_max() {
            levels.push(ThinkingLevel::Max);
        }
        levels
    }

    /// Clamp a requested thinking level to the model's capabilities.
    ///
    /// Non-reasoning models always return `Off`. Models without max support
    /// downgrade `Max` to `XHigh` (or `High` if xhigh is also unsupported);
    /// models without xhigh support downgrade `XHigh` to `High`. All other
    /// levels pass through unchanged.
    pub fn clamp_thinking_level(
        &self,
        thinking: crate::model::ThinkingLevel,
    ) -> crate::model::ThinkingLevel {
        if !self.model.reasoning {
            return crate::model::ThinkingLevel::Off;
        }
        let mut thinking = thinking;
        if thinking == crate::model::ThinkingLevel::Max && !self.supports_max() {
            thinking = if self.supports_xhigh() {
                crate::model::ThinkingLevel::XHigh
            } else {
                crate::model::ThinkingLevel::High
            };
        }
        if thinking == crate::model::ThinkingLevel::XHigh && !self.supports_xhigh() {
            return crate::model::ThinkingLevel::High;
        }
        thinking
    }
}

/// OAuth configuration for extension-registered providers.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub auth_url: String,
    pub token_url: String,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub redirect_uri: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsConfig {
    #[serde(deserialize_with = "deserialize_model_providers")]
    pub providers: HashMap<String, ProviderConfig>,
}

fn deserialize_model_providers<'de, D>(
    deserializer: D,
) -> std::result::Result<HashMap<String, ProviderConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ProvidersVisitor;

    impl<'de> Visitor<'de> for ProvidersVisitor {
        type Value = HashMap<String, ProviderConfig>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a map of model-provider configurations with unique keys")
        }

        fn visit_map<A>(self, mut entries: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut providers = HashMap::with_capacity(
                entries
                    .size_hint()
                    .unwrap_or_default()
                    .min(MAX_FETCHED_PROVIDERS),
            );
            while let Some(provider) = entries.next_key::<String>()? {
                if providers.contains_key(&provider) {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate JSON object key {provider:?} in models.json providers"
                    )));
                }
                let config = entries.next_value::<ProviderConfig>()?;
                providers.insert(provider, config);
            }
            Ok(providers)
        }
    }

    deserializer.deserialize_map(ProvidersVisitor)
}

pub(crate) const FETCHED_MODELS_SCHEMA: &str = "pi.models.fetched.v2";
pub(crate) const MAX_FETCHED_CATALOG_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_FETCHED_PROVIDERS: usize = 128;
pub(crate) const MAX_FETCHED_PROVIDER_ID_BYTES: usize = 256;
pub(crate) const MAX_FETCHED_MODELS_PER_PROVIDER: usize = 4_096;
pub(crate) const MAX_FETCHED_MODEL_ID_BYTES: usize = 512;
pub(crate) const MAX_FETCHED_MODEL_BYTES_PER_PROVIDER: usize = 2 * 1024 * 1024;

pub(crate) fn is_safe_model_catalog_identifier(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

/// Strict on-disk shape for the generated catalog.
///
/// Keeping this separate from [`ModelsConfig`] prevents a generated file from
/// silently acquiring routing, credential, or compatibility fields that only
/// belong in user-authored `models.json`.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedFetchedCatalog {
    pub(crate) schema: String,
    #[serde(deserialize_with = "deserialize_fetched_providers")]
    pub(crate) providers: BTreeMap<String, PersistedFetchedProvider>,
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
pub(crate) struct PersistedFetchedProvider {
    #[serde(rename = "routeFingerprint")]
    pub(crate) route_fingerprint: String,
    #[serde(rename = "fetchedAtUnixMs")]
    pub(crate) fetched_at_unix_ms: u64,
    #[serde(deserialize_with = "deserialize_fetched_models")]
    pub(crate) models: Vec<PersistedFetchedModel>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedFetchedModel {
    pub(crate) id: String,
}

fn deserialize_fetched_providers<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, PersistedFetchedProvider>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ProvidersVisitor;

    impl<'de> Visitor<'de> for ProvidersVisitor {
        type Value = BTreeMap<String, PersistedFetchedProvider>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded map of generated model providers")
        }

        fn visit_map<A>(self, mut entries: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut providers = BTreeMap::new();
            let mut canonical_providers = HashSet::new();
            while let Some(provider) = entries.next_key::<String>()? {
                if providers.len() >= MAX_FETCHED_PROVIDERS {
                    return Err(serde::de::Error::custom(format!(
                        "generated model catalog exceeds {MAX_FETCHED_PROVIDERS} providers"
                    )));
                }
                if !is_safe_model_catalog_identifier(&provider, MAX_FETCHED_PROVIDER_ID_BYTES) {
                    return Err(serde::de::Error::custom(
                        "generated model catalog contains an invalid provider ID",
                    ));
                }
                if providers.contains_key(&provider) {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate JSON object key {provider:?}"
                    )));
                }
                let canonical = canonical_provider_key(&provider);
                if !canonical_providers.insert(canonical.clone()) {
                    return Err(serde::de::Error::custom(format!(
                        "generated model catalog contains duplicate aliases for provider {canonical:?}"
                    )));
                }
                let config = entries.next_value::<PersistedFetchedProvider>()?;
                providers.insert(provider, config);
            }
            Ok(providers)
        }
    }

    deserializer.deserialize_map(ProvidersVisitor)
}

fn deserialize_fetched_models<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<PersistedFetchedModel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ModelsVisitor;

    impl<'de> Visitor<'de> for ModelsVisitor {
        type Value = Vec<PersistedFetchedModel>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded sequence of generated model IDs")
        }

        fn visit_seq<A>(self, mut rows: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut models = Vec::with_capacity(
                rows.size_hint()
                    .unwrap_or_default()
                    .min(MAX_FETCHED_MODELS_PER_PROVIDER),
            );
            let mut total_bytes = 0usize;
            while let Some(model) = rows.next_element::<PersistedFetchedModel>()? {
                if models.len() >= MAX_FETCHED_MODELS_PER_PROVIDER {
                    return Err(serde::de::Error::custom(format!(
                        "generated provider exceeds {MAX_FETCHED_MODELS_PER_PROVIDER} models"
                    )));
                }
                if !is_safe_model_catalog_identifier(&model.id, MAX_FETCHED_MODEL_ID_BYTES) {
                    return Err(serde::de::Error::custom(
                        "generated model catalog contains an invalid model ID",
                    ));
                }
                total_bytes = total_bytes.checked_add(model.id.len()).ok_or_else(|| {
                    serde::de::Error::custom("generated model catalog model-ID size overflow")
                })?;
                if total_bytes > MAX_FETCHED_MODEL_BYTES_PER_PROVIDER {
                    return Err(serde::de::Error::custom(format!(
                        "generated provider exceeds {MAX_FETCHED_MODEL_BYTES_PER_PROVIDER} model-ID bytes"
                    )));
                }
                models.push(model);
            }
            Ok(models)
        }
    }

    deserializer.deserialize_seq(ModelsVisitor)
}

/// Effective provider settings used by OpenAI-compatible model discovery.
///
/// This mirrors the provider-level merge performed for normal inference so a
/// `models.json` endpoint, credential, header, or auth-header override cannot
/// silently diverge from `--fetch-models`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelCatalogProviderConfig {
    pub(crate) base_url: String,
    pub(crate) api: String,
    pub(crate) api_key: Option<String>,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) auth_header: bool,
}

#[derive(Debug)]
pub(crate) struct PreparedModelCatalogProviderConfig {
    route: ModelCatalogProviderConfig,
    fallback_api_key: Option<String>,
    deferred_headers: HashMap<String, String>,
    base_dir: Option<PathBuf>,
}

impl PreparedModelCatalogProviderConfig {
    pub(crate) fn requires_runtime_api_key(&self) -> bool {
        self.route.auth_header && !has_complete_custom_authorization_header(&self.route.headers)
    }

    pub(crate) fn into_route(
        mut self,
        resolve_fallback_api_key: bool,
    ) -> ModelCatalogProviderConfig {
        self.route.headers.extend(resolve_headers_with_base(
            Some(&self.deferred_headers),
            self.base_dir.as_deref(),
        ));
        if resolve_fallback_api_key && self.requires_runtime_api_key() {
            self.route.api_key = self
                .fallback_api_key
                .as_deref()
                .and_then(|value| resolve_value_with_base(value, self.base_dir.as_deref()));
        }
        self.route
    }
}

/// Resolve the credential that a model-catalog request actually uses.
///
/// The caller-supplied credential represents normal runtime resolution
/// (explicit override, ambient/provider auth, and stored auth). A models.json
/// `apiKey` is only the fallback when that runtime credential is empty and the
/// route needs Pi to generate an Authorization header.
pub(crate) fn effective_model_catalog_api_key(
    caller_api_key: &str,
    route: &ModelCatalogProviderConfig,
) -> String {
    let caller_api_key = caller_api_key.trim();
    if caller_api_key.is_empty() {
        route
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|api_key| !api_key.is_empty())
            .unwrap_or_default()
            .to_string()
    } else {
        caller_api_key.to_string()
    }
}

fn update_catalog_fingerprint_component(hasher: &mut Sha256, label: &str, value: &[u8]) {
    hasher.update((label.len() as u64).to_le_bytes());
    hasher.update(label.as_bytes());
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn model_catalog_credential_query_name(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "access-token" | "access_token" | "api-key" | "api_key" | "apikey" | "key" | "token"
    )
}

fn model_catalog_credential_header_name(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "api-key"
            | "apikey"
            | "authorization"
            | "ocp-apim-subscription-key"
            | "proxy-authorization"
            | "x-api-key"
            | "x-auth-token"
            | "x-goog-api-key"
    )
}

fn parsed_model_catalog_route_url(base_url: &str) -> Option<url::Url> {
    let parsed = url::Url::parse(base_url.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }
    Some(parsed)
}

/// Whether persisted membership can be rebound without storing or hashing secret values.
///
/// Known credential query/header channels are rotation-tolerant: query names are classified
/// case-insensitively, but their exact decoded spelling, order, multiplicity, and empty/non-empty
/// shape are bound, while their values remain excluded. Header names remain case-insensitive by
/// HTTP semantics. Any non-empty value in an unclassified query/header channel may be tenant or
/// deployment routing, so persistence fails closed rather than reusing membership across an
/// unverifiable route.
pub(crate) fn model_catalog_route_is_persistable(route: &ModelCatalogProviderConfig) -> bool {
    let Some(parsed) = parsed_model_catalog_route_url(&route.base_url) else {
        return false;
    };
    let query_is_bindable = parsed.query_pairs().all(|(name, value)| {
        value.is_empty() || model_catalog_credential_query_name(name.as_ref())
    });
    query_is_bindable
        && route.headers.iter().all(|(name, value)| {
            value.trim().is_empty() || model_catalog_credential_header_name(name)
        })
}

/// Produce the non-secret endpoint/transport binding stored with fetched model
/// membership.
///
/// Credential values, URL query values, fragments, and header values are deliberately excluded.
/// Known credential query names and their ordered, case-sensitive multiplicity/presence shape are
/// bound alongside case-insensitive header names/presence. A plain SHA-256 digest of a credential
/// would still be an offline credential verifier, not harmless provenance. The process-local
/// fetch cache uses a separate credential-sensitive key.
pub(crate) fn model_catalog_route_fingerprint(
    provider: &str,
    route: &ModelCatalogProviderConfig,
) -> String {
    let mut hasher = Sha256::new();
    update_catalog_fingerprint_component(
        &mut hasher,
        "domain",
        b"pi.models.fetched.route-binding.v1",
    );
    update_catalog_fingerprint_component(
        &mut hasher,
        "provider",
        canonical_provider_key(provider).as_bytes(),
    );
    update_catalog_fingerprint_component(&mut hasher, "api", route.api.as_bytes());
    let parsed_route = parsed_model_catalog_route_url(&route.base_url);
    let normalized_base_url = parsed_route.clone().map_or_else(
        || "invalid-route-url".to_string(),
        |mut parsed| {
            parsed.set_query(None);
            parsed.set_fragment(None);
            parsed.to_string()
        },
    );
    update_catalog_fingerprint_component(&mut hasher, "base-url", normalized_base_url.as_bytes());
    if let Some(parsed) = parsed_route {
        for (name, value) in parsed.query_pairs() {
            update_catalog_fingerprint_component(&mut hasher, "query-name", name.as_bytes());
            update_catalog_fingerprint_component(
                &mut hasher,
                "query-value-present",
                &[u8::from(!value.is_empty())],
            );
        }
    }
    update_catalog_fingerprint_component(
        &mut hasher,
        "auth-header",
        &[u8::from(route.auth_header)],
    );

    let mut headers = route.headers.iter().collect::<Vec<_>>();
    headers.sort_unstable_by(|(left_name, _), (right_name, _)| {
        left_name
            .to_ascii_lowercase()
            .cmp(&right_name.to_ascii_lowercase())
            .then_with(|| left_name.cmp(right_name))
    });
    for (name, value) in headers {
        update_catalog_fingerprint_component(
            &mut hasher,
            "header-name",
            name.to_ascii_lowercase().as_bytes(),
        );
        update_catalog_fingerprint_component(
            &mut hasher,
            "header-present",
            &[u8::from(!value.trim().is_empty())],
        );
    }

    format!("sha256:{:x}", hasher.finalize())
}

fn is_valid_model_catalog_route_fingerprint(value: &str) -> bool {
    value.len() == "sha256:".len() + 64
        && value.starts_with("sha256:")
        && value["sha256:".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub base_url: Option<String>,
    pub api: Option<String>,
    pub api_key: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub auth_header: Option<bool>,
    pub compat: Option<CompatConfig>,
    pub models: Option<Vec<ModelConfig>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    pub id: String,
    pub name: Option<String>,
    pub api: Option<String>,
    pub reasoning: Option<bool>,
    pub input: Option<Vec<String>>,
    pub cost: Option<ModelCost>,
    pub context_window: Option<u32>,
    pub max_tokens: Option<u32>,
    pub headers: Option<HashMap<String, String>>,
    pub compat: Option<CompatConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatConfig {
    // ── Capability flags ────────────────────────────────────────────────
    pub supports_store: Option<bool>,
    pub supports_developer_role: Option<bool>,
    pub supports_reasoning_effort: Option<bool>,
    pub supports_usage_in_streaming: Option<bool>,
    pub supports_tools: Option<bool>,
    pub supports_streaming: Option<bool>,
    pub supports_parallel_tool_calls: Option<bool>,

    // ── Request field overrides ─────────────────────────────────────────
    /// Override the JSON field name for `max_tokens` (e.g., `"max_completion_tokens"` for o1).
    pub max_tokens_field: Option<String>,
    /// Override the system message role name (e.g., `"developer"` for some providers).
    pub system_role_name: Option<String>,
    /// Override the stop-reason field name in responses.
    pub stop_reason_field: Option<String>,

    // ── Per-provider request headers ────────────────────────────────────
    /// Extra HTTP headers injected into every request for this provider.
    /// Applied after default headers but before per-request `StreamOptions.headers`.
    pub custom_headers: Option<HashMap<String, String>>,

    // ── Gateway/routing metadata ────────────────────────────────────────
    pub open_router_routing: Option<serde_json::Value>,
    pub vercel_gateway_routing: Option<serde_json::Value>,

    // ── Reasoning / thinking controls (modern per-model capability data) ──
    /// Map pi's thinking levels onto the provider's native effort/thinking
    /// vocabulary, e.g. `{"xhigh": "max"}`. Keyed by the lowercase
    /// `ThinkingLevel` name
    /// (`off`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`).
    /// Lets the catalog steer a transport's effort serialization without code
    /// changes (gh #117). When absent, transports apply their built-in mapping.
    pub thinking_level_map: Option<HashMap<String, String>>,
    /// Force the modern adaptive-thinking API (`thinking: {type: "adaptive"}`
    /// plus `output_config.effort`) instead of the deprecated `budget_tokens`
    /// extended-thinking path. Authoritative over a transport's built-in
    /// model-id heuristic; the heuristic is consulted only when this is `None`
    /// (gh #116/#117).
    pub force_adaptive_thinking: Option<bool>,
    /// Provider-specific thinking serialization dialect carried from the
    /// catalog (e.g. `"zai"`, `"deepseek"`). Surfaced so transports can honor
    /// per-model thinking formats; previously silently dropped on parse
    /// (gh #117).
    pub thinking_format: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelRegistry {
    models: Vec<ModelEntry>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelAutocompleteCandidate {
    pub slug: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LegacyGeneratedModel {
    id: String,
    name: String,
    api: String,
    provider: String,
    #[serde(default)]
    base_url: String,
    /// Per-model reasoning capability as declared by the catalog. `Some` when
    /// the catalog explicitly carries the field (the common case — every
    /// generated entry sets it); `None` only when a future entry omits it.
    #[serde(default)]
    reasoning: Option<bool>,
    #[serde(default)]
    input: Vec<String>,
    #[serde(default)]
    cost: Option<ModelCost>,
    #[serde(default)]
    context_window: Option<u32>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    compat: Option<CompatConfig>,
}

const CODEX_RESPONSES_API_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const GOOGLE_GEMINI_CLI_API_URL: &str = "https://cloudcode-pa.googleapis.com";
const GOOGLE_ANTIGRAVITY_API_URL: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com";

static LEGACY_GENERATED_MODELS_CACHE: OnceLock<Vec<LegacyGeneratedModel>> = OnceLock::new();
static UPSTREAM_PROVIDER_MODEL_IDS_CACHE: OnceLock<HashMap<String, Vec<String>>> = OnceLock::new();
static MODEL_AUTOCOMPLETE_CACHE: OnceLock<Vec<ModelAutocompleteCandidate>> = OnceLock::new();
static MODEL_CATALOG_CACHE_FINGERPRINT: OnceLock<u64> = OnceLock::new();
static SATISFIES_RE: OnceLock<Regex> = OnceLock::new();
const INPUT_TEXT_ONLY: [InputType; 1] = [InputType::Text];
const INPUT_TEXT_AND_IMAGE: [InputType; 2] = [InputType::Text, InputType::Image];

fn canonicalize_openrouter_model_id(model_id: &str) -> String {
    let trimmed = model_id.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "auto" => "openrouter/auto".to_string(),
        "gpt-4o-mini" => "openai/gpt-4o-mini".to_string(),
        "gpt-4o" => "openai/gpt-4o".to_string(),
        "claude-3.5-sonnet" => "anthropic/claude-3.5-sonnet".to_string(),
        "gemini-2.5-pro" => "google/gemini-2.5-pro".to_string(),
        _ => trimmed.to_string(),
    }
}

pub(crate) fn canonicalize_model_id_for_provider(provider: &str, model_id: &str) -> String {
    if canonical_provider_id(provider).is_some_and(|canonical| canonical == "openrouter") {
        return canonicalize_openrouter_model_id(model_id);
    }
    model_id.trim().to_string()
}

pub(crate) fn normalized_registry_key(provider: &str, model_id: &str) -> (String, String) {
    let provider = provider.trim();
    let canonical_provider = canonical_provider_id(provider).unwrap_or(provider);
    let canonical_model_id = canonicalize_model_id_for_provider(canonical_provider, model_id);
    (
        canonical_provider.to_ascii_lowercase(),
        canonical_model_id.to_ascii_lowercase(),
    )
}

fn openrouter_model_lookup_ids(model_id: &str) -> Vec<String> {
    let raw = model_id.trim().to_string();
    let canonical = canonicalize_openrouter_model_id(model_id);
    if canonical.eq_ignore_ascii_case(&raw) {
        vec![canonical]
    } else {
        vec![raw, canonical]
    }
}

fn api_fallback_base_url(api: &str) -> Option<&'static str> {
    match api {
        "openai-codex-responses" => Some(CODEX_RESPONSES_API_URL),
        "google-gemini-cli" => Some(GOOGLE_GEMINI_CLI_API_URL),
        "google-antigravity" => Some(GOOGLE_ANTIGRAVITY_API_URL),
        _ => None,
    }
}

fn parse_input_types(input: &[String]) -> Vec<InputType> {
    input
        .iter()
        .filter_map(|value| match value.as_str() {
            "text" => Some(InputType::Text),
            "image" => Some(InputType::Image),
            _ => None,
        })
        .collect()
}

fn legacy_generated_models_cache_path() -> Option<PathBuf> {
    let checksum = crate::embedded_assets::legacy_models_generated_ts_crc32c();
    dirs::cache_dir().map(|dir| {
        dir.join("pi")
            .join("models-cache")
            .join(format!("legacy-generated-models-{checksum:08x}.json"))
    })
}

fn load_legacy_generated_models_cache() -> Option<Vec<LegacyGeneratedModel>> {
    let path = legacy_generated_models_cache_path()?;
    let cache = fs::read_to_string(path).ok()?;
    serde_json::from_str::<Vec<LegacyGeneratedModel>>(&cache).ok()
}

fn persist_legacy_generated_models_cache(models: &[LegacyGeneratedModel]) {
    let Some(path) = legacy_generated_models_cache_path() else {
        return;
    };
    if path.exists() {
        return;
    }
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }

    let temp_path = path.with_extension(format!("tmp-{}", std::process::id()));
    let Ok(file) = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
    else {
        return;
    };
    let mut writer = std::io::BufWriter::new(file);
    if serde_json::to_writer(&mut writer, models).is_ok() && writer.flush().is_ok() {
        let _ = fs::rename(&temp_path, path);
    } else {
        let _ = fs::remove_file(&temp_path);
    }
}

fn parse_legacy_generated_models() -> Vec<LegacyGeneratedModel> {
    if let Some(cached) = load_legacy_generated_models_cache() {
        return cached;
    }

    let source = crate::embedded_assets::legacy_models_generated_ts();
    let Some(models_decl_start) = source.find("export const MODELS =") else {
        tracing::warn!("Legacy model catalog missing MODELS declaration");
        return Vec::new();
    };
    let Some(object_start_rel) = source[models_decl_start..].find('{') else {
        tracing::warn!("Legacy model catalog missing object start after MODELS declaration");
        return Vec::new();
    };
    let object_start = models_decl_start + object_start_rel;
    let Some(end_marker_rel) = source[object_start..].rfind("} as const;") else {
        tracing::warn!("Legacy model catalog missing end marker");
        return Vec::new();
    };
    let end_marker = object_start + end_marker_rel;

    let mut object_source = source[object_start..=end_marker]
        .trim_end_matches(" as const;")
        .to_string();
    let satisfies_re = SATISFIES_RE.get_or_init(|| {
        Regex::new(r#"\s+satisfies\s+Model<"[^"]+">"#).expect("valid satisfies regex")
    });
    object_source = satisfies_re.replace_all(&object_source, "").into_owned();

    let parsed: HashMap<String, HashMap<String, LegacyGeneratedModel>> =
        match json5::from_str(&object_source) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(error = %err, "Failed to parse legacy model catalog");
                return Vec::new();
            }
        };

    let mut models = parsed
        .into_values()
        .flat_map(HashMap::into_values)
        .collect::<Vec<_>>();
    models.sort_by(|a, b| {
        a.provider
            .cmp(&b.provider)
            .then_with(|| a.id.cmp(&b.id))
            .then_with(|| a.api.cmp(&b.api))
    });
    persist_legacy_generated_models_cache(&models);
    models
}

fn legacy_generated_models() -> &'static [LegacyGeneratedModel] {
    LEGACY_GENERATED_MODELS_CACHE
        .get_or_init(parse_legacy_generated_models)
        .as_slice()
}

fn parse_upstream_provider_model_ids() -> HashMap<String, Vec<String>> {
    let parsed: HashMap<String, Vec<String>> =
        match serde_json::from_str(&crate::embedded_assets::provider_upstream_model_ids_json()) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(error = %err, "Failed to parse upstream provider model snapshot");
                return HashMap::new();
            }
        };

    let mut by_provider: HashMap<String, Vec<String>> = HashMap::new();
    merge_provider_model_ids(&mut by_provider, parsed);
    merge_provider_model_ids(&mut by_provider, parse_user_model_overrides());

    // GitHub Models was retired on 2026-07-30. Keep the captured upstream
    // artifact unchanged for historical reproducibility, but do not surface
    // its dead provider/model slugs through runtime autocomplete or fallback.
    by_provider.retain(|provider, _| !provider.eq_ignore_ascii_case("github-models"));

    for ids in by_provider.values_mut() {
        ids.sort_unstable();
        ids.dedup();
    }
    by_provider
}

fn merge_provider_model_ids(
    target: &mut HashMap<String, Vec<String>>,
    source: HashMap<String, Vec<String>>,
) {
    for (provider, ids) in source {
        let provider = provider.trim();
        if provider.is_empty() {
            continue;
        }
        let canonical_provider = canonical_provider_id(provider)
            .unwrap_or(provider)
            .to_string();
        let entry = target.entry(canonical_provider.clone()).or_default();
        for model_id in ids {
            let normalized = canonicalize_model_id_for_provider(&canonical_provider, &model_id);
            if !normalized.is_empty() {
                entry.push(normalized);
            }
        }
    }
}

/// Path to the user's optional model-override file.
///
/// Resolution order:
/// 1. `PI_MODELS_OVERRIDE` env var (absolute path) — primarily for tests and
///    advanced users who want to keep the override outside the standard config
///    directory.
/// 2. `<config_dir>/pi/models-override.json` — `<config_dir>` is whatever
///    `dirs::config_dir()` reports (e.g. `~/.config` on Linux,
///    `~/Library/Application Support` on macOS).
///
/// Returns `None` when no config directory can be resolved and no env override
/// is set; callers treat that as "no override available".
fn user_model_overrides_path() -> Option<PathBuf> {
    if let Ok(env_path) = std::env::var("PI_MODELS_OVERRIDE") {
        let trimmed = env_path.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    dirs::config_dir().map(|dir| dir.join("pi").join("models-override.json"))
}

/// Parse the user-supplied override file. Same shape as the bundled snapshot:
/// `{ "<provider>": ["<model-id>", ...], ... }`. Missing or unreadable files
/// are silently ignored; malformed JSON logs a warning and is treated as
/// empty so a typo in the override never breaks pi startup.
fn parse_user_model_overrides() -> HashMap<String, Vec<String>> {
    user_model_overrides_path()
        .map(|path| parse_user_model_overrides_at(&path))
        .unwrap_or_default()
}

fn parse_user_model_overrides_at(path: &Path) -> HashMap<String, Vec<String>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    path = %path.display(),
                    error = %err,
                    "User model override file present but unreadable; ignoring"
                );
            }
            return HashMap::new();
        }
    };
    if content.trim().is_empty() {
        return HashMap::new();
    }
    match serde_json::from_str::<HashMap<String, Vec<String>>>(&content) {
        Ok(value) => {
            tracing::debug!(
                path = %path.display(),
                providers = value.len(),
                "Loaded user model override file"
            );
            value
        }
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "Failed to parse pi user model override file; ignoring"
            );
            HashMap::new()
        }
    }
}

/// CRC32C of the user override file at process start, or 0 when no override
/// exists. Folded into [`model_catalog_cache_fingerprint`] so consumers that
/// memoize against the fingerprint refresh when a user changes their override.
fn user_model_overrides_fingerprint() -> u32 {
    user_model_overrides_path().map_or(0, |path| user_model_overrides_fingerprint_at(&path))
}

fn user_model_overrides_fingerprint_at(path: &Path) -> u32 {
    fs::read(path)
        .ok()
        .map_or(0, |bytes| crc32c::crc32c(&bytes))
}

fn upstream_provider_model_ids() -> &'static HashMap<String, Vec<String>> {
    UPSTREAM_PROVIDER_MODEL_IDS_CACHE.get_or_init(parse_upstream_provider_model_ids)
}

pub fn model_autocomplete_candidates() -> &'static [ModelAutocompleteCandidate] {
    MODEL_AUTOCOMPLETE_CACHE
        .get_or_init(|| {
            let mut candidates = legacy_generated_models()
                .iter()
                .map(|entry| ModelAutocompleteCandidate {
                    slug: format!("{}/{}", entry.provider, entry.id),
                    description: Some(entry.name.clone()).filter(|name| !name.trim().is_empty()),
                })
                .collect::<Vec<_>>();
            for (provider, ids) in upstream_provider_model_ids() {
                let provider = provider.trim();
                if provider.is_empty() {
                    continue;
                }
                for id in ids {
                    if id.trim().is_empty() {
                        continue;
                    }
                    candidates.push(ModelAutocompleteCandidate {
                        slug: format!("{provider}/{id}"),
                        description: None,
                    });
                }
            }
            candidates.push(ModelAutocompleteCandidate {
                slug: "anthropic/claude-sonnet-4-6".to_string(),
                description: Some("Claude Sonnet 4.6".to_string()),
            });
            candidates.push(ModelAutocompleteCandidate {
                slug: "openai/gpt-5.6".to_string(),
                description: Some("GPT-5.6 (Sol alias)".to_string()),
            });
            candidates.extend([
                ModelAutocompleteCandidate {
                    slug: "openai/gpt-5.6-sol".to_string(),
                    description: Some("GPT-5.6 Sol".to_string()),
                },
                ModelAutocompleteCandidate {
                    slug: "openai-codex/gpt-5.6-sol".to_string(),
                    description: Some("GPT-5.6 Sol Codex".to_string()),
                },
                ModelAutocompleteCandidate {
                    slug: "openai/gpt-5.6-terra".to_string(),
                    description: Some("GPT-5.6 Terra".to_string()),
                },
                ModelAutocompleteCandidate {
                    slug: "openai-codex/gpt-5.6-terra".to_string(),
                    description: Some("GPT-5.6 Terra Codex".to_string()),
                },
                ModelAutocompleteCandidate {
                    slug: "openai/gpt-5.6-luna".to_string(),
                    description: Some("GPT-5.6 Luna".to_string()),
                },
                ModelAutocompleteCandidate {
                    slug: "openai-codex/gpt-5.6-luna".to_string(),
                    description: Some("GPT-5.6 Luna Codex".to_string()),
                },
            ]);
            candidates.push(ModelAutocompleteCandidate {
                slug: "openai/gpt-5.5".to_string(),
                description: Some("GPT-5.5".to_string()),
            });
            candidates.push(ModelAutocompleteCandidate {
                slug: "openai/gpt-5.4".to_string(),
                description: Some("GPT-5.4".to_string()),
            });
            candidates.push(ModelAutocompleteCandidate {
                slug: "openai-codex/gpt-5.5".to_string(),
                description: Some("GPT-5.5 Codex".to_string()),
            });
            candidates.push(ModelAutocompleteCandidate {
                slug: "openai-codex/gpt-5.4".to_string(),
                description: Some("GPT-5.4 Codex".to_string()),
            });
            candidates.push(ModelAutocompleteCandidate {
                slug: "openai-codex/gpt-5.2-codex".to_string(),
                description: Some("GPT-5.2 Codex".to_string()),
            });
            candidates.push(ModelAutocompleteCandidate {
                slug: "google-gemini-cli/gemini-2.5-pro".to_string(),
                description: Some("Gemini 2.5 Pro (CLI)".to_string()),
            });
            candidates.push(ModelAutocompleteCandidate {
                slug: "google-antigravity/gemini-3-flash".to_string(),
                description: Some("Gemini 3 Flash (Antigravity)".to_string()),
            });
            candidates.sort_by_key(|candidate| candidate.slug.to_ascii_lowercase());
            candidates.dedup_by(|a, b| a.slug.eq_ignore_ascii_case(&b.slug));
            candidates
        })
        .as_slice()
}

pub fn model_catalog_cache_fingerprint() -> u64 {
    *MODEL_CATALOG_CACHE_FINGERPRINT.get_or_init(|| {
        let legacy = u64::from(crate::embedded_assets::legacy_models_generated_ts_crc32c());
        let upstream = u64::from(crate::embedded_assets::provider_upstream_model_ids_json_crc32c());
        let user_override = u64::from(user_model_overrides_fingerprint());
        // Mix the override CRC into both halves so any change forces cache
        // invalidation regardless of whether the snapshot or the override
        // moved.
        (legacy ^ user_override) << 32 | (upstream ^ user_override)
    })
}

pub(crate) fn normalize_api_key_opt(api_key: Option<String>) -> Option<String> {
    api_key.and_then(|key| {
        let trimmed = key.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

pub(crate) fn model_requires_configured_credential(entry: &ModelEntry) -> bool {
    let provider = entry.model.provider.as_str();
    let canonical_provider = canonical_provider_id(provider).unwrap_or(provider);

    // These native adapters resolve structured credentials at request time. Requiring a generic
    // `api_key` here would either reject valid AWS/SAP credential chains before the provider can
    // consume them or tempt callers to flatten one component into a bogus bearer token.
    if matches!(canonical_provider, "amazon-bedrock" | "sap-ai-core") {
        return false;
    }

    entry.auth_header
        || crate::provider_metadata::provider_metadata(provider)
            .is_some_and(|meta| !meta.auth_env_keys.is_empty())
        || entry.oauth_config.is_some()
}

pub(crate) fn model_entry_is_ready(entry: &ModelEntry) -> bool {
    !model_requires_configured_credential(entry)
        || entry
            .api_key
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModelRegistryLoadMode {
    Full,
    ListingLite,
}

trait ModelCredentialResolver {
    fn resolve_api_key(&self, provider: &str, override_key: Option<&str>) -> Option<String>;
}

type ProviderHeadersSnapshot = HashMap<String, HashMap<String, String>>;

impl ModelCredentialResolver for AuthStorage {
    fn resolve_api_key(&self, provider: &str, override_key: Option<&str>) -> Option<String> {
        Self::resolve_api_key(self, provider, override_key)
    }
}

impl<F> ModelCredentialResolver for F
where
    F: Fn(&str) -> Option<String>,
{
    fn resolve_api_key(&self, provider: &str, _override_key: Option<&str>) -> Option<String> {
        self(provider)
    }
}

impl ModelRegistry {
    #[cfg(test)]
    pub(crate) fn from_entries_for_tests(entries: Vec<ModelEntry>) -> Self {
        Self {
            models: entries,
            error: None,
        }
    }

    pub fn load(auth: &AuthStorage, models_path: Option<PathBuf>) -> Self {
        Self::load_with_mode(auth, models_path, ModelRegistryLoadMode::Full)
    }

    /// Load models with caller-controlled provider credential resolution.
    ///
    /// This keeps deterministic harnesses and embedders independent of ambient
    /// process credentials while leaving normal [`Self::load`] behavior intact.
    pub fn load_with_credential_resolver<F>(
        models_path: Option<PathBuf>,
        resolve_api_key: F,
    ) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        Self::load_with_mode_and_credential_resolver(
            models_path,
            ModelRegistryLoadMode::Full,
            &resolve_api_key,
        )
    }

    pub fn load_for_listing(auth: &AuthStorage, models_path: Option<PathBuf>) -> Self {
        Self::load_with_mode(auth, models_path, ModelRegistryLoadMode::ListingLite)
    }

    pub(crate) fn load_for_listing_with_credential_resolver<F>(
        models_path: Option<PathBuf>,
        resolve_api_key: F,
    ) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        Self::load_with_mode_and_credential_resolver(
            models_path,
            ModelRegistryLoadMode::ListingLite,
            &resolve_api_key,
        )
    }

    fn load_with_mode(
        auth: &AuthStorage,
        models_path: Option<PathBuf>,
        mode: ModelRegistryLoadMode,
    ) -> Self {
        Self::load_with_mode_and_credential_resolver(models_path, mode, &|provider| {
            auth.resolve_api_key(provider, None)
        })
    }

    fn load_with_mode_and_credential_resolver<F>(
        models_path: Option<PathBuf>,
        mode: ModelRegistryLoadMode,
        resolve_api_key: &F,
    ) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        // Credential lookup can involve mutable external state (credential
        // helpers, files, or an embedding callback). Resolve each canonical
        // provider once so built-in, fetched, and hand-authored entries cannot
        // observe different credentials during one registry load.
        let credential_snapshot = RefCell::new(HashMap::<String, Option<String>>::new());
        let stable_resolve_api_key = |provider: &str| {
            let key = canonical_provider_key(provider);
            let cached = credential_snapshot.borrow().get(&key).cloned();
            if let Some(value) = cached {
                return value;
            }
            let value = resolve_api_key(provider);
            credential_snapshot.borrow_mut().insert(key, value.clone());
            value
        };

        let mut models = built_in_models(&stable_resolve_api_key, mode);
        let mut errors = Vec::new();

        if let Some(path) = models_path {
            let fetched_path = fetched_models_path(&path);
            let mut manual_config_load_failed = false;
            let manual_config = match fs::symlink_metadata(&path) {
                Ok(_) => match load_models_config(&path) {
                    Ok(config) => Some(config),
                    Err(error) => {
                        manual_config_load_failed = true;
                        errors.push(format!("{error}\n\nFile: {}", path.display()));
                        None
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    manual_config_load_failed = true;
                    errors.push(format!(
                        "Failed to inspect model catalog {}: {error}",
                        path.display()
                    ));
                    None
                }
            };
            let manual_provider_headers = manual_config
                .as_ref()
                .map(|config| resolve_provider_headers_snapshot(config, path.parent()));

            match fs::symlink_metadata(&fetched_path) {
                Ok(_) if manual_config_load_failed => errors.push(format!(
                    "Ignoring generated model catalog {} because the current models.json route configuration could not be loaded; repair {} before refreshing persisted membership",
                    fetched_path.display(),
                    path.display()
                )),
                Ok(_) => match load_fetched_models_config(
                    &fetched_path,
                    &path,
                    manual_config.as_ref(),
                    manual_provider_headers.as_ref(),
                ) {
                    Ok((config, binding_errors)) => {
                        errors.extend(binding_errors);
                        apply_fetched_models(
                            &stable_resolve_api_key,
                            &mut models,
                            &config,
                            manual_config.as_ref(),
                            fetched_path.parent(),
                        );
                    }
                    Err(error) => {
                        errors.push(format!("{error}\n\nFile: {}", fetched_path.display()));
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => errors.push(format!(
                    "Failed to inspect generated model catalog {}: {error}",
                    fetched_path.display()
                )),
            }

            if let Some(config) = manual_config {
                // User-authored models.json is intentionally applied last so
                // every manual provider/model override keeps final authority
                // over the generated catalog.
                apply_custom_models_with_provider_headers(
                    &stable_resolve_api_key,
                    &mut models,
                    &config,
                    path.parent(),
                    manual_provider_headers.as_ref(),
                );
            }
        }

        Self {
            models,
            error: (!errors.is_empty()).then(|| errors.join("\n\n")),
        }
    }

    pub fn models(&self) -> &[ModelEntry] {
        &self.models
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn available_models(&self) -> Vec<&ModelEntry> {
        self.models
            .iter()
            .filter(|m| model_entry_is_ready(m))
            .collect()
    }

    pub fn get_available(&self) -> Vec<ModelEntry> {
        self.available_models().into_iter().cloned().collect()
    }

    pub fn find(&self, provider: &str, id: &str) -> Option<ModelEntry> {
        let provider = provider.trim();
        let canonical_provider = canonical_provider_id(provider).unwrap_or(provider);
        let is_openrouter = canonical_provider.eq_ignore_ascii_case("openrouter");
        // Avoid Vec + String allocation for the common (non-OpenRouter) path.
        let openrouter_ids = if is_openrouter {
            openrouter_model_lookup_ids(id)
        } else {
            Vec::new()
        };
        let trimmed_id = id.trim();

        self.models
            .iter()
            .find(|m| {
                let model_provider = m.model.provider.as_str();
                let model_provider_canonical =
                    canonical_provider_id(model_provider).unwrap_or(model_provider);
                let provider_matches = model_provider.eq_ignore_ascii_case(provider)
                    || model_provider.eq_ignore_ascii_case(canonical_provider)
                    || model_provider_canonical.eq_ignore_ascii_case(provider)
                    || model_provider_canonical.eq_ignore_ascii_case(canonical_provider);
                provider_matches
                    && if is_openrouter {
                        openrouter_ids
                            .iter()
                            .any(|lookup_id| m.model.id.eq_ignore_ascii_case(lookup_id))
                    } else {
                        m.model.id.eq_ignore_ascii_case(trimmed_id)
                    }
            })
            .cloned()
    }

    /// Find a model by ID alone (ignoring provider), useful for extension models
    /// where the provider name may be custom.
    ///
    /// When multiple providers carry the same model ID, the canonical/primary
    /// provider is preferred (e.g. `anthropic` for Claude models, `openai` for
    /// GPT models). If no canonical match exists, the first alphabetical
    /// provider wins, ensuring deterministic results regardless of insertion
    /// order.
    pub fn find_by_id(&self, id: &str) -> Option<ModelEntry> {
        let id = id.trim();
        let mut best: Option<&ModelEntry> = None;
        for entry in &self.models {
            if !entry.model.id.eq_ignore_ascii_case(id) {
                continue;
            }
            let Some(current_best) = best else {
                best = Some(entry);
                continue;
            };
            let entry_canonical = is_canonical_provider_for_model(id, &entry.model.provider);
            let best_canonical = is_canonical_provider_for_model(id, &current_best.model.provider);
            if entry_canonical && !best_canonical {
                best = Some(entry);
            } else if entry_canonical == best_canonical
                && entry.model.provider < current_best.model.provider
            {
                // Tie-break alphabetically for determinism.
                best = Some(entry);
            }
        }
        best.cloned()
    }

    /// Merge extension-provided model entries into the registry.
    pub fn merge_entries(&mut self, entries: Vec<ModelEntry>) {
        for entry in entries {
            // Skip duplicates (canonical provider + canonical model id, case-insensitive).
            let entry_key = normalized_registry_key(&entry.model.provider, &entry.model.id);
            let exists = self
                .models
                .iter()
                .any(|m| normalized_registry_key(&m.model.provider, &m.model.id) == entry_key);
            if !exists {
                self.models.push(entry);
            }
        }
    }
}

/// Returns `true` when `provider` is the canonical/primary source for a model
/// identified by `model_id`. Used by `find_by_id` to prefer the authoritative
/// provider when the same model ID appears under multiple resellers.
fn is_canonical_provider_for_model(model_id: &str, provider: &str) -> bool {
    let id_lower = model_id.to_ascii_lowercase();
    let prov_lower = provider.to_ascii_lowercase();
    if id_lower.starts_with("claude") {
        prov_lower == "anthropic"
    } else if id_lower.starts_with("gpt-")
        || id_lower.starts_with("o1")
        || id_lower.starts_with("o3")
        || id_lower.starts_with("o4")
    {
        prov_lower == "openai"
    } else if id_lower.starts_with("gemini") {
        prov_lower == "google"
    } else if id_lower.starts_with("command") {
        prov_lower == "cohere"
    } else if id_lower.starts_with("mistral") || id_lower.starts_with("codestral") {
        prov_lower == "mistral"
    } else if id_lower.starts_with("deepseek") {
        prov_lower == "deepseek"
    } else {
        false
    }
}

/// Determine per-model reasoning capability. Returns `Some(true/false)` for
/// known model ID patterns, `None` for unknown models (caller should fall back
/// to the provider-level default).
///
/// This prevents non-reasoning models like `gpt-4o` from inheriting a
/// provider-level `reasoning: true` flag from their provider (Issue #19).
fn model_is_reasoning(model_id: &str) -> Option<bool> {
    let raw_id = model_id.to_ascii_lowercase();
    let id = [
        "claude-",
        "gpt-",
        "gemini-",
        "command-",
        "deepseek",
        "qwq-",
        "mistral",
        "codestral",
        "pixtral",
        "llama",
        "o1",
        "o3",
        "o4",
    ]
    .iter()
    .find_map(|needle| raw_id.find(needle).map(|idx| &raw_id[idx..]))
    .unwrap_or(raw_id.as_str());

    // OpenAI: o1/o3/o4 series and gpt-5.x are reasoning.
    // All gpt-4 variants (gpt-4o, gpt-4-turbo, gpt-4-0613, etc.) and gpt-3.5 are NOT.
    if id.starts_with("o1") || id.starts_with("o3") || id.starts_with("o4") {
        return Some(true);
    }
    if id.starts_with("gpt-5") {
        return Some(true);
    }
    if id.starts_with("gpt-4") || id.starts_with("gpt-3.5") {
        return Some(false);
    }

    // Anthropic: Claude 3.5 Sonnet and Claude 4+ support extended thinking.
    // Claude 3 (Haiku/Sonnet/Opus) and Claude 3.5 Haiku do NOT.
    if id.starts_with("claude-3-5-haiku")
        || id.starts_with("claude-3-haiku")
        || id.starts_with("claude-3-sonnet")
        || id.starts_with("claude-3-opus")
    {
        return Some(false);
    }
    if id.starts_with("claude") {
        // Claude 3.5 Sonnet, Claude 4.x, Claude Opus 4+, Claude Sonnet 4+ etc.
        return Some(true);
    }

    // Google: gemini-2.5+ and gemini-2.0-flash-thinking are reasoning.
    // All other gemini models (2.0-flash, 2.0-flash-lite, 1.x, etc.) are NOT.
    if id.starts_with("gemini-2.5")
        || id.starts_with("gemini-3")
        || id.starts_with("gemini-2.0-flash-thinking")
    {
        return Some(true);
    }
    if id.starts_with("gemini") {
        return Some(false);
    }

    // Cohere: command-a is reasoning; command-r is not.
    if id.starts_with("command-a") {
        return Some(true);
    }
    if id.starts_with("command-r") {
        return Some(false);
    }

    // DeepSeek: thinking-mode models are reasoning.
    // - deepseek-reasoner (legacy thinking alias) and the R-series (R1).
    // - deepseek-v4-pro / deepseek-v4-flash: the current V4 models, both
    //   thinking-capable with reasoning_effort high/max (gh #114;
    //   https://api-docs.deepseek.com/news/news260424).
    // The legacy non-thinking deepseek-chat (V3 / non-thinking alias) and
    // deepseek-coder are NOT reasoning.
    if id.starts_with("deepseek-reasoner")
        || id.starts_with("deepseek-r")
        || id.starts_with("deepseek-v4-pro")
        || id.starts_with("deepseek-v4-flash")
    {
        return Some(true);
    }
    if id.starts_with("deepseek") {
        return Some(false);
    }

    // Qwen: qwq- series are reasoning.
    if id.starts_with("qwq-") {
        return Some(true);
    }

    // Mistral/Codestral: no reasoning support currently.
    if id.starts_with("mistral") || id.starts_with("codestral") || id.starts_with("pixtral") {
        return Some(false);
    }

    // Meta Llama: no reasoning support.
    if id.starts_with("llama") {
        return Some(false);
    }

    // Groq-hosted models: groq model IDs typically include the upstream model name
    // (e.g., "llama-3.3-70b-versatile"), so the upstream checks above should catch them.
    None
}

/// Resolve the effective reasoning flag for a model, preferring per-model
/// detection over the provider-level default.
fn effective_reasoning(model_id: &str, provider_default: bool) -> bool {
    model_is_reasoning(model_id).unwrap_or(provider_default)
}

fn native_adapter_seed_defaults(provider: &str) -> Option<AdHocProviderDefaults> {
    match provider {
        "openai-codex" => Some(AdHocProviderDefaults {
            api: "openai-codex-responses",
            base_url: CODEX_RESPONSES_API_URL,
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT_AND_IMAGE,
            context_window: 272_000,
            max_tokens: 128_000,
        }),
        "google-gemini-cli" => Some(AdHocProviderDefaults {
            api: "google-gemini-cli",
            base_url: GOOGLE_GEMINI_CLI_API_URL,
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT_AND_IMAGE,
            context_window: 128_000,
            max_tokens: 8192,
        }),
        "google-antigravity" => Some(AdHocProviderDefaults {
            api: "google-gemini-cli",
            base_url: GOOGLE_ANTIGRAVITY_API_URL,
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT_AND_IMAGE,
            context_window: 128_000,
            max_tokens: 8192,
        }),
        "azure-openai" => Some(AdHocProviderDefaults {
            api: "openai-completions",
            base_url: "",
            auth_header: false,
            reasoning: true,
            input: &INPUT_TEXT_AND_IMAGE,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        "github-copilot" | "sap-ai-core" => Some(AdHocProviderDefaults {
            api: "openai-completions",
            base_url: "",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT_ONLY,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        "gitlab" => Some(AdHocProviderDefaults {
            api: "gitlab-chat",
            base_url: "",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT_ONLY,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        _ => None,
    }
}

fn custom_provider_defaults(provider: &str) -> Option<AdHocProviderDefaults> {
    let canonical_provider = canonical_provider_id(provider).unwrap_or(provider);
    ad_hoc_provider_defaults(canonical_provider)
        .or_else(|| native_adapter_seed_defaults(canonical_provider))
}

fn provider_has_catalog_route(provider: &str, config: &ProviderConfig) -> bool {
    config
        .base_url
        .as_deref()
        .is_some_and(|base_url| !base_url.trim().is_empty())
        || custom_provider_defaults(provider)
            .is_some_and(|defaults| !defaults.base_url.trim().is_empty())
}

fn resolved_provider_transport(
    provider: &str,
    config: &ProviderConfig,
) -> (Option<AdHocProviderDefaults>, String, String, bool) {
    let defaults = custom_provider_defaults(provider);
    let default_api = defaults.map_or("openai-completions", |value| value.api);
    let requested_api = config.api.as_deref().unwrap_or(default_api);
    let api = requested_api
        .parse::<Api>()
        .unwrap_or_else(|_| Api::Custom(requested_api.to_string()))
        .to_string();
    let base_url = config.base_url.clone().unwrap_or_else(|| {
        defaults.map_or_else(
            || {
                api_fallback_base_url(&api)
                    .unwrap_or("https://api.openai.com/v1")
                    .to_string()
            },
            |value| {
                if value.base_url.is_empty() {
                    api_fallback_base_url(&api).unwrap_or_default().to_string()
                } else {
                    value.base_url.to_string()
                }
            },
        )
    });
    let auth_header = config
        .auth_header
        .unwrap_or_else(|| defaults.is_some_and(|value| value.auth_header));
    (defaults, api, base_url, auth_header)
}

fn legacy_provider_ids() -> HashSet<String> {
    legacy_generated_models()
        .iter()
        .map(|model| {
            let provider = model.provider.trim();
            canonical_provider_id(provider)
                .unwrap_or(provider)
                .to_ascii_lowercase()
        })
        .collect()
}

fn resolve_provider_api_key_cached(
    auth: &impl ModelCredentialResolver,
    canonical_provider: &str,
    provider: &str,
    canonical_cache: &mut HashMap<String, Option<String>>,
    provider_cache: &mut HashMap<String, Option<String>>,
) -> Option<String> {
    let canonical_key = canonical_provider.to_ascii_lowercase();
    let canonical_result = canonical_cache
        .entry(canonical_key)
        .or_insert_with(|| auth.resolve_api_key(canonical_provider, None))
        .clone();

    if canonical_result.is_some() || canonical_provider.eq_ignore_ascii_case(provider) {
        return canonical_result;
    }

    provider_cache
        .entry(provider.to_ascii_lowercase())
        .or_insert_with(|| auth.resolve_api_key(provider, None))
        .clone()
}

/// Native-adapter providers whose request path resolves its own endpoint and
/// therefore does not need a non-empty seed `base_url` to be routable. Today
/// this is only `github-copilot`, whose adapter discovers the Copilot proxy
/// endpoint via GitHub's token-exchange API (see `providers::copilot`). Such
/// providers can be safely seeded from the upstream snapshot /
/// models-override.json even though their seed default carries an empty
/// `base_url`. Contrast with `azure-openai` / `sap-ai-core`, which also have an
/// empty seed `base_url` but require a user-supplied resource/base_url and must
/// stay excluded. (#100)
fn provider_self_routes_without_base_url(canonical_provider: &str) -> bool {
    matches!(
        canonical_provider.to_ascii_lowercase().as_str(),
        "github-copilot"
    )
}

fn append_upstream_nonlegacy_models(
    auth: &impl ModelCredentialResolver,
    models: &mut Vec<ModelEntry>,
    seen: &mut HashSet<String>,
    canonical_api_key_cache: &mut HashMap<String, Option<String>>,
    provider_api_key_cache: &mut HashMap<String, Option<String>>,
) {
    let legacy_providers = legacy_provider_ids();
    for (provider, ids) in upstream_provider_model_ids() {
        let provider = provider.trim();
        if provider.is_empty() {
            continue;
        }
        let canonical_provider = canonical_provider_id(provider).unwrap_or(provider);
        if legacy_providers.contains(&canonical_provider.to_ascii_lowercase()) {
            // Native-adapter legacy providers (openai-codex, github-copilot,
            // google-gemini-cli, google-antigravity) should still honor
            // snapshot / models-override.json entries so their model IDs become
            // resolvable registry entries instead of dead autocomplete
            // candidates. We admit them only when their native adapter can
            // route a request without per-user configuration. Providers whose
            // seed default has an empty base_url AND lack a self-resolving
            // native adapter (notably azure-openai / sap-ai-core, which need a
            // user-supplied resource/base_url) would fail at request time, so
            // they stay excluded. (#100)
            match native_adapter_seed_defaults(canonical_provider) {
                Some(seed)
                    if !seed.base_url.is_empty()
                        || provider_self_routes_without_base_url(canonical_provider) =>
                {
                    // fall through and admit the snapshot/override entries
                }
                _ => continue,
            }
        }

        let Some(defaults) = ad_hoc_provider_defaults(canonical_provider)
            .or_else(|| native_adapter_seed_defaults(canonical_provider))
        else {
            continue;
        };

        let api_key = resolve_provider_api_key_cached(
            auth,
            canonical_provider,
            provider,
            canonical_api_key_cache,
            provider_api_key_cache,
        );

        for model_id in ids {
            let normalized_model_id =
                canonicalize_model_id_for_provider(canonical_provider, model_id);
            if normalized_model_id.is_empty() {
                continue;
            }
            let dedupe_key = format!(
                "{}::{}",
                canonical_provider.to_ascii_lowercase(),
                normalized_model_id.to_ascii_lowercase()
            );
            if !seen.insert(dedupe_key) {
                continue;
            }

            let reasoning = effective_reasoning(&normalized_model_id, defaults.reasoning);
            models.push(ModelEntry {
                model: Model {
                    id: normalized_model_id.clone(),
                    name: normalized_model_id.clone(),
                    api: defaults.api.to_string(),
                    provider: canonical_provider.to_string(),
                    base_url: defaults.base_url.to_string(),
                    reasoning,
                    input: defaults.input.to_vec(),
                    cost: ModelCost {
                        input: 0.0,
                        output: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                    },
                    context_window: defaults.context_window,
                    max_tokens: defaults.max_tokens,
                    headers: HashMap::new(),
                },
                api_key: api_key.clone(),
                headers: HashMap::new(),
                auth_header: defaults.auth_header,
                compat: None,
                oauth_config: None,
            });
        }
    }
}

#[allow(clippy::too_many_lines)]
fn built_in_models(
    auth: &impl ModelCredentialResolver,
    mode: ModelRegistryLoadMode,
) -> Vec<ModelEntry> {
    let mut models = Vec::with_capacity(legacy_generated_models().len() + 8);
    let mut seen = HashSet::new();
    let mut canonical_api_key_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut provider_api_key_cache: HashMap<String, Option<String>> = HashMap::new();

    for legacy in legacy_generated_models() {
        let provider = legacy.provider.trim();
        if provider.is_empty() {
            continue;
        }

        let normalized_model_id = canonicalize_model_id_for_provider(provider, &legacy.id);
        if normalized_model_id.is_empty() {
            continue;
        }

        let dedupe_key = format!(
            "{}::{}",
            provider.to_ascii_lowercase(),
            normalized_model_id.to_ascii_lowercase()
        );
        if !seen.insert(dedupe_key) {
            continue;
        }

        let routing_defaults = provider_routing_defaults(provider);
        let api_string = if mode == ModelRegistryLoadMode::Full {
            legacy
                .api
                .parse::<Api>()
                .unwrap_or_else(|_| Api::Custom(legacy.api.clone()))
                .to_string()
        } else {
            legacy.api.clone()
        };

        let base_url = if mode == ModelRegistryLoadMode::Full {
            if !legacy.base_url.trim().is_empty() {
                legacy.base_url.trim().to_string()
            } else if let Some(default_base) = routing_defaults
                .map(|defaults| defaults.base_url)
                .or_else(|| api_fallback_base_url(api_string.as_str()))
            {
                default_base.to_string()
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        let input = {
            let parsed = parse_input_types(&legacy.input);
            if parsed.is_empty() {
                routing_defaults
                    .map_or_else(|| vec![InputType::Text], |defaults| defaults.input.to_vec())
            } else {
                parsed
            }
        };

        let auth_header = match api_string.as_str() {
            "openai-codex-responses" | "google-gemini-cli" => true,
            _ => routing_defaults.is_some_and(|defaults| defaults.auth_header),
        };

        let canonical_provider = canonical_provider_id(provider).unwrap_or(provider);
        let api_key = resolve_provider_api_key_cached(
            auth,
            canonical_provider,
            provider,
            &mut canonical_api_key_cache,
            &mut provider_api_key_cache,
        );

        let default_cost = ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        };
        let model_name = if mode == ModelRegistryLoadMode::Full && !legacy.name.trim().is_empty() {
            legacy.name.clone()
        } else {
            normalized_model_id.clone()
        };
        let model_headers = if mode == ModelRegistryLoadMode::Full {
            legacy.headers.clone()
        } else {
            HashMap::new()
        };
        let entry_headers = if mode == ModelRegistryLoadMode::Full {
            legacy.headers.clone()
        } else {
            HashMap::new()
        };

        models.push(ModelEntry {
            model: Model {
                id: normalized_model_id.clone(),
                name: model_name,
                api: api_string,
                provider: provider.to_string(),
                base_url,
                // The catalog is authoritative for per-model reasoning: every
                // generated entry carries an explicit `reasoning` flag, so honor
                // it directly rather than letting the built-in `model_is_reasoning`
                // heuristic override it (gh #117 — a stale heuristic must not win
                // over correct catalog data, e.g. #114). The heuristic is only a
                // fallback for the rare entry that omits the field.
                reasoning: legacy
                    .reasoning
                    .unwrap_or_else(|| effective_reasoning(&normalized_model_id, false)),
                input,
                cost: if mode == ModelRegistryLoadMode::Full {
                    legacy.cost.clone().unwrap_or_else(|| default_cost.clone())
                } else {
                    default_cost
                },
                context_window: legacy.context_window.unwrap_or_else(|| {
                    routing_defaults.map_or(128_000, |defaults| defaults.context_window)
                }),
                max_tokens: legacy.max_tokens.unwrap_or_else(|| {
                    routing_defaults.map_or(16_384, |defaults| defaults.max_tokens)
                }),
                headers: model_headers,
            },
            api_key,
            headers: entry_headers,
            auth_header,
            compat: if mode == ModelRegistryLoadMode::Full {
                legacy.compat.clone()
            } else {
                None
            },
            oauth_config: None,
        });
    }

    append_upstream_nonlegacy_models(
        auth,
        &mut models,
        &mut seen,
        &mut canonical_api_key_cache,
        &mut provider_api_key_cache,
    );

    // Ensure the latest Sonnet alias is present in built-ins.
    if !models.iter().any(|entry| {
        entry.model.provider == "anthropic"
            && (entry.model.id == "claude-sonnet-4-6"
                || entry.model.id == "claude-sonnet-4-6-20260217")
    }) {
        models.push(ModelEntry {
            model: Model {
                id: "claude-sonnet-4-6".to_string(),
                name: "Claude Sonnet 4.6".to_string(),
                api: if mode == ModelRegistryLoadMode::Full {
                    Api::AnthropicMessages.to_string()
                } else {
                    "anthropic-messages".to_string()
                },
                provider: "anthropic".to_string(),
                base_url: if mode == ModelRegistryLoadMode::Full {
                    "https://api.anthropic.com/v1/messages".to_string()
                } else {
                    String::new()
                },
                reasoning: true,
                input: vec![InputType::Text, InputType::Image],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 1_000_000,
                max_tokens: 128_000,
                headers: HashMap::new(),
            },
            api_key: resolve_provider_api_key_cached(
                auth,
                "anthropic",
                "anthropic",
                &mut canonical_api_key_cache,
                &mut provider_api_key_cache,
            ),
            headers: HashMap::new(),
            auth_header: false,
            compat: None,
            oauth_config: None,
        });
    }

    // Seed the GPT-5.6 family (Sol / Terra / Luna, released 2026-07-09) for
    // direct OpenAI API-key routing and ChatGPT/Codex OAuth routing. The bare
    // `gpt-5.6` API alias resolves to Sol. These IDs post-date the bundled
    // upstream catalog snapshot, so without explicit seeds they are missing
    // from listing, lookup, and autocomplete. Costs and the 1.05M context
    // window follow the current OpenAI model catalog; Codex entries keep the
    // existing zero cache-write convention. See pi_agent_rust#135.
    for (id, name, input, output, cache_read, cache_write, include_codex) in [
        ("gpt-5.6", "GPT-5.6", 5.0, 30.0, 0.5, 6.25, false),
        ("gpt-5.6-sol", "GPT-5.6 Sol", 5.0, 30.0, 0.5, 6.25, true),
        ("gpt-5.6-terra", "GPT-5.6 Terra", 2.0, 12.0, 0.2, 2.5, true),
        ("gpt-5.6-luna", "GPT-5.6 Luna", 0.2, 1.2, 0.02, 0.25, true),
    ] {
        let providers: &[&str] = if include_codex {
            &["openai", "openai-codex"]
        } else {
            &["openai"]
        };
        for &provider in providers {
            if models
                .iter()
                .any(|entry| entry.model.provider == provider && entry.model.id == id)
            {
                continue;
            }

            let is_codex = provider == "openai-codex";
            let (api, base_url, display_name) = if is_codex {
                (
                    if mode == ModelRegistryLoadMode::Full {
                        Api::OpenAICodexResponses.to_string()
                    } else {
                        "openai-codex-responses".to_string()
                    },
                    if mode == ModelRegistryLoadMode::Full {
                        "https://chatgpt.com/backend-api".to_string()
                    } else {
                        String::new()
                    },
                    format!("{name} Codex"),
                )
            } else {
                (
                    if mode == ModelRegistryLoadMode::Full {
                        Api::OpenAIResponses.to_string()
                    } else {
                        "openai-responses".to_string()
                    },
                    if mode == ModelRegistryLoadMode::Full {
                        "https://api.openai.com/v1".to_string()
                    } else {
                        String::new()
                    },
                    name.to_string(),
                )
            };

            models.push(ModelEntry {
                model: Model {
                    id: id.to_string(),
                    name: display_name,
                    api,
                    provider: provider.to_string(),
                    base_url,
                    reasoning: true,
                    input: vec![InputType::Text, InputType::Image],
                    cost: ModelCost {
                        input,
                        output,
                        cache_read,
                        cache_write: if is_codex { 0.0 } else { cache_write },
                    },
                    context_window: 1_050_000,
                    max_tokens: 128_000,
                    headers: HashMap::new(),
                },
                api_key: resolve_provider_api_key_cached(
                    auth,
                    provider,
                    provider,
                    &mut canonical_api_key_cache,
                    &mut provider_api_key_cache,
                ),
                headers: HashMap::new(),
                auth_header: true,
                compat: None,
                oauth_config: None,
            });
        }
    }

    // Ensure the latest GPT-5 default exists for OpenAI routing.
    //
    // The legacy catalog can lag behind upstream model IDs; we add a
    // conservative seed so listing, lookup, and autocomplete stay current.
    if !models
        .iter()
        .any(|entry| entry.model.provider == "openai" && entry.model.id == "gpt-5.5")
    {
        models.push(ModelEntry {
            model: Model {
                id: "gpt-5.5".to_string(),
                name: "GPT-5.5".to_string(),
                api: if mode == ModelRegistryLoadMode::Full {
                    Api::OpenAIResponses.to_string()
                } else {
                    "openai-responses".to_string()
                },
                provider: "openai".to_string(),
                base_url: if mode == ModelRegistryLoadMode::Full {
                    "https://api.openai.com/v1".to_string()
                } else {
                    String::new()
                },
                reasoning: true,
                input: vec![InputType::Text, InputType::Image],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 1_000_000,
                max_tokens: 128_000,
                headers: HashMap::new(),
            },
            api_key: resolve_provider_api_key_cached(
                auth,
                "openai",
                "openai",
                &mut canonical_api_key_cache,
                &mut provider_api_key_cache,
            ),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        });
    }

    if !models
        .iter()
        .any(|entry| entry.model.provider == "openai" && entry.model.id == "gpt-5.4")
    {
        models.push(ModelEntry {
            model: Model {
                id: "gpt-5.4".to_string(),
                name: "GPT-5.4".to_string(),
                api: if mode == ModelRegistryLoadMode::Full {
                    Api::OpenAIResponses.to_string()
                } else {
                    "openai-responses".to_string()
                },
                provider: "openai".to_string(),
                base_url: if mode == ModelRegistryLoadMode::Full {
                    "https://api.openai.com/v1".to_string()
                } else {
                    String::new()
                },
                reasoning: true,
                input: vec![InputType::Text, InputType::Image],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 400_000,
                max_tokens: 128_000,
                headers: HashMap::new(),
            },
            api_key: resolve_provider_api_key_cached(
                auth,
                "openai",
                "openai",
                &mut canonical_api_key_cache,
                &mut provider_api_key_cache,
            ),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        });
    }

    // Ensure the latest Codex default exists for OpenAI Codex (ChatGPT) routing.
    //
    // The legacy catalog can lag behind upstream model IDs; we use a conservative
    // seed here to keep the default selection stable.
    if !models
        .iter()
        .any(|entry| entry.model.provider == "openai-codex" && entry.model.id == "gpt-5.5")
    {
        models.push(ModelEntry {
            model: Model {
                id: "gpt-5.5".to_string(),
                name: "GPT-5.5 Codex".to_string(),
                api: if mode == ModelRegistryLoadMode::Full {
                    Api::OpenAICodexResponses.to_string()
                } else {
                    "openai-codex-responses".to_string()
                },
                provider: "openai-codex".to_string(),
                base_url: if mode == ModelRegistryLoadMode::Full {
                    "https://chatgpt.com/backend-api".to_string()
                } else {
                    String::new()
                },
                reasoning: true,
                input: vec![InputType::Text, InputType::Image],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 1_000_000,
                max_tokens: 128_000,
                headers: HashMap::new(),
            },
            api_key: resolve_provider_api_key_cached(
                auth,
                "openai-codex",
                "openai-codex",
                &mut canonical_api_key_cache,
                &mut provider_api_key_cache,
            ),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        });
    }

    if !models
        .iter()
        .any(|entry| entry.model.provider == "openai-codex" && entry.model.id == "gpt-5.4")
    {
        models.push(ModelEntry {
            model: Model {
                id: "gpt-5.4".to_string(),
                name: "GPT-5.4 Codex".to_string(),
                api: if mode == ModelRegistryLoadMode::Full {
                    Api::OpenAICodexResponses.to_string()
                } else {
                    "openai-codex-responses".to_string()
                },
                provider: "openai-codex".to_string(),
                base_url: if mode == ModelRegistryLoadMode::Full {
                    "https://chatgpt.com/backend-api".to_string()
                } else {
                    String::new()
                },
                reasoning: true,
                input: vec![InputType::Text, InputType::Image],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 272_000,
                max_tokens: 128_000,
                headers: HashMap::new(),
            },
            api_key: resolve_provider_api_key_cached(
                auth,
                "openai-codex",
                "openai-codex",
                &mut canonical_api_key_cache,
                &mut provider_api_key_cache,
            ),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        });
    }

    if !models
        .iter()
        .any(|entry| entry.model.provider == "openai-codex" && entry.model.id == "gpt-5.2-codex")
    {
        models.push(ModelEntry {
            model: Model {
                id: "gpt-5.2-codex".to_string(),
                name: "GPT-5.2 Codex".to_string(),
                api: if mode == ModelRegistryLoadMode::Full {
                    Api::OpenAICodexResponses.to_string()
                } else {
                    "openai-codex-responses".to_string()
                },
                provider: "openai-codex".to_string(),
                base_url: if mode == ModelRegistryLoadMode::Full {
                    "https://chatgpt.com/backend-api".to_string()
                } else {
                    String::new()
                },
                reasoning: true,
                input: vec![InputType::Text, InputType::Image],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 272_000,
                max_tokens: 128_000,
                headers: HashMap::new(),
            },
            api_key: resolve_provider_api_key_cached(
                auth,
                "openai-codex",
                "openai-codex",
                &mut canonical_api_key_cache,
                &mut provider_api_key_cache,
            ),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        });
    }

    // Keep the prior Codex default available until the bundled legacy catalog catches up.
    if !models
        .iter()
        .any(|entry| entry.model.provider == "openai-codex" && entry.model.id == "gpt-5.3-codex")
    {
        models.push(ModelEntry {
            model: Model {
                id: "gpt-5.3-codex".to_string(),
                name: "GPT-5.3 Codex".to_string(),
                api: if mode == ModelRegistryLoadMode::Full {
                    Api::OpenAICodexResponses.to_string()
                } else {
                    "openai-codex-responses".to_string()
                },
                provider: "openai-codex".to_string(),
                base_url: if mode == ModelRegistryLoadMode::Full {
                    "https://chatgpt.com/backend-api".to_string()
                } else {
                    String::new()
                },
                reasoning: true,
                input: vec![InputType::Text, InputType::Image],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 272_000,
                max_tokens: 128_000,
                headers: HashMap::new(),
            },
            api_key: resolve_provider_api_key_cached(
                auth,
                "openai-codex",
                "openai-codex",
                &mut canonical_api_key_cache,
                &mut provider_api_key_cache,
            ),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        });
    }

    // Ensure the latest Codex Spark variant exists for OpenAI Codex routing.
    if !models.iter().any(|entry| {
        entry.model.provider == "openai-codex" && entry.model.id == "gpt-5.3-codex-spark"
    }) {
        models.push(ModelEntry {
            model: Model {
                id: "gpt-5.3-codex-spark".to_string(),
                name: "GPT-5.3 Codex Spark".to_string(),
                api: if mode == ModelRegistryLoadMode::Full {
                    Api::OpenAICodexResponses.to_string()
                } else {
                    "openai-codex-responses".to_string()
                },
                provider: "openai-codex".to_string(),
                base_url: if mode == ModelRegistryLoadMode::Full {
                    "https://chatgpt.com/backend-api".to_string()
                } else {
                    String::new()
                },
                reasoning: true,
                input: vec![InputType::Text, InputType::Image],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 272_000,
                max_tokens: 128_000,
                headers: HashMap::new(),
            },
            api_key: resolve_provider_api_key_cached(
                auth,
                "openai-codex",
                "openai-codex",
                &mut canonical_api_key_cache,
                &mut provider_api_key_cache,
            ),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        });
    }

    if !models.iter().any(|entry| {
        entry.model.provider == "google-gemini-cli" && entry.model.id == "gemini-2.5-pro"
    }) {
        models.push(ModelEntry {
            model: Model {
                id: "gemini-2.5-pro".to_string(),
                name: "Gemini 2.5 Pro".to_string(),
                api: "google-gemini-cli".to_string(),
                provider: "google-gemini-cli".to_string(),
                base_url: if mode == ModelRegistryLoadMode::Full {
                    GOOGLE_GEMINI_CLI_API_URL.to_string()
                } else {
                    String::new()
                },
                reasoning: true,
                input: vec![InputType::Text, InputType::Image],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 8192,
                headers: HashMap::new(),
            },
            api_key: resolve_provider_api_key_cached(
                auth,
                "google",
                "google-gemini-cli",
                &mut canonical_api_key_cache,
                &mut provider_api_key_cache,
            ),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        });
    }

    if !models.iter().any(|entry| {
        entry.model.provider == "google-antigravity" && entry.model.id == "gemini-3-flash"
    }) {
        models.push(ModelEntry {
            model: Model {
                id: "gemini-3-flash".to_string(),
                name: "Gemini 3 Flash".to_string(),
                api: "google-gemini-cli".to_string(),
                provider: "google-antigravity".to_string(),
                base_url: if mode == ModelRegistryLoadMode::Full {
                    GOOGLE_ANTIGRAVITY_API_URL.to_string()
                } else {
                    String::new()
                },
                reasoning: true,
                input: vec![InputType::Text, InputType::Image],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 8192,
                headers: HashMap::new(),
            },
            api_key: resolve_provider_api_key_cached(
                auth,
                "google",
                "google-antigravity",
                &mut canonical_api_key_cache,
                &mut provider_api_key_cache,
            ),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        });
    }

    // Sort for deterministic find_by_id: canonical providers first, then alphabetical.
    models.sort_by(|a, b| {
        let priority = |e: &ModelEntry| -> u8 {
            let p = e.model.provider.as_str();
            let id = e.model.id.as_str();
            // Canonical provider gets priority 0
            let is_canonical = (id.starts_with("claude") && p == "anthropic")
                || (id.starts_with("gpt-") && p == "openai")
                || (id.starts_with("o1") && p == "openai")
                || (id.starts_with("o3") && p == "openai")
                || (id.starts_with("o4") && p == "openai")
                || (id.starts_with("gemini") && p == "google")
                || (id.starts_with("command") && p == "cohere");
            u8::from(!is_canonical)
        };
        priority(a)
            .cmp(&priority(b))
            .then_with(|| a.model.provider.cmp(&b.model.provider))
            .then_with(|| a.model.id.cmp(&b.model.id))
    });

    models
}

fn canonical_provider_key(provider: &str) -> String {
    canonical_provider_id(provider)
        .unwrap_or(provider)
        .to_ascii_lowercase()
}

fn fetched_model_key(provider: &str, model_id: &str) -> (String, String) {
    let canonical_provider = canonical_provider_key(provider);
    let canonical_model = canonicalize_model_id_for_provider(&canonical_provider, model_id);
    (canonical_provider, canonical_model.to_ascii_lowercase())
}

/// Apply the exact membership returned by live discovery while retaining rich
/// built-in metadata for model IDs Pi already knows.
///
/// Newly discovered IDs use provider defaults; disappeared IDs stay absent.
/// This avoids turning a persisted catalog refresh into a silent
/// capability/context-window downgrade for every known model in that provider.
fn apply_fetched_models(
    auth: &impl ModelCredentialResolver,
    models: &mut Vec<ModelEntry>,
    config: &ModelsConfig,
    manual_config: Option<&ModelsConfig>,
    base_dir: Option<&Path>,
) {
    let config = ModelsConfig {
        providers: config
            .providers
            .iter()
            .filter_map(|(provider, config)| {
                let canonical = canonical_provider_key(provider);
                let has_builtin_route = custom_provider_defaults(provider).is_some()
                    || models.iter().any(|entry| {
                        canonical_provider_key(&entry.model.provider) == canonical
                    });
                let has_manual_route = manual_config.is_some_and(|manual| {
                    manual.providers.iter().any(|(candidate, candidate_config)| {
                        canonical_provider_key(candidate) == canonical
                            && provider_has_catalog_route(candidate, candidate_config)
                    })
                });
                if has_builtin_route || has_manual_route {
                    Some((provider.clone(), config.clone()))
                } else {
                    tracing::warn!(
                        provider = %provider,
                        "Ignoring generated model membership without a built-in or models.json provider route"
                    );
                    None
                }
            })
            .collect(),
    };
    let fetched_providers = config
        .providers
        .keys()
        .map(|provider| canonical_provider_key(provider))
        .collect::<HashSet<_>>();
    let preserved = models
        .iter()
        .filter(|entry| fetched_providers.contains(&canonical_provider_key(&entry.model.provider)))
        .map(|entry| {
            (
                fetched_model_key(&entry.model.provider, &entry.model.id),
                entry.clone(),
            )
        })
        .collect::<HashMap<_, _>>();

    apply_custom_models(auth, models, &config, base_dir);

    for entry in models
        .iter_mut()
        .filter(|entry| fetched_providers.contains(&canonical_provider_key(&entry.model.provider)))
    {
        if let Some(existing) =
            preserved.get(&fetched_model_key(&entry.model.provider, &entry.model.id))
        {
            entry.clone_from(existing);
        }
    }
}

fn apply_custom_models(
    auth: &impl ModelCredentialResolver,
    models: &mut Vec<ModelEntry>,
    config: &ModelsConfig,
    base_dir: Option<&Path>,
) {
    apply_custom_models_with_provider_headers(auth, models, config, base_dir, None);
}

#[allow(clippy::too_many_lines)]
fn apply_custom_models_with_provider_headers(
    auth: &impl ModelCredentialResolver,
    models: &mut Vec<ModelEntry>,
    config: &ModelsConfig,
    base_dir: Option<&Path>,
    provider_headers_snapshot: Option<&ProviderHeadersSnapshot>,
) {
    for (provider_id, provider_cfg) in &config.providers {
        let provider_id_str = provider_id.as_str();
        let (provider_defaults, provider_api_string, provider_base, auth_header) =
            resolved_provider_transport(provider_id, provider_cfg);

        let provider_headers = provider_headers_snapshot.map_or_else(
            || resolve_headers_with_base(provider_cfg.headers.as_ref(), base_dir),
            |snapshot| snapshot.get(provider_id).cloned().unwrap_or_default(),
        );
        let canonical_provider = canonical_provider_id(provider_id).unwrap_or(provider_id_str);
        let provider_matches = |candidate_provider: &str| {
            let candidate_canonical =
                canonical_provider_id(candidate_provider).unwrap_or(candidate_provider);
            candidate_provider.eq_ignore_ascii_case(provider_id_str)
                || candidate_provider.eq_ignore_ascii_case(canonical_provider)
                || candidate_canonical.eq_ignore_ascii_case(provider_id_str)
                || candidate_canonical.eq_ignore_ascii_case(canonical_provider)
        };
        let provider_key = normalize_api_key_opt(auth.resolve_api_key(canonical_provider, None))
            .or_else(|| {
                provider_cfg
                    .api_key
                    .as_deref()
                    .and_then(|value| resolve_value_with_base(value, base_dir))
            });

        if provider_defaults.is_some() {
            tracing::debug!(
                event = "pi.provider.schema_defaults",
                provider = %provider_id,
                canonical_provider = %canonical_provider,
                api = %provider_api_string,
                base_url = %provider_base,
                auth_header,
                "Applied provider metadata defaults"
            );
        }

        let has_models = provider_cfg.models.as_ref().is_some();
        let is_override = !has_models;

        if is_override {
            for entry in models
                .iter_mut()
                .filter(|m| provider_matches(&m.model.provider))
            {
                // Only override base_url and api if explicitly set in models.json.
                // Otherwise keep the built-in defaults (e.g. anthropic's /v1/messages URL).
                if provider_cfg.base_url.is_some() {
                    entry.model.base_url.clone_from(&provider_base);
                }
                if provider_cfg.api.is_some() {
                    entry.model.api.clone_from(&provider_api_string);
                }
                if should_apply_headers_override(provider_cfg.headers.as_ref(), &provider_headers) {
                    entry.headers.clone_from(&provider_headers);
                }
                if provider_key.is_some() {
                    entry.api_key.clone_from(&provider_key);
                }
                if provider_cfg.compat.is_some() {
                    entry.compat.clone_from(&provider_cfg.compat);
                }
                if provider_cfg.auth_header.is_some() {
                    entry.auth_header = auth_header;
                }
            }
            continue;
        }

        // Remove built-in provider models if fully overridden
        models.retain(|m| !provider_matches(&m.model.provider));

        let mut normalized_provider_ids = HashSet::new();
        for model_cfg in provider_cfg.models.clone().unwrap_or_default() {
            let normalized_model_id =
                canonicalize_model_id_for_provider(provider_id, &model_cfg.id);
            if normalized_model_id.is_empty() {
                tracing::warn!(
                    provider = %provider_id,
                    model_id = %model_cfg.id,
                    "Skipping model with empty normalized id"
                );
                continue;
            }

            if canonical_provider == "openrouter"
                && !normalized_provider_ids.insert(normalized_model_id.to_ascii_lowercase())
            {
                tracing::warn!(
                    provider = %provider_id,
                    model_id = %normalized_model_id,
                    "Skipping duplicate OpenRouter model id after alias normalization"
                );
                continue;
            }

            let model_api = model_cfg
                .api
                .as_deref()
                .unwrap_or(provider_api_string.as_str());
            let model_api_parsed: Api = model_api
                .parse()
                .unwrap_or_else(|_| Api::Custom(model_api.to_string()));
            let model_headers = merge_headers(
                &provider_headers,
                resolve_headers_with_base(model_cfg.headers.as_ref(), base_dir),
            );
            let default_input_types = provider_defaults
                .map_or_else(|| vec![InputType::Text], |defaults| defaults.input.to_vec());
            let input_types = model_cfg.input.as_ref().map_or_else(
                || default_input_types.clone(),
                |input| {
                    input
                        .iter()
                        .filter_map(|i| match i.as_str() {
                            "text" => Some(InputType::Text),
                            "image" => Some(InputType::Image),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                },
            );
            let input_types = if input_types.is_empty() {
                default_input_types
            } else {
                input_types
            };
            let default_reasoning = provider_defaults.is_some_and(|defaults| defaults.reasoning);
            let default_context_window =
                provider_defaults.map_or(128_000, |defaults| defaults.context_window);
            let default_max_tokens =
                provider_defaults.map_or(16_384, |defaults| defaults.max_tokens);

            let model = Model {
                id: normalized_model_id.clone(),
                name: model_cfg
                    .name
                    .clone()
                    .unwrap_or_else(|| normalized_model_id.clone()),
                api: model_api_parsed.to_string(),
                provider: provider_id.clone(),
                base_url: provider_base.clone(),
                reasoning: model_cfg.reasoning.unwrap_or_else(|| {
                    effective_reasoning(&normalized_model_id, default_reasoning)
                }),
                input: input_types,
                cost: model_cfg.cost.clone().unwrap_or(ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                }),
                context_window: model_cfg.context_window.unwrap_or(default_context_window),
                max_tokens: model_cfg.max_tokens.unwrap_or(default_max_tokens),
                headers: HashMap::new(),
            };

            models.push(ModelEntry {
                model,
                api_key: provider_key.clone(),
                headers: model_headers,
                auth_header,
                compat: merge_compat(provider_cfg.compat.as_ref(), model_cfg.compat.as_ref()),
                oauth_config: None,
            });
        }
    }
}

fn merge_compat(
    provider_compat: Option<&CompatConfig>,
    model_compat: Option<&CompatConfig>,
) -> Option<CompatConfig> {
    match (provider_compat, model_compat) {
        (None, None) => None,
        (Some(provider), None) => Some(provider.clone()),
        (None, Some(model)) => Some(model.clone()),
        (Some(provider), Some(model)) => {
            let custom_headers = match (&provider.custom_headers, &model.custom_headers) {
                (None, None) => None,
                (Some(headers), None) | (None, Some(headers)) => Some(headers.clone()),
                (Some(provider_headers), Some(model_headers)) => {
                    let mut merged = provider_headers.clone();
                    for (key, value) in model_headers {
                        merged.insert(key.clone(), value.clone());
                    }
                    Some(merged)
                }
            };

            Some(CompatConfig {
                supports_store: model.supports_store.or(provider.supports_store),
                supports_developer_role: model
                    .supports_developer_role
                    .or(provider.supports_developer_role),
                supports_reasoning_effort: model
                    .supports_reasoning_effort
                    .or(provider.supports_reasoning_effort),
                supports_usage_in_streaming: model
                    .supports_usage_in_streaming
                    .or(provider.supports_usage_in_streaming),
                supports_tools: model.supports_tools.or(provider.supports_tools),
                supports_streaming: model.supports_streaming.or(provider.supports_streaming),
                supports_parallel_tool_calls: model
                    .supports_parallel_tool_calls
                    .or(provider.supports_parallel_tool_calls),
                max_tokens_field: model
                    .max_tokens_field
                    .clone()
                    .or_else(|| provider.max_tokens_field.clone()),
                system_role_name: model
                    .system_role_name
                    .clone()
                    .or_else(|| provider.system_role_name.clone()),
                stop_reason_field: model
                    .stop_reason_field
                    .clone()
                    .or_else(|| provider.stop_reason_field.clone()),
                custom_headers,
                open_router_routing: model
                    .open_router_routing
                    .clone()
                    .or_else(|| provider.open_router_routing.clone()),
                vercel_gateway_routing: model
                    .vercel_gateway_routing
                    .clone()
                    .or_else(|| provider.vercel_gateway_routing.clone()),
                thinking_level_map: model
                    .thinking_level_map
                    .clone()
                    .or_else(|| provider.thinking_level_map.clone()),
                force_adaptive_thinking: model
                    .force_adaptive_thinking
                    .or(provider.force_adaptive_thinking),
                thinking_format: model
                    .thinking_format
                    .clone()
                    .or_else(|| provider.thinking_format.clone()),
            })
        }
    }
}

fn merge_headers(
    base: &HashMap<String, String>,
    override_headers: HashMap<String, String>,
) -> HashMap<String, String> {
    let mut merged = base.clone();
    for (k, v) in override_headers {
        merged.insert(k, v);
    }
    merged
}

fn should_apply_headers_override(
    configured_headers: Option<&HashMap<String, String>>,
    resolved_headers: &HashMap<String, String>,
) -> bool {
    configured_headers.is_some_and(|headers| headers.is_empty() || !resolved_headers.is_empty())
}

#[cfg(test)]
fn resolve_headers(headers: Option<&HashMap<String, String>>) -> HashMap<String, String> {
    resolve_headers_with_base(headers, None)
}

fn resolve_headers_with_base(
    headers: Option<&HashMap<String, String>>,
    base_dir: Option<&Path>,
) -> HashMap<String, String> {
    let mut resolved = HashMap::new();
    if let Some(headers) = headers {
        for (k, v) in headers {
            if let Some(val) = resolve_value_with_base(v, base_dir) {
                resolved.insert(k.clone(), val);
            }
        }
    }
    resolved
}

fn has_complete_custom_authorization_header(headers: &HashMap<String, String>) -> bool {
    let mut authorization_headers = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("Authorization"));
    let Some((_, value)) = authorization_headers.next() else {
        return false;
    };

    !value.trim().is_empty() && authorization_headers.next().is_none()
}

fn prepare_model_catalog_headers(
    headers: Option<&HashMap<String, String>>,
    base_dir: Option<&Path>,
) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut deferred = headers.cloned().unwrap_or_default();
    let mut authorization_names = deferred
        .keys()
        .filter(|name| name.eq_ignore_ascii_case("Authorization"));
    let Some(authorization_name) = authorization_names.next().cloned() else {
        return (HashMap::new(), deferred);
    };
    if authorization_names.next().is_some() {
        return (HashMap::new(), deferred);
    }

    let Some(unresolved_value) = deferred.remove(&authorization_name) else {
        return (HashMap::new(), deferred);
    };
    let resolved = resolve_value_with_base(&unresolved_value, base_dir)
        .map(|value| HashMap::from([(authorization_name, value)]))
        .unwrap_or_default();
    (resolved, deferred)
}

fn resolve_provider_headers_snapshot(
    config: &ModelsConfig,
    base_dir: Option<&Path>,
) -> ProviderHeadersSnapshot {
    config
        .providers
        .iter()
        .map(|(provider, provider_config)| {
            (
                provider.clone(),
                resolve_headers_with_base(provider_config.headers.as_ref(), base_dir),
            )
        })
        .collect()
}

#[cfg(test)]
fn resolve_value(value: &str) -> Option<String> {
    resolve_value_with_base(value, None)
}

fn resolve_value_with_base(value: &str, base_dir: Option<&Path>) -> Option<String> {
    resolve_value_with_resolvers(value, base_dir, |var| std::env::var(var).ok())
}

/// Testable helper. Behaves the same as [`resolve_value_with_base`] but with an
/// injectable environment lookup so unit tests can exercise the env-var
/// indirection path without mutating process-wide state (the crate forbids
/// `unsafe`, so `std::env::set_var` cannot be used in tests).
fn resolve_value_with_resolvers<F>(
    value: &str,
    base_dir: Option<&Path>,
    env_lookup: F,
) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(rest) = value.strip_prefix('!') {
        return resolve_shell(rest);
    }

    if let Some(var_name) = value.strip_prefix("env:") {
        if var_name.is_empty() {
            return None;
        }
        return env_lookup(var_name).filter(|v| !v.is_empty());
    }

    if let Some(file_path) = value.strip_prefix("file:") {
        if file_path.is_empty() {
            return None;
        }
        let path = Path::new(file_path);
        let resolved_path = if path.is_absolute() {
            path.to_path_buf()
        } else if let Some(base_dir) = base_dir {
            base_dir.join(path)
        } else {
            path.to_path_buf()
        };
        return std::fs::read_to_string(resolved_path)
            .ok()
            .map(|contents| contents.trim().to_string())
            .filter(|v| !v.is_empty());
    }

    // pi parity (issue #64): values that look like an env var name and end with
    // `_API_KEY` (e.g. `DASHSCOPE_API_KEY`) are treated as a reference to that
    // env var, matching the original `pi` convention. Real provider API keys do
    // not end with the literal suffix `_API_KEY`, so this is a safe signal that
    // the user wants indirection rather than a literal credential.
    if looks_like_api_key_env_var(value) {
        match env_lookup(value) {
            Some(env_value) => {
                let trimmed = env_value.trim();
                if trimmed.is_empty() {
                    tracing::warn!(
                        event = "pi.models.api_key_env_empty",
                        var = value,
                        "models.json apiKey references env var that is set but empty; \
                         falling back to literal value"
                    );
                } else {
                    return Some(trimmed.to_string());
                }
            }
            None => {
                tracing::warn!(
                    event = "pi.models.api_key_env_missing",
                    var = value,
                    "models.json apiKey references an env var that is not set; \
                     falling back to literal value (auth will likely fail)"
                );
            }
        }
    }

    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Whether `value` should be treated as the *name* of an environment variable
/// holding the real API key (matching the original `pi` convention).
///
/// Conservative check: uppercase ASCII letters/digits/underscores, starting
/// with a letter, ending with the literal suffix `_API_KEY`, and at least one
/// character before that suffix (so `_API_KEY` itself is rejected).
fn looks_like_api_key_env_var(value: &str) -> bool {
    const SUFFIX: &str = "_API_KEY";
    if !value.ends_with(SUFFIX) {
        return false;
    }
    let prefix = &value[..value.len() - SUFFIX.len()];
    if prefix.is_empty() {
        return false;
    }
    let mut chars = prefix.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_uppercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn resolve_shell(cmd: &str) -> Option<String> {
    let output = if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", cmd])
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?
    } else {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?
    };

    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        None
    } else {
        Some(stdout)
    }
}

/// Convenience for default models.json path.
pub fn default_models_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("models.json")
}

/// Path for the generated, opt-in provider catalog stored beside
/// `models.json`.
///
/// The generated file is loaded before `models.json`, ensuring
/// that user-authored configuration always wins without either file being
/// merged or rewritten on disk.
pub fn fetched_models_path(models_path: &Path) -> PathBuf {
    models_path.with_file_name("models.fetched.json")
}

#[cfg(windows)]
fn windows_metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
const fn windows_metadata_is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

fn open_regular_file_for_read(path: &Path, allow_final_symlink: bool) -> std::io::Result<fs::File> {
    #[cfg(unix)]
    let access_context = crate::platform::EffectiveModeAccessContext::current()?;
    #[cfg(unix)]
    ensure_model_catalog_ancestors_searchable(path, &access_context)?;

    let initial_metadata = fs::symlink_metadata(path)?;
    let initial_is_symlink = initial_metadata.file_type().is_symlink();
    if windows_metadata_is_reparse_point(&initial_metadata) && !initial_is_symlink {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "model catalog path must not be a Windows reparse point",
        ));
    }
    let resolved_path = if initial_is_symlink && allow_final_symlink {
        fs::canonicalize(path)?
    } else {
        path.to_path_buf()
    };
    let metadata = fs::symlink_metadata(&resolved_path)?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || (initial_is_symlink && !allow_final_symlink)
        || windows_metadata_is_reparse_point(&metadata)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "model catalog path must be a regular, non-symlink file",
        ));
    }

    #[cfg(unix)]
    {
        ensure_model_catalog_ancestors_searchable(&resolved_path, &access_context)?;
        access_context.ensure(
            &metadata,
            &resolved_path,
            crate::platform::UNIX_ACCESS_READ,
            "model catalog read access",
        )?;
    }

    #[cfg(unix)]
    let file = {
        let descriptor = rustix::fs::open(
            &resolved_path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        fs::File::from(descriptor)
    };
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt as _;

        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&resolved_path)?
    };
    #[cfg(not(any(unix, windows)))]
    let file = fs::File::open(&resolved_path)?;

    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() || windows_metadata_is_reparse_point(&opened_metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "model catalog path changed to a non-regular file while opening it",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        if metadata.dev() != opened_metadata.dev() || metadata.ino() != opened_metadata.ino() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "model catalog path changed while opening it",
            ));
        }
        access_context.ensure(
            &opened_metadata,
            &resolved_path,
            crate::platform::UNIX_ACCESS_READ,
            "model catalog read access",
        )?;
    }
    Ok(file)
}

#[cfg(unix)]
fn absolute_model_catalog_path(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(unix)]
fn ensure_model_catalog_lexical_ancestors_searchable(
    path: &Path,
    access_context: &crate::platform::EffectiveModeAccessContext,
) -> std::io::Result<Option<PathBuf>> {
    let absolute = absolute_model_catalog_path(path)?;
    let mut nearest_existing = None;
    let mut ancestor = absolute.parent();
    while let Some(directory) = ancestor {
        if directory.as_os_str().is_empty() {
            break;
        }

        match fs::symlink_metadata(directory) {
            Ok(lexical_metadata) => {
                let metadata = if lexical_metadata.file_type().is_symlink() {
                    fs::metadata(directory).map_err(|error| {
                        if error.kind() == std::io::ErrorKind::NotFound {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!(
                                    "model catalog path contains a dangling symlink ancestor: {}",
                                    directory.display()
                                ),
                            )
                        } else {
                            error
                        }
                    })?
                } else {
                    lexical_metadata
                };
                if nearest_existing.is_none() {
                    nearest_existing = Some(directory.to_path_buf());
                }
                if !metadata.is_dir() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotADirectory,
                        format!(
                            "model catalog path ancestor is not a directory: {}",
                            directory.display()
                        ),
                    ));
                }
                access_context.ensure(
                    &metadata,
                    directory,
                    crate::platform::UNIX_ACCESS_SEARCH,
                    "model catalog path traversal",
                )?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        ancestor = directory.parent();
    }

    Ok(nearest_existing)
}

#[cfg(unix)]
fn ensure_model_catalog_ancestors_searchable(
    path: &Path,
    access_context: &crate::platform::EffectiveModeAccessContext,
) -> std::io::Result<Option<PathBuf>> {
    let nearest_existing = ensure_model_catalog_lexical_ancestors_searchable(path, access_context)?;

    // The lexical walk covers components named by the caller. The resolved
    // walk additionally covers directories reached through symlinked parents
    // or through an allowed final symlink (models.json only).
    let canonical_target = match fs::canonicalize(path) {
        Ok(target) => Some(target),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => nearest_existing
            .as_deref()
            .map(fs::canonicalize)
            .transpose()?,
        Err(error) => return Err(error),
    };
    if let Some(target) = canonical_target {
        ensure_model_catalog_lexical_ancestors_searchable(&target, access_context)?;
    }

    Ok(nearest_existing)
}

#[cfg(unix)]
fn ensure_existing_model_catalog_target_access(
    path: &Path,
    initial_metadata: &fs::Metadata,
    access_context: &crate::platform::EffectiveModeAccessContext,
) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let resolved = fs::canonicalize(path)?;
    ensure_model_catalog_ancestors_searchable(&resolved, access_context)?;
    access_context.ensure(
        initial_metadata,
        path,
        crate::platform::UNIX_ACCESS_READ | crate::platform::UNIX_ACCESS_WRITE,
        "generated model catalog read-write access",
    )?;

    // Open through the caller's original path with NOFOLLOW, then compare
    // against the first metadata snapshot. Opening the canonicalized target
    // would let a final-component swap to a symlink escape the explicit
    // no-symlink policy during this preflight window.
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let opened = fs::File::from(descriptor);
    let opened_metadata = opened.metadata()?;
    if !opened_metadata.is_file()
        || initial_metadata.dev() != opened_metadata.dev()
        || initial_metadata.ino() != opened_metadata.ino()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "generated model catalog target changed during persistence preflight",
        ));
    }
    access_context.ensure(
        &opened_metadata,
        path,
        crate::platform::UNIX_ACCESS_READ | crate::platform::UNIX_ACCESS_WRITE,
        "generated model catalog read-write access",
    )
}

#[cfg(unix)]
fn ensure_model_catalog_creation_boundary_access(
    path: &Path,
    target_exists: bool,
    nearest_existing: Option<PathBuf>,
    access_context: &crate::platform::EffectiveModeAccessContext,
) -> std::io::Result<()> {
    let creation_directory = if target_exists {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    } else {
        nearest_existing.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "no existing ancestor for generated model catalog {}",
                    path.display()
                ),
            )
        })?
    };
    let resolved_directory = fs::canonicalize(&creation_directory)?;
    ensure_model_catalog_ancestors_searchable(&resolved_directory, access_context)?;
    let directory_metadata = fs::metadata(&resolved_directory)?;
    if !directory_metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!(
                "generated model catalog creation boundary is not a directory: {}",
                creation_directory.display()
            ),
        ));
    }
    access_context.ensure(
        &directory_metadata,
        &resolved_directory,
        crate::platform::UNIX_ACCESS_READ
            | crate::platform::UNIX_ACCESS_WRITE
            | crate::platform::UNIX_ACCESS_SEARCH,
        "generated model catalog creation, replacement, and directory sync",
    )
}

#[cfg(unix)]
fn ensure_model_catalog_persistence_access_for_platform(
    path: &Path,
    target_metadata: Option<&fs::Metadata>,
) -> std::io::Result<()> {
    let access_context = crate::platform::EffectiveModeAccessContext::current()?;
    let nearest_existing = ensure_model_catalog_ancestors_searchable(path, &access_context)?;
    if let Some(initial_metadata) = target_metadata {
        ensure_existing_model_catalog_target_access(path, initial_metadata, &access_context)?;
    }
    ensure_model_catalog_creation_boundary_access(
        path,
        target_metadata.is_some(),
        nearest_existing,
        &access_context,
    )
}

#[cfg(windows)]
fn ensure_model_catalog_persistence_access_for_platform(
    path: &Path,
    target_metadata: Option<&fs::Metadata>,
) -> std::io::Result<()> {
    if target_metadata.is_none() {
        return Ok(());
    }

    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() || windows_metadata_is_reparse_point(&opened_metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "generated model catalog target changed or became a reparse point during persistence preflight: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn ensure_model_catalog_persistence_access_for_platform(
    _path: &Path,
    _target_metadata: Option<&fs::Metadata>,
) -> std::io::Result<()> {
    Ok(())
}

/// Validate every permission needed for an atomic generated-catalog update
/// before the caller creates directories, lock entries, or temporary files.
///
/// Existing targets must be regular non-symlink files and grant the effective
/// Unix owner/group/other class both read and write access. Missing targets use
/// the closest existing directory as the creation boundary. In either case the
/// effective class must grant read + write + search on the directory that will
/// be mutated: atomic replacement needs write/search, and the final durability
/// sync opens that directory for reading. UID 0 intentionally receives no
/// policy bypass.
pub(crate) fn ensure_model_catalog_persistence_access(path: &Path) -> std::io::Result<()> {
    let target_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if target_metadata.as_ref().is_some_and(|metadata| {
        metadata.file_type().is_symlink() || windows_metadata_is_reparse_point(metadata)
    }) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "generated model catalog target must not be a symlink: {}",
                path.display()
            ),
        ));
    }
    if target_metadata
        .as_ref()
        .is_some_and(|metadata| !metadata.is_file())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "generated model catalog target must be a regular file: {}",
                path.display()
            ),
        ));
    }

    ensure_model_catalog_persistence_access_for_platform(path, target_metadata.as_ref())
}

fn read_bounded_model_catalog(
    path: &Path,
    max_bytes: usize,
    description: &str,
    allow_final_symlink: bool,
) -> std::io::Result<String> {
    let file = open_regular_file_for_read(path, allow_final_symlink)?;
    if file.metadata()?.len() > max_bytes as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{description} exceeds {max_bytes} bytes"),
        ));
    }
    let mut bytes = Vec::new();
    file.take((max_bytes + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{description} exceeds {max_bytes} bytes"),
        ));
    }
    String::from_utf8(bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{description} is not valid UTF-8: {error}"),
        )
    })
}

fn load_models_config(path: &Path) -> std::result::Result<ModelsConfig, Error> {
    read_bounded_model_catalog(path, MAX_FETCHED_CATALOG_BYTES, "model catalog", true)
        .map_err(|error| {
            Error::config(format!(
                "Failed to read model catalog {}: {error}",
                path.display()
            ))
        })
        .and_then(|contents| serde_json::from_str::<ModelsConfig>(&contents).map_err(Error::from))
}

pub(crate) fn resolve_model_catalog_provider_config(
    provider: &str,
    models_path: &Path,
) -> std::result::Result<Option<ModelCatalogProviderConfig>, Error> {
    resolve_model_catalog_provider_config_with_api_key(provider, models_path, "")
}

/// Resolve the non-secret shape of a live model-discovery route without
/// resolving any credential or configured header values.
pub(crate) fn model_catalog_provider_route_shape(
    provider: &str,
    models_path: &Path,
) -> std::result::Result<Option<(String, String)>, Error> {
    let config = match fs::symlink_metadata(models_path) {
        Ok(_) => Some(load_models_config(models_path)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(Error::config(format!(
                "Failed to inspect model catalog {}: {error}",
                models_path.display()
            )));
        }
    };

    let target_provider = canonical_provider_key(provider);
    let mut matching_configs = config.as_ref().into_iter().flat_map(|config| {
        config
            .providers
            .iter()
            .filter(|(configured, _)| canonical_provider_key(configured) == target_provider)
    });
    let configured = matching_configs.next();
    if matching_configs.next().is_some() {
        return Err(Error::config(format!(
            "models.json contains multiple provider aliases matching {provider:?}; keep one canonical provider entry"
        )));
    }

    if configured.is_none() && custom_provider_defaults(provider).is_none() {
        return Ok(None);
    }

    let empty_config = ProviderConfig::default();
    let (configured_provider, provider_config) = configured
        .map_or((provider, &empty_config), |(configured, config)| {
            (configured.as_str(), config)
        });
    if !provider_has_catalog_route(configured_provider, provider_config) {
        return Ok(None);
    }
    let (_, api, base_url, _) = resolved_provider_transport(configured_provider, provider_config);
    Ok(Some((base_url, api)))
}

/// Resolve a model-catalog route without evaluating its configured fallback
/// credential unless the route needs Pi to generate an Authorization header.
///
/// Apart from preserving the documented precedence, the lazy resolution is
/// important for `apiKey` values backed by files or shell commands: an unused
/// fallback must not produce side effects or failures merely because the route
/// itself is being inspected.
pub(crate) fn resolve_model_catalog_provider_config_with_api_key(
    provider: &str,
    models_path: &Path,
    caller_api_key: &str,
) -> std::result::Result<Option<ModelCatalogProviderConfig>, Error> {
    Ok(
        prepare_model_catalog_provider_config(provider, models_path)?
            .map(|prepared| prepared.into_route(caller_api_key.trim().is_empty())),
    )
}

pub(crate) fn prepare_model_catalog_provider_config(
    provider: &str,
    models_path: &Path,
) -> std::result::Result<Option<PreparedModelCatalogProviderConfig>, Error> {
    let config = match fs::symlink_metadata(models_path) {
        Ok(_) => Some(load_models_config(models_path)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(Error::config(format!(
                "Failed to inspect model catalog {}: {error}",
                models_path.display()
            )));
        }
    };

    prepare_model_catalog_provider_config_from_loaded(provider, models_path, config.as_ref(), None)
}

fn resolve_model_catalog_provider_config_from_loaded(
    provider: &str,
    models_path: &Path,
    config: Option<&ModelsConfig>,
    resolve_fallback_api_key: bool,
    provider_headers_snapshot: Option<&ProviderHeadersSnapshot>,
) -> std::result::Result<Option<ModelCatalogProviderConfig>, Error> {
    Ok(prepare_model_catalog_provider_config_from_loaded(
        provider,
        models_path,
        config,
        provider_headers_snapshot,
    )?
    .map(|prepared| prepared.into_route(resolve_fallback_api_key)))
}

fn prepare_model_catalog_provider_config_from_loaded(
    provider: &str,
    models_path: &Path,
    config: Option<&ModelsConfig>,
    provider_headers_snapshot: Option<&ProviderHeadersSnapshot>,
) -> std::result::Result<Option<PreparedModelCatalogProviderConfig>, Error> {
    let target_provider = canonical_provider_key(provider);
    let mut matching_configs = config.into_iter().flat_map(|config| {
        config
            .providers
            .iter()
            .filter(|(configured, _)| canonical_provider_key(configured) == target_provider)
    });
    let configured = matching_configs.next();
    if matching_configs.next().is_some() {
        return Err(Error::config(format!(
            "models.json contains multiple provider aliases matching {provider:?}; keep one canonical provider entry"
        )));
    }

    if configured.is_none() && custom_provider_defaults(provider).is_none() {
        return Ok(None);
    }

    let empty_config = ProviderConfig::default();
    let (configured_provider, provider_config) = configured
        .map_or((provider, &empty_config), |(configured, config)| {
            (configured.as_str(), config)
        });
    if !provider_has_catalog_route(configured_provider, provider_config) {
        return Err(Error::config(format!(
            "models.json provider {configured_provider:?} requires a non-empty baseUrl before live model discovery"
        )));
    }
    let (_, api, base_url, auth_header) =
        resolved_provider_transport(configured_provider, provider_config);
    let base_dir = models_path.parent();
    let (headers, deferred_headers) = provider_headers_snapshot.map_or_else(
        || prepare_model_catalog_headers(provider_config.headers.as_ref(), base_dir),
        |snapshot| {
            (
                snapshot
                    .get(configured_provider)
                    .cloned()
                    .unwrap_or_default(),
                HashMap::new(),
            )
        },
    );
    Ok(Some(PreparedModelCatalogProviderConfig {
        route: ModelCatalogProviderConfig {
            base_url,
            api,
            api_key: None,
            headers,
            auth_header,
        },
        fallback_api_key: provider_config.api_key.clone(),
        deferred_headers,
        base_dir: base_dir.map(Path::to_path_buf),
    }))
}

fn load_fetched_models_config(
    path: &Path,
    models_path: &Path,
    manual_config: Option<&ModelsConfig>,
    provider_headers_snapshot: Option<&ProviderHeadersSnapshot>,
) -> std::result::Result<(ModelsConfig, Vec<String>), Error> {
    let contents = read_generated_catalog(path).map_err(|error| {
        Error::config(format!(
            "Failed to read generated model catalog {}: {error}",
            path.display()
        ))
    })?;
    let catalog = parse_persisted_fetched_catalog(&contents).map_err(|error| {
        Error::config(format!(
            "Invalid generated model catalog {}: {error}",
            path.display()
        ))
    })?;

    let mut binding_errors = Vec::new();
    let providers = catalog.providers.into_iter().filter_map(|(provider, fetched)| {
        let route = match resolve_model_catalog_provider_config_from_loaded(
            &provider,
            models_path,
            manual_config,
            false,
            provider_headers_snapshot,
        ) {
            Ok(Some(route)) => route,
            Ok(None) => {
                binding_errors.push(format!(
                    "Ignoring generated model membership for provider {provider:?}: no current built-in or models.json route exists; refresh and persist the catalog after configuring a route"
                ));
                return None;
            }
            Err(error) => {
                binding_errors.push(format!(
                    "Ignoring generated model membership for provider {provider:?}: current route could not be resolved: {error}"
                ));
                return None;
            }
        };
        if !model_catalog_route_is_persistable(&route) {
            binding_errors.push(format!(
                "Ignoring generated model membership for provider {provider:?}: the current route contains a non-empty query/header value outside a recognized credential channel, so tenant or deployment identity cannot be verified; refresh live without persistence or configure a bindable route"
            ));
            return None;
        }
        let current_fingerprint = model_catalog_route_fingerprint(&provider, &route);
        if current_fingerprint != fetched.route_fingerprint {
            binding_errors.push(format!(
                "Ignoring generated model membership for provider {provider:?}: the saved endpoint/transport binding no longer matches the current route; run --fetch-models {provider} --refresh-models --persist-models to replace it"
            ));
            return None;
        }

        let models = fetched
            .models
            .into_iter()
            .map(|model| ModelConfig {
                id: model.id,
                ..ModelConfig::default()
            })
            .collect();
        Some((
            provider,
            ProviderConfig {
                models: Some(models),
                ..ProviderConfig::default()
            },
        ))
    })
    .collect();
    Ok((ModelsConfig { providers }, binding_errors))
}

pub(crate) fn parse_persisted_fetched_catalog(
    contents: &str,
) -> std::result::Result<PersistedFetchedCatalog, Error> {
    let mut deserializer = serde_json::Deserializer::from_str(contents);
    let catalog = PersistedFetchedCatalog::deserialize(&mut deserializer).map_err(Error::from)?;
    deserializer.end().map_err(Error::from)?;
    validate_persisted_fetched_catalog(&catalog)?;
    Ok(catalog)
}

pub(crate) fn validate_persisted_fetched_catalog(
    catalog: &PersistedFetchedCatalog,
) -> std::result::Result<(), Error> {
    if catalog.schema != FETCHED_MODELS_SCHEMA {
        return Err(Error::config(format!(
            "Unsupported generated model catalog schema {:?}; expected {FETCHED_MODELS_SCHEMA:?}",
            catalog.schema
        )));
    }

    if catalog.providers.len() > MAX_FETCHED_PROVIDERS {
        return Err(Error::config(format!(
            "Generated model catalog contains {} providers; maximum is {MAX_FETCHED_PROVIDERS}",
            catalog.providers.len()
        )));
    }

    let mut canonical_providers = HashSet::new();
    for (provider, fetched) in &catalog.providers {
        if !is_safe_model_catalog_identifier(provider, MAX_FETCHED_PROVIDER_ID_BYTES) {
            return Err(Error::config(
                "Generated model catalog contains an invalid provider ID",
            ));
        }
        let canonical = canonical_provider_key(provider);
        if !canonical_providers.insert(canonical.clone()) {
            return Err(Error::config(format!(
                "Generated model catalog contains duplicate aliases for provider {canonical:?}"
            )));
        }
        if !is_valid_model_catalog_route_fingerprint(&fetched.route_fingerprint) {
            return Err(Error::config(format!(
                "Generated model catalog contains an invalid route fingerprint for provider {provider:?}"
            )));
        }
        if fetched.fetched_at_unix_ms == 0 {
            return Err(Error::config(format!(
                "Generated model catalog contains an invalid fetched timestamp for provider {provider:?}"
            )));
        }
        let mut seen_model_ids = HashSet::new();
        if fetched.models.is_empty() {
            return Err(Error::config(format!(
                "Generated model catalog contains an empty model list for provider {provider:?}"
            )));
        }
        if fetched.models.len() > MAX_FETCHED_MODELS_PER_PROVIDER {
            return Err(Error::config(format!(
                "Generated model catalog contains {} models for provider {provider:?}; maximum is {MAX_FETCHED_MODELS_PER_PROVIDER}",
                fetched.models.len()
            )));
        }
        let total_model_id_bytes = fetched
            .models
            .iter()
            .try_fold(0usize, |total, model| total.checked_add(model.id.len()))
            .ok_or_else(|| Error::config("Generated model catalog model-ID size overflow"))?;
        if total_model_id_bytes > MAX_FETCHED_MODEL_BYTES_PER_PROVIDER {
            return Err(Error::config(format!(
                "Generated model catalog contains {total_model_id_bytes} model-ID bytes for provider {provider:?}; maximum is {MAX_FETCHED_MODEL_BYTES_PER_PROVIDER}"
            )));
        }
        for model in &fetched.models {
            if !is_safe_model_catalog_identifier(&model.id, MAX_FETCHED_MODEL_ID_BYTES) {
                return Err(Error::config(format!(
                    "Generated model catalog contains an invalid model ID for provider {provider:?}"
                )));
            }
            if !seen_model_ids.insert(normalized_registry_key(provider, &model.id)) {
                return Err(Error::config(format!(
                    "Generated model catalog contains duplicate registry identity for model ID {:?} and provider {provider:?}",
                    model.id
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn read_generated_catalog(path: &Path) -> std::io::Result<String> {
    read_bounded_model_catalog(
        path,
        MAX_FETCHED_CATALOG_BYTES,
        "generated model catalog",
        false,
    )
}

// === Ad-hoc model support ===

#[derive(Debug, Clone, Copy)]
struct AdHocProviderDefaults {
    api: &'static str,
    base_url: &'static str,
    auth_header: bool,
    reasoning: bool,
    input: &'static [InputType],
    context_window: u32,
    max_tokens: u32,
}

impl From<ProviderRoutingDefaults> for AdHocProviderDefaults {
    fn from(value: ProviderRoutingDefaults) -> Self {
        Self {
            api: value.api,
            base_url: value.base_url,
            auth_header: value.auth_header,
            reasoning: value.reasoning,
            input: value.input,
            context_window: value.context_window,
            max_tokens: value.max_tokens,
        }
    }
}

fn ad_hoc_provider_defaults(provider: &str) -> Option<AdHocProviderDefaults> {
    provider_routing_defaults(provider).map(AdHocProviderDefaults::from)
}

fn sap_chat_completions_endpoint(service_url: &str, model_id: &str) -> Option<String> {
    let base = service_url.trim().trim_end_matches('/');
    let deployment = model_id.trim();
    if base.is_empty() || deployment.is_empty() {
        return None;
    }
    Some(format!(
        "{base}/v2/inference/deployments/{deployment}/chat/completions"
    ))
}

fn ad_hoc_model_entry_with_sap_resolver<F>(
    provider: &str,
    model_id: &str,
    mut resolve_sap: F,
) -> Option<ModelEntry>
where
    F: FnMut() -> Option<SapResolvedCredentials>,
{
    if canonical_provider_id(provider).is_some_and(|canonical| canonical == "sap-ai-core") {
        let sap_creds = resolve_sap()?;
        let base_url = sap_chat_completions_endpoint(&sap_creds.service_url, model_id)?;
        return Some(ModelEntry {
            model: Model {
                id: model_id.to_string(),
                name: model_id.to_string(),
                api: "openai-completions".to_string(),
                provider: provider.to_string(),
                base_url,
                reasoning: effective_reasoning(model_id, true),
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 16_384,
                headers: HashMap::new(),
            },
            api_key: None,
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        });
    }

    let defaults = ad_hoc_provider_defaults(provider)?;
    let normalized_model_id = canonicalize_model_id_for_provider(provider, model_id);
    if normalized_model_id.is_empty() {
        return None;
    }
    let reasoning = effective_reasoning(&normalized_model_id, defaults.reasoning);
    Some(ModelEntry {
        model: Model {
            id: normalized_model_id.clone(),
            name: normalized_model_id,
            api: defaults.api.to_string(),
            provider: provider.to_string(),
            base_url: defaults.base_url.to_string(),
            reasoning,
            input: defaults.input.to_vec(),
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: defaults.context_window,
            max_tokens: defaults.max_tokens,
            headers: HashMap::new(),
        },
        api_key: None,
        headers: HashMap::new(),
        auth_header: defaults.auth_header,
        compat: None,
        oauth_config: None,
    })
}

pub(crate) fn ad_hoc_model_entry(provider: &str, model_id: &str) -> Option<ModelEntry> {
    let auth = AuthStorage::load(crate::config::Config::auth_path()).ok();
    let mut entry = ad_hoc_model_entry_with_sap_resolver(provider, model_id, || {
        auth.as_ref().and_then(resolve_sap_credentials)
    })?;

    // Synthesized entries start without credentials. Resolve them from stored
    // auth / environment variables so `model_entry_is_ready` reflects reality
    // and downstream selection logic does not treat an otherwise-usable
    // provider as unconfigured.
    if entry.api_key.is_none()
        && let Some(auth) = auth.as_ref()
    {
        entry.api_key = normalize_api_key_opt(auth.resolve_api_key(provider, None));
    }

    Some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthCredential, AuthStorage};
    use tempfile::tempdir;

    fn test_auth_storage() -> (tempfile::TempDir, AuthStorage) {
        let dir = tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load auth");
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "anthropic-auth-key".to_string(),
            },
        );
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "openai-auth-key".to_string(),
            },
        );
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: "google-auth-key".to_string(),
            },
        );
        auth.set(
            "openrouter",
            AuthCredential::ApiKey {
                key: "openrouter-auth-key".to_string(),
            },
        );
        auth.set(
            "acme",
            AuthCredential::ApiKey {
                key: "acme-auth-key".to_string(),
            },
        );
        (dir, auth)
    }

    fn expected_env_pair() -> (String, String) {
        let key = ["PATH", "HOME", "PWD"]
            .iter()
            .find_map(|k| {
                std::env::var(k)
                    .ok()
                    .filter(|v| !v.is_empty())
                    .map(|v| ((*k).to_string(), v))
            })
            .expect("expected at least one non-empty environment variable");
        (key.0, key.1)
    }

    fn test_catalog_route(
        base_url: &str,
        api_key: Option<&str>,
        auth_header: bool,
    ) -> ModelCatalogProviderConfig {
        ModelCatalogProviderConfig {
            base_url: base_url.to_string(),
            api: "openai-completions".to_string(),
            api_key: api_key.map(ToString::to_string),
            headers: HashMap::new(),
            auth_header,
        }
    }

    fn fetched_provider_json(
        provider: &str,
        route: &ModelCatalogProviderConfig,
        model_ids: &[&str],
    ) -> serde_json::Value {
        serde_json::json!({
            "routeFingerprint": model_catalog_route_fingerprint(provider, route),
            "fetchedAtUnixMs": 1_800_000_000_000_u64,
            "models": model_ids
                .iter()
                .map(|id| serde_json::json!({"id": id}))
                .collect::<Vec<_>>(),
        })
    }

    #[test]
    fn persisted_route_fingerprint_excludes_secret_values_but_binds_transport_shape() {
        let mut first = test_catalog_route(
            "https://catalog.example/v1?api_key=first-secret&token=one&token=two#fragment",
            Some("first-secret"),
            true,
        );
        first.headers.insert(
            "Authorization".to_string(),
            "Bearer first-secret".to_string(),
        );
        first
            .headers
            .insert("X-API-Key".to_string(), "header-secret-a".to_string());

        let mut rotated = test_catalog_route(
            "https://catalog.example/v1?api_key=second-secret&token=three&token=four#different",
            Some("second-secret"),
            true,
        );
        rotated.headers.insert(
            "authorization".to_string(),
            "Bearer second-secret".to_string(),
        );
        rotated
            .headers
            .insert("x-api-key".to_string(), "header-secret-b".to_string());

        assert!(model_catalog_route_is_persistable(&first));
        assert!(model_catalog_route_is_persistable(&rotated));

        assert_eq!(
            model_catalog_route_fingerprint("acme", &first),
            model_catalog_route_fingerprint("acme", &rotated),
            "credential values, fragments, header casing, and header values must not become persisted verifiers"
        );

        rotated.base_url =
            "https://catalog.example/v1?API_KEY=second-secret&token=three&token=four".to_string();
        assert_ne!(
            model_catalog_route_fingerprint("acme", &first),
            model_catalog_route_fingerprint("acme", &rotated),
            "case-sensitive query-name changes must invalidate generated membership"
        );

        rotated.base_url =
            "https://catalog.example/v1?token=three&api_key=second-secret&token=four".to_string();
        assert_ne!(
            model_catalog_route_fingerprint("acme", &first),
            model_catalog_route_fingerprint("acme", &rotated),
            "query-pair ordering changes must invalidate generated membership"
        );

        rotated.base_url =
            "https://catalog.example/v1?api_key=second-secret&token=three".to_string();
        assert_ne!(
            model_catalog_route_fingerprint("acme", &first),
            model_catalog_route_fingerprint("acme", &rotated),
            "repeated credential-query multiplicity is part of the transport shape"
        );

        rotated.base_url = "https://catalog.example/v2".to_string();
        assert_ne!(
            model_catalog_route_fingerprint("acme", &first),
            model_catalog_route_fingerprint("acme", &rotated),
            "endpoint path changes must invalidate generated membership"
        );
    }

    #[test]
    fn catalog_persistence_classification_is_exact_not_substring_based() {
        let mut recognized = test_catalog_route(
            "https://catalog.example/v1?ToKeN=rotatable-secret",
            None,
            false,
        );
        recognized.headers.insert(
            "x-GoOg-ApI-kEy".to_string(),
            "rotatable-header-secret".to_string(),
        );
        assert!(model_catalog_route_is_persistable(&recognized));

        let ambiguous_query = test_catalog_route(
            "https://catalog.example/v1?tenant_token=tenant-a",
            None,
            false,
        );
        assert!(!model_catalog_route_is_persistable(&ambiguous_query));

        let mut ambiguous_header = test_catalog_route("https://catalog.example/v1", None, false);
        ambiguous_header
            .headers
            .insert("x-tenant-token".to_string(), "tenant-a".to_string());
        assert!(!model_catalog_route_is_persistable(&ambiguous_header));
    }

    #[cfg(unix)]
    struct UnixModeGuard {
        path: PathBuf,
        original: fs::Permissions,
    }

    #[cfg(unix)]
    impl UnixModeGuard {
        fn set(path: &Path, mode: u32) -> Self {
            use std::os::unix::fs::PermissionsExt as _;

            let original = fs::metadata(path)
                .expect("stat permission fixture")
                .permissions();
            let mut restricted = original.clone();
            restricted.set_mode(mode);
            fs::set_permissions(path, restricted).expect("restrict permission fixture");
            Self {
                path: path.to_path_buf(),
                original,
            }
        }
    }

    #[cfg(unix)]
    impl Drop for UnixModeGuard {
        fn drop(&mut self) {
            if let Err(error) = fs::set_permissions(&self.path, self.original.clone()) {
                eprintln!(
                    "failed to restore permissions for {}: {error}",
                    self.path.display()
                );
            }
        }
    }

    #[test]
    fn parse_legacy_generated_models_extracts_known_legacy_only_providers() {
        let parsed = parse_legacy_generated_models();
        if crate::embedded_assets::legacy_models_generated_ts()
            .contains("export const MODELS = {} as const;")
        {
            assert!(
                parsed.is_empty(),
                "published stub catalog should not parse into legacy entries"
            );
            return;
        }
        assert!(
            !parsed.is_empty(),
            "legacy generated model catalog should parse into entries"
        );

        assert!(
            parsed
                .iter()
                .any(|m| m.provider == "azure-openai-responses")
        );
        assert!(parsed.iter().any(|m| m.provider == "vercel-ai-gateway"));
        assert!(parsed.iter().any(|m| m.provider == "kimi-coding"));
    }

    #[test]
    fn built_in_models_include_all_legacy_provider_model_pairs() {
        let (_dir, auth) = test_auth_storage();
        let built = built_in_models(&auth, ModelRegistryLoadMode::Full);

        let built_keys: HashSet<(String, String)> = built
            .iter()
            .map(|entry| {
                (
                    entry.model.provider.to_ascii_lowercase(),
                    entry.model.id.to_ascii_lowercase(),
                )
            })
            .collect();

        let mut missing = Vec::new();
        for legacy in legacy_generated_models() {
            let normalized_id = canonicalize_model_id_for_provider(&legacy.provider, &legacy.id);
            if normalized_id.is_empty() {
                continue;
            }
            let key = (
                legacy.provider.to_ascii_lowercase(),
                normalized_id.to_ascii_lowercase(),
            );
            if !built_keys.contains(&key) {
                missing.push(format!("{}/{}", legacy.provider, legacy.id));
            }
        }

        assert!(
            missing.is_empty(),
            "missing legacy provider/model entries in built-in registry: {}",
            missing.join(", ")
        );
    }

    #[test]
    fn built_in_models_preserve_legacy_model_display_names() {
        let (_dir, auth) = test_auth_storage();
        let built = built_in_models(&auth, ModelRegistryLoadMode::Full);

        let name_by_key: HashMap<(String, String), String> = built
            .iter()
            .map(|entry| {
                (
                    (
                        entry.model.provider.to_ascii_lowercase(),
                        entry.model.id.to_ascii_lowercase(),
                    ),
                    entry.model.name.clone(),
                )
            })
            .collect();

        let mut mismatches = Vec::new();
        for legacy in legacy_generated_models() {
            let normalized_id = canonicalize_model_id_for_provider(&legacy.provider, &legacy.id);
            if normalized_id.is_empty() {
                continue;
            }
            let key = (
                legacy.provider.to_ascii_lowercase(),
                normalized_id.to_ascii_lowercase(),
            );
            let Some(built_name) = name_by_key.get(&key) else {
                continue;
            };
            if !legacy.name.trim().is_empty() && built_name != &legacy.name {
                mismatches.push(format!(
                    "{}/{} => expected {:?}, got {:?}",
                    legacy.provider, legacy.id, legacy.name, built_name
                ));
            }
        }

        assert!(
            mismatches.is_empty(),
            "legacy model display name mismatches: {}",
            mismatches.join("; ")
        );
    }

    #[test]
    fn built_in_models_include_core_provider_entries() {
        let (_dir, auth) = test_auth_storage();
        let models = built_in_models(&auth, ModelRegistryLoadMode::Full);

        assert!(
            models.iter().any(
                |m| m.model.provider == "anthropic" && m.model.id == "claude-sonnet-4-20250514"
            )
        );
        assert!(
            models
                .iter()
                .any(|m| m.model.provider == "openai" && m.model.id == "gpt-4o")
        );
        assert!(
            models
                .iter()
                .any(|m| m.model.provider == "openai" && m.model.id == "gpt-5.4")
        );
        assert!(
            models
                .iter()
                .any(|m| m.model.provider == "google" && m.model.id == "gemini-2.5-pro")
        );
        assert!(
            models
                .iter()
                .any(|m| m.model.provider == "openrouter" && m.model.id == "openrouter/auto")
        );

        let anthropic = models
            .iter()
            .find(|m| m.model.provider == "anthropic")
            .expect("anthropic model");
        let openai = models
            .iter()
            .find(|m| m.model.provider == "openai")
            .expect("openai model");
        let google = models
            .iter()
            .find(|m| m.model.provider == "google")
            .expect("google model");
        let openrouter = models
            .iter()
            .find(|m| m.model.provider == "openrouter")
            .expect("openrouter model");
        assert_eq!(anthropic.api_key.as_deref(), Some("anthropic-auth-key"));
        assert_eq!(openai.api_key.as_deref(), Some("openai-auth-key"));
        assert_eq!(google.api_key.as_deref(), Some("google-auth-key"));
        assert_eq!(openrouter.api_key.as_deref(), Some("openrouter-auth-key"));
    }

    #[test]
    fn built_in_models_seed_gpt_5_6_family_for_openai_and_codex() {
        let (_dir, auth) = test_auth_storage();
        let models = built_in_models(&auth, ModelRegistryLoadMode::Full);

        for (id, input, output, cache_read, cache_write) in [
            ("gpt-5.6", 5.0, 30.0, 0.5, 6.25),
            ("gpt-5.6-sol", 5.0, 30.0, 0.5, 6.25),
            ("gpt-5.6-terra", 2.0, 12.0, 0.2, 2.5),
            ("gpt-5.6-luna", 0.2, 1.2, 0.02, 0.25),
        ] {
            let assert_cost = |actual: f64, expected: f64, field: &str| {
                assert!(
                    (actual - expected).abs() < f64::EPSILON,
                    "{id} {field}: expected {expected}, got {actual}"
                );
            };
            let openai = models
                .iter()
                .find(|m| m.model.provider == "openai" && m.model.id == id);
            assert!(openai.is_some(), "missing openai seed for {id}");
            let Some(openai) = openai else {
                return;
            };
            assert_eq!(openai.model.api, Api::OpenAIResponses.to_string());
            assert_eq!(openai.model.base_url, "https://api.openai.com/v1");
            assert!(openai.model.reasoning);
            assert_eq!(openai.api_key.as_deref(), Some("openai-auth-key"));
            assert_eq!(openai.model.context_window, 1_050_000);
            assert_eq!(openai.model.max_tokens, 128_000);
            assert_cost(openai.model.cost.input, input, "input cost");
            assert_cost(openai.model.cost.output, output, "output cost");
            assert_cost(openai.model.cost.cache_read, cache_read, "cache-read cost");
            assert_cost(
                openai.model.cost.cache_write,
                cache_write,
                "cache-write cost",
            );

            // All GPT-5.6 API models accept the xhigh and max reasoning tiers.
            assert!(openai.supports_xhigh(), "{id} should support xhigh");
            assert!(openai.supports_max(), "{id} should support max");

            if id != "gpt-5.6" {
                let codex = models
                    .iter()
                    .find(|m| m.model.provider == "openai-codex" && m.model.id == id);
                assert!(codex.is_some(), "missing openai-codex seed for {id}");
                let Some(codex) = codex else {
                    return;
                };
                assert_eq!(codex.model.api, Api::OpenAICodexResponses.to_string());
                assert_eq!(codex.model.base_url, "https://chatgpt.com/backend-api");
                assert!(codex.model.name.ends_with("Codex"));
                assert_eq!(codex.model.context_window, 1_050_000);
                assert_cost(codex.model.cost.input, input, "Codex input cost");
                assert_cost(codex.model.cost.output, output, "Codex output cost");
                assert_cost(
                    codex.model.cost.cache_read,
                    cache_read,
                    "Codex cache-read cost",
                );
                assert_cost(codex.model.cost.cache_write, 0.0, "Codex cache-write cost");
                assert!(codex.supports_xhigh(), "{id} Codex should support xhigh");
                assert!(codex.supports_max(), "{id} Codex should support max");
            }
        }
    }

    #[test]
    fn built_in_models_include_oauth_provider_entries() {
        let (_dir, auth) = test_auth_storage();
        let models = built_in_models(&auth, ModelRegistryLoadMode::Full);

        assert!(models.iter().any(|m| {
            m.model.provider == "openai-codex"
                && m.model.api == "openai-codex-responses"
                && m.model.id == "gpt-5.4"
        }));
        assert!(models.iter().any(|m| {
            m.model.provider == "openai-codex"
                && m.model.api == "openai-codex-responses"
                && m.model.id == "gpt-5.2-codex"
        }));
        assert!(models.iter().any(|m| {
            m.model.provider == "google-gemini-cli"
                && m.model.api == "google-gemini-cli"
                && m.model.id == "gemini-2.5-pro"
        }));
        assert!(models.iter().any(|m| {
            m.model.provider == "google-antigravity"
                && m.model.api == "google-gemini-cli"
                && m.model.id == "gemini-3-flash"
        }));
    }

    #[test]
    fn built_in_models_include_non_legacy_provider_model_strings_from_snapshot() {
        let (_dir, auth) = test_auth_storage();
        let models = built_in_models(&auth, ModelRegistryLoadMode::Full);

        assert!(
            models
                .iter()
                .any(|m| { m.model.provider == "groq" && m.model.id == "llama-3.3-70b-versatile" })
        );
        assert!(
            models
                .iter()
                .any(|m| { m.model.provider == "zhipuai" && m.model.id == "glm-4.6" })
        );
        assert!(models.iter().any(|m| {
            m.model.provider == "openrouter" && m.model.id == "anthropic/claude-sonnet-4"
        }));
    }

    #[test]
    fn built_in_models_seed_gitlab_upstream_entries_with_gitlab_chat_api() {
        let (_dir, auth) = test_auth_storage();
        let models = built_in_models(&auth, ModelRegistryLoadMode::Full);

        let gitlab = models
            .iter()
            .find(|m| m.model.provider == "gitlab" && m.model.id == "duo-chat-gpt-5-1")
            .expect("gitlab upstream model");
        assert_eq!(gitlab.model.api, "gitlab-chat");
        assert!(gitlab.auth_header);
    }

    #[test]
    fn built_in_models_seed_github_copilot_snapshot_entries_but_not_azure_openai() {
        // #100: native-adapter legacy providers that self-route (github-copilot)
        // must surface their snapshot / models-override.json model IDs as real
        // registry entries so autocomplete candidates actually resolve. Providers
        // that need per-user routing config (azure-openai, empty seed base_url and
        // no self-resolving adapter) must stay excluded.
        let (_dir, auth) = test_auth_storage();
        let models = built_in_models(&auth, ModelRegistryLoadMode::Full);

        let copilot = models
            .iter()
            .find(|m| m.model.provider == "github-copilot" && m.model.id == "claude-opus-4.6")
            .expect("github-copilot snapshot model should be admitted");
        assert_eq!(copilot.model.api, "openai-completions");
        assert!(copilot.auth_header);

        // azure-openai has an empty seed base_url and requires a user-supplied
        // resource, so its snapshot IDs must NOT become registry entries. The
        // snapshot lists an azure-only "model-router" id (absent from the
        // curated legacy catalog), which serves as a canary: if the exclusion
        // regressed, this snapshot-only id would leak into the registry.
        assert!(
            !models.iter().any(|m| m.model.id == "model-router"),
            "azure-openai snapshot entries (e.g. model-router) must not be admitted from the upstream snapshot"
        );
    }

    #[test]
    fn autocomplete_candidates_include_legacy_and_latest_entries() {
        let candidates = model_autocomplete_candidates();
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.slug == "openai-codex/gpt-5.4")
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.slug == "openai-codex/gpt-5.2-codex")
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.slug == "google-gemini-cli/gemini-2.5-pro")
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.slug == "openai/gpt-5.4")
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.slug == "anthropic/claude-opus-4-5")
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.slug == "groq/llama-3.3-70b-versatile")
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.slug == "openrouter/anthropic/claude-sonnet-4.6")
        );
        assert!(
            candidates
                .iter()
                .all(|candidate| !candidate.slug.starts_with("github-models/")),
            "the retired GitHub Models service must not leak through the captured upstream snapshot"
        );
    }

    #[test]
    fn autocomplete_candidates_are_case_insensitively_unique() {
        let candidates = model_autocomplete_candidates();
        let mut seen = HashSet::new();
        for candidate in candidates {
            let key = candidate.slug.to_ascii_lowercase();
            assert!(
                seen.insert(key),
                "duplicate autocomplete slug (case-insensitive): {}",
                candidate.slug
            );
        }
    }

    #[test]
    fn apply_custom_models_overrides_provider_fields_but_not_runtime_credentials() {
        let (_dir, auth) = test_auth_storage();
        let mut models = built_in_models(&auth, ModelRegistryLoadMode::Full);
        let (env_key, _) = expected_env_pair();
        let mut provider_headers = HashMap::new();
        provider_headers.insert("x-provider".to_string(), "provider-header".to_string());

        let config = ModelsConfig {
            providers: HashMap::from([(
                "anthropic".to_string(),
                ProviderConfig {
                    base_url: Some("https://proxy.example/v1/messages".to_string()),
                    api: Some("anthropic-messages".to_string()),
                    api_key: Some(format!("env:{env_key}")),
                    headers: Some(provider_headers),
                    auth_header: Some(true),
                    compat: Some(CompatConfig {
                        supports_store: Some(true),
                        ..CompatConfig::default()
                    }),
                    models: None,
                },
            )]),
        };

        apply_custom_models(&auth, &mut models, &config, None);

        for entry in models.iter().filter(|m| m.model.provider == "anthropic") {
            assert_eq!(entry.model.base_url, "https://proxy.example/v1/messages");
            assert_eq!(entry.model.api, "anthropic-messages");
            assert_eq!(
                entry.api_key.as_deref(),
                Some("anthropic-auth-key"),
                "the normal runtime credential must win over models.json apiKey"
            );
            assert_eq!(
                entry.headers.get("x-provider").map(String::as_str),
                Some("provider-header")
            );
            assert!(entry.auth_header);
            assert!(
                entry
                    .compat
                    .as_ref()
                    .and_then(|c| c.supports_store)
                    .unwrap_or(false)
            );
        }
    }

    #[test]
    fn apply_custom_models_preserves_existing_headers_when_provider_header_values_unresolved() {
        let (dir, auth) = test_auth_storage();
        let mut models = vec![ModelEntry {
            model: Model {
                id: "claude-test".to_string(),
                name: "Claude Test".to_string(),
                api: "anthropic-messages".to_string(),
                provider: "anthropic".to_string(),
                base_url: "https://api.anthropic.com/v1/messages".to_string(),
                reasoning: false,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 200_000,
                max_tokens: 8_192,
                headers: HashMap::new(),
            },
            api_key: None,
            headers: HashMap::from([("x-built-in".to_string(), "keep-me".to_string())]),
            auth_header: false,
            compat: None,
            oauth_config: None,
        }];

        let config = ModelsConfig {
            providers: HashMap::from([(
                "anthropic".to_string(),
                ProviderConfig {
                    headers: Some(HashMap::from([(
                        "x-provider".to_string(),
                        "file:missing-header.txt".to_string(),
                    )])),
                    ..ProviderConfig::default()
                },
            )]),
        };

        apply_custom_models(&auth, &mut models, &config, Some(dir.path()));

        assert_eq!(
            models[0].headers.get("x-built-in").map(String::as_str),
            Some("keep-me")
        );
        assert!(
            !models[0].headers.contains_key("x-provider"),
            "unresolved provider header values should not inject empty overrides"
        );
    }

    #[test]
    fn apply_custom_models_empty_provider_header_map_clears_existing_headers() {
        let (_dir, auth) = test_auth_storage();
        let mut models = vec![ModelEntry {
            model: Model {
                id: "claude-test".to_string(),
                name: "Claude Test".to_string(),
                api: "anthropic-messages".to_string(),
                provider: "anthropic".to_string(),
                base_url: "https://api.anthropic.com/v1/messages".to_string(),
                reasoning: false,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 200_000,
                max_tokens: 8_192,
                headers: HashMap::new(),
            },
            api_key: None,
            headers: HashMap::from([("x-built-in".to_string(), "remove-me".to_string())]),
            auth_header: false,
            compat: None,
            oauth_config: None,
        }];

        let config = ModelsConfig {
            providers: HashMap::from([(
                "anthropic".to_string(),
                ProviderConfig {
                    headers: Some(HashMap::new()),
                    ..ProviderConfig::default()
                },
            )]),
        };

        apply_custom_models(&auth, &mut models, &config, None);

        assert!(
            models[0].headers.is_empty(),
            "an explicit empty header map should still clear inherited headers"
        );
    }

    #[test]
    fn apply_custom_models_uses_schema_defaults_for_provider_models() {
        let (_dir, auth) = test_auth_storage();
        let mut models = Vec::new();
        let config = ModelsConfig {
            providers: HashMap::from([(
                "cohere".to_string(),
                ProviderConfig {
                    models: Some(vec![ModelConfig {
                        id: "command-r-plus".to_string(),
                        ..ModelConfig::default()
                    }]),
                    ..ProviderConfig::default()
                },
            )]),
        };

        apply_custom_models(&auth, &mut models, &config, None);

        let cohere = models
            .iter()
            .find(|entry| entry.model.provider == "cohere")
            .expect("cohere model should be added");
        assert_eq!(cohere.model.api, "cohere-chat");
        assert_eq!(cohere.model.base_url, "https://api.cohere.com/v2");
        assert!(
            !cohere.model.reasoning,
            "command-r-plus is non-reasoning; command-a is the reasoning line"
        );
        assert_eq!(cohere.model.input, vec![InputType::Text]);
        assert_eq!(cohere.model.context_window, 128_000);
        assert_eq!(cohere.model.max_tokens, 8192);
        assert!(!cohere.auth_header);
    }

    /// End-to-end coverage for gh #122 (custom base URL for custom providers).
    ///
    /// The canonical use case is pointing an OpenAI-compatible provider at a
    /// user-supplied endpoint (a local model server, a proxy, or an alternate
    /// gateway). This exercises the full path: the `baseUrl` from a
    /// user-defined provider in `models.json` flows through
    /// `apply_custom_models` onto the model entry, and the transport-facing URL
    /// builder (`normalize_openai_base`) turns it into the correct request
    /// endpoint — appending the API path exactly once and not doubling the
    /// slash introduced by a trailing `/`. It also asserts that omitting
    /// `baseUrl` leaves the default endpoint unchanged.
    #[test]
    fn apply_custom_models_honors_custom_base_url_for_openai_compatible_provider() {
        use crate::providers::normalize_openai_base;

        let (_dir, auth) = test_auth_storage();

        // (1) Custom provider WITH an explicit base_url. A trailing slash is
        // included deliberately to exercise path-join robustness.
        let mut models = Vec::new();
        let config = ModelsConfig {
            providers: HashMap::from([(
                "my-local".to_string(),
                ProviderConfig {
                    api: Some("openai-completions".to_string()),
                    base_url: Some("http://localhost:11434/v1/".to_string()),
                    models: Some(vec![ModelConfig {
                        id: "llama-3.1-70b".to_string(),
                        ..ModelConfig::default()
                    }]),
                    ..ProviderConfig::default()
                },
            )]),
        };
        apply_custom_models(&auth, &mut models, &config, None);

        let entry = models
            .iter()
            .find(|entry| entry.model.provider == "my-local")
            .expect("custom provider model should be added");
        // The configured base URL is carried onto the model entry verbatim;
        // normalization to a concrete request endpoint happens at request time.
        assert_eq!(entry.model.base_url, "http://localhost:11434/v1/");
        // The transport builds the correct endpoint: the API path is appended
        // exactly once and the trailing '/' does not produce a doubled slash.
        assert_eq!(
            normalize_openai_base(&entry.model.base_url),
            "http://localhost:11434/v1/chat/completions"
        );

        // (2) Custom provider WITHOUT a base_url falls back to the
        // openai-completions default endpoint (unchanged default behavior).
        let mut defaulted = Vec::new();
        let default_config = ModelsConfig {
            providers: HashMap::from([(
                "my-proxy".to_string(),
                ProviderConfig {
                    api: Some("openai-completions".to_string()),
                    models: Some(vec![ModelConfig {
                        id: "proxy-model".to_string(),
                        ..ModelConfig::default()
                    }]),
                    ..ProviderConfig::default()
                },
            )]),
        };
        apply_custom_models(&auth, &mut defaulted, &default_config, None);

        let default_entry = defaulted
            .iter()
            .find(|entry| entry.model.provider == "my-proxy")
            .expect("defaulted custom provider model should be added");
        assert_eq!(default_entry.model.base_url, "https://api.openai.com/v1");
        assert_eq!(
            normalize_openai_base(&default_entry.model.base_url),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn apply_custom_models_merges_provider_and_model_compat() {
        let (_dir, auth) = test_auth_storage();
        let mut models = Vec::new();
        let config = ModelsConfig {
            providers: HashMap::from([(
                "custom-openai".to_string(),
                ProviderConfig {
                    api: Some("openai-completions".to_string()),
                    base_url: Some("https://compat.example/v1".to_string()),
                    compat: Some(CompatConfig {
                        supports_tools: Some(false),
                        supports_usage_in_streaming: Some(false),
                        max_tokens_field: Some("max_completion_tokens".to_string()),
                        custom_headers: Some(HashMap::from([
                            ("x-provider-only".to_string(), "provider".to_string()),
                            ("x-shared".to_string(), "provider".to_string()),
                        ])),
                        ..CompatConfig::default()
                    }),
                    models: Some(vec![ModelConfig {
                        id: "custom-model".to_string(),
                        compat: Some(CompatConfig {
                            supports_tools: Some(true),
                            system_role_name: Some("developer".to_string()),
                            custom_headers: Some(HashMap::from([
                                ("x-model-only".to_string(), "model".to_string()),
                                ("x-shared".to_string(), "model".to_string()),
                            ])),
                            ..CompatConfig::default()
                        }),
                        ..ModelConfig::default()
                    }]),
                    ..ProviderConfig::default()
                },
            )]),
        };

        apply_custom_models(&auth, &mut models, &config, None);

        let entry = models
            .iter()
            .find(|m| m.model.provider == "custom-openai" && m.model.id == "custom-model")
            .expect("custom model should be added");
        let compat = entry.compat.as_ref().expect("compat should be merged");
        assert_eq!(
            compat.max_tokens_field.as_deref(),
            Some("max_completion_tokens")
        );
        assert_eq!(compat.system_role_name.as_deref(), Some("developer"));
        assert_eq!(compat.supports_usage_in_streaming, Some(false));
        assert_eq!(compat.supports_tools, Some(true));
        let custom_headers = compat
            .custom_headers
            .as_ref()
            .expect("custom headers should be merged");
        assert_eq!(
            custom_headers.get("x-provider-only").map(String::as_str),
            Some("provider")
        );
        assert_eq!(
            custom_headers.get("x-model-only").map(String::as_str),
            Some("model")
        );
        assert_eq!(
            custom_headers.get("x-shared").map(String::as_str),
            Some("model")
        );
    }

    #[test]
    fn apply_custom_models_uses_schema_defaults_for_native_anthropic_models() {
        let (_dir, auth) = test_auth_storage();
        let mut models = Vec::new();
        let config = ModelsConfig {
            providers: HashMap::from([(
                "anthropic".to_string(),
                ProviderConfig {
                    models: Some(vec![ModelConfig {
                        id: "claude-schema-default".to_string(),
                        ..ModelConfig::default()
                    }]),
                    ..ProviderConfig::default()
                },
            )]),
        };

        apply_custom_models(&auth, &mut models, &config, None);

        let anthropic = models
            .iter()
            .find(|entry| entry.model.provider == "anthropic")
            .expect("anthropic model should be added");
        assert_eq!(anthropic.model.api, "anthropic-messages");
        assert_eq!(
            anthropic.model.base_url,
            "https://api.anthropic.com/v1/messages"
        );
        assert!(anthropic.model.reasoning);
        assert_eq!(
            anthropic.model.input,
            vec![InputType::Text, InputType::Image]
        );
        assert_eq!(anthropic.model.context_window, 200_000);
        assert_eq!(anthropic.model.max_tokens, 8192);
        assert!(!anthropic.auth_header);
    }

    #[test]
    fn apply_custom_models_uses_native_adapter_defaults_for_codex_alias_models() {
        let (_dir, auth) = test_auth_storage();
        let mut models = Vec::new();
        let config = ModelsConfig {
            providers: HashMap::from([(
                "codex".to_string(),
                ProviderConfig {
                    models: Some(vec![ModelConfig {
                        id: "gpt-5.4".to_string(),
                        ..ModelConfig::default()
                    }]),
                    ..ProviderConfig::default()
                },
            )]),
        };

        apply_custom_models(&auth, &mut models, &config, None);

        let codex = models
            .iter()
            .find(|entry| entry.model.provider == "codex")
            .expect("codex model should be added");
        assert_eq!(codex.model.api, "openai-codex-responses");
        assert_eq!(codex.model.base_url, CODEX_RESPONSES_API_URL);
        assert!(codex.model.reasoning);
        assert_eq!(codex.model.input, vec![InputType::Text, InputType::Image]);
        assert_eq!(codex.model.context_window, 272_000);
        assert_eq!(codex.model.max_tokens, 128_000);
        assert!(codex.auth_header);
    }

    #[test]
    fn apply_custom_models_uses_native_adapter_defaults_for_google_cli_alias_models() {
        let (_dir, auth) = test_auth_storage();
        let mut models = Vec::new();
        let config = ModelsConfig {
            providers: HashMap::from([
                (
                    "gemini-cli".to_string(),
                    ProviderConfig {
                        models: Some(vec![ModelConfig {
                            id: "gemini-2.5-pro".to_string(),
                            ..ModelConfig::default()
                        }]),
                        ..ProviderConfig::default()
                    },
                ),
                (
                    "antigravity".to_string(),
                    ProviderConfig {
                        models: Some(vec![ModelConfig {
                            id: "gemini-3-flash".to_string(),
                            ..ModelConfig::default()
                        }]),
                        ..ProviderConfig::default()
                    },
                ),
            ]),
        };

        apply_custom_models(&auth, &mut models, &config, None);

        let gemini_cli = models
            .iter()
            .find(|entry| entry.model.provider == "gemini-cli")
            .expect("gemini-cli model should be added");
        assert_eq!(gemini_cli.model.api, "google-gemini-cli");
        assert_eq!(gemini_cli.model.base_url, GOOGLE_GEMINI_CLI_API_URL);
        assert!(gemini_cli.model.reasoning);
        assert_eq!(
            gemini_cli.model.input,
            vec![InputType::Text, InputType::Image]
        );
        assert_eq!(gemini_cli.model.context_window, 128_000);
        assert_eq!(gemini_cli.model.max_tokens, 8192);
        assert!(gemini_cli.auth_header);

        let antigravity = models
            .iter()
            .find(|entry| entry.model.provider == "antigravity")
            .expect("antigravity model should be added");
        assert_eq!(antigravity.model.api, "google-gemini-cli");
        assert_eq!(antigravity.model.base_url, GOOGLE_ANTIGRAVITY_API_URL);
        assert!(antigravity.model.reasoning);
        assert_eq!(
            antigravity.model.input,
            vec![InputType::Text, InputType::Image]
        );
        assert_eq!(antigravity.model.context_window, 128_000);
        assert_eq!(antigravity.model.max_tokens, 8192);
        assert!(antigravity.auth_header);
    }

    #[test]
    fn apply_custom_models_alias_resolves_canonical_provider_api_key() {
        let (_dir, mut auth) = test_auth_storage();
        auth.set(
            "moonshotai",
            AuthCredential::ApiKey {
                key: "moonshot-auth-key".to_string(),
            },
        );

        let mut models = Vec::new();
        let config = ModelsConfig {
            providers: HashMap::from([(
                "kimi".to_string(),
                ProviderConfig {
                    models: Some(vec![ModelConfig {
                        id: "kimi-k2-instruct".to_string(),
                        ..ModelConfig::default()
                    }]),
                    ..ProviderConfig::default()
                },
            )]),
        };

        apply_custom_models(&auth, &mut models, &config, None);

        let kimi = models
            .iter()
            .find(|entry| entry.model.provider == "kimi")
            .expect("kimi model should be added");
        assert_eq!(kimi.model.api, "openai-completions");
        assert_eq!(kimi.model.base_url, "https://api.moonshot.ai/v1");
        assert_eq!(kimi.api_key.as_deref(), Some("moonshot-auth-key"));
        assert!(kimi.auth_header);
    }

    #[test]
    fn model_registry_find_and_find_by_id_work() {
        let (_dir, auth) = test_auth_storage();
        let registry = ModelRegistry::load(&auth, None);

        let by_provider_and_id = registry
            .find("openai", "gpt-4o")
            .expect("openai/gpt-4o should exist");
        assert_eq!(by_provider_and_id.model.provider, "openai");
        assert_eq!(by_provider_and_id.model.id, "gpt-4o");

        let by_id = registry
            .find_by_id("claude-opus-4-5")
            .expect("claude-opus-4-5 should exist");
        assert_eq!(by_id.model.provider, "anthropic");
        assert_eq!(by_id.model.id, "claude-opus-4-5");

        assert!(registry.find("openai", "does-not-exist").is_none());
        assert!(registry.find_by_id("does-not-exist").is_none());
    }

    #[test]
    fn model_registry_find_by_id_is_case_insensitive() {
        let (_dir, auth) = test_auth_storage();
        let registry = ModelRegistry::load(&auth, None);

        let by_id = registry
            .find_by_id("GPT-5.2-CODEX")
            .expect("gpt-5.2-codex should resolve case-insensitively");
        assert_eq!(by_id.model.id, "gpt-5.2-codex");
    }

    #[test]
    fn model_registry_finds_latest_openai_codex_seed() {
        let (_dir, auth) = test_auth_storage();
        let registry = ModelRegistry::load(&auth, None);

        let by_provider = registry
            .find("openai-codex", "GPT-5.4")
            .expect("gpt-5.4 codex should resolve case-insensitively");
        assert_eq!(by_provider.model.provider, "openai-codex");
        assert_eq!(by_provider.model.id, "gpt-5.4");
    }

    #[test]
    fn model_registry_find_normalizes_openrouter_model_aliases() {
        let (_dir, auth) = test_auth_storage();
        let registry = ModelRegistry::load(&auth, None);

        let gpt4o_mini = registry
            .find("openrouter", "gpt-4o-mini")
            .expect("openrouter alias should resolve");
        assert_eq!(gpt4o_mini.model.provider, "openrouter");
        assert_eq!(gpt4o_mini.model.id, "openai/gpt-4o-mini");

        let auto = registry
            .find("openrouter", "auto")
            .expect("openrouter auto alias should resolve");
        assert_eq!(auto.model.id, "openrouter/auto");

        let provider_alias = registry
            .find("open-router", "gpt-4o-mini")
            .expect("open-router provider alias should resolve");
        assert_eq!(provider_alias.model.provider, "openrouter");
        assert_eq!(provider_alias.model.id, "openai/gpt-4o-mini");
    }

    #[test]
    fn ad_hoc_model_entry_normalizes_openrouter_aliases() {
        let auto = ad_hoc_model_entry("openrouter", "auto").expect("openrouter auto ad-hoc");
        assert_eq!(auto.model.id, "openrouter/auto");

        let gpt4o_mini =
            ad_hoc_model_entry("openrouter", "gpt-4o-mini").expect("openrouter gpt-4o-mini ad-hoc");
        assert_eq!(gpt4o_mini.model.id, "openai/gpt-4o-mini");
    }

    #[test]
    fn model_registry_merge_entries_deduplicates() {
        let (_dir, auth) = test_auth_storage();
        let mut registry = ModelRegistry::load(&auth, None);
        let before = registry.models().len();
        let duplicate = registry
            .find("openai", "gpt-4o")
            .expect("expected built-in openai model");

        let new_entry = ModelEntry {
            model: Model {
                id: "acme-chat".to_string(),
                name: "Acme Chat".to_string(),
                api: "openai-completions".to_string(),
                provider: "acme".to_string(),
                base_url: "https://acme.example/v1".to_string(),
                reasoning: true,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 64_000,
                max_tokens: 4096,
                headers: HashMap::new(),
            },
            api_key: Some("acme-auth-key".to_string()),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        };

        registry.merge_entries(vec![duplicate, new_entry]);
        assert_eq!(registry.models().len(), before + 1);
        assert!(registry.find("acme", "acme-chat").is_some());
    }

    #[test]
    fn model_registry_merge_entries_deduplicates_alias_and_case_variants() {
        let (_dir, auth) = test_auth_storage();
        let mut registry = ModelRegistry::load(&auth, None);
        let before = registry.models().len();

        let source = registry
            .find("openrouter", "gpt-4o-mini")
            .or_else(|| registry.find("openrouter", "openai/gpt-4o-mini"))
            .expect("expected built-in openrouter gpt-4o-mini model");

        let mut alias_case_variant = source.clone();
        alias_case_variant.model.provider = "open-router".to_string();
        alias_case_variant.model.id = source.model.id.to_ascii_uppercase();

        registry.merge_entries(vec![alias_case_variant]);
        assert_eq!(registry.models().len(), before);
    }

    #[test]
    fn apply_custom_models_dedupes_openrouter_alias_conflicts() {
        let (_dir, auth) = test_auth_storage();
        let mut models = Vec::new();
        let config = ModelsConfig {
            providers: HashMap::from([(
                "openrouter".to_string(),
                ProviderConfig {
                    models: Some(vec![
                        ModelConfig {
                            id: "gpt-4o-mini".to_string(),
                            ..ModelConfig::default()
                        },
                        ModelConfig {
                            id: "openai/gpt-4o-mini".to_string(),
                            ..ModelConfig::default()
                        },
                        ModelConfig {
                            id: "auto".to_string(),
                            ..ModelConfig::default()
                        },
                    ]),
                    ..ProviderConfig::default()
                },
            )]),
        };

        apply_custom_models(&auth, &mut models, &config, None);

        let openrouter_models: Vec<&ModelEntry> = models
            .iter()
            .filter(|entry| entry.model.provider == "openrouter")
            .collect();
        assert_eq!(openrouter_models.len(), 2);
        assert!(
            openrouter_models
                .iter()
                .any(|entry| entry.model.id == "openai/gpt-4o-mini")
        );
        assert!(
            openrouter_models
                .iter()
                .any(|entry| entry.model.id == "openrouter/auto")
        );
    }

    #[test]
    fn resolve_value_supports_env_and_file_prefixes() {
        let (env_key, env_val) = expected_env_pair();
        assert_eq!(
            resolve_value(&format!("env:{env_key}")).as_deref(),
            Some(env_val.as_str())
        );

        let dir = tempdir().expect("tempdir");
        let key_path = dir.path().join("api_key.txt");
        std::fs::write(&key_path, "file-key\n").expect("write key file");
        assert_eq!(
            resolve_value(&format!("file:{}", key_path.display())).as_deref(),
            Some("file-key")
        );
        assert!(resolve_value("file:/definitely/missing/path").is_none());
    }

    // ─── pi parity: bare *_API_KEY env-var indirection (issue #64) ───────────

    #[test]
    fn looks_like_api_key_env_var_accepts_typical_names() {
        assert!(looks_like_api_key_env_var("DASHSCOPE_API_KEY"));
        assert!(looks_like_api_key_env_var("OPENAI_API_KEY"));
        assert!(looks_like_api_key_env_var("ANTHROPIC_API_KEY"));
        assert!(looks_like_api_key_env_var("MY_CUSTOM_API_KEY"));
        // Digits in the prefix are fine, as long as it starts with a letter.
        assert!(looks_like_api_key_env_var("PROVIDER42_API_KEY"));
    }

    #[test]
    fn looks_like_api_key_env_var_rejects_non_matches() {
        // Wrong suffix.
        assert!(!looks_like_api_key_env_var("DASHSCOPE_API"));
        assert!(!looks_like_api_key_env_var("DASHSCOPE_TOKEN"));
        // Lowercase letters anywhere → looks like a literal key.
        assert!(!looks_like_api_key_env_var("dashscope_api_key"));
        assert!(!looks_like_api_key_env_var("My_API_KEY"));
        // Real-shaped keys.
        assert!(!looks_like_api_key_env_var("sk-ant-api03-AAAA_API_KEY"));
        assert!(!looks_like_api_key_env_var("sk-1234567890"));
        // Bare suffix only.
        assert!(!looks_like_api_key_env_var("_API_KEY"));
        assert!(!looks_like_api_key_env_var(""));
        // Must start with a letter.
        assert!(!looks_like_api_key_env_var("0DASH_API_KEY"));
    }

    #[test]
    fn resolve_value_resolves_bare_api_key_env_var_when_set() {
        let resolved = resolve_value_with_resolvers("DASHSCOPE_API_KEY", None, |var| {
            assert_eq!(var, "DASHSCOPE_API_KEY");
            Some("sk-real-secret-from-env".to_string())
        });
        assert_eq!(resolved.as_deref(), Some("sk-real-secret-from-env"));
    }

    #[test]
    fn resolve_value_trims_whitespace_from_resolved_env_value() {
        let resolved = resolve_value_with_resolvers("DASHSCOPE_API_KEY", None, |_| {
            Some("  sk-trimmed  \n".to_string())
        });
        assert_eq!(resolved.as_deref(), Some("sk-trimmed"));
    }

    #[test]
    fn resolve_value_falls_back_to_literal_when_referenced_env_var_unset() {
        // When the env var is unset we keep the literal so existing
        // configurations that just happened to choose an `_API_KEY`-shaped
        // value continue to work; the user sees the auth failure as before.
        let resolved = resolve_value_with_resolvers("UNSET_PROVIDER_API_KEY", None, |_| None);
        assert_eq!(resolved.as_deref(), Some("UNSET_PROVIDER_API_KEY"));
    }

    #[test]
    fn resolve_value_falls_back_to_literal_when_referenced_env_var_empty() {
        let resolved =
            resolve_value_with_resolvers("DASHSCOPE_API_KEY", None, |_| Some("   ".to_string()));
        assert_eq!(resolved.as_deref(), Some("DASHSCOPE_API_KEY"));
    }

    #[test]
    fn resolve_value_treats_literal_key_unchanged() {
        // Real-looking provider keys are passed through verbatim and must NOT
        // hit the env_lookup closure.
        let resolved = resolve_value_with_resolvers("sk-ant-api03-abcdef123", None, |_| {
            panic!("env_lookup should not be invoked for literal-shaped values");
        });
        assert_eq!(resolved.as_deref(), Some("sk-ant-api03-abcdef123"));
    }

    #[test]
    fn model_registry_uses_models_json_api_key_only_when_runtime_key_is_absent() {
        let (dir, _auth) = test_auth_storage();
        let models_path = dir.path().join("models.json");
        let key_path = dir.path().join("custom_key.txt");
        std::fs::write(&key_path, "acme-file-key\n").expect("write custom key");

        let models_json = serde_json::json!({
            "providers": {
                "acme": {
                    "baseUrl": "https://acme.example/v1",
                    "api": "openai-completions",
                    "apiKey": format!("file:{}", key_path.display()),
                    "headers": {
                        "x-provider": "provider-level"
                    },
                    "authHeader": true,
                    "models": [
                        {
                            "id": "acme-chat",
                            "name": "Acme Chat",
                            "input": ["text", "image"],
                            "reasoning": true,
                            "contextWindow": 64000,
                            "maxTokens": 4096,
                            "headers": {
                                "x-model": "model-level"
                            }
                        }
                    ]
                }
            }
        });

        std::fs::write(
            &models_path,
            serde_json::to_string_pretty(&models_json).expect("serialize models json"),
        )
        .expect("write models.json");

        let registry = ModelRegistry::load_with_credential_resolver(Some(models_path), |_| None);
        let acme = registry
            .find("acme", "acme-chat")
            .expect("custom acme model should load from models.json");

        assert_eq!(acme.model.name, "Acme Chat");
        assert_eq!(acme.model.api, "openai-completions");
        assert_eq!(acme.model.base_url, "https://acme.example/v1");
        assert_eq!(acme.model.context_window, 64_000);
        assert_eq!(acme.model.max_tokens, 4096);
        assert_eq!(acme.api_key.as_deref(), Some("acme-file-key"));
        assert!(acme.auth_header);
        assert_eq!(
            acme.headers.get("x-provider").map(String::as_str),
            Some("provider-level")
        );
        assert_eq!(
            acme.headers.get("x-model").map(String::as_str),
            Some("model-level")
        );
        assert_eq!(acme.model.input, vec![InputType::Text, InputType::Image]);
    }

    #[test]
    fn model_catalog_discovery_uses_resolved_custom_provider_transport() {
        let dir = tempdir().expect("tempdir");
        let models_path = dir.path().join("models.json");
        let key_path = dir.path().join("catalog-key.txt");
        std::fs::write(&key_path, "custom-catalog-key\n").expect("write catalog key");
        std::fs::write(
            &models_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "providers": {
                    "acme": {
                        "baseUrl": "https://models.acme.example/openai/v1",
                        "api": "openai-completions",
                        "apiKey": "file:catalog-key.txt",
                        "authHeader": false,
                        "headers": {"x-acme-key": "header-secret"}
                    }
                }
            }))
            .expect("serialize models.json"),
        )
        .expect("write models.json");

        let route = resolve_model_catalog_provider_config("acme", &models_path)
            .expect("resolve custom discovery route")
            .expect("custom provider route");
        assert_eq!(route.base_url, "https://models.acme.example/openai/v1");
        assert_eq!(route.api, "openai-completions");
        assert_eq!(route.api_key, None);
        assert!(!route.auth_header);
        assert_eq!(
            route.headers.get("x-acme-key").map(String::as_str),
            Some("header-secret")
        );
    }

    #[cfg(unix)]
    #[test]
    fn model_catalog_discovery_does_not_evaluate_unused_fallback_api_key() {
        let dir = tempdir().expect("tempdir");
        let models_path = dir.path().join("models.json");
        let marker_path = dir.path().join("fallback-was-evaluated");
        let quoted_marker = marker_path.to_string_lossy().replace('\'', "'\\''");
        let fallback_command = format!("!touch '{quoted_marker}'");
        std::fs::write(
            &models_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "providers": {
                    "acme": {
                        "baseUrl": "https://models.acme.example/v1",
                        "api": "openai-completions",
                        "apiKey": fallback_command
                    }
                }
            }))
            .expect("serialize models.json"),
        )
        .expect("write models.json");

        let route =
            resolve_model_catalog_provider_config_with_api_key("acme", &models_path, "runtime-key")
                .expect("resolve custom discovery route")
                .expect("custom provider route");

        assert_eq!(route.api_key, None);
        assert_eq!(
            effective_model_catalog_api_key("runtime-key", &route),
            "runtime-key"
        );
        assert!(
            !marker_path.exists(),
            "models.json fallback command must not run when runtime auth wins"
        );
    }

    #[cfg(unix)]
    #[test]
    fn custom_authorization_does_not_evaluate_unused_fallback_api_key() {
        let dir = tempdir().expect("tempdir");
        let models_path = dir.path().join("models.json");
        let marker_path = dir.path().join("fallback-was-evaluated");
        let quoted_marker = marker_path.to_string_lossy().replace('\'', "'\\''");
        let fallback_command = format!("!touch '{quoted_marker}'");
        std::fs::write(
            &models_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "providers": {
                    "acme": {
                        "baseUrl": "https://models.acme.example/v1",
                        "api": "openai-completions",
                        "apiKey": fallback_command,
                        "headers": {
                            "Authorization": "Token configured-only"
                        }
                    }
                }
            }))
            .expect("serialize models.json"),
        )
        .expect("write models.json");

        let route = resolve_model_catalog_provider_config_with_api_key("acme", &models_path, "")
            .expect("resolve custom discovery route")
            .expect("custom provider route");

        assert_eq!(route.api_key, None);
        assert_eq!(
            route.headers.get("Authorization").map(String::as_str),
            Some("Token configured-only")
        );
        assert!(
            !marker_path.exists(),
            "models.json fallback command must not run when custom Authorization wins"
        );
    }

    #[cfg(unix)]
    #[test]
    fn disabled_auth_header_does_not_evaluate_unused_fallback_api_key() {
        let dir = tempdir().expect("tempdir");
        let models_path = dir.path().join("models.json");
        let marker_path = dir.path().join("fallback-was-evaluated");
        let quoted_marker = marker_path.to_string_lossy().replace('\'', "'\\''");
        let fallback_command = format!("!touch '{quoted_marker}'");
        std::fs::write(
            &models_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "providers": {
                    "acme": {
                        "baseUrl": "https://models.acme.example/v1",
                        "api": "openai-completions",
                        "apiKey": fallback_command,
                        "authHeader": false,
                        "headers": {
                            "x-acme-key": "configured-only"
                        }
                    }
                }
            }))
            .expect("serialize models.json"),
        )
        .expect("write models.json");

        let route = resolve_model_catalog_provider_config_with_api_key("acme", &models_path, "")
            .expect("resolve custom discovery route")
            .expect("custom provider route");

        assert!(!route.auth_header);
        assert_eq!(route.api_key, None);
        assert_eq!(
            route.headers.get("x-acme-key").map(String::as_str),
            Some("configured-only")
        );
        assert!(
            !marker_path.exists(),
            "models.json fallback command must not run when generated Authorization is disabled"
        );
    }

    #[test]
    fn model_catalog_discovery_honors_builtin_endpoint_override() {
        let dir = tempdir().expect("tempdir");
        let models_path = dir.path().join("models.json");
        std::fs::write(
            &models_path,
            r#"{"providers":{"ollama":{"baseUrl":"http://127.0.0.1:11000/v1"}}}"#,
        )
        .expect("write ollama override");

        let route = resolve_model_catalog_provider_config("ollama", &models_path)
            .expect("resolve ollama discovery route")
            .expect("built-in provider route");
        assert_eq!(route.base_url, "http://127.0.0.1:11000/v1");
        assert_eq!(route.api, "openai-completions");
        assert!(!route.auth_header);
    }

    #[test]
    fn model_catalog_discovery_rejects_ambiguous_provider_aliases() {
        let dir = tempdir().expect("tempdir");
        let models_path = dir.path().join("models.json");
        std::fs::write(&models_path, r#"{"providers":{"openai":{},"OpenAI":{}}}"#)
            .expect("write ambiguous aliases");

        let error = resolve_model_catalog_provider_config("openai", &models_path)
            .expect_err("ambiguous provider aliases must fail deterministically");
        assert!(
            error.to_string().contains("multiple provider aliases"),
            "{error}"
        );
    }

    #[test]
    fn model_catalog_discovery_rejects_exact_and_escaped_duplicate_provider_keys() {
        let dir = tempdir().expect("tempdir");
        let models_path = dir.path().join("models.json");
        for contents in [
            r#"{"providers":{"openai":{"baseUrl":"https://safe.example/v1"},"openai":{"baseUrl":"https://attacker.example/v1"}}}"#,
            r#"{"providers":{"openai":{"baseUrl":"https://safe.example/v1"},"\u006fpenai":{"baseUrl":"https://attacker.example/v1"}}}"#,
        ] {
            std::fs::write(&models_path, contents).expect("write duplicate provider route");
            let error = resolve_model_catalog_provider_config("openai", &models_path)
                .expect_err("duplicate provider routes must fail closed");
            assert!(
                error.to_string().contains("duplicate JSON object key"),
                "{error}"
            );
        }
    }

    #[test]
    fn generated_catalog_parser_rejects_duplicate_fields_aliases_and_trailing_data() {
        for (label, contents, expected) in [
            (
                "duplicate top-level schema",
                r#"{"schema":"pi.models.fetched.v2","schema":"pi.models.fetched.v2","providers":{"openai":{"routeFingerprint":"sha256:0000000000000000000000000000000000000000000000000000000000000000","fetchedAtUnixMs":1,"models":[{"id":"model"}]}}}"#,
                "duplicate field",
            ),
            (
                "duplicate model id",
                r#"{"schema":"pi.models.fetched.v2","providers":{"openai":{"routeFingerprint":"sha256:0000000000000000000000000000000000000000000000000000000000000000","fetchedAtUnixMs":1,"models":[{"id":"first","id":"second"}]}}}"#,
                "duplicate field",
            ),
            (
                "canonical provider aliases",
                r#"{"schema":"pi.models.fetched.v2","providers":{"openai":{"routeFingerprint":"sha256:0000000000000000000000000000000000000000000000000000000000000000","fetchedAtUnixMs":1,"models":[{"id":"first"}]},"OpenAI":{"routeFingerprint":"sha256:0000000000000000000000000000000000000000000000000000000000000000","fetchedAtUnixMs":1,"models":[{"id":"second"}]}}}"#,
                "duplicate aliases",
            ),
            (
                "escaped exact provider key",
                r#"{"schema":"pi.models.fetched.v2","providers":{"openai":{"routeFingerprint":"sha256:0000000000000000000000000000000000000000000000000000000000000000","fetchedAtUnixMs":1,"models":[{"id":"first"}]},"\u006fpenai":{"routeFingerprint":"sha256:0000000000000000000000000000000000000000000000000000000000000000","fetchedAtUnixMs":1,"models":[{"id":"second"}]}}}"#,
                "duplicate JSON object key",
            ),
            (
                "trailing JSON value",
                r#"{"schema":"pi.models.fetched.v2","providers":{"openai":{"routeFingerprint":"sha256:0000000000000000000000000000000000000000000000000000000000000000","fetchedAtUnixMs":1,"models":[{"id":"model"}]}}} {}"#,
                "trailing characters",
            ),
        ] {
            let error = parse_persisted_fetched_catalog(contents)
                .expect_err("malformed generated catalog must fail closed");
            assert!(error.to_string().contains(expected), "{label}: {error}");
        }
    }

    #[test]
    fn generated_catalog_v2_requires_valid_route_and_timestamp_provenance() {
        for (label, provider, expected) in [
            (
                "missing route fingerprint",
                serde_json::json!({
                    "fetchedAtUnixMs": 1,
                    "models": [{"id": "model"}]
                }),
                "routeFingerprint",
            ),
            (
                "malformed route fingerprint",
                serde_json::json!({
                    "routeFingerprint": "sha256:not-a-digest",
                    "fetchedAtUnixMs": 1,
                    "models": [{"id": "model"}]
                }),
                "invalid route fingerprint",
            ),
            (
                "zero fetch timestamp",
                serde_json::json!({
                    "routeFingerprint": format!("sha256:{}", "0".repeat(64)),
                    "fetchedAtUnixMs": 0,
                    "models": [{"id": "model"}]
                }),
                "invalid fetched timestamp",
            ),
        ] {
            let payload = serde_json::json!({
                "schema": FETCHED_MODELS_SCHEMA,
                "providers": {"openai": provider},
            });
            let error = parse_persisted_fetched_catalog(&payload.to_string())
                .expect_err("schema-v2 provenance must fail closed");
            assert!(error.to_string().contains(expected), "{label}: {error}");
        }
    }

    #[test]
    fn generated_catalog_parser_enforces_provider_and_model_cardinality_during_decode() {
        let providers = (0..=MAX_FETCHED_PROVIDERS)
            .map(|index| {
                (
                    format!("provider-{index}"),
                    serde_json::json!({
                        "routeFingerprint": format!("sha256:{}", "0".repeat(64)),
                        "fetchedAtUnixMs": 1,
                        "models": [{"id": "model"}]
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let provider_payload = serde_json::json!({
            "schema": FETCHED_MODELS_SCHEMA,
            "providers": providers,
        });
        let error = parse_persisted_fetched_catalog(&provider_payload.to_string())
            .expect_err("provider cap must be enforced while decoding");
        assert!(error.to_string().contains("exceeds"), "{error}");

        let models = (0..=MAX_FETCHED_MODELS_PER_PROVIDER)
            .map(|index| serde_json::json!({"id": format!("model-{index}")}))
            .collect::<Vec<_>>();
        let model_payload = serde_json::json!({
            "schema": FETCHED_MODELS_SCHEMA,
            "providers": {"openai": {
                "routeFingerprint": format!("sha256:{}", "0".repeat(64)),
                "fetchedAtUnixMs": 1,
                "models": models
            }},
        });
        let error = parse_persisted_fetched_catalog(&model_payload.to_string())
            .expect_err("model cap must be enforced while decoding");
        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    #[test]
    fn model_catalog_discovery_rejects_custom_provider_without_explicit_endpoint() {
        let dir = tempdir().expect("tempdir");
        let models_path = dir.path().join("models.json");
        std::fs::write(
            &models_path,
            r#"{"providers":{"acme":{"apiKey":"must-not-be-sent-elsewhere"}}}"#,
        )
        .expect("write incomplete custom route");

        let error = resolve_model_catalog_provider_config("acme", &models_path)
            .expect_err("a custom provider must not inherit OpenAI's public endpoint");
        assert!(error.to_string().contains("non-empty baseUrl"), "{error}");
    }

    #[test]
    fn model_catalog_discovery_requires_routes_for_self_routed_native_adapters() {
        let dir = tempdir().expect("tempdir");
        let models_path = dir.path().join("models.json");

        for provider in ["sap-ai-core", "github-copilot"] {
            assert!(
                model_catalog_provider_route_shape(provider, &models_path)
                    .expect("probe empty native adapter route")
                    .is_none(),
                "route probing must reject empty native defaults before credential resolution"
            );
            let error = resolve_model_catalog_provider_config(provider, &models_path)
                .expect_err("empty native adapter seed must not inherit the OpenAI endpoint");
            assert!(error.to_string().contains("non-empty baseUrl"), "{error}");
        }

        std::fs::write(
            &models_path,
            r#"{"providers":{"sap-ai-core":{"baseUrl":"https://sap.example/v1"},"github-copilot":{"baseUrl":"https://copilot.example/v1"}}}"#,
        )
        .expect("write explicit native catalog routes");

        for (provider, expected) in [
            ("sap-ai-core", "https://sap.example/v1"),
            ("github-copilot", "https://copilot.example/v1"),
        ] {
            assert!(
                model_catalog_provider_route_shape(provider, &models_path)
                    .expect("probe explicit native adapter route")
                    .is_some(),
                "configured native routes must survive the credential preflight"
            );
            let route = resolve_model_catalog_provider_config(provider, &models_path)
                .expect("resolve explicit native adapter route")
                .expect("configured native adapter route");
            assert_eq!(route.base_url, expected);
        }
    }

    #[cfg(unix)]
    #[test]
    fn manual_model_catalog_symlink_to_regular_file_remains_supported() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("actual-models.json");
        let models_path = dir.path().join("models.json");
        std::fs::write(
            &target,
            r#"{"providers":{"acme":{"baseUrl":"https://acme.example/v1"}}}"#,
        )
        .expect("write model catalog target");
        symlink(&target, &models_path).expect("symlink model catalog");

        assert!(
            resolve_model_catalog_provider_config("acme", &models_path)
                .expect("read symlinked manual catalog")
                .is_some()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_model_catalog_reparse_policy_distinguishes_manual_and_generated_files() {
        use std::os::windows::fs::symlink_file;

        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("actual-models.json");
        let manual_path = dir.path().join("models.json");
        let generated_path = dir.path().join("models.fetched.json");
        std::fs::write(
            &target,
            r#"{"providers":{"acme":{"baseUrl":"https://acme.example/v1"}}}"#,
        )
        .expect("write catalog target");

        for link in [&manual_path, &generated_path] {
            if let Err(error) = symlink_file(&target, link) {
                // Creating a Windows symlink still requires Developer Mode or
                // SeCreateSymbolicLinkPrivilege on some runners. The target
                // build continues to compile this complete test even there.
                if error.raw_os_error() == Some(1314) {
                    eprintln!("skipping Windows symlink runtime assertion: {error}");
                    return;
                }
                panic!("create catalog symlink {}: {error}", link.display());
            }
        }

        assert!(
            resolve_model_catalog_provider_config("acme", &manual_path)
                .expect("manual catalog symlink remains supported")
                .is_some()
        );

        let read_error = read_generated_catalog(&generated_path)
            .expect_err("generated catalog symlink must not be followed");
        assert_eq!(read_error.kind(), std::io::ErrorKind::InvalidData);
        let persist_error = ensure_model_catalog_persistence_access(&generated_path)
            .expect_err("generated catalog symlink must not pass persistence preflight");
        assert_eq!(persist_error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            std::fs::symlink_metadata(&generated_path)
                .expect("generated catalog link still exists")
                .file_type()
                .is_symlink(),
            "preflight must not replace or otherwise mutate the reparse point"
        );
    }

    #[cfg(unix)]
    #[test]
    fn model_catalog_reads_enforce_effective_owner_class_even_for_uid_zero() {
        let dir = tempdir().expect("tempdir");
        let manual_path = dir.path().join("models.json");
        let fetched_path = dir.path().join("models.fetched.json");
        std::fs::write(&manual_path, r#"{"providers":{}}"#).expect("write manual catalog");
        std::fs::write(&fetched_path, "{}\n").expect("write generated catalog");

        let manual_mode = UnixModeGuard::set(&manual_path, 0o004);
        let manual_error = load_models_config(&manual_path)
            .expect_err("owner class without read must be denied even when other has read");
        assert!(
            manual_error.to_string().contains("Permission denied"),
            "{manual_error}"
        );
        drop(manual_mode);

        let fetched_mode = UnixModeGuard::set(&fetched_path, 0o000);
        let fetched_error = read_generated_catalog(&fetched_path)
            .expect_err("mode-000 generated catalog must be denied even to UID 0");
        assert_eq!(fetched_error.kind(), std::io::ErrorKind::PermissionDenied);
        drop(fetched_mode);
    }

    #[cfg(unix)]
    #[test]
    fn model_catalog_reads_require_lexical_and_resolved_ancestors_to_be_searchable() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().expect("tempdir");
        let lexical_dir = dir.path().join("lexical-denied");
        std::fs::create_dir(&lexical_dir).expect("create lexical directory");
        let lexical_catalog = lexical_dir.join("models.json");
        std::fs::write(&lexical_catalog, r#"{"providers":{}}"#).expect("write lexical catalog");
        let lexical_mode = UnixModeGuard::set(&lexical_dir, 0o007);
        let lexical_error = load_models_config(&lexical_catalog)
            .expect_err("owner class without search must block lexical traversal");
        assert!(
            lexical_error.to_string().contains("Permission denied"),
            "{lexical_error}"
        );
        drop(lexical_mode);

        let target_dir = dir.path().join("resolved-denied");
        std::fs::create_dir(&target_dir).expect("create target directory");
        let target = target_dir.join("actual-models.json");
        std::fs::write(&target, r#"{"providers":{}}"#).expect("write target catalog");
        let symlink_path = dir.path().join("models.json");
        symlink(&target, &symlink_path).expect("create final catalog symlink");
        let target_mode = UnixModeGuard::set(&target_dir, 0o007);
        let resolved_error = load_models_config(&symlink_path)
            .expect_err("resolved target ancestors must be independently searchable");
        assert!(
            resolved_error.to_string().contains("Permission denied"),
            "{resolved_error}"
        );
        drop(target_mode);
    }

    #[cfg(unix)]
    #[test]
    fn generated_model_catalog_fifo_is_rejected_without_opening() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("models.fetched.json");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("run mkfifo");
        assert!(status.success(), "mkfifo must create the test fixture");

        let error = read_generated_catalog(&path)
            .expect_err("a FIFO must be rejected instead of blocking for a writer");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("regular"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn model_registry_reports_broken_catalog_symlinks() {
        use std::os::unix::fs::symlink;

        let (dir, auth) = test_auth_storage();
        let models_path = dir.path().join("models.json");
        symlink(dir.path().join("missing-models.json"), &models_path)
            .expect("create broken model catalog symlink");

        let registry = ModelRegistry::load(&auth, Some(models_path));
        let error = registry
            .error()
            .expect("a configured but unreadable catalog must not be silently ignored");
        assert!(error.contains("Failed to read model catalog"), "{error}");
    }

    #[test]
    fn model_registry_loads_fetched_catalog_before_manual_models_json() {
        let (dir, auth) = test_auth_storage();
        let models_path = dir.path().join("models.json");
        let fetched_path = fetched_models_path(&models_path);
        let manual_json = serde_json::json!({
            "futureUserField": {"mustRemain": true},
            "providers": {
                "shared-provider": {
                    "baseUrl": "https://manual.example/v1",
                    "models": [{"id": "manual-shared", "name": "Manual wins"}]
                }
            }
        });
        let manual_bytes =
            serde_json::to_vec_pretty(&manual_json).expect("serialize manual config");
        std::fs::write(&models_path, &manual_bytes).expect("write manual models.json");
        let openai_route = resolve_model_catalog_provider_config("openai", &models_path)
            .expect("resolve OpenAI route")
            .expect("built-in OpenAI route");
        let shared_route = resolve_model_catalog_provider_config("shared-provider", &models_path)
            .expect("resolve shared-provider route")
            .expect("manual shared-provider route");
        let fetched_json = serde_json::json!({
            "schema": FETCHED_MODELS_SCHEMA,
            "providers": {
                "shared-provider": fetched_provider_json(
                    "shared-provider",
                    &shared_route,
                    &["generated-shared"],
                ),
                "openai": fetched_provider_json(
                    "openai",
                    &openai_route,
                    &["fetched-model"],
                ),
            }
        });
        std::fs::write(
            &fetched_path,
            serde_json::to_string_pretty(&fetched_json).expect("serialize fetched catalog"),
        )
        .expect("write fetched catalog");

        let registry = ModelRegistry::load(&auth, Some(models_path.clone()));

        assert!(
            registry.find("openai", "fetched-model").is_some(),
            "generated membership with a built-in provider route must survive"
        );
        let manual = registry
            .find("shared-provider", "manual-shared")
            .expect("manual provider catalog should win");
        assert_eq!(manual.model.name, "Manual wins");
        assert_eq!(manual.model.base_url, "https://manual.example/v1");
        assert!(
            registry
                .find("shared-provider", "generated-shared")
                .is_none(),
            "manual models list should retain its existing full-provider override semantics"
        );
        assert_eq!(
            std::fs::read(&models_path).expect("re-read manual models.json"),
            manual_bytes,
            "loading generated models must never rewrite user models.json"
        );
    }

    #[test]
    fn fetched_custom_provider_requires_an_explicit_manual_endpoint() {
        let (dir, auth) = test_auth_storage();
        let models_path = dir.path().join("models.json");
        std::fs::write(
            fetched_models_path(&models_path),
            format!(
                r#"{{"schema":"{FETCHED_MODELS_SCHEMA}","providers":{{"acme":{{"routeFingerprint":"sha256:{}","fetchedAtUnixMs":1,"models":[{{"id":"acme-live"}}]}}}}}}"#,
                "0".repeat(64),
            ),
        )
        .expect("write fetched catalog");
        std::fs::write(&models_path, r#"{"providers":{"acme":{}}}"#)
            .expect("write route-less manual catalog");

        let registry = ModelRegistry::load(&auth, Some(models_path));
        assert!(
            registry.find("acme", "acme-live").is_none(),
            "generated membership must not synthesize an OpenAI endpoint for a custom provider"
        );
    }

    #[test]
    fn fetched_catalog_ignores_unverifiable_value_routed_membership() {
        let (dir, auth) = test_auth_storage();
        let models_path = dir.path().join("models.json");
        std::fs::write(
            &models_path,
            r#"{"providers":{"acme":{"baseUrl":"https://acme.example/v1?tenant=tenant-a","headers":{"x-deployment":"blue"}}}}"#,
        )
        .expect("write value-routed manual catalog");
        let route = resolve_model_catalog_provider_config("acme", &models_path)
            .expect("resolve value-routed catalog")
            .expect("manual catalog route");
        assert!(!model_catalog_route_is_persistable(&route));
        std::fs::write(
            fetched_models_path(&models_path),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema": FETCHED_MODELS_SCHEMA,
                "providers": {
                    "acme": fetched_provider_json("acme", &route, &["must-not-load"])
                }
            }))
            .expect("serialize fetched catalog"),
        )
        .expect("write fetched catalog");

        let registry = ModelRegistry::load(&auth, Some(models_path));
        assert!(registry.find("acme", "must-not-load").is_none());
        assert!(
            registry
                .error()
                .is_some_and(|error| error.contains("outside a recognized credential channel")),
            "unverifiable persisted membership must produce an explicit binding error: {:?}",
            registry.error()
        );
    }

    #[test]
    fn fetched_catalog_preserves_known_model_metadata_with_live_membership() {
        let (dir, auth) = test_auth_storage();
        let baseline = ModelRegistry::load(&auth, None)
            .find("openai", "gpt-5.6")
            .expect("built-in GPT-5.6");
        let models_path = dir.path().join("models.json");
        let route = resolve_model_catalog_provider_config("openai", &models_path)
            .expect("resolve OpenAI route")
            .expect("built-in OpenAI route");
        std::fs::write(
            fetched_models_path(&models_path),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema": FETCHED_MODELS_SCHEMA,
                "providers": {
                    "openai": fetched_provider_json(
                        "openai",
                        &route,
                        &["gpt-5.6", "new-live-model"],
                    )
                }
            }))
            .expect("serialize fetched catalog"),
        )
        .expect("write fetched catalog");

        let registry = ModelRegistry::load(&auth, Some(models_path));
        let preserved = registry
            .find("openai", "gpt-5.6")
            .expect("known fetched model");
        assert_eq!(preserved.model.api, baseline.model.api);
        assert_eq!(preserved.model.base_url, baseline.model.base_url);
        assert_eq!(
            preserved.model.context_window,
            baseline.model.context_window
        );
        assert_eq!(preserved.model.max_tokens, baseline.model.max_tokens);
        assert_eq!(preserved.model.reasoning, baseline.model.reasoning);
        assert_eq!(preserved.supports_max(), baseline.supports_max());
        assert!(registry.find("openai", "new-live-model").is_some());
        assert!(
            registry.find("openai", "gpt-4o").is_none(),
            "models absent from the live membership must not be reintroduced"
        );
    }

    #[test]
    fn fetched_catalog_rejects_wrong_schema_and_user_only_fields() {
        let (dir, auth) = test_auth_storage();
        let models_path = dir.path().join("models.json");
        let fetched_path = fetched_models_path(&models_path);
        std::fs::write(
            &fetched_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema": "pi.models.fetched.v999",
                "providers": {
                    "openrouter": {
                        "routeFingerprint": format!("sha256:{}", "0".repeat(64)),
                        "fetchedAtUnixMs": 1,
                        "models": [{"id": "untrusted-generated-model"}]
                    }
                }
            }))
            .expect("serialize invalid fetched catalog"),
        )
        .expect("write invalid fetched catalog");

        let registry = ModelRegistry::load(&auth, Some(models_path.clone()));
        let error = registry.error().expect("generated schema error");
        assert!(
            error.contains("Unsupported generated model catalog schema"),
            "{error}"
        );
        assert!(
            registry
                .find("openrouter", "untrusted-generated-model")
                .is_none(),
            "invalid generated input must fail closed"
        );

        std::fs::write(
            &fetched_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema": FETCHED_MODELS_SCHEMA,
                "providers": {
                    "openrouter": {
                        "routeFingerprint": format!("sha256:{}", "0".repeat(64)),
                        "fetchedAtUnixMs": 1,
                        "apiKey": "must-not-be-accepted-here",
                        "models": [{"id": "untrusted-generated-model"}]
                    }
                }
            }))
            .expect("serialize invalid fetched fields"),
        )
        .expect("write invalid fetched fields");
        let registry = ModelRegistry::load(&auth, Some(models_path));
        let error = registry.error().expect("generated field error");
        assert!(error.contains("Invalid generated model catalog"), "{error}");
    }

    #[test]
    fn fetched_catalog_is_ignored_when_manual_route_configuration_is_unreadable() {
        let (dir, auth) = test_auth_storage();
        let models_path = dir.path().join("models.json");
        let defaults = provider_routing_defaults("openai").expect("OpenAI route defaults");
        let route = ModelCatalogProviderConfig {
            base_url: defaults.base_url.to_string(),
            api: defaults.api.to_string(),
            api_key: None,
            headers: HashMap::new(),
            auth_header: defaults.auth_header,
        };
        std::fs::write(&models_path, "{ malformed").expect("write malformed models.json");
        std::fs::write(
            fetched_models_path(&models_path),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema": FETCHED_MODELS_SCHEMA,
                "providers": {
                    "openai": fetched_provider_json(
                        "openai",
                        &route,
                        &["must-not-load"],
                    )
                }
            }))
            .expect("serialize fetched catalog"),
        )
        .expect("write fetched catalog");

        let registry = ModelRegistry::load(&auth, Some(models_path));
        assert!(registry.find("openai", "must-not-load").is_none());
        let error = registry
            .error()
            .expect("manual and generated catalog errors");
        assert!(
            error.contains("current models.json route configuration could not be loaded"),
            "{error}"
        );
    }

    #[test]
    fn model_registry_snapshots_one_credential_for_fetched_and_manual_entries() {
        let dir = tempdir().expect("tempdir");
        let models_path = dir.path().join("models.json");
        let route = test_catalog_route(
            "https://account-a.example/v1",
            Some("route-fallback-key"),
            true,
        );
        std::fs::write(
            &models_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "providers": {
                    "acme": {
                        "api": "openai-completions",
                        "baseUrl": &route.base_url,
                        "apiKey": "route-fallback-key",
                        "authHeader": true
                    }
                }
            }))
            .expect("serialize manual route"),
        )
        .expect("write manual route");
        std::fs::write(
            fetched_models_path(&models_path),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema": FETCHED_MODELS_SCHEMA,
                "providers": {
                    "acme": fetched_provider_json(
                        "acme",
                        &route,
                        &["route-bound-model"],
                    )
                }
            }))
            .expect("serialize fetched catalog"),
        )
        .expect("write fetched catalog");

        let acme_calls = std::cell::Cell::new(0_u8);
        let registry =
            ModelRegistry::load_with_credential_resolver(Some(models_path), |provider| {
                if !provider.eq_ignore_ascii_case("acme") {
                    return None;
                }
                let call = acme_calls.get();
                acme_calls.set(call.saturating_add(1));
                Some(if call == 0 {
                    "account-a-runtime-key".to_string()
                } else {
                    "account-b-runtime-key".to_string()
                })
            });

        assert!(registry.error().is_none(), "{:?}", registry.error());
        let model = registry
            .find("acme", "route-bound-model")
            .expect("route-matched generated model");
        assert_eq!(model.api_key.as_deref(), Some("account-a-runtime-key"));
        assert_eq!(
            acme_calls.get(),
            1,
            "one registry load must not re-resolve a credential across merged catalog layers"
        );
    }

    #[cfg(unix)]
    #[test]
    fn model_registry_snapshots_manual_headers_across_binding_and_application() {
        let dir = tempdir().expect("tempdir");
        let models_path = dir.path().join("models.json");
        let counter_path = dir.path().join("header-helper-count");
        let quoted_counter = counter_path.to_string_lossy().replace('\'', "'\\''");
        let header_command =
            format!("!printf x >> '{quoted_counter}'; printf stable-header-secret");

        let mut route = test_catalog_route("https://account-a.example/v1", None, false);
        route
            .headers
            .insert("x-api-key".to_string(), "stable-header-secret".to_string());
        std::fs::write(
            &models_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "providers": {
                    "acme": {
                        "api": &route.api,
                        "baseUrl": &route.base_url,
                        "authHeader": false,
                        "headers": {"x-api-key": header_command}
                    }
                }
            }))
            .expect("serialize manual route"),
        )
        .expect("write manual route");
        std::fs::write(
            fetched_models_path(&models_path),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema": FETCHED_MODELS_SCHEMA,
                "providers": {
                    "acme": fetched_provider_json(
                        "acme",
                        &route,
                        &["route-bound-model"],
                    )
                }
            }))
            .expect("serialize fetched catalog"),
        )
        .expect("write fetched catalog");

        let registry = ModelRegistry::load_with_credential_resolver(Some(models_path), |_| None);

        assert!(registry.error().is_none(), "{:?}", registry.error());
        let model = registry
            .find("acme", "route-bound-model")
            .expect("route-matched generated model");
        assert_eq!(
            model.headers.get("x-api-key").map(String::as_str),
            Some("stable-header-secret")
        );
        assert_eq!(
            std::fs::read(&counter_path).expect("read helper counter"),
            b"x",
            "one registry load must evaluate each manual provider header helper exactly once"
        );
    }

    #[test]
    fn model_registry_load_resolves_relative_file_values_against_models_json_dir() {
        let (dir, auth) = test_auth_storage();
        let models_dir = dir.path().join("config");
        std::fs::create_dir_all(&models_dir).expect("create models dir");
        let models_path = models_dir.join("models.json");
        std::fs::write(models_dir.join("relative_key.txt"), "relative-api-key\n")
            .expect("write relative key");
        std::fs::write(
            models_dir.join("provider_header.txt"),
            "provider-from-file\n",
        )
        .expect("write provider header");
        std::fs::write(models_dir.join("model_header.txt"), "model-from-file\n")
            .expect("write model header");

        let models_json = serde_json::json!({
            "providers": {
                "acme-relative": {
                    "baseUrl": "https://acme.example/v1",
                    "api": "openai-completions",
                    "apiKey": "file:relative_key.txt",
                    "headers": {
                        "x-provider-file": "file:provider_header.txt"
                    },
                    "models": [
                        {
                            "id": "acme-relative-chat",
                            "headers": {
                                "x-model-file": "file:model_header.txt"
                            }
                        }
                    ]
                }
            }
        });

        std::fs::write(
            &models_path,
            serde_json::to_string_pretty(&models_json).expect("serialize models json"),
        )
        .expect("write models.json");

        let registry = ModelRegistry::load(&auth, Some(models_path));
        let acme = registry
            .find("acme-relative", "acme-relative-chat")
            .expect("custom model should load with relative file-backed values");

        assert_eq!(acme.api_key.as_deref(), Some("relative-api-key"));
        assert_eq!(
            acme.headers.get("x-provider-file").map(String::as_str),
            Some("provider-from-file")
        );
        assert_eq!(
            acme.headers.get("x-model-file").map(String::as_str),
            Some("model-from-file")
        );
    }

    // ─── supports_xhigh ──────────────────────────────────────────────

    fn make_model_entry(id: &str, reasoning: bool) -> ModelEntry {
        ModelEntry {
            model: Model {
                id: id.to_string(),
                name: id.to_string(),
                api: "openai-responses".to_string(),
                provider: "test".to_string(),
                base_url: "https://example.com".to_string(),
                reasoning,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 8192,
                headers: HashMap::new(),
            },
            api_key: None,
            headers: HashMap::new(),
            auth_header: false,
            compat: None,
            oauth_config: None,
        }
    }

    /// Like `make_model_entry`, but lets a test set the provider id and base URL
    /// (needed to exercise DeepSeek thinking-format detection — gh #114).
    fn make_model_entry_with_provider(
        id: &str,
        reasoning: bool,
        provider: &str,
        base_url: &str,
    ) -> ModelEntry {
        let mut entry = make_model_entry(id, reasoning);
        entry.model.provider = provider.to_string();
        entry.model.base_url = base_url.to_string();
        entry
    }

    #[test]
    fn supports_xhigh_for_known_models() {
        assert!(make_model_entry("gpt-5.1-codex-max", true).supports_xhigh());
        assert!(make_model_entry("gpt-5.2", true).supports_xhigh());
        assert!(make_model_entry("gpt-5.4", true).supports_xhigh());
        assert!(make_model_entry("gpt-5.2-codex", true).supports_xhigh());
        assert!(make_model_entry("gpt-5.3-codex", true).supports_xhigh());
        assert!(make_model_entry("gpt-5.3-codex-spark", true).supports_xhigh());
    }

    #[test]
    fn supports_xhigh_false_for_other_models() {
        assert!(!make_model_entry("gpt-4o", true).supports_xhigh());
        assert!(!make_model_entry("claude-sonnet-4-20250514", true).supports_xhigh());
        assert!(!make_model_entry("gemini-2.5-pro", true).supports_xhigh());
    }

    #[test]
    fn available_thinking_levels_non_reasoning_is_off_only() {
        use crate::model::ThinkingLevel;
        let entry = make_model_entry("gpt-4o-mini", false);
        assert_eq!(entry.available_thinking_levels(), vec![ThinkingLevel::Off]);
    }

    #[test]
    fn available_thinking_levels_reasoning_without_xhigh_stops_at_high() {
        use crate::model::ThinkingLevel;
        let entry = make_model_entry("claude-sonnet-4-20250514", true);
        assert_eq!(
            entry.available_thinking_levels(),
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Minimal,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ]
        );
    }

    #[test]
    fn available_thinking_levels_reasoning_with_xhigh_includes_xhigh() {
        use crate::model::ThinkingLevel;
        let entry = make_model_entry("gpt-5.2", true);
        assert_eq!(
            entry.available_thinking_levels(),
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Minimal,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::XHigh,
            ]
        );
    }

    // ─── clamp_thinking_level ────────────────────────────────────────

    #[test]
    fn clamp_non_reasoning_always_off() {
        use crate::model::ThinkingLevel;
        let entry = make_model_entry("gpt-4o-mini", false);
        assert_eq!(
            entry.clamp_thinking_level(ThinkingLevel::High),
            ThinkingLevel::Off
        );
        assert_eq!(
            entry.clamp_thinking_level(ThinkingLevel::Medium),
            ThinkingLevel::Off
        );
        assert_eq!(
            entry.clamp_thinking_level(ThinkingLevel::Off),
            ThinkingLevel::Off
        );
    }

    #[test]
    fn clamp_xhigh_downgraded_without_support() {
        use crate::model::ThinkingLevel;
        let entry = make_model_entry("claude-sonnet-4-20250514", true);
        assert_eq!(
            entry.clamp_thinking_level(ThinkingLevel::XHigh),
            ThinkingLevel::High,
        );
    }

    #[test]
    fn clamp_xhigh_preserved_with_support() {
        use crate::model::ThinkingLevel;
        let entry = make_model_entry("gpt-5.2", true);
        assert_eq!(
            entry.clamp_thinking_level(ThinkingLevel::XHigh),
            ThinkingLevel::XHigh,
        );
    }

    // ─── DeepSeek xhigh support (gh #114) ────────────────────────────

    #[test]
    fn supports_xhigh_true_for_deepseek_reasoning_models() {
        // Detected via the provider id...
        assert!(
            make_model_entry_with_provider(
                "deepseek-v4-pro",
                true,
                "deepseek",
                "https://api.deepseek.com"
            )
            .supports_xhigh()
        );
        assert!(
            make_model_entry_with_provider(
                "deepseek-reasoner",
                true,
                "deepseek",
                "https://api.deepseek.com"
            )
            .supports_xhigh()
        );
        // ...and via a deepseek.com base URL even if the provider id is generic.
        assert!(
            make_model_entry_with_provider(
                "deepseek-v4-flash",
                true,
                "custom",
                "https://api.deepseek.com/v1"
            )
            .supports_xhigh()
        );
    }

    #[test]
    fn supports_xhigh_false_for_non_reasoning_deepseek() {
        // deepseek-chat / V3 are non-thinking models: xhigh must stay off.
        assert!(
            !make_model_entry_with_provider(
                "deepseek-chat",
                false,
                "deepseek",
                "https://api.deepseek.com"
            )
            .supports_xhigh()
        );
    }

    #[test]
    fn available_thinking_levels_deepseek_reasoning_includes_xhigh() {
        use crate::model::ThinkingLevel;
        let entry = make_model_entry_with_provider(
            "deepseek-v4-pro",
            true,
            "deepseek",
            "https://api.deepseek.com",
        );
        assert_eq!(
            entry.available_thinking_levels(),
            vec![
                ThinkingLevel::Off,
                ThinkingLevel::Minimal,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::XHigh,
                ThinkingLevel::Max,
            ]
        );
    }

    #[test]
    fn clamp_xhigh_preserved_for_deepseek_reasoning() {
        use crate::model::ThinkingLevel;
        let entry = make_model_entry_with_provider(
            "deepseek-v4-pro",
            true,
            "deepseek",
            "https://api.deepseek.com",
        );
        assert_eq!(
            entry.clamp_thinking_level(ThinkingLevel::XHigh),
            ThinkingLevel::XHigh
        );
    }

    /// End-to-end regression for gh #114: the runtime path is
    /// `clamp_thinking_level` -> `OpenAIProvider::build_request`. #113's unit test
    /// called `build_request()` directly with `XHigh`, bypassing the clamp that
    /// (before this fix) downgraded `XHigh -> High` for DeepSeek. This drives the
    /// full chain and asserts the wire body carries `reasoning_effort: "max"`.
    #[test]
    fn deepseek_reasoning_xhigh_survives_clamp_and_serializes_as_max() {
        use crate::model::ThinkingLevel;
        use crate::provider::{Context, StreamOptions};

        let entry = make_model_entry_with_provider(
            "deepseek-v4-pro",
            true,
            "deepseek",
            "https://api.deepseek.com",
        );

        // (1) The clamp must pass XHigh through (the #114 gap).
        let effective = entry.clamp_thinking_level(ThinkingLevel::XHigh);
        assert_eq!(
            effective,
            ThinkingLevel::XHigh,
            "clamp must not downgrade xhigh for a DeepSeek reasoning model"
        );

        // (2) Feed the clamped level into the real request builder.
        let provider = crate::providers::openai::OpenAIProvider::new(entry.model.id.as_str())
            .with_provider_name(entry.model.provider.as_str())
            .with_reasoning(entry.model.reasoning);
        let context = Context {
            system_prompt: None,
            messages: vec![crate::model::Message::User(crate::model::UserMessage {
                content: crate::model::UserContent::Text("solve it".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::<crate::provider::ToolDef>::new().into(),
        };
        let body = |level: ThinkingLevel| {
            let options = StreamOptions {
                thinking_level: Some(level),
                ..Default::default()
            };
            serde_json::to_value(provider.build_request(&context, &options))
                .expect("serialize request")
        };

        let xhigh_body = body(effective);
        assert_eq!(xhigh_body["thinking"]["type"], "enabled");
        assert_eq!(
            xhigh_body["reasoning_effort"], "max",
            "xhigh must reach the wire as reasoning_effort=max end-to-end"
        );

        // (3) high (and the other levels) still serialize exactly as before.
        let high = entry.clamp_thinking_level(ThinkingLevel::High);
        assert_eq!(high, ThinkingLevel::High);
        let high_body = body(high);
        assert_eq!(high_body["thinking"]["type"], "enabled");
        assert_eq!(high_body["reasoning_effort"], "high");
    }

    /// Stronger end-to-end variant that derives the `reasoning` flag through the
    /// REAL classification path (`model_is_reasoning` -> `effective_reasoning`)
    /// instead of hardcoding `true`. This is the case #114's first cut missed: in
    /// production `model_is_reasoning("deepseek-v4-pro")` was `Some(false)`, so the
    /// model was non-reasoning and the whole feature was inert for it.
    #[test]
    fn deepseek_v4_pro_real_registry_path_xhigh_reaches_wire_as_max() {
        use crate::model::ThinkingLevel;
        use crate::provider::{Context, StreamOptions};

        // The production reasoning flag is DERIVED, not hardcoded.
        assert_eq!(model_is_reasoning("deepseek-v4-pro"), Some(true));
        assert_eq!(model_is_reasoning("deepseek-v4-flash"), Some(true));
        // Even against a non-reasoning provider default, the model classification wins.
        let reasoning = effective_reasoning("deepseek-v4-pro", false);
        assert!(
            reasoning,
            "deepseek-v4-pro must be reasoning via effective_reasoning/model_is_reasoning"
        );

        // Build the entry with the DERIVED reasoning flag (not a hardcoded true).
        let entry = make_model_entry_with_provider(
            "deepseek-v4-pro",
            reasoning,
            "deepseek",
            "https://api.deepseek.com",
        );
        assert!(entry.supports_xhigh());
        let effective = entry.clamp_thinking_level(ThinkingLevel::XHigh);
        assert_eq!(effective, ThinkingLevel::XHigh);

        let provider = crate::providers::openai::OpenAIProvider::new(entry.model.id.as_str())
            .with_provider_name(entry.model.provider.as_str())
            .with_reasoning(entry.model.reasoning);
        let context = Context {
            system_prompt: None,
            messages: vec![crate::model::Message::User(crate::model::UserMessage {
                content: crate::model::UserContent::Text("solve it".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::<crate::provider::ToolDef>::new().into(),
        };
        let options = StreamOptions {
            thinking_level: Some(effective),
            ..Default::default()
        };
        let body = serde_json::to_value(provider.build_request(&context, &options))
            .expect("serialize request");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(
            body["reasoning_effort"], "max",
            "xhigh must reach the wire as max via the real registry classification path"
        );
    }

    /// `deepseek-chat` classifies as non-reasoning, so it exposes only `[Off]`,
    /// the clamp pins to Off, and the transport emits NO `thinking`/`reasoning_effort`
    /// (pre-#113 wire body preserved — gh #114, finding 2).
    #[test]
    fn deepseek_chat_non_reasoning_emits_no_thinking_end_to_end() {
        use crate::model::ThinkingLevel;
        use crate::provider::{Context, StreamOptions};

        assert_eq!(model_is_reasoning("deepseek-chat"), Some(false));
        let reasoning = effective_reasoning("deepseek-chat", true);
        assert!(!reasoning, "deepseek-chat must classify as non-reasoning");

        let entry = make_model_entry_with_provider(
            "deepseek-chat",
            reasoning,
            "deepseek",
            "https://api.deepseek.com",
        );
        assert!(!entry.supports_xhigh());
        assert_eq!(entry.available_thinking_levels(), vec![ThinkingLevel::Off]);
        // Whatever the user asks for, a non-reasoning model clamps to Off.
        assert_eq!(
            entry.clamp_thinking_level(ThinkingLevel::XHigh),
            ThinkingLevel::Off
        );

        let provider = crate::providers::openai::OpenAIProvider::new(entry.model.id.as_str())
            .with_provider_name(entry.model.provider.as_str())
            .with_reasoning(entry.model.reasoning);
        let context = Context {
            system_prompt: None,
            messages: vec![crate::model::Message::User(crate::model::UserMessage {
                content: crate::model::UserContent::Text("hi".to_string()),
                timestamp: 0,
            })]
            .into(),
            tools: Vec::<crate::provider::ToolDef>::new().into(),
        };
        let options = StreamOptions {
            thinking_level: Some(entry.clamp_thinking_level(ThinkingLevel::XHigh)),
            ..Default::default()
        };
        let body = serde_json::to_value(provider.build_request(&context, &options))
            .expect("serialize request");
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn clamp_passthrough_for_regular_levels() {
        use crate::model::ThinkingLevel;
        let entry = make_model_entry("claude-sonnet-4-20250514", true);
        assert_eq!(
            entry.clamp_thinking_level(ThinkingLevel::High),
            ThinkingLevel::High
        );
        assert_eq!(
            entry.clamp_thinking_level(ThinkingLevel::Medium),
            ThinkingLevel::Medium
        );
        assert_eq!(
            entry.clamp_thinking_level(ThinkingLevel::Low),
            ThinkingLevel::Low
        );
        assert_eq!(
            entry.clamp_thinking_level(ThinkingLevel::Minimal),
            ThinkingLevel::Minimal
        );
        assert_eq!(
            entry.clamp_thinking_level(ThinkingLevel::Off),
            ThinkingLevel::Off
        );
    }

    // ─── ad_hoc_provider_defaults ────────────────────────────────────

    #[test]
    fn ad_hoc_known_providers() {
        let providers = [
            "anthropic",
            "openai",
            "google",
            "cohere",
            "amazon-bedrock",
            "groq",
            "deepinfra",
            "cerebras",
            "openrouter",
            "mistral",
            "deepseek",
            "fireworks",
            "togetherai",
            "perplexity",
            "xai",
            "baseten",
            "llama",
            "lmstudio",
            "ollama-cloud",
        ];
        for provider in providers {
            assert!(
                ad_hoc_provider_defaults(provider).is_some(),
                "expected defaults for '{provider}'"
            );
        }
    }

    #[test]
    fn ad_hoc_alibaba_aliases() {
        for alias in ["alibaba", "dashscope", "qwen"] {
            let defaults = ad_hoc_provider_defaults(alias)
                .unwrap_or_else(|| unreachable!("expected defaults for '{alias}'"));
            assert!(defaults.base_url.contains("dashscope"));
        }
    }

    #[test]
    fn ad_hoc_moonshot_aliases() {
        for alias in ["moonshotai", "moonshot", "kimi"] {
            let defaults = ad_hoc_provider_defaults(alias)
                .unwrap_or_else(|| unreachable!("expected defaults for '{alias}'"));
            assert!(defaults.base_url.contains("moonshot"));
        }
    }

    #[test]
    fn ad_hoc_batch_b1_defaults_resolve_expected_routes() {
        let alibaba_cn =
            ad_hoc_provider_defaults("alibaba-cn").expect("expected defaults for alibaba-cn");
        assert_eq!(alibaba_cn.api, "openai-completions");
        assert!(alibaba_cn.auth_header);
        assert!(alibaba_cn.base_url.contains("dashscope.aliyuncs.com"));

        let alibaba_us =
            ad_hoc_provider_defaults("alibaba-us").expect("expected defaults for alibaba-us");
        assert_eq!(alibaba_us.api, "openai-completions");
        assert!(alibaba_us.auth_header);
        assert!(alibaba_us.base_url.contains("dashscope-us.aliyuncs.com"));

        let kimi_for_coding = ad_hoc_provider_defaults("kimi-for-coding")
            .expect("expected defaults for kimi-for-coding");
        assert_eq!(kimi_for_coding.api, "anthropic-messages");
        assert!(!kimi_for_coding.auth_header);
        assert!(kimi_for_coding.base_url.contains("api.kimi.com/coding"));

        for provider in [
            "minimax",
            "minimax-cn",
            "minimax-coding-plan",
            "minimax-cn-coding-plan",
        ] {
            let defaults = ad_hoc_provider_defaults(provider)
                .unwrap_or_else(|| unreachable!("expected defaults for '{provider}'"));
            assert_eq!(defaults.api, "anthropic-messages");
            assert!(!defaults.auth_header);
            assert!(defaults.base_url.contains("api.minimax"));
        }
    }

    #[test]
    fn ad_hoc_batch_b2_defaults_resolve_expected_routes() {
        let cases = [
            ("modelscope", "https://api-inference.modelscope.cn/v1"),
            ("moonshotai-cn", "https://api.moonshot.cn/v1"),
            ("nebius", "https://api.tokenfactory.nebius.com/v1"),
            (
                "ovhcloud",
                "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1",
            ),
            ("scaleway", "https://api.scaleway.ai/v1"),
        ];
        for (provider, expected_base_url) in &cases {
            let defaults = ad_hoc_provider_defaults(provider)
                .unwrap_or_else(|| unreachable!("expected defaults for '{provider}'"));
            assert_eq!(defaults.api, "openai-completions");
            assert!(defaults.auth_header);
            assert_eq!(defaults.base_url, *expected_base_url);
        }
    }

    #[test]
    fn ad_hoc_batch_b3_defaults_resolve_expected_routes() {
        let cases = [
            ("siliconflow", "https://api.siliconflow.com/v1"),
            ("siliconflow-cn", "https://api.siliconflow.cn/v1"),
            ("upstage", "https://api.upstage.ai/v1/solar"),
            ("venice", "https://api.venice.ai/api/v1"),
            ("zai", "https://api.z.ai/api/paas/v4"),
            ("zai-coding-plan", "https://api.z.ai/api/coding/paas/v4"),
            ("zhipuai", "https://open.bigmodel.cn/api/paas/v4"),
            (
                "zhipuai-coding-plan",
                "https://open.bigmodel.cn/api/coding/paas/v4",
            ),
        ];
        for (provider, expected_base_url) in &cases {
            let defaults = ad_hoc_provider_defaults(provider)
                .unwrap_or_else(|| unreachable!("expected defaults for '{provider}'"));
            assert_eq!(defaults.api, "openai-completions");
            assert!(defaults.auth_header);
            assert_eq!(defaults.base_url, *expected_base_url);
        }
    }

    #[test]
    fn ad_hoc_batch_b3_coding_plan_and_regional_variants_remain_distinct() {
        let siliconflow = ad_hoc_provider_defaults("siliconflow").expect("siliconflow defaults");
        let siliconflow_cn =
            ad_hoc_provider_defaults("siliconflow-cn").expect("siliconflow-cn defaults");
        assert_eq!(canonical_provider_id("siliconflow"), Some("siliconflow"));
        assert_eq!(
            canonical_provider_id("siliconflow-cn"),
            Some("siliconflow-cn")
        );
        assert_ne!(siliconflow.base_url, siliconflow_cn.base_url);

        let zai = ad_hoc_provider_defaults("zai").expect("zai defaults");
        let zai_coding = ad_hoc_provider_defaults("zai-coding-plan").expect("zai-coding defaults");
        assert_eq!(canonical_provider_id("zai"), Some("zai"));
        assert_eq!(
            canonical_provider_id("zai-coding-plan"),
            Some("zai-coding-plan")
        );
        assert_eq!(zai.api, "openai-completions");
        assert_eq!(zai_coding.api, "openai-completions");
        assert_ne!(zai.base_url, zai_coding.base_url);

        let zhipu = ad_hoc_provider_defaults("zhipuai").expect("zhipu defaults");
        let zhipu_coding =
            ad_hoc_provider_defaults("zhipuai-coding-plan").expect("zhipu-coding defaults");
        assert_eq!(canonical_provider_id("zhipuai"), Some("zhipuai"));
        assert_eq!(
            canonical_provider_id("zhipuai-coding-plan"),
            Some("zhipuai-coding-plan")
        );
        assert_eq!(zhipu.api, "openai-completions");
        assert_eq!(zhipu_coding.api, "openai-completions");
        assert_ne!(zhipu.base_url, zhipu_coding.base_url);
    }

    #[test]
    fn ad_hoc_batch_c1_defaults_resolve_expected_routes() {
        let cases = [
            ("baseten", "https://inference.baseten.co/v1"),
            ("llama", "https://api.llama.com/compat/v1"),
            ("lmstudio", "http://127.0.0.1:1234/v1"),
            ("ollama-cloud", "https://ollama.com/v1"),
        ];
        for (provider, expected_base_url) in &cases {
            let defaults = ad_hoc_provider_defaults(provider)
                .unwrap_or_else(|| unreachable!("expected defaults for '{provider}'"));
            assert_eq!(defaults.api, "openai-completions");
            assert!(defaults.auth_header);
            assert_eq!(defaults.base_url, *expected_base_url);
        }
    }

    #[test]
    fn ad_hoc_kimi_alias_and_kimi_for_coding_remain_distinct() {
        assert_eq!(canonical_provider_id("kimi"), Some("moonshotai"));
        assert_eq!(
            canonical_provider_id("kimi-for-coding"),
            Some("kimi-for-coding")
        );

        let kimi_alias = ad_hoc_provider_defaults("kimi").expect("kimi alias defaults");
        let kimi_for_coding =
            ad_hoc_provider_defaults("kimi-for-coding").expect("kimi-for-coding defaults");
        assert!(kimi_alias.base_url.contains("moonshot.ai"));
        assert!(kimi_for_coding.base_url.contains("api.kimi.com"));
        assert_ne!(kimi_alias.base_url, kimi_for_coding.base_url);
        assert_ne!(kimi_alias.api, kimi_for_coding.api);
    }

    #[test]
    fn ad_hoc_alibaba_cn_is_distinct_from_alibaba_family_aliases() {
        let alibaba = ad_hoc_provider_defaults("alibaba").expect("alibaba defaults");
        let alibaba_cn = ad_hoc_provider_defaults("alibaba-cn").expect("alibaba-cn defaults");
        let alibaba_us = ad_hoc_provider_defaults("alibaba-us").expect("alibaba-us defaults");
        assert_eq!(canonical_provider_id("dashscope"), Some("alibaba"));
        assert_eq!(canonical_provider_id("alibaba-cn"), Some("alibaba-cn"));
        assert_eq!(canonical_provider_id("alibaba-us"), Some("alibaba-us"));
        assert_eq!(alibaba.api, "openai-completions");
        assert_eq!(alibaba_cn.api, "openai-completions");
        assert_eq!(alibaba_us.api, "openai-completions");
        assert_ne!(alibaba.base_url, alibaba_cn.base_url);
        assert_ne!(alibaba.base_url, alibaba_us.base_url);
        assert_ne!(alibaba_cn.base_url, alibaba_us.base_url);
    }

    #[test]
    fn ad_hoc_moonshot_cn_is_distinct_from_global_moonshot_aliases() {
        let moonshot_global = ad_hoc_provider_defaults("moonshot").expect("moonshot defaults");
        let moonshot_cn =
            ad_hoc_provider_defaults("moonshotai-cn").expect("moonshotai-cn defaults");
        assert_eq!(canonical_provider_id("moonshot"), Some("moonshotai"));
        assert_eq!(
            canonical_provider_id("moonshotai-cn"),
            Some("moonshotai-cn")
        );
        assert_eq!(moonshot_global.api, "openai-completions");
        assert_eq!(moonshot_cn.api, "openai-completions");
        assert_ne!(moonshot_global.base_url, moonshot_cn.base_url);
    }

    #[test]
    fn ad_hoc_unknown_returns_none() {
        assert!(ad_hoc_provider_defaults("unknown-provider").is_none());
        assert!(ad_hoc_provider_defaults("").is_none());
    }

    #[test]
    fn ad_hoc_anthropic_uses_messages_api() {
        let defaults = ad_hoc_provider_defaults("anthropic").unwrap();
        assert_eq!(defaults.api, "anthropic-messages");
        assert_eq!(defaults.base_url, "https://api.anthropic.com/v1/messages");
        assert!(defaults.reasoning);
    }

    #[test]
    fn ad_hoc_openai_uses_responses_api() {
        let defaults = ad_hoc_provider_defaults("openai").unwrap();
        assert_eq!(defaults.api, "openai-responses");
    }

    #[test]
    fn ad_hoc_groq_uses_completions_api() {
        let defaults = ad_hoc_provider_defaults("groq").unwrap();
        assert_eq!(defaults.api, "openai-completions");
        assert!(defaults.base_url.contains("groq.com"));
    }

    #[test]
    fn ad_hoc_bedrock_uses_converse_api() {
        let defaults = ad_hoc_provider_defaults("amazon-bedrock").unwrap();
        assert_eq!(defaults.api, "bedrock-converse-stream");
        assert_eq!(defaults.base_url, "");
        assert!(!defaults.auth_header);
    }

    #[test]
    fn request_time_auth_providers_are_not_blocked_by_generic_api_key_preflight() {
        let bedrock = ad_hoc_model_entry_with_sap_resolver(
            "bedrock",
            "anthropic.claude-3-5-sonnet-20240620-v1:0",
            || None,
        )
        .expect("bedrock ad-hoc entry");
        assert!(!model_requires_configured_credential(&bedrock));
        assert!(model_entry_is_ready(&bedrock));

        let sap = ad_hoc_model_entry_with_sap_resolver("sap-ai-core", "deployment-a", || {
            Some(SapResolvedCredentials {
                client_id: "sap-client".to_string(),
                // ubs:ignore test fixture credential, not live secret.
                client_secret: "sap-secret".to_string(),
                token_url: "https://auth.sap.example.com/oauth/token".to_string(),
                service_url: "https://api.ai.sap.example.com".to_string(),
            })
        })
        .expect("sap ad-hoc entry");
        assert!(!model_requires_configured_credential(&sap));
        assert!(model_entry_is_ready(&sap));
    }

    #[test]
    fn native_adapter_seed_defaults_gitlab_use_gitlab_chat_api() {
        let defaults = native_adapter_seed_defaults("gitlab").expect("gitlab seed defaults");
        assert_eq!(defaults.api, "gitlab-chat");
        assert_eq!(defaults.base_url, "");
        assert!(defaults.auth_header);
        assert!(defaults.reasoning);
        assert_eq!(defaults.input, &INPUT_TEXT_ONLY);
    }

    // ─── ad_hoc_model_entry ──────────────────────────────────────────

    #[test]
    fn ad_hoc_model_entry_creates_valid_entry() {
        // Use the pure SAP-resolver seam so the assertion stays hermetic and
        // independent of ambient `GROQ_API_KEY` / on-disk auth (the public
        // `ad_hoc_model_entry` intentionally resolves credentials).
        let entry = ad_hoc_model_entry_with_sap_resolver("groq", "llama-3-70b", || None).unwrap();
        assert_eq!(entry.model.id, "llama-3-70b");
        assert_eq!(entry.model.name, "llama-3-70b");
        assert_eq!(entry.model.provider, "groq");
        assert_eq!(entry.model.api, "openai-completions");
        assert!(entry.model.base_url.contains("groq.com"));
        assert!(entry.auth_header); // openai-compatible → auth_header = true
        assert!(entry.api_key.is_none()); // pure synthesis performs no auth lookup
    }

    #[test]
    fn ad_hoc_model_entry_anthropic_no_auth_header() {
        let entry = ad_hoc_model_entry("anthropic", "claude-custom").unwrap();
        assert!(!entry.auth_header); // anthropic uses x-api-key, not Authorization
    }

    #[test]
    fn ad_hoc_model_entry_unknown_returns_none() {
        assert!(ad_hoc_model_entry("nonexistent", "model").is_none());
    }

    #[test]
    fn sap_chat_completions_endpoint_formats_expected_path() {
        let endpoint =
            sap_chat_completions_endpoint("https://api.ai.sap.example.com/", "deployment-a")
                .expect("endpoint");
        assert_eq!(
            endpoint,
            "https://api.ai.sap.example.com/v2/inference/deployments/deployment-a/chat/completions"
        );
    }

    #[test]
    fn ad_hoc_model_entry_supports_sap_with_resolved_service_key() {
        let entry = ad_hoc_model_entry_with_sap_resolver("sap-ai-core", "dep-123", || {
            Some(SapResolvedCredentials {
                client_id: "id".to_string(),
                client_secret: "secret".to_string(),
                token_url: "https://auth.sap.example.com/oauth/token".to_string(),
                service_url: "https://api.ai.sap.example.com".to_string(),
            })
        })
        .expect("sap ad-hoc entry");

        assert_eq!(entry.model.provider, "sap-ai-core");
        assert_eq!(entry.model.api, "openai-completions");
        assert_eq!(
            entry.model.base_url,
            "https://api.ai.sap.example.com/v2/inference/deployments/dep-123/chat/completions"
        );
        assert!(entry.auth_header);
    }

    #[test]
    fn ad_hoc_model_entry_supports_sap_alias() {
        let entry = ad_hoc_model_entry_with_sap_resolver("sap", "dep-123", || {
            Some(SapResolvedCredentials {
                client_id: "id".to_string(),
                client_secret: "secret".to_string(),
                token_url: "https://auth.sap.example.com/oauth/token".to_string(),
                service_url: "https://api.ai.sap.example.com".to_string(),
            })
        })
        .expect("sap alias ad-hoc entry");

        assert_eq!(entry.model.provider, "sap");
        assert_eq!(entry.model.api, "openai-completions");
        assert!(entry.auth_header);
    }

    #[test]
    fn ad_hoc_model_entry_sap_without_credentials_returns_none() {
        assert!(ad_hoc_model_entry_with_sap_resolver("sap-ai-core", "dep-123", || None).is_none());
    }

    #[test]
    fn ad_hoc_model_entry_sap_uses_effective_reasoning() {
        let sap_creds = || {
            Some(SapResolvedCredentials {
                client_id: "id".to_string(),
                client_secret: "secret".to_string(),
                token_url: "https://auth.sap.example.com/oauth/token".to_string(),
                service_url: "https://api.ai.sap.example.com".to_string(),
            })
        };

        // A reasoning model (gpt-5.2) should have reasoning = true.
        let reasoning_entry =
            ad_hoc_model_entry_with_sap_resolver("sap-ai-core", "gpt-5.2", sap_creds)
                .expect("reasoning sap entry");
        assert!(reasoning_entry.model.reasoning);

        // A non-reasoning model (gpt-4o) should have reasoning = false.
        let non_reasoning_entry =
            ad_hoc_model_entry_with_sap_resolver("sap-ai-core", "gpt-4o", sap_creds)
                .expect("non-reasoning sap entry");
        assert!(!non_reasoning_entry.model.reasoning);
    }

    // ─── merge_headers ───────────────────────────────────────────────

    #[test]
    fn merge_headers_combines_both() {
        let base = HashMap::from([
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
        ]);
        let overrides = HashMap::from([
            ("b".to_string(), "override".to_string()),
            ("c".to_string(), "3".to_string()),
        ]);
        let merged = merge_headers(&base, overrides);
        assert_eq!(merged.get("a").unwrap(), "1");
        assert_eq!(merged.get("b").unwrap(), "override");
        assert_eq!(merged.get("c").unwrap(), "3");
    }

    #[test]
    fn merge_headers_empty_base() {
        let merged = merge_headers(
            &HashMap::new(),
            HashMap::from([("x".to_string(), "y".to_string())]),
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged.get("x").unwrap(), "y");
    }

    #[test]
    fn merge_headers_empty_overrides() {
        let base = HashMap::from([("x".to_string(), "y".to_string())]);
        let merged = merge_headers(&base, HashMap::new());
        assert_eq!(merged, base);
    }

    // ─── resolve_value ───────────────────────────────────────────────

    #[test]
    fn resolve_value_plain_literal() {
        assert_eq!(resolve_value("my-key").as_deref(), Some("my-key"));
    }

    #[test]
    fn resolve_value_empty_returns_none() {
        assert!(resolve_value("").is_none());
    }

    #[test]
    fn resolve_value_env_empty_var_name_returns_none() {
        assert!(resolve_value("env:").is_none());
    }

    #[test]
    fn resolve_value_file_empty_path_returns_none() {
        assert!(resolve_value("file:").is_none());
    }

    #[test]
    fn resolve_value_file_missing_returns_none() {
        assert!(resolve_value("file:/nonexistent/path/key.txt").is_none());
    }

    #[test]
    fn resolve_value_file_relative_to_base_dir() {
        let dir = tempdir().expect("tempdir");
        let nested = dir.path().join("config");
        std::fs::create_dir_all(&nested).expect("create nested dir");
        let key_path = nested.join("relative-key.txt");
        std::fs::write(&key_path, "relative-value\n").expect("write relative key");

        assert_eq!(
            resolve_value_with_base("file:relative-key.txt", Some(&nested)).as_deref(),
            Some("relative-value")
        );
    }

    #[test]
    fn resolve_value_shell_echo() {
        let result = resolve_value("!echo hello");
        assert_eq!(result.as_deref(), Some("hello"));
    }

    #[test]
    fn resolve_value_shell_failing_command() {
        assert!(resolve_value("!false").is_none());
    }

    // ─── resolve_headers ─────────────────────────────────────────────

    #[test]
    fn resolve_headers_none_returns_empty() {
        assert!(resolve_headers(None).is_empty());
    }

    #[test]
    fn resolve_headers_resolves_literal_values() {
        let mut headers = HashMap::new();
        headers.insert("x-key".to_string(), "literal-value".to_string());
        let resolved = resolve_headers(Some(&headers));
        assert_eq!(resolved.get("x-key").unwrap(), "literal-value");
    }

    // ─── ModelRegistry ───────────────────────────────────────────────

    #[test]
    fn model_registry_get_available_returns_only_ready_models() {
        let (_dir, auth) = test_auth_storage();
        let registry = ModelRegistry::load(&auth, None);
        let available = registry.get_available();
        assert!(!available.is_empty());
        for entry in &available {
            assert!(
                model_entry_is_ready(entry),
                "all available models should be ready for use"
            );
        }
    }

    #[test]
    fn model_registry_get_available_includes_keyless_models() {
        let dir = tempdir().expect("tempdir");
        let auth = AuthStorage::load(dir.path().join("auth.json")).expect("auth");
        let models_path = dir.path().join("models.json");
        let config = serde_json::json!({
            "providers": {
                "acme-local": {
                    "baseUrl": "http://127.0.0.1:11434/v1",
                    "api": "openai-completions",
                    "authHeader": false,
                    "models": [
                        { "id": "dev-model", "name": "Dev Model", "reasoning": false }
                    ]
                }
            }
        });
        std::fs::write(
            &models_path,
            serde_json::to_string(&config).expect("serialize models"),
        )
        .expect("write models.json");

        let registry = ModelRegistry::load(&auth, Some(models_path));
        let available = registry.get_available();
        assert!(
            available
                .iter()
                .any(|entry| entry.model.provider == "acme-local" && entry.model.id == "dev-model"),
            "keyless models should be considered available"
        );
    }

    #[test]
    fn local_providers_synthesize_ready_keyless_entries() {
        // #104: ollama, llamacpp and mistralrs are local OpenAI-compatible
        // providers with no API key. A `--provider X --model Y` invocation
        // synthesizes an ad-hoc entry; that entry must be considered READY
        // without any configured credential, so the agent attempts a connection
        // to the local server instead of erroring with "Missing API key".
        for provider in ["ollama", "llamacpp", "mistralrs"] {
            let entry = ad_hoc_model_entry(provider, "some-local-model")
                .unwrap_or_else(|| unreachable!("expected ad-hoc entry for '{provider}'"));
            assert_eq!(entry.model.provider, provider);
            assert!(
                !entry.auth_header,
                "{provider} ad-hoc entry must not require an auth header"
            );
            assert!(
                !model_requires_configured_credential(&entry),
                "{provider} must not require a configured credential"
            );
            assert!(
                model_entry_is_ready(&entry),
                "{provider} ad-hoc entry must be ready without an API key"
            );
        }
    }

    #[test]
    fn model_registry_error_none_for_valid_load() {
        let (_dir, auth) = test_auth_storage();
        let registry = ModelRegistry::load(&auth, None);
        assert!(registry.error().is_none());
    }

    #[test]
    fn model_registry_error_on_invalid_json() {
        let dir = tempdir().expect("tempdir");
        let auth = AuthStorage::load(dir.path().join("auth.json")).expect("auth");
        let models_path = dir.path().join("models.json");
        std::fs::write(&models_path, "not valid json").expect("write bad json");
        let registry = ModelRegistry::load(&auth, Some(models_path));
        assert!(registry.error().is_some());
    }

    #[test]
    fn model_registry_load_missing_models_json_is_fine() {
        let dir = tempdir().expect("tempdir");
        let auth = AuthStorage::load(dir.path().join("auth.json")).expect("auth");
        let registry = ModelRegistry::load(&auth, Some(dir.path().join("nonexistent.json")));
        assert!(registry.error().is_none());
    }

    // ─── default_models_path ─────────────────────────────────────────

    #[test]
    fn default_models_path_joins_correctly() {
        let path = default_models_path(Path::new("/home/user/.pi"));
        assert_eq!(path, PathBuf::from("/home/user/.pi/models.json"));
    }

    #[test]
    fn fetched_provider_bound_covers_supported_provider_inventory() {
        assert!(
            crate::provider_metadata::PROVIDER_METADATA.len() <= MAX_FETCHED_PROVIDERS,
            "generated catalog provider cap must cover every supported provider"
        );
    }

    // ─── ModelsConfig deserialization ────────────────────────────────

    #[test]
    fn models_config_deserialize_camel_case() {
        let json = r#"{
            "providers": {
                "acme": {
                    "baseUrl": "https://acme.com/v1",
                    "apiKey": "env:ACME_KEY",
                    "authHeader": true,
                    "models": [{
                        "id": "acme-1",
                        "contextWindow": 32000,
                        "maxTokens": 2048
                    }]
                }
            }
        }"#;
        let config: ModelsConfig = serde_json::from_str(json).expect("parse");
        let acme = config.providers.get("acme").expect("acme provider");
        assert_eq!(acme.base_url.as_deref(), Some("https://acme.com/v1"));
        assert_eq!(acme.auth_header, Some(true));
        let model = &acme.models.as_ref().unwrap()[0];
        assert_eq!(model.context_window, Some(32000));
        assert_eq!(model.max_tokens, Some(2048));
    }

    #[test]
    fn models_config_empty_providers_ok() {
        let json = r#"{"providers": {}}"#;
        let config: ModelsConfig = serde_json::from_str(json).expect("parse");
        assert!(config.providers.is_empty());
    }

    #[test]
    fn compat_config_deserialize() {
        let json = r#"{
            "supportsStore": true,
            "supportsDeveloperRole": false,
            "supportsReasoningEffort": true,
            "supportsUsageInStreaming": false,
            "maxTokensField": "max_completion_tokens"
        }"#;
        let compat: CompatConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(compat.supports_store, Some(true));
        assert_eq!(compat.supports_developer_role, Some(false));
        assert_eq!(compat.supports_reasoning_effort, Some(true));
        assert_eq!(compat.supports_usage_in_streaming, Some(false));
        assert_eq!(
            compat.max_tokens_field.as_deref(),
            Some("max_completion_tokens")
        );
    }

    #[test]
    fn compat_config_deserialize_all_fields() {
        let json = r#"{
            "supportsStore": true,
            "supportsDeveloperRole": true,
            "supportsReasoningEffort": false,
            "supportsUsageInStreaming": false,
            "supportsTools": false,
            "supportsStreaming": true,
            "supportsParallelToolCalls": false,
            "maxTokensField": "max_completion_tokens",
            "systemRoleName": "developer",
            "stopReasonField": "finish_reason",
            "customHeaders": {"X-Region": "us-east-1", "X-Tag": "override"},
            "openRouterRouting": {"order": ["fallback"]},
            "vercelGatewayRouting": {"priority": 1}
        }"#;
        let compat: CompatConfig = serde_json::from_str(json).expect("parse");
        assert_eq!(compat.supports_tools, Some(false));
        assert_eq!(compat.supports_streaming, Some(true));
        assert_eq!(compat.supports_parallel_tool_calls, Some(false));
        assert_eq!(compat.system_role_name.as_deref(), Some("developer"));
        assert_eq!(compat.stop_reason_field.as_deref(), Some("finish_reason"));
        let custom = compat.custom_headers.as_ref().expect("custom_headers");
        assert_eq!(
            custom.get("X-Region").map(String::as_str),
            Some("us-east-1")
        );
        assert_eq!(custom.get("X-Tag").map(String::as_str), Some("override"));
        assert!(compat.open_router_routing.is_some());
        assert!(compat.vercel_gateway_routing.is_some());
    }

    #[test]
    fn compat_config_default_all_none() {
        let compat = CompatConfig::default();
        assert!(compat.supports_store.is_none());
        assert!(compat.supports_tools.is_none());
        assert!(compat.supports_streaming.is_none());
        assert!(compat.max_tokens_field.is_none());
        assert!(compat.system_role_name.is_none());
        assert!(compat.stop_reason_field.is_none());
        assert!(compat.custom_headers.is_none());
    }

    #[test]
    fn compat_config_deserialize_empty_object() {
        let compat: CompatConfig = serde_json::from_str("{}").expect("parse");
        assert!(compat.supports_store.is_none());
        assert!(compat.supports_tools.is_none());
        assert!(compat.custom_headers.is_none());
    }

    // ─── apply_custom_models: provider replaces built-ins ────────────

    #[test]
    fn apply_custom_models_replaces_built_in_when_models_specified() {
        let (_dir, auth) = test_auth_storage();
        let mut models = built_in_models(&auth, ModelRegistryLoadMode::Full);
        let anthropic_before = models
            .iter()
            .filter(|m| m.model.provider == "anthropic")
            .count();
        assert!(anthropic_before > 0);

        let config = ModelsConfig {
            providers: HashMap::from([(
                "anthropic".to_string(),
                ProviderConfig {
                    base_url: Some("https://proxy.example/v1".to_string()),
                    api: Some("anthropic-messages".to_string()),
                    models: Some(vec![ModelConfig {
                        id: "custom-claude".to_string(),
                        name: Some("Custom Claude".to_string()),
                        ..ModelConfig::default()
                    }]),
                    ..ProviderConfig::default()
                },
            )]),
        };

        apply_custom_models(&auth, &mut models, &config, None);

        // Built-in anthropic models should be replaced
        let anthropic_after: Vec<_> = models
            .iter()
            .filter(|m| m.model.provider == "anthropic")
            .collect();
        assert_eq!(anthropic_after.len(), 1);
        assert_eq!(anthropic_after[0].model.id, "custom-claude");
    }

    #[test]
    fn apply_custom_models_alias_replaces_canonical_built_ins_when_models_specified() {
        let (_dir, auth) = test_auth_storage();
        let mut models = built_in_models(&auth, ModelRegistryLoadMode::Full);
        let google_before = models
            .iter()
            .filter(|m| m.model.provider == "google")
            .count();
        assert!(google_before > 0);

        let config = ModelsConfig {
            providers: HashMap::from([(
                "gemini".to_string(),
                ProviderConfig {
                    models: Some(vec![ModelConfig {
                        id: "gemini-custom".to_string(),
                        name: Some("Gemini Custom".to_string()),
                        ..ModelConfig::default()
                    }]),
                    ..ProviderConfig::default()
                },
            )]),
        };

        apply_custom_models(&auth, &mut models, &config, None);

        assert!(
            !models.iter().any(|m| m.model.provider == "google"),
            "canonical google built-ins should be replaced when alias config provides explicit models"
        );
        let gemini_models: Vec<_> = models
            .iter()
            .filter(|m| m.model.provider == "gemini")
            .collect();
        assert_eq!(gemini_models.len(), 1);
        assert_eq!(gemini_models[0].model.id, "gemini-custom");
    }

    #[test]
    fn apply_custom_models_alias_override_without_models_updates_canonical_provider_models() {
        let (_dir, auth) = test_auth_storage();
        let mut models = built_in_models(&auth, ModelRegistryLoadMode::Full);
        let google_before = models
            .iter()
            .filter(|m| m.model.provider == "google")
            .count();
        assert!(google_before > 0);

        let config = ModelsConfig {
            providers: HashMap::from([(
                "gemini".to_string(),
                ProviderConfig {
                    base_url: Some("https://proxy.example/v1".to_string()),
                    api: Some("google-generative-ai".to_string()),
                    auth_header: Some(true),
                    ..ProviderConfig::default()
                },
            )]),
        };

        apply_custom_models(&auth, &mut models, &config, None);

        let google_after: Vec<_> = models
            .iter()
            .filter(|m| m.model.provider == "google")
            .collect();
        assert_eq!(google_after.len(), google_before);
        assert!(
            google_after
                .iter()
                .all(|m| m.model.base_url == "https://proxy.example/v1")
        );
        assert!(
            google_after
                .iter()
                .all(|m| m.model.api == "google-generative-ai")
        );
        assert!(google_after.iter().all(|m| m.auth_header));
    }

    #[test]
    fn model_registry_find_canonical_provider_matches_alias_backed_custom_model() {
        let (_dir, auth) = test_auth_storage();
        let mut models = Vec::new();
        let config = ModelsConfig {
            providers: HashMap::from([(
                "gemini".to_string(),
                ProviderConfig {
                    models: Some(vec![ModelConfig {
                        id: "gemini-custom-find".to_string(),
                        ..ModelConfig::default()
                    }]),
                    ..ProviderConfig::default()
                },
            )]),
        };

        apply_custom_models(&auth, &mut models, &config, None);
        let registry = ModelRegistry {
            models,
            error: None,
        };

        assert!(
            registry.find("gemini", "gemini-custom-find").is_some(),
            "alias lookup should resolve"
        );
        assert!(
            registry.find("google", "gemini-custom-find").is_some(),
            "canonical provider lookup should also match alias-backed model"
        );
    }

    // ─── OAuthConfig ─────────────────────────────────────────────────

    #[test]
    fn oauth_config_fields() {
        let config = OAuthConfig {
            auth_url: "https://auth.example.com/authorize".to_string(),
            token_url: "https://auth.example.com/token".to_string(),
            client_id: "client-123".to_string(),
            scopes: vec!["read".to_string(), "write".to_string()],
            redirect_uri: Some("http://localhost:8080/callback".to_string()),
        };
        assert_eq!(config.client_id, "client-123");
        assert_eq!(config.scopes.len(), 2);
        assert!(config.redirect_uri.is_some());
    }

    // ─── Built-in model properties ───────────────────────────────────

    #[test]
    fn built_in_anthropic_models_use_correct_api() {
        let (_dir, auth) = test_auth_storage();
        let models = built_in_models(&auth, ModelRegistryLoadMode::Full);
        for m in models.iter().filter(|m| m.model.provider == "anthropic") {
            assert_eq!(m.model.api, "anthropic-messages");
            assert!(!m.auth_header, "anthropic uses x-api-key, not auth header");
            assert!(
                m.model.context_window >= 200_000,
                "anthropic model {} should expose a modern context window",
                m.model.id
            );
        }
    }

    #[test]
    fn built_in_openai_models_use_auth_header() {
        let (_dir, auth) = test_auth_storage();
        let models = built_in_models(&auth, ModelRegistryLoadMode::Full);
        for m in models.iter().filter(|m| m.model.provider == "openai") {
            assert!(m.auth_header, "openai uses Authorization header");
            assert_eq!(m.model.api, "openai-responses");
        }
    }

    #[test]
    fn built_in_google_models_no_auth_header() {
        let (_dir, auth) = test_auth_storage();
        let models = built_in_models(&auth, ModelRegistryLoadMode::Full);
        for m in models.iter().filter(|m| m.model.provider == "google") {
            assert!(!m.auth_header, "google uses api key in URL, not header");
            assert_eq!(m.model.api, "google-generative-ai");
        }
    }

    #[test]
    fn built_in_reasoning_models_marked_correctly() {
        let (_dir, auth) = test_auth_storage();
        let models = built_in_models(&auth, ModelRegistryLoadMode::Full);
        // Legacy Haiku 3.5 should remain non-reasoning.
        for m in models
            .iter()
            .filter(|m| m.model.id.contains("3-5-haiku-20241022"))
        {
            assert!(!m.model.reasoning, "{} should be non-reasoning", m.model.id);
        }
        let anthropic_opus_sonnet = models
            .iter()
            .filter(|m| {
                m.model.provider == "anthropic"
                    && (m.model.id.contains("opus") || m.model.id.contains("sonnet"))
            })
            .collect::<Vec<_>>();
        assert!(
            !anthropic_opus_sonnet.is_empty(),
            "expected anthropic opus/sonnet models in built-ins"
        );
        assert!(
            anthropic_opus_sonnet.iter().any(|m| m.model.reasoning),
            "expected at least one reasoning anthropic opus/sonnet model"
        );

        // Modern Opus/Sonnet 4 family should be reasoning-enabled.
        for m in anthropic_opus_sonnet
            .iter()
            .filter(|m| m.model.id.contains("opus-4") || m.model.id.contains("sonnet-4"))
        {
            assert!(m.model.reasoning, "{} should be reasoning", m.model.id);
        }
    }

    #[test]
    fn model_is_reasoning_known_families() {
        // OpenAI
        assert_eq!(model_is_reasoning("o1-preview"), Some(true));
        assert_eq!(model_is_reasoning("o3-mini"), Some(true));
        assert_eq!(model_is_reasoning("o4-mini"), Some(true));
        assert_eq!(model_is_reasoning("gpt-5"), Some(true));
        assert_eq!(model_is_reasoning("gpt-4o"), Some(false));
        assert_eq!(model_is_reasoning("gpt-4-turbo"), Some(false));
        assert_eq!(model_is_reasoning("gpt-3.5-turbo"), Some(false));

        // Anthropic
        assert_eq!(model_is_reasoning("claude-sonnet-4-20250514"), Some(true));
        assert_eq!(model_is_reasoning("claude-opus-4-20250514"), Some(true));
        assert_eq!(model_is_reasoning("claude-3-5-sonnet-20241022"), Some(true));
        assert_eq!(model_is_reasoning("claude-3-5-haiku-20241022"), Some(false));
        assert_eq!(model_is_reasoning("claude-3-haiku-20240307"), Some(false));
        assert_eq!(model_is_reasoning("claude-3-opus-20240229"), Some(false));
        assert_eq!(model_is_reasoning("claude-3-sonnet-20240229"), Some(false));

        // Google
        assert_eq!(model_is_reasoning("gemini-2.5-pro"), Some(true));
        assert_eq!(model_is_reasoning("gemini-2.5-flash"), Some(true));
        assert_eq!(
            model_is_reasoning("gemini-2.0-flash-thinking-exp"),
            Some(true)
        );
        assert_eq!(model_is_reasoning("gemini-2.0-flash"), Some(false));
        assert_eq!(model_is_reasoning("gemini-2.0-flash-lite"), Some(false));
        assert_eq!(model_is_reasoning("gemini-1.5-pro"), Some(false));

        // Cohere
        assert_eq!(model_is_reasoning("command-a-03-2025"), Some(true));
        assert_eq!(model_is_reasoning("command-r-plus"), Some(false));
        assert_eq!(model_is_reasoning("command-r"), Some(false));

        // DeepSeek
        assert_eq!(model_is_reasoning("deepseek-reasoner"), Some(true));
        assert_eq!(model_is_reasoning("deepseek-r1"), Some(true));
        assert_eq!(model_is_reasoning("deepseek-v4-pro"), Some(true));
        assert_eq!(model_is_reasoning("deepseek-v4-flash"), Some(true));
        assert_eq!(model_is_reasoning("deepseek-chat"), Some(false));
        assert_eq!(model_is_reasoning("deepseek-coder"), Some(false));

        // Qwen
        assert_eq!(model_is_reasoning("qwq-32b"), Some(true));
        assert_eq!(model_is_reasoning("qwq-1b"), Some(true));

        // Mistral
        assert_eq!(model_is_reasoning("mistral-large-latest"), Some(false));
        assert_eq!(model_is_reasoning("mistral-small-latest"), Some(false));
        assert_eq!(model_is_reasoning("codestral-latest"), Some(false));
        assert_eq!(model_is_reasoning("pixtral-large-latest"), Some(false));

        // Meta Llama
        assert_eq!(model_is_reasoning("llama-3.3-70b-versatile"), Some(false));
        assert_eq!(model_is_reasoning("llama-4-scout"), Some(false));

        // Unknown models return None (fall back to provider default)
        assert_eq!(model_is_reasoning("some-custom-model"), None);
        assert_eq!(model_is_reasoning("my-fine-tune"), None);
    }

    // -------- User model overrides (issue #60) --------

    #[test]
    fn parse_user_model_overrides_at_returns_empty_for_missing_file() {
        let dir = tempdir().expect("tempdir");
        let missing = dir.path().join("nope.json");
        assert!(parse_user_model_overrides_at(&missing).is_empty());
    }

    #[test]
    fn parse_user_model_overrides_at_returns_empty_for_blank_file() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("models-override.json");
        fs::write(&path, "   \n  \t").expect("write blank override");
        assert!(parse_user_model_overrides_at(&path).is_empty());
    }

    #[test]
    fn parse_user_model_overrides_at_returns_empty_for_malformed_json() {
        // A malformed override file must not break startup — it should log
        // and return an empty map. (issue #60: "no surprises" requirement.)
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("models-override.json");
        fs::write(&path, "{ this is not json }").expect("write bad json");
        assert!(parse_user_model_overrides_at(&path).is_empty());
    }

    #[test]
    fn parse_user_model_overrides_at_loads_well_formed_overrides() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("models-override.json");
        fs::write(
            &path,
            r#"{"anthropic": ["claude-opus-4-7"], "openrouter": ["anthropic/claude-opus-4-7"]}"#,
        )
        .expect("write override");

        let overrides = parse_user_model_overrides_at(&path);
        assert_eq!(
            overrides.get("anthropic").map(Vec::as_slice),
            Some(&["claude-opus-4-7".to_string()][..])
        );
        assert_eq!(
            overrides.get("openrouter").map(Vec::as_slice),
            Some(&["anthropic/claude-opus-4-7".to_string()][..])
        );
    }

    #[test]
    fn merge_provider_model_ids_unions_entries_per_provider() {
        // Set-union semantics from issue #60: if a model appears in both the
        // bundled snapshot and the user override, dedup keeps it once.
        let mut target: HashMap<String, Vec<String>> = HashMap::new();
        let mut snapshot = HashMap::new();
        snapshot.insert(
            "anthropic".to_string(),
            vec![
                "claude-opus-4-6".to_string(),
                "claude-haiku-4-5".to_string(),
            ],
        );
        merge_provider_model_ids(&mut target, snapshot);

        let mut user = HashMap::new();
        user.insert(
            "anthropic".to_string(),
            vec!["claude-opus-4-6".to_string(), "claude-opus-4-7".to_string()],
        );
        merge_provider_model_ids(&mut target, user);

        let mut anthropic = target.remove("anthropic").expect("anthropic key");
        anthropic.sort_unstable();
        anthropic.dedup();
        assert_eq!(
            anthropic,
            vec![
                "claude-haiku-4-5".to_string(),
                "claude-opus-4-6".to_string(),
                "claude-opus-4-7".to_string(),
            ]
        );
    }

    #[test]
    fn merge_provider_model_ids_skips_blank_entries() {
        let mut target: HashMap<String, Vec<String>> = HashMap::new();
        let mut user = HashMap::new();
        user.insert(
            " ".to_string(), // blank provider
            vec!["foo".to_string()],
        );
        user.insert(
            "anthropic".to_string(),
            vec![
                String::new(),
                " ".to_string(),
                "claude-opus-4-7".to_string(),
            ],
        );
        merge_provider_model_ids(&mut target, user);

        assert_eq!(
            target.get("anthropic").map_or(&[][..], Vec::as_slice),
            &["claude-opus-4-7".to_string()]
        );
        assert!(!target.contains_key(" "));
    }

    #[test]
    fn user_model_overrides_fingerprint_at_changes_with_content() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("models-override.json");

        // Missing file => 0
        assert_eq!(user_model_overrides_fingerprint_at(&path), 0);

        fs::write(&path, r#"{"anthropic":["a"]}"#).expect("write v1");
        let fp_v1 = user_model_overrides_fingerprint_at(&path);
        assert_ne!(fp_v1, 0, "non-empty file should not hash to 0");

        fs::write(&path, r#"{"anthropic":["b"]}"#).expect("write v2");
        let fp_v2 = user_model_overrides_fingerprint_at(&path);
        assert_ne!(fp_v1, fp_v2, "fingerprint must change when content changes");
    }

    mod max_thinking_level {
        use super::*;
        use crate::model::ThinkingLevel;

        fn entry_with(id: &str, provider: &str, api: &str, reasoning: bool) -> ModelEntry {
            ModelEntry {
                model: Model {
                    id: id.to_string(),
                    name: id.to_string(),
                    provider: provider.to_string(),
                    api: api.to_string(),
                    base_url: String::new(),
                    reasoning,
                    input: vec![InputType::Text],
                    context_window: 128_000,
                    max_tokens: 4096,
                    cost: ModelCost {
                        input: 0.0,
                        output: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                    },
                    headers: HashMap::new(),
                },
                api_key: None,
                headers: HashMap::new(),
                auth_header: false,
                compat: None,
                oauth_config: None,
            }
        }

        #[test]
        fn anthropic_xhigh_families_also_support_max() {
            for id in [
                "claude-opus-4-7",
                "claude-opus-4-8",
                "claude-opus-5",
                "claude-sonnet-5",
                "claude-fable-5",
                "claude-mythos-5",
            ] {
                let entry = entry_with(id, "anthropic", "anthropic-messages", true);
                assert!(entry.supports_xhigh(), "{id} should support xhigh");
                assert!(entry.supports_max(), "{id} should support max");
                assert_eq!(
                    entry.clamp_thinking_level(ThinkingLevel::Max),
                    ThinkingLevel::Max
                );
            }
        }

        #[test]
        fn anthropic_4_6_family_supports_max_without_xhigh() {
            for id in ["claude-opus-4-6", "claude-sonnet-4-6"] {
                let entry = entry_with(id, "anthropic", "anthropic-messages", true);
                assert!(!entry.supports_xhigh(), "{id} has no xhigh tier");
                assert!(entry.supports_max(), "{id} should support max");
                assert_eq!(
                    entry.clamp_thinking_level(ThinkingLevel::Max),
                    ThinkingLevel::Max
                );
                assert_eq!(
                    entry.clamp_thinking_level(ThinkingLevel::XHigh),
                    ThinkingLevel::High
                );
            }
        }

        #[test]
        fn deepseek_reasoning_supports_max() {
            let entry = entry_with("deepseek-reasoner", "deepseek", "openai-completions", true);
            assert!(entry.supports_max());
            assert_eq!(
                entry.clamp_thinking_level(ThinkingLevel::Max),
                ThinkingLevel::Max
            );
        }

        #[test]
        fn xhigh_only_models_clamp_max_to_xhigh() {
            let entry = entry_with("gpt-5.2", "openai", "openai-completions", true);
            assert!(entry.supports_xhigh());
            assert!(!entry.supports_max());
            assert_eq!(
                entry.clamp_thinking_level(ThinkingLevel::Max),
                ThinkingLevel::XHigh
            );
        }

        #[test]
        fn plain_models_clamp_max_to_high() {
            let entry = entry_with("gpt-4o", "openai", "openai-completions", true);
            assert!(!entry.supports_max());
            assert_eq!(
                entry.clamp_thinking_level(ThinkingLevel::Max),
                ThinkingLevel::High
            );
        }

        #[test]
        fn available_levels_include_max_when_supported() {
            let entry = entry_with("claude-opus-4-7", "anthropic", "anthropic-messages", true);
            let levels = entry.available_thinking_levels();
            assert!(levels.contains(&ThinkingLevel::XHigh));
            assert!(levels.contains(&ThinkingLevel::Max));

            let entry46 = entry_with("claude-opus-4-6", "anthropic", "anthropic-messages", true);
            let levels46 = entry46.available_thinking_levels();
            assert!(!levels46.contains(&ThinkingLevel::XHigh));
            assert!(levels46.contains(&ThinkingLevel::Max));
        }
    }

    mod proptest_models {
        use super::*;
        use proptest::prelude::*;

        fn dummy_model(id: &str, reasoning: bool) -> ModelEntry {
            ModelEntry {
                model: Model {
                    id: id.to_string(),
                    name: id.to_string(),
                    provider: "test".to_string(),
                    api: "messages".to_string(),
                    base_url: String::new(),
                    reasoning,
                    input: vec![InputType::Text],
                    context_window: 128_000,
                    max_tokens: 4096,
                    cost: ModelCost {
                        input: 0.0,
                        output: 0.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                    },
                    headers: HashMap::new(),
                },
                api_key: None,
                headers: HashMap::new(),
                auth_header: false,
                compat: None,
                oauth_config: None,
            }
        }

        proptest! {
            /// Non-reasoning models always clamp to `Off`.
            #[test]
            fn clamp_thinking_non_reasoning(level_idx in 0..7usize) {
                use crate::model::ThinkingLevel;
                let levels = [
                    ThinkingLevel::Off,
                    ThinkingLevel::Minimal,
                    ThinkingLevel::Low,
                    ThinkingLevel::Medium,
                    ThinkingLevel::High,
                    ThinkingLevel::XHigh,
                    ThinkingLevel::Max,
                ];
                let entry = dummy_model("non-reasoning-model", false);
                assert_eq!(entry.clamp_thinking_level(levels[level_idx]), ThinkingLevel::Off);
            }

            /// Reasoning models without xhigh downgrade `XHigh` to `High`.
            #[test]
            fn clamp_thinking_reasoning_no_xhigh(level_idx in 0..7usize) {
                use crate::model::ThinkingLevel;
                let levels = [
                    ThinkingLevel::Off,
                    ThinkingLevel::Minimal,
                    ThinkingLevel::Low,
                    ThinkingLevel::Medium,
                    ThinkingLevel::High,
                    ThinkingLevel::XHigh,
                    ThinkingLevel::Max,
                ];
                let entry = dummy_model("claude-sonnet-4-5", true);
                let result = entry.clamp_thinking_level(levels[level_idx]);
                if levels[level_idx] == ThinkingLevel::XHigh
                    || levels[level_idx] == ThinkingLevel::Max
                {
                    assert_eq!(result, ThinkingLevel::High);
                } else {
                    assert_eq!(result, levels[level_idx]);
                }
            }

            /// `supports_xhigh` only returns true for specific model IDs.
            #[test]
            fn supports_xhigh_specific_ids(id in "[a-z\\-0-9]{5,20}") {
                let entry = dummy_model(&id, true);
                let expected = matches!(
                    id.as_str(),
                    "gpt-5.1-codex-max"
                        | "gpt-5.2"
                        | "gpt-5.4"
                        | "gpt-5.2-codex"
                        | "gpt-5.3-codex"
                        | "gpt-5.3-codex-spark"
                );
                assert_eq!(entry.supports_xhigh(), expected);
            }

            /// `canonicalize_openrouter_model_id` maps known aliases.
            #[test]
            fn openrouter_known_aliases(idx in 0..5usize) {
                let pairs = [
                    ("auto", "openrouter/auto"),
                    ("gpt-4o-mini", "openai/gpt-4o-mini"),
                    ("gpt-4o", "openai/gpt-4o"),
                    ("claude-3.5-sonnet", "anthropic/claude-3.5-sonnet"),
                    ("gemini-2.5-pro", "google/gemini-2.5-pro"),
                ];
                let (input, expected) = pairs[idx];
                assert_eq!(canonicalize_openrouter_model_id(input), expected);
            }

            /// `canonicalize_openrouter_model_id` is case-insensitive for aliases.
            #[test]
            fn openrouter_case_insensitive(idx in 0..5usize) {
                let aliases = ["auto", "gpt-4o-mini", "gpt-4o", "claude-3.5-sonnet", "gemini-2.5-pro"];
                let lower = canonicalize_openrouter_model_id(aliases[idx]);
                let upper = canonicalize_openrouter_model_id(&aliases[idx].to_uppercase());
                assert_eq!(lower, upper);
            }

            /// `canonicalize_openrouter_model_id` passes unknown IDs through.
            #[test]
            fn openrouter_passthrough(id in "[a-z]/[a-z]{5,15}") {
                let result = canonicalize_openrouter_model_id(&id);
                assert_eq!(result, id);
            }

            /// `openrouter_model_lookup_ids` always includes the canonical form.
            #[test]
            fn openrouter_lookup_includes_canonical(id in "[a-z\\-0-9]{1,20}") {
                let ids = openrouter_model_lookup_ids(&id);
                let canonical = canonicalize_openrouter_model_id(&id);
                assert!(ids.contains(&canonical));
            }

            /// `merge_headers` override wins for duplicate keys.
            #[test]
            fn merge_headers_override_wins(key in "[a-z]{1,5}", v1 in "[a-z]{1,5}", v2 in "[a-z]{1,5}") {
                let base = HashMap::from([(key.clone(), v1)]);
                let over = HashMap::from([(key.clone(), v2.clone())]);
                let merged = merge_headers(&base, over);
                assert_eq!(merged.get(&key).unwrap(), &v2);
            }

            /// `merge_headers` preserves non-overlapping keys.
            #[test]
            fn merge_headers_preserves_both(k1 in "[a-z]{1,5}", k2 in "[A-Z]{1,5}", v1 in "[a-z]{1,5}", v2 in "[a-z]{1,5}") {
                let base = HashMap::from([(k1.clone(), v1.clone())]);
                let over = HashMap::from([(k2.clone(), v2.clone())]);
                let merged = merge_headers(&base, over);
                assert_eq!(merged.get(&k1), Some(&v1));
                assert_eq!(merged.get(&k2), Some(&v2));
            }

            /// `sap_chat_completions_endpoint` rejects empty inputs.
            #[test]
            fn sap_endpoint_rejects_empty(s in "[a-z]{0,10}") {
                assert_eq!(sap_chat_completions_endpoint("", &s), None);
                assert_eq!(sap_chat_completions_endpoint(&s, ""), None);
                assert_eq!(sap_chat_completions_endpoint("  ", &s), None);
            }

            /// `sap_chat_completions_endpoint` formats correctly.
            #[test]
            fn sap_endpoint_format(base in "[a-z]{3,10}", deployment in "[a-z]{3,10}") {
                let url = format!("https://{base}.example.com");
                let result = sap_chat_completions_endpoint(&url, &deployment);
                assert!(result.is_some());
                let endpoint = result.unwrap();
                assert!(endpoint.contains(&deployment));
                assert!(endpoint.contains("/v2/inference/deployments/"));
                assert!(endpoint.ends_with("/chat/completions"));
            }

            /// `sap_chat_completions_endpoint` strips trailing slashes.
            #[test]
            fn sap_endpoint_strips_trailing_slash(base in "[a-z]{5,10}") {
                let url_no_slash = format!("https://{base}");
                let url_slash = format!("https://{base}/");
                let r1 = sap_chat_completions_endpoint(&url_no_slash, "model");
                let r2 = sap_chat_completions_endpoint(&url_slash, "model");
                assert_eq!(r1, r2);
            }
        }
    }
}
