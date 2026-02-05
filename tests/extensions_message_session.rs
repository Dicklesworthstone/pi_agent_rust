//! Unit tests for extension message injection, session control, tool management,
//! and model control APIs.
//!
//! These tests exercise the `ExtensionManager` public API for session attachment,
//! model/thinking-level caching, active-tool filtering, and provider registration
//! integration. Session-dependent tests use real `SessionHandle` backed by an
//! in-memory `Session`, exercising the full session persistence plumbing.

use pi::extensions::{ExtensionManager, ExtensionSession, PROTOCOL_VERSION, RegisterPayload};
use pi::session::{Session, SessionHandle};
use serde_json::{Value, json};
use std::sync::Arc;

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Create a real `SessionHandle` backed by an in-memory `Session`.
fn create_test_session() -> SessionHandle {
    SessionHandle(Arc::new(asupersync::sync::Mutex::new(Session::create())))
}

fn empty_payload(name: &str) -> RegisterPayload {
    RegisterPayload {
        name: name.to_string(),
        version: "1.0.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: Vec::new(),
        shortcuts: Vec::new(),
        flags: Vec::new(),
        event_hooks: Vec::new(),
    }
}

// ─── Model Control Tests ────────────────────────────────────────────────────

#[test]
fn model_defaults_to_none() {
    let mgr = ExtensionManager::new();
    let (provider, model_id) = mgr.current_model();
    assert!(provider.is_none());
    assert!(model_id.is_none());
}

#[test]
fn set_model_updates_cache() {
    let mgr = ExtensionManager::new();
    mgr.set_current_model(
        Some("anthropic".to_string()),
        Some("claude-sonnet-4-20250514".to_string()),
    );

    let (provider, model_id) = mgr.current_model();
    assert_eq!(provider.as_deref(), Some("anthropic"));
    assert_eq!(model_id.as_deref(), Some("claude-sonnet-4-20250514"));
}

#[test]
fn set_model_can_clear() {
    let mgr = ExtensionManager::new();
    mgr.set_current_model(Some("openai".to_string()), Some("gpt-4o".to_string()));
    mgr.set_current_model(None, None);

    let (provider, model_id) = mgr.current_model();
    assert!(provider.is_none());
    assert!(model_id.is_none());
}

#[test]
fn set_model_partial_update() {
    let mgr = ExtensionManager::new();
    mgr.set_current_model(Some("anthropic".to_string()), None);

    let (provider, model_id) = mgr.current_model();
    assert_eq!(provider.as_deref(), Some("anthropic"));
    assert!(model_id.is_none());
}

// ─── Thinking Level Tests ───────────────────────────────────────────────────

#[test]
fn thinking_level_defaults_to_none() {
    let mgr = ExtensionManager::new();
    assert!(mgr.current_thinking_level().is_none());
}

#[test]
fn set_thinking_level_updates_cache() {
    let mgr = ExtensionManager::new();
    mgr.set_current_thinking_level(Some("high".to_string()));
    assert_eq!(mgr.current_thinking_level().as_deref(), Some("high"));
}

#[test]
fn set_thinking_level_can_clear() {
    let mgr = ExtensionManager::new();
    mgr.set_current_thinking_level(Some("medium".to_string()));
    mgr.set_current_thinking_level(None);
    assert!(mgr.current_thinking_level().is_none());
}

// ─── Active Tool Management Tests ───────────────────────────────────────────

#[test]
fn active_tools_defaults_to_none() {
    let mgr = ExtensionManager::new();
    assert!(mgr.active_tools().is_none());
}

#[test]
fn set_active_tools_stores_filter() {
    let mgr = ExtensionManager::new();
    mgr.set_active_tools(vec!["read".to_string(), "bash".to_string()]);

    let tools = mgr.active_tools().expect("should have active tools");
    assert_eq!(tools, vec!["read", "bash"]);
}

#[test]
fn set_active_tools_replaces_previous() {
    let mgr = ExtensionManager::new();
    mgr.set_active_tools(vec!["read".to_string()]);
    mgr.set_active_tools(vec!["bash".to_string(), "edit".to_string()]);

    let tools = mgr.active_tools().expect("should have active tools");
    assert_eq!(tools, vec!["bash", "edit"]);
}

#[test]
fn extension_tool_defs_collected_from_payload() {
    let mgr = ExtensionManager::new();
    let mut payload = empty_payload("ext");
    payload.tools = vec![
        json!({"name": "custom_read", "description": "Read custom data"}),
        json!({"name": "custom_write", "description": "Write custom data"}),
    ];
    mgr.register(payload);

    let defs = mgr.extension_tool_defs();
    assert_eq!(defs.len(), 2);
    assert_eq!(defs[0]["name"], "custom_read");
    assert_eq!(defs[1]["name"], "custom_write");
}

#[test]
fn extension_tool_defs_empty_when_no_extensions() {
    let mgr = ExtensionManager::new();
    assert!(mgr.extension_tool_defs().is_empty());
}

// ─── Session Attachment Tests ───────────────────────────────────────────────

#[test]
fn session_handle_defaults_to_none() {
    let mgr = ExtensionManager::new();
    assert!(mgr.session_handle().is_none());
}

#[test]
fn set_session_attaches_handle() {
    let mgr = ExtensionManager::new();
    let handle = create_test_session();
    mgr.set_session(Arc::new(handle) as Arc<dyn ExtensionSession>);

    assert!(mgr.session_handle().is_some());
}

#[test]
fn session_get_state_via_handle() {
    let mgr = ExtensionManager::new();
    let handle = create_test_session();
    mgr.set_session(Arc::new(handle) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let session = mgr.session_handle().expect("session attached");
        async move {
            let state = session.get_state().await;
            assert!(state.get("sessionName").is_some());
        }
    });
}

#[test]
fn session_set_name_persists() {
    let mgr = ExtensionManager::new();
    let handle = create_test_session();
    mgr.set_session(Arc::new(handle) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let session = mgr.session_handle().expect("session attached");
        async move {
            session
                .set_name("My Test Session".to_string())
                .await
                .unwrap();
            let state = session.get_state().await;
            assert_eq!(state["sessionName"], "My Test Session");
        }
    });
}

#[test]
fn session_append_custom_entry() {
    let mgr = ExtensionManager::new();
    let handle = create_test_session();
    mgr.set_session(Arc::new(handle) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let session = mgr.session_handle().expect("session attached");
        async move {
            session
                .append_custom_entry(
                    "ext.note".to_string(),
                    Some(json!({"text": "Hello from extension"})),
                )
                .await
                .unwrap();

            let entries = session.get_entries().await;
            let custom = entries
                .iter()
                .find(|e| e.get("type").and_then(Value::as_str) == Some("custom"));
            assert!(custom.is_some(), "custom entry should exist in session");
            let custom = custom.unwrap();
            assert_eq!(custom["customType"], "ext.note");
            assert_eq!(custom["data"]["text"], "Hello from extension");
        }
    });
}

#[test]
fn session_set_model_persists() {
    let mgr = ExtensionManager::new();
    let handle = create_test_session();
    mgr.set_session(Arc::new(handle) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let session = mgr.session_handle().expect("session attached");
        async move {
            session
                .set_model("openai".to_string(), "gpt-4o".to_string())
                .await
                .unwrap();

            let (provider, model_id) = session.get_model().await;
            assert_eq!(provider.as_deref(), Some("openai"));
            assert_eq!(model_id.as_deref(), Some("gpt-4o"));
        }
    });
}

#[test]
fn session_get_model_returns_stored_value() {
    let mgr = ExtensionManager::new();
    let handle = create_test_session();
    mgr.set_session(Arc::new(handle) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let session = mgr.session_handle().expect("session attached");
        async move {
            // Set model via the real session path, then verify read-back
            session
                .set_model(
                    "anthropic".to_string(),
                    "claude-opus-4-5-20251101".to_string(),
                )
                .await
                .unwrap();

            let (provider, model_id) = session.get_model().await;
            assert_eq!(provider.as_deref(), Some("anthropic"));
            assert_eq!(model_id.as_deref(), Some("claude-opus-4-5-20251101"));
        }
    });
}

#[test]
fn session_set_thinking_level_persists() {
    let mgr = ExtensionManager::new();
    let handle = create_test_session();
    mgr.set_session(Arc::new(handle) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let session = mgr.session_handle().expect("session attached");
        async move {
            session
                .set_thinking_level("high".to_string())
                .await
                .unwrap();
            let level = session.get_thinking_level().await;
            assert_eq!(level.as_deref(), Some("high"));
        }
    });
}

#[test]
fn session_get_thinking_level_returns_stored_value() {
    let mgr = ExtensionManager::new();
    let handle = create_test_session();
    mgr.set_session(Arc::new(handle) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let session = mgr.session_handle().expect("session attached");
        async move {
            // Set thinking level via the real session path, then verify read-back
            session
                .set_thinking_level("medium".to_string())
                .await
                .unwrap();
            let level = session.get_thinking_level().await;
            assert_eq!(level.as_deref(), Some("medium"));
        }
    });
}

#[test]
fn session_set_label_records_mutation() {
    let mgr = ExtensionManager::new();
    let handle = create_test_session();
    mgr.set_session(Arc::new(handle) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let session = mgr.session_handle().expect("session attached");
        async move {
            // First create a custom entry so we have a valid target ID for the label
            session
                .append_custom_entry("note".to_string(), Some(json!({"text": "target"})))
                .await
                .unwrap();

            // Find the custom entry's ID
            let entries = session.get_entries().await;
            let target_id = entries
                .iter()
                .find(|e| e.get("type").and_then(Value::as_str) == Some("custom"))
                .and_then(|e| e.get("id").and_then(Value::as_str))
                .expect("custom entry should have an id")
                .to_string();

            // Set label on that entry
            session
                .set_label(target_id.clone(), Some("important".to_string()))
                .await
                .unwrap();

            // Verify via entries
            let entries = session.get_entries().await;
            let label_entry = entries
                .iter()
                .find(|e| e.get("type").and_then(Value::as_str) == Some("label"));
            assert!(label_entry.is_some(), "label entry should exist");
            let label_entry = label_entry.unwrap();
            assert_eq!(label_entry["targetId"], target_id);
            assert_eq!(label_entry["label"], "important");
        }
    });
}

#[test]
fn session_set_label_can_remove_label() {
    let mgr = ExtensionManager::new();
    let handle = create_test_session();
    mgr.set_session(Arc::new(handle) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let session = mgr.session_handle().expect("session attached");
        async move {
            // Create a target entry
            session
                .append_custom_entry("note".to_string(), Some(json!({"text": "target"})))
                .await
                .unwrap();

            let entries = session.get_entries().await;
            let target_id = entries
                .iter()
                .find(|e| e.get("type").and_then(Value::as_str) == Some("custom"))
                .and_then(|e| e.get("id").and_then(Value::as_str))
                .expect("custom entry should have an id")
                .to_string();

            // Set label = None (remove)
            session.set_label(target_id.clone(), None).await.unwrap();

            let entries = session.get_entries().await;
            let label_entry = entries
                .iter()
                .find(|e| e.get("type").and_then(Value::as_str) == Some("label"));
            assert!(label_entry.is_some(), "label entry should exist");
            let label_entry = label_entry.unwrap();
            assert_eq!(label_entry["targetId"], target_id);
            assert!(label_entry.get("label").is_none() || label_entry["label"].is_null());
        }
    });
}

// ─── Cross-cutting Integration Tests ────────────────────────────────────────

#[test]
fn model_cache_independent_of_session() {
    let mgr = ExtensionManager::new();

    // Set model in cache (no session)
    mgr.set_current_model(
        Some("anthropic".to_string()),
        Some("claude-sonnet-4-20250514".to_string()),
    );

    // Attach session afterward
    let handle = create_test_session();
    mgr.set_session(Arc::new(handle) as Arc<dyn ExtensionSession>);

    // Cache should still hold value
    let (provider, model_id) = mgr.current_model();
    assert_eq!(provider.as_deref(), Some("anthropic"));
    assert_eq!(model_id.as_deref(), Some("claude-sonnet-4-20250514"));
}

#[test]
fn thinking_level_cache_independent_of_session() {
    let mgr = ExtensionManager::new();
    mgr.set_current_thinking_level(Some("xhigh".to_string()));

    let handle = create_test_session();
    mgr.set_session(Arc::new(handle) as Arc<dyn ExtensionSession>);

    assert_eq!(mgr.current_thinking_level().as_deref(), Some("xhigh"));
}

#[test]
fn multiple_sessions_can_be_swapped() {
    let mgr = ExtensionManager::new();

    let handle_a = create_test_session();
    let verify_a = handle_a.clone();
    mgr.set_session(Arc::new(handle_a) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let session = mgr.session_handle().expect("session attached");
        async move {
            session.set_name("Session A".to_string()).await.unwrap();
        }
    });

    // Swap to session B
    let handle_b = create_test_session();
    let verify_b = handle_b.clone();
    mgr.set_session(Arc::new(handle_b) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let session = mgr.session_handle().expect("session attached");
        async move {
            session.set_name("Session B".to_string()).await.unwrap();
        }
    });

    // Verify both sessions recorded their respective names via the real Session
    asupersync::test_utils::run_test(|| async move {
        let state_a = verify_a.get_state().await;
        assert_eq!(state_a["sessionName"], "Session A");

        let state_b = verify_b.get_state().await;
        assert_eq!(state_b["sessionName"], "Session B");
    });
}

#[test]
fn tool_defs_from_multiple_extensions() {
    let mgr = ExtensionManager::new();

    let mut ext_a = empty_payload("ext-a");
    ext_a.tools = vec![json!({"name": "tool_a", "description": "Tool A"})];
    mgr.register(ext_a);

    let mut ext_b = empty_payload("ext-b");
    ext_b.tools = vec![json!({"name": "tool_b", "description": "Tool B"})];
    mgr.register(ext_b);

    let defs = mgr.extension_tool_defs();
    assert_eq!(defs.len(), 2);

    let names: Vec<&str> = defs
        .iter()
        .filter_map(|d| d.get("name").and_then(Value::as_str))
        .collect();
    assert!(names.contains(&"tool_a"));
    assert!(names.contains(&"tool_b"));
}

#[test]
fn active_tools_filter_does_not_affect_extension_tool_defs() {
    let mgr = ExtensionManager::new();

    let mut payload = empty_payload("ext");
    payload.tools = vec![json!({"name": "my_tool", "description": "My tool"})];
    mgr.register(payload);

    // Set active tools filter to something that doesn't include extension tool
    mgr.set_active_tools(vec!["read".to_string()]);

    // Extension tool defs should still return all extension tools regardless
    let defs = mgr.extension_tool_defs();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0]["name"], "my_tool");
}
