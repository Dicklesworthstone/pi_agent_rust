#![allow(clippy::redundant_clone)]
//! Tests for extension event hook cancellation semantics (bd-2p8).
//!
//! Tests cover:
//! - `dispatch_cancellable_event` with `{cancelled: true}`, `{cancel: true}`, and `false`
//! - Session lifecycle before-hooks: switch, fork, compact
//! - Session lifecycle after-events fire-and-forget
//! - Cancellation payloads are forwarded to hooks
//! - No cancellation when no hooks registered

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::{Value, json};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_js_extension(harness: &common::TestHarness, source: &str) -> ExtensionManager {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_entry_path = harness.create_file("extensions/ext.mjs", source.as_bytes());
    let spec = JsExtensionLoadSpec::from_entry_path(&ext_entry_path).expect("load spec");

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start js runtime")
        }
    });
    manager.set_js_runtime(runtime);

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load extension");
        }
    });

    manager
}

// ---------------------------------------------------------------------------
// Extension sources
// ---------------------------------------------------------------------------

/// Extension that cancels `session_before_switch` via `{cancelled: true}`,
/// `session_before_fork` via `{cancel: true}`, and `session_before_compact` via `false`.
const SESSION_CANCEL_EXT: &str = r#"
export default function init(pi) {
    pi.on("session_before_switch", (event, ctx) => {
        return { cancelled: true, reason: "Extension vetoed switch" };
    });

    pi.on("session_before_fork", (event, ctx) => {
        return { cancel: true };
    });

    pi.on("session_before_compact", (event, ctx) => {
        return false;
    });

    pi.on("session_switch", (event, ctx) => {
        return null;
    });

    pi.on("session_fork", (event, ctx) => {
        return null;
    });

    pi.on("session_compact", (event, ctx) => {
        return null;
    });
}
"#;

/// Extension that does NOT cancel any session lifecycle events.
const SESSION_ALLOW_EXT: &str = r#"
export default function init(pi) {
    pi.on("session_before_switch", (event, ctx) => {
        return { cancelled: false };
    });

    pi.on("session_before_fork", (event, ctx) => {
        return null;
    });

    pi.on("session_before_compact", (event, ctx) => {
        return true;
    });
}
"#;

/// Extension with NO event hooks.
const NO_HOOKS_EXT: &str = r#"
export default function init(pi) {
    pi.registerCommand("noop", {
        description: "No-op command",
        handler: async () => null
    });
}
"#;

// ---------------------------------------------------------------------------
// Tests: cancellation via {cancelled: true}
// ---------------------------------------------------------------------------

#[test]
fn session_before_switch_cancelled_via_cancelled_object() {
    let harness = common::TestHarness::new("session_before_switch_cancelled_via_cancelled_object");
    let manager = load_js_extension(&harness, SESSION_CANCEL_EXT);

    let cancelled = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_cancellable_event(
                    ExtensionEventName::SessionBeforeSwitch,
                    Some(json!({
                        "reason": "resume",
                        "targetSessionFile": "/tmp/test-session.jsonl",
                    })),
                    5000,
                )
                .await
                .expect("dispatch cancellable")
        }
    });

    assert!(
        cancelled,
        "Expected cancellation when handler returns {{cancelled: true}}"
    );
}

// ---------------------------------------------------------------------------
// Tests: cancellation via {cancel: true}
// ---------------------------------------------------------------------------

#[test]
fn session_before_fork_cancelled_via_cancel_object() {
    let harness = common::TestHarness::new("session_before_fork_cancelled_via_cancel_object");
    let manager = load_js_extension(&harness, SESSION_CANCEL_EXT);

    let cancelled = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_cancellable_event(
                    ExtensionEventName::SessionBeforeFork,
                    Some(json!({
                        "entryId": "entry-123",
                        "summary": "test fork",
                    })),
                    5000,
                )
                .await
                .expect("dispatch cancellable")
        }
    });

    assert!(
        cancelled,
        "Expected cancellation when handler returns {{cancel: true}}"
    );
}

// ---------------------------------------------------------------------------
// Tests: cancellation via returning false
// ---------------------------------------------------------------------------

#[test]
fn session_before_compact_cancelled_via_false() {
    let harness = common::TestHarness::new("session_before_compact_cancelled_via_false");
    let manager = load_js_extension(&harness, SESSION_CANCEL_EXT);

    let cancelled = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_cancellable_event(
                    ExtensionEventName::SessionBeforeCompact,
                    Some(json!({ "sessionId": "session-abc" })),
                    5000,
                )
                .await
                .expect("dispatch cancellable")
        }
    });

    assert!(
        cancelled,
        "Expected cancellation when handler returns false"
    );
}

// ---------------------------------------------------------------------------
// Tests: no cancellation when hook allows the action
// ---------------------------------------------------------------------------

#[test]
fn session_before_switch_not_cancelled_when_allowed() {
    let harness = common::TestHarness::new("session_before_switch_not_cancelled_when_allowed");
    let manager = load_js_extension(&harness, SESSION_ALLOW_EXT);

    let cancelled = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_cancellable_event(
                    ExtensionEventName::SessionBeforeSwitch,
                    Some(json!({ "targetSessionFile": "/tmp/test.jsonl" })),
                    5000,
                )
                .await
                .expect("dispatch")
        }
    });

    assert!(
        !cancelled,
        "Should not cancel when handler returns {{cancelled: false}}"
    );
}

#[test]
fn session_before_fork_not_cancelled_when_null() {
    let harness = common::TestHarness::new("session_before_fork_not_cancelled_when_null");
    let manager = load_js_extension(&harness, SESSION_ALLOW_EXT);

    let cancelled = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_cancellable_event(
                    ExtensionEventName::SessionBeforeFork,
                    Some(json!({ "entryId": "e1" })),
                    5000,
                )
                .await
                .expect("dispatch")
        }
    });

    assert!(!cancelled, "Should not cancel when handler returns null");
}

#[test]
fn session_before_compact_not_cancelled_when_true() {
    let harness = common::TestHarness::new("session_before_compact_not_cancelled_when_true");
    let manager = load_js_extension(&harness, SESSION_ALLOW_EXT);

    let cancelled = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_cancellable_event(
                    ExtensionEventName::SessionBeforeCompact,
                    Some(json!({ "sessionId": "s1" })),
                    5000,
                )
                .await
                .expect("dispatch")
        }
    });

    assert!(!cancelled, "Should not cancel when handler returns true");
}

// ---------------------------------------------------------------------------
// Tests: after-events (fire-and-forget)
// ---------------------------------------------------------------------------

#[test]
fn session_after_events_dispatch_successfully() {
    let harness = common::TestHarness::new("session_after_events_dispatch_successfully");
    let manager = load_js_extension(&harness, SESSION_CANCEL_EXT);

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event(
                    ExtensionEventName::SessionSwitch,
                    Some(json!({
                        "reason": "resume",
                        "targetSessionFile": "/tmp/target.jsonl",
                        "sessionId": "new-session-id",
                    })),
                )
                .await
                .expect("session_switch after-event");

            manager
                .dispatch_event(
                    ExtensionEventName::SessionFork,
                    Some(json!({
                        "entryId": "e1",
                        "summary": "forked here",
                        "newSessionId": "fork-session-id",
                    })),
                )
                .await
                .expect("session_fork after-event");

            manager
                .dispatch_event(
                    ExtensionEventName::SessionCompact,
                    Some(json!({
                        "tokensBefore": 50000,
                        "firstKeptEntryId": "entry-42",
                    })),
                )
                .await
                .expect("session_compact after-event");
        }
    });
}

// ---------------------------------------------------------------------------
// Tests: payload forwarding
// ---------------------------------------------------------------------------

#[test]
fn session_before_hook_receives_payload() {
    let harness = common::TestHarness::new("session_before_hook_receives_payload");

    let echo_ext = r#"
export default function init(pi) {
    pi.on("session_before_switch", (event, ctx) => {
        return { received: event, cancelled: false };
    });
}
"#;
    let manager = load_js_extension(&harness, echo_ext);

    let response: Option<Value> = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::SessionBeforeSwitch,
                    Some(json!({
                        "reason": "new_session",
                        "targetSessionFile": "/sessions/new.jsonl",
                    })),
                    5000,
                )
                .await
                .expect("dispatch with response")
        }
    });

    let response = response.expect("should get a response");
    let received = response
        .get("received")
        .expect("should have received field");
    assert_eq!(
        received.get("reason").and_then(Value::as_str),
        Some("new_session")
    );
    assert_eq!(
        received.get("targetSessionFile").and_then(Value::as_str),
        Some("/sessions/new.jsonl")
    );
}

// ---------------------------------------------------------------------------
// Tests: no hooks registered
// ---------------------------------------------------------------------------

#[test]
fn no_cancellation_when_no_hooks_registered() {
    let harness = common::TestHarness::new("no_cancellation_when_no_hooks_registered");
    let manager = load_js_extension(&harness, NO_HOOKS_EXT);

    for event_name in [
        ExtensionEventName::SessionBeforeSwitch,
        ExtensionEventName::SessionBeforeFork,
        ExtensionEventName::SessionBeforeCompact,
    ] {
        let cancelled = common::run_async({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch_cancellable_event(event_name, None, 5000)
                    .await
                    .expect("dispatch cancellable")
            }
        });

        assert!(
            !cancelled,
            "Should not cancel when no hooks registered for {event_name:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests: ExtensionEvent serialization for session lifecycle types
// ---------------------------------------------------------------------------

#[test]
fn session_before_switch_event_serialization() {
    use pi::extension_events::ExtensionEvent;

    let event = ExtensionEvent::SessionBeforeSwitch {
        current_session: Some("session-1".to_string()),
        target_session: "session-2".to_string(),
    };

    assert_eq!(event.event_name(), "session_before_switch");

    let value = serde_json::to_value(&event).expect("serialize");
    assert_eq!(
        value.get("type").and_then(Value::as_str),
        Some("session_before_switch")
    );
    assert_eq!(
        value.get("current_session").and_then(Value::as_str),
        Some("session-1")
    );
    assert_eq!(
        value.get("target_session").and_then(Value::as_str),
        Some("session-2")
    );
}

#[test]
fn session_before_fork_event_serialization() {
    use pi::extension_events::ExtensionEvent;

    let event = ExtensionEvent::SessionBeforeFork {
        current_session: None,
        fork_entry_id: "entry-42".to_string(),
    };

    assert_eq!(event.event_name(), "session_before_fork");

    let value = serde_json::to_value(&event).expect("serialize");
    assert_eq!(
        value.get("type").and_then(Value::as_str),
        Some("session_before_fork")
    );
    assert!(value.get("current_session").unwrap().is_null());
    assert_eq!(
        value.get("fork_entry_id").and_then(Value::as_str),
        Some("entry-42")
    );
}
