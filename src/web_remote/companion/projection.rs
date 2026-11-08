//! Complete semantic values are screened before clipping or browser encoding.

use crate::model::{BillingUsage, ContentBlock, Message, UserContent};
use crate::text_completion::{AuxiliaryPrivacySnapshot, MAX_INPUT_BYTES};
use serde::Serialize;
use serde_json::Value;
use std::io;

const MAX_MESSAGES: usize = 48;
const MAX_MESSAGE_INPUT: usize = 32 * 1024;
const MAX_MESSAGE_OUTPUT: usize = 8 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 256 * 1024;
const MAX_DISCOVERY_ITEMS: usize = 4096;
const MAX_DISCOVERY_DEPTH: usize = 64;
const OMITTED: &str = "[message omitted: browser privacy scan budget]";

#[derive(Serialize)]
struct Snapshot {
    name: String,
    model: String,
    messages: Vec<BrowserMessage>,
    omitted: bool,
}

#[derive(Serialize)]
struct BrowserMessage {
    role: &'static str,
    text: String,
}

/// The display may hide a tool result or omit a large message, but that source
/// can still explain a credential echoed in another visible message. Discover
/// from complete semantic fields first, on a disposable privacy snapshot.
struct Discovery {
    fields: Vec<Value>,
    joined_fields: Vec<Value>,
    bytes_left: usize,
    items_left: usize,
}

impl Discovery {
    fn new() -> Self {
        Self {
            fields: Vec::new(),
            joined_fields: Vec::new(),
            bytes_left: MAX_INPUT_BYTES,
            // Include both discovery context arrays in the structural bound.
            items_left: MAX_DISCOVERY_ITEMS - 2,
        }
    }

    fn items(&mut self, count: usize) -> Option<()> {
        self.items_left = self.items_left.checked_sub(count)?;
        Some(())
    }

    fn bytes(&mut self, count: usize) -> Option<()> {
        self.bytes_left = self.bytes_left.checked_sub(count)?;
        Some(())
    }

    fn text(&mut self, text: &str) -> Option<()> {
        self.items(1)?;
        self.bytes(text.len())?;
        self.fields.push(Value::String(text.to_string()));
        Some(())
    }

    fn fragments(&mut self, parts: &[&str]) -> Option<()> {
        for part in parts {
            self.text(part)?;
        }
        if parts.len() > 1 {
            self.items(1)?;
            let bytes = parts
                .iter()
                .try_fold(0usize, |bytes, part| bytes.checked_add(part.len()))?;
            // The complete fields above already paid for these source bytes.
            // A second bounded view catches credentials split over blocks,
            // without losing assignments completed at a field boundary.
            let mut joined = String::with_capacity(bytes);
            for part in parts {
                joined.push_str(part);
            }
            self.joined_fields.push(Value::String(joined));
        }
        Some(())
    }

    fn json(&mut self, value: &Value) -> Option<()> {
        self.json_shape(value, 0)?;
        // Measure without allocating a serialized copy. Only clone a JSON
        // subtree after its complete bytes, item count and depth are bounded.
        let mut writer = ScanWriter {
            bytes_left: self.bytes_left,
        };
        serde_json::to_writer(&mut writer, value).ok()?;
        self.bytes_left = writer.bytes_left;
        self.fields.push(value.clone());
        Some(())
    }

    fn json_shape(&mut self, value: &Value, depth: usize) -> Option<()> {
        if depth > MAX_DISCOVERY_DEPTH {
            return None;
        }
        self.items(1)?;
        match value {
            Value::String(text) if text.len() > self.bytes_left => return None,
            Value::Array(values) => {
                if values.len() > self.items_left {
                    return None;
                }
                for value in values {
                    self.json_shape(value, depth + 1)?;
                }
            }
            Value::Object(values) => {
                if values.len().checked_mul(2)? > self.items_left {
                    return None;
                }
                for (key, value) in values {
                    self.items(1)?;
                    if key.len() > self.bytes_left {
                        return None;
                    }
                    self.json_shape(value, depth + 1)?;
                }
            }
            _ => {}
        }
        Some(())
    }

    fn blocks(&mut self, blocks: &[ContentBlock]) -> Option<()> {
        // Bound the walk even when every block contains only opaque data.
        self.items(blocks.len().checked_add(1)?)?;
        let mut text = Vec::new();
        let mut thinking = Vec::new();
        for block in blocks {
            match block {
                // Match the display's concatenation so a credential split
                // over text blocks is still a complete discovery input.
                ContentBlock::Text(content) => text.push(content.text.as_str()),
                ContentBlock::Thinking(content) => {
                    thinking.push(content.thinking.as_str());
                }
                ContentBlock::ToolCall(call) => {
                    self.text(&call.id)?;
                    self.text(&call.name)?;
                    self.json(&call.arguments)?;
                }
                ContentBlock::Image(content) => self.text(&content.mime_type)?,
                ContentBlock::Media(content) => {
                    self.text(&content.mime_type)?;
                    if let Some(name) = &content.name {
                        self.text(name)?;
                    }
                }
                // Payload bytes, signatures and provider-redacted reasoning
                // are opaque, never semantic discovery or display inputs.
                ContentBlock::RedactedThinking(_) => {}
            }
        }
        self.fragments(&text)?;
        self.fragments(&thinking)
    }

    fn message(&mut self, message: &Message) -> Option<()> {
        self.items(1)?;
        match message {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => self.text(text)?,
                UserContent::Blocks(blocks) => self.blocks(blocks)?,
            },
            Message::Assistant(assistant) => {
                self.blocks(&assistant.content)?;
                self.text(&assistant.api)?;
                self.text(&assistant.provider)?;
                self.text(&assistant.model)?;
                if let Some(error) = &assistant.error_message {
                    self.text(error)?;
                }
                if let Some(BillingUsage::Unavailable { reason }) = &assistant.usage.billing {
                    self.text(reason)?;
                }
                if let Some(details) = &assistant.stop_details {
                    self.text(&details.kind)?;
                    if let Some(category) = &details.category {
                        self.text(category)?;
                    }
                    if let Some(explanation) = &details.explanation {
                        self.text(explanation)?;
                    }
                }
            }
            Message::ToolResult(result) => {
                self.text(&result.tool_call_id)?;
                self.text(&result.tool_name)?;
                self.blocks(&result.content)?;
                if let Some(details) = &result.details {
                    self.json(details)?;
                }
            }
            Message::Custom(custom) => {
                self.text(&custom.custom_type)?;
                self.text(&custom.content)?;
                if let Some(details) = &custom.details {
                    self.json(details)?;
                }
            }
        }
        Some(())
    }
}

struct ScanWriter {
    bytes_left: usize,
}

impl io::Write for ScanWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes_left = self
            .bytes_left
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("browser privacy scan budget exceeded"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn discovered_privacy(
    name: &str,
    model: &str,
    messages: &[Message],
    privacy: &AuxiliaryPrivacySnapshot,
) -> Option<AuxiliaryPrivacySnapshot> {
    let mut discovery = Discovery::new();
    discovery.text(name)?;
    discovery.text(model)?;
    for message in messages {
        discovery.message(message)?;
    }
    let mut privacy = privacy.clone();
    privacy.capture_json(&Value::Array(discovery.fields)).ok()?;
    privacy
        .capture_json(&Value::Array(discovery.joined_fields))
        .ok()?;
    Some(privacy)
}

fn blocks_text(blocks: &[ContentBlock]) -> Option<String> {
    if blocks.len() > 1024 {
        return None;
    }
    // Discovery already inspected the complete semantic source. Apply the
    // smaller display budget now, before copying the visible text again.
    let bytes = blocks.iter().try_fold(0usize, |total, block| {
        let length = match block {
            ContentBlock::Text(text) => text.text.len(),
            ContentBlock::Image(_) | ContentBlock::Media(_) => 32,
            _ => 0,
        };
        total.checked_add(length)
    })?;
    if bytes > MAX_MESSAGE_INPUT {
        return None;
    }
    let mut text = String::with_capacity(bytes);
    let mut attachments = false;
    for block in blocks {
        match block {
            ContentBlock::Text(content) => text.push_str(&content.text),
            ContentBlock::Image(_) | ContentBlock::Media(_) => {
                attachments = true;
            }
            // Hidden semantic fields were used for discovery, but they are
            // never part of the companion's visible conversation.
            _ => {}
        }
    }
    if attachments {
        text.push_str("\n[attachment stays local]\n");
    }
    Some(text)
}

fn message_text(message: &Message) -> Option<BrowserMessage> {
    let (role, text) = match message {
        Message::User(user) => (
            "user",
            match &user.content {
                UserContent::Text(text) if text.len() <= MAX_MESSAGE_INPUT => Some(text.clone()),
                UserContent::Text(_) => None,
                UserContent::Blocks(blocks) => blocks_text(blocks),
            },
        ),
        Message::Assistant(assistant) => ("assistant", blocks_text(&assistant.content)),
        Message::ToolResult(result) => (
            "tool",
            Some(if result.is_error {
                "A tool reported an error. Details remain in the local terminal.".to_string()
            } else {
                "Tool completed. Output remains in the local terminal.".to_string()
            }),
        ),
        // Custom/system messages can contain local auth links, extension UI
        // instructions or approval text. Their origin is not a public channel.
        Message::Custom(_) => return None,
    };
    Some(BrowserMessage {
        role,
        text: text.unwrap_or_else(|| OMITTED.to_string()),
    })
}

pub(super) fn project(
    name: &str,
    model: &str,
    messages: &[Message],
    privacy: &AuxiliaryPrivacySnapshot,
) -> String {
    let Some(privacy) = discovered_privacy(name, model, messages, privacy) else {
        // An uninspected hidden field may contain the original assignment for
        // a visible bare echo. Withhold the whole snapshot on any scan limit.
        return encode(withheld());
    };
    // A display tail is not a privacy boundary: older assignments must still
    // protect visible echoes when the primary agent's vault is disabled.
    let selected = &messages[messages.len().saturating_sub(MAX_MESSAGES)..];
    let mut retained = Vec::new();
    let mut input_bytes = name.len().saturating_add(model.len());
    let mut omitted = messages.len() > MAX_MESSAGES;
    for message in selected.iter().rev() {
        let Some(mut message) = message_text(message) else {
            continue;
        };
        if input_bytes.saturating_add(message.text.len()) > MAX_INPUT_BYTES {
            message.text = OMITTED.to_string();
        }
        omitted |= message.text == OMITTED;
        input_bytes = input_bytes.saturating_add(message.text.len());
        retained.push(message);
    }
    retained.reverse();
    let mut parts = vec![name, model];
    parts.extend(retained.iter().map(|message| message.text.as_str()));
    // The private snapshot now knows hidden and omitted source discoveries;
    // the frozen agent snapshot also protects values learned before this tail.
    let protected = privacy.project_inputs(&parts);
    let snapshot = match protected {
        Ok(protected) => {
            let mut values = protected.into_iter();
            let name = clipped(values.next().unwrap_or_default(), 512);
            let model = clipped(values.next().unwrap_or_default(), 512);
            for (message, text) in retained.iter_mut().zip(values) {
                omitted |= text.len() > MAX_MESSAGE_OUTPUT;
                message.text = clipped(text, MAX_MESSAGE_OUTPUT);
            }
            Snapshot {
                name,
                model,
                messages: retained,
                omitted,
            }
        }
        Err(_) => withheld(),
    };
    encode(snapshot)
}

fn withheld() -> Snapshot {
    Snapshot {
        name: "Pi session".to_string(),
        model: String::new(),
        messages: vec![BrowserMessage {
            role: "notice",
            text: "Conversation withheld by the session's privacy policy.".to_string(),
        }],
        omitted: true,
    }
}

fn encode(snapshot: Snapshot) -> String {
    let encoded = serde_json::to_string(&snapshot).unwrap_or_default();
    if !encoded.is_empty() && encoded.len() <= MAX_SNAPSHOT_BYTES {
        encoded
    } else {
        String::from(
            r#"{"name":"Pi session","model":"","messages":[{"role":"notice","text":"Conversation exceeds the browser snapshot budget."}],"omitted":true}"#,
        )
    }
}

fn clipped(mut text: String, bytes: usize) -> String {
    if text.len() > bytes {
        let mut end = bytes.saturating_sub(3);
        while !text.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        text.truncate(end);
        text.push_str("...");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AssistantMessage, CustomMessage, ImageContent, MediaContent, RedactedThinkingContent,
        TextContent, ThinkingContent, ToolCall, ToolResultMessage, Usage, UserMessage,
    };
    use crate::text_completion::AuxiliaryPrivacy;
    use serde_json::json;

    fn privacy() -> AuxiliaryPrivacySnapshot {
        AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::from_settings(Some(
            &crate::secrets::SecretsSettings {
                mode: Some("off".to_string()),
                extra_patterns: None,
            },
        )))
    }

    fn user(text: impl Into<String>) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp: 0,
        })
    }

    fn assistant(blocks: Vec<ContentBlock>) -> Message {
        Message::assistant(AssistantMessage {
            content: blocks,
            ..AssistantMessage::default()
        })
    }

    fn echo(secret: &str) -> Message {
        assistant(vec![ContentBlock::Text(TextContent::new(format!(
            "Visible reply echoes {secret}."
        )))])
    }

    fn tool(content: Vec<ContentBlock>, details: Option<Value>) -> Message {
        Message::tool_result(ToolResultMessage {
            tool_call_id: "private-call-id".to_string(),
            tool_name: "read".to_string(),
            content,
            details,
            is_error: false,
            timestamp: 0,
        })
    }

    fn custom(content: &str, details: Option<Value>) -> Message {
        Message::Custom(CustomMessage {
            content: content.to_string(),
            custom_type: "private-extension-note".to_string(),
            display: false,
            details,
            timestamp: 0,
        })
    }

    fn assert_masked(encoded: &str, secret: &str) {
        assert!(!encoded.contains(secret), "unredacted source credential");
        assert!(encoded.contains("pi-secret:redacted"));
        assert!(encoded.contains("Visible reply echoes"));
        assert!(!encoded.contains("withheld"));
    }

    #[test]
    fn tool_result_assignment_is_discovered_before_its_generic_display_label() {
        let secret = "toolOutputCredentialValue123456";
        let messages = [
            tool(
                vec![ContentBlock::Text(TextContent::new(format!(
                    "private-tool-source\nAPI_KEY={secret}"
                )))],
                None,
            ),
            echo(secret),
        ];
        let encoded = project("session", "model", &messages, &privacy());
        assert_masked(&encoded, secret);
        assert!(encoded.contains("Tool completed. Output remains in the local terminal."));
        assert!(!encoded.contains("private-tool-source"));
        assert!(!encoded.contains("private-call-id"));
    }

    #[test]
    fn hidden_structured_fields_and_thinking_protect_earlier_visible_echoes() {
        let secret = "hiddenFieldCredentialValue123456";
        let assignment = format!("API_KEY={secret}");
        let hidden_sources = [
            tool(Vec::new(), Some(json!({"api_key": secret}))),
            assistant(vec![ContentBlock::Thinking(ThinkingContent {
                thinking: assignment.clone(),
                thinking_signature: None,
            })]),
            Message::assistant(AssistantMessage {
                usage: Usage {
                    billing: Some(BillingUsage::Unavailable {
                        reason: assignment.clone(),
                    }),
                    ..Usage::default()
                },
                ..AssistantMessage::default()
            }),
            assistant(vec![ContentBlock::ToolCall(ToolCall {
                id: "private-tool-call".to_string(),
                name: "bash".to_string(),
                arguments: json!({"credentials": {"api_key": secret}}),
                thought_signature: None,
            })]),
            custom(&assignment, None),
            custom("private-custom-source", Some(json!({"api_key": secret}))),
        ];
        for source in hidden_sources {
            let encoded = project("session", "model", &[echo(secret), source], &privacy());
            assert_masked(&encoded, secret);
            assert!(!encoded.contains("private-custom-source"));
            assert!(!encoded.contains("private-extension-note"));
            assert!(!encoded.contains("private-tool-call"));
        }
    }

    #[test]
    fn omitted_large_message_still_protects_a_visible_bare_echo() {
        let secret = "omittedMessageCredentialValue123456";
        let source = format!("{}\nAPI_KEY={secret}", "x".repeat(40 * 1024));
        let messages = [echo(secret), user(source)];
        let encoded = project("session", "model", &messages, &privacy());
        assert_masked(&encoded, secret);
        assert!(encoded.contains(OMITTED));
        assert_eq!(
            serde_json::from_str::<Value>(&encoded).unwrap()["omitted"],
            true
        );
    }

    #[test]
    fn hidden_assignment_before_the_display_tail_protects_a_visible_echo() {
        let secret = "olderHiddenCredentialValue123456";
        for source in [
            tool(Vec::new(), Some(json!({"api_key": secret}))),
            custom(&format!("API_KEY={secret}"), None),
        ] {
            let mut messages = vec![source];
            messages.extend((0..MAX_MESSAGES).map(|_| user("Ordinary historical message.")));
            messages.push(echo(secret));

            // privacy() uses secrets.mode=off and an empty live-agent vault.
            // Discovery must include the hidden source outside the shown tail.
            let encoded = project("session", "model", &messages, &privacy());
            assert_masked(&encoded, secret);
            let snapshot = serde_json::from_str::<Value>(&encoded).unwrap();
            assert_eq!(snapshot["messages"].as_array().unwrap().len(), MAX_MESSAGES);
            assert_eq!(snapshot["omitted"], true);
            assert!(!encoded.contains("private-extension-note"));
            assert!(!encoded.contains("private-call-id"));
        }
    }

    #[test]
    fn history_outside_the_display_tail_still_obeys_complete_source_bounds() {
        let secret = "unscannedOlderCredentialValue123456";
        for source in [
            user(format!("{}\nAPI_KEY={secret}", "x".repeat(MAX_INPUT_BYTES))),
            custom(
                "",
                Some(Value::Array(vec![Value::Null; MAX_DISCOVERY_ITEMS + 1])),
            ),
        ] {
            let mut messages = vec![source];
            messages.extend((0..MAX_MESSAGES).map(|_| user("Ordinary message.")));
            messages.push(echo(secret));

            let encoded = project("session", "model", &messages, &privacy());
            assert!(encoded.contains("Conversation withheld"));
            assert!(!encoded.contains(secret));
            assert!(!encoded.contains("Visible reply echoes"));
        }

        // Individually small messages must also share one discovery budget.
        // The visible 48-message tail fits, but the complete source does not.
        let messages = (0..100)
            .map(|_| user("x".repeat(3 * 1024)))
            .collect::<Vec<_>>();
        let encoded = project("session", "model", &messages, &privacy());
        assert!(encoded.contains("Conversation withheld"));
        assert_eq!(
            serde_json::from_str::<Value>(&encoded).unwrap()["omitted"],
            true
        );
    }

    #[test]
    fn complete_tool_text_fragments_are_discovered_together() {
        let secret = "fragmentedToolCredentialValue123456";
        let assignment = format!("API_KEY={secret}");
        for content in [
            vec![
                ContentBlock::Text(TextContent::new(&assignment[..12])),
                ContentBlock::Text(TextContent::new(&assignment[12..])),
            ],
            vec![
                ContentBlock::Text(TextContent::new(&assignment)),
                ContentBlock::Text(TextContent::new("suffix")),
            ],
            vec![
                ContentBlock::Thinking(ThinkingContent {
                    thinking: assignment[..12].to_string(),
                    thinking_signature: None,
                }),
                ContentBlock::Thinking(ThinkingContent {
                    thinking: assignment[12..].to_string(),
                    thinking_signature: None,
                }),
            ],
            vec![
                ContentBlock::Thinking(ThinkingContent {
                    thinking: assignment.clone(),
                    thinking_signature: None,
                }),
                ContentBlock::Thinking(ThinkingContent {
                    thinking: "suffix".to_string(),
                    thinking_signature: None,
                }),
            ],
        ] {
            let messages = [tool(content, None), echo(secret)];
            let encoded = project("session", "model", &messages, &privacy());
            assert_masked(&encoded, secret);
            assert!(!encoded.contains("mentedToolCredentialValue123456"));
        }
    }

    #[test]
    fn metadata_is_discovered_before_display_clipping_and_shares_the_source_budget() {
        let secret = "metadataCredentialValue123456";
        let label = format!("{}\nAPI_KEY={secret}", "n".repeat(5000));
        for (name, model) in [(label.as_str(), "model"), ("session", label.as_str())] {
            let encoded = project(name, model, &[echo(secret)], &privacy());
            assert_masked(&encoded, secret);
        }
        let name = "n".repeat(MAX_INPUT_BYTES / 2 + 1);
        let model = "m".repeat(MAX_INPUT_BYTES / 2 + 1);
        let encoded = project(&name, &model, &[echo(secret)], &privacy());
        assert!(encoded.contains("Conversation withheld"));
        assert!(!encoded.contains(secret));
    }

    #[test]
    fn any_unscannable_semantic_source_withholds_the_entire_snapshot() {
        let secret = "unscannedCredentialValue123456";
        let mut deep = json!({"api_key": secret});
        for _ in 0..=MAX_DISCOVERY_DEPTH {
            deep = Value::Array(vec![deep]);
        }
        let sources = [
            user(format!("{}\nAPI_KEY={secret}", "x".repeat(MAX_INPUT_BYTES))),
            tool(Vec::new(), Some(deep)),
            custom(
                "",
                Some(Value::Array(vec![Value::Null; MAX_DISCOVERY_ITEMS + 1])),
            ),
            tool(
                vec![ContentBlock::Text(TextContent::new("")); MAX_DISCOVERY_ITEMS + 1],
                None,
            ),
            // Escaped JSON can exceed the byte budget even when its raw
            // string length and structural item count are small enough.
            tool(
                Vec::new(),
                Some(json!({"padding": "\u{1}".repeat(MAX_INPUT_BYTES / 3)})),
            ),
        ];
        for source in sources {
            let encoded = project("session", "model", &[echo(secret), source], &privacy());
            assert!(encoded.contains("Conversation withheld"));
            assert!(!encoded.contains(secret));
            assert!(!encoded.contains("Visible reply echoes"));
            assert_eq!(
                serde_json::from_str::<Value>(&encoded).unwrap()["omitted"],
                true
            );
        }
    }

    #[test]
    fn opaque_payloads_and_signatures_do_not_consume_the_semantic_scan_budget() {
        let opaque = "opaque-binary-canary".repeat(MAX_INPUT_BYTES / 10);
        let messages = [assistant(vec![
            ContentBlock::Text(TextContent {
                text: "Public completion.".to_string(),
                text_signature: Some(opaque.clone()),
            }),
            ContentBlock::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: Some(opaque.clone()),
            }),
            ContentBlock::RedactedThinking(RedactedThinkingContent {
                data: opaque.clone(),
            }),
            ContentBlock::Image(ImageContent {
                data: opaque.clone(),
                mime_type: "image/png".to_string(),
            }),
            ContentBlock::Media(MediaContent {
                data: opaque,
                mime_type: "audio/wav".to_string(),
                name: Some("recording.wav".to_string()),
            }),
        ])];
        let encoded = project("session", "model", &messages, &privacy());
        assert!(encoded.contains("Public completion."));
        assert!(encoded.contains("attachment stays local"));
        assert!(!encoded.contains("opaque-binary-canary"));
        assert!(!encoded.contains("withheld"));
    }

    #[test]
    fn discovery_does_not_mutate_the_frozen_agent_privacy_snapshot() {
        let secret = "disposableSnapshotCredentialValue123456";
        let privacy = privacy();
        let encoded = project(
            "session",
            "model",
            &[custom(&format!("API_KEY={secret}"), None), echo(secret)],
            &privacy,
        );
        assert_masked(&encoded, secret);
        let unrelated = project("session", "model", &[echo(secret)], &privacy);
        assert!(unrelated.contains(secret));
    }
}
