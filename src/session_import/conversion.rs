//! Foreign wire records -> native message sequences.
//!
//! One source record can contain several tool results interleaved with text.
//! Conversion therefore returns a sequence, never a single optional message.
//! Unsupported material is reported to the importer for durable preservation;
//! it is not silently filtered out or promoted into active system instructions.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::ImportSource;
use crate::model::{
    AssistantMessage, ContentBlock, CustomMessage, ImageContent, Message, StopReason, TextContent,
    ThinkingContent, ToolCall, ToolResultMessage, UserContent, UserMessage,
};

#[derive(Default)]
pub(super) struct ConvertedEntry {
    pub messages: Vec<Message>,
    pub notes: Vec<String>,
    pub metadata: bool,
}

pub(super) struct ForeignReader {
    source: ImportSource,
    tool_names: HashMap<String, String>,
}

impl ForeignReader {
    pub(super) fn new(source: ImportSource) -> Self {
        Self {
            source,
            tool_names: HashMap::new(),
        }
    }

    pub(super) fn convert(&mut self, entry: &Value) -> ConvertedEntry {
        let mut converted = match self.source {
            ImportSource::Claude => convert_claude(entry),
            ImportSource::Codex => convert_codex(entry),
        };
        // Results identify the call, not necessarily its name. Resolve names
        // across records and across all results in one Claude user envelope.
        for message in &mut converted.messages {
            match message {
                Message::Assistant(assistant) => {
                    for block in &assistant.content {
                        if let ContentBlock::ToolCall(call) = block {
                            self.tool_names
                                .entry(call.id.clone())
                                .or_insert_with(|| call.name.clone());
                        }
                    }
                }
                Message::ToolResult(result) => {
                    if let Some(name) = self.tool_names.get(&result.tool_call_id) {
                        let result = std::sync::Arc::make_mut(result);
                        if !result.tool_name.is_empty() && result.tool_name != *name {
                            converted.notes.push(format!(
                                "tool result name disagrees with call {}",
                                result.tool_call_id
                            ));
                        }
                        result.tool_name.clone_from(name);
                    }
                }
                _ => {}
            }
        }
        converted
    }
}

fn timestamp(entry: &Value) -> i64 {
    entry
        .get("timestamp")
        .or_else(|| entry.pointer("/message/timestamp"))
        .and_then(|value| {
            value.as_i64().or_else(|| {
                value
                    .as_str()
                    .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                    .map(|value| value.timestamp_millis())
            })
        })
        .unwrap_or(0)
}

fn assistant(content: Vec<ContentBlock>, timestamp: i64) -> Message {
    let stop_reason = if content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall(_)))
    {
        StopReason::ToolUse
    } else {
        StopReason::Stop
    };
    Message::assistant(AssistantMessage {
        content,
        timestamp,
        stop_reason,
        ..AssistantMessage::default()
    })
}

fn flush_blocks(role: &str, blocks: &mut Vec<ContentBlock>, ts: i64, out: &mut Vec<Message>) {
    if blocks.is_empty() {
        return;
    }
    let content = std::mem::take(blocks);
    out.push(if role == "assistant" {
        assistant(content, ts)
    } else {
        Message::User(UserMessage {
            content: UserContent::Blocks(content),
            timestamp: ts,
        })
    });
}

fn text_message(role: &str, text: &str, ts: i64) -> Message {
    if role == "assistant" {
        assistant(vec![ContentBlock::Text(TextContent::new(text))], ts)
    } else {
        Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: ts,
        })
    }
}

fn nonempty_string<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

/// Inline images only: an import must never fetch a URL from an old log.
fn image_block(block: &Value) -> Option<ContentBlock> {
    let (mime, data) = if block.get("type").and_then(Value::as_str) == Some("image") {
        let source = block.get("source")?;
        if source.get("type").and_then(Value::as_str) != Some("base64") {
            return None;
        }
        (
            nonempty_string(source, "media_type")?,
            nonempty_string(source, "data")?,
        )
    } else {
        let url = block
            .get("image_url")
            .and_then(|value| value.as_str().or_else(|| value.get("url")?.as_str()))?;
        url.strip_prefix("data:")?.split_once(";base64,")?
    };
    if !mime.starts_with("image/")
        || crate::model::sanitize_image_mime_type(mime) != mime
        || data.is_empty()
    {
        return None;
    }
    Some(ContentBlock::Image(ImageContent {
        data: data.to_string(),
        mime_type: mime.to_string(),
    }))
}

/// Text and inline images shared by messages and structured tool outputs.
fn content_blocks(value: &Value, notes: &mut Vec<String>) -> Vec<ContentBlock> {
    match value {
        Value::String(text) => vec![ContentBlock::Text(TextContent::new(text))],
        Value::Array(items) => {
            let mut blocks = Vec::new();
            for item in items {
                let kind = item.get("type").and_then(Value::as_str);
                match kind {
                    Some("text" | "input_text" | "output_text" | "summary_text") | None => {
                        let text = item
                            .get("text")
                            .or_else(|| item.get("input_text"))
                            .or_else(|| item.get("output_text"))
                            .and_then(Value::as_str);
                        if let Some(text) = text {
                            blocks.push(ContentBlock::Text(TextContent::new(text)));
                        } else {
                            notes.push("content block has no text".to_string());
                        }
                    }
                    Some("image" | "input_image" | "image_url") => {
                        if let Some(image) = image_block(item) {
                            blocks.push(image);
                        } else {
                            notes.push("non-inline or malformed image retained as attachment".into());
                            blocks.push(ContentBlock::Text(TextContent::new(
                                "[Imported image is retained in the source attachment]",
                            )));
                        }
                    }
                    Some(kind) => {
                        notes.push(format!("unsupported content block: {kind}"));
                        blocks.push(ContentBlock::Text(TextContent::new(
                            "[Unsupported imported content is retained in the source attachment]",
                        )));
                    }
                }
            }
            blocks
        }
        // Some older/custom tools wrote JSON values instead of the documented
        // string or content array. Preserve their value as text AND their full
        // original record as an attachment, rather than substituting null.
        other => {
            notes.push("nonstandard content value retained as JSON text".to_string());
            vec![ContentBlock::Text(TextContent::new(other.to_string()))]
        }
    }
}

fn convert_claude(entry: &Value) -> ConvertedEntry {
    let mut converted = ConvertedEntry::default();
    let Some(role @ ("user" | "assistant")) = entry.get("type").and_then(Value::as_str) else {
        converted.metadata = true;
        return converted;
    };
    let ts = timestamp(entry);
    let Some(content) = entry.pointer("/message/content").or_else(|| entry.get("content")) else {
        converted.notes.push("message has no content field".into());
        return converted;
    };
    if let Some(text) = content.as_str() {
        if !text.is_empty() {
            converted.messages.push(text_message(role, text, ts));
        }
        return converted;
    }
    let Some(blocks) = content.as_array() else {
        converted.notes.push("message content is not text or an array".into());
        return converted;
    };
    let mut pending = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_result") if role == "user" => {
                flush_blocks(role, &mut pending, ts, &mut converted.messages);
                let Some(id) = nonempty_string(block, "tool_use_id") else {
                    converted.notes.push("tool result has no tool_use_id".into());
                    continue;
                };
                let Some(output) = block.get("content") else {
                    converted.notes.push(format!("tool result {id} has no content"));
                    continue;
                };
                let content = content_blocks(output, &mut converted.notes);
                converted.messages.push(Message::tool_result(ToolResultMessage {
                    tool_call_id: id.to_string(),
                    tool_name: String::new(),
                    content,
                    is_error: block.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                    timestamp: ts,
                    details: None,
                }));
            }
            Some("tool_use") if role == "assistant" => {
                let (Some(id), Some(name), Some(input)) = (
                    nonempty_string(block, "id"),
                    nonempty_string(block, "name"),
                    block.get("input").filter(|value| value.is_object()),
                ) else {
                    converted.notes.push("tool_use requires id, name and object input".into());
                    continue;
                };
                pending.push(ContentBlock::ToolCall(ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments: input.clone(),
                    thought_signature: None,
                }));
            }
            Some("thinking") if role == "assistant" => {
                if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                    pending.push(ContentBlock::Thinking(ThinkingContent {
                        thinking: text.to_string(),
                        // Provider-bound signatures cannot authorize imported
                        // reasoning in a different provider/session.
                        thinking_signature: None,
                    }));
                } else {
                    converted.notes.push("thinking block has no thinking text".into());
                }
                if block.get("signature").is_some() {
                    converted.notes.push("thinking signature retained only in source attachment".into());
                }
            }
            Some("text" | "image") => {
                pending.extend(content_blocks(&json!([block]), &mut converted.notes));
            }
            Some(kind) => {
                converted.notes.push(format!("unsupported {role} block: {kind}"));
            }
            None => converted.notes.push("content block has no type".into()),
        }
    }
    flush_blocks(role, &mut pending, ts, &mut converted.messages);
    converted
}

fn convert_codex(entry: &Value) -> ConvertedEntry {
    let mut converted = ConvertedEntry::default();
    if entry.get("type").and_then(Value::as_str) != Some("response_item") {
        // event_msg repeats response content and must not create duplicate
        // conversational messages. All envelopes still survive as metadata.
        converted.metadata = true;
        return converted;
    }
    let Some(payload) = entry.get("payload").filter(|value| value.is_object()) else {
        converted.notes.push("response_item has no object payload".into());
        return converted;
    };
    let ts = timestamp(entry);
    match payload.get("type").and_then(Value::as_str) {
        Some("message") => {
            let role = payload.get("role").and_then(Value::as_str).unwrap_or("user");
            let Some(content) = payload.get("content") else {
                converted.notes.push("message has no content".into());
                return converted;
            };
            if matches!(role, "system" | "developer") {
                // Foreign instructions are historical context, never the new
                // session's active system prompt or an unlabelled user request.
                converted.metadata = true;
                let content = content_blocks(content, &mut converted.notes);
                let text = content.iter().filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                }).collect::<Vec<_>>().join("\n");
                converted.messages.push(Message::Custom(CustomMessage {
                    content: format!("[Historical {role} instructions from Codex; not active session instructions]\n{text}"),
                    custom_type: "foreign_instructions".to_string(),
                    display: false,
                    details: Some(json!({"sourceRole": role})),
                    timestamp: ts,
                }));
            } else if matches!(role, "user" | "assistant") {
                if let Some(text) = content.as_str() {
                    if !text.is_empty() {
                        converted.messages.push(text_message(role, text, ts));
                    }
                } else {
                    let mut blocks = content_blocks(content, &mut converted.notes);
                    flush_blocks(role, &mut blocks, ts, &mut converted.messages);
                }
            } else {
                converted.notes.push(format!("unsupported message role: {role}"));
            }
        }
        Some("reasoning") => {
            let mut blocks = Vec::new();
            for key in ["summary", "content"] {
                if let Some(items) = payload.get(key).filter(|value| !value.is_null()) {
                    for block in content_blocks(items, &mut converted.notes) {
                        if let ContentBlock::Text(text) = block {
                            if !text.text.is_empty() {
                                blocks.push(ContentBlock::Thinking(ThinkingContent {
                                    thinking: text.text,
                                    thinking_signature: None,
                                }));
                            }
                        } else {
                            converted.notes.push("non-text reasoning retained as attachment".into());
                        }
                    }
                }
            }
            if payload.get("encrypted_content").is_some_and(|value| !value.is_null()) {
                converted.notes.push("encrypted reasoning retained only in source attachment".into());
            }
            if !blocks.is_empty() {
                converted.messages.push(assistant(blocks, ts));
            }
        }
        Some(kind @ ("function_call" | "custom_tool_call")) => {
            let (Some(name), Some(id)) = (
                nonempty_string(payload, "name"),
                nonempty_string(payload, "call_id"),
            ) else {
                converted.notes.push(format!("{kind} requires name and call_id"));
                return converted;
            };
            let arguments = if kind == "custom_tool_call" {
                let Some(input) = payload.get("input").and_then(Value::as_str) else {
                    converted.notes.push("custom_tool_call has no text input".into());
                    return converted;
                };
                // Custom tools carry arbitrary text, not a JSON arguments
                // string. Preserve it in an explicit input field for native
                // history; importing never executes the foreign tool.
                json!({"input": input})
            } else {
                let arguments = match payload.get("arguments") {
                    Some(Value::String(raw)) => serde_json::from_str::<Value>(raw).ok(),
                    Some(value @ Value::Object(_)) => Some(value.clone()),
                    _ => None,
                };
                let Some(arguments) = arguments.filter(Value::is_object) else {
                    converted.notes.push(format!("function call {id} has invalid JSON object arguments"));
                    return converted;
                };
                arguments
            };
            converted.messages.push(assistant(vec![ContentBlock::ToolCall(ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments,
                thought_signature: None,
            })], ts));
        }
        Some(kind @ ("function_call_output" | "custom_tool_call_output")) => {
            let Some(id) = nonempty_string(payload, "call_id") else {
                converted.notes.push(format!("{kind} has no call_id"));
                return converted;
            };
            let Some(output) = payload.get("output") else {
                converted.notes.push(format!("tool output {id} has no output field"));
                return converted;
            };
            let content = content_blocks(output, &mut converted.notes);
            let is_error = payload.get("is_error").and_then(Value::as_bool)
                .unwrap_or_else(|| payload.get("success").and_then(Value::as_bool) == Some(false));
            converted.messages.push(Message::tool_result(ToolResultMessage {
                tool_call_id: id.to_string(),
                tool_name: payload.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                content,
                is_error,
                timestamp: ts,
                details: None,
            }));
        }
        Some(kind) => converted.notes.push(format!("unsupported response_item: {kind}")),
        None => converted.notes.push("response_item payload has no type".into()),
    }
    converted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codex(payload: Value) -> Value {
        json!({"type": "response_item", "timestamp": "2026-01-01T00:00:01Z", "payload": payload})
    }

    fn call(reader: &mut ForeignReader, id: &str, name: &str) {
        let entry = match reader.source {
            ImportSource::Claude => json!({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "id": id, "name": name, "input": {}}
            ]}}),
            ImportSource::Codex => codex(json!({"type": "function_call", "call_id": id, "name": name, "arguments": "{}"})),
        };
        let converted = reader.convert(&entry);
        assert!(converted.notes.is_empty(), "{:?}", converted.notes);
    }

    fn result(message: &Message) -> &ToolResultMessage {
        match message {
            Message::ToolResult(result) => result,
            _ => panic!("expected tool result, got {message:?}"),
        }
    }

    fn text(blocks: &[ContentBlock]) -> Vec<&str> {
        blocks.iter().filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        }).collect()
    }

    #[test]
    fn claude_imports_every_batched_tool_result_with_names_and_error_flags() {
        let mut reader = ForeignReader::new(ImportSource::Claude);
        call(&mut reader, "a", "read");
        call(&mut reader, "b", "bash");
        let converted = reader.convert(&json!({"type": "user", "timestamp": 17, "message": {"content": [
            {"type": "tool_result", "tool_use_id": "b", "content": "stderr\n", "is_error": true},
            {"type": "tool_result", "tool_use_id": "a", "content": [{"type": "text", "text": "first"}, {"type": "text", "text": "second"}]}
        ]}}));
        assert_eq!(converted.messages.len(), 2);
        assert!(converted.notes.is_empty());
        let first = result(&converted.messages[0]);
        assert_eq!((&*first.tool_call_id, &*first.tool_name), ("b", "bash"));
        assert!(first.is_error);
        assert_eq!(first.timestamp, 17);
        assert_eq!(text(&first.content), ["stderr\n"]);
        let second = result(&converted.messages[1]);
        assert_eq!((&*second.tool_call_id, &*second.tool_name), ("a", "read"));
        assert!(!second.is_error);
        assert_eq!(text(&second.content), ["first", "second"]);
    }

    #[test]
    fn claude_mixed_text_and_results_are_not_mutually_exclusive() {
        let mut reader = ForeignReader::new(ImportSource::Claude);
        call(&mut reader, "a", "read");
        let converted = reader.convert(&json!({"type": "user", "message": {"content": [
            {"type": "text", "text": "before"},
            {"type": "tool_result", "tool_use_id": "a", "content": "contents"},
            {"type": "text", "text": "after"}
        ]}}));
        assert_eq!(converted.messages.len(), 3);
        assert!(matches!(&converted.messages[0], Message::User(_)));
        assert_eq!(text(&result(&converted.messages[1]).content), ["contents"]);
        assert!(matches!(&converted.messages[2], Message::User(_)));
    }

    #[test]
    fn codex_function_outputs_round_trip_plain_and_structured_content() {
        let mut reader = ForeignReader::new(ImportSource::Codex);
        call(&mut reader, "a", "exec_command");
        for output in [json!("  exact\nbytes\t"), json!([{"type": "input_text", "text": "  exact\nbytes\t"}])] {
            let converted = reader.convert(&codex(json!({"type": "function_call_output", "call_id": "a", "output": output})));
            assert!(converted.notes.is_empty(), "{:?}", converted.notes);
            let result = result(&converted.messages[0]);
            assert_eq!(result.tool_name, "exec_command");
            assert_eq!(text(&result.content), ["  exact\nbytes\t"]);
            assert_eq!(result.timestamp, 1_767_225_601_000);
        }
    }

    #[test]
    fn codex_custom_tools_keep_arbitrary_input_and_their_output() {
        let mut reader = ForeignReader::new(ImportSource::Codex);
        let patch = "*** Begin Patch\n*** Add File: x\n+hello\n*** End Patch";
        let converted = reader.convert(&codex(json!({"type": "custom_tool_call", "call_id": "p", "name": "apply_patch", "input": patch})));
        let Message::Assistant(message) = &converted.messages[0] else { panic!("assistant") };
        let ContentBlock::ToolCall(call) = &message.content[0] else { panic!("tool call") };
        assert_eq!(call.arguments, json!({"input": patch}));
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        let converted = reader.convert(&codex(json!({"type": "custom_tool_call_output", "call_id": "p", "output": "applied", "success": false})));
        let result = result(&converted.messages[0]);
        assert_eq!(result.tool_name, "apply_patch");
        assert!(result.is_error);
        assert_eq!(text(&result.content), ["applied"]);
    }

    #[test]
    fn malformed_function_arguments_are_not_replaced_by_null_calls() {
        let mut reader = ForeignReader::new(ImportSource::Codex);
        for arguments in [json!("{bad"), json!("null"), json!("[]"), Value::Null] {
            let converted = reader.convert(&codex(json!({"type": "function_call", "call_id": "a", "name": "read", "arguments": arguments})));
            assert!(converted.messages.is_empty());
            assert!(!converted.notes.is_empty());
        }
    }

    #[test]
    fn object_arguments_are_supported_without_reencoding() {
        let mut reader = ForeignReader::new(ImportSource::Codex);
        let converted = reader.convert(&codex(json!({"type": "function_call", "call_id": "a", "name": "read", "arguments": {"path": "x"}})));
        let Message::Assistant(message) = &converted.messages[0] else { panic!("assistant") };
        let ContentBlock::ToolCall(call) = &message.content[0] else { panic!("tool call") };
        assert_eq!(call.arguments, json!({"path": "x"}));
        assert!(converted.notes.is_empty());
    }

    #[test]
    fn partial_unknown_claude_content_keeps_known_messages_and_requests_attachment() {
        let mut reader = ForeignReader::new(ImportSource::Claude);
        let converted = reader.convert(&json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "kept"},
            {"type": "future_block", "opaque": "must survive"}
        ]}}));
        assert_eq!(converted.messages.len(), 1);
        assert_eq!(converted.notes.len(), 1);
        let Message::Assistant(message) = &converted.messages[0] else { panic!("assistant") };
        assert_eq!(text(&message.content), ["kept"]);
    }

    #[test]
    fn missing_identifiers_do_not_create_empty_id_tool_messages() {
        for source in [ImportSource::Claude, ImportSource::Codex] {
            let mut reader = ForeignReader::new(source);
            let entry = match source {
                ImportSource::Claude => json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "output"}]}}),
                ImportSource::Codex => codex(json!({"type": "function_call_output", "output": "output"})),
            };
            let converted = reader.convert(&entry);
            assert!(converted.messages.is_empty());
            assert!(!converted.notes.is_empty());
        }
    }

    #[test]
    fn inline_images_survive_in_tool_results_without_fetching_remote_urls() {
        let mut reader = ForeignReader::new(ImportSource::Codex);
        call(&mut reader, "a", "view_image");
        let converted = reader.convert(&codex(json!({"type": "function_call_output", "call_id": "a", "output": [
            {"type": "input_image", "image_url": "data:image/png;base64,aGVsbG8="},
            {"type": "input_image", "image_url": "https://example.invalid/private"}
        ]})));
        let result = result(&converted.messages[0]);
        assert!(matches!(&result.content[0], ContentBlock::Image(image) if image.data == "aGVsbG8=" && image.mime_type == "image/png"));
        assert!(matches!(&result.content[1], ContentBlock::Text(_)));
        assert_eq!(converted.notes.len(), 1);
    }

    #[test]
    fn foreign_system_and_developer_instructions_are_explicit_historical_context() {
        let mut reader = ForeignReader::new(ImportSource::Codex);
        for role in ["system", "developer"] {
            let converted = reader.convert(&codex(json!({"type": "message", "role": role, "content": [{"type": "input_text", "text": "old instructions"}]})));
            let Message::Custom(message) = &converted.messages[0] else { panic!("historical context") };
            assert_eq!(message.custom_type, "foreign_instructions");
            assert!(message.content.contains("not active session instructions"));
            assert!(message.content.ends_with("old instructions"));
            assert!(converted.metadata);
        }
    }

    #[test]
    fn codex_events_and_metadata_are_not_duplicate_user_messages() {
        let mut reader = ForeignReader::new(ImportSource::Codex);
        for kind in ["event_msg", "session_meta", "turn_context"] {
            let converted = reader.convert(&json!({"type": kind, "payload": {"message": "already present"}}));
            assert!(converted.messages.is_empty());
            assert!(converted.metadata);
        }
    }

    #[test]
    fn opaque_reasoning_and_unknown_response_items_are_reported() {
        let mut reader = ForeignReader::new(ImportSource::Codex);
        let reasoning = reader.convert(&codex(json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "summary"}], "encrypted_content": "opaque"})));
        assert_eq!(reasoning.messages.len(), 1);
        assert_eq!(reasoning.notes.len(), 1);
        let unknown = reader.convert(&codex(json!({"type": "future_item", "payload": "retain"})));
        assert!(unknown.messages.is_empty());
        assert_eq!(unknown.notes.len(), 1);
    }
}
