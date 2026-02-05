//! Model registry: built-in + models.json overrides.

use crate::auth::AuthStorage;
use crate::error::Error;
use crate::provider::{Api, InputType, Model, ModelCost};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub model: Model,
    pub api_key: Option<String>,
    pub headers: HashMap<String, String>,
    pub auth_header: bool,
    pub compat: Option<CompatConfig>,
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatConfig {
    pub supports_store: Option<bool>,
    pub supports_developer_role: Option<bool>,
    pub supports_reasoning_effort: Option<bool>,
    pub supports_usage_in_streaming: Option<bool>,
    pub max_tokens_field: Option<String>,
    pub open_router_routing: Option<serde_json::Value>,
    pub vercel_gateway_routing: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct ModelRegistry {
    models: Vec<ModelEntry>,
    error: Option<String>,
}

impl ModelRegistry {
    pub fn load(auth: &AuthStorage, models_path: Option<PathBuf>) -> Self {
        let mut models = built_in_models(auth);
        let mut error = None;

        if let Some(path) = models_path {
            if path.exists() {
                match std::fs::read_to_string(&path)
                    .map_err(|e| Error::config(format!("Failed to read models.json: {e}")))
                    .and_then(|s| serde_json::from_str::<ModelsConfig>(&s).map_err(Error::from))
                {
                    Ok(config) => {
                        apply_custom_models(auth, &mut models, &config);
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

    pub fn get_available(&self) -> Vec<ModelEntry> {
        self.models
            .iter()
            .filter(|&m| m.api_key.is_some())
            .cloned()
            .collect()
    }

    pub fn find(&self, provider: &str, id: &str) -> Option<ModelEntry> {
        self.models
            .iter()
            .find(|m| m.model.provider == provider && m.model.id == id)
            .cloned()
    }

    /// Find a model by ID alone (ignoring provider), useful for extension models
    /// where the provider name may be custom.
    pub fn find_by_id(&self, id: &str) -> Option<ModelEntry> {
        self.models
            .iter()
            .find(|m| m.model.id == id)
            .cloned()
    }

    /// Merge extension-provided model entries into the registry.
    pub fn merge_entries(&mut self, entries: Vec<ModelEntry>) {
        for entry in entries {
            // Skip duplicates (same provider + id).
            let exists = self
                .models
                .iter()
                .any(|m| m.model.provider == entry.model.provider && m.model.id == entry.model.id);
            if !exists {
                self.models.push(entry);
            }
        }
    }
}

fn built_in_models(auth: &AuthStorage) -> Vec<ModelEntry> {
    let mut models = Vec::new();

    let anthropic_key = auth.resolve_api_key("anthropic", None);
    for (id, name, reasoning) in [
        ("claude-sonnet-4-5", "Claude Sonnet 4.5", true),
        ("claude-opus-4-5", "Claude Opus 4.5", true),
        ("claude-haiku-4-5", "Claude Haiku 4.5", false),
        ("claude-3-5-sonnet-20241022", "Claude Sonnet 3.5", true),
        ("claude-3-5-haiku-20241022", "Claude Haiku 3.5", false),
        ("claude-3-opus-20240229", "Claude Opus 3", true),
    ] {
        models.push(ModelEntry {
            model: Model {
                id: id.to_string(),
                name: name.to_string(),
                api: Api::AnthropicMessages.to_string(),
                provider: "anthropic".to_string(),
                base_url: "https://api.anthropic.com/v1/messages".to_string(),
                reasoning,
                input: vec![InputType::Text, InputType::Image],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 200_000,
                max_tokens: 8192,
                headers: HashMap::new(),
            },
            api_key: anthropic_key.clone(),
            headers: HashMap::new(),
            auth_header: false,
            compat: None,
        });
    }

    let openai_key = auth.resolve_api_key("openai", None);
    for (id, name) in [
        ("gpt-5.1-codex", "GPT-5.1 Codex"),
        ("gpt-4o", "GPT-4o"),
        ("gpt-4o-mini", "GPT-4o Mini"),
    ] {
        models.push(ModelEntry {
            model: Model {
                id: id.to_string(),
                name: name.to_string(),
                api: Api::OpenAIResponses.to_string(),
                provider: "openai".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                reasoning: true,
                input: vec![InputType::Text, InputType::Image],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 16384,
                headers: HashMap::new(),
            },
            api_key: openai_key.clone(),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
        });
    }

    let google_key = auth.resolve_api_key("google", None);
    for (id, name) in [
        ("gemini-2.5-pro", "Gemini 2.5 Pro"),
        ("gemini-2.5-flash", "Gemini 2.5 Flash"),
        ("gemini-1.5-pro", "Gemini 1.5 Pro"),
        ("gemini-1.5-flash", "Gemini 1.5 Flash"),
    ] {
        models.push(ModelEntry {
            model: Model {
                id: id.to_string(),
                name: name.to_string(),
                api: Api::GoogleGenerativeAI.to_string(),
                provider: "google".to_string(),
                base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
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
            api_key: google_key.clone(),
            headers: HashMap::new(),
            auth_header: false,
            compat: None,
        });
    }

    models
}

fn apply_custom_models(auth: &AuthStorage, models: &mut Vec<ModelEntry>, config: &ModelsConfig) {
    for (provider_id, provider_cfg) in &config.providers {
        let provider_api = provider_cfg.api.as_deref().unwrap_or("openai-completions");
        let provider_api_parsed: Api = provider_api
            .parse()
            .unwrap_or_else(|_| Api::Custom(provider_api.to_string()));
        let provider_api_string = provider_api_parsed.to_string();
        let provider_base = provider_cfg
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

        let provider_headers = resolve_headers(provider_cfg.headers.as_ref());
        let provider_key = provider_cfg
            .api_key
            .as_deref()
            .and_then(resolve_value)
            .or_else(|| auth.resolve_api_key(provider_id, None));

        let auth_header = provider_cfg.auth_header.unwrap_or(false);

        let has_models = provider_cfg.models.as_ref().is_some();
        let is_override = !has_models;

        if is_override {
            for entry in models
                .iter_mut()
                .filter(|m| m.model.provider == *provider_id)
            {
                entry.model.base_url.clone_from(&provider_base);
                entry.model.api.clone_from(&provider_api_string);
                entry.headers.clone_from(&provider_headers);
                if provider_key.is_some() {
                    entry.api_key.clone_from(&provider_key);
                }
                if provider_cfg.compat.is_some() {
                    entry.compat.clone_from(&provider_cfg.compat);
                }
                entry.auth_header = auth_header;
            }
            continue;
        }

        // Remove built-in provider models if fully overridden
        models.retain(|m| m.model.provider != *provider_id);

        for model_cfg in provider_cfg.models.clone().unwrap_or_default() {
            let model_api = model_cfg.api.as_deref().unwrap_or(provider_api);
            let model_api_parsed: Api = model_api
                .parse()
                .unwrap_or_else(|_| Api::Custom(model_api.to_string()));
            let model_headers = merge_headers(
                &provider_headers,
                resolve_headers(model_cfg.headers.as_ref()),
            );
            let input = model_cfg
                .input
                .clone()
                .unwrap_or_else(|| vec!["text".to_string()]);

            let input_types = input
                .iter()
                .filter_map(|i| match i.as_str() {
                    "text" => Some(InputType::Text),
                    "image" => Some(InputType::Image),
                    _ => None,
                })
                .collect::<Vec<_>>();

            let model = Model {
                id: model_cfg.id.clone(),
                name: model_cfg
                    .name
                    .clone()
                    .unwrap_or_else(|| model_cfg.id.clone()),
                api: model_api_parsed.to_string(),
                provider: provider_id.clone(),
                base_url: provider_base.clone(),
                reasoning: model_cfg.reasoning.unwrap_or(false),
                input: if input_types.is_empty() {
                    vec![InputType::Text]
                } else {
                    input_types
                },
                cost: model_cfg.cost.clone().unwrap_or(ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                }),
                context_window: model_cfg.context_window.unwrap_or(128_000),
                max_tokens: model_cfg.max_tokens.unwrap_or(16_384),
                headers: HashMap::new(),
            };

            models.push(ModelEntry {
                model,
                api_key: provider_key.clone(),
                headers: model_headers,
                auth_header,
                compat: model_cfg
                    .compat
                    .clone()
                    .or_else(|| provider_cfg.compat.clone()),
            });
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

fn resolve_headers(headers: Option<&HashMap<String, String>>) -> HashMap<String, String> {
    let mut resolved = HashMap::new();
    if let Some(headers) = headers {
        for (k, v) in headers {
            if let Some(val) = resolve_value(v) {
                resolved.insert(k.clone(), val);
            }
        }
    }
    resolved
}

fn resolve_value(value: &str) -> Option<String> {
    if let Some(rest) = value.strip_prefix('!') {
        return resolve_shell(rest);
    }

    if let Ok(env_val) = std::env::var(value) {
        if !env_val.is_empty() {
            return Some(env_val);
        }
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
            .output()
            .ok()?
    } else {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
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
