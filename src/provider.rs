//! LLM provider abstraction layer.
//!
//! This module defines the [`Provider`] trait and shared request/response types used by all
//! backends (Anthropic/OpenAI/Gemini/etc).
//!
//! Providers are responsible for:
//! - Translating [`crate::model::Message`] history into provider-specific HTTP requests.
//! - Emitting [`StreamEvent`] values as SSE/HTTP chunks arrive.
//! - Advertising tool schemas to the model (so it can call [`crate::tools`] by name).

pub use crate::model::StreamEvent;
use crate::model::{Message, ThinkingLevel};
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::pin::Pin;

// ============================================================================
// Provider Trait
// ============================================================================

/// An LLM backend capable of streaming assistant output (and tool calls).
///
/// A `Provider` is typically configured for a specific API + model and is used by the agent loop
/// to produce a stream of [`StreamEvent`] updates.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Get the provider name.
    fn name(&self) -> &str;

    /// Get the API type.
    fn api(&self) -> &str;

    /// Get the model identifier used by this provider.
    fn model_id(&self) -> &str;

    /// Start streaming a completion.
    ///
    /// Implementations should yield [`StreamEvent`] items as soon as they are decoded, and should
    /// stop promptly when the request is cancelled.
    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> crate::error::Result<Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>>;
}

// ============================================================================
// Context
// ============================================================================

/// Inputs to a single completion request.
///
/// The agent loop builds a `Context` from the current session state and tool registry, then hands
/// it to a [`Provider`] implementation to perform provider-specific request encoding.
///
/// Uses [`Cow`] for `messages` and `tools` to avoid deep-cloning the full conversation history on
/// every turn when no mutation is needed (the common case).
#[derive(Debug, Clone)]
pub struct Context<'a> {
    /// Provider-specific system prompt content.
    ///
    /// Uses [`Cow`] to borrow from `AgentConfig.system_prompt` on every turn without
    /// cloning.  Providers that need an owned `String` can call `.into_owned()`.
    pub system_prompt: Option<Cow<'a, str>>,
    /// Conversation history (user/assistant/tool results).
    pub messages: Cow<'a, [Message]>,
    /// Tool definitions available to the model for this request.
    pub tools: Cow<'a, [ToolDef]>,
}

impl Default for Context<'_> {
    fn default() -> Self {
        Self {
            system_prompt: None,
            messages: Cow::Owned(Vec::new()),
            tools: Cow::Owned(Vec::new()),
        }
    }
}

impl Context<'_> {
    /// Create a `Context` with fully-owned data (no borrowing).
    ///
    /// Convenient for tests and one-off callers that already have owned vectors.
    pub fn owned(
        system_prompt: Option<String>,
        messages: Vec<Message>,
        tools: Vec<ToolDef>,
    ) -> Context<'static> {
        Context {
            system_prompt: system_prompt.map(Cow::Owned),
            messages: Cow::Owned(messages),
            tools: Cow::Owned(tools),
        }
    }
}

// ============================================================================
// Tool Definition
// ============================================================================

/// A tool definition exposed to the model.
///
/// Providers translate this struct into the backend's tool/schema representation (typically JSON
/// Schema) so the model can emit tool calls that the host executes locally.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
}

// ============================================================================
// Stream Options
// ============================================================================

/// Options that control streaming completion behavior.
///
/// Most options are passed through to the provider request (temperature, max tokens, headers).
/// Some fields are Pi-specific conveniences (e.g. `session_id` for logging/correlation).
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
    /// Provider-managed short-lived caching (provider-specific semantics).
    Short,
    /// Provider-managed long-lived caching (e.g. ~1 hour TTL on Anthropic).
    Long,
}

/// Custom thinking token budgets per level.
#[derive(Debug, Clone)]
pub struct ThinkingBudgets {
    pub minimal: u32,
    pub low: u32,
    pub medium: u32,
    pub high: u32,
    pub xhigh: u32,
    pub max: u32,
}

impl Default for ThinkingBudgets {
    fn default() -> Self {
        Self {
            minimal: 1024,
            low: 2048,
            medium: 8192,
            high: 16384,
            xhigh: 32768, // Default to double high, or model max? Let's pick a reasonable default.
            max: 65536,
        }
    }
}

// ============================================================================
// Model Definition
// ============================================================================

/// A model definition loaded from the models registry.
///
/// This struct is used to drive provider selection, request limits (context window/max tokens),
/// and cost accounting.
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    OpenAICodexResponses,
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
            Self::OpenAICodexResponses => write!(f, "openai-codex-responses"),
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
            "openai-codex-responses" => Ok(Self::OpenAICodexResponses),
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

impl std::str::FromStr for KnownProvider {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "anthropic" => Ok(Self::Anthropic),
            "openai" => Ok(Self::OpenAI),
            "google" => Ok(Self::Google),
            "google-vertex" => Ok(Self::GoogleVertex),
            "amazon-bedrock" => Ok(Self::AmazonBedrock),
            "azure-openai" | "azure" | "azure-cognitive-services" => Ok(Self::AzureOpenAI),
            "github-copilot" => Ok(Self::GithubCopilot),
            "xai" => Ok(Self::XAI),
            "groq" => Ok(Self::Groq),
            "cerebras" => Ok(Self::Cerebras),
            "openrouter" => Ok(Self::OpenRouter),
            "mistral" => Ok(Self::Mistral),
            other if !other.is_empty() => Ok(Self::Custom(other.to_string())),
            _ => Err("Provider identifier cannot be empty".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // Api enum: FromStr + Display round-trips
    // ========================================================================

    #[test]
    fn api_from_str_known_variants() {
        let cases = [
            ("anthropic-messages", Api::AnthropicMessages),
            ("openai-completions", Api::OpenAICompletions),
            ("openai-responses", Api::OpenAIResponses),
            ("openai-codex-responses", Api::OpenAICodexResponses),
            ("azure-openai-responses", Api::AzureOpenAIResponses),
            ("bedrock-converse-stream", Api::BedrockConverseStream),
            ("google-generative-ai", Api::GoogleGenerativeAI),
            ("google-gemini-cli", Api::GoogleGeminiCli),
            ("google-vertex", Api::GoogleVertex),
        ];
        for (input, expected) in &cases {
            let parsed: Api = input.parse().unwrap();
            assert_eq!(&parsed, expected, "from_str({input})");
        }
    }

    #[test]
    fn api_display_known_variants() {
        let cases = [
            (Api::AnthropicMessages, "anthropic-messages"),
            (Api::OpenAICompletions, "openai-completions"),
            (Api::OpenAIResponses, "openai-responses"),
            (Api::OpenAICodexResponses, "openai-codex-responses"),
            (Api::AzureOpenAIResponses, "azure-openai-responses"),
            (Api::BedrockConverseStream, "bedrock-converse-stream"),
            (Api::GoogleGenerativeAI, "google-generative-ai"),
            (Api::GoogleGeminiCli, "google-gemini-cli"),
            (Api::GoogleVertex, "google-vertex"),
        ];
        for (variant, expected) in &cases {
            assert_eq!(&variant.to_string(), expected, "display for {variant:?}");
        }
    }

    #[test]
    fn api_round_trip_all_known() {
        let variants = [
            Api::AnthropicMessages,
            Api::OpenAICompletions,
            Api::OpenAIResponses,
            Api::OpenAICodexResponses,
            Api::AzureOpenAIResponses,
            Api::BedrockConverseStream,
            Api::GoogleGenerativeAI,
            Api::GoogleGeminiCli,
            Api::GoogleVertex,
        ];
        for variant in &variants {
            let s = variant.to_string();
            let parsed: Api = s.parse().unwrap();
            assert_eq!(&parsed, variant, "round-trip failed for {variant:?} -> {s}");
        }
    }

    #[test]
    fn api_custom_variant() {
        let parsed: Api = "my-custom-api".parse().unwrap();
        assert_eq!(parsed, Api::Custom("my-custom-api".to_string()));
        assert_eq!(parsed.to_string(), "my-custom-api");
    }

    #[test]
    fn api_empty_string_rejected() {
        let result: Result<Api, _> = "".parse();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "API identifier cannot be empty");
    }

    // ========================================================================
    // KnownProvider enum: FromStr + Display round-trips
    // ========================================================================

    #[test]
    fn provider_from_str_known_variants() {
        let cases = [
            ("anthropic", KnownProvider::Anthropic),
            ("openai", KnownProvider::OpenAI),
            ("google", KnownProvider::Google),
            ("google-vertex", KnownProvider::GoogleVertex),
            ("amazon-bedrock", KnownProvider::AmazonBedrock),
            ("azure-openai", KnownProvider::AzureOpenAI),
            ("azure", KnownProvider::AzureOpenAI),
            ("azure-cognitive-services", KnownProvider::AzureOpenAI),
            ("github-copilot", KnownProvider::GithubCopilot),
            ("xai", KnownProvider::XAI),
            ("groq", KnownProvider::Groq),
            ("cerebras", KnownProvider::Cerebras),
            ("openrouter", KnownProvider::OpenRouter),
            ("mistral", KnownProvider::Mistral),
        ];
        for (input, expected) in &cases {
            let parsed: KnownProvider = input.parse().unwrap();
            assert_eq!(&parsed, expected, "from_str({input})");
        }
    }

    #[test]
    fn provider_display_known_variants() {
        let cases = [
            (KnownProvider::Anthropic, "anthropic"),
            (KnownProvider::OpenAI, "openai"),
            (KnownProvider::Google, "google"),
            (KnownProvider::GoogleVertex, "google-vertex"),
            (KnownProvider::AmazonBedrock, "amazon-bedrock"),
            (KnownProvider::AzureOpenAI, "azure-openai"),
            (KnownProvider::GithubCopilot, "github-copilot"),
            (KnownProvider::XAI, "xai"),
            (KnownProvider::Groq, "groq"),
            (KnownProvider::Cerebras, "cerebras"),
            (KnownProvider::OpenRouter, "openrouter"),
            (KnownProvider::Mistral, "mistral"),
        ];
        for (variant, expected) in &cases {
            assert_eq!(&variant.to_string(), expected, "display for {variant:?}");
        }
    }

    #[test]
    fn provider_round_trip_all_known() {
        let variants = [
            KnownProvider::Anthropic,
            KnownProvider::OpenAI,
            KnownProvider::Google,
            KnownProvider::GoogleVertex,
            KnownProvider::AmazonBedrock,
            KnownProvider::AzureOpenAI,
            KnownProvider::GithubCopilot,
            KnownProvider::XAI,
            KnownProvider::Groq,
            KnownProvider::Cerebras,
            KnownProvider::OpenRouter,
            KnownProvider::Mistral,
        ];
        for variant in &variants {
            let s = variant.to_string();
            let parsed: KnownProvider = s.parse().unwrap();
            assert_eq!(&parsed, variant, "round-trip failed for {variant:?} -> {s}");
        }
    }

    #[test]
    fn provider_custom_variant() {
        let parsed: KnownProvider = "my-custom-provider".parse().unwrap();
        assert_eq!(
            parsed,
            KnownProvider::Custom("my-custom-provider".to_string())
        );
        assert_eq!(parsed.to_string(), "my-custom-provider");
    }

    #[test]
    fn provider_empty_string_rejected() {
        let result: Result<KnownProvider, _> = "".parse();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Provider identifier cannot be empty");
    }

    // ========================================================================
    // Model::calculate_cost
    // ========================================================================

    fn test_model() -> Model {
        Model {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            reasoning: false,
            input: vec![InputType::Text],
            cost: ModelCost {
                input: 3.0,   // $3 per million input tokens
                output: 15.0, // $15 per million output tokens
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: 200_000,
            max_tokens: 8192,
            headers: HashMap::new(),
        }
    }

    #[test]
    fn calculate_cost_basic() {
        let model = test_model();
        // 1000 input tokens at $3/M = $0.003
        // 500 output tokens at $15/M = $0.0075
        let cost = model.calculate_cost(1000, 500, 0, 0);
        let input_expected = (3.0 / 1_000_000.0) * 1000.0;
        let output_expected = (15.0 / 1_000_000.0) * 500.0;
        let expected = input_expected + output_expected;
        assert!(
            (cost - expected).abs() < f64::EPSILON,
            "expected {expected}, got {cost}"
        );
    }

    #[test]
    fn calculate_cost_with_cache() {
        let model = test_model();
        let cost = model.calculate_cost(1000, 500, 2000, 1000);
        let input_expected = (3.0 / 1_000_000.0) * 1000.0;
        let output_expected = (15.0 / 1_000_000.0) * 500.0;
        let cache_read_expected = (0.3 / 1_000_000.0) * 2000.0;
        let cache_write_expected = (3.75 / 1_000_000.0) * 1000.0;
        let expected =
            input_expected + output_expected + cache_read_expected + cache_write_expected;
        assert!(
            (cost - expected).abs() < 1e-12,
            "expected {expected}, got {cost}"
        );
    }

    #[test]
    fn calculate_cost_zero_tokens() {
        let model = test_model();
        let cost = model.calculate_cost(0, 0, 0, 0);
        assert!((cost).abs() < f64::EPSILON, "zero tokens should cost $0");
    }

    #[test]
    fn calculate_cost_large_token_count() {
        let model = test_model();
        // 1 million tokens each
        let cost = model.calculate_cost(1_000_000, 1_000_000, 0, 0);
        let expected = 3.0 + 15.0; // $3 input + $15 output
        assert!(
            (cost - expected).abs() < 1e-10,
            "expected {expected}, got {cost}"
        );
    }

    // ========================================================================
    // Default values
    // ========================================================================

    #[test]
    fn thinking_budgets_default() {
        let budgets = ThinkingBudgets::default();
        assert_eq!(budgets.minimal, 1024);
        assert_eq!(budgets.low, 2048);
        assert_eq!(budgets.medium, 8192);
        assert_eq!(budgets.high, 16384);
        assert_eq!(budgets.xhigh, 32768);
    }

    #[test]
    fn cache_retention_default_is_none() {
        assert_eq!(CacheRetention::default(), CacheRetention::None);
    }

    #[test]
    fn stream_options_default() {
        let opts = StreamOptions::default();
        assert!(opts.temperature.is_none());
        assert!(opts.max_tokens.is_none());
        assert!(opts.api_key.is_none());
        assert_eq!(opts.cache_retention, CacheRetention::None);
        assert!(opts.session_id.is_none());
        assert!(opts.headers.is_empty());
        assert!(opts.thinking_level.is_none());
        assert!(opts.thinking_budgets.is_none());
    }

    #[test]
    fn context_default() {
        let ctx = Context::default();
        assert!(ctx.system_prompt.is_none());
        assert!(ctx.messages.is_empty());
        assert!(ctx.tools.is_empty());
    }

    // ========================================================================
    // InputType serde
    // ========================================================================

    #[test]
    fn input_type_serialization() {
        let text_json = serde_json::to_string(&InputType::Text).unwrap();
        assert_eq!(text_json, "\"text\"");

        let image_json = serde_json::to_string(&InputType::Image).unwrap();
        assert_eq!(image_json, "\"image\"");

        let text: InputType = serde_json::from_str("\"text\"").unwrap();
        assert_eq!(text, InputType::Text);

        let image: InputType = serde_json::from_str("\"image\"").unwrap();
        assert_eq!(image, InputType::Image);
    }

    // ========================================================================
    // ModelCost serde
    // ========================================================================

    #[test]
    fn model_cost_camel_case_serialization() {
        let cost = ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        };
        let json = serde_json::to_string(&cost).unwrap();
        assert!(
            json.contains("\"cacheRead\""),
            "should use camelCase: {json}"
        );
        assert!(
            json.contains("\"cacheWrite\""),
            "should use camelCase: {json}"
        );

        let parsed: ModelCost = serde_json::from_str(&json).unwrap();
        assert!((parsed.input - 3.0).abs() < f64::EPSILON);
        assert!((parsed.cache_read - 0.3).abs() < f64::EPSILON);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        fn arb_model(rate: f64) -> Model {
            Model {
                id: "m".to_string(),
                name: "m".to_string(),
                api: "anthropic-messages".to_string(),
                provider: "test".to_string(),
                base_url: String::new(),
                reasoning: false,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: rate,
                    output: rate,
                    cache_read: rate,
                    cache_write: rate,
                },
                context_window: 128_000,
                max_tokens: 8192,
                headers: HashMap::new(),
            }
        }

        // ====================================================================
        // calculate_cost
        // ====================================================================

        proptest! {
            #[test]
            fn cost_zero_tokens_is_zero(rate in 0.0f64..1000.0) {
                let m = arb_model(rate);
                let cost = m.calculate_cost(0, 0, 0, 0);
                assert!((cost).abs() < f64::EPSILON);
            }

            #[test]
            fn cost_non_negative(
                rate in 0.0f64..100.0,
                input in 0u64..10_000_000,
                output in 0u64..10_000_000,
                cr in 0u64..10_000_000,
                cw in 0u64..10_000_000,
            ) {
                let m = arb_model(rate);
                assert!(m.calculate_cost(input, output, cr, cw) >= 0.0);
            }

            #[test]
            fn cost_linearity(
                rate in 0.001f64..50.0,
                tokens in 1u64..1_000_000,
            ) {
                let m = arb_model(rate);
                let single = m.calculate_cost(tokens, 0, 0, 0);
                let double = m.calculate_cost(tokens * 2, 0, 0, 0);
                assert!((double - single * 2.0).abs() < 1e-6,
                    "doubling tokens should double cost: single={single}, double={double}");
            }

            #[test]
            fn cost_additivity(
                rate in 0.001f64..50.0,
                input in 0u64..1_000_000,
                output in 0u64..1_000_000,
            ) {
                let m = arb_model(rate);
                let combined = m.calculate_cost(input, output, 0, 0);
                let separate = m.calculate_cost(input, 0, 0, 0)
                    + m.calculate_cost(0, output, 0, 0);
                assert!((combined - separate).abs() < 1e-10,
                    "cost should be additive: combined={combined}, separate={separate}");
            }
        }

        // ====================================================================
        // Api FromStr + Display round-trip
        // ====================================================================

        proptest! {
            #[test]
            fn api_custom_round_trip(s in "[a-z][a-z0-9-]{0,20}") {
                let known = [
                    "anthropic-messages", "openai-completions", "openai-responses", "openai-codex-responses",
                    "azure-openai-responses", "bedrock-converse-stream",
                    "google-generative-ai", "google-gemini-cli", "google-vertex",
                ];
                if !known.contains(&s.as_str()) {
                    let parsed: Api = s.parse().unwrap();
                    assert_eq!(parsed.to_string(), s);
                }
            }

            #[test]
            fn api_never_panics(s in ".*") {
                let _ = s.parse::<Api>(); // must not panic
            }

            #[test]
            fn api_empty_always_rejected(ws in "[ \t]*") {
                if ws.is_empty() {
                    assert!(ws.parse::<Api>().is_err());
                }
            }
        }

        // ====================================================================
        // KnownProvider FromStr + Display round-trip
        // ====================================================================

        proptest! {
            #[test]
            fn provider_custom_round_trip(s in "[a-z][a-z0-9-]{0,20}") {
                let known = [
                    "anthropic", "openai", "google", "google-vertex",
                    "amazon-bedrock", "azure-openai", "azure",
                    "azure-cognitive-services", "github-copilot", "xai",
                    "groq", "cerebras", "openrouter", "mistral",
                ];
                if !known.contains(&s.as_str()) {
                    let parsed: KnownProvider = s.parse().unwrap();
                    assert_eq!(parsed.to_string(), s);
                }
            }

            #[test]
            fn provider_never_panics(s in ".*") {
                let _ = s.parse::<KnownProvider>(); // must not panic
            }
        }

        // ====================================================================
        // ModelCost serde round-trip
        // ====================================================================

        proptest! {
            #[test]
            fn model_cost_serde_round_trip(
                input in 0.0f64..1000.0,
                output in 0.0f64..1000.0,
                cr in 0.0f64..1000.0,
                cw in 0.0f64..1000.0,
            ) {
                let cost = ModelCost { input, output, cache_read: cr, cache_write: cw };
                let json = serde_json::to_string(&cost).unwrap();
                let parsed: ModelCost = serde_json::from_str(&json).unwrap();
                assert!((parsed.input - input).abs() < 1e-10);
                assert!((parsed.output - output).abs() < 1e-10);
                assert!((parsed.cache_read - cr).abs() < 1e-10);
                assert!((parsed.cache_write - cw).abs() < 1e-10);
            }
        }
    }
}
