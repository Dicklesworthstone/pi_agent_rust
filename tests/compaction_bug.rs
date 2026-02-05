use pi::compaction::{ResolvedCompactionSettings, prepare_compaction};
use pi::model::{AssistantMessage, ContentBlock, Cost, StopReason, TextContent, Usage};
use pi::session::{EntryBase, MessageEntry, SessionEntry, SessionMessage};

#[test]
fn test_compaction_usage_double_counting_bug() {
    // Create an assistant message with specific usage
    let usage = Usage {
        input: 100, // Total input tokens (includes cached)
        output: 10,
        cache_read: 20,
        cache_write: 30,
        total_tokens: 0, // Simulate missing/default total_tokens
        cost: Cost::default(),
    };

    let message = SessionMessage::Assistant {
        message: AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("test"))],
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test".to_string(),
            usage,
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        },
    };

    let entry = SessionEntry::Message(MessageEntry {
        base: EntryBase::new(None, "msg1".to_string()),
        message,
    });

    let entries = vec![entry];
    let settings = ResolvedCompactionSettings::default();

    // prepare_compaction calculates tokens_before using estimate_context_tokens
    // which uses calculate_context_tokens
    let prep = prepare_compaction(&entries, settings);

    // We expect prep to be Some because we passed entries.
    // However, prepare_compaction has conditions:
    // It scans for previous compaction. If none, boundary_start=0.
    // It estimates tokens for usage_messages (from boundary_start to end).

    assert!(prep.is_some());
    let prep = prep.unwrap();

    // The bug: calculate_context_tokens fallback sums input + output + cache_read + cache_write
    // 100 + 10 + 20 + 30 = 160
    // Correct behavior (assuming input includes cache): 100 + 10 = 110

    // Check what we get currently (to confirm reproduction)
    // If bug exists, it will be 160.
    // If fixed, it will be 110.

    // We assert equality to the BUGGY value to confirm reproduction,
    // or we assert equality to CORRECT value and expect failure?
    // Let's assert equality to CORRECT value and expect it to FAIL.
    assert_eq!(
        prep.tokens_before, 110,
        "Expected 110 tokens (100 input + 10 output), got double-counted tokens"
    );
}
