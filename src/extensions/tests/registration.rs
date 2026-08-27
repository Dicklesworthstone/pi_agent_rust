//! Registration, model control, session, and message API tests.

use super::*;

// ========================================================================
// Extension Registration API tests (bd-1yh7)
// ========================================================================

// --- registerCommand tests ---

#[test]
fn register_command_stores_metadata() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerCommand",
            json!({ "name": "deploy", "description": "Deploy the app" }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Success(_)));
        assert!(manager.has_command("deploy"));

        let commands = manager.list_commands();
        let cmd = commands
            .iter()
            .find(|c| c.get("name").and_then(Value::as_str) == Some("deploy"))
            .expect("deploy command should exist");
        assert_eq!(
            cmd.get("description").and_then(Value::as_str),
            Some("Deploy the app")
        );
    });
}

#[test]
fn register_command_empty_name_fails() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerCommand",
            json!({ "name": "" }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Error { .. }));
        if let HostcallOutcome::Error { code, message } = outcome {
            assert_eq!(code, "invalid_request");
            assert!(message.contains("name is required"));
        }
    });
}

#[test]
fn register_command_missing_name_fails() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        let outcome =
            dispatch_hostcall_events("call-1", &manager, &tools, "registerCommand", json!({}))
                .await;

        assert!(matches!(outcome, HostcallOutcome::Error { .. }));
    });
}

#[test]
fn register_command_no_description_ok() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerCommand",
            json!({ "name": "build" }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Success(_)));
        assert!(manager.has_command("build"));
    });
}

#[test]
fn register_command_multiple_commands() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        for name in &["deploy", "build", "test"] {
            let outcome = dispatch_hostcall_events(
                "call-1",
                &manager,
                &tools,
                "registerCommand",
                json!({ "name": name }),
            )
            .await;
            assert!(matches!(outcome, HostcallOutcome::Success(_)));
        }

        assert!(manager.has_command("deploy"));
        assert!(manager.has_command("build"));
        assert!(manager.has_command("test"));
        assert_eq!(manager.list_commands().len(), 3);
    });
}

#[test]
fn register_command_via_register_payload() {
    let manager = ExtensionManager::new();
    manager.register(RegisterPayload {
        name: "test-ext".to_string(),
        version: "1.0.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: vec![
            json!({ "name": "deploy", "description": "Deploy" }),
            json!({ "name": "rollback", "description": "Rollback" }),
        ],
        shortcuts: Vec::new(),
        flags: Vec::new(),
        event_hooks: Vec::new(),
    });

    assert!(manager.has_command("deploy"));
    assert!(manager.has_command("rollback"));
    assert!(!manager.has_command("nonexistent"));

    let commands = manager.list_commands();
    assert_eq!(commands.len(), 2);
}

// --- registerFlag tests ---

#[test]
fn register_flag_stores_spec() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        let outcome = dispatch_hostcall_events(
                "call-1",
                &manager,
                &tools,
                "registerFlag",
                json!({ "name": "verbose", "type": "bool", "default": false, "description": "Enable verbose output" }),
            )
            .await;

        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        let flags = manager.list_flags();
        assert_eq!(flags.len(), 1);
        let flag = &flags[0];
        assert_eq!(flag.get("name").and_then(Value::as_str), Some("verbose"));
        assert_eq!(flag.get("type").and_then(Value::as_str), Some("bool"));
        assert_eq!(flag.get("default").and_then(Value::as_bool), Some(false));
    });
}

#[test]
fn register_flag_empty_name_fails() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerFlag",
            json!({ "name": "", "type": "string" }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Error { .. }));
        if let HostcallOutcome::Error { code, message } = outcome {
            assert_eq!(code, "invalid_request");
            assert!(message.contains("name is required"));
        }
    });
}

#[test]
fn register_flag_hostcall_deduplicates_by_name() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerFlag",
            json!({ "name": "output", "type": "string", "default": "json" }),
        )
        .await;

        dispatch_hostcall_events(
            "call-2",
            &manager,
            &tools,
            "registerFlag",
            json!({ "name": "output", "type": "string", "default": "yaml" }),
        )
        .await;

        let flags = manager.list_flags();
        assert_eq!(flags.len(), 1);
        assert_eq!(
            flags[0].get("default").and_then(Value::as_str),
            Some("yaml")
        );
    });
}

#[test]
fn register_flag_multiple_types() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        for (name, ty, default) in [
            ("verbose", "bool", json!(false)),
            ("timeout", "number", json!(30)),
            ("format", "string", json!("json")),
        ] {
            dispatch_hostcall_events(
                "call-1",
                &manager,
                &tools,
                "registerFlag",
                json!({ "name": name, "type": ty, "default": default }),
            )
            .await;
        }

        let flags = manager.list_flags();
        assert_eq!(flags.len(), 3);
    });
}

#[test]
fn register_flag_via_register_payload() {
    let manager = ExtensionManager::new();
    manager.register(RegisterPayload {
        name: "test-ext".to_string(),
        version: "1.0.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: Vec::new(),
        shortcuts: Vec::new(),
        flags: vec![
            json!({ "name": "verbose", "type": "bool", "default": false }),
            json!({ "name": "format", "type": "string", "default": "json" }),
        ],
        event_hooks: Vec::new(),
    });

    let flags = manager.list_flags();
    assert_eq!(flags.len(), 2);
}

// --- registerProvider tests ---

#[test]
fn register_provider_stores_config() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerProvider",
            json!({
                "id": "my-llm",
                "api": "openai-completions",
                "baseUrl": "https://api.example.com/v1",
                "apiKey": "MY_API_KEY",
                "models": [{ "id": "fast-1", "name": "Fast Model" }]
            }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        let providers = manager.extension_providers();
        assert_eq!(providers.len(), 1);
        assert_eq!(
            providers[0].get("id").and_then(Value::as_str),
            Some("my-llm")
        );
    });
}

#[test]
fn register_provider_missing_id_fails() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerProvider",
            json!({ "api": "openai-completions" }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Error { .. }));
        if let HostcallOutcome::Error { code, message } = outcome {
            assert_eq!(code, "invalid_request");
            assert!(message.contains("id is required"));
        }
    });
}

#[test]
fn register_provider_missing_api_fails() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerProvider",
            json!({ "id": "my-llm" }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Error { .. }));
        if let HostcallOutcome::Error { code, message } = outcome {
            assert_eq!(code, "invalid_request");
            assert!(message.contains("api is required"));
        }
    });
}

#[test]
fn register_provider_unsupported_api_type_fails() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerProvider",
            json!({ "id": "my-llm", "api": "custom-nonsense" }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Error { .. }));
        if let HostcallOutcome::Error { code, message } = outcome {
            assert_eq!(code, "invalid_request");
            assert!(message.contains("unsupported api type"));
        }
    });
}

#[test]
fn register_provider_all_valid_api_types() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        for api in [
            "anthropic-messages",
            "openai-completions",
            "openai-responses",
            "google-generative-ai",
        ] {
            let outcome = dispatch_hostcall_events(
                "call-1",
                &manager,
                &tools,
                "registerProvider",
                json!({ "id": format!("provider-{api}"), "api": api }),
            )
            .await;
            assert!(
                matches!(outcome, HostcallOutcome::Success(_)),
                "api type {api} should be accepted"
            );
        }

        assert_eq!(manager.extension_providers().len(), 4);
    });
}

#[test]
fn register_provider_model_entries() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerProvider",
            json!({
                "id": "my-llm",
                "api": "openai-completions",
                "baseUrl": "https://api.example.com/v1",
                "models": [
                    { "id": "fast-1", "name": "Fast Model" },
                    { "id": "slow-1", "name": "Slow Model", "reasoning": true }
                ]
            }),
        )
        .await;

        let entries = manager.extension_model_entries();
        assert_eq!(entries.len(), 2);
    });
}

#[test]
fn register_provider_oauth_config_extracted() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        dispatch_hostcall_events(
            "call-oauth",
            &manager,
            &tools,
            "registerProvider",
            json!({
                "id": "oauth-llm",
                "api": "openai-completions",
                "baseUrl": "https://api.oauth-llm.com/v1",
                "oauth": {
                    "authUrl": "https://auth.oauth-llm.com/authorize",
                    "tokenUrl": "https://auth.oauth-llm.com/token",
                    "clientId": "client-abc",
                    "scopes": ["read", "write", "admin"],
                    "redirectUri": "http://localhost:9999/callback"
                },
                "models": [
                    { "id": "oauth-model-1", "name": "OAuth Model" }
                ]
            }),
        )
        .await;

        let entries = manager.extension_model_entries();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        let oauth = entry
            .oauth_config
            .as_ref()
            .expect("oauth_config should be present");
        assert_eq!(oauth.auth_url, "https://auth.oauth-llm.com/authorize");
        assert_eq!(oauth.token_url, "https://auth.oauth-llm.com/token");
        assert_eq!(oauth.client_id, "client-abc");
        assert_eq!(oauth.scopes, vec!["read", "write", "admin"]);
        assert_eq!(
            oauth.redirect_uri.as_deref(),
            Some("http://localhost:9999/callback")
        );
    });
}

#[test]
fn register_provider_without_oauth_config_has_none() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        dispatch_hostcall_events(
            "call-no-oauth",
            &manager,
            &tools,
            "registerProvider",
            json!({
                "id": "plain-llm",
                "api": "anthropic-messages",
                "baseUrl": "https://api.plain-llm.com/v1",
                "models": [
                    { "id": "plain-model", "name": "Plain Model" }
                ]
            }),
        )
        .await;

        let entries = manager.extension_model_entries();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].oauth_config.is_none());
    });
}

#[test]
fn register_provider_oauth_missing_required_fields_ignored() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        dispatch_hostcall_events(
            "call-bad-oauth",
            &manager,
            &tools,
            "registerProvider",
            json!({
                "id": "bad-oauth-llm",
                "api": "openai-completions",
                "baseUrl": "https://api.bad.com/v1",
                "oauth": {
                    "authUrl": "https://auth.bad.com/authorize",
                    "tokenUrl": "https://auth.bad.com/token"
                },
                "models": [
                    { "id": "bad-model", "name": "Bad Model" }
                ]
            }),
        )
        .await;

        let entries = manager.extension_model_entries();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].oauth_config.is_none());
    });
}

#[test]
fn register_provider_oauth_no_redirect_uri() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        dispatch_hostcall_events(
            "call-no-redirect",
            &manager,
            &tools,
            "registerProvider",
            json!({
                "id": "no-redirect-llm",
                "api": "openai-completions",
                "baseUrl": "https://api.nr.com/v1",
                "oauth": {
                    "authUrl": "https://auth.nr.com/authorize",
                    "tokenUrl": "https://auth.nr.com/token",
                    "clientId": "client-nr"
                },
                "models": [
                    { "id": "nr-model", "name": "NR Model" }
                ]
            }),
        )
        .await;

        let entries = manager.extension_model_entries();
        assert_eq!(entries.len(), 1);
        let oauth = entries[0]
            .oauth_config
            .as_ref()
            .expect("oauth should be present");
        assert_eq!(oauth.client_id, "client-nr");
        assert!(oauth.redirect_uri.is_none());
        assert!(oauth.scopes.is_empty());
    });
}

// --- registerShortcut tests ---

#[test]
fn register_shortcut_via_payload() {
    let manager = ExtensionManager::new();
    manager.register(RegisterPayload {
        name: "test-ext".to_string(),
        version: "1.0.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: Vec::new(),
        shortcuts: vec![json!({
            "key": "Ctrl+Shift+D",
            "key_id": "ctrl+shift+d",
            "description": "Deploy shortcut"
        })],
        flags: Vec::new(),
        event_hooks: Vec::new(),
    });

    assert!(manager.has_shortcut("ctrl+shift+d"));
    assert!(!manager.has_shortcut("ctrl+x"));

    let shortcuts = manager.list_shortcuts();
    assert_eq!(shortcuts.len(), 1);
    assert_eq!(
        shortcuts[0].get("description").and_then(Value::as_str),
        Some("Deploy shortcut")
    );
}

#[test]
fn register_shortcut_case_insensitive_lookup() {
    let manager = ExtensionManager::new();
    manager.register(RegisterPayload {
        name: "test-ext".to_string(),
        version: "1.0.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: Vec::new(),
        shortcuts: vec![json!({
            "key": "Ctrl+K",
            "key_id": "ctrl+k",
            "description": "Quick action"
        })],
        flags: Vec::new(),
        event_hooks: Vec::new(),
    });

    assert!(manager.has_shortcut("ctrl+k"));
    assert!(manager.has_shortcut("Ctrl+K"));
    assert!(manager.has_shortcut("CTRL+K"));
}

#[test]
fn register_shortcut_multiple() {
    let manager = ExtensionManager::new();
    manager.register(RegisterPayload {
        name: "test-ext".to_string(),
        version: "1.0.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: Vec::new(),
        slash_commands: Vec::new(),
        shortcuts: vec![
            json!({ "key": "Ctrl+K", "key_id": "ctrl+k", "description": "Action 1" }),
            json!({ "key": "Alt+D", "key_id": "alt+d", "description": "Action 2" }),
            json!({ "key": "F5", "key_id": "f5", "description": "Action 3" }),
        ],
        flags: Vec::new(),
        event_hooks: Vec::new(),
    });

    assert_eq!(manager.list_shortcuts().len(), 3);
    assert!(manager.has_shortcut("ctrl+k"));
    assert!(manager.has_shortcut("alt+d"));
    assert!(manager.has_shortcut("f5"));
}

// --- Combined registration tests ---

#[test]
fn register_all_apis_on_single_extension() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);

        // Register extension with commands, shortcuts, flags, and a tool
        manager.register(RegisterPayload {
            name: "full-ext".to_string(),
            version: "2.0.0".to_string(),
            api_version: PROTOCOL_VERSION.to_string(),
            capabilities: Vec::new(),
            capability_manifest: None,
            tools: vec![json!({
                "name": "ext_tool",
                "label": "Extension Tool",
                "description": "A tool",
                "parameters": { "type": "object" }
            })],
            slash_commands: vec![json!({ "name": "deploy", "description": "Deploy" })],
            shortcuts: vec![json!({
                "key": "Ctrl+D",
                "key_id": "ctrl+d",
                "description": "Deploy shortcut"
            })],
            flags: vec![json!({ "name": "verbose", "type": "bool", "default": false })],
            event_hooks: vec!["tool_call".to_string()],
        });

        // Also register a provider via hostcall
        dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerProvider",
            json!({
                "id": "my-llm",
                "api": "anthropic-messages",
                "models": [{ "id": "model-1" }]
            }),
        )
        .await;

        // Verify everything is accessible
        assert!(manager.has_command("deploy"));
        assert!(manager.has_shortcut("ctrl+d"));
        assert_eq!(manager.list_commands().len(), 1);
        assert_eq!(manager.list_shortcuts().len(), 1);
        assert_eq!(manager.list_flags().len(), 1);
        assert_eq!(manager.extension_providers().len(), 1);
        assert_eq!(manager.extension_model_entries().len(), 1);
    });
}

// ========================================================================
// Model Control API tests (bd-1rqs / bd-vs72)
// ========================================================================

#[test]
fn events_get_model_returns_null_when_no_session() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);

        let outcome =
            dispatch_hostcall_events("call-1", &manager, &tools, "getModel", json!({})).await;

        let value = match outcome {
            HostcallOutcome::Success(value) => value,
            HostcallOutcome::Error { code, message } => {
                unreachable!("expected success, got error {code}: {message}");
            }
            HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            } => {
                unreachable!(
                    "expected success, got stream chunk seq={sequence} final={is_final}: {chunk}"
                );
            }
        };
        assert!(value.get("provider").unwrap().is_null());
        assert!(value.get("modelId").unwrap().is_null());
    });
}

#[test]
fn events_set_model_updates_in_memory_state() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);

        // Set model via hostcall.
        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "setModel",
            json!({ "provider": "anthropic", "modelId": "claude-opus-4-5-20251101" }),
        )
        .await;
        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        // In-memory state should reflect the change.
        let (provider, model_id) = manager.current_model();
        assert_eq!(provider.as_deref(), Some("anthropic"));
        assert_eq!(model_id.as_deref(), Some("claude-opus-4-5-20251101"));
    });
}

#[test]
fn events_set_model_invalidates_ctx_cache_generation() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);

        let gen_before = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "setModel",
            json!({ "provider": "anthropic", "modelId": "claude-opus-4-5-20251101" }),
        )
        .await;
        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        let gen_after = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        assert_eq!(gen_after, gen_before + 1);
    });
}

#[test]
fn events_get_thinking_level_returns_null_when_not_set() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);

        let outcome =
            dispatch_hostcall_events("call-1", &manager, &tools, "getThinkingLevel", json!({}))
                .await;

        let value = match outcome {
            HostcallOutcome::Success(value) => value,
            HostcallOutcome::Error { code, message } => {
                unreachable!("expected success, got error {code}: {message}");
            }
            HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            } => {
                unreachable!(
                    "expected success, got stream chunk seq={sequence} final={is_final}: {chunk}"
                );
            }
        };
        assert!(value.get("thinkingLevel").unwrap().is_null());
    });
}

#[test]
fn events_set_thinking_level_updates_and_reflects() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);

        // Set thinking level.
        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "setThinkingLevel",
            json!({ "thinkingLevel": "high" }),
        )
        .await;
        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        // In-memory state should reflect the change.
        assert_eq!(manager.current_thinking_level().as_deref(), Some("high"));

        // Getting via hostcall should also reflect.
        let outcome =
            dispatch_hostcall_events("call-2", &manager, &tools, "getThinkingLevel", json!({}))
                .await;

        let value = match outcome {
            HostcallOutcome::Success(value) => value,
            HostcallOutcome::Error { code, message } => {
                unreachable!("expected success, got error {code}: {message}");
            }
            HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            } => {
                unreachable!(
                    "expected success, got stream chunk seq={sequence} final={is_final}: {chunk}"
                );
            }
        };
        assert_eq!(
            value.get("thinkingLevel").and_then(Value::as_str),
            Some("high")
        );
    });
}

#[test]
fn events_set_thinking_level_invalidates_ctx_cache_generation() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);

        let gen_before = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "setThinkingLevel",
            json!({ "thinkingLevel": "high" }),
        )
        .await;
        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        let gen_after = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        assert_eq!(gen_after, gen_before + 1);
    });
}

#[test]
fn events_set_model_snake_case_variant() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "set_model",
            json!({ "provider": "openai", "model_id": "gpt-5.2" }),
        )
        .await;
        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        let (provider, model_id) = manager.current_model();
        assert_eq!(provider.as_deref(), Some("openai"));
        assert_eq!(model_id.as_deref(), Some("gpt-5.2"));
    });
}

#[test]
fn events_set_thinking_level_empty_becomes_none() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);

        // Set a level first.
        dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "setThinkingLevel",
            json!({ "thinkingLevel": "medium" }),
        )
        .await;
        assert_eq!(manager.current_thinking_level().as_deref(), Some("medium"));

        // Set empty string should clear (filter removes empty).
        dispatch_hostcall_events(
            "call-2",
            &manager,
            &tools,
            "setThinkingLevel",
            json!({ "thinkingLevel": "" }),
        )
        .await;
        assert!(manager.current_thinking_level().is_none());
    });
}

// ========================================================================
// Session dispatch tests (bd-1rqs)
// ========================================================================

/// Minimal test session for session dispatch testing.
pub(super) struct MockSession {
    name: std::sync::Mutex<Option<String>>,
    labels: std::sync::Mutex<Vec<(String, Option<String>)>>,
    model: std::sync::Mutex<(Option<String>, Option<String>)>,
    thinking_level: std::sync::Mutex<Option<String>>,
}

impl MockSession {
    pub(super) fn new() -> Self {
        Self {
            name: std::sync::Mutex::new(None),
            labels: std::sync::Mutex::new(Vec::new()),
            model: std::sync::Mutex::new((None, None)),
            thinking_level: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait]
impl ExtensionSession for MockSession {
    async fn get_state(&self) -> Value {
        let name = self
            .name
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        json!({ "sessionName": name })
    }
    async fn get_messages(&self) -> Vec<crate::session::SessionMessage> {
        Vec::new()
    }
    async fn get_entries(&self) -> Vec<Value> {
        Vec::new()
    }
    async fn get_branch(&self) -> Vec<Value> {
        Vec::new()
    }
    async fn set_name(&self, name: String) -> Result<()> {
        *self
            .name
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(name);
        Ok(())
    }
    async fn append_message(&self, _message: crate::session::SessionMessage) -> Result<()> {
        Ok(())
    }
    async fn append_custom_entry(&self, _custom_type: String, _data: Option<Value>) -> Result<()> {
        Ok(())
    }
    async fn set_model(&self, provider: String, model_id: String) -> Result<()> {
        *self
            .model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (Some(provider), Some(model_id));
        Ok(())
    }
    async fn get_model(&self) -> (Option<String>, Option<String>) {
        self.model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    async fn set_thinking_level(&self, level: String) -> Result<()> {
        *self
            .thinking_level
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(level);
        Ok(())
    }
    async fn get_thinking_level(&self) -> Option<String> {
        self.thinking_level
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    async fn set_label(&self, target_id: String, label: Option<String>) -> Result<()> {
        self.labels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((target_id, label));
        Ok(())
    }
}

#[test]
fn session_set_name_and_get_name() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        // Set name via session dispatch.
        let outcome = dispatch_hostcall_session(
            "call-1",
            &manager,
            "set_name",
            json!({ "name": "My Feature Work" }),
        )
        .await;
        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        // Get name via session dispatch.
        let outcome = dispatch_hostcall_session("call-2", &manager, "get_name", json!({})).await;
        let value = match outcome {
            HostcallOutcome::Success(value) => value,
            HostcallOutcome::Error { code, message } => {
                unreachable!("expected success, got error {code}: {message}");
            }
            HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            } => {
                unreachable!(
                    "expected success, got stream chunk seq={sequence} final={is_final}: {chunk}"
                );
            }
        };
        assert_eq!(value.as_str(), Some("My Feature Work"));
    });
}

#[test]
fn session_set_name_invalidates_ctx_cache_generation() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        let gen_before = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        let outcome = dispatch_hostcall_session(
            "call-1",
            &manager,
            "set_name",
            json!({ "name": "My Feature Work" }),
        )
        .await;
        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        let gen_after = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        assert_eq!(gen_after, gen_before + 1);
    });
}

#[test]
fn session_set_label_dispatches_to_session() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        let outcome = dispatch_hostcall_session(
            "call-1",
            &manager,
            "set_label",
            json!({ "targetId": "entry-42", "label": "important" }),
        )
        .await;
        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        {
            let labels = session
                .labels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(labels.len(), 1);
            assert_eq!(labels[0].0, "entry-42");
            assert_eq!(labels[0].1.as_deref(), Some("important"));
            drop(labels);
        }
    });
}

#[test]
fn session_append_message_snake_case_alias_succeeds() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        let outcome = dispatch_hostcall_session(
            "call-append-msg",
            &manager,
            "append_message",
            json!({
                "message": {
                    "role": "user",
                    "content": "hello"
                }
            }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Success(_)));
    });
}

#[test]
fn session_append_message_invalidates_ctx_cache_generation() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        let gen_before = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        let outcome = dispatch_hostcall_session(
            "call-append-msg",
            &manager,
            "append_message",
            json!({
                "message": {
                    "role": "user",
                    "content": "hello"
                }
            }),
        )
        .await;
        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        let gen_after = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        assert_eq!(gen_after, gen_before + 1);
    });
}

#[test]
fn session_set_model_invalidates_ctx_cache_generation() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        let gen_before = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        let outcome = dispatch_hostcall_session(
            "call-set-model",
            &manager,
            "set_model",
            json!({
                "provider": "anthropic",
                "modelId": "claude-opus"
            }),
        )
        .await;
        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        let gen_after = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        assert_eq!(gen_after, gen_before + 1);
    });
}

#[test]
fn session_set_thinking_level_invalidates_ctx_cache_generation() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        let gen_before = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        let outcome = dispatch_hostcall_session(
            "call-set-thinking",
            &manager,
            "setThinkingLevel",
            json!({ "level": "high" }),
        )
        .await;
        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        let gen_after = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        assert_eq!(gen_after, gen_before + 1);
    });
}

#[test]
fn session_set_label_invalidates_ctx_cache_generation() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        let gen_before = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        let outcome = dispatch_hostcall_session(
            "call-set-label",
            &manager,
            "setLabel",
            json!({
                "targetId": "entry-42",
                "label": "important"
            }),
        )
        .await;
        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        let gen_after = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        assert_eq!(gen_after, gen_before + 1);
    });
}

#[test]
fn session_set_label_requires_target_id() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        let outcome = dispatch_hostcall_session(
            "call-1",
            &manager,
            "set_label",
            json!({ "label": "important" }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Error { .. }));
    });
}

#[test]
fn session_set_label_null_label_clears() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        let outcome = dispatch_hostcall_session(
            "call-1",
            &manager,
            "set_label",
            json!({ "targetId": "entry-99" }),
        )
        .await;
        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        {
            let labels = session
                .labels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(labels.len(), 1);
            assert_eq!(labels[0].0, "entry-99");
            assert!(labels[0].1.is_none());
            drop(labels);
        }
    });
}

#[test]
fn session_dispatch_fails_without_session() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();

        let outcome = dispatch_hostcall_session("call-1", &manager, "get_name", json!({})).await;
        assert!(matches!(outcome, HostcallOutcome::Error { .. }));
    });
}

#[test]
fn session_model_control_via_session_dispatch() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        // setModel via events should persist to session.
        dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "setModel",
            json!({ "provider": "anthropic", "modelId": "claude-opus-4-5-20251101" }),
        )
        .await;

        // Verify session was updated.
        let (provider, model_id) = session
            .model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(provider.as_deref(), Some("anthropic"));
        assert_eq!(model_id.as_deref(), Some("claude-opus-4-5-20251101"));

        // getModel via events should read from session.
        let outcome =
            dispatch_hostcall_events("call-2", &manager, &tools, "getModel", json!({})).await;
        let value = match outcome {
            HostcallOutcome::Success(value) => value,
            HostcallOutcome::Error { code, message } => {
                unreachable!("expected success, got error {code}: {message}");
            }
            HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            } => {
                unreachable!(
                    "expected success, got stream chunk seq={sequence} final={is_final}: {chunk}"
                );
            }
        };
        assert_eq!(
            value.get("provider").and_then(Value::as_str),
            Some("anthropic")
        );
        assert_eq!(
            value.get("modelId").and_then(Value::as_str),
            Some("claude-opus-4-5-20251101")
        );
    });
}

#[test]
fn session_thinking_level_via_session_dispatch() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        // setThinkingLevel via events should persist to session.
        dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "setThinkingLevel",
            json!({ "thinkingLevel": "low" }),
        )
        .await;

        // Verify session was updated.
        let level = session
            .thinking_level
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(level.as_deref(), Some("low"));

        // getThinkingLevel via events should read from session.
        let outcome =
            dispatch_hostcall_events("call-2", &manager, &tools, "getThinkingLevel", json!({}))
                .await;
        let value = match outcome {
            HostcallOutcome::Success(value) => value,
            HostcallOutcome::Error { code, message } => {
                unreachable!("expected success, got error {code}: {message}");
            }
            HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            } => {
                unreachable!(
                    "expected success, got stream chunk seq={sequence} final={is_final}: {chunk}"
                );
            }
        };
        assert_eq!(
            value.get("thinkingLevel").and_then(Value::as_str),
            Some("low")
        );
    });
}

// ========================================================================
// MockHostActions for sendMessage / sendUserMessage tests
// ========================================================================

pub(super) struct MockHostActions {
    pub(super) messages: std::sync::Mutex<Vec<ExtensionSendMessage>>,
    user_messages: std::sync::Mutex<Vec<ExtensionSendUserMessage>>,
    ai_requests: std::sync::Mutex<Vec<ExtensionAiCompletionRequest>>,
    ai_models: std::sync::Mutex<Value>,
}

impl MockHostActions {
    pub(super) fn new() -> Self {
        Self {
            messages: std::sync::Mutex::new(Vec::new()),
            user_messages: std::sync::Mutex::new(Vec::new()),
            ai_requests: std::sync::Mutex::new(Vec::new()),
            ai_models: std::sync::Mutex::new(json!([
                {
                    "id": "mock-model",
                    "provider": "mock-provider",
                    "api": "mock-api"
                }
            ])),
        }
    }
}

#[async_trait]
impl ExtensionHostActions for MockHostActions {
    async fn send_message(&self, message: ExtensionSendMessage) -> Result<()> {
        self.messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(message);
        Ok(())
    }
    async fn send_user_message(&self, message: ExtensionSendUserMessage) -> Result<()> {
        self.user_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(message);
        Ok(())
    }

    async fn complete_ai(&self, request: ExtensionAiCompletionRequest) -> Result<Value> {
        self.ai_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        Ok(json!({
            "text": "mock completion",
            "provider": "mock-provider",
            "model": "mock-model"
        }))
    }

    async fn list_ai_models(&self) -> Result<Value> {
        Ok(self
            .ai_models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }
}

// ========================================================================
// sendMessage tests (bd-1rqs)
// ========================================================================

#[test]
fn events_send_message_dispatches_to_host_actions() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);
        let actions = Arc::new(MockHostActions::new());
        manager.set_host_actions(actions.clone());

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "sendMessage",
            json!({
                "message": {
                    "customType": "status-update",
                    "content": "Deployment succeeded",
                    "display": true,
                    "details": { "version": "1.2.3" }
                },
                "options": {
                    "deliverAs": "followUp",
                    "triggerTurn": true
                }
            }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Success(_)));
        {
            let msgs = actions
                .messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].custom_type, "status-update");
            assert_eq!(msgs[0].content, "Deployment succeeded");
            assert!(msgs[0].display);
            assert!(msgs[0].trigger_turn);
            assert!(msgs[0].details.is_some());
            drop(msgs);
        }
    });
}

#[test]
fn events_send_message_requires_custom_type() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);
        let actions = Arc::new(MockHostActions::new());
        manager.set_host_actions(actions.clone());

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "sendMessage",
            json!({
                "message": {
                    "content": "No type here"
                }
            }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Error { .. }));
        // No message should have been dispatched.
        assert!(
            actions
                .messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    });
}

#[test]
fn events_send_message_without_host_actions_fails() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "sendMessage",
            json!({
                "message": {
                    "customType": "test",
                    "content": "hello"
                }
            }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Error { .. }));
    });
}

// ========================================================================
// sendUserMessage tests (bd-1rqs)
// ========================================================================

#[test]
fn events_send_user_message_dispatches_to_host_actions() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);
        let actions = Arc::new(MockHostActions::new());
        manager.set_host_actions(actions.clone());

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "sendUserMessage",
            json!({
                "text": "Please review the PR",
                "options": {
                    "deliverAs": "steer"
                }
            }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Success(_)));
        {
            let msgs = actions
                .user_messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].text, "Please review the PR");
            drop(msgs);
        }
    });
}

#[test]
fn events_send_user_message_snake_case_alias_dispatches() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);
        let actions = Arc::new(MockHostActions::new());
        manager.set_host_actions(actions.clone());

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "send_user_message",
            json!({
                "text": "Please review the PR",
                "options": {
                    "deliver_as": "steer"
                }
            }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Success(_)));
        let msgs = actions
            .user_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "Please review the PR");
        drop(msgs);
    });
}

#[test]
fn events_send_user_message_empty_text_succeeds_noop() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);
        let actions = Arc::new(MockHostActions::new());
        manager.set_host_actions(actions.clone());

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "sendUserMessage",
            json!({ "text": "  " }),
        )
        .await;

        // Empty text returns Success(null) without dispatching.
        assert!(matches!(outcome, HostcallOutcome::Success(_)));
        assert!(
            actions
                .user_messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    });
}

#[test]
fn events_complete_ai_dispatches_to_host_actions() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);
        let actions = Arc::new(MockHostActions::new());
        manager.set_host_actions(actions.clone());

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "completeAi",
            json!({
                "model": { "id": "mock-model" },
                "context": [
                    { "role": "user", "content": "hello" }
                ],
                "options": { "maxTokens": 8 },
                "simple": false
            }),
        )
        .await;

        let HostcallOutcome::Success(value) = outcome else {
            panic!("expected completeAi success");
        };
        assert_eq!(value["text"], json!("mock completion"));

        let (request_len, model_id, content, simple) = {
            let requests = actions
                .ai_requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                requests.len(),
                requests.first().map(|request| request.model["id"].clone()),
                requests
                    .first()
                    .map(|request| request.context[0]["content"].clone()),
                requests.first().is_none_or(|request| request.simple),
            )
        };
        assert_eq!(request_len, 1);
        assert_eq!(model_id, Some(json!("mock-model")));
        assert_eq!(content, Some(json!("hello")));
        assert!(!simple);
    });
}

#[test]
fn events_get_models_dispatches_to_host_actions() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);
        let actions = Arc::new(MockHostActions::new());
        manager.set_host_actions(actions);

        let outcome =
            dispatch_hostcall_events("call-1", &manager, &tools, "getModels", json!({})).await;

        let HostcallOutcome::Success(value) = outcome else {
            panic!("expected getModels success");
        };
        assert_eq!(value[0]["id"], json!("mock-model"));
        assert_eq!(value[0]["provider"], json!("mock-provider"));
    });
}

#[test]
fn events_get_models_without_host_actions_fails_closed() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);

        let outcome =
            dispatch_hostcall_events("call-1", &manager, &tools, "getModels", json!({})).await;

        assert!(matches!(
            outcome,
            HostcallOutcome::Error { code, .. } if code == "denied"
        ));
    });
}

// ========================================================================
// appendEntry tests (bd-1rqs)
// ========================================================================

#[test]
fn session_append_entry_dispatches_to_session() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        let outcome = dispatch_hostcall_session(
            "call-1",
            &manager,
            "append_entry",
            json!({
                "customType": "bookmark",
                "data": { "line": 42, "file": "main.rs" }
            }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Success(_)));
    });
}

#[test]
fn session_append_entry_invalidates_ctx_cache_generation() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        let gen_before = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        let outcome = dispatch_hostcall_session(
            "call-1",
            &manager,
            "append_entry",
            json!({
                "customType": "bookmark",
                "data": { "line": 42, "file": "main.rs" }
            }),
        )
        .await;
        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        let gen_after = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        assert_eq!(gen_after, gen_before + 1);
    });
}

#[test]
fn events_append_entry_dispatches_to_session() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "appendEntry",
            json!({
                "customType": "annotation",
                "data": { "note": "refactor candidate" }
            }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Success(_)));
    });
}

#[test]
fn events_append_entry_invalidates_ctx_cache_generation() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        let gen_before = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "appendEntry",
            json!({
                "customType": "annotation",
                "data": { "note": "ctx bump expected" }
            }),
        )
        .await;
        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        let gen_after = manager
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ctx_generation;
        assert_eq!(gen_after, gen_before + 1);
    });
}

#[test]
fn events_append_entry_without_session_fails() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&["read"], Path::new("."), None);

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "appendEntry",
            json!({
                "customType": "annotation",
                "data": { "note": "test" }
            }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Error { .. }));
    });
}

#[test]
fn session_unknown_op_returns_error() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let session = Arc::new(MockSession::new());
        manager.set_session(session.clone());

        let outcome =
            dispatch_hostcall_session("call-1", &manager, "nonexistent_op", json!({})).await;

        assert!(matches!(outcome, HostcallOutcome::Error { .. }));
    });
}

// --- registerFlag hostcall tests ---

#[test]
fn register_flag_via_hostcall() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerFlag",
            json!({
                "name": "verbose",
                "description": "Enable verbose output",
                "type": "boolean",
                "default": false
            }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Success(_)));

        let flags = manager.list_flags();
        assert_eq!(flags.len(), 1);
        assert_eq!(
            flags[0].get("name").and_then(Value::as_str),
            Some("verbose")
        );
        assert_eq!(
            flags[0].get("type").and_then(Value::as_str),
            Some("boolean")
        );
    });
}

#[test]
fn register_flag_missing_name_fails() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerFlag",
            json!({ "description": "No name" }),
        )
        .await;

        assert!(matches!(outcome, HostcallOutcome::Error { .. }));
        if let HostcallOutcome::Error { code, message } = outcome {
            assert_eq!(code, "invalid_request");
            assert!(message.contains("name is required"));
        }
    });
}

#[test]
fn register_flag_dedup_last_write_wins() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerFlag",
            json!({ "name": "flag-a", "type": "string", "default": "v1" }),
        )
        .await;

        dispatch_hostcall_events(
            "call-2",
            &manager,
            &tools,
            "registerFlag",
            json!({ "name": "flag-a", "type": "string", "default": "v2" }),
        )
        .await;

        let flags = manager.list_flags();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].get("default").and_then(Value::as_str), Some("v2"));
    });
}

#[test]
fn get_flag_returns_registered_flag() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "registerFlag",
            json!({ "name": "output-dir", "type": "string", "default": "/tmp" }),
        )
        .await;

        let outcome = dispatch_hostcall_events(
            "call-2",
            &manager,
            &tools,
            "getFlag",
            json!({ "name": "output-dir" }),
        )
        .await;

        let val = match outcome {
            HostcallOutcome::Success(val) => val,
            HostcallOutcome::Error { code, message } => {
                unreachable!("expected success, got error {code}: {message}");
            }
            HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            } => {
                unreachable!(
                    "expected success, got stream chunk seq={sequence} final={is_final}: {chunk}"
                );
            }
        };
        assert_eq!(val.get("name").and_then(Value::as_str), Some("output-dir"));
        assert_eq!(val.get("type").and_then(Value::as_str), Some("string"));
    });
}

#[test]
fn get_flag_missing_name_fails() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        let outcome =
            dispatch_hostcall_events("call-1", &manager, &tools, "getFlag", json!({})).await;

        assert!(matches!(outcome, HostcallOutcome::Error { .. }));
    });
}

#[test]
fn get_flag_unknown_returns_null() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        let outcome = dispatch_hostcall_events(
            "call-1",
            &manager,
            &tools,
            "getFlag",
            json!({ "name": "nonexistent" }),
        )
        .await;

        let val = match outcome {
            HostcallOutcome::Success(val) => val,
            HostcallOutcome::Error { code, message } => {
                unreachable!("expected success with null, got error {code}: {message}");
            }
            HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            } => {
                unreachable!(
                    "expected success with null, got stream chunk seq={sequence} final={is_final}: {chunk}"
                );
            }
        };
        assert!(val.is_null());
    });
}

#[test]
fn list_flags_hostcall_returns_all() {
    asupersync::test_utils::run_test(|| async {
        let manager = ExtensionManager::new();
        let tools = crate::tools::ToolRegistry::new(&[], Path::new("."), None);

        for flag_name in ["alpha", "beta", "gamma"] {
            dispatch_hostcall_events(
                "call-1",
                &manager,
                &tools,
                "registerFlag",
                json!({ "name": flag_name, "type": "string" }),
            )
            .await;
        }

        let outcome =
            dispatch_hostcall_events("call-2", &manager, &tools, "listFlags", json!({})).await;

        let val = match outcome {
            HostcallOutcome::Success(val) => val,
            HostcallOutcome::Error { code, message } => {
                unreachable!("expected success, got error {code}: {message}");
            }
            HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            } => {
                unreachable!(
                    "expected success, got stream chunk seq={sequence} final={is_final}: {chunk}"
                );
            }
        };
        let arr = val.as_array().expect("expected array");
        assert_eq!(arr.len(), 3);
    });
}

// --- provider_has_stream_simple tests ---

#[test]
fn provider_has_stream_simple_detects_flag() {
    let manager = ExtensionManager::new();
    manager.register_provider(json!({
        "id": "custom-provider",
        "api": "openai-completions",
        "hasStreamSimple": true,
    }));

    assert!(manager.provider_has_stream_simple("custom-provider"));
    assert!(!manager.provider_has_stream_simple("nonexistent"));
}

#[test]
fn provider_has_stream_simple_false_when_not_set() {
    let manager = ExtensionManager::new();
    manager.register_provider(json!({
        "id": "standard-provider",
        "api": "openai-completions",
    }));

    assert!(!manager.provider_has_stream_simple("standard-provider"));
}

#[test]
fn provider_has_stream_simple_empty_id_returns_false() {
    let manager = ExtensionManager::new();
    manager.register_provider(json!({
        "id": "custom-provider",
        "api": "openai-completions",
        "hasStreamSimple": true,
    }));

    assert!(!manager.provider_has_stream_simple(""));
    assert!(!manager.provider_has_stream_simple("  "));
}

#[test]
fn issued_provider_stream_ids_require_canonical_in_range_sequence() {
    let shards = JsRuntimeShardSet {
        next_provider_stream_id: 3,
        ..JsRuntimeShardSet::default()
    };

    assert!(shards.provider_stream_id_was_issued("provider-stream-1"));
    assert!(shards.provider_stream_id_was_issued("provider-stream-3"));
    assert!(!shards.provider_stream_id_was_issued("provider-stream-0"));
    assert!(!shards.provider_stream_id_was_issued("provider-stream-01"));
    assert!(!shards.provider_stream_id_was_issued("provider-stream-4"));
    assert!(!shards.provider_stream_id_was_issued("unrelated-stream-1"));
}

// --- streamSimple JS runtime integration tests ---

#[test]
fn stream_simple_yields_chunks_in_order() {
    let manager = ExtensionManager::new();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");

    runtime.block_on(async move {
        let dir = tempdir().expect("tempdir");
        let entry_path = dir.path().join("ext.mjs");
        std::fs::write(
            &entry_path,
            r#"
                export default function init(pi) {
                    pi.registerProvider("stream-test", {
                        api: "openai-completions",
                        baseUrl: "https://not-used.example.com",
                        models: [{ id: "test-model", name: "Test Model" }],
                        streamSimple: async function*(model, context, options) {
                            yield "Hello";
                            yield " ";
                            yield "World";
                        }
                    });
                }
                "#,
        )
        .expect("write extension entry");

        let tools = Arc::new(crate::tools::ToolRegistry::new(&[], dir.path(), None));
        let js_runtime = JsExtensionRuntimeHandle::start(
            PiJsRuntimeConfig {
                cwd: dir.path().display().to_string(),
                ..Default::default()
            },
            Arc::clone(&tools),
            manager.clone(),
        )
        .await
        .expect("start js runtime");
        manager.set_js_runtime(js_runtime.clone());

        let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("load spec");
        manager
            .load_js_extensions(vec![spec])
            .await
            .expect("load extension");

        assert!(manager.provider_has_stream_simple("stream-test"));

        let stream_id = js_runtime
            .provider_stream_simple_start(
                "stream-test".to_string(),
                json!({"id": "test-model"}),
                json!({"messages": []}),
                json!({}),
                30_000,
            )
            .await
            .expect("start stream");

        let mut chunks = Vec::new();
        while let Some(val) = js_runtime
            .provider_stream_simple_next(stream_id.clone(), 30_000)
            .await
            .expect("next")
        {
            chunks.push(val.as_str().unwrap_or_default().to_string());
        }

        assert_eq!(chunks, vec!["Hello", " ", "World"]);
    });
}

#[test]
fn stream_simple_error_in_js_propagates() {
    let manager = ExtensionManager::new();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");

    runtime.block_on(async move {
        let dir = tempdir().expect("tempdir");
        let entry_path = dir.path().join("ext.mjs");
        std::fs::write(
            &entry_path,
            r#"
                export default function init(pi) {
                    pi.registerProvider("error-provider", {
                        api: "openai-completions",
                        baseUrl: "https://not-used.example.com",
                        models: [{ id: "err-model", name: "Error Model" }],
                        streamSimple: async function*(model, context, options) {
                            yield "partial";
                            throw new Error("stream explosion");
                        }
                    });
                }
                "#,
        )
        .expect("write extension entry");

        let tools = Arc::new(crate::tools::ToolRegistry::new(&[], dir.path(), None));
        let js_runtime = JsExtensionRuntimeHandle::start(
            PiJsRuntimeConfig {
                cwd: dir.path().display().to_string(),
                ..Default::default()
            },
            Arc::clone(&tools),
            manager.clone(),
        )
        .await
        .expect("start js runtime");
        manager.set_js_runtime(js_runtime.clone());

        let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("load spec");
        manager
            .load_js_extensions(vec![spec])
            .await
            .expect("load extension");

        let stream_id = js_runtime
            .provider_stream_simple_start(
                "error-provider".to_string(),
                json!({"id": "err-model"}),
                json!({"messages": []}),
                json!({}),
                30_000,
            )
            .await
            .expect("start stream");

        // First chunk should succeed.
        let first = js_runtime
            .provider_stream_simple_next(stream_id.clone(), 30_000)
            .await
            .expect("first next");
        assert!(first.is_some());
        assert_eq!(first.unwrap().as_str().unwrap_or_default(), "partial");

        // Second call should fail with the JS error.
        let result = js_runtime
            .provider_stream_simple_next(stream_id.clone(), 30_000)
            .await;
        assert!(result.is_err(), "expected error from JS throw");
    });
}

#[test]
fn stream_simple_cancel_stops_iteration() {
    let manager = ExtensionManager::new();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");

    runtime.block_on(async move {
        let dir = tempdir().expect("tempdir");
        let entry_path = dir.path().join("ext.mjs");
        std::fs::write(
            &entry_path,
            r#"
                export default function init(pi) {
                    pi.registerProvider("cancel-provider", {
                        api: "openai-completions",
                        baseUrl: "https://not-used.example.com",
                        models: [{ id: "cancel-model", name: "Cancel Model" }],
                        streamSimple: async function*(model, context, options) {
                            yield "chunk-1";
                            yield "chunk-2";
                            yield "chunk-3";
                            yield "chunk-4";
                        }
                    });
                }
                "#,
        )
        .expect("write extension entry");

        let tools = Arc::new(crate::tools::ToolRegistry::new(&[], dir.path(), None));
        let js_runtime = JsExtensionRuntimeHandle::start(
            PiJsRuntimeConfig {
                cwd: dir.path().display().to_string(),
                ..Default::default()
            },
            Arc::clone(&tools),
            manager.clone(),
        )
        .await
        .expect("start js runtime");
        manager.set_js_runtime(js_runtime.clone());

        let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("load spec");
        manager
            .load_js_extensions(vec![spec])
            .await
            .expect("load extension");

        let stream_id = js_runtime
            .provider_stream_simple_start(
                "cancel-provider".to_string(),
                json!({"id": "cancel-model"}),
                json!({"messages": []}),
                json!({}),
                30_000,
            )
            .await
            .expect("start stream");

        // Read first chunk.
        let first = js_runtime
            .provider_stream_simple_next(stream_id.clone(), 30_000)
            .await
            .expect("first next");
        assert!(first.is_some());

        // Cancel the stream.
        js_runtime
            .provider_stream_simple_cancel(stream_id.clone(), 30_000)
            .await
            .expect("cancel");

        // After cancel, next should return done.
        let after_cancel = js_runtime
            .provider_stream_simple_next(stream_id, 30_000)
            .await
            .expect("next after cancel");
        assert!(after_cancel.is_none(), "expected None after cancellation");

        let unknown = js_runtime
            .provider_stream_simple_next("provider-stream-999".to_string(), 30_000)
            .await
            .expect_err("never-issued stream id should remain an error");
        assert!(
            unknown
                .to_string()
                .contains("Unknown extension provider stream: provider-stream-999"),
            "unexpected unknown-stream error: {unknown}"
        );
    });
}

#[test]
fn isolated_runtime_cold_reload_calls_return_on_active_provider_iterators() {
    let manager = ExtensionManager::new();
    let actions = Arc::new(MockHostActions::new());
    manager.set_host_actions(actions.clone());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");

    runtime.block_on(async move {
            let dir = tempdir().expect("tempdir");
            let old_dir = dir.path().join("old-provider");
            let new_dir = dir.path().join("replacement");
            std::fs::create_dir_all(&old_dir).expect("mkdir old provider");
            std::fs::create_dir_all(&new_dir).expect("mkdir replacement");
            let old_entry = old_dir.join("index.mjs");
            std::fs::write(
                &old_entry,
                r#"
                    export default function init(pi) {
                      pi.registerProvider("reload-cleanup-provider", {
                        api: "openai-completions",
                        baseUrl: "https://not-used.example.com",
                        models: [{ id: "cleanup-model", name: "Cleanup Model" }],
                        streamSimple: async function*() {
                          try {
                            yield "first";
                            yield "second";
                          } finally {
                            pi.sendMessage({ customType: "provider-return", content: "return-called" });
                          }
                        },
                      });
                    }
                    "#
            )
            .expect("write old provider extension");
            let new_entry = new_dir.join("index.mjs");
            std::fs::write(&new_entry, "export default function init(_pi) {}")
                .expect("write replacement extension");

            let tools = Arc::new(ToolRegistry::new(&[], dir.path(), None));
            let js_runtime = JsExtensionRuntimeHandle::start(
                PiJsRuntimeConfig {
                    cwd: dir.path().display().to_string(),
                    ..Default::default()
                },
                Arc::clone(&tools),
                manager.clone(),
            )
            .await
            .expect("start js runtime");
            manager.set_js_runtime(js_runtime.clone());

            let old_spec = JsExtensionLoadSpec::from_entry_path(&old_entry).expect("old spec");
            manager
                .load_js_extensions(vec![old_spec])
                .await
                .expect("load old provider");
            let stream_id = js_runtime
                .provider_stream_simple_start(
                    "reload-cleanup-provider".to_string(),
                    json!({"id": "cleanup-model"}),
                    json!({"messages": []}),
                    json!({}),
                    30_000,
                )
                .await
                .expect("start active provider stream");
            assert_eq!(
                js_runtime
                    .provider_stream_simple_next(stream_id, 30_000)
                    .await
                    .expect("first provider chunk"),
                Some(Value::String("first".to_string()))
            );
            assert!(
                actions
                    .messages
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_empty(),
                "iterator must remain active before reload"
            );

            let new_spec = JsExtensionLoadSpec::from_entry_path(&new_entry).expect("new spec");
            manager
                .load_js_extensions(vec![new_spec])
                .await
                .expect("cold replacement load");
            {
                let messages = actions
                    .messages
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                assert_eq!(messages.len(), 1, "reload must invoke iterator.return()");
                assert_eq!(messages[0].custom_type, "provider-return");
                assert_eq!(messages[0].content, "return-called");
            }

            assert!(manager.shutdown(Duration::from_secs(3)).await);
        });
}

#[test]
fn isolated_runtime_provider_collision_cancels_inner_and_quarantines_shard() {
    let manager = ExtensionManager::new();
    let actions = Arc::new(MockHostActions::new());
    manager.set_host_actions(actions.clone());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");

    runtime.block_on(async move {
        let dir = tempdir().expect("tempdir");
        let (provider_entry, command_entry) = create_provider_collision_fixture(dir.path());

        let tools = Arc::new(ToolRegistry::new(&[], dir.path(), None));
        let js_runtime = JsExtensionRuntimeHandle::start(
            PiJsRuntimeConfig {
                cwd: dir.path().display().to_string(),
                ..Default::default()
            },
            Arc::clone(&tools),
            manager.clone(),
        )
        .await
        .expect("start js runtime");
        manager.set_js_runtime(js_runtime.clone());
        let provider_spec =
            JsExtensionLoadSpec::from_entry_path(&provider_entry).expect("provider extension spec");
        let command_spec =
            JsExtensionLoadSpec::from_entry_path(&command_entry).expect("command extension spec");
        manager
            .load_js_extensions(vec![provider_spec, command_spec])
            .await
            .expect("load collision scenario");

        let err = js_runtime
            .provider_stream_simple_start(
                "collision-provider".to_string(),
                json!({"id": "collision-model"}),
                json!({"messages": []}),
                json!({}),
                5_000,
            )
            .await
            .expect_err("dynamic cross-shard route collision must reject start");
        assert!(
            err.to_string().contains("quarantined")
                && err.to_string().contains("command name collision"),
            "unexpected collision error: {err}"
        );
        {
            let messages = actions
                .messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(messages.len(), 1, "collision must invoke iterator.return()");
            assert_eq!(messages[0].custom_type, "provider-return");
            assert_eq!(messages[0].content, "return-called");
        }

        let retry = js_runtime
            .provider_stream_simple_start(
                "collision-provider".to_string(),
                json!({"id": "collision-model"}),
                json!({"messages": []}),
                json!({}),
                5_000,
            )
            .await
            .expect_err("quarantined provider shard must reject before invocation");
        assert!(retry.to_string().contains("quarantined"));

        assert!(manager.shutdown(Duration::from_secs(3)).await);
    });
}
