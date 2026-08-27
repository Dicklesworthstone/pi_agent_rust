use crate::model::{
    ContentBlock, ImageContent, Message as ModelMessage, TextContent, Usage, UserContent,
};
use crate::models::ModelEntry;
use crate::session::{Session, SessionEntry, SessionMessage, bash_execution_to_text};
use serde_json::{Value, json};

use super::text_utils::push_line;
use super::{ConversationMessage, MessageRole};

pub(super) fn user_content_to_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => content_blocks_to_text(blocks),
    }
}

pub(super) fn assistant_content_to_text(content: &[ContentBlock]) -> (String, Option<String>) {
    let mut text = String::new();
    let mut thinking = String::new();

    for block in content {
        match block {
            ContentBlock::Text(t) => push_line(&mut text, &t.text),
            ContentBlock::Thinking(t) => push_line(&mut thinking, &t.thinking),
            _ => {}
        }
    }

    let thinking = if thinking.trim().is_empty() {
        None
    } else {
        Some(thinking)
    };

    (text, thinking)
}

pub(super) fn content_blocks_to_text(blocks: &[ContentBlock]) -> String {
    let mut output = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text_block) => push_line(&mut output, &text_block.text),
            ContentBlock::Image(image) => {
                let rendered =
                    crate::terminal_images::render_inline(&image.data, &image.mime_type, 72);
                push_line(&mut output, &rendered);
            }
            ContentBlock::Thinking(thinking_block) => {
                push_line(&mut output, &thinking_block.thinking);
            }
            ContentBlock::ToolCall(call) => {
                push_line(&mut output, &format!("[tool call: {}]", call.name));
            }
            // Opaque safety-redacted reasoning has no surfaceable text.
            ContentBlock::RedactedThinking(_) => {}
        }
    }
    output
}

pub(super) fn split_content_blocks_for_input(
    blocks: &[ContentBlock],
) -> (String, Vec<ImageContent>) {
    let mut text = String::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text_block) => push_line(&mut text, &text_block.text),
            ContentBlock::Image(image) => images.push(image.clone()),
            _ => {}
        }
    }
    (text, images)
}

pub(super) fn build_content_blocks_for_input(
    text: &str,
    images: &[ImageContent],
) -> Vec<ContentBlock> {
    let mut content = Vec::new();
    if !text.trim().is_empty() {
        content.push(ContentBlock::Text(TextContent::new(text.to_string())));
    }
    for image in images {
        content.push(ContentBlock::Image(image.clone()));
    }
    content
}

pub(super) fn tool_content_blocks_to_text(blocks: &[ContentBlock], show_images: bool) -> String {
    let mut output = String::new();
    let mut hidden_images = 0usize;

    for block in blocks {
        match block {
            ContentBlock::Text(text_block) => push_line(&mut output, &text_block.text),
            ContentBlock::Image(image) => {
                if show_images {
                    let rendered =
                        crate::terminal_images::render_inline(&image.data, &image.mime_type, 72);
                    push_line(&mut output, &rendered);
                } else {
                    hidden_images = hidden_images.saturating_add(1);
                }
            }
            ContentBlock::Thinking(thinking_block) => {
                push_line(&mut output, &thinking_block.thinking);
            }
            ContentBlock::ToolCall(call) => {
                push_line(&mut output, &format!("[tool call: {}]", call.name));
            }
            ContentBlock::RedactedThinking(_) => {}
        }
    }

    if !show_images && hidden_images > 0 {
        push_line(&mut output, &format!("[{hidden_images} image(s) hidden]"));
    }

    output
}

pub fn conversation_from_session(session: &Session) -> (Vec<ConversationMessage>, Usage) {
    let mut messages = Vec::new();
    let mut usage = Usage::default();

    for entry in session.entries_for_current_path() {
        let SessionEntry::Message(message_entry) = entry else {
            continue;
        };

        match &message_entry.message {
            SessionMessage::User { content, .. } => {
                messages.push(ConversationMessage::new(
                    MessageRole::User,
                    user_content_to_text(content),
                    None,
                ));
            }
            SessionMessage::Assistant { message } => {
                let (text, thinking) = assistant_content_to_text(&message.content);
                add_usage(&mut usage, &message.usage);
                messages.push(ConversationMessage::new(
                    MessageRole::Assistant,
                    text,
                    thinking,
                ));
            }
            SessionMessage::ToolResult {
                tool_name,
                content,
                details,
                is_error,
                ..
            } => {
                let (mut text, _) = assistant_content_to_text(content);
                if let Some(diff) = details
                    .as_ref()
                    .and_then(|d: &Value| d.get("diff"))
                    .and_then(Value::as_str)
                {
                    let diff = diff.trim();
                    if !diff.is_empty() {
                        if !text.trim().is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str("Diff:\n");
                        text.push_str(diff);
                    }
                }
                let prefix = if *is_error {
                    "Tool error"
                } else {
                    "Tool result"
                };
                messages.push(ConversationMessage::tool(format!(
                    "{prefix} ({tool_name}): {text}"
                )));
            }
            SessionMessage::BashExecution {
                command,
                output,
                extra,
                ..
            } => {
                let mut text = bash_execution_to_text(command, output, 0, false, false, None);
                if extra
                    .get("excludeFromContext")
                    .and_then(Value::as_bool)
                    .is_some_and(|v| v)
                {
                    text.push_str("\n\n[Output excluded from model context]");
                }
                messages.push(ConversationMessage::tool(text));
            }
            SessionMessage::Custom {
                content,
                display: true,
                ..
            } => {
                messages.push(ConversationMessage::new(
                    MessageRole::System,
                    content.clone(),
                    None,
                ));
            }
            _ => {}
        }
    }

    (messages, usage)
}

pub(super) fn extension_model_from_entry(entry: &ModelEntry) -> Value {
    let api_key_present = entry
        .api_key
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty());
    json!({
        "provider": entry.model.provider.as_str(),
        "id": entry.model.id.as_str(),
        "name": entry.model.name.as_str(),
        "api": entry.model.api.as_str(),
        "baseUrl": entry.model.base_url.as_str(),
        "reasoning": entry.model.reasoning,
        "contextWindow": entry.model.context_window,
        "maxTokens": entry.model.max_tokens,
        "apiKeyPresent": api_key_present,
    })
}

pub(super) fn last_assistant_message(
    messages: &[ModelMessage],
) -> Option<&crate::model::AssistantMessage> {
    messages.iter().rev().find_map(|msg| match msg {
        ModelMessage::Assistant(assistant) => Some(assistant.as_ref()),
        _ => None,
    })
}

pub(super) fn add_usage(total: &mut Usage, delta: &Usage) {
    total.input = total.input.saturating_add(delta.input);
    total.output = total.output.saturating_add(delta.output);
    total.cache_read = total.cache_read.saturating_add(delta.cache_read);
    total.cache_write = total.cache_write.saturating_add(delta.cache_write);
    total.total_tokens = total.total_tokens.saturating_add(delta.total_tokens);
    total.cost.input += delta.cost.input;
    total.cost.output += delta.cost.output;
    total.cost.cache_read += delta.cost.cache_read;
    total.cost.cache_write += delta.cost.cache_write;
    total.cost.total += delta.cost.total;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AssistantMessage, ContentBlock, ImageContent, StopReason, TextContent, ThinkingContent,
        ToolCall, UserContent, UserMessage,
    };
    use std::collections::HashMap;

    // ── user_content_to_text ────────────────────────────────────────────

    #[test]
    fn user_content_text_variant() {
        let content = UserContent::Text("hello world".to_string());
        assert_eq!(user_content_to_text(&content), "hello world");
    }

    #[test]
    fn user_content_blocks_variant() {
        let content = UserContent::Blocks(vec![
            ContentBlock::Text(TextContent::new("first".to_string())),
            ContentBlock::Text(TextContent::new("second".to_string())),
        ]);
        let result = user_content_to_text(&content);
        assert!(result.contains("first"));
        assert!(result.contains("second"));
    }

    // ── assistant_content_to_text ───────────────────────────────────────

    #[test]
    fn assistant_content_text_only() {
        let blocks = vec![ContentBlock::Text(TextContent::new("response".to_string()))];
        let (text, thinking) = assistant_content_to_text(&blocks);
        assert_eq!(text, "response");
        assert!(thinking.is_none());
    }

    #[test]
    fn assistant_content_with_thinking() {
        let blocks = vec![
            ContentBlock::Thinking(ThinkingContent {
                thinking: "hmm let me think".to_string(),
                thinking_signature: None,
            }),
            ContentBlock::Text(TextContent::new("answer".to_string())),
        ];
        let (text, thinking) = assistant_content_to_text(&blocks);
        assert_eq!(text, "answer");
        assert_eq!(thinking.unwrap(), "hmm let me think");
    }

    #[test]
    fn assistant_content_empty_thinking_returns_none() {
        let blocks = vec![
            ContentBlock::Thinking(ThinkingContent {
                thinking: "  ".to_string(),
                thinking_signature: None,
            }),
            ContentBlock::Text(TextContent::new("text".to_string())),
        ];
        let (_, thinking) = assistant_content_to_text(&blocks);
        assert!(thinking.is_none());
    }

    #[test]
    fn assistant_content_tool_call_ignored() {
        let blocks = vec![
            ContentBlock::ToolCall(ToolCall {
                id: "tc_1".to_string(),
                name: "bash".to_string(),
                arguments: serde_json::json!({"cmd": "ls"}),
                thought_signature: None,
            }),
            ContentBlock::Text(TextContent::new("done".to_string())),
        ];
        let (text, _) = assistant_content_to_text(&blocks);
        assert_eq!(text, "done");
    }

    // ── split_content_blocks_for_input ──────────────────────────────────

    #[test]
    fn split_content_separates_text_and_images() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("prompt".to_string())),
            ContentBlock::Image(ImageContent {
                data: "base64data".to_string(),
                mime_type: "image/png".to_string(),
            }),
        ];
        let (text, images) = split_content_blocks_for_input(&blocks);
        assert!(text.contains("prompt"));
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/png");
    }

    #[test]
    fn split_content_ignores_thinking_and_tool_calls() {
        let blocks = vec![
            ContentBlock::Thinking(ThinkingContent {
                thinking: "ignored".to_string(),
                thinking_signature: None,
            }),
            ContentBlock::ToolCall(ToolCall {
                id: "tc_1".to_string(),
                name: "bash".to_string(),
                arguments: serde_json::json!({}),
                thought_signature: None,
            }),
        ];
        let (text, images) = split_content_blocks_for_input(&blocks);
        assert!(text.is_empty());
        assert!(images.is_empty());
    }

    // ── build_content_blocks_for_input ──────────────────────────────────

    #[test]
    fn build_content_text_only() {
        let blocks = build_content_blocks_for_input("hello", &[]);
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::Text(t) if t.text == "hello"));
    }

    #[test]
    fn build_content_empty_text_omitted() {
        let blocks = build_content_blocks_for_input("  ", &[]);
        assert!(blocks.is_empty());
    }

    #[test]
    fn build_content_text_and_images() {
        let images = vec![ImageContent {
            data: "abc".to_string(),
            mime_type: "image/jpeg".to_string(),
        }];
        let blocks = build_content_blocks_for_input("prompt", &images);
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[0], ContentBlock::Text(_)));
        assert!(matches!(&blocks[1], ContentBlock::Image(_)));
    }

    // ── tool_content_blocks_to_text ─────────────────────────────────────

    #[test]
    fn tool_content_text_blocks() {
        let blocks = vec![ContentBlock::Text(TextContent::new("output".to_string()))];
        let result = tool_content_blocks_to_text(&blocks, true);
        assert_eq!(result, "output");
    }

    #[test]
    fn tool_content_hidden_images_counted() {
        let blocks = vec![
            ContentBlock::Image(ImageContent {
                data: "a".to_string(),
                mime_type: "image/png".to_string(),
            }),
            ContentBlock::Image(ImageContent {
                data: "b".to_string(),
                mime_type: "image/png".to_string(),
            }),
        ];
        let result = tool_content_blocks_to_text(&blocks, false);
        assert!(result.contains("[2 image(s) hidden]"));
    }

    #[test]
    fn tool_content_tool_call_rendered() {
        let blocks = vec![ContentBlock::ToolCall(ToolCall {
            id: "tc_1".to_string(),
            name: "read".to_string(),
            arguments: serde_json::json!({}),
            thought_signature: None,
        })];
        let result = tool_content_blocks_to_text(&blocks, true);
        assert!(result.contains("[tool call: read]"));
    }

    // ── add_usage ───────────────────────────────────────────────────────

    #[test]
    fn add_usage_accumulates() {
        let mut total = Usage::default();
        let delta = Usage {
            input: 100,
            output: 50,
            cache_read: 10,
            cache_write: 5,
            total_tokens: 165,
            ..Usage::default()
        };
        add_usage(&mut total, &delta);
        assert_eq!(total.input, 100);
        assert_eq!(total.output, 50);
        assert_eq!(total.total_tokens, 165);
    }

    #[test]
    fn add_usage_saturating() {
        let mut total = Usage {
            input: u64::MAX - 10,
            ..Usage::default()
        };
        let delta = Usage {
            input: 100,
            ..Usage::default()
        };
        add_usage(&mut total, &delta);
        assert_eq!(total.input, u64::MAX);
    }

    // ── extension_model_from_entry ──────────────────────────────────────

    #[test]
    fn extension_model_json_structure() {
        use crate::models::ModelEntry;
        use crate::provider::{InputType, Model, ModelCost};

        let entry = ModelEntry {
            model: Model {
                id: "gpt-4".to_string(),
                name: "GPT-4".to_string(),
                api: "openai-chat".to_string(),
                provider: "openai".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                reasoning: true,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.03,
                    output: 0.06,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 4096,
                headers: HashMap::new(),
            },
            api_key: Some("sk-test".to_string()),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        };
        let json = extension_model_from_entry(&entry);
        assert_eq!(json["provider"], "openai");
        assert_eq!(json["id"], "gpt-4");
        assert_eq!(json["reasoning"], true);
        assert_eq!(json["contextWindow"], 128_000);
        assert_eq!(json["apiKeyPresent"], true);
    }

    #[test]
    fn extension_model_json_treats_blank_key_as_missing() {
        use crate::models::ModelEntry;
        use crate::provider::{InputType, Model, ModelCost};

        let entry = ModelEntry {
            model: Model {
                id: "dev-model".to_string(),
                name: "Dev Model".to_string(),
                api: "openai-completions".to_string(),
                provider: "acme".to_string(),
                base_url: "https://example.invalid/v1".to_string(),
                reasoning: false,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 8_192,
                max_tokens: 2_048,
                headers: HashMap::new(),
            },
            api_key: Some("   ".to_string()),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        };

        let json = extension_model_from_entry(&entry);
        assert_eq!(json["apiKeyPresent"], false);
    }

    // ── last_assistant_message ───────────────────────────────────────────

    #[test]
    fn last_assistant_message_found() {
        let messages = vec![
            ModelMessage::User(UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: 0,
            }),
            ModelMessage::assistant(AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("hi".to_string()))],
                api: "openai-completions".to_string(),
                provider: "openai".to_string(),
                model: "gpt-4o-mini".to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            }),
        ];
        let result = last_assistant_message(&messages);
        assert!(result.is_some());
    }

    #[test]
    fn last_assistant_message_none_when_empty() {
        let messages: Vec<ModelMessage> = Vec::new();
        assert!(last_assistant_message(&messages).is_none());
    }

    #[test]
    fn last_assistant_message_none_when_no_assistant() {
        let messages = vec![ModelMessage::User(UserMessage {
            content: UserContent::Text("hello".to_string()),
            timestamp: 0,
        })];
        assert!(last_assistant_message(&messages).is_none());
    }
}
