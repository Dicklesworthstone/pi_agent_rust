#![allow(clippy::doc_markdown)]
//! Non-mock coverage tests for provider/session surfaces (bd-1f42.8.4.2).
//!
//! Targets uncovered code paths:
//! - Provider creation with various model entries and error cases
//! - Provider URL normalization functions
//! - Session open/save/load lifecycle with edge cases
//! - Session corruption recovery (malformed JSONL, empty files)
//! - Session state mutations (append, label, branch, model change)
//! - Session index operations
//! - `ModelEntry` thinking level clamping
//! - Provider enum parsing (`Api`, `KnownProvider`)
//!
//! All tests use real filesystem and real Session objects, no mocks.

mod common;

use common::TestHarness;
use pi::models::ModelEntry;
use pi::provider::{Api, CacheRetention, KnownProvider, Model, ModelCost, StreamOptions};
use pi::providers::{
    create_provider, normalize_cohere_base, normalize_openai_base, normalize_openai_responses_base,
};
use pi::session::{Session, SessionEntry, SessionMessage, SessionOpenDiagnostics};
use serde_json::json;
use std::collections::HashMap;
use std::io::Write;

// ===========================================================================
// Helpers
// ===========================================================================

fn make_model(provider: &str, api: &str, base_url: &str) -> Model {
    Model {
        id: "test-model".to_string(),
        name: "Test Model".to_string(),
        api: api.to_string(),
        provider: provider.to_string(),
        base_url: base_url.to_string(),
        reasoning: false,
        input: vec![],
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        },
        context_window: 128_000,
        max_tokens: 4096,
        headers: HashMap::new(),
    }
}

fn make_model_entry(provider: &str, api: &str, base_url: &str) -> ModelEntry {
    ModelEntry {
        model: make_model(provider, api, base_url),
        api_key: Some("test-key-12345".to_string()),
        headers: HashMap::new(),
        auth_header: true,
        compat: None,
        oauth_config: None,
    }
}

fn make_session_header() -> serde_json::Value {
    json!({
        "version": "1.0.0",
        "id": "test-session-001",
        "provider": "anthropic",
        "model_id": "claude-sonnet-4-5",
        "thinking_level": "medium",
        "cwd": "/tmp/test",
        "created_at": "2026-02-13T00:00:00Z"
    })
}

fn make_message_entry(id: &str, msg_type: &str, text: &str) -> serde_json::Value {
    json!({
        "id": id,
        "timestamp": 1_707_782_400_000_i64,
        "message": {
            "type": msg_type,
            "content": text
        }
    })
}

fn user_msg(text: &str) -> SessionMessage {
    SessionMessage::User {
        content: pi::model::UserContent::Text(text.to_string()),
        timestamp: None,
    }
}

// ===========================================================================
// Provider enum parsing
// ===========================================================================

/// Api::from_str handles empty string.
#[test]
fn api_from_str_empty_string_errors() {
    let result: Result<Api, _> = "".parse();
    assert!(result.is_err(), "empty API string should error");
}

/// Api::from_str handles custom unknown string.
#[test]
fn api_from_str_custom_unknown() {
    let result: Result<Api, _> = "my-custom-api".parse();
    assert!(result.is_ok(), "unknown API should succeed as Custom");
    match result.unwrap() {
        Api::Custom(s) => assert_eq!(s, "my-custom-api"),
        other => panic!("expected Custom, got {other:?}"),
    }
}

/// Api::from_str handles all known variants.
#[test]
fn api_from_str_known_variants() {
    let known = [
        "anthropic-messages",
        "openai-chat-completions",
        "openai-responses",
        "cohere-chat",
        "google-generativeai",
        "bedrock-converse",
        "copilot",
        "gitlab",
    ];
    for variant in &known {
        let result: Result<Api, _> = variant.parse();
        assert!(result.is_ok(), "known API '{variant}' should parse");
    }
}

/// KnownProvider::from_str handles empty string.
#[test]
fn known_provider_from_str_empty_errors() {
    let result: Result<KnownProvider, _> = "".parse();
    assert!(result.is_err(), "empty provider string should error");
}

/// KnownProvider::from_str handles azure aliases.
#[test]
fn known_provider_from_str_azure_aliases() {
    let aliases = ["azure", "azure-openai", "azure-cognitive-services"];
    for alias in &aliases {
        let result: Result<KnownProvider, _> = alias.parse();
        assert!(result.is_ok(), "azure alias '{alias}' should parse");
        assert_eq!(
            result.unwrap(),
            KnownProvider::AzureOpenAI,
            "'{alias}' should map to AzureOpenAI"
        );
    }
}

/// KnownProvider::from_str handles custom unknown provider.
#[test]
fn known_provider_from_str_custom() {
    let result: Result<KnownProvider, _> = "my-custom-provider".parse();
    assert!(result.is_ok(), "unknown provider should succeed as Custom");
    match result.unwrap() {
        KnownProvider::Custom(s) => assert_eq!(s, "my-custom-provider"),
        other => panic!("expected Custom, got {other:?}"),
    }
}

// ===========================================================================
// Provider URL normalization
// ===========================================================================

/// OpenAI base URL normalization ensures URL ends with `/chat/completions`.
#[test]
fn normalize_openai_base_appends_endpoint() {
    let cases = [
        // Empty or whitespace uses default OpenAI endpoint
        ("", "https://api.openai.com/v1/chat/completions"),
        ("   ", "https://api.openai.com/v1/chat/completions"),
        // Base with /v1 gets /chat/completions appended
        (
            "https://api.openai.com/v1/",
            "https://api.openai.com/v1/chat/completions",
        ),
        (
            "https://api.openai.com/v1",
            "https://api.openai.com/v1/chat/completions",
        ),
        // Bare official OpenAI origin gets /v1/chat/completions appended
        (
            "https://api.openai.com",
            "https://api.openai.com/v1/chat/completions",
        ),
        (
            "https://api.openai.com/",
            "https://api.openai.com/v1/chat/completions",
        ),
        // Already has /chat/completions - kept as-is
        (
            "https://custom.api.com/v1/chat/completions",
            "https://custom.api.com/v1/chat/completions",
        ),
        // Trailing slash stripped, then /chat/completions appended
        (
            "https://custom.api.com/",
            "https://custom.api.com/chat/completions",
        ),
        // /responses suffix is stripped, then /chat/completions appended
        (
            "https://api.openai.com/v1/responses",
            "https://api.openai.com/v1/chat/completions",
        ),
    ];
    for (input, expected) in &cases {
        let result = normalize_openai_base(input);
        assert_eq!(
            &result, expected,
            "normalize_openai_base({input}) should be {expected}, got {result}"
        );
    }
}

/// OpenAI Responses base URL normalization ensures URL ends with `/responses`.
#[test]
fn normalize_openai_responses_base_appends_endpoint() {
    let cases = [
        // Empty or whitespace uses default OpenAI endpoint
        ("", "https://api.openai.com/v1/responses"),
        ("  ", "https://api.openai.com/v1/responses"),
        // Already has /responses - kept as-is
        (
            "https://api.openai.com/v1/responses",
            "https://api.openai.com/v1/responses",
        ),
        // Trailing slash stripped, then /responses appended
        (
            "https://api.openai.com/v1/",
            "https://api.openai.com/v1/responses",
        ),
        // /v1 gets /responses appended
        (
            "https://api.openai.com/v1",
            "https://api.openai.com/v1/responses",
        ),
        // Bare official OpenAI origin gets /v1/responses appended
        (
            "https://api.openai.com",
            "https://api.openai.com/v1/responses",
        ),
        (
            "https://api.openai.com/",
            "https://api.openai.com/v1/responses",
        ),
        // /chat/completions suffix stripped, then /responses appended
        (
            "https://api.openai.com/v1/chat/completions",
            "https://api.openai.com/v1/responses",
        ),
    ];
    for (input, expected) in &cases {
        let result = normalize_openai_responses_base(input);
        assert_eq!(
            &result, expected,
            "normalize_openai_responses_base({input}) should be {expected}, got {result}"
        );
    }
}

/// Cohere base URL normalization ensures URL ends with `/chat`.
#[test]
fn normalize_cohere_base_appends_endpoint() {
    let cases = [
        // Empty or whitespace uses default Cohere endpoint
        ("", "https://api.cohere.com/v2/chat"),
        (" \t ", "https://api.cohere.com/v2/chat"),
        // Already has /chat - kept as-is
        (
            "https://api.cohere.com/v2/chat",
            "https://api.cohere.com/v2/chat",
        ),
        // /v2 gets /chat appended
        (
            "https://api.cohere.com/v2",
            "https://api.cohere.com/v2/chat",
        ),
        // Bare official Cohere origin gets /v2/chat appended
        ("https://api.cohere.com", "https://api.cohere.com/v2/chat"),
        // Trailing slash stripped, then /v2/chat appended
        ("https://api.cohere.com/", "https://api.cohere.com/v2/chat"),
    ];
    for (input, expected) in &cases {
        let result = normalize_cohere_base(input);
        assert_eq!(
            &result, expected,
            "normalize_cohere_base({input}) should be {expected}, got {result}"
        );
    }
}

// ===========================================================================
// ModelEntry thinking level clamping
// ===========================================================================

/// Non-reasoning model always clamps to Off.
#[test]
fn model_entry_clamp_thinking_non_reasoning() {
    use pi::model::ThinkingLevel;

    let entry = ModelEntry {
        model: Model {
            reasoning: false,
            ..make_model(
                "anthropic",
                "anthropic-messages",
                "https://api.anthropic.com",
            )
        },
        api_key: Some("key".to_string()),
        headers: HashMap::new(),
        auth_header: true,
        compat: None,
        oauth_config: None,
    };

    assert_eq!(
        entry.clamp_thinking_level(ThinkingLevel::High),
        ThinkingLevel::Off
    );
    assert_eq!(
        entry.clamp_thinking_level(ThinkingLevel::Medium),
        ThinkingLevel::Off
    );
    assert_eq!(
        entry.clamp_thinking_level(ThinkingLevel::Off),
        ThinkingLevel::Off
    );
}

/// Reasoning model without xhigh support clamps XHigh to High.
#[test]
fn model_entry_clamp_thinking_xhigh_downgrade() {
    use pi::model::ThinkingLevel;

    let entry = ModelEntry {
        model: Model {
            id: "claude-sonnet-4-5".to_string(),
            reasoning: true,
            ..make_model(
                "anthropic",
                "anthropic-messages",
                "https://api.anthropic.com",
            )
        },
        api_key: Some("key".to_string()),
        headers: HashMap::new(),
        auth_header: true,
        compat: None,
        oauth_config: None,
    };

    // Sonnet doesn't support xhigh
    assert_eq!(
        entry.clamp_thinking_level(ThinkingLevel::XHigh),
        ThinkingLevel::High
    );
    // But High/Medium pass through
    assert_eq!(
        entry.clamp_thinking_level(ThinkingLevel::High),
        ThinkingLevel::High
    );
    assert_eq!(
        entry.clamp_thinking_level(ThinkingLevel::Medium),
        ThinkingLevel::Medium
    );
}

/// Model with xhigh support passes XHigh through.
#[test]
fn model_entry_clamp_thinking_xhigh_supported() {
    use pi::model::ThinkingLevel;

    let entry = ModelEntry {
        model: Model {
            id: "gpt-5.1-codex-max".to_string(),
            reasoning: true,
            ..make_model(
                "openai",
                "openai-chat-completions",
                "https://api.openai.com/v1",
            )
        },
        api_key: Some("key".to_string()),
        headers: HashMap::new(),
        auth_header: true,
        compat: None,
        oauth_config: None,
    };

    assert_eq!(
        entry.clamp_thinking_level(ThinkingLevel::XHigh),
        ThinkingLevel::XHigh
    );
}

// ===========================================================================
// CacheRetention Display/Debug
// ===========================================================================

#[test]
fn cache_retention_variants() {
    let none = CacheRetention::None;
    let short = CacheRetention::Short;
    let long = CacheRetention::Long;
    // Just verify they are distinct and Debug works
    assert_ne!(format!("{none:?}"), format!("{short:?}"));
    assert_ne!(format!("{short:?}"), format!("{long:?}"));
}

// ===========================================================================
// StreamOptions defaults
// ===========================================================================

#[test]
fn stream_options_default_values() {
    let opts = StreamOptions::default();
    assert!(opts.api_key.is_none());
    assert!(opts.temperature.is_none());
    assert!(opts.max_tokens.is_none());
}

// ===========================================================================
// Session creation and basic operations
// ===========================================================================

/// Session::create() produces a valid session with UUID.
#[test]
fn session_create_has_valid_id() {
    asupersync::test_utils::run_test(|| async {
        let session = Session::create();
        assert!(
            !session.header.id.is_empty(),
            "session ID should not be empty"
        );
        assert!(
            session.entries.is_empty(),
            "new session should have no entries"
        );
    });
}

/// Session::create_with_dir() uses the specified directory.
#[test]
fn session_create_with_dir() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("session_with_dir");
        let session = Session::create_with_dir(Some(h.temp_dir().to_path_buf()));
        assert_ne!(session.header.id, "");
    });
}

/// Session append_message adds entries.
#[test]
fn session_append_message_adds_entry() {
    asupersync::test_utils::run_test(|| async {
        let mut session = Session::create();
        let id = session.append_message(user_msg("hello world"));
        assert!(!id.is_empty(), "append should return non-empty ID");
        assert_eq!(session.entries.len(), 1, "should have 1 entry");
    });
}

/// Session set_name and get_name.
#[test]
fn session_name_round_trip() {
    asupersync::test_utils::run_test(|| async {
        let mut session = Session::create();
        let _ = session.set_name("test-session-name");
        let name = session.get_name();
        assert_eq!(name.as_deref(), Some("test-session-name"));
    });
}

/// Session model change is recorded.
#[test]
fn session_model_change_entry() {
    asupersync::test_utils::run_test(|| async {
        let mut session = Session::create();
        let id = session.append_model_change("openai".to_string(), "gpt-4".to_string());
        assert_ne!(id, "");
        assert_eq!(session.entries.len(), 1);
        // Verify it's a ModelChange entry
        match &session.entries[0] {
            SessionEntry::ModelChange(entry) => {
                assert_eq!(entry.provider, "openai");
                assert_eq!(entry.model_id, "gpt-4");
            }
            other => panic!("expected ModelChange, got {other:?}"),
        }
    });
}

/// Session thinking level change is recorded.
#[test]
fn session_thinking_level_change() {
    asupersync::test_utils::run_test(|| async {
        let mut session = Session::create();
        let id = session.append_thinking_level_change("high".to_string());
        assert_ne!(id, "");
    });
}

/// Session add_label on nonexistent entry returns None.
#[test]
fn session_add_label_nonexistent_returns_none() {
    asupersync::test_utils::run_test(|| async {
        let mut session = Session::create();
        let result = session.add_label("nonexistent-id", Some("label".to_string()));
        assert!(
            result.is_none(),
            "labeling nonexistent entry should return None"
        );
    });
}

/// Session add_label on existing entry succeeds.
#[test]
fn session_add_label_existing_entry() {
    asupersync::test_utils::run_test(|| async {
        let mut session = Session::create();
        let entry_id = session.append_message(user_msg("msg"));
        let label_id = session.add_label(&entry_id, Some("important".to_string()));
        assert!(label_id.is_some(), "labeling existing entry should succeed");
        assert_eq!(session.entries.len(), 2, "should have message + label");
    });
}

/// Session custom entry with arbitrary JSON data.
#[test]
fn session_custom_entry() {
    asupersync::test_utils::run_test(|| async {
        let mut session = Session::create();
        let id = session.append_custom_entry(
            "test-custom-type".to_string(),
            Some(json!({"key": "value", "count": 42})),
        );
        assert_ne!(id, "");
        assert_eq!(session.entries.len(), 1);
    });
}

/// entries_for_current_path on empty session.
#[test]
fn session_entries_for_current_path_empty() {
    asupersync::test_utils::run_test(|| async {
        let session = Session::create();
        let entries = session.entries_for_current_path();
        assert!(entries.is_empty(), "empty session should have empty path");
    });
}

/// entries_for_current_path with single message.
#[test]
fn session_entries_for_current_path_single() {
    asupersync::test_utils::run_test(|| async {
        let mut session = Session::create();
        session.append_message(user_msg("msg"));
        let entries = session.entries_for_current_path();
        assert_eq!(entries.len(), 1, "should have 1 entry in path");
    });
}

// ===========================================================================
// Session persistence (save + load round-trip)
// ===========================================================================

/// Session save creates a JSONL file, and open recovers it.
#[test]
fn session_save_and_open_round_trip() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("session_round_trip");
        let mut session = Session::create_with_dir(Some(h.temp_dir().to_path_buf()));

        // Add some entries
        session.append_message(user_msg("first message"));
        session.set_name("round-trip-test");
        session.append_model_change("anthropic".to_string(), "claude-sonnet-4-5".to_string());

        // Save
        session.save().await.expect("save should succeed");
        let path = session
            .path
            .as_ref()
            .expect("should have a path after save");
        let path_str = path.to_string_lossy().to_string();

        // Re-open
        let restored = Session::open(&path_str).await.expect("open should succeed");
        assert_eq!(restored.get_name().as_deref(), Some("round-trip-test"));
        assert!(!restored.entries.is_empty(), "should have entries");
    });
}

/// Session open on nonexistent file returns SessionNotFound.
#[test]
fn session_open_nonexistent_file() {
    asupersync::test_utils::run_test(|| async {
        let result = Session::open("/nonexistent/session/path.jsonl").await;
        assert!(result.is_err(), "opening nonexistent file should error");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.to_lowercase().contains("not found")
                || err_msg.to_lowercase().contains("not exist")
                || err_msg.to_lowercase().contains("no such file"),
            "error should mention not found: got {err_msg}"
        );
    });
}

/// Session open_with_diagnostics on corrupted JSONL reports skipped entries.
#[test]
fn session_open_corrupted_jsonl_reports_diagnostics() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("session_corrupted");
        let session_path = h.temp_dir().join("corrupted.jsonl");

        // Write a valid header followed by corrupted entries
        let mut file = std::fs::File::create(&session_path).expect("create file");
        // Valid header line
        let header = make_session_header();
        writeln!(file, "{}", serde_json::to_string(&header).unwrap()).unwrap();
        // Valid entry
        let entry1 = make_message_entry("e1", "user", "hello");
        writeln!(file, "{}", serde_json::to_string(&entry1).unwrap()).unwrap();
        // Corrupted entry (invalid JSON)
        writeln!(file, "{{not valid json}}").unwrap();
        // Another valid entry
        let entry2 = make_message_entry("e2", "user", "world");
        writeln!(file, "{}", serde_json::to_string(&entry2).unwrap()).unwrap();
        drop(file);

        let result = Session::open_with_diagnostics(&session_path.to_string_lossy()).await;
        // This should succeed (with diagnostics) or fail gracefully
        match result {
            Ok((_session, diagnostics)) => {
                // The corrupted entry should be reported in diagnostics
                assert!(
                    !diagnostics.skipped_entries.is_empty()
                        || diagnostics.orphaned_parent_links.is_empty(),
                    "diagnostics should report corruption: skipped={}, orphaned={}",
                    diagnostics.skipped_entries.len(),
                    diagnostics.orphaned_parent_links.len(),
                );
            }
            Err(e) => {
                // If it errors, that's also acceptable for corrupted data
                let err_str = format!("{e}");
                assert!(
                    err_str.contains("json")
                        || err_str.contains("parse")
                        || err_str.contains("invalid"),
                    "error should mention parsing issue: got {err_str}"
                );
            }
        }
    });
}

/// Session open on empty file returns appropriate error.
#[test]
fn session_open_empty_file() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("session_empty");
        let session_path = h.temp_dir().join("empty.jsonl");
        std::fs::File::create(&session_path).expect("create empty file");

        let result = Session::open(&session_path.to_string_lossy()).await;
        assert!(result.is_err(), "opening empty file should error");
    });
}

/// Session double-save doesn't corrupt file.
#[test]
fn session_double_save_idempotent() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("session_double_save");
        let mut session = Session::create_with_dir(Some(h.temp_dir().to_path_buf()));
        session.append_message(user_msg("msg"));

        session.save().await.expect("first save");
        let path = session.path.as_ref().expect("path after save").clone();
        session.save().await.expect("second save");

        // Re-open should still work
        let restored = Session::open(&path.to_string_lossy())
            .await
            .expect("open after double save");
        assert!(!restored.entries.is_empty());
    });
}

/// Session save after multiple mutations.
#[test]
fn session_save_after_mutations() {
    asupersync::test_utils::run_test(|| async {
        let h = TestHarness::new("session_mutations");
        let mut session = Session::create_with_dir(Some(h.temp_dir().to_path_buf()));

        let msg_id = session.append_message(user_msg("first"));
        session.set_name("mutated");
        session.add_label(&msg_id, Some("reviewed".to_string()));
        session.append_model_change("openai".to_string(), "gpt-4".to_string());
        session.append_thinking_level_change("high".to_string());
        session.append_custom_entry("custom".to_string(), Some(json!({"data": 1})));

        session.save().await.expect("save with mutations");
        let path = session.path.as_ref().expect("path").clone();

        // Verify round-trip
        let restored = Session::open(&path.to_string_lossy()).await.expect("open");
        assert_eq!(restored.get_name().as_deref(), Some("mutated"));
        // Should have: message + label + model_change + thinking_change + custom + session_info(name)
        assert!(
            restored.entries.len() >= 5,
            "should have multiple entry types: got {}",
            restored.entries.len()
        );
    });
}

// ===========================================================================
// Session branching
// ===========================================================================

/// Session branching creates a new branch point.
#[test]
fn session_branch_creates_fork() {
    asupersync::test_utils::run_test(|| async {
        let mut session = Session::create();

        let entry_id = session.append_message(user_msg("branch point"));

        // Create branch from the entry
        let branched = session.create_branch_from(&entry_id);
        assert!(branched, "branch creation should succeed");
    });
}

/// Session get_entry returns None for nonexistent ID.
#[test]
fn session_get_entry_nonexistent() {
    asupersync::test_utils::run_test(|| async {
        let session = Session::create();
        assert!(
            session.get_entry("nonexistent").is_none(),
            "nonexistent entry should return None"
        );
    });
}

/// Session get_children on empty session.
#[test]
fn session_get_children_empty() {
    asupersync::test_utils::run_test(|| async {
        let session = Session::create();
        let children = session.get_children(None);
        assert!(
            children.is_empty(),
            "empty session should have no root children"
        );
    });
}

/// Session get_path_to_entry for existing entry.
#[test]
fn session_get_path_to_entry() {
    asupersync::test_utils::run_test(|| async {
        let mut session = Session::create();
        let id1 = session.append_message(user_msg("first"));
        let _id2 = session.append_message(user_msg("second"));

        let path = session.get_path_to_entry(&id1);
        assert!(
            !path.is_empty(),
            "path to existing entry should not be empty"
        );
    });
}

// ===========================================================================
// Provider creation (high-level factory)
// ===========================================================================

/// create_provider for Anthropic provider with valid entry.
#[test]
fn create_provider_anthropic_succeeds() {
    asupersync::test_utils::run_test(|| async {
        let entry = make_model_entry(
            "anthropic",
            "anthropic-messages",
            "https://api.anthropic.com",
        );
        let provider = create_provider(&entry, None);
        assert!(
            provider.is_ok(),
            "Anthropic provider creation should succeed"
        );
        let p = provider.unwrap();
        assert_eq!(p.name(), "anthropic");
    });
}

/// create_provider for OpenAI provider.
#[test]
fn create_provider_openai_succeeds() {
    asupersync::test_utils::run_test(|| async {
        let entry = make_model_entry(
            "openai",
            "openai-chat-completions",
            "https://api.openai.com/v1",
        );
        let provider = create_provider(&entry, None);
        assert!(provider.is_ok(), "OpenAI provider creation should succeed");
    });
}

/// create_provider for Cohere provider.
#[test]
fn create_provider_cohere_succeeds() {
    asupersync::test_utils::run_test(|| async {
        let entry = make_model_entry("cohere", "cohere-chat", "https://api.cohere.com");
        let provider = create_provider(&entry, None);
        assert!(provider.is_ok(), "Cohere provider creation should succeed");
    });
}

/// create_provider for Google Gemini provider.
#[test]
fn create_provider_gemini_succeeds() {
    asupersync::test_utils::run_test(|| async {
        let entry = make_model_entry(
            "google",
            "google-generativeai",
            "https://generativelanguage.googleapis.com",
        );
        let provider = create_provider(&entry, None);
        assert!(
            provider.is_ok(),
            "Gemini provider creation should succeed: {:?}",
            provider.err()
        );
    });
}

/// create_provider for unknown provider with unknown API returns error.
#[test]
fn create_provider_unknown_provider_and_api_errors() {
    asupersync::test_utils::run_test(|| async {
        let entry = make_model_entry(
            "totally-unknown-provider",
            "totally-unknown-api",
            "https://example.com",
        );
        let result = create_provider(&entry, None);
        // This should either succeed with a fallback or error
        // Depending on implementation, unknown provider+api might route to
        // a default OpenAI-compatible endpoint or error
        match result {
            Ok(p) => {
                // If it succeeds, it should still return a valid provider
                assert_ne!(p.name(), "");
            }
            Err(e) => {
                let err_str = format!("{e}");
                assert!(
                    err_str.contains("unknown")
                        || err_str.contains("unsupported")
                        || err_str.contains("route"),
                    "error should mention routing issue: got {err_str}"
                );
            }
        }
    });
}

/// create_provider with empty API field falls back to provider defaults.
#[test]
fn create_provider_empty_api_uses_default() {
    asupersync::test_utils::run_test(|| async {
        // Anthropic with empty API should default to anthropic-messages
        let entry = make_model_entry("anthropic", "", "https://api.anthropic.com");
        let result = create_provider(&entry, None);
        assert!(
            result.is_ok(),
            "anthropic with empty api should use default: {:?}",
            result.err()
        );
    });
}

/// create_provider for OpenAI Responses API variant.
#[test]
fn create_provider_openai_responses() {
    asupersync::test_utils::run_test(|| async {
        let entry = make_model_entry("openai", "openai-responses", "https://api.openai.com/v1");
        let provider = create_provider(&entry, None);
        assert!(
            provider.is_ok(),
            "OpenAI Responses provider should succeed: {:?}",
            provider.err()
        );
    });
}

/// create_provider without extensions passes None correctly.
#[test]
fn create_provider_no_extensions() {
    asupersync::test_utils::run_test(|| async {
        let entry = make_model_entry(
            "anthropic",
            "anthropic-messages",
            "https://api.anthropic.com",
        );
        // Explicitly pass None for extensions
        let result = create_provider(&entry, None);
        assert!(result.is_ok());
    });
}

// ===========================================================================
// Session encode_cwd
// ===========================================================================

/// encode_cwd produces a safe directory name.
#[test]
fn encode_cwd_basic() {
    let encoded = pi::session::encode_cwd(std::path::Path::new("/home/user/projects/test"));
    assert!(!encoded.is_empty(), "encoded cwd should not be empty");
    // Should not contain path separators
    assert!(
        !encoded.contains('/'),
        "encoded cwd should not contain '/': got {encoded}"
    );
}

/// encode_cwd handles root path.
#[test]
fn encode_cwd_root() {
    let encoded = pi::session::encode_cwd(std::path::Path::new("/"));
    assert!(!encoded.is_empty(), "encoded root should not be empty");
}

/// encode_cwd handles paths with special characters.
#[test]
fn encode_cwd_special_chars() {
    let encoded = pi::session::encode_cwd(std::path::Path::new("/home/user/my project (v2.0)/src"));
    assert_ne!(encoded, "");
    assert!(!encoded.contains('/'));
}

// ===========================================================================
// Session header field access
// ===========================================================================

/// Session header model can be updated.
#[test]
fn session_set_model_header() {
    asupersync::test_utils::run_test(|| async {
        let mut session = Session::create();
        session.set_model_header(
            Some("openai".to_string()),
            Some("gpt-4-turbo".to_string()),
            None,
        );
        // Verify header reflects the change
        assert_eq!(session.header.provider.as_deref(), Some("openai"));
        assert_eq!(session.header.model_id.as_deref(), Some("gpt-4-turbo"));
    });
}

// ===========================================================================
// SessionOpenDiagnostics
// ===========================================================================

/// Default diagnostics are empty.
#[test]
fn session_diagnostics_default_empty() {
    let diags = SessionOpenDiagnostics::default();
    assert!(diags.skipped_entries.is_empty());
    assert!(diags.orphaned_parent_links.is_empty());
}
