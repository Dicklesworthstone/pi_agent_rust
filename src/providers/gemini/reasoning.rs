//! Native generateContent thinking controls and streamed reasoning state.
//!
//! Google documents budgets for 2.5 and levels for 3.x separately:
//! https://ai.google.dev/gemini-api/docs/generate-content/thinking
//! Signed parts, including empty text parts, must not be merged on replay:
//! https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures

use crate::error::{Error, Result};
use crate::model::{
    AssistantMessage, ContentBlock, StreamEvent, TextContent, ThinkingContent, ThinkingLevel, Usage,
};
use crate::provider::StreamOptions;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;

#[derive(Clone, Copy)]
enum Family {
    Budget {
        minimum: u32,
        maximum: u32,
        can_disable: bool,
    },
    Levels {
        minimal: bool,
        medium: bool,
    },
}

fn family(model: &str) -> Option<Family> {
    let model = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    // Image, audio, live and specialized models do not share this contract.
    // Restrict suffixes instead of accepting every model containing "gemini".
    let matches = |stem: &str| {
        model.strip_prefix(stem).is_some_and(|suffix| {
            suffix.is_empty()
                || suffix == "-001"
                || suffix == "-preview"
                || suffix.strip_prefix("-preview-").is_some_and(|date| {
                    !date.is_empty() && date.bytes().all(|b| b.is_ascii_digit() || b == b'-')
                })
        })
    };
    if matches("gemini-2.5-pro") {
        Some(Family::Budget {
            minimum: 128,
            maximum: 32_768,
            can_disable: false,
        })
    } else if matches("gemini-2.5-flash-lite") {
        Some(Family::Budget {
            minimum: 512,
            maximum: 24_576,
            can_disable: true,
        })
    } else if matches("gemini-2.5-flash") {
        Some(Family::Budget {
            minimum: 1,
            maximum: 24_576,
            can_disable: true,
        })
    } else if matches("gemini-3-pro") {
        Some(Family::Levels {
            minimal: false,
            medium: false,
        })
    } else if matches("gemini-3.1-pro")
        || matches("gemini-3.7-flash")
        || matches("gemini-3.8-flash")
    {
        Some(Family::Levels {
            minimal: false,
            medium: true,
        })
    } else if [
        "gemini-3-flash",
        "gemini-3.5-flash",
        "gemini-3.6-flash",
        "gemini-3.1-flash-lite",
        "gemini-3.5-flash-lite",
    ]
    .iter()
    .any(|stem| matches(stem))
    {
        Some(Family::Levels {
            minimal: true,
            medium: true,
        })
    } else {
        None
    }
}

fn budget(level: ThinkingLevel, options: &StreamOptions) -> u32 {
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

/// Build the exact inner wire payload before the request-rewrite hook. Never
/// increase maxOutputTokens to fund thinking, and never attach guessed Google
/// fields to an unrecognized/custom model. None preserves provider defaults.
pub fn prepare_request(
    model: &str,
    options: &StreamOptions,
    request: &impl Serialize,
) -> Result<Value> {
    if options.max_tokens == Some(0) {
        return Err(Error::provider("google", "max_tokens must be positive"));
    }
    let mut body = serde_json::to_value(request)?;
    let (Some(family), Some(level)) = (family(model), options.thinking_level) else {
        return Ok(body);
    };
    let config = body
        .get_mut("generationConfig")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| Error::provider("google", "Missing Gemini generation configuration"))?;
    let output_cap = config
        .get("maxOutputTokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| Error::provider("google", "Invalid Gemini output token limit"))?;
    let enabled = level != ThinkingLevel::Off;
    let thinking = match family {
        Family::Levels { minimal, medium } => {
            // "off" means the least supported effort on always-thinking
            // models, not a promise that the service performs zero reasoning.
            let native = match level {
                ThinkingLevel::Off | ThinkingLevel::Minimal if minimal => "minimal",
                ThinkingLevel::Off | ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
                ThinkingLevel::Medium if medium => "medium",
                ThinkingLevel::Medium
                | ThinkingLevel::High
                | ThinkingLevel::XHigh
                | ThinkingLevel::Max => "high",
            };
            json!({"thinkingLevel": native, "includeThoughts": enabled})
        }
        Family::Budget {
            minimum,
            maximum,
            can_disable,
        } => {
            let requested = budget(level, options);
            let native = if requested == 0 && can_disable {
                0
            } else {
                // Reserve half of small limits, or 4096 answer tokens for
                // larger limits. A too-small cap is rejected, never enlarged.
                let reserve = (output_cap / 2).clamp(1, 4096);
                let available = output_cap.saturating_sub(reserve).min(maximum);
                if available < minimum {
                    return Err(Error::provider(
                        "google",
                        "max_tokens is too small for this model's minimum thinking budget and an answer",
                    ));
                }
                requested.max(minimum).min(available)
            };
            json!({"thinkingBudget": native, "includeThoughts": enabled && native > 0})
        }
    };
    config.insert("thinkingConfig".to_string(), thinking);
    Ok(body)
}

/// Keep first-party reasoning metadata out of cross-provider replays. A text
/// signature from Responses, for example, is not a Gemini thought signature.
pub fn is_google_message(message: &AssistantMessage) -> bool {
    if message.api == "google-vertex" {
        // Vertex's Claude adapter reports google-vertex too. Its Anthropic
        // signatures must not be reinterpreted as Gemini thought signatures.
        return message
            .model
            .rsplit('/')
            .next()
            .is_some_and(|model| model.to_ascii_lowercase().starts_with("gemini-"));
    }
    matches!(
        message.api.as_str(),
        "google-generative-ai" | "google-generative" | "google-gemini-cli" | "google"
    )
}

/// Validate the explicit thought envelope. The wire decoder chooses this
/// envelope before deserializing, so invalid thoughts cannot fall through
/// to ordinary answer text.
pub fn deserialize_true<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<bool, D::Error> {
    if bool::deserialize(deserializer)? {
        Ok(true)
    } else {
        Err(serde::de::Error::custom("not a thought part"))
    }
}

/// Tracks the one open text/thought block. Closing on type changes makes
/// thinking → answer → tool call transitions explicit and closes each once.
#[derive(Default)]
pub struct ContentState {
    open: Option<usize>,
}

impl ContentState {
    #[allow(clippy::too_many_arguments)]
    pub fn append(
        &mut self,
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
        let can_extend = signature.is_none()
            && self.open.is_some_and(|index| {
                index + 1 == partial.content.len()
                    && match partial.content.get(index) {
                        Some(ContentBlock::Thinking(block)) if thought => {
                            block.thinking_signature.is_none()
                        }
                        Some(ContentBlock::Text(block)) if !thought => {
                            block.text_signature.is_none()
                        }
                        _ => false,
                    }
            });
        let content_index = if can_extend {
            self.open.expect("open block checked")
        } else {
            self.close(partial, events);
            let index = partial.content.len();
            if thought {
                partial
                    .content
                    .push(ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: signature,
                    }));
                events.push_back(StreamEvent::ThinkingStart {
                    content_index: index,
                });
            } else {
                partial.content.push(ContentBlock::Text(TextContent {
                    text: String::new(),
                    text_signature: signature,
                }));
                events.push_back(StreamEvent::TextStart {
                    content_index: index,
                });
            }
            self.open = Some(index);
            index
        };
        match &mut partial.content[content_index] {
            ContentBlock::Thinking(block) => {
                block.thinking.push_str(&text);
                if !text.is_empty() {
                    events.push_back(StreamEvent::ThinkingDelta {
                        content_index,
                        delta: text,
                    });
                }
            }
            ContentBlock::Text(block) => {
                block.text.push_str(&text);
                if !text.is_empty() {
                    events.push_back(StreamEvent::TextDelta {
                        content_index,
                        delta: text,
                    });
                }
            }
            _ => unreachable!("open block is text or thinking"),
        }
    }

    pub fn close(&mut self, partial: &AssistantMessage, events: &mut VecDeque<StreamEvent>) {
        let Some(content_index) = self.open.take() else {
            return;
        };
        match &partial.content[content_index] {
            ContentBlock::Text(block) => events.push_back(StreamEvent::TextEnd {
                content_index,
                content: block.text.clone(),
            }),
            ContentBlock::Thinking(block) => events.push_back(StreamEvent::ThinkingEnd {
                content_index,
                content: block.thinking.clone(),
            }),
            _ => unreachable!("open block is text or thinking"),
        }
    }
}

/// Usage chunks are cumulative snapshots but may omit individual counters.
/// Keep candidate and thought counters separate internally to avoid either
/// double-counting repeated chunks or losing one on a later metadata-only chunk.
#[derive(Default)]
pub struct UsageState {
    prompt: u64,
    candidates: u64,
    thoughts: u64,
    cached: u64,
}

impl UsageState {
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        usage: &mut Usage,
        prompt: Option<u64>,
        candidates: Option<u64>,
        thoughts: Option<u64>,
        cached: Option<u64>,
        total: Option<u64>,
    ) {
        if let Some(value) = prompt {
            self.prompt = value;
        }
        if let Some(value) = candidates {
            self.candidates = value;
        }
        if let Some(value) = thoughts {
            self.thoughts = value;
        }
        if let Some(value) = cached {
            self.cached = value;
        }
        // Google's prompt count includes cache hits. Pi bills cache reads
        // separately, while thought tokens are part of billed output.
        usage.input = self.prompt.saturating_sub(self.cached);
        usage.cache_read = self.cached;
        usage.output = self.candidates.saturating_add(self.thoughts);
        usage.total_tokens = total.filter(|total| *total > 0).unwrap_or_else(|| {
            usage
                .input
                .saturating_add(usage.cache_read)
                .saturating_add(usage.output)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ThinkingBudgets;

    fn request(model: &str, level: Option<ThinkingLevel>, cap: u32) -> Result<Value> {
        prepare_request(
            model,
            &StreamOptions {
                thinking_level: level,
                max_tokens: Some(cap),
                ..StreamOptions::default()
            },
            &json!({"contents": [], "generationConfig": {"maxOutputTokens": cap, "temperature": 0.8}}),
        )
    }

    #[test]
    fn absent_thinking_preserves_service_defaults() {
        let body = request("gemini-3.1-pro-preview", None, 8192).unwrap();
        assert!(body["generationConfig"].get("thinkingConfig").is_none());
        assert_eq!(body["generationConfig"]["temperature"], 0.8);
    }

    #[test]
    fn unrecognized_and_specialized_models_receive_no_guessed_fields() {
        for model in [
            "gemini-2.0-flash",
            "my-gemini-3-flash",
            "gemini-3-flash-image",
            "gemini-2.5-flash-native-audio-preview",
            "gemini-3.99-flash",
            "custom-model",
        ] {
            let body = request(model, Some(ThinkingLevel::High), 8192).unwrap();
            assert!(
                body["generationConfig"].get("thinkingConfig").is_none(),
                "{model}"
            );
        }
    }

    #[test]
    fn level_mapping_respects_each_documented_family() {
        for (model, low, medium) in [
            ("gemini-3-pro-preview", "low", "high"),
            ("gemini-3.1-pro-preview", "low", "medium"),
            ("gemini-3-flash-preview", "minimal", "medium"),
            ("gemini-3.1-flash-lite", "minimal", "medium"),
            ("gemini-3.8-flash", "low", "medium"),
        ] {
            for (level, expected) in [
                (ThinkingLevel::Off, low),
                (ThinkingLevel::Minimal, low),
                (ThinkingLevel::Medium, medium),
                (ThinkingLevel::Max, "high"),
            ] {
                let body = request(model, Some(level), 8192).unwrap();
                let config = &body["generationConfig"]["thinkingConfig"];
                assert_eq!(config["thinkingLevel"], expected, "{model}/{level}");
                assert!(config.get("thinkingBudget").is_none());
                assert_eq!(config["includeThoughts"], level != ThinkingLevel::Off);
            }
        }
    }

    #[test]
    fn two_five_budgets_keep_the_original_output_cap() {
        for (model, level, cap, expected) in [
            ("gemini-2.5-flash", ThinkingLevel::High, 8192, 4096),
            ("gemini-2.5-pro", ThinkingLevel::Max, 65_536, 32_768),
            ("gemini-2.5-flash-lite", ThinkingLevel::Max, 65_536, 24_576),
            ("gemini-2.5-pro", ThinkingLevel::Off, 8192, 128),
            ("gemini-2.5-flash", ThinkingLevel::Off, 1, 0),
        ] {
            let body = request(model, Some(level), cap).unwrap();
            assert_eq!(body["generationConfig"]["maxOutputTokens"], cap);
            assert_eq!(
                body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
                expected
            );
            assert!(
                body["generationConfig"]["thinkingConfig"]
                    .get("thinkingLevel")
                    .is_none()
            );
        }
    }

    #[test]
    fn custom_budget_levels_are_bounded_without_becoming_a_level_enum() {
        let options = StreamOptions {
            max_tokens: Some(8192),
            thinking_level: Some(ThinkingLevel::Medium),
            thinking_budgets: Some(ThinkingBudgets {
                medium: 3000,
                ..ThinkingBudgets::default()
            }),
            ..StreamOptions::default()
        };
        let body = prepare_request(
            "models/gemini-2.5-pro",
            &options,
            &json!({"generationConfig":{"maxOutputTokens":8192}}),
        )
        .unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            3000
        );
    }

    #[test]
    fn impossible_caps_fail_before_serialization_or_network_work() {
        assert!(request("gemini-2.5-pro", Some(ThinkingLevel::High), 128).is_err());
        assert!(request("gemini-2.5-flash-lite", Some(ThinkingLevel::Low), 512).is_err());
        assert!(request("gemini-3-flash-preview", None, 0).is_err());
    }

    #[test]
    fn omitted_usage_fields_do_not_reset_or_double_count_thinking() {
        let mut state = UsageState::default();
        let mut usage = Usage::default();
        state.update(&mut usage, Some(100), Some(7), Some(20), Some(40), None);
        assert_eq!(
            (
                usage.input,
                usage.output,
                usage.cache_read,
                usage.total_tokens
            ),
            (60, 27, 40, 127)
        );
        state.update(&mut usage, None, Some(9), None, None, None);
        assert_eq!(
            (
                usage.input,
                usage.output,
                usage.cache_read,
                usage.total_tokens
            ),
            (60, 29, 40, 129)
        );
        state.update(&mut usage, None, None, Some(20), None, Some(140));
        assert_eq!((usage.output, usage.total_tokens), (29, 140));
    }

    #[test]
    fn usage_arithmetic_saturates_on_corrupt_extreme_counters() {
        let mut state = UsageState::default();
        let mut usage = Usage::default();
        state.update(
            &mut usage,
            Some(1),
            Some(u64::MAX),
            Some(10),
            Some(2),
            Some(0),
        );
        assert_eq!(usage.input, 0);
        assert_eq!(usage.output, u64::MAX);
        assert_eq!(usage.total_tokens, u64::MAX);
    }

    #[test]
    fn thoughts_and_answers_have_separate_ordered_lifecycles() {
        let mut state = ContentState::default();
        let mut message = AssistantMessage::default();
        let mut events = VecDeque::new();
        let mut started = false;
        state.append(
            &mut message,
            &mut events,
            &mut started,
            "think ".into(),
            true,
            None,
        );
        state.append(
            &mut message,
            &mut events,
            &mut started,
            "more".into(),
            true,
            None,
        );
        state.append(
            &mut message,
            &mut events,
            &mut started,
            "answer".into(),
            false,
            None,
        );
        state.close(&message, &mut events);
        state.close(&message, &mut events);
        assert_eq!(message.content.len(), 2);
        assert!(
            matches!(&message.content[0], ContentBlock::Thinking(t) if t.thinking == "think more")
        );
        assert!(matches!(&message.content[1], ContentBlock::Text(t) if t.text == "answer"));
        assert!(matches!(&events[0], StreamEvent::Start { .. }));
        assert!(matches!(
            &events[4],
            StreamEvent::ThinkingEnd {
                content_index: 0,
                ..
            }
        ));
        assert!(matches!(
            &events[5],
            StreamEvent::TextStart { content_index: 1 }
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    StreamEvent::ThinkingEnd { .. } | StreamEvent::TextEnd { .. }
                ))
                .count(),
            2
        );
    }

    #[test]
    fn signed_parts_are_never_merged_including_empty_signature_chunks() {
        let mut state = ContentState::default();
        let mut message = AssistantMessage::default();
        let mut events = VecDeque::new();
        let mut started = false;
        state.append(
            &mut message,
            &mut events,
            &mut started,
            "plain".into(),
            false,
            None,
        );
        state.append(
            &mut message,
            &mut events,
            &mut started,
            "signed".into(),
            false,
            Some("one".into()),
        );
        state.append(
            &mut message,
            &mut events,
            &mut started,
            String::new(),
            false,
            Some("two".into()),
        );
        state.append(
            &mut message,
            &mut events,
            &mut started,
            "tail".into(),
            false,
            None,
        );
        state.close(&message, &mut events);
        assert_eq!(message.content.len(), 4);
        let ContentBlock::Text(empty) = &message.content[2] else {
            panic!("signed text");
        };
        assert!(empty.text.is_empty());
        assert_eq!(empty.text_signature.as_deref(), Some("two"));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, StreamEvent::TextEnd { .. }))
                .count(),
            4
        );
        let stored = serde_json::to_string(&message).unwrap();
        let replay: AssistantMessage = serde_json::from_str(&stored).unwrap();
        assert_eq!(
            serde_json::to_value(replay).unwrap(),
            serde_json::to_value(message).unwrap()
        );
    }

    #[test]
    fn google_provenance_does_not_reinterpret_other_provider_signatures() {
        let mut message = AssistantMessage::default();
        for api in [
            "anthropic-messages",
            "openai-responses",
            "bedrock-converse-stream",
        ] {
            message.api = api.to_string();
            assert!(!is_google_message(&message));
        }
        for api in ["google-generative-ai", "google-gemini-cli", "google-vertex"] {
            message.api = api.to_string();
            message.model = "gemini-2.5-pro".to_string();
            assert!(is_google_message(&message));
        }
    }

    #[test]
    fn vertex_api_name_alone_does_not_authorize_gemini_signature_replay() {
        let mut message = AssistantMessage {
            api: "google-vertex".to_string(),
            ..AssistantMessage::default()
        };
        for model in [
            "claude-sonnet-4-6",
            "publishers/anthropic/models/claude-opus-4-6",
            "",
        ] {
            message.model = model.to_string();
            assert!(!is_google_message(&message));
        }
        for model in [
            "gemini-2.5-pro",
            "publishers/google/models/gemini-3-flash-preview",
        ] {
            message.model = model.to_string();
            assert!(is_google_message(&message));
        }
    }
}
