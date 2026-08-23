//! Event classification and timeout policy tests.

use super::*;

// ===================================================================
// #50: informational-vs-actionable event classification drives the
// per-event default timeout.
// ===================================================================

#[test]
fn informational_events_use_short_timeout() {
    let info_events = [
        ExtensionEventName::Startup,
        ExtensionEventName::AgentStart,
        ExtensionEventName::AgentEnd,
        ExtensionEventName::TurnStart,
        ExtensionEventName::TurnEnd,
        ExtensionEventName::MessageStart,
        ExtensionEventName::MessageUpdate,
        ExtensionEventName::MessageEnd,
        ExtensionEventName::ToolExecutionStart,
        ExtensionEventName::ToolExecutionUpdate,
        ExtensionEventName::ToolExecutionEnd,
        ExtensionEventName::SessionStart,
        ExtensionEventName::SessionSwitch,
        ExtensionEventName::SessionFork,
        ExtensionEventName::SessionCompact,
        ExtensionEventName::SessionTree,
        ExtensionEventName::SessionShutdown,
        ExtensionEventName::ModelSelect,
        ExtensionEventName::UserBash,
    ];
    for event in info_events {
        assert!(
            event.is_informational(),
            "{event} should be classified as informational"
        );
        assert_eq!(
            event.default_timeout_ms(),
            EXTENSION_INFO_EVENT_TIMEOUT_MS,
            "{event} should use the short default timeout"
        );
    }
}

#[test]
fn actionable_events_use_full_timeout() {
    // These events feed a decision (block/cancel/transform), so a
    // handler needs room to do real work before a verdict is expected.
    // ToolResult belongs here per its ExtensionEvent docstring
    // ("can modify result") — the dispatcher consumes handler-returned
    // changes to the tool result content.
    let actionable_events = [
        ExtensionEventName::Input,
        ExtensionEventName::BeforeAgentStart,
        ExtensionEventName::Context,
        ExtensionEventName::ToolCall,
        ExtensionEventName::ToolResult,
        ExtensionEventName::SessionBeforeSwitch,
        ExtensionEventName::SessionBeforeFork,
        ExtensionEventName::SessionBeforeTree,
        ExtensionEventName::ResourcesDiscover,
    ];
    for event in actionable_events {
        assert!(
            !event.is_informational(),
            "{event} should NOT be classified as informational"
        );
        assert_eq!(
            event.default_timeout_ms(),
            EXTENSION_EVENT_TIMEOUT_MS,
            "{event} should use the full default timeout"
        );
    }
}

#[test]
fn session_before_compact_uses_dedicated_long_running_timeout() {
    // gh #178: session_before_compact is actionable AND is the one hook
    // whose documented contract includes awaiting a real provider request
    // through the host compaction bridge (`ctx.compact()`), so it gets a
    // dedicated budget well above the generic actionable timeout. The
    // planted negative: the post-hoc informational SessionCompact event
    // must NOT inherit the long budget.
    let event = ExtensionEventName::SessionBeforeCompact;
    assert!(!event.is_informational());
    assert_eq!(
        event.default_timeout_ms(),
        EXTENSION_COMPACT_EVENT_TIMEOUT_MS
    );
    assert!(
        EXTENSION_COMPACT_EVENT_TIMEOUT_MS > EXTENSION_EVENT_TIMEOUT_MS,
        "compact hook budget must exceed the generic event budget"
    );
    assert!(
        EXTENSION_COMPACT_EVENT_TIMEOUT_MS >= EXTENSION_PROVIDER_BUDGET_MS,
        "compact hook budget must cover at least one full provider-stream budget"
    );
    assert_eq!(
        ExtensionEventName::SessionCompact.default_timeout_ms(),
        EXTENSION_INFO_EVENT_TIMEOUT_MS,
        "post-hoc SessionCompact notification must keep the short budget"
    );
}

#[test]
fn info_timeout_is_strictly_shorter_than_general_timeout() {
    assert!(
        EXTENSION_INFO_EVENT_TIMEOUT_MS < EXTENSION_EVENT_TIMEOUT_MS,
        "info timeout ({EXTENSION_INFO_EVENT_TIMEOUT_MS}ms) must be strictly shorter than general timeout ({EXTENSION_EVENT_TIMEOUT_MS}ms)"
    );
}
