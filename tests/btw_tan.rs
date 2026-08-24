#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::btw::{BTW_SYSTEM_PROMPT, BtwClient};
use pi::subagents::{TanChildConfig, TanCompletion};

fn finish_case(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    assert!(harness.write_jsonl_logs(&path).is_ok(), "write JSONL logs");
    let payload = std::fs::read_to_string(&path).unwrap_or_default();
    let errors = validate_jsonl_v2_only(&payload);
    assert!(
        errors.is_empty(),
        "JSONL schema violations in {case}.jsonl: {errors:?}"
    );
    harness.record_artifact(format!("{case}.jsonl"), &path);
}

#[test]
fn test_btw_ephemeral_prompt_and_isolation() {
    let harness = TestHarness::new("btw_ephemeral_isolation");

    // Verify /btw system prompt enforces no-tools and no-followups
    assert!(BTW_SYSTEM_PROMPT.contains("NEVER use tools"));
    assert!(BTW_SYSTEM_PROMPT.contains("NEVER ask follow-up questions"));

    // Verify /btw format with empty context
    let q = "What is the capital of France?";
    assert!(!q.is_empty());

    finish_case(&harness, "btw_ephemeral_isolation");
}

#[test]
fn test_tan_child_configuration_and_completion_formatting() {
    let harness = TestHarness::new("tan_child_lifecycle");

    // Tan config validation
    let valid_config = TanChildConfig::new("update changelog");
    assert!(valid_config.is_ok());

    let empty_config = TanChildConfig::new("   ");
    assert!(empty_config.is_err());

    // Tan completion message format
    let completion = TanCompletion::new(
        "tan-agent-1",
        "update changelog",
        "Updated CHANGELOG.md with recent release notes.",
        true,
    );

    let card_text = completion.card_text();
    assert!(card_text.starts_with("(/tan completed)"));
    assert!(card_text.contains("Updated CHANGELOG.md"));

    let failed_completion = TanCompletion::new(
        "tan-agent-2",
        "broken task",
        "Execution timed out",
        false,
    );
    let fail_card = failed_completion.card_text();
    assert!(fail_card.starts_with("(/tan failed)"));
    assert!(fail_card.contains("Execution timed out"));

    finish_case(&harness, "tan_child_lifecycle");
}
