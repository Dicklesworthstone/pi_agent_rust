//! Model-specific Converse request controls.
//!
//! Thinking belongs in `additionalModelRequestFields`, while prompt caching
//! uses Converse's standalone `cachePoint` blocks, not Anthropic `cache_control`.
//! Apply these controls before the request hook and SigV4 serialization.
//! Sources:
//! https://docs.aws.amazon.com/bedrock/latest/userguide/claude-messages-adaptive-thinking.html
//! https://docs.aws.amazon.com/bedrock/latest/userguide/claude-messages-extended-thinking.html
//! https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html

use super::BedrockProvider;
use crate::error::{Error, Result};
use crate::model::ThinkingLevel;
use crate::models::CompatConfig;
use crate::provider::{CacheRetention, Context, StreamOptions};
use serde_json::{Map, Value, json};

const MIN_THINKING_TOKENS: u32 = 1024;
const DEFAULT_ANSWER_TOKENS: u32 = 4096;
const DEFAULT_MAX_TOKENS: u32 = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThinkingMode {
    Unsupported,
    Budget,
    Adaptive,
}

/// Build the actual model-aware request without changing persisted messages.
/// Opaque application inference-profile ARNs can select a thinking dialect via
/// `forceAdaptiveThinking`; they do not implicitly opt into Claude caching.
pub(super) fn prepare(
    model: &str,
    compat: Option<&CompatConfig>,
    context: &Context<'_>,
    options: &StreamOptions,
) -> Result<Value> {
    let mut body = serde_json::to_value(BedrockProvider::build_request(context, options))?;
    let id = claude_model_id(model);
    let mode = compat
        .and_then(|config| config.force_adaptive_thinking)
        .map_or_else(
            || thinking_mode(id.as_deref()),
            |adaptive| {
                if adaptive {
                    ThinkingMode::Adaptive
                } else {
                    ThinkingMode::Budget
                }
            },
        );
    apply_thinking(&mut body, id.as_deref(), mode, compat, options)?;
    if let Some(id) = id.as_deref() {
        apply_caching(&mut body, id, options.cache_retention);
    }
    Ok(body)
}

fn request_error(message: &str) -> Error {
    Error::provider("amazon-bedrock", message)
}

fn claude_model_id(model: &str) -> Option<String> {
    // Foundation-model and system inference-profile ARNs retain the model in
    // their final path component. Never infer a model from an opaque app ID.
    let resource = model.rsplit('/').next()?.to_ascii_lowercase();
    let resource = ["us.", "eu.", "apac.", "global."]
        .iter()
        .find_map(|prefix| resource.strip_prefix(*prefix))
        .unwrap_or(&resource);
    let id = resource.strip_prefix("anthropic.").unwrap_or(resource);
    id.starts_with("claude-").then(|| id.to_string())
}

fn family(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.is_empty() || suffix.starts_with('-') || suffix.starts_with(':')
    })
}

fn adaptive_model(id: &str) -> bool {
    [
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-opus-5",
        "claude-sonnet-4-6",
        "claude-sonnet-5",
        "claude-fable-5",
        "claude-mythos-5",
        "claude-mythos-preview",
    ]
    .iter()
    .any(|prefix| family(id, prefix))
}

fn thinking_mode(id: Option<&str>) -> ThinkingMode {
    let Some(id) = id else {
        return ThinkingMode::Unsupported;
    };
    if adaptive_model(id) {
        return ThinkingMode::Adaptive;
    }
    if [
        "claude-3-7-sonnet",
        "claude-sonnet-4-20250514",
        "claude-sonnet-4-5",
        "claude-opus-4-20250514",
        "claude-opus-4-1",
        "claude-opus-4-5",
        "claude-haiku-4-5",
    ]
    .iter()
    .any(|prefix| family(id, prefix))
        || matches!(id, "claude-sonnet-4" | "claude-opus-4")
    {
        ThinkingMode::Budget
    } else {
        ThinkingMode::Unsupported
    }
}

fn thinking_is_required(id: &str) -> bool {
    ["claude-fable-5", "claude-mythos-5", "claude-mythos-preview"]
        .iter()
        .any(|prefix| family(id, prefix))
}

fn effort(level: ThinkingLevel, id: Option<&str>, compat: Option<&CompatConfig>) -> Result<String> {
    if let Some(mapped) = compat
        .and_then(|config| config.thinking_level_map.as_ref())
        .and_then(|map| map.get(&level.to_string()))
    {
        let Some(mapped) = mapped else {
            return Err(request_error(
                "Bedrock thinkingLevelMap disables the requested effort",
            ));
        };
        if matches!(mapped.as_str(), "low" | "medium" | "high" | "xhigh" | "max") {
            return Ok(mapped.clone());
        }
        return Err(request_error(
            "Bedrock thinkingLevelMap must map enabled levels to low, medium, high, xhigh, or max",
        ));
    }
    let value = match level {
        ThinkingLevel::Off => {
            return Err(request_error("Cannot assign effort to disabled thinking"));
        }
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        // 4.6 and Mythos Preview lack the separate xhigh tier in the Claude
        // effort contract; use their high tier, or the catalog's explicit map.
        ThinkingLevel::XHigh
            if id.is_none_or(|id| {
                family(id, "claude-opus-4-6")
                    || family(id, "claude-sonnet-4-6")
                    || family(id, "claude-mythos-preview")
            }) =>
        {
            "high"
        }
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    };
    Ok(value.to_string())
}

fn requested_budget(level: ThinkingLevel, options: &StreamOptions) -> u32 {
    options.thinking_budgets.as_ref().map_or_else(
        || level.default_budget(),
        |budgets| match level {
            ThinkingLevel::Off => 0,
            ThinkingLevel::Minimal => budgets.minimal,
            ThinkingLevel::Low => budgets.low,
            ThinkingLevel::Medium => budgets.medium,
            ThinkingLevel::High => budgets.high,
            ThinkingLevel::XHigh => budgets.xhigh,
            ThinkingLevel::Max => budgets.max,
        },
    )
}

fn budget_and_limit(level: ThinkingLevel, options: &StreamOptions) -> Result<(u32, u32)> {
    let requested = requested_budget(level, options).max(MIN_THINKING_TOKENS);
    let max_tokens = match options.max_tokens {
        Some(limit) => limit,
        None => requested
            .checked_add(DEFAULT_ANSWER_TOKENS)
            .ok_or_else(|| request_error("Bedrock thinking budget is too large"))?
            .max(DEFAULT_MAX_TOKENS),
    };
    if max_tokens <= MIN_THINKING_TOKENS {
        return Err(request_error(
            "Bedrock extended thinking requires max_tokens greater than 1024; increase the limit or disable thinking",
        ));
    }
    // max_tokens is the caller's hard spending/output cap. Fit the reasoning
    // budget inside it instead of silently increasing it. Reserve up to 4096
    // answer tokens, but permit the smallest legal 1024 + 1 configuration.
    let available = max_tokens
        .saturating_sub(DEFAULT_ANSWER_TOKENS)
        .max(MIN_THINKING_TOKENS);
    Ok((requested.min(available), max_tokens))
}

fn inference_config(body: &mut Value) -> Result<&mut Map<String, Value>> {
    body.as_object_mut()
        .ok_or_else(|| request_error("Bedrock request must be an object"))?
        .entry("inferenceConfig")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| request_error("Bedrock inferenceConfig must be an object"))
}

fn apply_thinking(
    body: &mut Value,
    id: Option<&str>,
    mode: ThinkingMode,
    compat: Option<&CompatConfig>,
    options: &StreamOptions,
) -> Result<()> {
    if mode == ThinkingMode::Unsupported {
        return Ok(());
    }
    // New adaptive models reject sampling controls even when thinking is off.
    if mode == ThinkingMode::Adaptive
        && let Some(config) = body
            .get_mut("inferenceConfig")
            .and_then(Value::as_object_mut)
    {
        config.remove("temperature");
        config.remove("topP");
    }
    let Some(level) = options.thinking_level else {
        return Ok(()); // Leave the model's default intact, distinct from Off.
    };
    let fields = if level == ThinkingLevel::Off {
        if mode == ThinkingMode::Adaptive && id.is_some_and(thinking_is_required) {
            return Err(request_error(
                "This Bedrock model requires adaptive thinking; select minimal/low effort instead of off",
            ));
        }
        json!({"thinking": {"type": "disabled"}})
    } else if mode == ThinkingMode::Adaptive {
        let effort = effort(level, id, compat)?;
        json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": effort}})
    } else {
        let (budget, max_tokens) = budget_and_limit(level, options)?;
        let config = inference_config(body)?;
        config.insert("maxTokens".to_string(), json!(max_tokens));
        config.remove("temperature");
        config.remove("topP");
        json!({"thinking": {"type": "enabled", "budget_tokens": budget}})
    };
    body["additionalModelRequestFields"] = fields;
    Ok(())
}

fn cache_support(id: &str) -> Option<bool> {
    // Some(true) supports 1h; Some(false) supports only the default 5m.
    if adaptive_model(id)
        || ["claude-opus-4-5", "claude-sonnet-4-5", "claude-haiku-4-5"]
            .iter()
            .any(|prefix| family(id, prefix))
    {
        Some(true)
    } else if family(id, "claude-3-7-sonnet") || family(id, "claude-3-5-sonnet-20241022-v2") {
        Some(false)
    } else {
        None
    }
}

fn append_checkpoint(array: Option<&mut Value>, checkpoint: &Value) {
    if let Some(array) = array.and_then(Value::as_array_mut)
        && !array.is_empty()
    {
        array.push(checkpoint.clone());
    }
}

fn apply_caching(body: &mut Value, id: &str, retention: CacheRetention) {
    if retention == CacheRetention::None {
        return; // Do not opt callers into explicit cache-write charges.
    }
    let Some(supports_long) = cache_support(id) else {
        return;
    };
    let checkpoint = if retention == CacheRetention::Long && supports_long {
        json!({"cachePoint": {"type": "default", "ttl": "1h"}})
    } else {
        json!({"cachePoint": {"type": "default"}})
    };
    // Three breakpoints maximum, in service order: tools, system, messages.
    // All have the same TTL. The service decides whether token minima are met.
    append_checkpoint(body.pointer_mut("/toolConfig/tools"), &checkpoint);
    append_checkpoint(body.get_mut("system"), &checkpoint);
    if let Some(last) = body
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .and_then(|messages| messages.last_mut())
        && last.get("role").and_then(Value::as_str) == Some("user")
    {
        append_checkpoint(last.get_mut("content"), &checkpoint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AssistantMessage, ContentBlock, Message, TextContent, UserContent, UserMessage,
    };
    use crate::provider::{ThinkingBudgets, ToolDef};
    use std::collections::HashMap;

    fn context() -> Context<'static> {
        Context::owned(
            Some("Stable system prompt".to_string()),
            vec![Message::User(UserMessage {
                content: UserContent::Text("Question".to_string()),
                timestamp: 0,
            })],
            vec![ToolDef {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({"type": "object"}),
            }],
        )
    }

    fn options(level: ThinkingLevel) -> StreamOptions {
        StreamOptions {
            thinking_level: Some(level),
            temperature: Some(0.2),
            ..StreamOptions::default()
        }
    }

    fn build(model: &str, options: &StreamOptions) -> Value {
        prepare(model, None, &context(), options).expect("valid request")
    }

    #[test]
    fn budget_models_send_converse_thinking_fields_and_reserve_an_answer() {
        for model in [
            "anthropic.claude-3-7-sonnet-20250219-v1:0",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "eu.anthropic.claude-opus-4-1-20250805-v1:0",
            "anthropic.claude-haiku-4-5-20251001-v1:0",
        ] {
            let body = build(model, &options(ThinkingLevel::High));
            assert_eq!(
                body["additionalModelRequestFields"]["thinking"],
                json!({"type": "enabled", "budget_tokens": 16384})
            );
            assert_eq!(body["inferenceConfig"]["maxTokens"], 20480);
            assert!(body["inferenceConfig"].get("temperature").is_none());
            assert!(body.get("thinking").is_none());
            assert!(body.get("model").is_none());
        }
    }

    #[test]
    fn custom_budget_is_bounded_by_explicit_output_cap() {
        let mut opts = options(ThinkingLevel::Medium);
        opts.max_tokens = Some(8192);
        opts.thinking_budgets = Some(ThinkingBudgets {
            medium: 7000,
            ..ThinkingBudgets::default()
        });
        let body = build("anthropic.claude-sonnet-4-5", &opts);
        assert_eq!(body["inferenceConfig"]["maxTokens"], 8192);
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            4096
        );
        opts.thinking_budgets.as_mut().unwrap().medium = 2048;
        let body = build("anthropic.claude-sonnet-4-5", &opts);
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            2048
        );
    }

    #[test]
    fn manual_budget_minimum_and_overflow_are_handled_before_dispatch() {
        let mut opts = options(ThinkingLevel::Minimal);
        opts.thinking_budgets = Some(ThinkingBudgets {
            minimal: 0,
            ..ThinkingBudgets::default()
        });
        opts.max_tokens = Some(1025);
        let body = build("anthropic.claude-3-7-sonnet", &opts);
        assert_eq!(body["inferenceConfig"]["maxTokens"], 1025);
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            1024
        );
        for limit in [0, 1, 1024] {
            opts.max_tokens = Some(limit);
            assert!(prepare("anthropic.claude-3-7-sonnet", None, &context(), &opts).is_err());
        }
        opts.max_tokens = None;
        opts.thinking_budgets.as_mut().unwrap().minimal = u32::MAX;
        assert!(prepare("anthropic.claude-3-7-sonnet", None, &context(), &opts).is_err());
    }

    #[test]
    fn all_budget_levels_use_their_own_custom_value() {
        let budgets = ThinkingBudgets {
            minimal: 1100,
            low: 1200,
            medium: 1300,
            high: 1400,
            xhigh: 1500,
            max: 1600,
        };
        for (level, expected) in [
            (ThinkingLevel::Minimal, 1100),
            (ThinkingLevel::Low, 1200),
            (ThinkingLevel::Medium, 1300),
            (ThinkingLevel::High, 1400),
            (ThinkingLevel::XHigh, 1500),
            (ThinkingLevel::Max, 1600),
        ] {
            let opts = StreamOptions {
                thinking_budgets: Some(budgets.clone()),
                ..options(level)
            };
            let body = build("anthropic.claude-sonnet-4-5", &opts);
            assert_eq!(
                body["additionalModelRequestFields"]["thinking"]["budget_tokens"],
                expected
            );
        }
    }

    #[test]
    fn adaptive_models_use_effort_without_a_fixed_budget() {
        let opts = StreamOptions {
            max_tokens: Some(500),
            ..options(ThinkingLevel::Medium)
        };
        for model in [
            "global.anthropic.claude-opus-4-6-v1",
            "anthropic.claude-opus-4-7",
            "anthropic.claude-sonnet-4-6",
            "anthropic.claude-opus-5",
            "anthropic.claude-fable-5-1",
            "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-sonnet-5",
        ] {
            let body = build(model, &opts);
            assert_eq!(
                body["additionalModelRequestFields"],
                json!({
                    "thinking": {"type": "adaptive"},
                    "output_config": {"effort": "medium"}
                })
            );
            assert_eq!(body["inferenceConfig"]["maxTokens"], 500);
            assert!(body["inferenceConfig"].get("temperature").is_none());
        }
    }

    #[test]
    fn adaptive_effort_tiers_and_catalog_overrides_are_distinct() {
        assert_eq!(
            build("anthropic.claude-opus-4-6", &options(ThinkingLevel::XHigh))["additionalModelRequestFields"]
                ["output_config"]["effort"],
            "high"
        );
        assert_eq!(
            build("anthropic.claude-opus-5", &options(ThinkingLevel::XHigh))["additionalModelRequestFields"]
                ["output_config"]["effort"],
            "xhigh"
        );
        assert_eq!(
            build("anthropic.claude-opus-5", &options(ThinkingLevel::Max))["additionalModelRequestFields"]
                ["output_config"]["effort"],
            "max"
        );
        let mut compat = CompatConfig {
            force_adaptive_thinking: Some(true),
            thinking_level_map: Some(HashMap::from([(
                "xhigh".to_string(),
                Some("max".to_string()),
            )])),
            ..CompatConfig::default()
        };
        let body = prepare(
            "opaque-profile",
            Some(&compat),
            &context(),
            &options(ThinkingLevel::XHigh),
        )
        .unwrap();
        assert_eq!(
            body["additionalModelRequestFields"]["output_config"]["effort"],
            "max"
        );
        compat
            .thinking_level_map
            .as_mut()
            .unwrap()
            .insert("xhigh".to_string(), Some("typo".to_string()));
        assert!(
            prepare(
                "opaque-profile",
                Some(&compat),
                &context(),
                &options(ThinkingLevel::XHigh)
            )
            .is_err()
        );
        compat.force_adaptive_thinking = Some(false);
        let body = prepare(
            "opaque-profile",
            Some(&compat),
            &context(),
            &options(ThinkingLevel::Low),
        )
        .unwrap();
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["type"],
            "enabled"
        );
    }

    #[test]
    fn unspecified_thinking_and_explicit_off_are_not_conflated() {
        let body = build("anthropic.claude-opus-5", &StreamOptions::default());
        assert!(body.get("additionalModelRequestFields").is_none());
        let body = build("anthropic.claude-opus-5", &options(ThinkingLevel::Off));
        assert_eq!(
            body["additionalModelRequestFields"],
            json!({"thinking": {"type": "disabled"}})
        );
        assert!(body["inferenceConfig"].get("temperature").is_none());
        let body = build("anthropic.claude-sonnet-4-5", &options(ThinkingLevel::Off));
        assert_eq!(body["inferenceConfig"]["temperature"], json!(0.2_f32));
        assert!(
            prepare(
                "anthropic.claude-fable-5",
                None,
                &context(),
                &options(ThinkingLevel::Off)
            )
            .is_err()
        );
    }

    #[test]
    fn nullable_effort_map_rejects_disabled_tier_before_request() {
        let compat = CompatConfig {
            force_adaptive_thinking: Some(true),
            thinking_level_map: Some(std::collections::HashMap::from([("max".to_string(), None)])),
            ..CompatConfig::default()
        };
        let result = prepare(
            "opaque-profile",
            Some(&compat),
            &context(),
            &options(ThinkingLevel::Max),
        );
        assert!(result.is_err());
    }

    #[test]
    fn unknown_and_non_claude_models_keep_the_standard_request() {
        let opts = StreamOptions {
            cache_retention: CacheRetention::Long,
            ..options(ThinkingLevel::High)
        };
        let original =
            serde_json::to_value(BedrockProvider::build_request(&context(), &opts)).unwrap();
        for model in [
            "amazon.nova-pro-v1:0",
            "deepseek.r1-v1:0",
            "opaque-profile",
            "anthropic.claude-3-5-haiku",
            "anthropic.claude-opus-50",
        ] {
            assert_eq!(build(model, &opts), original, "{model}");
        }
    }

    #[test]
    fn caching_is_opt_in_and_uses_three_native_checkpoints() {
        let model = "anthropic.claude-sonnet-4-5";
        let original = build(model, &StreamOptions::default());
        let opts = StreamOptions {
            cache_retention: CacheRetention::Short,
            ..StreamOptions::default()
        };
        let mut cached = build(model, &opts);
        let marker = json!({"cachePoint": {"type": "default"}});
        for pointer in ["/toolConfig/tools", "/system", "/messages/0/content"] {
            let array = cached.pointer_mut(pointer).unwrap().as_array_mut().unwrap();
            assert_eq!(array.pop(), Some(marker.clone()));
        }
        assert_eq!(cached, original);
    }

    #[test]
    fn long_cache_uses_one_hour_only_on_supported_models() {
        let opts = StreamOptions {
            cache_retention: CacheRetention::Long,
            ..StreamOptions::default()
        };
        let modern = build("eu.anthropic.claude-opus-4-6-v1", &opts);
        assert_eq!(modern["system"][1]["cachePoint"]["ttl"], "1h");
        assert_eq!(modern["toolConfig"]["tools"][1]["cachePoint"]["ttl"], "1h");
        assert_eq!(
            modern["messages"][0]["content"][1]["cachePoint"]["ttl"],
            "1h"
        );
        let old = build("anthropic.claude-3-7-sonnet", &opts);
        assert_eq!(old["system"][1], json!({"cachePoint": {"type": "default"}}));
    }

    #[test]
    fn cache_markers_never_mutate_history_or_accumulate_between_turns() {
        let context = context();
        let before = serde_json::to_value(context.messages.as_ref()).unwrap();
        let opts = StreamOptions {
            cache_retention: CacheRetention::Short,
            ..StreamOptions::default()
        };
        let first = prepare("anthropic.claude-sonnet-4-5", None, &context, &opts).unwrap();
        let second = prepare("anthropic.claude-sonnet-4-5", None, &context, &opts).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            serde_json::to_value(context.messages.as_ref()).unwrap(),
            before
        );
    }

    #[test]
    fn caching_does_not_add_empty_sections_or_rewrite_assistant_blocks() {
        let mut context = context();
        context.system_prompt = None;
        context.tools.to_mut().clear();
        context
            .messages
            .to_mut()
            .push(Message::assistant(AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("Assistant prefix"))],
                ..AssistantMessage::default()
            }));
        let opts = StreamOptions {
            cache_retention: CacheRetention::Short,
            ..StreamOptions::default()
        };
        let original =
            serde_json::to_value(BedrockProvider::build_request(&context, &opts)).unwrap();
        let body = prepare("anthropic.claude-sonnet-4-5", None, &context, &opts).unwrap();
        assert_eq!(body, original);
    }
}
