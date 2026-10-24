//! Public document-converter coverage using a saved Jupyter-format fixture.
use pi::url_read::{UrlContentKind, convert_bytes};

const NOTEBOOK: &[u8] = include_bytes!("fixtures/url_notebook_results.ipynb");

#[test]
fn public_reader_exposes_saved_results_not_just_output_counts() {
    let out = convert_bytes(
        "https://example.com/analysis.ipynb",
        "application/json",
        NOTEBOOK,
        false,
    )
    .unwrap();
    assert_eq!(out.kind, UrlContentKind::Notebook);
    assert_eq!(out.extractor, "notebook");
    assert!(out.content.contains("loaded three rows"));
    assert!(out.content.contains("\"total\": 42"));
    assert!(out.content.contains("ValueError: saved failure"));
    assert!(out.content.contains("code was not executed"));
    assert!(!out.content.contains("aW1hZ2UtcGF5"));
    assert!(!out.content.contains('\u{1b}'));
}

#[test]
fn json_mime_and_bom_do_not_hide_extensionless_notebooks() {
    let mut bytes = b"\xef\xbb\xbf \n".to_vec();
    bytes.extend_from_slice(NOTEBOOK);
    let out = convert_bytes(
        "https://example.com/download?id=1",
        "application/json; charset=utf-8",
        &bytes,
        false,
    )
    .unwrap();
    assert_eq!(out.kind, UrlContentKind::Notebook);
    assert!(out.content.contains("Saved analysis"));
    let plain = convert_bytes(
        "https://example.com/data",
        "application/json",
        br#"{"text":"nbformat"}"#,
        false,
    )
    .unwrap();
    assert_eq!(plain.kind, UrlContentKind::PlainText);
}

#[test]
fn known_incomplete_or_unsupported_notebooks_never_become_valid_results() {
    let error = convert_bytes(
        "https://example.com/a.ipynb",
        "application/json",
        NOTEBOOK,
        true,
    )
    .unwrap_err();
    assert!(error.to_string().contains("NOTEBOOK_INCOMPLETE"));
    assert!(
        convert_bytes(
            "https://example.com/a",
            "application/x-ipynb+json",
            br#"{"nbformat":3,"worksheets":[]}"#,
            false
        )
        .is_err()
    );
}
