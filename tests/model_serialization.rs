//! Unit tests for model message/content serialization and invariants.
//!
//! Tests JSON round-trips for all Message variants, `ContentBlock` types,
//! enums, and usage tracking. Validates that serialization matches
//! provider expectations and handles edge cases correctly.

mod common;

use common::TestHarness;
use pi::model::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Cost, ImageContent, Message, StopReason,
    TextContent, ThinkingContent, ThinkingLevel, ToolCall, ToolResultMessage, Usage, UserContent,
    UserMessage,
};
use serde_json::{Value, json};

// ============================================================================
// Helper Functions
// ============================================================================

#[allow(dead_code)]
fn assert_json_round_trip<T>(harness: &TestHarness, name: &str, value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug + PartialEq,
{
    harness.log().info("round_trip", format!("Testing: {name}"));
    let serialized = serde_json::to_string(value).expect("serialize");
    harness
        .log()
        .debug("round_trip", format!("JSON: {serialized}"));
    let deserialized: T = serde_json::from_str(&serialized).expect("deserialize");
    assert_eq!(
        value, &deserialized,
        "{name}: round-trip mismatch\noriginal: {value:?}\ndeserialized: {deserialized:?}"
    );
}

fn assert_json_contains(harness: &TestHarness, name: &str, json: &str, key: &str) {
    harness
        .log()
        .debug("assert", format!("{name}: checking key '{key}'"));
    let parsed: Value = serde_json::from_str(json).expect("parse JSON");
    assert!(
        json.contains(key),
        "{name}: expected key '{key}' in JSON: {json}"
    );
    let _ = parsed; // suppress unused warning
}

fn make_assistant_message(content: Vec<ContentBlock>) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-3-5-sonnet-20241022".to_string(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 1_700_000_000,
    }
}

// ============================================================================
// Message Round-Trip Tests
// ============================================================================

#[test]
fn test_user_message_text_round_trip() {
    let harness = TestHarness::new("user_message_text_round_trip");

    let msg = Message::User(UserMessage {
        content: UserContent::Text("Hello, world!".to_string()),
        timestamp: 1_700_000_000,
    });

    let json = serde_json::to_string(&msg).unwrap();
    harness.log().info_ctx("serialize", "User message", |ctx| {
        ctx.push(("json".into(), json.clone()));
    });

    // Check role tag
    assert_json_contains(&harness, "user_text", &json, "\"role\":\"user\"");

    // Deserialize and verify
    let parsed: Message = serde_json::from_str(&json).unwrap();
    if let Message::User(user) = parsed {
        if let UserContent::Text(text) = &user.content {
            assert_eq!(text, "Hello, world!");
        } else {
            panic!("Expected Text content");
        }
    } else {
        panic!("Expected User message");
    }
}

#[test]
fn test_user_message_blocks_round_trip() {
    let harness = TestHarness::new("user_message_blocks_round_trip");

    let msg = Message::User(UserMessage {
        content: UserContent::Blocks(vec![
            ContentBlock::Text(TextContent::new("First block")),
            ContentBlock::Image(ImageContent {
                data: "aGVsbG8=".to_string(), // "hello" base64
                mime_type: "image/png".to_string(),
            }),
        ]),
        timestamp: 1_700_000_000,
    });

    let json = serde_json::to_string(&msg).unwrap();
    harness.log().info("serialize", format!("JSON: {json}"));

    let parsed: Message = serde_json::from_str(&json).unwrap();
    if let Message::User(user) = parsed {
        if let UserContent::Blocks(blocks) = &user.content {
            assert_eq!(blocks.len(), 2);
        } else {
            panic!("Expected Blocks content");
        }
    } else {
        panic!("Expected User message");
    }
}

#[test]
fn test_assistant_message_round_trip() {
    let harness = TestHarness::new("assistant_message_round_trip");

    let msg = Message::Assistant(AssistantMessage {
        content: vec![
            ContentBlock::Text(TextContent::new("Hello")),
            ContentBlock::Thinking(ThinkingContent {
                thinking: "Let me think...".to_string(),
                thinking_signature: Some("sig123".to_string()),
            }),
        ],
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-3-5-sonnet-20241022".to_string(),
        usage: Usage {
            input: 100,
            output: 50,
            cache_read: 10,
            cache_write: 5,
            total_tokens: 165,
            cost: Cost {
                input: 0.001,
                output: 0.002,
                cache_read: 0.0001,
                cache_write: 0.0001,
                total: 0.0032,
            },
        },
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 1_700_000_000,
    });

    let json = serde_json::to_string(&msg).unwrap();
    harness
        .log()
        .info("serialize", format!("JSON length: {}", json.len()));

    // Verify key fields present
    assert_json_contains(&harness, "assistant", &json, "\"role\":\"assistant\"");
    assert_json_contains(&harness, "assistant", &json, "\"provider\":\"anthropic\"");
    assert_json_contains(&harness, "assistant", &json, "\"stopReason\":\"stop\"");

    let parsed: Message = serde_json::from_str(&json).unwrap();
    if let Message::Assistant(assistant) = parsed {
        assert_eq!(assistant.content.len(), 2);
        assert_eq!(assistant.usage.input, 100);
        assert_eq!(assistant.stop_reason, StopReason::Stop);
    } else {
        panic!("Expected Assistant message");
    }
}

#[test]
fn test_assistant_message_with_error() {
    let harness = TestHarness::new("assistant_message_with_error");

    let msg = Message::Assistant(AssistantMessage {
        content: vec![],
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-3-5-sonnet-20241022".to_string(),
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        error_message: Some("Rate limit exceeded".to_string()),
        timestamp: 1_700_000_000,
    });

    let json = serde_json::to_string(&msg).unwrap();
    harness.log().info("serialize", format!("JSON: {json}"));

    assert_json_contains(&harness, "error_msg", &json, "\"errorMessage\"");
    assert_json_contains(&harness, "error_msg", &json, "Rate limit exceeded");

    let parsed: Message = serde_json::from_str(&json).unwrap();
    if let Message::Assistant(assistant) = parsed {
        assert_eq!(
            assistant.error_message,
            Some("Rate limit exceeded".to_string())
        );
        assert_eq!(assistant.stop_reason, StopReason::Error);
    } else {
        panic!("Expected Assistant message");
    }
}

#[test]
fn test_tool_result_message_round_trip() {
    let harness = TestHarness::new("tool_result_message_round_trip");

    let msg = Message::ToolResult(ToolResultMessage {
        tool_call_id: "call_123".to_string(),
        tool_name: "read".to_string(),
        content: vec![ContentBlock::Text(TextContent::new("File contents here"))],
        details: Some(json!({
            "lines": 42,
            "truncated": false
        })),
        is_error: false,
        timestamp: 1_700_000_000,
    });

    let json = serde_json::to_string(&msg).unwrap();
    harness.log().info("serialize", format!("JSON: {json}"));

    assert_json_contains(&harness, "tool_result", &json, "\"role\":\"toolResult\"");
    assert_json_contains(
        &harness,
        "tool_result",
        &json,
        "\"toolCallId\":\"call_123\"",
    );
    assert_json_contains(&harness, "tool_result", &json, "\"toolName\":\"read\"");

    let parsed: Message = serde_json::from_str(&json).unwrap();
    if let Message::ToolResult(result) = parsed {
        assert_eq!(result.tool_call_id, "call_123");
        assert_eq!(result.tool_name, "read");
        assert!(!result.is_error);
        assert!(result.details.is_some());
    } else {
        panic!("Expected ToolResult message");
    }
}

#[test]
fn test_tool_result_error() {
    let harness = TestHarness::new("tool_result_error");

    let msg = Message::ToolResult(ToolResultMessage {
        tool_call_id: "call_456".to_string(),
        tool_name: "bash".to_string(),
        content: vec![ContentBlock::Text(TextContent::new(
            "Command failed: exit code 1",
        ))],
        details: None,
        is_error: true,
        timestamp: 1_700_000_000,
    });

    let json = serde_json::to_string(&msg).unwrap();
    harness.log().info("serialize", format!("JSON: {json}"));

    assert_json_contains(&harness, "tool_error", &json, "\"isError\":true");

    let parsed: Message = serde_json::from_str(&json).unwrap();
    if let Message::ToolResult(result) = parsed {
        assert!(result.is_error);
    } else {
        panic!("Expected ToolResult message");
    }
}

// ============================================================================
// ContentBlock Tests
// ============================================================================

#[test]
fn test_content_block_text() {
    let harness = TestHarness::new("content_block_text");

    let block = ContentBlock::Text(TextContent {
        text: "Hello, world!".to_string(),
        text_signature: Some("sig_abc".to_string()),
    });

    let json = serde_json::to_string(&block).unwrap();
    harness.log().info("serialize", format!("JSON: {json}"));

    assert_json_contains(&harness, "text_block", &json, "\"type\":\"text\"");
    assert_json_contains(
        &harness,
        "text_block",
        &json,
        "\"textSignature\":\"sig_abc\"",
    );

    let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
    if let ContentBlock::Text(text) = parsed {
        assert_eq!(text.text, "Hello, world!");
        assert_eq!(text.text_signature, Some("sig_abc".to_string()));
    } else {
        panic!("Expected Text block");
    }
}

#[test]
fn test_content_block_text_no_signature() {
    let harness = TestHarness::new("content_block_text_no_signature");

    let block = ContentBlock::Text(TextContent::new("Simple text"));

    let json = serde_json::to_string(&block).unwrap();
    harness.log().info("serialize", format!("JSON: {json}"));

    // textSignature should be omitted when None
    assert!(
        !json.contains("textSignature"),
        "textSignature should be omitted"
    );

    let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
    if let ContentBlock::Text(text) = parsed {
        assert_eq!(text.text, "Simple text");
        assert!(text.text_signature.is_none());
    } else {
        panic!("Expected Text block");
    }
}

#[test]
fn test_content_block_thinking() {
    let harness = TestHarness::new("content_block_thinking");

    let block = ContentBlock::Thinking(ThinkingContent {
        thinking: "Analyzing the problem...".to_string(),
        thinking_signature: Some("think_sig".to_string()),
    });

    let json = serde_json::to_string(&block).unwrap();
    harness.log().info("serialize", format!("JSON: {json}"));

    assert_json_contains(&harness, "thinking_block", &json, "\"type\":\"thinking\"");
    assert_json_contains(&harness, "thinking_block", &json, "Analyzing the problem");

    let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
    if let ContentBlock::Thinking(thinking) = parsed {
        assert_eq!(thinking.thinking, "Analyzing the problem...");
    } else {
        panic!("Expected Thinking block");
    }
}

#[test]
fn test_content_block_image() {
    let harness = TestHarness::new("content_block_image");

    let block = ContentBlock::Image(ImageContent {
        data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==".to_string(),
        mime_type: "image/png".to_string(),
    });

    let json = serde_json::to_string(&block).unwrap();
    harness
        .log()
        .info("serialize", format!("JSON length: {}", json.len()));

    assert_json_contains(&harness, "image_block", &json, "\"type\":\"image\"");
    assert_json_contains(&harness, "image_block", &json, "\"mimeType\":\"image/png\"");

    let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
    if let ContentBlock::Image(image) = parsed {
        assert_eq!(image.mime_type, "image/png");
        assert!(!image.data.is_empty());
    } else {
        panic!("Expected Image block");
    }
}

#[test]
fn test_content_block_tool_call() {
    let harness = TestHarness::new("content_block_tool_call");

    let block = ContentBlock::ToolCall(ToolCall {
        id: "call_789".to_string(),
        name: "read".to_string(),
        arguments: json!({
            "path": "/tmp/test.txt",
            "offset": 0,
            "limit": 100
        }),
        thought_signature: Some("thought_sig".to_string()),
    });

    let json = serde_json::to_string(&block).unwrap();
    harness.log().info("serialize", format!("JSON: {json}"));

    assert_json_contains(&harness, "tool_call", &json, "\"type\":\"toolCall\"");
    assert_json_contains(&harness, "tool_call", &json, "\"name\":\"read\"");
    assert_json_contains(&harness, "tool_call", &json, "\"path\":\"/tmp/test.txt\"");

    let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
    if let ContentBlock::ToolCall(call) = parsed {
        assert_eq!(call.id, "call_789");
        assert_eq!(call.name, "read");
        assert_eq!(call.arguments["path"], "/tmp/test.txt");
    } else {
        panic!("Expected ToolCall block");
    }
}

#[test]
fn test_content_block_tool_call_complex_args() {
    let harness = TestHarness::new("content_block_tool_call_complex_args");

    let block = ContentBlock::ToolCall(ToolCall {
        id: "call_complex".to_string(),
        name: "edit".to_string(),
        arguments: json!({
            "path": "/src/main.rs",
            "old_string": "fn main() {\n    println!(\"Hello\");\n}",
            "new_string": "fn main() {\n    println!(\"Hello, World!\");\n}",
            "replace_all": false,
            "nested": {
                "deep": {
                    "value": [1, 2, 3]
                }
            }
        }),
        thought_signature: None,
    });

    let json = serde_json::to_string(&block).unwrap();
    harness
        .log()
        .info_ctx("serialize", "Complex tool call", |ctx| {
            ctx.push(("json_length".into(), json.len().to_string()));
        });

    let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
    if let ContentBlock::ToolCall(call) = parsed {
        assert_eq!(call.arguments["nested"]["deep"]["value"][0], 1);
    } else {
        panic!("Expected ToolCall block");
    }
}

// ============================================================================
// StopReason Tests
// ============================================================================

#[test]
fn test_stop_reason_serialization() {
    let harness = TestHarness::new("stop_reason_serialization");

    let test_cases = [
        (StopReason::Stop, "\"stop\""),
        (StopReason::Length, "\"length\""),
        (StopReason::ToolUse, "\"toolUse\""),
        (StopReason::Error, "\"error\""),
        (StopReason::Aborted, "\"aborted\""),
    ];

    for (reason, expected) in test_cases {
        let json = serde_json::to_string(&reason).unwrap();
        harness.log().info_ctx("stop_reason", "Serialized", |ctx| {
            ctx.push(("reason".into(), format!("{reason:?}")));
            ctx.push(("json".into(), json.clone()));
        });
        assert_eq!(json, expected, "StopReason::{reason:?} mismatch");

        let parsed: StopReason = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, reason);
    }
}

#[test]
fn test_stop_reason_default() {
    let harness = TestHarness::new("stop_reason_default");

    let default = StopReason::default();
    harness
        .log()
        .info("default", format!("Default: {default:?}"));
    assert_eq!(default, StopReason::Stop);
}

// ============================================================================
// ThinkingLevel Tests
// ============================================================================

#[test]
fn test_thinking_level_serialization() {
    let harness = TestHarness::new("thinking_level_serialization");

    let test_cases = [
        (ThinkingLevel::Off, "\"off\""),
        (ThinkingLevel::Minimal, "\"minimal\""),
        (ThinkingLevel::Low, "\"low\""),
        (ThinkingLevel::Medium, "\"medium\""),
        (ThinkingLevel::High, "\"high\""),
        (ThinkingLevel::XHigh, "\"xhigh\""),
    ];

    for (level, expected) in test_cases {
        let json = serde_json::to_string(&level).unwrap();
        harness
            .log()
            .info_ctx("thinking_level", "Serialized", |ctx| {
                ctx.push(("level".into(), format!("{level:?}")));
                ctx.push(("json".into(), json.clone()));
            });
        assert_eq!(json, expected, "ThinkingLevel::{level:?} mismatch");

        let parsed: ThinkingLevel = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, level);
    }
}

#[test]
fn test_thinking_level_from_str() {
    let harness = TestHarness::new("thinking_level_from_str");

    let test_cases = [
        ("off", ThinkingLevel::Off),
        ("OFF", ThinkingLevel::Off),
        ("minimal", ThinkingLevel::Minimal),
        ("MINIMAL", ThinkingLevel::Minimal),
        ("low", ThinkingLevel::Low),
        ("medium", ThinkingLevel::Medium),
        ("high", ThinkingLevel::High),
        ("xhigh", ThinkingLevel::XHigh),
        ("XHIGH", ThinkingLevel::XHigh),
    ];

    for (input, expected) in test_cases {
        let parsed: ThinkingLevel = input.parse().unwrap();
        harness.log().info_ctx("parse", "Parsed", |ctx| {
            ctx.push(("input".into(), input.to_string()));
            ctx.push(("result".into(), format!("{parsed:?}")));
        });
        assert_eq!(parsed, expected);
    }
}

#[test]
fn test_thinking_level_from_str_invalid() {
    let harness = TestHarness::new("thinking_level_from_str_invalid");

    let result: Result<ThinkingLevel, _> = "invalid".parse();
    harness.log().info("parse", format!("Result: {result:?}"));
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Invalid thinking level"));
}

#[test]
fn test_thinking_level_display() {
    let harness = TestHarness::new("thinking_level_display");

    let test_cases = [
        (ThinkingLevel::Off, "off"),
        (ThinkingLevel::Minimal, "minimal"),
        (ThinkingLevel::Low, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::XHigh, "xhigh"),
    ];

    for (level, expected) in test_cases {
        let display = format!("{level}");
        harness.log().info_ctx("display", "Display", |ctx| {
            ctx.push(("level".into(), format!("{level:?}")));
            ctx.push(("display".into(), display.clone()));
        });
        assert_eq!(display, expected);
    }
}

#[test]
fn test_thinking_level_default_budget() {
    let harness = TestHarness::new("thinking_level_default_budget");

    let test_cases = [
        (ThinkingLevel::Off, 0),
        (ThinkingLevel::Minimal, 1024),
        (ThinkingLevel::Low, 2048),
        (ThinkingLevel::Medium, 8192),
        (ThinkingLevel::High, 16384),
        (ThinkingLevel::XHigh, 32768),
    ];

    for (level, expected) in test_cases {
        let budget = level.default_budget();
        harness.log().info_ctx("budget", "Budget", |ctx| {
            ctx.push(("level".into(), format!("{level:?}")));
            ctx.push(("budget".into(), budget.to_string()));
        });
        assert_eq!(budget, expected);
    }
}

#[test]
fn test_thinking_level_default() {
    let harness = TestHarness::new("thinking_level_default");

    let default = ThinkingLevel::default();
    harness
        .log()
        .info("default", format!("Default: {default:?}"));
    assert_eq!(default, ThinkingLevel::Off);
}

// ============================================================================
// Usage and Cost Tests
// ============================================================================

#[test]
fn test_usage_default() {
    let harness = TestHarness::new("usage_default");

    let usage = Usage::default();
    harness.log().info_ctx("default", "Usage defaults", |ctx| {
        ctx.push(("input".into(), usage.input.to_string()));
        ctx.push(("output".into(), usage.output.to_string()));
        ctx.push(("total".into(), usage.total_tokens.to_string()));
    });

    assert_eq!(usage.input, 0);
    assert_eq!(usage.output, 0);
    assert_eq!(usage.cache_read, 0);
    assert_eq!(usage.cache_write, 0);
    assert_eq!(usage.total_tokens, 0);
}

#[test]
fn test_usage_round_trip() {
    let harness = TestHarness::new("usage_round_trip");

    let usage = Usage {
        input: 1000,
        output: 500,
        cache_read: 200,
        cache_write: 100,
        total_tokens: 1800,
        cost: Cost {
            input: 0.01,
            output: 0.02,
            cache_read: 0.001,
            cache_write: 0.001,
            total: 0.032,
        },
    };

    let json = serde_json::to_string(&usage).unwrap();
    harness.log().info("serialize", format!("JSON: {json}"));

    let parsed: Usage = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.input, 1000);
    assert_eq!(parsed.output, 500);
    assert_eq!(parsed.total_tokens, 1800);
    assert!((parsed.cost.total - 0.032).abs() < 0.0001);
}

#[test]
fn test_cost_default() {
    let harness = TestHarness::new("cost_default");

    let cost = Cost::default();
    harness.log().info("default", format!("Cost: {cost:?}"));

    assert!((cost.input - 0.0).abs() < f64::EPSILON);
    assert!((cost.total - 0.0).abs() < f64::EPSILON);
}

// ============================================================================
// AssistantMessageEvent Tests
// ============================================================================

#[test]
fn test_assistant_message_event_start() {
    let harness = TestHarness::new("assistant_message_event_start");

    let event = AssistantMessageEvent::Start {
        partial: make_assistant_message(vec![]),
    };

    let json = serde_json::to_string(&event).unwrap();
    harness.log().info("serialize", format!("JSON: {json}"));

    assert_json_contains(&harness, "event_start", &json, "\"type\":\"start\"");
}

#[test]
fn test_assistant_message_event_text_delta() {
    let harness = TestHarness::new("assistant_message_event_text_delta");

    let event = AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "Hello".to_string(),
        partial: make_assistant_message(vec![ContentBlock::Text(TextContent::new("Hello"))]),
    };

    let json = serde_json::to_string(&event).unwrap();
    harness.log().info("serialize", format!("JSON: {json}"));

    assert_json_contains(&harness, "text_delta", &json, "\"type\":\"text_delta\"");
    assert_json_contains(&harness, "text_delta", &json, "\"contentIndex\":0");
    assert_json_contains(&harness, "text_delta", &json, "\"delta\":\"Hello\"");
}

#[test]
fn test_assistant_message_event_done() {
    let harness = TestHarness::new("assistant_message_event_done");

    let event = AssistantMessageEvent::Done {
        reason: StopReason::Stop,
        message: make_assistant_message(vec![ContentBlock::Text(TextContent::new("Complete"))]),
    };

    let json = serde_json::to_string(&event).unwrap();
    harness.log().info("serialize", format!("JSON: {json}"));

    assert_json_contains(&harness, "done", &json, "\"type\":\"done\"");
    assert_json_contains(&harness, "done", &json, "\"reason\":\"stop\"");
}

#[test]
fn test_assistant_message_event_tool_call_end() {
    let harness = TestHarness::new("assistant_message_event_tool_call_end");

    let tool_call = ToolCall {
        id: "call_test".to_string(),
        name: "bash".to_string(),
        arguments: json!({"command": "ls -la"}),
        thought_signature: None,
    };

    let event = AssistantMessageEvent::ToolCallEnd {
        content_index: 0,
        tool_call: tool_call.clone(),
        partial: make_assistant_message(vec![ContentBlock::ToolCall(tool_call)]),
    };

    let json = serde_json::to_string(&event).unwrap();
    harness.log().info("serialize", format!("JSON: {json}"));

    assert_json_contains(
        &harness,
        "tool_call_end",
        &json,
        "\"type\":\"toolcall_end\"",
    );
    assert_json_contains(&harness, "tool_call_end", &json, "\"toolCall\"");
}

// ============================================================================
// Edge Cases and Unicode Tests
// ============================================================================

#[test]
fn test_empty_content_blocks() {
    let harness = TestHarness::new("empty_content_blocks");

    let msg = Message::Assistant(AssistantMessage {
        content: vec![],
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-3-5-sonnet-20241022".to_string(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 1_700_000_000,
    });

    let json = serde_json::to_string(&msg).unwrap();
    harness.log().info("serialize", format!("JSON: {json}"));

    assert_json_contains(&harness, "empty", &json, "\"content\":[]");

    let parsed: Message = serde_json::from_str(&json).unwrap();
    if let Message::Assistant(assistant) = parsed {
        assert!(assistant.content.is_empty());
    } else {
        panic!("Expected Assistant message");
    }
}

#[test]
fn test_unicode_content() {
    let harness = TestHarness::new("unicode_content");

    let msg = Message::User(UserMessage {
        content: UserContent::Text("Hello 你好 שלום مرحبا 🌍🚀".to_string()),
        timestamp: 1_700_000_000,
    });

    let json = serde_json::to_string(&msg).unwrap();
    harness.log().info("serialize", format!("JSON: {json}"));

    let parsed: Message = serde_json::from_str(&json).unwrap();
    if let Message::User(user) = parsed {
        if let UserContent::Text(text) = &user.content {
            assert!(text.contains("你好"));
            assert!(text.contains("שלום"));
            assert!(text.contains("🌍"));
        } else {
            panic!("Expected Text content");
        }
    } else {
        panic!("Expected User message");
    }
}

#[test]
fn test_multiline_text_content() {
    let harness = TestHarness::new("multiline_text_content");

    let multiline = "Line 1\nLine 2\n\nLine 4 with blank above\n\tIndented line";
    let block = ContentBlock::Text(TextContent::new(multiline));

    let json = serde_json::to_string(&block).unwrap();
    harness
        .log()
        .info("serialize", format!("JSON length: {}", json.len()));

    let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
    if let ContentBlock::Text(text) = parsed {
        assert_eq!(text.text, multiline);
        assert!(text.text.contains('\n'));
        assert!(text.text.contains('\t'));
    } else {
        panic!("Expected Text block");
    }
}

#[test]
fn test_special_characters_in_tool_args() {
    let harness = TestHarness::new("special_characters_in_tool_args");

    let block = ContentBlock::ToolCall(ToolCall {
        id: "call_special".to_string(),
        name: "bash".to_string(),
        arguments: json!({
            "command": "echo \"Hello, World!\" && ls -la | grep 'test'"
        }),
        thought_signature: None,
    });

    let json = serde_json::to_string(&block).unwrap();
    harness.log().info("serialize", format!("JSON: {json}"));

    let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
    if let ContentBlock::ToolCall(call) = parsed {
        let cmd = call.arguments["command"].as_str().unwrap();
        assert!(cmd.contains("&&"));
        assert!(cmd.contains('|'));
        assert!(cmd.contains("grep"));
    } else {
        panic!("Expected ToolCall block");
    }
}

#[test]
fn test_large_tool_arguments() {
    let harness = TestHarness::new("large_tool_arguments");

    // Create a large payload (2KB string)
    let large_payload = "A".repeat(2048);

    let block = ContentBlock::ToolCall(ToolCall {
        id: "call_large".to_string(),
        name: "write".to_string(),
        arguments: json!({
            "path": "/tmp/large.txt",
            "content": large_payload
        }),
        thought_signature: None,
    });

    let json = serde_json::to_string(&block).unwrap();
    harness.log().info_ctx("serialize", "Large args", |ctx| {
        ctx.push(("json_length".into(), json.len().to_string()));
    });

    assert!(json.len() > 2048);

    let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
    if let ContentBlock::ToolCall(call) = parsed {
        let content = call.arguments["content"].as_str().unwrap();
        assert_eq!(content.len(), 2048);
    } else {
        panic!("Expected ToolCall block");
    }
}

// ============================================================================
// Mixed Content Tests
// ============================================================================

#[test]
fn test_mixed_content_sequence() {
    let harness = TestHarness::new("mixed_content_sequence");

    let msg = Message::Assistant(AssistantMessage {
        content: vec![
            ContentBlock::Thinking(ThinkingContent {
                thinking: "Let me analyze this...".to_string(),
                thinking_signature: None,
            }),
            ContentBlock::Text(TextContent::new("Based on my analysis:")),
            ContentBlock::ToolCall(ToolCall {
                id: "call_1".to_string(),
                name: "read".to_string(),
                arguments: json!({"path": "/src/main.rs"}),
                thought_signature: None,
            }),
            ContentBlock::ToolCall(ToolCall {
                id: "call_2".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "fn main", "path": "/src"}),
                thought_signature: None,
            }),
        ],
        api: "anthropic-messages".to_string(),
        provider: "anthropic".to_string(),
        model: "claude-3-5-sonnet-20241022".to_string(),
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        timestamp: 1_700_000_000,
    });

    let json = serde_json::to_string(&msg).unwrap();
    harness.log().info_ctx("serialize", "Mixed content", |ctx| {
        ctx.push(("json_length".into(), json.len().to_string()));
    });

    let parsed: Message = serde_json::from_str(&json).unwrap();
    if let Message::Assistant(assistant) = parsed {
        assert_eq!(assistant.content.len(), 4);
        assert!(matches!(assistant.content[0], ContentBlock::Thinking(_)));
        assert!(matches!(assistant.content[1], ContentBlock::Text(_)));
        assert!(matches!(assistant.content[2], ContentBlock::ToolCall(_)));
        assert!(matches!(assistant.content[3], ContentBlock::ToolCall(_)));
    } else {
        panic!("Expected Assistant message");
    }
}

#[test]
fn test_multiple_tool_results() {
    let harness = TestHarness::new("multiple_tool_results");

    // Simulate multiple tool results in sequence
    let results = [
        Message::ToolResult(ToolResultMessage {
            tool_call_id: "call_1".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("fn main() {}"))],
            details: Some(json!({"lines": 1})),
            is_error: false,
            timestamp: 1_700_000_001,
        }),
        Message::ToolResult(ToolResultMessage {
            tool_call_id: "call_2".to_string(),
            tool_name: "grep".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(
                "src/main.rs:1:fn main()",
            ))],
            details: Some(json!({"matches": 1})),
            is_error: false,
            timestamp: 1_700_000_002,
        }),
    ];

    for (i, result) in results.iter().enumerate() {
        let json = serde_json::to_string(result).unwrap();
        harness
            .log()
            .info_ctx("serialize", format!("Result {i}"), |ctx| {
                ctx.push(("json".into(), json.clone()));
            });

        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Message::ToolResult(_)));
    }
}

// ============================================================================
// Deserialization from Provider Formats
// ============================================================================

#[test]
fn test_deserialize_anthropic_style_message() {
    let harness = TestHarness::new("deserialize_anthropic_style_message");

    // Simulate a message as it might come from session storage
    let json = r#"{
        "role": "assistant",
        "content": [
            {"type": "text", "text": "Hello from the API"}
        ],
        "api": "anthropic-messages",
        "provider": "anthropic",
        "model": "claude-3-5-sonnet-20241022",
        "usage": {
            "input": 10,
            "output": 5,
            "cacheRead": 0,
            "cacheWrite": 0,
            "totalTokens": 15,
            "cost": {
                "input": 0.0001,
                "output": 0.0002,
                "cacheRead": 0,
                "cacheWrite": 0,
                "total": 0.0003
            }
        },
        "stopReason": "stop",
        "timestamp": 1700000000
    }"#;

    harness
        .log()
        .info("deserialize", "Parsing Anthropic-style JSON");

    let parsed: Message = serde_json::from_str(json).unwrap();
    if let Message::Assistant(assistant) = parsed {
        assert_eq!(assistant.provider, "anthropic");
        assert_eq!(assistant.usage.input, 10);
        assert_eq!(assistant.stop_reason, StopReason::Stop);
    } else {
        panic!("Expected Assistant message");
    }
}

#[test]
fn test_deserialize_user_text_vs_blocks() {
    let harness = TestHarness::new("deserialize_user_text_vs_blocks");

    // Test simple text format
    let text_json = r#"{
        "role": "user",
        "content": "Simple text message",
        "timestamp": 1700000000
    }"#;

    let parsed: Message = serde_json::from_str(text_json).unwrap();
    harness.log().info("text", "Parsed text format");
    if let Message::User(user) = &parsed {
        assert!(matches!(user.content, UserContent::Text(_)));
    }

    // Test blocks format
    let blocks_json = r#"{
        "role": "user",
        "content": [
            {"type": "text", "text": "Block text"}
        ],
        "timestamp": 1700000000
    }"#;

    let parsed: Message = serde_json::from_str(blocks_json).unwrap();
    harness.log().info("blocks", "Parsed blocks format");
    if let Message::User(user) = &parsed {
        assert!(matches!(user.content, UserContent::Blocks(_)));
    }
}
