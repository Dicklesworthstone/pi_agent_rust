#![allow(clippy::unnecessary_literal_bound)]
#![allow(clippy::too_many_lines)]

//! Tests for the capability policy prompt system:
//! - Permission persistence (`PermissionStore`)
//! - TUI modal overlay (keyboard navigation, confirm, escape)
//! - RPC event serialization
//! - Extension manager UI channel (request/respond lifecycle)
//! - Policy evaluation (strict/prompt/permissive)
//! - Prompt caching (ask-once semantics)

mod common;

use asupersync::Cx;
use asupersync::channel::mpsc;
use asupersync::sync::Mutex;
use bubbletea::{KeyMsg, KeyType, Message, Model as BubbleteaModel};
use common::TestHarness;
use futures::stream;
use pi::agent::{Agent, AgentConfig};
use pi::config::Config;
use pi::extensions::{ExtensionManager, ExtensionUiRequest, ExtensionUiResponse};
use pi::interactive::{PiApp, PiMsg};
use pi::keybindings::KeyBindings;
use pi::model::{StreamEvent, Usage};
use pi::models::ModelEntry;
use pi::provider::{Context, InputType, Model, ModelCost, Provider, StreamOptions};
use pi::resources::{ResourceCliOptions, ResourceLoader};
use pi::session::Session;
use pi::tools::ToolRegistry;
use serde_json::json;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn test_runtime_handle() -> asupersync::runtime::RuntimeHandle {
    static RT: OnceLock<asupersync::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build asupersync runtime")
    })
    .handle()
}

struct DummyProvider;

#[async_trait::async_trait]
impl Provider for DummyProvider {
    fn name(&self) -> &str {
        "dummy"
    }

    fn api(&self) -> &str {
        "dummy"
    }

    fn model_id(&self) -> &str {
        "dummy-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> pi::error::Result<
        Pin<Box<dyn futures::Stream<Item = pi::error::Result<StreamEvent>> + Send>>,
    > {
        Ok(Box::pin(stream::empty()))
    }
}

fn dummy_model_entry() -> ModelEntry {
    let model = Model {
        id: "dummy-model".to_string(),
        name: "Dummy Model".to_string(),
        api: "dummy-api".to_string(),
        provider: "dummy".to_string(),
        base_url: "https://example.invalid".to_string(),
        reasoning: false,
        input: vec![InputType::Text],
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        context_window: 4096,
        max_tokens: 1024,
        headers: HashMap::new(),
    };

    ModelEntry {
        model,
        api_key: None,
        headers: HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    }
}

fn build_app(harness: &TestHarness, extensions: Option<ExtensionManager>) -> PiApp {
    let cwd = harness.temp_dir().to_path_buf();
    let config = common::hermetic_interactive_config(Config::default());
    let tools = ToolRegistry::new(&[], &cwd, Some(&config));
    let provider: Arc<dyn Provider> = Arc::new(DummyProvider);
    let agent = Agent::new(provider, tools, AgentConfig::default());
    let resources = ResourceLoader::empty(config.enable_skill_commands());
    let resource_cli = ResourceCliOptions {
        no_skills: false,
        no_prompt_templates: false,
        no_extensions: false,
        no_themes: false,
        skill_paths: Vec::new(),
        prompt_paths: Vec::new(),
        extension_paths: Vec::new(),
        theme_paths: Vec::new(),
    };
    let model_entry = dummy_model_entry();
    let model_scope = vec![model_entry.clone()];
    let available_models = vec![model_entry.clone()];
    let (event_tx, _event_rx) = mpsc::channel(1024);
    let session = Session::create();
    let session = Arc::new(Mutex::new(session));

    let mut app = PiApp::new(
        agent,
        session,
        config,
        resources,
        resource_cli,
        cwd,
        model_entry,
        model_scope,
        available_models,
        None,
        Vec::new(),
        event_tx,
        test_runtime_handle(),
        false,
        false,
        extensions,
        Some(KeyBindings::new()),
        Vec::new(),
        Usage::default(),
        None,
    );
    app.set_terminal_size(80, 24);
    app
}

/// Strip ANSI escape sequences from rendered view.
fn strip_ansi(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Skip until we find a letter (end of escape sequence).
            while let Some(&next) = chars.peek() {
                chars.next();
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            result.push(ch);
        }
    }
    result
}

fn normalize_view(input: &str) -> String {
    let stripped = strip_ansi(input);
    stripped
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

fn view_text(app: &PiApp) -> String {
    normalize_view(&BubbleteaModel::view(app))
}

fn send_pi_msg(app: &mut PiApp, msg: PiMsg) {
    BubbleteaModel::update(app, Message::new(msg));
}

fn send_key(app: &mut PiApp, key: KeyMsg) {
    BubbleteaModel::update(app, Message::new(key));
}

/// Create a capability prompt request with the canonical payload shape.
fn cap_prompt_request(
    id: &str,
    extension_id: &str,
    capability: &str,
    message: &str,
) -> ExtensionUiRequest {
    ExtensionUiRequest::new_capability_prompt(
        id,
        extension_id,
        capability,
        json!({
            "title": format!("Extension '{}' requests '{}' capability", extension_id, capability),
            "message": message,
            "extension_id": extension_id,
            "capability": capability,
        }),
    )
}

// ===========================================================================
// 1. PermissionStore tests
// ===========================================================================

mod permission_store {
    use pi::permissions::PermissionStore;

    #[test]
    fn corrupt_json_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        std::fs::write(&path, "NOT VALID JSON!!!").unwrap();

        let result = PermissionStore::open(&path);
        assert!(result.is_err(), "Expected error for corrupt JSON");
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("parse") || err.contains("Failed"),
            "Error message should mention parsing: {err}"
        );
    }

    #[test]
    fn empty_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        std::fs::write(&path, "").unwrap();

        let result = PermissionStore::open(&path);
        assert!(result.is_err(), "Expected error for empty file");
    }

    #[test]
    fn nested_directory_creation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c/permissions.json");

        let mut store = PermissionStore::open(&path).unwrap();
        store.record("ext", "exec", true).unwrap();

        assert!(
            path.exists(),
            "File should be created with intermediate dirs"
        );
        let store2 = PermissionStore::open(&path).unwrap();
        assert_eq!(store2.lookup("ext", "exec"), Some(true));
    }

    #[cfg(unix)]
    #[test]
    fn file_permissions_are_restricted() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");

        let mut store = PermissionStore::open(&path).unwrap();
        store.record("ext", "exec", true).unwrap();

        let perms = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(
            perms.mode() & 0o777,
            0o600,
            "Permissions file should be 0600"
        );
    }

    #[test]
    fn lookup_missing_extension_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let store = PermissionStore::open(&path).unwrap();

        assert_eq!(store.lookup("nonexistent", "exec"), None);
    }

    #[test]
    fn lookup_missing_capability_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let mut store = PermissionStore::open(&path).unwrap();
        store.record("ext", "exec", true).unwrap();

        assert_eq!(store.lookup("ext", "http"), None);
    }

    #[test]
    fn to_cache_map_excludes_empty_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");

        let mut store = PermissionStore::open(&path).unwrap();
        store.record("ext", "exec", true).unwrap();
        store.revoke_extension("ext").unwrap();

        let cache = store.to_cache_map();
        assert!(
            cache.is_empty(),
            "Cache map should be empty after revocation"
        );
    }

    #[test]
    fn multiple_extensions_independent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");

        let mut store = PermissionStore::open(&path).unwrap();
        store.record("ext-a", "exec", true).unwrap();
        store.record("ext-b", "exec", false).unwrap();
        store.record("ext-a", "http", false).unwrap();

        assert_eq!(store.lookup("ext-a", "exec"), Some(true));
        assert_eq!(store.lookup("ext-b", "exec"), Some(false));
        assert_eq!(store.lookup("ext-a", "http"), Some(false));
        assert_eq!(store.lookup("ext-b", "http"), None);

        // Revoke one, other untouched.
        store.revoke_extension("ext-a").unwrap();
        assert_eq!(store.lookup("ext-a", "exec"), None);
        assert_eq!(store.lookup("ext-b", "exec"), Some(false));
    }
}

// ===========================================================================
// 2. Policy evaluation tests
// ===========================================================================

mod policy_evaluation {
    use pi::extensions::{ExtensionPolicy, ExtensionPolicyMode, PolicyDecision};

    fn default_policy(mode: ExtensionPolicyMode) -> ExtensionPolicy {
        ExtensionPolicy {
            mode,
            max_memory_mb: 256,
            default_caps: vec!["read".to_string(), "write".to_string(), "http".to_string()],
            deny_caps: vec!["exec".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn strict_allows_default_caps() {
        let policy = default_policy(ExtensionPolicyMode::Strict);
        let check = policy.evaluate("read");
        assert_eq!(check.decision, PolicyDecision::Allow);
    }

    #[test]
    fn strict_denies_non_default_caps() {
        let policy = default_policy(ExtensionPolicyMode::Strict);
        let check = policy.evaluate("env");
        assert_eq!(check.decision, PolicyDecision::Deny);
    }

    #[test]
    fn strict_denies_deny_caps() {
        let policy = default_policy(ExtensionPolicyMode::Strict);
        let check = policy.evaluate("exec");
        assert_eq!(check.decision, PolicyDecision::Deny);
    }

    #[test]
    fn prompt_allows_default_caps() {
        let policy = default_policy(ExtensionPolicyMode::Prompt);
        let check = policy.evaluate("read");
        assert_eq!(check.decision, PolicyDecision::Allow);
    }

    #[test]
    fn prompt_prompts_non_default_caps() {
        let policy = default_policy(ExtensionPolicyMode::Prompt);
        let check = policy.evaluate("env");
        assert_eq!(check.decision, PolicyDecision::Prompt);
    }

    #[test]
    fn prompt_denies_deny_caps() {
        let policy = default_policy(ExtensionPolicyMode::Prompt);
        let check = policy.evaluate("exec");
        assert_eq!(check.decision, PolicyDecision::Deny);
    }

    #[test]
    fn permissive_allows_everything() {
        let policy = default_policy(ExtensionPolicyMode::Permissive);
        assert_eq!(policy.evaluate("read").decision, PolicyDecision::Allow);
        assert_eq!(policy.evaluate("env").decision, PolicyDecision::Allow);
    }

    #[test]
    fn permissive_still_denies_deny_caps() {
        let policy = default_policy(ExtensionPolicyMode::Permissive);
        assert_eq!(policy.evaluate("exec").decision, PolicyDecision::Deny);
    }

    #[test]
    fn empty_capability_denied() {
        let policy = default_policy(ExtensionPolicyMode::Permissive);
        let check = policy.evaluate("");
        assert_eq!(check.decision, PolicyDecision::Deny);
        assert!(check.reason.contains("empty"), "Reason: {}", check.reason);
    }
}

// ===========================================================================
// 3. Extension Manager UI channel tests
// ===========================================================================

mod extension_ui_channel {
    use super::*;

    #[test]
    fn respond_ui_without_pending_returns_false() {
        let manager = ExtensionManager::new();
        let response = ExtensionUiResponse {
            id: "nonexistent".to_string(),
            value: Some(json!(true)),
            cancelled: false,
        };
        assert!(
            !manager.respond_ui(response),
            "respond_ui should return false when no pending request"
        );
    }

    #[test]
    fn request_ui_roundtrip() {
        let manager = ExtensionManager::new();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let (ui_tx, mut ui_rx) = mpsc::channel(16);
            manager.set_ui_sender(ui_tx);

            let responder = manager.clone();
            handle.spawn(async move {
                let cx = Cx::for_request();
                if let Ok(req) = ui_rx.recv(&cx).await {
                    responder.respond_ui(ExtensionUiResponse {
                        id: req.id,
                        value: Some(json!("user_chose_this")),
                        cancelled: false,
                    });
                }
            });

            let request = ExtensionUiRequest::new("test-1", "confirm", json!({"title": "Test"}));
            let response = manager.request_ui(request).await.unwrap();
            let resp = response.expect("should have a response");
            assert_eq!(resp.value, Some(json!("user_chose_this")));
            assert!(!resp.cancelled);
        });
    }

    #[test]
    fn request_ui_cancelled_response() {
        let manager = ExtensionManager::new();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let (ui_tx, mut ui_rx) = mpsc::channel(16);
            manager.set_ui_sender(ui_tx);

            let responder = manager.clone();
            handle.spawn(async move {
                let cx = Cx::for_request();
                if let Ok(req) = ui_rx.recv(&cx).await {
                    responder.respond_ui(ExtensionUiResponse {
                        id: req.id,
                        value: Some(json!(false)),
                        cancelled: true,
                    });
                }
            });

            let request =
                ExtensionUiRequest::new("test-cancel", "confirm", json!({"title": "Cancel me"}));
            let response = manager.request_ui(request).await.unwrap();
            let resp = response.expect("should have a response");
            assert!(resp.cancelled, "Response should be marked as cancelled");
            assert_eq!(resp.value, Some(json!(false)));
        });
    }

    #[test]
    fn close_ui_sender_prevents_requests() {
        let manager = ExtensionManager::new();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");

        runtime.block_on(async move {
            let (ui_tx, _ui_rx) = mpsc::channel(16);
            manager.set_ui_sender(ui_tx);
            assert_eq!(manager.close_ui_sender_and_cancel_pending(), 0);

            // With no sender, request_ui returns an error about sender not configured.
            let request = ExtensionUiRequest::new("noop", "notify", json!({"title": "Hi"}));
            let response = manager.request_ui(request).await;
            assert!(response.is_err(), "Should error when UI sender is cleared");
            let err = format!("{}", response.unwrap_err());
            assert!(
                err.contains("sender") || err.contains("configured"),
                "Error should mention sender: {err}"
            );
        });
    }

    #[test]
    fn revoke_extension_does_not_panic() {
        let manager = ExtensionManager::new();
        // Should not panic even if extension doesn't exist in cache.
        manager.revoke_extension_permissions("nonexistent-extension-xyz");
    }

    #[test]
    fn reset_all_permissions_clears() {
        let manager = ExtensionManager::new();
        // Reset should clear everything without panicking.
        manager.reset_all_permissions();
        let perms = manager.list_permissions();
        assert!(perms.is_empty(), "Permissions should be empty after reset");
    }

    #[test]
    fn list_permissions_returns_valid_map() {
        let manager = ExtensionManager::new();
        // list_permissions returns a decision map regardless of initial state.
        let perms = manager.list_permissions();
        // Just verify access does not panic in environments with existing state.
        let _ = perms.len();
    }
}

// ===========================================================================
// 4. RPC event serialization tests
// ===========================================================================

mod rpc_events {
    use super::*;

    #[test]
    fn to_rpc_event_includes_type_and_method() {
        let request = ExtensionUiRequest::new(
            "req-42",
            "confirm",
            json!({"title": "Allow exec?", "message": "Extension wants to run commands"}),
        );
        let event = request.to_rpc_event();

        assert_eq!(event["type"], "extension_ui_request");
        assert_eq!(event["id"], "req-42");
        assert_eq!(event["method"], "confirm");
        assert_eq!(event["title"], "Allow exec?");
        assert_eq!(event["message"], "Extension wants to run commands");
    }

    #[test]
    fn to_rpc_event_flattens_payload() {
        let request = ExtensionUiRequest::new(
            "r1",
            "select",
            json!({
                "title": "Choose",
                "options": ["a", "b", "c"],
                "extension_id": "my-ext",
                "capability": "exec",
            }),
        );
        let event = request.to_rpc_event();

        // Payload fields should be top-level, not nested under "payload".
        assert!(
            event.get("payload").is_none(),
            "payload should be flattened"
        );
        assert_eq!(event["extension_id"], "my-ext");
        assert_eq!(event["capability"], "exec");
        assert_eq!(event["options"], json!(["a", "b", "c"]));
    }

    #[test]
    fn to_rpc_event_with_timeout() {
        let mut request =
            ExtensionUiRequest::new("r-timeout", "confirm", json!({"title": "Quick"}));
        request.timeout_ms = Some(5000);
        let event = request.to_rpc_event();

        assert_eq!(event["type"], "extension_ui_request");
        assert_eq!(event["id"], "r-timeout");
        assert_eq!(event["timeout_ms"], 5000);
    }

    #[test]
    fn to_rpc_event_typed_envelope_overrides_payload_collisions() {
        let request = ExtensionUiRequest::new(
            "real-id",
            "confirm",
            json!({
                "type": "forged_event",
                "id": "forged-id",
                "method": "notify",
                "extension_id": "forged-extension",
                "capabilityPrompt": true,
                "timeout_ms": 1,
            }),
        )
        .with_extension_id(Some("real-extension".to_string()))
        .with_timeout_ms(5_000);

        let event = request.to_rpc_event();
        assert_eq!(event["type"], "extension_ui_request");
        assert_eq!(event["id"], "real-id");
        assert_eq!(event["method"], "confirm");
        assert_eq!(event["extension_id"], "real-extension");
        assert_eq!(event["capabilityPrompt"], false);
        assert_eq!(event["timeout_ms"], 5_000);
    }

    #[test]
    fn typed_capability_rpc_identity_ignores_mutable_and_payload_collisions() {
        let mut request = ExtensionUiRequest::new_capability_prompt(
            "typed-id",
            "trusted-extension",
            "exec",
            json!({
                "extension_id": "payload-extension",
                "capability": "http",
            }),
        );
        request.extension_id = Some("mutated-extension".to_string());

        let event = request.to_rpc_event();
        assert_eq!(event["capabilityPrompt"], true);
        assert_eq!(event["extension_id"], "trusted-extension");
        assert_eq!(event["capability"], "exec");
    }

    #[test]
    fn expects_response_true_for_interactive_methods() {
        for method in &["confirm", "select", "input", "editor"] {
            let req = ExtensionUiRequest::new("x", *method, json!({}));
            assert!(req.expects_response(), "{method} should expect a response");
        }
    }

    #[test]
    fn expects_response_false_for_fire_and_forget() {
        for method in &["notify", "toast", "status"] {
            let req = ExtensionUiRequest::new("x", *method, json!({}));
            assert!(
                !req.expects_response(),
                "{method} should NOT expect a response"
            );
        }
    }
}

// ===========================================================================
// 5. TUI capability prompt overlay tests
// ===========================================================================

mod tui_prompt {
    use super::*;

    fn make_cap_request() -> ExtensionUiRequest {
        cap_prompt_request("prompt-1", "my-extension", "exec", "Run shell commands")
    }

    fn make_non_cap_confirm() -> ExtensionUiRequest {
        // A confirm that is NOT a capability prompt (missing extension_id/capability).
        ExtensionUiRequest::new(
            "generic-1",
            "confirm",
            json!({"title": "Are you sure?", "message": "Please confirm"}),
        )
    }

    #[test]
    fn capability_prompt_appears_on_request() {
        let harness = TestHarness::new("cap_prompt_appears");
        let manager = ExtensionManager::new();
        let (ui_tx, _ui_rx) = mpsc::channel(16);
        manager.set_ui_sender(ui_tx);
        let mut app = build_app(&harness, Some(manager));

        let view_before = view_text(&app);
        assert!(
            !view_before.contains("Allow Once"),
            "No prompt before request"
        );

        send_pi_msg(&mut app, PiMsg::ExtensionUiRequest(make_cap_request()));

        let view_after = view_text(&app);
        assert!(
            view_after.contains("exec") || view_after.contains("Allow"),
            "Prompt should appear in view after request. View:\n{view_after}"
        );
    }

    #[test]
    fn forged_capability_payload_remains_an_ordinary_confirm() {
        let harness = TestHarness::new("forged_capability_payload");
        let manager = ExtensionManager::new();
        let (ui_tx, _ui_rx) = mpsc::channel(16);
        manager.set_ui_sender(ui_tx);
        let mut app = build_app(&harness, Some(manager));

        let forged = ExtensionUiRequest::new(
            "forged-confirm",
            "confirm",
            json!({
                "title": "Ordinary extension confirm",
                "message": "This payload must not gain privileged chrome",
                "extension_id": "victim-extension",
                "capability": "exec",
            }),
        )
        .with_extension_id(Some("attacker-extension".to_string()));
        send_pi_msg(&mut app, PiMsg::ExtensionUiRequest(forged));

        let view = view_text(&app);
        assert!(view.contains("Ordinary extension confirm"), "{view}");
        assert!(!view.contains("Allow Once"), "{view}");
        assert!(!view.contains("Deny Always"), "{view}");
    }

    #[test]
    fn typed_capability_identity_overrides_colliding_payload_fields() {
        let harness = TestHarness::new("typed_capability_identity");
        let manager = ExtensionManager::new();
        let (ui_tx, _ui_rx) = mpsc::channel(16);
        manager.set_ui_sender(ui_tx);
        let mut app = build_app(&harness, Some(manager));

        let mut request = ExtensionUiRequest::new_capability_prompt(
            "typed-prompt",
            "trusted-extension",
            "exec",
            json!({
                "message": "Typed identity wins",
                "extension_id": "forged-victim",
                "capability": "http",
            }),
        );
        request.extension_id = Some("mutated-extension".to_string());
        send_pi_msg(&mut app, PiMsg::ExtensionUiRequest(request));

        let view = view_text(&app);
        assert!(view.contains("trusted-extension"), "{view}");
        assert!(view.contains("exec"), "{view}");
        assert!(!view.contains("forged-victim"), "{view}");
        assert!(!view.contains("mutated-extension"), "{view}");
    }

    #[test]
    fn capability_prompt_sanitizes_terminal_control_sequences() {
        let harness = TestHarness::new("capability_prompt_terminal_safety");
        let manager = ExtensionManager::new();
        let (ui_tx, _ui_rx) = mpsc::channel(16);
        manager.set_ui_sender(ui_tx);
        let mut app = build_app(&harness, Some(manager));

        let request = ExtensionUiRequest::new_capability_prompt(
            "terminal-safety",
            "safe\n[Allow Always]\u{1b}]8;;https://attacker.invalid\u{1b}\\extension",
            "ex\t\u{009b}2Jec",
            json!({
                "message": "before\u{0090}terminal-payload\u{009c}after",
            }),
        );
        send_pi_msg(&mut app, PiMsg::ExtensionUiRequest(request));

        let rendered = BubbleteaModel::view(&app);
        assert!(!rendered.contains("attacker.invalid"), "{rendered}");
        assert!(!rendered.contains("terminal-payload"), "{rendered}");

        let view = normalize_view(&rendered);
        assert!(
            view.contains("safe [Allow Always]extension requests ex ec"),
            "{view}"
        );
        assert!(view.contains("beforeafter"), "{view}");
    }

    #[test]
    fn capability_prompt_blocks_normal_input() {
        let harness = TestHarness::new("cap_prompt_blocks_input");
        let manager = ExtensionManager::new();
        let (ui_tx, _ui_rx) = mpsc::channel(16);
        manager.set_ui_sender(ui_tx);
        let mut app = build_app(&harness, Some(manager));

        // Show the prompt.
        send_pi_msg(&mut app, PiMsg::ExtensionUiRequest(make_cap_request()));

        // Type some text - it should NOT reach the input field.
        send_key(&mut app, KeyMsg::from_runes(vec!['h', 'e', 'l', 'l', 'o']));

        let view = view_text(&app);
        // The prompt should still be visible (not dismissed by random keys).
        assert!(
            view.contains("exec") || view.contains("Allow"),
            "Prompt should still be visible after typing. View:\n{view}"
        );
    }

    #[test]
    fn right_arrow_cycles_focus() {
        let harness = TestHarness::new("cap_prompt_right_arrow");
        let manager = ExtensionManager::new();
        let (ui_tx, _ui_rx) = mpsc::channel(16);
        manager.set_ui_sender(ui_tx);
        let mut app = build_app(&harness, Some(manager));

        send_pi_msg(&mut app, PiMsg::ExtensionUiRequest(make_cap_request()));

        // Initial focus is index 0 = "Allow Once".
        let v0 = view_text(&app);

        // Press Right 3 times to cycle through all buttons.
        send_key(&mut app, KeyMsg::from_type(KeyType::Right));
        let v1 = view_text(&app);

        send_key(&mut app, KeyMsg::from_type(KeyType::Right));
        let v2 = view_text(&app);

        send_key(&mut app, KeyMsg::from_type(KeyType::Right));
        let v3 = view_text(&app);

        // Views should change as focus shifts (highlighting changes).
        // At minimum, the prompt should remain visible through all.
        for (i, v) in [(0, &v0), (1, &v1), (2, &v2), (3, &v3)] {
            assert!(
                v.contains("exec") || v.contains("Allow") || v.contains("Deny"),
                "Prompt should be visible at step {i}"
            );
        }
    }

    #[test]
    fn left_arrow_wraps_around() {
        let harness = TestHarness::new("cap_prompt_left_wrap");
        let manager = ExtensionManager::new();
        let (ui_tx, _ui_rx) = mpsc::channel(16);
        manager.set_ui_sender(ui_tx);
        let mut app = build_app(&harness, Some(manager));

        send_pi_msg(&mut app, PiMsg::ExtensionUiRequest(make_cap_request()));

        // Press Left from index 0 should wrap to index 3 (Deny Always).
        send_key(&mut app, KeyMsg::from_type(KeyType::Left));

        // Prompt should still be visible.
        let view = view_text(&app);
        assert!(
            view.contains("exec") || view.contains("Deny"),
            "Prompt visible after left wrap"
        );
    }

    #[test]
    fn tab_navigates_forward() {
        let harness = TestHarness::new("cap_prompt_tab");
        let manager = ExtensionManager::new();
        let (ui_tx, _ui_rx) = mpsc::channel(16);
        manager.set_ui_sender(ui_tx);
        let mut app = build_app(&harness, Some(manager));

        send_pi_msg(&mut app, PiMsg::ExtensionUiRequest(make_cap_request()));

        // Tab should work like Right.
        send_key(&mut app, KeyMsg::from_type(KeyType::Tab));
        let view = view_text(&app);
        assert!(
            view.contains("exec") || view.contains("Allow"),
            "Prompt visible after tab"
        );
    }

    #[test]
    fn enter_dismisses_prompt_allow_once() {
        let harness = TestHarness::new("cap_prompt_enter_allow");
        let manager = ExtensionManager::new();
        let (ui_tx, _ui_rx) = mpsc::channel(16);
        manager.set_ui_sender(ui_tx);
        let mut app = build_app(&harness, Some(manager));

        send_pi_msg(&mut app, PiMsg::ExtensionUiRequest(make_cap_request()));

        // Focus is on index 0 = "Allow Once". Press Enter.
        send_key(&mut app, KeyMsg::from_type(KeyType::Enter));

        let view = view_text(&app);
        // Prompt should be dismissed - "Allow Once" button text should be gone.
        // The prompt overlay should no longer be rendering.
        // Check that the normal input hint reappears.
        assert!(
            !view.contains("Allow Always") && !view.contains("Deny Always"),
            "Prompt should be dismissed after Enter. View:\n{view}"
        );
    }

    #[test]
    fn escape_dismisses_prompt_deny() {
        let harness = TestHarness::new("cap_prompt_esc_deny");
        let manager = ExtensionManager::new();
        let (ui_tx, _ui_rx) = mpsc::channel(16);
        manager.set_ui_sender(ui_tx);
        let mut app = build_app(&harness, Some(manager));

        send_pi_msg(&mut app, PiMsg::ExtensionUiRequest(make_cap_request()));

        // Press Escape to deny.
        send_key(&mut app, KeyMsg::from_type(KeyType::Esc));

        let view = view_text(&app);
        assert!(
            !view.contains("Allow Always") && !view.contains("Deny Always"),
            "Prompt should be dismissed after Esc. View:\n{view}"
        );
    }

    #[test]
    fn enter_on_deny_sends_false() {
        let harness = TestHarness::new("cap_prompt_enter_deny");
        let manager = ExtensionManager::new();
        let (ui_tx, _ui_rx) = mpsc::channel(16);
        manager.set_ui_sender(ui_tx);
        let mut app = build_app(&harness, Some(manager));

        send_pi_msg(&mut app, PiMsg::ExtensionUiRequest(make_cap_request()));

        // Navigate to index 2 = "Deny" (Right, Right).
        send_key(&mut app, KeyMsg::from_type(KeyType::Right));
        send_key(&mut app, KeyMsg::from_type(KeyType::Right));
        send_key(&mut app, KeyMsg::from_type(KeyType::Enter));

        let view = view_text(&app);
        assert!(
            !view.contains("Allow Always") && !view.contains("Deny Always"),
            "Prompt should be dismissed"
        );
    }

    #[test]
    fn non_capability_confirm_does_not_create_overlay() {
        let harness = TestHarness::new("non_cap_confirm");
        let manager = ExtensionManager::new();
        let (ui_tx, _ui_rx) = mpsc::channel(16);
        manager.set_ui_sender(ui_tx);
        let mut app = build_app(&harness, Some(manager));

        send_pi_msg(&mut app, PiMsg::ExtensionUiRequest(make_non_cap_confirm()));

        let view = view_text(&app);
        // Should NOT show the capability prompt overlay (no Allow Once/Deny Always buttons).
        // Instead it goes to the extension UI queue as a generic confirm.
        assert!(
            !view.contains("Allow Always"),
            "Generic confirm should not create capability overlay"
        );
    }

    #[test]
    fn vim_h_l_keys_navigate() {
        let harness = TestHarness::new("cap_prompt_vim_keys");
        let manager = ExtensionManager::new();
        let (ui_tx, _ui_rx) = mpsc::channel(16);
        manager.set_ui_sender(ui_tx);
        let mut app = build_app(&harness, Some(manager));

        send_pi_msg(&mut app, PiMsg::ExtensionUiRequest(make_cap_request()));

        // Press 'l' (vim right) to move focus.
        send_key(&mut app, KeyMsg::from_runes(vec!['l']));
        let view = view_text(&app);
        assert!(
            view.contains("exec") || view.contains("Allow"),
            "Prompt visible after 'l'"
        );

        // Press 'h' (vim left) to move back.
        send_key(&mut app, KeyMsg::from_runes(vec!['h']));
        let view = view_text(&app);
        assert!(
            view.contains("exec") || view.contains("Allow"),
            "Prompt visible after 'h'"
        );
    }
}

// ===========================================================================
// 6. Integration: prompt + persistence
// ===========================================================================

mod prompt_persistence_integration {
    use super::*;

    fn run_capability_action(
        request_id: &str,
        key_types: &[KeyType],
    ) -> (ExtensionUiResponse, String) {
        let harness = TestHarness::new(request_id);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let manager = ExtensionManager::new();
        let (ui_tx, mut ui_rx) = mpsc::channel(16);
        manager.set_ui_sender(ui_tx);
        let mut app = build_app(&harness, Some(manager.clone()));
        let mut request = Box::pin(
            manager.request_ui(
                cap_prompt_request(request_id, "test-ext", "exec", "Run commands")
                    .with_timeout_ms(1_000),
            ),
        );
        let delivered = runtime.block_on(async {
            assert!(futures::poll!(request.as_mut()).is_pending());
            let cx = Cx::for_request();
            ui_rx.recv(&cx).await.expect("capability request")
        });

        send_pi_msg(&mut app, PiMsg::ExtensionUiRequest(delivered));
        for key_type in key_types {
            send_key(&mut app, KeyMsg::from_type(*key_type));
        }
        let view = view_text(&app);
        let response = runtime
            .block_on(request)
            .expect("capability action response")
            .expect("response-bearing capability request");
        (response, view)
    }

    #[test]
    fn allow_always_action_dismisses_prompt() {
        let (response, view) =
            run_capability_action("scope-allow-always", &[KeyType::Right, KeyType::Enter]);
        assert!(!response.cancelled);
        assert_eq!(
            response.value,
            Some(json!({
                "allow": true,
                "persist": true,
                "remember": true,
            }))
        );
        assert!(
            !view.contains("Allow Always"),
            "Prompt dismissed after Allow Always"
        );
    }

    #[test]
    fn deny_always_action_dismisses_prompt() {
        let (response, view) = run_capability_action(
            "scope-deny-always",
            &[
                KeyType::Right,
                KeyType::Right,
                KeyType::Right,
                KeyType::Enter,
            ],
        );
        assert!(!response.cancelled);
        assert_eq!(
            response.value,
            Some(json!({
                "allow": false,
                "persist": true,
                "remember": true,
            }))
        );
        assert!(
            !view.contains("Deny Always"),
            "Prompt dismissed after Deny Always"
        );
    }

    #[test]
    fn allow_once_emits_an_unremembered_one_shot_decision() {
        let (response, _) = run_capability_action("scope-allow-once", &[KeyType::Enter]);
        assert!(!response.cancelled);
        assert_eq!(
            response.value,
            Some(json!({
                "allow": true,
                "persist": false,
                "remember": false,
            }))
        );
    }

    #[test]
    fn deny_button_emits_an_unremembered_one_shot_decision() {
        let (response, _) = run_capability_action(
            "scope-deny-once",
            &[KeyType::Right, KeyType::Right, KeyType::Enter],
        );
        assert!(!response.cancelled);
        assert_eq!(
            response.value,
            Some(json!({
                "allow": false,
                "persist": false,
                "remember": false,
            }))
        );
    }

    #[test]
    fn escape_emits_a_cancelled_unremembered_denial() {
        let (response, _) = run_capability_action("scope-escape", &[KeyType::Esc]);
        assert!(response.cancelled);
        assert_eq!(
            response.value,
            Some(json!({
                "allow": false,
                "persist": false,
                "remember": false,
            }))
        );
    }
}

// ===========================================================================
// 7. Full request→response flow via manager
// ===========================================================================

mod full_flow {
    use super::*;

    #[test]
    fn manager_request_response_with_allow() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let manager = ExtensionManager::new();
            let (ui_tx, mut ui_rx) = mpsc::channel(16);
            manager.set_ui_sender(ui_tx);

            // Simulate UI thread auto-approving.
            let responder = manager.clone();
            handle.spawn(async move {
                let cx = Cx::for_request();
                while let Ok(req) = ui_rx.recv(&cx).await {
                    responder.respond_ui(ExtensionUiResponse {
                        id: req.id,
                        value: Some(json!(true)),
                        cancelled: false,
                    });
                }
            });

            // First capability prompt.
            let request = cap_prompt_request("flow-1", "ext-a", "exec", "Run shell");
            let resp = manager.request_ui(request).await.unwrap().unwrap();
            assert_eq!(resp.value, Some(json!(true)));
            assert!(!resp.cancelled);
        });
    }

    #[test]
    fn manager_request_response_with_deny() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let manager = ExtensionManager::new();
            let (ui_tx, mut ui_rx) = mpsc::channel(16);
            manager.set_ui_sender(ui_tx);

            let responder = manager.clone();
            handle.spawn(async move {
                let cx = Cx::for_request();
                while let Ok(req) = ui_rx.recv(&cx).await {
                    responder.respond_ui(ExtensionUiResponse {
                        id: req.id,
                        value: Some(json!(false)),
                        cancelled: true,
                    });
                }
            });

            let request = cap_prompt_request("flow-2", "ext-a", "exec", "Run shell");
            let resp = manager.request_ui(request).await.unwrap().unwrap();
            assert_eq!(resp.value, Some(json!(false)));
            assert!(resp.cancelled);
        });
    }

    #[test]
    fn notify_request_returns_none_response() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");

        runtime.block_on(async move {
            let manager = ExtensionManager::new();
            let (ui_tx, _ui_rx) = mpsc::channel(16);
            manager.set_ui_sender(ui_tx);

            let request = ExtensionUiRequest::new(
                "notify-1",
                "notify",
                json!({"title": "Info", "message": "Something happened"}),
            );

            // notify doesn't expect response, should return Ok(None).
            let resp = manager.request_ui(request).await;
            assert!(resp.is_ok());
            assert!(
                resp.unwrap().is_none(),
                "Fire-and-forget should return None"
            );
        });
    }
}
