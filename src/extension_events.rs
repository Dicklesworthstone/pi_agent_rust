//! Typed extension event definitions + dispatch helper.
//!
//! This module defines the JSON-serializable event payloads that can be sent to
//! JavaScript extensions via the `dispatch_event` hook system.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::extensions::{EXTENSION_EVENT_TIMEOUT_MS, JsExtensionRuntimeHandle};
use crate::model::{AssistantMessage, ContentBlock, ImageContent, Message, ToolResultMessage};

/// Events that can be dispatched to extension handlers.
///
/// The serialized representation is tagged with `type` in `snake_case`, matching
/// the string event name used by JS hooks (e.g. `"tool_call"`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtensionEvent {
    /// Agent startup (once per session).
    Startup {
        version: String,
        session_file: Option<String>,
    },

    /// Before first API call in a run.
    AgentStart { session_id: String },

    /// After agent loop ends.
    AgentEnd {
        session_id: String,
        messages: Vec<Message>,
        error: Option<String>,
    },

    /// Before provider.stream() call.
    TurnStart {
        session_id: String,
        turn_index: usize,
    },

    /// After response processed.
    TurnEnd {
        session_id: String,
        turn_index: usize,
        message: AssistantMessage,
        tool_results: Vec<ToolResultMessage>,
    },

    /// Before tool execution (can block).
    ToolCall {
        tool_name: String,
        tool_call_id: String,
        input: Value,
    },

    /// After tool execution (can modify result).
    ToolResult {
        tool_name: String,
        tool_call_id: String,
        input: Value,
        content: Vec<ContentBlock>,
        details: Option<Value>,
        is_error: bool,
    },

    /// Before session switch (can cancel).
    SessionBeforeSwitch {
        current_session: Option<String>,
        target_session: String,
    },

    /// Before session fork (can cancel).
    SessionBeforeFork {
        current_session: Option<String>,
        fork_entry_id: String,
    },

    /// Before processing user input (can transform).
    Input {
        content: String,
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
    runtime: JsExtensionRuntimeHandle,
}

impl EventDispatcher {
    #[must_use]
    pub const fn new(runtime: JsExtensionRuntimeHandle) -> Self {
        Self { runtime }
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
            .dispatch_event(event_name, event_payload, ctx_payload, timeout_ms)
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
                    message: sample_assistant_message(),
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
}
