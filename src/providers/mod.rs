//! Provider implementations.
//!
//! This module contains concrete implementations of the Provider trait
//! for various LLM APIs.

use crate::error::{Error, Result};
use crate::models::ModelEntry;
use crate::provider::Provider;
use std::sync::Arc;

pub mod anthropic;
pub mod azure;
pub mod gemini;
pub mod openai;

pub fn create_provider(entry: &ModelEntry) -> Result<Arc<dyn Provider>> {
    match entry.model.provider.as_str() {
        "anthropic" => Ok(Arc::new(
            anthropic::AnthropicProvider::new(entry.model.id.clone())
                .with_base_url(entry.model.base_url.clone()),
        )),
        "openai" => Ok(Arc::new(
            openai::OpenAIProvider::new(entry.model.id.clone())
                .with_base_url(normalize_openai_base(&entry.model.base_url)),
        )),
        "google" => Ok(Arc::new(
            gemini::GeminiProvider::new(entry.model.id.clone())
                .with_base_url(entry.model.base_url.clone()),
        )),
        "azure-openai" => Err(Error::provider(
            "azure-openai",
            "Azure OpenAI provider requires resource+deployment; configure via models.json",
        )),
        other => Err(Error::provider(other, "Provider not implemented")),
    }
}

fn normalize_openai_base(base_url: &str) -> String {
    if base_url.ends_with("/chat/completions") || base_url.ends_with("/responses") {
        return base_url.to_string();
    }
    if base_url.ends_with("/v1") {
        return format!("{base_url}/chat/completions");
    }
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}
