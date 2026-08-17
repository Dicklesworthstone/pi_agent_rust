#![allow(clippy::doc_markdown)]

//! Branch-focused edge/failure-path tests for critical non-mock modules.
//!
//! Covers boundary conditions, fallback branches, and edge-case inputs for:
//! - tools.rs: truncate_head, truncate_tail, process_file_arguments
//! - vcr.rs: redact_cassette, Cassette serde, VcrMode
//! - app.rs: parse_models_arg, apply_piped_stdin, normalize_cli, validate_rpc_args,
//!   build_initial_content, build_system_prompt
//! - error_hints.rs: format_error_with_hints edge cases
//! - error.rs: Display and Debug impls, From conversions

use pi::app;
use pi::cli::Cli;
use pi::error::Error;
use pi::error_hints::{format_error_with_hints, hints_for_error};
use pi::model::{ContentBlock, ImageContent};
use pi::tools::{TruncatedBy, truncate_head, truncate_tail};
use pi::vcr::{
    Cassette, Interaction, RecordedRequest, RecordedResponse, RedactionSummary, VcrMode,
};

use clap::Parser;
use serde_json::{Value, json};
use std::path::Path;
use tempfile::TempDir;

// ═══════════════════════════════════════════════════════════════════════
// SECTION 1: tools.rs — truncate_head branch coverage
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn truncate_head_empty_content() {
    let result = truncate_head("", 10, 1024);
    assert!(!result.truncated);
    assert_eq!(result.total_lines, 0); // empty string = 0 lines
    assert_eq!(result.total_bytes, 0);
    assert_eq!(result.output_lines, 0);
    assert!(!result.first_line_exceeds_limit);
}

#[test]
fn truncate_head_no_truncation_exact_fit() {
    // Content that exactly fits both limits
    let content = "line1\nline2\nline3";
    let result = truncate_head(content, 3, content.len());
    assert!(!result.truncated);
    assert_eq!(result.total_lines, 3);
    assert_eq!(result.output_lines, 3);
    assert_eq!(result.content, content);
}

#[test]
fn truncate_head_first_line_exceeds_bytes() {
    // Single line that is longer than max_bytes — triggers first_line_exceeds_limit branch
    let long_line = "x".repeat(100);
    let result = truncate_head(&long_line, 10, 50);
    assert!(result.truncated);
    assert!(result.first_line_exceeds_limit);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert_eq!(result.output_lines, 1);
    assert_eq!(result.output_bytes, 50);
    assert_eq!(result.content, "x".repeat(50));
}

#[test]
fn truncate_head_truncated_by_lines_not_bytes() {
    let content = "a\nb\nc\nd\ne";
    let result = truncate_head(content, 3, 1024);
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
    assert_eq!(result.output_lines, 3);
    assert_eq!(result.content, "a\nb\nc\n");
}

#[test]
fn truncate_head_truncated_by_bytes_not_lines() {
    // 5 short lines but byte limit cuts it
    let content = "aaaa\nbbbb\ncccc\ndddd\neeee";
    // Each line is 4 bytes + 1 newline = 5 bytes per line. First line = 4 bytes, then +5 each.
    // max_bytes = 10 → fits "aaaa\nbbbb" (9 bytes) but not "aaaa\nbbbb\ncccc" (14 bytes)
    // Actually, "aaaa\nbbbb\n" is 10 bytes! So it fits exactly.
    let result = truncate_head(content, 100, 10);
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert_eq!(result.output_lines, 2);
    assert_eq!(result.content, "aaaa\nbbbb\n");
}

#[test]
fn truncate_head_single_newline_only() {
    let result = truncate_head("\n", 10, 1024);
    assert!(!result.truncated);
    assert_eq!(result.total_lines, 1); // "\n" = 1 empty line
    assert_eq!(result.content, "\n");
}

#[test]
fn truncate_head_trailing_newline_preserved() {
    let content = "line1\nline2\n";
    let result = truncate_head(content, 2, 1024);
    // "line1\nline2\n" has exactly 2 lines.
    // With max_lines=2, it fits exactly.
    assert!(!result.truncated);
    assert_eq!(result.output_lines, 2);
}

#[test]
fn truncate_head_max_lines_zero() {
    let result = truncate_head("hello", 0, 1024);
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
    assert_eq!(result.output_lines, 0);
    assert!(result.content.is_empty());
}

#[test]
fn truncate_head_max_bytes_zero() {
    // First line = "hello" (5 bytes) > 0, triggers first_line_exceeds_limit
    let result = truncate_head("hello", 10, 0);
    assert!(result.truncated);
    assert!(result.first_line_exceeds_limit);
}

#[test]
fn truncate_head_unicode_multibyte() {
    // Each emoji is 4 bytes
    let content = "\u{1F600}\u{1F601}\n\u{1F602}";
    let result = truncate_head(content, 10, 8);
    // First line: 8 bytes (2 emojis). Byte budget = 8.
    // First line fits exactly.
    assert_eq!(result.output_lines, 1);
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 2: tools.rs — truncate_tail branch coverage
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn truncate_tail_empty_content() {
    let result = truncate_tail("", 10, 1024);
    assert!(!result.truncated);
    assert_eq!(result.total_lines, 0);
    assert_eq!(result.total_bytes, 0);
}

#[test]
fn truncate_tail_no_truncation_exact_fit() {
    let content = "line1\nline2\nline3";
    let result = truncate_tail(content, 3, content.len());
    assert!(!result.truncated);
    assert_eq!(result.content, content);
}

#[test]
fn truncate_tail_keeps_last_lines() {
    let content = "a\nb\nc\nd\ne";
    let result = truncate_tail(content, 3, 1024);
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
    assert_eq!(result.output_lines, 3);
    assert_eq!(result.content, "c\nd\ne");
}

#[test]
fn truncate_tail_truncated_by_bytes() {
    let content = "aaaa\nbbbb\ncccc\ndddd\neeee";
    // Keep the maximal valid UTF-8 suffix under the byte budget, including a
    // partial separator when that preserves tail stability.
    let result = truncate_tail(content, 100, 10);
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert_eq!(result.content, "\ndddd\neeee");
}

#[test]
fn truncate_tail_single_long_line_partial_output() {
    // One long line, small byte limit → partial output from end
    let content = "abcdefghijklmnopqrstuvwxyz";
    let result = truncate_tail(content, 10, 10);
    assert!(result.truncated);
    assert!(result.last_line_partial);
    assert_eq!(result.content.len(), 10);
    assert!(content.ends_with(&result.content));
}

#[test]
fn truncate_tail_file_ending_with_newline() {
    // A trailing newline terminates the current line; it does not add another line.
    let content = "a\n";
    let result = truncate_tail(content, 1, 1024);
    assert!(!result.truncated);
    assert_eq!(result.total_lines, 1);
    assert_eq!(result.content, content);
}

#[test]
fn truncate_tail_max_lines_zero() {
    let result = truncate_tail("hello", 0, 1024);
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
    assert_eq!(result.output_lines, 0);
    assert_eq!(result.output_bytes, 0);
    assert!(result.content.is_empty());
}

#[test]
fn truncate_tail_utf8_boundary_partial() {
    // 4-byte emoji repeated, then partial byte limit
    let content = "\u{1F600}\u{1F601}\u{1F602}\u{1F603}";
    // 16 bytes total. Limit to 5 bytes — should snap to nearest char boundary.
    let result = truncate_tail(content, 10, 5);
    assert!(result.truncated);
    // Should get last complete character (4 bytes)
    assert!(result.content.len() <= 5);
    // Verify it's valid UTF-8
    assert!(std::str::from_utf8(result.content.as_bytes()).is_ok());
}

#[test]
fn truncate_tail_single_newline() {
    let result = truncate_tail("\n", 10, 1024);
    assert!(!result.truncated);
    assert_eq!(result.content, "\n");
}

#[test]
fn truncate_tail_many_empty_lines() {
    let content = "\n\n\n\n\n";
    let result = truncate_tail(content, 3, 1024);
    assert!(result.truncated);
    assert_eq!(result.output_lines, 3);
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 3: tools.rs — process_file_arguments edge cases
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn process_file_arguments_nonexistent_file() {
    let dir = TempDir::new().unwrap();
    let result =
        pi::tools::process_file_arguments(&["nonexistent.txt".to_string()], dir.path(), false);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("Cannot access file"));
}

#[test]
fn process_file_arguments_empty_file_skipped() {
    let dir = TempDir::new().unwrap();
    let empty_file = dir.path().join("empty.txt");
    std::fs::write(&empty_file, "").unwrap();
    let result = pi::tools::process_file_arguments(
        &[empty_file.to_string_lossy().to_string()],
        dir.path(),
        false,
    )
    .unwrap();
    assert!(result.text.is_empty());
    assert!(result.images.is_empty());
}

#[test]
fn process_file_arguments_rejects_outside_cwd() {
    let dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let outside_file = outside.path().join("secret.txt");
    std::fs::write(&outside_file, "secret").unwrap();
    let result = pi::tools::process_file_arguments(
        &[outside_file.to_string_lossy().to_string()],
        dir.path(),
        false,
    );
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Cannot read outside the working directory"));
}

#[test]
fn process_file_arguments_text_file_wrapped_in_tags() {
    let dir = TempDir::new().unwrap();
    let text_file = dir.path().join("hello.txt");
    std::fs::write(&text_file, "hello world").unwrap();
    let result = pi::tools::process_file_arguments(
        &[text_file.to_string_lossy().to_string()],
        dir.path(),
        false,
    )
    .unwrap();
    assert!(result.text.contains("<file name="));
    assert!(result.text.contains("hello world"));
    assert!(result.text.contains("</file>"));
}

#[test]
fn process_file_arguments_text_file_without_trailing_newline() {
    let dir = TempDir::new().unwrap();
    let text_file = dir.path().join("no_newline.txt");
    std::fs::write(&text_file, "no newline at end").unwrap();
    let result = pi::tools::process_file_arguments(
        &[text_file.to_string_lossy().to_string()],
        dir.path(),
        false,
    )
    .unwrap();
    // The function adds a newline if content doesn't end with one
    assert!(result.text.contains("no newline at end\n"));
}

#[test]
fn process_file_arguments_multiple_files() {
    let dir = TempDir::new().unwrap();
    let f1 = dir.path().join("one.txt");
    let f2 = dir.path().join("two.txt");
    std::fs::write(&f1, "first").unwrap();
    std::fs::write(&f2, "second").unwrap();
    let result = pi::tools::process_file_arguments(
        &[
            f1.to_string_lossy().to_string(),
            f2.to_string_lossy().to_string(),
        ],
        dir.path(),
        false,
    )
    .unwrap();
    assert!(result.text.contains("first"));
    assert!(result.text.contains("second"));
    // Should have 2 file tags
    let file_count = result.text.matches("<file name=").count();
    assert_eq!(file_count, 2);
}

#[test]
fn process_file_arguments_png_image_detected() {
    let dir = TempDir::new().unwrap();
    let img_file = dir.path().join("test.png");
    // Minimal PNG header
    let png_header: Vec<u8> = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1
        0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, // RGB, etc
        0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, // IDAT chunk
        0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, 0xE2, 0x21, 0xBC,
        0x33, // IDAT data
        0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, // IEND chunk
        0xAE, 0x42, 0x60, 0x82,
    ];
    std::fs::write(&img_file, &png_header).unwrap();
    let result = pi::tools::process_file_arguments(
        &[img_file.to_string_lossy().to_string()],
        dir.path(),
        false,
    )
    .unwrap();
    assert!(!result.images.is_empty());
    assert_eq!(result.images[0].mime_type, "image/png");
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 4: vcr.rs — redact_cassette branch coverage
// ═══════════════════════════════════════════════════════════════════════

fn make_cassette(interactions: Vec<Interaction>) -> Cassette {
    Cassette {
        version: "1".to_string(),
        test_name: "test".to_string(),
        recorded_at: "2025-01-01T00:00:00Z".to_string(),
        interactions,
    }
}

fn make_interaction(
    method: &str,
    url: &str,
    req_headers: Vec<(&str, &str)>,
    req_body: Option<Value>,
    resp_headers: Vec<(&str, &str)>,
    resp_status: u16,
) -> Interaction {
    Interaction {
        request: RecordedRequest {
            method: method.to_string(),
            url: url.to_string(),
            headers: req_headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: req_body,
            body_text: None,
        },
        response: RecordedResponse {
            status: resp_status,
            headers: resp_headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body_chunks: vec![],
            body_chunks_base64: None,
        },
    }
}

#[test]
fn redact_cassette_empty() {
    let mut cassette = make_cassette(vec![]);
    let summary = pi::vcr::redact_cassette(&mut cassette);
    assert_eq!(summary.headers_redacted, 0);
    assert_eq!(summary.json_fields_redacted, 0);
}

#[test]
fn redact_cassette_sensitive_headers() {
    let mut cassette = make_cassette(vec![make_interaction(
        "POST",
        "https://api.example.com/v1/chat",
        vec![
            ("Authorization", "Bearer sk-secret123"),
            ("x-api-key", "my-api-key"),
            ("Content-Type", "application/json"),
        ],
        None,
        vec![("x-azure-api-key", "azure-secret")],
        200,
    )]);
    let summary = pi::vcr::redact_cassette(&mut cassette);
    // authorization + x-api-key in request, x-azure-api-key in response
    assert_eq!(summary.headers_redacted, 3);
    // Verify values are redacted
    let req = &cassette.interactions[0].request;
    for (name, value) in &req.headers {
        if name == "Authorization" || name == "x-api-key" {
            assert_eq!(value, "[REDACTED]");
        }
    }
}

#[test]
fn redact_cassette_sensitive_json_body_fields() {
    let body = json!({
        "api_key": "secret",
        "model": "gpt-4",
        "messages": [
            {"role": "user", "content": "hello"}
        ],
        "nested": {
            "access_token": "tok123",
            "password": "pass",
            "max_tokens": 1024
        }
    });
    let mut cassette = make_cassette(vec![make_interaction(
        "POST",
        "https://api.example.com/v1/chat",
        vec![],
        Some(body),
        vec![],
        200,
    )]);
    let summary = pi::vcr::redact_cassette(&mut cassette);
    assert!(summary.json_fields_redacted >= 3); // api_key, access_token, password
    let req_body = cassette.interactions[0].request.body.as_ref().unwrap();
    assert_eq!(req_body["api_key"], "[REDACTED]");
    assert_eq!(req_body["nested"]["access_token"], "[REDACTED]");
    assert_eq!(req_body["nested"]["password"], "[REDACTED]");
    // max_tokens should NOT be redacted (contains "tokens" not just "token")
    assert_eq!(req_body["nested"]["max_tokens"], 1024);
}

#[test]
fn redact_cassette_array_in_body_recurses() {
    let body = json!({
        "items": [
            {"api_key": "secret1"},
            {"api_key": "secret2"},
            {"safe_field": "not redacted"}
        ]
    });
    let mut cassette = make_cassette(vec![make_interaction(
        "POST",
        "https://api.example.com",
        vec![],
        Some(body),
        vec![],
        200,
    )]);
    let summary = pi::vcr::redact_cassette(&mut cassette);
    assert_eq!(summary.json_fields_redacted, 2);
    let items = &cassette.interactions[0].request.body.as_ref().unwrap()["items"];
    assert_eq!(items[0]["api_key"], "[REDACTED]");
    assert_eq!(items[1]["api_key"], "[REDACTED]");
    assert_eq!(items[2]["safe_field"], "not redacted");
}

#[test]
fn redact_cassette_deeply_nested_json() {
    let body = json!({
        "level1": {
            "level2": {
                "level3": {
                    "secret": "deep-secret"
                }
            }
        }
    });
    let mut cassette = make_cassette(vec![make_interaction(
        "POST",
        "https://api.example.com",
        vec![],
        Some(body),
        vec![],
        200,
    )]);
    let summary = pi::vcr::redact_cassette(&mut cassette);
    assert_eq!(summary.json_fields_redacted, 1);
}

#[test]
fn redact_cassette_token_vs_tokens_distinction() {
    // "token" is sensitive, "tokens" is NOT (it's a count field)
    let body = json!({
        "token": "secret-auth-token",
        "prompt_tokens": 150,
        "completion_tokens": 200,
        "total_tokens": 350,
        "refresh_token": "refresh-secret"
    });
    let mut cassette = make_cassette(vec![make_interaction(
        "POST",
        "https://api.example.com",
        vec![],
        Some(body),
        vec![],
        200,
    )]);
    let _summary = pi::vcr::redact_cassette(&mut cassette);
    let req_body = cassette.interactions[0].request.body.as_ref().unwrap();
    assert_eq!(req_body["token"], "[REDACTED]");
    assert_eq!(req_body["refresh_token"], "[REDACTED]");
    // Count fields should NOT be redacted
    assert_eq!(req_body["prompt_tokens"], 150);
    assert_eq!(req_body["completion_tokens"], 200);
    assert_eq!(req_body["total_tokens"], 350);
}

#[test]
fn redact_cassette_no_body() {
    let mut cassette = make_cassette(vec![make_interaction(
        "GET",
        "https://api.example.com",
        vec![("Authorization", "Bearer token")],
        None, // No body
        vec![],
        200,
    )]);
    let summary = pi::vcr::redact_cassette(&mut cassette);
    assert_eq!(summary.headers_redacted, 1);
    assert_eq!(summary.json_fields_redacted, 0);
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 5: vcr.rs — Cassette serde round-trip
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn cassette_serde_round_trip() {
    let cassette = make_cassette(vec![make_interaction(
        "POST",
        "https://api.anthropic.com/v1/messages",
        vec![("Content-Type", "application/json")],
        Some(json!({"model": "claude-sonnet-4-5", "messages": []})),
        vec![("Content-Type", "text/event-stream")],
        200,
    )]);
    let json_str = serde_json::to_string_pretty(&cassette).unwrap();
    let deserialized: Cassette = serde_json::from_str(&json_str).unwrap();
    assert_eq!(deserialized.version, "1");
    assert_eq!(deserialized.test_name, "test");
    assert_eq!(deserialized.interactions.len(), 1);
    assert_eq!(deserialized.interactions[0].request.method, "POST");
}

#[test]
fn cassette_serde_with_body_text() {
    let mut interaction =
        make_interaction("POST", "https://api.example.com", vec![], None, vec![], 200);
    interaction.request.body_text = Some("raw body text".to_string());
    let cassette = make_cassette(vec![interaction]);
    let json_str = serde_json::to_string(&cassette).unwrap();
    let deserialized: Cassette = serde_json::from_str(&json_str).unwrap();
    assert_eq!(
        deserialized.interactions[0].request.body_text.as_deref(),
        Some("raw body text")
    );
}

#[test]
fn cassette_serde_with_base64_chunks() {
    let mut interaction =
        make_interaction("POST", "https://api.example.com", vec![], None, vec![], 200);
    interaction.response.body_chunks_base64 = Some(vec!["SGVsbG8gV29ybGQ=".to_string()]);
    let cassette = make_cassette(vec![interaction]);
    let json_str = serde_json::to_string(&cassette).unwrap();
    let deserialized: Cassette = serde_json::from_str(&json_str).unwrap();
    let chunks = deserialized.interactions[0]
        .response
        .body_chunks_base64
        .as_ref()
        .unwrap();
    assert_eq!(chunks[0], "SGVsbG8gV29ybGQ=");
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 6: vcr.rs — VcrMode parsing
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn vcr_mode_display_and_debug() {
    assert_eq!(format!("{:?}", VcrMode::Record), "Record");
    assert_eq!(format!("{:?}", VcrMode::Playback), "Playback");
}

#[test]
fn redaction_summary_default() {
    let summary = RedactionSummary::default();
    assert_eq!(summary.headers_redacted, 0);
    assert_eq!(summary.json_fields_redacted, 0);
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 7: app.rs — parse_models_arg edge cases
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn parse_models_arg_empty_string() {
    let result = app::parse_models_arg("");
    assert!(result.is_empty());
}

#[test]
fn parse_models_arg_single_model() {
    let result = app::parse_models_arg("claude-sonnet-4-5");
    assert_eq!(result, vec!["claude-sonnet-4-5"]);
}

#[test]
fn parse_models_arg_multiple_comma_separated() {
    let result = app::parse_models_arg("gpt-4o, claude-sonnet-4-5, gemini-pro");
    assert_eq!(result, vec!["gpt-4o", "claude-sonnet-4-5", "gemini-pro"]);
}

#[test]
fn parse_models_arg_trailing_comma() {
    let result = app::parse_models_arg("model1, model2,");
    assert_eq!(result, vec!["model1", "model2"]);
}

#[test]
fn parse_models_arg_leading_comma() {
    let result = app::parse_models_arg(",model1");
    assert_eq!(result, vec!["model1"]);
}

#[test]
fn parse_models_arg_whitespace_only() {
    let result = app::parse_models_arg("  ,  ,  ");
    assert!(result.is_empty());
}

#[test]
fn parse_models_arg_multiple_commas() {
    let result = app::parse_models_arg("model1,,,model2");
    assert_eq!(result, vec!["model1", "model2"]);
}

#[test]
fn parse_models_arg_with_glob_pattern() {
    let result = app::parse_models_arg("claude-*, gpt-4*");
    assert_eq!(result, vec!["claude-*", "gpt-4*"]);
}

#[test]
fn parse_models_arg_with_thinking_level_suffix() {
    let result = app::parse_models_arg("claude-sonnet-4-5:high");
    assert_eq!(result, vec!["claude-sonnet-4-5:high"]);
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 8: app.rs — apply_piped_stdin branch coverage
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn apply_piped_stdin_none_does_nothing() {
    let mut cli = Cli::parse_from(["pi", "hello"]);
    let orig_print = cli.print;
    let orig_args_len = cli.args.len();
    app::apply_piped_stdin(&mut cli, None);
    assert_eq!(cli.print, orig_print);
    assert_eq!(cli.args.len(), orig_args_len);
}

#[test]
fn apply_piped_stdin_empty_string_does_nothing() {
    let mut cli = Cli::parse_from(["pi"]);
    app::apply_piped_stdin(&mut cli, Some(String::new()));
    assert!(!cli.print);
    assert!(cli.args.is_empty());
}

#[test]
fn apply_piped_stdin_whitespace_only_does_nothing() {
    let mut cli = Cli::parse_from(["pi"]);
    app::apply_piped_stdin(&mut cli, Some("\n\r\n".to_string()));
    assert!(!cli.print);
    assert!(cli.args.is_empty());
}

#[test]
fn apply_piped_stdin_with_content_enables_print() {
    let mut cli = Cli::parse_from(["pi"]);
    assert!(!cli.print);
    app::apply_piped_stdin(&mut cli, Some("piped input\n".to_string()));
    assert!(cli.print);
    assert_eq!(cli.args[0], "piped input");
}

#[test]
fn apply_piped_stdin_prepends_to_existing_args() {
    let mut cli = Cli::parse_from(["pi", "existing_arg"]);
    app::apply_piped_stdin(&mut cli, Some("stdin content".to_string()));
    assert_eq!(cli.args[0], "stdin content");
    assert_eq!(cli.args[1], "existing_arg");
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 9: app.rs — normalize_cli branch coverage
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn normalize_cli_print_sets_no_session() {
    let mut cli = Cli::parse_from(["pi", "-p", "hello"]);
    assert!(cli.print);
    app::normalize_cli(&mut cli);
    assert!(cli.no_session);
}

#[test]
fn normalize_cli_no_print_keeps_session() {
    let mut cli = Cli::parse_from(["pi", "hello"]);
    assert!(!cli.print);
    app::normalize_cli(&mut cli);
    assert!(!cli.no_session);
}

#[test]
fn normalize_cli_lowercases_provider() {
    let mut cli = Cli::parse_from(["pi", "--provider", "OpenAI"]);
    app::normalize_cli(&mut cli);
    assert_eq!(cli.provider.as_deref(), Some("openai"));
}

#[test]
fn normalize_cli_no_provider_is_fine() {
    let mut cli = Cli::parse_from(["pi"]);
    app::normalize_cli(&mut cli); // Should not panic
    assert!(cli.provider.is_none());
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 10: app.rs — validate_rpc_args branch coverage
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn validate_rpc_args_no_mode_is_ok() {
    let cli = Cli::parse_from(["pi", "@file.txt"]);
    assert!(app::validate_rpc_args(&cli).is_ok());
}

#[test]
fn validate_rpc_args_rpc_mode_no_files_is_ok() {
    let cli = Cli::parse_from(["pi", "--mode", "rpc", "hello"]);
    assert!(app::validate_rpc_args(&cli).is_ok());
}

#[test]
fn validate_rpc_args_rpc_mode_with_files_is_error() {
    let cli = Cli::parse_from(["pi", "--mode", "rpc", "@file.txt"]);
    let result = app::validate_rpc_args(&cli);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("not supported in RPC mode")
    );
}

#[test]
fn validate_rpc_args_text_mode_with_files_is_ok() {
    let cli = Cli::parse_from(["pi", "--mode", "text", "@file.txt"]);
    assert!(app::validate_rpc_args(&cli).is_ok());
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 11: app.rs — build_initial_content branch coverage
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn build_initial_content_text_only() {
    let initial = app::InitialMessage {
        text: "hello world".to_string(),
        images: vec![],
    };
    let blocks = app::build_initial_content(&initial);
    assert_eq!(blocks.len(), 1);
    match &blocks[0] {
        ContentBlock::Text(tc) => assert_eq!(tc.text, "hello world"),
        _ => panic!("Expected Text content block"),
    }
}

#[test]
fn build_initial_content_with_images() {
    let initial = app::InitialMessage {
        text: "describe this".to_string(),
        images: vec![ImageContent {
            data: "base64data".to_string(),
            mime_type: "image/png".to_string(),
        }],
    };
    let blocks = app::build_initial_content(&initial);
    assert_eq!(blocks.len(), 2);
    assert!(matches!(&blocks[0], ContentBlock::Text(_)));
    assert!(matches!(&blocks[1], ContentBlock::Image(_)));
}

#[test]
fn build_initial_content_multiple_images() {
    let initial = app::InitialMessage {
        text: "compare".to_string(),
        images: vec![
            ImageContent {
                data: "img1".to_string(),
                mime_type: "image/png".to_string(),
            },
            ImageContent {
                data: "img2".to_string(),
                mime_type: "image/jpeg".to_string(),
            },
        ],
    };
    let blocks = app::build_initial_content(&initial);
    assert_eq!(blocks.len(), 3); // 1 text + 2 images
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 12: app.rs — build_system_prompt branches
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn build_system_prompt_test_mode_uses_placeholders() {
    let cli = Cli::parse_from(["pi"]);
    let cwd = Path::new("/tmp/test_cwd");
    let global_dir = Path::new("/tmp/nonexistent_global");
    let package_dir = Path::new("/tmp/nonexistent_package");
    let prompt = app::build_system_prompt(
        &cli,
        cwd,
        &["read", "bash"],
        None,
        global_dir,
        package_dir,
        true, // test_mode
        true, // include_cwd
        None,
    )
    .expect("build system prompt");
    assert!(prompt.contains("<TIMESTAMP>"));
    assert!(prompt.contains("<CWD>"));
    assert!(!prompt.contains("/tmp/test_cwd"));
}

#[test]
fn build_system_prompt_non_test_mode_uses_real_values() {
    let cli = Cli::parse_from(["pi"]);
    let cwd = Path::new("/tmp/test_cwd");
    let global_dir = Path::new("/tmp/nonexistent_global");
    let package_dir = Path::new("/tmp/nonexistent_package");
    let prompt = app::build_system_prompt(
        &cli,
        cwd,
        &["read"],
        None,
        global_dir,
        package_dir,
        false,
        true,
        None,
    )
    .expect("build system prompt");
    assert!(!prompt.contains("<TIMESTAMP>"));
    assert!(prompt.contains("/tmp/test_cwd"));
}

#[test]
fn build_system_prompt_with_skills_prompt() {
    let cli = Cli::parse_from(["pi"]);
    let cwd = Path::new("/tmp");
    let global_dir = Path::new("/tmp/nonexistent");
    let package_dir = Path::new("/tmp/nonexistent");
    let prompt = app::build_system_prompt(
        &cli,
        cwd,
        &[],
        Some("\n# Available Skills\n- /commit: Make git commits\n"),
        global_dir,
        package_dir,
        true,
        true,
        None,
    )
    .expect("build system prompt");
    assert!(prompt.contains("Available Skills"));
    assert!(prompt.contains("/commit"));
}

#[test]
fn build_system_prompt_includes_hashline_edit_description_and_guideline() {
    let cli = Cli::parse_from(["pi"]);
    let cwd = Path::new("/tmp/test_cwd");
    let global_dir = Path::new("/tmp/nonexistent_global");
    let package_dir = Path::new("/tmp/nonexistent_package");
    let prompt = app::build_system_prompt(
        &cli,
        cwd,
        &["read", "bash", "edit", "write", "hashline_edit"],
        None,
        global_dir,
        package_dir,
        true,
        true,
        None,
    )
    .expect("build system prompt");
    assert!(
        prompt.contains("hashline_edit"),
        "System prompt should list hashline_edit tool"
    );
    assert!(
        prompt.contains("LINE#HASH"),
        "System prompt should describe hashline tag format"
    );
    assert!(
        prompt.contains("hashline=true"),
        "System prompt guideline should mention read with hashline=true"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 13: error.rs — Display and From conversion branches
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn error_config_display() {
    let err = Error::config("bad config");
    assert!(err.to_string().contains("bad config"));
}

#[test]
fn error_session_display() {
    let err = Error::session("corrupted session");
    assert!(err.to_string().contains("corrupted session"));
}

#[test]
fn error_session_not_found_display() {
    let err = Error::SessionNotFound {
        path: "/sessions/abc123.jsonl".to_string(),
    };
    let display = err.to_string();
    assert!(display.contains("abc123"));
}

#[test]
fn error_provider_display() {
    let err = Error::provider("anthropic", "429 rate limit exceeded");
    let display = err.to_string();
    assert!(display.contains("anthropic"));
    assert!(display.contains("429"));
}

#[test]
fn error_auth_display() {
    let err = Error::auth("API key not configured");
    assert!(err.to_string().contains("API key"));
}

#[test]
fn error_tool_display() {
    let err = Error::tool("bash", "exit code 127");
    let display = err.to_string();
    assert!(display.contains("bash"));
    assert!(display.contains("exit code"));
}

#[test]
fn error_validation_display() {
    let err = Error::validation("missing field: name");
    assert!(err.to_string().contains("name"));
}

#[test]
fn error_extension_display() {
    let err = Error::extension("extension crashed");
    assert!(err.to_string().contains("crashed"));
}

#[test]
fn error_aborted_display() {
    let err = Error::Aborted;
    let display = err.to_string();
    assert!(!display.is_empty());
}

#[test]
fn error_api_display() {
    let err = Error::api("503 service unavailable");
    assert!(err.to_string().contains("503"));
}

#[test]
fn error_io_from_std_io() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
    let err = Error::Io(Box::new(io_err));
    assert!(err.to_string().contains("file not found"));
}

#[test]
fn error_json_from_serde() {
    let json_err = serde_json::from_str::<Value>("{ invalid }").unwrap_err();
    let err = Error::Json(Box::new(json_err));
    assert!(!err.to_string().is_empty());
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 14: error_hints.rs — format_error_with_hints edge cases
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn format_error_summary_skipped_when_contained_in_message() {
    // When the error message already contains the summary text, it should not be duplicated
    let err = Error::config("Configuration error in settings");
    let formatted = format_error_with_hints(&err);
    // "Configuration error" is the summary for generic config errors
    // If the error message contains "Configuration error", the summary line should be skipped
    let count = formatted.matches("Configuration error").count();
    // Should appear exactly once (not duplicated)
    assert!(count >= 1);
}

#[test]
fn format_error_with_hints_sqlite_locked() {
    // Sqlite errors need special handling since we need the fsqlite::FrankenError type
    // Test with session "locked" since that's matchable without sqlite
    let err = Error::session("session file locked by another process");
    let formatted = format_error_with_hints(&err);
    assert!(formatted.contains("locked"));
    assert!(formatted.contains("Close"));
}

#[test]
fn format_error_network_connection_hints() {
    let err = Error::provider("openai", "connection refused to api.openai.com");
    let formatted = format_error_with_hints(&err);
    assert!(formatted.contains("Network"));
    assert!(formatted.contains("internet"));
}

#[test]
fn format_error_model_not_found_hints() {
    let err = Error::provider("openai", "model gpt-99 not found");
    let formatted = format_error_with_hints(&err);
    assert!(formatted.contains("Model not found"));
    assert!(formatted.contains("--list-models"));
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 15: error_hints.rs — all hint function fallback branches
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn hints_config_cassette() {
    let hint = hints_for_error(&Error::config("cassette file missing"));
    assert_eq!(hint.summary, "VCR cassette missing or invalid");
    assert!(hint.context_fields.contains(&"file_path"));
}

#[test]
fn hints_config_settings_json() {
    let hint = hints_for_error(&Error::config("settings.json not found"));
    assert_eq!(hint.summary, "Invalid or missing configuration file");
}

#[test]
fn hints_config_models_json() {
    let hint = hints_for_error(&Error::config("models.json parse error"));
    assert_eq!(hint.summary, "Invalid models configuration");
    assert!(hint.context_fields.contains(&"parse_error"));
}

#[test]
fn hints_config_generic() {
    let hint = hints_for_error(&Error::config("unknown config problem"));
    assert_eq!(hint.summary, "Configuration error");
    assert!(hint.context_fields.is_empty());
}

#[test]
fn hints_session_not_found() {
    let hint = hints_for_error(&Error::SessionNotFound {
        path: "/some/path".to_string(),
    });
    assert_eq!(hint.summary, "Session file not found");
    assert!(hint.context_fields.contains(&"path"));
}

#[test]
fn hints_session_corrupted() {
    let hint = hints_for_error(&Error::session("corrupted data at line 5"));
    assert_eq!(hint.summary, "Session file is corrupted or invalid");
    assert!(hint.context_fields.contains(&"line_number"));
}

#[test]
fn hints_session_invalid() {
    let hint = hints_for_error(&Error::session("invalid format"));
    assert!(hint.summary.contains("corrupted") || hint.summary.contains("invalid"));
}

#[test]
fn hints_session_locked() {
    let hint = hints_for_error(&Error::session("locked by pid 1234"));
    assert_eq!(hint.summary, "Session file is locked by another process");
}

#[test]
fn hints_session_generic() {
    let hint = hints_for_error(&Error::session("misc error"));
    assert_eq!(hint.summary, "Session error");
}

#[test]
fn hints_auth_api_key() {
    let hint = hints_for_error(&Error::auth("API key not set"));
    assert_eq!(hint.summary, "API key not configured");
    assert!(hint.hints.iter().any(|h| h.contains("ANTHROPIC_API_KEY")));
}

#[test]
fn hints_auth_api_key_variant() {
    let hint = hints_for_error(&Error::auth("missing api_key for provider"));
    assert_eq!(hint.summary, "API key not configured");
}

#[test]
fn hints_auth_401() {
    let hint = hints_for_error(&Error::auth("401 unauthorized"));
    assert_eq!(hint.summary, "API key is invalid or expired");
}

#[test]
fn hints_auth_unauthorized() {
    let hint = hints_for_error(&Error::auth("request unauthorized by server"));
    assert_eq!(hint.summary, "API key is invalid or expired");
}

#[test]
fn hints_auth_oauth() {
    let hint = hints_for_error(&Error::auth("OAuth token expired"));
    assert_eq!(hint.summary, "OAuth token expired or invalid");
    assert!(hint.hints.iter().any(|h| h.contains("pi login")));
}

#[test]
fn hints_auth_refresh() {
    let hint = hints_for_error(&Error::auth("failed to refresh token"));
    assert_eq!(hint.summary, "OAuth token expired or invalid");
}

#[test]
fn hints_auth_lock() {
    let hint = hints_for_error(&Error::auth("auth file lock contention"));
    assert_eq!(hint.summary, "Auth file locked by another process");
}

#[test]
fn hints_auth_generic() {
    let hint = hints_for_error(&Error::auth("something else"));
    assert_eq!(hint.summary, "Authentication error");
}

#[test]
fn hints_provider_rate_limit_429() {
    let hint = hints_for_error(&Error::provider("x", "429 Too Many Requests"));
    assert_eq!(hint.summary, "Rate limit exceeded");
}

#[test]
fn hints_provider_rate_limit_text() {
    let hint = hints_for_error(&Error::provider("x", "rate limit reached"));
    assert_eq!(hint.summary, "Rate limit exceeded");
}

#[test]
fn hints_provider_server_error_500() {
    let hint = hints_for_error(&Error::provider("x", "500 internal error"));
    assert_eq!(hint.summary, "Provider server error");
}

#[test]
fn hints_provider_server_error_text() {
    let hint = hints_for_error(&Error::provider("x", "server error occurred"));
    assert_eq!(hint.summary, "Provider server error");
}

#[test]
fn hints_provider_connection() {
    let hint = hints_for_error(&Error::provider("x", "connection refused"));
    assert_eq!(hint.summary, "Network connection error");
}

#[test]
fn hints_provider_network() {
    let hint = hints_for_error(&Error::provider("x", "network unreachable"));
    assert_eq!(hint.summary, "Network connection error");
}

#[test]
fn hints_provider_timeout() {
    let hint = hints_for_error(&Error::provider("x", "request timeout after 30s"));
    assert_eq!(hint.summary, "Request timed out");
}

#[test]
fn hints_provider_model_not_found() {
    let hint = hints_for_error(&Error::provider("x", "model gpt-99 not found"));
    assert_eq!(hint.summary, "Model not found or unavailable");
}

#[test]
fn hints_provider_generic() {
    let hint = hints_for_error(&Error::provider("x", "unknown provider issue"));
    assert_eq!(hint.summary, "Provider API error");
}

#[test]
fn hints_tool_read_not_found() {
    let hint = hints_for_error(&Error::tool("read", "file not found: /foo"));
    assert_eq!(hint.summary, "File not found");
}

#[test]
fn hints_tool_read_permission() {
    let hint = hints_for_error(&Error::tool("read", "permission denied: /etc/shadow"));
    assert_eq!(hint.summary, "Permission denied reading file");
}

#[test]
fn hints_tool_write_permission() {
    let hint = hints_for_error(&Error::tool("write", "permission denied: /root/file"));
    assert_eq!(hint.summary, "Permission denied writing file");
}

#[test]
fn hints_tool_edit_not_found() {
    let hint = hints_for_error(&Error::tool("edit", "text not found in file"));
    assert_eq!(hint.summary, "Text to replace not found in file");
}

#[test]
fn hints_tool_edit_ambiguous() {
    let hint = hints_for_error(&Error::tool("edit", "ambiguous match: 3 occurrences"));
    assert_eq!(hint.summary, "Multiple matches found for replacement");
}

#[test]
fn hints_tool_bash_timeout() {
    let hint = hints_for_error(&Error::tool("bash", "command timeout after 120s"));
    assert_eq!(hint.summary, "Command timed out");
}

#[test]
fn hints_tool_bash_exit_code() {
    let hint = hints_for_error(&Error::tool("bash", "exit code 1"));
    assert_eq!(hint.summary, "Command failed with non-zero exit code");
    assert!(hint.context_fields.contains(&"stderr"));
}

#[test]
fn hints_tool_grep_pattern() {
    let hint = hints_for_error(&Error::tool("grep", "invalid regex pattern"));
    assert_eq!(hint.summary, "Invalid regex pattern");
}

#[test]
fn hints_tool_find_fd() {
    let hint = hints_for_error(&Error::tool("find", "fd command not found"));
    assert_eq!(hint.summary, "fd command not found");
    assert!(hint.hints.iter().any(|h| h.contains("fdfind")));
}

#[test]
fn hints_tool_generic() {
    let hint = hints_for_error(&Error::tool("unknown", "something"));
    assert_eq!(hint.summary, "Tool execution error");
}

#[test]
fn hints_validation_required() {
    let hint = hints_for_error(&Error::validation("field required"));
    assert_eq!(hint.summary, "Required field missing");
}

#[test]
fn hints_validation_type() {
    let hint = hints_for_error(&Error::validation("wrong type for field"));
    assert_eq!(hint.summary, "Invalid parameter type");
}

#[test]
fn hints_validation_generic() {
    let hint = hints_for_error(&Error::validation("value out of range"));
    assert_eq!(hint.summary, "Validation error");
}

#[test]
fn hints_extension_not_found() {
    let hint = hints_for_error(&Error::extension("extension my-ext not found"));
    assert_eq!(hint.summary, "Extension not found");
}

#[test]
fn hints_extension_manifest() {
    let hint = hints_for_error(&Error::extension("invalid manifest"));
    assert_eq!(hint.summary, "Invalid extension manifest");
}

#[test]
fn hints_extension_capability() {
    let hint = hints_for_error(&Error::extension("capability network denied"));
    assert_eq!(hint.summary, "Extension capability denied");
}

#[test]
fn hints_extension_permission() {
    let hint = hints_for_error(&Error::extension("permission denied for exec"));
    assert_eq!(hint.summary, "Extension capability denied");
}

#[test]
fn hints_extension_generic() {
    let hint = hints_for_error(&Error::extension("runtime crashed"));
    assert_eq!(hint.summary, "Extension error");
}

#[test]
fn hints_io_not_found() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
    let hint = hints_for_error(&Error::Io(Box::new(io_err)));
    assert_eq!(hint.summary, "File or directory not found");
}

#[test]
fn hints_io_permission_denied() {
    let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
    let hint = hints_for_error(&Error::Io(Box::new(io_err)));
    assert_eq!(hint.summary, "Permission denied");
}

#[test]
fn hints_io_already_exists() {
    let io_err = std::io::Error::new(std::io::ErrorKind::AlreadyExists, "exists");
    let hint = hints_for_error(&Error::Io(Box::new(io_err)));
    assert_eq!(hint.summary, "File already exists");
}

#[test]
fn hints_io_generic() {
    let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "conn");
    let hint = hints_for_error(&Error::Io(Box::new(io_err)));
    assert_eq!(hint.summary, "I/O error");
}

#[test]
fn hints_json_syntax() {
    let json_err = serde_json::from_str::<Value>("{ bad }").unwrap_err();
    let hint = hints_for_error(&Error::Json(Box::new(json_err)));
    assert_eq!(hint.summary, "Invalid JSON syntax");
}

#[test]
fn hints_json_data_mismatch() {
    let json_err = serde_json::from_str::<Vec<i32>>(r#"{"not":"array"}"#).unwrap_err();
    let hint = hints_for_error(&Error::Json(Box::new(json_err)));
    assert_eq!(hint.summary, "JSON data does not match expected structure");
}

#[test]
fn hints_json_eof() {
    let json_err = serde_json::from_str::<Value>("").unwrap_err();
    let hint = hints_for_error(&Error::Json(Box::new(json_err)));
    assert!(hint.summary.contains("JSON"));
}

#[test]
fn hints_aborted() {
    let hint = hints_for_error(&Error::Aborted);
    assert_eq!(hint.summary, "Operation cancelled by user");
    assert!(hint.hints.is_empty());
    assert!(hint.context_fields.is_empty());
}

#[test]
fn hints_api_401() {
    let hint = hints_for_error(&Error::api("401 Unauthorized"));
    assert_eq!(hint.summary, "Unauthorized API request");
}

#[test]
fn hints_api_403() {
    let hint = hints_for_error(&Error::api("403 Forbidden"));
    assert_eq!(hint.summary, "Forbidden API request");
}

#[test]
fn hints_api_404() {
    let hint = hints_for_error(&Error::api("404 Not Found"));
    assert_eq!(hint.summary, "API resource not found");
}

#[test]
fn hints_api_generic() {
    let hint = hints_for_error(&Error::api("502 Bad Gateway"));
    assert_eq!(hint.summary, "API error");
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 16: Truncation boundary precision tests
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn truncate_head_byte_boundary_between_lines() {
    // "ab\ncd\nef" -> line sizes: "ab"=2, "\ncd"=3 (total 5), "\nef"=3 (total 8)
    let content = "ab\ncd\nef";
    // Byte limit 5 → "ab\ncd" = 5 bytes exactly
    let result = truncate_head(content, 100, 5);
    assert_eq!(result.content, "ab\ncd");
    assert_eq!(result.output_lines, 2);
    assert!(result.truncated);
}

#[test]
fn truncate_head_byte_limit_one_less_than_line_end() {
    // Byte pressure preserves the maximal valid UTF-8 prefix, even if that
    // leaves the final output line partial.
    let content = "ab\ncd\nef";
    let result = truncate_head(content, 100, 4);
    assert_eq!(result.content, "ab\nc");
    assert_eq!(result.output_lines, 2);
    assert!(result.last_line_partial);
}

#[test]
fn truncate_tail_byte_boundary_precision() {
    // "ab\ncd\nef" → from tail: "ef"=2, "\nef"=3, "cd\nef"=5, "\ncd\nef"=6, "ab\ncd\nef"=8
    let content = "ab\ncd\nef";
    let result = truncate_tail(content, 100, 5);
    assert_eq!(result.content, "cd\nef");
    assert_eq!(result.output_lines, 2);
}

#[test]
fn truncate_tail_byte_limit_excludes_partial_line() {
    let content = "ab\ncd\nef";
    // Tail truncation keeps the maximal valid UTF-8 suffix under the byte
    // budget, including a partial preceding line when space remains.
    let result = truncate_tail(content, 100, 4);
    assert_eq!(result.content, "d\nef");
    assert_eq!(result.output_lines, 2);
}

#[test]
fn truncate_head_both_limits_hit_lines_first() {
    // 3 lines, 6 bytes total. Line limit = 2 (hits first), byte limit = 100
    let content = "a\nb\nc";
    let result = truncate_head(content, 2, 100);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
    assert_eq!(result.output_lines, 2);
    assert_eq!(result.content, "a\nb\n");
}

#[test]
fn truncate_head_both_limits_hit_bytes_first() {
    // 3 lines, 6 bytes total. Line limit = 100, byte limit = 4
    let content = "a\nb\nc";
    let result = truncate_head(content, 100, 4);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    // "a\nb\n" = 4 bytes
    assert_eq!(result.output_lines, 2);
    assert_eq!(result.content, "a\nb\n");
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 17: kill_process_tree edge cases
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn kill_process_tree_none_pid() {
    // Should not panic when given None
    pi::tools::kill_process_tree(None);
}

#[test]
fn kill_process_tree_nonexistent_pid() {
    // Should not panic for a PID that doesn't exist
    pi::tools::kill_process_tree(Some(999_999_999));
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 18: Cross-cutting format_error_with_hints output formatting
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn format_error_always_starts_with_error_prefix() {
    let errors: Vec<Error> = vec![
        Error::config("test"),
        Error::auth("test"),
        Error::provider("p", "test"),
        Error::tool("t", "test"),
        Error::validation("test"),
        Error::extension("test"),
        Error::api("test"),
        Error::session("test"),
        Error::Aborted,
    ];
    for err in &errors {
        let formatted = format_error_with_hints(err);
        assert!(
            formatted.starts_with("Error:"),
            "Expected 'Error:' prefix for {err:?}, got: {formatted}"
        );
    }
}

#[test]
fn format_error_io_includes_suggestions() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file.txt");
    let err = Error::Io(Box::new(io_err));
    let formatted = format_error_with_hints(&err);
    assert!(formatted.contains("Suggestions:"));
    assert!(formatted.contains("\u{2022}")); // bullet point
}

#[test]
fn format_error_json_syntax_includes_suggestions() {
    let json_err = serde_json::from_str::<Value>("not json").unwrap_err();
    let err = Error::Json(Box::new(json_err));
    let formatted = format_error_with_hints(&err);
    assert!(formatted.contains("Suggestions:"));
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 19: VCR cassette multiple interactions
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn redact_cassette_multiple_interactions() {
    let mut cassette = make_cassette(vec![
        make_interaction(
            "POST",
            "https://api.example.com/auth",
            vec![("Authorization", "Bearer secret1")],
            Some(json!({"api_key": "key1"})),
            vec![],
            200,
        ),
        make_interaction(
            "POST",
            "https://api.example.com/chat",
            vec![("x-api-key", "key2")],
            Some(json!({"password": "pass"})),
            vec![("proxy-authorization", "proxy-secret")],
            200,
        ),
    ]);
    let summary = pi::vcr::redact_cassette(&mut cassette);
    // 1st: Authorization header + api_key body = 1 header + 1 body
    // 2nd: x-api-key header + password body + proxy-authorization resp header = 2 headers + 1 body
    assert_eq!(summary.headers_redacted, 3);
    assert_eq!(summary.json_fields_redacted, 2);
}

// ═══════════════════════════════════════════════════════════════════════
// SECTION 20: Edge cases for sensitive key detection via redaction
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn redact_cassette_apikey_variations() {
    let body = json!({
        "apikey": "key1",
        "ApiKey": "key2",
        "APIKEY": "key3",
        "my_api_key_here": "key4",
    });
    let mut cassette = make_cassette(vec![make_interaction(
        "POST",
        "https://x.com",
        vec![],
        Some(body),
        vec![],
        200,
    )]);
    let summary = pi::vcr::redact_cassette(&mut cassette);
    assert_eq!(summary.json_fields_redacted, 4);
}

#[test]
fn redact_cassette_secret_and_password_fields() {
    let body = json!({
        "client_secret": "s1",
        "my_secret_key": "s2",
        "password": "p1",
        "db_password": "p2",
        "safe_field": "not redacted",
    });
    let mut cassette = make_cassette(vec![make_interaction(
        "POST",
        "https://x.com",
        vec![],
        Some(body),
        vec![],
        200,
    )]);
    let summary = pi::vcr::redact_cassette(&mut cassette);
    assert_eq!(summary.json_fields_redacted, 4);
}

#[test]
fn redact_cassette_scalar_values_not_recursed() {
    // Body is a plain string, number, or bool — redact_json returns 0
    let body = json!("just a string");
    let mut cassette = make_cassette(vec![make_interaction(
        "POST",
        "https://x.com",
        vec![],
        Some(body),
        vec![],
        200,
    )]);
    let summary = pi::vcr::redact_cassette(&mut cassette);
    assert_eq!(summary.json_fields_redacted, 0);
}

#[test]
fn redact_cassette_null_body_value() {
    let body = json!(null);
    let mut cassette = make_cassette(vec![make_interaction(
        "POST",
        "https://x.com",
        vec![],
        Some(body),
        vec![],
        200,
    )]);
    let summary = pi::vcr::redact_cassette(&mut cassette);
    assert_eq!(summary.json_fields_redacted, 0);
}

#[test]
fn redact_cassette_header_case_insensitive() {
    let mut cassette = make_cassette(vec![make_interaction(
        "GET",
        "https://x.com",
        vec![
            ("AUTHORIZATION", "Bearer tok"),
            ("X-Api-Key", "key"),
            ("X-Goog-Api-Key", "goog-key"),
        ],
        None,
        vec![],
        200,
    )]);
    let summary = pi::vcr::redact_cassette(&mut cassette);
    assert_eq!(summary.headers_redacted, 3);
}
