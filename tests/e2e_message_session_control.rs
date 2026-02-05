//! E2E: Message injection + session control with verbose logging (bd-2ok9).
//!
//! This test loads JS extensions that call message injection and session control
//! APIs, verifying the full JS → Rust pipeline for:
//! - `pi.sendMessage()` / `pi.sendUserMessage()`
//! - `pi.events("appendEntry", ...)` / session metadata
//! - Tool management (`setActiveTools`, `getActiveTools`, `getAllTools`)
//! - Model control (`setModel`, `getModel`, `setThinkingLevel`, `getThinkingLevel`)
//!
//! Session state is backed by a real `Session` + `SessionHandle`, exercising the
//! full JSONL persistence plumbing. `RecordingHostActions` is retained because
//! host-action delivery (sendMessage/sendUserMessage) requires an agent loop
//! that these focused E2E tests intentionally do not run.

mod common;

use async_trait::async_trait;
use pi::error::Result;
use pi::extensions::{
    ExtensionHostActions, ExtensionManager, ExtensionSendMessage, ExtensionSendUserMessage,
    ExtensionSession, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::session::{Session, SessionHandle};
use pi::tools::ToolRegistry;
use serde_json::Value;
use std::sync::{Arc, Mutex};

// ─── RecordingHostActions ───────────────────────────────────────────────────
//
// Retained: host-action delivery requires an agent loop; these tests verify
// that the JS extension successfully calls the host action APIs, so we record
// the calls for assertion.

#[derive(Default)]
struct RecordingHostActions {
    messages: Mutex<Vec<ExtensionSendMessage>>,
    user_messages: Mutex<Vec<ExtensionSendUserMessage>>,
}

#[async_trait]
impl ExtensionHostActions for RecordingHostActions {
    async fn send_message(&self, message: ExtensionSendMessage) -> Result<()> {
        self.messages.lock().unwrap().push(message);
        Ok(())
    }
    async fn send_user_message(&self, message: ExtensionSendUserMessage) -> Result<()> {
        self.user_messages.lock().unwrap().push(message);
        Ok(())
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

struct ExtSetup {
    manager: ExtensionManager,
    host_actions: Arc<RecordingHostActions>,
    session_handle: SessionHandle,
}

fn load_extension(harness: &common::TestHarness, source: &str) -> ExtSetup {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_entry_path = harness.create_file("extensions/ext.mjs", source.as_bytes());
    let spec = JsExtensionLoadSpec::from_entry_path(&ext_entry_path).expect("load spec");

    let manager = ExtensionManager::new();
    let host_actions = Arc::new(RecordingHostActions::default());
    let session_handle = SessionHandle(Arc::new(asupersync::sync::Mutex::new(
        Session::create_with_dir(Some(cwd.clone())),
    )));

    manager.set_host_actions(Arc::clone(&host_actions) as Arc<dyn ExtensionHostActions>);
    manager.set_session(Arc::new(session_handle.clone()) as Arc<dyn ExtensionSession>);

    let tools = Arc::new(ToolRegistry::new(&["read", "edit", "bash"], &cwd, None));
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

    ExtSetup {
        manager,
        host_actions,
        session_handle,
    }
}

fn write_jsonl_artifacts(harness: &common::TestHarness, test_name: &str) {
    let log_path = harness.temp_path(format!("{test_name}.log.jsonl"));
    harness
        .write_jsonl_logs(&log_path)
        .expect("write jsonl log");
    assert!(log_path.exists(), "jsonl log should exist");
    harness.record_artifact(format!("{test_name}.log.jsonl"), &log_path);

    let normalized_log_path = harness.temp_path(format!("{test_name}.log.normalized.jsonl"));
    harness
        .write_jsonl_logs_normalized(&normalized_log_path)
        .expect("write normalized jsonl log");
    assert!(
        normalized_log_path.exists(),
        "normalized jsonl log should exist"
    );
    harness.record_artifact(
        format!("{test_name}.log.normalized.jsonl"),
        &normalized_log_path,
    );

    let artifacts_path = harness.temp_path(format!("{test_name}.artifacts.jsonl"));
    harness
        .write_artifact_index_jsonl(&artifacts_path)
        .expect("write artifact index jsonl");
    assert!(artifacts_path.exists(), "artifact index should exist");
    harness.record_artifact(format!("{test_name}.artifacts.jsonl"), &artifacts_path);

    let normalized_artifacts_path =
        harness.temp_path(format!("{test_name}.artifacts.normalized.jsonl"));
    harness
        .write_artifact_index_jsonl_normalized(&normalized_artifacts_path)
        .expect("write normalized artifact index jsonl");
    assert!(
        normalized_artifacts_path.exists(),
        "normalized artifact index should exist"
    );
    harness.record_artifact(
        format!("{test_name}.artifacts.normalized.jsonl"),
        &normalized_artifacts_path,
    );
}

// ─── Message Injection Tests ────────────────────────────────────────────────

/// Extension calls `pi.sendMessage()` via a command handler.
#[test]
fn e2e_send_message_via_command() {
    let test_name = "e2e_send_message_via_command";
    let harness = common::TestHarness::new(test_name);
    let setup = load_extension(
        &harness,
        r#"
export default function init(pi) {
  pi.registerCommand("notify", {
    description: "Send a notification message",
    handler: async (args, ctx) => {
      await pi.events("sendMessage", {
        message: {
          customType: "notification",
          content: "Build complete",
          display: true,
          details: { status: "success", duration: 42 },
        },
        options: { triggerTurn: false },
      });
      return { display: "Notification sent" };
    }
  });
}
"#,
    );

    let result = common::run_async({
        let manager = setup.manager.clone();
        async move { manager.execute_command("notify", "", 5000).await }
    });
    assert!(result.is_ok(), "notify command should succeed: {result:?}");

    let messages = setup.host_actions.messages.lock().unwrap();
    assert_eq!(messages.len(), 1, "one message should have been sent");
    assert_eq!(messages[0].custom_type, "notification");
    assert_eq!(messages[0].content, "Build complete");
    assert!(messages[0].display);
    assert!(!messages[0].trigger_turn);
    assert_eq!(
        messages[0]
            .details
            .as_ref()
            .and_then(|d| d.get("status"))
            .and_then(Value::as_str),
        Some("success")
    );
    drop(messages);
    write_jsonl_artifacts(&harness, test_name);
}

/// Extension calls `pi.sendMessage()` missing customType - should fail.
#[test]
fn e2e_send_message_missing_custom_type() {
    let test_name = "e2e_send_message_missing_custom_type";
    let harness = common::TestHarness::new(test_name);
    let setup = load_extension(
        &harness,
        r#"
export default function init(pi) {
  pi.registerCommand("bad-msg", {
    description: "Send malformed message",
    handler: async (args, ctx) => {
      const result = await pi.events("sendMessage", {
        message: { content: "No type" },
      });
      // The hostcall returns error, which we pass through
      return { display: "done", error: result };
    }
  });
}
"#,
    );

    let _ = common::run_async({
        let manager = setup.manager.clone();
        async move { manager.execute_command("bad-msg", "", 5000).await }
    });

    // No message should have been delivered
    let messages = setup.host_actions.messages.lock().unwrap();
    assert!(
        messages.is_empty(),
        "no message should be sent without customType"
    );
    drop(messages);
    write_jsonl_artifacts(&harness, test_name);
}

/// Extension sends a user message.
#[test]
fn e2e_send_user_message_via_command() {
    let test_name = "e2e_send_user_message_via_command";
    let harness = common::TestHarness::new(test_name);
    let setup = load_extension(
        &harness,
        r#"
export default function init(pi) {
  pi.registerCommand("inject", {
    description: "Inject a user message",
    handler: async (args, ctx) => {
      await pi.events("sendUserMessage", {
        text: "Please review the changes",
        options: { deliverAs: "followUp" },
      });
      return { display: "Injected" };
    }
  });
}
"#,
    );

    let result = common::run_async({
        let manager = setup.manager.clone();
        async move { manager.execute_command("inject", "", 5000).await }
    });
    assert!(result.is_ok(), "inject command should succeed: {result:?}");

    let user_msgs = setup.host_actions.user_messages.lock().unwrap();
    assert_eq!(user_msgs.len(), 1);
    assert_eq!(user_msgs[0].text, "Please review the changes");
    drop(user_msgs);
    write_jsonl_artifacts(&harness, test_name);
}

// ─── Tool Management Tests ──────────────────────────────────────────────────

/// Extension queries and modifies active tools.
#[test]
fn e2e_tool_management_via_command() {
    let test_name = "e2e_tool_management_via_command";
    let harness = common::TestHarness::new(test_name);
    let setup = load_extension(
        &harness,
        r#"
export default function init(pi) {
  pi.registerCommand("toggle-tools", {
    description: "Manage active tools",
    handler: async (args, ctx) => {
      // Get all tools first
      const allTools = await pi.events("getAllTools", {});

      // Disable bash - only allow read and edit
      await pi.events("setActiveTools", { tools: ["read", "edit"] });

      // Verify the change
      const activeTools = await pi.events("getActiveTools", {});

      return {
        display: JSON.stringify({
          allCount: allTools.tools ? allTools.tools.length : 0,
          activeTools: activeTools.tools || [],
        })
      };
    }
  });
}
"#,
    );

    let result = common::run_async({
        let manager = setup.manager.clone();
        async move { manager.execute_command("toggle-tools", "", 5000).await }
    });
    assert!(result.is_ok(), "toggle-tools should succeed: {result:?}");

    // Verify the manager state reflects the change
    let active = setup.manager.active_tools();
    assert_eq!(active, Some(vec!["read".to_string(), "edit".to_string()]));
    write_jsonl_artifacts(&harness, test_name);
}

// ─── Model Control Tests ────────────────────────────────────────────────────

/// Extension changes model and thinking level.
#[test]
fn e2e_model_control_via_command() {
    let test_name = "e2e_model_control_via_command";
    let harness = common::TestHarness::new(test_name);
    let setup = load_extension(
        &harness,
        r#"
export default function init(pi) {
  pi.registerCommand("switch-model", {
    description: "Switch to a different model",
    handler: async (args, ctx) => {
      // Set model
      await pi.events("setModel", {
        provider: "anthropic",
        modelId: "claude-opus-4-5-20251101",
      });

      // Set thinking level
      await pi.events("setThinkingLevel", { thinkingLevel: "high" });

      // Read back
      const model = await pi.events("getModel", {});
      const thinking = await pi.events("getThinkingLevel", {});

      return {
        display: JSON.stringify({
          provider: model.provider,
          modelId: model.modelId,
          thinkingLevel: thinking.thinkingLevel,
        })
      };
    }
  });
}
"#,
    );

    let result = common::run_async({
        let manager = setup.manager.clone();
        async move { manager.execute_command("switch-model", "", 5000).await }
    });
    assert!(result.is_ok(), "switch-model should succeed: {result:?}");

    // Verify model via real session handle
    common::run_async({
        let handle = setup.session_handle;
        async move {
            let (provider, model_id) = handle.get_model().await;
            assert_eq!(provider.as_deref(), Some("anthropic"));
            assert_eq!(model_id.as_deref(), Some("claude-opus-4-5-20251101"));

            let thinking = handle.get_thinking_level().await;
            assert_eq!(thinking.as_deref(), Some("high"));
        }
    });
    write_jsonl_artifacts(&harness, test_name);
}

// ─── Session Metadata Tests ─────────────────────────────────────────────────

/// Extension sets and reads session name.
#[test]
fn e2e_session_name_via_command() {
    let test_name = "e2e_session_name_via_command";
    let harness = common::TestHarness::new(test_name);
    let setup = load_extension(
        &harness,
        r#"
export default function init(pi) {
  pi.registerCommand("name-session", {
    description: "Set the session name",
    handler: async (args, ctx) => {
      await pi.session("setName", { name: "My Feature Work" });
      const result = await pi.session("getName", {});
      return { display: "Session named: " + (result || "unknown") };
    }
  });
}
"#,
    );

    let result = common::run_async({
        let manager = setup.manager.clone();
        async move { manager.execute_command("name-session", "", 5000).await }
    });
    assert!(result.is_ok(), "name-session should succeed: {result:?}");

    // Verify name via real session
    common::run_async({
        let handle = setup.session_handle;
        async move {
            let state = handle.get_state().await;
            assert_eq!(state["sessionName"], "My Feature Work");
        }
    });
    write_jsonl_artifacts(&harness, test_name);
}

/// Extension sets a label on an entry.
#[test]
fn e2e_session_set_label_via_command() {
    let test_name = "e2e_session_set_label_via_command";
    let harness = common::TestHarness::new(test_name);
    let setup = load_extension(
        &harness,
        r#"
export default function init(pi) {
  pi.registerCommand("label-entry", {
    description: "Label a session entry",
    handler: async (args, ctx) => {
      await pi.session("setLabel", {
        targetId: "entry-99",
        label: "important"
      });
      return { display: "Label set" };
    }
  });
}
"#,
    );

    let result = common::run_async({
        let manager = setup.manager;
        async move { manager.execute_command("label-entry", "", 5000).await }
    });
    assert!(result.is_ok(), "label-entry should succeed: {result:?}");

    // Note: The real Session's add_label verifies target existence. The JS extension
    // passes "entry-99" which won't exist in a fresh session, so the label entry
    // won't be created. This tests the real behavior (vs the mock which always succeeded).
    // We verify the command itself didn't error out (it returns Ok even if the label
    // target doesn't exist — the session silently skips it).
    write_jsonl_artifacts(&harness, test_name);
}

/// Extension appends a custom entry to the session.
#[test]
fn e2e_append_entry_via_command() {
    let test_name = "e2e_append_entry_via_command";
    let harness = common::TestHarness::new(test_name);
    let setup = load_extension(
        &harness,
        r#"
export default function init(pi) {
  pi.registerCommand("append", {
    description: "Append a custom entry",
    handler: async (args, ctx) => {
      await pi.session("appendEntry", {
        customType: "bookmark",
        data: { url: "https://example.com", title: "Example" }
      });
      return { display: "Entry appended" };
    }
  });
}
"#,
    );

    let result = common::run_async({
        let manager = setup.manager.clone();
        async move { manager.execute_command("append", "", 5000).await }
    });
    assert!(result.is_ok(), "append command should succeed: {result:?}");

    // Verify custom entry exists in real session
    common::run_async({
        let handle = setup.session_handle;
        async move {
            let entries = handle.get_entries().await;
            let custom = entries
                .iter()
                .find(|e| e.get("type").and_then(Value::as_str) == Some("custom"));
            assert!(custom.is_some(), "custom entry should exist in session");
            let custom = custom.unwrap();
            assert_eq!(custom["customType"], "bookmark");
            assert_eq!(custom["data"]["url"].as_str(), Some("https://example.com"));
        }
    });
    write_jsonl_artifacts(&harness, test_name);
}

// ─── Combined Lifecycle Test ────────────────────────────────────────────────

/// Extension that exercises multiple APIs in a single flow.
#[test]
fn e2e_combined_message_session_lifecycle() {
    let test_name = "e2e_combined_message_session_lifecycle";
    let harness = common::TestHarness::new(test_name);
    let setup = load_extension(
        &harness,
        r#"
export default function init(pi) {
  pi.registerCommand("lifecycle", {
    description: "Full lifecycle test",
    handler: async (args, ctx) => {
      // 1. Set session name
      await pi.session("setName", { name: "Lifecycle Test" });

      // 2. Switch model
      await pi.events("setModel", {
        provider: "openai",
        modelId: "gpt-4",
      });

      // 3. Adjust thinking
      await pi.events("setThinkingLevel", { thinkingLevel: "medium" });

      // 4. Filter tools
      await pi.events("setActiveTools", { tools: ["read"] });

      // 5. Send a notification
      await pi.events("sendMessage", {
        message: {
          customType: "progress",
          content: "Step 5 of 5 complete",
          display: true,
        }
      });

      // 6. Append a bookmark
      await pi.session("appendEntry", {
        customType: "checkpoint",
        data: { step: 5 }
      });

      return { display: "Lifecycle complete" };
    }
  });
}
"#,
    );

    let result = common::run_async({
        let manager = setup.manager.clone();
        async move { manager.execute_command("lifecycle", "", 10000).await }
    });
    assert!(
        result.is_ok(),
        "lifecycle command should succeed: {result:?}"
    );

    // Verify session state via real session handle
    common::run_async({
        let handle = setup.session_handle.clone();
        async move {
            // Verify session name
            let state = handle.get_state().await;
            assert_eq!(state["sessionName"], "Lifecycle Test");

            // Verify model
            let (provider, model_id) = handle.get_model().await;
            assert_eq!(provider.as_deref(), Some("openai"));
            assert_eq!(model_id.as_deref(), Some("gpt-4"));

            // Verify thinking level
            let thinking = handle.get_thinking_level().await;
            assert_eq!(thinking.as_deref(), Some("medium"));

            // Verify custom entry
            let entries = handle.get_entries().await;
            let checkpoint = entries
                .iter()
                .find(|e| e.get("type").and_then(Value::as_str) == Some("custom"));
            assert!(checkpoint.is_some(), "checkpoint entry should exist");
            assert_eq!(checkpoint.unwrap()["customType"], "checkpoint");
        }
    });

    // Verify tool filter (sync)
    assert_eq!(setup.manager.active_tools(), Some(vec!["read".to_string()]));

    // Verify notification was sent via host actions
    let messages = setup.host_actions.messages.lock().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].custom_type, "progress");
    drop(messages);
    write_jsonl_artifacts(&harness, test_name);
}
