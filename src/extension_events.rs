//! Typed extension event definitions + dispatch helper.
//!
//! This module defines the JSON-serializable event payloads that can be sent to
//! JavaScript extensions via the `dispatch_event` hook system.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::extensions::{EXTENSION_EVENT_TIMEOUT_MS, ExtensionRuntimeHandle};
use crate::model::{
    AssistantMessage, ContentBlock, CustomMessage, ImageContent, Message, ToolResultMessage,
};

/// Events that can be dispatched to extension handlers.
///
/// The serialized representation is tagged with `type` in `snake_case`, matching
/// the string event name used by JS hooks (e.g. `"tool_call"`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtensionEvent {
    /// Agent startup (once per session).
    #[serde(rename_all = "camelCase")]
    Startup {
        version: String,
        session_file: Option<String>,
    },

    /// Before first API call in a run.
    #[serde(rename_all = "camelCase")]
    AgentStart { session_id: String },

    /// After agent loop ends.
    #[serde(rename_all = "camelCase")]
    AgentEnd {
        session_id: String,
        messages: Vec<Message>,
        error: Option<String>,
    },

    /// Before provider.stream() call.
    #[serde(rename_all = "camelCase")]
    TurnStart {
        session_id: String,
        turn_index: usize,
    },

    /// After response processed.
    #[serde(rename_all = "camelCase")]
    TurnEnd {
        session_id: String,
        turn_index: usize,
        message: Box<AssistantMessage>,
        tool_results: Vec<ToolResultMessage>,
    },

    /// Before tool execution (can block).
    #[serde(rename_all = "camelCase")]
    ToolCall {
        tool_name: String,
        tool_call_id: String,
        input: Value,
    },

    /// After tool execution (can modify result).
    #[serde(rename_all = "camelCase")]
    ToolResult {
        tool_name: String,
        tool_call_id: String,
        input: Value,
        content: Vec<ContentBlock>,
        details: Option<Value>,
        is_error: bool,
    },

    /// Before session switch (can cancel).
    #[serde(rename_all = "camelCase")]
    SessionBeforeSwitch {
        current_session: Option<String>,
        target_session: String,
    },

    /// Before session fork (can cancel).
    #[serde(rename_all = "camelCase")]
    SessionBeforeFork {
        current_session: Option<String>,
        fork_entry_id: String,
    },

    /// Before processing user input (can transform).
    #[serde(rename_all = "camelCase")]
    Input {
        #[serde(rename = "text")]
        content: String,
        #[serde(rename = "images")]
        attachments: Vec<ImageContent>,
    },
}

impl ExtensionEvent {
    /// Get the event name for dispatch.
    #[must_use]
    pub const fn event_name(&self) -> &'static str {
        match self {
            Self::Startup { .. } => "startup",
            Self::AgentStart { .. } => "agent_start",
            Self::AgentEnd { .. } => "agent_end",
            Self::TurnStart { .. } => "turn_start",
            Self::TurnEnd { .. } => "turn_end",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::SessionBeforeSwitch { .. } => "session_before_switch",
            Self::SessionBeforeFork { .. } => "session_before_fork",
            Self::Input { .. } => "input",
        }
    }
}

/// Result from a tool_call event handler.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallEventResult {
    /// If true, block tool execution.
    #[serde(default)]
    pub block: bool,

    /// Reason for blocking (shown to user).
    pub reason: Option<String>,
}

/// Result from a tool_result event handler.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultEventResult {
    /// Modified content (if None, use original).
    pub content: Option<Vec<ContentBlock>>,

    /// Modified details (if None, use original).
    pub details: Option<Value>,
}

/// Result from an input event handler.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InputEventResult {
    /// Transformed content (if None, use original).
    pub content: Option<String>,

    /// If true, block processing.
    #[serde(default)]
    pub block: bool,

    /// Reason for blocking.
    pub reason: Option<String>,
}

/// Result from a before_agent_start event handler.
#[derive(Debug, Clone)]
pub struct BeforeAgentStartOutcome {
    pub messages: Vec<CustomMessage>,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone)]
pub enum InputEventOutcome {
    Continue {
        text: String,
        images: Vec<ImageContent>,
    },
    Block {
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct SessionBeforeCompactOutcome {
    pub cancel: bool,
    pub compaction: Option<SessionBeforeCompactCompaction>,
}

#[derive(Debug, Clone)]
pub struct SessionBeforeCompactCompaction {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub details: Option<Value>,
}

#[must_use]
pub fn apply_input_event_response(
    response: Option<Value>,
    original_text: String,
    original_images: Vec<ImageContent>,
) -> InputEventOutcome {
    let Some(response) = response else {
        return InputEventOutcome::Continue {
            text: original_text,
            images: original_images,
        };
    };

    if response.is_null() {
        return InputEventOutcome::Continue {
            text: original_text,
            images: original_images,
        };
    }

    if let Some(obj) = response.as_object() {
        let reason = obj
            .get("reason")
            .or_else(|| obj.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string);

        if let Some(action) = obj
            .get("action")
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase)
        {
            match action.as_str() {
                "handled" | "block" | "blocked" => {
                    return InputEventOutcome::Block { reason };
                }
                "transform" => {
                    let text = obj
                        .get("text")
                        .or_else(|| obj.get("content"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or(original_text);
                    let images = parse_input_event_images(obj, original_images);
                    return InputEventOutcome::Continue { text, images };
                }
                "continue" => {
                    return InputEventOutcome::Continue {
                        text: original_text,
                        images: original_images,
                    };
                }
                _ => {}
            }
        }

        if obj.get("block").and_then(Value::as_bool) == Some(true) {
            return InputEventOutcome::Block { reason };
        }

        let text_override = obj
            .get("text")
            .or_else(|| obj.get("content"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let images_override = parse_input_event_images_opt(obj);

        if text_override.is_some() || images_override.is_some() {
            return InputEventOutcome::Continue {
                text: text_override.unwrap_or(original_text),
                images: images_override.unwrap_or(original_images),
            };
        }
    }

    if let Some(text) = response.as_str() {
        return InputEventOutcome::Continue {
            text: text.to_string(),
            images: original_images,
        };
    }

    InputEventOutcome::Continue {
        text: original_text,
        images: original_images,
    }
}

#[must_use]
pub fn apply_session_before_compact_response(
    response: Option<Value>,
    fallback_tokens_before: u64,
) -> SessionBeforeCompactOutcome {
    let Some(response) = response else {
        return SessionBeforeCompactOutcome::default();
    };

    if response.is_null() {
        return SessionBeforeCompactOutcome::default();
    }

    if response.as_bool() == Some(false) {
        return SessionBeforeCompactOutcome {
            cancel: true,
            compaction: None,
        };
    }

    let Some(obj) = response.as_object() else {
        return SessionBeforeCompactOutcome::default();
    };

    let cancel = obj.get("cancel").and_then(Value::as_bool).unwrap_or(false)
        || obj
            .get("cancelled")
            .and_then(Value::as_bool)
            .unwrap_or(false);

    if cancel {
        return SessionBeforeCompactOutcome {
            cancel: true,
            compaction: None,
        };
    }

    let Some(compaction_value) = obj.get("compaction") else {
        return SessionBeforeCompactOutcome::default();
    };
    let Some(compaction_obj) = compaction_value.as_object() else {
        return SessionBeforeCompactOutcome::default();
    };

    let summary = compaction_obj
        .get("summary")
        .and_then(Value::as_str)
        .map(str::to_string);
    let first_kept_entry_id = compaction_obj
        .get("firstKeptEntryId")
        .or_else(|| compaction_obj.get("first_kept_entry_id"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let Some(summary) = summary else {
        return SessionBeforeCompactOutcome::default();
    };
    let Some(first_kept_entry_id) = first_kept_entry_id else {
        return SessionBeforeCompactOutcome::default();
    };

    let tokens_before = compaction_obj
        .get("tokensBefore")
        .or_else(|| compaction_obj.get("tokens_before"))
        .and_then(Value::as_u64)
        .unwrap_or(fallback_tokens_before);
    let details = compaction_obj.get("details").cloned();

    SessionBeforeCompactOutcome {
        cancel: false,
        compaction: Some(SessionBeforeCompactCompaction {
            summary,
            first_kept_entry_id,
            tokens_before,
            details,
        }),
    }
}

#[must_use]
pub fn apply_before_agent_start_response(
    response: Option<Value>,
    timestamp: i64,
) -> BeforeAgentStartOutcome {
    let mut outcome = BeforeAgentStartOutcome {
        messages: Vec::new(),
        system_prompt: None,
    };

    let Some(response) = response else {
        return outcome;
    };

    if response.is_null() {
        return outcome;
    }

    let Some(obj) = response.as_object() else {
        return outcome;
    };

    if let Some(system_prompt) = obj
        .get("systemPrompt")
        .or_else(|| obj.get("system_prompt"))
        .and_then(Value::as_str)
    {
        outcome.system_prompt = Some(system_prompt.to_string());
    }

    if let Some(messages_value) = obj.get("messages") {
        if let Some(list) = messages_value.as_array() {
            for item in list {
                if let Some(msg) = parse_custom_message(item, timestamp) {
                    outcome.messages.push(msg);
                }
            }
        } else if let Some(msg) = parse_custom_message(messages_value, timestamp) {
            outcome.messages.push(msg);
        }
        return outcome;
    }

    if let Some(message_value) = obj.get("message")
        && let Some(msg) = parse_custom_message(message_value, timestamp)
    {
        outcome.messages.push(msg);
    }

    outcome
}

fn parse_custom_message(value: &Value, timestamp: i64) -> Option<CustomMessage> {
    let obj = value.as_object()?;
    let custom_type = obj
        .get("customType")
        .or_else(|| obj.get("custom_type"))
        .and_then(Value::as_str)?
        .to_string();
    let content = obj.get("content").and_then(Value::as_str)?.to_string();
    let display = obj.get("display").and_then(Value::as_bool).unwrap_or(true);
    let details = obj.get("details").cloned();
    Some(CustomMessage {
        content,
        custom_type,
        display,
        details,
        timestamp,
    })
}

fn parse_input_event_images_opt(obj: &serde_json::Map<String, Value>) -> Option<Vec<ImageContent>> {
    let value = obj.get("images").or_else(|| obj.get("attachments"))?;
    if value.is_null() {
        return Some(Vec::new());
    }
    serde_json::from_value(value.clone()).ok()
}

fn parse_input_event_images(
    obj: &serde_json::Map<String, Value>,
    fallback: Vec<ImageContent>,
) -> Vec<ImageContent> {
    parse_input_event_images_opt(obj).unwrap_or(fallback)
}

fn json_to_value<T: Serialize>(value: &T) -> Result<Value> {
    serde_json::to_value(value).map_err(|err| Error::Json(Box::new(err)))
}

fn json_from_value<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|err| Error::Json(Box::new(err)))
}

/// Dispatches events to extension handlers.
#[derive(Clone)]
pub struct EventDispatcher {
    runtime: ExtensionRuntimeHandle,
}

impl EventDispatcher {
    #[must_use]
    pub fn new<R>(runtime: R) -> Self
    where
        R: Into<ExtensionRuntimeHandle>,
    {
        Self {
            runtime: runtime.into(),
        }
    }

    /// Dispatch an event with an explicit context payload and timeout.
    pub async fn dispatch_with_context<R: DeserializeOwned>(
        &self,
        event: ExtensionEvent,
        ctx_payload: Value,
        timeout_ms: u64,
    ) -> Result<Option<R>> {
        let event_name = event.event_name().to_string();
        let event_payload = json_to_value(&event)?;
        let response = self
            .runtime
            .dispatch_event(event_name, event_payload, Arc::new(ctx_payload), timeout_ms)
            .await?;

        if response.is_null() {
            Ok(None)
        } else {
            Ok(Some(json_from_value(response)?))
        }
    }

    /// Dispatch an event with an empty context payload and default timeout.
    pub async fn dispatch<R: DeserializeOwned>(&self, event: ExtensionEvent) -> Result<Option<R>> {
        self.dispatch_with_context(
            event,
            Value::Object(serde_json::Map::new()),
            EXTENSION_EVENT_TIMEOUT_MS,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    fn sample_images() -> Vec<ImageContent> {
        vec![ImageContent {
            data: "ORIGINAL_BASE64".to_string(),
            mime_type: "image/png".to_string(),
        }]
    }

    fn assert_continue(
        outcome: InputEventOutcome,
        expected_text: &str,
        expected_images: &[ImageContent],
    ) {
        match outcome {
            InputEventOutcome::Continue { text, images } => {
                assert_eq!(text, expected_text);
                assert_eq!(images.len(), expected_images.len());
                for (actual, expected) in images.iter().zip(expected_images.iter()) {
                    assert_eq!(actual.data, expected.data);
                    assert_eq!(actual.mime_type, expected.mime_type);
                }
            }
            InputEventOutcome::Block { reason } => {
                panic!();
            }
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn event_name_matches_expected_strings() {
        fn sample_message() -> Message {
            Message::Custom(crate::model::CustomMessage {
                content: "hi".to_string(),
                custom_type: "test".to_string(),
                display: true,
                details: None,
                timestamp: 0,
            })
        }

        fn sample_assistant_message() -> AssistantMessage {
            AssistantMessage {
                content: vec![ContentBlock::Text(crate::model::TextContent::new("ok"))],
                api: "test".to_string(),
                provider: "test".to_string(),
                model: "test".to_string(),
                usage: crate::model::Usage::default(),
                stop_reason: crate::model::StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            }
        }

        fn sample_tool_result() -> ToolResultMessage {
            ToolResultMessage {
                tool_call_id: "call-1".to_string(),
                tool_name: "read".to_string(),
                content: vec![ContentBlock::Text(crate::model::TextContent::new("ok"))],
                details: None,
                is_error: false,
                timestamp: 0,
            }
        }

        fn sample_image() -> ImageContent {
            ImageContent {
                data: "BASE64".to_string(),
                mime_type: "image/png".to_string(),
            }
        }

        let cases: Vec<(ExtensionEvent, &str)> = vec![
            (
                ExtensionEvent::Startup {
                    version: "0.1.0".to_string(),
                    session_file: None,
                },
                "startup",
            ),
            (
                ExtensionEvent::AgentStart {
                    session_id: "s".to_string(),
                },
                "agent_start",
            ),
            (
                ExtensionEvent::AgentEnd {
                    session_id: "s".to_string(),
                    messages: vec![sample_message()],
                    error: None,
                },
                "agent_end",
            ),
            (
                ExtensionEvent::TurnStart {
                    session_id: "s".to_string(),
                    turn_index: 0,
                },
                "turn_start",
            ),
            (
                ExtensionEvent::TurnEnd {
                    session_id: "s".to_string(),
                    turn_index: 0,
                    message: Box::new(sample_assistant_message()),
                    tool_results: vec![sample_tool_result()],
                },
                "turn_end",
            ),
            (
                ExtensionEvent::ToolCall {
                    tool_name: "read".to_string(),
                    tool_call_id: "call-1".to_string(),
                    input: json!({ "path": "a.txt" }),
                },
                "tool_call",
            ),
            (
                ExtensionEvent::ToolResult {
                    tool_name: "read".to_string(),
                    tool_call_id: "call-1".to_string(),
                    input: json!({ "path": "a.txt" }),
                    content: vec![ContentBlock::Text(crate::model::TextContent::new("ok"))],
                    details: Some(json!({ "k": "v" })),
                    is_error: false,
                },
                "tool_result",
            ),
            (
                ExtensionEvent::SessionBeforeSwitch {
                    current_session: None,
                    target_session: "next".to_string(),
                },
                "session_before_switch",
            ),
            (
                ExtensionEvent::SessionBeforeFork {
                    current_session: Some("cur".to_string()),
                    fork_entry_id: "entry-1".to_string(),
                },
                "session_before_fork",
            ),
            (
                ExtensionEvent::Input {
                    content: "hello".to_string(),
                    attachments: vec![sample_image()],
                },
                "input",
            ),
        ];

        for (event, expected) in cases {
            assert_eq!(event.event_name(), expected);
            let value = serde_json::to_value(&event).expect("serialize");
            assert_eq!(value.get("type").and_then(Value::as_str), Some(expected));

            // Verify camelCase fields match the actual protocol used by the agent
            if expected == "input" {
                assert!(
                    value.get("text").is_some(),
                    "Input event should have 'text' field"
                );
                assert!(
                    value.get("images").is_some(),
                    "Input event should have 'images' field"
                );
            } else if expected == "tool_call" {
                assert!(
                    value.get("toolName").is_some(),
                    "ToolCall event should have 'toolName' field"
                );
                assert!(
                    value.get("toolCallId").is_some(),
                    "ToolCall event should have 'toolCallId' field"
                );
            } else if expected == "agent_start" {
                assert!(
                    value.get("sessionId").is_some(),
                    "AgentStart event should have 'sessionId' field"
                );
            } else if expected == "turn_end" {
                assert!(
                    value.get("toolResults").is_some(),
                    "TurnEnd event should have 'toolResults' field"
                );
            }
        }
    }

    #[test]
    fn result_types_deserialize_defaults() {
        let result: ToolCallEventResult =
            serde_json::from_value(json!({ "reason": "nope" })).expect("deserialize");
        assert_eq!(
            result,
            ToolCallEventResult {
                block: false,
                reason: Some("nope".to_string())
            }
        );
    }

    #[test]
    fn result_types_deserialize_all() {
        let tool_call: ToolCallEventResult =
            serde_json::from_value(json!({ "block": true })).expect("deserialize tool_call");
        assert!(tool_call.block);
        assert_eq!(tool_call.reason, None);

        let tool_result: ToolResultEventResult = serde_json::from_value(json!({
            "content": [{ "type": "text", "text": "hello" }],
            "details": { "k": "v" }
        }))
        .expect("deserialize tool_result");
        assert!(tool_result.content.is_some());
        assert_eq!(tool_result.details, Some(json!({ "k": "v" })));

        let input: InputEventResult =
            serde_json::from_value(json!({ "content": "hi" })).expect("deserialize input");
        assert_eq!(input.content.as_deref(), Some("hi"));
        assert!(!input.block);
        assert_eq!(input.reason, None);
    }

    #[test]
    fn apply_input_event_response_preserves_original_for_none_and_null() {
        let original_images = sample_images();

        let none_response =
            apply_input_event_response(None, "original".to_string(), original_images.clone());
        assert_continue(none_response, "original", &original_images);

        let null_response = apply_input_event_response(
            Some(Value::Null),
            "original".to_string(),
            original_images.clone(),
        );
        assert_continue(null_response, "original", &original_images);
    }

    #[test]
    fn apply_input_event_response_blocks_for_action_variants() {
        for action in ["handled", "block", "blocked"] {
            let outcome = apply_input_event_response(
                Some(json!({ "action": action, "reason": "Denied by policy" })),
                "original".to_string(),
                sample_images(),
            );

            match outcome {
                InputEventOutcome::Block { reason } => {
                    assert_eq!(reason.as_deref(), Some("Denied by policy"));
                }
                InputEventOutcome::Continue { .. } => {
                    panic!();
                }
            }
        }
    }

    #[test]
    fn apply_input_event_response_transform_uses_overrides_and_fallbacks() {
        let original_images = sample_images();
        let override_images = vec![ImageContent {
            data: "NEW_BASE64".to_string(),
            mime_type: "image/jpeg".to_string(),
        }];

        let transformed = apply_input_event_response(
            Some(json!({
                "action": "transform",
                "text": "rewritten",
                "images": [{ "data": "NEW_BASE64", "mimeType": "image/jpeg" }]
            })),
            "original".to_string(),
            original_images.clone(),
        );
        assert_continue(transformed, "rewritten", &override_images);

        let invalid_images = apply_input_event_response(
            Some(json!({
                "action": "transform",
                "text": "still rewritten",
                "images": "not-an-array"
            })),
            "original".to_string(),
            original_images.clone(),
        );
        assert_continue(invalid_images, "still rewritten", &original_images);

        let null_images = apply_input_event_response(
            Some(json!({
                "content": "alt text",
                "images": null
            })),
            "original".to_string(),
            original_images,
        );
        assert_continue(null_images, "alt text", &[]);
    }

    #[test]
    fn apply_input_event_response_continue_action_and_shorthand_string() {
        let original_images = sample_images();

        let explicit_continue = apply_input_event_response(
            Some(json!({
                "action": "continue",
                "text": "ignored",
                "images": []
            })),
            "original".to_string(),
            original_images.clone(),
        );
        assert_continue(explicit_continue, "original", &original_images);

        let shorthand = apply_input_event_response(
            Some(Value::String("replacement".to_string())),
            "original".to_string(),
            original_images.clone(),
        );
        assert_continue(shorthand, "replacement", &original_images);
    }

    #[test]
    fn apply_input_event_response_block_flag_and_message_fallback() {
        let blocked = apply_input_event_response(
            Some(json!({ "block": true, "message": "Policy denied" })),
            "original".to_string(),
            sample_images(),
        );

        match blocked {
            InputEventOutcome::Block { reason } => {
                assert_eq!(reason.as_deref(), Some("Policy denied"));
            }
            InputEventOutcome::Continue { .. } => panic!(),
        }
    }

    // ── unknown action falls through to continue ───────────────────────

    #[test]
    fn apply_input_event_response_unknown_action_falls_through() {
        let original_images = sample_images();
        // Unknown action, no block flag, no text override → falls through to original
        let outcome = apply_input_event_response(
            Some(json!({ "action": "unknown_action" })),
            "original".to_string(),
            original_images.clone(),
        );
        assert_continue(outcome, "original", &original_images);
    }

    // ── non-object, non-string, non-null response ──────────────────────

    #[test]
    fn apply_input_event_response_number_returns_original() {
        let original_images = sample_images();
        let outcome = apply_input_event_response(
            Some(json!(42)),
            "original".to_string(),
            original_images.clone(),
        );
        assert_continue(outcome, "original", &original_images);
    }

    #[test]
    fn apply_input_event_response_boolean_returns_original() {
        let original_images = sample_images();
        let outcome = apply_input_event_response(
            Some(json!(true)),
            "original".to_string(),
            original_images.clone(),
        );
        assert_continue(outcome, "original", &original_images);
    }

    #[test]
    fn apply_input_event_response_array_returns_original() {
        let original_images = sample_images();
        let outcome = apply_input_event_response(
            Some(json!([1, 2, 3])),
            "original".to_string(),
            original_images.clone(),
        );
        assert_continue(outcome, "original", &original_images);
    }

    // ── ToolCallEventResult default ────────────────────────────────────

    #[test]
    fn tool_call_event_result_default_is_not_blocked() {
        let result = ToolCallEventResult::default();
        assert!(!result.block);
        assert!(result.reason.is_none());
    }

    // ── InputEventResult equality ──────────────────────────────────────

    #[test]
    fn input_event_result_equality() {
        let a = InputEventResult {
            content: Some("hello".to_string()),
            block: false,
            reason: None,
        };
        let b = InputEventResult {
            content: Some("hello".to_string()),
            block: false,
            reason: None,
        };
        assert_eq!(a, b);
    }

    // ── transform with content key instead of text ─────────────────────

    #[test]
    fn apply_input_event_response_transform_uses_content_key() {
        let original_images = sample_images();
        let outcome = apply_input_event_response(
            Some(json!({ "action": "transform", "content": "transformed via content" })),
            "original".to_string(),
            original_images.clone(),
        );
        assert_continue(outcome, "transformed via content", &original_images);
    }

    // ── text override without action ───────────────────────────────────

    #[test]
    fn apply_input_event_response_text_override_without_action() {
        let original_images = sample_images();
        let outcome = apply_input_event_response(
            Some(json!({ "text": "overridden text" })),
            "original".to_string(),
            original_images.clone(),
        );
        assert_continue(outcome, "overridden text", &original_images);
    }

    // ── attachments key for images ─────────────────────────────────────

    #[test]
    fn apply_input_event_response_attachments_key_for_images() {
        let outcome = apply_input_event_response(
            Some(json!({
                "text": "with attachments",
                "attachments": [{ "data": "ATT_BASE64", "mimeType": "image/gif" }]
            })),
            "original".to_string(),
            sample_images(),
        );
        match outcome {
            InputEventOutcome::Continue { text, images } => {
                assert_eq!(text, "with attachments");
                assert_eq!(images.len(), 1);
                assert_eq!(images[0].data, "ATT_BASE64");
            }
            InputEventOutcome::Block { .. } => panic!(),
        }
    }

    // ── block: false doesn't block ─────────────────────────────────────

    #[test]
    fn apply_input_event_response_block_false_does_not_block() {
        let original_images = sample_images();
        let outcome = apply_input_event_response(
            Some(json!({ "block": false })),
            "original".to_string(),
            original_images.clone(),
        );
        assert_continue(outcome, "original", &original_images);
    }

    // ── empty object returns original ──────────────────────────────────

    #[test]
    fn apply_input_event_response_empty_object_returns_original() {
        let original_images = sample_images();
        let outcome = apply_input_event_response(
            Some(json!({})),
            "original".to_string(),
            original_images.clone(),
        );
        assert_continue(outcome, "original", &original_images);
    }

    mod proptest_extension_events {
        use super::*;
        use proptest::prelude::*;

        /// All event names are unique, lowercase, with underscores only.
        const ALL_EVENT_NAMES: &[&str] = &[
            "startup",
            "agent_start",
            "agent_end",
            "turn_start",
            "turn_end",
            "tool_call",
            "tool_result",
            "session_before_switch",
            "session_before_fork",
            "input",
        ];

        proptest! {
            /// `event_name` returns valid snake_case names.
            #[test]
            fn event_names_are_snake_case(idx in 0..ALL_EVENT_NAMES.len()) {
                let name = ALL_EVENT_NAMES[idx];
                assert!(
                    name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                    "not snake_case: {name}"
                );
                assert!(!name.is_empty());
            }

            /// `apply_input_event_response(None, ..)` always returns Continue with original.
            #[test]
            fn none_response_preserves_original(text in ".{0,50}") {
                match apply_input_event_response(None, text.clone(), Vec::new()) {
                    InputEventOutcome::Continue { text: t, images } => {
                        assert_eq!(t, text);
                        assert!(images.is_empty());
                    }
                    InputEventOutcome::Block { .. } => assert!(false, "expected Continue"),
                }
            }

            /// `apply_input_event_response(null, ..)` always returns Continue with original.
            #[test]
            fn null_response_preserves_original(text in ".{0,50}") {
                match apply_input_event_response(Some(Value::Null), text.clone(), Vec::new()) {
                    InputEventOutcome::Continue { text: t, images } => {
                        assert_eq!(t, text);
                        assert!(images.is_empty());
                    }
                    InputEventOutcome::Block { .. } => assert!(false, "expected Continue"),
                }
            }

            /// String response replaces text, preserves images.
            #[test]
            fn string_response_replaces_text(
                original in "[a-z]{1,10}",
                replacement in "[A-Z]{1,10}"
            ) {
                match apply_input_event_response(
                    Some(Value::String(replacement.clone())),
                    original,
                    Vec::new(),
                ) {
                    InputEventOutcome::Continue { text, images } => {
                        assert_eq!(text, replacement);
                        assert!(images.is_empty());
                    }
                    InputEventOutcome::Block { .. } => assert!(false, "expected Continue"),
                }
            }

            /// "block" action always produces Block outcome.
            #[test]
            fn block_action_blocks(
                action_idx in 0..3usize,
                text in "[a-z]{1,10}"
            ) {
                let actions = ["handled", "block", "blocked"];
                let response = json!({"action": actions[action_idx]});
                match apply_input_event_response(Some(response), text, Vec::new()) {
                    InputEventOutcome::Block { .. } => {}
                    InputEventOutcome::Continue { .. } => {
                        assert!(false, "expected Block for action '{}'", actions[action_idx]);
                    }
                }
            }

            /// "continue" action preserves original text.
            #[test]
            fn continue_action_preserves(text in "[a-z]{1,20}") {
                let response = json!({"action": "continue"});
                match apply_input_event_response(Some(response), text.clone(), Vec::new()) {
                    InputEventOutcome::Continue { text: t, .. } => {
                        assert_eq!(t, text);
                    }
                    InputEventOutcome::Block { .. } => assert!(false, "expected Continue"),
                }
            }

            /// "transform" action with text field replaces text.
            #[test]
            fn transform_replaces_text(
                original in "[a-z]{1,10}",
                new_text in "[A-Z]{1,10}"
            ) {
                let response = json!({"action": "transform", "text": &new_text});
                match apply_input_event_response(Some(response), original, Vec::new()) {
                    InputEventOutcome::Continue { text, .. } => {
                        assert_eq!(text, new_text);
                    }
                    InputEventOutcome::Block { .. } => assert!(false, "expected Continue"),
                }
            }

            /// `block: true` without action produces Block.
            #[test]
            fn block_true_flag_blocks(text in "[a-z]{1,10}") {
                let response = json!({"block": true});
                match apply_input_event_response(Some(response), text, Vec::new()) {
                    InputEventOutcome::Block { .. } => {}
                    InputEventOutcome::Continue { .. } => assert!(false, "expected Block"),
                }
            }

            /// `block: false` without action returns Continue.
            #[test]
            fn block_false_continues(text in "[a-z]{1,10}") {
                let response = json!({"block": false});
                match apply_input_event_response(Some(response), text.clone(), Vec::new()) {
                    InputEventOutcome::Continue { text: t, .. } => {
                        assert_eq!(t, text);
                    }
                    InputEventOutcome::Block { .. } => assert!(false, "expected Continue"),
                }
            }

            /// Non-object, non-string, non-null values preserve original.
            #[test]
            fn numeric_response_preserves(n in -100i64..100, text in "[a-z]{1,10}") {
                let response = Value::from(n);
                match apply_input_event_response(Some(response), text.clone(), Vec::new()) {
                    InputEventOutcome::Continue { text: t, .. } => {
                        assert_eq!(t, text);
                    }
                    InputEventOutcome::Block { .. } => assert!(false, "expected Continue"),
                }
            }

            /// Block reason is extracted from "reason" or "message" field.
            #[test]
            fn block_reason_extracted(
                reason in "[a-z]{1,20}",
                use_message_key in proptest::bool::ANY
            ) {
                let key = if use_message_key { "message" } else { "reason" };
                let response = json!({"action": "block", key: &reason});
                match apply_input_event_response(Some(response), String::new(), Vec::new()) {
                    InputEventOutcome::Block { reason: r } => {
                        assert_eq!(r.as_deref(), Some(reason.as_str()));
                    }
                    InputEventOutcome::Continue { .. } => assert!(false, "expected Block"),
                }
            }

            /// `ToolCallEventResult` deserializes with correct defaults.
            #[test]
            fn tool_call_result_deserialize(
                block in proptest::bool::ANY,
                reason in prop::option::of("[a-z ]{1,30}")
            ) {
                let mut obj = serde_json::Map::new();
                obj.insert("block".to_string(), json!(block));
                if let Some(ref r) = reason {
                    obj.insert("reason".to_string(), json!(r));
                }
                let back: ToolCallEventResult =
                    serde_json::from_value(Value::Object(obj)).unwrap();
                assert_eq!(back.block, block);
                assert_eq!(back.reason, reason);
            }

            /// `ToolCallEventResult` default has block=false.
            #[test]
            fn tool_call_result_default(_dummy in 0..1u8) {
                let d = ToolCallEventResult::default();
                assert!(!d.block);
                assert!(d.reason.is_none());
            }

            /// `InputEventResult` deserializes correctly.
            #[test]
            fn input_event_result_deserialize(
                content in prop::option::of("[a-z]{1,20}"),
                block in proptest::bool::ANY,
                reason in prop::option::of("[a-z]{1,20}")
            ) {
                let mut obj = serde_json::Map::new();
                if let Some(ref c) = content {
                    obj.insert("content".to_string(), json!(c));
                }
                obj.insert("block".to_string(), json!(block));
                if let Some(ref r) = reason {
                    obj.insert("reason".to_string(), json!(r));
                }
                let back: InputEventResult =
                    serde_json::from_value(Value::Object(obj)).unwrap();
                assert_eq!(back.content, content);
                assert_eq!(back.block, block);
                assert_eq!(back.reason, reason);
            }
        }
    }
}
