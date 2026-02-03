//! LLM provider abstraction layer.
//!
//! This module defines the provider trait and common types for interacting
//! with different LLM APIs.

use crate::model::{Message, StreamEvent, ThinkingLevel};
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::pin::Pin;

// ============================================================================
// Provider Trait
// ============================================================================

/// A provider for LLM completions.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Get the provider name.
    fn name(&self) -> &str;

    /// Get the API type.
    fn api(&self) -> &str;

    /// Get the model identifier used by this provider.
    fn model_id(&self) -> &str;

    /// Stream a completion.
    async fn stream(
        &self,
        context: &Context,
        options: &StreamOptions,
    ) -> crate::error::Result<Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>>;
}

// ============================================================================
// Context
// ============================================================================

/// Context for a completion request.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// A tool definition for the API.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
}

// ============================================================================
// Stream Options
// ============================================================================

/// Options for streaming completion.
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub api_key: Option<String>,
    pub cache_retention: CacheRetention,
    pub session_id: Option<String>,
    pub headers: HashMap<String, String>,
    pub thinking_level: Option<ThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
}

/// Cache retention policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CacheRetention {
    #[default]
    None,
    Short,
    Long, // 1 hour TTL on Anthropic
}

/// Custom thinking token budgets per level.
#[derive(Debug, Clone)]
pub struct ThinkingBudgets {
    pub minimal: u32,
    pub low: u32,
    pub medium: u32,
    pub high: u32,
    pub xhigh: u32,
}

impl Default for ThinkingBudgets {
    fn default() -> Self {
        Self {
            minimal: 1024,
            low: 2048,
            medium: 8192,
            high: 16384,
            xhigh: 32768, // Default to double high, or model max? Let's pick a reasonable default.
        }
    }
}

// ============================================================================
// Model Definition
// ============================================================================

/// A model definition.
#[derive(Debug, Clone, Serialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub reasoning: bool,
    pub input: Vec<InputType>,
    pub cost: ModelCost,
    pub context_window: u32,
    pub max_tokens: u32,
    pub headers: HashMap<String, String>,
}

/// Input types supported by a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputType {
    Text,
    Image,
}

/// Model pricing per million tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Model {
    /// Calculate cost for usage.
    #[allow(clippy::cast_precision_loss)] // Token counts within practical range won't lose precision
    pub fn calculate_cost(
        &self,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> f64 {
        let input_cost = (self.cost.input / 1_000_000.0) * input as f64;
        let output_cost = (self.cost.output / 1_000_000.0) * output as f64;
        let cache_read_cost = (self.cost.cache_read / 1_000_000.0) * cache_read as f64;
        let cache_write_cost = (self.cost.cache_write / 1_000_000.0) * cache_write as f64;
        input_cost + output_cost + cache_read_cost + cache_write_cost
    }
}

// ============================================================================
// Known APIs and Providers
// ============================================================================

/// Known API types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Api {
    AnthropicMessages,
    OpenAICompletions,
    OpenAIResponses,
    AzureOpenAIResponses,
    BedrockConverseStream,
    GoogleGenerativeAI,
    GoogleGeminiCli,
    GoogleVertex,
    Custom(String),
}

impl std::fmt::Display for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AnthropicMessages => write!(f, "anthropic-messages"),
            Self::OpenAICompletions => write!(f, "openai-completions"),
            Self::OpenAIResponses => write!(f, "openai-responses"),
            Self::AzureOpenAIResponses => write!(f, "azure-openai-responses"),
            Self::BedrockConverseStream => write!(f, "bedrock-converse-stream"),
            Self::GoogleGenerativeAI => write!(f, "google-generative-ai"),
            Self::GoogleGeminiCli => write!(f, "google-gemini-cli"),
            Self::GoogleVertex => write!(f, "google-vertex"),
            Self::Custom(s) => write!(f, "{s}"),
        }
    }
}

impl std::str::FromStr for Api {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "anthropic-messages" => Ok(Self::AnthropicMessages),
            "openai-completions" => Ok(Self::OpenAICompletions),
            "openai-responses" => Ok(Self::OpenAIResponses),
            "azure-openai-responses" => Ok(Self::AzureOpenAIResponses),
            "bedrock-converse-stream" => Ok(Self::BedrockConverseStream),
            "google-generative-ai" => Ok(Self::GoogleGenerativeAI),
            "google-gemini-cli" => Ok(Self::GoogleGeminiCli),
            "google-vertex" => Ok(Self::GoogleVertex),
            other if !other.is_empty() => Ok(Self::Custom(other.to_string())),
            _ => Err("API identifier cannot be empty".to_string()),
        }
    }
}

/// Known providers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::upper_case_acronyms)] // These are proper names/brands
pub enum KnownProvider {
    Anthropic,
    OpenAI,
    Google,
    GoogleVertex,
    AmazonBedrock,
    AzureOpenAI,
    GithubCopilot,
    XAI,
    Groq,
    Cerebras,
    OpenRouter,
    Mistral,
    Custom(String),
}

impl std::fmt::Display for KnownProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Anthropic => write!(f, "anthropic"),
            Self::OpenAI => write!(f, "openai"),
            Self::Google => write!(f, "google"),
            Self::GoogleVertex => write!(f, "google-vertex"),
            Self::AmazonBedrock => write!(f, "amazon-bedrock"),
            Self::AzureOpenAI => write!(f, "azure-openai"),
            Self::GithubCopilot => write!(f, "github-copilot"),
            Self::XAI => write!(f, "xai"),
            Self::Groq => write!(f, "groq"),
            Self::Cerebras => write!(f, "cerebras"),
            Self::OpenRouter => write!(f, "openrouter"),
            Self::Mistral => write!(f, "mistral"),
            Self::Custom(s) => write!(f, "{s}"),
        }
    }
}
