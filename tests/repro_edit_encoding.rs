use pi::tools::{EditTool, Tool};
use serde_json::json;
use std::fs::File;
use std::io::Write;
use tempfile::tempdir;

#[test]
fn test_edit_tool_fails_on_invalid_utf8() {
    asupersync::test_utils::run_test(|| async {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.bin");

        // Create a file with invalid UTF-8 (0xFF byte)
        let original_bytes = b"Hello \xFF World";
        {
            let mut f = File::create(&file_path).unwrap();
            f.write_all(original_bytes).unwrap();
        }

        let tool = EditTool::new(dir.path());

        // Attempt to replace "Hello" with "Hi"
        let input = json!({
            "path": "test.bin",
            "oldText": "Hello",
            "newText": "Hi"
        });

        let result = tool.execute("call1", input, None).await;

        assert!(
            result.is_err(),
            "Edit tool should return error for invalid UTF-8"
        );

        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("invalid UTF-8"),
            "Error message should mention UTF-8: {err}",
        );

        // Read back bytes to ensure NO corruption occurred
        let new_bytes = std::fs::read(&file_path).unwrap();
        assert_eq!(
            new_bytes, original_bytes,
            "File content should remain unchanged"
        );
    });
}
