//! Message types, content blocks, and streaming events.
//!
//! These types are the shared “wire format” used across the project:
//! - Providers stream [`StreamEvent`] values that incrementally build an assistant reply.
//! - Sessions persist [`Message`] values as JSON (see [`crate::session`]).
//! - Tools return [`ContentBlock`] output that can be rendered in the TUI and replayed to providers.

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
    Assistant(AssistantMessage),
    /// Tool result produced by the host after executing a tool call.
    ToolResult(ToolResultMessage),
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub api: String,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    pub stop_reason: StopReason,
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
/// Provider implementations emit this enum while decoding SSE/HTTP streams. Most variants carry a
/// `partial` [`AssistantMessage`] snapshot so the caller can update UI state without having to
/// re-implement incremental assembly.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Start {
        partial: AssistantMessage,
    },

    TextStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    TextDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    TextEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },

    ThinkingStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },

    ToolCallStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    ToolCallDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ToolCallEnd {
        content_index: usize,
        tool_call: ToolCall,
        partial: AssistantMessage,
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
    Start { partial: AssistantMessage },
    #[serde(rename = "text_start")]
    TextStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        partial: AssistantMessage,
    },
    #[serde(rename = "text_delta")]
    TextDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    #[serde(rename = "text_end")]
    TextEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    #[serde(rename = "thinking_start")]
    ThinkingStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        partial: AssistantMessage,
    },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    #[serde(rename = "thinking_end")]
    ThinkingEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "toolCall")]
        tool_call: ToolCall,
        partial: AssistantMessage,
    },
    #[serde(rename = "done")]
    Done {
        reason: StopReason,
        message: AssistantMessage,
    },
    #[serde(rename = "error")]
    Error {
        reason: StopReason,
        error: AssistantMessage,
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
        };
        write!(f, "{s}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            error_message: None,
            timestamp: 1_700_000_000,
        }
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
            _ => panic!("expected User variant"),
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
                UserContent::Text(_) => panic!("expected Blocks"),
            },
            _ => panic!("expected User variant"),
        }
    }

    #[test]
    fn message_assistant_roundtrip() {
        let msg = Message::Assistant(sample_assistant_message());
        let json = serde_json::to_string(&msg).expect("serialize");
        let parsed: Message = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            Message::Assistant(a) => {
                assert_eq!(a.model, "claude-sonnet-4");
                assert_eq!(a.stop_reason, StopReason::Stop);
                assert_eq!(a.usage.input, 100);
            }
            _ => panic!("expected Assistant variant"),
        }
    }

    #[test]
    fn message_tool_result_roundtrip() {
        let msg = Message::ToolResult(ToolResultMessage {
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
            _ => panic!("expected ToolResult variant"),
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
            _ => panic!("expected Custom variant"),
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

        let assistant = Message::Assistant(sample_assistant_message());
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
            UserContent::Text(_) => panic!("expected Blocks variant"),
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
            _ => panic!("expected Text"),
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
            _ => panic!("expected Image"),
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
            _ => panic!("expected ToolCall"),
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
                    partial: sample_assistant_message(),
                },
                "start",
            ),
            (
                AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "hi".to_string(),
                    partial: sample_assistant_message(),
                },
                "text_delta",
            ),
            (
                AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message: sample_assistant_message(),
                },
                "done",
            ),
            (
                AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: sample_assistant_message(),
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
            partial: sample_assistant_message(),
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
            _ => panic!("expected TextEnd"),
        }
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
}
