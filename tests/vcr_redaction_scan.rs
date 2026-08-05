//! Per-provider VCR cassette redaction scan.
//!
//! Scans all VCR cassettes in `tests/fixtures/vcr/` to verify that no
//! unredacted secrets (API keys, tokens, passwords, etc.) appear in
//! recorded request/response data. This closes the evidence gap
//! identified as DISC-018 in the provider discrepancy classification.
//!
//! Bead: bd-2vlau

mod common;

use common::logging::find_unredacted_keys;
use std::collections::BTreeMap;
use std::path::Path;

const VCR_DIR: &str = "tests/fixtures/vcr";
const REDACTED: &str = "[REDACTED]";

/// Cassettes whose **response** `body_chunks` intentionally contain
/// unredacted sensitive-looking keys with synthetic test values.
/// Each entry is `(cassette_filename, path_suffix)`.
const RESPONSE_BODY_ALLOWLIST: [(&str, &str); 2] = [
    ("oauth_refresh_success.json", "access_token"),
    ("oauth_refresh_success.json", "refresh_token"),
];

/// Known sensitive HTTP headers (case-insensitive).
/// Matches the set in `src/vcr.rs` `SENSITIVE_HEADERS`.
const SENSITIVE_HEADERS: [&str; 6] = [
    "authorization",
    "x-api-key",
    "api-key",
    "x-goog-api-key",
    "x-azure-api-key",
    "proxy-authorization",
];

/// Extract provider name from a VCR cassette filename.
/// e.g., `anthropic_auth_failure_401.json` -> "anthropic"
///       `verify_bedrock_simple_text.json` -> "bedrock"
fn provider_from_filename(filename: &str) -> &str {
    let name = filename.strip_suffix(".json").unwrap_or(filename);
    // Handle "verify_" prefix
    let name = name.strip_prefix("verify_").unwrap_or(name);
    // Provider is the first segment before '_'
    name.split('_').next().unwrap_or(name)
}

/// Check whether a header value is safely redacted.
/// Accepts: `[REDACTED]`, `Bearer [REDACTED]`, `token [REDACTED]`, etc.
/// The auth scheme prefix (Bearer, token, Basic) is not sensitive — only the
/// credential portion must be redacted.
fn is_header_value_redacted(value: &str) -> bool {
    value == REDACTED || value.ends_with(REDACTED)
}

/// Check that all header values for sensitive header keys are redacted.
fn check_headers_redacted(headers: &serde_json::Value, path: &str) -> Vec<String> {
    let mut violations = Vec::new();
    if let Some(arr) = headers.as_array() {
        for (i, pair) in arr.iter().enumerate() {
            if let Some(pair_arr) = pair.as_array()
                && pair_arr.len() >= 2
                && let (Some(key), Some(value)) = (pair_arr[0].as_str(), pair_arr[1].as_str())
            {
                let key_lower = key.to_ascii_lowercase();
                if SENSITIVE_HEADERS.iter().any(|h| key_lower == *h)
                    && !is_header_value_redacted(value)
                {
                    violations.push(format!("{path}[{i}]: header '{key}' has unredacted value"));
                }
            }
        }
    }
    violations
}

/// Scan a single VCR cassette for unredacted secrets.
fn scan_cassette(cassette: &serde_json::Value, filename: &str) -> Vec<String> {
    let mut violations = Vec::new();

    let Some(interactions) = cassette.get("interactions").and_then(|v| v.as_array()) else {
        return violations;
    };

    for (idx, interaction) in interactions.iter().enumerate() {
        let prefix = format!("{filename}:interactions[{idx}]");

        // Check request headers
        if let Some(headers) = interaction.pointer("/request/headers") {
            violations.extend(check_headers_redacted(
                headers,
                &format!("{prefix}.request.headers"),
            ));
        }

        // Check response headers
        if let Some(headers) = interaction.pointer("/response/headers") {
            violations.extend(check_headers_redacted(
                headers,
                &format!("{prefix}.response.headers"),
            ));
        }

        // Check request body for unredacted JSON fields
        if let Some(body) = interaction.pointer("/request/body") {
            let body_leaks = find_unredacted_keys(body);
            for leak in body_leaks {
                violations.push(format!("{prefix}.request.body.{leak}"));
            }
        }

        // Check response body_chunks for unredacted JSON fields
        if let Some(chunks) = interaction.pointer("/response/body_chunks")
            && let Some(arr) = chunks.as_array()
        {
            for (ci, chunk) in arr.iter().enumerate() {
                if let Some(chunk_str) = chunk.as_str()
                    && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(chunk_str)
                {
                    let chunk_leaks = find_unredacted_keys(&parsed);
                    for leak in chunk_leaks {
                        // Skip allowlisted response-body fields (synthetic test values).
                        let is_allowed = RESPONSE_BODY_ALLOWLIST
                            .iter()
                            .any(|(f, suffix)| *f == filename && leak.ends_with(suffix));
                        if !is_allowed {
                            violations.push(format!("{prefix}.response.body_chunks[{ci}].{leak}"));
                        }
                    }
                }
            }
        }
    }

    violations
}

#[test]
fn all_vcr_cassettes_have_no_unredacted_secrets() {
    let vcr_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(VCR_DIR);
    assert!(
        vcr_path.exists(),
        "VCR directory does not exist: {}",
        vcr_path.display()
    );

    let mut all_violations: Vec<String> = Vec::new();
    let mut cassette_count = 0;

    for entry in std::fs::read_dir(&vcr_path).expect("read VCR dir") {
        let entry = entry.expect("read dir entry");
        let path = entry.path();

        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read cassette {filename}: {e}"));

        let cassette: serde_json::Value = serde_json::from_str(&content)
            .unwrap_or_else(|e| panic!("parse cassette {filename}: {e}"));

        let violations = scan_cassette(&cassette, filename);
        all_violations.extend(violations);
        cassette_count += 1;
    }

    assert!(
        cassette_count > 0,
        "No VCR cassettes found in {}",
        vcr_path.display()
    );

    if !all_violations.is_empty() {
        let report = all_violations.join("\n  ");
        panic!(
            "Found {} unredacted secret(s) across {cassette_count} cassettes:\n  {report}",
            all_violations.len()
        );
    }

    eprintln!("Scanned {cassette_count} VCR cassettes — all clean");
}

#[test]
fn per_provider_cassette_coverage() {
    let vcr_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(VCR_DIR);

    let mut provider_counts: BTreeMap<String, usize> = BTreeMap::new();

    for entry in std::fs::read_dir(&vcr_path).expect("read VCR dir") {
        let entry = entry.expect("read dir entry");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let provider = provider_from_filename(filename);
        *provider_counts.entry(provider.to_string()).or_default() += 1;
    }

    // Verify minimum provider coverage
    let required_providers = ["anthropic", "openai", "azure", "gemini"];
    for provider in &required_providers {
        assert!(
            provider_counts.contains_key(*provider),
            "No VCR cassettes found for provider '{provider}'"
        );
    }

    eprintln!("Provider cassette coverage:");
    for (provider, count) in &provider_counts {
        eprintln!("  {provider}: {count} cassettes");
    }
}

#[test]
fn scan_detects_deliberately_unredacted_cassette() {
    // Construct a synthetic cassette with an unredacted API key
    // to verify the scanner actually catches violations.
    let cassette = serde_json::json!({
        "version": "1.0",
        "test_name": "synthetic_leak_test",
        "interactions": [
            {
                "request": {
                    "method": "POST",
                    "url": "https://api.example.com/v1/chat",
                    "headers": [
                        ["Authorization", "Bearer sk-live-leaked-key-12345"],
                        ["Content-Type", "application/json"]
                    ],
                    "body": {
                        "api_key": "sk-another-leaked-key",
                        "model": "test-model",
                        "messages": []
                    }
                },
                "response": {
                    "status": 200,
                    "headers": [],
                    "body_chunks": [
                        "{\"token\": \"leaked-response-token\", \"data\": \"safe\"}"
                    ]
                }
            }
        ]
    });

    let violations = scan_cassette(&cassette, "synthetic_leak_test.json");

    // Should detect: Authorization header, api_key in body, token in response
    assert!(
        violations.len() >= 3,
        "Expected at least 3 violations, got {}: {:?}",
        violations.len(),
        violations
    );

    // Check specific violations
    assert!(
        violations.iter().any(|v| v.contains("Authorization")),
        "Should detect unredacted Authorization header"
    );
    assert!(
        violations.iter().any(|v| v.contains("api_key")),
        "Should detect unredacted api_key in body"
    );
    assert!(
        violations.iter().any(|v| v.contains("token")),
        "Should detect unredacted token in response"
    );
}

#[test]
fn scan_passes_properly_redacted_cassette() {
    let cassette = serde_json::json!({
        "version": "1.0",
        "test_name": "synthetic_clean_test",
        "interactions": [
            {
                "request": {
                    "method": "POST",
                    "url": "https://api.example.com/v1/chat",
                    "headers": [
                        ["Authorization", "Bearer [REDACTED]"],
                        ["Content-Type", "application/json"],
                        ["X-API-Key", "[REDACTED]"]
                    ],
                    "body": {
                        "api_key": "[REDACTED]",
                        "model": "test-model",
                        "messages": [{"role": "user", "content": "hello"}]
                    }
                },
                "response": {
                    "status": 200,
                    "headers": [],
                    "body_chunks": [
                        "{\"token\": \"[REDACTED]\", \"text\": \"response\"}"
                    ]
                }
            }
        ]
    });

    let violations = scan_cassette(&cassette, "synthetic_clean_test.json");
    assert!(
        violations.is_empty(),
        "Properly redacted cassette should have no violations: {violations:?}"
    );
}
