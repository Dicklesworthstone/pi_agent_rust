//! Thinking controls, text/signature assembly, and usage for Google transports.
//!
//! The GenerateContent API uses token budgets on Gemini 2.5 and levels on
//! Gemini 3. Never send both. Keep this shared by Developer API, Cloud Code
//! Assist, and Vertex so their request and stream semantics cannot drift.

use crate::model::{
    AssistantMessage, ContentBlock, StreamEvent, TextContent, ThinkingContent, ThinkingLevel, Usage,
};
use crate::provider::{StreamOptions, ThinkingBudgets};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThinkingConfig {
    include_thoughts: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_budget: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_level: Option<&'static str>,
}

/// Map Pi's explicit setting to the supported model control. An unspecified
/// setting leaves provider defaults alone, as does an unrecognized model.
/// `Off` selects the minimum supported effort when a model cannot disable
/// thinking; it always disables delivery of thought summaries. Output caps
/// remain the caller's caps: this does not silently raise `max_tokens`.
#[must_use]
pub(crate) fn config(model: &str, options: &StreamOptions) -> Option<ThinkingConfig> {
    let level = options.thinking_level?;
    let model = model.rsplit('/').next()?.to_ascii_lowercase();
    let enabled = level != ThinkingLevel::Off;
    let budget_range = if model.starts_with("gemini-2.5-pro") {
        Some((128, 32_768, false))
    } else if model.starts_with("gemini-2.5-flash-lite") {
        Some((512, 24_576, true))
    } else if model.starts_with("gemini-2.5-flash")
        && !model.contains("image")
        && !model.contains("audio")
        && !model.contains("live")
    {
        Some((0, 24_576, true))
    } else {
        None
    };
    if let Some((minimum, maximum, can_disable)) = budget_range {
        let defaults = ThinkingBudgets::default();
        let budgets = options.thinking_budgets.as_ref().unwrap_or(&defaults);
        let requested = match level {
            ThinkingLevel::Off => 0,
            ThinkingLevel::Minimal => budgets.minimal,
            ThinkingLevel::Low => budgets.low,
            ThinkingLevel::Medium => budgets.medium,
            ThinkingLevel::High => budgets.high,
            ThinkingLevel::XHigh => budgets.xhigh,
            ThinkingLevel::Max => budgets.max,
        };
        let budget = if !enabled && can_disable {
            0
        } else {
            // A positive setting on Flash-Lite must meet its 512-token floor.
            requested.clamp(minimum, maximum)
        };
        return Some(ThinkingConfig {
            include_thoughts: enabled,
            thinking_budget: Some(budget),
            thinking_level: None,
        });
    }

    // Model-specific level support. Unknown model generations are deliberately
    // not inferred from a substring: extensions may configure their own schema.
    let (minimal, medium, low) = if model.starts_with("gemini-3-pro") {
        (false, false, true)
    } else if model.starts_with("gemini-3.1-pro") {
        (false, true, true)
    } else if model.starts_with("gemini-3.1-flash-lite") && model.contains("image") {
        (true, false, false)
    } else if model.starts_with("gemini-3-flash")
        || model.starts_with("gemini-3.1-flash-lite")
        || model.starts_with("gemini-3.5-flash")
        || model.starts_with("gemini-3.6-flash")
    {
        (true, true, true)
    } else if model.starts_with("gemini-3.7-flash") || model.starts_with("gemini-3.8-flash") {
        (false, true, true)
    } else {
        return None;
    };
    let wire_level = match level {
        ThinkingLevel::Off | ThinkingLevel::Minimal if minimal => "MINIMAL",
        ThinkingLevel::Off | ThinkingLevel::Minimal | ThinkingLevel::Low if low => "LOW",
        ThinkingLevel::Low => "MINIMAL",
        ThinkingLevel::Medium if medium => "MEDIUM",
        _ => "HIGH",
    };
    Some(ThinkingConfig {
        include_thoughts: enabled,
        thinking_budget: None,
        thinking_level: Some(wire_level),
    })
}

/// Assemble only compatible, unsigned text fragments. A signed part is sealed:
/// neither another signed part nor later unsigned content may be merged into
/// it. Empty trailing signature chunks attach to an open compatible block.
/// Signatures are opaque strings; concatenating them corrupts model state.
pub(crate) fn push_text(
    partial: &mut AssistantMessage,
    events: &mut VecDeque<StreamEvent>,
    started: &mut bool,
    text: String,
    thought: bool,
    signature: Option<String>,
) {
    if text.is_empty() && signature.is_none() {
        return;
    }
    if !*started {
        *started = true;
        events.push_back(StreamEvent::Start {
            partial: partial.clone(),
        });
    }
    let compatible_open = match partial.content.last() {
        Some(ContentBlock::Thinking(block)) if thought => block.thinking_signature.is_none(),
        Some(ContentBlock::Text(block)) if !thought => block.text_signature.is_none(),
        _ => false,
    };
    let append = compatible_open && (signature.is_none() || text.is_empty());
    let index = if append {
        partial.content.len() - 1
    } else {
        let index = partial.content.len();
        if thought {
            partial
                .content
                .push(ContentBlock::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: None,
                }));
            events.push_back(StreamEvent::ThinkingStart {
                content_index: index,
            });
        } else {
            partial
                .content
                .push(ContentBlock::Text(TextContent::new("")));
            events.push_back(StreamEvent::TextStart {
                content_index: index,
            });
        }
        index
    };
    match &mut partial.content[index] {
        ContentBlock::Thinking(block) => {
            block.thinking.push_str(&text);
            if signature.is_some() {
                block.thinking_signature = signature;
            }
        }
        ContentBlock::Text(block) => {
            block.text.push_str(&text);
            if signature.is_some() {
                block.text_signature = signature;
            }
        }
        _ => unreachable!("text assembly only creates text or thinking blocks"),
    }
    if !text.is_empty() {
        events.push_back(if thought {
            StreamEvent::ThinkingDelta {
                content_index: index,
                delta: text,
            }
        } else {
            StreamEvent::TextDelta {
                content_index: index,
                delta: text,
            }
        });
    }
}

/// Foreign provider signatures are not Google thought signatures. This also
/// keeps Anthropic reasoning from being presented as Google-authored thoughts.
#[must_use]
pub(crate) fn google_history(message: &AssistantMessage) -> bool {
    message.api.starts_with("google-") || message.api == "google"
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_field_names)]
pub(crate) struct UsageMetadata {
    pub(crate) prompt_token_count: Option<u64>,
    pub(crate) candidates_token_count: Option<u64>,
    pub(crate) thoughts_token_count: Option<u64>,
    pub(crate) cached_content_token_count: Option<u64>,
    pub(crate) total_token_count: Option<u64>,
}

/// Google reports cumulative snapshots, sometimes with only some fields in a
/// chunk. Preserve omitted counters, replace supplied ones (including zero),
/// and never add snapshots together or charge cached input twice.
#[derive(Default)]
pub(crate) struct UsageAccumulator {
    prompt: u64,
    candidates: u64,
    thoughts: u64,
    cached: u64,
}

impl UsageAccumulator {
    pub(crate) fn update(&mut self, metadata: UsageMetadata, usage: &mut Usage) {
        if let Some(value) = metadata.prompt_token_count {
            self.prompt = value;
        }
        if let Some(value) = metadata.candidates_token_count {
            self.candidates = value;
        }
        if let Some(value) = metadata.thoughts_token_count {
            self.thoughts = value;
        }
        if let Some(value) = metadata.cached_content_token_count {
            self.cached = value;
        }
        usage.cache_read = self.cached.min(self.prompt);
        usage.input = self.prompt.saturating_sub(usage.cache_read);
        usage.output = self.candidates.saturating_add(self.thoughts);
        usage.total_tokens = metadata
            .total_token_count
            .unwrap_or_else(|| self.prompt.saturating_add(usage.output));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn configuration(model: &str, level: ThinkingLevel) -> Value {
        serde_json::to_value(
            config(
                model,
                &StreamOptions {
                    thinking_level: Some(level),
                    ..StreamOptions::default()
                },
            )
            .expect("supported model"),
        )
        .unwrap()
    }

    #[test]
    fn unspecified_and_unsupported_models_keep_their_defaults() {
        assert!(config("gemini-2.5-pro", &StreamOptions::default()).is_none());
        let options = StreamOptions {
            thinking_level: Some(ThinkingLevel::High),
            ..Default::default()
        };
        for model in [
            "gemini-2.0-flash",
            "custom-model",
            "not-gemini-3-pro",
            "gemini-2.5-flash-image",
            "gemini-2.5-flash-native-audio",
        ] {
            assert!(config(model, &options).is_none(), "{model}");
        }
    }

    #[test]
    fn budget_models_use_budgets_and_honor_custom_values() {
        let options = StreamOptions {
            thinking_level: Some(ThinkingLevel::Medium),
            thinking_budgets: Some(ThinkingBudgets {
                medium: 1234,
                ..ThinkingBudgets::default()
            }),
            max_tokens: Some(4096),
            ..Default::default()
        };
        let value =
            serde_json::to_value(config("models/gemini-2.5-flash", &options).unwrap()).unwrap();
        assert_eq!(
            value,
            json!({"includeThoughts": true, "thinkingBudget": 1234})
        );
        assert_eq!(options.max_tokens, Some(4096));
    }

    #[test]
    fn budgets_respect_model_floors_and_ceilings() {
        let mut options = StreamOptions {
            thinking_level: Some(ThinkingLevel::Minimal),
            thinking_budgets: Some(ThinkingBudgets {
                minimal: 1,
                ..ThinkingBudgets::default()
            }),
            ..Default::default()
        };
        let value =
            serde_json::to_value(config("gemini-2.5-flash-lite", &options).unwrap()).unwrap();
        assert_eq!(value["thinkingBudget"], 512);
        options.thinking_level = Some(ThinkingLevel::Max);
        let value =
            serde_json::to_value(config("gemini-2.5-pro-preview", &options).unwrap()).unwrap();
        assert_eq!(value["thinkingBudget"], 32768);
        let value = serde_json::to_value(config("gemini-2.5-flash", &options).unwrap()).unwrap();
        assert_eq!(value["thinkingBudget"], 24576);
    }

    #[test]
    fn off_disables_flash_but_minimizes_non_disablable_models() {
        assert_eq!(
            configuration("gemini-2.5-flash-lite", ThinkingLevel::Off),
            json!({"includeThoughts": false, "thinkingBudget": 0})
        );
        assert_eq!(
            configuration("gemini-2.5-pro", ThinkingLevel::Off),
            json!({"includeThoughts": false, "thinkingBudget": 128})
        );
        assert_eq!(
            configuration("gemini-3.1-pro", ThinkingLevel::Off),
            json!({"includeThoughts": false, "thinkingLevel": "LOW"})
        );
    }

    #[test]
    fn level_models_never_send_token_budgets() {
        for level in [
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
            ThinkingLevel::Max,
        ] {
            let value = configuration("gemini-3-flash-preview", level);
            assert!(value.get("thinkingBudget").is_none());
            assert_eq!(value["includeThoughts"], true);
        }
        assert_eq!(
            configuration("gemini-3-pro-preview", ThinkingLevel::Medium)["thinkingLevel"],
            "HIGH"
        );
        assert_eq!(
            configuration("gemini-3.1-pro-preview", ThinkingLevel::Medium)["thinkingLevel"],
            "MEDIUM"
        );
        assert_eq!(
            configuration("gemini-3-flash-preview", ThinkingLevel::Minimal)["thinkingLevel"],
            "MINIMAL"
        );
    }

    #[test]
    fn newer_flash_and_image_variants_only_receive_supported_levels() {
        assert_eq!(
            configuration("gemini-3.8-flash", ThinkingLevel::Minimal)["thinkingLevel"],
            "LOW"
        );
        assert_eq!(
            configuration("gemini-3.7-flash", ThinkingLevel::Off)["thinkingLevel"],
            "LOW"
        );
        assert_eq!(
            configuration("gemini-3.5-flash", ThinkingLevel::Minimal)["thinkingLevel"],
            "MINIMAL"
        );
        assert_eq!(
            configuration("gemini-3.1-flash-lite-image", ThinkingLevel::Low)["thinkingLevel"],
            "MINIMAL"
        );
        assert_eq!(
            configuration("gemini-3.1-flash-lite-image", ThinkingLevel::Medium)["thinkingLevel"],
            "HIGH"
        );
    }

    #[test]
    fn thought_deltas_are_not_answer_text() {
        let mut partial = AssistantMessage::default();
        let mut events = VecDeque::new();
        let mut started = false;
        push_text(
            &mut partial,
            &mut events,
            &mut started,
            "Consider ".into(),
            true,
            None,
        );
        push_text(
            &mut partial,
            &mut events,
            &mut started,
            "the cases".into(),
            true,
            None,
        );
        push_text(
            &mut partial,
            &mut events,
            &mut started,
            "Answer".into(),
            false,
            None,
        );
        assert_eq!(partial.content.len(), 2);
        assert!(
            matches!(&partial.content[0], ContentBlock::Thinking(t) if t.thinking == "Consider the cases")
        );
        assert!(matches!(&partial.content[1], ContentBlock::Text(t) if t.text == "Answer"));
        assert!(matches!(events[0], StreamEvent::Start { .. }));
        assert!(matches!(
            events[1],
            StreamEvent::ThinkingStart { content_index: 0 }
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::ThinkingDelta { .. }))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::TextDelta { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn trailing_signatures_seal_blocks_without_spurious_text_deltas() {
        let mut partial = AssistantMessage::default();
        let mut events = VecDeque::new();
        let mut started = false;
        push_text(
            &mut partial,
            &mut events,
            &mut started,
            "reasoning".into(),
            true,
            None,
        );
        let event_count = events.len();
        push_text(
            &mut partial,
            &mut events,
            &mut started,
            String::new(),
            true,
            Some("signature-one".into()),
        );
        assert_eq!(events.len(), event_count);
        assert!(
            matches!(&partial.content[0], ContentBlock::Thinking(t) if t.thinking_signature.as_deref() == Some("signature-one"))
        );
        push_text(
            &mut partial,
            &mut events,
            &mut started,
            "another part".into(),
            true,
            None,
        );
        assert_eq!(
            partial.content.len(),
            2,
            "do not merge unsigned content into a signed part"
        );
    }

    #[test]
    fn signed_parts_remain_distinct_and_empty_signed_parts_survive() {
        let mut partial = AssistantMessage::default();
        let mut events = VecDeque::new();
        let mut started = false;
        for (text, signature) in [("a", "sig-a"), ("b", "sig-b"), ("", "sig-c")] {
            push_text(
                &mut partial,
                &mut events,
                &mut started,
                text.into(),
                false,
                Some(signature.into()),
            );
        }
        assert_eq!(partial.content.len(), 3);
        let serialized = serde_json::to_string(&partial).unwrap();
        let replay: AssistantMessage = serde_json::from_str(&serialized).unwrap();
        for (block, signature) in replay.content.iter().zip(["sig-a", "sig-b", "sig-c"]) {
            assert!(
                matches!(block, ContentBlock::Text(t) if t.text_signature.as_deref() == Some(signature))
            );
        }
    }

    #[test]
    fn empty_unsigned_chunks_do_not_create_an_answer() {
        let mut partial = AssistantMessage::default();
        let mut events = VecDeque::new();
        let mut started = false;
        push_text(
            &mut partial,
            &mut events,
            &mut started,
            String::new(),
            false,
            None,
        );
        assert!(partial.content.is_empty());
        assert!(events.is_empty());
        assert!(!started);
    }

    #[test]
    fn foreign_thought_signatures_are_not_google_history() {
        let mut message = AssistantMessage::default();
        for api in [
            "anthropic-messages",
            "openai-responses",
            "openai-completions",
            "",
        ] {
            message.api = api.into();
            assert!(!google_history(&message));
        }
        for api in [
            "google-generative-ai",
            "google-gemini-cli",
            "google-vertex",
            "google",
        ] {
            message.api = api.into();
            assert!(google_history(&message));
        }
    }

    #[test]
    fn usage_counts_thinking_and_does_not_charge_cached_input_twice() {
        let metadata: UsageMetadata = serde_json::from_value(json!({
            "promptTokenCount": 100, "cachedContentTokenCount": 80,
            "candidatesTokenCount": 10, "thoughtsTokenCount": 30, "totalTokenCount": 140
        }))
        .unwrap();
        let mut usage = Usage::default();
        UsageAccumulator::default().update(metadata, &mut usage);
        assert_eq!(
            (
                usage.input,
                usage.cache_read,
                usage.output,
                usage.total_tokens
            ),
            (20, 80, 40, 140)
        );
    }

    #[test]
    fn sparse_usage_updates_preserve_counters_and_repeated_snapshots_are_not_added() {
        let mut accumulator = UsageAccumulator::default();
        let mut usage = Usage::default();
        for _ in 0..2 {
            accumulator.update(
                serde_json::from_value(json!({
                    "promptTokenCount": 100, "candidatesTokenCount": 10
                }))
                .unwrap(),
                &mut usage,
            );
        }
        accumulator.update(
            serde_json::from_value(
                json!({"thoughtsTokenCount": 30, "cachedContentTokenCount": 80}),
            )
            .unwrap(),
            &mut usage,
        );
        assert_eq!(
            (
                usage.input,
                usage.cache_read,
                usage.output,
                usage.total_tokens
            ),
            (20, 80, 40, 140)
        );
        accumulator.update(
            serde_json::from_value(json!({"thoughtsTokenCount": 0})).unwrap(),
            &mut usage,
        );
        assert_eq!(usage.output, 10, "explicit zero is not an omitted field");
    }

    #[test]
    fn malformed_usage_cannot_underflow_or_overflow() {
        let mut usage = Usage::default();
        UsageAccumulator::default().update(
            serde_json::from_value(json!({
                "promptTokenCount": 1, "cachedContentTokenCount": u64::MAX,
                "candidatesTokenCount": u64::MAX, "thoughtsTokenCount": u64::MAX
            }))
            .unwrap(),
            &mut usage,
        );
        assert_eq!(usage.input, 0);
        assert_eq!(usage.cache_read, 1);
        assert_eq!(usage.output, u64::MAX);
        assert_eq!(usage.total_tokens, u64::MAX);
    }
}
