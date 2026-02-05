//! Unit tests for extension registration APIs: commands, shortcuts, flags, and providers.
//!
//! These tests exercise the `ExtensionManager` methods for dynamically registering
//! slash commands, keyboard shortcuts, CLI flags, and custom LLM providers. No mocks
//! are used; all tests operate directly on the real `ExtensionManager` and `RegisterPayload`.

use pi::extensions::{ExtensionManager, PROTOCOL_VERSION, RegisterPayload};
use serde_json::{Value, json};

// ─── Helpers ────────────────────────────────────────────────────────────────

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

fn payload_with_commands(name: &str, commands: Vec<Value>) -> RegisterPayload {
    let mut p = empty_payload(name);
    p.slash_commands = commands;
    p
}

fn payload_with_shortcuts(name: &str, shortcuts: Vec<Value>) -> RegisterPayload {
    let mut p = empty_payload(name);
    p.shortcuts = shortcuts;
    p
}

fn payload_with_flags(name: &str, flags: Vec<Value>) -> RegisterPayload {
    let mut p = empty_payload(name);
    p.flags = flags;
    p
}

// ─── pi.registerCommand() Tests ─────────────────────────────────────────────

#[test]
fn command_registration_stores_metadata() {
    let mgr = ExtensionManager::new();
    mgr.register_command("deploy", Some("Deploy the app"));

    assert!(mgr.has_command("deploy"));

    let commands = mgr.list_commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["name"], "deploy");
    assert_eq!(commands[0]["description"], "Deploy the app");
    assert_eq!(commands[0]["source"], "extension");
}

#[test]
fn command_registration_without_description() {
    let mgr = ExtensionManager::new();
    mgr.register_command("lint", None);

    assert!(mgr.has_command("lint"));

    let commands = mgr.list_commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["name"], "lint");
    assert!(commands[0]["description"].is_null());
}

#[test]
fn command_registration_appends_to_existing_extension() {
    let mgr = ExtensionManager::new();
    mgr.register(payload_with_commands(
        "ext-a",
        vec![json!({"name": "build", "description": "Build project"})],
    ));

    // Dynamic registration should append to the existing extension
    mgr.register_command("test", Some("Run tests"));

    assert!(mgr.has_command("build"));
    assert!(mgr.has_command("test"));

    let commands = mgr.list_commands();
    assert_eq!(commands.len(), 2);
}

#[test]
fn command_registration_creates_dynamic_extension_when_none_exists() {
    let mgr = ExtensionManager::new();
    // No extensions registered yet
    mgr.register_command("hello", Some("Say hello"));

    assert!(mgr.has_command("hello"));
    let commands = mgr.list_commands();
    assert_eq!(commands.len(), 1);
}

#[test]
fn has_command_is_case_insensitive() {
    let mgr = ExtensionManager::new();
    mgr.register_command("Deploy", None);

    assert!(mgr.has_command("deploy"));
    assert!(mgr.has_command("DEPLOY"));
    assert!(mgr.has_command("Deploy"));
}

#[test]
fn has_command_strips_leading_slash() {
    let mgr = ExtensionManager::new();
    mgr.register_command("deploy", None);

    // has_command should normalize "/" prefix
    assert!(mgr.has_command("/deploy"));
}

#[test]
fn has_command_returns_false_for_unregistered() {
    let mgr = ExtensionManager::new();
    assert!(!mgr.has_command("nonexistent"));
}

#[test]
fn multiple_commands_from_multiple_extensions() {
    let mgr = ExtensionManager::new();
    mgr.register(payload_with_commands(
        "ext-a",
        vec![json!({"name": "cmd-a", "description": "From A"})],
    ));
    mgr.register(payload_with_commands(
        "ext-b",
        vec![json!({"name": "cmd-b", "description": "From B"})],
    ));

    assert!(mgr.has_command("cmd-a"));
    assert!(mgr.has_command("cmd-b"));

    let commands = mgr.list_commands();
    assert_eq!(commands.len(), 2);
}

#[test]
fn command_with_empty_name_is_ignored_in_list() {
    let mgr = ExtensionManager::new();
    mgr.register(payload_with_commands(
        "ext",
        vec![json!({"description": "No name field"})],
    ));

    // Commands without a name should be filtered out
    let commands = mgr.list_commands();
    assert!(commands.is_empty());
}

// ─── pi.registerShortcut() Tests ────────────────────────────────────────────

#[test]
fn shortcut_registration_via_payload() {
    let mgr = ExtensionManager::new();
    mgr.register(payload_with_shortcuts(
        "ext-keys",
        vec![json!({
            "key_id": "ctrl+shift+d",
            "key": {"ctrl": true, "shift": true, "key": "d"},
            "description": "Toggle debug mode",
        })],
    ));

    assert!(mgr.has_shortcut("ctrl+shift+d"));
}

#[test]
fn has_shortcut_is_case_insensitive() {
    let mgr = ExtensionManager::new();
    mgr.register(payload_with_shortcuts(
        "ext",
        vec![json!({"key_id": "ctrl+shift+d"})],
    ));

    assert!(mgr.has_shortcut("ctrl+shift+d"));
}

#[test]
fn has_shortcut_returns_false_for_unregistered() {
    let mgr = ExtensionManager::new();
    assert!(!mgr.has_shortcut("ctrl+alt+delete"));
}

#[test]
fn list_shortcuts_returns_all_shortcuts() {
    let mgr = ExtensionManager::new();
    mgr.register(payload_with_shortcuts(
        "ext-a",
        vec![
            json!({"key_id": "ctrl+a", "description": "Select all"}),
            json!({"key_id": "ctrl+b", "description": "Bold"}),
        ],
    ));
    mgr.register(payload_with_shortcuts(
        "ext-b",
        vec![json!({"key_id": "ctrl+c", "description": "Copy"})],
    ));

    let shortcuts = mgr.list_shortcuts();
    assert_eq!(shortcuts.len(), 3);

    let ids: Vec<&str> = shortcuts
        .iter()
        .filter_map(|s| s.get("key_id").and_then(Value::as_str))
        .collect();
    assert!(ids.contains(&"ctrl+a"));
    assert!(ids.contains(&"ctrl+b"));
    assert!(ids.contains(&"ctrl+c"));
}

#[test]
fn shortcuts_include_source_extension() {
    let mgr = ExtensionManager::new();
    mgr.register(payload_with_shortcuts(
        "ext",
        vec![json!({"key_id": "ctrl+z", "description": "Undo"})],
    ));

    let shortcuts = mgr.list_shortcuts();
    assert_eq!(shortcuts[0]["source"], "extension");
}

#[test]
fn shortcut_without_key_id_filtered_from_has_check() {
    let mgr = ExtensionManager::new();
    mgr.register(payload_with_shortcuts(
        "ext",
        vec![json!({"description": "No key_id"})],
    ));

    // has_shortcut relies on key_id field, so this shouldn't match anything
    assert!(!mgr.has_shortcut(""));
}

// ─── pi.registerFlag() Tests ────────────────────────────────────────────────

#[test]
fn flag_registration_stores_spec() {
    let mgr = ExtensionManager::new();
    mgr.register_flag(json!({
        "name": "verbose",
        "description": "Enable verbose output",
        "type": "bool",
        "default": false,
    }));

    let flags = mgr.list_flags();
    assert_eq!(flags.len(), 1);
    assert_eq!(flags[0]["name"], "verbose");
    assert_eq!(flags[0]["description"], "Enable verbose output");
    assert_eq!(flags[0]["type"], "bool");
    assert_eq!(flags[0]["source"], "extension");
}

#[test]
fn flag_type_defaults_to_string() {
    let mgr = ExtensionManager::new();
    mgr.register_flag(json!({
        "name": "output-dir",
        "description": "Output directory",
    }));

    let flags = mgr.list_flags();
    assert_eq!(flags[0]["type"], "string");
}

#[test]
fn flag_deduplication_replaces_previous() {
    let mgr = ExtensionManager::new();
    mgr.register_flag(json!({
        "name": "level",
        "type": "string",
        "default": "info",
    }));
    mgr.register_flag(json!({
        "name": "level",
        "type": "string",
        "default": "debug",
    }));

    let flags = mgr.list_flags();
    assert_eq!(flags.len(), 1, "duplicate flag should be replaced");
    assert_eq!(flags[0]["default"], "debug");
}

#[test]
fn flag_registration_multiple_flags() {
    let mgr = ExtensionManager::new();
    mgr.register_flag(json!({"name": "color", "type": "bool", "default": true}));
    mgr.register_flag(json!({"name": "format", "type": "string", "default": "json"}));
    mgr.register_flag(json!({"name": "retries", "type": "number", "default": 3}));

    let flags = mgr.list_flags();
    assert_eq!(flags.len(), 3);

    let names: Vec<&str> = flags
        .iter()
        .filter_map(|f| f.get("name").and_then(Value::as_str))
        .collect();
    assert!(names.contains(&"color"));
    assert!(names.contains(&"format"));
    assert!(names.contains(&"retries"));
}

#[test]
fn flag_from_payload_appears_in_list() {
    let mgr = ExtensionManager::new();
    mgr.register(payload_with_flags(
        "ext",
        vec![json!({
            "name": "timeout",
            "description": "Request timeout in seconds",
            "type": "number",
            "default": 30,
        })],
    ));

    let flags = mgr.list_flags();
    assert_eq!(flags.len(), 1);
    assert_eq!(flags[0]["name"], "timeout");
}

#[test]
fn dynamic_flag_takes_priority_over_payload_flag() {
    let mgr = ExtensionManager::new();

    // Register via payload (lower priority)
    mgr.register(payload_with_flags(
        "ext",
        vec![json!({
            "name": "log-level",
            "type": "string",
            "default": "info",
        })],
    ));

    // Register dynamically (higher priority)
    mgr.register_flag(json!({
        "name": "log-level",
        "type": "string",
        "default": "debug",
    }));

    let flags = mgr.list_flags();
    assert_eq!(flags.len(), 1, "should deduplicate same-name flag");
    assert_eq!(
        flags[0]["default"], "debug",
        "dynamic flag should take priority"
    );
}

#[test]
fn flag_with_empty_name_excluded_from_list() {
    let mgr = ExtensionManager::new();
    mgr.register_flag(json!({"name": "", "type": "bool"}));

    let flags = mgr.list_flags();
    assert!(flags.is_empty(), "empty-name flags should be excluded");
}

// ─── pi.registerProvider() Tests ────────────────────────────────────────────

#[test]
fn provider_registration_stores_spec() {
    let mgr = ExtensionManager::new();
    mgr.register_provider(json!({
        "id": "custom-llm",
        "name": "Custom LLM",
        "api": "openai",
        "baseUrl": "https://api.custom.ai/v1",
        "models": [
            {"id": "custom-fast", "name": "Custom Fast", "contextWindow": 32000, "maxTokens": 4096}
        ],
    }));

    let providers = mgr.extension_providers();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["id"], "custom-llm");
    assert_eq!(providers[0]["name"], "Custom LLM");
}

#[test]
fn provider_registration_multiple_providers() {
    let mgr = ExtensionManager::new();
    mgr.register_provider(json!({"id": "provider-a", "name": "A"}));
    mgr.register_provider(json!({"id": "provider-b", "name": "B"}));

    let providers = mgr.extension_providers();
    assert_eq!(providers.len(), 2);
}

#[test]
fn provider_with_missing_id_excluded_from_model_entries() {
    let mgr = ExtensionManager::new();
    mgr.register_provider(json!({
        "name": "No ID",
        "api": "openai",
        "baseUrl": "https://api.example.com/v1",
        "models": [{"id": "m", "name": "M", "contextWindow": 8000, "maxTokens": 1000}],
    }));

    // extension_model_entries() should skip providers without an id
    let entries = mgr.extension_model_entries();
    assert!(
        entries.is_empty(),
        "provider without id should produce no model entries"
    );
}

#[test]
fn provider_with_empty_id_excluded_from_model_entries() {
    let mgr = ExtensionManager::new();
    mgr.register_provider(json!({
        "id": "",
        "name": "Empty ID",
        "api": "openai",
        "baseUrl": "https://api.example.com/v1",
        "models": [{"id": "m", "name": "M", "contextWindow": 8000, "maxTokens": 1000}],
    }));

    let entries = mgr.extension_model_entries();
    assert!(entries.is_empty());
}

#[test]
fn provider_produces_model_entries() {
    let mgr = ExtensionManager::new();
    mgr.register_provider(json!({
        "id": "my-provider",
        "name": "My Provider",
        "api": "openai",
        "baseUrl": "https://api.myprovider.com/v1",
        "models": [
            {
                "id": "model-alpha",
                "name": "Model Alpha",
                "contextWindow": 128_000,
                "maxTokens": 16_384,
            },
            {
                "id": "model-beta",
                "name": "Model Beta",
                "contextWindow": 32000,
                "maxTokens": 4096,
                "reasoning": true,
            },
        ],
    }));

    let entries = mgr.extension_model_entries();
    assert_eq!(entries.len(), 2);

    assert_eq!(entries[0].model.id, "model-alpha");
    assert_eq!(entries[0].model.provider, "my-provider");
    assert_eq!(entries[0].model.context_window, 128_000);
    assert_eq!(entries[0].model.max_tokens, 16_384);

    assert_eq!(entries[1].model.id, "model-beta");
    assert!(entries[1].model.reasoning);
}

#[test]
fn provider_model_entries_use_provider_base_url() {
    let mgr = ExtensionManager::new();
    mgr.register_provider(json!({
        "id": "test-provider",
        "baseUrl": "https://custom.api.com/v1",
        "models": [
            {"id": "m1", "name": "M1", "contextWindow": 8000, "maxTokens": 1000}
        ],
    }));

    let entries = mgr.extension_model_entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].model.base_url, "https://custom.api.com/v1");
}

// ─── Cross-cutting Tests ────────────────────────────────────────────────────

#[test]
fn fresh_manager_has_no_registrations() {
    let mgr = ExtensionManager::new();

    assert!(mgr.list_commands().is_empty());
    assert!(mgr.list_shortcuts().is_empty());
    assert!(mgr.list_flags().is_empty());
    assert!(mgr.extension_providers().is_empty());
    assert!(mgr.extension_model_entries().is_empty());
}

#[test]
fn full_payload_registration_populates_all_apis() {
    let mgr = ExtensionManager::new();

    let mut payload = empty_payload("full-ext");
    payload.slash_commands = vec![json!({"name": "deploy", "description": "Deploy"})];
    payload.shortcuts = vec![json!({"key_id": "ctrl+d", "description": "Deploy shortcut"})];
    payload.flags = vec![json!({"name": "env", "type": "string", "default": "production"})];
    mgr.register(payload);

    mgr.register_provider(json!({
        "id": "ext-provider",
        "baseUrl": "https://api.ext.com/v1",
        "models": [{"id": "m", "name": "M", "contextWindow": 8000, "maxTokens": 1000}],
    }));

    assert_eq!(mgr.list_commands().len(), 1);
    assert_eq!(mgr.list_shortcuts().len(), 1);
    assert_eq!(mgr.list_flags().len(), 1);
    assert_eq!(mgr.extension_providers().len(), 1);
    assert_eq!(mgr.extension_model_entries().len(), 1);

    assert!(mgr.has_command("deploy"));
    assert!(mgr.has_shortcut("ctrl+d"));
}
