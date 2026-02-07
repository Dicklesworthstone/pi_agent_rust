//! Validation tests for GitHub code search extension discovery (bd-3l39).
//!
//! These tests verify the signature-matching rules used to identify true Pi
//! extensions from code search results.  They run entirely offline against
//! fixture data — no network access required.

mod common;

use serde_json::Value;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns `true` if the file content looks like a real Pi extension.
fn is_true_extension(content: &str) -> bool {
    let has_api_import = content.contains("@mariozechner/pi-coding-agent")
        || content.contains("@mariozechner/pi-ai")
        || content.contains("ExtensionAPI");

    let has_export_default = content.contains("export default");

    let has_registration = [
        "registerTool(",
        "registerCommand(",
        "registerProvider(",
        "registerFlag(",
        "registerShortcut(",
        "registerMessageRenderer(",
    ]
    .iter()
    .any(|pat| content.contains(pat));

    has_api_import && (has_export_default || has_registration)
}

/// Extracts which registration calls appear in the content.
fn extract_registrations(content: &str) -> Vec<&'static str> {
    let registrations = [
        "registerTool",
        "registerCommand",
        "registerProvider",
        "registerFlag",
        "registerShortcut",
        "registerMessageRenderer",
    ];
    registrations
        .iter()
        .copied()
        .filter(|reg| {
            let pattern = format!("{reg}(");
            content.contains(&pattern)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// True-positive tests: known extension content patterns
// ---------------------------------------------------------------------------

#[test]
fn detects_basic_extension_with_export_default() {
    let content = r#"
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function init(api: ExtensionAPI) {
    api.registerTool({ name: "hello", description: "Hello world" });
}
"#;
    assert!(is_true_extension(content));
    assert_eq!(extract_registrations(content), vec!["registerTool"]);
}

#[test]
fn detects_extension_with_pi_ai_import() {
    let content = r#"
import { ExtensionAPI } from "@mariozechner/pi-ai";

export default (api: ExtensionAPI) => {
    api.registerCommand({ name: "/greet", handler: () => {} });
};
"#;
    assert!(is_true_extension(content));
    assert_eq!(extract_registrations(content), vec!["registerCommand"]);
}

#[test]
fn detects_extension_with_multiple_registrations() {
    let content = r#"
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function init(api: ExtensionAPI) {
    api.registerTool({ name: "review" });
    api.registerCommand({ name: "/review" });
    api.registerFlag({ name: "verbose" });
    api.registerShortcut({ key: "ctrl+r" });
}
"#;
    assert!(is_true_extension(content));
    let regs = extract_registrations(content);
    assert_eq!(
        regs,
        vec![
            "registerTool",
            "registerCommand",
            "registerFlag",
            "registerShortcut"
        ]
    );
}

#[test]
fn detects_extension_with_provider_registration() {
    let content = r#"
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function init(api: ExtensionAPI) {
    api.registerProvider({
        name: "my-llm",
        streamSimple: async function* (model, context) { yield "hello"; }
    });
}
"#;
    assert!(is_true_extension(content));
    assert_eq!(extract_registrations(content), vec!["registerProvider"]);
}

#[test]
fn detects_extension_without_export_default_but_with_registration() {
    // Some extensions use named exports with registration
    let content = r#"
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

function setup(api: ExtensionAPI) {
    api.registerTool({ name: "test" });
    api.registerCommand({ name: "/test" });
}

module.exports = setup;
"#;
    assert!(is_true_extension(content));
}

#[test]
fn detects_extension_with_message_renderer() {
    let content = r#"
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default (api: ExtensionAPI) => {
    api.registerMessageRenderer({ pattern: /```mermaid/ });
};
"#;
    assert!(is_true_extension(content));
    assert_eq!(
        extract_registrations(content),
        vec!["registerMessageRenderer"]
    );
}

// ---------------------------------------------------------------------------
// False-positive suppression tests
// ---------------------------------------------------------------------------

#[test]
fn rejects_file_that_mentions_pi_but_is_not_extension() {
    // Documentation file that mentions Pi but has no extension API
    let content = r"
# Pi Extensions Guide

You can create extensions for the Pi coding agent.
Import `ExtensionAPI` from `@mariozechner/pi-coding-agent`.

## Not an actual extension
This is just documentation.
";
    // Has ExtensionAPI mention but no export default or registration
    assert!(!is_true_extension(content));
}

#[test]
fn rejects_test_file_without_extension_pattern() {
    let content = r#"
import { describe, it, expect } from "vitest";

describe("Extension tests", () => {
    it("should load the extension", () => {
        expect(true).toBe(true);
    });
});
"#;
    assert!(!is_true_extension(content));
}

#[test]
fn rejects_file_with_only_export_default_no_api() {
    let content = r#"
export default function hello() {
    console.log("hello world");
}
"#;
    assert!(!is_true_extension(content));
}

#[test]
fn rejects_file_with_only_registration_no_api() {
    // Has registerTool but no ExtensionAPI import
    let content = r#"
function registerTool(name: string) {
    return { name };
}

registerTool("my-tool");
"#;
    assert!(!is_true_extension(content));
}

// ---------------------------------------------------------------------------
// Known-corpus regression test
// ---------------------------------------------------------------------------

#[test]
fn known_entrypoints_fixture_is_valid_json() {
    let fixture_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/known_extension_entrypoints.json"
    );
    let content = std::fs::read_to_string(fixture_path).expect("read fixture");
    let data: Value = serde_json::from_str(&content).expect("parse JSON");
    let entrypoints = data["entrypoints"].as_array().expect("entrypoints array");
    assert!(
        entrypoints.len() >= 5,
        "expected at least 5 known entrypoints, got {}",
        entrypoints.len()
    );

    for ep in entrypoints {
        assert!(
            ep["repo"].as_str().is_some(),
            "entrypoint missing repo field"
        );
        assert!(
            ep["path"].as_str().is_some(),
            "entrypoint missing path field"
        );
        // Repo should be owner/name format
        let repo = ep["repo"].as_str().unwrap();
        assert!(
            repo.contains('/'),
            "repo should be owner/name format: {repo}"
        );
    }
}

#[test]
fn search_inventory_file_is_valid() {
    let inv_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/extension-code-search-summary.json"
    );
    let content = std::fs::read_to_string(inv_path).expect("read inventory");
    let data: Value = serde_json::from_str(&content).expect("parse JSON");

    assert_eq!(data["task"].as_str(), Some("bd-3l39"));
    assert!(
        data["queries_executed"].as_u64().unwrap_or(0) >= 8,
        "expected at least 8 queries executed"
    );
    assert!(
        data["total_repos_searched"].as_u64().unwrap_or(0) >= 100,
        "expected at least 100 repos searched"
    );
    assert!(
        data["validated_extensions"].as_u64().unwrap_or(0) >= 50,
        "expected at least 50 validated extensions"
    );

    let repos = data["repos"].as_array().expect("repos array");
    for repo in repos {
        assert!(repo["repo"].as_str().is_some());
        assert!(repo["entrypoint"].as_str().is_some());
    }
}

#[test]
fn search_inventory_repos_have_valid_structure() {
    let inv_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/extension-code-search-summary.json"
    );
    let content = std::fs::read_to_string(inv_path).expect("read inventory");
    let data: Value = serde_json::from_str(&content).expect("parse JSON");
    let repos = data["repos"].as_array().expect("repos array");

    // All repos should have owner/name format
    for repo in repos {
        let name = repo["repo"].as_str().unwrap();
        assert!(name.contains('/'), "invalid repo format: {name}");
        assert!(!name.starts_with('/'), "repo starts with /: {name}");
    }

    // No duplicate repos
    let mut seen = std::collections::HashSet::new();
    for repo in repos {
        let name = repo["repo"].as_str().unwrap();
        assert!(seen.insert(name), "duplicate repo: {name}");
    }
}

#[test]
fn registration_api_breakdown_is_present() {
    let inv_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/extension-code-search-summary.json"
    );
    let content = std::fs::read_to_string(inv_path).expect("read inventory");
    let data: Value = serde_json::from_str(&content).expect("parse JSON");
    let breakdown = data["registration_api_breakdown"]
        .as_object()
        .expect("breakdown object");

    // Should have entries for the main registration APIs
    for api in &["registerTool", "registerCommand"] {
        assert!(breakdown.contains_key(*api), "missing {api} in breakdown");
        let count = breakdown[*api].as_u64().unwrap_or(0);
        assert!(count > 0, "{api} should have non-zero count");
    }
}
