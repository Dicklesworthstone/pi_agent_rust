//! Unit tests for interactive extension UI formatting and parsing (bd-2hz.3).
//!
//! Tests cover:
//! - `format_extension_ui_prompt`: provenance display, all methods, edge cases
//! - `parse_extension_ui_response`: confirm, select, input, editor, cancel, errors

use pi::extensions::ExtensionUiRequest;
use pi::interactive::{format_extension_ui_prompt, parse_extension_ui_response};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Helper: build a request with common fields
// ---------------------------------------------------------------------------

fn make_request(method: &str, payload: Value) -> ExtensionUiRequest {
    ExtensionUiRequest::new("req-1", method, payload)
}

fn make_request_with_ext(method: &str, payload: Value, ext_id: &str) -> ExtensionUiRequest {
    ExtensionUiRequest::new("req-1", method, payload).with_extension_id(Some(ext_id.to_string()))
}

// ===========================================================================
// format_extension_ui_prompt — confirm
// ===========================================================================

#[test]
fn format_confirm_basic() {
    let req = make_request(
        "confirm",
        json!({ "title": "Delete file?", "message": "This cannot be undone." }),
    );
    let prompt = format_extension_ui_prompt(&req);
    assert!(prompt.contains("confirm:"), "prompt={prompt}");
    assert!(prompt.contains("Delete file?"), "prompt={prompt}");
    assert!(prompt.contains("This cannot be undone."), "prompt={prompt}");
    assert!(prompt.contains("yes/no"), "prompt={prompt}");
}

#[test]
fn format_confirm_with_provenance() {
    let req = make_request_with_ext(
        "confirm",
        json!({ "title": "Proceed?", "message": "" }),
        "my-extension",
    );
    let prompt = format_extension_ui_prompt(&req);
    assert!(
        prompt.contains("[my-extension]"),
        "should show extension id: {prompt}"
    );
}

#[test]
fn format_confirm_provenance_from_payload() {
    // When extension_id is not set on the struct, fall back to payload.extension_id.
    let req = make_request(
        "confirm",
        json!({ "title": "OK?", "message": "", "extension_id": "payload-ext" }),
    );
    let prompt = format_extension_ui_prompt(&req);
    assert!(
        prompt.contains("[payload-ext]"),
        "should fall back to payload extension_id: {prompt}"
    );
}

#[test]
fn format_confirm_provenance_struct_takes_priority() {
    // Struct extension_id takes priority over payload.
    let req = make_request_with_ext(
        "confirm",
        json!({ "title": "OK?", "message": "", "extension_id": "payload-ext" }),
        "struct-ext",
    );
    let prompt = format_extension_ui_prompt(&req);
    assert!(
        prompt.contains("[struct-ext]"),
        "struct extension_id should take priority: {prompt}"
    );
}

#[test]
fn format_capability_prompt_uses_typed_provenance_and_sanitizes_terminal_controls() {
    let mut req = ExtensionUiRequest::new_capability_prompt(
        "req-cap-safe",
        "trusted\n[Allow Always]\u{1b}]8;;https://attacker.invalid\u{1b}\\extension",
        "ex\t\u{009b}2Jec",
        json!({
            "title": "payload claims http",
            "message": "left\u{1b}Pterminal-payload\u{1b}\\right",
            "extension_id": "payload-extension",
            "capability": "http",
        }),
    );
    req.extension_id = Some("mutated-extension".to_string());

    let prompt = format_extension_ui_prompt(&req);
    assert!(
        prompt.contains("[trusted [Allow Always]extension]"),
        "{prompt}"
    );
    assert!(prompt.contains("capability request: ex ec"), "{prompt}");
    assert!(prompt.contains("leftright"), "{prompt}");
    assert!(!prompt.contains("payload claims http"), "{prompt}");
    assert!(!prompt.contains("attacker.invalid"), "{prompt}");
    assert!(!prompt.contains("terminal-payload"), "{prompt}");
    assert!(!prompt.contains("payload-extension"), "{prompt}");
    assert!(!prompt.contains("mutated-extension"), "{prompt}");
}

#[test]
fn format_confirm_no_provenance() {
    let req = make_request("confirm", json!({ "title": "OK?" }));
    let prompt = format_extension_ui_prompt(&req);
    assert!(
        prompt.contains("[unknown]"),
        "should show unknown when no extension_id: {prompt}"
    );
}

#[test]
fn format_confirm_empty_message() {
    let req = make_request("confirm", json!({ "title": "Continue?" }));
    let prompt = format_extension_ui_prompt(&req);
    assert!(prompt.contains("Continue?"), "prompt={prompt}");
}

#[test]
fn format_confirm_default_title() {
    let req = make_request("confirm", json!({}));
    let prompt = format_extension_ui_prompt(&req);
    assert!(
        prompt.contains("Extension"),
        "should use default title: {prompt}"
    );
}

// ===========================================================================
// format_extension_ui_prompt — select
// ===========================================================================

#[test]
fn format_select_with_options() {
    let req = make_request_with_ext(
        "select",
        json!({
            "title": "Choose model",
            "message": "Select a model:",
            "options": [
                { "label": "Claude Opus", "value": "opus" },
                { "label": "Claude Sonnet", "value": "sonnet" },
                { "label": "Claude Haiku", "value": "haiku" },
            ]
        }),
        "model-selector",
    );
    let prompt = format_extension_ui_prompt(&req);
    assert!(prompt.contains("[model-selector]"), "prompt={prompt}");
    assert!(prompt.contains("Choose model"), "prompt={prompt}");
    assert!(prompt.contains("Select a model:"), "prompt={prompt}");
    assert!(prompt.contains("1) Claude Opus"), "prompt={prompt}");
    assert!(prompt.contains("2) Claude Sonnet"), "prompt={prompt}");
    assert!(prompt.contains("3) Claude Haiku"), "prompt={prompt}");
    assert!(
        prompt.contains("number, label, or 'cancel'"),
        "prompt={prompt}"
    );
}

#[test]
fn format_select_value_only_options() {
    let req = make_request(
        "select",
        json!({
            "title": "Pick",
            "options": [
                { "value": "a" },
                { "value": "b" },
            ]
        }),
    );
    let prompt = format_extension_ui_prompt(&req);
    assert!(prompt.contains("1) a"), "prompt={prompt}");
    assert!(prompt.contains("2) b"), "prompt={prompt}");
}

#[test]
fn format_select_string_options() {
    let req = make_request(
        "select",
        json!({
            "title": "Pick",
            "options": ["alpha", "beta"]
        }),
    );
    let prompt = format_extension_ui_prompt(&req);
    assert!(prompt.contains("1) alpha"), "prompt={prompt}");
    assert!(prompt.contains("2) beta"), "prompt={prompt}");
}

#[test]
fn format_select_empty_options() {
    let req = make_request("select", json!({ "title": "Pick", "options": [] }));
    let prompt = format_extension_ui_prompt(&req);
    assert!(prompt.contains("Pick"), "prompt={prompt}");
    // No numbered items, but still shows instructions
    assert!(
        prompt.contains("number, label, or 'cancel'"),
        "prompt={prompt}"
    );
}

#[test]
fn format_select_no_message() {
    let req = make_request("select", json!({ "title": "Pick", "options": ["x"] }));
    let prompt = format_extension_ui_prompt(&req);
    // With no message, the output should not contain the message text as a separate line.
    // The prompt starts with title and then lists options.
    assert!(prompt.contains("Pick"), "prompt={prompt}");
    assert!(prompt.contains("1) x"), "prompt={prompt}");
}

// ===========================================================================
// format_extension_ui_prompt — input / editor / unknown
// ===========================================================================

#[test]
fn format_input_basic() {
    let req = make_request_with_ext(
        "input",
        json!({ "title": "API Key", "message": "Enter your key:" }),
        "auth-ext",
    );
    let prompt = format_extension_ui_prompt(&req);
    assert!(prompt.contains("[auth-ext]"), "prompt={prompt}");
    assert!(prompt.contains("input:"), "prompt={prompt}");
    assert!(prompt.contains("API Key"), "prompt={prompt}");
    assert!(prompt.contains("Enter your key:"), "prompt={prompt}");
}

#[test]
fn format_editor_basic() {
    let req = make_request(
        "editor",
        json!({ "title": "Edit config", "message": "Modify the YAML below:" }),
    );
    let prompt = format_extension_ui_prompt(&req);
    assert!(prompt.contains("editor:"), "prompt={prompt}");
    assert!(prompt.contains("Edit config"), "prompt={prompt}");
}

#[test]
fn format_unknown_method() {
    let req = make_request(
        "custom_method",
        json!({ "title": "Custom", "message": "hello" }),
    );
    let prompt = format_extension_ui_prompt(&req);
    assert!(prompt.contains("Custom"), "prompt={prompt}");
    assert!(prompt.contains("hello"), "prompt={prompt}");
}

// ===========================================================================
// parse_extension_ui_response — cancel
// ===========================================================================

#[test]
fn parse_cancel_keyword() {
    let req = make_request("confirm", json!({ "title": "OK?" }));
    let resp = parse_extension_ui_response(&req, "cancel").unwrap();
    assert!(resp.cancelled);
    assert_eq!(resp.id, "req-1");
    assert!(resp.value.is_none());
}

#[test]
fn parse_cancel_shortcut() {
    let req = make_request("confirm", json!({ "title": "OK?" }));
    let resp = parse_extension_ui_response(&req, "c").unwrap();
    assert!(resp.cancelled);
}

#[test]
fn parse_cancel_case_insensitive() {
    let req = make_request("select", json!({ "title": "Pick", "options": ["a"] }));
    let resp = parse_extension_ui_response(&req, "CANCEL").unwrap();
    assert!(resp.cancelled);
}

#[test]
fn parse_cancel_with_whitespace() {
    let req = make_request("input", json!({ "title": "Name" }));
    let resp = parse_extension_ui_response(&req, "  cancel  ").unwrap();
    assert!(resp.cancelled);
}

// ===========================================================================
// parse_extension_ui_response — confirm
// ===========================================================================

#[test]
fn parse_confirm_yes_variants() {
    let req = make_request("confirm", json!({ "title": "OK?" }));
    for input in &["yes", "y", "YES", "Yes", "true", "1"] {
        let resp = parse_extension_ui_response(&req, input).unwrap();
        assert!(!resp.cancelled, "input={input}");
        assert_eq!(resp.value, Some(Value::Bool(true)), "input={input}");
    }
}

#[test]
fn parse_confirm_no_variants() {
    let req = make_request("confirm", json!({ "title": "OK?" }));
    for input in &["no", "n", "NO", "No", "false", "0"] {
        let resp = parse_extension_ui_response(&req, input).unwrap();
        assert!(!resp.cancelled, "input={input}");
        assert_eq!(resp.value, Some(Value::Bool(false)), "input={input}");
    }
}

#[test]
fn parse_confirm_invalid() {
    let req = make_request("confirm", json!({ "title": "OK?" }));
    let err = parse_extension_ui_response(&req, "maybe").unwrap_err();
    assert!(err.contains("yes/no"), "err={err}");
}

#[test]
fn parse_confirm_whitespace() {
    let req = make_request("confirm", json!({ "title": "OK?" }));
    let resp = parse_extension_ui_response(&req, "  yes  ").unwrap();
    assert_eq!(resp.value, Some(Value::Bool(true)));
}

#[test]
fn parse_capability_confirm_defaults_ftui_choice_to_one_shot_scope() {
    let req = ExtensionUiRequest::new_capability_prompt(
        "req-cap",
        "trusted-extension",
        "exec",
        json!({"title": "Capability?"}),
    );
    let response = parse_extension_ui_response(&req, "yes").expect("parse capability confirm");
    assert_eq!(
        response.value,
        Some(json!({
            "allow": true,
            "persist": false,
            "remember": false,
        }))
    );
}

// ===========================================================================
// parse_extension_ui_response — select
// ===========================================================================

#[test]
fn parse_select_by_number() {
    let req = make_request(
        "select",
        json!({
            "title": "Pick",
            "options": [
                { "label": "Alpha", "value": "a" },
                { "label": "Beta", "value": "b" },
            ]
        }),
    );
    let resp = parse_extension_ui_response(&req, "1").unwrap();
    assert!(!resp.cancelled);
    assert_eq!(resp.value, Some(Value::String("a".to_string())));
}

#[test]
fn parse_select_by_number_last() {
    let req = make_request(
        "select",
        json!({
            "title": "Pick",
            "options": [
                { "label": "A", "value": "first" },
                { "label": "B", "value": "second" },
                { "label": "C", "value": "third" },
            ]
        }),
    );
    let resp = parse_extension_ui_response(&req, "3").unwrap();
    assert_eq!(resp.value, Some(Value::String("third".to_string())));
}

#[test]
fn parse_select_by_label() {
    let req = make_request(
        "select",
        json!({
            "title": "Pick",
            "options": [
                { "label": "Alpha", "value": "a" },
                { "label": "Beta", "value": "b" },
            ]
        }),
    );
    let resp = parse_extension_ui_response(&req, "beta").unwrap();
    assert_eq!(resp.value, Some(Value::String("b".to_string())));
}

#[test]
fn parse_select_by_value() {
    let req = make_request(
        "select",
        json!({
            "title": "Pick",
            "options": [
                { "label": "Alpha", "value": "opt_a" },
                { "label": "Beta", "value": "opt_b" },
            ]
        }),
    );
    let resp = parse_extension_ui_response(&req, "opt_b").unwrap();
    assert_eq!(resp.value, Some(Value::String("opt_b".to_string())));
}

#[test]
fn parse_select_string_options() {
    let req = make_request(
        "select",
        json!({
            "title": "Pick",
            "options": ["alpha", "beta", "gamma"]
        }),
    );
    let resp = parse_extension_ui_response(&req, "alpha").unwrap();
    assert_eq!(resp.value, Some(Value::String("alpha".to_string())));
}

#[test]
fn parse_select_number_out_of_range() {
    let req = make_request("select", json!({ "title": "Pick", "options": ["a", "b"] }));
    // 0 is out of range (1-indexed)
    let err = parse_extension_ui_response(&req, "0").unwrap_err();
    assert!(err.contains("Invalid selection"), "err={err}");
    // 3 is out of range
    let err = parse_extension_ui_response(&req, "3").unwrap_err();
    assert!(err.contains("Invalid selection"), "err={err}");
}

#[test]
fn parse_select_no_match() {
    let req = make_request("select", json!({ "title": "Pick", "options": ["a", "b"] }));
    let err = parse_extension_ui_response(&req, "xyz").unwrap_err();
    assert!(err.contains("Invalid selection"), "err={err}");
}

#[test]
fn parse_select_no_options() {
    let req = make_request("select", json!({ "title": "Pick" }));
    let err = parse_extension_ui_response(&req, "1").unwrap_err();
    assert!(err.contains("Invalid selection"), "err={err}");
}

#[test]
fn parse_select_label_fallback_when_no_value() {
    // When an option has only a label (no value), the label is used as the value.
    let req = make_request(
        "select",
        json!({
            "title": "Pick",
            "options": [{ "label": "Only Label" }]
        }),
    );
    let resp = parse_extension_ui_response(&req, "1").unwrap();
    assert_eq!(resp.value, Some(Value::String("Only Label".to_string())));
}

// ===========================================================================
// parse_extension_ui_response — input / editor (passthrough)
// ===========================================================================

#[test]
fn parse_input_passthrough() {
    let req = make_request("input", json!({ "title": "Name" }));
    let resp = parse_extension_ui_response(&req, "Hello World").unwrap();
    assert!(!resp.cancelled);
    assert_eq!(resp.value, Some(Value::String("Hello World".to_string())));
    assert_eq!(resp.id, "req-1");
}

#[test]
fn parse_input_preserves_whitespace() {
    let req = make_request("input", json!({ "title": "Name" }));
    // Input for non-select/non-confirm methods preserves original text (no trimming).
    let resp = parse_extension_ui_response(&req, "  spaced  ").unwrap();
    assert_eq!(resp.value, Some(Value::String("  spaced  ".to_string())));
}

#[test]
fn parse_editor_passthrough() {
    let req = make_request("editor", json!({ "title": "Edit" }));
    let resp = parse_extension_ui_response(&req, "multi\nline\ncontent").unwrap();
    assert_eq!(
        resp.value,
        Some(Value::String("multi\nline\ncontent".to_string()))
    );
}

#[test]
fn parse_unknown_method_passthrough() {
    let req = make_request("custom", json!({ "title": "Custom" }));
    let resp = parse_extension_ui_response(&req, "anything").unwrap();
    assert_eq!(resp.value, Some(Value::String("anything".to_string())));
}

// ===========================================================================
// ExtensionUiRequest — method classification and builder
// ===========================================================================

#[test]
fn expects_response_for_dialog_methods() {
    for method in &["select", "confirm", "input", "editor"] {
        let req = make_request(method, json!({}));
        assert!(req.expects_response(), "method={method}");
    }
}

#[test]
fn no_response_for_fire_and_forget() {
    for method in &[
        "notify",
        "setStatus",
        "set_status",
        "setWidget",
        "set_widget",
    ] {
        let req = make_request(method, json!({}));
        assert!(!req.expects_response(), "method={method}");
    }
}

#[test]
fn effective_timeout_from_field() {
    let mut req = make_request("confirm", json!({}));
    req.timeout_ms = Some(5000);
    assert_eq!(req.effective_timeout_ms(), Some(5000));
}

#[test]
fn effective_timeout_from_payload() {
    let req = make_request("confirm", json!({ "timeout": 3000 }));
    assert_eq!(req.effective_timeout_ms(), Some(3000));
}

#[test]
fn effective_timeout_field_overrides_payload() {
    let mut req = make_request("confirm", json!({ "timeout": 3000 }));
    req.timeout_ms = Some(5000);
    assert_eq!(req.effective_timeout_ms(), Some(5000));
}

#[test]
fn effective_timeout_none() {
    let req = make_request("confirm", json!({}));
    assert_eq!(req.effective_timeout_ms(), None);
}

#[test]
fn with_extension_id_builder() {
    let req = make_request("confirm", json!({})).with_extension_id(Some("my-ext".to_string()));
    assert_eq!(req.extension_id.as_deref(), Some("my-ext"));
}

#[test]
fn to_rpc_event_includes_all_fields() {
    let req = make_request("confirm", json!({ "title": "OK?", "message": "sure?" }));
    let event = req.to_rpc_event();
    assert_eq!(event["type"], "extension_ui_request");
    assert_eq!(event["id"], "req-1");
    assert_eq!(event["method"], "confirm");
    assert_eq!(event["title"], "OK?");
    assert_eq!(event["message"], "sure?");
}

// ===========================================================================
// ExtensionUiResponse — basic checks
// ===========================================================================

#[test]
fn response_id_matches_request() {
    let req = ExtensionUiRequest::new("unique-42", "input", json!({ "title": "Name" }));
    let resp = parse_extension_ui_response(&req, "Alice").unwrap();
    assert_eq!(resp.id, "unique-42");
}
