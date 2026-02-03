//! Conformance tests for built-in tools.
//!
//! These tests verify that the Rust tool implementations match the
//! behavior of the original TypeScript implementations.

use pi::tools::Tool;

mod read_tool {
    use super::*;

    #[tokio::test]
    async fn test_read_existing_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_file = temp_dir.path().join("test.txt");
        std::fs::write(&test_file, "line1\nline2\nline3\nline4\nline5").unwrap();

        let tool = pi::tools::ReadTool::new(temp_dir.path());
        let input = serde_json::json!({
            "path": test_file.to_string_lossy()
        });

        let result = tool
            .execute("test-id", input, None)
            .await
            .expect("should succeed");

        let text = get_text_content(&result.content);
        // Line numbers are right-aligned to 5 chars with arrow separator (cat -n style)
        assert_eq!(
            text,
            "    1→line1\n    2→line2\n    3→line3\n    4→line4\n    5→line5"
        );
        assert!(result.details.is_none());
    }

    #[tokio::test]
    async fn test_read_with_offset_and_limit() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_file = temp_dir.path().join("test.txt");
        std::fs::write(&test_file, "line1\nline2\nline3\nline4\nline5").unwrap();

        let tool = pi::tools::ReadTool::new(temp_dir.path());
        let input = serde_json::json!({
            "path": test_file.to_string_lossy(),
            "offset": 2,
            "limit": 2
        });

        let result = tool
            .execute("test-id", input, None)
            .await
            .expect("should succeed");

        let text = get_text_content(&result.content);
        assert!(text.contains("line2"));
        assert!(text.contains("line3"));
        assert!(text.contains("[2 more lines in file. Use offset=4 to continue.]"));
        assert!(result.details.is_none());
    }

    #[tokio::test]
    async fn test_read_nonexistent_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let tool = pi::tools::ReadTool::new(temp_dir.path());
        let input = serde_json::json!({
            "path": "/nonexistent/path/file.txt"
        });

        let result = tool.execute("test-id", input, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let tool = pi::tools::ReadTool::new(temp_dir.path());
        let input = serde_json::json!({
            "path": temp_dir.path().to_string_lossy()
        });

        let result = tool.execute("test-id", input, None).await;
        assert!(result.is_err());
    }
}

mod write_tool {
    use super::*;

    #[tokio::test]
    async fn test_write_new_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_file = temp_dir.path().join("new_file.txt");
        let content = "Hello, World!\nLine 2";

        let tool = pi::tools::WriteTool::new(temp_dir.path());
        let input = serde_json::json!({
            "path": test_file.to_string_lossy(),
            "content": content
        });

        let result = tool
            .execute("test-id", input, None)
            .await
            .expect("should succeed");

        // Verify file was created
        assert!(test_file.exists());
        assert_eq!(std::fs::read_to_string(&test_file).unwrap(), content);

        let text = get_text_content(&result.content);
        assert!(text.contains("Successfully wrote 20 bytes"));
        assert!(result.details.is_none());
    }

    #[tokio::test]
    async fn test_write_creates_directories() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_file = temp_dir.path().join("nested/dir/file.txt");

        let tool = pi::tools::WriteTool::new(temp_dir.path());
        let input = serde_json::json!({
            "path": test_file.to_string_lossy(),
            "content": "content"
        });

        let result = tool.execute("test-id", input, None).await;
        assert!(result.is_ok());
        assert!(test_file.exists());
    }
}

mod edit_tool {
    use super::*;

    #[tokio::test]
    async fn test_edit_replace_text() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_file = temp_dir.path().join("test.txt");
        std::fs::write(&test_file, "Hello, World!\nHow are you?").unwrap();

        let tool = pi::tools::EditTool::new(temp_dir.path());
        let input = serde_json::json!({
            "path": test_file.to_string_lossy(),
            "oldText": "World",
            "newText": "Rust"
        });

        let result = tool
            .execute("test-id", input, None)
            .await
            .expect("should succeed");

        // Verify file was edited
        let content = std::fs::read_to_string(&test_file).unwrap();
        assert!(content.contains("Rust"));
        assert!(!content.contains("World"));

        // Verify success message output
        let text = get_text_content(&result.content);
        assert!(text.contains("Successfully replaced text in"));
        assert!(text.contains("test.txt"));
        assert!(
            result
                .details
                .as_ref()
                .is_some_and(|d| d.get("diff").is_some())
        );
    }

    #[tokio::test]
    async fn test_edit_text_not_found() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_file = temp_dir.path().join("test.txt");
        std::fs::write(&test_file, "Hello, World!").unwrap();

        let tool = pi::tools::EditTool::new(temp_dir.path());
        let input = serde_json::json!({
            "path": test_file.to_string_lossy(),
            "oldText": "NotFound",
            "newText": "New"
        });

        let result = tool.execute("test-id", input, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_edit_multiple_occurrences() {
        let temp_dir = tempfile::tempdir().unwrap();
        let test_file = temp_dir.path().join("test.txt");
        std::fs::write(&test_file, "Hello, Hello, Hello!").unwrap();

        let tool = pi::tools::EditTool::new(temp_dir.path());
        let input = serde_json::json!({
            "path": test_file.to_string_lossy(),
            "oldText": "Hello",
            "newText": "Hi"
        });

        let result = tool.execute("test-id", input, None).await;
        assert!(result.is_err()); // Should fail due to multiple occurrences
    }
}

mod bash_tool {
    use super::*;

    #[tokio::test]
    async fn test_bash_simple_command() {
        let temp_dir = tempfile::tempdir().unwrap();
        let tool = pi::tools::BashTool::new(temp_dir.path());
        let input = serde_json::json!({
            "command": "echo 'Hello, World!'"
        });

        let result = tool
            .execute("test-id", input, None)
            .await
            .expect("should succeed");

        let text = get_text_content(&result.content);
        assert!(text.contains("Hello, World!"));
        assert!(result.details.is_none());
    }

    #[tokio::test]
    async fn test_bash_exit_code() {
        let temp_dir = tempfile::tempdir().unwrap();
        let tool = pi::tools::BashTool::new(temp_dir.path());
        let input = serde_json::json!({
            "command": "exit 42"
        });

        let err = tool
            .execute("test-id", input, None)
            .await
            .expect_err("should error");
        assert!(err.to_string().contains("Command exited with code 42"));
    }

    #[tokio::test]
    async fn test_bash_working_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("test.txt"), "content").unwrap();

        let tool = pi::tools::BashTool::new(temp_dir.path());
        let input = serde_json::json!({
            "command": "ls test.txt"
        });

        let result = tool.execute("test-id", input, None).await;
        assert!(result.is_ok());
    }
}

mod grep_tool {
    use super::*;

    #[tokio::test]
    async fn test_grep_basic_pattern() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            temp_dir.path().join("test.txt"),
            "hello world\ngoodbye world\nhello again",
        )
        .unwrap();

        let tool = pi::tools::GrepTool::new(temp_dir.path());
        let input = serde_json::json!({
            "pattern": "hello"
        });

        let result = tool
            .execute("test-id", input, None)
            .await
            .expect("should succeed");

        let text = get_text_content(&result.content);
        assert!(text.contains("hello world"));
        assert!(text.contains("hello again"));
        // Details are only present when limits/truncation occur
    }

    #[tokio::test]
    async fn test_grep_case_insensitive() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("test.txt"), "Hello World\nHELLO WORLD").unwrap();

        let tool = pi::tools::GrepTool::new(temp_dir.path());
        let input = serde_json::json!({
            "pattern": "hello",
            "ignoreCase": true
        });

        let result = tool
            .execute("test-id", input, None)
            .await
            .expect("should succeed");

        let text = get_text_content(&result.content);
        assert!(text.contains("Hello World"));
        assert!(text.contains("HELLO WORLD"));
        // Details are only present when limits/truncation occur
    }

    #[tokio::test]
    async fn test_grep_no_matches() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("test.txt"), "hello world").unwrap();

        let tool = pi::tools::GrepTool::new(temp_dir.path());
        let input = serde_json::json!({
            "pattern": "notfound"
        });

        let result = tool
            .execute("test-id", input, None)
            .await
            .expect("should succeed");

        let text = get_text_content(&result.content);
        assert!(text.contains("No matches found"));
    }
}

mod find_tool {
    use super::*;

    #[tokio::test]
    async fn test_find_glob_pattern() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("file1.txt"), "").unwrap();
        std::fs::write(temp_dir.path().join("file2.txt"), "").unwrap();
        std::fs::write(temp_dir.path().join("file.rs"), "").unwrap();

        let tool = pi::tools::FindTool::new(temp_dir.path());
        let input = serde_json::json!({
            "pattern": "*.txt"
        });

        let result = tool
            .execute("test-id", input, None)
            .await
            .expect("should succeed");

        let text = get_text_content(&result.content);
        assert!(text.contains("file1.txt"));
        assert!(text.contains("file2.txt"));
        assert!(!text.contains("file.rs"));
        // Details are only present when limits/truncation occur
    }

    #[tokio::test]
    async fn test_find_no_matches() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("file.txt"), "").unwrap();

        let tool = pi::tools::FindTool::new(temp_dir.path());
        let input = serde_json::json!({
            "pattern": "*.rs"
        });

        let result = tool
            .execute("test-id", input, None)
            .await
            .expect("should succeed");

        let text = get_text_content(&result.content);
        assert!(text.contains("No files found"));
    }
}

mod ls_tool {
    use super::*;

    #[tokio::test]
    async fn test_ls_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("file.txt"), "content").unwrap();
        std::fs::create_dir(temp_dir.path().join("subdir")).unwrap();

        let tool = pi::tools::LsTool::new(temp_dir.path());
        let input = serde_json::json!({});

        let result = tool
            .execute("test-id", input, None)
            .await
            .expect("should succeed");

        let text = get_text_content(&result.content);
        assert!(text.contains("file.txt"));
        assert!(text.contains("subdir/"));
        // Details are only present when limits/truncation occur
    }

    #[tokio::test]
    async fn test_ls_nonexistent_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let tool = pi::tools::LsTool::new(temp_dir.path());
        let input = serde_json::json!({
            "path": "/nonexistent/directory"
        });

        let result = tool.execute("test-id", input, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_ls_empty_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let tool = pi::tools::LsTool::new(temp_dir.path());
        let input = serde_json::json!({});

        let result = tool
            .execute("test-id", input, None)
            .await
            .expect("should succeed");

        let text = get_text_content(&result.content);
        assert!(text.contains("empty directory"));
    }
}

// Helper function to extract text content from tool output
fn get_text_content(content: &[pi::model::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| {
            if let pi::model::ContentBlock::Text(text) = block {
                Some(text.text.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
