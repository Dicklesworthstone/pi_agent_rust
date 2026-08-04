//! Message types, content blocks, and streaming events.
//!
//! These types are the shared “wire format” used across the project:
//! - Providers stream [`StreamEvent`] values that incrementally build an assistant reply.
//! - Sessions persist [`Message`] values as JSON (see [`crate::session`]).
//! - Tools return [`ContentBlock`] output that can be rendered in the TUI and replayed to providers.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

// ============================================================================
// Message Types
// ============================================================================

/// A message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    /// Message authored by the user.
    User(UserMessage),
    /// Message authored by the assistant/model.
    ///
    /// Wrapped in [`Arc`] for cheap cloning during streaming – the streaming
    /// hot-path emits many events per token and [`Arc::make_mut`] gives O(1)
    /// copy-on-write when the refcount is 1.
    Assistant(Arc<AssistantMessage>),
    /// Tool result produced by the host after executing a tool call.
    ///
    /// Wrapped in [`Arc`] for cheap cloning – tool results often contain large
    /// file contents from the `read` tool and are cloned multiple times during
    /// event dispatch and session persistence.
    ToolResult(Arc<ToolResultMessage>),
    /// Host/extension-defined message type.
    Custom(CustomMessage),
}

/// A user message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub content: UserContent,
    pub timestamp: i64,
}

/// User message content - either plain text or blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    /// Plain text content (common for interactive input).
    Text(String),
    /// Structured content blocks (e.g. text + images).
    Blocks(Vec<ContentBlock>),
}

/// An assistant message.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub api: String,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    pub stop_reason: StopReason,
    /// Provider-supplied structured details for a terminal stop reason.
    ///
    /// Anthropic currently emits these for `refusal` stops. Keeping the
    /// provider response structured (rather than flattening it into
    /// `error_message`) lets callers make an informed policy decision and
    /// preserves the complete response in persisted sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<StopDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: i64,
}

/// A tool result message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    pub is_error: bool,
    pub timestamp: i64,
}

/// A custom message injected by the host or extensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    pub content: String,
    pub custom_type: String,
    #[serde(default)]
    pub display: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    pub timestamp: i64,
}

impl Message {
    /// Convenience constructor: wraps an [`AssistantMessage`] in [`Arc`].
    pub fn assistant(msg: AssistantMessage) -> Self {
        Self::Assistant(Arc::new(msg))
    }

    /// Convenience constructor: wraps a [`ToolResultMessage`] in [`Arc`].
    pub fn tool_result(msg: ToolResultMessage) -> Self {
        Self::ToolResult(Arc::new(msg))
    }
}

// ============================================================================
// Stop Reasons
// ============================================================================

/// Why a response ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    #[default]
    /// The provider signaled a normal stop (end of message).
    Stop,
    /// The provider hit a token limit.
    Length,
    /// The provider requested tool execution.
    ToolUse,
    /// The stream terminated due to an error.
    Error,
    /// The request was aborted locally.
    Aborted,
    /// The provider paused a long-running turn and expects the assistant
    /// response to be resubmitted unchanged to continue it.
    PauseTurn,
    /// The provider completed the request but declined it for policy reasons.
    Refusal,
}

/// Provider-supplied structured details for a terminal stop reason.
///
/// The category is deliberately represented as a string. Providers may add
/// categories without changing the response envelope, and preserving the
/// received value is safer than turning an otherwise valid refusal into a
/// deserialization failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopDetails {
    /// Discriminator supplied by the provider (currently `"refusal"`).
    #[serde(rename = "type")]
    pub kind: String,
    /// Provider policy category, if one is available.
    pub category: Option<String>,
    /// Human-readable provider explanation, if one is available.
    pub explanation: Option<String>,
}

// ============================================================================
// Content Blocks
// ============================================================================

/// A content block in a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    /// Plain text content.
    Text(TextContent),
    /// Provider “thinking” / reasoning (if enabled).
    Thinking(ThinkingContent),
    /// Provider-redacted reasoning. Anthropic emits this on the wire as
    /// `{"type":"redacted_thinking","data":"<opaque>"}` when the safety
    /// classifier hides upstream reasoning; OpenRouter's Anthropic-compatible
    /// relay forwards it verbatim. The block carries no user-surfaceable text
    /// but must round-trip through the deserializer or the agent loop fails.
    #[serde(rename = "redacted_thinking")]
    RedactedThinking(RedactedThinkingContent),
    /// An inline image (base64 + MIME type).
    Image(ImageContent),
    /// A request to call a tool with JSON arguments.
    ToolCall(ToolCall),
}

/// Text content block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
}

impl TextContent {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            text_signature: None,
        }
    }
}

/// Thinking/reasoning content block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    pub thinking: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
}

/// Image content block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    pub data: String, // Base64 encoded
    pub mime_type: String,
}

/// Redacted-thinking content block — opaque marker emitted by Anthropic's safety pipeline.
///
/// The `data` field is provider-controlled and not intended for user display;
/// it is preserved on round-trip so cross-provider replays stay faithful.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RedactedThinkingContent {
    pub data: String,
}

/// Tool call content block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

// ============================================================================
// Usage Tracking
// ============================================================================

/// Token usage and cost tracking.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
    pub cost: Cost,
}

/// Cost breakdown in dollars.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

// ============================================================================
// Streaming Events
// ============================================================================

/// Streaming event from a provider.
///
/// Provider implementations emit this enum while decoding SSE/HTTP streams.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Start {
        partial: AssistantMessage,
    },

    TextStart {
        content_index: usize,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    TextEnd {
        content_index: usize,
        content: String,
    },

    ThinkingStart {
        content_index: usize,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
    },

    /// A tool-call content block opened. `id`/`name` carry whatever the
    /// provider already knows at this point (most providers send both on the
    /// opening chunk); empty strings mean "not known yet" and the terminal
    /// [`StreamEvent::ToolCallEnd`] still carries the authoritative values.
    /// Populating them here lets snapshot clients (RPC/ACP) correlate the
    /// growing partial with the later tool-execution events (#129).
    ToolCallStart {
        content_index: usize,
        id: String,
        name: String,
    },
    ToolCallDelta {
        content_index: usize,
        delta: String,
    },
    ToolCallEnd {
        content_index: usize,
        tool_call: ToolCall,
    },

    Done {
        reason: StopReason,
        message: AssistantMessage,
    },
    Error {
        reason: StopReason,
        error: AssistantMessage,
    },
}

// ============================================================================
// Assistant Message Events (Streaming)
// ============================================================================

/// Streaming event emitted for assistant message updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AssistantMessageEvent {
    #[serde(rename = "start")]
    Start { partial: Arc<AssistantMessage> },
    #[serde(rename = "text_start")]
    TextStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        partial: Arc<AssistantMessage>,
    },
    #[serde(rename = "text_delta")]
    TextDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
        partial: Arc<AssistantMessage>,
    },
    #[serde(rename = "text_end")]
    TextEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: String,
        partial: Arc<AssistantMessage>,
    },
    #[serde(rename = "thinking_start")]
    ThinkingStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        partial: Arc<AssistantMessage>,
    },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
        partial: Arc<AssistantMessage>,
    },
    #[serde(rename = "thinking_end")]
    ThinkingEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: String,
        partial: Arc<AssistantMessage>,
    },
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        partial: Arc<AssistantMessage>,
    },
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
        partial: Arc<AssistantMessage>,
    },
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "toolCall")]
        tool_call: ToolCall,
        partial: Arc<AssistantMessage>,
    },
    #[serde(rename = "done")]
    Done {
        reason: StopReason,
        message: Arc<AssistantMessage>,
    },
    #[serde(rename = "error")]
    Error {
        reason: StopReason,
        error: Arc<AssistantMessage>,
    },
}

// ============================================================================
// Thinking Level
// ============================================================================

/// Extended thinking level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl std::str::FromStr for ThinkingLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "off" | "none" | "0" => Ok(Self::Off),
            "minimal" | "min" => Ok(Self::Minimal),
            "low" | "1" => Ok(Self::Low),
            "medium" | "med" | "2" => Ok(Self::Medium),
            "high" | "3" => Ok(Self::High),
            "xhigh" | "4" => Ok(Self::XHigh),
            "max" | "5" => Ok(Self::Max),
            _ => Err(format!("Invalid thinking level: {s}")),
        }
    }
}

impl ThinkingLevel {
    /// Get the default token budget for this level.
    pub const fn default_budget(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Minimal => 1024,
            Self::Low => 2048,
            Self::Medium => 8192,
            Self::High => 16384,
            Self::XHigh => 32768, // High reasonable limit
            Self::Max => 65536,   // Top tier above xhigh
        }
    }
}

impl std::fmt::Display for ThinkingLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        };
        write!(f, "{s}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;
    use std::collections::BTreeSet;

    // ── Helper ─────────────────────────────────────────────────────────

    fn sample_usage() -> Usage {
        Usage {
            input: 100,
            output: 50,
            cache_read: 10,
            cache_write: 5,
            total_tokens: 165,
            cost: Cost {
                input: 0.001,
                output: 0.002,
                cache_read: 0.0001,
                cache_write: 0.0002,
                total: 0.0033,
            },
        }
    }

    fn sample_assistant_message() -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("Hello"))],
            api: "anthropic".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4".to_string(),
            usage: sample_usage(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 1_700_000_000,
        }
    }

    #[derive(Debug, Default)]
    struct EventTransitionState {
        seen_start: bool,
        finished: bool,
        open_text_indices: BTreeSet<usize>,
        open_thinking_indices: BTreeSet<usize>,
        open_tool_indices: BTreeSet<usize>,
    }

    fn event_transition_diag(
        fixture_id: &str,
        step: usize,
        event_type: &str,
        state: &EventTransitionState,
        detail: &str,
    ) -> String {
        json!({
            "fixture_id": fixture_id,
            "seed": "deterministic-static",
            "env": {
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
            },
            "step": step,
            "event_type": event_type,
            "state_snapshot": {
                "seen_start": state.seen_start,
                "finished": state.finished,
                "open_text_indices": state.open_text_indices.iter().copied().collect::<Vec<_>>(),
                "open_thinking_indices": state.open_thinking_indices.iter().copied().collect::<Vec<_>>(),
                "open_tool_indices": state.open_tool_indices.iter().copied().collect::<Vec<_>>(),
            },
            "detail": detail,
        })
        .to_string()
    }

    #[allow(clippy::too_many_lines)]
    fn validate_event_transitions(
        fixture_id: &str,
        events: &[AssistantMessageEvent],
    ) -> Result<(), String> {
        let mut state = EventTransitionState::default();

        for (step, event) in events.iter().enumerate() {
            match event {
                AssistantMessageEvent::Start { .. } => {
                    if state.seen_start || state.finished {
                        return Err(event_transition_diag(
                            fixture_id,
                            step,
                            "start",
                            &state,
                            "start must appear exactly once before done/error",
                        ));
                    }
                    state.seen_start = true;
                }
                AssistantMessageEvent::TextStart { content_index, .. } => {
                    if !state.seen_start || state.finished {
                        return Err(event_transition_diag(
                            fixture_id,
                            step,
                            "text_start",
                            &state,
                            "text_start before start or after done/error",
                        ));
                    }
                    if !state.open_text_indices.insert(*content_index) {
                        return Err(event_transition_diag(
                            fixture_id,
                            step,
                            "text_start",
                            &state,
                            "duplicate text_start for same content index",
                        ));
                    }
                }
                AssistantMessageEvent::TextDelta { content_index, .. } => {
                    if !state.open_text_indices.contains(content_index) {
                        return Err(event_transition_diag(
                            fixture_id,
                            step,
                            "text_delta",
                            &state,
                            "text_delta without matching text_start",
                        ));
                    }
                }
                AssistantMessageEvent::TextEnd { content_index, .. } => {
                    if !state.open_text_indices.remove(content_index) {
                        return Err(event_transition_diag(
                            fixture_id,
                            step,
                            "text_end",
                            &state,
                            "text_end without matching text_start",
                        ));
                    }
                }
                AssistantMessageEvent::ThinkingStart { content_index, .. } => {
                    if !state.open_thinking_indices.insert(*content_index) {
                        return Err(event_transition_diag(
                            fixture_id,
                            step,
                            "thinking_start",
                            &state,
                            "duplicate thinking_start for same content index",
                        ));
                    }
                }
                AssistantMessageEvent::ThinkingDelta { content_index, .. } => {
                    if !state.open_thinking_indices.contains(content_index) {
                        return Err(event_transition_diag(
                            fixture_id,
                            step,
                            "thinking_delta",
                            &state,
                            "thinking_delta without matching thinking_start",
                        ));
                    }
                }
                AssistantMessageEvent::ThinkingEnd { content_index, .. } => {
                    if !state.open_thinking_indices.remove(content_index) {
                        return Err(event_transition_diag(
                            fixture_id,
                            step,
                            "thinking_end",
                            &state,
                            "thinking_end without matching thinking_start",
                        ));
                    }
                }
                AssistantMessageEvent::ToolCallStart { content_index, .. } => {
                    if !state.open_tool_indices.insert(*content_index) {
                        return Err(event_transition_diag(
                            fixture_id,
                            step,
                            "toolcall_start",
                            &state,
                            "duplicate toolcall_start for same content index",
                        ));
                    }
                }
                AssistantMessageEvent::ToolCallDelta { content_index, .. } => {
                    if !state.open_tool_indices.contains(content_index) {
                        return Err(event_transition_diag(
                            fixture_id,
                            step,
                            "toolcall_delta",
                            &state,
                            "toolcall_delta without matching toolcall_start",
                        ));
                    }
                }
                AssistantMessageEvent::ToolCallEnd { content_index, .. } => {
                    if !state.open_tool_indices.remove(content_index) {
                        return Err(event_transition_diag(
                            fixture_id,
                            step,
                            "toolcall_end",
                            &state,
                            "toolcall_end without matching toolcall_start",
                        ));
                    }
                }
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                    if !state.seen_start {
                        return Err(event_transition_diag(
                            fixture_id,
                            step,
                            "terminal",
                            &state,
                            "done/error before start",
                        ));
                    }
                    if state.finished {
                        return Err(event_transition_diag(
                            fixture_id,
                            step,
                            "terminal",
                            &state,
                            "multiple terminal events",
                        ));
                    }
                    if !state.open_text_indices.is_empty()
                        || !state.open_thinking_indices.is_empty()
                        || !state.open_tool_indices.is_empty()
                    {
                        return Err(event_transition_diag(
                            fixture_id,
                            step,
                            "terminal",
                            &state,
                            "done/error while content blocks still open",
                        ));
                    }
                    state.finished = true;
                }
            }
        }

        if !state.finished {
            return Err(event_transition_diag(
                fixture_id,
                events.len(),
                "end_of_stream",
                &state,
                "missing terminal done/error event",
            ));
        }

        Ok(())
    }

    // ── Message enum serialization ─────────────────────────────────────

    #[test]
    fn message_user_text_roundtrip() {
        let msg = Message::User(UserMessage {
            content: UserContent::Text("hi".to_string()),
            timestamp: 1_700_000_000,
        });
        let json = serde_json::to_string(&msg).expect("serialize");
        let parsed: Message = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            Message::User(u) => {
                assert!(matches!(u.content, UserContent::Text(ref s) if s == "hi"));
                assert_eq!(u.timestamp, 1_700_000_000);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn message_user_blocks_roundtrip() {
        let msg = Message::User(UserMessage {
            content: UserContent::Blocks(vec![ContentBlock::Text(TextContent::new("hello"))]),
            timestamp: 42,
        });
        let json = serde_json::to_string(&msg).expect("serialize");
        let parsed: Message = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            Message::User(u) => match u.content {
                UserContent::Blocks(blocks) => {
                    assert_eq!(blocks.len(), 1);
                    assert!(matches!(&blocks[0], ContentBlock::Text(t) if t.text == "hello"));
                }
                UserContent::Text(_) => panic!(),
            },
            _ => panic!(),
        }
    }

    #[test]
    fn message_assistant_roundtrip() {
        let msg = Message::assistant(sample_assistant_message());
        let json = serde_json::to_string(&msg).expect("serialize");
        let parsed: Message = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            Message::Assistant(a) => {
                assert_eq!(a.model, "claude-sonnet-4");
                assert_eq!(a.stop_reason, StopReason::Stop);
                assert_eq!(a.usage.input, 100);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn message_tool_result_roundtrip() {
        let msg = Message::tool_result(ToolResultMessage {
            tool_call_id: "call_1".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("file contents"))],
            details: Some(json!({"path": "/tmp/test.txt"})),
            is_error: false,
            timestamp: 99,
        });
        let json = serde_json::to_string(&msg).expect("serialize");
        let parsed: Message = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            Message::ToolResult(tr) => {
                assert_eq!(tr.tool_call_id, "call_1");
                assert_eq!(tr.tool_name, "read");
                assert!(!tr.is_error);
                assert!(tr.details.is_some());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn message_custom_roundtrip() {
        let msg = Message::Custom(CustomMessage {
            content: "custom data".to_string(),
            custom_type: "extension_output".to_string(),
            display: true,
            details: None,
            timestamp: 77,
        });
        let json = serde_json::to_string(&msg).expect("serialize");
        let parsed: Message = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            Message::Custom(c) => {
                assert_eq!(c.custom_type, "extension_output");
                assert!(c.display);
                assert!(c.details.is_none());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn message_role_tag_in_json() {
        let user = Message::User(UserMessage {
            content: UserContent::Text("x".to_string()),
            timestamp: 0,
        });
        let v: serde_json::Value = serde_json::to_value(&user).expect("to_value");
        assert_eq!(v["role"], "user");

        let assistant = Message::assistant(sample_assistant_message());
        let v: serde_json::Value = serde_json::to_value(&assistant).expect("to_value");
        assert_eq!(v["role"], "assistant");
    }

    // ── UserContent untagged deserialization ────────────────────────────

    #[test]
    fn user_content_text_from_string() {
        let content: UserContent = serde_json::from_str("\"hello\"").expect("deserialize");
        assert!(matches!(content, UserContent::Text(s) if s == "hello"));
    }

    #[test]
    fn user_content_blocks_from_array() {
        let json = json!([{"type": "text", "text": "hi"}]);
        let content: UserContent = serde_json::from_value(json).expect("deserialize");
        match content {
            UserContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
            }
            UserContent::Text(_) => panic!(),
        }
    }

    #[test]
    fn user_content_empty_string() {
        let content: UserContent = serde_json::from_str("\"\"").expect("deserialize");
        assert!(matches!(content, UserContent::Text(s) if s.is_empty()));
    }

    // ── StopReason ─────────────────────────────────────────────────────

    #[test]
    fn stop_reason_default_is_stop() {
        assert_eq!(StopReason::default(), StopReason::Stop);
    }

    #[test]
    fn stop_reason_serde_roundtrip() {
        let reasons = [
            StopReason::Stop,
            StopReason::Length,
            StopReason::ToolUse,
            StopReason::PauseTurn,
            StopReason::Refusal,
            StopReason::Error,
            StopReason::Aborted,
        ];
        for reason in &reasons {
            let json = serde_json::to_string(reason).expect("serialize");
            let parsed: StopReason = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(*reason, parsed);
        }
    }

    #[test]
    fn stop_reason_camel_case_serialization() {
        assert_eq!(
            serde_json::to_string(&StopReason::ToolUse).unwrap(),
            "\"toolUse\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::Stop).unwrap(),
            "\"stop\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::PauseTurn).unwrap(),
            "\"pauseTurn\""
        );
    }

    #[test]
    fn refusal_stop_details_roundtrip_and_are_optional() {
        let mut message = sample_assistant_message();
        message.stop_reason = StopReason::Refusal;
        message.stop_details = Some(StopDetails {
            kind: "refusal".to_string(),
            category: Some("cyber".to_string()),
            explanation: Some("The request could enable cyber harm.".to_string()),
        });

        let json = serde_json::to_string(&message).expect("serialize");
        assert!(json.contains("stopDetails"));
        let parsed: AssistantMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.stop_reason, StopReason::Refusal);
        assert_eq!(parsed.stop_details, message.stop_details);
    }

    // ── ContentBlock ───────────────────────────────────────────────────

    #[test]
    fn content_block_text_roundtrip() {
        let block = ContentBlock::Text(TextContent {
            text: "hello".to_string(),
            text_signature: Some("sig123".to_string()),
        });
        let json = serde_json::to_string(&block).expect("serialize");
        let parsed: ContentBlock = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            ContentBlock::Text(t) => {
                assert_eq!(t.text, "hello");
                assert_eq!(t.text_signature.as_deref(), Some("sig123"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn content_block_thinking_roundtrip() {
        let block = ContentBlock::Thinking(ThinkingContent {
            thinking: "reasoning...".to_string(),
            thinking_signature: None,
        });
        let json = serde_json::to_string(&block).expect("serialize");
        let parsed: ContentBlock = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(parsed, ContentBlock::Thinking(t) if t.thinking == "reasoning..."));
    }

    /// Anthropic emits `{"type":"redacted_thinking","data":"<opaque>"}` when
    /// the safety pipeline hides upstream reasoning; OpenRouter relays it
    /// verbatim. The deserializer must accept the variant or the agent loop
    /// terminates on every redaction (issue tracked in pi_agent_rust#80).
    #[test]
    fn content_block_redacted_thinking_wire_form_is_accepted() {
        let wire = serde_json::json!({
            "type": "redacted_thinking",
            "data": "OPAQUE_BLOB",
        });
        let parsed: ContentBlock =
            serde_json::from_value(wire).expect("redacted_thinking must deserialize");
        let ContentBlock::RedactedThinking(rt) = &parsed else {
            panic!("expected RedactedThinking, got {parsed:?}");
        };
        assert_eq!(rt.data, "OPAQUE_BLOB");

        // Round-trip the variant back to wire form so cross-provider replays
        // (e.g. via session save/restore) preserve the opaque payload.
        let reserialized = serde_json::to_value(&parsed).expect("re-serialize");
        assert_eq!(reserialized["type"], "redacted_thinking");
        assert_eq!(reserialized["data"], "OPAQUE_BLOB");
    }

    /// Mixed content vec — the realistic shape OpenRouter sends back when its
    /// upstream produced redacted reasoning interleaved with normal output.
    #[test]
    fn content_block_redacted_thinking_in_mixed_assistant_content() {
        let original = AssistantMessage {
            content: vec![
                ContentBlock::Text(TextContent::new("Before.")),
                ContentBlock::RedactedThinking(RedactedThinkingContent {
                    data: "REDACTED".to_string(),
                }),
                ContentBlock::Text(TextContent::new("After.")),
            ],
            stop_details: None,
            ..AssistantMessage::default()
        };
        let json = serde_json::to_value(&original).expect("serialize");
        let parsed: AssistantMessage =
            serde_json::from_value(json).expect("deserialize mixed-content message");
        assert_eq!(parsed.content.len(), 3);
        assert!(matches!(&parsed.content[0], ContentBlock::Text(t) if t.text == "Before."));
        assert!(matches!(
            &parsed.content[1],
            ContentBlock::RedactedThinking(rt) if rt.data == "REDACTED"
        ));
        assert!(matches!(&parsed.content[2], ContentBlock::Text(t) if t.text == "After."));
    }

    /// Forward compatibility: if Anthropic ever adds sibling fields to the
    /// redacted_thinking block (e.g. a future marker_id), the deserializer
    /// should ignore them rather than reject the whole block.
    #[test]
    fn content_block_redacted_thinking_ignores_unknown_siblings() {
        let wire = serde_json::json!({
            "type": "redacted_thinking",
            "data": "OPAQUE",
            "futureFieldFromAnthropic": "ignore me",
        });
        let parsed: ContentBlock =
            serde_json::from_value(wire).expect("unknown sibling fields must not break parsing");
        assert!(matches!(parsed, ContentBlock::RedactedThinking(_)));
    }

    #[test]
    fn content_block_image_roundtrip() {
        let block = ContentBlock::Image(ImageContent {
            data: "aGVsbG8=".to_string(),
            mime_type: "image/png".to_string(),
        });
        let json = serde_json::to_string(&block).expect("serialize");
        let parsed: ContentBlock = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            ContentBlock::Image(img) => {
                assert_eq!(img.data, "aGVsbG8=");
                assert_eq!(img.mime_type, "image/png");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn content_block_tool_call_roundtrip() {
        let block = ContentBlock::ToolCall(ToolCall {
            id: "tc_1".to_string(),
            name: "read".to_string(),
            arguments: json!({"path": "/tmp/test.txt"}),
            thought_signature: None,
        });
        let json = serde_json::to_string(&block).expect("serialize");
        let parsed: ContentBlock = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            ContentBlock::ToolCall(tc) => {
                assert_eq!(tc.id, "tc_1");
                assert_eq!(tc.name, "read");
                assert_eq!(tc.arguments["path"], "/tmp/test.txt");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn content_block_type_tag_in_json() {
        let text = ContentBlock::Text(TextContent::new("x"));
        let v: serde_json::Value = serde_json::to_value(&text).expect("to_value");
        assert_eq!(v["type"], "text");

        let thinking = ContentBlock::Thinking(ThinkingContent {
            thinking: "t".to_string(),
            thinking_signature: None,
        });
        let v: serde_json::Value = serde_json::to_value(&thinking).expect("to_value");
        assert_eq!(v["type"], "thinking");
    }

    // ── TextContent::new ───────────────────────────────────────────────

    #[test]
    fn text_content_new_sets_none_signature() {
        let tc = TextContent::new("test");
        assert_eq!(tc.text, "test");
        assert!(tc.text_signature.is_none());
    }

    #[test]
    fn text_content_new_accepts_string() {
        let tc = TextContent::new(String::from("owned"));
        assert_eq!(tc.text, "owned");
    }

    // ── Usage and Cost ─────────────────────────────────────────────────

    #[test]
    fn usage_default_is_zero() {
        let u = Usage::default();
        assert_eq!(u.input, 0);
        assert_eq!(u.output, 0);
        assert_eq!(u.total_tokens, 0);
        assert!((u.cost.total - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn usage_serde_roundtrip() {
        let u = sample_usage();
        let json = serde_json::to_string(&u).expect("serialize");
        let parsed: Usage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.input, 100);
        assert_eq!(parsed.output, 50);
        assert!((parsed.cost.total - 0.0033).abs() < 1e-10);
    }

    #[test]
    fn cost_default_is_zero() {
        let c = Cost::default();
        assert!((c.input - 0.0).abs() < f64::EPSILON);
        assert!((c.output - 0.0).abs() < f64::EPSILON);
        assert!((c.total - 0.0).abs() < f64::EPSILON);
    }

    // ── ThinkingLevel ──────────────────────────────────────────────────

    #[test]
    fn thinking_level_default_is_off() {
        assert_eq!(ThinkingLevel::default(), ThinkingLevel::Off);
    }

    #[test]
    fn thinking_level_from_str_all_valid() {
        let cases = [
            ("off", ThinkingLevel::Off),
            ("none", ThinkingLevel::Off),
            ("0", ThinkingLevel::Off),
            ("minimal", ThinkingLevel::Minimal),
            ("min", ThinkingLevel::Minimal),
            ("low", ThinkingLevel::Low),
            ("1", ThinkingLevel::Low),
            ("medium", ThinkingLevel::Medium),
            ("med", ThinkingLevel::Medium),
            ("2", ThinkingLevel::Medium),
            ("high", ThinkingLevel::High),
            ("3", ThinkingLevel::High),
            ("xhigh", ThinkingLevel::XHigh),
            ("4", ThinkingLevel::XHigh),
            ("max", ThinkingLevel::Max),
            ("5", ThinkingLevel::Max),
        ];
        for (input, expected) in &cases {
            let parsed: ThinkingLevel = input.parse().expect(input);
            assert_eq!(parsed, *expected, "input: {input}");
        }
    }

    #[test]
    fn thinking_level_from_str_case_insensitive() {
        let parsed: ThinkingLevel = "HIGH".parse().expect("HIGH");
        assert_eq!(parsed, ThinkingLevel::High);
        let parsed: ThinkingLevel = "Medium".parse().expect("Medium");
        assert_eq!(parsed, ThinkingLevel::Medium);
    }

    #[test]
    fn thinking_level_from_str_trims_whitespace() {
        let parsed: ThinkingLevel = "  off  ".parse().expect("trimmed");
        assert_eq!(parsed, ThinkingLevel::Off);
    }

    #[test]
    fn thinking_level_from_str_invalid() {
        let result: Result<ThinkingLevel, _> = "invalid".parse();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid thinking level"));
    }

    #[test]
    fn thinking_level_display_roundtrip() {
        let levels = [
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
        ];
        for level in &levels {
            let displayed = level.to_string();
            let parsed: ThinkingLevel = displayed.parse().expect(&displayed);
            assert_eq!(*level, parsed);
        }
    }

    #[test]
    fn thinking_level_default_budget_values() {
        assert_eq!(ThinkingLevel::Off.default_budget(), 0);
        assert_eq!(ThinkingLevel::Minimal.default_budget(), 1024);
        assert_eq!(ThinkingLevel::Low.default_budget(), 2048);
        assert_eq!(ThinkingLevel::Medium.default_budget(), 8192);
        assert_eq!(ThinkingLevel::High.default_budget(), 16384);
        assert_eq!(ThinkingLevel::XHigh.default_budget(), 32768);
    }

    #[test]
    fn thinking_level_budgets_are_monotonically_increasing() {
        let levels = [
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
        ];
        for pair in levels.windows(2) {
            assert!(
                pair[0].default_budget() < pair[1].default_budget(),
                "{} budget ({}) should be less than {} budget ({})",
                pair[0],
                pair[0].default_budget(),
                pair[1],
                pair[1].default_budget()
            );
        }
    }

    #[test]
    fn thinking_level_serde_roundtrip() {
        let levels = [
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
        ];
        for level in &levels {
            let json = serde_json::to_string(level).expect("serialize");
            let parsed: ThinkingLevel = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(*level, parsed);
        }
    }

    // ── AssistantMessage optional fields ────────────────────────────────

    #[test]
    fn assistant_message_error_message_skipped_when_none() {
        let msg = sample_assistant_message();
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(!json.contains("errorMessage"), "None should be skipped");
    }

    #[test]
    fn assistant_message_error_message_included_when_some() {
        let mut msg = sample_assistant_message();
        msg.error_message = Some("rate limit".to_string());
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains("errorMessage"));
        assert!(json.contains("rate limit"));
    }

    // ── ToolCall optional fields ───────────────────────────────────────

    #[test]
    fn tool_call_thought_signature_skipped_when_none() {
        let tc = ToolCall {
            id: "t1".to_string(),
            name: "read".to_string(),
            arguments: json!({}),
            thought_signature: None,
        };
        let json = serde_json::to_string(&tc).expect("serialize");
        assert!(!json.contains("thoughtSignature"));
    }

    // ── AssistantMessageEvent ──────────────────────────────────────────

    #[test]
    fn assistant_message_event_type_tags() {
        let events = vec![
            (
                AssistantMessageEvent::Start {
                    partial: sample_assistant_message().into(),
                },
                "start",
            ),
            (
                AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "hi".to_string(),
                    partial: sample_assistant_message().into(),
                },
                "text_delta",
            ),
            (
                AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message: sample_assistant_message().into(),
                },
                "done",
            ),
            (
                AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: sample_assistant_message().into(),
                },
                "error",
            ),
        ];
        for (event, expected_type) in &events {
            let v: serde_json::Value = serde_json::to_value(event).expect("to_value");
            assert_eq!(
                v["type"].as_str(),
                Some(*expected_type),
                "expected type={expected_type}"
            );
        }
    }

    #[test]
    fn assistant_message_event_roundtrip() {
        let event = AssistantMessageEvent::TextEnd {
            content_index: 2,
            content: "final text".to_string(),
            partial: sample_assistant_message().into(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: AssistantMessageEvent = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                ..
            } => {
                assert_eq!(content_index, 2);
                assert_eq!(content, "final text");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn assistant_message_event_rejects_malformed_payload() {
        let malformed = json!({
            "type": "text_delta",
            "delta": "hi",
            "partial": sample_assistant_message()
        });
        let encoded = malformed.to_string();
        let err = serde_json::from_str::<AssistantMessageEvent>(&encoded)
            .expect_err("text_delta without contentIndex should fail");
        let diag = json!({
            "fixture_id": "model-assistant-event-malformed-payload",
            "seed": "deterministic-static",
            "expected": "serde error for missing contentIndex",
            "actual_error": err.to_string(),
            "payload": malformed,
        })
        .to_string();
        assert!(
            err.to_string().contains("contentIndex"),
            "missing contentIndex not reported: {diag}"
        );
    }

    #[test]
    fn assistant_message_event_transitions_accept_valid_sequence() {
        let partial = sample_assistant_message();
        let message = sample_assistant_message();
        let events = vec![
            AssistantMessageEvent::Start {
                partial: partial.clone().into(),
            },
            AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: partial.clone().into(),
            },
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "he".to_string(),
                partial: partial.clone().into(),
            },
            AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: "hello".to_string(),
                partial: partial.into(),
            },
            AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message: message.into(),
            },
        ];

        validate_event_transitions("model-event-transition-valid", &events)
            .expect("valid sequence should pass");
    }

    #[test]
    fn assistant_message_event_transitions_reject_out_of_order_delta() {
        let partial = sample_assistant_message();
        let message = sample_assistant_message();
        let events = vec![
            AssistantMessageEvent::Start {
                partial: partial.clone().into(),
            },
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "hi".to_string(),
                partial: partial.into(),
            },
            AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message: message.into(),
            },
        ];

        let err = validate_event_transitions("model-event-transition-out-of-order", &events)
            .expect_err("out-of-order text_delta should fail");
        assert!(
            err.contains("\"fixture_id\":\"model-event-transition-out-of-order\"")
                && err.contains("text_delta without matching text_start"),
            "unexpected diagnostic payload: {err}"
        );
    }

    // ── ToolResultMessage optional details ──────────────────────────────

    #[test]
    fn tool_result_details_skipped_when_none() {
        let tr = ToolResultMessage {
            tool_call_id: "c1".to_string(),
            tool_name: "bash".to_string(),
            content: vec![],
            details: None,
            is_error: false,
            timestamp: 0,
        };
        let json = serde_json::to_string(&tr).expect("serialize");
        assert!(!json.contains("details"));
    }

    #[test]
    fn tool_result_is_error_roundtrip() {
        let tr = ToolResultMessage {
            tool_call_id: "c1".to_string(),
            tool_name: "bash".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("error output"))],
            details: None,
            is_error: true,
            timestamp: 1,
        };
        let json = serde_json::to_string(&tr).expect("serialize");
        let parsed: ToolResultMessage = serde_json::from_str(&json).expect("deserialize");
        assert!(parsed.is_error);
        assert_eq!(parsed.tool_name, "bash");
    }

    // ── CustomMessage display default ──────────────────────────────────

    #[test]
    fn custom_message_display_defaults_to_false() {
        let json = json!({
            "content": "data",
            "customType": "ext",
            "timestamp": 0
        });
        let msg: CustomMessage = serde_json::from_value(json).expect("deserialize");
        assert!(!msg.display);
    }

    // ── Proptest serde invariants ───────────────────────────────────────

    fn arbitrary_small_string() -> impl Strategy<Value = String> {
        prop::collection::vec(any::<u8>(), 0..128)
            .prop_map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    fn interesting_text_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            arbitrary_small_string(),
            Just(String::new()),
            Just("[]".to_string()),
            Just("{}".to_string()),
            Just("cafe\u{0301}".to_string()),
            Just("emoji \u{1F600}".to_string()),
        ]
    }

    fn scalar_json_value_strategy() -> impl Strategy<Value = serde_json::Value> {
        prop_oneof![
            Just(serde_json::Value::Null),
            any::<bool>().prop_map(serde_json::Value::Bool),
            any::<i64>().prop_map(|n| json!(n)),
            any::<u64>().prop_map(|n| json!(n)),
            interesting_text_strategy().prop_map(serde_json::Value::String),
        ]
    }

    fn bounded_json_value_strategy() -> impl Strategy<Value = serde_json::Value> {
        prop_oneof![
            scalar_json_value_strategy(),
            prop::collection::vec(scalar_json_value_strategy(), 0..5)
                .prop_map(serde_json::Value::Array),
            prop::collection::btree_map(
                arbitrary_small_string(),
                scalar_json_value_strategy(),
                0..5
            )
            .prop_map(|map| {
                serde_json::Value::Object(
                    map.into_iter()
                        .collect::<serde_json::Map<String, serde_json::Value>>(),
                )
            }),
        ]
    }

    fn stop_reason_strategy() -> impl Strategy<Value = StopReason> {
        prop_oneof![
            Just(StopReason::Stop),
            Just(StopReason::Length),
            Just(StopReason::ToolUse),
            Just(StopReason::PauseTurn),
            Just(StopReason::Refusal),
            Just(StopReason::Error),
            Just(StopReason::Aborted),
        ]
    }

    fn usage_strategy() -> impl Strategy<Value = Usage> {
        (
            any::<u16>(),
            any::<u16>(),
            any::<u16>(),
            any::<u16>(),
            any::<u16>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
        )
            .prop_map(
                |(
                    input,
                    output,
                    cache_read,
                    cache_write,
                    total_tokens,
                    cost_input,
                    cost_output,
                    cost_cache_read,
                    cost_cache_write,
                    cost_total,
                )| Usage {
                    input: u64::from(input),
                    output: u64::from(output),
                    cache_read: u64::from(cache_read),
                    cache_write: u64::from(cache_write),
                    total_tokens: u64::from(total_tokens),
                    cost: Cost {
                        input: f64::from(cost_input) / 1_000_000.0,
                        output: f64::from(cost_output) / 1_000_000.0,
                        cache_read: f64::from(cost_cache_read) / 1_000_000.0,
                        cache_write: f64::from(cost_cache_write) / 1_000_000.0,
                        total: f64::from(cost_total) / 1_000_000.0,
                    },
                },
            )
    }

    fn text_content_strategy() -> impl Strategy<Value = TextContent> {
        (
            interesting_text_strategy(),
            prop::option::of(interesting_text_strategy()),
        )
            .prop_map(|(text, text_signature)| TextContent {
                text,
                text_signature,
            })
    }

    fn thinking_content_strategy() -> impl Strategy<Value = ThinkingContent> {
        (
            interesting_text_strategy(),
            prop::option::of(interesting_text_strategy()),
        )
            .prop_map(|(thinking, thinking_signature)| ThinkingContent {
                thinking,
                thinking_signature,
            })
    }

    fn image_content_strategy() -> impl Strategy<Value = ImageContent> {
        (
            interesting_text_strategy(),
            prop_oneof![
                Just("image/png".to_string()),
                Just("image/jpeg".to_string()),
                Just("image/webp".to_string()),
                interesting_text_strategy(),
            ],
        )
            .prop_map(|(data, mime_type)| ImageContent { data, mime_type })
    }

    fn tool_call_strategy() -> impl Strategy<Value = ToolCall> {
        // Use scalar_json_value_strategy for arguments to keep proptest
        // strategy tree shallow enough for the default thread stack.
        (
            interesting_text_strategy(),
            interesting_text_strategy(),
            scalar_json_value_strategy(),
            prop::option::of(interesting_text_strategy()),
        )
            .prop_map(|(id, name, arguments, thought_signature)| ToolCall {
                id,
                name,
                arguments,
                thought_signature,
            })
    }

    fn content_block_strategy() -> impl Strategy<Value = ContentBlock> {
        prop_oneof![
            text_content_strategy().prop_map(ContentBlock::Text),
            thinking_content_strategy().prop_map(ContentBlock::Thinking),
            image_content_strategy().prop_map(ContentBlock::Image),
            tool_call_strategy().prop_map(ContentBlock::ToolCall),
        ]
    }

    fn content_block_json_strategy() -> impl Strategy<Value = serde_json::Value> {
        content_block_strategy()
            .prop_map(|block| serde_json::to_value(block).expect("content block should serialize"))
    }

    fn invalid_content_block_json_strategy() -> impl Strategy<Value = serde_json::Value> {
        prop_oneof![
            interesting_text_strategy().prop_map(|text| json!({ "text": text })),
            interesting_text_strategy().prop_map(|text| json!({ "type": "unknown", "text": text })),
            Just(json!({ "type": 42, "text": "bad-discriminator-type" })),
            Just(json!({ "type": "text" })),
            Just(json!({ "type": "image", "mimeType": "image/png" })),
            Just(json!({ "type": "toolCall", "id": "tool-only-id" })),
        ]
    }

    fn user_content_strategy() -> impl Strategy<Value = UserContent> {
        prop_oneof![
            interesting_text_strategy().prop_map(UserContent::Text),
            prop::collection::vec(content_block_strategy(), 0..6).prop_map(UserContent::Blocks),
        ]
    }

    fn assistant_message_strategy() -> impl Strategy<Value = AssistantMessage> {
        (
            prop::collection::vec(content_block_strategy(), 0..3),
            interesting_text_strategy(),
            interesting_text_strategy(),
            interesting_text_strategy(),
            usage_strategy(),
            stop_reason_strategy(),
            prop::option::of(interesting_text_strategy()),
            any::<i64>(),
        )
            .prop_map(
                |(content, api, provider, model, usage, stop_reason, error_message, timestamp)| {
                    AssistantMessage {
                        content,
                        api,
                        provider,
                        model,
                        usage,
                        stop_reason,
                        stop_details: None,
                        error_message,
                        timestamp,
                    }
                },
            )
    }

    fn tool_result_message_strategy() -> impl Strategy<Value = ToolResultMessage> {
        (
            interesting_text_strategy(),
            interesting_text_strategy(),
            prop::collection::vec(content_block_strategy(), 0..3),
            prop::option::of(scalar_json_value_strategy()),
            any::<bool>(),
            any::<i64>(),
        )
            .prop_map(
                |(tool_call_id, tool_name, content, details, is_error, timestamp)| {
                    ToolResultMessage {
                        tool_call_id,
                        tool_name,
                        content,
                        details,
                        is_error,
                        timestamp,
                    }
                },
            )
    }

    fn custom_message_strategy() -> impl Strategy<Value = CustomMessage> {
        (
            interesting_text_strategy(),
            interesting_text_strategy(),
            any::<bool>(),
            prop::option::of(scalar_json_value_strategy()),
            any::<i64>(),
        )
            .prop_map(|(content, custom_type, display, details, timestamp)| {
                CustomMessage {
                    content,
                    custom_type,
                    display,
                    details,
                    timestamp,
                }
            })
    }

    fn message_strategy() -> impl Strategy<Value = Message> {
        prop_oneof![
            (user_content_strategy(), any::<i64>())
                .prop_map(|(content, timestamp)| Message::User(UserMessage { content, timestamp })),
            assistant_message_strategy().prop_map(|m| Message::Assistant(Arc::new(m))),
            tool_result_message_strategy().prop_map(|m| Message::ToolResult(Arc::new(m))),
            custom_message_strategy().prop_map(Message::Custom),
        ]
    }

    fn non_string_or_array_json_strategy() -> impl Strategy<Value = serde_json::Value> {
        prop_oneof![
            Just(serde_json::Value::Null),
            any::<bool>().prop_map(serde_json::Value::Bool),
            any::<i64>().prop_map(|n| json!(n)),
            prop::collection::btree_map(
                arbitrary_small_string(),
                scalar_json_value_strategy(),
                0..4
            )
            .prop_map(|map| {
                serde_json::Value::Object(
                    map.into_iter()
                        .collect::<serde_json::Map<String, serde_json::Value>>(),
                )
            }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, .. ProptestConfig::default() })]

        #[test]
        fn proptest_user_content_untagged_text_vs_blocks(
            text in interesting_text_strategy(),
            blocks in prop::collection::vec(content_block_json_strategy(), 0..5),
        ) {
            let parsed_text: UserContent = serde_json::from_value(serde_json::Value::String(text.clone()))
                .expect("string must deserialize as UserContent::Text");
            prop_assert!(matches!(parsed_text, UserContent::Text(ref s) if s == &text));

            let parsed_blocks: UserContent = serde_json::from_value(serde_json::Value::Array(blocks.clone()))
                .expect("array of content-block JSON must deserialize as UserContent::Blocks");
            match parsed_blocks {
                UserContent::Blocks(parsed) => prop_assert_eq!(parsed.len(), blocks.len()),
                UserContent::Text(_) => {
                    prop_assert!(false, "array input must not deserialize as UserContent::Text");
                }
            }
        }

        #[test]
        fn proptest_user_content_rejects_non_string_or_array(value in non_string_or_array_json_strategy()) {
            let result = serde_json::from_value::<UserContent>(value);
            prop_assert!(result.is_err());
        }

        #[test]
        fn proptest_content_block_roundtrip(block in content_block_strategy()) {
            let serialized = serde_json::to_value(&block).expect("content block should serialize");
            let parsed: ContentBlock = serde_json::from_value(serialized.clone())
                .expect("serialized content block should deserialize");
            let reserialized = serde_json::to_value(parsed).expect("re-serialize should succeed");
            prop_assert_eq!(reserialized, serialized);
        }

        #[test]
        fn proptest_content_block_invalid_discriminator_errors(payload in invalid_content_block_json_strategy()) {
            let result = serde_json::from_value::<ContentBlock>(payload);
            prop_assert!(result.is_err());
        }

        #[test]
        fn proptest_message_roundtrip_and_unknown_fields(
            message in message_strategy(),
            extra_value in scalar_json_value_strategy(),
        ) {
            let serialized = serde_json::to_value(&message).expect("message should serialize");
            let parsed: Message = serde_json::from_value(serialized.clone())
                .expect("serialized message should deserialize");
            let reserialized = serde_json::to_value(parsed).expect("re-serialize should succeed");

            // Some representational forms are semantically equivalent for Option<Value>
            // fields (e.g., `details: null` vs omitted), so assert canonical stability
            // after one deserialize/serialize cycle.
            let reparsed: Message = serde_json::from_value(reserialized.clone())
                .expect("re-serialized message should deserialize");
            let stabilized = serde_json::to_value(reparsed).expect("stabilized serialize");
            prop_assert_eq!(stabilized, reserialized);

            let mut with_extra = serialized;
            if let serde_json::Value::Object(ref mut obj) = with_extra {
                obj.insert("extraFieldProptest".to_string(), extra_value);
            }
            let parsed_with_extra = serde_json::from_value::<Message>(with_extra);
            prop_assert!(parsed_with_extra.is_ok());
        }
    }
}
