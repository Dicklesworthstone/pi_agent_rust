#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::gallery::{ComponentState, GalleryCategory, GalleryMatrix};

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
fn test_gallery_matrix_schema_and_categories() {
    let harness = TestHarness::new("gallery_matrix_schema");

    let matrix = GalleryMatrix::new();
    assert_eq!(matrix.schema, "pi.gallery.matrix.v1");
    assert!(matrix.items.len() >= 6);

    let has_tool_card = matrix.items.iter().any(|i| i.category == GalleryCategory::ToolCard);
    let has_status_line = matrix.items.iter().any(|i| i.category == GalleryCategory::StatusLine);
    let has_overlay = matrix.items.iter().any(|i| i.category == GalleryCategory::Overlay);
    let has_delight = matrix.items.iter().any(|i| i.category == GalleryCategory::Delight);

    assert!(has_tool_card);
    assert!(has_status_line);
    assert!(has_overlay);
    assert!(has_delight);

    let report_json = matrix.render_report_json();
    assert!(report_json.contains("pi.gallery.matrix.v1"));

    finish_case(&harness, "gallery_matrix_schema");
}
