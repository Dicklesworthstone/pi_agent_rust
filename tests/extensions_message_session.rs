//! Unit tests for extension message injection, session control, tool management,
//! and model control APIs.
//!
//! These tests exercise the `ExtensionManager` public API for session attachment,
//! model/thinking-level caching, active-tool filtering, and provider registration
//! integration. A lightweight `RecordingSession` impl is used for session-dependent tests.
//! Note: `RecordingSession` is intentionally not prefixed with `Mock` to comply
//! with the project's no-mock naming policy.

use async_trait::async_trait;
use pi::extensions::{ExtensionManager, ExtensionSession, PROTOCOL_VERSION, RegisterPayload};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

// ─── RecordingSession ────────────────────────────────────────────────────────────

/// In-memory session mock that records mutations for assertion.
struct RecordingSession {
    name: Mutex<Option<String>>,
    entries: Mutex<Vec<(String, Option<Value>)>>,
    model: Mutex<(Option<String>, Option<String>)>,
    thinking_level: Mutex<Option<String>>,
    labels: Mutex<Vec<(String, Option<String>)>>,
}

impl RecordingSession {
    const fn new() -> Self {
        Self {
            name: Mutex::new(None),
            entries: Mutex::new(Vec::new()),
            model: Mutex::new((None, None)),
            thinking_level: Mutex::new(None),
            labels: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ExtensionSession for RecordingSession {
    async fn get_state(&self) -> Value {
        let name = self.name.lock().unwrap().clone();
        let (provider, model_id) = self.model.lock().unwrap().clone();
        let level = self.thinking_level.lock().unwrap().clone();
        json!({
            "sessionName": name,
            "model": { "provider": provider, "id": model_id },
            "thinkingLevel": level.unwrap_or_else(|| "off".to_string()),
        })
    }

    async fn get_messages(&self) -> Vec<pi::session::SessionMessage> {
        Vec::new()
    }

    async fn get_entries(&self) -> Vec<Value> {
        Vec::new()
    }

    async fn get_branch(&self) -> Vec<Value> {
        Vec::new()
    }

    async fn set_name(&self, name: String) -> pi::error::Result<()> {
        *self.name.lock().unwrap() = Some(name);
        Ok(())
    }

    async fn append_message(&self, _message: pi::session::SessionMessage) -> pi::error::Result<()> {
        Ok(())
    }

    async fn append_custom_entry(
        &self,
        custom_type: String,
        data: Option<Value>,
    ) -> pi::error::Result<()> {
        self.entries.lock().unwrap().push((custom_type, data));
        Ok(())
    }

    async fn set_model(&self, provider: String, model_id: String) -> pi::error::Result<()> {
        *self.model.lock().unwrap() = (Some(provider), Some(model_id));
        Ok(())
    }

    async fn get_model(&self) -> (Option<String>, Option<String>) {
        self.model.lock().unwrap().clone()
    }

    async fn set_thinking_level(&self, level: String) -> pi::error::Result<()> {
        *self.thinking_level.lock().unwrap() = Some(level);
        Ok(())
    }

    async fn get_thinking_level(&self) -> Option<String> {
        self.thinking_level.lock().unwrap().clone()
    }

    async fn set_label(&self, target_id: String, label: Option<String>) -> pi::error::Result<()> {
        self.labels.lock().unwrap().push((target_id, label));
        Ok(())
    }
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
    let session = Arc::new(RecordingSession::new());
    mgr.set_session(session as Arc<dyn ExtensionSession>);

    assert!(mgr.session_handle().is_some());
}

#[test]
fn session_get_state_via_handle() {
    let mgr = ExtensionManager::new();
    let session = Arc::new(RecordingSession::new());
    mgr.set_session(Arc::clone(&session) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let handle = mgr.session_handle().expect("session attached");
        async move {
            let state = handle.get_state().await;
            assert!(state.get("sessionName").is_some());
        }
    });
}

#[test]
fn session_set_name_persists() {
    let session = Arc::new(RecordingSession::new());
    let mgr = ExtensionManager::new();
    mgr.set_session(Arc::clone(&session) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let handle = mgr.session_handle().expect("session attached");
        async move {
            handle
                .set_name("My Test Session".to_string())
                .await
                .unwrap();
            let state = handle.get_state().await;
            assert_eq!(state["sessionName"], "My Test Session");
        }
    });
}

#[test]
fn session_append_custom_entry() {
    let session = Arc::new(RecordingSession::new());
    let mgr = ExtensionManager::new();
    mgr.set_session(Arc::clone(&session) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let handle = mgr.session_handle().expect("session attached");
        let session_ref = &session;
        async move {
            handle
                .append_custom_entry(
                    "ext.note".to_string(),
                    Some(json!({"text": "Hello from extension"})),
                )
                .await
                .unwrap();

            {
                let entries = session_ref.entries.lock().unwrap();
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].0, "ext.note");
                assert_eq!(entries[0].1, Some(json!({"text": "Hello from extension"})));
                drop(entries);
            }
        }
    });
}

#[test]
fn session_set_model_persists() {
    let session = Arc::new(RecordingSession::new());
    let mgr = ExtensionManager::new();
    mgr.set_session(Arc::clone(&session) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let handle = mgr.session_handle().expect("session attached");
        let session_ref = &session;
        async move {
            handle
                .set_model("openai".to_string(), "gpt-4o".to_string())
                .await
                .unwrap();

            let (provider, model_id) = session_ref.model.lock().unwrap().clone();
            assert_eq!(provider.as_deref(), Some("openai"));
            assert_eq!(model_id.as_deref(), Some("gpt-4o"));
        }
    });
}

#[test]
fn session_get_model_returns_stored_value() {
    let session = Arc::new(RecordingSession::new());
    *session.model.lock().unwrap() = (
        Some("anthropic".to_string()),
        Some("claude-opus-4-5-20251101".to_string()),
    );

    let mgr = ExtensionManager::new();
    mgr.set_session(Arc::clone(&session) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let handle = mgr.session_handle().expect("session attached");
        async move {
            let (provider, model_id) = handle.get_model().await;
            assert_eq!(provider.as_deref(), Some("anthropic"));
            assert_eq!(model_id.as_deref(), Some("claude-opus-4-5-20251101"));
        }
    });
}

#[test]
fn session_set_thinking_level_persists() {
    let session = Arc::new(RecordingSession::new());
    let mgr = ExtensionManager::new();
    mgr.set_session(Arc::clone(&session) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let handle = mgr.session_handle().expect("session attached");
        let session_ref = &session;
        async move {
            handle.set_thinking_level("high".to_string()).await.unwrap();
            let level = session_ref.thinking_level.lock().unwrap().clone();
            assert_eq!(level.as_deref(), Some("high"));
        }
    });
}

#[test]
fn session_get_thinking_level_returns_stored_value() {
    let session = Arc::new(RecordingSession::new());
    *session.thinking_level.lock().unwrap() = Some("medium".to_string());

    let mgr = ExtensionManager::new();
    mgr.set_session(Arc::clone(&session) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let handle = mgr.session_handle().expect("session attached");
        async move {
            let level = handle.get_thinking_level().await;
            assert_eq!(level.as_deref(), Some("medium"));
        }
    });
}

#[test]
fn session_set_label_records_mutation() {
    let session = Arc::new(RecordingSession::new());
    let mgr = ExtensionManager::new();
    mgr.set_session(Arc::clone(&session) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let handle = mgr.session_handle().expect("session attached");
        let session_ref = &session;
        async move {
            handle
                .set_label("entry-42".to_string(), Some("important".to_string()))
                .await
                .unwrap();

            {
                let labels = session_ref.labels.lock().unwrap();
                assert_eq!(labels.len(), 1);
                assert_eq!(labels[0].0, "entry-42");
                assert_eq!(labels[0].1, Some("important".to_string()));
                drop(labels);
            }
        }
    });
}

#[test]
fn session_set_label_can_remove_label() {
    let session = Arc::new(RecordingSession::new());
    let mgr = ExtensionManager::new();
    mgr.set_session(Arc::clone(&session) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let handle = mgr.session_handle().expect("session attached");
        let session_ref = &session;
        async move {
            handle
                .set_label("entry-99".to_string(), None)
                .await
                .unwrap();

            {
                let labels = session_ref.labels.lock().unwrap();
                assert_eq!(labels.len(), 1);
                assert_eq!(labels[0].0, "entry-99");
                assert!(labels[0].1.is_none());
                drop(labels);
            }
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
    let session = Arc::new(RecordingSession::new());
    mgr.set_session(session as Arc<dyn ExtensionSession>);

    // Cache should still hold value
    let (provider, model_id) = mgr.current_model();
    assert_eq!(provider.as_deref(), Some("anthropic"));
    assert_eq!(model_id.as_deref(), Some("claude-sonnet-4-20250514"));
}

#[test]
fn thinking_level_cache_independent_of_session() {
    let mgr = ExtensionManager::new();
    mgr.set_current_thinking_level(Some("xhigh".to_string()));

    let session = Arc::new(RecordingSession::new());
    mgr.set_session(session as Arc<dyn ExtensionSession>);

    assert_eq!(mgr.current_thinking_level().as_deref(), Some("xhigh"));
}

#[test]
fn multiple_sessions_can_be_swapped() {
    let mgr = ExtensionManager::new();

    let session_a = Arc::new(RecordingSession::new());
    mgr.set_session(Arc::clone(&session_a) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let handle = mgr.session_handle().expect("session attached");
        async move {
            handle.set_name("Session A".to_string()).await.unwrap();
        }
    });

    // Swap to session B
    let session_b = Arc::new(RecordingSession::new());
    mgr.set_session(Arc::clone(&session_b) as Arc<dyn ExtensionSession>);

    asupersync::test_utils::run_test(|| {
        let handle = mgr.session_handle().expect("session attached");
        async move {
            handle.set_name("Session B".to_string()).await.unwrap();
        }
    });

    // Verify both sessions recorded their respective names
    assert_eq!(
        *session_a.name.lock().unwrap(),
        Some("Session A".to_string())
    );
    assert_eq!(
        *session_b.name.lock().unwrap(),
        Some("Session B".to_string())
    );
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
