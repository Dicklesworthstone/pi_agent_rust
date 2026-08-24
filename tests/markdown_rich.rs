#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::markdown_rich::{
    format_osc8_link, latex_to_unicode, render_hex_swatches, render_mermaid_diagram,
    HighlightLanguage,
};

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
fn test_syntax_highlight_languages() {
    let harness = TestHarness::new("syntax_highlight_languages");

    assert_eq!(HighlightLanguage::from_fence_tag("rust"), HighlightLanguage::Rust);
    assert_eq!(HighlightLanguage::from_fence_tag("rs"), HighlightLanguage::Rust);
    assert_eq!(HighlightLanguage::from_fence_tag("python"), HighlightLanguage::Python);
    assert_eq!(HighlightLanguage::from_fence_tag("javascript"), HighlightLanguage::JavaScript);
    assert_eq!(HighlightLanguage::from_fence_tag("typescript"), HighlightLanguage::TypeScript);
    assert_eq!(HighlightLanguage::from_fence_tag("bash"), HighlightLanguage::Bash);
    assert_eq!(HighlightLanguage::from_fence_tag("go"), HighlightLanguage::Go);
    assert_eq!(HighlightLanguage::from_fence_tag("json"), HighlightLanguage::Json);
    assert_eq!(HighlightLanguage::from_fence_tag("toml"), HighlightLanguage::Toml);
    assert_eq!(HighlightLanguage::from_fence_tag("yaml"), HighlightLanguage::Yaml);
    assert_eq!(HighlightLanguage::from_fence_tag("diff"), HighlightLanguage::Diff);
    assert_eq!(HighlightLanguage::from_fence_tag("unknown_lang"), HighlightLanguage::Plain);

    finish_case(&harness, "syntax_highlight_languages");
}

#[test]
fn test_math_and_hex_swatches() {
    let harness = TestHarness::new("math_and_hex_swatches");

    let math_input = r"\sum_{i=1}^n x_i \approx \sqrt{\pi} \pm \delta";
    let math_rendered = latex_to_unicode(math_input);
    assert!(math_rendered.contains("∑"));
    assert!(math_rendered.contains("≈"));
    assert!(math_rendered.contains("√"));
    assert!(math_rendered.contains("π"));
    assert!(math_rendered.contains("±"));
    assert!(math_rendered.contains("δ"));

    let hex_input = "Background is #1e1e2e and foreground is #cdd6f4.";
    let hex_swatched = render_hex_swatches(hex_input);
    assert!(hex_swatched.contains("■ #1e1e2e"));
    assert!(hex_swatched.contains("■ #cdd6f4"));

    finish_case(&harness, "math_and_hex_swatches");
}

#[test]
fn test_osc8_links_and_mermaid() {
    let harness = TestHarness::new("osc8_links_and_mermaid");

    let link = format_osc8_link("file:///path/to/file.rs", "file.rs");
    assert!(link.starts_with("\x1b]8;;file:///path/to/file.rs\x1b\\"));
    assert!(link.ends_with("\x1b]8;;\x1b\\"));

    let mermaid = "graph LR\n  A[Start] --> B[Processing]\n  B --> C[Finish]";
    let rendered_mermaid = render_mermaid_diagram(mermaid, 80);
    assert!(rendered_mermaid.contains("Mermaid Diagram"));
    assert!(rendered_mermaid.contains("Start"));
    assert!(rendered_mermaid.contains("Processing"));

    finish_case(&harness, "osc8_links_and_mermaid");
}
