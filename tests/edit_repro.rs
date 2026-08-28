use asupersync::test_utils;
use pi::model::ContentBlock;
use pi::tools::{EditTool, Tool};
use serde_json::json;
use unicode_normalization::UnicodeNormalization;

fn get_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn test_edit_unicode_normalization_mismatch() {
    test_utils::run_test(|| async {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("unicode.txt");

        // File has NFD (Decomposed)
        let decomposed_text = "café".nfd().collect::<String>();
        std::fs::write(&file_path, &decomposed_text).unwrap();

        let tool = EditTool::new(tmp.path());

        // User provides NFC (Composed) - typical from keyboard/clipboard
        let composed_text = "café".nfc().collect::<String>();
        assert_ne!(
            decomposed_text, composed_text,
            "NFD and NFC bytes should differ"
        );

        let out = tool
            .execute(
                "t",
                json!({
                    "path": file_path.to_string_lossy(),
                    "oldText": composed_text,
                    "newText": "done"
                }),
                None,
            )
            .await;

        let result = out.expect("NFC oldText should match NFD file content");
        let output = get_text(&result.content);
        assert!(
            output.contains("Successfully replaced text"),
            "unexpected edit output: {output}"
        );
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "done");
    });
}
