//! Conformance tests for built-in tools.
//!
//! These tests verify that the Rust tool implementations match the
//! behavior of the original TypeScript implementations.

mod common;

use common::TestHarness;
use pi::tools::Tool;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

mod read_tool {
    use super::*;

    #[test]
    fn test_read_existing_file() {
        asupersync::test_utils::run_test(|| async {
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
        });
    }

    #[test]
    fn test_read_with_offset_and_limit() {
        asupersync::test_utils::run_test(|| async {
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
        });
    }

    #[test]
    fn test_read_offset_beyond_eof_reports_error() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("read_offset_beyond_eof_reports_error");
            let path = harness.create_file("tiny.txt", b"line1\nline2");
            let tool = pi::tools::ReadTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "offset": 10
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("should error");
            let message = err.to_string();
            harness
                .log()
                .info_ctx("verify", "offset error message", |ctx| {
                    ctx.push(("message".into(), message.clone()));
                });
            assert!(message.contains("Offset 10 is beyond end of file"));
        });
    }

    #[test]
    fn test_read_first_line_exceeds_limit_sets_truncation_details() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("read_first_line_exceeds_limit_sets_truncation_details");
            let long_line = "a".repeat(pi::tools::DEFAULT_MAX_BYTES + 128);
            let path = harness.create_file("huge.txt", long_line.as_bytes());
            let tool = pi::tools::ReadTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy()
            });

            let result = tool
                .execute("test-id", input, None)
                .await
                .expect("should succeed with truncation guidance");
            let text = get_text_content(&result.content);
            assert!(text.contains("exceeds 50.0KB limit"));
            let details = result.details.expect("expected truncation details");
            let truncation = details
                .get("truncation")
                .expect("expected truncation object");
            assert_eq!(
                truncation.get("firstLineExceedsLimit"),
                Some(&serde_json::Value::Bool(true))
            );
        });
    }

    #[test]
    fn test_read_truncation_sets_details_and_hint() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("read_truncation_sets_details_and_hint");
            let total_lines = pi::tools::DEFAULT_MAX_LINES + 5;
            let lines: Vec<String> = (1..=total_lines).map(|i| format!("line{i}")).collect();
            let content = lines.join("\n");
            let path = harness.create_file("big.txt", content.as_bytes());
            let tool = pi::tools::ReadTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy()
            });

            let result = tool
                .execute("test-id", input, None)
                .await
                .expect("should truncate");
            let text = get_text_content(&result.content);
            let tail = text
                .lines()
                .rev()
                .take(6)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            let expected_hint = format!(
                "Showing lines 1-{} of {}",
                pi::tools::DEFAULT_MAX_LINES,
                total_lines
            );
            assert!(
                text.contains(&expected_hint),
                "expected hint not found.\nexpected: {expected_hint}\ntext tail:\n{tail}"
            );
            let expected_offset = format!("Use offset={}", pi::tools::DEFAULT_MAX_LINES + 1);
            assert!(
                text.contains(&expected_offset),
                "expected offset not found.\nexpected: {expected_offset}\ntext tail:\n{tail}"
            );
            let details = result.details.expect("expected truncation details");
            let truncation = details
                .get("truncation")
                .expect("expected truncation object");
            assert_eq!(
                truncation.get("truncatedBy"),
                Some(&serde_json::Value::String("lines".to_string()))
            );
        });
    }

    #[test]
    fn test_read_blocked_images_returns_error() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("read_blocked_images_returns_error");
            let path = harness.create_file("image.png", b"\x89PNG\r\n\x1A\n");
            let tool = pi::tools::ReadTool::with_settings(harness.temp_dir(), true, true);
            let input = serde_json::json!({
                "path": path.to_string_lossy()
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("should error");
            let message = err.to_string();
            harness
                .log()
                .info_ctx("verify", "blocked image error", |ctx| {
                    ctx.push(("message".into(), message.clone()));
                });
            assert!(message.contains("Images are blocked by configuration"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_read_permission_denied_is_reported() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("read_permission_denied_is_reported");
            let path = harness.create_file("secret.txt", b"top secret");
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o000);
            std::fs::set_permissions(&path, perms).unwrap();

            let tool = pi::tools::ReadTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy()
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("should error");
            let message = err.to_string();
            harness
                .log()
                .info_ctx("verify", "permission denied", |ctx| {
                    ctx.push(("message".into(), message.clone()));
                });
            assert!(message.contains("Tool error: read:"));
            assert!(message.to_lowercase().contains("permission"));
        });
    }

    #[test]
    fn test_read_nonexistent_file() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let tool = pi::tools::ReadTool::new(temp_dir.path());
            let input = serde_json::json!({
                "path": "/nonexistent/path/file.txt"
            });

            let result = tool.execute("test-id", input, None).await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn test_read_directory() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let tool = pi::tools::ReadTool::new(temp_dir.path());
            let input = serde_json::json!({
                "path": temp_dir.path().to_string_lossy()
            });

            let result = tool.execute("test-id", input, None).await;
            assert!(result.is_err());
        });
    }
}

mod write_tool {
    use super::*;

    #[test]
    fn test_write_new_file() {
        asupersync::test_utils::run_test(|| async {
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
        });
    }

    #[test]
    fn test_write_reports_utf16_byte_count() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("write_reports_utf16_byte_count");
            let test_file = harness.temp_path("utf16.txt");
            let content = "A😃";
            let expected = content.encode_utf16().count();

            let tool = pi::tools::WriteTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": test_file.to_string_lossy(),
                "content": content
            });

            let result = tool
                .execute("test-id", input, None)
                .await
                .expect("should succeed");

            let text = get_text_content(&result.content);
            assert!(text.contains(&format!("Successfully wrote {expected} bytes")));
            assert_eq!(std::fs::read_to_string(&test_file).unwrap(), content);
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_write_permission_denied_is_reported() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("write_permission_denied_is_reported");
            let dir = harness.create_dir("readonly");
            let mut perms = std::fs::metadata(&dir).unwrap().permissions();
            perms.set_mode(0o500);
            std::fs::set_permissions(&dir, perms).unwrap();

            let tool = pi::tools::WriteTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": dir.join("file.txt").to_string_lossy(),
                "content": "data"
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("should error");
            let message = err.to_string();
            harness
                .log()
                .info_ctx("verify", "write permission error", |ctx| {
                    ctx.push(("message".into(), message.clone()));
                });
            assert!(message.contains("Tool error: write:"));
        });
    }

    #[test]
    fn test_write_creates_directories() {
        asupersync::test_utils::run_test(|| async {
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
        });
    }
}

mod edit_tool {
    use super::*;

    #[test]
    fn test_edit_replace_text() {
        asupersync::test_utils::run_test(|| async {
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
        });
    }

    #[test]
    fn test_edit_missing_file_reports_not_found() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("edit_missing_file_reports_not_found");
            let tool = pi::tools::EditTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": "missing.txt",
                "oldText": "old",
                "newText": "new"
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("should error");
            let message = err.to_string();
            harness
                .log()
                .info_ctx("verify", "missing file error", |ctx| {
                    ctx.push(("message".into(), message.clone()));
                });
            assert!(message.contains("File not found"));
        });
    }

    #[test]
    fn test_edit_text_not_found() {
        asupersync::test_utils::run_test(|| async {
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
        });
    }

    #[test]
    fn test_edit_multiple_occurrences() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let test_file = temp_dir.path().join("test.txt");
            std::fs::write(&test_file, "Hello, Hello, Hello!").unwrap();

            let tool = pi::tools::EditTool::new(temp_dir.path());
            let input = serde_json::json!({
                "path": test_file.to_string_lossy(),
                "oldText": "Hello",
                "newText": "Hi"
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("should error");
            let message = err.to_string();
            assert!(message.contains("Found 3 occurrences"));
        });
    }
}

mod bash_tool {
    use super::*;

    #[test]
    fn test_bash_simple_command() {
        asupersync::test_utils::run_test(|| async {
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
        });
    }

    #[test]
    fn test_bash_timeout_is_reported() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("bash_timeout_is_reported");
            let tool = pi::tools::BashTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "command": "sleep 2",
                "timeout": 1
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("should timeout");
            let message = err.to_string();
            harness.log().info_ctx("verify", "timeout message", |ctx| {
                ctx.push(("message".into(), message.clone()));
            });
            assert!(message.contains("Command timed out after 1 seconds"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_bash_truncation_sets_details() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("bash_truncation_sets_details");
            let tool = pi::tools::BashTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "command": "head -c 60000 /dev/zero | tr '\\\\0' 'a'"
            });

            let result = tool
                .execute("test-id", input, None)
                .await
                .expect("should succeed");

            let text = get_text_content(&result.content);
            assert!(text.contains("Full output:"));
            let details = result.details.expect("expected details");
            assert!(details.get("truncation").is_some());
            assert!(details.get("fullOutputPath").is_some());
        });
    }

    #[test]
    fn test_bash_exit_code() {
        asupersync::test_utils::run_test(|| async {
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
        });
    }

    #[test]
    fn test_bash_working_directory() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            std::fs::write(temp_dir.path().join("test.txt"), "content").unwrap();

            let tool = pi::tools::BashTool::new(temp_dir.path());
            let input = serde_json::json!({
                "command": "ls test.txt"
            });

            let result = tool.execute("test-id", input, None).await;
            assert!(result.is_ok());
        });
    }
}

mod grep_tool {
    use super::*;

    #[test]
    fn test_grep_basic_pattern() {
        asupersync::test_utils::run_test(|| async {
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
        });
    }

    #[test]
    fn test_grep_invalid_path_reports_error() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_invalid_path_reports_error");
            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "pattern": "needle",
                "path": "missing_dir"
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("should error");
            let message = err.to_string();
            harness
                .log()
                .info_ctx("verify", "grep invalid path error", |ctx| {
                    ctx.push(("message".into(), message.clone()));
                });
            assert!(message.contains("Cannot access path"));
        });
    }

    #[test]
    fn test_grep_case_insensitive() {
        asupersync::test_utils::run_test(|| async {
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
        });
    }

    #[test]
    fn test_grep_limit_reached_sets_details() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_limit_reached_sets_details");
            harness.create_file("test.txt", b"match\nmatch\nmatch\n");
            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "pattern": "match",
                "limit": 1
            });

            let result = tool
                .execute("test-id", input, None)
                .await
                .expect("should succeed");

            let text = get_text_content(&result.content);
            assert!(text.contains("matches limit reached"));
            let details = result.details.expect("expected details");
            assert_eq!(
                details.get("matchLimitReached"),
                Some(&serde_json::Value::Number(1u64.into()))
            );
        });
    }

    #[test]
    fn test_grep_long_line_truncates_and_marks_details() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_long_line_truncates_and_marks_details");
            let long_line = format!("match {}", "a".repeat(600));
            harness.create_file("long.txt", long_line.as_bytes());
            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "pattern": "match"
            });

            let result = tool
                .execute("test-id", input, None)
                .await
                .expect("should succeed");

            let text = get_text_content(&result.content);
            assert!(text.contains("... [truncated]"));
            let details = result.details.expect("expected details");
            assert_eq!(
                details.get("linesTruncated"),
                Some(&serde_json::Value::Bool(true))
            );
        });
    }

    #[test]
    fn test_grep_no_matches() {
        asupersync::test_utils::run_test(|| async {
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
        });
    }
}

mod find_tool {
    use super::*;

    #[test]
    fn test_find_glob_pattern() {
        asupersync::test_utils::run_test(|| async {
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
        });
    }

    #[test]
    fn test_find_invalid_path_reports_error() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("find_invalid_path_reports_error");
            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "pattern": "*.txt",
                "path": "missing_dir"
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("should error");
            let message = err.to_string();
            harness
                .log()
                .info_ctx("verify", "find invalid path error", |ctx| {
                    ctx.push(("message".into(), message.clone()));
                });
            assert!(message.contains("Path not found"));
        });
    }

    #[test]
    fn test_find_limit_reached_sets_details() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("find_limit_reached_sets_details");
            harness.create_file("file1.txt", b"");
            harness.create_file("file2.txt", b"");
            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "pattern": "*.txt",
                "limit": 1
            });

            let result = tool
                .execute("test-id", input, None)
                .await
                .expect("should succeed");

            let text = get_text_content(&result.content);
            assert!(text.contains("results limit reached"));
            let details = result.details.expect("expected details");
            assert_eq!(
                details.get("resultLimitReached"),
                Some(&serde_json::Value::Number(1u64.into()))
            );
        });
    }

    #[test]
    fn test_find_no_matches() {
        asupersync::test_utils::run_test(|| async {
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
        });
    }
}

mod ls_tool {
    use super::*;

    #[test]
    fn test_ls_directory() {
        asupersync::test_utils::run_test(|| async {
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
        });
    }

    #[test]
    fn test_ls_nonexistent_directory() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let tool = pi::tools::LsTool::new(temp_dir.path());
            let input = serde_json::json!({
                "path": "/nonexistent/directory"
            });

            let result = tool.execute("test-id", input, None).await;
            assert!(result.is_err());
        });
    }

    #[test]
    fn test_ls_path_is_file_reports_error() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("ls_path_is_file_reports_error");
            let path = harness.create_file("file.txt", b"content");
            let tool = pi::tools::LsTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy()
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("should error");
            let message = err.to_string();
            assert!(message.contains("Not a directory"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_ls_permission_denied_is_reported() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("ls_permission_denied_is_reported");
            let dir = harness.create_dir("locked");
            let mut perms = std::fs::metadata(&dir).unwrap().permissions();
            perms.set_mode(0o000);
            std::fs::set_permissions(&dir, perms).unwrap();

            let tool = pi::tools::LsTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": dir.to_string_lossy()
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("should error");
            let message = err.to_string();
            assert!(message.contains("Cannot read directory"));
        });
    }

    #[test]
    fn test_ls_limit_reached_sets_details() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("ls_limit_reached_sets_details");
            harness.create_file("file1.txt", b"");
            harness.create_file("file2.txt", b"");
            let tool = pi::tools::LsTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "limit": 1
            });

            let result = tool
                .execute("test-id", input, None)
                .await
                .expect("should succeed");

            let text = get_text_content(&result.content);
            assert!(text.contains("entries limit reached"));
            let details = result.details.expect("expected details");
            assert_eq!(
                details.get("entryLimitReached"),
                Some(&serde_json::Value::Number(1u64.into()))
            );
        });
    }

    #[test]
    fn test_ls_empty_directory() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let tool = pi::tools::LsTool::new(temp_dir.path());
            let input = serde_json::json!({});

            let result = tool
                .execute("test-id", input, None)
                .await
                .expect("should succeed");

            let text = get_text_content(&result.content);
            assert!(text.contains("empty directory"));
        });
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
