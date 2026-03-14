//! Model registry: built-in + models.json overrides.

use crate::auth::{AuthStorage, SapResolvedCredentials, resolve_sap_credentials};
use crate::error::Error;
use crate::provider::{Api, InputType, Model, ModelCost};
use crate::provider_metadata::{
    ProviderRoutingDefaults, canonical_provider_id, provider_routing_defaults,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
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
                | "gpt-5.4"
                | "gpt-5.2-codex"
                | "gpt-5.3-codex"
                | "gpt-5.3-codex-spark"
        )
    }

    /// Clamp a requested thinking level to the model's capabilities.
    ///
    /// Non-reasoning models always return `Off`. Models without xhigh support
    /// downgrade `XHigh` to `High`. All other levels pass through unchanged.
    pub fn clamp_thinking_level(
        &self,
        thinking: crate::model::ThinkingLevel,
    ) -> crate::model::ThinkingLevel {
        if !self.model.reasoning {
            return crate::model::ThinkingLevel::Off;
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
    pub providers: HashMap<String, ProviderConfig>,
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
    #[serde(default)]
    reasoning: bool,
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

const LEGACY_MODELS_GENERATED_TS: &str =
    include_str!("../legacy_pi_mono_code/pi-mono/packages/ai/src/models.generated.ts");
const UPSTREAM_PROVIDER_MODEL_IDS_JSON: &str =
    include_str!("../docs/provider-upstream-model-ids-snapshot.json");
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

fn canonicalize_model_id_for_provider(provider: &str, model_id: &str) -> String {
    if canonical_provider_id(provider).is_some_and(|canonical| canonical == "openrouter") {
        return canonicalize_openrouter_model_id(model_id);
    }
    model_id.trim().to_string()
}

fn normalized_registry_key(provider: &str, model_id: &str) -> (String, String) {
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
    let checksum = crc32c::crc32c(LEGACY_MODELS_GENERATED_TS.as_bytes());
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

    let Some(models_decl_start) = LEGACY_MODELS_GENERATED_TS.find("export const MODELS =") else {
        tracing::warn!("Legacy model catalog missing MODELS declaration");
        return Vec::new();
    };
    let Some(object_start_rel) = LEGACY_MODELS_GENERATED_TS[models_decl_start..].find('{') else {
        tracing::warn!("Legacy model catalog missing object start after MODELS declaration");
        return Vec::new();
    };
    let object_start = models_decl_start + object_start_rel;
    let Some(end_marker_rel) = LEGACY_MODELS_GENERATED_TS[object_start..].rfind("} as const;")
    else {
        tracing::warn!("Legacy model catalog missing end marker");
        return Vec::new();
    };
    let end_marker = object_start + end_marker_rel;

    let mut object_source = LEGACY_MODELS_GENERATED_TS[object_start..=end_marker]
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
        match serde_json::from_str(UPSTREAM_PROVIDER_MODEL_IDS_JSON) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(error = %err, "Failed to parse upstream provider model snapshot");
                return HashMap::new();
            }
        };

    let mut by_provider: HashMap<String, Vec<String>> = HashMap::new();
    for (provider, ids) in parsed {
        let provider = provider.trim();
        if provider.is_empty() {
            continue;
        }
        let canonical_provider = canonical_provider_id(provider)
            .unwrap_or(provider)
            .to_string();
        let entry = by_provider.entry(canonical_provider.clone()).or_default();
        for model_id in ids {
            let normalized = canonicalize_model_id_for_provider(&canonical_provider, &model_id);
            if !normalized.is_empty() {
                entry.push(normalized);
            }
        }
    }

    for ids in by_provider.values_mut() {
        ids.sort_unstable();
        ids.dedup();
    }
    by_provider
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
                slug: "openai/gpt-5.4".to_string(),
                description: Some("GPT-5.4".to_string()),
            });
            candidates.push(ModelAutocompleteCandidate {
                slug: "openai-codex/gpt-5.4".to_string(),
                description: Some("GPT-5.4 Codex".to_string()),
            });
            candidates.sort_by_key(|candidate| candidate.slug.to_ascii_lowercase());
            candidates.dedup_by(|a, b| a.slug.eq_ignore_ascii_case(&b.slug));
            candidates
        })
        .as_slice()
}

pub fn model_catalog_cache_fingerprint() -> u64 {
    *MODEL_CATALOG_CACHE_FINGERPRINT.get_or_init(|| {
        let legacy = u64::from(crc32c::crc32c(LEGACY_MODELS_GENERATED_TS.as_bytes()));
        let upstream = u64::from(crc32c::crc32c(UPSTREAM_PROVIDER_MODEL_IDS_JSON.as_bytes()));
        (legacy << 32) | upstream
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

impl ModelRegistry {
    pub fn load(auth: &AuthStorage, models_path: Option<PathBuf>) -> Self {
        Self::load_with_mode(auth, models_path, ModelRegistryLoadMode::Full)
    }

    pub fn load_for_listing(auth: &AuthStorage, models_path: Option<PathBuf>) -> Self {
        Self::load_with_mode(auth, models_path, ModelRegistryLoadMode::ListingLite)
    }

    fn load_with_mode(
        auth: &AuthStorage,
        models_path: Option<PathBuf>,
        mode: ModelRegistryLoadMode,
    ) -> Self {
        let mut models = built_in_models(auth, mode);
        let mut error = None;

        if let Some(path) = models_path {
            if path.exists() {
                match std::fs::read_to_string(&path)
                    .map_err(|e| Error::config(format!("Failed to read models.json: {e}")))
                    .and_then(|s| serde_json::from_str::<ModelsConfig>(&s).map_err(Error::from))
                {
                    Ok(config) => {
                        apply_custom_models(auth, &mut models, &config, path.parent());
                    }
                    Err(e) => {
                        error = Some(format!("{e}\n\nFile: {}", path.display()));
                    }
                }
            }
        }

        Self { models, error }
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
    let id = model_id.to_ascii_lowercase();

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

    // DeepSeek: deepseek-reasoner (R1) is reasoning; deepseek-chat (V3) and others are not.
    if id.starts_with("deepseek-reasoner") || id.starts_with("deepseek-r") {
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
    auth: &AuthStorage,
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

fn append_upstream_nonlegacy_models(
    auth: &AuthStorage,
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
            continue;
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
fn built_in_models(auth: &AuthStorage, mode: ModelRegistryLoadMode) -> Vec<ModelEntry> {
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
                reasoning: effective_reasoning(&normalized_model_id, legacy.reasoning),
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

    // Ensure the latest GPT-5 default exists for OpenAI routing.
    //
    // The legacy catalog can lag behind upstream model IDs; we add a
    // conservative seed so listing, lookup, and autocomplete stay current.
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

#[allow(clippy::too_many_lines)]
fn apply_custom_models(
    auth: &AuthStorage,
    models: &mut Vec<ModelEntry>,
    config: &ModelsConfig,
    base_dir: Option<&Path>,
) {
    for (provider_id, provider_cfg) in &config.providers {
        let provider_id_str = provider_id.as_str();
        let routing_defaults = provider_routing_defaults(provider_id);
        let default_api = routing_defaults.map_or("openai-completions", |defaults| defaults.api);
        let provider_api = provider_cfg.api.as_deref().unwrap_or(default_api);
        let provider_api_parsed: Api = provider_api
            .parse()
            .unwrap_or_else(|_| Api::Custom(provider_api.to_string()));
        let provider_api_string = provider_api_parsed.to_string();
        let provider_base = provider_cfg.base_url.clone().unwrap_or_else(|| {
            routing_defaults.map_or_else(
                || "https://api.openai.com/v1".to_string(),
                |defaults| defaults.base_url.to_string(),
            )
        });

        let provider_headers = resolve_headers_with_base(provider_cfg.headers.as_ref(), base_dir);
        let canonical_provider = canonical_provider_id(provider_id).unwrap_or(provider_id_str);
        let provider_matches = |candidate_provider: &str| {
            let candidate_canonical =
                canonical_provider_id(candidate_provider).unwrap_or(candidate_provider);
            candidate_provider.eq_ignore_ascii_case(provider_id_str)
                || candidate_provider.eq_ignore_ascii_case(canonical_provider)
                || candidate_canonical.eq_ignore_ascii_case(provider_id_str)
                || candidate_canonical.eq_ignore_ascii_case(canonical_provider)
        };
        let provider_key = provider_cfg
            .api_key
            .as_deref()
            .and_then(|value| resolve_value_with_base(value, base_dir))
            .or_else(|| auth.resolve_api_key(canonical_provider, None));

        let auth_header = provider_cfg
            .auth_header
            .unwrap_or_else(|| routing_defaults.is_some_and(|defaults| defaults.auth_header));

        if routing_defaults.is_some() {
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

            let model_api = model_cfg.api.as_deref().unwrap_or(provider_api);
            let model_api_parsed: Api = model_api
                .parse()
                .unwrap_or_else(|_| Api::Custom(model_api.to_string()));
            let model_headers = merge_headers(
                &provider_headers,
                resolve_headers_with_base(model_cfg.headers.as_ref(), base_dir),
            );
            let default_input_types = routing_defaults
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
            let default_reasoning = routing_defaults.is_some_and(|defaults| defaults.reasoning);
            let default_context_window =
                routing_defaults.map_or(128_000, |defaults| defaults.context_window);
            let default_max_tokens =
                routing_defaults.map_or(16_384, |defaults| defaults.max_tokens);

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

fn resolve_value(value: &str) -> Option<String> {
    resolve_value_with_base(value, None)
}

fn resolve_value_with_base(value: &str, base_dir: Option<&Path>) -> Option<String> {
    if let Some(rest) = value.strip_prefix('!') {
        return resolve_shell(rest);
    }

    if let Some(var_name) = value.strip_prefix("env:") {
        if var_name.is_empty() {
            return None;
        }
        return std::env::var(var_name).ok().filter(|v| !v.is_empty());
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

    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
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
    ad_hoc_model_entry_with_sap_resolver(provider, model_id, || {
        let auth = AuthStorage::load(crate::config::Config::auth_path()).ok()?;
        resolve_sap_credentials(&auth)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{ApiKeyValue, AuthCredential, AuthStorage};
    use tempfile::tempdir;

    fn test_auth_storage() -> (tempfile::TempDir, AuthStorage) {
        let dir = tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load auth");
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("anthropic-auth-key".to_string()),
            },
        );
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("openai-auth-key".to_string()),
            },
        );
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("google-auth-key".to_string()),
            },
        );
        auth.set(
            "openrouter",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("openrouter-auth-key".to_string()),
            },
        );
        auth.set(
            "acme",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("acme-auth-key".to_string()),
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

    #[test]
    fn parse_legacy_generated_models_extracts_known_legacy_only_providers() {
        let parsed = parse_legacy_generated_models();
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
    fn apply_custom_models_overrides_provider_fields() {
        let (_dir, auth) = test_auth_storage();
        let mut models = built_in_models(&auth, ModelRegistryLoadMode::Full);
        let (env_key, env_val) = expected_env_pair();
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
            assert_eq!(entry.api_key.as_deref(), Some(env_val.as_str()));
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
    fn apply_custom_models_alias_resolves_canonical_provider_api_key() {
        let (_dir, mut auth) = test_auth_storage();
        auth.set(
            "moonshotai",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("moonshot-auth-key".to_string()),
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

    #[test]
    fn model_registry_load_reads_models_json_and_applies_config() {
        let (dir, auth) = test_auth_storage();
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

        let registry = ModelRegistry::load(&auth, Some(models_path));
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
        assert_eq!(canonical_provider_id("dashscope"), Some("alibaba"));
        assert_eq!(canonical_provider_id("alibaba-cn"), Some("alibaba-cn"));
        assert_eq!(alibaba.api, "openai-completions");
        assert_eq!(alibaba_cn.api, "openai-completions");
        assert_ne!(alibaba.base_url, alibaba_cn.base_url);
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
        let entry = ad_hoc_model_entry("groq", "llama-3-70b").unwrap();
        assert_eq!(entry.model.id, "llama-3-70b");
        assert_eq!(entry.model.name, "llama-3-70b");
        assert_eq!(entry.model.provider, "groq");
        assert_eq!(entry.model.api, "openai-completions");
        assert!(entry.model.base_url.contains("groq.com"));
        assert!(entry.auth_header); // openai-compatible → auth_header = true
        assert!(entry.api_key.is_none()); // no auth lookup
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
            fn clamp_thinking_non_reasoning(level_idx in 0..6usize) {
                use crate::model::ThinkingLevel;
                let levels = [
                    ThinkingLevel::Off,
                    ThinkingLevel::Minimal,
                    ThinkingLevel::Low,
                    ThinkingLevel::Medium,
                    ThinkingLevel::High,
                    ThinkingLevel::XHigh,
                ];
                let entry = dummy_model("non-reasoning-model", false);
                assert_eq!(entry.clamp_thinking_level(levels[level_idx]), ThinkingLevel::Off);
            }

            /// Reasoning models without xhigh downgrade `XHigh` to `High`.
            #[test]
            fn clamp_thinking_reasoning_no_xhigh(level_idx in 0..6usize) {
                use crate::model::ThinkingLevel;
                let levels = [
                    ThinkingLevel::Off,
                    ThinkingLevel::Minimal,
                    ThinkingLevel::Low,
                    ThinkingLevel::Medium,
                    ThinkingLevel::High,
                    ThinkingLevel::XHigh,
                ];
                let entry = dummy_model("claude-sonnet-4-5", true);
                let result = entry.clamp_thinking_level(levels[level_idx]);
                if levels[level_idx] == ThinkingLevel::XHigh {
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
