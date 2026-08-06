//! Canonical provider metadata shared across runtime surfaces.
//!
//! This module is intentionally data-first: it centralizes provider identifiers,
//! aliases, auth env keys, and default routing hints so models/auth/provider
//! selection paths don't drift independently.

use crate::provider::InputType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderOnboardingMode {
    BuiltInNative,
    OpenAICompatiblePreset,
    NativeAdapterRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ProviderTestObligations {
    pub unit: bool,
    pub contract: bool,
    pub conformance: bool,
    pub e2e: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderRoutingDefaults {
    pub api: &'static str,
    pub base_url: &'static str,
    pub auth_header: bool,
    pub reasoning: bool,
    pub input: &'static [InputType],
    /// Provider-level fallback context window.  Per-model values from the
    /// model registry (`models.json` / built-in catalog) take precedence
    /// at runtime.  This is only used when a model entry lacks an explicit
    /// `context_window` value.
    pub context_window: u32,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderMetadata {
    pub canonical_id: &'static str,
    pub display_name: Option<&'static str>,
    pub aliases: &'static [&'static str],
    pub auth_env_keys: &'static [&'static str],
    pub onboarding: ProviderOnboardingMode,
    pub routing_defaults: Option<ProviderRoutingDefaults>,
    pub test_obligations: ProviderTestObligations,
}

const INPUT_TEXT: [InputType; 1] = [InputType::Text];
const INPUT_TEXT_IMAGE: [InputType; 2] = [InputType::Text, InputType::Image];

const TEST_REQUIRED: ProviderTestObligations = ProviderTestObligations {
    unit: true,
    contract: true,
    conformance: true,
    e2e: true,
};

pub const PROVIDER_METADATA: &[ProviderMetadata] = &[
    ProviderMetadata {
        canonical_id: "anthropic",
        display_name: Some("Anthropic"),
        aliases: &[],
        auth_env_keys: &["ANTHROPIC_API_KEY"],
        onboarding: ProviderOnboardingMode::BuiltInNative,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "anthropic-messages",
            base_url: "https://api.anthropic.com/v1/messages",
            auth_header: false,
            reasoning: true,
            input: &INPUT_TEXT_IMAGE,
            context_window: 200_000,
            max_tokens: 8192,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "openai",
        display_name: Some("OpenAI"),
        aliases: &[],
        auth_env_keys: &["OPENAI_API_KEY"],
        onboarding: ProviderOnboardingMode::BuiltInNative,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-responses",
            base_url: "https://api.openai.com/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT_IMAGE,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "openai-codex",
        display_name: Some("OpenAI Codex (ChatGPT)"),
        aliases: &["codex", "chatgpt-codex"],
        auth_env_keys: &[],
        onboarding: ProviderOnboardingMode::NativeAdapterRequired,
        routing_defaults: None,
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "google",
        display_name: Some("Google Gemini"),
        aliases: &["gemini"],
        auth_env_keys: &["GOOGLE_API_KEY", "GEMINI_API_KEY"],
        onboarding: ProviderOnboardingMode::BuiltInNative,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "google-generative-ai",
            base_url: "https://generativelanguage.googleapis.com/v1beta",
            auth_header: false,
            reasoning: true,
            input: &INPUT_TEXT_IMAGE,
            context_window: 128_000,
            max_tokens: 8192,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "google-gemini-cli",
        display_name: Some("Google Cloud Code Assist"),
        aliases: &["gemini-cli"],
        auth_env_keys: &[],
        onboarding: ProviderOnboardingMode::NativeAdapterRequired,
        routing_defaults: None,
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "google-antigravity",
        display_name: Some("Google Antigravity"),
        aliases: &["antigravity"],
        auth_env_keys: &[],
        onboarding: ProviderOnboardingMode::NativeAdapterRequired,
        routing_defaults: None,
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "cohere",
        display_name: Some("Cohere"),
        aliases: &[],
        auth_env_keys: &["COHERE_API_KEY"],
        onboarding: ProviderOnboardingMode::BuiltInNative,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "cohere-chat",
            base_url: "https://api.cohere.com/v2",
            auth_header: false,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 8192,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "cursor",
        display_name: Some("Cursor"),
        aliases: &["cursor-agent"],
        auth_env_keys: &["CURSOR_API_KEY", "CURSOR_ACCESS_TOKEN"],
        onboarding: ProviderOnboardingMode::BuiltInNative,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "cursor-agent",
            base_url: "https://api2.cursor.sh/agent.v1.AgentService/Run",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 200_000,
            max_tokens: 64_000,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "groq",
        display_name: Some("Groq"),
        aliases: &[],
        auth_env_keys: &["GROQ_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.groq.com/openai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "deepinfra",
        display_name: Some("Deep Infra"),
        aliases: &["deep-infra"],
        auth_env_keys: &["DEEPINFRA_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.deepinfra.com/v1/openai",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "cerebras",
        display_name: Some("Cerebras"),
        aliases: &[],
        auth_env_keys: &["CEREBRAS_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.cerebras.ai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        // OpenAI-compatible GPU cloud (https://www.atlascloud.ai); serves
        // open-weight chat/reasoning models via chat completions (gh #141).
        canonical_id: "atlascloud",
        display_name: Some("Atlas Cloud"),
        aliases: &["atlas-cloud", "atlas"],
        auth_env_keys: &["ATLASCLOUD_API_KEY", "ATLAS_CLOUD_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.atlascloud.ai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 131_072,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "openrouter",
        display_name: Some("OpenRouter"),
        aliases: &["open-router"],
        auth_env_keys: &["OPENROUTER_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://openrouter.ai/api/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "mistral",
        display_name: Some("Mistral AI"),
        aliases: &["mistralai"],
        auth_env_keys: &["MISTRAL_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.mistral.ai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "moonshotai",
        display_name: Some("Moonshot AI"),
        aliases: &["moonshot", "kimi"],
        auth_env_keys: &["MOONSHOT_API_KEY", "KIMI_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.moonshot.ai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "alibaba",
        display_name: Some("Alibaba (Qwen)"),
        aliases: &["dashscope", "qwen"],
        auth_env_keys: &["DASHSCOPE_API_KEY", "QWEN_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "deepseek",
        display_name: Some("DeepSeek"),
        aliases: &["deep-seek"],
        auth_env_keys: &["DEEPSEEK_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.deepseek.com",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "fireworks",
        display_name: Some("Fireworks AI"),
        aliases: &["fireworks-ai"],
        auth_env_keys: &["FIREWORKS_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.fireworks.ai/inference/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "togetherai",
        display_name: Some("Together AI"),
        aliases: &["together", "together-ai"],
        auth_env_keys: &["TOGETHER_API_KEY", "TOGETHER_AI_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.together.xyz/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "perplexity",
        display_name: Some("Perplexity"),
        aliases: &["pplx"],
        auth_env_keys: &["PERPLEXITY_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.perplexity.ai",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "xai",
        display_name: Some("xAI (Grok)"),
        aliases: &["grok", "x-ai"],
        auth_env_keys: &["XAI_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.x.ai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    // ── Batch A1: OAI-compatible preset providers ──────────────────────
    ProviderMetadata {
        canonical_id: "302ai",
        display_name: Some("302.AI"),
        aliases: &[],
        auth_env_keys: &["302AI_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.302.ai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "abacus",
        display_name: Some("Abacus AI"),
        aliases: &[],
        auth_env_keys: &["ABACUS_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://routellm.abacus.ai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "aihubmix",
        display_name: Some("AIHubMix"),
        aliases: &[],
        auth_env_keys: &["AIHUBMIX_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://aihubmix.com/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "bailing",
        display_name: Some("Bailing"),
        aliases: &[],
        auth_env_keys: &["BAILING_API_TOKEN"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.tbox.cn/api/llm/v1",
            auth_header: true,
            reasoning: false,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "berget",
        display_name: Some("Berget"),
        aliases: &[],
        auth_env_keys: &["BERGET_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.berget.ai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "chutes",
        display_name: Some("Chutes"),
        aliases: &[],
        auth_env_keys: &["CHUTES_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://llm.chutes.ai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "cortecs",
        display_name: Some("Cortecs"),
        aliases: &[],
        auth_env_keys: &["CORTECS_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.cortecs.ai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "fastrouter",
        display_name: Some("FastRouter"),
        aliases: &[],
        auth_env_keys: &["FASTROUTER_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://go.fastrouter.ai/api/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    // ── Batch A2: OAI-compatible preset providers ──────────────────────
    ProviderMetadata {
        canonical_id: "firmware",
        display_name: Some("Firmware"),
        aliases: &[],
        auth_env_keys: &["FIRMWARE_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://app.firmware.ai/api/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "friendli",
        display_name: Some("Friendli"),
        aliases: &[],
        auth_env_keys: &["FRIENDLI_TOKEN"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.friendli.ai/serverless/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    // `github-models` is intentionally absent. GitHub retired the Models
    // catalog, inference API, and BYOK service on 2026-07-30. The separate
    // `github-copilot` native adapter remains supported.
    ProviderMetadata {
        canonical_id: "helicone",
        display_name: Some("Helicone"),
        aliases: &[],
        auth_env_keys: &["HELICONE_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://ai-gateway.helicone.ai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "huggingface",
        display_name: Some("Hugging Face"),
        aliases: &["hf", "hugging-face"],
        auth_env_keys: &["HF_TOKEN"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://router.huggingface.co/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "iflowcn",
        display_name: Some("iFlow"),
        aliases: &[],
        auth_env_keys: &["IFLOW_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://apis.iflow.cn/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "inception",
        display_name: Some("Inception"),
        aliases: &[],
        auth_env_keys: &["INCEPTION_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.inceptionlabs.ai/v1",
            auth_header: true,
            reasoning: false,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "inference",
        display_name: Some("Inference"),
        aliases: &[],
        auth_env_keys: &["INFERENCE_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://inference.net/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    // ── Batch A3: OAI-compatible preset providers ──────────────────────
    ProviderMetadata {
        canonical_id: "io-net",
        display_name: Some("io.net"),
        aliases: &[],
        auth_env_keys: &["IOINTELLIGENCE_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.intelligence.io.solutions/api/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "jiekou",
        display_name: Some("Jiekou"),
        aliases: &[],
        auth_env_keys: &["JIEKOU_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.jiekou.ai/openai",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "lucidquery",
        display_name: Some("LucidQuery"),
        aliases: &[],
        auth_env_keys: &["LUCIDQUERY_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://lucidquery.com/api/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "moark",
        display_name: Some("Moark"),
        aliases: &[],
        auth_env_keys: &["MOARK_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://moark.com/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "morph",
        display_name: Some("Morph"),
        aliases: &[],
        auth_env_keys: &["MORPH_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.morphllm.com/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "nano-gpt",
        display_name: Some("NanoGPT"),
        aliases: &["nanogpt"],
        auth_env_keys: &["NANO_GPT_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://nano-gpt.com/api/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "nova",
        display_name: Some("Nova"),
        aliases: &[],
        auth_env_keys: &["NOVA_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.nova.amazon.com/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "novita-ai",
        display_name: Some("Novita AI"),
        aliases: &["novita"],
        auth_env_keys: &["NOVITA_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.novita.ai/openai",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "nvidia",
        display_name: Some("NVIDIA NIM"),
        aliases: &["nim", "nvidia-nim"],
        auth_env_keys: &["NVIDIA_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://integrate.api.nvidia.com/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    // ── Batch A4: OAI-compatible preset providers ──────────────────────
    ProviderMetadata {
        canonical_id: "poe",
        display_name: Some("Poe"),
        aliases: &[],
        auth_env_keys: &["POE_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.poe.com/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "privatemode-ai",
        display_name: Some("PrivateMode AI"),
        aliases: &[],
        auth_env_keys: &["PRIVATEMODE_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            // Default is localhost; users override via PRIVATEMODE_ENDPOINT env var.
            base_url: "http://localhost:8080/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "requesty",
        display_name: Some("Requesty"),
        aliases: &[],
        auth_env_keys: &["REQUESTY_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://router.requesty.ai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "submodel",
        display_name: Some("Submodel"),
        aliases: &[],
        auth_env_keys: &["SUBMODEL_INSTAGEN_ACCESS_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://llm.submodel.ai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "synthetic",
        display_name: Some("Synthetic"),
        aliases: &[],
        auth_env_keys: &["SYNTHETIC_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.synthetic.new/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "vivgrid",
        display_name: Some("Vivgrid"),
        aliases: &[],
        auth_env_keys: &["VIVGRID_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.vivgrid.com/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "vultr",
        display_name: Some("Vultr"),
        aliases: &[],
        auth_env_keys: &["VULTR_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.vultrinference.com/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "wandb",
        display_name: Some("Weights & Biases"),
        aliases: &[],
        auth_env_keys: &["WANDB_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.inference.wandb.ai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "xiaomi",
        display_name: Some("Xiaomi"),
        aliases: &[],
        auth_env_keys: &["XIAOMI_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.xiaomimimo.com/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    // ── Batch B1: Regional + coding-plan providers ─────────────────────
    ProviderMetadata {
        canonical_id: "alibaba-cn",
        display_name: Some("Alibaba China"),
        aliases: &[],
        auth_env_keys: &["DASHSCOPE_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "alibaba-us",
        display_name: Some("Alibaba US"),
        aliases: &[],
        auth_env_keys: &["DASHSCOPE_API_KEY", "QWEN_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://dashscope-us.aliyuncs.com/compatible-mode/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "kimi-for-coding",
        display_name: Some("Kimi for Coding"),
        aliases: &["kimi-coding", "kimi-code"],
        auth_env_keys: &["KIMI_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "anthropic-messages",
            base_url: "https://api.kimi.com/coding/v1/messages",
            auth_header: false,
            reasoning: true,
            input: &INPUT_TEXT_IMAGE,
            context_window: 262_144,
            max_tokens: 32_768,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "minimax",
        display_name: Some("MiniMax"),
        aliases: &[],
        auth_env_keys: &["MINIMAX_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "anthropic-messages",
            base_url: "https://api.minimax.io/anthropic/v1/messages",
            auth_header: false,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 204_800,
            max_tokens: 131_072,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "minimax-cn",
        display_name: Some("MiniMax China"),
        aliases: &[],
        auth_env_keys: &["MINIMAX_CN_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "anthropic-messages",
            base_url: "https://api.minimaxi.com/anthropic/v1/messages",
            auth_header: false,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 204_800,
            max_tokens: 131_072,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "minimax-coding-plan",
        display_name: Some("MiniMax Coding Plan"),
        aliases: &[],
        auth_env_keys: &["MINIMAX_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "anthropic-messages",
            base_url: "https://api.minimax.io/anthropic/v1/messages",
            auth_header: false,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 204_800,
            max_tokens: 131_072,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "minimax-cn-coding-plan",
        display_name: Some("MiniMax China Coding Plan"),
        aliases: &[],
        auth_env_keys: &["MINIMAX_CN_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "anthropic-messages",
            base_url: "https://api.minimaxi.com/anthropic/v1/messages",
            auth_header: false,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 204_800,
            max_tokens: 131_072,
        }),
        test_obligations: TEST_REQUIRED,
    },
    // ── Batch B2: Regional + cloud providers ────────────────────────────
    ProviderMetadata {
        canonical_id: "modelscope",
        display_name: Some("ModelScope"),
        aliases: &[],
        auth_env_keys: &["MODELSCOPE_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api-inference.modelscope.cn/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 131_072,
            max_tokens: 98_304,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "moonshotai-cn",
        display_name: Some("Moonshot AI China"),
        aliases: &[],
        auth_env_keys: &["MOONSHOT_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.moonshot.cn/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 262_144,
            max_tokens: 262_144,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "nebius",
        display_name: Some("Nebius"),
        aliases: &[],
        auth_env_keys: &["NEBIUS_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.tokenfactory.nebius.com/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 8192,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "ovhcloud",
        display_name: Some("OVHcloud"),
        aliases: &[],
        auth_env_keys: &["OVHCLOUD_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 32_768,
            max_tokens: 32_768,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "scaleway",
        display_name: Some("Scaleway"),
        aliases: &[],
        auth_env_keys: &["SCALEWAY_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.scaleway.ai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 260_000,
            max_tokens: 8192,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "stackit",
        display_name: Some("STACKIT"),
        aliases: &[],
        auth_env_keys: &["STACKIT_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.openai-compat.model-serving.eu01.onstackit.cloud/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 8192,
        }),
        test_obligations: TEST_REQUIRED,
    },
    // ── Batch B3: Regional + coding-plan providers ──────────────────────
    ProviderMetadata {
        canonical_id: "siliconflow",
        display_name: Some("SiliconFlow"),
        aliases: &["silicon-flow"],
        auth_env_keys: &["SILICONFLOW_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.siliconflow.com/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "siliconflow-cn",
        display_name: Some("SiliconFlow China"),
        aliases: &[],
        auth_env_keys: &["SILICONFLOW_CN_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.siliconflow.cn/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "upstage",
        display_name: Some("Upstage"),
        aliases: &[],
        auth_env_keys: &["UPSTAGE_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.upstage.ai/v1/solar",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "venice",
        display_name: Some("Venice AI"),
        aliases: &[],
        auth_env_keys: &["VENICE_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.venice.ai/api/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "zai",
        display_name: Some("Zai"),
        aliases: &[],
        auth_env_keys: &["ZHIPU_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.z.ai/api/paas/v4",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "zai-coding-plan",
        display_name: Some("Zai Coding Plan"),
        aliases: &[],
        auth_env_keys: &["ZHIPU_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.z.ai/api/coding/paas/v4",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "zhipuai",
        display_name: Some("Zhipu AI"),
        aliases: &["zhipu", "glm"],
        auth_env_keys: &["ZHIPU_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://open.bigmodel.cn/api/paas/v4",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "zhipuai-coding-plan",
        display_name: Some("Zhipu AI Coding Plan"),
        aliases: &[],
        auth_env_keys: &["ZHIPU_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    // ── Batch C1: Local/self-hosted preset providers ──────────────────────
    ProviderMetadata {
        canonical_id: "baseten",
        display_name: Some("Baseten"),
        aliases: &[],
        auth_env_keys: &["BASETEN_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://inference.baseten.co/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 262_144,
            max_tokens: 65_536,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "llama",
        display_name: Some("Meta Llama"),
        aliases: &[],
        auth_env_keys: &["LLAMA_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.llama.com/compat/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT_IMAGE,
            context_window: 128_000,
            max_tokens: 4096,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "lmstudio",
        display_name: Some("LM Studio"),
        aliases: &["lm-studio"],
        auth_env_keys: &["LMSTUDIO_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "http://127.0.0.1:1234/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 131_072,
            max_tokens: 32_768,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "ollama",
        display_name: Some("Ollama"),
        aliases: &[],
        auth_env_keys: &[],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "http://127.0.0.1:11434/v1",
            auth_header: false,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 131_072,
            max_tokens: 32_768,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        // llama.cpp's bundled `llama-server` exposes an OpenAI-compatible API on
        // localhost and requires NO API key by default (parity with ollama /
        // lmstudio). It is registered as a first-class local provider so that
        // `pi --provider llamacpp --model <id>` works out-of-the-box without a
        // models.json entry and, critically, without tripping the API-key gate.
        // (#104)
        canonical_id: "llamacpp",
        display_name: Some("llama.cpp"),
        aliases: &["llama-cpp", "llama.cpp", "llama-server"],
        auth_env_keys: &[],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            // llama-server's default bind address/port.
            base_url: "http://127.0.0.1:8080/v1",
            auth_header: false,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 131_072,
            max_tokens: 32_768,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        // mistral.rs ships an OpenAI-compatible HTTP server that listens on
        // localhost and accepts an empty API key by default. Like ollama /
        // llamacpp it is a LOCAL provider with no auth, so it must bypass the
        // API-key requirement. (#104)
        canonical_id: "mistralrs",
        display_name: Some("mistral.rs"),
        aliases: &["mistral.rs", "mistral-rs"],
        auth_env_keys: &[],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            // mistral.rs server's default bind address/port.
            base_url: "http://127.0.0.1:1234/v1",
            auth_header: false,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 131_072,
            max_tokens: 32_768,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "ollama-cloud",
        display_name: Some("Ollama Cloud"),
        aliases: &[],
        auth_env_keys: &["OLLAMA_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://ollama.com/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT_IMAGE,
            context_window: 262_144,
            max_tokens: 131_072,
        }),
        test_obligations: TEST_REQUIRED,
    },
    // ── Special routing providers (bd-3uqg.3.9) ────────────────────────────
    ProviderMetadata {
        canonical_id: "opencode",
        display_name: Some("OpenCode"),
        aliases: &[],
        auth_env_keys: &["OPENCODE_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://opencode.ai/zen/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "vercel",
        display_name: Some("Vercel AI"),
        aliases: &["vercel-ai-gateway"],
        auth_env_keys: &["AI_GATEWAY_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://ai-gateway.vercel.sh/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "zenmux",
        display_name: Some("ZenMux"),
        aliases: &[],
        auth_env_keys: &["ZENMUX_API_KEY"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "anthropic-messages",
            base_url: "https://zenmux.ai/api/anthropic/v1/messages",
            auth_header: false,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 200_000,
            max_tokens: 8192,
        }),
        test_obligations: TEST_REQUIRED,
    },
    // ── Cloudflare provider IDs (gateway + workers-ai) ────────────────────
    ProviderMetadata {
        canonical_id: "cloudflare-ai-gateway",
        display_name: Some("Cloudflare AI Gateway"),
        aliases: &[],
        auth_env_keys: &["CLOUDFLARE_API_TOKEN"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://gateway.ai.cloudflare.com/v1/{account_id}/{gateway_id}/openai",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "cloudflare-workers-ai",
        display_name: Some("Cloudflare Workers AI"),
        aliases: &[],
        auth_env_keys: &["CLOUDFLARE_API_TOKEN"],
        onboarding: ProviderOnboardingMode::OpenAICompatiblePreset,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "openai-completions",
            base_url: "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 128_000,
            max_tokens: 16_384,
        }),
        test_obligations: TEST_REQUIRED,
    },
    // ── Native adapter required providers ────────────────────────────────
    ProviderMetadata {
        canonical_id: "google-vertex",
        display_name: Some("Google Vertex AI"),
        aliases: &["vertexai", "google-vertex-anthropic"],
        auth_env_keys: &["GOOGLE_CLOUD_API_KEY", "VERTEX_API_KEY"],
        onboarding: ProviderOnboardingMode::BuiltInNative,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "google-vertex",
            base_url: "",
            auth_header: true,
            reasoning: true,
            input: &INPUT_TEXT_IMAGE,
            context_window: 1_000_000,
            max_tokens: 8192,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "amazon-bedrock",
        display_name: Some("Amazon Bedrock"),
        aliases: &["bedrock"],
        auth_env_keys: &[
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_BEARER_TOKEN_BEDROCK",
            "AWS_PROFILE",
            "AWS_REGION",
        ],
        onboarding: ProviderOnboardingMode::NativeAdapterRequired,
        routing_defaults: Some(ProviderRoutingDefaults {
            api: "bedrock-converse-stream",
            base_url: "",
            auth_header: false,
            reasoning: true,
            input: &INPUT_TEXT,
            context_window: 200_000,
            max_tokens: 8192,
        }),
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "sap-ai-core",
        display_name: Some("SAP AI Core"),
        aliases: &["sap"],
        auth_env_keys: &[
            "AICORE_SERVICE_KEY",
            "SAP_AI_CORE_CLIENT_ID",
            "SAP_AI_CORE_CLIENT_SECRET",
            "SAP_AI_CORE_TOKEN_URL",
            "SAP_AI_CORE_SERVICE_URL",
        ],
        onboarding: ProviderOnboardingMode::NativeAdapterRequired,
        routing_defaults: None,
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "v0",
        display_name: Some("v0 by Vercel"),
        aliases: &[],
        auth_env_keys: &["V0_API_KEY"],
        onboarding: ProviderOnboardingMode::NativeAdapterRequired,
        routing_defaults: None,
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "azure-openai",
        display_name: Some("Azure OpenAI"),
        aliases: &[
            "azure",
            "azure_openai",
            "azure-cognitive-services",
            "azure-openai-responses",
        ],
        auth_env_keys: &["AZURE_OPENAI_API_KEY"],
        onboarding: ProviderOnboardingMode::NativeAdapterRequired,
        routing_defaults: None,
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "github-copilot",
        display_name: Some("GitHub Copilot"),
        aliases: &["copilot", "github-copilot-enterprise"],
        auth_env_keys: &["GITHUB_COPILOT_API_KEY", "GITHUB_TOKEN"],
        onboarding: ProviderOnboardingMode::NativeAdapterRequired,
        routing_defaults: None,
        test_obligations: TEST_REQUIRED,
    },
    ProviderMetadata {
        canonical_id: "gitlab",
        display_name: Some("GitLab Duo"),
        aliases: &["gitlab-duo"],
        auth_env_keys: &["GITLAB_TOKEN", "GITLAB_API_KEY"],
        onboarding: ProviderOnboardingMode::NativeAdapterRequired,
        routing_defaults: None,
        test_obligations: TEST_REQUIRED,
    },
];

pub fn provider_metadata(provider_id: &str) -> Option<&'static ProviderMetadata> {
    let provider_id = provider_id.trim();
    if provider_id.is_empty() {
        return None;
    }

    PROVIDER_METADATA.iter().find(|meta| {
        meta.canonical_id.eq_ignore_ascii_case(provider_id)
            || meta
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(provider_id))
    })
}

pub fn canonical_provider_id(provider_id: &str) -> Option<&'static str> {
    provider_metadata(provider_id).map(|meta| meta.canonical_id)
}

pub fn provider_auth_env_keys(provider_id: &str) -> &'static [&'static str] {
    provider_metadata(provider_id).map_or(&[], |meta| meta.auth_env_keys)
}

pub fn provider_routing_defaults(provider_id: &str) -> Option<ProviderRoutingDefaults> {
    provider_metadata(provider_id).and_then(|meta| meta.routing_defaults)
}

/// Whether `provider_id` is a known local / self-hosted, no-auth provider.
///
/// Such providers (e.g. `ollama`, `llamacpp`, `mistralrs`) run an
/// OpenAI-compatible server on localhost and require NO authentication: no API
/// key and no `Authorization` header.
///
/// Used by the OpenAI-completions / OpenAI-responses request paths so that a
/// missing API key is NOT treated as a fatal error for these providers (they
/// are simply called without a bearer token), matching how `ollama` already
/// works. Without this, `pi --provider llamacpp ...` errors with
/// "Missing API key for provider" even though the provider needs none. (#104)
///
/// The predicate is derived from the canonical metadata (`auth_env_keys` empty
/// AND routing `auth_header == false`) rather than a hardcoded allowlist, so it
/// stays correct as new keyless local providers are added.
pub fn provider_is_keyless_local(provider_id: &str) -> bool {
    provider_metadata(provider_id).is_some_and(|meta| {
        meta.auth_env_keys.is_empty()
            && meta
                .routing_defaults
                .is_some_and(|defaults| !defaults.auth_header)
    })
}

/// Compare two provider IDs for equality, resolving aliases via
/// [`canonical_provider_id`].
pub fn provider_ids_match(left: &str, right: &str) -> bool {
    let left = left.trim();
    let right = right.trim();
    if left.eq_ignore_ascii_case(right) {
        return true;
    }

    let left_canonical = canonical_provider_id(left).unwrap_or(left);
    let right_canonical = canonical_provider_id(right).unwrap_or(right);

    left_canonical.eq_ignore_ascii_case(right)
        || right_canonical.eq_ignore_ascii_case(left)
        || left_canonical.eq_ignore_ascii_case(right_canonical)
}

/// Split a `"provider/model"` spec into `(provider, model_id)`.
///
/// Handles nested model paths such as `"openrouter/anthropic/claude-sonnet-4.5"`.
pub fn split_provider_model_spec(model_spec: &str) -> Option<(&str, &str)> {
    let (provider, model_id) = model_spec.split_once('/')?;
    let provider = provider.trim();
    let model_id = model_id.trim();
    if provider.is_empty() || model_id.is_empty() {
        return None;
    }
    Some((provider, model_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_resolves_canonical_and_alias_names() {
        let canonical = provider_metadata("moonshotai").expect("moonshot metadata");
        assert_eq!(canonical.canonical_id, "moonshotai");
        let alias = provider_metadata("kimi").expect("alias metadata");
        assert_eq!(alias.canonical_id, "moonshotai");
        let google_alias = provider_metadata("gemini").expect("gemini alias metadata");
        assert_eq!(google_alias.canonical_id, "google");
        let azure_alias = provider_metadata("azure").expect("azure alias metadata");
        assert_eq!(azure_alias.canonical_id, "azure-openai");
        let azure_cognitive_alias =
            provider_metadata("azure-cognitive-services").expect("azure-cognitive alias metadata");
        assert_eq!(azure_cognitive_alias.canonical_id, "azure-openai");
        let azure_responses_alias =
            provider_metadata("azure-openai-responses").expect("azure responses alias metadata");
        assert_eq!(azure_responses_alias.canonical_id, "azure-openai");
        let vertex_anthropic_alias = provider_metadata("google-vertex-anthropic")
            .expect("google-vertex-anthropic alias metadata");
        assert_eq!(vertex_anthropic_alias.canonical_id, "google-vertex");
        let copilot_enterprise_alias = provider_metadata("github-copilot-enterprise")
            .expect("github-copilot-enterprise alias metadata");
        assert_eq!(copilot_enterprise_alias.canonical_id, "github-copilot");
        let openrouter_alias =
            provider_metadata("open-router").expect("open-router alias metadata");
        assert_eq!(openrouter_alias.canonical_id, "openrouter");
        let vercel_gateway_alias =
            provider_metadata("vercel-ai-gateway").expect("vercel alias metadata");
        assert_eq!(vercel_gateway_alias.canonical_id, "vercel");
        let kimi_coding_alias =
            provider_metadata("kimi-coding").expect("kimi-coding alias metadata");
        assert_eq!(kimi_coding_alias.canonical_id, "kimi-for-coding");
        let kimi_code_alias = provider_metadata("kimi-code").expect("kimi-code alias metadata");
        assert_eq!(kimi_code_alias.canonical_id, "kimi-for-coding");
    }

    #[test]
    fn metadata_trims_provider_id_whitespace() {
        let meta = provider_metadata("  openai  ").expect("openai metadata");
        assert_eq!(meta.canonical_id, "openai");
        assert_eq!(
            canonical_provider_id("  openrouter ").expect("openrouter canonical"),
            "openrouter"
        );
    }

    #[test]
    fn metadata_resolves_ux_discoverability_aliases() {
        // Aliases added to improve user discoverability (bd-tuh3g / bd-3uqg.14.3.4)
        let cases: &[(&str, &str)] = &[
            ("together", "togetherai"),
            ("together-ai", "togetherai"),
            ("grok", "xai"),
            ("x-ai", "xai"),
            ("hf", "huggingface"),
            ("hugging-face", "huggingface"),
            ("nim", "nvidia"),
            ("nvidia-nim", "nvidia"),
            ("lm-studio", "lmstudio"),
            ("deep-seek", "deepseek"),
            ("pplx", "perplexity"),
            ("deep-infra", "deepinfra"),
            ("mistralai", "mistral"),
            ("silicon-flow", "siliconflow"),
            ("zhipu", "zhipuai"),
            ("glm", "zhipuai"),
            ("novita", "novita-ai"),
            ("nanogpt", "nano-gpt"),
        ];
        for &(alias, expected_canonical) in cases {
            let meta = provider_metadata(alias)
                .unwrap_or_else(|| unreachable!("expected metadata for '{alias}'"));
            assert_eq!(
                meta.canonical_id, expected_canonical,
                "alias '{alias}' should resolve to '{expected_canonical}', got '{}'",
                meta.canonical_id
            );
        }
    }

    #[test]
    fn provider_auth_env_keys_support_aliases() {
        assert_eq!(
            provider_auth_env_keys("dashscope"),
            &["DASHSCOPE_API_KEY", "QWEN_API_KEY"]
        );
        assert_eq!(
            provider_auth_env_keys("qwen"),
            &["DASHSCOPE_API_KEY", "QWEN_API_KEY"]
        );
        assert_eq!(
            provider_auth_env_keys("kimi"),
            &["MOONSHOT_API_KEY", "KIMI_API_KEY"]
        );
        assert_eq!(
            provider_auth_env_keys("togetherai"),
            &["TOGETHER_API_KEY", "TOGETHER_AI_API_KEY"]
        );
        assert_eq!(
            provider_auth_env_keys("fireworks-ai"),
            &["FIREWORKS_API_KEY"]
        );
        assert_eq!(
            provider_auth_env_keys("vertexai"),
            &["GOOGLE_CLOUD_API_KEY", "VERTEX_API_KEY"]
        );
        assert_eq!(
            provider_auth_env_keys("bedrock"),
            &[
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_SESSION_TOKEN",
                "AWS_BEARER_TOKEN_BEDROCK",
                "AWS_PROFILE",
                "AWS_REGION",
            ]
        );
        assert_eq!(provider_auth_env_keys("azure"), &["AZURE_OPENAI_API_KEY"]);
        assert_eq!(
            provider_auth_env_keys("azure-cognitive-services"),
            &["AZURE_OPENAI_API_KEY"]
        );
        assert_eq!(
            provider_auth_env_keys("azure-openai-responses"),
            &["AZURE_OPENAI_API_KEY"]
        );
        assert_eq!(
            provider_auth_env_keys("copilot"),
            &["GITHUB_COPILOT_API_KEY", "GITHUB_TOKEN"]
        );
        assert_eq!(
            provider_auth_env_keys("github-copilot-enterprise"),
            &["GITHUB_COPILOT_API_KEY", "GITHUB_TOKEN"]
        );
        assert_eq!(
            provider_auth_env_keys("google-vertex-anthropic"),
            &["GOOGLE_CLOUD_API_KEY", "VERTEX_API_KEY"]
        );
        assert_eq!(
            provider_auth_env_keys("open-router"),
            &["OPENROUTER_API_KEY"]
        );
        assert_eq!(
            provider_auth_env_keys("vercel-ai-gateway"),
            &["AI_GATEWAY_API_KEY"]
        );
        assert_eq!(provider_auth_env_keys("kimi-coding"), &["KIMI_API_KEY"]);
        assert_eq!(provider_auth_env_keys("kimi-code"), &["KIMI_API_KEY"]);
        // New UX aliases resolve to same auth keys as canonical
        assert_eq!(
            provider_auth_env_keys("together"),
            &["TOGETHER_API_KEY", "TOGETHER_AI_API_KEY"]
        );
        assert_eq!(provider_auth_env_keys("grok"), &["XAI_API_KEY"]);
        assert_eq!(provider_auth_env_keys("hf"), &["HF_TOKEN"]);
        assert_eq!(provider_auth_env_keys("nim"), &["NVIDIA_API_KEY"]);
        assert_eq!(provider_auth_env_keys("lm-studio"), &["LMSTUDIO_API_KEY"]);
        assert_eq!(provider_auth_env_keys("deep-seek"), &["DEEPSEEK_API_KEY"]);
        assert_eq!(provider_auth_env_keys("pplx"), &["PERPLEXITY_API_KEY"]);
        assert_eq!(provider_auth_env_keys("deep-infra"), &["DEEPINFRA_API_KEY"]);
        assert_eq!(provider_auth_env_keys("mistralai"), &["MISTRAL_API_KEY"]);
        assert_eq!(
            provider_auth_env_keys("silicon-flow"),
            &["SILICONFLOW_API_KEY"]
        );
    }

    #[test]
    fn provider_auth_env_keys_support_shared_fallbacks() {
        assert_eq!(
            provider_auth_env_keys("google"),
            &["GOOGLE_API_KEY", "GEMINI_API_KEY"]
        );
        assert_eq!(
            provider_auth_env_keys("moonshotai"),
            &["MOONSHOT_API_KEY", "KIMI_API_KEY"]
        );
        assert_eq!(
            provider_auth_env_keys("alibaba"),
            &["DASHSCOPE_API_KEY", "QWEN_API_KEY"]
        );
        assert_eq!(
            provider_auth_env_keys("alibaba-us"),
            &["DASHSCOPE_API_KEY", "QWEN_API_KEY"]
        );
    }

    #[test]
    fn provider_routing_defaults_available_for_openai_compatible_providers() {
        let defaults = provider_routing_defaults("groq").expect("groq defaults");
        assert_eq!(defaults.api, "openai-completions");
        assert!(defaults.auth_header);
        assert!(defaults.base_url.contains("groq"));
    }

    #[test]
    fn provider_routing_defaults_absent_for_native_adapter_only_providers() {
        assert!(provider_routing_defaults("azure-openai").is_none());
    }

    #[test]
    fn provider_routing_defaults_present_for_bedrock_native_adapter() {
        let defaults = provider_routing_defaults("amazon-bedrock").expect("bedrock defaults");
        assert_eq!(defaults.api, "bedrock-converse-stream");
        assert_eq!(defaults.base_url, "");
        assert!(!defaults.auth_header);
    }

    #[test]
    fn cloudflare_metadata_registered_with_openai_compatible_defaults() {
        let gateway =
            provider_metadata("cloudflare-ai-gateway").expect("cloudflare-ai-gateway metadata");
        assert_eq!(
            gateway.onboarding,
            ProviderOnboardingMode::OpenAICompatiblePreset
        );
        let gateway_defaults =
            provider_routing_defaults("cloudflare-ai-gateway").expect("gateway defaults");
        assert_eq!(gateway_defaults.api, "openai-completions");
        assert!(
            gateway_defaults
                .base_url
                .contains("gateway.ai.cloudflare.com")
        );

        let workers =
            provider_metadata("cloudflare-workers-ai").expect("cloudflare-workers-ai metadata");
        assert_eq!(
            workers.onboarding,
            ProviderOnboardingMode::OpenAICompatiblePreset
        );
        let workers_defaults =
            provider_routing_defaults("cloudflare-workers-ai").expect("workers defaults");
        assert_eq!(workers_defaults.api, "openai-completions");
        assert!(
            workers_defaults
                .base_url
                .contains("api.cloudflare.com/client/v4/accounts")
        );

        assert_eq!(
            provider_auth_env_keys("cloudflare-ai-gateway"),
            &["CLOUDFLARE_API_TOKEN"]
        );
        assert_eq!(
            provider_auth_env_keys("cloudflare-workers-ai"),
            &["CLOUDFLARE_API_TOKEN"]
        );
    }

    #[test]
    fn batch_a1_metadata_resolves_all_eight_providers() {
        let ids = [
            "302ai",
            "abacus",
            "aihubmix",
            "bailing",
            "berget",
            "chutes",
            "cortecs",
            "fastrouter",
        ];
        for id in &ids {
            let meta = provider_metadata(id)
                .unwrap_or_else(|| unreachable!("expected metadata for '{id}'"));
            assert_eq!(meta.canonical_id, *id);
            assert_eq!(
                meta.onboarding,
                ProviderOnboardingMode::OpenAICompatiblePreset
            );
        }
    }

    #[test]
    fn batch_a1_env_keys_match_upstream_registry() {
        assert_eq!(provider_auth_env_keys("302ai"), &["302AI_API_KEY"]);
        assert_eq!(provider_auth_env_keys("abacus"), &["ABACUS_API_KEY"]);
        assert_eq!(provider_auth_env_keys("aihubmix"), &["AIHUBMIX_API_KEY"]);
        assert_eq!(provider_auth_env_keys("bailing"), &["BAILING_API_TOKEN"]);
        assert_eq!(provider_auth_env_keys("berget"), &["BERGET_API_KEY"]);
        assert_eq!(provider_auth_env_keys("chutes"), &["CHUTES_API_KEY"]);
        assert_eq!(provider_auth_env_keys("cortecs"), &["CORTECS_API_KEY"]);
        assert_eq!(
            provider_auth_env_keys("fastrouter"),
            &["FASTROUTER_API_KEY"]
        );
    }

    #[test]
    fn batch_a1_routing_defaults_use_openai_completions() {
        let ids = [
            "302ai",
            "abacus",
            "aihubmix",
            "bailing",
            "berget",
            "chutes",
            "cortecs",
            "fastrouter",
        ];
        for id in &ids {
            let defaults = provider_routing_defaults(id)
                .unwrap_or_else(|| unreachable!("expected routing defaults for '{id}'"));
            assert_eq!(defaults.api, "openai-completions", "{id} api mismatch");
            assert!(defaults.auth_header, "{id} must use auth header");
        }
    }

    #[test]
    fn batch_a1_base_urls_are_distinct_and_nonempty() {
        let ids = [
            "302ai",
            "abacus",
            "aihubmix",
            "bailing",
            "berget",
            "chutes",
            "cortecs",
            "fastrouter",
        ];
        let mut urls: Vec<&str> = Vec::new();
        for id in &ids {
            let defaults = provider_routing_defaults(id)
                .unwrap_or_else(|| unreachable!("expected routing defaults for '{id}'"));
            assert!(
                !defaults.base_url.is_empty(),
                "{id} base_url must not be empty"
            );
            assert!(
                defaults.base_url.starts_with("https://"),
                "{id} base_url must use HTTPS"
            );
            urls.push(defaults.base_url);
        }
        // All URLs must be unique.
        urls.sort_unstable();
        urls.dedup();
        assert_eq!(urls.len(), ids.len(), "duplicate base URLs detected");
    }

    #[test]
    fn batch_a2_metadata_resolves_all_active_providers() {
        let ids = [
            "firmware",
            "friendli",
            "helicone",
            "huggingface",
            "iflowcn",
            "inception",
            "inference",
        ];
        for id in &ids {
            let meta = provider_metadata(id)
                .unwrap_or_else(|| unreachable!("expected metadata for '{id}'"));
            assert_eq!(meta.canonical_id, *id);
            assert_eq!(
                meta.onboarding,
                ProviderOnboardingMode::OpenAICompatiblePreset
            );
        }
    }

    #[test]
    fn batch_a2_env_keys_match_upstream_registry() {
        assert_eq!(provider_auth_env_keys("firmware"), &["FIRMWARE_API_KEY"]);
        assert_eq!(provider_auth_env_keys("friendli"), &["FRIENDLI_TOKEN"]);
        assert_eq!(provider_auth_env_keys("helicone"), &["HELICONE_API_KEY"]);
        assert_eq!(provider_auth_env_keys("huggingface"), &["HF_TOKEN"]);
        assert_eq!(provider_auth_env_keys("iflowcn"), &["IFLOW_API_KEY"]);
        assert_eq!(provider_auth_env_keys("inception"), &["INCEPTION_API_KEY"]);
        assert_eq!(provider_auth_env_keys("inference"), &["INFERENCE_API_KEY"]);
    }

    #[test]
    fn batch_a2_routing_defaults_use_openai_completions() {
        let ids = [
            "firmware",
            "friendli",
            "helicone",
            "huggingface",
            "iflowcn",
            "inception",
            "inference",
        ];
        for id in &ids {
            let defaults = provider_routing_defaults(id)
                .unwrap_or_else(|| unreachable!("expected routing defaults for '{id}'"));
            assert_eq!(defaults.api, "openai-completions", "{id} api mismatch");
            assert!(defaults.auth_header, "{id} must use auth header");
        }
    }

    #[test]
    fn batch_a2_base_urls_are_distinct_and_nonempty() {
        let ids = [
            "firmware",
            "friendli",
            "helicone",
            "huggingface",
            "iflowcn",
            "inception",
            "inference",
        ];
        let mut urls: Vec<&str> = Vec::new();
        for id in &ids {
            let defaults = provider_routing_defaults(id)
                .unwrap_or_else(|| unreachable!("expected routing defaults for '{id}'"));
            assert!(
                !defaults.base_url.is_empty(),
                "{id} base_url must not be empty"
            );
            assert!(
                defaults.base_url.starts_with("https://"),
                "{id} base_url must use HTTPS"
            );
            urls.push(defaults.base_url);
        }
        urls.sort_unstable();
        urls.dedup();
        assert_eq!(urls.len(), ids.len(), "duplicate base URLs detected");
    }

    #[test]
    fn retired_github_models_is_not_exposed_but_github_copilot_remains() {
        assert!(provider_metadata("github-models").is_none());
        assert!(provider_routing_defaults("github-models").is_none());
        let copilot = provider_metadata("github-copilot").expect("GitHub Copilot metadata");
        assert_eq!(copilot.canonical_id, "github-copilot");
        assert_eq!(
            copilot.onboarding,
            ProviderOnboardingMode::NativeAdapterRequired
        );
    }

    // ── Batch A3 tests ────────────────────────────────────────────────

    #[test]
    fn batch_a3_metadata_resolves_all_nine_providers() {
        let ids = [
            "io-net",
            "jiekou",
            "lucidquery",
            "moark",
            "morph",
            "nano-gpt",
            "nova",
            "novita-ai",
            "nvidia",
        ];
        for id in &ids {
            let meta = provider_metadata(id)
                .unwrap_or_else(|| unreachable!("expected metadata for '{id}'"));
            assert_eq!(meta.canonical_id, *id);
            assert_eq!(
                meta.onboarding,
                ProviderOnboardingMode::OpenAICompatiblePreset,
                "{id} onboarding mode mismatch"
            );
        }
    }

    #[test]
    fn batch_a3_env_keys_match_upstream_registry() {
        assert_eq!(
            provider_metadata("io-net").unwrap().auth_env_keys,
            &["IOINTELLIGENCE_API_KEY"]
        );
        assert_eq!(
            provider_metadata("jiekou").unwrap().auth_env_keys,
            &["JIEKOU_API_KEY"]
        );
        assert_eq!(
            provider_metadata("lucidquery").unwrap().auth_env_keys,
            &["LUCIDQUERY_API_KEY"]
        );
        assert_eq!(
            provider_metadata("moark").unwrap().auth_env_keys,
            &["MOARK_API_KEY"]
        );
        assert_eq!(
            provider_metadata("morph").unwrap().auth_env_keys,
            &["MORPH_API_KEY"]
        );
        assert_eq!(
            provider_metadata("nano-gpt").unwrap().auth_env_keys,
            &["NANO_GPT_API_KEY"]
        );
        assert_eq!(
            provider_metadata("nova").unwrap().auth_env_keys,
            &["NOVA_API_KEY"]
        );
        assert_eq!(
            provider_metadata("novita-ai").unwrap().auth_env_keys,
            &["NOVITA_API_KEY"]
        );
        assert_eq!(
            provider_metadata("nvidia").unwrap().auth_env_keys,
            &["NVIDIA_API_KEY"]
        );
    }

    #[test]
    fn batch_a3_routing_defaults_use_openai_completions() {
        let ids = [
            "io-net",
            "jiekou",
            "lucidquery",
            "moark",
            "morph",
            "nano-gpt",
            "nova",
            "novita-ai",
            "nvidia",
        ];
        for id in &ids {
            let defaults = provider_routing_defaults(id)
                .unwrap_or_else(|| unreachable!("expected routing defaults for '{id}'"));
            assert_eq!(
                defaults.api, "openai-completions",
                "{id} api should be openai-completions"
            );
            assert!(defaults.auth_header, "{id} auth_header should be true");
        }
    }

    #[test]
    fn batch_a3_base_urls_are_distinct_and_nonempty() {
        let ids = [
            "io-net",
            "jiekou",
            "lucidquery",
            "moark",
            "morph",
            "nano-gpt",
            "nova",
            "novita-ai",
            "nvidia",
        ];
        let mut urls: Vec<&str> = Vec::new();
        for id in &ids {
            let defaults = provider_routing_defaults(id)
                .unwrap_or_else(|| unreachable!("expected routing defaults for '{id}'"));
            assert!(
                !defaults.base_url.is_empty(),
                "{id} base_url must not be empty"
            );
            assert!(
                defaults.base_url.starts_with("https://"),
                "{id} base_url must use HTTPS"
            );
            urls.push(defaults.base_url);
        }
        urls.sort_unstable();
        urls.dedup();
        assert_eq!(urls.len(), ids.len(), "duplicate base URLs detected");
    }

    #[test]
    fn fireworks_ai_alias_already_registered() {
        // fireworks-ai is listed in Batch A2 bead but already exists as alias
        // for the "fireworks" canonical entry from the initial metadata set.
        let meta = provider_metadata("fireworks-ai").expect("fireworks-ai alias");
        assert_eq!(meta.canonical_id, "fireworks");
    }

    // ── Batch A4 tests ────────────────────────────────────────────────

    #[test]
    fn batch_a4_metadata_resolves_all_nine_providers() {
        let ids = [
            "poe",
            "privatemode-ai",
            "requesty",
            "submodel",
            "synthetic",
            "vivgrid",
            "vultr",
            "wandb",
            "xiaomi",
        ];
        for id in &ids {
            let meta = provider_metadata(id)
                .unwrap_or_else(|| unreachable!("expected metadata for '{id}'"));
            assert_eq!(meta.canonical_id, *id);
            assert_eq!(
                meta.onboarding,
                ProviderOnboardingMode::OpenAICompatiblePreset,
                "{id} onboarding mode mismatch"
            );
        }
    }

    #[test]
    fn batch_a4_env_keys_match_upstream_registry() {
        assert_eq!(
            provider_metadata("poe").unwrap().auth_env_keys,
            &["POE_API_KEY"]
        );
        assert_eq!(
            provider_metadata("privatemode-ai").unwrap().auth_env_keys,
            &["PRIVATEMODE_API_KEY"]
        );
        assert_eq!(
            provider_metadata("requesty").unwrap().auth_env_keys,
            &["REQUESTY_API_KEY"]
        );
        assert_eq!(
            provider_metadata("submodel").unwrap().auth_env_keys,
            &["SUBMODEL_INSTAGEN_ACCESS_KEY"]
        );
        assert_eq!(
            provider_metadata("synthetic").unwrap().auth_env_keys,
            &["SYNTHETIC_API_KEY"]
        );
        assert_eq!(
            provider_metadata("vivgrid").unwrap().auth_env_keys,
            &["VIVGRID_API_KEY"]
        );
        assert_eq!(
            provider_metadata("vultr").unwrap().auth_env_keys,
            &["VULTR_API_KEY"]
        );
        assert_eq!(
            provider_metadata("wandb").unwrap().auth_env_keys,
            &["WANDB_API_KEY"]
        );
        assert_eq!(
            provider_metadata("xiaomi").unwrap().auth_env_keys,
            &["XIAOMI_API_KEY"]
        );
    }

    #[test]
    fn batch_a4_routing_defaults_use_openai_completions() {
        let ids = [
            "poe",
            "privatemode-ai",
            "requesty",
            "submodel",
            "synthetic",
            "vivgrid",
            "vultr",
            "wandb",
            "xiaomi",
        ];
        for id in &ids {
            let defaults = provider_routing_defaults(id)
                .unwrap_or_else(|| unreachable!("expected routing defaults for '{id}'"));
            assert_eq!(
                defaults.api, "openai-completions",
                "{id} api should be openai-completions"
            );
            assert!(defaults.auth_header, "{id} auth_header should be true");
        }
    }

    #[test]
    fn batch_a4_base_urls_are_distinct_and_nonempty() {
        let ids = [
            "poe",
            "privatemode-ai",
            "requesty",
            "submodel",
            "synthetic",
            "vivgrid",
            "vultr",
            "wandb",
            "xiaomi",
        ];
        let mut urls: Vec<&str> = Vec::new();
        for id in &ids {
            let defaults = provider_routing_defaults(id)
                .unwrap_or_else(|| unreachable!("expected routing defaults for '{id}'"));
            assert!(
                !defaults.base_url.is_empty(),
                "{id} base_url must not be empty"
            );
            // privatemode-ai uses localhost (self-hosted); all others use HTTPS.
            if *id != "privatemode-ai" {
                assert!(
                    defaults.base_url.starts_with("https://"),
                    "{id} base_url must use HTTPS"
                );
            }
            urls.push(defaults.base_url);
        }
        urls.sort_unstable();
        urls.dedup();
        assert_eq!(urls.len(), ids.len(), "duplicate base URLs detected");
    }

    #[test]
    fn batch_b1_metadata_resolves_all_seven_providers() {
        let ids = [
            "alibaba-cn",
            "alibaba-us",
            "kimi-for-coding",
            "minimax",
            "minimax-cn",
            "minimax-coding-plan",
            "minimax-cn-coding-plan",
        ];
        for id in &ids {
            let meta = provider_metadata(id)
                .unwrap_or_else(|| unreachable!("expected metadata for '{id}'"));
            assert_eq!(meta.canonical_id, *id);
            assert_eq!(
                meta.onboarding,
                ProviderOnboardingMode::OpenAICompatiblePreset
            );
        }
    }

    #[test]
    fn batch_b1_env_keys_match_expected_families() {
        assert_eq!(
            provider_metadata("alibaba-cn").unwrap().auth_env_keys,
            &["DASHSCOPE_API_KEY"]
        );
        assert_eq!(
            provider_metadata("alibaba-us").unwrap().auth_env_keys,
            &["DASHSCOPE_API_KEY", "QWEN_API_KEY"]
        );
        assert_eq!(
            provider_metadata("kimi-for-coding").unwrap().auth_env_keys,
            &["KIMI_API_KEY"]
        );
        assert_eq!(
            provider_metadata("minimax").unwrap().auth_env_keys,
            &["MINIMAX_API_KEY"]
        );
        assert_eq!(
            provider_metadata("minimax-cn").unwrap().auth_env_keys,
            &["MINIMAX_CN_API_KEY"]
        );
        assert_eq!(
            provider_metadata("minimax-coding-plan")
                .unwrap()
                .auth_env_keys,
            &["MINIMAX_API_KEY"]
        );
        assert_eq!(
            provider_metadata("minimax-cn-coding-plan")
                .unwrap()
                .auth_env_keys,
            &["MINIMAX_CN_API_KEY"]
        );
    }

    #[test]
    fn batch_b1_routing_defaults_match_expected_api_families() {
        let alibaba_cn = provider_routing_defaults("alibaba-cn").expect("alibaba-cn defaults");
        assert_eq!(alibaba_cn.api, "openai-completions");
        assert!(alibaba_cn.auth_header);
        assert!(alibaba_cn.base_url.contains("dashscope.aliyuncs.com"));

        let alibaba_us = provider_routing_defaults("alibaba-us").expect("alibaba-us defaults");
        assert_eq!(alibaba_us.api, "openai-completions");
        assert!(alibaba_us.auth_header);
        assert!(alibaba_us.base_url.contains("dashscope-us.aliyuncs.com"));

        let kimi = provider_routing_defaults("kimi-for-coding").expect("kimi-for-coding defaults");
        assert_eq!(kimi.api, "anthropic-messages");
        assert!(!kimi.auth_header);
        assert!(kimi.base_url.contains("api.kimi.com/coding"));

        for id in [
            "minimax",
            "minimax-cn",
            "minimax-coding-plan",
            "minimax-cn-coding-plan",
        ] {
            let defaults = provider_routing_defaults(id)
                .unwrap_or_else(|| unreachable!("expected routing defaults for '{id}'"));
            assert_eq!(defaults.api, "anthropic-messages");
            assert!(!defaults.auth_header);
        }
    }

    #[test]
    fn batch_b1_family_coherence_is_explicit() {
        let alibaba_global = provider_routing_defaults("alibaba").expect("alibaba defaults");
        let alibaba_cn = provider_routing_defaults("alibaba-cn").expect("alibaba-cn defaults");
        let alibaba_us = provider_routing_defaults("alibaba-us").expect("alibaba-us defaults");
        assert_eq!(alibaba_global.api, "openai-completions");
        assert_eq!(alibaba_cn.api, "openai-completions");
        assert_eq!(alibaba_us.api, "openai-completions");
        assert_ne!(alibaba_global.base_url, alibaba_cn.base_url);
        assert_ne!(alibaba_global.base_url, alibaba_us.base_url);
        assert_ne!(alibaba_cn.base_url, alibaba_us.base_url);

        let kimi_alias = canonical_provider_id("kimi").expect("kimi alias");
        let kimi_coding = canonical_provider_id("kimi-for-coding").expect("kimi-for-coding");
        let kimi_coding_legacy =
            canonical_provider_id("kimi-coding").expect("kimi-coding legacy alias");
        let kimi_code_alias = canonical_provider_id("kimi-code").expect("kimi-code alias");
        assert_eq!(kimi_alias, "moonshotai");
        assert_eq!(kimi_coding, "kimi-for-coding");
        assert_eq!(kimi_coding_legacy, "kimi-for-coding");
        assert_eq!(kimi_code_alias, "kimi-for-coding");

        let minimax = provider_routing_defaults("minimax").expect("minimax defaults");
        let minimax_cp =
            provider_routing_defaults("minimax-coding-plan").expect("minimax-coding-plan");
        assert_eq!(minimax.base_url, minimax_cp.base_url);

        let minimax_cn = provider_routing_defaults("minimax-cn").expect("minimax-cn defaults");
        let minimax_cn_cp = provider_routing_defaults("minimax-cn-coding-plan")
            .expect("minimax-cn-coding-plan defaults");
        assert_eq!(minimax_cn.base_url, minimax_cn_cp.base_url);
        assert_ne!(minimax.base_url, minimax_cn.base_url);
    }

    #[test]
    fn batch_b2_metadata_resolves_all_six_providers() {
        let ids = [
            "modelscope",
            "moonshotai-cn",
            "nebius",
            "ovhcloud",
            "scaleway",
            "stackit",
        ];
        for id in &ids {
            let meta = provider_metadata(id)
                .unwrap_or_else(|| unreachable!("expected metadata for '{id}'"));
            assert_eq!(meta.canonical_id, *id);
            assert_eq!(
                meta.onboarding,
                ProviderOnboardingMode::OpenAICompatiblePreset
            );
        }
    }

    #[test]
    fn batch_b2_env_keys_match_expected() {
        assert_eq!(
            provider_metadata("modelscope").unwrap().auth_env_keys,
            &["MODELSCOPE_API_KEY"]
        );
        assert_eq!(
            provider_metadata("moonshotai-cn").unwrap().auth_env_keys,
            &["MOONSHOT_API_KEY"]
        );
        assert_eq!(
            provider_metadata("nebius").unwrap().auth_env_keys,
            &["NEBIUS_API_KEY"]
        );
        assert_eq!(
            provider_metadata("ovhcloud").unwrap().auth_env_keys,
            &["OVHCLOUD_API_KEY"]
        );
        assert_eq!(
            provider_metadata("scaleway").unwrap().auth_env_keys,
            &["SCALEWAY_API_KEY"]
        );
        assert_eq!(
            provider_metadata("stackit").unwrap().auth_env_keys,
            &["STACKIT_API_KEY"]
        );
    }

    #[test]
    fn batch_b2_routing_defaults_use_openai_completions_and_bearer_auth() {
        let ids = [
            ("modelscope", "api-inference.modelscope.cn"),
            ("moonshotai-cn", "api.moonshot.cn"),
            ("nebius", "api.tokenfactory.nebius.com"),
            ("ovhcloud", "oai.endpoints.kepler.ai.cloud.ovh.net"),
            ("scaleway", "api.scaleway.ai"),
            (
                "stackit",
                "api.openai-compat.model-serving.eu01.onstackit.cloud",
            ),
        ];
        for (id, expected_host) in &ids {
            let defaults = provider_routing_defaults(id)
                .unwrap_or_else(|| unreachable!("expected routing defaults for '{id}'"));
            assert_eq!(defaults.api, "openai-completions");
            assert!(defaults.auth_header);
            assert!(defaults.base_url.contains(expected_host));
        }
    }

    #[test]
    fn batch_b2_moonshot_cn_and_global_moonshot_stay_distinct() {
        let moonshot_global =
            provider_routing_defaults("moonshotai").expect("moonshotai defaults missing");
        let moonshot_cn =
            provider_routing_defaults("moonshotai-cn").expect("moonshotai-cn defaults missing");

        assert_eq!(canonical_provider_id("moonshot"), Some("moonshotai"));
        assert_eq!(
            canonical_provider_id("moonshotai-cn"),
            Some("moonshotai-cn")
        );
        assert_eq!(
            provider_auth_env_keys("moonshotai"),
            &["MOONSHOT_API_KEY", "KIMI_API_KEY"]
        );
        assert_eq!(
            provider_auth_env_keys("moonshotai-cn"),
            &["MOONSHOT_API_KEY"]
        );
        assert_eq!(moonshot_global.api, "openai-completions");
        assert_eq!(moonshot_cn.api, "openai-completions");
        assert_ne!(moonshot_global.base_url, moonshot_cn.base_url);
    }

    #[test]
    fn batch_b3_metadata_resolves_all_eight_providers() {
        let ids = [
            "siliconflow",
            "siliconflow-cn",
            "upstage",
            "venice",
            "zai",
            "zai-coding-plan",
            "zhipuai",
            "zhipuai-coding-plan",
        ];
        for id in &ids {
            let meta = provider_metadata(id)
                .unwrap_or_else(|| unreachable!("expected metadata for '{id}'"));
            assert_eq!(meta.canonical_id, *id);
            assert_eq!(
                meta.onboarding,
                ProviderOnboardingMode::OpenAICompatiblePreset
            );
        }
    }

    #[test]
    fn batch_b3_env_keys_match_expected() {
        assert_eq!(
            provider_metadata("siliconflow").unwrap().auth_env_keys,
            &["SILICONFLOW_API_KEY"]
        );
        assert_eq!(
            provider_metadata("siliconflow-cn").unwrap().auth_env_keys,
            &["SILICONFLOW_CN_API_KEY"]
        );
        assert_eq!(
            provider_metadata("upstage").unwrap().auth_env_keys,
            &["UPSTAGE_API_KEY"]
        );
        assert_eq!(
            provider_metadata("venice").unwrap().auth_env_keys,
            &["VENICE_API_KEY"]
        );
        assert_eq!(
            provider_metadata("zai").unwrap().auth_env_keys,
            &["ZHIPU_API_KEY"]
        );
        assert_eq!(
            provider_metadata("zai-coding-plan").unwrap().auth_env_keys,
            &["ZHIPU_API_KEY"]
        );
        assert_eq!(
            provider_metadata("zhipuai").unwrap().auth_env_keys,
            &["ZHIPU_API_KEY"]
        );
        assert_eq!(
            provider_metadata("zhipuai-coding-plan")
                .unwrap()
                .auth_env_keys,
            &["ZHIPU_API_KEY"]
        );
    }

    #[test]
    fn batch_b3_routing_defaults_use_openai_completions_and_bearer_auth() {
        let ids = [
            ("siliconflow", "api.siliconflow.com"),
            ("siliconflow-cn", "api.siliconflow.cn"),
            ("upstage", "api.upstage.ai"),
            ("venice", "api.venice.ai"),
            ("zai", "api.z.ai"),
            ("zai-coding-plan", "api.z.ai"),
            ("zhipuai", "open.bigmodel.cn"),
            ("zhipuai-coding-plan", "open.bigmodel.cn"),
        ];
        for (id, expected_host) in &ids {
            let defaults = provider_routing_defaults(id)
                .unwrap_or_else(|| unreachable!("expected routing defaults for '{id}'"));
            assert_eq!(defaults.api, "openai-completions");
            assert!(defaults.auth_header);
            assert!(defaults.base_url.contains(expected_host));
        }
    }

    #[test]
    fn batch_b3_coding_plan_variants_keep_family_auth_but_distinct_base_urls() {
        let zai = provider_routing_defaults("zai").expect("zai defaults");
        let zai_coding = provider_routing_defaults("zai-coding-plan").expect("zai-coding defaults");
        assert_eq!(zai.api, "openai-completions");
        assert_eq!(zai_coding.api, "openai-completions");
        assert_ne!(zai.base_url, zai_coding.base_url);

        let zhipu = provider_routing_defaults("zhipuai").expect("zhipu defaults");
        let zhipu_coding =
            provider_routing_defaults("zhipuai-coding-plan").expect("zhipu-coding defaults");
        assert_eq!(zhipu.api, "openai-completions");
        assert_eq!(zhipu_coding.api, "openai-completions");
        assert_ne!(zhipu.base_url, zhipu_coding.base_url);

        assert_eq!(provider_auth_env_keys("zai"), &["ZHIPU_API_KEY"]);
        assert_eq!(provider_auth_env_keys("zhipuai"), &["ZHIPU_API_KEY"]);
    }

    #[test]
    fn batch_c1_metadata_resolves_all_five_providers() {
        let ids = ["baseten", "llama", "lmstudio", "ollama", "ollama-cloud"];
        for id in &ids {
            let meta = provider_metadata(id)
                .unwrap_or_else(|| unreachable!("expected metadata for '{id}'"));
            assert_eq!(meta.canonical_id, *id);
            assert_eq!(
                meta.onboarding,
                ProviderOnboardingMode::OpenAICompatiblePreset
            );
        }
    }

    #[test]
    fn batch_c1_env_keys_match_expected() {
        assert_eq!(
            provider_metadata("baseten").unwrap().auth_env_keys,
            &["BASETEN_API_KEY"]
        );
        assert_eq!(
            provider_metadata("llama").unwrap().auth_env_keys,
            &["LLAMA_API_KEY"]
        );
        assert_eq!(
            provider_metadata("lmstudio").unwrap().auth_env_keys,
            &["LMSTUDIO_API_KEY"]
        );
        assert!(
            provider_metadata("ollama")
                .unwrap()
                .auth_env_keys
                .is_empty()
        );
        assert_eq!(
            provider_metadata("ollama-cloud").unwrap().auth_env_keys,
            &["OLLAMA_API_KEY"]
        );
    }

    #[test]
    fn local_keyless_providers_registered_without_auth() {
        // #104: llamacpp and mistralrs are local OpenAI-compatible providers
        // with NO API key, exactly like ollama. They must be keyless (empty
        // auth_env_keys) and route with auth_header == false so the request
        // path does not demand a bearer token.
        for (id, base_url) in [
            ("llamacpp", "http://127.0.0.1:8080/v1"),
            ("mistralrs", "http://127.0.0.1:1234/v1"),
        ] {
            let meta = provider_metadata(id)
                .unwrap_or_else(|| unreachable!("expected metadata for '{id}'"));
            assert_eq!(meta.canonical_id, id);
            assert!(
                meta.auth_env_keys.is_empty(),
                "{id} must declare no auth env keys"
            );
            let defaults = provider_routing_defaults(id)
                .unwrap_or_else(|| unreachable!("expected routing defaults for '{id}'"));
            assert_eq!(defaults.api, "openai-completions");
            assert!(
                !defaults.auth_header,
                "{id} must not require an auth header"
            );
            assert_eq!(defaults.base_url, base_url);
        }
    }

    #[test]
    fn local_keyless_provider_aliases_resolve() {
        // #104: the common spellings users reach for must resolve to the
        // canonical local provider so `--provider llama.cpp` / `mistral.rs` work.
        for (alias, canonical) in [
            ("llama-cpp", "llamacpp"),
            ("llama.cpp", "llamacpp"),
            ("llama-server", "llamacpp"),
            ("mistral.rs", "mistralrs"),
            ("mistral-rs", "mistralrs"),
        ] {
            let meta = provider_metadata(alias)
                .unwrap_or_else(|| unreachable!("expected metadata for alias '{alias}'"));
            assert_eq!(
                meta.canonical_id, canonical,
                "alias '{alias}' should resolve to '{canonical}'"
            );
        }
    }

    #[test]
    fn keyless_local_predicate_matches_local_providers_only() {
        // #104: the predicate that lets the OpenAI-completions request path skip
        // the missing-API-key error must hold for ollama + llamacpp + mistralrs
        // (and only for genuinely keyless local providers).
        for id in ["ollama", "llamacpp", "mistralrs"] {
            assert!(
                provider_is_keyless_local(id),
                "{id} should be recognised as a keyless local provider"
            );
        }
        // Cloud / keyed providers must still require credentials.
        for id in [
            "openai",
            "anthropic",
            "groq",
            "ollama-cloud", // cloud variant DOES need OLLAMA_API_KEY
            "lmstudio",     // declares LMSTUDIO_API_KEY + auth_header true
        ] {
            assert!(
                !provider_is_keyless_local(id),
                "{id} must NOT be treated as keyless local"
            );
        }
        // Unknown providers are not keyless-local (they fall back to the
        // generic key-required gate).
        assert!(!provider_is_keyless_local("definitely-not-a-provider"));
    }

    #[test]
    fn batch_c1_routing_defaults_use_openai_completions_with_expected_endpoints() {
        let ids = [
            ("baseten", "https://inference.baseten.co/v1", true),
            ("llama", "https://api.llama.com/compat/v1", true),
            ("lmstudio", "http://127.0.0.1:1234/v1", true),
            ("ollama", "http://127.0.0.1:11434/v1", false),
            ("ollama-cloud", "https://ollama.com/v1", true),
        ];
        for (id, expected_base_url, expected_auth_header) in &ids {
            let defaults = provider_routing_defaults(id)
                .unwrap_or_else(|| unreachable!("expected routing defaults for '{id}'"));
            assert_eq!(defaults.api, "openai-completions");
            assert_eq!(defaults.auth_header, *expected_auth_header);
            assert_eq!(defaults.base_url, *expected_base_url);
        }
    }

    #[test]
    fn special_routing_metadata_resolves_all_three_providers() {
        let ids = ["opencode", "vercel", "zenmux"];
        for id in &ids {
            let meta = provider_metadata(id)
                .unwrap_or_else(|| unreachable!("expected metadata for '{id}'"));
            assert_eq!(meta.canonical_id, *id);
            assert_eq!(
                meta.onboarding,
                ProviderOnboardingMode::OpenAICompatiblePreset
            );
        }
    }

    #[test]
    fn special_routing_env_keys_match_expected() {
        assert_eq!(
            provider_metadata("opencode").unwrap().auth_env_keys,
            &["OPENCODE_API_KEY"]
        );
        assert_eq!(
            provider_metadata("vercel").unwrap().auth_env_keys,
            &["AI_GATEWAY_API_KEY"]
        );
        assert_eq!(
            provider_metadata("zenmux").unwrap().auth_env_keys,
            &["ZENMUX_API_KEY"]
        );
    }

    #[test]
    fn special_routing_defaults_match_expected_api_families() {
        let opencode = provider_routing_defaults("opencode").expect("opencode defaults");
        assert_eq!(opencode.api, "openai-completions");
        assert_eq!(opencode.base_url, "https://opencode.ai/zen/v1");
        assert!(opencode.auth_header);

        let vercel = provider_routing_defaults("vercel").expect("vercel defaults");
        assert_eq!(vercel.api, "openai-completions");
        assert_eq!(vercel.base_url, "https://ai-gateway.vercel.sh/v1");
        assert!(vercel.auth_header);
        let vercel_alias =
            provider_routing_defaults("vercel-ai-gateway").expect("vercel alias defaults");
        assert_eq!(vercel_alias.api, "openai-completions");
        assert_eq!(vercel_alias.base_url, "https://ai-gateway.vercel.sh/v1");
        assert!(vercel_alias.auth_header);

        let zenmux = provider_routing_defaults("zenmux").expect("zenmux defaults");
        assert_eq!(zenmux.api, "anthropic-messages");
        assert_eq!(
            zenmux.base_url,
            "https://zenmux.ai/api/anthropic/v1/messages"
        );
        assert!(!zenmux.auth_header);
    }
    #[test]
    fn v0_registered_as_native_adapter_required_without_routing_defaults() {
        let meta = provider_metadata("v0").expect("v0 metadata");
        assert_eq!(meta.canonical_id, "v0");
        assert_eq!(
            meta.onboarding,
            ProviderOnboardingMode::NativeAdapterRequired
        );
        assert_eq!(meta.auth_env_keys, &["V0_API_KEY"]);
        assert!(provider_routing_defaults("v0").is_none());
    }

    #[test]
    fn display_name_populated_for_major_providers() {
        let cases: &[(&str, &str)] = &[
            ("anthropic", "Anthropic"),
            ("openai", "OpenAI"),
            ("google", "Google Gemini"),
            ("xai", "xAI (Grok)"),
            ("togetherai", "Together AI"),
            ("huggingface", "Hugging Face"),
            ("nvidia", "NVIDIA NIM"),
            ("amazon-bedrock", "Amazon Bedrock"),
            ("azure-openai", "Azure OpenAI"),
            ("github-copilot", "GitHub Copilot"),
            ("google-vertex", "Google Vertex AI"),
            ("deepseek", "DeepSeek"),
            ("mistral", "Mistral AI"),
            ("lmstudio", "LM Studio"),
        ];
        for &(id, expected_name) in cases {
            let meta = provider_metadata(id)
                .unwrap_or_else(|| unreachable!("expected metadata for '{id}'"));
            assert_eq!(
                meta.display_name,
                Some(expected_name),
                "display_name for '{id}' should be '{expected_name}'"
            );
        }
    }

    #[test]
    fn all_providers_have_display_name() {
        for meta in PROVIDER_METADATA {
            assert!(
                meta.display_name.is_some(),
                "provider '{}' should have a display_name",
                meta.canonical_id
            );
        }
    }

    mod proptest_provider_metadata {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// `provider_metadata` never panics on arbitrary input.
            #[test]
            fn provider_metadata_never_panics(s in ".*") {
                let _ = provider_metadata(&s);
            }

            /// Empty string always returns None.
            #[test]
            fn provider_metadata_empty_returns_none(_dummy in 0..10u32) {
                assert!(provider_metadata("").is_none());
                assert!(canonical_provider_id("").is_none());
                assert!(provider_auth_env_keys("").is_empty());
                assert!(provider_routing_defaults("").is_none());
            }

            /// All canonical IDs resolve to themselves.
            #[test]
            fn canonical_ids_resolve_to_self(idx in 0..PROVIDER_METADATA.len()) {
                let meta = &PROVIDER_METADATA[idx];
                let resolved = canonical_provider_id(meta.canonical_id);
                assert_eq!(resolved, Some(meta.canonical_id));
            }

            /// Lookup is case-insensitive for all canonical IDs.
            #[test]
            fn case_insensitive_lookup(idx in 0..PROVIDER_METADATA.len()) {
                let meta = &PROVIDER_METADATA[idx];
                let upper = provider_metadata(&meta.canonical_id.to_uppercase());
                let lower = provider_metadata(&meta.canonical_id.to_lowercase());
                assert!(upper.is_some());
                assert!(lower.is_some());
                assert_eq!(upper.unwrap().canonical_id, lower.unwrap().canonical_id);
            }

            /// All aliases resolve to the same canonical ID.
            #[test]
            fn aliases_resolve_to_canonical(idx in 0..PROVIDER_METADATA.len()) {
                let meta = &PROVIDER_METADATA[idx];
                for alias in meta.aliases {
                    let resolved = canonical_provider_id(alias);
                    assert_eq!(
                        resolved,
                        Some(meta.canonical_id),
                        "alias '{alias}' should resolve to '{}'",
                        meta.canonical_id
                    );
                }
            }

            /// `canonical_provider_id` is idempotent.
            #[test]
            fn canonical_id_is_idempotent(idx in 0..PROVIDER_METADATA.len()) {
                let meta = &PROVIDER_METADATA[idx];
                let first = canonical_provider_id(meta.canonical_id).unwrap();
                let second = canonical_provider_id(first).unwrap();
                assert_eq!(first, second);
            }

            /// Unknown providers return None.
            #[test]
            fn unknown_provider_returns_none(s in "[a-z]{20,30}") {
                // 20+ char lowercase strings won't match any provider
                assert!(provider_metadata(&s).is_none());
                assert!(canonical_provider_id(&s).is_none());
                assert!(provider_auth_env_keys(&s).is_empty());
                assert!(provider_routing_defaults(&s).is_none());
            }

            /// Auth env keys (when present) are valid environment variable names.
            #[test]
            fn auth_env_keys_are_valid_env_vars(idx in 0..PROVIDER_METADATA.len()) {
                let meta = &PROVIDER_METADATA[idx];
                let keys = provider_auth_env_keys(meta.canonical_id);
                // Some providers (e.g. ollama) have no auth keys — that's valid.
                for &key in keys {
                    assert!(!key.is_empty());
                    assert!(
                        key.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
                        "invalid env var name: {key}"
                    );
                }
            }

            /// `provider_auth_env_keys` matches `provider_metadata().auth_env_keys`.
            #[test]
            fn auth_keys_consistent_with_metadata(idx in 0..PROVIDER_METADATA.len()) {
                let meta = &PROVIDER_METADATA[idx];
                let keys = provider_auth_env_keys(meta.canonical_id);
                assert_eq!(keys, meta.auth_env_keys);
            }

            /// `provider_routing_defaults` matches `provider_metadata().routing_defaults`.
            #[test]
            fn routing_defaults_consistent_with_metadata(idx in 0..PROVIDER_METADATA.len()) {
                let meta = &PROVIDER_METADATA[idx];
                let defaults = provider_routing_defaults(meta.canonical_id);
                match (defaults, meta.routing_defaults) {
                    (Some(d), Some(m)) => {
                        assert_eq!(d.base_url, m.base_url);
                        assert_eq!(d.api, m.api);
                    }
                    (None, None) => {}
                    _ => assert!(false,
                        "mismatch for '{}': fn={:?} meta={:?}",
                        meta.canonical_id, defaults, meta.routing_defaults
                    ),
                }
            }
        }
    }
}
