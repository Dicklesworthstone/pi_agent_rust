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
            assert!(
                text.contains("matches limit reached"),
                "expected grep output to include match-limit notice; got: {text:?}"
            );
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
            assert!(
                text.contains("... [truncated]"),
                "expected grep output to include per-line truncation marker; got: {text:?}"
            );
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

// ---------------------------------------------------------------------------
// E2E tool tests with artifact logging (bd-2xyv)
// ---------------------------------------------------------------------------

/// Check whether a binary is available on PATH.
fn binary_available(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Log a tool execution as an artifact: input JSON, output text, details, `is_error`.
fn log_tool_execution(
    logger: &common::logging::TestLogger,
    tool_name: &str,
    tool_call_id: &str,
    input: &serde_json::Value,
    result: &pi::PiResult<pi::tools::ToolOutput>,
) {
    match result {
        Ok(output) => {
            let text = get_text_content(&output.content);
            logger.info_ctx("tool_exec", format!("{tool_name} succeeded"), |ctx| {
                ctx.push(("tool_call_id".into(), tool_call_id.to_string()));
                ctx.push(("input".into(), input.to_string()));
                ctx.push(("output_text".into(), text));
                ctx.push((
                    "details".into(),
                    output
                        .details
                        .as_ref()
                        .map_or_else(|| "null".to_string(), |d: &serde_json::Value| d.to_string()),
                ));
                ctx.push(("is_error".into(), output.is_error.to_string()));
            });
        }
        Err(e) => {
            let err_str = e.to_string();
            logger.info_ctx("tool_exec", format!("{tool_name} errored"), |ctx| {
                ctx.push(("tool_call_id".into(), tool_call_id.to_string()));
                ctx.push(("input".into(), input.to_string()));
                ctx.push(("error".into(), err_str.clone()));
            });
        }
    }
}

mod e2e_read {
    use super::*;

    #[test]
    fn e2e_read_success_with_artifacts() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_read_success_with_artifacts");
            let path = harness.create_file("sample.txt", b"alpha\nbeta\ngamma");
            let tool = pi::tools::ReadTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy()
            });

            let result = tool.execute("read-001", input.clone(), None).await;
            log_tool_execution(harness.log(), "read", "read-001", &input, &result);

            let output = result.expect("should succeed");
            let text = get_text_content(&output.content);
            assert!(text.contains("alpha"));
            assert!(text.contains("gamma"));
            assert!(!output.is_error);
        });
    }

    #[test]
    fn e2e_read_empty_file() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_read_empty_file");
            let path = harness.create_file("empty.txt", b"");
            let tool = pi::tools::ReadTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy()
            });

            let result = tool.execute("read-002", input.clone(), None).await;
            log_tool_execution(harness.log(), "read", "read-002", &input, &result);

            let output = result.expect("should succeed");
            let text = get_text_content(&output.content);
            assert!(
                text.contains("empty") || text.is_empty() || text.trim().is_empty(),
                "empty file should produce empty or 'empty' message, got: {text}"
            );
        });
    }

    #[test]
    fn e2e_read_missing_file_with_artifacts() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_read_missing_file_with_artifacts");
            let tool = pi::tools::ReadTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": "/nonexistent/path/ghost.txt"
            });

            let result = tool.execute("read-003", input.clone(), None).await;
            log_tool_execution(harness.log(), "read", "read-003", &input, &result);

            assert!(result.is_err());
        });
    }

    #[test]
    fn e2e_read_truncation_details_captured() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_read_truncation_details_captured");
            let total_lines = pi::tools::DEFAULT_MAX_LINES + 10;
            let mut content = String::new();
            for i in 1..=total_lines {
                content.push_str("line");
                content.push_str(&i.to_string());
                content.push('\n');
            }
            let path = harness.create_file("big.txt", content.as_bytes());
            let tool = pi::tools::ReadTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy()
            });

            let result = tool.execute("read-004", input.clone(), None).await;
            log_tool_execution(harness.log(), "read", "read-004", &input, &result);

            let output = result.expect("should truncate");
            let details = output.details.expect("truncation details");
            let truncation = details.get("truncation").expect("truncation object");
            assert_eq!(
                truncation.get("truncated"),
                Some(&serde_json::Value::Bool(true))
            );
            assert_eq!(
                truncation.get("truncatedBy"),
                Some(&serde_json::Value::String("lines".to_string()))
            );
            assert!(
                truncation
                    .get("totalLines")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
                    >= total_lines as u64
            );
        });
    }
}

mod e2e_write {
    use super::*;

    #[test]
    fn e2e_write_new_file_with_artifacts() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_write_new_file_with_artifacts");
            let path = harness.temp_path("output.txt");
            let tool = pi::tools::WriteTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "content": "hello world\nline two"
            });

            let result = tool.execute("write-001", input.clone(), None).await;
            log_tool_execution(harness.log(), "write", "write-001", &input, &result);

            let output = result.expect("should succeed");
            assert!(!output.is_error);
            assert!(path.exists());
            let disk = std::fs::read_to_string(&path).unwrap();
            assert_eq!(disk, "hello world\nline two");
        });
    }

    #[test]
    fn e2e_write_overwrite_existing() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_write_overwrite_existing");
            let path = harness.create_file("existing.txt", b"old content");
            let tool = pi::tools::WriteTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "content": "new content"
            });

            let result = tool.execute("write-002", input.clone(), None).await;
            log_tool_execution(harness.log(), "write", "write-002", &input, &result);

            let output = result.expect("should succeed");
            assert!(!output.is_error);
            let disk = std::fs::read_to_string(&path).unwrap();
            assert_eq!(disk, "new content");
        });
    }
}

mod e2e_edit {
    use super::*;

    #[test]
    fn e2e_edit_success_with_artifacts() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_edit_success_with_artifacts");
            let path = harness.create_file("code.rs", b"fn main() {\n    println!(\"old\");\n}\n");
            let tool = pi::tools::EditTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "oldText": "\"old\"",
                "newText": "\"new\""
            });

            let result = tool.execute("edit-001", input.clone(), None).await;
            log_tool_execution(harness.log(), "edit", "edit-001", &input, &result);

            let output = result.expect("should succeed");
            assert!(!output.is_error);
            let disk = std::fs::read_to_string(&path).unwrap();
            assert!(disk.contains("\"new\""));
            assert!(!disk.contains("\"old\""));

            // Verify diff details are present
            let details = output.details.expect("should have diff details");
            assert!(details.get("diff").is_some());
        });
    }

    #[test]
    fn e2e_edit_text_not_found_with_artifacts() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_edit_text_not_found_with_artifacts");
            let path = harness.create_file("stable.txt", b"content stays");
            let tool = pi::tools::EditTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "oldText": "nonexistent needle",
                "newText": "replacement"
            });

            let result = tool.execute("edit-002", input.clone(), None).await;
            log_tool_execution(harness.log(), "edit", "edit-002", &input, &result);

            assert!(result.is_err());
            // File should not be modified
            let disk = std::fs::read_to_string(&path).unwrap();
            assert_eq!(disk, "content stays");
        });
    }
}

mod e2e_bash {
    use super::*;

    #[test]
    fn e2e_bash_success_with_artifacts() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_bash_success_with_artifacts");
            let tool = pi::tools::BashTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "command": "echo hello && echo world"
            });

            let result = tool.execute("bash-001", input.clone(), None).await;
            log_tool_execution(harness.log(), "bash", "bash-001", &input, &result);

            let output = result.expect("should succeed");
            let text = get_text_content(&output.content);
            assert!(text.contains("hello"));
            assert!(text.contains("world"));
        });
    }

    #[test]
    fn e2e_bash_stderr_captured() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_bash_stderr_captured");
            let tool = pi::tools::BashTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "command": "echo stdout_msg && echo stderr_msg >&2"
            });

            let result = tool.execute("bash-002", input.clone(), None).await;
            log_tool_execution(harness.log(), "bash", "bash-002", &input, &result);

            let output = result.expect("should succeed");
            let text = get_text_content(&output.content);
            // Both stdout and stderr should be captured
            assert!(text.contains("stdout_msg"));
            assert!(text.contains("stderr_msg"));
        });
    }

    #[test]
    fn e2e_bash_nonexistent_command() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_bash_nonexistent_command");
            let tool = pi::tools::BashTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "command": "totally_nonexistent_binary_xyz_123"
            });

            let result = tool.execute("bash-003", input.clone(), None).await;
            log_tool_execution(harness.log(), "bash", "bash-003", &input, &result);

            // Should error with non-zero exit code (127 = command not found)
            assert!(result.is_err(), "nonexistent command should fail");
            let message = result.unwrap_err().to_string();
            assert!(
                message.contains("127") || message.contains("not found"),
                "expected exit code 127 or 'not found' in: {message}"
            );
        });
    }

    #[test]
    fn e2e_bash_timeout_with_artifacts() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_bash_timeout_with_artifacts");
            let tool = pi::tools::BashTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "command": "sleep 10",
                "timeout": 1
            });

            let result = tool.execute("bash-004", input.clone(), None).await;
            log_tool_execution(harness.log(), "bash", "bash-004", &input, &result);

            assert!(result.is_err());
            let message = result.unwrap_err().to_string();
            assert!(message.contains("timed out"));
        });
    }
}

mod e2e_grep {
    use super::*;

    #[test]
    fn e2e_grep_success_with_artifacts() {
        if !binary_available("rg") {
            eprintln!("SKIP: rg (ripgrep) not available on PATH");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_grep_success_with_artifacts");
            harness.create_file("src/main.rs", b"fn main() {\n    println!(\"hello\");\n}\n");
            harness.create_file(
                "src/lib.rs",
                b"pub fn greet() -> &'static str {\n    \"hello\"\n}\n",
            );
            harness.create_file(
                "readme.md",
                b"# Project\nNo hello here... actually hello.\n",
            );

            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "pattern": "hello"
            });

            let result = tool.execute("grep-001", input.clone(), None).await;
            log_tool_execution(harness.log(), "grep", "grep-001", &input, &result);

            let output = result.expect("should succeed");
            let text = get_text_content(&output.content);
            assert!(text.contains("hello"));
        });
    }

    #[test]
    fn e2e_grep_invalid_regex() {
        if !binary_available("rg") {
            eprintln!("SKIP: rg (ripgrep) not available on PATH");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_grep_invalid_regex");
            harness.create_file("data.txt", b"some text");
            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "pattern": "[invalid("
            });

            let result = tool.execute("grep-002", input.clone(), None).await;
            log_tool_execution(harness.log(), "grep", "grep-002", &input, &result);

            assert!(result.is_err(), "invalid regex should fail");
        });
    }

    #[test]
    fn e2e_grep_with_context_lines() {
        if !binary_available("rg") {
            eprintln!("SKIP: rg (ripgrep) not available on PATH");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_grep_with_context_lines");
            harness.create_file("data.txt", b"line1\nline2\nTARGET\nline4\nline5");

            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "pattern": "TARGET",
                "context": 1
            });

            let result = tool.execute("grep-003", input.clone(), None).await;
            log_tool_execution(harness.log(), "grep", "grep-003", &input, &result);

            let output = result.expect("should succeed");
            let text = get_text_content(&output.content);
            assert!(text.contains("TARGET"), "should contain match: {text}");
            // Context lines should include adjacent lines
            assert!(
                text.contains("line2") || text.contains("line4"),
                "should contain context lines: {text}"
            );
        });
    }
}

mod e2e_find {
    use super::*;

    #[test]
    fn e2e_find_success_with_artifacts() {
        if !binary_available("fd") {
            eprintln!("SKIP: fd not available on PATH");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_find_success_with_artifacts");
            harness.create_file("src/main.rs", b"");
            harness.create_file("src/lib.rs", b"");
            harness.create_file("tests/test.rs", b"");
            harness.create_file("readme.md", b"");

            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "pattern": "*.rs"
            });

            let result = tool.execute("find-001", input.clone(), None).await;
            log_tool_execution(harness.log(), "find", "find-001", &input, &result);

            let output = result.expect("should succeed");
            let text = get_text_content(&output.content);
            assert!(text.contains("main.rs"));
            assert!(text.contains("lib.rs"));
            assert!(text.contains("test.rs"));
            assert!(!text.contains("readme.md"));
        });
    }

    #[test]
    fn e2e_find_invalid_path_with_artifacts() {
        if !binary_available("fd") {
            eprintln!("SKIP: fd not available on PATH");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_find_invalid_path_with_artifacts");
            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "pattern": "*.txt",
                "path": "does_not_exist"
            });

            let result = tool.execute("find-002", input.clone(), None).await;
            log_tool_execution(harness.log(), "find", "find-002", &input, &result);

            assert!(result.is_err());
            let message = result.unwrap_err().to_string();
            assert!(message.contains("Path not found"));
        });
    }
}

mod e2e_ls {
    use super::*;

    #[test]
    fn e2e_ls_success_with_artifacts() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_ls_success_with_artifacts");
            harness.create_file("alpha.txt", b"a");
            harness.create_file("beta.txt", b"b");
            harness.create_dir("subdir");

            let tool = pi::tools::LsTool::new(harness.temp_dir());
            let input = serde_json::json!({});

            let result = tool.execute("ls-001", input.clone(), None).await;
            log_tool_execution(harness.log(), "ls", "ls-001", &input, &result);

            let output = result.expect("should succeed");
            let text = get_text_content(&output.content);
            assert!(text.contains("alpha.txt"));
            assert!(text.contains("beta.txt"));
            assert!(text.contains("subdir/"));
        });
    }

    #[test]
    fn e2e_ls_nonexistent_with_artifacts() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_ls_nonexistent_with_artifacts");
            let tool = pi::tools::LsTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": "/no/such/directory"
            });

            let result = tool.execute("ls-002", input.clone(), None).await;
            log_tool_execution(harness.log(), "ls", "ls-002", &input, &result);

            assert!(result.is_err());
        });
    }

    #[test]
    fn e2e_ls_file_not_dir_with_artifacts() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_ls_file_not_dir_with_artifacts");
            let path = harness.create_file("just_a_file.txt", b"contents");
            let tool = pi::tools::LsTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy()
            });

            let result = tool.execute("ls-003", input.clone(), None).await;
            log_tool_execution(harness.log(), "ls", "ls-003", &input, &result);

            assert!(result.is_err());
            let message = result.unwrap_err().to_string();
            assert!(message.contains("Not a directory"));
        });
    }

    #[test]
    fn e2e_ls_truncation_details_captured() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_ls_truncation_details_captured");
            // Create enough files to exceed limit=2
            harness.create_file("a.txt", b"");
            harness.create_file("b.txt", b"");
            harness.create_file("c.txt", b"");

            let tool = pi::tools::LsTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "limit": 2
            });

            let result = tool.execute("ls-004", input.clone(), None).await;
            log_tool_execution(harness.log(), "ls", "ls-004", &input, &result);

            let output = result.expect("should succeed");
            let text = get_text_content(&output.content);
            assert!(text.contains("entries limit reached"));
            let details = output.details.expect("truncation details");
            assert_eq!(
                details.get("entryLimitReached"),
                Some(&serde_json::Value::Number(2u64.into()))
            );
        });
    }
}

/// Comprehensive E2E: exercise all 7 tools in a single test workspace with full artifact logging.
#[test]
fn e2e_all_tools_roundtrip() {
    asupersync::test_utils::run_test(|| async {
        let harness = TestHarness::new("e2e_all_tools_roundtrip");
        harness.section("Setup workspace");

        // Write a file
        let write_tool = pi::tools::WriteTool::new(harness.temp_dir());
        let write_input = serde_json::json!({
            "path": harness.temp_path("project/hello.rs").to_string_lossy().to_string(),
            "content": "fn main() {\n    println!(\"Hello, world!\");\n}\n"
        });
        let result = write_tool
            .execute("rt-write", write_input.clone(), None)
            .await;
        log_tool_execution(harness.log(), "write", "rt-write", &write_input, &result);
        result.expect("write should succeed");

        // Read the file back
        harness.section("Read");
        let read_tool = pi::tools::ReadTool::new(harness.temp_dir());
        let read_input = serde_json::json!({
            "path": harness.temp_path("project/hello.rs").to_string_lossy().to_string()
        });
        let result = read_tool.execute("rt-read", read_input.clone(), None).await;
        log_tool_execution(harness.log(), "read", "rt-read", &read_input, &result);
        let output = result.expect("read should succeed");
        let text = get_text_content(&output.content);
        assert!(text.contains("Hello, world!"));

        // Edit the file
        harness.section("Edit");
        let edit_tool = pi::tools::EditTool::new(harness.temp_dir());
        let edit_input = serde_json::json!({
            "path": harness.temp_path("project/hello.rs").to_string_lossy().to_string(),
            "oldText": "Hello, world!",
            "newText": "Hello, Rust!"
        });
        let result = edit_tool.execute("rt-edit", edit_input.clone(), None).await;
        log_tool_execution(harness.log(), "edit", "rt-edit", &edit_input, &result);
        result.expect("edit should succeed");

        // Verify edit with read
        let result = read_tool
            .execute("rt-read2", read_input.clone(), None)
            .await;
        let output = result.expect("read after edit should succeed");
        let text = get_text_content(&output.content);
        assert!(text.contains("Hello, Rust!"));
        assert!(!text.contains("Hello, world!"));

        // Ls the directory
        harness.section("Ls");
        let ls_tool = pi::tools::LsTool::new(harness.temp_dir());
        let ls_input = serde_json::json!({
            "path": harness.temp_path("project").to_string_lossy().to_string()
        });
        let result = ls_tool.execute("rt-ls", ls_input.clone(), None).await;
        log_tool_execution(harness.log(), "ls", "rt-ls", &ls_input, &result);
        let output = result.expect("ls should succeed");
        let text = get_text_content(&output.content);
        assert!(text.contains("hello.rs"));

        // Bash
        harness.section("Bash");
        let bash_tool = pi::tools::BashTool::new(harness.temp_dir());
        let bash_input = serde_json::json!({
            "command": "wc -l project/hello.rs"
        });
        let result = bash_tool.execute("rt-bash", bash_input.clone(), None).await;
        log_tool_execution(harness.log(), "bash", "rt-bash", &bash_input, &result);
        let output = result.expect("bash should succeed");
        let text = get_text_content(&output.content);
        // wc output should contain a number
        assert!(text.chars().any(|c| c.is_ascii_digit()));

        // Grep (if rg available)
        if binary_available("rg") {
            harness.section("Grep");
            let grep_tool = pi::tools::GrepTool::new(harness.temp_dir());
            let grep_input = serde_json::json!({
                "pattern": "Rust"
            });
            let result = grep_tool.execute("rt-grep", grep_input.clone(), None).await;
            log_tool_execution(harness.log(), "grep", "rt-grep", &grep_input, &result);
            let output = result.expect("grep should succeed");
            let text = get_text_content(&output.content);
            assert!(text.contains("Rust"));
        } else {
            harness
                .log()
                .warn("skip", "rg not available, skipping grep step");
        }

        // Find (if fd available)
        if binary_available("fd") {
            harness.section("Find");
            let find_tool = pi::tools::FindTool::new(harness.temp_dir());
            let find_input = serde_json::json!({
                "pattern": "*.rs"
            });
            let result = find_tool.execute("rt-find", find_input.clone(), None).await;
            log_tool_execution(harness.log(), "find", "rt-find", &find_input, &result);
            let output = result.expect("find should succeed");
            let text = get_text_content(&output.content);
            assert!(text.contains("hello.rs"));
        } else {
            harness
                .log()
                .warn("skip", "fd not available, skipping find step");
        }

        harness.section("Done");
        harness
            .log()
            .info("summary", "All tool roundtrip steps passed");
    });
}
